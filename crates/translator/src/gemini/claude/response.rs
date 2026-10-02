//! Gemini response to Claude response (Go: gemini/claude/gemini_claude_response.go).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use cpa_core::util::{
    map_tool_name, restore_sanitized_tool_name, sanitize_claude_tool_id, sanitized_tool_name_map,
    tool_name_map_from_claude_request,
};
use cpa_json::{json, Res, Value, J};

use crate::common::{append_sse_event_string, claude_input_tokens_json};
use crate::registry::{Ctx, Param};

/// Per-stream conversion state.
pub struct Params {
    pub has_first_response: bool,
    /// 0 = none, 1 = content, 2 = thinking, 3 = function.
    pub response_type: i32,
    pub response_index: i64,
    /// Whether any content (text, thinking, or tool use) has been output.
    pub has_content: bool,
    pub tool_name_map: HashMap<String, String>,
    pub sanitized_name_map: HashMap<String, String>,
    pub saw_tool_call: bool,
    pub has_final_events: bool,
}

/// Process-wide counter for tool use identifiers.
static TOOL_USE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn event(output: &mut Vec<u8>, name: &str, payload: &Value) {
    append_sse_event_string(output, name, &cpa_json::to_string(payload), 3);
}

fn delta_event(index: i64, delta: Value) -> Value {
    json!({ "type": "content_block_delta", "index": index, "delta": delta })
}

fn stop_event(index: i64) -> Value {
    json!({ "type": "content_block_stop", "index": index })
}

fn start_event(index: i64, block: Value) -> Value {
    json!({ "type": "content_block_start", "index": index, "content_block": block })
}

/// Part's thought signature (`thoughtSignature`, falling back to `thought_signature`).
fn part_signature<'a>(part: &'a Res<'_>) -> Res<'a> {
    let sig = part.g("thoughtSignature");
    if sig.exists() { sig } else { part.g("thought_signature") }
}

/// Source text of a part's `functionCall.args` (Go's `Raw`), falling back to the compact form.
fn args_raw(src: &[u8], part_index: usize, args: &Res<'_>) -> String {
    cpa_json::raw_at(src, &format!("candidates.0.content.parts.{part_index}.functionCall.args"))
        .map(str::to_string)
        .unwrap_or_else(|| args.raw())
}

