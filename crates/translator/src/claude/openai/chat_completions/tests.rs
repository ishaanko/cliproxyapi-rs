use cpa_json::{J, parse};

use super::*;

// Compat mode keeps assistant reasoning_content as an unsigned thinking block, ahead of text and
// tool_use, in one grouped assistant message. The registered transform never does.
#[test]
fn compat_preserves_reasoning_content() {
    let payload = br#"{"messages":[
        {"role":"assistant","reasoning_content":"reason","content":"answer"},
        {"role":"assistant","content":"","tool_calls":[
            {"id":"call_1","type":"function","function":{"name":"first","arguments":"{}"}},
            {"id":"call_2","type":"function","function":{"name":"second","arguments":"{}"}}]}]}"#;

    let plain = parse(&convert_openai_request_to_claude("claude-test", payload, false));
    assert!(!plain.g("messages.0.content.#(type=thinking)").exists());

    let out = parse(&convert_openai_request_to_claude_with_compat("claude-test", payload, false));
    assert_eq!(out.g("messages.#").int(), 1);
    let types: Vec<String> = out.g("messages.0.content").array().iter().map(|p| p.g("type").str()).collect();
    assert_eq!(types, ["thinking", "text", "tool_use", "tool_use"]);
    assert_eq!(out.g("messages.0.content.0.thinking").str(), "reason");
    assert_eq!(out.g("messages.0.content.0.signature").str(), "");
}

// A repeated tool_call_id is emitted once, at its first position, with the last content; a tool
// message without an id gets a generated id and joins the same user turn. (The corpus case for
// this input cannot pass the harness: the fallback id embeds a nanosecond timestamp.)
#[test]
fn duplicate_and_idless_tool_results() {
    let payload = br#"{"messages":[
        {"role":"user","content":"Run tools"},
        {"role":"assistant","tool_calls":[{"id":"call_dup","type":"function","function":{"name":"lookup","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"call_dup","content":"first output"},
        {"role":"assistant","content":"Next step","tool_calls":[{"id":"call_other","type":"function","function":{"name":"search","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"call_dup","content":"final output"},
        {"role":"tool","tool_call_id":"call_other","content":"search output"},
        {"role":"tool","tool_call_id":"","content":"empty id output"}]}"#;
    let out = parse(&convert_openai_request_to_claude("claude-test", payload, false));
    assert_eq!(out.g("messages.#").int(), 5);
    assert_eq!(out.g("messages.2.content.0.tool_use_id").str(), "call_dup");
    assert_eq!(out.g("messages.2.content.0.content").str(), "final output");
    assert_eq!(out.g("messages.4.content.#").int(), 2);
    assert_eq!(out.g("messages.4.content.0.content").str(), "search output");
    assert!(out.g("messages.4.content.1.tool_use_id").str().starts_with("toolu_"));
    assert_eq!(out.g("messages.4.content.1.content").str(), "empty id output");
}

// The structured output instruction embeds the schema text exactly as the client sent it.
#[test]
fn structured_output_keeps_schema_text_as_sent() {
    let body = b"{\"messages\":[],\"response_format\":{\"type\":\"json_schema\",\"json_schema\":{\"name\":\"s\",\"schema\":{\n  \"type\": \"object\"\n}}}}";
    let out = parse(&convert_openai_request_to_claude("claude-test", body, false));
    let text = out.g("system.0.text").str();
    assert!(text.contains("JSON Schema:\n{\n  \"type\": \"object\"\n}\nDo not include"), "{text}");
}
