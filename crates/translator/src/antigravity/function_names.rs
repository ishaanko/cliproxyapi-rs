//! Function name rewriting shared by the Gemini and Interactions request translators
//! (Go duplicates it in both packages).

use std::collections::HashMap;

use cpa_core::util;
use cpa_json::{J, Value};

/// Maps every `<field>.name` of `request.contents[].parts[]` and every name at `allowed_paths`
/// through the sanitized function name map (non-string names are coerced to strings). Edits in
/// place, equivalent to Go's rebuild-on-demand of the contents array.
fn rewrite_part_names(part: &mut Value, function_name_map: &HashMap<String, String>, fields: &[&str]) {
    for field in fields.iter().copied() {
        let path = format!("{field}.name");
        let name_result = part.g(&path);
        let name = name_result.str();
        if name.is_empty() {
            continue;
        }
        let mapped = util::map_sanitized_function_name(function_name_map, &name);
        if name_result.is_string() && mapped == name {
            continue;
        }
        cpa_json::set(part, &path, mapped);
    }
}

pub fn rewrite_function_names(
    root: &mut Value,
    function_name_map: &HashMap<String, String>,
    fields: &[&str],
    allowed_paths: &[&str],
) {
    let contents = root.g("request.contents");
    let mut can_batch = contents.is_array();
    if can_batch {
        contents.for_each(|_, content| {
            let parts = content.g("parts");
            if parts.exists() && !parts.is_array() {
                can_batch = false;
                return false;
            }
            true
        });
    }
    if can_batch {
        // Mutating in place is equivalent to Go's rebuild-on-demand of the contents array.
        if let Some(Value::Array(contents)) = cpa_json::get_mut(root, "request.contents") {
            for content in contents.iter_mut() {
                if let Some(Value::Array(parts)) = content.as_object_mut().and_then(|m| m.get_mut("parts")) {
                    for part in parts.iter_mut() {
                        rewrite_part_names(part, function_name_map, fields);
                    }
                }
            }
        }
    } else {
        let mut edits: Vec<(String, String)> = Vec::new();
        for (content_index, content) in contents.array().iter().enumerate() {
            for (part_index, part) in content.g("parts").array().iter().enumerate() {
                for field in fields.iter().copied() {
                    let name_result = part.g(&format!("{field}.name"));
                    let name = name_result.str();
                    if name.is_empty() {
                        continue;
                    }
                    let mapped = util::map_sanitized_function_name(function_name_map, &name);
                    if name_result.is_string() && mapped == name {
                        continue;
                    }
                    edits.push((format!("request.contents.{content_index}.parts.{part_index}.{field}.name"), mapped));
                }
            }
        }
        for (path, mapped) in edits {
            cpa_json::set(root, &path, mapped);
        }
    }

    for allowed_path in allowed_paths.iter().copied() {
        let allowed_names = root.g(allowed_path);
        if allowed_names.is_array() {
            let mut names_changed = false;
            let mut name_items: Vec<Value> = Vec::new();
            for name in allowed_names.array() {
                let mapped = util::map_sanitized_function_name(function_name_map, &name.str());
                names_changed = names_changed || !name.is_string() || mapped != name.str();
                name_items.push(Value::String(mapped));
            }
            if names_changed {
                cpa_json::set(root, allowed_path, Value::Array(name_items));
            }
        } else {
            let mut edits: Vec<(String, String)> = Vec::new();
            for (index, name) in allowed_names.array().iter().enumerate() {
                let mapped = util::map_sanitized_function_name(function_name_map, &name.str());
                if name.is_string() && mapped == name.str() {
                    continue;
                }
                edits.push((format!("{allowed_path}.{index}"), mapped));
            }
            for (path, mapped) in edits {
                cpa_json::set(root, &path, mapped);
            }
        }
    }
}

