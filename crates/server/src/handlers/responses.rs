//! OpenAI Responses over HTTP: `POST /v1/responses` (SSE or JSON) and `/v1/responses/compact`
//! (Go: openai/openai_responses_handlers.go).
//!
//! `client.codex.optimize-multi-agent-v2` tool rewriting and the Codex orphan-delegation input
//! rewrite (internal/client/codex/optimize-multi-agent-v2) are not ported; both are opt-in
//! config features that leave the payload untouched when disabled.

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::Bytes;
use cpa_core::format::Format;
use cpa_json::J;
use serde_json::Value;
use tokio::sync::mpsc;

use super::{ok_reply, read_request_body};
use crate::bodyview::Want;
use crate::error::{ErrorMessage, error_response_json};
use crate::exec::{ExecArgs, ExecRx, Pipeline};
use crate::forward::{StreamHooks, openai_error_reply, start_sse_stream, with_nonstream_keepalive};
use crate::reply::Reply;
use crate::req::ReqInfo;
use crate::reqlog::ApiLog;
use crate::responses_error::{build_error_chunk, build_failed_chunk, sanitize_error_message, stream_error_text};
use crate::responses_framer::{ResponsesSseFramer, is_codex_responses_client};
use crate::state::AppState;

/// `POST /v1/responses` (also `/backend-api/codex/responses`).
pub async fn responses(State(st): State<AppState>, info: ReqInfo, body: Bytes) -> Response {
    let raw = match read_request_body(&info, body) {
        Ok(b) => b,
        Err(reply) => return reply.into_response(),
    };
    let root = crate::bodyview::fields_or_parse(&raw, &[("model", Want::Value), ("stream", Want::Value)]);
    let model = root.g("model").str();
    if matches!(root.g("stream").v(), Some(Value::Bool(true))) {
        stream_responses(&st, &info, &model, raw).await
    } else {
        nonstream(&st, &info, &model, raw, "").await
    }
}

/// `POST /v1/responses/compact`: non-stream only, executed with the internal alt
/// `responses/compact`.
pub async fn compact(State(st): State<AppState>, info: ReqInfo, body: Bytes) -> Response {
    let raw = match read_request_body(&info, body) {
        Ok(b) => b,
        Err(reply) => return reply.into_response(),
    };
    let mut root = cpa_json::parse(&raw);
    let stream = root.g("stream");
    if matches!(stream.v(), Some(Value::Bool(true))) {
        return Reply::json(
            400,
            error_response_json("Streaming not supported for compact responses", "invalid_request_error"),
        )
        .into_response();
    }
    let raw = if stream.exists() {
        cpa_json::delete(&mut root, "stream");
        Bytes::from(cpa_json::to_vec(&root))
    } else {
        raw
    };
    let model = root.g("model").str();
    nonstream(&st, &info, &model, raw, "responses/compact").await
}

async fn nonstream(st: &AppState, info: &ReqInfo, model: &str, raw: Bytes, alt: &str) -> Response {
    let pipeline = Pipeline::new(st, info);
    let interval = pipeline.settings.nonstream_keepalive;
    let passthrough = pipeline.settings.passthrough_headers;
    let model = model.to_string();
    let alt = alt.to_string();
    with_nonstream_keepalive(interval, async move {
        match pipeline.execute(ExecArgs::new(Format::OpenAIResponse, &model, raw, &alt)).await {
            Err(err) => openai_error_reply(&err, passthrough),
            Ok(ok) => {
                let b = ok.body.clone();
                ok_reply(ok, b)
            }
        }
    })
    .await
}

/// Writers for the Responses SSE stream: every chunk goes through the framer; mid-stream errors
/// become `error` / `response.failed` events preceded by a blank line; a clean close without a
/// terminal event becomes a 502 error event; a clean close with one ends with a lone `\n`.
struct ResponsesHooks {
    framer: ResponsesSseFramer,
    is_codex: bool,
    api_log: Arc<ApiLog>,
}

/// `logResponsesStreamError`: records `responses stream terminated after <lastEvent>: <text>`.
fn log_stream_error(api_log: &ApiLog, framer: &ResponsesSseFramer, err: &ErrorMessage) {
    let status = match err.status_or_500() {
        s @ 400..=599 => s,
        _ => 500,
    };
    let last = if framer.last_event.is_empty() { "none" } else { &framer.last_event };
    let text = stream_error_text(Some(err), status);
    api_log.record_error(status, &format!("responses stream terminated after {last}: {text}"));
}

impl StreamHooks for ResponsesHooks {
    fn write_chunk(&mut self, out: &mut Vec<u8>, chunk: &[u8]) {
        self.framer.write_chunk(out, chunk);
    }

    fn chunk_error(&mut self) -> Option<ErrorMessage> {
        let err = self.framer.terminal_error.clone()?;
        log_stream_error(&self.api_log, &self.framer, &err);
        Some(err)
    }

    fn normalize_terminal_error(&mut self, err: ErrorMessage) -> ErrorMessage {
        sanitize_error_message(&err)
    }

