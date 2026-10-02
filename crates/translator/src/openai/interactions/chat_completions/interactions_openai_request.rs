//! Client Interactions request -> upstream OpenAI chat request
//! (Go: interactions_openai_request.go).

use crate::common::input_audio_format_from_mime;
use cpa_json::{Res, Value, J};

use super::{copy_number, first_existing, first_non_blank, is_antigravity_model, json_string_value, set_items};
use crate::common;
use crate::openai::interactions::responses::raw_text::restore_input_raw_text;

pub(super) fn convert_interactions_request_to_openai(model_name: &str, input_raw_json: &[u8], stream: bool) -> Vec<u8> {
    let mut root = cpa_json::parse(input_raw_json);
    restore_input_raw_text(input_raw_json, &mut root);
    let mut out = cpa_json::parse_str(r#"{"model":"","messages":[]}"#);
    let model = first_non_blank(&[model_name, &root.g("model").str()]);
    cpa_json::set(&mut out, "model", model.as_str());
    if stream || root.g("stream").bool() {
        cpa_json::set(&mut out, "stream", true);
    }
    let mut message_items: Vec<Value> = Vec::new();
    append_system(&mut message_items, &root);
    let for_antigravity = is_antigravity_model(&model);
    append_input_messages(&mut message_items, &root.g("input"), for_antigravity);
    set_items(&mut out, "messages", message_items);
    copy_tools(&mut out, &root, for_antigravity);
    copy_generation_config(&mut out, &root);
    copy_top_level(&mut out, &root);
    cpa_json::to_vec(&out)
}

fn append_system(items: &mut Vec<Value>, root: &Value) {
    let text = interactions_text(&root.g("system_instruction"));
    if text.is_empty() {
        return;
    }
    let mut msg = cpa_json::parse_str(r#"{"role":"system","content":""}"#);
    cpa_json::set(&mut msg, "content", text);
    items.push(msg);
}

fn append_input_messages(items: &mut Vec<Value>, input: &Res<'_>, for_antigravity: bool) {
    if let Some(text) = input.as_str() {
        let mut msg = cpa_json::parse_str(r#"{"role":"user","content":""}"#);
        cpa_json::set(&mut msg, "content", text);
        items.push(msg);
    } else if input.is_array() {
        for step in input.array() {
            append_step(items, &step, "user", for_antigravity);
        }
    } else if input.is_object() {
        append_step(items, input, "user", for_antigravity);
    }
}

fn append_step(items: &mut Vec<Value>, step: &Res<'_>, default_role: &str, for_antigravity: bool) {
    match step.g("type").str().as_str() {
        "user_input" => append_message(items, step, "user"),
        "model_output" => append_message(items, step, "assistant"),
        "thought" => {
            let mut msg = cpa_json::parse_str(r#"{"role":"assistant","content":"","reasoning_content":""}"#);
            cpa_json::set(&mut msg, "reasoning_content", interactions_text(&step.g("content")));
            items.push(msg);
        }
        "function_call" => append_function_call(items, step, for_antigravity),
        "function_result" => {
            let mut msg = cpa_json::parse_str(r#"{"role":"tool","tool_call_id":"","content":""}"#);
            cpa_json::set(&mut msg, "tool_call_id", first_non_blank(&[&step.g("call_id").str(), &step.g("id").str()]));
            let result = first_existing([step.g("result"), step.g("output")]);
            cpa_json::set(&mut msg, "content", json_string_value(&result, ""));
            items.push(msg);
        }
        _ => {
            if let Some(text) = step.as_str() {
                let mut msg = cpa_json::parse_str(r#"{"role":"","content":""}"#);
                cpa_json::set(&mut msg, "role", default_role);
                cpa_json::set(&mut msg, "content", text);
                items.push(msg);
            }
        }
    }
}

fn append_message(items: &mut Vec<Value>, step: &Res<'_>, role: &str) {
    let mut msg = cpa_json::parse_str(r#"{"role":"","content":""}"#);
    cpa_json::set(&mut msg, "role", role);
    let content = step.g("content");
    match content.as_str() {
        Some(text) => {
            cpa_json::set(&mut msg, "content", text);
        }
        None => append_content(&mut msg, &content),
    }
    items.push(msg);
}

/// Converts Interactions content parts into the message `content`: a plain string when every part
/// is text, else an array of OpenAI content parts. Unconvertible parts are dropped.
fn append_content(msg: &mut Value, content: &Res<'_>) {
    if !content.exists() {
        return;
    }
    let parts: Vec<Value> = if content.is_array() {
        content.array().iter().filter_map(content_part_to_openai).collect()
    } else if content.is_object() {
        content_part_to_openai(content).into_iter().collect()
    } else {
        vec![]
    };
    if parts.is_empty() {
        return;
    }
    let is_text = |part: &Value| part.g("type").str() == "text";
    if parts.iter().all(is_text) {
        let text: String = parts.iter().map(|p| p.g("text").str()).collect();
        cpa_json::set(msg, "content", text);
    } else {
        cpa_json::set(msg, "content", Value::Array(parts));
    }
}

fn append_function_call(items: &mut Vec<Value>, step: &Res<'_>, for_antigravity: bool) {
    let mut msg = cpa_json::parse_str(r#"{"role":"assistant","content":"","tool_calls":[]}"#);
    let tool_call = super::openai_interactions_response::tool_call_from_interactions(step, &Res::NONE, for_antigravity);
    cpa_json::set(&mut msg, "tool_calls", Value::Array(vec![tool_call]));
    items.push(msg);
}

fn copy_tools(out: &mut Value, root: &Value, for_antigravity: bool) {
    let tools = root.g("tools");
    if !tools.is_array() {
        return;
    }
    let mut tool_items: Vec<Value> = Vec::new();
    for tool in tools.array() {
        tool_items.extend(tool_from_interactions_tool(&tool, for_antigravity));
        let decls = first_existing([tool.g("function_declarations"), tool.g("functionDeclarations")]);
        if decls.is_array() {
            tool_items.extend(decls.array().iter().filter_map(|decl| tool_from_interactions_tool(decl, for_antigravity)));
        }
    }
    if !tool_items.is_empty() {
        cpa_json::set(out, "tools", Value::Array(tool_items));
    }
}

fn copy_generation_config(out: &mut Value, root: &Value) {
    let mut gcfg = root.g("generation_config");
    if !gcfg.exists() {
        gcfg = root.g("generationConfig");
    }
    copy_number(out, "temperature", &first_existing([gcfg.g("temperature"), root.g("temperature")]));
    copy_number(
        out,
        "max_tokens",
        &first_existing([gcfg.g("max_output_tokens"), gcfg.g("maxOutputTokens"), root.g("max_tokens"), root.g("max_completion_tokens")]),
    );
    copy_number(out, "top_p", &first_existing([gcfg.g("top_p"), gcfg.g("topP"), root.g("top_p")]));
    copy_number(out, "top_k", &first_existing([gcfg.g("top_k"), gcfg.g("topK")]));
    copy_number(out, "n", &first_existing([gcfg.g("candidate_count"), gcfg.g("candidateCount"), root.g("n")]));
    let stop = first_existing([gcfg.g("stop_sequences"), gcfg.g("stopSequences"), root.g("stop")]);
    if stop.exists() {
        cpa_json::set(out, "stop", stop.value());
    }
    let tool_choice = first_existing([gcfg.g("tool_choice"), root.g("tool_choice")]);
    if tool_choice.exists() {
        cpa_json::set(out, "tool_choice", tool_choice.value());
    }
    let effort = reasoning_effort(root, &gcfg);
    if !effort.is_empty() {
        cpa_json::set(out, "reasoning_effort", effort);
    }
    let modalities = root.g("response_modalities");
    if modalities.exists() {
        cpa_json::set(out, "modalities", modalities.value());
    }
}

fn copy_top_level(out: &mut Value, root: &Value) {
    let format = root.g("response_format");
    if format.exists() {
        cpa_json::set(out, "response_format", format.value());
    }
    let service_tier = root.g("service_tier");
    if service_tier.is_string() {
        cpa_json::set(out, "service_tier", service_tier.str());
    }
    let previous = first_non_blank(&[&root.g("previous_interaction_id").str(), &root.g("previous_response_id").str()]);
    if !previous.is_empty() {
        cpa_json::set(out, "previous_response_id", previous);
    }
    let environment_id = first_non_blank(&[&root.g("environment_id").str(), &root.g("environment.id").str()]);
    if !environment_id.is_empty() {
        cpa_json::set(out, "environment_id", environment_id);
    }
    let agent_config = root.g("agent_config");
    if agent_config.exists() {
        cpa_json::set(out, "agent_config", agent_config.value());
    }
    for key in ["parallel_tool_calls", "seed", "user"] {
        let value = root.g(key);
        if value.exists() {
            cpa_json::set(out, key, value.value());
        }
    }
}

fn content_part_to_openai(part: &Res<'_>) -> Option<Value> {
    let mut part_type = part.g("type").str();
    if part_type.is_empty() && part.g("text").exists() {
        part_type = "text".to_string();
    }
    match part_type.as_str() {
        "text" => {
            let mut out = cpa_json::parse_str(r#"{"type":"text","text":""}"#);
            cpa_json::set(&mut out, "text", part.g("text").str());
            Some(out)
        }
        "image" => {
            let mut out = cpa_json::parse_str(r#"{"type":"image_url","image_url":{"url":""}}"#);
            cpa_json::set(&mut out, "image_url.url", media_data_url(part, "application/octet-stream"));
            Some(out)
        }
        "audio" => {
            let mut out = cpa_json::parse_str(r#"{"type":"input_audio","input_audio":{"data":"","format":""}}"#);
            cpa_json::set(&mut out, "input_audio.data", part.g("data").str());
            cpa_json::set(&mut out, "input_audio.format", input_audio_format_from_mime(&part.g("mime_type").str()));
            Some(out)
        }
        "video" => {
            let mut out = cpa_json::parse_str(r#"{"type":"video_url","video_url":{"url":""}}"#);
            cpa_json::set(&mut out, "video_url.url", media_data_url(part, "video/mp4"));
            Some(out)
        }
        "document" | "file" => {
            let mut out = cpa_json::parse_str(r#"{"type":"file","file":{"filename":"","file_data":""}}"#);
            let filename = first_non_blank(&[&part.g("filename").str(), &file_name_from_mime(&part.g("mime_type").str())]);
            cpa_json::set(&mut out, "file.filename", filename);
            cpa_json::set(&mut out, "file.file_data", part.g("data").str());
            let url = first_non_blank(&[&part.g("file_url").str(), &part.g("url").str()]);
            if !url.is_empty() {
                cpa_json::delete(&mut out, "file.file_data");
                cpa_json::set(&mut out, "file.file_url", url);
            }
            Some(out)
        }
        _ => None,
    }
}

fn tool_from_interactions_tool(tool: &Res<'_>, for_antigravity: bool) -> Option<Value> {
    let mut name = first_non_blank(&[&tool.g("name").str(), &tool.g("function.name").str()]);
    if name.is_empty() {
        return None;
    }
    if for_antigravity {
        name = common::antigravity_upstream_tool_name_to_client(&name);
    }
    let mut out = cpa_json::parse_str(r#"{"type":"function","function":{"name":""}}"#);
    cpa_json::set(&mut out, "function.name", name);
    let desc = first_existing([tool.g("description"), tool.g("function.description")]);
    if desc.exists() {
        cpa_json::set(&mut out, "function.description", desc.str());
    }
    let params = first_existing([tool.g("parameters"), tool.g("function.parameters"), tool.g("parametersJsonSchema")]);
    if params.exists() {
        cpa_json::set(&mut out, "function.parameters", params.value());
    }
    Some(out)
}

/// Text of an Interactions text-bearing value: a string, `text`, or the joined part texts of
/// `content` / `parts`.
fn interactions_text(value: &Res<'_>) -> String {
    if !value.exists() {
        return String::new();
    }
    if let Some(s) = value.as_str() {
        return s.to_string();
    }
    let text = value.g("text");
    if text.exists() {
        return text.str();
    }
    for path in ["content", "parts"] {
        let parts = value.g(path);
        if !parts.is_array() {
            continue;
        }
        return parts
            .array()
            .iter()
            .map(|part| first_non_blank(&[&part.g("text").str(), &part.g("content.text").str()]))
            .collect();
    }
    String::new()
}

/// First string-typed effort among the known spellings, trimmed and lowercased.
fn reasoning_effort(root: &Value, gcfg: &Res<'_>) -> String {
    [
        gcfg.g("reasoning_effort"),
        gcfg.g("thinking_level"),
        gcfg.g("thinkingLevel"),
        gcfg.g("thinking_config.thinking_level"),
        gcfg.g("thinkingConfig.thinkingLevel"),
        root.g("reasoning_effort"),
    ]
    .iter()
    .find(|v| v.is_string())
    .map(|v| v.str().trim().to_lowercase())
    .unwrap_or_default()
}

fn media_data_url(part: &Res<'_>, fallback_mime_type: &str) -> String {
    let url = first_non_blank(&[&part.g("image_url").str(), &part.g("file_data").str(), &part.g("url").str()]);
    if !url.is_empty() {
        return url;
    }
    let data = part.g("data").str();
    if data.is_empty() {
        return String::new();
    }
    let mime_type = first_non_blank(&[&part.g("mime_type").str(), fallback_mime_type]);
    format!("data:{mime_type};base64,{data}")
}

fn file_name_from_mime(mime_type: &str) -> String {
    match mime_type.trim().to_lowercase().as_str() {
        "application/pdf" => "document.pdf".to_string(),
        "text/plain" => "document.txt".to_string(),
        "text/csv" => "document.csv".to_string(),
        "application/json" => "document.json".to_string(),
        _ => match mime_type.split_once('/') {
            Some((_, suffix)) if !suffix.is_empty() => format!("document.{}", suffix.replace('+', ".")),
            _ => "document.bin".to_string(),
        },
    }
}
