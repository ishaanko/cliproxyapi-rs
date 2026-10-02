//! Antigravity response -> OpenAI Chat Completions response (Go: antigravity_openai_response.go).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cpa_core::util;
use cpa_json::{json, J, Res, Value};

use crate::antigravity::gemini::has_response_payload;
use crate::gemini::openai::chat_completions::convert_gemini_response_to_openai_non_stream;
use crate::registry::{Ctx, Param};

/// Per-stream conversion state.
#[derive(Default)]
struct StreamParams {
    unix_timestamp: i64,
    function_index: i64,
    /// Whether any upstream response chunk was seen.
    saw_response: bool,
    /// Whether any tool call was seen in the whole stream.
    saw_tool_call: bool,
    /// Whether finish_reason has been emitted.
    saw_finish_reason: bool,
    /// Upstream finish reason (uppercased), cached for the final chunk.
    upstream_finish_reason: String,
    model_version: String,
    response_id: String,
    pending_usage_metadata: Option<Value>,
    sanitized_name_map: Option<HashMap<String, String>>,
}

/// Process-wide counter for function call identifiers.
static FUNCTION_CALL_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Go: `ConvertAntigravityResponseToOpenAI`. Output chunks are bare JSON (no SSE framing); every
/// non-`[DONE]` input yields exactly one chunk.
pub fn convert_antigravity_response_to_openai(
    _ctx: &Ctx,
    _model: &str,
    original_request_raw_json: &[u8],
    _request_raw_json: &[u8],
    raw_json: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let params = param.state(StreamParams::default);
    if params.sanitized_name_map.is_none() {
        params.sanitized_name_map = Some(util::disambiguated_tool_name_map(original_request_raw_json));
    }

    if raw_json == b"[DONE]" {
        // Never finalize a stream that produced no response at all: an empty upstream body must
        // stay detectable as a failure rather than a successful empty completion.
        if params.saw_response && !params.saw_finish_reason {
            params.saw_finish_reason = true;
            let (finish_reason, native_finish_reason) = resolve_finish_reason(params);
            let mut template = json!({
                "id": "", "object": "chat.completion.chunk", "created": 0, "model": "model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop", "native_finish_reason": "stop"}]
            });
            cpa_json::set(&mut template, "created", params.unix_timestamp);
            if !params.model_version.is_empty() {
                cpa_json::set(&mut template, "model", params.model_version.clone());
            }
            if !params.response_id.is_empty() {
                cpa_json::set(&mut template, "id", params.response_id.clone());
            }
            if let Some(usage) = &params.pending_usage_metadata {
                set_usage_metadata(&mut template, &Res::of(usage));
            }
            cpa_json::set(&mut template, "choices.0.finish_reason", finish_reason);
            cpa_json::set(&mut template, "choices.0.native_finish_reason", native_finish_reason);
            return vec![cpa_json::to_vec(&template)];
        }
        return vec![];
    }

    let raw = cpa_json::parse(raw_json);
    if !params.saw_response {
        params.saw_response = has_response_payload(&raw);
    }

    let mut template = json!({
        "id": "", "object": "chat.completion.chunk", "created": 12345, "model": "model",
        "choices": [{
            "index": 0,
            "delta": {"role": null, "content": null, "reasoning_content": null, "tool_calls": null},
            "finish_reason": null, "native_finish_reason": null
        }]
    });

    let model_version = raw.g("response.modelVersion");
    if model_version.exists() {
        params.model_version = model_version.str();
        cpa_json::set(&mut template, "model", params.model_version.clone());
    }

    let create_time = raw.g("response.createTime");
    if create_time.exists() {
        if let Ok(t) = chrono::DateTime::parse_from_rfc3339(&create_time.str()) {
            params.unix_timestamp = t.timestamp();
        }
    }
    cpa_json::set(&mut template, "created", params.unix_timestamp);

    let response_id = raw.g("response.responseId");
    if response_id.exists() {
        params.response_id = response_id.str();
        cpa_json::set(&mut template, "id", params.response_id.clone());
    }

    // The finish reason is only cached; it is written on the final chunk.
    let finish_reason = raw.g("response.candidates.0.finishReason");
    if finish_reason.exists() {
        params.upstream_finish_reason = finish_reason.str().to_uppercase();
    }

    // FilterSSEUsageMetadata renames non-terminal usage to cpaUsageMetadata: keep the latest copy
    // for [DONE] instead of treating an intermediate chunk as terminal.
    let usage = raw.g("response.usageMetadata");
    if !usage.exists() {
        let pending = raw.g("response.cpaUsageMetadata");
        if pending.exists() {
            params.pending_usage_metadata = Some(pending.value());
        }
    } else {
        set_usage_metadata(&mut template, &usage);
    }

    let parts = raw.g("response.candidates.0.content.parts");
    if parts.is_array() {
        let name_map = params.sanitized_name_map.get_or_insert_with(HashMap::new);
        let part_raws = cpa_json::raw_children(raw_json, "response.candidates.0.content.parts");
        for (i, part) in parts.array().iter().enumerate() {
            let part_text = part.g("text");
            let function_call = part.g("functionCall");
            let mut thought_signature = part.g("thoughtSignature");
            if !thought_signature.exists() {
                thought_signature = part.g("thought_signature");
            }
            let mut inline_data = part.g("inlineData");
            if !inline_data.exists() {
                inline_data = part.g("inline_data");
            }

            let has_thought_signature = thought_signature.exists() && !thought_signature.str().is_empty();
            let has_content_payload = part_text.exists() || function_call.exists() || inline_data.exists();

            // The encrypted thoughtSignature is ignored, but content in the same part is kept.
            if has_thought_signature && !has_content_payload {
                continue;
            }

            if part_text.exists() {
                let text_content = part_text.str();
                if part.g("thought").bool() {
                    cpa_json::set(&mut template, "choices.0.delta.reasoning_content", text_content);
                } else {
                    cpa_json::set(&mut template, "choices.0.delta.content", text_content);
                }
                cpa_json::set(&mut template, "choices.0.delta.role", "assistant");
            } else if function_call.exists() {
                params.saw_tool_call = true;
                let tool_calls = template.g("choices.0.delta.tool_calls");
                let mut function_call_index = params.function_index;
                params.function_index += 1;
                if tool_calls.exists() && tool_calls.is_array() {
                    function_call_index = tool_calls.array().len() as i64;
                } else {
                    cpa_json::set(&mut template, "choices.0.delta.tool_calls", json!([]));
                }

                let mut function_call_template = json!({"id": "", "index": 0, "type": "function", "function": {"name": "", "arguments": ""}});
                let fc_name = util::restore_sanitized_tool_name(name_map, &function_call.g("name").str());
                let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
                let counter = FUNCTION_CALL_ID_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
                cpa_json::set(&mut function_call_template, "id", format!("{fc_name}-{nanos}-{counter}"));
                cpa_json::set(&mut function_call_template, "index", function_call_index);
                cpa_json::set(&mut function_call_template, "function.name", fc_name);
                let args = function_call.g("args");
                if args.exists() {
                    // Go copies `args.Raw`: keep the upstream text as sent.
                    let raw_args = crate::common::raw_in(part_raws.get(i), "functionCall.args");
                    cpa_json::set(&mut function_call_template, "function.arguments", raw_args.map_or_else(|| args.raw(), str::to_string));
                }
                cpa_json::set(&mut template, "choices.0.delta.role", "assistant");
                cpa_json::set(&mut template, "choices.0.delta.tool_calls.-1", function_call_template);
            } else if inline_data.exists() {
                let data = inline_data.g("data").str();
                if data.is_empty() {
                    continue;
                }
                let mut mime_type = inline_data.g("mimeType").str();
                if mime_type.is_empty() {
                    mime_type = inline_data.g("mime_type").str();
                }
                if mime_type.is_empty() {
                    mime_type = "image/png".to_string();
                }
                let image_url = format!("data:{mime_type};base64,{data}");
                let images = template.g("choices.0.delta.images");
                if !images.exists() || !images.is_array() {
                    cpa_json::set(&mut template, "choices.0.delta.images", json!([]));
                }
                let image_index = template.g("choices.0.delta.images").array().len();
                let mut image_payload = json!({"type": "image_url", "image_url": {"url": ""}});
                cpa_json::set(&mut image_payload, "index", image_index);
                cpa_json::set(&mut image_payload, "image_url.url", image_url);
                cpa_json::set(&mut template, "choices.0.delta.role", "assistant");
                cpa_json::set(&mut template, "choices.0.delta.images.-1", image_payload);
            }
        }
    }

    // finish_reason only on the final chunk, which upstream marks with both finishReason and usage
    // metadata. A usage-carrying chunk without finishReason is not terminal (upstream can split
    // the two and keep streaming); with no finishReason at all, [DONE] synthesizes the terminal chunk.
    let is_final_chunk = !params.upstream_finish_reason.is_empty() && raw.g("response.usageMetadata").exists();
    if is_final_chunk {
        let (finish_reason, native_finish_reason) = resolve_finish_reason(params);
        cpa_json::set(&mut template, "choices.0.finish_reason", finish_reason);
        cpa_json::set(&mut template, "choices.0.native_finish_reason", native_finish_reason);
        params.saw_finish_reason = true;
    }

    vec![cpa_json::to_vec(&template)]
}

