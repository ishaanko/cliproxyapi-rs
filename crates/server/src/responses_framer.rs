//! SSE framer for `POST /v1/responses` streams (Go: `responsesSSEFramer` in
//! openai/openai_responses_handlers.go).
//!
//! Upstream chunks may be partial or merged frames. The framer reassembles frames, drops private
//! events, rebuilds an empty `response.completed` output from the collected
//! `response.output_item.done` items, rewrites error payloads into the dialect's error events and
//! remembers the terminal event.

use std::collections::HashMap;

use cpa_json::J;
use serde_json::Value;

use crate::error::ErrorMessage;
use crate::responses_error::{
    build_error_chunk, build_failed_chunk, sanitize_error_message, sanitize_event_name, stream_error_text,
};

/// Client kind decides the failure event and which private events are dropped.
#[derive(Debug, Default)]
pub struct ResponsesSseFramer {
    pending: Vec<u8>,
    output_items: HashMap<i64, Value>,
    output_order: Vec<i64>,
    unindexed_output_items: Vec<Value>,
    pub last_event: String,
    pub terminal_event: String,
    pub terminal_error: Option<ErrorMessage>,
    /// `response.failed` for Codex clients, `error` otherwise.
    pub failure_event: String,
    pub is_codex_client: bool,
    pub data_frames: usize,
}

/// `isCodexResponsesClientRequest`.
pub fn is_codex_responses_client(headers: &axum::http::HeaderMap) -> bool {
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("").trim();
    if ua.starts_with("Codex Desktop/")
        || ua.starts_with("codex-tui/")
        || ua == "codex_cli_rs"
        || ua.starts_with("codex_cli_rs/")
        || ua.starts_with("codex_exec/")
    {
        return true;
    }
    let originator = headers
        .get("originator")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    matches!(originator.as_str(), "codex desktop" | "codex-tui" | "codex_cli_rs")
        || originator.starts_with("codex desktop/")
        || originator.starts_with("codex-tui/")
        || originator.starts_with("codex_cli_rs/")
}

impl ResponsesSseFramer {
    pub fn new(is_codex_client: bool) -> Self {
        ResponsesSseFramer {
            failure_event: if is_codex_client { "response.failed" } else { "error" }.into(),
            is_codex_client,
            ..Default::default()
        }
    }

    /// `WriteChunk`: feeds one upstream chunk, appending finished frames to `out`.
    pub fn write_chunk(&mut self, out: &mut Vec<u8>, chunk: &[u8]) {
        if chunk.is_empty() || !self.terminal_event.is_empty() {
            return;
        }
        if starts_new_data_frame(&self.pending, chunk) {
            let frame = std::mem::take(&mut self.pending);
            self.write_frame(out, &frame);
            if !self.terminal_event.is_empty() {
                return;
            }
        }
        if needs_line_break(&self.pending, chunk) {
            self.pending.push(b'\n');
        }
        self.pending.extend_from_slice(chunk);
        loop {
            let frame_len = frame_len(&self.pending);
            if frame_len == 0 {
                break;
            }
            let frame: Vec<u8> = self.pending.drain(..frame_len).collect();
            self.write_frame(out, &frame);
            if !self.terminal_event.is_empty() {
                self.pending.clear();
                return;
            }
        }
        if self.pending.trim_ascii().is_empty() {
            self.pending.clear();
            return;
        }
        if self.pending.is_empty() || !can_emit_without_delimiter(&self.pending) {
            return;
        }
        let frame = std::mem::take(&mut self.pending);
        self.write_frame(out, &frame);
    }

    /// `Flush`: emits a dangling frame when it is complete enough.
    pub fn flush(&mut self, out: &mut Vec<u8>) {
        if self.pending.is_empty() || !self.terminal_event.is_empty() {
            return;
        }
        if self.pending.trim_ascii().is_empty() {
            self.pending.clear();
            return;
        }
        if !can_flush_without_delimiter(&self.pending) {
            self.pending.clear();
            return;
        }
        let frame = std::mem::take(&mut self.pending);
        self.write_frame(out, &frame);
    }

