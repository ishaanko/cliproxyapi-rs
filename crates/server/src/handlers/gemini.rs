//! Gemini `/v1beta/models/*action` and `/v1beta/interactions` (Go: gemini/gemini_handlers.go,
//! gemini/interactions_handlers.go).

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use cpa_core::format::{Format, constant};
use cpa_json::J;
use serde_json::Value;

use super::{bad_request_message, ok_reply};
use crate::error::{ErrorMessage, build_error_response_body, error_response_json, status_text};
use crate::exec::{ExecArgs, Pipeline};
use crate::forward::{First, StreamHooks, empty_stream_reply, openai_error_reply, peek_first, start_sse_stream, with_nonstream_keepalive};
use crate::models;
use crate::reply::Reply;
use crate::req::ReqInfo;
use crate::state::AppState;

/// `GET /v1beta/models`.
pub async fn list_models() -> Response {
    models::gemini_models().into_response()
}

/// `GET /v1beta/models/*action`: single model lookup.
pub async fn get_model(Path(action): Path<String>) -> Response {
    models::gemini_get_model(&action).into_response()
}

/// `GET /v1beta/models/` (gin's wildcard also matches the empty action).
pub async fn get_model_root() -> Response {
    models::gemini_get_model("/").into_response()
}

/// `POST /v1beta/models/` (empty action).
pub async fn post_action_root(info: ReqInfo) -> Response {
    Reply::json(
        404,
        error_response_json(&format!("{} not found.", info.path), "invalid_request_error"),
    )
    .into_response()
}

/// `POST /v1beta/models/*action`: `<model>:generateContent|streamGenerateContent|countTokens`.
pub async fn post_action(State(st): State<AppState>, info: ReqInfo, Path(action): Path<String>, body: Bytes) -> Response {
    let action = action.strip_prefix('/').unwrap_or(&action);
    let parts: Vec<&str> = action.split(':').collect();
    if parts.len() != 2 {
        return Reply::json(
            404,
            error_response_json(&format!("{} not found.", info.path), "invalid_request_error"),
        )
        .into_response();
    }
    let (model, method) = (parts[0].to_string(), parts[1]);
    match method {
        "generateContent" => generate_content(&st, &info, &model, body).await,
        "streamGenerateContent" => stream_generate_content(&st, &info, &model, body).await,
        "countTokens" => count_tokens(&st, &info, &model, body).await,
        // Unknown methods are silently accepted with an empty 200.
        _ => Reply::new(200).into_response(),
    }
}

async fn generate_content(st: &AppState, info: &ReqInfo, model: &str, body: Bytes) -> Response {
    let pipeline = Pipeline::new(st, info);
    let interval = pipeline.settings.nonstream_keepalive;
    let passthrough = pipeline.settings.passthrough_headers;
    let alt = info.alt();
    let model = model.to_string();
    with_nonstream_keepalive(interval, async move {
        match pipeline.execute(ExecArgs::new(Format::Gemini, &model, body, &alt)).await {
            Err(err) => openai_error_reply(&err, passthrough),
            Ok(ok) => {
                let b = ok.body.clone();
                ok_reply(ok, b)
            }
        }
    })
    .await
}

async fn count_tokens(st: &AppState, info: &ReqInfo, model: &str, body: Bytes) -> Response {
    let pipeline = Pipeline::new(st, info);
    let passthrough = pipeline.settings.passthrough_headers;
    let alt = info.alt();
    match pipeline.execute_count(ExecArgs::new(Format::Gemini, model, body, &alt)).await {
        Err(err) => openai_error_reply(&err, passthrough).into_response(),
        Ok(ok) => {
            let b = ok.body.clone();
            ok_reply(ok, b).into_response()
        }
    }
}

/// Error body for terminal stream failures (`BuildErrorResponseBody`).
fn terminal_error_body(err: &ErrorMessage) -> Vec<u8> {
    let status = err.status_or_500();
    let text = if err.text.is_empty() {
        status_text(status).to_string()
    } else {
        err.text.clone()
    };
    build_error_response_body(status, &text)
}

/// SSE `data:` framing without `alt`; raw JSON chunks (and raw error JSON) with `alt`.
struct GeminiHooks {
    alt: bool,
}

impl StreamHooks for GeminiHooks {
    fn write_chunk(&mut self, out: &mut Vec<u8>, chunk: &[u8]) {
        if self.alt {
            out.extend_from_slice(chunk);
        } else {
            out.extend_from_slice(b"data: ");
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\n\n");
        }
    }

