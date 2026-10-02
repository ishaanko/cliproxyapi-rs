//! Raw-text aware variant of `common::set_gemini_function_response_result`.

use cpa_json::{Res, Value};

use crate::common;

/// Sets the functionResponse result at `path` like [`common::set_gemini_function_response_result`],
/// except that a result stringified because it contains a `$ref` keeps `raw_text` (the value's
/// original text, via `cpa_json::raw_at`) instead of its re-serialized form, as Go copies
/// `gjson.Result.Raw` there.
pub fn set_function_response_result(part: &mut Value, path: &str, result: &Res<'_>, raw_text: Option<&str>) {
    if let Some(text) = raw_text {
        if result.exists() && common::contains_json_ref(result) {
            let target = if path.ends_with("response") { format!("{path}.result") } else { path.to_string() };
            cpa_json::set(part, &target, text);
            return;
        }
    }
    let out = common::set_gemini_function_response_result(&cpa_json::to_vec(part), path, result);
    *part = cpa_json::parse(&out);
}
