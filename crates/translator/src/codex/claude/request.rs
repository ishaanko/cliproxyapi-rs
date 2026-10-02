//! Claude request -> Codex Responses request (Go: codex_claude_request.go).

use std::collections::{HashMap, HashSet};

use cpa_core::signature::{
    SignatureBlockKind, SignatureProvider, compatible_signature_for_provider,
    detect_signature_provider_for_block, inspect_grok_encrypted_content,
};
use cpa_core::thinking;
use cpa_core::util::{
    GoJsonStyle, SCHEMA_MAP_KEYWORDS, SCHEMA_VALUE_KEYWORDS, go_json_sorted,
    has_unsupported_unicode_property_escape, is_claude_code_attribution_system_text,
};
use cpa_json::{J, Res, Value, json};
use sha2::{Digest, Sha256};

use crate::codex::raw::raw_at;
use crate::codex::util::{build_short_name_map, shorten_name_if_needed, truncate_bytes};
use crate::common::{align_claude_tool_results, claude_message_system_reminder_text};

const DEFAULT_PARAMETERS: &str = r#"{"type":"object","properties":{}}"#;

/// Go: ConvertClaudeRequestToCodex.
pub fn convert_claude_request_to_codex(
    model_name: &str,
    input_raw_json: &[u8],
    _stream: bool,
) -> Vec<u8> {
    convert(model_name, input_raw_json, false)
}

/// Go: ConvertClaudeRequestToCodexWithCompat. Keeps assistant thinking blocks with empty or
/// unknown-format signatures for configured compatibility endpoints.
pub fn convert_claude_request_to_codex_with_compat(
    model_name: &str,
    input_raw_json: &[u8],
    _stream: bool,
) -> Vec<u8> {
    convert(model_name, input_raw_json, true)
}