    fn write_terminal_error(&mut self, out: &mut Vec<u8>, err: &ErrorMessage) {
        let body = terminal_error_body(err);
        if self.alt {
            out.extend_from_slice(&body);
        } else {
            out.extend_from_slice(b"event: error\ndata: ");
            out.extend_from_slice(&body);
            out.extend_from_slice(b"\n\n");
        }
    }
}

async fn stream_generate_content(st: &AppState, info: &ReqInfo, model: &str, body: Bytes) -> Response {
    let alt = info.alt();
    let pipeline = Pipeline::new(st, info);
    let passthrough = pipeline.settings.passthrough_headers;
    // A non-empty `alt` serves a raw JSON stream and turns the SSE comment keepalive off.
    let keepalive = if alt.is_empty() { pipeline.settings.stream_keepalive } else { Duration::ZERO };
    let set_sse = alt.is_empty();
    let mut es = pipeline.execute_stream(ExecArgs::new(Format::Gemini, model, body, &alt)).await;
    match peek_first(&mut es).await {
        First::Error(err) => openai_error_reply(&err, passthrough).into_response(),
        First::Closed => empty_stream_reply(&es.headers, "", set_sse).into_response(),
        First::Chunk(chunk) => {
            let mut hooks = GeminiHooks { alt: !alt.is_empty() };
            let mut initial = Vec::new();
            hooks.write_chunk(&mut initial, &chunk);
            start_sse_stream(HeaderMap::new(), &es.headers, initial, es.rx, hooks, keepalive, set_sse)
        }
    }
}

// ------------------------------------------------------------------ interactions

const INTERACTIONS_AGENT_AUTH_SELECTION_MODEL: &str = "gemini-2.5-flash";

#[derive(Debug, PartialEq, Eq)]
pub struct InteractionsTarget {
    pub model: String,
    pub agent: String,
    pub stream: bool,
}

/// `parseInteractionsRequestTarget`.
pub fn parse_interactions_target(raw: &[u8]) -> Result<InteractionsTarget, &'static str> {
    if !cpa_json::valid(raw) {
        return Err("invalid JSON body");
    }
    let root = cpa_json::parse(raw);
    let model = root.g("model").str().trim().to_string();
    let agent = root.g("agent").str().trim().to_string();
    if model.is_empty() == agent.is_empty() {
        return Err("request requires exactly one of model or agent");
    }
    let node = root.g("stream");
    let stream = if node.exists() {
        match node.v() {
            Some(Value::Bool(b)) => *b,
            _ => return Err("stream must be a boolean"),
        }
    } else {
        false
    };
    Ok(InteractionsTarget { model, agent, stream })
}

/// `normalizeGeminiModelResourceName`.
fn normalize_gemini_model_resource_name(model: &str) -> String {
    let model = model.trim();
    match model.strip_prefix("models/") {
        Some(rest) if !rest.is_empty() => rest.to_string(),
        _ => model.to_string(),
    }
}

/// `prepareInteractionsExecutionTarget`: the routing model and the (possibly rewritten) body.
fn prepare_interactions_target(raw: Bytes, target: &InteractionsTarget) -> (String, Bytes) {
    if !target.agent.is_empty() {
        return (target.agent.clone(), raw);
    }
    let model = normalize_gemini_model_resource_name(&target.model);
    if model == target.model {
        return (model, raw);
    }
    let mut root = cpa_json::parse(&raw);
    cpa_json::set(&mut root, "model", model.clone());
    (model, Bytes::from(cpa_json::to_vec(&root)))
}

struct InteractionsHooks;

impl StreamHooks for InteractionsHooks {
    fn write_chunk(&mut self, out: &mut Vec<u8>, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        let trimmed = chunk.trim_ascii();
        if trimmed.starts_with(b"event:") || trimmed.starts_with(b"data:") {
            out.extend_from_slice(chunk);
        } else {
            out.extend_from_slice(b"data: ");
            out.extend_from_slice(chunk);
        }
        if !chunk.ends_with(b"\n\n") {
            out.extend_from_slice(b"\n\n");
        }
    }

    fn write_terminal_error(&mut self, out: &mut Vec<u8>, err: &ErrorMessage) {
        out.extend_from_slice(b"event: error\ndata: ");
        out.extend_from_slice(&terminal_error_body(err));
        out.extend_from_slice(b"\n\n");
    }
}

