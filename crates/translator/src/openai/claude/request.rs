//! Claude Messages request -> OpenAI Chat Completions request
//! (Go: openai/claude/openai_claude_request.go).

use std::collections::HashMap;

use cpa_core::signature::{compatible_signature_for_provider, SignatureProvider};
use cpa_core::thinking::{self, level};
use cpa_core::util::{
    go_json_sorted, has_unsupported_unicode_property_escape, is_claude_code_attribution_system_text, GoJsonStyle,
    SCHEMA_MAP_KEYWORDS, SCHEMA_VALUE_KEYWORDS,
};
use crate::common::raw_in;
use cpa_json::{raw_children, Map, Res, Value, J};

use crate::common;

/// Placeholder text keeping an OpenAI tool message non-empty when a Claude tool_result carried
/// nothing but images.
const TOOL_RESULT_IMAGE_PLACEHOLDER: &str = "[Tool returned image content; the images follow in the next user message.]";

/// Labels the user message that carries relayed tool images.
const TOOL_RESULT_IMAGE_RELAY_NOTICE: &str = "Images returned by the preceding tool call(s):";

/// Converts a Claude Messages request into an OpenAI Chat Completions request.
pub fn convert_claude_request_to_openai(model_name: &str, input: &[u8], stream: bool) -> Vec<u8> {
    convert(model_name, input, stream, false)
}

/// As [`convert_claude_request_to_openai`], preserving assistant thinking text for configured
/// compatibility endpoints.
pub fn convert_claude_request_to_openai_with_compat(model_name: &str, input: &[u8], stream: bool) -> Vec<u8> {
    convert(model_name, input, stream, true)
}

