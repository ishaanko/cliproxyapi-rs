//! Request translation for both directions (Go: interactions_openai_responses_request.go).

use std::collections::{HashMap, HashSet};

use cpa_core::applypatch;
use cpa_core::util;
use cpa_json::{Res, Value, J};

use super::{first_existing, first_non_blank, is_antigravity_model, json_string_value, set_items, set_json_value};
use super::raw_text::restore_input_raw_text;
use crate::common;

/// Go: `isDevinModel`.
pub(super) fn is_devin_model(model: &str) -> bool {
    model.trim().to_lowercase().contains("devin")
}

fn request_model(model_name: &str, root: &Value) -> String {
    if !model_name.trim().is_empty() {
        return model_name.to_string();
    }
    root.g("model").str()
}

/// The body's `stream` when present, else `true` when the caller requested streaming.
fn request_stream_value(root: &Value, stream: bool) -> Option<bool> {
    let value = root.g("stream");
    if value.exists() {
        return Some(value.bool());
    }
    stream.then_some(true)
}

fn joined_part_texts(parts: &Res<'_>) -> String {
    parts.array().iter().map(|part| part.g("text").str()).collect()
}

fn responses_instructions_text(instructions: &Res<'_>) -> String {
    if let Some(s) = instructions.as_str() {
        return s.to_string();
    }
    let text = instructions.g("text");
    if text.exists() {
        return text.str();
    }
    let parts = instructions.g("content");
    if parts.is_array() {
        return joined_part_texts(&parts);
    }
    instructions.str()
}

fn interactions_system_instruction_text(root: &Value) -> String {
    let sys = root.g("system_instruction");
    if !sys.exists() {
        return String::new();
    }
    if let Some(s) = sys.as_str() {
        return s.to_string();
    }
    let text = sys.g("text");
    if text.exists() {
        return text.str();
    }
    let parts = sys.g("parts");
    if parts.is_array() {
        return joined_part_texts(&parts);
    }
    String::new()
}

fn interactions_thinking_effort(root: &Value) -> String {
    [
        "generation_config.thinking_level",
        "generation_config.thinkingConfig.thinkingLevel",
        "generation_config.thinkingConfig.thinking_level",
        "generation_config.thinking_config.thinking_level",
    ]
    .iter()
    .map(|path| root.g(path))
    .find(Res::is_string)
    .map(|level| level.str().trim().to_lowercase())
    .unwrap_or_default()
}