/// Translates one Gemini streaming chunk into Claude SSE events (one output buffer per call).
pub fn convert_gemini_response_to_claude(
    _ctx: &Ctx,
    _model: &str,
    original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let p = param.state(|| Params {
        has_first_response: false,
        response_type: 0,
        response_index: 0,
        has_content: false,
        tool_name_map: tool_name_map_from_claude_request(original),
        sanitized_name_map: sanitized_tool_name_map(original),
        saw_tool_call: false,
        has_final_events: false,
    });

    if raw == b"[DONE]" {
        // Only send message_stop if we have actually output content.
        if p.has_content {
            let mut out = Vec::new();
            append_sse_event_string(&mut out, "message_stop", r#"{"type":"message_stop"}"#, 3);
            return vec![out];
        }
        return vec![];
    }

    let root = cpa_json::parse(raw);
    let mut output: Vec<u8> = Vec::with_capacity(1024);

    // Initialize the streaming session with a message_start event (first chunk only).
    if !p.has_first_response {
        let mut message_start = cpa_json::parse_str(
            r#"{"type":"message_start","message":{"id":"msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY","type":"message","role":"assistant","content":[],"model":"claude-3-5-sonnet-20241022","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}"#,
        );
        let model_version = root.g("modelVersion");
        if model_version.exists() {
            cpa_json::set(&mut message_start, "message.model", model_version.str());
        }
        let response_id = root.g("responseId");
        if response_id.exists() {
            cpa_json::set(&mut message_start, "message.id", response_id.str());
        }
        event(&mut output, "message_start", &message_start);
        p.has_first_response = true;
    }

    let signature_delta = |p: &mut Params, output: &mut Vec<u8>, signature: &str| {
        if signature.is_empty() || p.response_type != 2 {
            return;
        }
        let data = delta_event(p.response_index, json!({ "type": "signature_delta", "signature": signature }));
        event(output, "content_block_delta", &data);
        p.has_content = true;
    };

    // Each part can contain text content, thinking content, or function calls.
    let parts = root.g("candidates.0.content.parts");
    if parts.is_array() {
        for (part_index, part) in parts.array().into_iter().enumerate() {
            let part_text = part.g("text");
            let function_call = part.g("functionCall");
            let thought_signature = part_signature(&part);
            let has_thought_signature = thought_signature.exists() && !thought_signature.str().is_empty();

            if has_thought_signature && !part_text.exists() && !function_call.exists() {
                signature_delta(p, &mut output, &thought_signature.str());
                continue;
            }

            if part_text.exists() {
                if part.g("thought").bool() || has_thought_signature {
                    if has_thought_signature && part_text.str().is_empty() {
                        signature_delta(p, &mut output, &thought_signature.str());
                        continue;
                    }
                    if p.response_type == 2 {
                        // Continue the existing thinking block.
                        let data = delta_event(p.response_index, json!({ "type": "thinking_delta", "thinking": part_text.str() }));
                        event(&mut output, "content_block_delta", &data);
                        p.has_content = true;
                    } else {
                        // Close any existing content block, then start a thinking block.
                        if p.response_type != 0 {
                            event(&mut output, "content_block_stop", &stop_event(p.response_index));
                            p.response_index += 1;
                        }
                        event(
                            &mut output,
                            "content_block_start",
                            &start_event(p.response_index, json!({ "type": "thinking", "thinking": "" })),
                        );
                        let data = delta_event(p.response_index, json!({ "type": "thinking_delta", "thinking": part_text.str() }));
                        event(&mut output, "content_block_delta", &data);
                        p.response_type = 2;
                        p.has_content = true;
                    }
                    signature_delta(p, &mut output, &thought_signature.str());
                } else if p.response_type == 1 {
                    // Continue the existing text block.
                    let data = delta_event(p.response_index, json!({ "type": "text_delta", "text": part_text.str() }));
                    event(&mut output, "content_block_delta", &data);
                    p.has_content = true;
                } else {
                    // Close any existing content block, then start a text block.
                    if p.response_type != 0 {
                        event(&mut output, "content_block_stop", &stop_event(p.response_index));
                        p.response_index += 1;
                    }
                    event(
                        &mut output,
                        "content_block_start",
                        &start_event(p.response_index, json!({ "type": "text", "text": "" })),
                    );
                    let data = delta_event(p.response_index, json!({ "type": "text_delta", "text": part_text.str() }));
                    event(&mut output, "content_block_delta", &data);
                    p.response_type = 1;
                    p.has_content = true;
                }
            } else if function_call.exists() {
                p.saw_tool_call = true;
                let mut upstream_tool_name = function_call.g("name").str();
                upstream_tool_name = restore_sanitized_tool_name(&p.sanitized_name_map, &upstream_tool_name);
                let client_tool_name = map_tool_name(&p.tool_name_map, &upstream_tool_name);

                // Streaming split/delta: an empty name while already in tool use mode is a
                // continuation of the arguments.
                if p.response_type == 3 && upstream_tool_name.is_empty() {
                    let args = function_call.g("args");
                    if args.exists() {
                        let data = delta_event(
                            p.response_index,
                            json!({ "type": "input_json_delta", "partial_json": args_raw(raw, part_index, &args) }),
                        );
                        event(&mut output, "content_block_delta", &data);
                    }
                    continue;
                }

                // Close any existing function call block first.
                if p.response_type == 3 {
                    event(&mut output, "content_block_stop", &stop_event(p.response_index));
                    p.response_index += 1;
                    p.response_type = 0;
                }

                // Close any other existing content block.
                if p.response_type != 0 {
                    event(&mut output, "content_block_stop", &stop_event(p.response_index));
                    p.response_index += 1;
                }

                // Start a new tool use content block.
                let id = sanitize_claude_tool_id(&format!(
                    "{}-{}",
                    upstream_tool_name,
                    TOOL_USE_ID_COUNTER.fetch_add(1, Ordering::SeqCst) + 1
                ));
                let data = start_event(
                    p.response_index,
                    json!({ "type": "tool_use", "id": id, "name": client_tool_name, "input": {} }),
                );
                event(&mut output, "content_block_start", &data);

                let args = function_call.g("args");
                if args.exists() {
                    let data = delta_event(
                        p.response_index,
                        json!({ "type": "input_json_delta", "partial_json": args_raw(raw, part_index, &args) }),
                    );
                    event(&mut output, "content_block_delta", &data);
                }
                p.response_type = 3;
                p.has_content = true;
            }
        }
    }

    let usage = root.g("usageMetadata");
    const FINISH_REASON_KEY: &[u8] = br#""finishReason""#;
    let has_finish_reason = raw.windows(FINISH_REASON_KEY.len()).any(|w| w == FINISH_REASON_KEY);
    if usage.exists() && has_finish_reason && !p.has_final_events {
        // Only send final events if we have actually output content.
        if p.has_content {
            if p.response_type != 0 {
                event(&mut output, "content_block_stop", &stop_event(p.response_index));
                p.response_type = 0;
            }

            let stop_reason = if p.saw_tool_call {
                "tool_use"
            } else if root.g("candidates.0.finishReason").exists() && root.g("candidates.0.finishReason").str() == "MAX_TOKENS" {
                "max_tokens"
            } else {
                "end_turn"
            };
            let mut template = json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason, "stop_sequence": null },
                "usage": { "input_tokens": 0, "output_tokens": 0 },
            });

            let thoughts = usage.g("thoughtsTokenCount").int();
            let candidates = usage.g("candidatesTokenCount").int();
            let cached = usage.g("cachedContentTokenCount").int();
            let prompt = (usage.g("promptTokenCount").int() - cached).max(0);
            cpa_json::set(&mut template, "usage.output_tokens", candidates + thoughts);
            cpa_json::set(&mut template, "usage.input_tokens", prompt);
            if cached > 0 {
                cpa_json::set(&mut template, "usage.cache_read_input_tokens", cached);
            }

            event(&mut output, "message_delta", &template);
            p.has_final_events = true;
        }
    }

    vec![output]
}

