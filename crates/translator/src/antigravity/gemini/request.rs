//! Gemini request -> Antigravity request (Go: antigravity_gemini_request.go).

use std::collections::{HashMap, HashSet};

use cpa_core::signature;
use cpa_core::util::{self, go_json_sorted, GoJsonStyle};
use cpa_json::{json, J, Res, Value};

use crate::antigravity::function_names;
use crate::common;
use crate::gemini::common::attach_default_safety_settings;

/// Part fields that can carry a function name.
const FUNCTION_NAME_FIELDS: [&str; 4] = ["functionCall", "functionResponse", "function_call", "function_response"];

/// Go: `ConvertGeminiRequestToAntigravity`. Also called directly by the openai-response translator.
pub fn convert_gemini_request_to_antigravity(model: &str, input_raw_json: &[u8], _stream: bool) -> Vec<u8> {
    let function_name_map = util::sanitized_function_name_map(input_raw_json);
    let mut root = json!({"project": "", "request": {}, "model": ""});
    cpa_json::set(&mut root, "model", model);
    cpa_json::set(&mut root, "request", cpa_json::parse(input_raw_json));
    if root.g("request.model").exists() {
        cpa_json::delete(&mut root, "request.model");
    }

    if fix_cli_tool_response(&mut root).is_err() {
        return vec![];
    }

    let system_instruction = root.g("request.system_instruction");
    if system_instruction.exists() {
        let v = system_instruction.value();
        cpa_json::set(&mut root, "request.systemInstruction", v);
        cpa_json::delete(&mut root, "request.system_instruction");
    }

    normalize_generation_config_response_schema(&mut root);

    // Normalize roles in request.contents: default to valid values if missing/invalid.
    let contents = root.g("request.contents");
    if contents.is_array() && content_roles_need_normalization(&contents) {
        let mut previous_role = String::new();
        let mut items: Vec<Value> = Vec::new();
        for value in contents.array() {
            let mut role = value.g("role").str();
            let mut content = value.value();
            if role != "user" && role != "model" {
                role = if common::content_has_gemini_function_response(&cpa_json::to_vec(&content)) || previous_role.is_empty() || previous_role == "model" {
                    "user".to_string()
                } else {
                    "model".to_string()
                };
                cpa_json::set(&mut content, "role", role.clone());
            }
            previous_role = role;
            items.push(content);
        }
        cpa_json::set(&mut root, "request.contents", Value::Array(items));
    }

    normalize_tools(&mut root, &function_name_map);
    function_names::rewrite_function_names(
        &mut root,
        &function_name_map,
        &FUNCTION_NAME_FIELDS,
        &[
            "request.toolConfig.functionCallingConfig.allowedFunctionNames",
            "request.tool_config.function_calling_config.allowed_function_names",
        ],
    );

    let mut raw = cpa_json::to_vec(&root);
    if model.to_lowercase().contains("claude") {
        raw = sanitize_antigravity_claude_gemini_request_signatures(model, &raw);
    } else {
        raw = signature::sanitize_gemini_request_thought_signatures(&raw, "request.contents");
    }
    attach_default_safety_settings(&raw, "request.safetySettings")
}

/// Declaration names are sanitized and deduplicated across tools (first wins), `parameters` is
/// renamed to `parametersJsonSchema`, and empty function tools are dropped.
fn normalize_tools(root: &mut Value, function_name_map: &HashMap<String, String>) {
    let tools_result = root.g("request.tools");
    if !tools_result.is_array() {
        return;
    }
    let mut seen_function_names: HashSet<String> = HashSet::new();
    let mut tools_changed = false;
    let mut tool_items: Vec<Value> = Vec::new();
    for tool in tools_result.array() {
        let mut tool_json = tool.value();
        for key in ["functionDeclarations", "function_declarations"] {
            let declarations = tool.g(key);
            if !declarations.is_array() {
                continue;
            }
            let mut declarations_changed = false;
            let mut declaration_items: Vec<Value> = Vec::new();
            for declaration in declarations.array() {
                let name_result = declaration.g("name");
                let original_name = name_result.str();
                let mapped_name = util::map_sanitized_function_name(function_name_map, &original_name);
                if !mapped_name.is_empty() {
                    if !seen_function_names.insert(mapped_name.clone()) {
                        declarations_changed = true;
                        continue;
                    }
                }
                let mut declaration_json = declaration.value();
                if !name_result.is_string() || mapped_name != original_name {
                    cpa_json::set(&mut declaration_json, "name", mapped_name);
                    declarations_changed = true;
                }
                let parameters = declaration.g("parameters");
                if parameters.exists() {
                    cpa_json::set(&mut declaration_json, "parametersJsonSchema", parameters.value());
                    cpa_json::delete(&mut declaration_json, "parameters");
                    declarations_changed = true;
                }
                declaration_items.push(declaration_json);
            }
            if declarations_changed {
                cpa_json::set(&mut tool_json, key, Value::Array(declaration_items));
                tools_changed = true;
            }
        }
        tool_items.push(tool_json);
    }
    if tools_changed {
        cpa_json::set(root, "request.tools", Value::Array(tool_items));
    }
    remove_empty_function_tools(root);
}

