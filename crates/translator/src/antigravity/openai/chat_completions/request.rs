//! OpenAI Chat Completions request -> Antigravity request (Go: antigravity_openai_request.go).

use std::collections::{HashMap, HashSet};

use cpa_core::thinking;
use cpa_core::util;
use cpa_json::{json, J, Res, Value};

use crate::antigravity::gemini::sanitize_antigravity_claude_gemini_request_signatures;
use crate::common;
use crate::gemini::common::attach_default_safety_settings;

const FUNCTION_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

/// Go: `ConvertOpenAIRequestToAntigravity`.
pub fn convert_openai_request_to_antigravity(model: &str, input_raw_json: &[u8], _stream: bool) -> Vec<u8> {
    let raw = cpa_json::parse(input_raw_json);
    let function_name_map = util::sanitized_function_name_map(input_raw_json);
    // Base envelope (no default thinkingConfig).
    let mut out = json!({"project": "", "request": {"contents": []}, "model": "gemini-2.5-pro"});
    cpa_json::set(&mut out, "model", model);

    // User-provided generationConfig passes through.
    let gen_config = raw.g("generationConfig");
    if gen_config.exists() {
        cpa_json::set(&mut out, "request.generationConfig", gen_config.value());
    } else {
        let gen_config = raw.g("generation_config");
        if gen_config.exists() {
            cpa_json::set(&mut out, "request.generationConfig", gen_config.value());
        }
    }

    // reasoning_effort -> thinkingConfig; capability checks happen later in ApplyThinking.
    let re = raw.g("reasoning_effort");
    if re.exists() {
        let effort = re.str().trim().to_lowercase();
        if !effort.is_empty() {
            let thinking_path = "request.generationConfig.thinkingConfig";
            if effort == "auto" {
                cpa_json::set(&mut out, &format!("{thinking_path}.thinkingBudget"), -1);
            } else {
                cpa_json::set(&mut out, &format!("{thinking_path}.thinkingLevel"), effort);
            }
        }
    }
    apply_thinking_compatibility(&mut out, input_raw_json);

    for (src, dst) in [("temperature", "temperature"), ("top_p", "topP"), ("top_k", "topK")] {
        let v = raw.g(src);
        if v.exists() && v.is_number() {
            cpa_json::set(&mut out, &format!("request.generationConfig.{dst}"), cpa_json::num_f64(v.float()));
        }
    }
    let max_tok = raw.g("max_tokens");
    let mct = raw.g("max_completion_tokens");
    if max_tok.exists() && max_tok.is_number() {
        cpa_json::set(&mut out, "request.generationConfig.maxOutputTokens", cpa_json::num_f64(max_tok.float()));
    } else if mct.exists() && mct.is_number() {
        cpa_json::set(&mut out, "request.generationConfig.maxOutputTokens", cpa_json::num_f64(mct.float()));
    }

    // response_format -> structured output settings.
    let response_format = raw.g("response_format");
    if response_format.exists() {
        let format_type = response_format.g("type").str().trim().to_lowercase();
        if matches!(format_type.as_str(), "json_object" | "json_schema") {
            for schema_key in ["responseSchema", "responseJsonSchema", "response_schema", "response_json_schema"] {
                cpa_json::delete(&mut out, &format!("request.generationConfig.{schema_key}"));
            }
            cpa_json::set(&mut out, "request.generationConfig.responseMimeType", "application/json");
            if format_type == "json_schema" {
                let schema = response_format.g("json_schema.schema");
                if schema.exists() {
                    cpa_json::set(&mut out, "request.generationConfig.responseSchema", schema.value());
                }
            }
        }
    }

    // Candidate count (OpenAI 'n').
    let n = raw.g("n");
    if n.exists() && n.is_number() && n.int() > 1 {
        cpa_json::set(&mut out, "request.generationConfig.candidateCount", n.int());
    }

    // modalities ["image","text"] -> responseModalities ["IMAGE","TEXT"].
    let mods = raw.g("modalities");
    if mods.exists() && mods.is_array() {
        let response_mods: Vec<&str> = mods
            .array()
            .iter()
            .filter_map(|m| match m.str().to_lowercase().as_str() {
                "text" => Some("TEXT"),
                "image" => Some("IMAGE"),
                _ => None,
            })
            .collect();
        if !response_mods.is_empty() {
            cpa_json::set(&mut out, "request.generationConfig.responseModalities", json!(response_mods));
        }
    }

    // OpenRouter-style image_config.
    let img_cfg = raw.g("image_config");
    if img_cfg.exists() && img_cfg.is_object() {
        let ar = img_cfg.g("aspect_ratio");
        if ar.exists() && ar.is_string() {
            cpa_json::set(&mut out, "request.generationConfig.imageConfig.aspectRatio", ar.str());
        }
        let size = img_cfg.g("image_size");
        if size.exists() && size.is_string() {
            cpa_json::set(&mut out, "request.generationConfig.imageConfig.imageSize", size.str());
        }
    }

    convert_messages(&mut out, &raw, &function_name_map);
    convert_tools(&mut out, &raw, &function_name_map);
    apply_tool_choice(&mut out, &raw, &function_name_map);

    let mut out_bytes = cpa_json::to_vec(&out);
    if model.to_lowercase().contains("claude") {
        out_bytes = sanitize_antigravity_claude_gemini_request_signatures(model, &out_bytes);
    }
    attach_default_safety_settings(&out_bytes, "request.safetySettings")
}

