//! Interactions request -> Codex Responses request (Go: interactions_codex_request.go).

use cpa_core::thinking;
use cpa_json::{J, Res, Value, json};

use crate::codex::util::{
    file_name_from_mime, input_audio_format_from_mime, shorten_name_if_needed,
};
use cpa_json::raw_at;

/// Source bytes plus the output item list being built. `path` arguments below are gjson-style
/// paths into `src`, used to copy client values verbatim where Go uses `Raw` in a string.
struct Ctx<'a> {
    src: &'a [u8],
    items: Vec<Value>,
}

/// Go: ConvertInteractionsRequestToCodex.
pub fn convert_interactions_request_to_codex(
    model_name: &str,
    input_raw_json: &[u8],
    stream: bool,
) -> Vec<u8> {
    let root = cpa_json::parse(input_raw_json);
    let mut out = cpa_json::parse_str(r#"{"model":"","instructions":"","input":[]}"#);
    cpa_json::set(&mut out, "model", model_name);
    if stream || root.g("stream").bool() {
        cpa_json::set(&mut out, "stream", true);
    }
    copy_system(&mut out, &root);
    copy_generation_config(&mut out, &root);
    let mut cx = Ctx {
        src: input_raw_json,
        items: Vec::new(),
    };
    append_input(&mut cx, &root.g("input"), "input");
    if !cx.items.is_empty() {
        cpa_json::set(&mut out, "input", Value::Array(cx.items));
    }
    copy_tools(&mut out, &root);
    copy_top_level(&mut out, &root);
    cpa_json::to_vec(&out)
}

fn copy_system(out: &mut Value, root: &Value) {
    let mut system = root.g("system_instruction");
    if !system.exists() {
        system = root.g("systemInstruction");
    }
    if !system.exists() {
        return;
    }
    if system.is_string() {
        cpa_json::set(out, "instructions", system.str());
        return;
    }
    let text = system.g("text");
    if text.exists() && text.is_string() {
        cpa_json::set(out, "instructions", text.str());
        return;
    }
    let parts = system.g("parts");
    if parts.exists() && parts.is_array() {
        let joined = join_texts(parts.array().iter().map(|p| p.g("text").str()));
        if !joined.is_empty() {
            cpa_json::set(out, "instructions", joined);
        }
    }
}

/// Non-empty texts joined with newlines.
fn join_texts(texts: impl Iterator<Item = String>) -> String {
    texts
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn copy_generation_config(out: &mut Value, root: &Value) {
    let mut cfg = root.g("generation_config");
    if !cfg.exists() {
        cfg = root.g("generationConfig");
    }
    if !cfg.exists() {
        let reasoning = root.g("reasoning");
        if reasoning.exists() {
            cpa_json::set(out, "reasoning", reasoning.value());
        }
        return;
    }
    let reasoning = cfg.g("reasoning");
    if reasoning.exists() {
        cpa_json::set(out, "reasoning", reasoning.value());
    }
    if let Some(effort) = reasoning_effort(&cfg) {
        cpa_json::set(out, "reasoning.effort", effort);
    }
    if let Some(summary) = reasoning_summary(&cfg) {
        cpa_json::set(out, "reasoning.summary", summary);
    }
    // `text` precedes `verbosity` so the nested verbosity survives (Go iterates a map).
    const COPY_PATHS: [(&str, &str); 21] = [
        ("max_output_tokens", "max_output_tokens"),
        ("maxOutputTokens", "max_output_tokens"),
        ("max_tokens", "max_output_tokens"),
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("topP", "top_p"),
        ("presence_penalty", "presence_penalty"),
        ("presencePenalty", "presence_penalty"),
        ("frequency_penalty", "frequency_penalty"),
        ("frequencyPenalty", "frequency_penalty"),
        ("parallel_tool_calls", "parallel_tool_calls"),
        ("parallelToolCalls", "parallel_tool_calls"),
        ("response_format", "response_format"),
        ("responseFormat", "response_format"),
        ("text", "text"),
        ("verbosity", "text.verbosity"),
        ("truncation", "truncation"),
        ("tool_choice", "tool_choice"),
        ("toolChoice", "tool_choice"),
        ("service_tier", "service_tier"),
        ("serviceTier", "service_tier"),
    ];
    for (source, target) in COPY_PATHS {
        let value = cfg.g(source);
        if value.exists() {
            cpa_json::set(out, target, value.value());
        }
    }
}

fn reasoning_effort(cfg: &Res<'_>) -> Option<String> {
    for path in [
        "thinking_level",
        "thinkingLevel",
        "thinking_config.thinking_level",
        "thinking_config.thinkingLevel",
        "thinkingConfig.thinking_level",
        "thinkingConfig.thinkingLevel",
        "reasoning.effort",
    ] {
        let value = cfg.g(path);
        if value.exists() {
            let effort = value.str().trim().to_lowercase();
            if !effort.is_empty() {
                return Some(effort);
            }
        }
    }
    for path in [
        "thinking_budget",
        "thinkingBudget",
        "thinking_config.thinking_budget",
        "thinking_config.thinkingBudget",
        "thinkingConfig.thinking_budget",
        "thinkingConfig.thinkingBudget",
    ] {
        let value = cfg.g(path);
        if value.exists()
            && let Some(effort) = thinking::convert_budget_to_level(value.int())
        {
            return Some(effort.to_string());
        }
    }
    None
}

fn reasoning_summary(cfg: &Res<'_>) -> Option<&'static str> {
    for path in [
        "thinking_summaries",
        "thinkingSummaries",
        "reasoning.summary",
    ] {
        if let Some(s) = cfg.g(path).as_str() {
            match s.trim().to_lowercase().as_str() {
                "auto" => return Some("auto"),
                "none" => return Some("none"),
                _ => {}
            }
        }
    }
    for path in [
        "include_thoughts",
        "includeThoughts",
        "thinking_config.include_thoughts",
        "thinking_config.includeThoughts",
        "thinkingConfig.include_thoughts",
        "thinkingConfig.includeThoughts",
    ] {
        match cfg.g(path).v() {
            Some(Value::Bool(true)) => return Some("auto"),
            Some(Value::Bool(false)) => return Some("none"),
            _ => {}
        }
    }
    None
}

fn append_input(cx: &mut Ctx<'_>, input: &Res<'_>, path: &str) {
    if !input.exists() {
        return;
    }
    if input.is_string() {
        append_text(cx, "user", &input.str());
        return;
    }
    if input.is_array() {
        for (i, step) in input.array().iter().enumerate() {
            append_step(cx, step, "user", &format!("{path}.{i}"));
        }
        return;
    }
    let steps = input.g("steps");
    if steps.exists() && steps.is_array() {
        let default_role = default_role(&input.g("role").str(), "user");
        for (i, step) in steps.array().iter().enumerate() {
            append_step(cx, step, default_role, &format!("{path}.steps.{i}"));
        }
        return;
    }
    append_step(cx, input, "user", path);
}

fn append_step(cx: &mut Ctx<'_>, step: &Res<'_>, default_role_name: &str, path: &str) {
    if step.is_string() {
        append_text(cx, default_role_name, &step.str());
        return;
    }
    let steps = step.g("steps");
    if steps.exists() && steps.is_array() {
        let role = default_role(&step.g("role").str(), default_role_name);
        for (i, nested) in steps.array().iter().enumerate() {
            append_step(cx, nested, role, &format!("{path}.steps.{i}"));
        }
        return;
    }
    let step_type = step.g("type").str().trim().to_lowercase();
    match step_type.as_str() {
        "function_call" => append_function_call(cx, step, path),
        "function_result" | "function_call_output" => append_function_result(cx, step, path),
        "model_output" | "assistant" => append_content_item(cx, &step.g("content"), "assistant"),
        "thought" | "reasoning" => append_thought(cx, step),
        // "user_input", "message", "" and unknown types share the message handling.
        _ => {
            let role = default_role(&step.g("role").str(), default_role_name);
            let content = step.g("content");
            if content.exists() {
                append_content_item(cx, &content, role);
            } else {
                let text = step.g("text");
                if text.exists() {
                    append_text(cx, role, &text.str());
                }
            }
        }
    }
}

fn append_content_item(cx: &mut Ctx<'_>, content: &Res<'_>, role: &str) {
    if !content.exists() {
        return;
    }
    if content.is_string() {
        append_text(cx, role, &content.str());
        return;
    }
    if content.is_array() {
        for part in content.array() {
            if let Some(item) = message_part(&part, role) {
                append_message_part(cx, role, item);
            }
        }
        return;
    }
    if content.is_object()
        && let Some(item) = message_part(content, role)
    {
        append_message_part(cx, role, item);
    }
}

fn append_function_call(cx: &mut Ctx<'_>, step: &Res<'_>, path: &str) {
    let mut item = json!({"type": "function_call"});
    let name = step.g("name");
    if name.exists() {
        cpa_json::set(&mut item, "name", shorten_name_if_needed(&name.str()));
    }
    let call_id = call_id(step);
    if !call_id.is_empty() {
        cpa_json::set(&mut item, "call_id", call_id);
    }
    let args = step.g("arguments");
    if args.exists() {
        let s = json_string(cx.src, &args, &format!("{path}.arguments"));
        cpa_json::set(&mut item, "arguments", s);
    } else {
        let args = step.g("args");
        if args.exists() {
            let s = json_string(cx.src, &args, &format!("{path}.args"));
            cpa_json::set(&mut item, "arguments", s);
        }
    }
    cx.items.push(item);
}

fn append_function_result(cx: &mut Ctx<'_>, step: &Res<'_>, path: &str) {
    let mut item = json!({"type": "function_call_output"});
    let call_id = call_id(step);
    if !call_id.is_empty() {
        cpa_json::set(&mut item, "call_id", call_id);
    }
    let result = step.g("result");
    if result.exists() {
        let s = output_string(cx.src, &result, &format!("{path}.result"));
        cpa_json::set(&mut item, "output", s);
    } else {
        let output = step.g("output");
        if output.exists() {
            let s = output_string(cx.src, &output, &format!("{path}.output"));
            cpa_json::set(&mut item, "output", s);
        }
    }
    cx.items.push(item);
}

fn copy_tools(out: &mut Value, root: &Value) {
    let tools = root.g("tools");
    if !tools.exists() {
        return;
    }
    if !tools.is_array() {
        cpa_json::set(out, "tools", tools.value());
        return;
    }
    let mut normalized: Vec<Value> = Vec::new();
    for tool in tools.array() {
        let decls = tool.g("function_declarations");
        if decls.exists() {
            append_tool_declarations(&mut normalized, &decls);
            continue;
        }
        let decls = tool.g("functionDeclarations");
        if decls.exists() {
            append_tool_declarations(&mut normalized, &decls);
            continue;
        }
        if tool.g("name").exists() {
            normalized.push(tool_from_declaration(&tool));
        }
    }
    if normalized.is_empty() {
        cpa_json::set(out, "tools", tools.value());
        return;
    }
    cpa_json::set(out, "tools", Value::Array(normalized));
    if !out.g("tool_choice").exists() {
        cpa_json::set(out, "tool_choice", "auto");
    }
}

fn copy_top_level(out: &mut Value, root: &Value) {
    let tier = root.g("service_tier");
    if let Some(s) = tier.as_str()
        && matches!(s.trim().to_lowercase().as_str(), "priority" | "fast")
        && out.g("service_tier").as_str() != Some("priority")
    {
        cpa_json::set(out, "service_tier", "priority");
    }
    let tool_choice = root.g("tool_choice");
    if tool_choice.exists() {
        set_if_different(out, "tool_choice", &tool_choice);
    }
    for path in [
        "parallel_tool_calls",
        "store",
        "metadata",
        "include",
        "truncation",
    ] {
        let value = root.g(path);
        if value.exists() {
            set_if_different(out, path, &value);
        }
    }
}

fn set_if_different(out: &mut Value, path: &str, value: &Res<'_>) {
    let current = out.g(path);
    if current.exists() && current.v() == value.v() {
        return;
    }
    cpa_json::set(out, path, value.value());
}

fn append_thought(cx: &mut Ctx<'_>, step: &Res<'_>) {
    let mut text = content_text(&step.g("content"));
    if text.is_empty() {
        text = step.g("text").str();
    }
    let mut item = json!({"type": "reasoning"});
    if !text.is_empty() {
        cpa_json::set(&mut item, "content", text);
    }
    let id = step.g("id");
    if id.exists() {
        cpa_json::set(&mut item, "id", id.str());
    }
    cx.items.push(item);
}

fn append_text(cx: &mut Ctx<'_>, role: &str, text: &str) {
    let part_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    append_message_part(cx, role, json!({"type": part_type, "text": text}));
}

fn append_message_part(cx: &mut Ctx<'_>, role: &str, part: Value) {
    cx.items
        .push(json!({"type": "message", "role": role, "content": [part]}));
}

fn message_part(part: &Res<'_>, role: &str) -> Option<Value> {
    let text = part.g("text");
    if text.exists() {
        let part_type = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        return Some(json!({"type": part_type, "text": text.str()}));
    }
    match part.g("type").str().trim().to_lowercase().as_str() {
        "text" | "" => None,
        "image" => image_part(part),
        "image_url" => {
            Some(json!({"type": "input_image", "image_url": part.g("image_url.url").str()}))
        }
        "audio" => audio_part(part),
        "input_audio" => {
            let mut item = json!({"type": "input_audio", "input_audio": {}});
            let audio = part.g("input_audio");
            if audio.exists() {
                cpa_json::set(&mut item, "input_audio", audio.value());
            }
            Some(item)
        }
        "video" | "document" | "file" => file_part(part),
        _ => {
            for key in ["inline_data", "inlineData"] {
                let inline = part.g(key);
                if inline.exists() {
                    return inline_part(&inline);
                }
            }
            for key in ["file_data", "fileData"] {
                let file = part.g(key);
                if file.exists() {
                    return file_data_part(&file);
                }
            }
            None
        }
    }
}

fn image_part(part: &Res<'_>) -> Option<Value> {
    let url = part.g("url");
    if url.exists() {
        return Some(json!({"type": "input_image", "image_url": url.str()}));
    }
    let file_uri = first_string(part, &["file_uri", "fileUri"]);
    if !file_uri.is_empty() {
        return Some(json!({"type": "input_image", "image_url": file_uri}));
    }
    let mime_type = first_string(part, &["mime_type", "mimeType"]);
    let data = part.g("data").str();
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(json!({"type": "input_image", "image_url": format!("data:{mime_type};base64,{data}")}))
}

fn audio_part(part: &Res<'_>) -> Option<Value> {
    let mime_type = first_string(part, &["mime_type", "mimeType"]);
    let data = part.g("data").str();
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(
        json!({"type": "input_audio", "input_audio": {"data": data, "format": input_audio_format_from_mime(&mime_type)}}),
    )
}

fn file_part(part: &Res<'_>) -> Option<Value> {
    let file_data = part.g("file.file_data").str();
    if !file_data.is_empty() {
        return Some(
            json!({"type": "input_file", "file_data": file_data, "filename": part.g("file.filename").str()}),
        );
    }
    let mime_type = first_string(part, &["mime_type", "mimeType"]);
    let file_uri = first_string(part, &["file_uri", "fileUri", "url"]);
    if !file_uri.is_empty() {
        return Some(
            json!({"type": "input_file", "file_url": file_uri, "filename": file_name_from_mime(&mime_type)}),
        );
    }
    let data = part.g("data").str();
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some(
        json!({"type": "input_file", "file_data": data, "filename": file_name_from_mime(&mime_type)}),
    )
}

fn inline_part(inline: &Res<'_>) -> Option<Value> {
    let mime_type = first_string(inline, &["mime_type", "mimeType"]);
    let data = inline.g("data").str();
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    let simple = Value::Object(
        [
            ("mime_type".to_string(), Value::String(mime_type.clone())),
            ("data".to_string(), Value::String(data)),
        ]
        .into_iter()
        .collect(),
    );
    let simple = Res::of(&simple);
    let lower = mime_type.to_lowercase();
    if lower.starts_with("image/") {
        image_part(&simple)
    } else if lower.starts_with("audio/") {
        audio_part(&simple)
    } else {
        file_part(&simple)
    }
}

fn file_data_part(file: &Res<'_>) -> Option<Value> {
    let mime_type = first_string(file, &["mime_type", "mimeType"]);
    let file_uri = first_string(file, &["file_uri", "fileUri"]);
    if file_uri.is_empty() {
        return None;
    }
    if mime_type.to_lowercase().starts_with("image/") {
        return Some(json!({"type": "input_image", "image_url": file_uri}));
    }
    Some(
        json!({"type": "input_file", "file_url": file_uri, "filename": file_name_from_mime(&mime_type)}),
    )
}

fn append_tool_declarations(normalized: &mut Vec<Value>, declarations: &Res<'_>) {
    if !declarations.is_array() {
        return;
    }
    for declaration in declarations.array() {
        if declaration.g("name").exists() {
            normalized.push(tool_from_declaration(&declaration));
        }
    }
}

/// Keys are in the sorted order Go's map marshaling produces.
fn tool_from_declaration(declaration: &Res<'_>) -> Value {
    let mut tool = cpa_json::parse_str("{}");
    let desc = declaration.g("description");
    if desc.exists() {
        cpa_json::set(&mut tool, "description", desc.str());
    }
    cpa_json::set(
        &mut tool,
        "name",
        shorten_name_if_needed(&declaration.g("name").str()),
    );
    let mut params = declaration.g("parameters");
    if !params.exists() {
        params = declaration.g("parametersJsonSchema");
    }
    if !params.exists() {
        params = declaration.g("parameters_json_schema");
    }
    if params.exists() {
        cpa_json::set(&mut tool, "parameters", cleaned_parameters(&params));
    }
    cpa_json::set(&mut tool, "strict", false);
    cpa_json::set(&mut tool, "type", "function");
    tool
}

fn cleaned_parameters(params: &Res<'_>) -> Value {
    let mut cleaned = params.value();
    if params.g("$schema").exists() {
        cpa_json::delete(&mut cleaned, "$schema");
    }
    if params.g("additionalProperties").v() != Some(&Value::Bool(false)) {
        cpa_json::set(&mut cleaned, "additionalProperties", false);
    }
    cleaned
}

fn content_text(content: &Res<'_>) -> String {
    if !content.exists() {
        return String::new();
    }
    if content.is_string() {
        return content.str();
    }
    if content.is_object() {
        return content.g("text").str();
    }
    if content.is_array() {
        return join_texts(content.array().iter().map(|p| p.g("text").str()));
    }
    String::new()
}

fn call_id(step: &Res<'_>) -> String {
    let id = step.g("call_id").str().trim().to_string();
    if !id.is_empty() {
        return id;
    }
    step.g("id").str().trim().to_string()
}

/// A string value verbatim, otherwise the source text of the value (`{}` when absent).
fn json_string(src: &[u8], value: &Res<'_>, path: &str) -> String {
    if value.is_string() {
        return value.str();
    }
    if value.exists() {
        return raw_at(src, path).map_or_else(|| value.raw(), str::to_string);
    }
    "{}".into()
}

fn output_string(src: &[u8], value: &Res<'_>, path: &str) -> String {
    if value.is_string() {
        return value.str();
    }
    if value.exists() {
        return raw_at(src, path).map_or_else(|| value.raw(), str::to_string);
    }
    String::new()
}

fn default_role(role: &str, fallback: &str) -> &'static str {
    match role.trim().to_lowercase().as_str() {
        "model" | "assistant" => "assistant",
        "developer" | "system" => "developer",
        "user" => "user",
        _ if fallback == "assistant" => "assistant",
        _ if fallback == "developer" => "developer",
        _ => "user",
    }
}

fn first_string(root: &Res<'_>, paths: &[&str]) -> String {
    for path in paths {
        let value = root.g(path);
        if value.exists() {
            return value.str();
        }
    }
    String::new()
}
