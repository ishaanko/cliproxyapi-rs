//! xAI request construction (Go: xai_executor_request.go): credentials, endpoint and header
//! selection, and the Responses body pipeline shared by chat, compact, token counting and the
//! websocket transport.

use std::collections::{HashMap, HashSet};

use cpa_auth::Auth;
use cpa_auth::xai::{CLI_CHAT_PROXY_BASE_URL, DEFAULT_API_BASE_URL};
use cpa_config::Config;
use cpa_core::thinking::parse_suffix;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ExecError, Metadata, Options, Request, meta};
use cpa_translator::Format;
use http::header::{ACCEPT, AUTHORIZATION, CONNECTION, CONTENT_TYPE, USER_AGENT};
use http::{HeaderMap, HeaderName, HeaderValue};

use super::replay::{ReplayScope, apply_reasoning_replay_cache};
use super::response::{
    normalize_codex_instructions, normalize_input_reasoning_items, sanitize_input_encrypted_content,
};
use super::tools::{
    ClientToolKey, MAX_TOOLS, NamespaceRefs, alias_client_web_search_function, alias_client_web_search_input,
    clamp_tools_limit, collect_client_declared_tool_keys, collect_namespace_tool_refs_with_fold,
    ensure_native_x_search_tool, has_client_web_search_function, normalize_forced_image_generation_tool_choice,
    normalize_forced_web_search_tool_choice, normalize_input_custom_tool_calls,
    normalize_input_namespace_tool_calls_with_fold, normalize_namespace_tool_choice_with_fold,
    normalize_tool_choice_for_tools, normalize_tools_with_fold, promote_additional_tools,
    prune_orphaned_tool_choice, request_has_native_x_search, resolve_client_web_search_alias,
    should_fold_namespace_tools, tool_choice_requires_hosted_tool_only_any,
};
use crate::helps::apply_patch_responses::{
    ApplyPatchResponsesState, normalize_apply_patch_responses_request_with_original,
};
use crate::helps::payload::{
    PayloadRequest, apply_payload_config, payload_request_path, payload_requested_model, set_bool_if_different,
    set_string_if_different,
};
use crate::helps::session::derived_session_uuid;
use crate::helps::thinking::{api_key_model_is_compat, apply_request_thinking};
use crate::openai_compat::translate::{claude_code_prompt_cache_id, source_handler_type, translate_request};

pub const IDENTIFIER: &str = "xai";
pub const IMAGE_HANDLER_TYPE: &str = "openai-image";
pub const VIDEO_HANDLER_TYPE: &str = "openai-video";
pub const IMAGES_GENERATIONS_PATH: &str = "/images/generations";
pub const IMAGES_EDITS_PATH: &str = "/images/edits";
pub const VIDEOS_GENERATIONS_PATH: &str = "/videos/generations";
pub const VIDEOS_EDITS_PATH: &str = "/videos/edits";
pub const VIDEOS_EXTENSIONS_PATH: &str = "/videos/extensions";
pub const VIDEOS_PATH: &str = "/videos";
pub const IDEMPOTENCY_KEY_META_KEY: &str = "idempotency_key";
const COMPOSER_MODEL_PREFIX: &str = "grok-composer-";
const TOKEN_AUTH_HEADER: &str = "X-XAI-Token-Auth";
const TOKEN_AUTH_VALUE: &str = "xai-grok-cli";
const CLIENT_VERSION_HEADER: &str = "x-grok-client-version";
/// Keep in sync with the Grok CLI version chat-proxy expects; older versions get HTTP 426.
pub const CLIENT_VERSION_VALUE: &str = "1.0.44";
const CLIENT_IDENTIFIER_HEADER: &str = "x-grok-client-identifier";
const CLIENT_IDENTIFIER_VALUE: &str = "grok-shell";
const AUTHENTICATE_RESPONSE_HEADER: &str = "x-authenticateresponse";
const AUTHENTICATE_RESPONSE_VALUE: &str = "authenticate-response";
const USING_API_ATTR: &str = "using_api";

