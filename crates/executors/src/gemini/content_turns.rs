//! Gemini `contents` turn-shape fixes (Go: helps/gemini_content_turns.go).
//!
//! Gemini and Antigravity upstreams reject histories that start or end with a model turn. These
//! helpers prepend or append an empty user turn at `path` (`contents`, or `request.contents` for
//! the Antigravity envelope). Payloads that need no change come back byte-identical.

use cpa_json::{J, Value, json};

fn empty_user_turn() -> Value {
    json!({"role": "user", "parts": [{"text": ""}]})
}

fn has_function_response(content: &Value) -> bool {
    match content.get("parts") {
        Some(Value::Array(parts)) => parts.iter().any(|part| part.g("functionResponse").exists()),
        _ => false,
    }
}

/// In-place leading fix on a parsed payload; true when it changed.
pub fn ensure_leading_user_content_value(payload: &mut Value, path: &str) -> bool {
    if payload.g(&format!("{path}.0.role")).str() != "model" {
        return false;
    }
    let Some(Value::Array(items)) = cpa_json::get_mut(payload, path) else {
        return false;
    };
    if items.is_empty() {
        return false;
    }
    items.insert(0, empty_user_turn());
    true
}

/// In-place trailing fix on a parsed payload; true when it changed. A final turn that carries a
/// `functionResponse` is kept as is because the model is expected to answer it.
pub fn ensure_trailing_user_content_value(payload: &mut Value, path: &str) -> bool {
    let Some(Value::Array(items)) = cpa_json::get_mut(payload, path) else {
        return false;
    };
    let Some(last) = items.last() else {
        return false;
    };
    let role = last.g("role").str();
    if (role != "model" && role != "assistant") || has_function_response(last) {
        return false;
    }
    items.push(empty_user_turn());
    true
}

fn rewrite(payload: &[u8], f: impl FnOnce(&mut Value) -> bool) -> Vec<u8> {
    let mut v = cpa_json::parse(payload);
    if f(&mut v) { cpa_json::to_vec(&v) } else { payload.to_vec() }
}

/// Go: EnsureGeminiLeadingUserContent.
pub fn ensure_gemini_leading_user_content(payload: &[u8], path: &str) -> Vec<u8> {
    rewrite(payload, |v| ensure_leading_user_content_value(v, path))
}

/// Go: EnsureGeminiTrailingUserContent.
pub fn ensure_gemini_trailing_user_content(payload: &[u8], path: &str) -> Vec<u8> {
    rewrite(payload, |v| ensure_trailing_user_content_value(v, path))
}

/// Go: EnsureGeminiBoundaryUserContent (leading then trailing).
pub fn ensure_gemini_boundary_user_content(payload: &[u8], path: &str) -> Vec<u8> {
    rewrite(payload, |v| {
        let leading = ensure_leading_user_content_value(v, path);
        let trailing = ensure_trailing_user_content_value(v, path);
        leading || trailing
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(b: Vec<u8>) -> String {
        String::from_utf8(b).unwrap()
    }

    #[test]
    fn leading_prepends_only_when_first_turn_is_model() {
        let payload = br#"{"contents":[{"role":"model","parts":[{"text":"hi"}]}]}"#;
        assert_eq!(
            s(ensure_gemini_leading_user_content(payload, "contents")),
            r#"{"contents":[{"role":"user","parts":[{"text":""}]},{"role":"model","parts":[{"text":"hi"}]}]}"#
        );
        let user_first = br#"{"contents":[{"role":"user","parts":[{"text":"x"}]}]}"#;
        assert_eq!(ensure_gemini_leading_user_content(user_first, "contents"), user_first.to_vec());
        assert_eq!(ensure_gemini_leading_user_content(b"{}", "contents"), b"{}".to_vec());
    }

    #[test]
    fn trailing_skips_function_responses_and_honors_nested_path() {
        let ends_model = br#"{"request":{"contents":[{"role":"user","parts":[{"text":"x"}]},{"role":"model","parts":[{"text":"y"}]}]}}"#;
        let out = s(ensure_gemini_trailing_user_content(ends_model, "request.contents"));
        assert!(out.ends_with(r#"{"role":"user","parts":[{"text":""}]}]}}"#), "{out}");
        let fn_resp = br#"{"contents":[{"role":"model","parts":[{"functionResponse":{"name":"f"}}]}]}"#;
        assert_eq!(ensure_gemini_trailing_user_content(fn_resp, "contents"), fn_resp.to_vec());
        let assistant = br#"{"contents":[{"role":"assistant","parts":[{"text":"y"}]}]}"#;
        assert_ne!(ensure_gemini_trailing_user_content(assistant, "contents"), assistant.to_vec());
    }

    #[test]
    fn boundary_applies_both_ends() {
        let payload = br#"{"contents":[{"role":"model","parts":[{"text":"y"}]}]}"#;
        let out = cpa_json::parse(&ensure_gemini_boundary_user_content(payload, "contents"));
        assert_eq!(out.g("contents.#").int(), 3);
        assert_eq!(out.g("contents.0.role").str(), "user");
        assert_eq!(out.g("contents.2.role").str(), "user");
    }
}
