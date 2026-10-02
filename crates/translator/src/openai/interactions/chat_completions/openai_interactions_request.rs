//! Client OpenAI chat request -> upstream Interactions request
//! (Go: openai_interactions_request.go).

use std::collections::HashMap;

use cpa_json::{Res, Value, J};

use super::{copy_number, first_existing, first_non_blank, is_antigravity_model, openai_reasoning_texts, set_items};
use crate::common;

pub(super) fn convert_openai_request_to_interactions(model_name: &str, input_raw_json: &[u8], stream: bool) -> Vec<u8> {
    let root = cpa_json::parse(input_raw_json);
    let mut out = cpa_json::parse_str(r#"{"model":"","input":[]}"#);
    let model = first_non_blank(&[model_name, &root.g("model").str()]);
    cpa_json::set(&mut out, "model", model.as_str());
    if let Some(stream_value) = request_stream_value(&root, stream) {
        cpa_json::set(&mut out, "stream", stream_value);
    }
    let previous = first_non_blank(&[&root.g("previous_response_id").str(), &root.g("previous_interaction_id").str()]);
    if !previous.is_empty() {
        cpa_json::set(&mut out, "previous_interaction_id", previous);
    }
    let environment_id = first_non_blank(&[&root.g("environment_id").str(), &root.g("environment.id").str()]);
    if !environment_id.is_empty() {
        cpa_json::set(&mut out, "environment_id", environment_id);
    }
    let agent_config = root.g("agent_config");
    if agent_config.exists() {
        cpa_json::set(&mut out, "agent_config", agent_config.value());
    }
    let for_antigravity = is_antigravity_model(&model);
    append_messages(&mut out, &root.g("messages"), for_antigravity);
    copy_generation_config(&mut out, &root, &model);
    append_tools(&mut out, &root.g("tools"), for_antigravity);
    cpa_json::to_vec(&out)
}

/// The body's `stream` when present, else `true` when the caller requested streaming.
fn request_stream_value(root: &Value, stream: bool) -> Option<bool> {
    let value = root.g("stream");
    if value.exists() {
        return Some(value.bool());
    }
    stream.then_some(true)
}

fn append_messages(out: &mut Value, messages: &Res<'_>, for_antigravity: bool) {
    if !messages.is_array() {
        return;
    }
    let mut input_items: Vec<Value> = Vec::new();
    let mut system = String::new();
    let mut tool_names_by_id: HashMap<String, String> = HashMap::new();
    for message in messages.array() {
        let role = message.g("role").str().trim().to_lowercase();
        match role.as_str() {
            "system" | "developer" => {
                let text = chat_content_text(&message.g("content"));
                if !text.is_empty() {
                    if !system.is_empty() {
                        system.push('\n');
                    }
                    system.push_str(&text);
                }
            }
            _ => append_message(&mut input_items, &message, for_antigravity, &mut tool_names_by_id),
        }
    }
    if !system.is_empty() {
        cpa_json::set(out, "system_instruction", system);
    }
    set_items(out, "input", input_items);
}

fn append_message(items: &mut Vec<Value>, message: &Res<'_>, for_antigravity: bool, tool_names_by_id: &mut HashMap<String, String>) {
    let role = message.g("role").str().trim().to_lowercase();
    match role.as_str() {
        "assistant" => {
            let reasoning = message.g("reasoning_content");
            if reasoning.exists() {
                for text in openai_reasoning_texts(&reasoning) {
                    items.push(super::interactions_text_step("thought", &text));
                }
            }
            if let Some(step) = chat_content_step("model_output", &message.g("content")) {
                items.push(step);
            }
            let tool_calls = message.g("tool_calls");
            if tool_calls.is_array() {
                for tool_call in tool_calls.array() {
                    let id = tool_call.g("id").str();
                    if !id.is_empty() {
                        let name = tool_call.g("function.name").str();
                        if !name.is_empty() {
                            tool_names_by_id.insert(id, name);
                        }
                    }
                    if let Some(step) = super::interactions_openai_response::tool_call_to_interactions_step(&tool_call, for_antigravity) {
                        items.push(step);
                    }
                }
            }
        }
        "tool" | "function" => items.push(tool_result_to_interactions(message, for_antigravity, tool_names_by_id)),
        _ => {
            if let Some(step) = chat_content_step("user_input", &message.g("content")) {
                items.push(step);
            }
        }
    }
}

/// A `{"type":<step_type>,"content":[...]}` step from OpenAI message content, `None` when the
/// content yields no parts.
fn chat_content_step(step_type: &str, content: &Res<'_>) -> Option<Value> {
    let mut content_items: Vec<Value> = Vec::new();
    if let Some(text) = content.as_str() {
        if text.is_empty() {
            return None;
        }
        let mut part = cpa_json::parse_str(r#"{"type":"text","text":""}"#);
        cpa_json::set(&mut part, "text", text);
        content_items.push(part);
    } else if content.is_array() {
        content_items.extend(content.array().iter().filter_map(chat_content_part_to_interactions));
    } else if content.is_object() {
        content_items.extend(chat_content_part_to_interactions(content));
    }
    if content_items.is_empty() {
        return None;
    }
    let mut step = cpa_json::parse_str(r#"{"type":"","content":[]}"#);
    cpa_json::set(&mut step, "type", step_type);
    cpa_json::set(&mut step, "content", Value::Array(content_items));
    Some(step)
}

fn chat_content_part_to_interactions(part: &Res<'_>) -> Option<Value> {
    let mut part_type = part.g("type").str().trim().to_lowercase();
    if part_type.is_empty() && part.g("text").exists() {
        part_type = "text".to_string();
    }
    match part_type.as_str() {
        "text" | "input_text" | "output_text" => {
            let mut out = cpa_json::parse_str(r#"{"type":"text","text":""}"#);
            cpa_json::set(&mut out, "text", part.g("text").str());
            Some(out)
        }
        "image_url" | "input_image" | "image" => Some(chat_image_part_to_interactions(part)),
        "input_audio" | "audio" => {
            let mut out = cpa_json::parse_str(r#"{"type":"audio","data":""}"#);
            let audio = part.g("input_audio");
            let data = first_non_blank(&[&audio.g("data").str(), &part.g("data").str()]);
            if data.is_empty() {
                return None;
            }
            cpa_json::set(&mut out, "data", data);
            let format = first_non_blank(&[&audio.g("format").str(), &part.g("format").str()]);
            if !format.is_empty() {
                cpa_json::set(&mut out, "mime_type", input_audio_mime_type(&format));
            }
            Some(out)
        }
        "file" | "input_file" | "document" => {
            let file = part.g("file");
            let filename = first_non_blank(&[&file.g("filename").str(), &part.g("filename").str()]);
            let fallback_mime = first_non_blank(&[
                &file.g("mime_type").str(),
                &file.g("mimeType").str(),
                &part.g("mime_type").str(),
                &part.g("mimeType").str(),
            ]);
            let file_data = first_non_blank(&[&file.g("file_data").str(), &part.g("file_data").str(), &part.g("data").str()]);
            let file_url = first_non_blank(&[&file.g("file_url").str(), &part.g("file_url").str(), &part.g("url").str()]);
            let mut out = cpa_json::parse_str(r#"{"type":"document"}"#);
            if !filename.is_empty() {
                cpa_json::set(&mut out, "filename", filename.as_str());
            }
            let mut has_content = false;
            if let Some((mime_type, data)) = common::normalize_openai_file_data(&filename, &fallback_mime, &file_data) {
                cpa_json::set(&mut out, "mime_type", mime_type);
                cpa_json::set(&mut out, "data", data);
                has_content = true;
            }
            if !file_url.is_empty() {
                cpa_json::set(&mut out, "file_url", file_url);
                has_content = true;
            }
            has_content.then_some(out)
        }
        _ => None,
    }
}

fn chat_image_part_to_interactions(part: &Res<'_>) -> Value {
    let mut out = cpa_json::parse_str(r#"{"type":"image"}"#);
    let image_url = first_non_blank(&[&part.g("image_url.url").str(), &part.g("image_url").str(), &part.g("url").str()]);
    if let Some((mime_type, data)) = parse_data_url(&image_url) {
        cpa_json::set(&mut out, "mime_type", mime_type);
        cpa_json::set(&mut out, "data", data);
        return out;
    }
    let data = part.g("data").str();
    if !data.is_empty() {
        cpa_json::set(&mut out, "data", data);
        let mime_type = part.g("mime_type").str();
        if !mime_type.is_empty() {
            cpa_json::set(&mut out, "mime_type", mime_type);
        }
        return out;
    }
    if !image_url.is_empty() {
        cpa_json::set(&mut out, "image_url", image_url);
    }
    out
}

fn tool_result_to_interactions(message: &Res<'_>, for_antigravity: bool, tool_names_by_id: &HashMap<String, String>) -> Value {
    let mut out = cpa_json::parse_str(r#"{"type":"function_result","result":""}"#);
    let call_id = first_non_blank(&[&message.g("tool_call_id").str(), &message.g("id").str()]);
    if !call_id.is_empty() {
        cpa_json::set(&mut out, "call_id", call_id.as_str());
    }
    let mut name = message.g("name").str();
    if name.is_empty() && !call_id.is_empty() {
        name = tool_names_by_id.get(&call_id).cloned().unwrap_or_default();
    }
    if !name.is_empty() {
        if for_antigravity {
            name = common::antigravity_tool_name_to_upstream(&name);
        }
        cpa_json::set(&mut out, "name", name);
    }
    let content = message.g("content");
    if content.exists() {
        cpa_json::set(&mut out, "result", content.value());
    }
    out
}

fn copy_generation_config(out: &mut Value, root: &Value, model: &str) {
    if is_antigravity_model(model) {
        let max_output_tokens = first_existing([root.g("max_completion_tokens"), root.g("max_tokens"), root.g("max_output_tokens")]);
        if max_output_tokens.exists() && !root.g("agent_config.max_total_tokens").exists() {
            cpa_json::set(out, "agent_config.max_total_tokens", max_output_tokens.int());
        }
    } else {
        copy_number(out, "generation_config.max_output_tokens", &first_existing([root.g("max_completion_tokens"), root.g("max_tokens")]));
        copy_number(out, "generation_config.temperature", &root.g("temperature"));
        copy_number(out, "generation_config.top_p", &root.g("top_p"));
        copy_number(out, "generation_config.presence_penalty", &root.g("presence_penalty"));
        copy_number(out, "generation_config.frequency_penalty", &root.g("frequency_penalty"));
        copy_number(out, "generation_config.candidate_count", &root.g("n"));
        let stop = root.g("stop");
        if stop.exists() {
            cpa_json::set(out, "generation_config.stop_sequences", stop.value());
        }
    }
    let tool_choice = root.g("tool_choice");
    if tool_choice.exists() {
        if is_antigravity_model(model) && tool_choice.is_object() {
            let mut tc = tool_choice.value();
            let fn_name = tool_choice.g("function.name").str();
            if !fn_name.is_empty() {
                cpa_json::set(&mut tc, "function.name", common::antigravity_tool_name_to_upstream(&fn_name));
            } else {
                let name = tool_choice.g("name").str();
                if !name.is_empty() {
                    cpa_json::set(&mut tc, "name", common::antigravity_tool_name_to_upstream(&name));
                }
            }
            cpa_json::set(out, "generation_config.tool_choice", tc);
        } else {
            cpa_json::set(out, "generation_config.tool_choice", tool_choice.value());
        }
    }
    let effort = root.g("reasoning_effort");
    if effort.is_string() {
        cpa_json::set(out, "generation_config.thinking_level", effort.str().trim().to_lowercase());
    }
    let response_format = root.g("response_format");
    if response_format.exists() {
        cpa_json::set(out, "response_format", response_format.value());
    }
    let modalities = root.g("modalities");
    if modalities.exists() {
        cpa_json::set(out, "response_modalities", modalities.value());
    }
    let service_tier = root.g("service_tier");
    if service_tier.is_string() {
        cpa_json::set(out, "service_tier", service_tier.str());
    }
}

fn append_tools(out: &mut Value, tools: &Res<'_>, for_antigravity: bool) {
    if !tools.is_array() {
        return;
    }
    let tool_items: Vec<Value> = tools.array().iter().filter_map(|tool| tool_to_interactions(tool, for_antigravity)).collect();
    if !tool_items.is_empty() {
        cpa_json::set(out, "tools", Value::Array(tool_items));
    }
}

fn tool_to_interactions(tool: &Res<'_>, for_antigravity: bool) -> Option<Value> {
    let tool_type = tool.g("type").str().trim().to_lowercase();
    if !tool_type.is_empty() && tool_type != "function" {
        return None;
    }
    let mut name = first_non_blank(&[&tool.g("function.name").str(), &tool.g("name").str()]);
    if name.is_empty() {
        return None;
    }
    if for_antigravity {
        name = common::antigravity_tool_name_to_upstream(&name);
    }
    let mut out = cpa_json::parse_str(r#"{"type":"function","name":""}"#);
    cpa_json::set(&mut out, "name", name);
    let desc = first_existing([tool.g("function.description"), tool.g("description")]);
    if desc.exists() {
        cpa_json::set(&mut out, "description", desc.str());
    }
    let parameters = first_existing([tool.g("function.parameters"), tool.g("parameters")]);
    if parameters.exists() {
        cpa_json::set(&mut out, "parameters", parameters.value());
    }
    Some(out)
}

/// Text of OpenAI message content: a string, an object's `text`, or all array part texts joined.
fn chat_content_text(content: &Res<'_>) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if content.is_object() {
        return content.g("text").str();
    }
    if !content.is_array() {
        return String::new();
    }
    content.array().iter().map(|part| part.g("text").str()).collect()
}

fn input_audio_mime_type(format: &str) -> &'static str {
    match format.trim().to_lowercase().as_str() {
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "opus" => "audio/opus",
        "pcm16" => "audio/pcm",
        _ => "audio/mpeg",
    }
}

/// `(mime, base64 data)` of a `data:<mime>;base64,<data>` URL.
fn parse_data_url(value: &str) -> Option<(String, String)> {
    let rest = value.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let (mime_type, encoding) = meta.split_once(';').unwrap_or((meta, ""));
    if !encoding.eq_ignore_ascii_case("base64") || mime_type.trim().is_empty() || data.is_empty() {
        return None;
    }
    Some((mime_type.to_string(), data.to_string()))
}