    fn write_frame(&mut self, out: &mut Vec<u8>, frame: &[u8]) {
        let repaired = self.repair_frame(frame);
        write_sse_chunk(out, &repaired);
    }

    fn should_filter_private_event(&self, stream_event: &str, payload_type: &str) -> bool {
        let check = |name: &str| {
            let name = name.trim();
            if name.is_empty() || is_error_event(name) {
                return false;
            }
            // Internal WebSocket timing telemetry never reaches SSE clients.
            if name.starts_with("responsesapi.") {
                return true;
            }
            // Official Codex clients keep codex.response.metadata but not rate limits.
            if self.is_codex_client {
                return name == "codex.rate_limits";
            }
            name.starts_with("codex.")
        };
        check(stream_event) || check(payload_type)
    }

    fn repair_frame(&mut self, frame: &[u8]) -> Vec<u8> {
        let (payload, ok) = data_payload(frame);
        let stream_event = event_name(frame);
        if !stream_event.is_empty() && self.should_filter_private_event(&stream_event, "") {
            return Vec::new();
        }
        if !ok || payload.is_empty() {
            return frame.to_vec();
        }
        if payload == b"[DONE]" {
            self.data_frames += 1;
            return frame.to_vec();
        }
        if !cpa_json::valid(&payload) {
            return frame.to_vec();
        }
        let root = cpa_json::parse(&payload);
        let payload_type = root.g("type").str();
        if self.should_filter_private_event(&stream_event, &payload_type) {
            return Vec::new();
        }
        self.data_frames += 1;

        if is_error_event(&payload_type) || payload_has_error(&root) {
            if !payload_type.is_empty() {
                self.last_event = sanitize_event_name(&payload_type);
            }
            return self.repair_error_payload(&root, &payload);
        }
        let mut event_type = payload_type;
        if is_terminal_event(&stream_event) {
            event_type = stream_event.clone();
        } else if event_type.is_empty() {
            event_type = stream_event.clone();
        }
        if !event_type.is_empty() {
            self.last_event = sanitize_event_name(&event_type);
        }
        if is_error_event(&event_type) {
            return self.repair_error_payload(&root, &payload);
        }
        if is_terminal_event(&event_type) {
            self.terminal_event = event_type.clone();
        }
        match event_type.as_str() {
            "response.output_item.done" => self.record_output_item(&root),
            "response.completed" => {
                if let Some(repaired) = self.repair_completed_payload(root) {
                    return frame_with_data(frame, &repaired);
                }
            }
            _ => {}
        }
        frame.to_vec()
    }

    fn repair_error_payload(&mut self, root: &Value, payload: &[u8]) -> Vec<u8> {
        let err = payload_error_message(root, payload);
        let status = err.status;
        self.terminal_error = Some(err.clone());
        let failure_event = if self.failure_event == "response.failed" { "response.failed" } else { "error" };
        self.terminal_event = failure_event.to_string();
        let err_text = stream_error_text(Some(&err), status);
        let seq_node = root.g("sequence_number");
        let seq = if seq_node.exists() {
            seq_node.int()
        } else {
            let from_text = cpa_json::parse_str(&err_text);
            let orig = from_text.g("sequence_number");
            if orig.exists() {
                orig.int()
            } else if self.data_frames > 0 {
                self.data_frames as i64 - 1
            } else {
                0
            }
        };
        if failure_event == "response.failed" {
            let chunk = build_failed_chunk(status, &err_text, seq);
            [b"event: response.failed\ndata: ".as_slice(), &chunk, b"\n\n"].concat()
        } else {
            let chunk = build_error_chunk(status, &err_text, seq);
            [b"event: error\ndata: ".as_slice(), &chunk, b"\n\n"].concat()
        }
    }