/// A request ready to send plus what the response side needs to undo its rewrites.
pub struct PreparedRequest {
    pub apply_patch: ApplyPatchResponsesState,
    pub base_model: String,
    pub from: Format,
    pub response_format: Format,
    pub to: Format,
    pub original_payload: Vec<u8>,
    pub body: Vec<u8>,
    pub namespace_tools: NamespaceRefs,
    pub client_declared_tools: HashSet<ClientToolKey>,
    pub session_id: String,
    pub replay_scope: ReplayScope,
    pub filter_internal_x_search: bool,
    pub web_search_alias: String,
}

// ---------------------------------------------------------------- credentials and endpoints

/// Go: xaiMetadataString over auth metadata.
pub fn auth_metadata_string(auth: &Auth, key: &str) -> String {
    match auth.metadata.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(v)) => v.trim().to_string(),
        Some(other) => other.to_string().trim().to_string(),
    }
}

/// Go: xaiMetadataString over execution metadata.
pub fn metadata_string(metadata: &Metadata, key: &str) -> String {
    match metadata.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(v)) => v.trim().to_string(),
        Some(other) => other.to_string().trim().to_string(),
    }
}

/// Go: xaiCreds. `(token, base_url)`: attribute `api_key`, then metadata `access_token`.
pub fn creds(auth: Option<&Auth>) -> (String, String) {
    let Some(auth) = auth else { return (String::new(), String::new()) };
    let mut token = auth.attr("api_key");
    let mut base_url = auth.attr("base_url");
    if token.is_empty() {
        token = auth_metadata_string(auth, "access_token");
    }
    if base_url.is_empty() {
        base_url = auth_metadata_string(auth, "base_url");
    }
    (token, base_url)
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// Go: xaiUsingAPI. Whether HTTP chat and media use the official API (true) or the Grok CLI
/// chat proxy; OAuth credentials default to the proxy.
pub fn using_api(auth: Option<&Auth>) -> bool {
    let Some(auth) = auth else { return true };
    if let Some(raw) = auth.attributes.get(USING_API_ATTR).map(|v| v.trim())
        && !raw.is_empty()
        && let Some(parsed) = parse_bool(raw)
    {
        return parsed;
    }
    match auth.metadata.get(USING_API_ATTR) {
        Some(Value::Bool(v)) => return *v,
        Some(Value::String(v)) => {
            if let Some(parsed) = parse_bool(v.trim()) {
                return parsed;
            }
        }
        _ => {}
    }
    let raw = auth.attr("auth_kind");
    if !raw.is_empty() {
        return !raw.eq_ignore_ascii_case("oauth");
    }
    !auth_metadata_string(auth, "auth_kind").eq_ignore_ascii_case("oauth")
}

fn normalize_base_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

pub fn is_default_api_base_url(base_url: &str) -> bool {
    normalize_base_url(base_url) == normalize_base_url(DEFAULT_API_BASE_URL)
}

pub fn is_cli_chat_proxy_base_url(base_url: &str) -> bool {
    normalize_base_url(base_url) == normalize_base_url(CLI_CHAT_PROXY_BASE_URL)
}

/// Go: xaiChatBaseURL. Base URL of HTTP chat and media requests.
pub fn chat_base_url(auth: Option<&Auth>) -> String {
    let (_, base_url) = creds(auth);
    if using_api(auth) {
        return if base_url.is_empty() { DEFAULT_API_BASE_URL.to_string() } else { base_url };
    }
    if !base_url.is_empty() && !is_default_api_base_url(&base_url) {
        return base_url;
    }
    CLI_CHAT_PROXY_BASE_URL.to_string()
}

/// Go: xaiCompactBaseURL. Compact stays on the official API (chat-proxy answers 404, which
/// would cool the whole auth pool down).
pub fn compact_base_url(auth: Option<&Auth>) -> String {
    let (_, base_url) = creds(auth);
    if base_url.is_empty() || is_cli_chat_proxy_base_url(&base_url) {
        return DEFAULT_API_BASE_URL.to_string();
    }
    base_url
}

fn base_url_source(base_url: &str) -> &'static str {
    if is_default_api_base_url(base_url) {
        "DefaultAPIBaseURL"
    } else if is_cli_chat_proxy_base_url(base_url) {
        "CLIChatProxyBaseURL"
    } else {
        "custom"
    }
}

