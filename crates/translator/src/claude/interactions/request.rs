//! Interactions request -> Claude Messages request (Go: interactions_claude_request.go).

use cpa_core::thinking::convert_level_to_budget;
use cpa_core::util::{normalize_claude_tool_input_schema, sanitize_claude_function_name, sanitize_claude_tool_id};
use cpa_json::{json, Res, Value};

use crate::common::{set_raw_array_items, ClaudeMessageAccumulator};

/// Converts an Interactions API request (steps based `input`) into a Claude Messages request.
pub fn convert_interactions_request_to_claude(model_name: &str, raw_json: &[u8], stream: bool) -> Vec<u8> {
    let root_value = cpa_json::parse(raw_json);
    let root = Res::of(&root_value);
    let mut out = cpa_json::parse_str(r#"{"model":"","max_tokens":32000,"messages":[]}"#);
    cpa_json::set(&mut out, "model", model_name);
    if stream || root.g("stream").bool() {
        cpa_json::set(&mut out, "stream", true);
    }
    copy_system(&mut out, &root);
    copy_generation_config(&mut out, &root);
    let mut accumulator = ClaudeMessageAccumulator::new(root.g("input.#").int().max(0) as usize);
    append_input_to_messages(&mut accumulator, &root.g("input"));
    let messages = accumulator.messages();
    let mut out = cpa_json::parse(&set_raw_array_items(&cpa_json::to_vec(&out), "messages", &messages));
    copy_tools(&mut out, &root);
    cpa_json::to_vec(&out)
}

fn copy_system(out: &mut Value, root: &Res<'_>) {
    let sys = first_existing(root, &["system_instruction", "systemInstruction"]);
    let text = claude_text(&sys);
    if text.is_empty() {
        return;
    }
    cpa_json::set(out, "system", text);
}

fn copy_generation_config(out: &mut Value, root: &Res<'_>) {
    let cfg = first_existing(root, &["generation_config", "generationConfig"]);
    if cfg.exists() {
        copy_json_field(out, &cfg, "max_output_tokens", "max_tokens");
        copy_json_field(out, &cfg, "maxOutputTokens", "max_tokens");
        copy_json_field(out, &cfg, "top_p", "top_p");
        copy_json_field(out, &cfg, "topP", "top_p");
        copy_json_field(out, &cfg, "temperature", "temperature");
        copy_json_field(out, &cfg, "stop_sequences", "stop_sequences");
        copy_json_field(out, &cfg, "stopSequences", "stop_sequences");
        let level = first_existing(&cfg, &["thinking_level", "thinkingLevel", "reasoning.effort"]);
        if level.exists() {
            set_thinking_from_level(out, &level.str());
        }
        copy_tool_choice(out, &cfg.g("tool_choice"));
        copy_tool_choice(out, &cfg.g("toolChoice"));
    }
    copy_reasoning(out, &root.g("reasoning"));
    copy_tool_choice(out, &root.g("tool_choice"));
    copy_tool_choice(out, &root.g("toolChoice"));
}

/// Copies the raw value at `from` to `to` when it exists.
fn copy_json_field(out: &mut Value, root: &Res<'_>, from: &str, to: &str) {
    let value = root.g(from);
    if value.exists() {
        cpa_json::set(out, to, value.value());
    }
}

fn copy_reasoning(out: &mut Value, reasoning: &Res<'_>) {
    if !reasoning.exists() {
        return;
    }
    let effort = reasoning.g("effort");
    if effort.exists() {
        set_thinking_from_level(out, &effort.str());
        return;
    }
    let level = reasoning.g("thinking_level");
    if level.exists() {
        set_thinking_from_level(out, &level.str());
    }
}

/// Maps a thinking level onto Claude `thinking`; unknown levels become adaptive thinking with
/// `output_config.effort` set to the level.
fn set_thinking_from_level(out: &mut Value, level: &str) {
    let normalized = level.trim().to_lowercase();
    if normalized.is_empty() {
        return;
    }
    match normalized.as_str() {
        "none" | "disabled" | "off" | "false" => {
            cpa_json::set(out, "thinking.type", "disabled");
            cpa_json::delete(out, "thinking.budget_tokens");
            return;
        }
        "auto" | "adaptive" => {
            cpa_json::set(out, "thinking.type", "adaptive");
            cpa_json::delete(out, "thinking.budget_tokens");
            return;
        }
        _ => {}
    }
    if let Some(budget) = convert_level_to_budget(&normalized) {
        match budget {
            0 => {
                cpa_json::set(out, "thinking.type", "disabled");
            }
            b if b < 0 => {
                cpa_json::set(out, "thinking.type", "enabled");
            }
            b => {
                cpa_json::set(out, "thinking.type", "enabled");
                cpa_json::set(out, "thinking.budget_tokens", b);
            }
        }
        return;
    }
    cpa_json::set(out, "thinking.type", "adaptive");
    cpa_json::set(out, "output_config.effort", normalized);
}

fn append_input_to_messages(accumulator: &mut ClaudeMessageAccumulator, input: &Res<'_>) {
    if !input.exists() {
        return;
    }
    if input.is_string() {
        let step = json!({"type": "user_input", "content": [{"type": "text", "text": input.str()}]});
        append_step(accumulator, &Res::owned(step), "user");
        return;
    }
    if input.is_object() {
        append_input_item(accumulator, input);
        return;
    }
    input.for_each(|_, step| {
        append_input_item(accumulator, &step);
        true
    });
}

fn append_input_item(accumulator: &mut ClaudeMessageAccumulator, step: &Res<'_>) {
    let nested_steps = step.g("steps");
    if nested_steps.is_array() {
        let role = step.g("role").str();
        let default_role = if role == "model" || role == "assistant" { "assistant" } else { "user" };
        nested_steps.for_each(|_, nested| {
            append_step(accumulator, &nested, default_role);
            true
        });
        return;
    }
    let parts = step.g("parts");
    if parts.exists() {
        let mut wrapped = json!({"type": "user_input", "content": []});
        let role = step.g("role").str();
        if role == "model" || role == "assistant" {
            cpa_json::set(&mut wrapped, "type", "model_output");
        }
        cpa_json::set(&mut wrapped, "content", parts.value());
        append_step(accumulator, &Res::owned(wrapped), "user");
        return;
    }
    match step.g("type").str().as_str() {
        "function_call" => append_function_call(accumulator, step),
        "function_result" => append_function_result(accumulator, step),
        "model_output" | "thought" => append_step(accumulator, step, "assistant"),
        _ => append_step(accumulator, step, "user"),
    }
}

fn append_step(accumulator: &mut ClaudeMessageAccumulator, step: &Res<'_>, default_role: &str) {
    let mut role = default_role.to_string();
    let step_role = step.g("role").str();
    if step_role == "user" || step_role == "assistant" {
        role = step_role;
    }
    let mut content_items: Vec<Value> = Vec::with_capacity(4);
    let step_content = step.g("content");
    if step_content.is_string() {
        content_items.push(json!({"type": "text", "text": step_content.str()}));
    } else if step_content.is_array() {
        step_content.for_each(|_, part| {
            if let Some(converted) = content_to_claude(&part, &role) {
                content_items.push(converted);
            }
            true
        });
    } else {
        let text = step.g("text");
        if text.exists() {
            content_items.push(json!({"type": "text", "text": text.str()}));
        }
    }
    if content_items.is_empty() {
        return;
    }
    let msg = json!({"role": role, "content": content_items});
    accumulator.append(&cpa_json::to_vec(&msg));
}

fn content_to_claude(part: &Res<'_>, role: &str) -> Option<Value> {
    let mut part_type = part.g("type").str();
    if part_type.is_empty() && part.g("text").exists() {
        part_type = "text".into();
    }
    match part_type.as_str() {
        "text" => Some(json!({"type": "text", "text": part.g("text").str()})),
        "thinking" | "reasoning" => {
            if role != "assistant" {
                return None;
            }
            Some(json!({"type": "thinking", "thinking": claude_text(part)}))
        }
        "image" => media_part(part, "image"),
        "document" | "file" => media_part(part, "document"),
        _ => {
            let text = claude_text(part);
            if !text.is_empty() {
                return Some(json!({"type": "text", "text": text}));
            }
            if !part.g("data").str().is_empty() || !part.g("file_data").str().is_empty() {
                return Some(json!({"type": "text", "text": format!("[{part_type} content omitted]")}));
            }
            None
        }
    }
}

fn append_function_call(accumulator: &mut ClaudeMessageAccumulator, step: &Res<'_>) {
    let mut tool_use = json!({"type": "tool_use", "id": "", "name": "", "input": {}});
    cpa_json::set(&mut tool_use, "id", tool_id(step));
    cpa_json::set(&mut tool_use, "name", sanitize_claude_function_name(&step.g("name").str()));
    let args = first_existing(step, &["arguments", "args"]);
    if args.is_object() {
        cpa_json::set(&mut tool_use, "input", args.value());
    }
    let msg = json!({"role": "assistant", "content": [tool_use]});
    accumulator.append(&cpa_json::to_vec(&msg));
}

fn append_function_result(accumulator: &mut ClaudeMessageAccumulator, step: &Res<'_>) {
    let mut tool_result = json!({"type": "tool_result", "tool_use_id": "", "content": ""});
    cpa_json::set(&mut tool_result, "tool_use_id", tool_id(step));
    let is_error = step.g("is_error");
    if is_error.exists() && is_error.bool() {
        cpa_json::set(&mut tool_result, "is_error", true);
    }
    let result = first_existing(step, &["result", "output"]);
    if result.is_array() {
        let mut content_items: Vec<Value> = Vec::with_capacity(4);
        result.for_each(|_, part| {
            if let Some(converted) = content_to_claude(&part, "user") {
                content_items.push(converted);
            }
            true
        });
        cpa_json::set(&mut tool_result, "content", Value::Array(content_items));
    } else if result.exists() && !result.raw().is_empty() {
        // The JSON text of the result, quotes included for plain strings.
        cpa_json::set(&mut tool_result, "content", result.raw());
    } else {
        cpa_json::set(&mut tool_result, "content", "");
    }
    let msg = json!({"role": "user", "content": [tool_result]});
    accumulator.append(&cpa_json::to_vec(&msg));
}

fn copy_tools(out: &mut Value, root: &Res<'_>) {
    let tools = root.g("tools");
    if !tools.is_array() {
        return;
    }
    let mut tool_items: Vec<Value> = Vec::new();
    tools.for_each(|_, tool| {
        for key in ["function_declarations", "functionDeclarations"] {
            let decls = tool.g(key);
            if decls.is_array() {
                decls.for_each(|_, decl| {
                    if let Some(converted) = claude_tool(&decl) {
                        tool_items.push(converted);
                    }
                    true
                });
                return true;
            }
        }
        if let Some(converted) = claude_tool(&tool) {
            tool_items.push(converted);
        }
        true
    });
    if !tool_items.is_empty() {
        cpa_json::set(out, "tools", Value::Array(tool_items));
    }
}

fn claude_tool(tool: &Res<'_>) -> Option<Value> {
    let mut name = tool.g("name").str();
    if name.is_empty() {
        name = tool.g("function.name").str();
    }
    if name.is_empty() {
        return None;
    }
    let mut converted = json!({"name": "", "input_schema": {"type": "object", "properties": {}}});
    cpa_json::set(&mut converted, "name", sanitize_claude_function_name(&name));
    let desc = tool.g("description");
    if desc.exists() {
        cpa_json::set(&mut converted, "description", desc.str());
    } else {
        let desc = tool.g("function.description");
        if desc.exists() {
            cpa_json::set(&mut converted, "description", desc.str());
        }
    }
    let params = first_existing(tool, &["parameters", "parametersJsonSchema", "parameters_json_schema", "input_schema"]);
    if params.is_object() {
        let schema = normalize_claude_tool_input_schema(params.raw().as_bytes());
        cpa_json::set(&mut converted, "input_schema", cpa_json::parse(&schema));
    }
    Some(converted)
}

fn copy_tool_choice(out: &mut Value, tool_choice: &Res<'_>) {
    if !tool_choice.exists() {
        return;
    }
    if tool_choice.is_string() {
        match tool_choice.str().trim().to_lowercase().as_str() {
            "auto" => {
                cpa_json::set(out, "tool_choice", json!({"type": "auto"}));
            }
            "required" | "any" => {
                cpa_json::set(out, "tool_choice", json!({"type": "any"}));
            }
            _ => {}
        }
    } else if tool_choice.is_object() || tool_choice.is_array() {
        match tool_choice.g("type").str().trim().to_lowercase().as_str() {
            "auto" => {
                cpa_json::set(out, "tool_choice", json!({"type": "auto"}));
            }
            "required" | "any" => {
                cpa_json::set(out, "tool_choice", json!({"type": "any"}));
            }
            "function" | "tool" => {
                let mut name = tool_choice.g("name").str();
                if name.is_empty() {
                    name = tool_choice.g("function.name").str();
                }
                if !name.is_empty() {
                    let choice = json!({"type": "tool", "name": sanitize_claude_function_name(&name)});
                    cpa_json::set(out, "tool_choice", choice);
                }
            }
            _ => {}
        }
    }
}

/// call_id, id or tool_use_id (sanitized), else `toolu_<name>`, else `toolu_interactions`.
fn tool_id(step: &Res<'_>) -> String {
    for path in ["call_id", "id", "tool_use_id"] {
        let value = step.g(path).str();
        if !value.is_empty() {
            return sanitize_claude_tool_id(&value);
        }
    }
    let name = step.g("name").str();
    if !name.is_empty() {
        return sanitize_claude_tool_id(&format!("toolu_{name}"));
    }
    "toolu_interactions".into()
}

/// Text of a string, `text`, `thinking`, nested `content`, or newline-joined `parts`.
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
    let thinking = value.g("thinking");
    if thinking.exists() {
        return thinking.str();
    }
    let content = value.g("content");
    if content.exists() {
        return claude_text(&content);
    }
    let parts = value.g("parts");
    if parts.is_array() {
        let mut builder = String::new();
        parts.for_each(|_, part| {
            let text = claude_text(&part);
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

/// A base64 `image`/`document` block; both a media type and data are required.
fn media_part(part: &Res<'_>, claude_type: &str) -> Option<Value> {
    let mut mime_type = first_existing(part, &["mime_type", "mimeType", "media_type", "mediaType"]).str();
    let mut data = first_existing(part, &["data", "file_data", "fileData"]).str();
    let source = part.g("source");
    if source.exists() {
        if mime_type.is_empty() {
            mime_type = source.g("media_type").str();
        }
        if data.is_empty() {
            data = source.g("data").str();
        }
    }
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(json!({"type": claude_type, "source": {"type": "base64", "media_type": mime_type, "data": data}}))
}

/// The first of `paths` that exists in `root`, else [`Res::NONE`].
fn first_existing<'a>(root: &'a Res<'_>, paths: &[&str]) -> Res<'a> {
    for path in paths {
        let value = root.g(path);
        if value.exists() {
            return value;
        }
    }
    Res::NONE
}
