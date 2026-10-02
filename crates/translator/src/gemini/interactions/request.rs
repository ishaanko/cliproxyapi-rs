//! Request converters between Interactions and Gemini (Go: interactions_gemini_common.go).

use cpa_core::util::go_json_canonicalize;
use cpa_json::{json, Map, Res, Value, J};

use super::shared::{
    first_trimmed, gemini_content, gemini_file_data_part_json, gemini_inline_data_part_json,
    gemini_inline_data_to_interactions_content, gemini_part_to_interactions_steps, gemini_text_part_json,
    interactions_content_part_to_gemini_part,
};
use crate::common::{contains_json_ref, reorder_gemini_user_parts, set_gemini_function_response_result};

/// Converts an Interactions request into a Gemini request.
pub fn convert_interactions_request_to_gemini(model_name: &str, raw: &[u8], _stream: bool) -> Vec<u8> {
    let root = cpa_json::parse(raw);
    let mut out = json!({ "model": "", "contents": [] });
    if !model_name.is_empty() && root.g("model").exists() {
        cpa_json::set(&mut out, "model", model_name);
    }
    copy_interactions_system_instruction(&mut out, &root);
    copy_interactions_generation_config(&mut out, &root);
    copy_interactions_response_modalities(&mut out, &root);
    copy_interactions_tools(&mut out, &root);
    copy_interactions_tool_choice(&mut out, &root);
    copy_interactions_service_tier(&mut out, &root);
    let mut ctx = InputContext::new();
    append_interactions_input(&mut ctx, &root.g("input"), cpa_json::raw_at(raw, "input"));
    // SetRawArrayItems is a no-op for an empty list.
    if !ctx.items.is_empty() {
        cpa_json::set(&mut out, "contents", Value::Array(ctx.items));
    }
    cpa_json::to_vec(&out)
}

/// Converts a Gemini request into an Interactions request.
pub fn convert_gemini_request_to_interactions(model_name: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    let root = cpa_json::parse(raw);
    let mut out = json!({ "model": "", "input": [] });
    cpa_json::set(&mut out, "model", model_name);
    copy_gemini_system_instruction_to_interactions(&mut out, &root);
    let generation_config = root.g("generationConfig");
    if generation_config.exists() {
        let converted = convert_camel_case_keys_to_snake_case(&generation_config.value());
        cpa_json::set(&mut out, "generation_config", converted);
        normalize_gemini_thinking_config_for_interactions(&mut out);
    }
    copy_gemini_tools_to_interactions(&mut out, &root);
    let mut input_items: Vec<Value> = Vec::new();
    root.g("contents").for_each(|_, content| {
        let role = content.g("role").str();
        let step_type = if role == "model" { "model_output" } else { "user_input" };
        content.g("parts").for_each(|_, part| {
            let part = part.value();
            let text = part.g("text");
            if part.g("functionCall").exists()
                || part.g("functionResponse").exists()
                || (text.exists() && text.str().is_empty())
            {
                input_items.extend(gemini_part_to_interactions_steps(&part));
                return true;
            }
            let Some(item) = gemini_part_to_interactions_content(&part) else { return true };
            let current_step_type = if part.g("thought").bool() && role == "model" { "thought" } else { step_type };
            input_items.push(json!({ "type": current_step_type, "content": [item] }));
            true
        });
        true
    });
    if !input_items.is_empty() {
        cpa_json::set(&mut out, "input", Value::Array(input_items));
    }
    cpa_json::set(&mut out, "stream", stream);
    cpa_json::to_vec(&out)
}

// ------------------------------------------------------------------ gemini -> interactions

fn copy_gemini_system_instruction_to_interactions(out: &mut Value, root: &Value) {
    let mut sys = root.g("systemInstruction");
    if !sys.exists() {
        sys = root.g("system_instruction");
    }
    let text = gemini_system_instruction_text(&sys);
    if text.is_empty() {
        return;
    }
    cpa_json::set(out, "system_instruction", text);
}

