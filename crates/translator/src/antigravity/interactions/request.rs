//! Interactions request -> Antigravity request (Go: interactions_antigravity_request.go).

use crate::common::first_trimmed;
use std::collections::HashMap;

use cpa_core::util;
use cpa_json::{json, J, Res, Value};

use crate::antigravity::{function_names, function_response};
use crate::common;
use crate::gemini::common::default_safety_settings;

/// Go: `ConvertInteractionsRequestToAntigravity`.
pub fn convert_interactions_request_to_antigravity(model: &str, input_raw_json: &[u8], stream: bool) -> Vec<u8> {
    let root = cpa_json::parse(input_raw_json);
    let function_name_map = util::sanitized_function_name_map(input_raw_json);
    let mut out = json!({"project": "", "request": {"contents": []}, "model": ""});
    cpa_json::set(&mut out, "model", model);
    if stream || root.g("stream").bool() {
        cpa_json::set(&mut out, "request.stream", true);
    }
    copy_system(&mut out, &root);
    copy_generation_config(&mut out, &root);
    let mut content_items: Vec<Value> = Vec::new();
    append_input(&mut content_items, &root.g("input"), input_raw_json);
    if !content_items.is_empty() {
        cpa_json::set(&mut out, "request.contents", Value::Array(content_items));
    }
    copy_tools(&mut out, &root, &function_name_map);
    if out.g("request.toolConfig.functionCallingConfig.mode").str() == "NONE" {
        cpa_json::delete(&mut out, "request.tools");
    }
    function_names::rewrite_function_names(
        &mut out,
        &function_name_map,
        &["functionCall", "functionResponse"],
        &["request.toolConfig.functionCallingConfig.allowedFunctionNames"],
    );
    attach_default_safety_settings(&mut out);
    cpa_json::to_vec(&out)
}

/// Sets the default safety settings at `request.safetySettings` unless already present.
fn attach_default_safety_settings(out: &mut Value) {
    if out.g("request.safetySettings").exists() {
        return;
    }
    cpa_json::set(out, "request.safetySettings", Value::Array(default_safety_settings()));
}

fn copy_system(out: &mut Value, root: &Value) {
    let sys = root.g("system_instruction");
    if !sys.exists() {
        return;
    }
    if sys.is_string() {
        cpa_json::set(out, "request.systemInstruction", json!({"parts": [{"text": sys.str()}]}));
        return;
    }
    let text = sys.g("text");
    if text.exists() && !sys.g("parts").exists() {
        cpa_json::set(out, "request.systemInstruction", json!({"parts": [{"text": text.str()}]}));
        return;
    }
    cpa_json::set(out, "request.systemInstruction", sys.value());
}

fn copy_generation_config(out: &mut Value, root: &Value) {
    let cfg = root.g("generation_config");
    if cfg.exists() {
        cpa_json::set(out, "request.generationConfig", convert_snake_case_keys_to_camel_case(&cfg.value()));
    } else {
        let cfg = root.g("generationConfig");
        if cfg.exists() {
            cpa_json::set(out, "request.generationConfig", cfg.value());
        }
    }
    normalize_generation_config(out);
    copy_reasoning(out, root);
    copy_response_modalities(out, root);
    copy_tool_choice(out, root);
}

/// Moves thinking fields under `thinkingConfig`, resolves `thinkingSummaries`, drops `toolChoice`.
fn normalize_generation_config(out: &mut Value) {
    for key in ["thinkingLevel", "thinkingBudget", "includeThoughts"] {
        let path = format!("request.generationConfig.{key}");
        let v = out.g(&path);
        if v.exists() {
            let v = v.value();
            cpa_json::set(out, &format!("request.generationConfig.thinkingConfig.{key}"), v);
            cpa_json::delete(out, &path);
        }
    }
    let summaries = out.g("request.generationConfig.thinkingSummaries");
    if summaries.exists() {
        let include = thinking_summaries_include_thoughts(&summaries);
        if let Some(include) = include {
            cpa_json::set(out, "request.generationConfig.thinkingConfig.includeThoughts", include);
        }
        cpa_json::delete(out, "request.generationConfig.thinkingSummaries");
    }
    if out.g("request.generationConfig.toolChoice").exists() {
        cpa_json::delete(out, "request.generationConfig.toolChoice");
    }
}