/// Go: logXAIResolvedBaseURL.
pub fn log_resolved_base_url(base_url: &str) {
    tracing::info!("xai: using base_url={base_url} source={}", base_url_source(base_url));
}

// ---------------------------------------------------------------- headers

fn header_value(v: &str) -> Result<HeaderValue, ExecError> {
    HeaderValue::from_str(v).map_err(|e| ExecError::new(0, format!("invalid header value: {e}")))
}

fn set_named(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), ExecError> {
    let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| ExecError::new(0, format!("invalid header name: {e}")))?;
    headers.insert(name, header_value(value)?);
    Ok(())
}

/// Go: applyXAIDefaultHeaders.
pub fn default_headers(token: &str, stream: bool, session_id: &str) -> Result<HeaderMap, ExecError> {
    let mut h = HeaderMap::new();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if !token.trim().is_empty() {
        h.insert(AUTHORIZATION, header_value(&format!("Bearer {token}"))?);
    }
    h.insert(ACCEPT, HeaderValue::from_static(if stream { "text/event-stream" } else { "application/json" }));
    h.insert(CONNECTION, HeaderValue::from_static("Keep-Alive"));
    // Go sets no User-Agent, so its transport sends the default one.
    h.insert(USER_AGENT, HeaderValue::from_static("Go-http-client/1.1"));
    if !session_id.is_empty() {
        set_named(&mut h, "x-grok-conv-id", session_id)?;
    }
    Ok(h)
}

/// Go: applyXAICustomHeaders.
pub fn apply_custom_headers(headers: &mut HeaderMap, auth: Option<&Auth>, opts: &Options, session: Option<&str>) {
    let attrs: HashMap<String, String> = auth
        .map(|a| a.attributes.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    cpa_core::util::apply_custom_headers_from_attrs(headers, &attrs, Some(&opts.headers), session);
}

/// Go: applyXAIHeaders (default headers plus custom `header:*` attributes).
pub fn apply_headers(
    auth: Option<&Auth>,
    token: &str,
    stream: bool,
    session_id: &str,
    opts: &Options,
    session: Option<&str>,
) -> Result<HeaderMap, ExecError> {
    let mut h = default_headers(token, stream, session_id)?;
    apply_custom_headers(&mut h, auth, opts, session);
    Ok(h)
}

/// Go: applyXAIChatHeaders. The CLI chat-proxy identity headers are attached only when
/// `using_api` is false and the resolved base URL is the chat proxy.
pub fn apply_chat_headers(
    auth: Option<&Auth>,
    token: &str,
    stream: bool,
    session_id: &str,
    opts: &Options,
    session: Option<&str>,
) -> Result<HeaderMap, ExecError> {
    if using_api(auth) {
        return apply_headers(auth, token, stream, session_id, opts, session);
    }
    let mut h = default_headers(token, stream, session_id)?;
    if is_cli_chat_proxy_base_url(&chat_base_url(auth)) {
        set_named(&mut h, TOKEN_AUTH_HEADER, TOKEN_AUTH_VALUE)?;
        set_named(&mut h, CLIENT_VERSION_HEADER, CLIENT_VERSION_VALUE)?;
        h.insert(USER_AGENT, header_value(&format!("xai-grok-workspace/{CLIENT_VERSION_VALUE}"))?);
        set_named(&mut h, CLIENT_IDENTIFIER_HEADER, CLIENT_IDENTIFIER_VALUE)?;
        set_named(&mut h, AUTHENTICATE_RESPONSE_HEADER, AUTHENTICATE_RESPONSE_VALUE)?;
    }
    apply_custom_headers(&mut h, auth, opts, session);
    Ok(h)
}

// ---------------------------------------------------------------- session

/// Go: xaiExecutionSessionID.
pub fn execution_session_id(req: &Request, opts: &Options) -> String {
    let value = metadata_string(&opts.metadata, meta::EXECUTION_SESSION_ID);
    if !value.is_empty() {
        return value;
    }
    let value = metadata_string(&req.metadata, meta::EXECUTION_SESSION_ID);
    if !value.is_empty() {
        return value;
    }
    let payload = cpa_json::parse(&req.payload);
    let prompt_cache_key = payload.g("prompt_cache_key");
    if prompt_cache_key.exists() {
        let value = prompt_cache_key.str().trim().to_string();
        if !value.is_empty() {
            return value;
        }
    }
    derived_session_uuid(IDENTIFIER, &[&opts.metadata, &req.metadata])
}

fn requires_isolated_conversation(model: &str) -> bool {
    model.trim().to_lowercase().starts_with(COMPOSER_MODEL_PREFIX)
}

/// Go: xaiResolveComposerSessionID. Composer models get an isolated conversation id.
fn resolve_composer_session_id(req: &Request, opts: &Options, base_model: &str) -> String {
    let session_id = execution_session_id(req, opts);
    if !session_id.is_empty() {
        return session_id;
    }
    if !requires_isolated_conversation(base_model) {
        return String::new();
    }
    match claude_code_prompt_cache_id(base_model, &req.payload, &opts.headers) {
        Some(id) => id,
        None => uuid::Uuid::new_v4().to_string(),
    }
}

// ---------------------------------------------------------------- endpoints by request kind

/// Go: xaiImageEndpointPath, "" when the request is not an image call.
pub fn image_endpoint_path(opts: &Options) -> &'static str {
    if source_handler_type(opts) != IMAGE_HANDLER_TYPE {
        return "";
    }
    let path = metadata_string(&opts.metadata, meta::REQUEST_PATH);
    if path.ends_with("/images/edits") { IMAGES_EDITS_PATH } else { IMAGES_GENERATIONS_PATH }
}