fn gemini_system_instruction_text(sys: &Res<'_>) -> String {
    if !sys.exists() {
        return String::new();
    }
    if let Some(s) = sys.as_str() {
        return s.to_string();
    }
    let text = sys.g("text");
    if let Some(s) = text.as_str() {
        return s.to_string();
    }
    let parts = sys.g("parts");
    if !parts.exists() || !parts.is_array() {
        return String::new();
    }
    let texts: Vec<String> = parts.array().iter().map(|p| p.g("text").str()).filter(|t| !t.is_empty()).collect();
    texts.join("\n")
}

/// First existing value among `paths` in `root`.
fn first_existing_path<'a>(root: &'a Value, paths: &[&str]) -> Res<'a> {
    for path in paths {
        let value = root.g(path);
        if value.exists() {
            return value;
        }
    }
    Res::NONE
}

fn normalize_gemini_thinking_config_for_interactions(out: &mut Value) {
    let level = first_existing_path(
        out,
        &[
            "generation_config.thinking_config.thinking_level",
            "generation_config.thinkingConfig.thinkingLevel",
            "generation_config.thinkingConfig.thinking_level",
        ],
    )
    .into_value();
    if let Some(level) = level {
        let level = Res::owned(level).str().trim().to_lowercase();
        cpa_json::set(out, "generation_config.thinking_level", level);
    }
    let budget = first_existing_path(
        out,
        &[
            "generation_config.thinking_config.thinking_budget",
            "generation_config.thinkingConfig.thinkingBudget",
            "generation_config.thinkingConfig.thinking_budget",
        ],
    )
    .into_value();
    if let Some(budget) = budget {
        cpa_json::set(out, "generation_config.thinking_budget", budget);
    }
    if !out.g("generation_config.thinking_summaries").exists() {
        let include = first_existing_path(
            out,
            &[
                "generation_config.thinking_config.include_thoughts",
                "generation_config.thinking_config.includeThoughts",
                "generation_config.thinkingConfig.include_thoughts",
                "generation_config.thinkingConfig.includeThoughts",
            ],
        )
        .into_value();
        if let Some(include) = include {
            let summary = if Res::owned(include).bool() { "auto" } else { "none" };
            cpa_json::set(out, "generation_config.thinking_summaries", summary);
        }
    }
}

/// One normalized tool entry: `{"type": kind, <kind>: raw?}` for url_context, code_execution and
/// google_search tools; the raw value is kept only for a non-empty object.
fn gemini_builtin_tool_entry(kind: &str, value: &Res<'_>) -> Value {
    let mut entry = Map::new();
    entry.insert("type".into(), Value::String(kind.into()));
    if value.is_object() && value.v().and_then(Value::as_object).is_some_and(|m| !m.is_empty()) {
        entry.insert(kind.into(), value.value());
    }
    Value::Object(entry)
}

/// Function declaration entry with sorted keys, as Go marshals `map[string]any`.
fn gemini_function_entry(decl: &Res<'_>) -> Value {
    let mut entry = Map::new();
    if let Some(desc) = decl.g("description").into_value() {
        entry.insert("description".into(), Value::String(Res::owned(desc).str()));
    }
    entry.insert("name".into(), Value::String(decl.g("name").str()));
    let params = decl.g("parameters");
    let params = if params.exists() { params } else { decl.g("parametersJsonSchema") };
    if let Some(params) = params.into_value() {
        entry.insert("parameters".into(), params);
    }
    entry.insert("type".into(), Value::String("function".into()));
    Value::Object(entry)
}