/// Client OpenAI Responses request -> upstream Interactions request.
pub(super) fn convert_openai_responses_request_to_interactions(model_name: &str, input_raw_json: &[u8], stream: bool) -> Vec<u8> {
    let root = cpa_json::parse(input_raw_json);
    let mut out = cpa_json::parse_str(r#"{"model":"","input":[]}"#);
    let model = request_model(model_name, &root);
    cpa_json::set(&mut out, "model", model.as_str());
    if let Some(stream_value) = request_stream_value(&root, stream) {
        cpa_json::set(&mut out, "stream", stream_value);
    }
    let instructions = root.g("instructions");
    if instructions.exists() {
        cpa_json::set(&mut out, "system_instruction", responses_instructions_text(&instructions));
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
    let for_devin = is_devin_model(&model) || !for_antigravity;
    let input = root.g("input");
    if input.exists() {
        set_responses_input_on_interactions(&mut out, &input, for_antigravity);
    }
    append_responses_tools_to_interactions(&mut out, &root, for_antigravity, for_devin);
    let tool_choice = root.g("tool_choice");
    if tool_choice.exists() {
        if tool_choice.is_object() {
            copy_object_tool_choice(&mut out, &tool_choice, for_antigravity, for_devin);
        } else {
            cpa_json::set(&mut out, "generation_config.tool_choice", tool_choice.value());
        }
    }
    let effort = root.g("reasoning.effort");
    if effort.is_string() {
        cpa_json::set(&mut out, "generation_config.thinking_level", effort.str().trim().to_lowercase());
    }
    let summary = root.g("reasoning.summary");
    if summary.is_string() {
        cpa_json::set(&mut out, "generation_config.thinking_summaries", summary.str());
    }
    let format = first_existing([root.g("response_format"), root.g("text.format")]);
    if format.exists() {
        cpa_json::set(&mut out, "response_format", format.value());
    }
    let max_output_tokens = first_existing([root.g("max_output_tokens"), root.g("max_tokens"), root.g("max_completion_tokens")]);
    if is_antigravity_model(&model) {
        if max_output_tokens.exists() && !root.g("agent_config.max_total_tokens").exists() {
            cpa_json::set(&mut out, "agent_config.max_total_tokens", max_output_tokens.int());
        }
        for knob in [
            "temperature",
            "top_p",
            "top_k",
            "stop_sequences",
            "max_output_tokens",
            "presence_penalty",
            "frequency_penalty",
            "candidate_count",
        ] {
            cpa_json::delete(&mut out, &format!("generation_config.{knob}"));
        }
    } else {
        if max_output_tokens.exists() {
            cpa_json::set(&mut out, "generation_config.max_output_tokens", max_output_tokens.int());
        }
        for (key, path) in [
            ("temperature", "generation_config.temperature"),
            ("top_p", "generation_config.top_p"),
            ("presence_penalty", "generation_config.presence_penalty"),
            ("frequency_penalty", "generation_config.frequency_penalty"),
        ] {
            let value = root.g(key);
            if value.exists() {
                cpa_json::set(&mut out, path, cpa_json::num_f64(value.float()));
            }
        }
        let stop = root.g("stop");
        if stop.exists() {
            cpa_json::set(&mut out, "generation_config.stop_sequences", stop.value());
        }
    }
    cpa_json::to_vec(&out)
}

/// Copies an object `tool_choice` to `generation_config.tool_choice`, qualifying the tool name with
/// its namespace and applying the Antigravity rename; Devin automation-update tools are dropped.
fn copy_object_tool_choice(out: &mut Value, tool_choice: &Res<'_>, for_antigravity: bool, for_devin: bool) {
    let mut tc = Some(tool_choice.value());
    let mut fn_name =
        first_non_blank(&[&tool_choice.g("function.name").str(), &tool_choice.g("name").str(), &tool_choice.g("custom.name").str()]);
    let ns = first_non_blank(&[
        &tool_choice.g("namespace").str(),
        &tool_choice.g("function.namespace").str(),
        &tool_choice.g("custom.namespace").str(),
    ]);
    if !ns.is_empty() && !fn_name.is_empty() {
        fn_name = util::qualify_responses_namespace_tool_name(&ns, &fn_name);
    }
    if for_devin
        && (common::is_devin_codex_app_automation_update(&ns, &fn_name) || common::is_devin_codex_app_automation_update("", &fn_name))
    {
        tc = None;
    }
    if for_antigravity && !fn_name.is_empty() {
        fn_name = common::antigravity_tool_name_to_upstream(&fn_name);
    }
    let Some(mut tc) = tc else { return };
    if !fn_name.is_empty() {
        if tool_choice.g("function.name").exists() {
            cpa_json::set(&mut tc, "function.name", fn_name);
        } else if tool_choice.g("name").exists() {
            cpa_json::set(&mut tc, "name", fn_name);
        } else if tool_choice.g("custom.name").exists() {
            cpa_json::set(&mut tc, "custom.name", fn_name);
        }
    }
    cpa_json::set(out, "generation_config.tool_choice", tc);
}

/// Client Interactions request -> upstream OpenAI Responses request.
pub(super) fn convert_interactions_request_to_openai_responses(model_name: &str, input_raw_json: &[u8], stream: bool) -> Vec<u8> {
    let mut root = cpa_json::parse(input_raw_json);
    restore_input_raw_text(input_raw_json, &mut root);
    let mut out = cpa_json::parse_str(r#"{"model":"","input":[]}"#);
    let model = request_model(model_name, &root);
    cpa_json::set(&mut out, "model", model.as_str());
    if stream || root.g("stream").bool() {
        cpa_json::set(&mut out, "stream", true);
    }
    let instructions = interactions_system_instruction_text(&root);
    if !instructions.is_empty() {
        cpa_json::set(&mut out, "instructions", instructions);
    }
    let previous = first_non_blank(&[&root.g("previous_interaction_id").str(), &root.g("previous_response_id").str()]);
    if !previous.is_empty() {
        cpa_json::set(&mut out, "previous_response_id", previous);
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
    let input = root.g("input");
    if input.exists() {
        set_interactions_input_on_responses(&mut out, &input, for_antigravity);
    }
    append_interactions_tools_to_responses(&mut out, &root.g("tools"), for_antigravity);
    let tool_choice = first_existing([root.g("generation_config.tool_choice"), root.g("tool_choice")]);
    if tool_choice.exists() {
        cpa_json::set(&mut out, "tool_choice", tool_choice.value());
    }
    let effort = interactions_thinking_effort(&root);
    if !effort.is_empty() {
        cpa_json::set(&mut out, "reasoning.effort", effort);
    }
    let summary = root.g("generation_config.thinking_summaries");
    if summary.is_string() {
        cpa_json::set(&mut out, "reasoning.summary", summary.str());
    }
    let modalities = root.g("response_modalities");
    if modalities.exists() {
        cpa_json::set(&mut out, "modalities", modalities.value());
    }
    let service_tier = root.g("service_tier");
    if service_tier.is_string() {
        cpa_json::set(&mut out, "service_tier", service_tier.str());
    }
    let format = root.g("response_format");
    if format.exists() {
        cpa_json::set(&mut out, "text.format", format.value());
    }
    cpa_json::to_vec(&out)
}

// ---- Responses -> Interactions

fn set_responses_input_on_interactions(out: &mut Value, input: &Res<'_>, for_antigravity: bool) {
    let mut function_names_by_call_id: HashMap<String, String> = HashMap::new();
    let mut items: Vec<Value> = Vec::new();
    if let Some(text) = input.as_str() {
        items.push(interactions_text_step("user_input", text));
    } else if input.is_array() {
        for item in input.array() {
            items.extend(responses_input_item_to_interactions(&item, &mut function_names_by_call_id, for_antigravity));
        }
    } else if input.is_object() {
        items.extend(responses_input_item_to_interactions(input, &mut function_names_by_call_id, for_antigravity));
    }
    set_items(out, "input", items);
}

/// Name of a function/custom tool call item, qualified with its namespace.
fn qualified_item_name(item: &Res<'_>) -> String {
    let name = item.g("name").str();
    let ns = item.g("namespace").str();
    if !ns.is_empty() && !name.is_empty() {
        return util::qualify_responses_namespace_tool_name(&ns, &name);
    }
    name
}

fn responses_input_item_to_interactions(
    item: &Res<'_>,
    function_names_by_call_id: &mut HashMap<String, String>,
    for_antigravity: bool,
) -> Option<Value> {
    let item_type = item.g("type").str();
    match item_type.as_str() {
        "message" => {
            let role = item.g("role").str();
            let step_type = if role == "assistant" || role == "model" { "model_output" } else { "user_input" };
            let mut step = cpa_json::parse_str(r#"{"type":"","content":[]}"#);
            cpa_json::set(&mut step, "type", step_type);
            append_responses_content_to_interactions(&mut step, &item.g("content"));
            Some(step)
        }
        "function_call" | "custom_tool_call" => {
            let call_id = first_non_blank(&[&item.g("call_id").str(), &item.g("id").str()]);
            let name = qualified_item_name(item);
            if !call_id.is_empty() && !name.is_empty() {
                function_names_by_call_id.insert(call_id, name);
            }
            Some(if item_type == "function_call" {
                responses_function_call_to_interactions(item, for_antigravity)
            } else {
                responses_custom_tool_call_to_interactions(item, for_antigravity)
            })
        }
        "function_call_output" | "custom_tool_call_output" => {
            Some(responses_function_output_to_interactions(item, function_names_by_call_id, for_antigravity))
        }
        "input_text" | "output_text" | "text" => {
            let step_type = if item_type == "output_text" { "model_output" } else { "user_input" };
            Some(interactions_text_step(step_type, &item.g("text").str()))
        }
        "input_image" | "output_image" => {
            let step_type = if item_type == "output_image" { "model_output" } else { "user_input" };
            let mut step = cpa_json::parse_str(r#"{"type":"","content":[]}"#);
            cpa_json::set(&mut step, "type", step_type);
            if let Some(part) = responses_content_part_to_interactions(item) {
                set_items(&mut step, "content", vec![part]);
            }
            Some(step)
        }
        _ => {
            let content = item.g("content");
            if !content.exists() {
                return None;
            }
            let mut step = cpa_json::parse_str(r#"{"type":"user_input","content":[]}"#);
            append_responses_content_to_interactions(&mut step, &content);
            Some(step)
        }
    }
}

fn append_responses_content_to_interactions(step: &mut Value, content: &Res<'_>) {
    let mut content_items: Vec<Value> = Vec::new();
    if let Some(text) = content.as_str() {
        let mut part = cpa_json::parse_str(r#"{"type":"text","text":""}"#);
        cpa_json::set(&mut part, "text", text);
        content_items.push(part);
    } else if content.is_array() {
        content_items.extend(content.array().iter().filter_map(responses_content_part_to_interactions));
    } else if content.is_object() {
        content_items.extend(responses_content_part_to_interactions(content));
    }
    set_items(step, "content", content_items);
}

/// Interactions content part for a Responses content part (also used by the response translator).
pub(super) fn responses_content_part_to_interactions(part: &Res<'_>) -> Option<Value> {
    match part.g("type").str().as_str() {
        "input_text" | "output_text" | "text" => {
            let mut out = cpa_json::parse_str(r#"{"type":"text","text":""}"#);
            cpa_json::set(&mut out, "text", part.g("text").str());
            return Some(out);
        }
        "input_image" | "output_image" => return Some(responses_image_part_to_interactions(part)),
        _ => {}
    }
    let text = part.g("text");
    if text.exists() {
        let mut out = cpa_json::parse_str(r#"{"type":"text","text":""}"#);
        cpa_json::set(&mut out, "text", text.str());
        return Some(out);
    }
    None
}

fn responses_image_part_to_interactions(part: &Res<'_>) -> Value {
    let mut out = cpa_json::parse_str(r#"{"type":"image"}"#);
    let image_url = first_non_blank(&[&part.g("image_url").str(), &part.g("url").str()]);
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

/// A `function_call` step for a Responses `function_call` item (also used by the response
/// translator for upstream output items).
pub(super) fn responses_function_call_to_interactions(item: &Res<'_>, for_antigravity: bool) -> Value {
    let mut out = cpa_json::parse_str(r#"{"type":"function_call","name":"","arguments":{}}"#);
    let mut name = qualified_item_name(item);
    if for_antigravity {
        name = common::antigravity_tool_name_to_upstream(&name);
    }
    cpa_json::set(&mut out, "name", name);
    let call_id = first_non_blank(&[&item.g("call_id").str(), &item.g("id").str()]);
    if !call_id.is_empty() {
        cpa_json::set(&mut out, "call_id", call_id);
    }
    set_json_value(&mut out, "arguments", &item.g("arguments"), "{}");
    out
}

fn responses_custom_tool_call_to_interactions(item: &Res<'_>, for_antigravity: bool) -> Value {
    let mut out = cpa_json::parse_str(r#"{"type":"function_call","name":"","arguments":{}}"#);
    let mut name = qualified_item_name(item);
    if for_antigravity {
        name = common::antigravity_tool_name_to_upstream(&name);
    }
    cpa_json::set(&mut out, "name", name);
    let call_id = first_non_blank(&[&item.g("call_id").str(), &item.g("id").str()]);
    if !call_id.is_empty() {
        cpa_json::set(&mut out, "call_id", call_id);
    }
    let input = item.g("input");
    if input.exists() {
        cpa_json::set(&mut out, "arguments.input", input.str());
    } else {
        set_json_value(&mut out, "arguments", &item.g("arguments"), "{}");
    }
    out
}

fn responses_function_output_to_interactions(
    item: &Res<'_>,
    function_names_by_call_id: &HashMap<String, String>,
    for_antigravity: bool,
) -> Value {
    let mut out = cpa_json::parse_str(r#"{"type":"function_result","name":"","result":{}}"#);
    let call_id = first_non_blank(&[&item.g("call_id").str(), &item.g("id").str()]);
    let mut name = qualified_item_name(item);
    if name.is_empty() && !call_id.is_empty() {
        name = function_names_by_call_id.get(&call_id).cloned().unwrap_or_default();
    }
    if !name.is_empty() {
        if for_antigravity {
            name = common::antigravity_tool_name_to_upstream(&name);
        }
        cpa_json::set(&mut out, "name", name);
    }
    if !call_id.is_empty() {
        cpa_json::set(&mut out, "call_id", call_id);
    }
    let result = first_existing([item.g("output"), item.g("result")]);
    set_json_value(&mut out, "result", &result, "{}");
    out
}

fn interactions_text_step(step_type: &str, text: &str) -> Value {
    let mut step = cpa_json::parse_str(r#"{"type":"","content":[{"type":"text","text":""}]}"#);
    cpa_json::set(&mut step, "type", step_type);
    cpa_json::set(&mut step, "content.0.text", text);
    step
}

fn append_responses_tools_to_interactions(out: &mut Value, root: &Value, for_antigravity: bool, for_devin: bool) {
    // A bare array body is treated as the `tools` list.
    let wrapped = root.is_array().then(|| {
        let mut wrapped = cpa_json::parse_str(r#"{"tools":[]}"#);
        cpa_json::set(&mut wrapped, "tools", root.clone());
        wrapped
    });
    let target_root = wrapped.as_ref().unwrap_or(root);
    let descriptors = util::collect_responses_tool_descriptors(target_root);
    if descriptors.is_empty() {
        return;
    }
    let winners = util::collect_responses_tool_winners(target_root);
    let mut seen_names: HashSet<&str> = HashSet::new();
    let mut tool_items: Vec<Value> = Vec::new();
    for descriptor in &descriptors {
        let Some(winner) = winners.get(&descriptor.name) else { continue };
        if winner.order != descriptor.order || !seen_names.insert(&descriptor.name) {
            continue;
        }
        if for_devin
            && (common::is_devin_codex_app_automation_update(&descriptor.namespace, &descriptor.local_name)
                || common::is_devin_codex_app_automation_update("", &descriptor.name))
        {
            continue;
        }
        let mut name = descriptor.name.clone();
        if for_antigravity {
            name = common::antigravity_tool_name_to_upstream(&name);
        }
        let mut item = cpa_json::parse_str(r#"{"type":"function","name":""}"#);
        cpa_json::set(&mut item, "name", name);
        let is_apply_patch = applypatch::is_custom_tool(&descriptor.tool);
        let mut desc = util::responses_tool_description(&descriptor.tool);
        if is_apply_patch {
            desc = applypatch::description(&descriptor.tool);
        }
        if !desc.is_empty() {
            if for_devin {
                desc = common::sanitize_devin_tool_description(&descriptor.name, &desc);
                if !descriptor.local_name.is_empty() && descriptor.local_name != descriptor.name {
                    desc = common::sanitize_devin_tool_description(&descriptor.local_name, &desc);
                }
            }
            cpa_json::set(&mut item, "description", desc);
        }
        if is_apply_patch {
            cpa_json::set(&mut item, "parameters", cpa_json::parse(&applypatch::parameters()));
        } else if descriptor.tool_type == "custom" {
            cpa_json::set(
                &mut item,
                "parameters",
                cpa_json::parse_str(r#"{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}"#),
            );
        } else if let Some(params) = util::responses_tool_parameters(&descriptor.tool) {
            cpa_json::set(&mut item, "parameters", params.clone());
        }
        tool_items.push(item);
    }
    if !tool_items.is_empty() {
        cpa_json::set(out, "tools", Value::Array(tool_items));
    }
}

// ---- Interactions -> Responses

fn set_interactions_input_on_responses(out: &mut Value, input: &Res<'_>, for_antigravity: bool) {
    let mut items: Vec<Value> = Vec::new();
    if let Some(text) = input.as_str() {
        items.push(interactions_text_message(text));
    } else if input.is_array() {
        items.extend(input.array().iter().filter_map(|item| interactions_input_item_to_responses(item, for_antigravity)));
    } else if input.is_object() {
        items.extend(interactions_input_item_to_responses(input, for_antigravity));
    }
    set_items(out, "input", items);
}

fn interactions_text_message(text: &str) -> Value {
    let mut item = cpa_json::parse_str(r#"{"type":"message","role":"user","content":[{"type":"input_text","text":""}]}"#);
    cpa_json::set(&mut item, "content.0.text", text);
    item
}

fn interactions_input_item_to_responses(item: &Res<'_>, for_antigravity: bool) -> Option<Value> {
    match item.g("type").str().as_str() {
        "user_input" => Some(interactions_message_to_responses(item, "user")),
        "model_output" => Some(interactions_message_to_responses(item, "assistant")),
        "thought" => Some(interactions_thought_to_responses(item)),
        "function_call" => Some(interactions_function_call_to_responses_with_identity(item, for_antigravity, None)),
        "function_result" => Some(interactions_function_result_to_responses(item, for_antigravity)),
        _ => item.as_str().map(interactions_text_message),
    }
}

fn interactions_message_to_responses(item: &Res<'_>, role: &str) -> Value {
    let mut content_items: Vec<Value> = Vec::new();
    let content = item.g("content");
    if let Some(text) = content.as_str() {
        let part_type = if role == "assistant" { "output_text" } else { "input_text" };
        let mut part = cpa_json::parse_str(r#"{"type":"","text":""}"#);
        cpa_json::set(&mut part, "type", part_type);
        cpa_json::set(&mut part, "text", text);
        content_items.push(part);
    } else {
        content.for_each(|_, part| {
            content_items.extend(interactions_content_part_to_responses(&part, role));
            true
        });
    }
    let mut out = cpa_json::parse_str(r#"{"type":"message","role":"","content":[]}"#);
    cpa_json::set(&mut out, "role", role);
    set_items(&mut out, "content", content_items);
    out
}

fn interactions_thought_to_responses(item: &Res<'_>) -> Value {
    let summary_items: Vec<Value> = interactions_content_texts(&item.g("content"))
        .iter()
        .map(|text| {
            let mut part = cpa_json::parse_str(r#"{"type":"summary_text","text":""}"#);
            cpa_json::set(&mut part, "text", text.as_str());
            part
        })
        .collect();
    let mut out = cpa_json::parse_str(r#"{"type":"reasoning","summary":[]}"#);
    set_items(&mut out, "summary", summary_items);
    out
}

/// Responses content part for an Interactions content part (also used by the response translator).
pub(super) fn interactions_content_part_to_responses(part: &Res<'_>, role: &str) -> Option<Value> {
    let mut part_type = part.g("type").str();
    if part_type.is_empty() && part.g("text").exists() {
        part_type = "text".to_string();
    }
    let assistant = role == "assistant";
    match part_type.as_str() {
        "text" => {
            let mut out = cpa_json::parse_str(r#"{"type":"","text":""}"#);
            cpa_json::set(&mut out, "type", if assistant { "output_text" } else { "input_text" });
            cpa_json::set(&mut out, "text", part.g("text").str());
            Some(out)
        }
        "image" => {
            let mut out = cpa_json::parse_str(r#"{"type":""}"#);
            cpa_json::set(&mut out, "type", if assistant { "output_image" } else { "input_image" });
            let image_url = interactions_media_data_url(part);
            if !image_url.is_empty() {
                cpa_json::set(&mut out, "image_url", image_url);
            }
            Some(out)
        }
        "audio" => {
            let mut out = cpa_json::parse_str(r#"{"type":"output_text","text":""}"#);
            let format = media_format(&part.g("mime_type").str());
            cpa_json::set(&mut out, "text", format!("Audio content: inline data (Format: {format})"));
            Some(out)
        }
        "video" | "document" => {
            let mut out = cpa_json::parse_str(r#"{"type":""}"#);
            cpa_json::set(&mut out, "type", if assistant { "output_file" } else { "input_file" });
            let data_url = interactions_media_data_url(part);
            if !data_url.is_empty() {
                cpa_json::set(&mut out, "file_data", data_url);
            }
            let filename = part.g("filename").str();
            if !filename.is_empty() {
                cpa_json::set(&mut out, "filename", filename);
            }
            Some(out)
        }
        _ => None,
    }
}

/// A Responses `function_call` (or `custom_tool_call`) item for an Interactions `function_call`
/// step. `tool_identity_map` restores namespace, local name and custom-ness from the request.
pub(super) fn interactions_function_call_to_responses_with_identity(
    item: &Res<'_>,
    for_antigravity: bool,
    tool_identity_map: Option<&HashMap<String, util::ResponsesToolIdentity>>,
) -> Value {
    let raw_name = item.g("name").str();
    let mut name = raw_name.clone();
    let mut namespace = String::new();
    let mut is_custom = false;
    if for_antigravity {
        name = common::antigravity_upstream_tool_name_to_client(&name);
    }
    if let Some(map) = tool_identity_map
        && let Some(identity) = map.get(&raw_name).or_else(|| map.get(&name))
    {
        name = identity.name.clone();
        namespace = identity.namespace.clone();
        is_custom = identity.custom;
    }
    let call_id = first_non_blank(&[&item.g("call_id").str(), &item.g("id").str()]);
    let arguments = json_string_value(&item.g("arguments"), "{}");
    if is_custom {
        let mut out = cpa_json::parse_str(r#"{"type":"custom_tool_call","call_id":"","name":"","input":""}"#);
        if !call_id.is_empty() {
            cpa_json::set(&mut out, "call_id", call_id);
        }
        if !namespace.is_empty() {
            cpa_json::set(&mut out, "namespace", namespace);
        }
        cpa_json::set(&mut out, "name", name);
        cpa_json::set(&mut out, "input", util::unwrap_responses_custom_tool_input(&arguments));
        return out;
    }
    let mut out = cpa_json::parse_str(r#"{"type":"function_call","call_id":"","name":"","arguments":"{}"}"#);
    if !call_id.is_empty() {
        cpa_json::set(&mut out, "call_id", call_id);
    }
    if !namespace.is_empty() {
        cpa_json::set(&mut out, "namespace", namespace);
    }
    cpa_json::set(&mut out, "name", name);
    cpa_json::set(&mut out, "arguments", arguments);
    out
}

fn interactions_function_result_to_responses(item: &Res<'_>, for_antigravity: bool) -> Value {
    let mut out = cpa_json::parse_str(r#"{"type":"function_call_output","call_id":"","output":""}"#);
    let call_id = first_non_blank(&[&item.g("call_id").str(), &item.g("id").str()]);
    if !call_id.is_empty() {
        cpa_json::set(&mut out, "call_id", call_id);
    }
    let mut name = item.g("name").str();
    if !name.is_empty() {
        if for_antigravity {
            name = common::antigravity_upstream_tool_name_to_client(&name);
        }
        cpa_json::set(&mut out, "name", name);
    }
    let result = first_existing([item.g("result"), item.g("output")]);
    cpa_json::set(&mut out, "output", json_string_value(&result, ""));
    out
}

fn append_interactions_tools_to_responses(out: &mut Value, tools: &Res<'_>, for_antigravity: bool) {
    if !tools.is_array() {
        return;
    }
    let mut tool_items: Vec<Value> = Vec::new();
    for tool in tools.array() {
        tool_items.extend(responses_tool_from_interactions_tool(&tool, for_antigravity));
        let decls = tool.g("function_declarations");
        if decls.is_array() {
            tool_items.extend(decls.array().iter().filter_map(|decl| responses_tool_from_interactions_tool(decl, for_antigravity)));
        }
    }
    if !tool_items.is_empty() {
        cpa_json::set(out, "tools", Value::Array(tool_items));
    }
}

fn responses_tool_from_interactions_tool(tool: &Res<'_>, for_antigravity: bool) -> Option<Value> {
    let mut name = first_non_blank(&[&tool.g("name").str(), &tool.g("function.name").str()]);
    if name.is_empty() {
        return None;
    }
    if for_antigravity {
        name = common::antigravity_upstream_tool_name_to_client(&name);
    }
    let mut out = cpa_json::parse_str(r#"{"type":"function","name":""}"#);
    cpa_json::set(&mut out, "name", name);
    let description = first_existing([tool.g("description"), tool.g("function.description")]);
    if description.exists() {
        cpa_json::set(&mut out, "description", description.str());
    }
    let parameters = first_existing([tool.g("parameters"), tool.g("function.parameters"), tool.g("parametersJsonSchema")]);
    if parameters.exists() {
        cpa_json::set(&mut out, "parameters", parameters.value());
    }
    Some(out)
}

/// Text parts of Interactions content: the string itself, or each part's `text` / `content.text`.
pub(super) fn interactions_content_texts(content: &Res<'_>) -> Vec<String> {
    if let Some(text) = content.as_str() {
        return vec![text.to_string()];
    }
    if !content.is_array() {
        return vec![];
    }
    content
        .array()
        .iter()
        .map(|part| first_non_blank(&[&part.g("text").str(), &part.g("content.text").str()]))
        .filter(|text| !text.is_empty())
        .collect()
}

fn interactions_media_data_url(part: &Res<'_>) -> String {
    let url = first_non_blank(&[&part.g("image_url").str(), &part.g("file_data").str(), &part.g("url").str()]);
    if !url.is_empty() {
        return url;
    }
    let data = part.g("data").str();
    if data.is_empty() {
        return String::new();
    }
    let mut mime_type = part.g("mime_type").str();
    if mime_type.is_empty() {
        mime_type = "application/octet-stream".to_string();
    }
    format!("data:{mime_type};base64,{data}")
}

fn media_format(mime_type: &str) -> String {
    if mime_type.is_empty() {
        return "unknown".to_string();
    }
    match mime_type.split_once('/') {
        Some((_, format)) if !format.is_empty() => format.to_string(),
        _ => mime_type.to_string(),
    }
}

/// `(mime, data)` of a `data:<mime>[;params],<data>` URL.
fn parse_data_url(value: &str) -> Option<(String, String)> {
    let rest = value.strip_prefix("data:")?;
    let (header, data) = rest.split_once(',')?;
    let mime_type = header.split(';').next().unwrap_or_default();
    let mime_type = if mime_type.is_empty() { "application/octet-stream" } else { mime_type };
    Some((mime_type.to_string(), data.to_string()))
}
