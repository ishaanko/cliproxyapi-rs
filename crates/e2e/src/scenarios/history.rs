//! Multi-turn requests that carry reasoning from a previous turn (Claude thinking signatures,
//! Gemini thought signatures, Codex encrypted reasoning, OpenAI reasoning_content) to every
//! upstream family: shows what each executor keeps, drops or sanitizes.

use serde_json::{Value, json};

use super::bodies::{self, FAMILIES, Family};
use crate::client::{HttpReq, Step as Req};
use crate::mock::script::{Content, Script};
use crate::scenario::Scenario;

/// A structurally valid Codex reasoning blob (0x80 prefix, 73 bytes, base64url).
const ENCRYPTED: &str = "gAAAAAAAAAAAAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs8PT4_QA==";

fn schema() -> Value {
    json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]})
}

fn chat(model: &str) -> Value {
    json!({
        "model": model,
        "reasoning_effort": "high",
        "messages": [
            {"role": "user", "content": "Weather in Paris?"},
            {"role": "assistant", "content": "Checking.", "reasoning_content": "The user wants weather; call the tool.",
             "tool_calls": [{"id": "call_abc123", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]},
            {"role": "tool", "tool_call_id": "call_abc123", "content": "18C"},
            {"role": "user", "content": "And Rome?"}
        ],
        "tools": [{"type": "function", "function": {"name": "get_weather", "parameters": schema()}}]
    })
}

fn responses(model: &str) -> Value {
    json!({
        "model": model,
        "reasoning": {"effort": "high"},
        "include": ["reasoning.encrypted_content"],
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Weather in Paris?"}]},
            {"type": "reasoning", "id": "rs_abc123", "summary": [{"type": "summary_text", "text": "Need the weather tool."}], "encrypted_content": ENCRYPTED},
            {"type": "function_call", "call_id": "call_abc123", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
            {"type": "function_call_output", "call_id": "call_abc123", "output": "18C"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "And Rome?"}]}
        ],
        "tools": [{"type": "function", "name": "get_weather", "parameters": schema()}]
    })
}

fn claude(model: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 4096,
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "messages": [
            {"role": "user", "content": "Weather in Paris?"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "The user wants weather; call the tool.", "signature": "EqQBCkYIBxgCKkDfakeSignatureForTheE2EHarnessOnly=="},
                {"type": "redacted_thinking", "data": "ZmFrZS1yZWRhY3RlZC1ibG9i"},
                {"type": "tool_use", "id": "toolu_abc123", "name": "get_weather", "input": {"city": "Paris"}}
            ]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_abc123", "content": "18C"}]},
            {"role": "user", "content": "And Rome?"}
        ],
        "tools": [{"name": "get_weather", "input_schema": schema()}]
    })
}

fn gemini() -> Value {
    json!({
        "contents": [
            {"role": "user", "parts": [{"text": "Weather in Paris?"}]},
            {"role": "model", "parts": [
                {"text": "The user wants weather; call the tool.", "thought": true, "thoughtSignature": "ZmFrZVRob3VnaHRTaWduYXR1cmU="},
                {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}, "thoughtSignature": "ZmFrZUNhbGxTaWduYXR1cmU="}
            ]},
            {"role": "user", "parts": [{"functionResponse": {"name": "get_weather", "response": {"result": "18C"}}}]},
            {"role": "user", "parts": [{"text": "And Rome?"}]}
        ],
        "tools": [{"functionDeclarations": [{"name": "get_weather", "parameters": schema()}]}],
        "generationConfig": {"thinkingConfig": {"thinkingBudget": 1024, "includeThoughts": true}}
    })
}

fn request(dialect: &str, f: Family) -> HttpReq {
    match dialect {
        "chat" => HttpReq::post("/v1/chat/completions", chat(f.model())),
        "responses" => HttpReq::post("/v1/responses", responses(f.model())),
        "claude" => HttpReq::post("/v1/messages", claude(f.model())),
        _ => HttpReq::post(&bodies::gemini_path(f.model(), "generateContent"), gemini()),
    }
}

pub fn scenarios() -> Vec<Scenario> {
    let mut out = vec![];
    for dialect in ["chat", "responses", "claude", "gemini"] {
        for f in FAMILIES {
            out.push(Scenario::new(
                format!("history.thinking.{dialect}.{}", f.label()),
                format!("turn carrying previous reasoning: {dialect} client -> {} upstream", f.label()),
                Script::ok(Content::Thinking),
                vec![Req::Http(request(dialect, f))],
            ));
        }
    }
    out
}