fn copy_gemini_tools_to_interactions(out: &mut Value, root: &Value) {
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
        for (camel, snake, kind) in [
            ("urlContext", "url_context", "url_context"),
            ("codeExecution", "code_execution", "code_execution"),
            ("googleSearch", "google_search", "google_search"),
        ] {
            let camel_value = tool.g(camel);
            if camel_value.exists() {
                normalized.push(gemini_builtin_tool_entry(kind, &camel_value));
            } else {
                let snake_value = tool.g(snake);
                if snake_value.exists() {
                    normalized.push(gemini_builtin_tool_entry(kind, &snake_value));
                }
            }
        }
        if tool.g("name").exists() {
            normalized.push(gemini_function_entry(&tool));
            continue;
        }
        let decls_camel = tool.g("functionDeclarations");
        let decls = if decls_camel.exists() { decls_camel } else { tool.g("function_declarations") };
        decls.for_each(|_, decl| {
            if decl.g("name").exists() {
                normalized.push(gemini_function_entry(&decl));
            }
            true
        });
    }
    if normalized.is_empty() {
        cpa_json::set(out, "tools", tools.value());
        return;
    }
    cpa_json::set(out, "tools", Value::Array(normalized));
}

fn gemini_part_to_interactions_content(part: &Value) -> Option<Value> {
    let text = part.g("text");
    if text.exists() {
        return Some(json!({ "type": "text", "text": text.str() }));
    }
    let inline = part.g("inlineData");
    if inline.exists() {
        let mut mime_type = inline.g("mimeType").str();
        if mime_type.is_empty() {
            mime_type = inline.g("mime_type").str();
        }
        return Some(gemini_inline_data_to_interactions_content(&mime_type, &inline.g("data").str()));
    }
    let inline = part.g("inline_data");
    if inline.exists() {
        return Some(gemini_inline_data_to_interactions_content(
            &inline.g("mime_type").str(),
            &inline.g("data").str(),
        ));
    }
    None
}

// ------------------------------------------------------------------ interactions -> gemini

fn copy_interactions_system_instruction(out: &mut Value, root: &Value) {
    let sys = root.g("system_instruction");
    if !sys.exists() {
        return;
    }
    if let Some(s) = sys.as_str() {
        cpa_json::set(out, "systemInstruction", json!({ "parts": [{ "text": s }] }));
        return;
    }
    let text = sys.g("text");
    if text.exists() && !sys.g("parts").exists() {
        cpa_json::set(out, "systemInstruction", json!({ "parts": [{ "text": text.str() }] }));
        return;
    }
    cpa_json::set(out, "systemInstruction", sys.value());
}

fn copy_interactions_generation_config(out: &mut Value, root: &Value) {
    let cfg = root.g("generation_config");
    if !cfg.exists() {
        let cfg = root.g("generationConfig");
        if !cfg.exists() {
            return;
        }
        cpa_json::set(out, "generationConfig", cfg.value());
        normalize_interactions_generation_config(out);
        return;
    }
    let converted = convert_snake_case_keys_to_camel_case(&cfg.value());
    cpa_json::set(out, "generationConfig", converted);
    normalize_interactions_generation_config(out);
}

fn normalize_interactions_generation_config(out: &mut Value) {
    if out.g("generationConfig.toolChoice").exists() {
        cpa_json::delete(out, "generationConfig.toolChoice");
    }
    for (from, to) in [
        ("generationConfig.thinkingLevel", "generationConfig.thinkingConfig.thinkingLevel"),
        ("generationConfig.thinkingBudget", "generationConfig.thinkingConfig.thinkingBudget"),
        ("generationConfig.includeThoughts", "generationConfig.thinkingConfig.includeThoughts"),
    ] {
        if let Some(value) = out.g(from).into_value() {
            cpa_json::set(out, to, value);
            cpa_json::delete(out, from);
        }
    }
    if let Some(summaries) = out.g("generationConfig.thinkingSummaries").into_value() {
        if let Some(include_thoughts) = thinking_summaries_include_thoughts(&summaries) {
            cpa_json::set(out, "generationConfig.thinkingConfig.includeThoughts", include_thoughts);
        }
        cpa_json::delete(out, "generationConfig.thinkingSummaries");
    }
}

/// `auto` -> true, `none` -> false; anything else (or a non-string) is unmapped.
fn thinking_summaries_include_thoughts(summary: &Value) -> Option<bool> {
    match summary.as_str()?.trim().to_lowercase().as_str() {
        "auto" => Some(true),
        "none" => Some(false),
        _ => None,
    }
}

