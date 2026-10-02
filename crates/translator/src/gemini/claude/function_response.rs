//! Gemini functionResponse helper shared by the claude and interactions converters.

use cpa_json::{Res, Value};

use crate::common::contains_json_ref;

/// Sets a Gemini functionResponse `path` (`...response` or `...response.result`) from JSON source
/// text the way Go's `SetGeminiFunctionResponseRaw` does, but keeping the exact text when the value
/// contains a `$ref` (which is stored as an opaque string). Blank or invalid text sets `""`.
pub(crate) fn set_function_response_from_text(part: &mut Value, path: &str, text: &str) {
    let text = text.trim();
    if text.is_empty() || !cpa_json::valid(text.as_bytes()) {
        cpa_json::set(part, path, "");
        return;
    }
    let parsed = cpa_json::parse_str(text);
    if contains_json_ref(&Res::of(&parsed)) {
        let target = if path.ends_with("response") { format!("{path}.result") } else { path.to_string() };
        cpa_json::set(part, &target, text);
    } else {
        cpa_json::set(part, path, parsed);
    }
}
