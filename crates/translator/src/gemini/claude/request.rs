//! Claude request to Gemini request (Go: gemini/claude/gemini_claude_request.go).

use std::collections::HashMap;

use cpa_core::registry::lookup_model_info;
use cpa_core::signature::{gemini_replay_signature_or_bypass, SignatureBlockKind};
use cpa_core::util::{
    clean_json_schema_for_gemini_json_schema, convert_claude_tool_result_content,
    is_claude_code_attribution_system_text, sanitize_function_name,
};
use cpa_json::{json, Res, Value, J};

use super::raw::set_function_response_from_text;
use super::RawDoc;
use crate::common::{
    align_claude_tool_results, claude_message_system_reminder_text, join_raw_array,
    merge_adjacent_gemini_contents, reorder_gemini_user_parts, set_gemini_function_response_raw,
};
use crate::gemini::common::attach_default_safety_settings;

const GEMINI_CLAUDE_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

/// Converts a Claude Messages request into a Gemini request body.
pub fn convert_claude_request_to_gemini(model_name: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    convert(model_name, raw, stream, false)
}

/// Like [`convert_claude_request_to_gemini`] but keeps assistant thinking blocks (with replay
/// signatures or the bypass sentinel) for configured compatibility endpoints.
pub fn convert_claude_request_to_gemini_with_compat(model_name: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    convert(model_name, raw, stream, true)
}

fn text_part(text: &str) -> Value {
    json!({ "text": text })
}

fn content_with_parts(role: &str, parts: Vec<Value>) -> Value {
    json!({ "role": role, "parts": parts })
}