fn copy_interactions_response_modalities(out: &mut Value, root: &Value) {
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
        cpa_json::set(out, "generationConfig.responseModalities", json!(response_mods));
    }
}

fn copy_interactions_tool_choice(out: &mut Value, root: &Value) {
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
    if let Some(s) = tool_choice.as_str() {
        match s.trim().to_lowercase().as_str() {
            "none" => mode = "NONE",
            "auto" => mode = "AUTO",
            "required" | "any" => mode = "ANY",
            _ => {}
        }
    } else if tool_choice.is_object() {
        match tool_choice.g("type").str().trim().to_lowercase().as_str() {
            "none" => mode = "NONE",
            "auto" => mode = "AUTO",
            "required" | "any" => mode = "ANY",
            "function" => {
                mode = "ANY";
                let name = tool_choice.g("function.name").str().trim().to_string();
                if !name.is_empty() {
                    allowed_names.push(name);
                }
            }
            "tool" => {
                mode = "ANY";
                let name = tool_choice.g("name").str().trim().to_string();
                if !name.is_empty() {
                    allowed_names.push(name);
                }
            }
            _ => {}
        }
    }
    if mode.is_empty() {
        return;
    }
    cpa_json::set(out, "toolConfig.functionCallingConfig.mode", mode);
    if !allowed_names.is_empty() {
        cpa_json::set(out, "toolConfig.functionCallingConfig.allowedFunctionNames", json!(allowed_names));
    }
}

fn copy_interactions_service_tier(out: &mut Value, root: &Value) {
    let service_tier = root.g("service_tier");
    if let Some(s) = service_tier.as_str() {
        cpa_json::set(out, "service_tier", s);
    }
}

// ------------------------------------------------------------------ key case conversion

/// Rebuilds `node` with keys converted by `convert`, the way Go copies leaves through sjson
/// paths: empty containers vanish, keys containing dots split into nested paths, and a scalar
/// root yields `{}`.
fn convert_keys(node: &Value, convert: fn(&str) -> String) -> Value {
    let mut out = json!({});
    copy_value_with_converted_keys(&mut out, "", node, convert);
    out
}

fn copy_value_with_converted_keys(out: &mut Value, path: &str, node: &Value, convert: fn(&str) -> String) {
    match node {
        Value::Object(map) => {
            for (key, value) in map {
                let child_path = join_json_path(path, &convert(key));
                copy_value_with_converted_keys(out, &child_path, value, convert);
            }
        }
        Value::Array(items) => {
            for value in items {
                let child_path = format!("{path}.-1");
                copy_value_with_converted_keys(out, &child_path, value, convert);
            }
        }
        // sjson rejects an empty path, leaving the output untouched.
        leaf => {
            if !path.is_empty() {
                cpa_json::set(out, path, leaf.clone());
            }
        }
    }
}

fn convert_snake_case_keys_to_camel_case(node: &Value) -> Value {
    convert_keys(node, to_camel_case)
}

fn convert_camel_case_keys_to_snake_case(node: &Value) -> Value {
    convert_keys(node, to_snake_case)
}

fn join_json_path(path: &str, key: &str) -> String {
    if path.is_empty() { key.to_string() } else { format!("{path}.{key}") }
}

fn to_camel_case(s: &str) -> String {
    let mut parts = s.split('_');
    let mut out = parts.next().unwrap_or_default().to_string();
    for p in parts {
        let mut chars = p.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

fn to_snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && c.is_ascii_uppercase() {
            out.push('_');
        }
        out.push(c);
    }
    out.to_lowercase()
}

// ------------------------------------------------------------------ tools

/// Wrapper entry for `url_context` / `code_execution` / `google_search`: an object value is kept,
/// otherwise `{}`.
fn builtin_entry(tool: &Value, key: &str, camel_key: &str) -> Value {
    let snake = tool.g(key);
    if snake.exists() && snake.is_object() {
        return snake.value();
    }
    let camel = tool.g(camel_key);
    if camel.exists() && camel.is_object() {
        return camel.value();
    }
    json!({})
}