fn remove_empty_function_tools(root: &mut Value) {
    let tools = root.g("request.tools");
    if tools.is_array() && tools.array().is_empty() {
        cpa_json::delete(root, "request.tools");
        return;
    }
    let mut changed = false;
    let mut cleaned: Vec<Value> = Vec::new();
    for tool in tools.array() {
        let mut tool_json = tool.value();
        if tool.is_object() {
            for key in ["functionDeclarations", "function_declarations"] {
                let declarations = tool.g(key);
                if declarations.is_array() && declarations.array().is_empty() {
                    cpa_json::delete(&mut tool_json, key);
                    changed = true;
                }
            }
            if tool_json.as_object().is_some_and(|m| m.is_empty()) {
                changed = true;
                continue;
            }
        }
        cleaned.push(tool_json);
    }
    if !changed {
        return;
    }
    if cleaned.is_empty() {
        cpa_json::delete(root, "request.tools");
        return;
    }
    cpa_json::set(root, "request.tools", Value::Array(cleaned));
}

/// Converts `generationConfig.responseJsonSchema` (and snake_case variants) to `responseSchema`.
fn normalize_generation_config_response_schema(root: &mut Value) {
    for container in ["request.generationConfig", "request.generation_config"] {
        if !root.g(container).exists() {
            continue;
        }
        for schema_key in ["responseJsonSchema", "response_json_schema"] {
            let old_path = format!("{container}.{schema_key}");
            let schema = root.g(&old_path);
            if schema.exists() {
                let schema = schema.value();
                let target_path = format!("{container}.responseSchema");
                if !root.g(&target_path).exists() {
                    cpa_json::set(root, &target_path, schema);
                }
                cpa_json::delete(root, &old_path);
            }
        }
    }
}

fn content_roles_need_normalization(contents: &Res<'_>) -> bool {
    let mut needs = false;
    contents.for_each(|_, value| {
        let role = value.g("role").str();
        if role != "user" && role != "model" {
            needs = true;
            return false;
        }
        true
    });
    needs
}

const SIGNATURE_PATHS: [&str; 7] = [
    "thoughtSignature",
    "thought_signature",
    "functionCall.thoughtSignature",
    "functionCall.thought_signature",
    "functionResponse.thoughtSignature",
    "functionResponse.thought_signature",
    "extra_content.google.thought_signature",
];

/// Any object key named `thoughtSignature` / `thought_signature`, at any depth.
fn has_thought_signature_key_deep(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.iter().any(|(k, child)| k == "thoughtSignature" || k == "thought_signature" || has_thought_signature_key_deep(child)),
        Value::Array(a) => a.iter().any(has_thought_signature_key_deep),
        _ => false,
    }
}

fn part_thought_signature(part: &Value) -> Option<String> {
    SIGNATURE_PATHS.iter().find_map(|path| part.g(path).as_str().map(str::to_owned))
}

fn delete_part_thought_signature_fields(part: &mut Value) {
    for path in SIGNATURE_PATHS {
        cpa_json::delete(part, path);
    }
}

/// Go re-marshals a decoded `map[string]any` (sorted keys); reproduce that ordering.
fn marshal_part(part: &Value) -> Value {
    go_json_sorted(part, GoJsonStyle::MARSHAL_USE_NUMBER).map(|s| cpa_json::parse_str(&s)).unwrap_or_else(|| part.clone())
}