    fn record_output_item(&mut self, root: &Value) {
        let item = root.g("item");
        if !item.exists() || !item.is_object() || item.g("type").str().is_empty() {
            return;
        }
        let value = item.value();
        let index = root.g("output_index");
        if index.exists() {
            let index = index.int();
            if !self.output_items.contains_key(&index) {
                self.output_order.push(index);
            }
            self.output_items.insert(index, value);
            return;
        }
        self.unindexed_output_items.push(value);
    }

    /// Rebuilds `response.output` from the recorded items when the upstream sent none.
    fn repair_completed_payload(&self, mut root: Value) -> Option<Vec<u8>> {
        if self.output_order.is_empty() && self.unindexed_output_items.is_empty() {
            return None;
        }
        let output = root.g("response.output");
        if output.exists() && (!output.is_array() || !output.array().is_empty()) {
            return None;
        }
        let mut indexes = self.output_order.clone();
        indexes.sort_unstable();
        let mut items: Vec<Value> = indexes.iter().filter_map(|i| self.output_items.get(i).cloned()).collect();
        items.extend(self.unindexed_output_items.iter().cloned());
        cpa_json::set(&mut root, "response.output", Value::Array(items));
        Some(cpa_json::to_vec(&root))
    }
}

/// `writeResponsesSSEChunk`: writes the frame and completes it to a blank-line terminator.
fn write_sse_chunk(out: &mut Vec<u8>, chunk: &[u8]) {
    if chunk.is_empty() {
        return;
    }
    out.extend_from_slice(chunk);
    if chunk.ends_with(b"\n\n") || chunk.ends_with(b"\r\n\r\n") {
        return;
    }
    let suffix: &[u8] = if chunk.ends_with(b"\r\n") {
        b"\r\n"
    } else if chunk.ends_with(b"\n") {
        b"\n"
    } else {
        b"\n\n"
    };
    out.extend_from_slice(suffix);
}

fn is_error_event(name: &str) -> bool {
    matches!(name, "response.failed" | "response.error" | "error")
}

fn is_terminal_event(name: &str) -> bool {
    matches!(
        name,
        "response.completed" | "response.incomplete" | "response.failed" | "response.done" | "response.error" | "error"
    )
}

fn payload_has_error(root: &Value) -> bool {
    for path in ["error", "response.error"] {
        let r = root.g(path);
        if r.exists() && !r.is_null() {
            return true;
        }
    }
    root.g("code").exists() && root.g("message").exists()
}

/// `responsesSSEPayloadErrorMessage`: status from the first plausible field, default 502.
fn payload_error_message(root: &Value, payload: &[u8]) -> ErrorMessage {
    let mut status = 502u16;
    for path in [
        "status",
        "status_code",
        "error.status",
        "error.status_code",
        "response.error.status",
        "response.error.status_code",
    ] {
        let candidate = root.g(path).int();
        if (400..=599).contains(&candidate) {
            status = candidate as u16;
            break;
        }
    }
    sanitize_error_message(&ErrorMessage::new(status, String::from_utf8_lossy(payload).into_owned()))
}

/// Joined `data:` lines of a frame (CR stripped).
fn data_payload(frame: &[u8]) -> (Vec<u8>, bool) {
    let mut payload = Vec::new();
    let mut found = false;
    for line in frame.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let trimmed = line.trim_ascii();
        let Some(rest) = trimmed.strip_prefix(b"data:") else {
            continue;
        };
        if found {
            payload.push(b'\n');
        }
        payload.extend_from_slice(rest.trim_ascii());
        found = true;
    }
    (payload, found)
}

