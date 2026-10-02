//! Codex client tool integer normalization (Go: internal/client/codex/tool-schema
//! `IsCodexUserAgent` / `NormalizeCodexToolIntegerTypes`, called from helps/payload_helpers.go).
//!
//! Codex clients deserialize some tool parameters as integers; schemas that declare them as
//! `number` (for example after a Gemini round trip) are rewritten to `integer` for a fixed set
//! of well-known tools. Only the payload pipeline needs this, so the rest of the Go tool-schema
//! package stays with the Codex executor.

use std::collections::HashSet;

use http::HeaderMap;
use serde_json::Value;

/// Whether the `User-Agent` header (first non-blank value) names a Codex client.
pub fn is_codex_user_agent(headers: Option<&HeaderMap>) -> bool {
    let Some(headers) = headers else {
        return false;
    };
    headers
        .get_all(http::header::USER_AGENT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::trim)
        .find(|v| !v.is_empty())
        .is_some_and(|ua| ua.to_lowercase().contains("codex"))
}

fn target_fields(tool_name: &str) -> &'static [&'static str] {
    let name = tool_name.trim();
    let name = name
        .strip_prefix("functions__")
        .or_else(|| name.strip_prefix("collab__"))
        .unwrap_or(name);
    match name {
        "exec_command" => &["yield_time_ms", "max_output_tokens", "timeout_ms"],
        "write_stdin" => &["session_id", "yield_time_ms", "max_output_tokens"],
        "sleep" => &["duration_ms"],
        "wait_agent" => &["timeout_ms"],
        "wait" => &["yield_time_ms", "max_tokens"],
        "tool_search" => &["limit"],
        "test_sync_tool" => &["sleep_before_ms", "sleep_after_ms", "participants", "timeout_ms"],
        _ => &[],
    }
}

/// Rewrites `number` to `integer` in the declared types of `fields`; true when anything changed.
fn normalize_field_types(params: &mut Value, fields: &[&str]) -> bool {
    let Some(properties) = params.get_mut("properties").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    for field in fields {
        let Some(type_val) = properties.get_mut(*field).and_then(|p| p.get_mut("type")) else {
            continue;
        };
        match type_val {
            Value::String(s) if s == "number" => {
                *s = "integer".into();
                changed = true;
            }
            Value::Array(items) => {
                let mut has_number = false;
                let mut seen: HashSet<String> = HashSet::new();
                let mut next = Vec::with_capacity(items.len());
                for item in items.iter() {
                    // Go's gjson `String()` stringifies non-string items.
                    let mut s = match item {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    if s == "number" {
                        has_number = true;
                        s = "integer".into();
                    }
                    if seen.insert(s.clone()) {
                        next.push(Value::String(s));
                    }
                }
                if has_number {
                    *type_val = Value::Array(next);
                    changed = true;
                }
            }
            _ => {}
        }
    }
    changed
}

fn normalize_array(tools: &mut Value) -> bool {
    let Some(items) = tools.as_array_mut() else {
        return false;
    };
    let mut changed = false;
    for tool in items {
        changed |= normalize_element(tool);
    }
    changed
}

fn normalize_element(tool: &mut Value) -> bool {
    if tool.get("type").and_then(Value::as_str) == Some("namespace") {
        return tool.get_mut("tools").is_some_and(normalize_array);
    }
    for key in ["function_declarations", "functionDeclarations"] {
        if tool.get(key).is_some_and(Value::is_array) {
            return tool.get_mut(key).is_some_and(normalize_array);
        }
    }
    let name = tool.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let is_object = |v: Option<&Value>| v.is_some_and(Value::is_object);
    let (path, name): (&[&str], String) = if is_object(tool.get("parameters")) {
        (&["parameters"], name)
    } else if is_object(tool.pointer("/function/parameters")) {
        let name = if name.is_empty() {
            tool.pointer("/function/name").and_then(Value::as_str).unwrap_or("").to_string()
        } else {
            name
        };
        (&["function", "parameters"], name)
    } else if is_object(tool.get("input_schema")) {
        (&["input_schema"], name)
    } else if is_object(tool.get("parametersJsonSchema")) {
        (&["parametersJsonSchema"], name)
    } else {
        return false;
    };
    let fields = target_fields(&name);
    if fields.is_empty() {
        return false;
    }
    let mut cur = tool;
    for key in path {
        match cur.get_mut(*key) {
            Some(next) => cur = next,
            None => return false,
        }
    }
    normalize_field_types(cur, fields)
}

/// Normalizes the well-known tool parameters to integers across `tools` and
/// `input[].additional_tools`, for Codex clients only. Returns `body` untouched (same bytes)
/// when the request is not from Codex or nothing changed.
pub fn normalize_codex_tool_integer_types(body: &[u8], headers: Option<&HeaderMap>) -> Vec<u8> {
    if body.is_empty() || !is_codex_user_agent(headers) {
        return body.to_vec();
    }
    let mut root = cpa_json::parse(body);
    if !root.is_object() {
        return body.to_vec();
    }
    let mut changed = false;
    if let Some(tools) = root.get_mut("tools") {
        changed |= normalize_array(tools);
    }
    if let Some(items) = root.get_mut("input").and_then(Value::as_array_mut) {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("additional_tools")
                && let Some(tools) = item.get_mut("tools")
            {
                changed |= normalize_array(tools);
            }
        }
    }
    if !changed {
        return body.to_vec();
    }
    tracing::debug!("codex: normalized target tool number types to integer");
    cpa_json::to_vec(&root)
}
