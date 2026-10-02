//! OpenAI Chat Completions request to Gemini request
//! (Go: gemini/openai/chat-completions/gemini_openai_request.go).

use std::collections::{HashMap, HashSet};

use cpa_core::signature::{gemini_replay_signature_or_bypass, SignatureBlockKind};
use cpa_core::util::{clean_json_schema_for_gemini_json_schema, sanitize_function_name};
use cpa_json::{json, Res, Value, J};

use crate::common::{normalize_openai_file_data, system_reminder_text};
use crate::gemini::claude::RawDoc;
use crate::gemini::common::attach_default_safety_settings;

const GEMINI_FUNCTION_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

const MODE_PATH: &str = "toolConfig.functionCallingConfig.mode";
const ALLOWED_NAMES_PATH: &str = "toolConfig.functionCallingConfig.allowedFunctionNames";

/// Converts an OpenAI Chat Completions request into a Gemini request body.
pub fn convert_openai_request_to_gemini(model_name: &str, raw: &[u8], _stream: bool) -> Vec<u8> {
    let root = cpa_json::parse(raw);
    let raw_doc = RawDoc::new(raw);
    let mut out = json!({ "contents": [] });
    cpa_json::set(&mut out, "model", model_name);

    // Let user-provided generationConfig pass through.
    let gen_config = root.g("generationConfig");
    if gen_config.exists() {
        cpa_json::set(&mut out, "generationConfig", gen_config.value());
    }

    // reasoning_effort -> thinkingConfig (translation only; capability checks happen in ApplyThinking).
    let re = root.g("reasoning_effort");
    if re.exists() {
        let effort = re.str().trim().to_lowercase();
        if !effort.is_empty() {
            let thinking_path = "generationConfig.thinkingConfig";
            if effort == "auto" {
                cpa_json::set(&mut out, &format!("{thinking_path}.thinkingBudget"), -1);
            } else {
                cpa_json::set(&mut out, &format!("{thinking_path}.thinkingLevel"), effort);
            }
        }
    }

    // temperature / top_p / top_k
    for (src, dst) in [
        ("temperature", "generationConfig.temperature"),
        ("top_p", "generationConfig.topP"),
        ("top_k", "generationConfig.topK"),
    ] {
        let v = root.g(src);
        if v.is_number() {
            cpa_json::set(&mut out, dst, cpa_json::num_f64(v.float()));
        }
    }

    // max_tokens / max_completion_tokens -> generationConfig.maxOutputTokens
    let mt = root.g("max_tokens");
    let mct = root.g("max_completion_tokens");
    if mt.is_number() {
        cpa_json::set(&mut out, "generationConfig.maxOutputTokens", cpa_json::num_f64(mt.float()));
    } else if mct.is_number() {
        cpa_json::set(&mut out, "generationConfig.maxOutputTokens", cpa_json::num_f64(mct.float()));
    }

    // Candidate count (OpenAI `n`).
    let n = root.g("n");
    if n.is_number() {
        let val = n.int();
        if val > 1 {
            cpa_json::set(&mut out, "generationConfig.candidateCount", val);
        }
    }

    // response_format -> structured output settings.
    apply_openai_response_format_to_gemini(&mut out, &root);

    // modalities ["image","text"] -> responseModalities ["IMAGE","TEXT"]
    let mods = root.g("modalities");
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
            cpa_json::set(&mut out, "generationConfig.responseModalities", json!(response_mods));
        }
    }

    // OpenRouter-style image_config.
    let img_cfg = root.g("image_config");
    if img_cfg.exists() && img_cfg.is_object() {
        if let Some(ar) = img_cfg.g("aspect_ratio").as_str() {
            cpa_json::set(&mut out, "generationConfig.imageConfig.aspectRatio", ar);
        }
        if let Some(size) = img_cfg.g("image_size").as_str() {
            cpa_json::set(&mut out, "generationConfig.imageConfig.imageSize", size);
        }
    }

    // messages -> systemInstruction + contents
    let messages = root.g("messages");
    if messages.is_array() {
        let arr = messages.array();
        let mut system_parts: Vec<Value> = Vec::with_capacity(2);
        let mut content_items: Vec<Value> = Vec::with_capacity(arr.len());

        let mut has_encountered_conversation = false;
        for (i, m) in arr.iter().enumerate() {
            let role = m.g("role").str();
            let content = m.g("content");

            if (role == "system" || role == "developer") && arr.len() > 1 && !has_encountered_conversation {
                // system -> systemInstruction as a user message style
                if let Some(text) = content.as_str() {
                    system_parts.push(text_part(text));
                } else if content.is_object() && content.g("type").str() == "text" {
                    system_parts.push(text_part(&content.g("text").str()));
                } else if content.is_array() {
                    for item in content.array() {
                        system_parts.push(text_part(&item.g("text").str()));
                    }
                }
            } else if role == "user" || role == "system" || role == "developer" {
                has_encountered_conversation = true;
                let is_demoted_system = role == "system" || role == "developer";
                // Single user content node to avoid splitting into multiple contents.
                let mut part_items: Vec<Value> = Vec::with_capacity(4);
                if let Some(text) = content.as_str() {
                    part_items.push(text_part(&demoted_system_text(text, is_demoted_system)));
                } else if content.is_object() && content.g("type").str() == "text" {
                    part_items.push(text_part(&demoted_system_text(&content.g("text").str(), is_demoted_system)));
                } else if content.is_array() {
                    for item in content.array() {
                        match item.g("type").str().as_str() {
                            "text" => {
                                let text = item.g("text").str();
                                if !text.is_empty() {
                                    part_items.push(text_part(&demoted_system_text(&text, is_demoted_system)));
                                }
                            }
                            "image_url" => {
                                if let Some((mime, data)) = data_url_parts(&item.g("image_url.url").str()) {
                                    part_items.push(inline_data_part(&mime, &data));
                                }
                            }
                            "video_url" => {
                                if let Some((mime, data)) = data_url_parts(&item.g("video_url.url").str()) {
                                    part_items.push(inline_data_part(&mime, &data));
                                }
                            }
                            "file" => {
                                let filename = item.g("file.filename").str();
                                let file_data = item.g("file.file_data").str();
                                if let Some((mime, data)) = normalize_openai_file_data(&filename, "", &file_data) {
                                    part_items.push(inline_data_part(&mime, &data));
                                } else {
                                    tracing::warn!("Invalid file data or unknown file name extension in user message, skip");
                                }
                            }
                            "input_audio" => {
                                let audio_data = item.g("input_audio.data").str();
                                if !audio_data.is_empty() {
                                    let mime = openai_input_audio_mime_type(&item.g("input_audio.format").str());
                                    part_items.push(inline_data_part(&mime, &audio_data));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if !part_items.is_empty() {
                    content_items.push(content_node("user", part_items));
                }
            } else if role == "assistant" {
                has_encountered_conversation = true;
                let mut part_items: Vec<Value> = Vec::with_capacity(4);
                let reasoning = m.g("reasoning_content");
                if let Some(text) = reasoning.as_str().filter(|t| !t.is_empty()) {
                    let mut part = text_part(text);
                    cpa_json::set(&mut part, "thought", true);
                    part_items.push(part);
                }
                if let Some(text) = content.as_str().filter(|t| !t.is_empty()) {
                    part_items.push(text_part(text));
                } else if content.is_array() {
                    // Assistant multimodal content (text + image) -> single model content.
                    for item in content.array() {
                        match item.g("type").str().as_str() {
                            "text" => {
                                let text = item.g("text").str();
                                if !text.is_empty() {
                                    part_items.push(text_part(&text));
                                }
                            }
                            "image_url" => {
                                if let Some((mime, data)) = data_url_parts(&item.g("image_url.url").str()) {
                                    part_items.push(inline_data_part(&mime, &data));
                                }
                            }
                            _ => {}
                        }
                    }
                }

                // Tool calls -> single model content with functionCall parts.
                let tcs = m.g("tool_calls");
                if tcs.is_array() {
                    let mut tool_calls: Vec<(String, String)> = Vec::new();
                    for tc in tcs.array() {
                        if tc.g("type").str() != "function" {
                            continue;
                        }
                        let function_id = tc.g("id").str();
                        let function_name = sanitize_function_name(&tc.g("function.name").str());
                        if function_name.is_empty() {
                            continue;
                        }
                        let mut part = json!({ "functionCall": { "name": "" } });
                        cpa_json::set(&mut part, "functionCall.name", function_name.as_str());
                        let _ = cpa_json::set_raw(&mut part, "functionCall.args", &tc.g("function.arguments").str());
                        cpa_json::set(&mut part, "thoughtSignature", tool_call_thought_signature(&tc));
                        part_items.push(part);
                        tool_calls.push((function_id, function_name));
                    }
                    if !part_items.is_empty() {
                        content_items.push(content_node("model", part_items));
                    }

                    // Tool responses scoped to this assistant turn.
                    let mut turn_tool_responses: HashMap<String, String> = HashMap::new();
                    for (next_index, next) in arr.iter().enumerate().skip(i + 1) {
                        let next_role = next.g("role").str();
                        if next_role == "assistant" {
                            break;
                        }
                        if next_role == "tool" {
                            let call_id = next.g("tool_call_id").str();
                            if !call_id.is_empty() {
                                // Go stores the content's source text (Raw) as a string.
                                let content_raw = raw_doc
                                    .at(&format!("messages.{next_index}.content"))
                                    .map(str::to_string)
                                    .unwrap_or_else(|| next.g("content").raw());
                                turn_tool_responses.insert(call_id, content_raw);
                            }
                        }
                    }

                    // One tool content combining name + response per function.
                    let mut response_parts: Vec<Value> = Vec::with_capacity(tool_calls.len());
                    for (id, name) in &tool_calls {
                        let mut part = json!({ "functionResponse": { "name": "", "response": { "result": "" } } });
                        cpa_json::set(&mut part, "functionResponse.name", name.as_str());
                        let response = match turn_tool_responses.get(id) {
                            Some(r) if !r.is_empty() => r.as_str(),
                            _ => "{}",
                        };
                        cpa_json::set(&mut part, "functionResponse.response.result", response);
                        response_parts.push(part);
                    }
                    if !response_parts.is_empty() {
                        content_items.push(content_node("user", response_parts));
                    }
                } else if !part_items.is_empty() {
                    content_items.push(content_node("model", part_items));
                }
            }
        }

        if !system_parts.is_empty() {
            cpa_json::set(&mut out, "systemInstruction", content_node("user", system_parts));
        }
        if content_items.last().is_some_and(|last| last.g("role").str() == "model") {
            content_items.pop();
        }
        // SetRawArrayItems is a no-op for an empty list.
        if !content_items.is_empty() {
            cpa_json::set(&mut out, "contents", Value::Array(content_items));
        }
    }

    // tools -> tools[].functionDeclarations + googleSearch/codeExecution/urlContext passthrough
    let mut allowed_tool_names: HashSet<String> = HashSet::new();
    let mut is_allowed_tools = false;
    let mut allowed_mode = "auto".to_string();
    let tool_choice = root.g("tool_choice");
    if tool_choice.exists() && tool_choice.is_object() && tool_choice.g("type").str() == "allowed_tools" {
        is_allowed_tools = true;
        let nested_tools = tool_choice.g("allowed_tools.tools");
        let flat_tools = tool_choice.g("tools");
        let mut tool_list = nested_tools.array();
        if tool_list.is_empty() {
            tool_list = flat_tools.array();
        }
        for t in tool_list {
            let mut fn_name = t.g("function.name").str().trim().to_string();
            if fn_name.is_empty() {
                fn_name = t.g("name").str().trim().to_string();
            }
            if !fn_name.is_empty() {
                allowed_tool_names.insert(fn_name);
            }
        }
        let mut mode_val = tool_choice.g("allowed_tools.mode").str().trim().to_lowercase();
        if mode_val.is_empty() {
            mode_val = tool_choice.g("mode").str().trim().to_lowercase();
        }
        if !mode_val.is_empty() {
            allowed_mode = mode_val;
        }
    }

    let mut declared_original_to_sanitized: HashMap<String, String> = HashMap::new();
    let mut sanitized_to_original_counts: HashMap<String, i64> = HashMap::new();
    let mut function_declarations: Vec<Value> = Vec::new();
    let mut has_strict_tool = false;
    let tools = root.g("tools");
    let tool_results = tools.array();
    if tools.is_array() && !tool_results.is_empty() {
        let mut google_search_nodes: Vec<Value> = Vec::new();
        let mut code_execution_nodes: Vec<Value> = Vec::new();
        let mut url_context_nodes: Vec<Value> = Vec::new();
        for t in &tool_results {
            if t.g("type").str() == "function" {
                let function = t.g("function");
                if function.exists() && function.is_object() {
                    let name_result = function.g("name");
                    let original_name = name_result.str();
                    if is_allowed_tools && !allowed_tool_names.contains(&original_name) {
                        continue;
                    }
                    let sanitized_name = sanitize_function_name(&original_name);
                    *sanitized_to_original_counts.entry(sanitized_name.clone()).or_insert(0) += 1;
                    declared_original_to_sanitized.insert(original_name.clone(), sanitized_name.clone());
                    let mut fn_value = function.value();
                    if let Some(parameters) = function.g("parameters").into_value() {
                        // Rename parameters -> parametersJsonSchema.
                        cpa_json::set(&mut fn_value, "parametersJsonSchema", parameters);
                        cpa_json::delete(&mut fn_value, "parameters");
                    } else {
                        cpa_json::set(&mut fn_value, "parametersJsonSchema.type", "object");
                        cpa_json::set(&mut fn_value, "parametersJsonSchema.properties", json!({}));
                    }
                    if !name_result.is_string() || sanitized_name != original_name {
                        cpa_json::set(&mut fn_value, "name", sanitized_name.as_str());
                    }
                    let parameters = fn_value.g("parametersJsonSchema");
                    if parameters.exists() {
                        let parameters_raw = parameters.raw();
                        let cleaned = clean_json_schema_for_gemini_json_schema(&parameters_raw);
                        if cleaned != parameters_raw {
                            let _ = cpa_json::set_raw(&mut fn_value, "parametersJsonSchema", &cleaned);
                        }
                    }
                    let mut strict_val = fn_value.g("strict").into_value();
                    if strict_val.is_none() {
                        strict_val = function.g("strict").into_value();
                        if strict_val.is_none() {
                            strict_val = t.g("strict").into_value();
                        }
                    }
                    if let Some(strict) = strict_val {
                        if strict == Value::Bool(true) {
                            has_strict_tool = true;
                        }
                        if fn_value.g("strict").exists() {
                            cpa_json::delete(&mut fn_value, "strict");
                        }
                    }
                    function_declarations.push(fn_value);
                }
            }
            for (key, node_key, nodes) in [
                ("google_search", "googleSearch", &mut google_search_nodes),
                ("code_execution", "codeExecution", &mut code_execution_nodes),
                ("url_context", "urlContext", &mut url_context_nodes),
            ] {
                let v = t.g(key);
                if v.exists() {
                    let mut node = json!({});
                    cpa_json::set(&mut node, node_key, v.value());
                    nodes.push(node);
                }
            }
        }
        if !function_declarations.is_empty()
            || !google_search_nodes.is_empty()
            || !code_execution_nodes.is_empty()
            || !url_context_nodes.is_empty()
        {
            let mut tool_items: Vec<Value> = Vec::new();
            if !function_declarations.is_empty() {
                tool_items.push(json!({ "functionDeclarations": function_declarations.clone() }));
            }
            tool_items.extend(google_search_nodes);
            tool_items.extend(code_execution_nodes);
            tool_items.extend(url_context_nodes);
            cpa_json::set(&mut out, "tools", Value::Array(tool_items));
        }
    }

    let has_sanitized_collision = sanitized_to_original_counts.values().any(|&count| count > 1);

    // tool_choice mapping
    if has_sanitized_collision {
        // Ambiguous collision in function names: fail closed to avoid invoking unintended tools.
        cpa_json::set(&mut out, MODE_PATH, "NONE");
    } else if is_allowed_tools {
        if function_declarations.is_empty() {
            // Fail closed when no allowed tools match or the subset is empty.
            cpa_json::set(&mut out, MODE_PATH, "NONE");
        } else if allowed_mode == "required" || allowed_mode == "any" {
            cpa_json::set(&mut out, MODE_PATH, "ANY");
            let allowed_list: Vec<String> = function_declarations.iter().map(|f| f.g("name").str()).collect();
            cpa_json::set(&mut out, ALLOWED_NAMES_PATH, json!(allowed_list));
        } else if has_strict_tool {
            cpa_json::set(&mut out, MODE_PATH, "VALIDATED");
        } else {
            // Mode AUTO: functionDeclarations holds only allowed tools, no allowedFunctionNames.
            cpa_json::set(&mut out, MODE_PATH, "AUTO");
        }
    } else if tool_choice.exists() && !tool_choice.is_null() {
        let mut tool_choice_type = String::new();
        if let Some(s) = tool_choice.as_str() {
            tool_choice_type = s.trim().to_lowercase();
        } else if tool_choice.is_object() {
            tool_choice_type = tool_choice.g("type").str().trim().to_lowercase();
        }

        match tool_choice_type.as_str() {
            "auto" => {
                cpa_json::set(&mut out, MODE_PATH, if has_strict_tool { "VALIDATED" } else { "AUTO" });
            }
            "none" => {
                cpa_json::set(&mut out, MODE_PATH, "NONE");
            }
            "required" | "any" => {
                cpa_json::set(&mut out, MODE_PATH, "ANY");
            }
            "function" | "tool" => {
                let mut fn_name = tool_choice.g("function.name").str().trim().to_string();
                if fn_name.is_empty() {
                    fn_name = tool_choice.g("name").str().trim().to_string();
                }
                match declared_original_to_sanitized.get(&fn_name) {
                    Some(sanitized) if sanitized_to_original_counts.get(sanitized) == Some(&1) => {
                        cpa_json::set(&mut out, MODE_PATH, "ANY");
                        cpa_json::set(&mut out, ALLOWED_NAMES_PATH, json!([sanitized]));
                    }
                    // Missing, undeclared, or ambiguous: fail closed.
                    _ => {
                        cpa_json::set(&mut out, MODE_PATH, "NONE");
                    }
                }
            }
            // Unrecognized tool_choice type: fail closed.
            _ => {
                cpa_json::set(&mut out, MODE_PATH, "NONE");
            }
        }
    } else if has_strict_tool && !function_declarations.is_empty() {
        cpa_json::set(&mut out, MODE_PATH, "VALIDATED");
    }

    // Gemini has no switch to disable parallel tool calls while keeping tools enabled, so an
    // explicit `parallel_tool_calls: false` fails closed with mode NONE.
    if root.g("parallel_tool_calls").v() == Some(&Value::Bool(false)) {
        cpa_json::set(&mut out, MODE_PATH, "NONE");
        cpa_json::delete(&mut out, ALLOWED_NAMES_PATH);
    }

    attach_default_safety_settings(&cpa_json::to_vec(&out), "safetySettings")
}

fn text_part(text: &str) -> Value {
    json!({ "text": text })
}

fn inline_data_part(mime_type: &str, data: &str) -> Value {
    json!({ "inlineData": { "mime_type": mime_type, "data": data } })
}

fn content_node(role: &str, parts: Vec<Value>) -> Value {
    json!({ "role": role, "parts": parts })
}

/// Splits a `data:<mime>;base64,<data>` style URL the way Go does with byte offsets: skip the
/// 5 byte scheme, split on the first `;`, and skip the 7 bytes of `base64,` without checking
/// them. `None` when the URL is too short or has no `;` or payload.
fn data_url_parts(url: &str) -> Option<(String, String)> {
    let bytes = url.as_bytes();
    if bytes.len() <= 5 {
        return None;
    }
    let rest = &bytes[5..];
    let semi = rest.iter().position(|&b| b == b';')?;
    let (mime, tail) = (&rest[..semi], &rest[semi + 1..]);
    if tail.len() <= 7 {
        return None;
    }
    Some((
        String::from_utf8_lossy(mime).into_owned(),
        String::from_utf8_lossy(&tail[7..]).into_owned(),
    ))
}

/// Gemini thought signature for a tool call: the first present of the extra_content/thought
/// signature fields (replay-sanitized), else the bypass sentinel.
fn tool_call_thought_signature(tool_call: &Res<'_>) -> String {
    for path in [
        "extra_content.google.thought_signature",
        "function.extra_content.google.thought_signature",
        "thoughtSignature",
        "thought_signature",
    ] {
        let signature = tool_call.g(path);
        if signature.exists() {
            return gemini_replay_signature_or_bypass(&signature.str(), SignatureBlockKind::GeminiFunctionCall);
        }
    }
    GEMINI_FUNCTION_THOUGHT_SIGNATURE.to_string()
}

fn openai_input_audio_mime_type(audio_format: &str) -> String {
    match audio_format {
        "" | "wav" => "audio/wav".into(),
        "mp3" => "audio/mpeg".into(),
        "ogg" => "audio/ogg".into(),
        "flac" => "audio/flac".into(),
        "aac" => "audio/aac".into(),
        "webm" => "audio/webm".into(),
        "pcm16" => "audio/pcm".into(),
        "g711_ulaw" | "g711_alaw" => "audio/basic".into(),
        other => format!("audio/{other}"),
    }
}

/// Maps OpenAI `response_format` to Gemini structured output settings. Response schemas pass
/// through unchanged because the tool schema cleaner removes supported response fields.
fn apply_openai_response_format_to_gemini(out: &mut Value, root: &Value) {
    let response_format = root.g("response_format");
    if !response_format.exists() {
        return;
    }
    match response_format.g("type").str().trim().to_lowercase().as_str() {
        "json_object" => {
            cpa_json::set(out, "generationConfig.responseMimeType", "application/json");
        }
        "json_schema" => {
            cpa_json::set(out, "generationConfig.responseMimeType", "application/json");
            cpa_json::delete(out, "generationConfig.responseSchema");
            let schema = response_format.g("json_schema.schema");
            if schema.exists() {
                cpa_json::set(out, "generationConfig.responseJsonSchema", schema.value());
            }
        }
        _ => {}
    }
}

/// Wraps a demoted mid-session system or developer message in the `<system-reminder>` envelope
/// so non-Claude upstream models treat it as a directive rather than user speech.
fn demoted_system_text(text: &str, is_demoted: bool) -> String {
    if !is_demoted || text.trim().is_empty() {
        return text.to_string();
    }
    system_reminder_text(text)
}