fn text_part(text: &str) -> Value {
    json!({"text": text})
}

fn inline_data_part(mime_type: &str, data: &str, snake_case: bool) -> Value {
    if snake_case {
        json!({"inlineData": {"mime_type": mime_type, "data": data}})
    } else {
        json!({"inlineData": {"mimeType": mime_type, "data": data}})
    }
}

fn content(role: &str, parts: Vec<Value>) -> Value {
    json!({"role": role, "parts": parts})
}

/// `data:<mime>;base64,<payload>` as an inlineData part. Like Go, the `base64,` marker is assumed
/// rather than verified (only its length is skipped).
fn data_url_part(url: &str) -> Option<Value> {
    if url.len() <= 5 {
        return None;
    }
    let (mime, rest) = url.get(5..)?.split_once(';')?;
    if rest.len() <= 7 {
        return None;
    }
    Some(inline_data_part(mime, rest.get(7..)?, false))
}

fn audio_mime_type(format: &str) -> String {
    match format {
        "mp3" => "audio/mpeg".into(),
        "ogg" => "audio/ogg".into(),
        "flac" => "audio/flac".into(),
        "aac" => "audio/aac".into(),
        "webm" => "audio/webm".into(),
        "pcm16" => "audio/pcm".into(),
        "g711_ulaw" | "g711_alaw" => "audio/basic".into(),
        "" | "wav" => "audio/wav".into(),
        other => format!("audio/{other}"),
    }
}

/// Wraps a demoted mid-session system or developer message in the `<system-reminder>` envelope so
/// non-Claude upstream models treat it as a directive rather than user speech.
fn demoted_system_text(text: &str, is_demoted: bool) -> String {
    if !is_demoted || text.trim().is_empty() {
        return text.to_string();
    }
    common::system_reminder_text(text)
}