fn convert(model_name: &str, raw: &[u8], _stream: bool, preserve_empty_thinking_blocks: bool) -> Vec<u8> {
    let root = cpa_json::parse(raw);
    let raw_doc = RawDoc::new(raw);
    let mut out = json!({ "contents": [] });
    cpa_json::set(&mut out, "model", model_name);

    // system instruction
    let system = root.g("system");
    if system.is_array() {
        let mut system_parts = Vec::new();
        for prompt in system.array() {
            if prompt.g("type").str() == "text" {
                let text = prompt.g("text");
                if let Some(text) = text.as_str() {
                    if is_claude_code_attribution_system_text(text) {
                        continue;
                    }
                    system_parts.push(text_part(text));
                }
            }
        }
        if !system_parts.is_empty() {
            let mut instruction = json!({ "role": "user", "parts": [] });
            cpa_json::set(&mut instruction, "parts", Value::Array(system_parts));
            cpa_json::set(&mut out, "systemInstruction", instruction);
        }
    } else if let Some(text) = system.as_str().filter(|t| !is_claude_code_attribution_system_text(t)) {
        let instruction = json!({ "parts": [text_part(text)] });
        cpa_json::set(&mut out, "systemInstruction", instruction);
    }

    // contents
    let messages = root.g("messages");
    if messages.is_array() {
        let mut content_items: Vec<Value> = Vec::new();
        let mut tool_name_by_id: HashMap<String, String> = HashMap::new();
        let mut pending_tool_use_ids: Vec<String> = Vec::new();
        for (message_index, message) in messages.array().into_iter().enumerate() {
            let role_result = message.g("role");
            let Some(original_role) = role_result.as_str() else { continue };
            let mut preceding_tool_use_ids: Vec<String> = Vec::new();
            if original_role != "system" && original_role != "developer" {
                preceding_tool_use_ids = std::mem::take(&mut pending_tool_use_ids);
            }
            let role = match original_role {
                "assistant" => "model",
                "system" | "developer" => "user",
                other => other,
            };

            let mut part_items: Vec<Value> = Vec::new();
            let original_contents = message.g("content");
            let mut contents = message.g("content");
            if original_role == "system" || original_role == "developer" {
                if let Some(reminder) = claude_message_system_reminder_text(&contents) {
                    part_items.push(text_part(&reminder));
                    content_items.push(content_with_parts(role, part_items));
                }
                continue;
            }
            if contents.is_array() {
                if original_role == "user" {
                    contents = align_claude_tool_results(contents, &preceding_tool_use_ids);
                }
                for content in contents.array() {
                    match content.g("type").str().as_str() {
                        "text" => {
                            let text = content.g("text").str();
                            if text.is_empty() {
                                continue;
                            }
                            part_items.push(text_part(&text));
                        }
                        "thinking" => {
                            if !preserve_empty_thinking_blocks {
                                continue;
                            }
                            let signature = gemini_replay_signature_or_bypass(
                                &content.g("signature").str(),
                                SignatureBlockKind::GeminiModelPart,
                            );
                            part_items.push(json!({
                                "text": content.g("thinking").str(),
                                "thought": true,
                                "thoughtSignature": signature,
                            }));
                        }
                        "tool_use" => {
                            let mut function_name = content.g("name").str();
                            let tool_use_id = content.g("id").str();
                            if !tool_use_id.is_empty() && !function_name.is_empty() {
                                tool_name_by_id.insert(tool_use_id.clone(), function_name.clone());
                            }
                            function_name = sanitize_function_name(&function_name);
                            let function_args = content.g("input").str();
                            if cpa_json::valid(function_args.as_bytes())
                                && cpa_json::parse_str(&function_args).is_object()
                            {
                                let mut part = json!({
                                    "thoughtSignature": "",
                                    "functionCall": { "name": "", "args": {} },
                                });
                                cpa_json::set(&mut part, "thoughtSignature", GEMINI_CLAUDE_THOUGHT_SIGNATURE);
                                if !tool_use_id.is_empty() {
                                    cpa_json::set(&mut part, "functionCall.id", tool_use_id.as_str());
                                }
                                cpa_json::set(&mut part, "functionCall.name", function_name);
                                let _ = cpa_json::set_raw(&mut part, "functionCall.args", &function_args);
                                part_items.push(part);
                                if original_role == "assistant" {
                                    pending_tool_use_ids.push(tool_use_id);
                                }
                            }
                        }
                        "tool_result" => {
                            let tool_call_id = content.g("tool_use_id").str();
                            if tool_call_id.is_empty() {
                                continue;
                            }
                            let mut func_name = tool_name_by_id.get(&tool_call_id).cloned().unwrap_or_default();
                            if func_name.is_empty() {
                                func_name = tool_name_from_claude_tool_use_id(&tool_call_id);
                            }
                            if func_name.is_empty() {
                                func_name = tool_call_id.clone();
                            }
                            func_name = sanitize_function_name(&func_name);
                            let tool_result = convert_claude_tool_result_content(content.g("content").v());
                            let mut part = json!({ "functionResponse": { "name": "", "response": { "result": "" } } });
                            cpa_json::set(&mut part, "functionResponse.id", tool_call_id);
                            cpa_json::set(&mut part, "functionResponse.name", func_name);
                            if tool_result.result_is_raw {
                                // Results holding `$ref` are stored as text, so keep the source text.
                                let source_text = if tool_result.result.contains("$ref") {
                                    tool_result_source_text(&raw_doc, message_index, &original_contents, &content)
                                } else {
                                    None
                                };
                                if let Some(text) = source_text {
                                    set_function_response_from_text(&mut part, "functionResponse.response.result", &text);
                                } else {
                                    let bytes = set_gemini_function_response_raw(
                                        &cpa_json::to_vec(&part),
                                        "functionResponse.response.result",
                                        &tool_result.result,
                                    );
                                    part = cpa_json::parse(&bytes);
                                }
                            } else {
                                cpa_json::set(&mut part, "functionResponse.response.result", tool_result.result);
                            }
                            part_items.push(part);
                            for img in tool_result.images {
                                part_items.push(json!({
                                    "inline_data": { "mime_type": img.mime_type, "data": img.data },
                                }));
                            }
                        }
                        "image" => {
                            let source = content.g("source");
                            if source.g("type").str() != "base64" {
                                continue;
                            }
                            let mime_type = source.g("media_type").str();
                            let data = source.g("data").str();
                            if mime_type.is_empty() || data.is_empty() {
                                continue;
                            }
                            part_items.push(json!({ "inline_data": { "mime_type": mime_type, "data": data } }));
                        }
                        _ => {}
                    }
                }
                if role == "user" {
                    part_items = reorder_user_parts(part_items);
                }
                content_items.push(content_with_parts(role, part_items));
            } else if let Some(text) = contents.as_str() {
                part_items.push(text_part(text));
                content_items.push(content_with_parts(role, part_items));
            }
        }

        // Strip a trailing model turn with unanswered function calls.
        if let Some(last) = content_items.last() {
            if last.g("role").str() == "model"
                && last.g("parts").array().iter().any(|part| part.g("functionCall").exists())
            {
                content_items.pop();
            }
        }
        let items: Vec<Vec<u8>> = content_items.iter().map(cpa_json::to_vec).collect();
        let merged = merge_adjacent_gemini_contents(&items);
        // SetRawArrayItems is a no-op for an empty list, keeping the template's `contents`.
        if !merged.is_empty() {
            cpa_json::set(&mut out, "contents", cpa_json::parse(&join_raw_array(&merged)));
        }
    }

    // tools
    let mut tool_items: Vec<Value> = Vec::new();
    let mut has_strict_tool = false;
    let tools = root.g("tools");
    if tools.is_array() {
        for tool in tools.array() {
            if tool.g("strict").v() == Some(&Value::Bool(true)) {
                has_strict_tool = true;
            }
            let input_schema = tool.g("input_schema");
            if !(input_schema.exists() && input_schema.is_object()) {
                continue;
            }
            let schema = clean_json_schema_for_gemini_json_schema(&input_schema.raw());
            let mut tool_value = tool.value();
            cpa_json::delete(&mut tool_value, "input_schema");
            if cpa_json::set_raw(&mut tool_value, "parametersJsonSchema", &schema).is_err() {
                continue;
            }
            for path in ["strict", "input_examples", "type", "cache_control", "defer_loading", "eager_input_streaming"] {
                if tool.g(path).exists() {
                    cpa_json::delete(&mut tool_value, path);
                }
            }
            let name_result = tool.g("name");
            let original_name = name_result.str();
            let sanitized_name = sanitize_function_name(&original_name);
            if !name_result.is_string() || sanitized_name != original_name {
                cpa_json::set(&mut tool_value, "name", sanitized_name);
            }
            if tool_value.is_object() {
                tool_items.push(tool_value);
            }
        }
        if !tool_items.is_empty() {
            let mut tools_out = json!([{ "functionDeclarations": [] }]);
            cpa_json::set(&mut tools_out, "0.functionDeclarations", Value::Array(tool_items.clone()));
            cpa_json::set(&mut out, "tools", tools_out);
        }
    }

    // tool_choice
    let tool_choice = root.g("tool_choice");
    let mode_path = "toolConfig.functionCallingConfig.mode";
    if tool_choice.exists() && !tool_choice.is_null() {
        let mut tool_choice_type = String::new();
        let mut tool_choice_name = String::new();
        if tool_choice.is_object() {
            tool_choice_type = tool_choice.g("type").str();
            tool_choice_name = tool_choice.g("name").str();
        } else if let Some(s) = tool_choice.as_str() {
            tool_choice_type = s.to_string();
        }
        match tool_choice_type.as_str() {
            "auto" => {
                cpa_json::set(&mut out, mode_path, if has_strict_tool { "VALIDATED" } else { "AUTO" });
            }
            "none" => {
                cpa_json::set(&mut out, mode_path, "NONE");
            }
            "any" => {
                cpa_json::set(&mut out, mode_path, "ANY");
            }
            "tool" => {
                cpa_json::set(&mut out, mode_path, "ANY");
                if !tool_choice_name.is_empty() {
                    cpa_json::set(
                        &mut out,
                        "toolConfig.functionCallingConfig.allowedFunctionNames",
                        json!([sanitize_function_name(&tool_choice_name)]),
                    );
                }
            }
            _ => {}
        }
    } else if has_strict_tool && !tool_items.is_empty() {
        cpa_json::set(&mut out, mode_path, "VALIDATED");
    }

    // Map Anthropic thinking -> Gemini thinking config when enabled. Capability validation
    // is left to ApplyThinking.
    let thinking = root.g("thinking");
    if thinking.exists() && thinking.is_object() {
        match thinking.g("type").str().as_str() {
            "enabled" => {
                let budget = thinking.g("budget_tokens");
                if budget.is_number() {
                    cpa_json::set(&mut out, "generationConfig.thinkingConfig.thinkingBudget", budget.int());
                }
            }
            "adaptive" | "auto" => {
                // With an explicit output_config.effort pass it through as thinkingLevel;
                // otherwise treat as "enabled with target-model maximum".
                let mut effort = String::new();
                let v = root.g("output_config.effort");
                if let Some(s) = v.as_str() {
                    effort = s.trim().to_lowercase();
                }
                if !effort.is_empty() {
                    cpa_json::set(&mut out, "generationConfig.thinkingConfig.thinkingLevel", effort);
                } else {
                    let max_budget = lookup_model_info(model_name, Some("gemini"))
                        .and_then(|mi| mi.thinking.map(|t| t.max))
                        .unwrap_or(0);
                    if max_budget > 0 {
                        cpa_json::set(&mut out, "generationConfig.thinkingConfig.thinkingBudget", max_budget);
                    } else {
                        cpa_json::set(&mut out, "generationConfig.thinkingConfig.thinkingLevel", "high");
                    }
                }
            }
            _ => {}
        }
    }
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

    attach_default_safety_settings(&cpa_json::to_vec(&out), "safetySettings")
}

