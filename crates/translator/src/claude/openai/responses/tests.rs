use cpa_json::{J, parse};

use super::tool_names::build_claude_tool_names;
use super::*;
use crate::registry::{Ctx, Param};

// Compat mode keeps an unsigned (or foreign-signed) reasoning item as a thinking block and keeps
// a trailing assistant prefill that the default mode drops for Fable.
#[test]
fn compat_keeps_foreign_reasoning_and_prefill() {
    let empty = br#"{"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"reason"}],"encrypted_content":""}]}"#;
    let plain = parse(&convert_openai_responses_request_to_claude("deepseek-v4", empty, false));
    assert_eq!(plain.g("messages.#").int(), 0);
    let compat = parse(&convert_openai_responses_request_to_claude_with_compat("deepseek-v4", empty, false));
    assert_eq!(compat.g("messages.0.content.0.type").str(), "thinking");
    assert_eq!(compat.g("messages.0.content.0.signature").str(), "");

    let opaque = br#"{"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"reason"}],"encrypted_content":"opaque-deepseek-id"}]}"#;
    let out = parse(&convert_openai_responses_request_to_claude_with_compat("deepseek-v4", opaque, false));
    assert_eq!(out.g("messages.0.content.0.thinking").str(), "reason");
    assert_eq!(out.g("messages.0.content.0.signature").str(), "opaque-deepseek-id");

    let prefill = br#"{"input":[
        {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]},
        {"type":"message","role":"assistant","content":[{"type":"output_text","text":"prefill text"}]}]}"#;
    let dropped = parse(&convert_openai_responses_request_to_claude("claude-fable-5", prefill, false));
    assert_eq!(dropped.g("messages.#").int(), 1);
    let kept = parse(&convert_openai_responses_request_to_claude_with_compat("claude-fable-5", prefill, false));
    assert_eq!(kept.g("messages.1.role").str(), "assistant");
}

// Declared tools keep valid names; a history-only name that sanitizes to a taken name gets a hash
// suffix; every Claude name is reversible and unique.
#[test]
fn tool_names_round_trip_and_declared_precede_history() {
    let req = parse(
        br#"{"tools":[
            {"type":"function","name":"tool_x"},
            {"type":"function","name":"very_long_tool_name_that_exceeds_sixty_four_characters_and_needs_truncation_alpha"},
            {"type":"function","name":"very_long_tool_name_that_exceeds_sixty_four_characters_and_needs_truncation_beta"},
            {"type":"namespace","name":"mcp__my_server","tools":[{"type":"function","name":"get_something"}]}],
          "input":[{"type":"function_call","name":"tool.x"}]}"#,
    );
    let names = build_claude_tool_names(&req);
    assert_eq!(names.claude_name("tool_x"), "tool_x");
    assert_eq!(names.claude_name("mcp__my_server__get_something"), "mcp__my_server__get_something");
    let history = names.claude_name("tool.x");
    assert!(history.starts_with("tool_x_") && history != "tool_x", "{history}");
    for id in ["tool_x", "tool.x", "mcp__my_server__get_something"] {
        assert_eq!(names.identity(&names.claude_name(id)), id);
    }
    let alpha = names.claude_name("very_long_tool_name_that_exceeds_sixty_four_characters_and_needs_truncation_alpha");
    let beta = names.claude_name("very_long_tool_name_that_exceeds_sixty_four_characters_and_needs_truncation_beta");
    assert_ne!(alpha, beta);
    assert!(alpha.len() <= 64 && beta.len() <= 64);
}

// An apply_patch stream that ends before message_stop fails the response, and the error is
// visible on the Param so the registry suppresses passthrough.
#[test]
fn finalize_tool_input_fails_unterminated_apply_patch_stream() {
    let request = br#"{"tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"start: patch"}}]}"#;
    let mut param = Param::default();
    let line = br#"data: {"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":1}}}"#;
    convert_claude_response_to_openai_responses(&Ctx::default(), "m", request, b"", line, &mut param);
    assert!(param.tool_input_error.is_none());

    let events = finalize_tool_input(&mut param);
    assert_eq!(events.len(), 1);
    assert!(String::from_utf8_lossy(&events[0]).starts_with("event: response.failed\n"));
    assert!(param.tool_input_error.is_some());
    assert!(finalize_tool_input(&mut param).is_empty());
}