fn copy_interactions_tools(out: &mut Value, root: &Value) {
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
        let tool = tool.value();
        if tool.g("functionDeclarations").exists() {
            // Already Gemini-shaped: keep the tools untouched.
            cpa_json::set(out, "tools", tools.value());
            return;
        }
        let tool_type = tool.g("type").str();
        let entry: Option<Value> = match tool_type.as_str() {
            "url_context" => Some(json!({ "urlContext": builtin_entry(&tool, "url_context", "urlContext") })),
            "code_execution" => Some(json!({ "codeExecution": builtin_entry(&tool, "code_execution", "codeExecution") })),
            "google_search" | "web_search" => {
                Some(json!({ "googleSearch": builtin_entry(&tool, "google_search", "googleSearch") }))
            }
            _ => {
                let decls = tool.g("function_declarations");
                let name = tool.g("name");
                if decls.exists() && decls.is_array() {
                    Some(json!({ "functionDeclarations": decls.value() }))
                } else if name.exists() {
                    // Sorted keys, as Go marshals `map[string]any`.
                    let mut decl = Map::new();
                    let desc = tool.g("description");
                    if desc.exists() {
                        decl.insert("description".into(), Value::String(desc.str()));
                    }
                    decl.insert("name".into(), Value::String(name.str()));
                    let params = tool.g("parameters");
                    if params.exists() {
                        decl.insert("parameters".into(), params.value());
                    }
                    Some(json!({ "functionDeclarations": [Value::Object(decl)] }))
                } else if tool.is_object() {
                    // Go round-trips through map[string]any (sorted keys, float64 numbers).
                    go_json_canonicalize(&tool.to_string()).map(|canonical| {
                        let mut raw_map = cpa_json::parse_str(&canonical);
                        if tool_type.is_empty() {
                            for (from, to) in [
                                ("url_context", "urlContext"),
                                ("code_execution", "codeExecution"),
                                ("google_search", "googleSearch"),
                                ("web_search", "googleSearch"),
                            ] {
                                if let Value::Object(map) = &mut raw_map
                                    && let Some(v) = map.shift_remove(from) {
                                        map.insert(to.into(), v);
                                    }
                            }
                            go_json_canonicalize(&raw_map.to_string())
                                .map(|c| cpa_json::parse_str(&c))
                                .unwrap_or(raw_map)
                        } else {
                            raw_map
                        }
                    })
                } else {
                    None
                }
            }
        };
        normalized.extend(entry);
    }
    // An empty result keeps the original tools.
    if normalized.is_empty() {
        cpa_json::set(out, "tools", tools.value());
        return;
    }
    cpa_json::set(out, "tools", Value::Array(normalized));
}

// ------------------------------------------------------------------ input

/// Accumulates Gemini contents while walking Interactions input steps.
struct InputContext {
    items: Vec<Value>,
    in_model_turn: bool,
    last_step_type: String,
    pending_signature: String,
}

impl InputContext {
    fn new() -> Self {
        Self { items: Vec::new(), in_model_turn: false, last_step_type: String::new(), pending_signature: String::new() }
    }

    fn last_role_is(&self, role: &str) -> bool {
        self.items.last().is_some_and(|last| last.g("role").str() == role)
    }

    /// Appends `parts` to the last content when it is a model turn and `in_model_turn` holds,
    /// else starts a new model content.
    fn add_model_parts(&mut self, parts: Vec<Value>) {
        if self.in_model_turn && self.last_role_is("model") {
            if let Some(last) = self.items.last_mut() {
                append_parts(last, parts);
            }
        } else {
            self.items.push(gemini_content("model", parts));
        }
    }

    /// Emits the pending signature as an empty text carrier part, on the last model turn when it
    /// is current, else on a new model content.
    fn emit_signature_carrier(&mut self, signature: &str) {
        let carrier = signature_carrier(signature);
        self.add_model_parts(vec![carrier]);
    }

