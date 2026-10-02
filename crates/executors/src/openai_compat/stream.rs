//! Streaming response handling of the OpenAI-compatible executor (Go: `ExecuteStream` /
//! `executeImagesStream` goroutines and the `openAICompat*` stream error helpers).
//!
//! SSE frames are assembled by hand (blank-line delimited, `data:` lines accumulated, `event:`
//! name tracked) so upstream error payloads and truncated streams are classified like Go does.

use bytes::Bytes;
use cpa_json::J;
use cpa_runtime::executor::{ExecError, StreamResult};
use cpa_translator::{Format, Param};
use futures_util::StreamExt;
use http::HeaderMap;
use tokio::sync::{mpsc, oneshot};

use super::errors::{transport_error, transport_message};
use super::translate::{end_apply_patch_stream, observe_body};
use super::claude_input_tokens::{ClaudeInputTokenState, translate_stream_with_claude_input_tokens};
use crate::helps::apply_patch::{
    APPLY_PATCH_UPSTREAM_ERROR_MESSAGE, ChunkSender, apply_patch_translation_error, initialize_apply_patch_stream,
    record_apply_patch_stream_failure,
};
use crate::helps::sse::{LineReader, STREAM_SCANNER_BUFFER, ScanError};
use crate::helps::status::status_err;
use crate::helps::stream_response_model_observer::StreamResponseModelObserver;
use crate::helps::text::trim_space;
use crate::helps::usage::{StreamUsageBuffer, UsageReporter};

/// Everything the chat stream task needs from the request.
pub struct ChatStreamParams {
    pub reporter: UsageReporter,
    pub from: Format,
    pub to: Format,
    pub response_format: Format,
    pub model: String,
    pub original_payload: Bytes,
    pub patch_original: Bytes,
    pub translated: Bytes,
}

/// Go: openAICompatErrorEvent.
fn is_error_event(event: &str) -> bool {
    ["error", "response.error", "response.failed"].iter().any(|e| event.eq_ignore_ascii_case(e))
}

/// Go: openAICompatStreamDataError. `Some(err)` when `payload` carries an upstream error.
pub fn stream_data_error(payload: &[u8], event: &str) -> Option<ExecError> {
    if payload.is_empty() || !cpa_json::valid(payload) {
        return None;
    }
    let v = cpa_json::parse(payload);
    let payload_type = v.g("type").str();
    let has_error = ["error", "response.error"].iter().any(|p| {
        let node = v.g(p);
        node.exists() && !node.is_null()
    });
    let has_top_level = v.g("code").exists() && v.g("message").exists();
    let typed_error = ["error", "response.error", "response.failed"].iter().any(|t| payload_type.eq_ignore_ascii_case(t));
    if !has_error && !typed_error && !is_error_event(event) && !has_top_level {
        return None;
    }
    let mut status = 0i64;
    for path in [
        "status",
        "status_code",
        "error.status",
        "error.status_code",
        "response.error.status",
        "response.error.status_code",
    ] {
        status = v.g(path).int();
        if (400..=599).contains(&status) {
            break;
        }
    }
    if !(400..=599).contains(&status) {
        status = 502;
    }
    Some(status_err(status as u16, String::from_utf8_lossy(payload).into_owned()))
}

struct ChatStream {
    out: ChunkSender,
    p: ChatStreamParams,
    param: Param,
    claude: ClaudeInputTokenState,
    usage: StreamUsageBuffer,
    seen_done: bool,
    failed: bool,
    aborted: bool,
    event: String,
    frame: Vec<Vec<u8>>,
}

impl ChatStream {
    fn gateway_error() -> ExecError {
        status_err(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE)
    }

    /// Reports a stream failure to usage and the client. With `contains_payload` the usage log
    /// gets a generic message instead of the upstream payload.
    async fn publish_error(&mut self, err: ExecError, contains_payload: bool) {
        let logged = if contains_payload {
            status_err(err.status, "upstream stream returned an error payload")
        } else {
            err.clone()
        };
        self.p.reporter.publish_failure(&logged);
        let _ = self.out.send(Err(err)).await;
        self.failed = true;
    }

    fn translate(&mut self, raw: &[u8]) -> Vec<Vec<u8>> {
        translate_stream_with_claude_input_tokens(
            self.p.to,
            self.p.response_format,
            &self.p.model,
            &self.p.patch_original,
            &self.p.translated,
            raw,
            &mut self.param,
            &mut self.claude,
        )
    }

    /// Handles one complete SSE frame; true ends the stream loop.
    async fn process_frame(&mut self) -> bool {
        let event = std::mem::take(&mut self.event);
        let data_lines = std::mem::take(&mut self.frame);
        if data_lines.is_empty() {
            if is_error_event(&event) {
                self.publish_error(status_err(502, "upstream error event ended without data"), false).await;
                return true;
            }
            return false;
        }
        if data_lines.len() > 1 && data_lines.iter().any(|d| trim_space(d) == b"[DONE]") {
            self.publish_error(status_err(502, "upstream stream ended with incomplete data before [DONE]"), false).await;
            return true;
        }
        let payload = trim_space(&data_lines.join(&b'\n')).to_vec();
        let is_done = payload == b"[DONE]";
        if is_done && is_error_event(&event) {
            self.publish_error(status_err(502, "upstream error event ended before [DONE]"), false).await;
            return true;
        }
        if !is_done && !cpa_json::valid(&payload) {
            self.publish_error(status_err(502, "upstream stream ended with incomplete SSE data frame"), false).await;
            return true;
        }
        if !is_done && let Some(err) = stream_data_error(&payload, &event) {
            self.publish_error(err, true).await;
            return true;
        }
        let mut line = b"data: ".to_vec();
        line.extend_from_slice(&payload);
        let chunks = self.translate(&line);
        record_apply_patch_stream_failure(&self.param, &self.p.reporter, &Self::gateway_error());
        for chunk in chunks {
            if self.out.send(Ok(Bytes::from(chunk))).await.is_err() {
                self.aborted = true;
                return true;
            }
        }
        if apply_patch_translation_error(&self.param).is_some() {
            self.publish_error(Self::gateway_error(), false).await;
            return true;
        }
        if is_done {
            self.seen_done = true;
            return true;
        }
        false
    }

