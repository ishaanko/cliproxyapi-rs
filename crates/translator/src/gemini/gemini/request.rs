//! Gemini request normalizer (Go: gemini/gemini/gemini_gemini_request.go).

use cpa_core::signature::sanitize_gemini_request_thought_signatures;
use cpa_core::util::rename_key;
use cpa_json::{Res, Value, J};

use crate::common::content_has_gemini_function_response;
use crate::gemini::common::attach_default_safety_settings;

/// Normalizes a Gemini v1beta request: renames `functionDeclarations`/`parameters` to the
/// snake_case/`parametersJsonSchema` forms, fixes missing or invalid content roles (first turn
/// `user`, then alternating), sanitizes thought signatures, renames `responseSchema`, backfills
/// empty functionResponse names and attaches default safety settings.
pub fn convert_gemini_request_to_gemini(_model: &str, raw: &[u8], _stream: bool) -> Vec<u8> {
    let mut root = cpa_json::parse(raw);
    // Fast path: no contents field, only attach safety settings.
    let Some(contents) = root.g("contents").into_value() else {
        return attach_default_safety_settings(raw, "safetySettings");
    };
    let contents_is_array = contents.is_array();

    let tools = root.g("tools");
    if tools.exists() && tools.is_array() {
        let mut tool_items: Vec<Value> = Vec::new();
        let mut tools_changed = false;
        for tool_result in tools.array() {
            let mut tool = tool_result.value();
            let mut tool_changed = false;
            let declarations = tool_result.g("functionDeclarations");
            if declarations.exists() {
                cpa_json::set(&mut tool, "function_declarations", declarations.value());
                cpa_json::delete(&mut tool, "functionDeclarations");
                tool_changed = true;
            }

            let declarations = tool.g("function_declarations").value();
            if let Value::Array(items) = declarations {
                let mut declaration_items: Vec<Value> = Vec::with_capacity(items.len());
                let mut declarations_changed = false;
                for declaration_value in items {
                    let mut declaration = declaration_value;
                    let parameters = declaration.g("parameters");
                    if parameters.exists() {
                        let parameters = parameters.value();
                        cpa_json::set(&mut declaration, "parametersJsonSchema", parameters);
                        cpa_json::delete(&mut declaration, "parameters");
                        declarations_changed = true;
                    }
                    declaration_items.push(declaration);
                }
                if declarations_changed {
                    cpa_json::set(&mut tool, "function_declarations", Value::Array(declaration_items));
                    tool_changed = true;
                }
            }
            tools_changed = tools_changed || tool_changed;
            tool_items.push(tool);
        }
        if tools_changed {
            cpa_json::set(&mut root, "tools", Value::Array(tool_items));
        }
    }

    // Walk contents and fix roles.
    let mut out = root;
    let mut prev_role = String::new();
    if contents_is_array {
        let items = contents.as_array().map(Vec::as_slice).unwrap_or_default();
        let mut roles_changed = false;
        for value in items {
            let mut role = value.g("role").str();
            if role != "user" && role != "model" {
                role = fixed_role(value, &prev_role);
                roles_changed = true;
            }
            prev_role = role;
        }
        if roles_changed {
            prev_role.clear();
            let mut content_items = Vec::with_capacity(items.len());
            for value in items {
                let mut role = value.g("role").str();
                let mut item = value.clone();
                if role != "user" && role != "model" {
                    role = fixed_role(value, &prev_role);
                    cpa_json::set(&mut item, "role", role.as_str());
                }
                prev_role = role;
                content_items.push(item);
            }
            cpa_json::set(&mut out, "contents", Value::Array(content_items));
        }
    } else {
        let mut idx = 0;
        Res::of(&contents).for_each(|_, value| {
            let mut role = value.g("role").str();
            if role != "user" && role != "model" {
                role = fixed_role(&value.value(), &prev_role);
                cpa_json::set(&mut out, &format!("contents.{idx}.role"), role.as_str());
            }
            prev_role = role;
            idx += 1;
            true
        });
    }

    let mut out_bytes = sanitize_gemini_request_thought_signatures(&cpa_json::to_vec(&out), "contents");

    if out.g("generationConfig.responseSchema").exists() {
        // Go ignores the error and uses the (empty) string it returned.
        out_bytes = rename_key(
            &String::from_utf8_lossy(&out_bytes),
            "generationConfig.responseSchema",
            "generationConfig.responseJsonSchema",
        )
        .unwrap_or_default()
        .into_bytes();
    }

    // Backfill empty functionResponse.name from the preceding functionCall.name. Some clients
    // send function responses with empty names; the Gemini API rejects these.
    let out_bytes = backfill_empty_function_response_names(out_bytes);

    attach_default_safety_settings(&out_bytes, "safetySettings")
}