    /// Go: flushPendingGeminiSignature (does not require `in_model_turn`).
    fn flush_pending_signature(&mut self) {
        if self.pending_signature.is_empty() {
            return;
        }
        let carrier = signature_carrier(&std::mem::take(&mut self.pending_signature));
        if self.last_role_is("model") {
            if let Some(last) = self.items.last_mut() {
                append_parts(last, vec![carrier]);
            }
        } else {
            self.items.push(gemini_content("model", vec![carrier]));
        }
    }

    fn end_model_turn(&mut self) {
        if self.in_model_turn {
            self.flush_pending_signature();
            self.in_model_turn = false;
        }
    }
}

fn signature_carrier(signature: &str) -> Value {
    let mut carrier = gemini_text_part_json("", false);
    cpa_json::set(&mut carrier, "thoughtSignature", signature);
    carrier
}

fn append_parts(content: &mut Value, new_parts: Vec<Value>) {
    if let Some(Value::Array(parts)) = content.get_mut("parts") {
        parts.extend(new_parts);
    }
}

fn append_user_content_part(content: &mut Value, part: Value) {
    if let Some(Value::Array(parts)) = content.get_mut("parts") {
        parts.push(part);
        let bytes = parts.iter().map(cpa_json::to_vec).collect();
        *parts = reorder_gemini_user_parts(bytes).iter().map(|p| cpa_json::parse(p)).collect();
    }
}

fn append_gemini_text_content(items: &mut Vec<Value>, role: &str, text: &str) {
    items.push(gemini_content(role, vec![gemini_text_part_json(text, false)]));
}

/// `raw` is the source text of `input`, used to copy function results containing `$ref` verbatim.
fn append_interactions_input(ctx: &mut InputContext, input: &Res<'_>, raw: Option<&str>) {
    if !input.exists() {
        return;
    }
    if let Some(text) = input.as_str() {
        append_gemini_text_content(&mut ctx.items, "user", text);
        ctx.last_step_type = "text".into();
        return;
    }
    if input.is_array() {
        let raws = child_raws(raw, "");
        for (i, item) in input.array().into_iter().enumerate() {
            append_interactions_step_to_gemini(ctx, &item.value(), "user", raws.get(i).copied());
        }
    } else if input.g("steps").is_array() {
        let role = input.g("role").str();
        let default_role = if role == "model" || role == "assistant" { "model" } else { "user" };
        let raws = child_raws(raw, "steps");
        for (i, step) in input.g("steps").array().into_iter().enumerate() {
            append_interactions_step_to_gemini(ctx, &step.value(), default_role, raws.get(i).copied());
        }
    } else {
        append_interactions_step_to_gemini(ctx, &input.value(), "user", raw);
    }
    ctx.flush_pending_signature();
}

/// Source text of each child of the array at `path` inside `raw` (empty when unavailable).
fn child_raws<'a>(raw: Option<&'a str>, path: &str) -> Vec<&'a str> {
    raw.map(|r| cpa_json::raw_children(r.as_bytes(), path)).unwrap_or_default()
}

