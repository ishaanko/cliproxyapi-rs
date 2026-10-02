//! Recovers the client's original formatting of JSON containers that Go reads with gjson `.Raw`
//! and forwards as strings (tool arguments and results). `Value` re-serializes compactly, so the
//! translators swap those containers for strings holding their original text (via
//! `cpa_json::raw_at`) before reading them. Also the lenient parser for truncated payloads.

use cpa_json::{J, Value};

/// Parses a payload like gjson reads it: a payload cut off before its closing brackets still
/// yields the fields that arrived. Hopeless input yields `Null`.
pub(crate) fn parse_lenient(bytes: &[u8]) -> Value {
    let parsed = cpa_json::parse(bytes);
    if !parsed.is_null() {
        return parsed;
    }
    let mut stack: Vec<u8> = Vec::new();
    let (mut in_string, mut escaped) = (false, false);
    for &b in bytes {
        if in_string {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => stack.push(b'}'),
            b'[' => stack.push(b']'),
            b'}' | b']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut fixed = bytes.trim_ascii_end().to_vec();
    if in_string {
        fixed.push(b'"');
    }
    if fixed.last() == Some(&b',') {
        fixed.pop();
    }
    fixed.extend(stack.iter().rev());
    cpa_json::parse(&fixed)
}

/// Replaces the object/array at `path` with a string holding its original text, so later
/// `json_string_value` calls see the client's formatting like gjson's `.Raw` does. `bytes` is the
/// JSON text `root` was parsed from.
fn restore_raw_text(bytes: &[u8], root: &mut Value, path: &str) {
    let target = root.g(path);
    if !target.is_object() && !target.is_array() {
        return;
    }
    if let Some(text) = cpa_json::raw_at(bytes, path) {
        let text = text.to_string();
        cpa_json::set(root, path, text);
    }
}

/// Restores the original text of Interactions function-call `arguments` objects: the event's
/// `step` and each entry of `steps` / `interaction.steps`.
pub(crate) fn restore_step_arguments(bytes: &[u8], root: &mut Value) {
    restore_raw_text(bytes, root, "step.arguments");
    for prefix in ["steps", "interaction.steps"] {
        let count = root.g(prefix).array().len();
        for i in 0..count {
            restore_raw_text(bytes, root, &format!("{prefix}.{i}.arguments"));
        }
    }
}

/// Restores the original text of `arguments`, `result` and `output` in the entries of an
/// Interactions request `input` (array of steps, or a single step object).
pub(crate) fn restore_input_raw_text(bytes: &[u8], root: &mut Value) {
    let input = root.g("input");
    let prefixes: Vec<String> = if input.is_array() {
        (0..input.array().len()).map(|i| format!("input.{i}")).collect()
    } else if input.is_object() {
        vec!["input".to_string()]
    } else {
        return;
    };
    for prefix in prefixes {
        for key in ["arguments", "result", "output"] {
            restore_raw_text(bytes, root, &format!("{prefix}.{key}"));
        }
    }
}