/// Go: `SanitizeAntigravityClaudeGeminiRequestSignatures`. For Claude targets, only model turns
/// may carry thought parts with a Claude-compatible signature; every other signature is stripped.
pub fn sanitize_antigravity_claude_gemini_request_signatures(_model: &str, raw_json: &[u8]) -> Vec<u8> {
    let mut root = cpa_json::parse(raw_json);
    let contents = root.g("request.contents");
    if !contents.is_array() {
        return raw_json.to_vec();
    }
    let contents_array = contents.array();
    let mut changed = false;
    let mut rewritten_contents: Vec<Value> = Vec::with_capacity(contents_array.len());

    for content in &contents_array {
        let parts = content.g("parts");
        if !parts.is_array() {
            rewritten_contents.push(content.value());
            continue;
        }
        let is_model_turn = content.g("role").str() == "model";
        let parts_array = parts.array();
        let mut content_changed = false;
        let mut rewritten_parts: Vec<Value> = Vec::with_capacity(parts_array.len());

        for part_result in &parts_array {
            let mut part = part_result.value();
            if !part.is_object() {
                rewritten_parts.push(part);
                continue;
            }
            let signature = part_thought_signature(&part);
            let has_signature_key = signature.is_some() || has_thought_signature_key_deep(&part);
            let raw_signature = signature.unwrap_or_default();

            if part.g("functionResponse").exists() || part.g("function_response").exists() || !is_model_turn {
                // functionResponse and non-model parts cannot replay Claude thinking signatures.
                if has_signature_key {
                    changed = true;
                    content_changed = true;
                    delete_part_thought_signature_fields(&mut part);
                    rewritten_parts.push(marshal_part(&part));
                } else {
                    rewritten_parts.push(part);
                }
                continue;
            }

            if part.g("thought").v() == Some(&Value::Bool(true)) {
                let Some(normalized) = signature::compatible_antigravity_claude_thinking_signature(&raw_signature) else {
                    changed = true;
                    content_changed = true;
                    continue;
                };
                let text = part.g("text").as_str().unwrap_or_default().to_string();
                if text.trim().is_empty() {
                    changed = true;
                    content_changed = true;
                    continue;
                }
                if normalized != raw_signature {
                    changed = true;
                    content_changed = true;
                }
                delete_part_thought_signature_fields(&mut part);
                cpa_json::set(&mut part, "thoughtSignature", normalized);
                rewritten_parts.push(marshal_part(&part));
                continue;
            }

            if has_signature_key {
                changed = true;
                content_changed = true;
                delete_part_thought_signature_fields(&mut part);
                rewritten_parts.push(marshal_part(&part));
            } else {
                rewritten_parts.push(part);
            }
        }

        if rewritten_parts.is_empty() {
            changed = true;
            continue;
        }
        let mut content_value = content.value();
        if content_changed || rewritten_parts.len() != parts_array.len() {
            cpa_json::set(&mut content_value, "parts", Value::Array(rewritten_parts));
        }
        rewritten_contents.push(content_value);
    }

    if !changed {
        return raw_json.to_vec();
    }
    cpa_json::set(&mut root, "request.contents", Value::Array(rewritten_contents));
    cpa_json::to_vec(&root)
}

/// Keeps functionResponse parts and moves sibling inline_data/inlineData onto the nearest
/// preceding functionResponse. Leading images before the first one attach to that first response.
fn collect_function_responses_with_sibling_inline_data(parts: &Res<'_>) -> Vec<Value> {
    let mut responses: Vec<Value> = Vec::new();
    let mut leading_images: Vec<Value> = Vec::new();
    parts.for_each(|_, part| {
        if part.g("functionResponse").exists() {
            responses.push(part.value());
            let current = responses.len() - 1;
            for img in leading_images.drain(..) {
                cpa_json::set(&mut responses[current], "functionResponse.parts.-1", img);
            }
            return true;
        }
        let Some(image_part) = normalize_inline_data_part(&part) else { return true };
        match responses.last_mut() {
            Some(current) => {
                cpa_json::set(current, "functionResponse.parts.-1", image_part);
            }
            None => leading_images.push(image_part),
        }
        true
    });
    responses
}

