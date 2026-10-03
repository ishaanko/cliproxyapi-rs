//! Request pipeline shared by the Codex HTTP and websocket paths (Go: the body half of
//! codex_executor_execute.go, codex_executor_stream.go, codex_websockets_execute.go and
//! codex_websockets_stream.go `prepareCodexWebsocketStream`).
//!
//! Each path translates the client payload to the Codex dialect, applies thinking and payload
//! rules, normalizes tools and reasoning items, and finally derives the prompt cache identity.

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::{Config, DisableImageGenerationMode};
use cpa_core::thinking::{apply_summary_config_for_model, extract_translated_summary_config, parse_suffix};
use cpa_core::util::is_codex_responses_lite_request;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ExecError, Options, Request};
use cpa_translator::{Ctx, Format, RequestEnvelope};
use http::HeaderMap;
use uuid::Uuid;

use super::creds::{is_free_plan_auth, resolve_model_is_compat};
use super::reasoning::{ReplayScope, apply_replay_cache, prompt_cache_uuid_for_api_key};
use crate::helps::session::claude_code_execution_scope;
use super::terminal::status_error;
use super::{input_ids, multi_agent_v2, tool_schema};
use crate::helps::openai_responses_signature::sanitize_openai_responses_reasoning_encrypted_content_with_compat;
use crate::helps::payload::{PayloadRequest, apply_payload_config, payload_request_path, payload_requested_model};
use crate::helps::session::provider_session_uuid;
use crate::helps::thinking::apply_request_thinking;
use crate::helps::usage::reporter::META_CLIENT_API_KEY;

/// Which upstream call a body is prepared for. The paths differ in the fields they strip and in
/// the payload-rules executor id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Non-stream over HTTP: upstream is always streamed.
    Execute,
    /// Stream over HTTP.
    Stream,
    /// `/responses/compact`.
    Compact,
    /// Non-stream over the upstream websocket.
    WsExecute,
    /// Stream over the upstream websocket.
    WsStream,
}

impl Mode {
    fn is_ws(self) -> bool {
        matches!(self, Mode::WsExecute | Mode::WsStream)
    }

    fn translates_stream(self) -> bool {
        matches!(self, Mode::Stream | Mode::WsStream)
    }
}

/// Output of the shared pipeline.
#[derive(Debug)]
pub struct Prepared {
    pub from: Format,
    pub to: Format,
    pub response_format: Format,
    /// Client payload the response translators receive as the original request.
    pub original_payload: Bytes,
    /// Translated body, before the prompt cache identity is applied.
    pub body: Vec<u8>,
    pub native: bool,
    pub replay_scope: ReplayScope,
    pub optimize_multi_agent_v2: bool,
    pub multi_agent_v2_conflict: bool,
    pub base_model: String,
}

pub fn thinking_error(err: cpa_core::thinking::ThinkingError) -> ExecError {
    status_error(err.status_code(), err.to_string())
}

/// Native Codex client requests: Codex or Responses in and out, marked as Responses-Lite (Go:
/// IsNativeCodexRequest).
pub fn is_native_request(body: &[u8], opts: &Options) -> bool {
    let names = [opts.source_format, opts.response_format_or_source()];
    if !names.iter().all(|f| matches!(f, Format::Codex | Format::OpenAIResponse)) {
        return false;
    }
    is_codex_responses_lite_request(&cpa_json::parse(body), &opts.headers)
}

/// Translates one payload, honoring compat mode for Claude to Codex (Go: translateCodexRequestPair).
fn translate_one(from: Format, to: Format, model: &str, raw: &[u8], stream: bool, is_compat: bool) -> (Vec<u8>, bool) {
    if is_compat && from == Format::Claude && to == Format::Codex {
        let translated = cpa_translator::codex::claude::convert_claude_request_to_codex_with_compat(model, raw, stream);
        let summary = extract_translated_summary_config(raw, from.as_str(), to.as_str());
        return (apply_summary_config_for_model(translated, to.as_str(), model, &summary), false);
    }
    let env = cpa_translator::translate_request_envelope(
        &Ctx::default(),
        from,
        to,
        RequestEnvelope { model: model.to_string(), stream, body: raw.to_vec(), ..Default::default() },
    );
    (env.body, env.configuration_updates_changed)
}

/// `(translated original, translated payload, update intent)`; identical inputs translate once.
pub fn translate_pair(from: Format, to: Format, model: &str, original: &[u8], payload: &[u8], stream: bool, is_compat: bool) -> (Vec<u8>, Vec<u8>, bool) {
    if original == payload {
        let (body, changed) = translate_one(from, to, model, payload, stream, is_compat);
        return (body.clone(), body, changed);
    }
    let (original_translated, _) = translate_one(from, to, model, original, stream, is_compat);
    let (body, changed) = translate_one(from, to, model, payload, stream, is_compat);
    (original_translated, body, changed)
}