fn convert_messages(out: &mut Value, raw: &Value, function_name_map: &HashMap<String, String>) {
    let messages = raw.g("messages");
    if !messages.is_array() {
        return;
    }
    let arr = messages.array();
    let mut system_parts: Vec<Value> = Vec::new();
    let mut content_items: Vec<Value> = Vec::with_capacity(arr.len());

    let mut has_encountered_conversation = false;
    for (i, m) in arr.iter().enumerate() {
        let role = m.g("role").str();
        let c = m.g("content");

        if (role == "system" || role == "developer") && arr.len() > 1 && !has_encountered_conversation {
            // system -> request.systemInstruction (user style)
            if c.is_string() {
                system_parts.push(text_part(&c.str()));
            } else if c.is_object() && c.g("type").str() == "text" {
                system_parts.push(text_part(&c.g("text").str()));
            } else if c.is_array() {
                for part in c.array() {
                    system_parts.push(text_part(&part.g("text").str()));
                }
            }
        } else if role == "user" || role == "system" || role == "developer" {
            has_encountered_conversation = true;
            let is_demoted = role == "system" || role == "developer";
            let mut part_items: Vec<Value> = Vec::new();
            if c.is_string() {
                part_items.push(text_part(&demoted_system_text(&c.str(), is_demoted)));
            } else if c.is_object() && c.g("type").str() == "text" {
                part_items.push(text_part(&demoted_system_text(&c.g("text").str(), is_demoted)));
            } else if c.is_array() {
                for item in c.array() {
                    match item.g("type").str().as_str() {
                        "text" => {
                            let text = item.g("text").str();
                            if !text.is_empty() {
                                part_items.push(text_part(&demoted_system_text(&text, is_demoted)));
                            }
                        }
                        "image_url" => part_items.extend(data_url_part(&item.g("image_url.url").str())),
                        "video_url" => part_items.extend(data_url_part(&item.g("video_url.url").str())),
                        "file" => {
                            let filename = item.g("file.filename").str();
                            let file_data = item.g("file.file_data").str();
                            match common::normalize_openai_file_data(&filename, "", &file_data) {
                                Some((mime_type, data)) => part_items.push(inline_data_part(&mime_type, &data, false)),
                                None => tracing::warn!("Invalid file data or unknown file name extension in user message, skip"),
                            }
                        }
                        "input_audio" => {
                            let audio_data = item.g("input_audio.data").str();
                            if !audio_data.is_empty() {
                                let mime_type = audio_mime_type(&item.g("input_audio.format").str());
                                part_items.push(inline_data_part(&mime_type, &audio_data, true));
                            }
                        }
                        _ => {}
                    }
                }
            }
            if !part_items.is_empty() {
                content_items.push(content("user", part_items));
            }
        } else if role == "assistant" {
            has_encountered_conversation = true;
            convert_assistant_message(&mut content_items, &arr, i, function_name_map);
        }
    }
    if !system_parts.is_empty() {
        cpa_json::set(out, "request.systemInstruction", content("user", system_parts));
    }
    if !content_items.is_empty() {
        cpa_json::set(out, "request.contents", Value::Array(content_items));
    }
}