pub fn is_video_request(opts: &Options) -> bool {
    source_handler_type(opts) == VIDEO_HANDLER_TYPE
}

/// Go: xaiVideoEndpointPath, "" when the path names no creation endpoint.
pub fn video_endpoint_path(opts: &Options) -> &'static str {
    if !is_video_request(opts) {
        return "";
    }
    let path = metadata_string(&opts.metadata, meta::REQUEST_PATH);
    if path.ends_with("/videos/edits") {
        VIDEOS_EDITS_PATH
    } else if path.ends_with("/videos/extensions") {
        VIDEOS_EXTENSIONS_PATH
    } else if path.ends_with("/videos/generations") {
        VIDEOS_GENERATIONS_PATH
    } else {
        ""
    }
}

// ---------------------------------------------------------------- image refs

/// Go: normalizeXAIImageRefs. Rewrites `{"image":{"image_url":..}}` to xAI's `{"url":..}` for
/// `image`, `images` and `reference_images` anywhere in the tree. When anything changed the
/// output is re-encoded like Go's `json.Marshal` (sorted keys).
pub fn normalize_image_refs(body: &[u8]) -> Vec<u8> {
    if !cpa_json::valid(body) {
        return body.to_vec();
    }
    let mut payload = cpa_json::parse(body);
    if !normalize_image_refs_value(&mut payload) {
        return body.to_vec();
    }
    match cpa_core::util::go_json_sorted(&payload, cpa_core::util::GoJsonStyle::MARSHAL_USE_NUMBER) {
        Some(text) => text.into_bytes(),
        None => body.to_vec(),
    }
}

/// [`normalize_image_refs`] for a body already parsed.
pub fn normalize_image_refs_in_place(body: &mut Value) {
    if !body.is_object() && !body.is_array() {
        return;
    }
    let mut candidate = body.clone();
    if !normalize_image_refs_value(&mut candidate) {
        return;
    }
    if let Some(text) = cpa_core::util::go_json_sorted(&candidate, cpa_core::util::GoJsonStyle::MARSHAL_USE_NUMBER) {
        *body = cpa_json::parse_str(&text);
    }
}

fn normalize_image_refs_value(value: &mut Value) -> bool {
    let mut changed = false;
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    "image" => changed = normalize_image_ref(child) || changed,
                    "images" | "reference_images" => {
                        if let Value::Array(refs) = child {
                            for r in refs.iter_mut() {
                                changed = normalize_image_ref(r) || changed;
                            }
                        }
                    }
                    _ => {}
                }
                changed = normalize_image_refs_value(child) || changed;
            }
        }
        Value::Array(children) => {
            for child in children.iter_mut() {
                changed = normalize_image_refs_value(child) || changed;
            }
        }
        _ => {}
    }
    changed
}