/// Role for a content whose role is missing or invalid: `user` when it carries a
/// functionResponse, else the next alternating role.
fn fixed_role(content: &Value, prev_role: &str) -> String {
    if content_has_gemini_function_response(&cpa_json::to_vec(content)) {
        "user".to_string()
    } else {
        next_gemini_role(prev_role).to_string()
    }
}

fn next_gemini_role(previous_role: &str) -> &'static str {
    if previous_role.is_empty() || previous_role == "model" { "user" } else { "model" }
}

/// functionCall names of a model turn, in order.
fn function_call_names(content: &Value) -> Vec<String> {
    let mut names = Vec::new();
    content.g("parts").for_each(|_, part| {
        if part.g("functionCall").exists() {
            names.push(part.g("functionCall.name").str());
        }
        true
    });
    names
}

/// Walks the contents and, for each user/function turn following a model turn with
/// functionCall parts, replaces blank functionResponse names with the call names in order.
fn backfill_empty_function_response_names(data: Vec<u8>) -> Vec<u8> {
    let mut root = cpa_json::parse(&data);
    let Some(contents) = root.g("contents").into_value() else {
        return data;
    };
    let contents = Res::of(&contents);
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
    if !can_batch {
        return backfill_legacy(root);
    }
    if !names_need_backfill(&contents) {
        return data;
    }

    let mut changed = false;
    let mut content_items: Vec<Value> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    contents.for_each(|_, content_res| {
        let mut content = content_res.value();
        if content_res.g("role").str() == "model" {
            pending = function_call_names(&content);
            content_items.push(content);
            return true;
        }
        if !pending.is_empty() {
            let mut response_index = 0;
            let mut parts_changed = false;
            let mut part_items: Vec<Value> = Vec::new();
            content_res.g("parts").for_each(|_, part_res| {
                let mut part = part_res.value();
                if part_res.g("functionResponse").exists() {
                    let name = part_res.g("functionResponse.name").str();
                    if name.trim().is_empty() {
                        if let Some(call_name) = pending.get(response_index) {
                            cpa_json::set(&mut part, "functionResponse.name", call_name.as_str());
                            parts_changed = true;
                        } else {
                            tracing::debug!("more function responses than calls, skipping name backfill");
                        }
                    }
                    response_index += 1;
                }
                part_items.push(part);
                true
            });
            if parts_changed {
                cpa_json::set(&mut content, "parts", Value::Array(part_items));
                changed = true;
            }
            pending.clear();
        }
        content_items.push(content);
        true
    });

    if !changed {
        return data;
    }
    cpa_json::set(&mut root, "contents", Value::Array(content_items));
    cpa_json::to_vec(&root)
}

/// Whether any functionResponse with a blank name has a call to take its name from.
fn names_need_backfill(contents: &Res<'_>) -> bool {
    let mut pending: Vec<String> = Vec::new();
    let mut needs = false;
    contents.for_each(|_, content| {
        if content.g("role").str() == "model" {
            pending = function_call_names(&content.value());
            return true;
        }
        if pending.is_empty() {
            return true;
        }
        let mut response_index = 0;
        content.g("parts").for_each(|_, part| {
            if part.g("functionResponse").exists() {
                if part.g("functionResponse.name").str().trim().is_empty() && response_index < pending.len() {
                    needs = true;
                    return false;
                }
                response_index += 1;
            }
            true
        });
        pending.clear();
        !needs
    });
    needs
}

/// Fallback for non-array contents or non-array parts: sets names by index path.
fn backfill_legacy(mut out: Value) -> Vec<u8> {
    let contents = out.g("contents").value();
    let mut pending: Vec<String> = Vec::new();
    Res::of(&contents).for_each(|content_idx, content| {
        if content.g("role").str() == "model" {
            pending = function_call_names(&content.value());
            return true;
        }
        if !pending.is_empty() {
            let mut response_index = 0;
            content.g("parts").for_each(|part_idx, part| {
                if part.g("functionResponse").exists() {
                    if part.g("functionResponse.name").str().trim().is_empty() {
                        if let Some(call_name) = pending.get(response_index) {
                            let path = format!(
                                "contents.{}.parts.{}.functionResponse.name",
                                content_idx.int(),
                                part_idx.int()
                            );
                            cpa_json::set(&mut out, &path, call_name.as_str());
                        } else {
                            tracing::debug!("more function responses than calls, skipping name backfill");
                        }
                    }
                    response_index += 1;
                }
                true
            });
            pending.clear();
        }
        true
    });
    cpa_json::to_vec(&out)
}
