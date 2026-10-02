//! Small JSON helpers for the xAI request/response normalizers (gjson/sjson idioms over `Value`).

use cpa_json::J;
use serde_json::Value;

/// The value at a plain dotted path (object keys and numeric array indexes only, no queries).
pub fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for key in path.split('.') {
        cur = match cur {
            Value::Object(m) => m.get(key)?,
            Value::Array(a) => a.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// gjson `Get(path).String()`.
pub fn s(v: &Value, path: &str) -> String {
    v.g(path).str()
}

/// `strings.TrimSpace(gjson.Get(path).String())`.
pub fn ts(v: &Value, path: &str) -> String {
    v.g(path).str().trim().to_string()
}

/// Elements of the array at `path`; empty when missing or not an array.
pub fn items<'a>(v: &'a Value, path: &str) -> &'a [Value] {
    at(v, path).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// Whether the value at `path` exists and is an array.
pub fn is_array_at(v: &Value, path: &str) -> bool {
    at(v, path).is_some_and(Value::is_array)
}

/// Whether the value at `path` exists (a JSON `null` counts, like gjson `Exists()`).
pub fn exists(v: &Value, path: &str) -> bool {
    at(v, path).is_some()
}

/// Compact JSON text.
pub fn compact(v: &Value) -> String {
    cpa_json::to_string(v)
}
