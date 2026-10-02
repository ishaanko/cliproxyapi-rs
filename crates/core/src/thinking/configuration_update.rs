//! Responses `configuration_update` input items (Go: internal/thinking/configuration_update.go).

use cpa_json::{J, Value};

use super::json::{delete_if_empty_object, parse_valid, to_bytes};
use super::types::{ThinkingConfig, level};

/// Whether `format` is a Responses-style source (`codex` or `openai-response`).
pub(crate) fn is_responses_format(format: &str) -> bool {
    format == "codex" || format == "openai-response"
}

/// The last nonempty `input[].configuration_update.reasoning.effort`, as a canonical config.
pub(crate) fn extract_configuration_update_config(body: &[u8]) -> ThinkingConfig {
    match parse_valid(body) {
        Some(v) => configuration_update_config(&v),
        None => ThinkingConfig::default(),
    }
}

pub(crate) fn configuration_update_config(v: &Value) -> ThinkingConfig {
    let input = v.g("input");
    let Value::Array(items) = input.v().unwrap_or(&Value::Null) else {
        return ThinkingConfig::default();
    };

    let mut effort = String::new();
    for item in items {
        if item.g("type").str() == "configuration_update"
            && let Some(value) = item.g("reasoning.effort").as_str()
        {
            let normalized = value.trim().to_lowercase();
            if !normalized.is_empty() {
                effort = normalized;
            }
        }
    }
    match effort.as_str() {
        "" => ThinkingConfig::default(),
        level::NONE => ThinkingConfig::none(),
        level::AUTO => ThinkingConfig::auto(),
        _ => ThinkingConfig::level(effort),
    }
}

/// Removes `configuration_update` items from `input`, leaving other items and the absence of
/// `input` untouched.
pub(crate) fn strip_configuration_updates(body: &[u8]) -> Vec<u8> {
    let Some(mut v) = parse_valid(body) else {
        return body.to_vec();
    };
    let Some(Value::Array(items)) = cpa_json::get_mut(&mut v, "input") else {
        return body.to_vec();
    };
    let before = items.len();
    items.retain(|item| item.g("type").str() != "configuration_update");
    if items.len() == before {
        return body.to_vec();
    }
    to_bytes(&v)
}

/// Deletes `reasoning.effort` (and an emptied `reasoning`), leaving summary and unrelated
/// reasoning fields intact.
pub(crate) fn strip_responses_effort(body: &[u8]) -> Vec<u8> {
    let Some(mut v) = parse_valid(body) else {
        return body.to_vec();
    };
    if !v.g("reasoning.effort").exists() {
        return body.to_vec();
    }
    cpa_json::delete(&mut v, "reasoning.effort");
    delete_if_empty_object(&mut v, "reasoning");
    to_bytes(&v)
}
