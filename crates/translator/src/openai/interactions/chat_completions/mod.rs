//! Port of internal/translator/openai/interactions/chat-completions.
//!
//! Two directions share this module:
//! - client OpenAI chat -> upstream Interactions (`openai_interactions_*`)
//! - client Interactions -> upstream OpenAI chat (`interactions_openai_*`)

mod interactions_openai_request;
mod interactions_openai_response;
mod openai_interactions_request;
mod openai_interactions_response;

use crate::common::{first_existing, first_non_blank, unix_nano_now};
use cpa_core::format::Format;
use cpa_json::{Res, Value};

use crate::registry::{Registry, ResponseFns};

pub fn register(r: &mut Registry) {
    r.register(
        Format::OpenAI,
        Format::Interactions,
        Some(openai_interactions_request::convert_openai_request_to_interactions),
        ResponseFns {
            stream: Some(openai_interactions_response::convert_interactions_response_to_openai),
            non_stream: Some(openai_interactions_response::convert_interactions_response_to_openai_non_stream),
            token_count: None,
            finalize: None,
        },
    );
    r.register(
        Format::Interactions,
        Format::OpenAI,
        Some(interactions_openai_request::convert_interactions_request_to_openai),
        ResponseFns {
            stream: Some(interactions_openai_response::convert_openai_response_to_interactions),
            non_stream: Some(interactions_openai_response::convert_openai_response_to_interactions_non_stream),
            token_count: None,
            finalize: None,
        },
    );
}

// Helpers shared by the four files (Go: package-level funcs).

/// Sets `path` to a copy of `value` when it exists (Go: `copyNumber`, a raw copy of any value).
fn copy_number(out: &mut Value, path: &str, value: &Res<'_>) {
    if value.exists() {
        cpa_json::set(out, path, value.value());
    }
}

/// Text of a value for string-typed fields: strings verbatim, other values as JSON text,
/// `fallback` when missing.
fn json_string_value(value: &Res<'_>, fallback: &str) -> String {
    if !value.exists() {
        return fallback.to_string();
    }
    match value.as_str() {
        Some(s) => s.to_string(),
        None => value.raw(),
    }
}

/// Sets the array at `path` to `items`; no-op for an empty list (Go: `SetRawArrayItems`).
fn set_items(out: &mut Value, path: &str, items: Vec<Value>) {
    if !items.is_empty() {
        cpa_json::set(out, path, Value::Array(items));
    }
}

fn is_antigravity_model(model: &str) -> bool {
    model.to_lowercase().contains("antigravity")
}

/// `{"type":<step_type>,"content":[{"type":"text","text":<text>}]}`.
fn interactions_text_step(step_type: &str, text: &str) -> Value {
    let mut step = cpa_json::parse_str(r#"{"type":"","content":[{"type":"text","text":""}]}"#);
    cpa_json::set(&mut step, "type", step_type);
    cpa_json::set(&mut step, "content.0.text", text);
    step
}

/// Texts of an OpenAI `reasoning_content` value (string, or array of `{text|content}`).
fn openai_reasoning_texts(reasoning: &Res<'_>) -> Vec<String> {
    if let Some(s) = reasoning.as_str() {
        return if s.is_empty() { vec![] } else { vec![s.to_string()] };
    }
    if !reasoning.is_array() {
        return vec![];
    }
    reasoning
        .array()
        .iter()
        .map(|item| first_non_blank(&[&item.g("text").str(), &item.g("content").str()]))
        .filter(|text| !text.is_empty())
        .collect()
}

/// Strips an SSE `data:` prefix, or joins the `data:` lines of a multi-line block; other input is
/// returned trimmed (Go: `openAIChatSSEPayload` / `openAIChatInteractionsPayload`).
fn sse_payload(raw: &[u8]) -> Vec<u8> {
    let trimmed = raw.trim_ascii();
    if trimmed.is_empty() || trimmed == b"[DONE]" {
        return trimmed.to_vec();
    }
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        return rest.trim_ascii().to_vec();
    }
    let data_lines: Vec<&[u8]> = trimmed
        .split(|b| *b == b'\n')
        .map(<[u8]>::trim_ascii)
        .filter_map(|line| line.strip_prefix(b"data:"))
        .map(<[u8]>::trim_ascii)
        .collect();
    if data_lines.is_empty() {
        return trimmed.to_vec();
    }
    data_lines.join(&b'\n')
}