fn normalize_image_ref(value: &mut Value) -> bool {
    let Value::Object(map) = value else { return false };
    let original_url = map.get("url").and_then(Value::as_str).unwrap_or_default().to_string();
    let mut url = original_url.trim().to_string();
    let has_image_url = map.contains_key("image_url");
    if url.is_empty() {
        match map.get("image_url") {
            Some(Value::String(v)) => url = v.trim().to_string(),
            Some(Value::Object(o)) => {
                url = o.get("url").and_then(Value::as_str).unwrap_or_default().trim().to_string();
            }
            _ => {}
        }
    }
    if url.is_empty() {
        return false;
    }
    if url == original_url && !has_image_url {
        return false;
    }
    // Always emit the xAI field name and drop the OpenAI alias.
    map.insert("url".into(), Value::String(url));
    map.shift_remove("image_url");
    true
}

// ---------------------------------------------------------------- output controls

/// Go: preserveXAIResponsesOutputControls. Copies the client's sampling and length limits onto
/// the translated body for OpenAI-style sources.
fn preserve_output_controls(body: &[u8], source: &[u8], from: Format) -> Vec<u8> {
    let src = cpa_json::parse(source);
    let max_output_tokens = match from {
        Format::OpenAI => {
            let v = src.g("max_completion_tokens");
            if !v.exists() || v.is_null() { src.g("max_tokens").value() } else { v.value() }
        }
        Format::OpenAIResponse => src.g("max_output_tokens").value(),
        _ => return body.to_vec(),
    };
    // `.value()` is Null for missing paths, so Null covers "absent or null".
    let src_max_present = !max_output_tokens.is_null();
    let mut out = cpa_json::parse(body);
    if src_max_present {
        cpa_json::set(&mut out, "max_output_tokens", max_output_tokens);
    }
    for field in ["temperature", "top_p", "top_k"] {
        let value = src.g(field).value();
        if !value.is_null() {
            cpa_json::set(&mut out, field, value);
        }
    }
    cpa_json::to_vec(&out)
}

// ---------------------------------------------------------------- pipeline

/// Go: prepareResponsesRequest (target format `codex`).
pub fn prepare_responses_request(
    cfg: &Config,
    req: &Request,
    opts: &Options,
    stream: bool,
) -> Result<PreparedRequest, ExecError> {
    prepare_responses_request_to(cfg, req, opts, stream, Format::Codex)
}

