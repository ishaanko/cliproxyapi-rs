//! Default Gemini safety settings (Go: gemini/common/safety.go).

use cpa_json::{J, Value, json};

/// The default Gemini safety configuration attached to requests: harassment, hate speech, sexually
/// explicit and dangerous content are `OFF`, civic integrity is `BLOCK_NONE`. Each setting's keys
/// are `category` then `threshold` (Go marshals `map[string]string`, which sorts keys).
pub fn default_safety_settings() -> Vec<Value> {
    [
        ("HARM_CATEGORY_HARASSMENT", "OFF"),
        ("HARM_CATEGORY_HATE_SPEECH", "OFF"),
        ("HARM_CATEGORY_SEXUALLY_EXPLICIT", "OFF"),
        ("HARM_CATEGORY_DANGEROUS_CONTENT", "OFF"),
        ("HARM_CATEGORY_CIVIC_INTEGRITY", "BLOCK_NONE"),
    ]
    .into_iter()
    .map(|(category, threshold)| json!({ "category": category, "threshold": threshold }))
    .collect()
}

/// Ensures the default safety settings are present: sets them at `path` (e.g. `safetySettings` or
/// `request.safetySettings`) only when that path does not exist yet. The JSON body is returned
/// unchanged when it is already set or `path` is empty.
pub fn attach_default_safety_settings(raw_json: &[u8], path: &str) -> Vec<u8> {
    let mut root = cpa_json::parse(raw_json);
    if path.is_empty() || root.g(path).exists() {
        return raw_json.to_vec();
    }
    cpa_json::set(&mut root, path, Value::Array(default_safety_settings()));
    cpa_json::to_vec(&root)
}
