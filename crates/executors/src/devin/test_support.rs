//! Shared test helpers: the golden file recorded from the Go implementation.
//!
//! `testdata/golden.json` holds inputs and the outputs of the Go functions for them: wire
//! encoding (hex), system prompt sanitizing, trailer mapping, model uid resolution, payload
//! parsing, signature handling, uuid normalization and complete frame streams (interactions
//! events, translated chunks and non-stream responses per client format). The per-module test
//! files replay the inputs through the Rust port and compare.

use std::sync::LazyLock;

use serde_json::Value;

use super::wire::{Image, Prompt, Tool, ToolCall};

static GOLDEN: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("testdata/golden.json")).expect("golden.json parses"));

pub fn golden() -> &'static Value {
    &GOLDEN
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// A prompt from the golden JSON shape.
pub fn prompt_from_json(p: &Value) -> Prompt {
    use base64::Engine as _;
    Prompt {
        message_id: s(&p["message_id"]),
        source: p["source"].as_i64().unwrap_or(0) as i32,
        content: s(&p["content"]),
        images: p["images"]
            .as_array()
            .map(|a| {
                a.iter().map(|i| Image { base64_data: s(&i["base64_data"]), mime_type: s(&i["mime_type"]) }).collect()
            })
            .unwrap_or_default(),
        tool_calls: p["tool_calls"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|t| ToolCall { id: s(&t["id"]), name: s(&t["name"]), arguments: s(&t["arguments"]) })
                    .collect()
            })
            .unwrap_or_default(),
        tool_call_id: s(&p["tool_call_id"]),
        original_tool_call_id: s(&p["original_tool_call_id"]),
        is_orphaned_tool: p["is_orphaned_tool"].as_bool().unwrap_or(false),
        thinking: s(&p["thinking"]),
        signature: base64::engine::general_purpose::STANDARD.decode(s(&p["signature_b64"])).unwrap_or_default(),
        signature_type: s(&p["signature_type"]),
    }
}

pub fn tool_from_json(t: &Value) -> Tool {
    Tool { name: s(&t["name"]), description: s(&t["description"]), parameters: s(&t["parameters"]).into_bytes() }
}

/// Masks per-run random values of Go and Rust outputs (interaction ids, timestamps) and the
/// library-specific detail of gzip failures. Go's sjson HTML-escapes `<`, `>` and `&` in strings
/// that also hold non-ASCII text; serde does not, so both escapes are folded to the plain
/// character (the JSON values are identical).
pub fn normalize(text: &str) -> String {
    use regex::Regex;
    static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"interaction_[0-9a-f]{8}-[0-9a-f]{3}").expect("regex"));
    static CREATED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""created":\d+"#).expect("regex"));
    static GZIP: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(decompress gzip connect frame:)[^\n]*").expect("regex"));
    let text = GZIP.replace_all(text, "$1");
    let text = ID.replace_all(&text, "interaction_ID");
    CREATED
        .replace_all(&text, r#""created":0"#)
        .replace("\\u003c", "<")
        .replace("\\u003e", ">")
        .replace("\\u0026", "&")
}
