//! Small body helpers shared by the thinking modules.

use cpa_json::{J, Value};

/// Parses a request body; `None` for empty or invalid JSON (Go: `len(body)==0 || !ValidBytes`).
pub(crate) fn parse_valid(body: &[u8]) -> Option<Value> {
    if body.is_empty() {
        return None;
    }
    serde_json::from_slice(body).ok()
}

/// Parses a body for writing; empty or invalid JSON becomes `{}` as every applier does.
pub(crate) fn parse_or_empty_object(body: &[u8]) -> Value {
    parse_valid(body).unwrap_or_else(|| Value::Object(Default::default()))
}

/// Compact serialization of a body value.
pub(crate) fn to_bytes(v: &Value) -> Vec<u8> {
    cpa_json::to_vec(v)
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
    if parse_valid(body).is_some() {
        body.to_vec()
    } else {
        b"{}".to_vec()
    }
}

/// sjson `Set` that refuses to put a named key into an array, as sjson does: the body is left
/// unchanged and the offending key is returned. (`cpa_json::set` would replace the array with an
/// object.) Paths here are plain dotted keys.
pub(crate) fn try_set(v: &mut Value, path: &str, val: impl Into<Value>) -> Result<(), String> {
    let mut cur: &Value = v;
    for key in path.split('.') {
        match cur {
            Value::Object(m) => match m.get(key) {
                Some(child) => cur = child,
                None => break,
            },
            Value::Array(a) => {
                let is_index =
                    key == "-1" || (!key.is_empty() && key.bytes().all(|b| b.is_ascii_digit()));
                if !is_index {
                    return Err(key.to_owned());
                }
                match key.parse::<usize>().ok().and_then(|i| a.get(i)) {
                    Some(child) => cur = child,
                    None => break,
                }
            }
            _ => break,
        }
    }
    cpa_json::set(v, path, val);
    Ok(())
}

/// [`try_set`] for callers that ignore failures (the Go appliers discard sjson errors).
pub(crate) fn set(v: &mut Value, path: &str, val: impl Into<Value>) {
    let _ = try_set(v, path, val);
}