fn is_base64_image(block: &Value) -> bool {
    block.g("type").str() == "image" && block.g("source.type").str() == "base64"
}

/// Original text of a tool_result block's normalized result, as Go's `Raw` copies would carry it:
/// the single non-image block, the `[a,b]` join of several, or the content itself. `original` is
/// the message content before tool results were reordered.
fn tool_result_source_text(
    doc: &RawDoc<'_>,
    message_index: usize,
    original: &Res<'_>,
    element: &Res<'_>,
) -> Option<String> {
    let value = element.value();
    let position = original.array().iter().position(|e| e.v() == Some(&value))?;
    let content_text = doc.at(&format!("messages.{message_index}.content.{position}.content"))?;
    let Some(Value::Array(blocks)) = value.get("content") else {
        return Some(content_text.to_string());
    };
    let inner = RawDoc::new(content_text.as_bytes());
    let texts: Vec<&str> = blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| !is_base64_image(block))
        .filter_map(|(k, _)| inner.at(&k.to_string()))
        .collect();
    match texts.len() {
        0 => None,
        1 => Some(texts[0].to_string()),
        _ => Some(format!("[{}]", texts.join(","))),
    }
}

/// Reorders a user turn's parts (text before functionResponse) via the shared byte helper.
fn reorder_user_parts(parts: Vec<Value>) -> Vec<Value> {
    let bytes = parts.iter().map(cpa_json::to_vec).collect();
    reorder_gemini_user_parts(bytes).iter().map(|p| cpa_json::parse(p)).collect()
}

