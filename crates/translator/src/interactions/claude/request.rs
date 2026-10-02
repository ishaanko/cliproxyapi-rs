//! Claude Messages request -> Interactions request (Go: interactions_claude_request.go).

use std::collections::HashMap;

use cpa_json::{json, Res, Value};

use crate::common::{
    align_claude_tool_results, claude_message_system_reminder_text, set_raw_array_items,
};

/// Converts a Claude Messages request into an Interactions API request (steps based `input`).
pub fn convert_claude_request_to_interactions(model_name: &str, raw_json: &[u8], stream: bool) -> Vec<u8> {
    convert(model_name, raw_json, stream, false)
}

/// Like [`convert_claude_request_to_interactions`], but keeps empty assistant thinking blocks as
/// `thought` steps (needed by some compatibility endpoints).
pub fn convert_claude_request_to_interactions_with_compat(model_name: &str, raw_json: &[u8], stream: bool) -> Vec<u8> {
    convert(model_name, raw_json, stream, true)
}

fn convert(model_name: &str, raw_json: &[u8], stream: bool, preserve_empty_thinking_blocks: bool) -> Vec<u8> {
    let root_value = cpa_json::parse(raw_json);
    let root = Res::of(&root_value);
    let mut out = json!({"model": "", "input": []});
    let request_model = root.g("model").str();
    let model = if !model_name.trim().is_empty() { model_name.to_string() } else { request_model };
    cpa_json::set(&mut out, "model", model);
    if let Some(stream_value) = request_stream_value(&root, stream) {
        cpa_json::set(&mut out, "stream", stream_value);
    }
    copy_system(&mut out, &root);
    copy_generation_config(&mut out, &root);
    append_messages(&mut out, &root.g("messages"), preserve_empty_thinking_blocks);
    copy_tools(&mut out, &root);
    cpa_json::to_vec(&out)
}

/// The request's own `stream` wins; otherwise the argument when true.
fn request_stream_value(root: &Res<'_>, stream: bool) -> Option<bool> {
    let value = root.g("stream");
    if value.exists() {
        return Some(value.bool());
    }
    stream.then_some(true)
}

fn copy_system(out: &mut Value, root: &Res<'_>) {
    let text = claude_text(&root.g("system"));
    if text.is_empty() {
        return;
    }
    cpa_json::set(out, "system_instruction", text);
}

fn copy_generation_config(out: &mut Value, root: &Res<'_>) {
    copy_json_field(out, root, "max_tokens", "generation_config.max_output_tokens");
    copy_json_field(out, root, "temperature", "generation_config.temperature");
    copy_json_field(out, root, "top_p", "generation_config.top_p");
    copy_json_field(out, root, "stop_sequences", "generation_config.stop_sequences");
    copy_thinking(out, root);
    copy_tool_choice(out, &root.g("tool_choice"));
}

/// Copies the raw value at `from` to `to` when it exists.
fn copy_json_field(out: &mut Value, root: &Res<'_>, from: &str, to: &str) {
    let value = root.g(from);
    if value.exists() {
        cpa_json::set(out, to, value.value());
    }
}

fn copy_thinking(out: &mut Value, root: &Res<'_>) {
    let thinking = root.g("thinking");
    if thinking.exists() {
        match thinking.g("type").str().trim().to_lowercase().as_str() {
            "disabled" => {
                cpa_json::set(out, "generation_config.thinking_level", "none");
            }
            "enabled" => {
                let budget = thinking.g("budget_tokens");
                if budget.exists() {
                    cpa_json::set(out, "generation_config.thinking_config.thinking_budget", budget.value());
                } else {
                    cpa_json::set(out, "generation_config.thinking_level", "high");
                }
            }
            "adaptive" => {
                cpa_json::set(out, "generation_config.thinking_level", "auto");
            }
            _ => {}
        }
    }
    let effort = root.g("output_config.effort");
    if effort.is_string() {
        cpa_json::set(out, "generation_config.thinking_level", effort.str().trim().to_lowercase());
    }
}

fn copy_tool_choice(out: &mut Value, tool_choice: &Res<'_>) {
    if !tool_choice.exists() {
        return;
    }
    if tool_choice.is_string() {
        match tool_choice.str().trim().to_lowercase().as_str() {
            "auto" => {
                cpa_json::set(out, "generation_config.tool_choice", "auto");
            }
            "any" | "required" => {
                cpa_json::set(out, "generation_config.tool_choice", "required");
            }
            _ => {}
        }
    } else if tool_choice.is_object() || tool_choice.is_array() {
        match tool_choice.g("type").str().trim().to_lowercase().as_str() {
            "auto" => {
                cpa_json::set(out, "generation_config.tool_choice", "auto");
            }
            "any" | "required" => {
                cpa_json::set(out, "generation_config.tool_choice", "required");
            }
            "tool" => {
                let name = tool_choice.g("name").str();
                let name = name.trim();
                if !name.is_empty() {
                    cpa_json::set(out, "generation_config.tool_choice", json!({"type": "function", "name": name}));
                }
            }
            _ => {}
        }
    }
}