fn copy_reasoning(out: &mut Value, root: &Value) {
    let reasoning = root.g("reasoning");
    if !reasoning.exists() {
        return;
    }
    let mut effort = reasoning.g("effort").str().trim().to_lowercase();
    if effort.is_empty() {
        effort = reasoning.g("thinking_level").str().trim().to_lowercase();
    }
    if !effort.is_empty() {
        // Thinking amount and summary visibility are independent: this OpenAI-style alias controls
        // only the amount; includeThoughts is written only for an explicit summary selector.
        if effort == "auto" {
            cpa_json::set(out, "request.generationConfig.thinkingConfig.thinkingBudget", -1);
        } else {
            cpa_json::set(out, "request.generationConfig.thinkingConfig.thinkingLevel", effort);
        }
    }
    let summary = reasoning.g("summary");
    if summary.exists() {
        if let Some(include) = thinking_summaries_include_thoughts(&summary) {
            cpa_json::set(out, "request.generationConfig.thinkingConfig.includeThoughts", include);
        }
    }
}

fn copy_response_modalities(out: &mut Value, root: &Value) {
    let mut mods = root.g("response_modalities");
    if !mods.exists() {
        mods = root.g("responseModalities");
    }
    if !mods.exists() || !mods.is_array() {
        return;
    }
    let response_mods: Vec<&str> = mods
        .array()
        .iter()
        .filter_map(|m| match m.str().trim().to_lowercase().as_str() {
            "text" => Some("TEXT"),
            "image" => Some("IMAGE"),
            "audio" => Some("AUDIO"),
            _ => None,
        })
        .collect();
    if !response_mods.is_empty() {
        cpa_json::set(out, "request.generationConfig.responseModalities", json!(response_mods));
    }
}

fn copy_tool_choice(out: &mut Value, root: &Value) {
    let mut tool_choice = root.g("tool_choice");
    if !tool_choice.exists() {
        tool_choice = root.g("generation_config.tool_choice");
    }
    if !tool_choice.exists() {
        tool_choice = root.g("generationConfig.toolChoice");
    }
    if !tool_choice.exists() {
        return;
    }
    let mut mode = "";
    let mut allowed_names: Vec<String> = Vec::new();
    if tool_choice.is_string() {
        mode = match tool_choice.str().trim().to_lowercase().as_str() {
            "none" => "NONE",
            "auto" => "AUTO",
            "required" | "any" => "ANY",
            _ => "",
        };
    } else if tool_choice.is_object() {
        match tool_choice.g("type").str().trim().to_lowercase().as_str() {
            "none" => mode = "NONE",
            "auto" => mode = "AUTO",
            "required" | "any" => mode = "ANY",
            "function" => {
                mode = "ANY";
                let name = tool_choice.g("function.name").str();
                if !name.trim().is_empty() {
                    allowed_names.push(name);
                }
            }
            "tool" => {
                mode = "ANY";
                let name = tool_choice.g("name").str();
                if !name.trim().is_empty() {
                    allowed_names.push(name);
                }
            }
            _ => {}
        }
    }
    if mode.is_empty() {
        return;
    }
    cpa_json::set(out, "request.toolConfig.functionCallingConfig.mode", mode);
    if !allowed_names.is_empty() {
        cpa_json::set(out, "request.toolConfig.functionCallingConfig.allowedFunctionNames", json!(allowed_names));
    }
}

/// Mutable walk state over the `input` steps (Go: `antigravityInteractionsInputContext`).
#[derive(Default)]
struct InputContext {
    items: Vec<Value>,
    in_model_turn: bool,
    last_step_type: String,
    pending_signature: String,
}

impl InputContext {
    fn last_role_is(&self, role: &str) -> bool {
        self.items.last().is_some_and(|c| c.g("role").str() == role)
    }

    /// Appends `part` to the open model turn, or starts a new model turn with it.
    fn push_model_part(&mut self, part: Value) {
        if self.in_model_turn && self.last_role_is("model") {
            append_content_parts(self.items.last_mut().expect("last_role_is checked"), vec![part]);
        } else {
            self.items.push(content("model", vec![part]));
        }
    }

    fn push_model_parts(&mut self, parts: Vec<Value>) {
        if self.in_model_turn && self.last_role_is("model") {
            append_content_parts(self.items.last_mut().expect("last_role_is checked"), parts);
        } else {
            self.items.push(content("model", parts));
        }
    }