/// Tool name encoded in a Gemini-generated Claude tool_use id (`<name>-<counter>`).
fn tool_name_from_claude_tool_use_id(tool_use_id: &str) -> String {
    let parts: Vec<&str> = tool_use_id.split('-').collect();
    if parts.len() <= 1 {
        return String::new();
    }
    parts[..parts.len() - 1].join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPTURED_GEMINI_SIGNATURE: &str = "EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA";

    fn thinking_payload(signature: &str) -> Vec<u8> {
        format!(
            r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"reason","signature":"{signature}"}}]}}]}}"#
        )
        .into_bytes()
    }

    #[test]
    fn compat_maps_signatures() {
        let cases = [
            (format!("gemini#{CAPTURED_GEMINI_SIGNATURE}"), CAPTURED_GEMINI_SIGNATURE),
            ("claude#opaque-signature-12345".to_string(), GEMINI_CLAUDE_THOUGHT_SIGNATURE),
            (String::new(), GEMINI_CLAUDE_THOUGHT_SIGNATURE),
        ];
        for (signature, want) in cases {
            let out = cpa_json::parse(&convert_claude_request_to_gemini_with_compat(
                "deepseek-v4",
                &thinking_payload(&signature),
                false,
            ));
            let part = out.g("contents.0.parts.0");
            assert!(part.g("thought").bool() && part.g("text").str() == "reason", "{out}");
            assert_eq!(part.g("thoughtSignature").str(), want, "{out}");
        }
    }

    #[test]
    fn default_conversion_drops_thinking_blocks() {
        let out = cpa_json::parse(&convert_claude_request_to_gemini("deepseek-v4", &thinking_payload(""), false));
        assert_eq!(out.g("contents.0.parts.#").int(), 0, "{out}");
    }
}