/// Maps the cached upstream state to the OpenAI `finish_reason` / `native_finish_reason` pair; the
/// terminal upstream chunk and the synthesized `[DONE]` chunk must agree on it.
fn resolve_finish_reason(params: &StreamParams) -> (&'static str, String) {
    let finish_reason = if params.saw_tool_call {
        "tool_calls"
    } else if params.upstream_finish_reason == "MAX_TOKENS" {
        "max_tokens"
    } else {
        "stop"
    };
    let native = if params.upstream_finish_reason.is_empty() { "stop".to_string() } else { params.upstream_finish_reason.to_lowercase() };
    (finish_reason, native)
}

fn set_usage_metadata(template: &mut Value, usage: &Res<'_>) {
    let cached_token_count = usage.g("cachedContentTokenCount").int();
    cpa_json::set(template, "usage.completion_tokens", usage.g("candidatesTokenCount").int());
    let total = usage.g("totalTokenCount");
    if total.exists() {
        cpa_json::set(template, "usage.total_tokens", total.int());
    }
    let prompt_token_count = usage.g("promptTokenCount").int();
    let thoughts_token_count = usage.g("thoughtsTokenCount").int();
    cpa_json::set(template, "usage.prompt_tokens", prompt_token_count);
    if thoughts_token_count > 0 {
        cpa_json::set(template, "usage.completion_tokens_details.reasoning_tokens", thoughts_token_count);
    }
    if cached_token_count > 0 {
        cpa_json::set(template, "usage.prompt_tokens_details.cached_tokens", cached_token_count);
    }
}

