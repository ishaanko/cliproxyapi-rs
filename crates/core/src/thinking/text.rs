//! Go: internal/thinking/text.go.

use cpa_json::{J, Value};

/// Extracts thinking text from a content part. Handles Gemini-style `{"thought":true,"text":..}`,
/// `{"thinking":"text"}` and the wrapped `{"thinking":{"text"|"thinking":..}}` forms.
pub fn get_thinking_text(part: &Value) -> String {
    if let Some(text) = part.g("text").as_str() {
        return text.to_owned();
    }
    let field = part.g("thinking");
    if !field.exists() {
        return String::new();
    }
    if let Some(s) = field.as_str() {
        return s.to_owned();
    }
    if field.is_object() {
        if let Some(s) = field.g("text").as_str() {
            return s.to_owned();
        }
        if let Some(s) = field.g("thinking").as_str() {
            return s.to_owned();
        }
    }
    String::new()
}
