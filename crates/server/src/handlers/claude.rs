//! Claude `POST /v1/messages` and `/v1/messages/count_tokens` (Go: claude/code_handlers.go).

use std::io::Read;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use cpa_core::format::Format;
use cpa_json::J;
use serde_json::Value;

use super::ok_reply;
use crate::error::{ErrorMessage, claude_error_body};
use crate::exec::{ExecArgs, Pipeline};
use crate::forward::{First, StreamHooks, claude_error_reply, empty_stream_reply, peek_first, start_sse_stream, with_nonstream_keepalive};
use cpa_runtime::service::resolve_claude_model_id_prefix;
use crate::req::ReqInfo;
use crate::state::AppState;

/// `rewriteClaudeDDModelInBody`: decodes cloaked `claude-fable-5-dd-<reversed>` model ids.
pub fn rewrite_claude_dd_model_in_body(raw: Bytes) -> Bytes {
    let mut root = cpa_json::parse(&raw);
    let model = root.g("model").str();
    let resolved = resolve_claude_model_id_prefix(&model);
    if resolved == model {
        return raw;
    }
    cpa_json::set(&mut root, "model", resolved);
    Bytes::from(cpa_json::to_vec(&root))
}

/// `POST /v1/messages`.
pub async fn messages(State(st): State<AppState>, info: ReqInfo, body: Bytes) -> Response {
    let raw = rewrite_claude_dd_model_in_body(body);
    let root = cpa_json::parse(&raw);
    // Streaming unless `stream` is absent or the JSON literal false.
    let stream = !matches!(root.g("stream").v(), None | Some(Value::Bool(false)));
    let model = root.g("model").str();
    if stream {
        stream_messages(&st, &info, &model, raw).await
    } else {
        nonstream_messages(&st, &info, &model, raw).await
    }
}

/// `POST /v1/messages/count_tokens`.
pub async fn count_tokens(State(st): State<AppState>, info: ReqInfo, body: Bytes) -> Response {
    let raw = rewrite_claude_dd_model_in_body(body);
    let model = cpa_json::parse(&raw).g("model").str();
    let alt = info.alt();
    let pipeline = Pipeline::new(&st, &info);
    let passthrough = pipeline.settings.passthrough_headers;
    match pipeline.execute_count(ExecArgs::new(Format::Claude, &model, raw, &alt)).await {
        Err(err) => claude_error_reply(&err, passthrough).into_response(),
        Ok(ok) => {
            let body = ok.body.clone();
            ok_reply(&info, ok, body).into_response()
        }
    }
}

/// Claude sometimes returns gzip without a `Content-Encoding` header; decompress it.
fn gunzip_if_needed(body: Bytes) -> Bytes {
    if body.len() >= 2 && body[0] == 0x1f && body[1] == 0x8b {
        let mut decoder = flate2::read::GzDecoder::new(&body[..]);
        let mut out = Vec::new();
        match decoder.read_to_end(&mut out) {
            Ok(_) => return Bytes::from(out),
            Err(e) => tracing::warn!("failed to decompress gzipped Claude response: {e}"),
        }
    }
    body
}

async fn nonstream_messages(st: &AppState, info: &ReqInfo, model: &str, raw: Bytes) -> Response {
    let pipeline = Pipeline::new(st, info);
    let interval = pipeline.settings.nonstream_keepalive;
    let passthrough = pipeline.settings.passthrough_headers;
    let alt = info.alt();
    let model = model.to_string();
    let info = info.clone();
    with_nonstream_keepalive(interval, async move {
        match pipeline.execute(ExecArgs::new(Format::Claude, &model, raw, &alt)).await {
            Err(err) => claude_error_reply(&err, passthrough),
            Ok(ok) => {
                let body = gunzip_if_needed(ok.body.clone());
                ok_reply(&info, ok, body)
            }
        }
    })
    .await
}

/// Raw passthrough of translator frames; the terminal error is an Anthropic `event: error`.
struct ClaudeHooks;

impl StreamHooks for ClaudeHooks {
    fn write_chunk(&mut self, out: &mut Vec<u8>, chunk: &[u8]) {
        out.extend_from_slice(chunk);
    }

    fn write_terminal_error(&mut self, out: &mut Vec<u8>, err: &ErrorMessage) {
        out.extend_from_slice(b"event: error\ndata: ");
        out.extend_from_slice(&claude_error_body(err));
        out.extend_from_slice(b"\n\n");
    }
}

async fn stream_messages(st: &AppState, info: &ReqInfo, model: &str, raw: Bytes) -> Response {
    let pipeline = Pipeline::new(st, info);
    let passthrough = pipeline.settings.passthrough_headers;
    let keepalive = pipeline.settings.stream_keepalive;
    // Streaming requests do not forward `alt` (Go passes an empty alt).
    let mut es = pipeline.execute_stream(ExecArgs::new(Format::Claude, model, raw, "")).await;
    match peek_first(&mut es).await {
        First::Error(err) => claude_error_reply(&err, passthrough).into_response(),
        First::Closed => empty_stream_reply(&es.headers, "", true).into_response(),
        First::Chunk(chunk) => {
            let mut initial = Vec::new();
            if !chunk.is_empty() {
                initial.extend_from_slice(&chunk);
            }
            start_sse_stream(HeaderMap::new(), &es.headers, initial, es.rx, ClaudeHooks, keepalive, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloaked_model_ids_are_rewritten_in_the_body() {
        let body = Bytes::from_static(br#"{"model":"claude-fable-5-dd-5-tpg(high)","max_tokens":1}"#);
        let out = rewrite_claude_dd_model_in_body(body);
        assert_eq!(&out[..], br#"{"model":"gpt-5(high)","max_tokens":1}"#);
        let plain = Bytes::from_static(br#"{ "model": "claude-opus" }"#);
        assert_eq!(rewrite_claude_dd_model_in_body(plain.clone()), plain);
    }

    #[test]
    fn gzip_bodies_are_unpacked() {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"{\"ok\":true}").unwrap();
        let gz = Bytes::from(enc.finish().unwrap());
        assert_eq!(&gunzip_if_needed(gz)[..], b"{\"ok\":true}");
        assert_eq!(&gunzip_if_needed(Bytes::from_static(b"{}"))[..], b"{}");
    }
}