/// Runs `edit` on the parsed body and re-serializes only when it reports a change.
fn edit(body: Vec<u8>, f: impl FnOnce(&mut Value) -> bool) -> Vec<u8> {
    let mut value = cpa_json::parse(&body);
    if f(&mut value) { cpa_json::to_vec(&value) } else { body }
}

fn set_string_if_different(v: &mut Value, path: &str, value: &str) -> bool {
    if v.g(path).as_str() == Some(value) {
        return false;
    }
    cpa_json::set(v, path, value)
}

fn set_bool_if_different(v: &mut Value, path: &str, value: bool) -> bool {
    if v.g(path).v() == Some(&Value::Bool(value)) {
        return false;
    }
    cpa_json::set(v, path, value)
}

fn delete_if_present(v: &mut Value, path: &str) -> bool {
    if !v.g(path).exists() {
        return false;
    }
    cpa_json::delete(v, path);
    true
}

/// Missing or null `instructions` become `""` unless the request is a native Codex request (Go:
/// normalizeCodexInstructions).
fn normalize_instructions(v: &mut Value, native: bool) -> bool {
    if native {
        return false;
    }
    let instructions = v.g("instructions");
    if !instructions.exists() || instructions.is_null() {
        return cpa_json::set(v, "instructions", "");
    }
    false
}

fn is_image_generation_function_tool(tool: &Value) -> bool {
    match tool.g("type").str().as_str() {
        "function" => tool.g("name").str() == "image_gen.imagegen",
        "namespace" => {
            if tool.g("name").str() != "image_gen" {
                return false;
            }
            let tools = tool.g("tools");
            tools.is_array() && tools.array().iter().any(|t| t.g("type").str() == "function" && t.g("name").str() == "imagegen")
        }
        _ => false,
    }
}

/// Appends the hosted image generation tool unless the request, model or plan rules it out (Go:
/// ensureImageGenerationTool).
pub fn ensure_image_generation_tool(v: &mut Value, base_model: &str, auth: &Auth, headers: &HeaderMap) -> bool {
    if is_codex_responses_lite_request(v, headers) || base_model.ends_with("spark") || is_free_plan_auth(auth) {
        return false;
    }
    let tool = serde_json::json!({"type": "image_generation", "output_format": "png"});
    let tools = v.g("tools");
    if !tools.is_array() {
        return cpa_json::set(v, "tools", Value::Array(vec![tool]));
    }
    let present = tools.array().iter().any(|t| t.g("type").str() == "image_generation" || is_image_generation_function_tool(&t.value()));
    if present {
        return false;
    }
    cpa_json::set(v, "tools.-1", tool)
}

/// Lite requests force `parallel_tool_calls=false`; HTTP paths also drop the flag when there are
/// no tools (Go: normalizeCodexParallelToolCalls / normalizeCodexWebsocketParallelToolCalls).
fn normalize_parallel_tool_calls(v: &mut Value, headers: &HeaderMap, ws: bool) -> bool {
    if is_codex_responses_lite_request(v, headers) {
        return set_bool_if_different(v, "parallel_tool_calls", false);
    }
    if ws || !v.g("parallel_tool_calls").exists() {
        return false;
    }
    let tools = v.g("tools");
    if tools.is_array() && !tools.array().is_empty() {
        return false;
    }
    cpa_json::delete(v, "parallel_tool_calls");
    true
}

