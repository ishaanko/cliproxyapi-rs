//! Token-event detection for time-to-first-token (Go: helps/chat_ttft_helpers.go and
//! responses_ttft_helpers.go). Claude and Gemini have their own helpers with their providers.

use cpa_json::{J, Value};

use cpa_runtime::conductor::session::lazy::Doc;

use super::text::trim_space;
use super::usage::UsageReporter;

fn non_empty(v: &Value, path: &str) -> bool {
    !v.g(path).str().is_empty()
}

/// Whether an OpenAI Chat Completions SSE line or JSON chunk carries substantive output (text,
/// reasoning, refusal or tool call data), a terminal marker, an error or a finish reason; role
/// announcements and empty deltas do not count.
pub fn is_chat_token_event(payload: &[u8]) -> bool {
    let mut payload = trim_space(payload);
    if payload.is_empty() {
        return false;
    }
    if payload == b"data: [DONE]" || payload == b"[DONE]" {
        return true;
    }
    if payload.starts_with(b"data:") {
        let prefix = if payload.starts_with(b"data: ") { 6 } else { 5 };
        payload = trim_space(&payload[prefix..]);
        if payload.is_empty() {
            return false;
        }
        if payload == b"[DONE]" {
            return true;
        }
    }
    let v = cpa_json::parse(payload);
    // Terminal error envelope.
    if v.g("error.message").exists() || v.g("error").exists() {
        return true;
    }
    let choices = v.g("choices");
    for choice in choices.array() {
        let choice = choice.value();
        let delta = choice.g("delta");
        if delta.exists() {
            let d = delta.value();
            if ["content", "reasoning_content", "reasoning", "refusal"].iter().any(|k| non_empty(&d, k)) {
                return true;
            }
            for tc in d.g("tool_calls").array() {
                let tc = tc.value();
                if non_empty(&tc, "function.arguments") || non_empty(&tc, "function.name") || non_empty(&tc, "custom.input") {
                    return true;
                }
            }
        }
        let message = choice.g("message");
        if message.exists() {
            let m = message.value();
            if ["content", "reasoning_content", "refusal"].iter().any(|k| non_empty(&m, k)) {
                return true;
            }
            for tc in m.g("tool_calls").array() {
                let tc = tc.value();
                if non_empty(&tc, "function.arguments") || non_empty(&tc, "function.name") {
                    return true;
                }
            }
        }
        if non_empty(&choice, "finish_reason") {
            return true;
        }
    }
    false
}

/// Observes a Chat Completions chunk: records the served model and, when the frame is the first
/// meaningful token event, TTFT (first-packet fallback otherwise).
pub fn observe_chat_token_event(reporter: &UsageReporter, payload: &[u8]) {
    if payload.is_empty() {
        return;
    }
    reporter.observe_response_model(payload);
    if reporter.is_ttft_set() {
        return;
    }
    reporter.observe_token_event(is_chat_token_event(payload));
}

/// Whether an OpenAI/xAI Responses API websocket or SSE event carries an output token, reasoning
/// trace, tool call argument or multimodal delta (including Codex-private events), or is a
/// terminal event (so TTFT is never missed on failed or truncated turns).
pub fn is_responses_token_event(payload: &[u8]) -> bool {
    let mut payload = trim_space(payload);
    if payload.is_empty() {
        return false;
    }
    if payload.starts_with(b"data:") {
        let prefix = if payload.starts_with(b"data: ") { 6 } else { 5 };
        payload = trim_space(&payload[prefix..]);
        if payload.is_empty() {
            return false;
        }
    }
    let v = Doc::new(payload);
    let has = |path: &str| !v.g(path).str().is_empty();
    match v.g("type").str().as_str() {
        "response.reasoning_summary_text.delta"
        | "response.reasoning.delta"
        | "response.reasoning_text.delta"
        | "response.output_text.delta"
        | "response.text.delta"
        | "response.function_call_arguments.delta"
        | "response.custom_tool_call_input.delta"
        | "response.code_interpreter_call_code.delta"
        | "response.mcp_call_arguments.delta"
        | "response.shell_call_command.delta"
        | "response.refusal.delta"
        | "response.audio.transcript.delta" => has("delta"),
        "response.audio.delta" => has("delta") || has("data"),
        "response.image_generation_call.partial_image" => has("partial_image_b64"),
        "response.shell_call_command.added" => has("command"),
        "response.reasoning_summary_text.done" | "response.reasoning_text.done" | "response.output_text.done" => has("text"),
        "response.refusal.done" => has("refusal"),
        "response.function_call_arguments.done" | "response.mcp_call_arguments.done" => has("arguments"),
        "response.custom_tool_call_input.done" => has("input"),
        "response.code_interpreter_call_code.done" => has("code"),
        "response.shell_call_command.done" => has("command"),
        "response.reasoning_summary_part.done" => has("part.text"),
        "response.content_part.done" => has("part.text") || has("part.refusal"),
        "response.output_item.done" => match v.g("item.type").str().as_str() {
            "function_call" => has("item.arguments"),
            "custom_tool_call" => has("item.input"),
            "message" => v
                .g("item.content")
                .array()
                .iter()
                .any(|c| !c.g("text").str().is_empty() || !c.g("refusal").str().is_empty()),
            _ => false,
        },
        "response.completed" | "response.done" | "response.incomplete" | "response.failed" | "error" => true,
        _ => false,
    }
}

/// Observes a Responses API frame: records the served model and TTFT like
/// [`observe_chat_token_event`].
pub fn observe_responses_token_event(reporter: &UsageReporter, payload: &[u8]) {
    if payload.is_empty() {
        return;
    }
    reporter.observe_response_model(payload);
    if reporter.is_ttft_set() {
        return;
    }
    reporter.observe_token_event(is_responses_token_event(payload));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_token_events() {
        assert!(!is_chat_token_event(br#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#));
        assert!(is_chat_token_event(br#"data: {"choices":[{"delta":{"content":"hi"}}]}"#));
        assert!(is_chat_token_event(br#"{"choices":[{"delta":{"tool_calls":[{"function":{"name":"f"}}]}}]}"#));
        assert!(is_chat_token_event(br#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#));
        assert!(is_chat_token_event(b"data: [DONE]"));
        assert!(is_chat_token_event(br#"{"error":{"message":"x"}}"#));
        assert!(!is_chat_token_event(b""));
    }

    #[test]
    fn responses_token_events() {
        assert!(is_responses_token_event(br#"data: {"type":"response.output_text.delta","delta":"x"}"#));
        assert!(!is_responses_token_event(br#"{"type":"response.output_text.delta","delta":""}"#));
        assert!(!is_responses_token_event(br#"{"type":"response.created"}"#));
        assert!(is_responses_token_event(br#"{"type":"response.failed"}"#));
        assert!(is_responses_token_event(
            br#"{"type":"response.output_item.done","item":{"type":"message","content":[{"text":"a"}]}}"#
        ));
    }

    #[test]
    fn first_token_sets_ttft_and_first_packet_is_fallback() {
        let r = UsageReporter::new("kimi", "KimiExecutor", "m", None, None);
        r.start_response_ttft();
        observe_chat_token_event(&r, br#"{"choices":[{"delta":{"role":"assistant"}}]}"#);
        assert!(r.is_first_packet_set() && !r.is_ttft_set());
        observe_chat_token_event(&r, br#"{"choices":[{"delta":{"content":"x"}}]}"#);
        assert!(r.is_ttft_set());
    }
}