    fn pending_carrier(&mut self) -> Value {
        let mut carrier = text_part("", false);
        cpa_json::set(&mut carrier, "thoughtSignature", std::mem::take(&mut self.pending_signature));
        carrier
    }

    fn flush_pending_signature(&mut self) {
        if self.pending_signature.is_empty() {
            return;
        }
        let carrier = self.pending_carrier();
        if self.last_role_is("model") {
            append_content_parts(self.items.last_mut().expect("last_role_is checked"), vec![carrier]);
        } else {
            self.items.push(content("model", vec![carrier]));
        }
    }

    /// Flushes the pending signature and leaves the model turn (before any non-model step).
    fn leave_model_turn(&mut self) {
        if self.in_model_turn {
            self.flush_pending_signature();
            self.in_model_turn = false;
        }
    }
}

/// `raw` is the request text: `path` arguments below locate steps in it so a stringified (`$ref`)
/// function result keeps its original whitespace.
fn append_input(items: &mut Vec<Value>, input: &Res<'_>, raw: &[u8]) {
    if !input.exists() {
        return;
    }
    let mut ctx = InputContext { items: std::mem::take(items), ..Default::default() };
    if input.is_string() {
        append_text_content(&mut ctx.items, "user", &input.str());
        ctx.last_step_type = "text".into();
        *items = ctx.items;
        return;
    }
    let steps = input.g("steps");
    if input.is_array() {
        for (k, item) in input.array().iter().enumerate() {
            append_step(&mut ctx, item, "user", raw, &format!("input.{k}"));
        }
    } else if steps.exists() && steps.is_array() {
        let role = input.g("role").str();
        let default_role = if role == "model" || role == "assistant" { "model" } else { "user" };
        for (k, step) in steps.array().iter().enumerate() {
            append_step(&mut ctx, step, default_role, raw, &format!("input.steps.{k}"));
        }
    } else {
        append_step(&mut ctx, input, "user", raw, "input");
    }
    ctx.flush_pending_signature();
    *items = ctx.items;
}

fn step_signature(step: &Res<'_>) -> String {
    first_trimmed(&[step.g("signature").str(), step.g("thought_signature").str(), step.g("thoughtSignature").str()])
}

fn append_step(ctx: &mut InputContext, step: &Res<'_>, default_role: &str, raw: &[u8], path: &str) {
    if step.is_string() {
        if ctx.in_model_turn {
            ctx.flush_pending_signature();
            ctx.in_model_turn = false;
        }
        append_text_content(&mut ctx.items, default_role, &step.str());
        ctx.last_step_type = "text".into();
        return;
    }
    let steps = step.g("steps");
    if steps.exists() && steps.is_array() {
        let item_role = step.g("role").str();
        let role = if item_role == "model" || item_role == "assistant" {
            "model"
        } else if item_role == "user" {
            "user"
        } else {
            default_role
        };
        for (k, child) in steps.array().iter().enumerate() {
            append_step(ctx, child, role, raw, &format!("{path}.steps.{k}"));
        }
        return;
    }
    match step.g("type").str().as_str() {
        "model_output" => {
            if !ctx.pending_signature.is_empty() {
                let carrier = ctx.pending_carrier();
                ctx.push_model_part(carrier);
            }
            let parts = extract_step_content_parts(step, false);
            if !parts.is_empty() {
                ctx.push_model_parts(parts);
            }
            ctx.in_model_turn = true;
            ctx.last_step_type = "model_output".into();
        }
        "thought" => {
            let sig = step_signature(step);
            if !sig.is_empty() {
                if !ctx.pending_signature.is_empty() && ctx.pending_signature != sig {
                    let mut carrier = text_part("", false);
                    cpa_json::set(&mut carrier, "thoughtSignature", ctx.pending_signature.clone());
                    ctx.push_model_part(carrier);
                }
                ctx.pending_signature = sig;
            }
            let parts = extract_thought_parts(step);
            if !parts.is_empty() {
                ctx.push_model_parts(parts);
            }
            ctx.in_model_turn = true;
            ctx.last_step_type = "thought".into();
        }
        "function_call" => {
            let mut part = build_function_call_part(step);
            let mut sig = step_signature(step);
            if sig.is_empty() && !ctx.pending_signature.is_empty() {
                sig = std::mem::take(&mut ctx.pending_signature);
            } else if !sig.is_empty() && !ctx.pending_signature.is_empty() {
                if ctx.pending_signature == sig {
                    ctx.pending_signature.clear();
                } else {
                    let carrier = ctx.pending_carrier();
                    ctx.push_model_part(carrier);
                }
            }
            if !sig.is_empty() {
                cpa_json::set(&mut part, "thoughtSignature", sig);
            }
            ctx.push_model_part(part);
            ctx.in_model_turn = true;
            ctx.last_step_type = "function_call".into();
        }
        "function_result" => {
            ctx.leave_model_turn();
            let part = build_function_result_part(step, cpa_json::raw_at(raw, &format!("{path}.result")));
            if ctx.last_step_type == "function_result" && ctx.last_role_is("user") {
                if let Some(last) = ctx.items.last_mut() {
                    append_user_content_part(last, part);
                }
            } else {
                ctx.items.push(content("user", vec![part]));
            }
            ctx.last_step_type = "function_result".into();
        }
        "user_input" | "" => {
            ctx.leave_model_turn();
            if step.g("parts").exists() {
                append_native_content(&mut ctx.items, step, default_role);
            } else {
                append_content_list(&mut ctx.items, default_role, &step.g("content"));
            }
            ctx.last_step_type = "user_input".into();
        }
        _ => {
            ctx.leave_model_turn();
            if step.g("parts").exists() {
                append_native_content(&mut ctx.items, step, default_role);
            } else if step.g("content").exists() {
                append_content_list(&mut ctx.items, default_role, &step.g("content"));
            } else if step.g("text").exists() {
                append_text_content(&mut ctx.items, default_role, &step.g("text").str());
            }
            ctx.last_step_type = "default".into();
        }
    }
}