    async fn run(&mut self, mut lines: LineReader) {
        let mut scan_err: Option<ScanError> = None;
        while let Some(next) = lines.next_line().await {
            let line = match next {
                Ok(line) => line,
                Err(err) => {
                    scan_err = Some(err);
                    break;
                }
            };
            self.p.reporter.observe_response_model(&line);
            self.usage.observe_openai_stream(&line);
            let trimmed = trim_space(&line);
            if trimmed.is_empty() {
                if self.process_frame().await {
                    break;
                }
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix(b"data:") {
                self.frame.push(trim_space(rest).to_vec());
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix(b"event:") {
                self.event = String::from_utf8_lossy(trim_space(rest)).into_owned();
                continue;
            }
            if trimmed.starts_with(b":") || trimmed.starts_with(b"id:") || trimmed.starts_with(b"retry:") {
                continue;
            }
            if trimmed.starts_with(b"{") || trimmed.starts_with(b"[") {
                let msg = String::from_utf8_lossy(trimmed).into_owned();
                self.publish_error(status_err(502, msg), true).await;
                break;
            }
        }
        if scan_err.is_none() && !self.seen_done && !self.failed && !self.aborted && !self.frame.is_empty() {
            let _ = self.process_frame().await;
        }
        if !self.failed
            && !self.aborted
            && end_apply_patch_stream(&mut self.param, &self.p.reporter, &self.out, Self::gateway_error()).await
        {
            return;
        }
        if self.failed || self.aborted {
            return;
        }
        if let Some(err) = scan_err {
            let err = ExecError::from(err);
            self.p.reporter.publish_failure(&err);
            let _ = self.out.send(Err(err)).await;
        } else if !self.seen_done {
            // Responses clients need an explicit terminal event, so a clean EOF without [DONE]
            // is a failed stream for them.
            if self.p.response_format == Format::OpenAIResponse {
                let err = status_err(502, "upstream stream closed before [DONE]");
                self.p.reporter.publish_failure(&err);
                let _ = self.out.send(Err(err)).await;
                return;
            }
            // Other protocols stay compatible with providers that omit [DONE].
            let chunks = self.translate(b"data: [DONE]");
            record_apply_patch_stream_failure(&self.param, &self.p.reporter, &Self::gateway_error());
            for chunk in chunks {
                if self.out.send(Ok(Bytes::from(chunk))).await.is_err() {
                    return;
                }
            }
        }
        self.p.reporter.publish_buffer(&self.usage);
        self.p.reporter.ensure_published();
    }
}

/// Starts the chat-completions stream task and returns the client-facing stream.
pub fn spawn_chat_stream(resp: reqwest::Response, headers: HeaderMap, p: ChatStreamParams) -> StreamResult {
    let (tx, rx) = mpsc::channel(16);
    let (usage_tx, usage_rx) = oneshot::channel();
    let lines = LineReader::new(
        Box::pin(observe_body(p.reporter.clone(), resp.bytes_stream(), false).map(|r| r.map_err(|e| transport_message(&e)))),
        STREAM_SCANNER_BUFFER,
    );
    tokio::spawn(async move {
        let mut param = Param::default();
        initialize_apply_patch_stream(p.to, p.response_format, &p.model, &p.patch_original, &p.translated, &mut param);
        let claude = ClaudeInputTokenState::new(p.from, p.to, p.response_format, &p.original_payload);
        let mut stream = ChatStream {
            out: tx,
            p,
            param,
            claude,
            usage: StreamUsageBuffer::default(),
            seen_done: false,
            failed: false,
            aborted: false,
            event: String::new(),
            frame: Vec::new(),
        };
        stream.run(lines).await;
        // Usage is published once; this covers the early-return paths.
        stream.p.reporter.publish_buffer(&stream.usage);
        if let Some(detail) = stream.usage.detail() {
            let _ = usage_tx.send(UsageReporter::usage_metadata(&detail));
        }
    });
    let mut result = StreamResult::new(headers, rx);
    result.usage = Some(usage_rx);
    result
}

/// Starts the image passthrough stream: raw SSE bytes are forwarded unchanged.
pub fn spawn_image_stream(resp: reqwest::Response, headers: HeaderMap, reporter: UsageReporter) -> StreamResult {
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let mut observer = StreamResponseModelObserver::new(reporter.clone());
        let mut body = Box::pin(observe_body(reporter.clone(), resp.bytes_stream(), false));
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(chunk) => {
                    observer.feed(&chunk);
                    if tx.send(Ok(chunk)).await.is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let err = transport_error(&err);
                    reporter.publish_failure(&err);
                    let _ = tx.send(Err(err)).await;
                    break;
                }
            }
        }
        observer.finish();
        reporter.ensure_published();
    });
    StreamResult::new(headers, rx)
}