/// Shared request pipeline (see module docs).
pub fn prepare(cfg: &Config, auth: &Auth, req: &Request, opts: &Options, mode: Mode) -> Result<Prepared, ExecError> {
    let base_model = parse_suffix(&req.model).model_name;
    let from = opts.source_format;
    let response_format = opts.response_format_or_source();
    let to = if mode == Mode::Compact { Format::OpenAIResponse } else { Format::Codex };
    let original_payload: Bytes = if opts.original_request.is_empty() { req.payload.clone() } else { opts.original_request.clone() };
    let is_compat = resolve_model_is_compat(cfg, auth, req, &base_model);
    let native = is_native_request(&req.payload, opts);

    let (original_translated, body, updates_changed) =
        translate_pair(from, to, &base_model, &original_payload, &req.payload, mode.translates_stream(), is_compat);
    let body = apply_request_thinking(&body, req, opts, from.as_str(), to.as_str(), "codex", updates_changed).map_err(thinking_error)?;

    let requested_model = payload_requested_model(opts, &req.model);
    let request_path = payload_request_path(opts);
    let payload_request = PayloadRequest {
        cfg: Some(cfg),
        target_executor: "codex",
        model: &base_model,
        protocol: to.as_str(),
        from_protocol: from.as_str(),
        root: "",
        requested_model: &requested_model,
        request_path: &request_path,
        headers: Some(&opts.headers),
    };
    // The websocket rules are keyed by their own executor id.
    let rules_executor = if mode.is_ws() { "codex-websockets" } else { "codex" };
    let payload_request = PayloadRequest { target_executor: rules_executor, ..payload_request };
    let body = apply_payload_config(&payload_request, &body, &original_translated);

    let image_tool_enabled = cfg.disable_image_generation == DisableImageGenerationMode::Off;
    let body = edit(body, |v| {
        let mut changed = false;
        match mode {
            Mode::Execute => {
                changed |= set_string_if_different(v, "model", &base_model);
                changed |= set_bool_if_different(v, "stream", true);
                for path in ["previous_response_id", "generate", "prompt_cache_retention", "safety_identifier", "stream_options"] {
                    changed |= delete_if_present(v, path);
                }
            }
            Mode::Stream => {
                for path in ["previous_response_id", "generate", "prompt_cache_retention", "safety_identifier"] {
                    changed |= delete_if_present(v, path);
                }
                // Only the summary delivery option of stream_options survives, and only here.
                let summary_delivery = v.g("stream_options.reasoning_summary_delivery");
                let summary_delivery = summary_delivery.exists().then(|| summary_delivery.value());
                changed |= delete_if_present(v, "stream_options");
                if let Some(value) = summary_delivery {
                    changed |= cpa_json::set(v, "stream_options.reasoning_summary_delivery", value);
                }
                changed |= set_string_if_different(v, "model", &base_model);
            }
            Mode::Compact => {
                changed |= set_string_if_different(v, "model", &base_model);
                changed |= delete_if_present(v, "stream");
            }
            Mode::WsExecute => {
                changed |= set_string_if_different(v, "model", &base_model);
                changed |= set_bool_if_different(v, "stream", true);
                for path in ["prompt_cache_retention", "safety_identifier"] {
                    changed |= delete_if_present(v, path);
                }
            }
            Mode::WsStream => {
                changed |= set_string_if_different(v, "model", &base_model);
            }
        }
        changed |= normalize_instructions(v, native);
        if mode != Mode::Compact && image_tool_enabled {
            changed |= ensure_image_generation_tool(v, &base_model, auth, &opts.headers);
        }
        changed
    });
    let label = if mode.is_ws() { "codex websockets executor" } else { "codex executor" };
    let body = sanitize_openai_responses_reasoning_encrypted_content_with_compat(label, &body, is_compat);
    let body = edit(body, |v| normalize_parallel_tool_calls(v, &opts.headers, mode.is_ws()));
    let body = tool_schema::normalize_codex_tool_schemas(&body);
    let multi_agent_v2_conflict = mode.is_ws() && multi_agent_v2::has_namespace_conflict(&body);
    let (body, optimize_multi_agent_v2) = multi_agent_v2::optimize_request_for_auth(&opts.headers, &body, Some(cfg), Some(auth), is_compat);
    let (body, replay_scope) = if mode == Mode::Compact {
        (body, ReplayScope::default())
    } else {
        apply_replay_cache(from, req, opts, body)?
    };
    Ok(Prepared {
        from,
        to,
        response_format,
        original_payload,
        body,
        native,
        replay_scope,
        optimize_multi_agent_v2,
        multi_agent_v2_conflict,
        base_model,
    })
}

/// The `prompt_cache_key` / `Session-Id` identity for the request (Go: the cache selection of
/// cacheHelper). `openai_chat_api_key_fallback` is true on the HTTP path only: the websocket path
/// does not derive the identity from the client API key.
pub fn prompt_cache_id(from: Format, req: &Request, opts: &Options, body: &[u8], openai_chat_api_key_fallback: bool) -> String {
    let mut id = String::new();
    if from == Format::Claude {
        let mut model_name = cpa_json::parse(body).g("model").str().trim().to_string();
        if model_name.is_empty() {
            model_name = parse_suffix(&req.model).model_name;
        }
        let model_name = model_name.trim();
        if !model_name.is_empty()
            && let Some(scope) = claude_code_execution_scope(&req.payload, &opts.headers)
        {
            let identity = ["cli-proxy-api:codex:claude-code", model_name, scope.as_str()].join("\0");
            id = Uuid::new_v5(&Uuid::NAMESPACE_OID, identity.as_bytes()).to_string();
        }
    } else if from == Format::OpenAIResponse {
        let payload = cpa_json::parse(&req.payload);
        let key = payload.g("prompt_cache_key");
        if key.exists() {
            id = key.str();
        }
    } else if from == Format::OpenAI && openai_chat_api_key_fallback {
        let payload = cpa_json::parse(&req.payload);
        let key = payload.g("prompt_cache_key");
        if key.exists() {
            id = key.str().trim().to_string();
        }
        if id.is_empty() {
            id = provider_session_uuid("codex", &[&req.metadata, &opts.metadata]);
        }
        if id.is_empty() {
            let api_key = opts.metadata.get(META_CLIENT_API_KEY).and_then(Value::as_str).unwrap_or_default().trim();
            if !api_key.is_empty() {
                id = prompt_cache_uuid_for_api_key(api_key);
            }
        }
    }
    if id.is_empty() {
        id = provider_session_uuid("codex", &[&req.metadata, &opts.metadata]);
    }
    id
}

