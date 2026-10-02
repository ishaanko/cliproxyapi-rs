//! Codex Responses -> Gemini response (Go: codex_gemini_response.go).

use std::collections::HashMap;

use cpa_json::{json, Res, Value, J};
use sha2::{Digest, Sha256};

use super::request::short_name_map_from_tools;
use crate::codex::util::{mime_type_from_output_format, rfc3339_local, reverse_map};
use crate::common::gemini_token_count_json;
use crate::registry::{Ctx, Param};

/// Per-stream state (Go: ConvertCodexResponseToGeminiParams).
struct State {
    model: String,
    created_at: i64,
    response_id: String,
    /// A completed function call held back until the next event so both go out together.
    last_storage_output: Option<Vec<u8>>,
    has_output_text_delta: bool,
    last_image_hash_by_id: HashMap<String, [u8; 32]>,
}

/// Go: ConvertCodexResponseToGemini. One `data:` line in, zero to two Gemini chunks out.
pub fn convert_codex_response_to_gemini(
    _ctx: &Ctx,
    model_name: &str,
    original_request: &[u8],
    _request: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let params = param.state(|| State {
        model: model_name.to_string(),
        created_at: 0,
        response_id: String::new(),
        last_storage_output: None,
        has_output_text_delta: false,
        last_image_hash_by_id: HashMap::new(),
    });

    if !raw.starts_with(b"data:") {
        return vec![];
    }
    let raw = raw[5..].trim_ascii();
    let root = cpa_json::parse(raw);
    let type_str = root.g("type").str();

    // Base Gemini response template.
    let mut template = cpa_json::parse_str(
        r#"{"candidates":[{"content":{"role":"model","parts":[]}}],"usageMetadata":{"trafficType":"PROVISIONED_THROUGHPUT"},"modelVersion":"gemini-2.5-pro","createTime":"2025-08-15T02:52:03.884209Z","responseId":"06CeaPH7NaCU48APvNXDyA4"}"#,
    );
    cpa_json::set(&mut template, "modelVersion", params.model.clone());
    let created_at = root.g("response.created_at");
    if created_at.exists() {
        params.created_at = created_at.int();
        cpa_json::set(&mut template, "createTime", rfc3339_local(params.created_at));
    }
    cpa_json::set(&mut template, "responseId", params.response_id.clone());

    if type_str == "response.image_generation_call.partial_image" {
        let item_id = root.g("item_id").str();
        let b64 = root.g("partial_image_b64").str();
        if b64.is_empty() {
            return vec![];
        }
        if is_duplicate_image(&mut params.last_image_hash_by_id, &item_id, &b64) {
            return vec![];
        }
        let mime_type = mime_type_from_output_format(&root.g("output_format").str());
        set_parts(&mut template, vec![inline_image_part(&b64, &mime_type)]);
        return vec![cpa_json::to_vec(&template)];
    }

    if type_str == "response.output_item.done" {
        let item = root.g("item");
        let item_type = item.g("type").str();
        if item_type == "image_generation_call" {
            let item_id = item.g("id").str();
            let b64 = item.g("result").str();
            if b64.is_empty() {
                return vec![];
            }
            if is_duplicate_image(&mut params.last_image_hash_by_id, &item_id, &b64) {
                return vec![];
            }
            let mime_type = mime_type_from_output_format(&item.g("output_format").str());
            set_parts(&mut template, vec![inline_image_part(&b64, &mime_type)]);
            return vec![cpa_json::to_vec(&template)];
        }
        if item_type == "function_call" {
            let function_call = function_call_part(&item, original_request, r#"{"functionCall":{"name":"","args":{}}}"#);
            set_parts(&mut template, vec![function_call]);
            cpa_json::set(&mut template, "candidates.0.finishReason", "STOP");
            // Hold the call back: it is emitted ahead of the next chunk.
            params.last_storage_output = Some(cpa_json::to_vec(&template));
            return vec![];
        }
    }

    if type_str == "response.created" {
        cpa_json::set(&mut template, "modelVersion", root.g("response.model").str());
        cpa_json::set(&mut template, "responseId", root.g("response.id").str());
        params.response_id = root.g("response.id").str();
    } else if type_str == "response.reasoning_summary_text.delta" {
        set_parts(&mut template, vec![json!({"thought": true, "text": root.g("delta").str()})]);
    } else if type_str == "response.output_text.delta" {
        params.has_output_text_delta = true;
        set_parts(&mut template, vec![json!({"text": root.g("delta").str()})]);
    } else if type_str == "response.output_item.done" {
        // Fallback: emit final message text when no delta chunks were received.
        let item = root.g("item");
        if item.g("type").str() != "message" || params.has_output_text_delta {
            return vec![];
        }
        let content = item.g("content");
        if !content.exists() || !content.is_array() {
            return vec![];
        }
        let mut wrote_text = false;
        for part in content.array() {
            if part.g("type").str() != "output_text" {
                continue;
            }
            let text = part.g("text").str();
            if text.is_empty() {
                continue;
            }
            cpa_json::set(&mut template, "candidates.0.content.parts.-1", json!({"text": text}));
            wrote_text = true;
        }
        if wrote_text {
            params.has_output_text_delta = true;
            return vec![cpa_json::to_vec(&template)];
        }
        return vec![];
    } else if type_str == "response.completed" || type_str == "response.incomplete" {
        let input = root.g("response.usage.input_tokens").int();
        let output = root.g("response.usage.output_tokens").int();
        cpa_json::set(&mut template, "usageMetadata.promptTokenCount", input);
        cpa_json::set(&mut template, "usageMetadata.candidatesTokenCount", output);
        cpa_json::set(&mut template, "usageMetadata.totalTokenCount", input + output);
        if type_str == "response.incomplete" {
            let reason = root.g("response.incomplete_details.reason").str();
            cpa_json::set(&mut template, "candidates.0.finishReason", incomplete_finish_reason(&reason));
        }
    } else {
        return vec![];
    }

    let current = cpa_json::to_vec(&template);
    match params.last_storage_output.take() {
        Some(stored) if !stored.is_empty() => vec![stored, current],
        _ => vec![current],
    }
}

/// Records the image hash for `item_id`; true when the same image was already emitted.
fn is_duplicate_image(hashes: &mut HashMap<String, [u8; 32]>, item_id: &str, b64: &str) -> bool {
    if item_id.is_empty() {
        return false;
    }
    let hash: [u8; 32] = Sha256::digest(b64.as_bytes()).into();
    if hashes.get(item_id) == Some(&hash) {
        return true;
    }
    hashes.insert(item_id.to_string(), hash);
    false
}

fn inline_image_part(b64: &str, mime_type: &str) -> Value {
    json!({"inlineData": {"data": b64, "mimeType": mime_type}})
}

fn set_parts(template: &mut Value, parts: Vec<Value>) {
    cpa_json::set(template, "candidates.0.content.parts", Value::Array(parts));
}

/// Builds a functionCall part from a Codex function_call item, restoring shortened tool names.
fn function_call_part(item: &Res<'_>, original_request: &[u8], template: &str) -> Value {
    let mut function_call = cpa_json::parse_str(template);
    let mut n = item.g("name").str();
    let rev = reverse_map(short_name_map_from_tools(&cpa_json::parse(original_request)));
    if let Some(orig) = rev.get(&n) {
        n = orig.clone();
    }
    cpa_json::set(&mut function_call, "functionCall.name", n);

    // Arguments are forwarded only when they parse as a JSON object.
    let args_str = item.g("arguments").str();
    if !args_str.is_empty() {
        let args = cpa_json::parse_str(&args_str);
        if args.is_object() {
            cpa_json::set(&mut function_call, "functionCall.args", args);
        }
    }
    set_function_call_id(&mut function_call, item);
    function_call
}

fn set_function_call_id(function_call: &mut Value, item: &Res<'_>) {
    let call_id = item.g("call_id").str().trim().to_string();
    if !call_id.is_empty() {
        cpa_json::set(function_call, "functionCall.id", call_id);
        return;
    }
    let id = item.g("id").str().trim().to_string();
    if !id.is_empty() {
        cpa_json::set(function_call, "functionCall.id", id);
    }
}

fn incomplete_finish_reason(reason: &str) -> &'static str {
    match reason {
        "max_tokens" | "max_output_tokens" => "MAX_TOKENS",
        "content_filter" => "SAFETY",
        _ => "OTHER",
    }
}