    fn write_terminal_error(&mut self, out: &mut Vec<u8>, err: &ErrorMessage) {
        self.framer.flush(out);
        let status = err.status_or_500();
        let err_text = stream_error_text(Some(err), status);
        log_stream_error(&self.api_log, &self.framer, err);
        if !self.framer.terminal_event.is_empty() {
            return;
        }
        let mut seq = self.framer.data_frames as i64;
        let from_text = cpa_json::parse_str(&err_text);
        let orig = from_text.g("sequence_number");
        if orig.exists() {
            seq = orig.int();
        }
        if self.is_codex {
            let chunk = build_failed_chunk(status, &err_text, seq);
            out.extend_from_slice(b"\nevent: response.failed\ndata: ");
            out.extend_from_slice(&chunk);
        } else {
            let chunk = build_error_chunk(status, &err_text, seq);
            out.extend_from_slice(b"\nevent: error\ndata: ");
            out.extend_from_slice(&chunk);
        }
        out.extend_from_slice(b"\n\n");
    }

    fn close_error(&mut self, out: &mut Vec<u8>) -> Option<ErrorMessage> {
        self.framer.flush(out);
        if let Some(err) = &self.framer.terminal_error {
            return Some(err.clone());
        }
        if !self.framer.terminal_event.is_empty() {
            return None;
        }
        let last = if self.framer.last_event.is_empty() { "none" } else { &self.framer.last_event };
        Some(ErrorMessage::new(
            502,
            format!("upstream stream closed before a terminal event (last event: {last})"),
        ))
    }

    fn write_done(&mut self, out: &mut Vec<u8>) {
        self.framer.flush(out);
        out.push(b'\n');
    }
}

/// A one-item error stream (the Go "pending error" channel handed to `forwardResponsesStream`).
fn error_only_stream(err: ErrorMessage) -> ExecRx {
    ExecRx::once(Err(err))
}

fn empty_stream() -> ExecRx {
    ExecRx::empty()
}

async fn stream_responses(st: &AppState, info: &ReqInfo, model: &str, raw: Bytes) -> Response {
    let pipeline = Pipeline::new(st, info);
    let passthrough = pipeline.settings.passthrough_headers;
    let keepalive = pipeline.settings.stream_keepalive;
    let is_codex = is_codex_responses_client(&info.headers);
    let mut es = pipeline.execute_stream(ExecArgs::new(Format::OpenAIResponse, model, raw, "")).await;
    let mut framer = ResponsesSseFramer::new(is_codex);
    let mut initial: Vec<u8> = Vec::new();

    // Frames are buffered until the first real data frame, so early failures still get a JSON
    // status; after that the response is committed as SSE and the forwarder takes over.
    let forward = |framer: ResponsesSseFramer, initial: Vec<u8>, rx: Rx, headers: &HeaderMap| {
        let hooks = ResponsesHooks { framer, is_codex, api_log: info.api_log.clone() };
        start_sse_stream(HeaderMap::new(), headers, initial, rx, hooks, keepalive, true)
    };

    loop {
        match es.rx.recv().await {
            Some(Err(err)) => {
                framer.flush(&mut initial);
                let safe = sanitize_error_message(&err);
                if framer.data_frames > 0 {
                    return forward(framer, initial, error_only_stream(safe), &es.headers);
                }
                info.api_log.record_error(safe.status_or_500(), &safe.text);
                return openai_error_reply(&safe, passthrough).into_response();
            }
            None => {
                framer.flush(&mut initial);
                if framer.data_frames > 0 {
                    if let Some(err) = &framer.terminal_error {
                        log_stream_error(&info.api_log, &framer, err);
                        return commit_terminal(initial, &es.headers);
                    }
                    if !framer.terminal_event.is_empty() {
                        return commit_terminal(initial, &es.headers);
                    }
                    let err = sanitize_error_message(&ErrorMessage::new(502, "upstream stream closed before a terminal event"));
                    return forward(framer, initial, error_only_stream(err), &es.headers);
                }
                if framer.terminal_event.is_empty() {
                    let err = sanitize_error_message(&ErrorMessage::new(502, "upstream stream closed before first payload"));
                    info.api_log.record_error(err.status_or_500(), &err.text);
                    return openai_error_reply(&err, passthrough).into_response();
                }
                return openai_error_reply(&ErrorMessage::new(500, ""), passthrough).into_response();
            }
            Some(Ok(chunk)) => {
                framer.write_chunk(&mut initial, &chunk);
                if framer.data_frames == 0 {
                    continue;
                }
                if let Some(err) = &framer.terminal_error {
                    log_stream_error(&info.api_log, &framer, err);
                    return commit_terminal(initial, &es.headers);
                }
                let rx = std::mem::replace(&mut es.rx, empty_stream());
                return forward(framer, initial, rx, &es.headers);
            }
        }
    }
}

type Rx = ExecRx;

/// The buffered output already ends the stream (terminal error or terminal event): send it and
/// finish without a trailing marker.
fn commit_terminal(initial: Vec<u8>, headers: &HeaderMap) -> Response {
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    let _ = tx.try_send(Bytes::from(initial));
    drop(tx);
    let mut h = HeaderMap::new();
    crate::reply::set_sse_headers(&mut h);
    crate::headers::write_upstream_headers(&mut h, headers);
    crate::reply::streaming_response(200, h, rx)
}
