//! Port of internal/translator/openai/interactions/responses.
//!
//! Two directions share this module:
//! - client OpenAI Responses -> upstream Interactions (request: `request`, responses:
//!   `interactions_to_responses`)
//! - client Interactions -> upstream OpenAI Responses (request: `request`, responses:
//!   `responses_to_interactions`)

mod interactions_to_responses;
pub(crate) mod raw_text;
mod request;
mod responses_to_interactions;

pub use interactions_to_responses::finalize_tool_input;

use cpa_core::format::Format;
use cpa_json::{Res, Value, J};

use crate::registry::{Registry, ResponseFns};

pub fn register(r: &mut Registry) {
    r.register(
        Format::OpenAIResponse,
        Format::Interactions,
        Some(request::convert_openai_responses_request_to_interactions),
        ResponseFns {
            stream: Some(interactions_to_responses::convert_interactions_response_to_openai_responses),
            non_stream: Some(interactions_to_responses::convert_interactions_response_to_openai_responses_non_stream),
            token_count: None,
        },
    );
    r.register(
        Format::Interactions,
        Format::OpenAIResponse,
        Some(request::convert_interactions_request_to_openai_responses),
        ResponseFns {
            stream: Some(responses_to_interactions::convert_openai_responses_response_to_interactions),
            non_stream: Some(responses_to_interactions::convert_openai_responses_response_to_interactions_non_stream),
            token_count: None,
        },
    );
}

// Helpers shared by the files of this package.

/// Parses a JSON template literal.
fn tmpl(s: &str) -> Value {
    cpa_json::parse_str(s)
}

/// First value that is not blank after trimming; the original (untrimmed) string is returned.
fn first_non_empty(values: &[&str]) -> String {
    values.iter().find(|v| !v.trim().is_empty()).map(|v| (*v).to_string()).unwrap_or_default()
}

/// First lookup result that exists, or a missing result.
fn first_existing<'a>(values: impl IntoIterator<Item = Res<'a>>) -> Res<'a> {
    values.into_iter().find(Res::exists).unwrap_or(Res::NONE)
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

/// Sets `path` from a tool arguments/result value: valid JSON text is stored parsed, other strings
/// as strings, other values as-is, `default_raw` JSON when missing.
fn set_json_value(out: &mut Value, path: &str, value: &Res<'_>, default_raw: &str) {
    if !value.exists() {
        cpa_json::set(out, path, tmpl(default_raw));
        return;
    }
    if let Some(s) = value.as_str() {
        if cpa_json::valid(s.as_bytes()) {
            cpa_json::set(out, path, cpa_json::parse_str(s));
        } else {
            cpa_json::set(out, path, s);
        }
        return;
    }
    cpa_json::set(out, path, value.value());
}

/// Strips an SSE `data:` prefix, or joins the `data:` lines of a multi-line block; other input is
/// returned trimmed (Go: `interactionsSSEPayload`).
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

/// The model name for a response: the caller's, else the model in the payload.
fn response_model(model_name: &str, root: &Value) -> String {
    first_non_empty(&[model_name, &root.g("model").str(), &root.g("response.model").str(), &root.g("interaction.model").str()])
}

fn unix_nanos() -> i64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
}