/// Parts from a step's `content` value: array elements, one object, or one text string.
fn parts_from_content_value(content: &Res<'_>, thought: bool) -> Vec<Value> {
    let mut parts = Vec::new();
    if content.is_array() {
        for part in content.array() {
            if let Some(p) = content_to_part(&part, thought) {
                parts.push(p);
            }
        }
    } else if content.is_object() {
        if let Some(p) = content_to_part(content, thought) {
            parts.push(p);
        }
    } else if content.is_string() {
        parts.push(text_part(&content.str(), thought));
    }
    parts
}

fn extract_thought_parts(step: &Res<'_>) -> Vec<Value> {
    let mut content = step.g("content");
    if !content.exists() {
        content = step.g("summary");
    }
    if !content.exists() {
        content = step.g("text");
    }
    if !content.exists() {
        return vec![];
    }
    parts_from_content_value(&content, true)
}

fn extract_step_content_parts(step: &Res<'_>, thought: bool) -> Vec<Value> {
    let mut content = step.g("content");
    if !content.exists() {
        content = step.g("text");
    }
    if !content.exists() {
        return vec![];
    }
    parts_from_content_value(&content, thought)
}

fn append_content_parts(content: &mut Value, new_parts: Vec<Value>) {
    if new_parts.is_empty() {
        return;
    }
    let mut parts: Vec<Value> = content.g("parts").array().iter().map(|p| p.value()).collect();
    parts.extend(new_parts);
    cpa_json::set(content, "parts", Value::Array(parts));
}

fn append_user_content_part(content: &mut Value, part: Value) {
    let mut raw_parts: Vec<Vec<u8>> = content.g("parts").array().iter().map(|p| cpa_json::to_vec(&p.value())).collect();
    raw_parts.push(cpa_json::to_vec(&part));
    let raw_parts = common::reorder_gemini_user_parts(raw_parts);
    cpa_json::set(content, "parts", Value::Array(raw_parts.iter().map(|p| cpa_json::parse(p)).collect()));
}

fn build_function_call_part(step: &Res<'_>) -> Value {
    let mut part = json!({"functionCall": {"name": "", "args": {}}});
    cpa_json::set(&mut part, "functionCall.name", step.g("name").str());
    let call_id = step.g("call_id");
    let id = step.g("id");
    if call_id.exists() {
        cpa_json::set(&mut part, "functionCall.id", call_id.str());
    } else if id.exists() {
        cpa_json::set(&mut part, "functionCall.id", id.str());
    }
    let args = step.g("arguments");
    if args.exists() {
        cpa_json::set(&mut part, "functionCall.args", args.value());
    }
    part
}

