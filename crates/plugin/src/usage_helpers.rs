//! Usage extraction for plugin executor responses (Go: `helps/plugin_executor_usage.go`).

use cpa_json::J;
use cpa_executors::claude::helps::ttft_helpers::observe_claude_token_event;
use cpa_executors::gemini::ttft::observe_gemini_token_event;
use cpa_executors::helps::text::{extract_stream_json_payload, iterate_stream_lines};
use cpa_executors::helps::ttft::{observe_chat_token_event, observe_responses_token_event};
use cpa_executors::helps::usage::{
    Detail, StreamUsageBuffer, UsageReporter, parse_antigravity_stream_usage, parse_antigravity_usage, parse_claude_usage,
    parse_codex_usage, parse_gemini_stream_usage, parse_gemini_usage, parse_interactions_stream_usage,
    parse_interactions_usage, parse_openai_usage,
};

/// Usage of a non-streaming plugin executor response in `protocol` (the output format name).
pub fn parse_plugin_executor_response_usage(protocol: &str, payload: &[u8]) -> Detail {
    if payload.is_empty() {
        return Detail::default();
    }
    match protocol.trim().to_lowercase().as_str() {
        "claude" => parse_claude_payload_usage(payload),
        "gemini" => parse_gemini_usage(payload),
        "interactions" | "interactions-response" => parse_interactions_usage(payload),
        "antigravity" => parse_antigravity_usage(payload),
        "codex" | "openai-response" => parse_codex_usage(payload).unwrap_or_else(|| parse_openai_usage(payload)),
        _ => parse_openai_usage(payload),
    }
}

/// Feeds streaming chunks into `buffer` using the parser of `protocol`.
pub fn observe_plugin_executor_stream_usage(protocol: &str, payload: &[u8], buffer: &mut StreamUsageBuffer) {
    if payload.is_empty() {
        return;
    }
    match protocol.trim().to_lowercase().as_str() {
        "claude" => iterate_stream_lines(payload, |line| {
            if let Some(detail) = parse_claude_stream_line(line) {
                buffer.observe_merged(detail);
            }
        }),
        "gemini" => iterate_stream_lines(payload, |line| {
            if let Some(detail) = parse_gemini_stream_usage(line) {
                buffer.observe(detail, true);
            }
        }),
        "interactions" | "interactions-response" => iterate_stream_lines(payload, |line| {
            if let Some(detail) = parse_interactions_stream_usage(line) {
                buffer.observe_merged(detail);
            }
        }),
        "antigravity" => iterate_stream_lines(payload, |line| {
            if let Some(detail) = parse_antigravity_stream_usage(line) {
                buffer.observe(detail, true);
            }
        }),
        "codex" | "openai-response" => iterate_stream_lines(payload, |line| {
            if let Some(json) = extract_stream_json_payload(line)
                && !json.is_empty()
                && let Some(detail) = parse_codex_usage(json)
            {
                buffer.observe(detail, true);
                return;
            }
            buffer.observe_openai_stream(line);
        }),
        _ => iterate_stream_lines(payload, |line| buffer.observe_openai_stream(line)),
    }
}

/// Records the first packet and token timing on `reporter` (Go: `ObservePluginExecutorStreamTTFT`).
pub fn observe_plugin_executor_stream_ttft(protocol: &str, reporter: &UsageReporter, payload: &[u8]) {
    if payload.is_empty() {
        return;
    }
    reporter.record_first_packet();
    match protocol.trim().to_lowercase().as_str() {
        "claude" => observe_claude_token_event(reporter, payload),
        "gemini" | "antigravity" | "interactions" | "interactions-response" => observe_gemini_token_event(reporter, payload),
        "codex" | "openai-response" => observe_responses_token_event(reporter, payload),
        _ => observe_chat_token_event(reporter, payload),
    }
}

fn usage_node_wrapped(payload: &[u8]) -> Option<Vec<u8>> {
    if !cpa_json::valid(payload) {
        return None;
    }
    let v = cpa_json::parse(payload);
    let mut node = v.g("usage");
    if !node.exists() {
        node = v.g("message.usage");
    }
    if !node.exists() {
        return None;
    }
    Some(format!("{{\"usage\":{}}}", node.raw()).into_bytes())
}

fn parse_claude_payload_usage(payload: &[u8]) -> Detail {
    match usage_node_wrapped(payload) {
        Some(wrapped) => parse_claude_usage(&wrapped),
        None => Detail::default(),
    }
}

fn parse_claude_stream_line(line: &[u8]) -> Option<Detail> {
    let payload = extract_stream_json_payload(line)?;
    if payload.is_empty() {
        return None;
    }
    let wrapped = usage_node_wrapped(payload)?;
    Some(parse_claude_usage(&wrapped))
}