/// Converts a complete Gemini response into a Claude message.
pub fn convert_gemini_response_to_claude_non_stream(
    _ctx: &Ctx,
    _model: &str,
    original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let root = cpa_json::parse(raw);
    let tool_name_map = tool_name_map_from_claude_request(original);
    let sanitized_name_map = sanitized_tool_name_map(original);

    let mut out = cpa_json::parse_str(
        r#"{"id":"","type":"message","role":"assistant","model":"","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}"#,
    );
    cpa_json::set(&mut out, "id", root.g("responseId").str());
    cpa_json::set(&mut out, "model", root.g("modelVersion").str());

    let cached_tokens = root.g("usageMetadata.cachedContentTokenCount").int();
    let input_tokens = (root.g("usageMetadata.promptTokenCount").int() - cached_tokens).max(0);
    let output_tokens = root.g("usageMetadata.candidatesTokenCount").int() + root.g("usageMetadata.thoughtsTokenCount").int();
    cpa_json::set(&mut out, "usage.input_tokens", input_tokens);
    cpa_json::set(&mut out, "usage.output_tokens", output_tokens);
    if cached_tokens > 0 {
        cpa_json::set(&mut out, "usage.cache_read_input_tokens", cached_tokens);
    }

    let parts = root.g("candidates.0.content.parts");
    let mut text = String::new();
    let mut thinking = String::new();
    let mut thinking_signature = String::new();
    let mut tool_id_counter = 0;
    let mut has_tool_call = false;
    let mut blocks: Vec<Value> = Vec::new();

    fn flush_text(text: &mut String, blocks: &mut Vec<Value>) {
        if text.is_empty() {
            return;
        }
        blocks.push(json!({ "type": "text", "text": std::mem::take(text) }));
    }
    fn flush_thinking(thinking: &mut String, signature: &mut String, blocks: &mut Vec<Value>) {
        if thinking.is_empty() && signature.is_empty() {
            return;
        }
        let mut block = json!({ "type": "thinking", "thinking": std::mem::take(thinking) });
        if !signature.is_empty() {
            cpa_json::set(&mut block, "signature", signature.as_str());
        }
        blocks.push(block);
        signature.clear();
    }

    if parts.is_array() {
        for part in parts.array() {
            let thought_signature = part_signature(&part);
            let has_thought_signature = thought_signature.exists() && !thought_signature.str().is_empty();
            if has_thought_signature {
                thinking_signature = thought_signature.str();
            }

            let part_text = part.g("text");
            let function_call = part.g("functionCall");

            if has_thought_signature && (!part_text.exists() || part_text.str().is_empty()) && !function_call.exists() {
                continue;
            }

            if part_text.exists() && !part_text.str().is_empty() {
                if part.g("thought").bool() || has_thought_signature {
                    flush_text(&mut text, &mut blocks);
                    thinking.push_str(&part_text.str());
                    continue;
                }
                flush_thinking(&mut thinking, &mut thinking_signature, &mut blocks);
                text.push_str(&part_text.str());
                continue;
            }

            if function_call.exists() {
                flush_thinking(&mut thinking, &mut thinking_signature, &mut blocks);
                flush_text(&mut text, &mut blocks);
                has_tool_call = true;

                let upstream_tool_name = restore_sanitized_tool_name(&sanitized_name_map, &function_call.g("name").str());
                let client_tool_name = map_tool_name(&tool_name_map, &upstream_tool_name);
                tool_id_counter += 1;
                let args = function_call.g("args");
                let input = if args.exists() && args.is_object() { args.value() } else { json!({}) };
                blocks.push(json!({
                    "type": "tool_use",
                    "id": sanitize_claude_tool_id(&format!("{upstream_tool_name}-{tool_id_counter}")),
                    "name": client_tool_name,
                    "input": input,
                }));
            }
        }
    }

    flush_thinking(&mut thinking, &mut thinking_signature, &mut blocks);
    flush_text(&mut text, &mut blocks);

    if !blocks.is_empty() {
        cpa_json::set(&mut out, "content", Value::Array(blocks));
    }

    let mut stop_reason = "end_turn";
    if has_tool_call {
        stop_reason = "tool_use";
    } else {
        let finish = root.g("candidates.0.finishReason");
        if finish.exists() && finish.str() == "MAX_TOKENS" {
            stop_reason = "max_tokens";
        }
    }
    cpa_json::set(&mut out, "stop_reason", stop_reason);

    if input_tokens == 0 && output_tokens == 0 && !root.g("usageMetadata").exists() {
        cpa_json::delete(&mut out, "usage");
    }

    Some(cpa_json::to_vec(&out))
}

/// Claude `count_tokens` response for a Gemini token count.
pub fn claude_token_count(_ctx: &Ctx, count: i64) -> Vec<u8> {
    claude_input_tokens_json(count)
}