/// Go: ConvertCodexResponseToGeminiNonStream. Builds one Gemini response from a terminal event.
pub fn convert_codex_response_to_gemini_non_stream(
    _ctx: &Ctx,
    model_name: &str,
    original_request: &[u8],
    _request: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let root = cpa_json::parse(raw);
    let response_type = root.g("type").str();
    if response_type != "response.completed" && response_type != "response.incomplete" {
        return Some(Vec::new());
    }

    let mut template = cpa_json::parse_str(
        r#"{"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP"}],"usageMetadata":{"trafficType":"PROVISIONED_THROUGHPUT"},"modelVersion":"","createTime":"","responseId":""}"#,
    );
    cpa_json::set(&mut template, "modelVersion", model_name);

    let data = root.g("response");
    if data.exists() {
        if response_type == "response.incomplete" {
            let reason = data.g("incomplete_details.reason").str();
            cpa_json::set(&mut template, "candidates.0.finishReason", incomplete_finish_reason(&reason));
        }
        let id = data.g("id");
        if id.exists() {
            cpa_json::set(&mut template, "responseId", id.str());
        }
        let created_at = data.g("created_at");
        if created_at.exists() {
            cpa_json::set(&mut template, "createTime", rfc3339_local(created_at.int()));
        }
        let usage = data.g("usage");
        if usage.exists() {
            let input = usage.g("input_tokens").int();
            let output = usage.g("output_tokens").int();
            cpa_json::set(&mut template, "usageMetadata.promptTokenCount", input);
            cpa_json::set(&mut template, "usageMetadata.candidatesTokenCount", output);
            cpa_json::set(&mut template, "usageMetadata.totalTokenCount", input + output);
        }

        let mut parts: Vec<Value> = Vec::new();
        let output = data.g("output");
        if output.exists() && output.is_array() {
            for value in output.array() {
                match value.g("type").str().as_str() {
                    "reasoning" => {
                        let content = value.g("content");
                        if content.exists() {
                            parts.push(json!({"text": content.str(), "thought": true}));
                        }
                    }
                    "message" => {
                        let content = value.g("content");
                        if content.exists() && content.is_array() {
                            for item in content.array() {
                                if item.g("type").str() == "output_text" {
                                    let text = item.g("text");
                                    if text.exists() {
                                        parts.push(json!({"text": text.str()}));
                                    }
                                }
                            }
                        }
                    }
                    "image_generation_call" => {
                        let b64 = value.g("result").str();
                        if b64.is_empty() {
                            continue;
                        }
                        let mime_type = mime_type_from_output_format(&value.g("output_format").str());
                        parts.push(inline_image_part(&b64, &mime_type));
                    }
                    "function_call" => {
                        // Consecutive calls stay grouped in output order.
                        parts.push(function_call_part(&value, original_request, r#"{"functionCall":{"args":{},"name":""}}"#));
                    }
                    _ => {}
                }
            }
            if !parts.is_empty() {
                set_parts(&mut template, parts);
            }
        }
    }
    Some(cpa_json::to_vec(&template))
}

/// Go: GeminiTokenCount.
pub fn gemini_token_count(_ctx: &Ctx, count: i64) -> Vec<u8> {
    gemini_token_count_json(count)
}