fn build_function_result_part(step: &Res<'_>, result_raw: Option<&str>) -> Value {
    let mut part = json!({"functionResponse": {"name": "", "response": {}}});
    cpa_json::set(&mut part, "functionResponse.name", step.g("name").str());
    let call_id = step.g("call_id");
    let id = step.g("id");
    if call_id.exists() {
        cpa_json::set(&mut part, "functionResponse.id", call_id.str());
    } else if id.exists() {
        cpa_json::set(&mut part, "functionResponse.id", id.str());
    }
    let result = step.g("result");
    if result.exists() {
        function_response::set_function_response_result(&mut part, "functionResponse.response", &result, result_raw);
    }
    part
}

fn append_native_content(items: &mut Vec<Value>, step: &Res<'_>, default_role: &str) {
    let parts = step.g("parts");
    if !parts.exists() || !parts.is_array() {
        return;
    }
    let part_items: Vec<Value> = parts.array().iter().filter_map(native_part).collect();
    if !part_items.is_empty() {
        let role = content_role(&step.g("role").str(), default_role);
        items.push(content(role, part_items));
    }
}

fn append_content_list(items: &mut Vec<Value>, role: &str, content_value: &Res<'_>) {
    if !content_value.exists() {
        return;
    }
    if content_value.is_array() {
        for part in content_value.array() {
            append_content_part(items, role, &part);
        }
        return;
    }
    if content_value.is_object() {
        append_content_part(items, role, content_value);
    } else if content_value.is_string() {
        append_text_content(items, role, &content_value.str());
    }
}

fn append_content_part(items: &mut Vec<Value>, role: &str, part: &Res<'_>) {
    if let Some(part_json) = content_to_part(part, false) {
        items.push(content(role, vec![part_json]));
    }
}

/// One Interactions content item as a Gemini part (`None` when unsupported or incomplete).
fn content_to_part(c: &Res<'_>, thought: bool) -> Option<Value> {
    let text = c.g("text");
    if text.exists() {
        return Some(text_part(&text.str(), thought));
    }
    let inline = c.g("inline_data");
    if inline.exists() {
        return inline_data_part(&inline);
    }
    let inline = c.g("inlineData");
    if inline.exists() {
        return inline_data_part(&inline);
    }
    match c.g("type").str().trim().to_lowercase().as_str() {
        "image" | "audio" | "video" | "document" => {
            let mime = c.g("mime_type");
            if mime.exists() || c.g("mimeType").exists() {
                let mut mime_type = mime.str();
                if mime_type.is_empty() {
                    mime_type = c.g("mimeType").str();
                }
                let data = c.g("data").str();
                if !data.is_empty() {
                    return inline_data_from(&mime_type, &data);
                }
            }
            let uri = c.g("file_uri");
            if uri.exists() || c.g("fileUri").exists() {
                let mut file_uri = uri.str();
                if file_uri.is_empty() {
                    file_uri = c.g("fileUri").str();
                }
                let mut mime_type = c.g("mime_type").str();
                if mime_type.is_empty() {
                    mime_type = c.g("mimeType").str();
                }
                return file_data_from(&mime_type, &file_uri);
            }
            let url = c.g("url");
            if url.exists() {
                return inline_data_part_from_data_url(&url.str());
            }
            None
        }
        "image_url" => inline_data_part_from_data_url(&c.g("image_url.url").str()),
        "input_audio" => {
            let mime_type = input_audio_mime_type(&c.g("input_audio.format").str());
            inline_data_from(mime_type, &c.g("input_audio.data").str())
        }
        "file" => {
            let filename = c.g("file.filename").str();
            let file_data = c.g("file.file_data").str();
            let (mime_type, data) = common::normalize_openai_file_data(&filename, "", &file_data)?;
            inline_data_from(&mime_type, &data)
        }
        _ => None,
    }
}

