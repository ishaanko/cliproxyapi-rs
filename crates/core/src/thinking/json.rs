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

#[cfg(test)]
mod tests {
    use super::*;

    /// gjson `Valid` accepts unpaired surrogate escapes and invalid UTF-8 inside strings, so
    /// these bodies are kept and parsed rather than replaced with `{}`.
    #[test]
    fn lenient_strings_are_valid_bodies() {
        let bodies: [&[u8]; 4] = [b"{\"a\":\"\\ud800\"}", b"{\"a\":\"\\udc00x\"}", b"{\"a\":\"\xff\"}", b"{\"a\":\"\xed\xa0\x80\"}"];
        for body in bodies {
            assert_eq!(body_or_empty_object(body), body);
            assert!(parse_valid(body).is_some_and(|v| v.g("a").exists()), "{body:?}");
        }
        assert_eq!(body_or_empty_object(b"{\"a\":\"\\ud8\"}"), b"{}");
    }
}