/// Go: `ConvertAntigravityResponseToOpenAINonStream`: unwraps `response`, restores tool names and
/// delegates to the Gemini -> OpenAI converter. A body without `response` yields an empty body.
pub fn convert_antigravity_response_to_openai_non_stream(
    ctx: &Ctx,
    model: &str,
    original_request_raw_json: &[u8],
    request_raw_json: &[u8],
    raw_json: &[u8],
    param: &mut Param,
) -> Option<Vec<u8>> {
    if let Some(response) = cpa_json::raw_at(raw_json, "response") {
        // Keep the original text unless a tool name has to be restored.
        let response_json = restore_function_names(response.as_bytes(), original_request_raw_json);
        return convert_gemini_response_to_openai_non_stream(ctx, model, original_request_raw_json, request_raw_json, &response_json, param);
    }
    Some(vec![])
}

/// Restores the client's tool names in functionCall/functionResponse parts of every candidate.
fn restore_function_names(raw_bytes: &[u8], original_request_raw_json: &[u8]) -> Vec<u8> {
    let name_map = util::disambiguated_tool_name_map(original_request_raw_json);
    if name_map.is_empty() {
        return raw_bytes.to_vec();
    }
    let mut raw = cpa_json::parse(raw_bytes);
    let mut changed = false;
    let candidates = raw.g("candidates").array().len();
    for candidate_index in 0..candidates {
        let parts = raw.g(&format!("candidates.{candidate_index}.content.parts")).array().len();
        for part_index in 0..parts {
            for field in ["functionCall", "functionResponse"] {
                let path = format!("candidates.{candidate_index}.content.parts.{part_index}.{field}.name");
                let name_result = raw.g(&path);
                let name = name_result.str();
                if name.is_empty() {
                    continue;
                }
                let restored = util::restore_sanitized_tool_name(&name_map, &name);
                if name_result.is_string() && restored == name {
                    continue;
                }
                cpa_json::set(&mut raw, &path, restored);
                changed = true;
            }
        }
    }
    if changed { cpa_json::to_vec(&raw) } else { raw_bytes.to_vec() }
}