/// Go: prepareResponsesRequestTo. Translates the client request to the Responses dialect and
/// applies thinking, payload rules, apply_patch bridging, tool normalization, reasoning replay
/// and input sanitizing.
pub fn prepare_responses_request_to(
    cfg: &Config,
    req: &Request,
    opts: &Options,
    stream: bool,
    to: Format,
) -> Result<PreparedRequest, ExecError> {
    let base_model = parse_suffix(&req.model).model_name;
    let from = opts.source_format;
    let response_format = opts.response_format_or_source();
    let original_source: &[u8] = if opts.original_request.is_empty() { &req.payload } else { &opts.original_request };
    let original_payload = original_source.to_vec();
    let is_compat = api_key_model_is_compat(req);
    let (original_translated, _) =
        translate_request(&opts.headers, from, to, &base_model, &original_payload, stream, is_compat);
    let original_translated = preserve_output_controls(&original_translated, &original_payload, from);
    let (body, updates_changed) =
        translate_request(&opts.headers, from, to, &base_model, &req.payload, stream, is_compat);
    let body = preserve_output_controls(&body, &req.payload, from);

    let body = apply_request_thinking(&body, req, opts, from.as_str(), IDENTIFIER, IDENTIFIER, updates_changed)
        .map_err(|e| ExecError::new(e.status_code(), e.to_string()))?;

    let requested_model = payload_requested_model(opts, &req.model);
    let request_path = payload_request_path(opts);
    let body = apply_payload_config(
        &PayloadRequest {
            cfg: Some(cfg),
            target_executor: IDENTIFIER,
            model: &base_model,
            protocol: to.as_str(),
            from_protocol: from.as_str(),
            root: "",
            requested_model: &requested_model,
            request_path: &request_path,
            headers: Some(&opts.headers),
        },
        &body,
        &original_translated,
    );
    let mut body = cpa_json::parse(&body);
    set_string_if_different(&mut body, "model", &base_model);
    set_bool_if_different(&mut body, "stream", stream);
    for field in ["previous_response_id", "prompt_cache_retention", "safety_identifier", "stream_options"] {
        cpa_json::delete(&mut body, field);
    }
    let mut apply_patch = ApplyPatchResponsesState::new(from, &original_payload, &original_translated);
    let normalized =
        normalize_apply_patch_responses_request_with_original(&cpa_json::to_vec(&body), Some(&original_payload))
            .map_err(|e| ExecError::new(0, e))?;
    let mut body = cpa_json::parse(&normalized);

    let will_inject_x_search = cfg.xai.inject_x_search;
    let should_fold = should_fold_namespace_tools(&body, will_inject_x_search);
    let namespace_tools = collect_namespace_tool_refs_with_fold(&body, should_fold);
    for (name, r) in &namespace_tools {
        if r.is_dispatcher {
            apply_patch.add_dispatcher(name, &r.namespace);
        }
    }
    // Collected before normalization flattens namespace wrappers so keys match the
    // post-restore (namespace, short name) shape the response filter uses.
    let client_declared_tools = collect_client_declared_tool_keys(&body);
    normalize_tools_with_fold(&mut body, should_fold);
    promote_additional_tools(&mut body);
    let mut web_search_alias = String::new();
    if has_client_web_search_function(&body, &namespace_tools) {
        web_search_alias = resolve_client_web_search_alias(&body);
        alias_client_web_search_function(&mut body, &web_search_alias, &namespace_tools);
    }
    // Drop choices pointing at tools normalization removed before any x_search injection, so
    // no surviving choice references a deleted tool.
    normalize_namespace_tool_choice_with_fold(&mut body, should_fold);
    // Prune before rewriting hosted tool choices so older models that still strip the tool do
    // not keep a leftover "required" selection.
    prune_orphaned_tool_choice(&mut body);
    normalize_forced_web_search_tool_choice(&mut body);
    normalize_forced_image_generation_tool_choice(&mut body);
    normalize_tool_choice_for_tools(&mut body);
    // Skip x_search injection when the request is forced to a hosted tool and the remaining
    // tools are only that hosted tool.
    if cfg.xai.inject_x_search && !tool_choice_requires_hosted_tool_only_any(&body) {
        ensure_native_x_search_tool(&mut body);
    }
    clamp_tools_limit(&mut body, MAX_TOOLS, &namespace_tools);
    let replay_scope = apply_reasoning_replay_cache(from, req, opts, &mut body);
    normalize_input_custom_tool_calls(&mut body);
    normalize_input_namespace_tool_calls_with_fold(&mut body, should_fold);
    if !web_search_alias.is_empty() {
        alias_client_web_search_input(&mut body, &web_search_alias, &namespace_tools);
    }
    normalize_input_reasoning_items(&mut body);
    sanitize_input_encrypted_content(&mut body);
    normalize_codex_instructions(&mut body);
    // stop is supported by Chat Completions but not by xAI's Responses API. Thinking was handled
    // before payload overrides and must not be revalidated here.
    cpa_json::delete(&mut body, "stop");
    normalize_image_refs_in_place(&mut body);

    let session_id = resolve_composer_session_id(req, opts, &base_model);
    if !session_id.is_empty() {
        set_string_if_different(&mut body, "prompt_cache_key", &session_id);
    }
    let filter_internal_x_search = request_has_native_x_search(&body);
    Ok(PreparedRequest {
        apply_patch,
        base_model,
        from,
        response_format,
        to,
        original_payload,
        body: cpa_json::to_vec(&body),
        namespace_tools,
        client_declared_tools,
        session_id,
        replay_scope,
        filter_internal_x_search,
        web_search_alias,
    })
}