fn convert(model_name: &str, raw_json: &[u8], preserve_empty_thinking_blocks: bool) -> Vec<u8> {
    let mut template = cpa_json::parse_str(r#"{"model":"","instructions":"","input":[]}"#);
    let root = cpa_json::parse(raw_json);
    let tool_name_map = build_reverse_map_original_to_short(&root);
    cpa_json::set(&mut template, "model", model_name);
    let mut input_items: Vec<Value> = Vec::new();

    // System messages become a developer message with input_text parts.
    let systems = root.g("system");
    if systems.exists() {
        let mut content_items: Vec<Value> = Vec::new();
        let mut append_system_text = |text: String| {
            if text.is_empty() || is_claude_code_attribution_system_text(&text) {
                return;
            }
            content_items.push(json!({"type": "input_text", "text": text}));
        };
        if systems.is_string() {
            append_system_text(systems.str());
        } else if systems.is_array() {
            for system in systems.array() {
                if system.g("type").str() == "text" {
                    append_system_text(system.g("text").str());
                }
            }
        }
        if !content_items.is_empty() {
            input_items
                .push(json!({"type": "message", "role": "developer", "content": content_items}));
        }
    }

    // Messages.
    let messages = root.g("messages");
    if messages.is_array() {
        let mut pending_tool_use_ids: Vec<String> = Vec::new();
        let mut pending_system_reminders: Vec<Value> = Vec::new();

        for (i, message) in messages.array().iter().enumerate() {
            let message_role = message.g("role").str();
            if message_role == "system" {
                if let Some(reminder) = claude_message_system_reminder_text(&message.g("content")) {
                    let msg = json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": reminder}]});
                    if !pending_tool_use_ids.is_empty() {
                        pending_system_reminders.push(msg);
                    } else {
                        input_items.push(msg);
                    }
                }
                continue;
            }

            let mut contents = message.g("content");
            // Source index of each content element (tool results may be reordered).
            let mut source_index: Vec<usize> = (0..contents.array().len()).collect();
            if message_role == "user" && !pending_tool_use_ids.is_empty() && contents.is_array() {
                let aligned = align_claude_tool_results(contents.clone(), &pending_tool_use_ids);
                source_index = original_indices(&contents, &aligned);
                contents = aligned;
            }
            pending_tool_use_ids.clear();
            let mut content_items: Vec<Value> = Vec::new();

            if contents.is_array() {
                for (j, content) in contents.array().iter().enumerate() {
                    let content_path = format!("messages.{i}.content.{}", source_index[j]);
                    match content.g("type").str().as_str() {
                        "text" => {
                            input_items.append(&mut pending_system_reminders);
                            append_text_content(
                                &mut content_items,
                                &message_role,
                                &content.g("text").str(),
                            );
                        }
                        "thinking" => {
                            append_reasoning_content(
                                &mut input_items,
                                &mut content_items,
                                &message_role,
                                content,
                                model_name,
                                preserve_empty_thinking_blocks,
                            );
                        }
                        "image" => {
                            input_items.append(&mut pending_system_reminders);
                            let source = content.g("source");
                            if source.exists()
                                && let Some(url) = image_data_url(&source)
                            {
                                content_items
                                    .push(json!({"type": "input_image", "image_url": url}));
                            }
                        }
                        "document" => {
                            input_items.append(&mut pending_system_reminders);
                            let source = content.g("source");
                            if source.g("type").str() != "base64" {
                                continue;
                            }
                            let media_type = source.g("media_type").str().trim().to_string();
                            if !media_type.eq_ignore_ascii_case("application/pdf") {
                                continue;
                            }
                            let mut data = source.g("data").str();
                            if data.is_empty() {
                                data = source.g("base64").str();
                            }
                            if !data.is_empty() {
                                content_items.push(json!({
                                    "type": "input_file",
                                    "file_data": format!("data:{media_type};base64,{data}"),
                                    "filename": "document.pdf",
                                }));
                            }
                        }
                        "tool_use" => {
                            flush_message(&mut input_items, &mut content_items, &message_role);
                            let id = content.g("id").str();
                            if !id.is_empty() {
                                pending_tool_use_ids.push(id.clone());
                            }
                            let name = content.g("name").str();
                            let name = tool_name_map
                                .get(&name)
                                .cloned()
                                .unwrap_or_else(|| shorten_name_if_needed(&name));
                            let input = content.g("input");
                            let arguments = if input.exists() {
                                raw_at(raw_json, &format!("{content_path}.input"))
                                    .unwrap_or_else(|| input.raw())
                            } else {
                                String::new()
                            };
                            input_items.push(json!({
                                "type": "function_call",
                                "call_id": shorten_call_id_if_needed(&id),
                                "name": name,
                                "arguments": arguments,
                            }));
                        }
                        "tool_result" => {
                            flush_message(&mut input_items, &mut content_items, &message_role);
                            let mut output_message = json!({
                                "type": "function_call_output",
                                "call_id": shorten_call_id_if_needed(&content.g("tool_use_id").str()),
                            });
                            let result_content = content.g("content");
                            let fallback_output = || {
                                if result_content.is_array() || result_content.is_object() {
                                    raw_at(raw_json, &format!("{content_path}.content"))
                                        .unwrap_or_else(|| result_content.str())
                                } else {
                                    result_content.str()
                                }
                            };
                            if result_content.is_array() {
                                let mut result_items: Vec<Value> = Vec::new();
                                for part in result_content.array() {
                                    match part.g("type").str().as_str() {
                                        "image" => {
                                            let source = part.g("source");
                                            if source.exists()
                                                && let Some(url) = image_data_url(&source)
                                            {
                                                result_items.push(json!({"type": "input_image", "image_url": url}));
                                            }
                                        }
                                        "text" => result_items.push(json!({"type": "input_text", "text": part.g("text").str()})),
                                        _ => {}
                                    }
                                }
                                if !result_items.is_empty() {
                                    cpa_json::set(
                                        &mut output_message,
                                        "output",
                                        Value::Array(result_items),
                                    );
                                } else {
                                    cpa_json::set(&mut output_message, "output", fallback_output());
                                }
                            } else {
                                cpa_json::set(&mut output_message, "output", fallback_output());
                            }
                            input_items.push(output_message);
                        }
                        _ => {}
                    }
                }
                flush_message(&mut input_items, &mut content_items, &message_role);
                input_items.append(&mut pending_system_reminders);
            } else if contents.is_string() {
                append_text_content(&mut content_items, &message_role, &contents.str());
                flush_message(&mut input_items, &mut content_items, &message_role);
                input_items.append(&mut pending_system_reminders);
            }
        }

        input_items.append(&mut pending_system_reminders);
    }

    // Tool declarations.
    let tools = root.g("tools");
    let mut tool_items: Vec<Value> = Vec::new();
    if tools.is_array() {
        let web_search_tool_names = build_web_search_tool_name_set(&tools);
        cpa_json::set(
            &mut template,
            "tool_choice",
            convert_tool_choice_to_codex(
                &root.g("tool_choice"),
                &tool_name_map,
                &web_search_tool_names,
            ),
        );
        for tool in tools.array() {
            // Claude web search maps to Codex web_search.
            if is_web_search_tool_type(&tool.g("type").str()) {
                tool_items.push(convert_web_search_tool_to_codex(&tool));
                continue;
            }
            let mut item = tool.value();
            let tool_type = tool.g("type");
            if tool_type.as_str() != Some("function") {
                cpa_json::set(&mut item, "type", "function");
            }
            let v = tool.g("name");
            if v.exists() {
                let original_name = v.str();
                let name = tool_name_map
                    .get(&original_name)
                    .cloned()
                    .unwrap_or_else(|| shorten_name_if_needed(&original_name));
                if !v.is_string() || name != original_name {
                    cpa_json::set(&mut item, "name", name);
                }
            }
            cpa_json::set(
                &mut item,
                "parameters",
                normalize_tool_parameters(&tool.g("input_schema")),
            );
            for path in [
                "input_schema",
                "parameters.$schema",
                "cache_control",
                "defer_loading",
            ] {
                if item.g(path).exists() {
                    cpa_json::delete(&mut item, path);
                }
            }
            if item.g("strict").v() != Some(&Value::Bool(false)) {
                cpa_json::set(&mut item, "strict", false);
            }
            tool_items.push(item);
        }
    }

    // Parallel tool calls unless tool_choice explicitly disables them.
    let mut parallel_tool_calls = true;
    let disable = root.g("tool_choice.disable_parallel_tool_use");
    if disable.exists() {
        parallel_tool_calls = !disable.bool();
    }
    cpa_json::set(&mut template, "parallel_tool_calls", parallel_tool_calls);

    // thinking.budget_tokens -> reasoning.effort.
    let mut reasoning_effort = "medium".to_string();
    let thinking_config = root.g("thinking");
    if thinking_config.exists() && thinking_config.is_object() {
        match thinking_config.g("type").str().as_str() {
            "enabled" => {
                let budget_tokens = thinking_config.g("budget_tokens");
                if budget_tokens.exists()
                    && let Some(effort) = thinking::convert_budget_to_level(budget_tokens.int())
                    && !effort.is_empty()
                {
                    reasoning_effort = effort.to_string();
                }
            }
            "adaptive" | "auto" => {
                // Adaptive thinking can carry an explicit effort in output_config.effort; it is
                // passed through and ApplyThinking clamps it to the target model's levels.
                let mut effort = String::new();
                let v = root.g("output_config.effort");
                if v.exists()
                    && let Some(s) = v.as_str()
                {
                    effort = s.trim().to_lowercase();
                }
                reasoning_effort = if effort.is_empty() {
                    thinking::level::XHIGH.to_string()
                } else {
                    effort
                };
            }
            "disabled" => {
                if let Some(effort) = thinking::convert_budget_to_level(0)
                    && !effort.is_empty()
                {
                    reasoning_effort = effort.to_string();
                }
            }
            _ => {}
        }
    }
    cpa_json::set(&mut template, "reasoning.effort", reasoning_effort);
    // reasoning.summary is left to the source request's canonical summary intent.
    let mut service_tier = normalize_service_tier(&root.g("service_tier"));
    if root.g("speed").as_str() == Some("fast") {
        service_tier = "priority";
    }
    if !service_tier.is_empty() {
        cpa_json::set(&mut template, "service_tier", service_tier);
    }
    cpa_json::set(&mut template, "stream", true);
    cpa_json::set(&mut template, "store", false);
    cpa_json::set(
        &mut template,
        "include",
        json!(["reasoning.encrypted_content"]),
    );

    // output_config.format -> text.format.
    let format = root.g("output_config.format");
    if format.is_object()
        && format.g("type").str() == "json_schema"
        && format.g("schema").is_object()
    {
        let mut name = "cli_proxy_structured_output".to_string();
        let n = format.g("name").str();
        if !n.is_empty() {
            name = n;
        }
        let mut strict = true;
        let s = format.g("strict");
        if s.exists() && s.v() == Some(&Value::Bool(false)) {
            strict = false;
        }
        // OpenAI strict mode requires every declared property to be listed in required;
        // downgrade instead of emitting a schema strict backends reject with HTTP 400.
        if strict && schema_misses_required(&format.g("schema")) {
            strict = false;
        }
        let translated = json!({"type": "json_schema", "name": name, "strict": strict, "schema": format.g("schema").value()});
        cpa_json::set(&mut template, "text.format", translated);
    }

    if tools.is_array() {
        cpa_json::set(&mut template, "tools", Value::Array(tool_items));
    }
    if !input_items.is_empty() {
        cpa_json::set(&mut template, "input", Value::Array(input_items));
    }
    cpa_json::to_vec(&template)
}

/// Original index of each element of `aligned` within `original` (matched by value, in order).
fn original_indices(original: &Res<'_>, aligned: &Res<'_>) -> Vec<usize> {
    let orig = original.array();
    let mut used = vec![false; orig.len()];
    aligned
        .array()
        .iter()
        .enumerate()
        .map(|(pos, a)| {
            let found = orig
                .iter()
                .enumerate()
                .position(|(k, o)| !used[k] && o.v() == a.v());
            match found {
                Some(k) => {
                    used[k] = true;
                    k
                }
                None => pos,
            }
        })
        .collect()
}

fn flush_message(input_items: &mut Vec<Value>, content_items: &mut Vec<Value>, role: &str) {
    if !content_items.is_empty() {
        input_items.push(
            json!({"type": "message", "role": role, "content": std::mem::take(content_items)}),
        );
    }
}

fn append_text_content(content_items: &mut Vec<Value>, role: &str, text: &str) {
    let part_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    content_items.push(json!({"type": part_type, "text": text}));
}

/// `data:<media type>;base64,<data>` for an image source, `None` without data.
fn image_data_url(source: &Res<'_>) -> Option<String> {
    let mut data = source.g("data").str();
    if data.is_empty() {
        data = source.g("base64").str();
    }
    if data.is_empty() {
        return None;
    }
    let mut media_type = source.g("media_type").str();
    if media_type.is_empty() {
        media_type = source.g("mime_type").str();
    }
    if media_type.is_empty() {
        media_type = "application/octet-stream".into();
    }
    Some(format!("data:{media_type};base64,{data}"))
}

/// Assistant thinking blocks become reasoning items carrying the encrypted signature when it is
/// replayable on the target.
fn append_reasoning_content(
    input_items: &mut Vec<Value>,
    content_items: &mut Vec<Value>,
    role: &str,
    part: &Res<'_>,
    model_name: &str,
    preserve_empty_thinking_blocks: bool,
) {
    if role != "assistant" {
        return;
    }
    let raw_signature = part.g("signature").str();
    let mut signature = compatible_signature_for_provider(SignatureProvider::Gpt, &raw_signature);
    if signature.is_none()
        && preserve_empty_thinking_blocks
        && part.g("signature").is_string()
        && !raw_signature.trim().is_empty()
        && detect_signature_provider_for_block(&raw_signature, SignatureBlockKind::ClaudeThinking)
            == SignatureProvider::Unknown
    {
        signature = Some(raw_signature.clone());
    }
    let signature = match signature {
        Some(s) => s,
        None => {
            if preserve_empty_thinking_blocks && raw_signature.trim().is_empty() {
                raw_signature.clone()
            } else {
                if !target_accepts_grok_signature(model_name) {
                    return;
                }
                if inspect_grok_encrypted_content(&raw_signature).is_err() {
                    return;
                }
                raw_signature.clone()
            }
        }
    };

    flush_message(input_items, content_items, role);
    let mut reasoning = json!({"type": "reasoning", "summary": [], "content": null});
    cpa_json::set(&mut reasoning, "encrypted_content", signature);
    input_items.push(reasoning);
}

fn target_accepts_grok_signature(model_name: &str) -> bool {
    thinking::parse_suffix(model_name)
        .model_name
        .trim()
        .to_lowercase()
        .contains("grok")
}

fn normalize_service_tier(result: &Res<'_>) -> &'static str {
    match result.as_str().map(|s| s.trim().to_lowercase()).as_deref() {
        Some("fast" | "priority") => "priority",
        _ => "",
    }
}

/// Keeps Claude tool ids within the OpenAI Responses `call_id` limit with a stable,
/// low-collision mapping: long ids become a prefix plus `_` and 16 hex digits of their SHA-256.
pub(super) fn shorten_call_id_if_needed(id: &str) -> String {
    const LIMIT: usize = 64;
    if id.len() <= LIMIT {
        return id.to_string();
    }
    let sum = Sha256::digest(id.as_bytes());
    let suffix = format!("_{}", hex::encode(&sum[..8]));
    let prefix_len = LIMIT as isize - suffix.len() as isize;
    if prefix_len <= 0 {
        return suffix[suffix.len() - LIMIT..].to_string();
    }
    format!("{}{suffix}", truncate_bytes(id, prefix_len as usize))
}

fn is_web_search_tool_type(tool_type: &str) -> bool {
    tool_type == "web_search_20250305" || tool_type == "web_search_20260209"
}

fn build_web_search_tool_name_set(tools: &Res<'_>) -> HashSet<String> {
    let mut names = HashSet::new();
    if !tools.is_array() {
        return names;
    }
    for tool in tools.array() {
        if !is_web_search_tool_type(&tool.g("type").str()) {
            continue;
        }
        let name = tool.g("name").str();
        if !name.is_empty() {
            names.insert(name);
        }
    }
    names
}

fn convert_tool_choice_to_codex(
    tool_choice: &Res<'_>,
    tool_name_map: &HashMap<String, String>,
    web_search_tool_names: &HashSet<String>,
) -> Value {
    let auto = || Value::String("auto".into());
    if !tool_choice.exists() || tool_choice.is_null() {
        return auto();
    }
    let mut choice_type = tool_choice.g("type").str();
    if choice_type.is_empty() && tool_choice.is_string() {
        choice_type = tool_choice.str();
    }
    match choice_type.as_str() {
        "auto" | "" => auto(),
        "any" => Value::String("required".into()),
        "none" => Value::String("none".into()),
        "tool" => {
            let name = tool_choice.g("name").str();
            if web_search_tool_names.contains(&name) {
                return json!({"type": "web_search"});
            }
            let name = tool_name_map
                .get(&name)
                .cloned()
                .unwrap_or_else(|| shorten_name_if_needed(&name));
            if name.is_empty() {
                return auto();
            }
            json!({"type": "function", "name": name})
        }
        _ => auto(),
    }
}

fn convert_web_search_tool_to_codex(tool: &Res<'_>) -> Value {
    let mut out = json!({"type": "web_search"});
    let allowed_domains = tool.g("allowed_domains");
    if allowed_domains.exists() && allowed_domains.is_array() {
        cpa_json::set(&mut out, "filters.allowed_domains", allowed_domains.value());
    }
    let user_location = tool.g("user_location");
    if user_location.exists() && user_location.is_object() {
        cpa_json::set(&mut out, "user_location", user_location.value());
    }
    out
}

/// Original tool name -> shortened name, for the request's declared tools.
fn build_reverse_map_original_to_short(root: &Value) -> HashMap<String, String> {
    let tools = root.g("tools");
    if !tools.is_array() {
        return HashMap::new();
    }
    let names: Vec<String> = tools
        .array()
        .iter()
        .map(|t| t.g("name").str())
        .filter(|n| !n.is_empty())
        .collect();
    if names.is_empty() {
        return HashMap::new();
    }
    build_short_name_map(&names, shorten_name_if_needed)
}

/// Shortened name -> original name, from the original Claude request's tools.
pub(super) fn build_reverse_map_short_to_original(original: &[u8]) -> HashMap<String, String> {
    let root = cpa_json::parse(original);
    crate::codex::util::reverse_map(build_reverse_map_original_to_short(&root))
}

/// Ensures object schemas contain at least an empty properties map, strips dialect keywords
/// (`$schema`, `$id`) and drops regex patterns with unsupported Unicode property escapes
/// (`\p{...}`), which fail upstream schema validation. Keys come out sorted like Go's map
/// marshaling.
fn normalize_tool_parameters(schema: &Res<'_>) -> Value {
    let default = || cpa_json::parse_str(DEFAULT_PARAMETERS);
    let Some(Value::Object(_)) = schema.v() else {
        return default();
    };
    let mut root = schema.value();
    strip_dialect_keywords(&mut root);

    let Value::Object(map) = &mut root else {
        return default();
    };
    let is_object = match map.get("type") {
        None | Some(Value::Null) => true,
        Some(Value::String(t)) if t.is_empty() => true,
        Some(Value::String(t)) => t == "object",
        Some(Value::Array(a)) => a.iter().any(|e| e.as_str() == Some("object")),
        Some(_) => false,
    };
    let type_missing = match map.get("type") {
        None | Some(Value::Null) => true,
        Some(Value::String(t)) => t.is_empty(),
        Some(_) => false,
    };
    if type_missing {
        map.insert("type".into(), Value::String("object".into()));
    }
    if is_object && matches!(map.get("properties"), None | Some(Value::Null)) {
        map.insert("properties".into(), Value::Object(Default::default()));
    }

    match go_json_sorted(&root, GoJsonStyle::NO_HTML_ESCAPE) {
        Some(s) => cpa_json::parse_str(&s),
        None => default(),
    }
}

fn strip_dialect_keywords(v: &mut Value) {
    match v {
        Value::Object(schema) => {
            schema.shift_remove("$schema");
            schema.shift_remove("$id");
            if schema
                .get("pattern")
                .and_then(Value::as_str)
                .is_some_and(has_unsupported_unicode_property_escape)
            {
                schema.shift_remove("pattern");
            }

            // Regex keys under patternProperties can carry unsupported escapes too.
            if let Some(Value::Object(pattern_props)) = schema.get_mut("patternProperties") {
                pattern_props.retain(|key, _| !has_unsupported_unicode_property_escape(key));
                for sub in pattern_props.values_mut() {
                    strip_dialect_keywords(sub);
                }
            }

            for map_key in SCHEMA_MAP_KEYWORDS {
                if map_key == "patternProperties" {
                    continue;
                }
                if let Some(Value::Object(sub_map)) = schema.get_mut(map_key) {
                    for sub in sub_map.values_mut() {
                        strip_dialect_keywords(sub);
                    }
                }
            }

            for val_key in SCHEMA_VALUE_KEYWORDS {
                match schema.get_mut(val_key) {
                    Some(sub @ Value::Object(_)) => strip_dialect_keywords(sub),
                    Some(Value::Array(items)) => items.iter_mut().for_each(strip_dialect_keywords),
                    _ => {}
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_dialect_keywords),
        _ => {}
    }
}

/// Whether any declared property is missing from its sibling `required` list (recursively).
/// OpenAI strict mode rejects such schemas, so callers downgrade `text.format.strict`.
fn schema_misses_required(schema: &Res<'_>) -> bool {
    if !schema.is_object() {
        if schema.is_array() {
            return schema.array().iter().any(schema_misses_required);
        }
        return false;
    }
    let properties = schema.g("properties");
    if properties.is_object() {
        let required = schema.g("required");
        if !required.is_array() {
            return !properties.entries().is_empty();
        }
        let names: HashSet<String> = required
            .array()
            .iter()
            .filter(|i| i.is_string())
            .map(|i| i.str())
            .collect();
        if properties
            .entries()
            .iter()
            .any(|(name, _)| !names.contains(*name))
        {
            return true;
        }
    }
    for keyword in SCHEMA_MAP_KEYWORDS {
        let children = schema.g(keyword);
        if !children.is_object() {
            continue;
        }
        if children
            .entries()
            .iter()
            .any(|(_, child)| schema_misses_required(child))
        {
            return true;
        }
    }
    for keyword in SCHEMA_VALUE_KEYWORDS {
        let child = schema.g(keyword);
        if child.exists() && schema_misses_required(&child) {
            return true;
        }
    }
    false
}