fn copy_tools(out: &mut Value, root: &Value, function_name_map: &HashMap<String, String>) {
    if out.g("request.toolConfig.functionCallingConfig.mode").str() == "NONE" {
        cpa_json::delete(out, "request.tools");
        return;
    }
    let tools = root.g("tools");
    if !tools.exists() {
        return;
    }
    if !tools.is_array() {
        cpa_json::set(out, "request.tools", tools.value());
        return;
    }
    let mut function_declarations: Vec<Value> = Vec::new();
    let mut other_tools: Vec<Value> = Vec::new();
    for tool in tools.array() {
        let decl_key = ["functionDeclarations", "function_declarations"].into_iter().find(|k| {
            let d = tool.g(k);
            d.exists() && d.is_array()
        });
        if let Some(key) = decl_key {
            function_declarations.extend(tool.g(key).array().iter().filter_map(|d| function_declaration(d, function_name_map)));
            continue;
        }
        if tool.g("type").str() == "function" || tool.g("name").exists() {
            if let Some(converted) = function_declaration(&tool, function_name_map) {
                function_declarations.push(converted);
            }
            continue;
        }
        let tool_type = tool.g("type").str();
        let builtin = match tool_type.as_str() {
            "url_context" => Some(("urlContext", "url_context")),
            "code_execution" => Some(("codeExecution", "code_execution")),
            "google_search" | "web_search" => Some(("googleSearch", "google_search")),
            _ => None,
        };
        if let Some((camel, snake)) = builtin {
            let mut node = json!({ camel: {} });
            let snake_v = tool.g(snake);
            let camel_v = tool.g(camel);
            if snake_v.exists() && snake_v.is_object() {
                cpa_json::set(&mut node, camel, snake_v.value());
            } else if camel_v.exists() && camel_v.is_object() {
                cpa_json::set(&mut node, camel, camel_v.value());
            }
            other_tools.push(node);
            continue;
        }
        let mut raw = tool.value();
        if tool_type.is_empty() {
            for (snake, camel) in [("url_context", "urlContext"), ("code_execution", "codeExecution"), ("google_search", "googleSearch"), ("web_search", "googleSearch")] {
                let v = tool.g(snake);
                if v.exists() {
                    cpa_json::set(&mut raw, camel, v.value());
                    cpa_json::delete(&mut raw, snake);
                }
            }
        }
        other_tools.push(raw);
    }
    let deduplicated = cpa_json::parse(&util::deduplicate_function_declarations(&common::join_raw_array(
        &function_declarations.iter().map(cpa_json::to_vec).collect::<Vec<_>>(),
    )));
    let has_function = deduplicated.as_array().is_some_and(|a| !a.is_empty());
    if has_function || !other_tools.is_empty() {
        let mut tool_items: Vec<Value> = Vec::with_capacity(1 + other_tools.len());
        if has_function {
            tool_items.push(json!({"functionDeclarations": deduplicated}));
        }
        tool_items.extend(other_tools);
        cpa_json::set(out, "request.tools", Value::Array(tool_items));
    }
}

fn function_declaration(decl: &Res<'_>, function_name_map: &HashMap<String, String>) -> Option<Value> {
    let nested = decl.g("function");
    let f = if nested.exists() && nested.is_object() { nested } else { decl.clone() };
    let name = f.g("name").str();
    if name.trim().is_empty() {
        return None;
    }
    let mut out = json!({"name": "", "parametersJsonSchema": {"type": "object", "properties": {}}});
    cpa_json::set(&mut out, "name", util::map_sanitized_function_name(function_name_map, &name));
    let desc = f.g("description");
    if desc.exists() {
        cpa_json::set(&mut out, "description", desc.str());
    }
    let params = f.g("parametersJsonSchema");
    if params.exists() {
        cpa_json::set(&mut out, "parametersJsonSchema", params.value());
    } else {
        let params = f.g("parameters");
        if params.exists() {
            cpa_json::set(&mut out, "parametersJsonSchema", params.value());
        }
    }
    let response = f.g("response");
    if response.exists() {
        cpa_json::set(&mut out, "response", response.value());
    }
    let response_schema = f.g("responseJsonSchema");
    if response_schema.exists() {
        cpa_json::set(&mut out, "responseJsonSchema", response_schema.value());
    }
    Some(out)
}

