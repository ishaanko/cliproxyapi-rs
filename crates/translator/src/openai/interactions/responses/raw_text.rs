//! Recovers the client's original formatting of JSON containers that Go reads with gjson `.Raw`
//! and forwards as strings (tool arguments and results). `Value` re-serializes compactly, so the
//! translators swap those containers for strings holding their original text (via
//! `cpa_json::raw_at` / `raw_children`) before reading them.

use cpa_json::{J, Value};

/// Replaces the object/array at `path` with a string holding its original text `raw` (looked up
/// by the caller), so later `json_string_value` calls see the client's formatting like gjson's
/// `.Raw` does.
fn restore_raw_text(root: &mut Value, path: &str, raw: Option<&str>) {
    let target = root.g(path);
    if !target.is_object() && !target.is_array() {
        return;
    }
    if let Some(text) = raw {
        let text = text.to_string();
        cpa_json::set(root, path, text);
    }
}

/// `raw_at` relative to a raw item slice.
fn raw_in(item: Option<&&str>, path: &str) -> Option<String> {
    cpa_json::raw_at(item?.as_bytes(), path).map(str::to_string)
}

/// Restores the original text of Interactions function-call `arguments` objects: the event's
/// `step` and each entry of `steps` / `interaction.steps`.
pub(crate) fn restore_step_arguments(bytes: &[u8], root: &mut Value) {
    let step = cpa_json::raw_at(bytes, "step.arguments");
    restore_raw_text(root, "step.arguments", step);
    for prefix in ["steps", "interaction.steps"] {
        let count = root.g(prefix).array().len();
        let raws = cpa_json::raw_children(bytes, prefix);
        for i in 0..count {
            let raw = raw_in(raws.get(i), "arguments");
            restore_raw_text(root, &format!("{prefix}.{i}.arguments"), raw.as_deref());
        }
    }
}

/// Restores the original text of `arguments`, `result` and `output` in the entries of an
/// Interactions request `input` (array of steps, or a single step object).
pub(crate) fn restore_input_raw_text(bytes: &[u8], root: &mut Value) {
    let input = root.g("input");
    if input.is_array() {
        let count = input.array().len();
        let raws = cpa_json::raw_children(bytes, "input");
        for i in 0..count {
            for key in ["arguments", "result", "output"] {
                let raw = raw_in(raws.get(i), key);
                restore_raw_text(root, &format!("input.{i}.{key}"), raw.as_deref());
            }
        }
    } else if input.is_object() {
        for key in ["arguments", "result", "output"] {
            let path = format!("input.{key}");
            restore_raw_text(root, &path, cpa_json::raw_at(bytes, &path));
        }
    }
}
