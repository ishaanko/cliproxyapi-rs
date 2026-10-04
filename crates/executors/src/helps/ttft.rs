//! Token-event detection for time-to-first-token (Go: helps/chat_ttft_helpers.go and
//! responses_ttft_helpers.go). Claude and Gemini have their own helpers with their providers.

use cpa_json::{J, Value};

use cpa_json::lazy::Doc;

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
    is_responses_token_doc(&Doc::new(payload))
}

fn is_responses_token_doc(v: &Doc<'_>) -> bool {
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
pub fn observe_responses_token_event_doc(reporter: &UsageReporter, payload: &[u8], doc: &Doc<'_>) {
    if payload.is_empty() {
        return;
    }
    reporter.observe_response_model_doc(payload, doc);
    if reporter.is_ttft_set() {
        return;
    }
    // `payload` is the bare data payload here (no `data:` prefix), as `is_responses_token_event` reads it.
    let token = !trim_space(payload).is_empty() && is_responses_token_doc(doc);
    reporter.observe_token_event(token);
}

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

    /// Go: TestIsResponsesTokenEvent_Classification.
    #[test]
    fn responses_token_events() {
        let cases: &[(&str, bool)] = &[
            ("", false),
            ("   \n\t  ", false),
            (r#"{"type":"codex.rate_limits","rate_limits":{"plan_type":"pro"}}"#, false),
            (r#"{"type":"codex.response.metadata","etag":"W/\"123\""}"#, false),
            (r#"{"type":"responsesapi.websocket_timing","timing":{"duration_ms":100}}"#, false),
            (r#"{"type":"response.created","response":{"id":"resp_123","status":"in_progress"}}"#, false),
            (r#"{"type":"response.in_progress","response":{"id":"resp_123","tools":[{"type":"function"}]}}"#, false),
            (r#"{"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"gAAAAAB..."}}"#, false),
            (r#"{"type":"response.content_part.added","part":{"type":"text","text":""}}"#, false),
            (r#"{"type":"response.reasoning_summary_part.added","part":{"type":"summary_text","text":""}}"#, false),
            (r#"{"type":"response.reasoning_summary_text.delta","delta":""}"#, false),
            (r#"{"type":"response.reasoning_summary_text.delta","delta":"**Inspecting**"}"#, true),
            (r#"{"type":"response.reasoning.delta","delta":"Analyzing requirements..."}"#, true),
            (r#"{"type":"response.reasoning_text.delta","delta":"Step 1: Check code"}"#, true),
            (r#"{"type":"response.output_text.delta","delta":"Hello world"}"#, true),
            (r#"{"type":"response.text.delta","delta":"Direct text chunk"}"#, true),
            (r#"{"type":"response.function_call_arguments.delta","delta":"{\"query\":\"test\"}"}"#, true),
            (r#"{"type":"response.custom_tool_call_input.delta","delta":"{\"param\":1}"}"#, true),
            (r#"{"type":"response.code_interpreter_call_code.delta","delta":"import math\n"}"#, true),
            (r#"{"type":"response.mcp_call_arguments.delta","delta":"{\"tool\":\"lookup\"}"}"#, true),
            (r#"{"type":"response.shell_call_command.added","command":"ls -la"}"#, true),
            (r#"{"type":"response.shell_call_command.added","command":""}"#, false),
            (r#"{"type":"response.shell_call_command.delta","delta":"ls -la\n"}"#, true),
            // Tool execution output is not a model token.
            (r#"{"type":"response.shell_call_output_content.delta","delta":{"stdout":"output text\n","stderr":""}}"#, false),
            (r#"{"type":"response.shell_call_output_content.done","output":[]}"#, false),
            (r#"{"type":"response.refusal.delta","delta":"I cannot fulfill this request"}"#, true),
            (r#"{"type":"response.audio.transcript.delta","delta":"Spoken text"}"#, true),
            (r#"{"type":"response.audio.delta","delta":"UklGRi..."}"#, true),
            (r#"{"type":"response.image_generation_call.partial_image","partial_image_b64":"iVBORw0KGgo..."}"#, true),
            (r#"{"type":"response.web_search_call.in_progress"}"#, false),
            (r#"{"type":"response.file_search_call.searching"}"#, false),
            (r#"{"type":"response.code_interpreter_call.interpreting"}"#, false),
            (r#"{"type":"response.mcp_call.in_progress"}"#, false),
            (r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"lookup","arguments":""}}"#, false),
            (r#"{"type":"response.output_item.done","item":{"type":"function_call","name":"lookup","arguments":"{\"q\":1}"}}"#, true),
            (r#"{"type":"response.output_item.done","item":{"type":"message","content":[]}}"#, false),
            (r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"text","text":"hello"}]}}"#, true),
            (r#"{"type":"response.completed","response":{"id":"resp_123","status":"completed"}}"#, true),
            (r#"{"type":"response.done","response":{"id":"resp_123"}}"#, true),
            (r#"{"type":"response.incomplete","response":{"id":"resp_123","status":"incomplete"}}"#, true),
            (r#"{"type":"response.failed","response":{"id":"resp_123","status":"failed"}}"#, true),
            (r#"{"type":"error","error":{"message":"overloaded","code":"rate_limit_exceeded"}}"#, true),
            (r#"data: {"type":"response.output_text.delta","delta":"Hello SSE"}"#, true),
            (r#"data: {"type":"response.created","response":{"id":"resp_sse"}}"#, false),
        ];
        for (payload, want) in cases {
            assert_eq!(is_responses_token_event(payload.as_bytes()), *want, "{payload}");
        }
    }

    /// Go: TestObserveResponsesTokenEvent_Behavior and _FirstPacketFallback.
    #[test]
    fn observe_responses_token_event_records_first_packet_then_token() {
        let r = UsageReporter::new("codex", "CodexExecutor", "gpt-5.6-luna", None, None);
        r.start_response_ttft();
        assert!(!r.is_ttft_set());
        observe_responses_token_event(&r, br#"{"type":"codex.rate_limits","rate_limits":{"plan_type":"pro"}}"#);
        assert!(!r.is_ttft_set() && r.is_first_packet_set());
        observe_responses_token_event(&r, br#"{"type":"response.created","response":{"id":"resp_1"}}"#);
        assert!(!r.is_ttft_set());
        std::thread::sleep(std::time::Duration::from_millis(5));
        observe_responses_token_event(&r, br#"{"type":"response.output_text.delta","delta":"First word"}"#);
        assert!(r.is_ttft_set());
        let first = r.current_ttft();
        assert!(first > std::time::Duration::ZERO);
        observe_responses_token_event(&r, br#"{"type":"response.output_text.delta","delta":"Second word"}"#);
        assert_eq!(r.current_ttft(), first);
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