/// `raw` is the source text of `item`.
fn append_interactions_step_to_gemini(ctx: &mut InputContext, item: &Value, default_role: &str, raw: Option<&str>) {
    if let Some(text) = item.as_str() {
        if ctx.in_model_turn {
            ctx.flush_pending_signature();
            ctx.in_model_turn = false;
        }
        append_gemini_text_content(&mut ctx.items, default_role, text);
        ctx.last_step_type = "text".into();
        return;
    }
    let steps = item.g("steps");
    if steps.exists() && steps.is_array() {
        let mut role = default_role.to_string();
        let item_role = item.g("role").str();
        if item_role == "model" || item_role == "assistant" {
            role = "model".into();
        } else if item_role == "user" {
            role = "user".into();
        }
        let raws = child_raws(raw, "steps");
        for (i, child) in steps.array().into_iter().enumerate() {
            append_interactions_step_to_gemini(ctx, &child.value(), &role, raws.get(i).copied());
        }
        return;
    }
    let step_type = item.g("type").str();
    let signature_of = |item: &Value| {
        first_trimmed(&[
            &item.g("signature").str(),
            &item.g("thought_signature").str(),
            &item.g("thoughtSignature").str(),
        ])
    };
    match step_type.as_str() {
        "model_output" => {
            if !ctx.pending_signature.is_empty() {
                let signature = std::mem::take(&mut ctx.pending_signature);
                ctx.emit_signature_carrier(&signature);
            }
            let part_items = extract_step_content_parts(item, false);
            if !part_items.is_empty() {
                ctx.add_model_parts(part_items);
            }
            ctx.in_model_turn = true;
            ctx.last_step_type = "model_output".into();
        }
        "thought" => {
            let sig = signature_of(item);
            if !sig.is_empty() {
                if !ctx.pending_signature.is_empty() && ctx.pending_signature != sig {
                    let pending = ctx.pending_signature.clone();
                    ctx.emit_signature_carrier(&pending);
                }
                ctx.pending_signature = sig;
            }
            let part_items = extract_thought_parts(item);
            if !part_items.is_empty() {
                ctx.add_model_parts(part_items);
            }
            ctx.in_model_turn = true;
            ctx.last_step_type = "thought".into();
        }
        "function_call" => {
            let mut part = build_gemini_function_call_part(item);
            let mut sig = signature_of(item);
            if sig.is_empty() && !ctx.pending_signature.is_empty() {
                sig = std::mem::take(&mut ctx.pending_signature);
            } else if !sig.is_empty() && !ctx.pending_signature.is_empty() {
                let pending = std::mem::take(&mut ctx.pending_signature);
                if pending != sig {
                    ctx.emit_signature_carrier(&pending);
                }
            }
            if !sig.is_empty() {
                cpa_json::set(&mut part, "thoughtSignature", sig);
            }
            ctx.add_model_parts(vec![part]);
            ctx.in_model_turn = true;
            ctx.last_step_type = "function_call".into();
        }
        "function_result" => {
            ctx.end_model_turn();
            let part = build_gemini_function_result_part(item, raw.and_then(|r| cpa_json::raw_at(r.as_bytes(), "result")));
            if ctx.last_step_type == "function_result" && ctx.last_role_is("user") {
                if let Some(last) = ctx.items.last_mut() {
                    append_user_content_part(last, part);
                }
            } else {
                ctx.items.push(gemini_content("user", vec![part]));
            }
            ctx.last_step_type = "function_result".into();
        }
        "user_input" | "" => {
            ctx.end_model_turn();
            if item.g("parts").exists() {
                append_interactions_native_content(&mut ctx.items, item, default_role);
            } else {
                append_interactions_content_list(&mut ctx.items, default_role, &item.g("content"));
            }
            ctx.last_step_type = "user_input".into();
        }
        _ => {
            ctx.end_model_turn();
            if item.g("parts").exists() {
                append_interactions_native_content(&mut ctx.items, item, default_role);
            } else if item.g("content").exists() {
                append_interactions_content_list(&mut ctx.items, default_role, &item.g("content"));
            } else if item.g("text").exists() {
                append_gemini_text_content(&mut ctx.items, default_role, &item.g("text").str());
            }
            ctx.last_step_type = "default".into();
        }
    }
}

/// Gemini parts for a content value that is a part array, a single part object, or a string.
fn content_to_gemini_parts(content: &Res<'_>, thought: bool) -> Vec<Value> {
    let mut parts = Vec::new();
    if content.is_array() {
        for part in content.array() {
            parts.extend(interactions_content_part_to_gemini_part(&part.value(), thought));
        }
    } else if content.is_object() {
        parts.extend(interactions_content_part_to_gemini_part(&content.value(), thought));
    } else if let Some(text) = content.as_str() {
        parts.push(gemini_text_part_json(text, thought));
    }
    parts
}

fn extract_thought_parts(step: &Value) -> Vec<Value> {
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
    content_to_gemini_parts(&content, true)
}