/// Mutable conversion state threaded through the message loop.
#[derive(Default)]
struct MessageState {
    items: Vec<Value>,
    /// tool_use ids of the previous assistant message, used to order the next tool_results.
    pending_tool_use_ids: Vec<String>,
    /// Mid-conversation system reminders held back while tool results are still pending.
    pending_system_reminders: Vec<Value>,
    tool_names_by_id: HashMap<String, String>,
}

fn append_messages(out: &mut Value, messages: &Res<'_>, preserve_empty_thinking_blocks: bool) {
    if !messages.is_array() {
        return;
    }
    let mut state = MessageState {
        items: Vec::with_capacity(messages.g("#").int().max(0) as usize),
        ..Default::default()
    };

    messages.for_each(|_, message| {
        let role = message.g("role").str().trim().to_lowercase();
        let mut content = message.g("content");
        if role == "system" {
            if let Some(reminder_text) = claude_message_system_reminder_text(&content) {
                let step = json!({"type": "user_input", "content": [{"type": "text", "text": reminder_text}]});
                if !state.pending_tool_use_ids.is_empty() {
                    state.pending_system_reminders.push(step);
                } else {
                    state.items.push(step);
                }
            }
            return true;
        }

        if role == "user" && !state.pending_tool_use_ids.is_empty() && content.is_array() {
            content = align_claude_tool_results(content, &state.pending_tool_use_ids);
        }
        state.pending_tool_use_ids.clear();

        append_message(&mut state, &role, &content, preserve_empty_thinking_blocks);
        if !state.pending_system_reminders.is_empty() {
            let reminders = std::mem::take(&mut state.pending_system_reminders);
            state.items.extend(reminders);
        }
        true
    });
    if !state.pending_system_reminders.is_empty() {
        let reminders = std::mem::take(&mut state.pending_system_reminders);
        state.items.extend(reminders);
    }
    let items: Vec<Vec<u8>> = state.items.iter().map(cpa_json::to_vec).collect();
    let updated = set_raw_array_items(&cpa_json::to_vec(out), "input", &items);
    *out = cpa_json::parse(&updated);
}

/// Appends the steps for one Claude message. Consecutive text/media blocks form one step;
/// thinking, tool_use and tool_result blocks each become their own step.
fn append_message(state: &mut MessageState, role: &str, content: &Res<'_>, preserve_empty_thinking_blocks: bool) {
    let default_step_type = if role == "assistant" { "model_output" } else { "user_input" };
    if content.is_string() {
        let reminders = std::mem::take(&mut state.pending_system_reminders);
        state.items.extend(reminders);
        state.items.push(json!({
            "type": default_step_type,
            "content": [{"type": "text", "text": content.str()}],
        }));
        return;
    }
    if !content.is_array() {
        return;
    }

    let mut step_content: Vec<Value> = Vec::with_capacity(4);
    fn flush_content(state: &mut MessageState, step_content: &mut Vec<Value>, default_step_type: &str) {
        if step_content.is_empty() {
            return;
        }
        let parts = std::mem::take(step_content);
        state.items.push(json!({"type": default_step_type, "content": parts}));
    }

    content.for_each(|_, part| {
        let part_type = part.g("type").str().trim().to_lowercase();
        match part_type.as_str() {
            "text" => {
                let text = part.g("text").str();
                if !text.is_empty() {
                    if !state.pending_system_reminders.is_empty() {
                        flush_content(state, &mut step_content, default_step_type);
                        let reminders = std::mem::take(&mut state.pending_system_reminders);
                        state.items.extend(reminders);
                    }
                    step_content.push(json!({"type": "text", "text": text}));
                }
            }
            "thinking" => {
                flush_content(state, &mut step_content, default_step_type);
                let text = part.g("thinking").str();
                if !text.is_empty() || preserve_empty_thinking_blocks {
                    state.items.push(json!({"type": "thought", "content": [{"type": "text", "text": text}]}));
                }
            }
            "image" | "document" => {
                if let Some(media_part) = media_part_to_interactions(&part, &part_type) {
                    if !state.pending_system_reminders.is_empty() {
                        flush_content(state, &mut step_content, default_step_type);
                        let reminders = std::mem::take(&mut state.pending_system_reminders);
                        state.items.extend(reminders);
                    }
                    step_content.push(media_part);
                }
            }
            "tool_use" => {
                flush_content(state, &mut step_content, default_step_type);
                let id = part.g("id").str();
                if !id.is_empty() {
                    state.pending_tool_use_ids.push(id.clone());
                    let name = part.g("name").str();
                    if !name.is_empty() {
                        state.tool_names_by_id.insert(id, name);
                    }
                }
                state.items.push(tool_use_to_interactions(&part));
            }
            "tool_result" => {
                flush_content(state, &mut step_content, default_step_type);
                let step = tool_result_to_interactions(&part, &state.tool_names_by_id);
                state.items.push(step);
            }
            _ => {}
        }
        true
    });
    flush_content(state, &mut step_content, default_step_type);
}