/// `responsesSSEFrameWithData`: keeps non-data lines (event, id, ...) and replaces `data:`.
fn frame_with_data(frame: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in frame.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let trimmed = line.trim_ascii();
        if trimmed.is_empty() || trimmed.starts_with(b"data:") {
            continue;
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    for line in payload.split(|b| *b == b'\n') {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out.push(b'\n');
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `responsesSSEFrameLen`: length of the first frame (through its blank line), 0 if incomplete.
fn frame_len(chunk: &[u8]) -> usize {
    if chunk.is_empty() {
        return 0;
    }
    match (find(chunk, b"\n\n"), find(chunk, b"\r\n\r\n")) {
        (None, None) => 0,
        (None, Some(crlf)) => crlf + 4,
        (Some(lf), None) => lf + 2,
        (Some(lf), Some(crlf)) => {
            if lf < crlf { lf + 2 } else { crlf + 4 }
        }
    }
}

fn has_field(chunk: &[u8], prefix: &[u8]) -> bool {
    chunk.split(|b| *b == b'\n').any(|line| line.trim_ascii().starts_with(prefix))
}

fn needs_more_data(chunk: &[u8]) -> bool {
    let trimmed = chunk.trim_ascii();
    !trimmed.is_empty() && has_field(trimmed, b"event:") && !has_field(trimmed, b"data:")
}

fn data_lines_valid(chunk: &[u8]) -> bool {
    let (payload, found) = data_payload(chunk);
    if !found {
        return true;
    }
    let payload = payload.trim_ascii();
    payload.is_empty() || payload == b"[DONE]" || cpa_json::valid(payload)
}

fn can_emit_without_delimiter(chunk: &[u8]) -> bool {
    let trimmed = chunk.trim_ascii();
    if trimmed.is_empty()
        || needs_more_data(trimmed)
        || !has_field(trimmed, b"event:")
        || !has_field(trimmed, b"data:")
    {
        return false;
    }
    data_lines_valid(trimmed)
}

fn can_flush_without_delimiter(chunk: &[u8]) -> bool {
    let trimmed = chunk.trim_ascii();
    !trimmed.is_empty() && has_field(trimmed, b"data:") && data_lines_valid(trimmed)
}

fn starts_new_data_frame(pending: &[u8], chunk: &[u8]) -> bool {
    let trimmed_pending = pending.trim_ascii();
    if trimmed_pending.is_empty()
        || has_field(trimmed_pending, b"event:")
        || !has_field(trimmed_pending, b"data:")
        || !data_lines_valid(trimmed_pending)
    {
        return false;
    }
    let start = chunk.iter().position(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n')).unwrap_or(chunk.len());
    chunk[start..].starts_with(b"data:")
}

fn event_name(frame: &[u8]) -> String {
    for line in frame.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line).trim_ascii();
        if let Some(rest) = line.strip_prefix(b"event:") {
            return String::from_utf8_lossy(rest).trim().to_string();
        }
    }
    String::new()
}

fn needs_line_break(pending: &[u8], chunk: &[u8]) -> bool {
    if pending.is_empty() || chunk.is_empty() {
        return false;
    }
    if pending.ends_with(b"\n") || pending.ends_with(b"\r") {
        return false;
    }
    if chunk[0] == b'\n' || chunk[0] == b'\r' {
        return false;
    }
    let start = chunk.iter().position(|b| !matches!(b, b' ' | b'\t')).unwrap_or(chunk.len());
    let trimmed = &chunk[start..];
    if trimmed.is_empty() {
        return false;
    }
    [b"data:".as_slice(), b"event:", b"id:", b"retry:", b":"]
        .iter()
        .any(|p| trimmed.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(framer: &mut ResponsesSseFramer, chunks: &[&str]) -> String {
        let mut out = Vec::new();
        for c in chunks {
            framer.write_chunk(&mut out, c.as_bytes());
        }
        framer.flush(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn partial_chunks_are_reassembled_into_frames() {
        let mut f = ResponsesSseFramer::new(false);
        let out = run(&mut f, &["event: response.created\nda", "ta: {\"type\":\"response.created\"}\n", "\n"]);
        assert_eq!(out, "event: response.created\ndata: {\"type\":\"response.created\"}\n\n");
        assert_eq!(f.data_frames, 1);
        assert_eq!(f.last_event, "response.created");
    }

    #[test]
    fn data_only_frames_without_delimiters_are_split_and_terminated() {
        let mut f = ResponsesSseFramer::new(false);
        let out = run(&mut f, &["data: {\"type\":\"a\"}", "data: {\"type\":\"b\"}"]);
        assert_eq!(out, "data: {\"type\":\"a\"}\n\ndata: {\"type\":\"b\"}\n\n");
        assert_eq!(f.data_frames, 2);
    }

    #[test]
    fn private_events_are_filtered_by_client_kind() {
        let frames = ["event: codex.rate_limits\ndata: {\"type\":\"codex.rate_limits\"}\n\n", "event: codex.response.metadata\ndata: {\"type\":\"codex.response.metadata\"}\n\n", "event: responsesapi.websocket_timing\ndata: {}\n\n"];
        let mut other = ResponsesSseFramer::new(false);
        assert_eq!(run(&mut other, &frames), "");
        let mut codex = ResponsesSseFramer::new(true);
        assert_eq!(
            run(&mut codex, &frames),
            "event: codex.response.metadata\ndata: {\"type\":\"codex.response.metadata\"}\n\n"
        );
    }

    #[test]
    fn completed_output_is_rebuilt_from_output_item_done() {
        let mut f = ResponsesSseFramer::new(false);
        let out = run(
            &mut f,
            &[
                "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"message\",\"id\":\"b\"}}\n\n",
                "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"a\"}}\n\n",
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n",
            ],
        );
        assert!(out.contains(
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"reasoning\",\"id\":\"a\"},{\"type\":\"message\",\"id\":\"b\"}]}}\n\n"
        ), "{out}");
        assert_eq!(f.terminal_event, "response.completed");
    }

    #[test]
    fn chunks_after_the_terminal_event_are_ignored() {
        let mut f = ResponsesSseFramer::new(false);
        let out = run(
            &mut f,
            &[
                "data: {\"type\":\"response.completed\",\"response\":{\"output\":[1]}}\n\n",
                "data: {\"type\":\"late\"}\n\n",
            ],
        );
        assert_eq!(out, "data: {\"type\":\"response.completed\",\"response\":{\"output\":[1]}}\n\n");
    }

    #[test]
    fn error_payloads_become_error_events_per_client() {
        let frame = "data: {\"type\":\"error\",\"status\":429,\"error\":{\"message\":\"slow\",\"type\":\"rate_limit\"}}\n\n";
        let mut other = ResponsesSseFramer::new(false);
        let out = run(&mut other, &[frame]);
        assert_eq!(
            out,
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"slow\",\"type\":\"rate_limit\"},\"sequence_number\":0}\n\n"
        );
        assert_eq!(other.terminal_error.as_ref().unwrap().status, 429);
        let mut codex = ResponsesSseFramer::new(true);
        let out = run(&mut codex, &[frame]);
        assert!(out.starts_with("event: response.failed\ndata: {\"type\":\"response.failed\""), "{out}");
        assert_eq!(codex.terminal_event, "response.failed");
    }

    #[test]
    fn done_marker_counts_as_a_data_frame() {
        let mut f = ResponsesSseFramer::new(false);
        let out = run(&mut f, &["data: [DONE]\n\n"]);
        assert_eq!(out, "data: [DONE]\n\n");
        assert_eq!(f.data_frames, 1);
    }

    #[test]
    fn codex_client_detection() {
        use axum::http::HeaderValue;
        let mut h = axum::http::HeaderMap::new();
        assert!(!is_codex_responses_client(&h));
        h.insert("user-agent", HeaderValue::from_static("codex_cli_rs/0.1"));
        assert!(is_codex_responses_client(&h));
        let mut h = axum::http::HeaderMap::new();
        h.insert("originator", HeaderValue::from_static("Codex Desktop/1.2"));
        assert!(is_codex_responses_client(&h));
    }
}