fn convert_assistant_message(content_items: &mut Vec<Value>, arr: &[Res<'_>], i: usize, function_name_map: &HashMap<String, String>) {
    let m = &arr[i];
    let c = m.g("content");
    let mut part_items: Vec<Value> = Vec::new();
    let reasoning_content = m.g("reasoning_content");
    if reasoning_content.is_string() && !reasoning_content.str().is_empty() {
        let mut part = text_part(&reasoning_content.str());
        cpa_json::set(&mut part, "thought", true);
        part_items.push(part);
    }
    if c.is_string() && !c.str().is_empty() {
        part_items.push(text_part(&c.str()));
    } else if c.is_array() {
        for item in c.array() {
            match item.g("type").str().as_str() {
                "text" => {
                    let text = item.g("text").str();
                    if !text.is_empty() {
                        part_items.push(text_part(&text));
                    }
                }
                "image_url" => part_items.extend(data_url_part(&item.g("image_url.url").str())),
                _ => {}
            }
        }
    }

    let tcs = m.g("tool_calls");
    if !tcs.is_array() {
        if !part_items.is_empty() {
            content_items.push(content("model", part_items));
        }
        return;
    }

    struct ToolCall {
        raw_id: String,
        id: String,
        name: String,
    }
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut used_tool_call_ids: HashSet<String> = HashSet::new();
    for tc in tcs.array() {
        if tc.g("type").str() != "function" {
            continue;
        }
        let raw_id = tc.g("id").str();
        let base_id = util::sanitize_claude_tool_id(&raw_id);
        let mut function_id = base_id.clone();
        let mut suffix = 1;
        while !used_tool_call_ids.insert(function_id.clone()) {
            function_id = format!("{base_id}_{suffix}");
            suffix += 1;
        }
        let function_name = util::map_sanitized_function_name(function_name_map, &tc.g("function.name").str());
        if function_name.is_empty() {
            continue;
        }
        let function_args = tc.g("function.arguments").str();
        let mut part = json!({"functionCall": {"id": "", "name": ""}});
        cpa_json::set(&mut part, "functionCall.id", function_id.clone());
        cpa_json::set(&mut part, "functionCall.name", function_name.clone());
        if cpa_json::valid(function_args.as_bytes()) {
            cpa_json::set(&mut part, "functionCall.args", cpa_json::parse_str(&function_args));
        } else {
            cpa_json::set(&mut part, "functionCall.args.params", function_args);
        }
        cpa_json::set(&mut part, "thoughtSignature", FUNCTION_THOUGHT_SIGNATURE);
        part_items.push(part);
        tool_calls.push(ToolCall { raw_id, id: function_id, name: function_name });
    }
    if !part_items.is_empty() {
        content_items.push(content("model", part_items));
    }

    // Tool responses scoped to this assistant turn.
    let mut turn_tool_responses: HashMap<String, String> = HashMap::new();
    for next in &arr[i + 1..] {
        let next_role = next.g("role").str();
        if next_role == "assistant" {
            break;
        }
        if next_role == "tool" {
            let call_id = next.g("tool_call_id").str();
            if !call_id.is_empty() {
                turn_tool_responses.insert(call_id, next.g("content").str());
            }
        }
    }

    let mut response_parts: Vec<Value> = Vec::with_capacity(tool_calls.len());
    for call in &tool_calls {
        let mut part = json!({"functionResponse": {"id": "", "name": ""}});
        cpa_json::set(&mut part, "functionResponse.id", call.id.clone());
        cpa_json::set(&mut part, "functionResponse.name", call.name.clone());
        let mut response = turn_tool_responses.get(&call.raw_id).cloned().unwrap_or_default();
        if response.is_empty() {
            response = "{}".to_string();
        }
        // Kept as a string: parsing it as JSON (like reading a JSON file via readFile) can trigger
        // an upstream 400.
        cpa_json::set(&mut part, "functionResponse.response.result", response);
        response_parts.push(part);
    }
    if !response_parts.is_empty() {
        content_items.push(content("user", response_parts));
    }
}

fn convert_tools(out: &mut Value, raw: &Value, function_name_map: &HashMap<String, String>) {
    let tools = raw.g("tools");
    let tool_results = tools.array();
    if !(tools.is_array() && !tool_results.is_empty()) {
        return;
    }
    let mut function_declarations: Vec<Vec<u8>> = Vec::with_capacity(tool_results.len());
    let mut google_search_nodes: Vec<Value> = Vec::new();
    let mut code_execution_nodes: Vec<Value> = Vec::new();
    let mut url_context_nodes: Vec<Value> = Vec::new();
    for t in &tool_results {
        if t.g("type").str() == "function" {
            let f = t.g("function");
            if f.exists() && f.is_object() {
                let mut fn_json = f.value();
                if f.g("parameters").exists() {
                    // RenameKey: set the new key (appended), delete the old.
                    let params = f.g("parameters").value();
                    cpa_json::set(&mut fn_json, "parametersJsonSchema", params);
                    cpa_json::delete(&mut fn_json, "parameters");
                } else {
                    cpa_json::set(&mut fn_json, "parametersJsonSchema.type", "object");
                    cpa_json::set(&mut fn_json, "parametersJsonSchema.properties", json!({}));
                }
                let name_result = f.g("name");
                let original_name = name_result.str();
                let mapped_name = util::map_sanitized_function_name(function_name_map, &original_name);
                if !name_result.is_string() || mapped_name != original_name {
                    cpa_json::set(&mut fn_json, "name", mapped_name);
                }
                if fn_json.g("strict").exists() {
                    cpa_json::delete(&mut fn_json, "strict");
                }
                function_declarations.push(cpa_json::to_vec(&fn_json));
            }
        }
        let gs = t.g("google_search");
        if gs.exists() {
            google_search_nodes.push(json!({"googleSearch": gs.value()}));
        }
        let ce = t.g("code_execution");
        if ce.exists() {
            code_execution_nodes.push(json!({"codeExecution": ce.value()}));
        }
        let uc = t.g("url_context");
        if uc.exists() {
            url_context_nodes.push(json!({"urlContext": uc.value()}));
        }
    }
    let deduplicated = cpa_json::parse(&util::deduplicate_function_declarations(&common::join_raw_array(&function_declarations)));
    let has_function = deduplicated.as_array().is_some_and(|a| !a.is_empty());
    if has_function || !google_search_nodes.is_empty() || !code_execution_nodes.is_empty() || !url_context_nodes.is_empty() {
        let mut tool_items: Vec<Value> = Vec::new();
        if has_function {
            tool_items.push(json!({"functionDeclarations": deduplicated}));
        }
        tool_items.extend(google_search_nodes);
        tool_items.extend(code_execution_nodes);
        tool_items.extend(url_context_nodes);
        cpa_json::set(out, "request.tools", Value::Array(tool_items));
    }
}

fn apply_tool_choice(out: &mut Value, raw: &Value, function_name_map: &HashMap<String, String>) {
    let tool_choice = raw.g("tool_choice");
    if !tool_choice.exists() {
        return;
    }
    let mut mode = "";
    let mut allowed_name = String::new();
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
            "function" => {
                mode = "ANY";
                allowed_name = tool_choice.g("function.name").str();
            }
            _ => {}
        }
    }
    if mode.is_empty() {
        return;
    }
    cpa_json::set(out, "request.toolConfig.functionCallingConfig.mode", mode);
    if mode == "NONE" {
        cpa_json::delete(out, "request.tools");
    }
    if !allowed_name.trim().is_empty() {
        let mapped = util::map_sanitized_function_name(function_name_map, &allowed_name);
        cpa_json::set(out, "request.toolConfig.functionCallingConfig.allowedFunctionNames", json!([mapped]));
    }
}

