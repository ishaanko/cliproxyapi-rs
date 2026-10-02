//! Gemini request -> Claude Messages request (Go: claude_gemini_request.go).

use cpa_core::registry::lookup_model_info;
use cpa_core::thinking::{
    apply_translated_summary_to_claude, convert_budget_to_level, convert_level_to_budget, has_level,
    level as thinking_level, map_to_claude_effort,
};
use cpa_core::util::{go_json_sorted, sanitize_claude_function_name, walk, GoJsonStyle};
use cpa_json::{json, Res, Value, J};

use crate::common::{
    derive_claude_user_id, is_gemini_thought_part, join_raw_array, ClaudeMessageAccumulator,
};

/// Converts a Gemini `generateContent` request into a Claude Messages request. Tool call ids
/// are paired through a FIFO queue because Gemini pairs functionCall/functionResponse by order.
pub fn convert_gemini_request_to_claude(model_name: &str, raw_json: &[u8], stream: bool) -> Vec<u8> {
    let user_id = derive_claude_user_id(raw_json);

    let mut out = cpa_json::parse_str(r#"{"model":"","max_tokens":32000,"messages":[],"metadata":{}}"#);
    cpa_json::set(&mut out, "metadata.user_id", user_id);

    let root = cpa_json::parse(raw_json);
    let mut accumulator = ClaudeMessageAccumulator::new(root.g("contents.#").int().max(0) as usize + 1);

    // FIFO of generated tool ids waiting for their functionResponse.
    let mut pending_tool_ids: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut tool_call_counter = 0u64;

    cpa_json::set(&mut out, "model", model_name);
    let service_tier = root.g("service_tier");
    if service_tier.is_string() {
        cpa_json::set(&mut out, "service_tier", service_tier.str());
    }

    let gen_config = root.g("generationConfig");
    if gen_config.exists() {
        let max_tokens = gen_config.g("maxOutputTokens");
        if max_tokens.exists() {
            cpa_json::set(&mut out, "max_tokens", max_tokens.int());
        }
        let top_p = gen_config.g("topP");
        if top_p.exists() {
            cpa_json::set(&mut out, "top_p", cpa_json::num_f64(top_p.float()));
        }
        let stop_seqs = gen_config.g("stopSequences");
        if stop_seqs.is_array() {
            let stop_sequences: Vec<String> = stop_seqs.array().iter().map(Res::str).collect();
            if !stop_sequences.is_empty() {
                cpa_json::set(&mut out, "stop_sequences", stop_sequences);
            }
        }
        let thinking_config = gen_config.g("thinkingConfig");
        if thinking_config.is_object() {
            apply_thinking_config(&mut out, model_name, &thinking_config);
        }
    }

    let sys_instr = root.g("system_instruction");
    if sys_instr.exists() {
        let parts = sys_instr.g("parts");
        if parts.is_array() {
            let mut system_text = String::new();
            for part in parts.array() {
                if is_gemini_thought_part(&part) {
                    continue;
                }
                let text = part.g("text");
                if text.exists() {
                    if !system_text.is_empty() {
                        system_text.push('\n');
                    }
                    system_text.push_str(&text.str());
                }
            }
            if !system_text.is_empty() {
                // Kept as a leading user message, flushed so it stays a separate turn.
                let message = json!({"role": "user", "content": [{"type": "text", "text": system_text}]});
                accumulator.append(&cpa_json::to_vec(&message));
                accumulator.flush();
            }
        }
    }

    let contents = root.g("contents");
    if contents.is_array() {
        for content in contents.array() {
            let mut role = content.g("role").str();
            if role == "model" {
                role = "assistant".into();
            }
            if role == "function" || role == "tool" {
                role = "user".into();
            }

            let mut content_items: Vec<Value> = Vec::with_capacity(4);
            let parts = content.g("parts");
            if parts.is_array() {
                for part in parts.array() {
                    if is_gemini_thought_part(&part) {
                        continue;
                    }

                    let text = part.g("text");
                    if text.exists() {
                        content_items.push(claude_text_content_part(&text.str()));
                        continue;
                    }

                    let fc = part.g("functionCall");
                    if fc.exists() && role == "assistant" {
                        let mut tool_use = json!({"type": "tool_use", "id": "", "name": "", "input": {}});
                        let mut tool_id = gemini_tool_id(&fc);
                        if tool_id.is_empty() {
                            tool_call_counter += 1;
                            tool_id = format!("toolu_gemini_{tool_call_counter:016}");
                        }
                        pending_tool_ids.push_back(tool_id.clone());
                        cpa_json::set(&mut tool_use, "id", tool_id);
                        let name = fc.g("name");
                        if name.exists() {
                            cpa_json::set(&mut tool_use, "name", sanitize_claude_function_name(&name.str()));
                        }
                        let args = fc.g("args");
                        if args.is_object() {
                            cpa_json::set(&mut tool_use, "input", args.value());
                        }
                        content_items.push(tool_use);
                        continue;
                    }

                    let fr = part.g("functionResponse");
                    if fr.exists() {
                        let mut tool_result = json!({"type": "tool_result", "tool_use_id": "", "content": ""});
                        // Attach the oldest queued id; with an empty queue generate a new one.
                        let custom_id = gemini_tool_id(&fr);
                        let tool_id = if !custom_id.is_empty() {
                            if let Some(pos) = pending_tool_ids.iter().position(|id| *id == custom_id) {
                                pending_tool_ids.remove(pos);
                            }
                            custom_id
                        } else if let Some(front) = pending_tool_ids.pop_front() {
                            front
                        } else {
                            tool_call_counter += 1;
                            format!("toolu_gemini_{tool_call_counter:016}")
                        };
                        cpa_json::set(&mut tool_result, "tool_use_id", tool_id);

                        let result = fr.g("response.result");
                        if result.exists() {
                            cpa_json::set(&mut tool_result, "content", result.str());
                        } else {
                            let response = fr.g("response");
                            if response.exists() {
                                cpa_json::set(&mut tool_result, "content", response.raw());
                            }
                        }
                        content_items.push(tool_result);
                        continue;
                    }

                    let inline_data = first_existing(&part, "inlineData", "inline_data");
                    if inline_data.exists() {
                        if let Some(content_part) = content_part_from_inline_data(&inline_data) {
                            content_items.push(content_part);
                        }
                        continue;
                    }

                    let file_data = first_existing(&part, "fileData", "file_data");
                    if file_data.exists()
                        && let Some(content_part) = content_part_from_file_data(&file_data) {
                            content_items.push(content_part);
                        }
                }
            }

            if !content_items.is_empty() {
                let msg = json!({"role": role, "content": content_items});
                accumulator.append(&cpa_json::to_vec(&msg));
            }
        }
    }
    let messages = accumulator.messages();
    if !messages.is_empty() {
        cpa_json::set(&mut out, "messages", cpa_json::parse(&join_raw_array(&messages)));
    }

    let tools = root.g("tools");
    if tools.is_array() {
        let mut anthropic_tools: Vec<Value> = Vec::new();
        for tool in tools.array() {
            let func_decls = tool.g("functionDeclarations");
            if !func_decls.is_array() {
                continue;
            }
            for func_decl in func_decls.array() {
                anthropic_tools.push(claude_tool_from_declaration(&func_decl));
            }
        }
        if !anthropic_tools.is_empty() {
            // Go round-trips the tools through map[string]any: keys sorted, numbers as float64.
            let tools_value = Value::Array(anthropic_tools);
            let tools_value = go_json_sorted(&tools_value, GoJsonStyle::MARSHAL_ANY)
                .map(|s| cpa_json::parse_str(&s))
                .unwrap_or(tools_value);
            cpa_json::set(&mut out, "tools", tools_value);
        }
    }

    let tool_config = root.g("tool_config");
    if tool_config.exists() {
        set_tool_choice_from_gemini_tool_config(&mut out, &tool_config.g("function_calling_config"));
    } else {
        let tool_config = root.g("toolConfig");
        if tool_config.exists() {
            set_tool_choice_from_gemini_tool_config(&mut out, &tool_config.g("functionCallingConfig"));
        }
    }

    cpa_json::set(&mut out, "stream", stream);

    apply_translated_summary_to_claude(&cpa_json::to_vec(&out), raw_json, "gemini", model_name)
}

/// Maps `generationConfig.thinkingConfig` onto Claude `thinking` / `output_config.effort`.
/// The translator only converts format; capability validation happens later in ApplyThinking.
fn apply_thinking_config(out: &mut Value, model_name: &str, thinking_config: &Res<'_>) {
    let model_info = lookup_model_info(model_name, Some("claude"));
    let levels = model_info
        .as_ref()
        .and_then(|mi| mi.thinking.as_ref())
        .map(|t| t.levels.as_slice())
        .unwrap_or_default();
    let supports_adaptive = !levels.is_empty();
    let supports_max = supports_adaptive && has_level(levels, thinking_level::MAX);

    let level_value = first_existing(thinking_config, "thinkingLevel", "thinking_level");
    if level_value.exists() {
        let mut level = level_value.str().trim().to_lowercase();
        if supports_adaptive {
            match level.as_str() {
                "" => {}
                "none" => {
                    cpa_json::set(out, "thinking.type", "disabled");
                    cpa_json::delete(out, "thinking.budget_tokens");
                    cpa_json::delete(out, "output_config.effort");
                }
                _ => {
                    if let Some(mapped) = map_to_claude_effort(&level, supports_max) {
                        level = mapped.to_string();
                    }
                    cpa_json::set(out, "thinking.type", "adaptive");
                    cpa_json::delete(out, "thinking.budget_tokens");
                    cpa_json::set(out, "output_config.effort", level);
                }
            }
        } else {
            match level.as_str() {
                "" => {}
                "none" => {
                    cpa_json::set(out, "thinking.type", "disabled");
                    cpa_json::delete(out, "thinking.budget_tokens");
                }
                "auto" => {
                    cpa_json::set(out, "thinking.type", "enabled");
                    cpa_json::delete(out, "thinking.budget_tokens");
                }
                _ => {
                    if let Some(budget) = convert_level_to_budget(&level) {
                        cpa_json::set(out, "thinking.type", "enabled");
                        cpa_json::set(out, "thinking.budget_tokens", budget);
                    }
                }
            }
        }
        return;
    }

    let budget_value = first_existing(thinking_config, "thinkingBudget", "thinking_budget");
    if !budget_value.exists() {
        return;
    }
    let budget = budget_value.int();
    if supports_adaptive {
        if budget == 0 {
            cpa_json::set(out, "thinking.type", "disabled");
            cpa_json::delete(out, "thinking.budget_tokens");
            cpa_json::delete(out, "output_config.effort");
        } else if let Some(level) = convert_budget_to_level(budget) {
            let level = map_to_claude_effort(level, supports_max).unwrap_or(level);
            cpa_json::set(out, "thinking.type", "adaptive");
            cpa_json::delete(out, "thinking.budget_tokens");
            cpa_json::set(out, "output_config.effort", level);
        }
    } else {
        match budget {
            0 => {
                cpa_json::set(out, "thinking.type", "disabled");
                cpa_json::delete(out, "thinking.budget_tokens");
            }
            -1 => {
                cpa_json::set(out, "thinking.type", "enabled");
                cpa_json::delete(out, "thinking.budget_tokens");
            }
            _ => {
                cpa_json::set(out, "thinking.type", "enabled");
                cpa_json::set(out, "thinking.budget_tokens", budget);
            }
        }
    }
}

/// One Claude tool from a Gemini function declaration.
fn claude_tool_from_declaration(func_decl: &Res<'_>) -> Value {
    let mut tool = json!({"name": "", "description": "", "input_schema": {"type": "object", "properties": {}}});

    let name = func_decl.g("name");
    if name.exists() {
        cpa_json::set(&mut tool, "name", sanitize_claude_function_name(&name.str()));
    }
    let desc = func_decl.g("description");
    if desc.exists() {
        cpa_json::set(&mut tool, "description", desc.str());
    }
    let params = first_existing(func_decl, "parameters", "parametersJsonSchema");
    if params.exists() {
        cpa_json::set(&mut tool, "input_schema", normalize_claude_tool_schema(&params));
    } else {
        cpa_json::set(&mut tool, "input_schema", json!({"type": "object", "properties": {}}));
    }

    lowercase_claude_tool_schema_types(&mut tool);
    tool
}

/// Forces `additionalProperties:false` and the draft-07 `$schema` onto a copy of `parameters`.
fn normalize_claude_tool_schema(parameters: &Res<'_>) -> Value {
    const SCHEMA: &str = "http://json-schema.org/draft-07/schema#";
    let mut cleaned = parameters.value();
    if parameters.g("additionalProperties").v() != Some(&Value::Bool(false)) {
        cpa_json::set(&mut cleaned, "additionalProperties", false);
    }
    if parameters.g("$schema").as_str() != Some(SCHEMA) {
        cpa_json::set(&mut cleaned, "$schema", SCHEMA);
    }
    cleaned
}

/// Lowercases every `type` value in the tool. Like Go, a non-string value found at a `type` key
/// (e.g. a property named "type") is replaced by the lowercased text of that value.
fn lowercase_claude_tool_schema_types(tool: &mut Value) {
    let mut paths_to_lower = Vec::new();
    walk(tool, "", "type", &mut paths_to_lower);
    for path in paths_to_lower {
        let type_value = tool.g(&path);
        let current = type_value.str();
        let normalized = current.to_lowercase();
        if type_value.is_string() && normalized == current {
            continue;
        }
        drop(type_value);
        cpa_json::set(tool, &path, normalized);
    }
}

fn set_tool_choice_from_gemini_tool_config(out: &mut Value, func_calling: &Res<'_>) {
    if !func_calling.exists() {
        return;
    }
    let mode = func_calling.g("mode");
    if !mode.exists() {
        return;
    }
    match mode.str().as_str() {
        "AUTO" => {
            cpa_json::set(out, "tool_choice", json!({"type": "auto"}));
        }
        "NONE" => {
            cpa_json::set(out, "tool_choice", json!({"type": "none"}));
        }
        "ANY" => {
            let allowed = first_existing(func_calling, "allowedFunctionNames", "allowed_function_names");
            let items = allowed.array();
            if allowed.is_array() && items.len() == 1 {
                let choice = json!({"type": "tool", "name": sanitize_claude_function_name(&items[0].str())});
                cpa_json::set(out, "tool_choice", choice);
            } else {
                cpa_json::set(out, "tool_choice", json!({"type": "any"}));
            }
        }
        _ => {}
    }
}

/// The trimmed `id`, else the trimmed `call_id`, of a Gemini functionCall/functionResponse.
fn gemini_tool_id(value: &Res<'_>) -> String {
    let id = value.g("id").str();
    if !id.trim().is_empty() {
        return id.trim().to_string();
    }
    value.g("call_id").str().trim().to_string()
}

/// `root.first` when it exists, else `root.second`.
fn first_existing<'a>(root: &'a Res<'_>, first: &str, second: &str) -> Res<'a> {
    let value = root.g(first);
    if value.exists() {
        return value;
    }
    root.g(second)
}

fn content_part_from_inline_data(inline_data: &Res<'_>) -> Option<Value> {
    let mut mime_type = inline_data.g("mimeType").str();
    if mime_type.is_empty() {
        mime_type = inline_data.g("mime_type").str();
    }
    let data = inline_data.g("data").str();
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    let lower = mime_type.to_lowercase();
    if lower.starts_with("image/") {
        Some(json!({"type": "image", "source": {"type": "base64", "media_type": mime_type, "data": data}}))
    } else if lower.starts_with("application/") || lower.starts_with("text/") {
        Some(json!({"type": "document", "source": {"type": "base64", "media_type": mime_type, "data": data}}))
    } else {
        Some(claude_text_content_part(&format!("Media content: inline data (Type: {mime_type})")))
    }
}

fn content_part_from_file_data(file_data: &Res<'_>) -> Option<Value> {
    let mut file_uri = file_data.g("fileUri").str();
    if file_uri.is_empty() {
        file_uri = file_data.g("file_uri").str();
    }
    if file_uri.is_empty() {
        return None;
    }
    let mut mime_type = file_data.g("mimeType").str();
    if mime_type.is_empty() {
        mime_type = file_data.g("mime_type").str();
    }
    let lower = mime_type.to_lowercase();
    if lower.starts_with("image/") {
        Some(json!({"type": "image", "source": {"type": "url", "url": file_uri}}))
    } else if lower.starts_with("application/") || lower.starts_with("text/") {
        let mut document = json!({"type": "document", "source": {"type": "url", "url": file_uri}});
        if !mime_type.is_empty() {
            cpa_json::set(&mut document, "source.media_type", mime_type);
        }
        Some(document)
    } else {
        let mut file_info = format!("File: {file_uri}");
        if !mime_type.is_empty() {
            file_info.push_str(&format!(" (Type: {mime_type})"));
        }
        Some(claude_text_content_part(&file_info))
    }
}

fn claude_text_content_part(text: &str) -> Value {
    json!({"type": "text", "text": text})
}