fn normalize_inline_data_part(part: &Res<'_>) -> Option<Value> {
    let mut inline = part.g("inlineData");
    if !inline.exists() {
        inline = part.g("inline_data");
    }
    if !inline.exists() {
        return None;
    }
    let data = inline.g("data").str();
    if data.is_empty() {
        return None;
    }
    let mut mime_type = inline.g("mimeType").str();
    if mime_type.is_empty() {
        mime_type = inline.g("mime_type").str();
    }
    if mime_type.is_empty() {
        // Cloud Code Assist ignores inlineData without mimeType.
        mime_type = "image/png".to_string();
    }
    Some(json!({"inlineData": {"mimeType": mime_type, "data": data}}))
}

/// Normalizes a collected functionResponse part: backfills an empty name with the call name.
/// Collected parts are always objects holding a `functionResponse`, so Go's parse-failure
/// fallbacks are unreachable.
fn parse_function_response(mut response: Value, fallback_name: &str) -> Value {
    if response.g("functionResponse.name").str().trim().is_empty() && !fallback_name.is_empty() {
        cpa_json::set(&mut response, "functionResponse.name", fallback_name);
    }
    response
}

struct FunctionCallGroup {
    responses_needed: usize,
    /// Ordered function call names for backfilling empty response names.
    call_names: Vec<String>,
}

/// Groups function calls with their responses: each model turn with calls opens a FIFO group, the
/// response parts of later contents are pooled, and once a group's quota is met one
/// `role: "function"` content is emitted right after it. Errors when `request.contents` is absent.
fn fix_cli_tool_response(root: &mut Value) -> Result<(), ()> {
    let contents = root.g("request.contents");
    if !contents.exists() {
        return Err(());
    }

    let mut needs_grouping = false;
    let mut all_contents_are_objects = true;
    contents.for_each(|_, content| {
        if !content.is_object() {
            all_contents_are_objects = false;
            return true;
        }
        content.g("parts").for_each(|_, part| {
            if part.g("functionResponse").exists() {
                needs_grouping = true;
                return false;
            }
            true
        });
        !needs_grouping
    });
    if contents.is_array() && all_contents_are_objects && !needs_grouping {
        return Ok(());
    }

    let mut content_items: Vec<Value> = Vec::new();
    let mut pending_groups: std::collections::VecDeque<FunctionCallGroup> = Default::default();
    let mut collected_responses: Vec<Value> = Vec::new();

    fn append_function_responses(content_items: &mut Vec<Value>, responses: Vec<Value>, call_names: &[String]) {
        let part_items: Vec<Value> = responses
            .into_iter()
            .enumerate()
            .map(|(i, response)| parse_function_response(response, call_names.get(i).map_or("", String::as_str)))
            .collect();
        if !part_items.is_empty() {
            content_items.push(json!({"parts": part_items, "role": "function"}));
        }
    }

    contents.for_each(|_, value| {
        let role = value.g("role").str();
        let parts = value.g("parts");

        // Function responses are pooled (with sibling inlineData attached) and their content dropped.
        let responses_in_this_content = collect_function_responses_with_sibling_inline_data(&parts);
        if !responses_in_this_content.is_empty() {
            collected_responses.extend(responses_in_this_content);
            // FIFO: the oldest group is satisfied first.
            while pending_groups.front().is_some_and(|g| collected_responses.len() >= g.responses_needed) {
                let Some(group) = pending_groups.pop_front() else { break };
                let group_responses: Vec<Value> = collected_responses.drain(..group.responses_needed).collect();
                append_function_responses(&mut content_items, group_responses, &group.call_names);
            }
            return true;
        }

        if !value.is_object() {
            tracing::warn!("failed to parse content");
            return true;
        }
        if role == "model" {
            let mut call_names: Vec<String> = Vec::new();
            parts.for_each(|_, part| {
                if part.g("functionCall").exists() {
                    call_names.push(part.g("functionCall.name").str());
                }
                true
            });
            content_items.push(value.value());
            if !call_names.is_empty() {
                pending_groups.push_back(FunctionCallGroup { responses_needed: call_names.len(), call_names });
            }
        } else {
            content_items.push(value.value());
        }
        true
    });

    // Remaining groups that have enough responses.
    for group in pending_groups {
        if collected_responses.len() >= group.responses_needed {
            let group_responses: Vec<Value> = collected_responses.drain(..group.responses_needed).collect();
            append_function_responses(&mut content_items, group_responses, &group.call_names);
        }
    }

    cpa_json::set(root, "request.contents", Value::Array(content_items));
    Ok(())
}