/// An Interactions `image`/`document` part from a Claude base64 source; media type and data
/// are both required.
fn media_part_to_interactions(part: &Res<'_>, part_type: &str) -> Option<Value> {
    let source = part.g("source");
    let mut mime_type = source.g("media_type").str();
    let mut data = source.g("data").str();
    if data.is_empty() {
        data = part.g("data").str();
    }
    if mime_type.is_empty() {
        mime_type = part.g("mime_type").str();
    }
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(json!({"type": part_type, "mime_type": mime_type, "data": data}))
}

fn tool_use_to_interactions(part: &Res<'_>) -> Value {
    let mut step = json!({"type": "function_call", "name": "", "arguments": {}});
    cpa_json::set(&mut step, "name", part.g("name").str());
    let id = part.g("id").str();
    if !id.is_empty() {
        cpa_json::set(&mut step, "id", id);
    }
    let input = part.g("input");
    if input.is_object() {
        cpa_json::set(&mut step, "arguments", input.value());
    }
    step
}

fn tool_result_to_interactions(part: &Res<'_>, tool_names_by_id: &HashMap<String, String>) -> Value {
    let mut step = json!({"type": "function_result", "call_id": "", "result": ""});
    let id = part.g("tool_use_id").str();
    if !id.is_empty() {
        cpa_json::set(&mut step, "call_id", id.as_str());
    }
    let mut name = part.g("name").str();
    if name.is_empty() && !id.is_empty() {
        name = tool_names_by_id.get(&id).cloned().unwrap_or_default();
    }
    if !name.is_empty() {
        cpa_json::set(&mut step, "name", name);
    }
    let is_error = part.g("is_error");
    if is_error.exists() && is_error.bool() {
        cpa_json::set(&mut step, "is_error", true);
    }
    let result = part.g("content");
    if result.exists() {
        if result.is_string() {
            cpa_json::set(&mut step, "result", result.str());
        } else if result.is_array() {
            let mut content_items: Vec<Value> = Vec::with_capacity(4);
            result.for_each(|_, item| {
                let item_type = item.g("type").str();
                match item_type.as_str() {
                    "text" => {
                        // Text blocks with extra fields (citations, ...) are passed through raw.
                        let is_pure_text = !item.is_object()
                            || item.entries().iter().all(|(k, _)| matches!(*k, "type" | "text" | "cache_control"));
                        if is_pure_text {
                            content_items.push(json!({"type": "text", "text": item.g("text").str()}));
                        } else {
                            content_items.push(item.value());
                        }
                    }
                    "image" | "document" => {
                        content_items.push(media_part_to_interactions(&item, &item_type).unwrap_or_else(|| item.value()));
                    }
                    _ => content_items.push(item.value()),
                }
                true
            });
            cpa_json::set(&mut step, "result", Value::Array(content_items));
        } else {
            cpa_json::set(&mut step, "result", result.value());
        }
    }
    step
}

fn copy_tools(out: &mut Value, root: &Res<'_>) {
    let tools = root.g("tools");
    if !tools.is_array() {
        return;
    }
    let mut tool_items: Vec<Value> = Vec::new();
    tools.for_each(|_, tool| {
        let name = tool.g("name").str();
        let name = name.trim();
        if name.is_empty() {
            return true;
        }
        let mut item = json!({"type": "function", "name": "", "parameters": {}});
        cpa_json::set(&mut item, "name", name);
        let desc = tool.g("description");
        if desc.exists() {
            cpa_json::set(&mut item, "description", desc.str());
        }
        let schema = tool.g("input_schema");
        if schema.is_object() {
            cpa_json::set(&mut item, "parameters", schema.value());
        }
        tool_items.push(item);
        true
    });
    if !tool_items.is_empty() {
        cpa_json::set(out, "tools", Value::Array(tool_items));
    }
}

/// Text of a string, an object's `text`, or an array's newline-joined texts.
fn claude_text(value: &Res<'_>) -> String {
    if !value.exists() {
        return String::new();
    }
    if value.is_string() {
        return value.str();
    }
    let text = value.g("text");
    if text.exists() {
        return text.str();
    }
    if value.is_array() {
        let mut builder = String::new();
        value.for_each(|_, item| {
            let text = claude_text(&item);
            if text.is_empty() {
                return true;
            }
            if !builder.is_empty() {
                builder.push('\n');
            }
            builder.push_str(&text);
            true
        });
        return builder;
    }
    String::new()
}