fn convert(model_name: &str, input_bytes: &[u8], stream: bool, preserve_thinking_blocks: bool) -> Vec<u8> {
    let mut out = cpa_json::parse_str(r#"{"model":"","messages":[]}"#);
    let root = cpa_json::parse(input_bytes);

    cpa_json::set(&mut out, "model", model_name);

    let max_tokens = root.g("max_tokens");
    if max_tokens.exists() {
        cpa_json::set(&mut out, "max_tokens", max_tokens.int());
    }

    let temp = root.g("temperature");
    let top_p = root.g("top_p");
    if temp.exists() {
        cpa_json::set(&mut out, "temperature", cpa_json::num_f64(temp.float()));
    } else if top_p.exists() {
        cpa_json::set(&mut out, "top_p", cpa_json::num_f64(top_p.float()));
    }

    let stop_sequences = root.g("stop_sequences");
    if stop_sequences.is_array() {
        let stops: Vec<Value> = stop_sequences.array().iter().map(|v| Value::String(v.str())).collect();
        if !stops.is_empty() {
            cpa_json::set(&mut out, "stop", Value::Array(stops));
        }
    }

    cpa_json::set(&mut out, "stream", stream);

    // Thinking: Claude thinking.budget_tokens -> OpenAI reasoning_effort.
    let thinking_config = root.g("thinking");
    if thinking_config.is_object() {
        let thinking_type = thinking_config.g("type");
        if thinking_type.exists() {
            match thinking_type.str().as_str() {
                "enabled" => {
                    let budget_tokens = thinking_config.g("budget_tokens");
                    let effort_cfg = root.g("output_config.effort");
                    if budget_tokens.exists() {
                        if let Some(effort) = thinking::convert_budget_to_level(budget_tokens.int()).filter(|e| !e.is_empty()) {
                            cpa_json::set(&mut out, "reasoning_effort", effort);
                        }
                    } else if effort_cfg.is_string() && !effort_cfg.str().trim().is_empty() {
                        // Manual thinking paired with output_config.effort: keep the explicit level.
                        cpa_json::set(&mut out, "reasoning_effort", effort_cfg.str().trim().to_lowercase());
                    } else if let Some(effort) = thinking::convert_budget_to_level(-1).filter(|e| !e.is_empty()) {
                        cpa_json::set(&mut out, "reasoning_effort", effort);
                    }
                }
                "adaptive" | "auto" => {
                    // Adaptive thinking may carry output_config.effort; ApplyThinking clamps later.
                    let effort_cfg = root.g("output_config.effort");
                    let effort = if effort_cfg.is_string() { effort_cfg.str().trim().to_lowercase() } else { String::new() };
                    if effort.is_empty() {
                        cpa_json::set(&mut out, "reasoning_effort", level::XHIGH);
                    } else {
                        cpa_json::set(&mut out, "reasoning_effort", effort);
                    }
                }
                "disabled" => {
                    if let Some(effort) = thinking::convert_budget_to_level(0).filter(|e| !e.is_empty()) {
                        cpa_json::set(&mut out, "reasoning_effort", effort);
                    }
                }
                _ => {}
            }
        }
    }

    // System message first, then the conversation.
    let mut message_items: Vec<Value> = Vec::new();

    let mut system_content_items: Vec<Value> = Vec::new();
    let system = root.g("system");
    if system.exists() {
        if let Some(text) = system.as_str() {
            if !text.is_empty() && !is_claude_code_attribution_system_text(text) {
                let mut item = cpa_json::parse_str(r#"{"type":"text","text":""}"#);
                cpa_json::set(&mut item, "text", text);
                system_content_items.push(item);
            }
        } else if system.is_array() {
            for item in system.array() {
                if let Some(content_item) = convert_claude_content_part(&item) {
                    system_content_items.push(content_item);
                }
            }
        }
    }
    if !system_content_items.is_empty() {
        let mut system_message = cpa_json::parse_str(r#"{"role":"system","content":[]}"#);
        cpa_json::set(&mut system_message, "content", Value::Array(system_content_items));
        message_items.push(system_message);
    }

    let messages = root.g("messages");
    if messages.is_array() {
        let mut pending_tool_use_ids: Vec<String> = Vec::new();
        let mut pending_system_reminders: Vec<Value> = Vec::new();
        let mut tool_name_by_id: HashMap<String, String> = HashMap::new();

        let message_raws = raw_children(input_bytes, "messages");
        for (message_index, message) in messages.array().into_iter().enumerate() {
            let role = message.g("role").str();
            let mut content_result = message.g("content");
            if role == "system" {
                if let Some(reminder_text) = common::claude_message_system_reminder_text(&content_result) {
                    let mut msg = cpa_json::parse_str(r#"{"role":"user","content":[{"type":"text","text":""}]}"#);
                    cpa_json::set(&mut msg, "content.0.text", reminder_text);
                    if !pending_tool_use_ids.is_empty() {
                        pending_system_reminders.push(msg);
                    } else {
                        message_items.push(msg);
                    }
                }
                continue;
            }

            if content_result.is_array() {
                let original_content = content_result.clone();
                if role == "user" && !pending_tool_use_ids.is_empty() {
                    content_result = common::align_claude_tool_results(content_result, &pending_tool_use_ids);
                }
                let preceding_tool_calls_pending = !pending_tool_use_ids.is_empty();
                pending_tool_use_ids.clear();

                let mut content_items: Vec<Value> = Vec::new();
                let mut reasoning_parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                // tool_result messages are emitted before the main message.
                let mut tool_results: Vec<Value> = Vec::new();
                // Images pulled out of tool_result content for user-message relay.
                let mut relayed_tool_images: Vec<Value> = Vec::new();

                let part_raws = message_raws.get(message_index).map(|m| raw_children(m.as_bytes(), "content")).unwrap_or_default();
                let aligned_parts = content_result.array();
                let original_indices = original_part_indices(&original_content.array(), &aligned_parts);
                for (slot, part) in aligned_parts.into_iter().enumerate() {
                    // Raw copies come from the part's place in the client's original request.
                    let part_index = original_indices[slot];
                    match part.g("type").str().as_str() {
                        "thinking" => {
                            // Only assistant thinking maps to reasoning_content (prevents injection).
                            if role == "assistant" {
                                if !should_map_claude_thinking_to_gpt_reasoning(&part, preserve_thinking_blocks) {
                                    continue;
                                }
                                let thinking_text = thinking::get_thinking_text(part.v().unwrap_or(&Value::Null));
                                if !thinking_text.trim().is_empty() {
                                    reasoning_parts.push(thinking_text);
                                }
                            }
                        }
                        // redacted_thinking never maps to reasoning_content.
                        "redacted_thinking" => {}
                        "text" | "image" => {
                            if let Some(content_item) = convert_claude_content_part(&part) {
                                content_items.push(content_item);
                            }
                        }
                        "tool_use" => {
                            // Only assistant tool_use maps to tool_calls (prevents injection).
                            if role == "assistant" {
                                let tool_use_id = part.g("id").str();
                                let tool_name = part.g("name").str();
                                if !tool_use_id.is_empty() {
                                    pending_tool_use_ids.push(tool_use_id.clone());
                                    if !tool_name.is_empty() {
                                        tool_name_by_id.insert(tool_use_id.clone(), tool_name.clone());
                                    }
                                }
                                // Go marshals tool calls through map[string]any: sorted keys.
                                let mut tool_call = cpa_json::parse_str(r#"{"function":{"arguments":"","name":""},"id":"","type":"function"}"#);
                                cpa_json::set(&mut tool_call, "id", tool_use_id);
                                cpa_json::set(&mut tool_call, "function.name", tool_name);
                                let input = part.g("input");
                                if input.exists() {
                                    // Go copies `input.Raw` verbatim (client whitespace included).
                                    let raw = raw_in(part_raws.get(part_index), "input").map_or_else(|| input.raw(), str::to_string);
                                    cpa_json::set(&mut tool_call, "function.arguments", raw);
                                } else {
                                    cpa_json::set(&mut tool_call, "function.arguments", "{}");
                                }
                                tool_calls.push(tool_call);
                            }
                        }
                        "tool_result" => {
                            let tool_use_id = part.g("tool_use_id").str();
                            let mut tool_result = cpa_json::parse_str(r#"{"role":"tool","tool_call_id":"","content":""}"#);
                            cpa_json::set(&mut tool_result, "tool_call_id", tool_use_id.clone());
                            if let Some(tool_name) = tool_name_by_id.get(&tool_use_id).filter(|n| !n.is_empty()) {
                                cpa_json::set(&mut tool_result, "name", tool_name.clone());
                            }
                            let (content, images) = convert_claude_tool_result_content(
                                &part.g("content"),
                                &RawSource::new(raw_in(part_raws.get(part_index), "content")),
                            );
                            cpa_json::set(&mut tool_result, "content", content);
                            relayed_tool_images.extend(images);
                            tool_results.push(tool_result);
                        }
                        _ => {}
                    }
                }

                let reasoning_content = reasoning_parts.join("\n\n");
                let has_content = !content_items.is_empty();
                let has_reasoning = !reasoning_content.is_empty();
                let has_tool_calls = !tool_calls.is_empty();
                let has_tool_results = !tool_results.is_empty();

                // Flush pending system reminders if no tool_result answered the preceding calls.
                if preceding_tool_calls_pending && !has_tool_results && !pending_system_reminders.is_empty() {
                    message_items.append(&mut pending_system_reminders);
                }

                // OpenAI requires tool messages to directly follow the assistant tool_calls, so
                // tool results go first, then queued system reminders, then the current message.
                message_items.extend(tool_results);

                // OpenAI tool messages cannot carry images: replay them as a user message.
                if !relayed_tool_images.is_empty() {
                    let mut relay_items: Vec<Value> = Vec::with_capacity(relayed_tool_images.len() + 1);
                    let mut notice = cpa_json::parse_str(r#"{"type":"text","text":""}"#);
                    cpa_json::set(&mut notice, "text", TOOL_RESULT_IMAGE_RELAY_NOTICE);
                    relay_items.push(notice);
                    relay_items.append(&mut relayed_tool_images);

                    if role == "user" && has_content {
                        // Merge into the current user message so the request keeps one user turn.
                        relay_items.append(&mut content_items);
                        content_items = relay_items;
                    } else {
                        let mut relay = cpa_json::parse_str(r#"{"role":"user"}"#);
                        cpa_json::set(&mut relay, "content", Value::Array(relay_items));
                        message_items.push(relay);
                    }
                }

                message_items.append(&mut pending_system_reminders);

                if role == "assistant" {
                    if has_content || has_reasoning || has_tool_calls {
                        let mut msg = cpa_json::parse_str(r#"{"role":"assistant"}"#);
                        if has_content {
                            cpa_json::set(&mut msg, "content", Value::Array(content_items));
                        } else {
                            // Keep a content field for OpenAI compatibility.
                            cpa_json::set(&mut msg, "content", "");
                        }
                        if has_reasoning {
                            cpa_json::set(&mut msg, "reasoning_content", reasoning_content);
                        }
                        if has_tool_calls {
                            cpa_json::set(&mut msg, "tool_calls", Value::Array(tool_calls));
                        }
                        message_items.push(msg);
                    }
                } else if has_content {
                    // Tool-result-only messages were already emitted above.
                    let mut msg = cpa_json::parse_str(r#"{"role":""}"#);
                    cpa_json::set(&mut msg, "role", role);
                    cpa_json::set(&mut msg, "content", Value::Array(content_items));
                    message_items.push(msg);
                }
            } else if content_result.is_string() {
                let mut msg = cpa_json::parse_str(r#"{"role":"","content":""}"#);
                cpa_json::set(&mut msg, "role", role);
                cpa_json::set(&mut msg, "content", content_result.str());
                message_items.push(msg);
            }
        }
        message_items.append(&mut pending_system_reminders);
    }

    if !message_items.is_empty() {
        let raw_items: Vec<Vec<u8>> = message_items.iter().map(cpa_json::to_vec).collect();
        let aligned = common::align_openai_tool_call_messages(&raw_items, &[]);
        let items: Vec<Value> = aligned.iter().map(|raw| cpa_json::parse(raw)).collect();
        cpa_json::set(&mut out, "messages", Value::Array(items));
    }

    // Tools: Anthropic tools -> OpenAI functions.
    let tools = root.g("tools");
    if tools.is_array() {
        let mut tool_items: Vec<Value> = Vec::new();
        for tool in tools.array() {
            let mut openai_tool = cpa_json::parse_str(r#"{"type":"function","function":{"name":"","description":""}}"#);
            cpa_json::set(&mut openai_tool, "function.name", tool.g("name").str());
            cpa_json::set(&mut openai_tool, "function.description", tool.g("description").str());

            let input_schema = tool.g("input_schema");
            if input_schema.exists() && !input_schema.is_null() {
                // Go decodes into map[string]any and re-marshals: sorted keys, float64 numbers.
                let mut schema = input_schema.value();
                normalize_object_schema_properties(&mut schema);
                let schema = match go_json_sorted(&schema, GoJsonStyle::MARSHAL_ANY) {
                    Some(s) => cpa_json::parse_str(&s),
                    None => schema,
                };
                cpa_json::set(&mut openai_tool, "function.parameters", schema);
            } else {
                cpa_json::set(&mut openai_tool, "function.parameters", cpa_json::parse_str(r#"{"type":"object","properties":{}}"#));
            }
            tool_items.push(openai_tool);
        }
        if !tool_items.is_empty() {
            cpa_json::set(&mut out, "tools", Value::Array(tool_items));
        }
    }

    // Tool choice mapping.
    let tool_choice = root.g("tool_choice");
    if tool_choice.exists() && !tool_choice.is_null() {
        let mut choice_type = tool_choice.g("type").str();
        if choice_type.is_empty() && tool_choice.is_string() {
            choice_type = tool_choice.str();
        }
        match choice_type.as_str() {
            "auto" => {
                cpa_json::set(&mut out, "tool_choice", "auto");
            }
            "any" => {
                cpa_json::set(&mut out, "tool_choice", "required");
            }
            "none" => {
                cpa_json::set(&mut out, "tool_choice", "none");
            }
            "tool" => {
                let tool_name = tool_choice.g("name").str();
                if tool_name.is_empty() {
                    cpa_json::set(&mut out, "tool_choice", "none");
                } else {
                    let mut choice = cpa_json::parse_str(r#"{"type":"function","function":{"name":""}}"#);
                    cpa_json::set(&mut choice, "function.name", tool_name);
                    cpa_json::set(&mut out, "tool_choice", choice);
                }
            }
            // Fail closed: unrecognized tool_choice values must not turn into permission.
            _ => {
                cpa_json::set(&mut out, "tool_choice", "none");
            }
        }

        if tool_choice.g("disable_parallel_tool_use").v() == Some(&Value::Bool(true)) {
            cpa_json::set(&mut out, "parallel_tool_calls", false);
        }
    }

    let user = root.g("user");
    if user.exists() {
        cpa_json::set(&mut out, "user", user.str());
    }

    cpa_json::to_vec(&out)
}

/// Normalizes a JSON Schema for OpenAI: boolean `true` subschemas become `{}`, object schemas get
/// `properties`, and patterns using unsupported Unicode property escapes are dropped.
fn normalize_object_schema_properties(schema: &mut Value) {
    match schema {
        // `true` (accept anything) becomes `{}`; `false` (reject all) is preserved.
        Value::Bool(true) => *schema = Value::Object(Map::new()),
        Value::Object(value) => {
            if value.get("type").and_then(Value::as_str) == Some("object") && !value.contains_key("properties") {
                value.insert("properties".into(), Value::Object(Map::new()));
            }
            if value.get("pattern").and_then(Value::as_str).is_some_and(has_unsupported_unicode_property_escape) {
                value.shift_remove("pattern");
            }

            if let Some(Value::Object(pattern_props)) = value.get_mut("patternProperties") {
                let keys: Vec<String> = pattern_props.keys().cloned().collect();
                for key in keys {
                    if has_unsupported_unicode_property_escape(&key) {
                        pattern_props.shift_remove(&key);
                    } else if let Some(sub) = pattern_props.get_mut(&key) {
                        normalize_object_schema_properties(sub);
                    }
                }
            }

            for map_key in SCHEMA_MAP_KEYWORDS {
                if map_key == "patternProperties" {
                    continue;
                }
                if let Some(Value::Object(sub_map)) = value.get_mut(map_key) {
                    for sub in sub_map.values_mut() {
                        normalize_object_schema_properties(sub);
                    }
                }
            }

            for val_key in SCHEMA_VALUE_KEYWORDS {
                let Some(val) = value.get_mut(val_key) else { continue };
                match val {
                    // Boolean subschemas are normalized, but boolean additionalProperties stays
                    // as OpenAI structured outputs require it.
                    Value::Bool(true) if val_key != "additionalProperties" => *val = Value::Object(Map::new()),
                    Value::Object(_) => normalize_object_schema_properties(val),
                    Value::Array(items) => items.iter_mut().for_each(normalize_object_schema_properties),
                    _ => {}
                }
            }
        }
        Value::Array(children) => children.iter_mut().for_each(normalize_object_schema_properties),
        _ => {}
    }
}

/// Whether an assistant thinking block may be replayed as GPT reasoning: always when compat mode
/// preserves thinking, otherwise only with a GPT-compatible signature.
fn should_map_claude_thinking_to_gpt_reasoning(part: &Res<'_>, preserve_thinking: bool) -> bool {
    if preserve_thinking {
        return true;
    }
    let signature = part.g("signature");
    if !signature.exists() || signature.str().trim().is_empty() {
        return false;
    }
    compatible_signature_for_provider(SignatureProvider::Gpt, &signature.str()).is_some()
}

/// Converts a Claude `text` or `image` content part into an OpenAI content part.
fn convert_claude_content_part(part: &Res<'_>) -> Option<Value> {
    match part.g("type").str().as_str() {
        "text" => {
            let text = part.g("text").str();
            if text.trim().is_empty() || is_claude_code_attribution_system_text(&text) {
                return None;
            }
            let mut content = cpa_json::parse_str(r#"{"type":"text","text":""}"#);
            cpa_json::set(&mut content, "text", text);
            Some(content)
        }
        "image" => {
            let mut image_url = String::new();
            let source = part.g("source");
            if source.exists() {
                match source.g("type").str().as_str() {
                    "base64" => {
                        let mut media_type = source.g("media_type").str();
                        if media_type.is_empty() {
                            media_type = "application/octet-stream".into();
                        }
                        let data = source.g("data").str();
                        if !data.is_empty() {
                            image_url = format!("data:{media_type};base64,{data}");
                        }
                    }
                    "url" => image_url = source.g("url").str(),
                    _ => {}
                }
            }
            if image_url.is_empty() {
                image_url = part.g("url").str();
            }
            if image_url.is_empty() {
                return None;
            }
            let mut content = cpa_json::parse_str(r#"{"type":"image_url","image_url":{"url":""}}"#);
            cpa_json::set(&mut content, "image_url.url", image_url);
            Some(content)
        }
        _ => None,
    }
}

/// Original array index of each part of `aligned`, which is `original` with tool_result parts
/// permuted by `align_claude_tool_results`.
fn original_part_indices(original: &[Res<'_>], aligned: &[Res<'_>]) -> Vec<usize> {
    let mut used = vec![false; original.len()];
    aligned
        .iter()
        .enumerate()
        .map(|(slot, part)| {
            let same = |j: usize| !used[j] && original[j].v() == part.v();
            let pos = if slot < original.len() && same(slot) { slot } else { (0..original.len()).find(|&j| same(j)).unwrap_or(slot) };
            if let Some(u) = used.get_mut(pos) {
                *u = true;
            }
            pos
        })
        .collect()
}

/// Source text of a tool_result `content` and of its elements, for verbatim `Raw` copies.
struct RawSource<'a> {
    whole: Option<&'a str>,
    items: Vec<&'a str>,
}

impl<'a> RawSource<'a> {
    fn new(whole: Option<&'a str>) -> Self {
        let items = whole.map(|w| raw_children(w.as_bytes(), "")).unwrap_or_default();
        Self { whole, items }
    }

    /// Original text of the value (or of its `index`th element), falling back to compact JSON.
    fn raw(&self, index: Option<usize>, fallback: &Res<'_>) -> String {
        let found = match index {
            Some(i) => self.items.get(i).copied(),
            None => self.whole,
        };
        found.map_or_else(|| fallback.raw(), str::to_string)
    }
}

/// Flattens a Claude tool_result `content` into OpenAI tool message text plus any images that
/// must be relayed in a following user message.
fn convert_claude_tool_result_content(content: &Res<'_>, src: &RawSource<'_>) -> (String, Vec<Value>) {
    if !content.exists() {
        return (String::new(), Vec::new());
    }
    if content.is_string() {
        return (content.str(), Vec::new());
    }

    if content.is_array() {
        let mut parts: Vec<String> = Vec::new();
        let mut images: Vec<Value> = Vec::new();
        for (item_index, item) in content.array().into_iter().enumerate() {
            let item_type = item.g("type").str();
            let text = item.g("text");
            if item.is_string() {
                parts.push(item.str());
            } else if item.is_object() && item_type == "text" {
                parts.push(text.str());
            } else if item.is_object() && item_type == "image" {
                match convert_claude_content_part(&item) {
                    Some(content_item) => images.push(content_item),
                    None => parts.push(src.raw(Some(item_index), &item)),
                }
            } else if item.is_object() && text.is_string() {
                parts.push(text.str());
            } else {
                parts.push(src.raw(Some(item_index), &item));
            }
        }

        let joined = parts.join("\n\n");
        if joined.trim().is_empty() {
            if !images.is_empty() {
                return (TOOL_RESULT_IMAGE_PLACEHOLDER.to_string(), images);
            }
            return (src.raw(None, content), Vec::new());
        }
        return (joined, images);
    }

    if content.is_object() {
        if content.g("type").str() == "image"
            && let Some(content_item) = convert_claude_content_part(content)
        {
            return (TOOL_RESULT_IMAGE_PLACEHOLDER.to_string(), vec![content_item]);
        }
        let text = content.g("text");
        if text.is_string() {
            return (text.str(), Vec::new());
        }
        return (src.raw(None, content), Vec::new());
    }

    (src.raw(None, content), Vec::new())
}