fn native_part(part: &Res<'_>) -> Option<Value> {
    if part.g("text").exists() || part.g("functionCall").exists() || part.g("functionResponse").exists() {
        return Some(part.value());
    }
    let inline = part.g("inlineData");
    if inline.exists() {
        return inline_data_part(&inline);
    }
    let file = part.g("fileData");
    if file.exists() {
        return file_data_part(&file);
    }
    let inline = part.g("inline_data");
    if inline.exists() {
        return inline_data_part(&inline);
    }
    let file = part.g("file_data");
    if file.exists() {
        return file_data_part(&file);
    }
    None
}

fn text_part(text: &str, thought: bool) -> Value {
    let mut part = json!({"text": text});
    if thought {
        cpa_json::set(&mut part, "thought", true);
    }
    part
}

fn inline_data_part(inline: &Res<'_>) -> Option<Value> {
    let mut mime_type = inline.g("mimeType").str();
    if mime_type.is_empty() {
        mime_type = inline.g("mime_type").str();
    }
    inline_data_from(&mime_type, &inline.g("data").str())
}

fn inline_data_from(mime_type: &str, data: &str) -> Option<Value> {
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(json!({"inlineData": {"mimeType": mime_type, "data": data}}))
}

fn file_data_part(file_data: &Res<'_>) -> Option<Value> {
    let mut mime_type = file_data.g("mimeType").str();
    if mime_type.is_empty() {
        mime_type = file_data.g("mime_type").str();
    }
    let mut file_uri = file_data.g("fileUri").str();
    if file_uri.is_empty() {
        file_uri = file_data.g("file_uri").str();
    }
    file_data_from(&mime_type, &file_uri)
}

fn file_data_from(mime_type: &str, file_uri: &str) -> Option<Value> {
    if mime_type.is_empty() || file_uri.is_empty() {
        return None;
    }
    Some(json!({"fileData": {"mimeType": mime_type, "fileUri": file_uri}}))
}

/// `data:<mime>;base64,<payload>` as inlineData; anything else is `None`.
fn inline_data_part_from_data_url(data_url: &str) -> Option<Value> {
    let payload = data_url.strip_prefix("data:")?;
    let (mime, rest) = payload.split_once(';')?;
    let data = rest.strip_prefix("base64,")?;
    inline_data_from(mime, data)
}

fn append_text_content(items: &mut Vec<Value>, role: &str, text: &str) {
    items.push(content(content_role(role, "user"), vec![text_part(text, false)]));
}

fn content(role: &str, parts: Vec<Value>) -> Value {
    json!({"role": role, "parts": parts})
}

fn content_role<'a>(role: &str, default_role: &'a str) -> &'a str {
    match role.trim().to_lowercase().as_str() {
        "model" | "assistant" => "model",
        "user" => "user",
        _ if default_role == "model" => "model",
        _ => "user",
    }
}

fn input_audio_mime_type(format: &str) -> &'static str {
    match format.trim().to_lowercase().as_str() {
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "opus" => "audio/opus",
        "pcm16" => "audio/pcm",
        // "mp3" and unknown formats
        _ => "audio/mpeg",
    }
}

/// `auto` shows thought summaries, `none` hides them; anything else is not a selector.
fn thinking_summaries_include_thoughts(summary: &Res<'_>) -> Option<bool> {
    if !summary.is_string() {
        return None;
    }
    match summary.str().trim().to_lowercase().as_str() {
        "auto" => Some(true),
        "none" => Some(false),
        _ => None,
    }
}

/// Rebuilds a value with camelCase keys through sjson-style path writes (so, like Go, empty
/// containers vanish and every object member inside an array becomes its own array element).
fn convert_snake_case_keys_to_camel_case(raw: &Value) -> Value {
    let mut out = json!({});
    copy_snake_case_value(&mut out, "", raw);
    out
}

fn copy_snake_case_value(out: &mut Value, path: &str, node: &Value) {
    match node {
        Value::Object(map) => {
            for (key, value) in map {
                let camel = to_camel_case(key);
                let child_path = if path.is_empty() { camel } else { format!("{path}.{camel}") };
                copy_snake_case_value(out, &child_path, value);
            }
        }
        Value::Array(items) => {
            for value in items {
                copy_snake_case_value(out, &format!("{path}.-1"), value);
            }
        }
        _ => {
            if !path.is_empty() {
                cpa_json::set(out, path, node.clone());
            }
        }
    }
}

fn to_camel_case(s: &str) -> String {
    let mut parts = s.split('_');
    let mut out = parts.next().unwrap_or_default().to_string();
    for part in parts {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}
