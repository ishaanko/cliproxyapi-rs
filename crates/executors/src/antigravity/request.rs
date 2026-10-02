//! Request construction (Go: antigravity_executor_request.go).
//!
//! Wraps the translated Gemini request in the Cloud Code envelope (`geminiToAntigravity`),
//! cleans JSON schemas only where schemas live, and assembles URL, headers and body.

use std::collections::HashMap;

use cpa_auth::Auth;
use cpa_json::J;
use http::{HeaderMap, HeaderValue};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::auth::{missing_project_id_error, project_id_from_auth};
use crate::helps::payload::set_string_if_different;
use cpa_runtime::executor::ExecError;

pub(crate) const BASE_URL_DAILY: &str = "https://daily-cloudcode-pa.googleapis.com";
pub(crate) const BASE_URL_PROD: &str = "https://cloudcode-pa.googleapis.com";
pub(crate) const COUNT_TOKENS_PATH: &str = "/v1internal:countTokens";
pub(crate) const STREAM_PATH: &str = "/v1internal:streamGenerateContent";
pub(crate) const GENERATE_PATH: &str = "/v1internal:generateContent";

/// A fully assembled upstream call.
pub(crate) struct BuiltRequest {
    pub url: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

// ---------------------------------------------------------------- endpoints and identity

/// `attributes.base_url`, else `metadata.base_url` (trailing slash trimmed); empty when unset.
pub(crate) fn resolve_custom_base_url(auth: &Auth) -> String {
    let v = auth.attr("base_url");
    let v = v.trim();
    if !v.is_empty() {
        return v.trim_end_matches('/').to_string();
    }
    let v = auth.metadata.get("base_url").and_then(Value::as_str).map(str::trim).unwrap_or("");
    v.trim_end_matches('/').to_string()
}

/// One request endpoint without cross-tier fallback: custom base URL, else the daily host.
pub(crate) fn resolve_request_base_url(auth: &Auth) -> String {
    let custom = resolve_custom_base_url(auth);
    if custom.is_empty() { BASE_URL_DAILY.to_string() } else { custom }
}

/// loadCodeAssist host: custom base URL, else the prod host.
pub(crate) fn load_code_assist_base_url(auth: &Auth) -> String {
    let custom = resolve_custom_base_url(auth);
    if custom.is_empty() { BASE_URL_PROD.to_string() } else { custom }
}

fn configured_user_agent(auth: &Auth) -> String {
    let ua = auth.attr("user_agent");
    if !ua.trim().is_empty() {
        return ua.trim().to_string();
    }
    auth.metadata
        .get("user_agent")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// Short runtime User-Agent (`antigravity/hub/<version> darwin/arm64`) unless the credential
/// pins an `antigravity/...` one.
pub(crate) fn resolve_user_agent(auth: &Auth) -> String {
    cpa_core::misc::antigravity_request_user_agent(&configured_user_agent(auth))
}

// ---------------------------------------------------------------- envelope

fn is_image_model(model: &str) -> bool {
    model.contains("image")
}

fn generate_request_id() -> String {
    format!("agent-{}", uuid::Uuid::new_v4())
}

fn generate_image_gen_request_id() -> String {
    format!("image_gen/{}/{}/12", chrono::Utc::now().timestamp_millis(), uuid::Uuid::new_v4())
}

/// Random negative decimal session id (`-` + value below 9e18).
pub(crate) fn generate_session_id() -> String {
    use rand::Rng;
    let n: i64 = rand::rng().random_range(0..9_000_000_000_000_000_000);
    format!("-{n}")
}

/// Session id derived from the first non-empty user text so one conversation keeps one id.
pub(crate) fn generate_stable_session_id(payload: &Value) -> String {
    let contents = payload.g("request.contents");
    if !contents.is_array() {
        return generate_session_id();
    }
    for content in contents.array() {
        if content.g("role").str() != "user" {
            continue;
        }
        let text = content.g("parts.0.text").str();
        if text.is_empty() {
            continue;
        }
        let hash = Sha256::digest(text.as_bytes());
        let value = (u64::from_be_bytes(hash[..8].try_into().unwrap_or([0; 8])) & 0x7FFF_FFFF_FFFF_FFFF) as i64;
        return format!("-{value}");
    }
    generate_session_id()
}

/// Wraps a Gemini request into the Cloud Code envelope (Go: geminiToAntigravity).
#[cfg(test)]
pub(crate) fn gemini_to_antigravity(model: &str, payload: &[u8], project_id: &str, derived_session_ids: &[String]) -> Vec<u8> {
    let mut template = cpa_json::parse(payload);
    gemini_to_antigravity_value(&mut template, model, project_id, derived_session_ids);
    cpa_json::to_vec(&template)
}

pub(crate) fn gemini_to_antigravity_value(template: &mut Value, model: &str, project_id: &str, derived_session_ids: &[String]) {
    set_string_if_different(template, "model", model);
    set_string_if_different(template, "userAgent", "antigravity");

    let image = is_image_model(model);
    let mut req_type = template.g("requestType").str().trim().to_string();
    if req_type.is_empty() {
        req_type = if image { "image_gen" } else { "agent" }.to_string();
        cpa_json::set(template, "requestType", req_type.clone());
    }

    if !project_id.is_empty() {
        set_string_if_different(template, "project", project_id);
    } else {
        cpa_json::delete(template, "project");
    }

    if image {
        cpa_json::set(template, "requestId", generate_image_gen_request_id());
    } else if req_type != "web_search" {
        cpa_json::set(template, "requestId", generate_request_id());
        let mut session_id = template.g("request.sessionId").str().trim().to_string();
        if session_id.is_empty()
            && let Some(first) = derived_session_ids.first()
        {
            session_id = first.trim().to_string();
        }
        if session_id.is_empty() {
            session_id = generate_stable_session_id(template);
        }
        cpa_json::set(template, "request.sessionId", session_id);
    }

    cpa_json::delete(template, "request.safetySettings");
    let tool_config = template.g("toolConfig");
    if tool_config.exists() && !template.g("request.toolConfig").exists() {
        let raw = tool_config.value();
        drop(tool_config);
        cpa_json::set(template, "request.toolConfig", raw);
        cpa_json::delete(template, "toolConfig");
    }
}

// ---------------------------------------------------------------- schema sanitization

/// Schema locations handled by the sanitizer; both spellings are cleaned in place.
const DECLARATION_SCHEMA_KEYS: [&str; 6] = [
    "parameters",
    "parametersJsonSchema",
    "parameters_json_schema",
    "response",
    "responseJsonSchema",
    "response_json_schema",
];
const GENERATION_CONFIG_CONTAINERS: [&str; 2] = ["request.generationConfig", "request.generation_config"];
const GENERATION_SCHEMA_KEYS: [&str; 4] = ["responseSchema", "responseJsonSchema", "response_schema", "response_json_schema"];
const SCHEMA_WRAPPER_KEY: &str = "schema";

/// Whether the payload has tools or response schemas that need cleaning.
pub(crate) fn request_needs_schema_sanitization(payload: &Value) -> bool {
    if payload.g("request.tools.0").exists() {
        return true;
    }
    GENERATION_CONFIG_CONTAINERS
        .iter()
        .any(|c| GENERATION_SCHEMA_KEYS.iter().any(|k| payload.g(&format!("{c}.{k}")).exists()))
}

/// Cleans the JSON schemas carried by the request, never touching functionCall args in history.
pub(crate) fn sanitize_request_schemas(payload: &mut Value, use_antigravity_schema: bool) {
    sanitize_tool_schemas(payload, use_antigravity_schema);
    sanitize_generation_schemas(payload);
}

fn sanitize_tool_schemas(payload: &mut Value, use_antigravity_schema: bool) {
    let tools = payload.g("request.tools");
    if !tools.is_array() {
        return;
    }
    let original = tools.value();
    drop(tools);
    let mut doc = serde_json::json!({"request": {"tools": original.clone()}});
    sanitize_tool_schema_document(&mut doc, use_antigravity_schema);
    let cleaned = doc.g("request.tools");
    if !cleaned.is_array() || cleaned.v() == Some(&original) {
        return;
    }
    let cleaned = cleaned.value();
    cpa_json::set(payload, "request.tools", cleaned);
}

/// Paths of every function declaration under `request.tools`.
fn function_declaration_paths(doc: &Value) -> Vec<String> {
    let tools = doc.g("request.tools");
    let mut paths = Vec::new();
    if !tools.is_array() {
        return paths;
    }
    for (i, tool) in tools.array().iter().enumerate() {
        for decl_key in ["functionDeclarations", "function_declarations"] {
            let decls = tool.g(decl_key);
            if !decls.is_array() {
                continue;
            }
            for j in 0..decls.array().len() {
                paths.push(format!("request.tools.{i}.{decl_key}.{j}"));
            }
        }
    }
    paths
}

fn sanitize_tool_schema_document(doc: &mut Value, use_antigravity_schema: bool) {
    for base in function_declaration_paths(doc) {
        let old_path = format!("{base}.parametersJsonSchema");
        let Some(value) = doc.g(&old_path).into_value() else { continue };
        cpa_json::set(doc, &format!("{base}.parameters"), value);
        cpa_json::delete(doc, &old_path);
    }

    // Paths are computed once, after the rename, like Go.
    let mut paths = Vec::new();
    for base in function_declaration_paths(doc) {
        for key in DECLARATION_SCHEMA_KEYS {
            if doc.g(&format!("{base}.{key}")).is_object() {
                paths.push(format!("{base}.{key}"));
            }
        }
    }
    for path in paths {
        let Some(schema) = doc.g(&path).into_value() else { continue };
        let cleaned = clean_nested_schema(&schema, |s| {
            cpa_core::util::clean_json_schema_for_antigravity_tool(s, use_antigravity_schema)
        });
        if cleaned != schema {
            cpa_json::set(doc, &path, cleaned);
        }
    }
}

/// Cleans a schema nested one level down, then unwraps it, so top-level placeholder rules do
/// not apply (Go: cleanNestedSchema).
fn clean_nested_schema(schema: &Value, clean: impl Fn(&str) -> String) -> Value {
    let raw = cpa_json::to_string(schema);
    let mut wrapped = serde_json::json!({});
    cpa_json::set(&mut wrapped, SCHEMA_WRAPPER_KEY, schema.clone());
    let cleaned = cpa_json::parse_str(&clean(&cpa_json::to_string(&wrapped)));
    match cleaned.g(SCHEMA_WRAPPER_KEY).into_value() {
        Some(v) => v,
        None => cpa_json::parse_str(&clean(&raw)),
    }
}

fn sanitize_generation_schemas(payload: &mut Value) {
    for container in GENERATION_CONFIG_CONTAINERS {
        let config = payload.g(container);
        if !config.is_object() {
            continue;
        }
        let original = config.value();
        drop(config);
        let mut cleaned_config = original.clone();
        for key in GENERATION_SCHEMA_KEYS {
            let Some(schema) = cleaned_config.g(key).into_value().filter(Value::is_object) else { continue };
            let cleaned = cpa_json::parse_str(&cpa_core::util::clean_json_schema_for_antigravity_response(
                &cpa_json::to_string(&schema),
            ));
            if cleaned != schema {
                cpa_json::set(&mut cleaned_config, key, cleaned);
            }
        }
        if cleaned_config != original {
            cpa_json::set(payload, container, cleaned_config);
        }
    }
}

// ---------------------------------------------------------------- URL and request

fn query_escape(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// Request URL: `?alt=sse` by default for streams, `?$alt=<alt>` when an alt is set.
pub(crate) fn request_url(base: &str, stream: bool, alt: &str) -> String {
    let mut url = String::from(base);
    url.push_str(if stream { STREAM_PATH } else { GENERATE_PATH });
    if stream {
        if alt.is_empty() {
            url.push_str("?alt=sse");
        } else {
            url.push_str("?$alt=");
            url.push_str(&query_escape(alt));
        }
    } else if !alt.is_empty() {
        url.push_str("?$alt=");
        url.push_str(&query_escape(alt));
    }
    url
}

/// `Content-Type`, bearer token and User-Agent, plus the credential's custom headers.
pub(crate) fn base_headers(auth: &Auth, token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(http::header::AUTHORIZATION, v);
    }
    if let Ok(v) = HeaderValue::from_str(&resolve_user_agent(auth)) {
        headers.insert(http::header::USER_AGENT, v);
    }
    apply_custom_headers(&mut headers, auth);
    headers
}

/// User-defined `header:` attributes of the credential override the defaults.
pub(crate) fn apply_custom_headers(headers: &mut HeaderMap, auth: &Auth) {
    let attrs: HashMap<String, String> = auth.attributes.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    cpa_core::util::apply_custom_headers_from_attrs(headers, &attrs, None, None);
}

/// Everything `buildRequest` does: envelope, token cap, schema cleaning, tool mode, headers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_request(
    auth: &Auth,
    token: &str,
    model_name: &str,
    payload: &[u8],
    stream: bool,
    alt: &str,
    base_url: &str,
    derived_session_ids: &[String],
) -> Result<BuiltRequest, ExecError> {
    if token.is_empty() {
        return Err(ExecError::new(401, "missing access token"));
    }
    let mut base = base_url.trim_end_matches('/').to_string();
    if base.is_empty() {
        base = resolve_request_base_url(auth);
    }
    let url = request_url(&base, stream, alt);

    let project_id = project_id_from_auth(auth);
    if project_id.is_empty() {
        return Err(missing_project_id_error(None));
    }
    let mut doc = cpa_json::parse(payload);
    gemini_to_antigravity_value(&mut doc, model_name, &project_id, derived_session_ids);

    // Cap maxOutputTokens to the registry's max_completion_tokens.
    let max_out = doc.g("request.generationConfig.maxOutputTokens");
    if max_out.is_number() {
        let requested = max_out.int();
        drop(max_out);
        if let Some(info) = cpa_core::registry::lookup_model_info(model_name, Some("antigravity"))
            && info.max_completion_tokens > 0
            && requested > info.max_completion_tokens
        {
            cpa_json::set(&mut doc, "request.generationConfig.maxOutputTokens", info.max_completion_tokens);
        }
    } else {
        drop(max_out);
    }

    let use_antigravity_schema =
        model_name.contains("claude") || model_name.contains("gemini-3-pro") || model_name.contains("gemini-3.1-pro");
    if request_needs_schema_sanitization(&doc) {
        sanitize_request_schemas(&mut doc, use_antigravity_schema);
    }
    if model_name.contains("claude") {
        cpa_json::set(&mut doc, "request.toolConfig.functionCallingConfig.mode", "VALIDATED");
    } else {
        cpa_json::delete(&mut doc, "request.generationConfig.maxOutputTokens");
    }

    Ok(BuiltRequest { url, headers: base_headers(auth, token), body: cpa_json::to_vec(&doc) })
}