/// `POST /v1beta/interactions`.
pub async fn interactions(State(st): State<AppState>, info: ReqInfo, body: Bytes) -> Response {
    let target = match parse_interactions_target(&body) {
        Ok(t) => t,
        Err(msg) => return bad_request_message(msg).into_response(),
    };
    let (model, raw) = prepare_interactions_target(body, &target);
    let alt = info.alt();
    let pipeline = Pipeline::new(&st, &info);
    let passthrough = pipeline.settings.passthrough_headers;
    let (forced, selection_model) = if target.agent.is_empty() {
        (None, None)
    } else {
        (Some(constant::GEMINI_INTERACTIONS), Some(INTERACTIONS_AGENT_AUTH_SELECTION_MODEL))
    };

    if !target.stream {
        let interval = pipeline.settings.nonstream_keepalive;
        return with_nonstream_keepalive(interval, async move {
            let mut args = ExecArgs::new(Format::Interactions, &model, raw, &alt);
            args.forced_provider = forced;
            args.auth_selection_model = selection_model;
            match pipeline.execute(args).await {
                Err(err) => openai_error_reply(&err, passthrough),
                Ok(ok) => {
                    let b = ok.body.clone();
                    ok_reply(ok, b)
                }
            }
        })
        .await;
    }

    let keepalive = pipeline.settings.stream_keepalive;
    let mut args = ExecArgs::new(Format::Interactions, &model, raw, &alt);
    args.forced_provider = forced;
    args.auth_selection_model = selection_model;
    let mut es = pipeline.execute_stream(args).await;
    match peek_first(&mut es).await {
        First::Error(err) => openai_error_reply(&err, passthrough).into_response(),
        First::Closed => empty_stream_reply(&es.headers, "", true).into_response(),
        First::Chunk(chunk) => {
            let mut hooks = InteractionsHooks;
            let mut initial = Vec::new();
            hooks.write_chunk(&mut initial, &chunk);
            start_sse_stream(HeaderMap::new(), &es.headers, initial, es.rx, hooks, keepalive, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactions_target_validation() {
        let ok = parse_interactions_target(br#"{"model":" models/gemini-2.5-pro "}"#).unwrap();
        assert_eq!(ok, InteractionsTarget { model: "models/gemini-2.5-pro".into(), agent: String::new(), stream: false });
        assert_eq!(parse_interactions_target(b"{").unwrap_err(), "invalid JSON body");
        assert_eq!(
            parse_interactions_target(br#"{"model":"a","agent":"b"}"#).unwrap_err(),
            "request requires exactly one of model or agent"
        );
        assert_eq!(parse_interactions_target(b"{}").unwrap_err(), "request requires exactly one of model or agent");
        assert_eq!(
            parse_interactions_target(br#"{"model":"a","stream":"yes"}"#).unwrap_err(),
            "stream must be a boolean"
        );
        assert!(parse_interactions_target(br#"{"agent":"deep","stream":true}"#).unwrap().stream);
    }

    #[test]
    fn models_prefix_is_stripped_from_the_body() {
        let target = parse_interactions_target(br#"{"model":"models/gemini-x","input":"hi"}"#).unwrap();
        let (model, body) = prepare_interactions_target(Bytes::from_static(br#"{"model":"models/gemini-x","input":"hi"}"#), &target);
        assert_eq!(model, "gemini-x");
        assert_eq!(&body[..], br#"{"model":"gemini-x","input":"hi"}"#);
    }

    #[test]
    fn interactions_framing() {
        let mut hooks = InteractionsHooks;
        let mut out = Vec::new();
        hooks.write_chunk(&mut out, b"{\"a\":1}");
        hooks.write_chunk(&mut out, b"event: x\ndata: {}\n\n");
        hooks.write_chunk(&mut out, b"data: {}\n");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "data: {\"a\":1}\n\nevent: x\ndata: {}\n\ndata: {}\n\n\n"
        );
    }

    #[test]
    fn gemini_alt_framing() {
        let mut sse = GeminiHooks { alt: false };
        let mut raw = GeminiHooks { alt: true };
        let (mut a, mut b) = (Vec::new(), Vec::new());
        sse.write_chunk(&mut a, b"{}");
        raw.write_chunk(&mut b, b"{}");
        assert_eq!(a, b"data: {}\n\n");
        assert_eq!(b, b"{}");
        let err = ErrorMessage::new(500, "boom");
        let (mut c, mut d) = (Vec::new(), Vec::new());
        sse.write_terminal_error(&mut c, &err);
        raw.write_terminal_error(&mut d, &err);
        assert!(String::from_utf8(c).unwrap().starts_with("event: error\ndata: {\"error\""));
        assert!(String::from_utf8(d).unwrap().starts_with("{\"error\""));
    }
}
