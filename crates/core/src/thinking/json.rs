//! Small body helpers shared by the thinking modules.

use cpa_json::{J, Value};

/// Parses a request body; `None` for empty or invalid JSON (Go: `len(body)==0 || !ValidBytes`).
pub(crate) fn parse_valid(body: &[u8]) -> Option<Value> {
    if body.is_empty() {
        return None;
    }
    cpa_json::parse_valid(body)
}

/// Parses a body for writing; empty or invalid JSON becomes `{}` as every applier does.
pub(crate) fn parse_or_empty_object(body: &[u8]) -> Value {
    parse_valid(body).unwrap_or_else(|| Value::Object(Default::default()))
}

/// Deletes `path` when it holds an empty JSON object (Go: `oc.IsObject() && len(oc.Map())==0`).
pub(crate) fn delete_if_empty_object(v: &mut Value, path: &str) {
    let r = v.g(path);
    if matches!(r.v(), Some(Value::Object(m)) if m.is_empty()) {
        cpa_json::delete(v, path);
    }
}

/// The body unchanged when it is valid JSON, else `{}`; what Go's appliers return on early exits
/// after their empty/invalid-body fallback.
pub(crate) fn body_or_empty_object(body: &[u8]) -> Vec<u8> {
    if !body.is_empty() && cpa_json::valid(body) {
        body.to_vec()
    } else {
        b"{}".to_vec()
    }
}