fn apply_thinking_compatibility(out: &mut Value, raw_json: &[u8]) {
    normalize_thinking_config(out);
    let config = thinking::extract_summary_config(raw_json, "openai");
    let bytes = thinking::apply_summary_config(cpa_json::to_vec(out), "antigravity", &config);
    *out = cpa_json::parse(&bytes);
}

/// Moves snake_case/legacy thinking fields into `generationConfig.thinkingConfig`.
fn normalize_thinking_config(out: &mut Value) {
    const TARGET: &str = "request.generationConfig.thinkingConfig";
    for prefix in ["request.generationConfig.thinking_config", "request.generationConfig.thinkingConfig"] {
        for key in ["includeThoughts", "include_thoughts"] {
            let source_path = format!("{prefix}.{key}");
            let v = out.g(&source_path);
            if v.exists() {
                let v = v.value();
                set_bool_if_valid(out, &format!("{TARGET}.includeThoughts"), &v);
                if !v.is_boolean() {
                    cpa_json::delete(out, &source_path);
                }
            }
        }
        for (key, target_key) in [
            ("thinkingLevel", "thinkingLevel"),
            ("thinking_level", "thinkingLevel"),
            ("thinkingBudget", "thinkingBudget"),
            ("thinking_budget", "thinkingBudget"),
        ] {
            let v = out.g(&format!("{prefix}.{key}"));
            if v.exists() {
                let v = v.value();
                set_raw_if_different(out, &format!("{TARGET}.{target_key}"), &v);
            }
        }
    }

    for path in ["request.generationConfig.includeThoughts", "request.generationConfig.include_thoughts"] {
        let v = out.g(path);
        if v.exists() {
            let v = v.value();
            set_bool_if_valid(out, &format!("{TARGET}.includeThoughts"), &v);
        }
    }

    for path in [
        "request.generationConfig.thinking_config",
        "request.generationConfig.thinkingConfig.include_thoughts",
        "request.generationConfig.thinkingConfig.thinking_level",
        "request.generationConfig.thinkingConfig.thinking_budget",
        "request.generationConfig.includeThoughts",
        "request.generationConfig.include_thoughts",
    ] {
        if out.g(path).exists() {
            cpa_json::delete(out, path);
        }
    }
}

fn set_bool_if_valid(out: &mut Value, path: &str, value: &Value) {
    if let Value::Bool(b) = value {
        if out.g(path).v() != Some(&Value::Bool(*b)) {
            cpa_json::set(out, path, *b);
        }
    }
}

fn set_raw_if_different(out: &mut Value, path: &str, value: &Value) {
    let current = out.g(path);
    if current.exists() && current.raw() == cpa_json::to_string(value) {
        return;
    }
    cpa_json::set(out, path, value.clone());
}
