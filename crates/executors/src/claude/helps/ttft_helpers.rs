//! Time-to-first-token detection for Anthropic Messages streams (Go: helps/claude_ttft_helpers.go).

use cpa_json::{J, Value};

use crate::helps::text::trim_space;
use crate::helps::usage::UsageReporter;

fn non_empty(v: &Value, path: &str) -> bool {
    !v.g(path).str().is_empty()
}

/// Whether an Anthropic Messages SSE chunk carries substantive output (text, thinking, tool input
/// or signature delta, a populated block start, a stop reason, `message_stop` or `error`) (Go:
/// `IsClaudeTokenEvent`). Accepts a bare JSON payload, a `data:` line or a multi-line event buffer
/// starting with `event:`. `message_start`, `ping` and `content_block_stop` never count.
pub fn is_claude_token_event(payload: &[u8]) -> bool {
    let mut payload = trim_space(payload);
    if payload.is_empty() {
        return false;
    }

    // Strip the "event: ..." line of a multi-line SSE buffer.
    if let Some(idx) = payload.iter().position(|b| *b == b'\n').filter(|_| payload.starts_with(b"event:")) {
        payload = trim_space(&payload[idx + 1..]);
    }

    // Strip the SSE data prefix ("data: {...}").
    if payload.starts_with(b"data:") {
        let prefix = if payload.starts_with(b"data: ") { 6 } else { 5 };
        payload = trim_space(&payload[prefix..]);
        if payload.is_empty() {
            return false;
        }
    }

    let v = crate::helps::parse_cache::parse(payload);
    match v.g("type").str().as_str() {
        // Substantive streaming block deltas.
        "content_block_delta" => ["delta.text", "delta.thinking", "delta.partial_json", "delta.signature"].iter().any(|p| non_empty(&v, p)),
        // Initial block start when populated upfront.
        "content_block_start" => non_empty(&v, "content_block.text") || non_empty(&v, "content_block.thinking"),
        // Terminal completion events.
        "message_delta" => non_empty(&v, "delta.stop_reason"),
        "message_stop" | "error" => true,
        // Handshake metadata and container boundaries.
        "message_start" | "ping" | "content_block_stop" => false,
        // Non-streaming complete message payload.
        _ => v.g("content").is_array()
            && v.g("content").array().iter().any(|block| {
                non_empty(&block.value(), "text")
                    || non_empty(&block.value(), "thinking")
                    || (block.g("type").str() == "tool_use" && non_empty(&block.value(), "name"))
            }),
    }
}

/// Observes an Anthropic Messages SSE chunk: records the served model and, when the frame is the
/// first meaningful token event, TTFT (first-packet fallback otherwise) (Go:
/// `ObserveClaudeTokenEvent`). Returns immediately once TTFT is set.
pub fn observe_claude_token_event(reporter: &UsageReporter, payload: &[u8]) {
    if payload.is_empty() {
        return;
    }
    reporter.observe_response_model(payload);
    if reporter.is_ttft_set() {
        return;
    }
    reporter.observe_token_event(is_claude_token_event(payload));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_event_classification() {
        let cases: &[(&str, bool)] = &[
            (r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#, true),
            (r#"data:{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"t"}}"#, true),
            (r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{"}}"#, true),
            (r#"{"type":"content_block_delta","delta":{"type":"signature_delta","signature":"EgI="}}"#, true),
            (r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":""}}"#, false),
            (r#"{"type":"content_block_start","content_block":{"type":"text","text":""}}"#, false),
            (r#"{"type":"content_block_start","content_block":{"type":"thinking","thinking":"x"}}"#, true),
            (r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#, true),
            (r#"{"type":"message_delta","delta":{"stop_reason":null}}"#, false),
            (r#"{"type":"message_stop"}"#, true),
            (r#"{"type":"error","error":{"message":"x"}}"#, true),
            (r#"{"type":"message_start","message":{"content":[]}}"#, false),
            (r#"{"type":"ping"}"#, false),
            (r#"{"type":"content_block_stop","index":0}"#, false),
            ("event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n", true),
            ("event: ping\ndata: {\"type\":\"ping\"}\n\n", false),
            (r#"{"id":"m","content":[{"type":"text","text":"ok"}]}"#, true),
            (r#"{"id":"m","content":[{"type":"tool_use","name":"Read"}]}"#, true),
            (r#"{"id":"m","content":[{"type":"text","text":""}]}"#, false),
            ("", false),
            ("data:", false),
        ];
        for (payload, want) in cases {
            assert_eq!(is_claude_token_event(payload.as_bytes()), *want, "{payload}");
        }
    }
}