/// Sets `prompt_cache_key` and sanitizes input item ids (the body half of cacheHelper).
pub fn apply_prompt_cache_and_ids(body: Vec<u8>, cache_id: &str) -> Vec<u8> {
    let body = if cache_id.is_empty() { body } else { edit(body, |v| set_string_if_different(v, "prompt_cache_key", cache_id)) };
    input_ids::sanitize_codex_input_item_ids(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> Auth {
        let mut a = Auth::new("a", "codex");
        a.metadata.insert("access_token".into(), "tok".into());
        a
    }

    #[test]
    fn image_generation_tool_is_added_once_and_skipped_for_free_and_spark() {
        let headers = HeaderMap::new();
        let mut v = cpa_json::parse(br#"{"tools":[{"type":"function","name":"f"}]}"#);
        assert!(ensure_image_generation_tool(&mut v, "gpt-5", &auth(), &headers));
        assert_eq!(v.g("tools").array().len(), 2);
        assert!(!ensure_image_generation_tool(&mut v, "gpt-5", &auth(), &headers));
        let mut none = cpa_json::parse(b"{}");
        assert!(!ensure_image_generation_tool(&mut none, "gpt-5-codex-spark", &auth(), &headers));
        let mut free = auth();
        free.attributes.insert("plan_type".into(), "free".into());
        assert!(!ensure_image_generation_tool(&mut none, "gpt-5", &free, &headers));
        assert!(ensure_image_generation_tool(&mut none, "gpt-5", &auth(), &headers));
        assert_eq!(none.g("tools.0.type").str(), "image_generation");
    }

    #[test]
    fn parallel_tool_calls_dropped_without_tools_and_forced_off_for_lite() {
        let headers = HeaderMap::new();
        let mut v = cpa_json::parse(br#"{"parallel_tool_calls":true,"tools":[]}"#);
        assert!(normalize_parallel_tool_calls(&mut v, &headers, false));
        assert!(!v.g("parallel_tool_calls").exists());
        let mut ws = cpa_json::parse(br#"{"parallel_tool_calls":true}"#);
        assert!(!normalize_parallel_tool_calls(&mut ws, &headers, true));
        let mut lite = cpa_json::parse(br#"{"parallel_tool_calls":true,"client_metadata":{"ws_request_header_x_openai_internal_codex_responses_lite":true}}"#);
        assert!(normalize_parallel_tool_calls(&mut lite, &headers, true));
        assert!(!lite.g("parallel_tool_calls").bool());
    }

    #[test]
    fn instructions_default_to_empty_unless_native() {
        let mut v = cpa_json::parse(br#"{"instructions":null}"#);
        assert!(normalize_instructions(&mut v, false));
        assert_eq!(v.g("instructions").as_str(), Some(""));
        let mut native = cpa_json::parse(b"{}");
        assert!(!normalize_instructions(&mut native, true));
    }

    #[test]
    fn prompt_cache_id_follows_source_format() {
        let mut req = Request { model: "gpt-5".into(), payload: Bytes::from_static(br#"{"prompt_cache_key":" k1 "}"#), format: Format::OpenAIResponse, metadata: Default::default() };
        let opts = Options::new(Format::OpenAIResponse);
        assert_eq!(prompt_cache_id(Format::OpenAIResponse, &req, &opts, b"{}", true), " k1 ");
        req.format = Format::OpenAI;
        assert_eq!(prompt_cache_id(Format::OpenAI, &req, &opts, b"{}", true), "k1");
        let bare = Request { payload: Bytes::from_static(b"{}"), ..req.clone() };
        assert_eq!(prompt_cache_id(Format::OpenAI, &bare, &opts, b"{}", true), "");
    }
}