fn extract_step_content_parts(step: &Value, thought: bool) -> Vec<Value> {
    let mut content = step.g("content");
    if !content.exists() {
        content = step.g("text");
    }
    if !content.exists() {
        return vec![];
    }
    content_to_gemini_parts(&content, thought)
}

fn build_gemini_function_call_part(item: &Value) -> Value {
    let mut part = json!({ "functionCall": { "name": "", "args": {} } });
    cpa_json::set(&mut part, "functionCall.name", item.g("name").str());
    let call_id = item.g("call_id");
    let id = item.g("id");
    if call_id.exists() {
        cpa_json::set(&mut part, "functionCall.id", call_id.str());
    } else if id.exists() {
        cpa_json::set(&mut part, "functionCall.id", id.str());
    }
    let args = item.g("arguments");
    if args.exists() {
        cpa_json::set(&mut part, "functionCall.args", args.value());
    }
    part
}

fn build_gemini_function_result_part(item: &Value, result_text: Option<&str>) -> Value {
    let mut part = json!({ "functionResponse": { "name": "", "response": {} } });
    cpa_json::set(&mut part, "functionResponse.name", item.g("name").str());
    let call_id = item.g("call_id");
    let id = item.g("id");
    if call_id.exists() {
        cpa_json::set(&mut part, "functionResponse.id", call_id.str());
    } else if id.exists() {
        cpa_json::set(&mut part, "functionResponse.id", id.str());
    }
    let result = item.g("result");
    if result.exists() {
        match result_text.filter(|_| contains_json_ref(&result)) {
            // A result with `$ref` is stored as its source text.
            Some(text) => {
                cpa_json::set(&mut part, "functionResponse.response.result", text);
            }
            None => {
                let bytes =
                    set_gemini_function_response_result(&cpa_json::to_vec(&part), "functionResponse.response", &result);
                part = cpa_json::parse(&bytes);
            }
        }
    }
    part
}

fn append_interactions_native_content(items: &mut Vec<Value>, item: &Value, default_role: &str) {
    let parts = item.g("parts");
    if !parts.exists() || !parts.is_array() {
        return;
    }
    let part_items: Vec<Value> = parts.array().iter().filter_map(|p| interactions_native_gemini_part(&p.value())).collect();
    if part_items.is_empty() {
        return;
    }
    let role = interactions_gemini_content_role(&item.g("role").str(), default_role);
    items.push(gemini_content(role, part_items));
}

fn interactions_gemini_content_role(role: &str, default_role: &str) -> &'static str {
    match role.trim().to_lowercase().as_str() {
        "model" | "assistant" => "model",
        "user" => "user",
        _ if default_role == "model" => "model",
        _ => "user",
    }
}

/// A native Gemini part passed through an Interactions step.
fn interactions_native_gemini_part(part: &Value) -> Option<Value> {
    if part.g("text").exists() || part.g("functionCall").exists() || part.g("functionResponse").exists() {
        return Some(part.clone());
    }
    for (key, file) in [("inlineData", false), ("fileData", true), ("inline_data", false), ("file_data", true)] {
        let value = part.g(key);
        if value.exists() {
            let value = value.value();
            return if file { gemini_file_data_part_json(&value) } else { gemini_inline_data_part_json(&value) };
        }
    }
    None
}

fn append_interactions_content_list(items: &mut Vec<Value>, role: &str, content: &Res<'_>) {
    if !content.exists() {
        return;
    }
    if content.is_array() {
        for part in content.array() {
            append_interactions_content_part(items, role, &part.value());
        }
    } else if content.is_object() {
        append_interactions_content_part(items, role, &content.value());
    } else if let Some(text) = content.as_str() {
        append_gemini_text_content(items, role, text);
    }
}

fn append_interactions_content_part(items: &mut Vec<Value>, role: &str, part: &Value) {
    if let Some(part_json) = interactions_content_part_to_gemini_part(part, false) {
        items.push(gemini_content(role, vec![part_json]));
    }
}
