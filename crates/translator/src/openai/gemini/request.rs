//! Gemini generateContent request -> OpenAI Chat Completions request
//! (Go: openai/gemini/openai_gemini_request.go).

use std::collections::HashMap;

use cpa_core::thinking;
use crate::common::raw_in;
use cpa_json::{raw_children, Res, Value, J};
use sha2::{Digest, Sha256};

use crate::common;

fn tpl(s: &str) -> Value {
    cpa_json::parse_str(s)
}

/// Converts a Gemini request into an OpenAI Chat Completions request.
pub fn convert_gemini_request_to_openai(model_name: &str, input: &[u8], stream: bool) -> Vec<u8> {
    let mut out = tpl(r#"{"model":"","messages":[]}"#);
    let root = cpa_json::parse(input);

    cpa_json::set(&mut out, "model", model_name);

    let gen_config = root.g("generationConfig");
    if gen_config.exists() {
        let temp = gen_config.g("temperature");
        if temp.exists() {
            cpa_json::set(&mut out, "temperature", cpa_json::num_f64(temp.float()));
        }

        let max_tokens = gen_config.g("maxOutputTokens");
        if max_tokens.exists() {
            cpa_json::set(&mut out, "max_tokens", max_tokens.int());
        }

        let top_p = gen_config.g("topP");
        if top_p.exists() {
            cpa_json::set(&mut out, "top_p", cpa_json::num_f64(top_p.float()));
        }

        // OpenAI has no topK; it is forwarded as a custom parameter.
        let top_k = gen_config.g("topK");
        if top_k.exists() {
            cpa_json::set(&mut out, "top_k", top_k.int());
        }

        let stop_sequences = gen_config.g("stopSequences");
        if stop_sequences.is_array() {
            let stops: Vec<Value> = stop_sequences.array().iter().map(|v| Value::String(v.str())).collect();
            if !stops.is_empty() {
                cpa_json::set(&mut out, "stop", Value::Array(stops));
            }
        }

        let candidate_count = gen_config.g("candidateCount");
        if candidate_count.exists() {
            cpa_json::set(&mut out, "n", candidate_count.int());
        }

        let response_modalities = gen_config.g("responseModalities");
        if response_modalities.is_array() {
            let modalities: Vec<Value> = response_modalities
                .array()
                .iter()
                .filter_map(|v| match v.str().trim().to_lowercase().as_str() {
                    m @ ("text" | "image" | "audio") => Some(Value::String(m.to_string())),
                    _ => None,
                })
                .collect();
            if !modalities.is_empty() {
                cpa_json::set(&mut out, "modalities", Value::Array(modalities));
            }
        }

        // thinkingConfig -> reasoning_effort, always converted so allowCompat models outside the
        // registry work. Google's Python SDK sends snake_case (thinking_level/thinking_budget).
        let thinking_config = gen_config.g("thinkingConfig");
        if thinking_config.is_object() {
            let mut thinking_level = thinking_config.g("thinkingLevel");
            if !thinking_level.exists() {
                thinking_level = thinking_config.g("thinking_level");
            }
            if thinking_level.exists() {
                let effort = thinking_level.str().trim().to_lowercase();
                if !effort.is_empty() {
                    cpa_json::set(&mut out, "reasoning_effort", effort);
                }
            } else {
                let mut thinking_budget = thinking_config.g("thinkingBudget");
                if !thinking_budget.exists() {
                    thinking_budget = thinking_config.g("thinking_budget");
                }
                if thinking_budget.exists()
                    && let Some(effort) = thinking::convert_budget_to_level(thinking_budget.int())
                {
                    cpa_json::set(&mut out, "reasoning_effort", effort);
                }
            }
        }
    }

    cpa_json::set(&mut out, "stream", stream);
    let service_tier = root.g("service_tier");
    if service_tier.is_string() {
        cpa_json::set(&mut out, "service_tier", service_tier.str());
    }

    let mut message_items: Vec<Value> = Vec::new();
    // Tool call ids per function name, matched FIFO against later functionResponses.
    let mut tool_call_ids_by_name: HashMap<String, Vec<String>> = HashMap::new();

    // Gemini accepts `systemInstruction` or `system_instruction`.
    let mut system_instruction = root.g("systemInstruction");
    if !system_instruction.exists() {
        system_instruction = root.g("system_instruction");
    }
    if system_instruction.exists() {
        let parts = system_instruction.g("parts");
        let mut content_items: Vec<Value> = Vec::new();
        if parts.is_array() {
            for part in parts.array() {
                if common::is_gemini_thought_part(&part) {
                    continue;
                }
                let text = part.g("text");
                if text.exists() {
                    let mut content_part = tpl(r#"{"type":"text","text":""}"#);
                    cpa_json::set(&mut content_part, "text", text.str());
                    content_items.push(content_part);
                }
                if let Some(content_part) = openai_content_part_from_gemini_inline_data(&part) {
                    content_items.push(content_part);
                }
                if let Some(content_part) = openai_content_part_from_gemini_file_data(&part) {
                    content_items.push(content_part);
                }
            }
        }
        if !content_items.is_empty() {
            let mut msg = tpl(r#"{"role":"system","content":[]}"#);
            cpa_json::set(&mut msg, "content", Value::Array(content_items));
            message_items.push(msg);
        }
    }

    let contents = root.g("contents");
    if contents.is_array() {
        let content_raws = raw_children(input, "contents");
        for (msg_idx, content) in contents.array().into_iter().enumerate() {
            let mut role = content.g("role").str();
            let parts = content.g("parts");

            if role == "model" {
                role = "assistant".into();
            }

            let mut msg = tpl(r#"{"role":"","content":""}"#);
            cpa_json::set(&mut msg, "role", role);

            let mut text_builder = String::new();
            let mut content_items: Vec<Value> = Vec::new();
            let mut only_text_content = true;
            let mut tool_call_items: Vec<Value> = Vec::new();
            let mut dropped_thought = false;

            if parts.is_array() {
                let part_raws = content_raws.get(msg_idx).map(|c| raw_children(c.as_bytes(), "parts")).unwrap_or_default();
                for (part_idx, part) in parts.array().into_iter().enumerate() {
                    if common::is_gemini_thought_part(&part) {
                        dropped_thought = true;
                        continue;
                    }

                    let text = part.g("text");
                    if text.exists() {
                        let formatted = text.str();
                        text_builder.push_str(&formatted);
                        let mut content_part = tpl(r#"{"type":"text","text":""}"#);
                        cpa_json::set(&mut content_part, "text", formatted);
                        content_items.push(content_part);
                    }

                    if let Some(content_part) = openai_content_part_from_gemini_inline_data(&part) {
                        only_text_content = false;
                        content_items.push(content_part);
                    }
                    if let Some(content_part) = openai_content_part_from_gemini_file_data(&part) {
                        only_text_content = false;
                        content_items.push(content_part);
                    }

                    // Verbatim `Raw` of a sub-value of this part (client whitespace included).
                    let part_raw = |sub: &str, fallback: &Res<'_>| -> String {
                        raw_in(part_raws.get(part_idx), sub).map_or_else(|| fallback.raw(), str::to_string)
                    };

                    let function_call = part.g("functionCall");
                    if function_call.exists() {
                        let func_name = function_call.g("name").str();
                        let args = function_call.g("args");
                        let args_raw = if args.exists() { part_raw("functionCall.args", &args) } else { String::new() };
                        let mut tool_call_id = explicit_gemini_tool_id(&function_call);
                        if tool_call_id.is_empty() {
                            tool_call_id = deterministic_tool_call_id("call", msg_idx, part_idx, &func_name, &args_raw);
                        }
                        tool_call_ids_by_name.entry(func_name.clone()).or_default().push(tool_call_id.clone());

                        let mut tool_call = tpl(r#"{"id":"","type":"function","function":{"name":"","arguments":""}}"#);
                        cpa_json::set(&mut tool_call, "id", tool_call_id);
                        cpa_json::set(&mut tool_call, "function.name", func_name);
                        if args_raw.is_empty() {
                            cpa_json::set(&mut tool_call, "function.arguments", "{}");
                        } else {
                            cpa_json::set(&mut tool_call, "function.arguments", args_raw);
                        }
                        tool_call_items.push(tool_call);
                    }

                    let function_response = part.g("functionResponse");
                    if function_response.exists() {
                        let func_name = function_response.g("name").str();
                        let mut tool_msg = tpl(r#"{"role":"tool","tool_call_id":"","content":""}"#);

                        let mut response_raw = String::new();
                        let response = function_response.g("response");
                        if response.exists() {
                            let content_field = response.g("content");
                            if content_field.exists() {
                                response_raw = part_raw("functionResponse.response.content", &content_field);
                            } else {
                                response_raw = part_raw("functionResponse.response", &response);
                            }
                            cpa_json::set(&mut tool_msg, "content", response_raw.clone());
                        }

                        let explicit_id = explicit_gemini_tool_id(&function_response);
                        let queue = tool_call_ids_by_name.entry(func_name.clone()).or_default();
                        if !explicit_id.is_empty() {
                            cpa_json::set(&mut tool_msg, "tool_call_id", explicit_id.clone());
                            if let Some(i) = queue.iter().position(|id| *id == explicit_id) {
                                queue.remove(i);
                            }
                        } else if !queue.is_empty() {
                            let tool_call_id = queue.remove(0);
                            cpa_json::set(&mut tool_msg, "tool_call_id", tool_call_id);
                        } else {
                            // No pending call to pair with: derive a stable id.
                            let fallback_id = deterministic_tool_call_id("response", msg_idx, part_idx, &func_name, &response_raw);
                            cpa_json::set(&mut tool_msg, "tool_call_id", fallback_id);
                        }

                        message_items.push(tool_msg);
                    }
                }
            }

            if !content_items.is_empty() {
                if only_text_content {
                    cpa_json::set(&mut msg, "content", text_builder);
                } else {
                    cpa_json::set(&mut msg, "content", Value::Array(content_items.clone()));
                }
            }

            if !tool_call_items.is_empty() {
                cpa_json::set(&mut msg, "tool_calls", Value::Array(tool_call_items.clone()));
            }

            if dropped_thought && content_items.is_empty() && tool_call_items.is_empty() {
                continue;
            }

            message_items.push(msg);
        }
    }
    if !message_items.is_empty() {
        cpa_json::set(&mut out, "messages", Value::Array(message_items));
    }

    // Tools: Gemini functionDeclarations -> OpenAI tools.
    let tools = root.g("tools");
    if tools.is_array() {
        let mut tool_items: Vec<Value> = Vec::new();
        for tool in tools.array() {
            let function_declarations = tool.g("functionDeclarations");
            if !function_declarations.is_array() {
                continue;
            }
            for func_decl in function_declarations.array() {
                let mut openai_tool = tpl(r#"{"type":"function","function":{"name":"","description":""}}"#);
                cpa_json::set(&mut openai_tool, "function.name", func_decl.g("name").str());
                cpa_json::set(&mut openai_tool, "function.description", func_decl.g("description").str());

                let parameters = func_decl.g("parameters");
                if parameters.exists() {
                    cpa_json::set(&mut openai_tool, "function.parameters", parameters.value());
                } else {
                    let parameters = func_decl.g("parametersJsonSchema");
                    if parameters.exists() {
                        cpa_json::set(&mut openai_tool, "function.parameters", parameters.value());
                    }
                }
                tool_items.push(openai_tool);
            }
        }
        if !tool_items.is_empty() {
            cpa_json::set(&mut out, "tools", Value::Array(tool_items));
        }
    }

    // Tool choice: Gemini has no direct equivalent, so map the calling mode.
    let tool_config = root.g("toolConfig");
    if tool_config.exists() {
        let function_calling_config = tool_config.g("functionCallingConfig");
        if function_calling_config.exists() {
            let mode = function_calling_config.g("mode").str();
            let allowed_names = function_calling_config.g("allowedFunctionNames");
            match mode.as_str() {
                "NONE" => {
                    cpa_json::set(&mut out, "tool_choice", "none");
                }
                "AUTO" => {
                    cpa_json::set(&mut out, "tool_choice", "auto");
                }
                "ANY" => {
                    let items = allowed_names.array();
                    if allowed_names.is_array() && items.len() == 1 {
                        let mut choice = tpl(r#"{"type":"function","function":{"name":""}}"#);
                        cpa_json::set(&mut choice, "function.name", items[0].str());
                        cpa_json::set(&mut out, "tool_choice", choice);
                    } else {
                        cpa_json::set(&mut out, "tool_choice", "required");
                    }
                }
                _ => {}
            }
        }
    }

    cpa_json::to_vec(&out)
}

/// `call_` + first 12 bytes of sha256 over `kind|msgIdx|partIdx|name|payload`, hex encoded.
fn deterministic_tool_call_id(kind: &str, msg_idx: usize, part_idx: usize, name: &str, payload: &str) -> String {
    let sum = Sha256::digest(format!("{kind}|{msg_idx}|{part_idx}|{name}|{payload}").as_bytes());
    format!("call_{}", hex::encode(&sum[..12]))
}

/// The first non-blank of `id`, `call_id`, `callId`.
fn explicit_gemini_tool_id(node: &Res<'_>) -> String {
    for key in ["id", "call_id", "callId"] {
        let id = node.g(key).str();
        if !id.trim().is_empty() {
            return id.trim().to_string();
        }
    }
    String::new()
}

fn openai_content_part_from_gemini_inline_data(part: &Res<'_>) -> Option<Value> {
    let mut inline_data = part.g("inlineData");
    if !inline_data.exists() {
        inline_data = part.g("inline_data");
    }
    if !inline_data.exists() {
        return None;
    }
    let mut mime_type = inline_data.g("mimeType").str();
    if mime_type.is_empty() {
        mime_type = inline_data.g("mime_type").str();
    }
    if mime_type.is_empty() {
        mime_type = "application/octet-stream".into();
    }
    let data = inline_data.g("data").str();
    if data.is_empty() {
        return None;
    }
    let data_url = format!("data:{mime_type};base64,{data}");
    let lower = mime_type.to_lowercase();
    Some(if lower.starts_with("image/") {
        let mut content_part = tpl(r#"{"type":"image_url","image_url":{"url":""}}"#);
        cpa_json::set(&mut content_part, "image_url.url", data_url);
        content_part
    } else if lower.starts_with("audio/") {
        let mut content_part = tpl(r#"{"type":"input_audio","input_audio":{"data":"","format":""}}"#);
        cpa_json::set(&mut content_part, "input_audio.data", data);
        cpa_json::set(&mut content_part, "input_audio.format", openai_input_audio_format_from_mime(&mime_type));
        content_part
    } else if lower.starts_with("video/") {
        let mut content_part = tpl(r#"{"type":"video_url","video_url":{"url":""}}"#);
        cpa_json::set(&mut content_part, "video_url.url", data_url);
        content_part
    } else {
        let mut content_part = tpl(r#"{"type":"file","file":{"filename":"","file_data":""}}"#);
        cpa_json::set(&mut content_part, "file.filename", openai_file_name_from_mime(&mime_type));
        cpa_json::set(&mut content_part, "file.file_data", data);
        content_part
    })
}

fn openai_content_part_from_gemini_file_data(part: &Res<'_>) -> Option<Value> {
    let mut file_data = part.g("fileData");
    if !file_data.exists() {
        file_data = part.g("file_data");
    }
    if !file_data.exists() {
        return None;
    }
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
        let mut content_part = tpl(r#"{"type":"image_url","image_url":{"url":""}}"#);
        cpa_json::set(&mut content_part, "image_url.url", file_uri);
        return Some(content_part);
    }
    if lower.starts_with("video/") {
        let mut content_part = tpl(r#"{"type":"video_url","video_url":{"url":""}}"#);
        cpa_json::set(&mut content_part, "video_url.url", file_uri);
        return Some(content_part);
    }
    if lower.starts_with("application/") || lower.starts_with("text/") {
        let mut content_part = tpl(r#"{"type":"file","file":{"filename":"","file_url":""}}"#);
        cpa_json::set(&mut content_part, "file.filename", openai_file_name_from_mime(&mime_type));
        cpa_json::set(&mut content_part, "file.file_url", file_uri);
        return Some(content_part);
    }
    let mut file_info = format!("File: {file_uri}");
    if !mime_type.is_empty() {
        file_info.push_str(&format!(" (Type: {mime_type})"));
    }
    let mut content_part = tpl(r#"{"type":"text","text":""}"#);
    cpa_json::set(&mut content_part, "text", file_info);
    Some(content_part)
}

fn openai_input_audio_format_from_mime(mime_type: &str) -> &'static str {
    match mime_type.trim().to_lowercase().as_str() {
        "audio/wav" | "audio/wave" | "audio/x-wav" => "wav",
        "audio/flac" => "flac",
        "audio/opus" | "audio/ogg" => "opus",
        "audio/pcm" | "audio/l16" => "pcm16",
        _ => "mp3",
    }
}

fn openai_file_name_from_mime(mime_type: &str) -> &'static str {
    let lower = mime_type.trim().to_lowercase();
    match lower.as_str() {
        "application/pdf" => "document.pdf",
        "text/plain" => "document.txt",
        "text/csv" => "document.csv",
        "application/json" => "document.json",
        "application/xml" | "text/xml" => "document.xml",
        _ if lower.starts_with("video/") => "video",
        _ => "document",
    }
}
