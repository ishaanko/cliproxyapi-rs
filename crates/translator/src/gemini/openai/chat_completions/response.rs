//! Gemini response to OpenAI Chat Completions response
//! (Go: gemini/openai/chat-completions/gemini_openai_response.go).

use crate::common::{args_raw, parse_create_time, unix_nano_now};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use cpa_core::util::{restore_sanitized_tool_name, sanitized_tool_name_map};
use cpa_json::{json, Res, Value, J};

use crate::registry::{Ctx, Param};

mod fast;
#[cfg(test)]
mod fast_tests;

/// Per-stream conversion state.
struct ChatParams {
    unix_timestamp: i64,
    /// Tool call indices per candidate index, to support multiple candidates.
    function_index: HashMap<i64, i64>,
    saw_tool_call: HashMap<i64, bool>,
    upstream_finish_reason: HashMap<i64, String>,
    sanitized_name_map: HashMap<String, String>,
}

/// Process-wide counter for function call identifiers.
static FUNCTION_CALL_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn function_call_id(name: &str) -> String {
    let nanos = unix_nano_now();
    format!("{name}-{nanos}-{}", FUNCTION_CALL_ID_COUNTER.fetch_add(1, Ordering::SeqCst) + 1)
}

fn inline_data_of<'a>(part: &'a Res<'_>) -> Res<'a> {
    let inline = part.g("inlineData");
    if inline.exists() { inline } else { part.g("inline_data") }
}

/// Part text, or the transcript of an `audioTranscription` part (speech-to-text models deliver
/// the transcript there instead of `text`).
fn part_text_of<'a>(part: &'a Res<'_>) -> Res<'a> {
    let text = part.g("text");
    if part.g("audioTranscription").exists() && !text.exists() {
        return part.g("audioTranscription.text");
    }
    text
}

/// `data:<mime>;base64,<data>` for an inlineData part (default mime `image/png`).
fn inline_image_url(inline: &Res<'_>) -> Option<String> {
    let data = inline.g("data").str();
    if data.is_empty() {
        return None;
    }
    let mut mime = inline.g("mimeType").str();
    if mime.is_empty() {
        mime = inline.g("mime_type").str();
    }
    if mime.is_empty() {
        mime = "image/png".to_string();
    }
    Some(format!("data:{mime};base64,{data}"))
}

/// Sets `usage` from Gemini usageMetadata on a chat completion template.
fn set_usage(template: &mut Value, usage: &Res<'_>) {
    let thoughts = usage.g("thoughtsTokenCount").int();
    let cached = usage.g("cachedContentTokenCount").int();
    cpa_json::set(template, "usage.completion_tokens", usage.g("candidatesTokenCount").int() + thoughts);
    let total = usage.g("totalTokenCount");
    if total.exists() {
        cpa_json::set(template, "usage.total_tokens", total.int());
    }
    cpa_json::set(template, "usage.prompt_tokens", usage.g("promptTokenCount").int());
    if thoughts > 0 {
        cpa_json::set(template, "usage.completion_tokens_details.reasoning_tokens", thoughts);
    }
    // Cached token count indicates prompt caching is working.
    if cached > 0 {
        cpa_json::set(template, "usage.prompt_tokens_details.cached_tokens", cached);
    }
}

/// Translates one Gemini streaming chunk into OpenAI `chat.completion.chunk` payloads (one per
/// candidate).
pub fn convert_gemini_response_to_openai(
    _ctx: &Ctx,
    _model: &str,
    original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let p = param.state(|| ChatParams {
        unix_timestamp: 0,
        function_index: HashMap::new(),
        saw_tool_call: HashMap::new(),
        upstream_finish_reason: HashMap::new(),
        sanitized_name_map: sanitized_tool_name_map(original),
    });

    let mut raw = raw;
    if let Some(rest) = raw.strip_prefix(b"data:") {
        raw = rest.trim_ascii();
    }
    if raw == b"[DONE]" {
        return vec![];
    }

    fast::convert(p, raw).unwrap_or_else(|| convert_general(p, raw))
}

/// The general conversion of one chunk through `Value`s; the reference for every shape the fast
/// path declines.
fn convert_general(p: &mut ChatParams, raw: &[u8]) -> Vec<Vec<u8>> {
    // Base template, cloned per candidate to support multiple candidates.
    let mut base_template = cpa_json::parse_str(
        r#"{"id":"","object":"chat.completion.chunk","created":12345,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}"#,
    );
    let root = cpa_json::parse(raw);

    let model_version = root.g("modelVersion");
    if model_version.exists() {
        cpa_json::set(&mut base_template, "model", model_version.str());
    }

    let create_time = root.g("createTime");
    if create_time.exists()
        && let Some(ts) = parse_create_time(&create_time.str()) {
            p.unix_timestamp = ts;
        }
    cpa_json::set(&mut base_template, "created", p.unix_timestamp);

    let response_id = root.g("responseId");
    if response_id.exists() {
        cpa_json::set(&mut base_template, "id", response_id.str());
    }

    // Usage is applied to the base template so it appears in every chunk.
    let usage = root.g("usageMetadata");
    if usage.exists() {
        set_usage(&mut base_template, &usage);
    }

    let mut response_strings: Vec<Vec<u8>> = Vec::new();
    let candidates = root.g("candidates");

    if candidates.is_array() {
        for (candidate_pos, candidate) in candidates.array().into_iter().enumerate() {
            let mut template = base_template.clone();

            let candidate_index = candidate.g("index").int();
            cpa_json::set(&mut template, "choices.0.index", candidate_index);

            let finish_reason = candidate.g("finishReason");
            if finish_reason.exists() {
                p.upstream_finish_reason.insert(candidate_index, finish_reason.str().to_uppercase());
            }

            let parts = candidate.g("content.parts");
            let mut assistant_role_set = false;
            let mut set_assistant_role = |template: &mut Value| {
                if assistant_role_set {
                    return;
                }
                cpa_json::set(template, "choices.0.delta.role", "assistant");
                assistant_role_set = true;
            };

            if parts.is_array() {
                let part_raws = cpa_json::raw_children(raw, &format!("candidates.{candidate_pos}.content.parts"));
                for (part_pos, part) in parts.array().into_iter().enumerate() {
                    let part_text = part_text_of(&part);
                    let function_call = part.g("functionCall");
                    let inline_data = inline_data_of(&part);
                    let mut thought_signature = part.g("thoughtSignature");
                    if !thought_signature.exists() {
                        thought_signature = part.g("thought_signature");
                    }

                    let has_thought_signature = thought_signature.exists() && !thought_signature.str().is_empty();
                    let has_content_payload = part_text.exists() || function_call.exists() || inline_data.exists();

                    // Skip pure thoughtSignature parts but keep any real payload in the same part.
                    if has_thought_signature && !has_content_payload {
                        continue;
                    }

                    if part_text.exists() {
                        let text = part_text.str();
                        set_assistant_role(&mut template);
                        // Distinguish regular content from reasoning/thoughts.
                        if part.g("thought").bool() {
                            cpa_json::set(&mut template, "choices.0.delta.reasoning_content", text);
                        } else {
                            cpa_json::set(&mut template, "choices.0.delta.content", text);
                        }
                    } else if function_call.exists() {
                        p.saw_tool_call.insert(candidate_index, true);
                        let existing_calls = template.g("choices.0.delta.tool_calls");

                        // Function index for this specific candidate.
                        let mut function_call_index = *p.function_index.get(&candidate_index).unwrap_or(&0);
                        *p.function_index.entry(candidate_index).or_insert(0) += 1;

                        if existing_calls.exists() && existing_calls.is_array() {
                            function_call_index = existing_calls.array().len() as i64;
                        } else {
                            cpa_json::set(&mut template, "choices.0.delta.tool_calls", json!([]));
                        }

                        let fc_name = restore_sanitized_tool_name(&p.sanitized_name_map, &function_call.g("name").str());
                        let mut call = json!({
                            "id": "",
                            "index": 0,
                            "type": "function",
                            "function": { "name": "", "arguments": "" },
                        });
                        cpa_json::set(&mut call, "id", function_call_id(&fc_name));
                        cpa_json::set(&mut call, "index", function_call_index);
                        cpa_json::set(&mut call, "function.name", fc_name);
                        let args = function_call.g("args");
                        if args.exists() {
                            cpa_json::set(&mut call, "function.arguments", args_raw(part_raws.get(part_pos), &args));
                        }
                        set_assistant_role(&mut template);
                        cpa_json::set(&mut template, "choices.0.delta.tool_calls.-1", call);
                    } else if inline_data.exists() {
                        let Some(image_url) = inline_image_url(&inline_data) else { continue };
                        let images = template.g("choices.0.delta.images");
                        if !images.exists() || !images.is_array() {
                            cpa_json::set(&mut template, "choices.0.delta.images", json!([]));
                        }
                        let image_index = template.g("choices.0.delta.images").array().len();
                        let mut payload = json!({ "type": "image_url", "image_url": { "url": "" } });
                        cpa_json::set(&mut payload, "index", image_index);
                        cpa_json::set(&mut payload, "image_url.url", image_url);
                        set_assistant_role(&mut template);
                        cpa_json::set(&mut template, "choices.0.delta.images.-1", payload);
                    }
                }
            }

            let upstream_finish_reason = p.upstream_finish_reason.get(&candidate_index).cloned().unwrap_or_default();
            let saw_tool_call = p.saw_tool_call.get(&candidate_index).copied().unwrap_or(false);
            let is_final_chunk = !upstream_finish_reason.is_empty() && usage.exists();

            if is_final_chunk {
                let finish_reason = if saw_tool_call {
                    "tool_calls"
                } else if upstream_finish_reason == "MAX_TOKENS" {
                    "max_tokens"
                } else {
                    "stop"
                };
                cpa_json::set(&mut template, "choices.0.finish_reason", finish_reason);
                cpa_json::set(&mut template, "choices.0.native_finish_reason", upstream_finish_reason.to_lowercase());
            }

            response_strings.push(cpa_json::to_vec(&template));
        }
    } else if usage.exists() && response_strings.is_empty() {
        // No candidates (a pure usageMetadata chunk): return the usage chunk.
        response_strings.push(cpa_json::to_vec(&base_template));
    }

    response_strings
}

/// Converts a complete Gemini response into a single OpenAI `chat.completion` (multiple
/// candidates become multiple choices). Also used by the antigravity chat translator.
pub fn convert_gemini_response_to_openai_non_stream(
    _ctx: &Ctx,
    _model: &str,
    original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let sanitized_name_map = sanitized_tool_name_map(original);
    let root = cpa_json::parse(raw);
    let mut unix_timestamp: i64 = 0;
    // Empty choices array to support multiple candidates.
    let mut template = cpa_json::parse_str(r#"{"id":"","object":"chat.completion","created":123456,"model":"model","choices":[]}"#);

    let model_version = root.g("modelVersion");
    if model_version.exists() {
        cpa_json::set(&mut template, "model", model_version.str());
    }

    let create_time = root.g("createTime");
    if create_time.exists()
        && let Some(ts) = parse_create_time(&create_time.str()) {
            unix_timestamp = ts;
        }
    cpa_json::set(&mut template, "created", unix_timestamp);

    let response_id = root.g("responseId");
    if response_id.exists() {
        cpa_json::set(&mut template, "id", response_id.str());
    }

    let usage = root.g("usageMetadata");
    if usage.exists() {
        set_usage(&mut template, &usage);
    }

    let candidates = root.g("candidates");
    if candidates.is_array() {
        let mut choices: Vec<Value> = Vec::new();
        for (candidate_pos, candidate) in candidates.array().into_iter().enumerate() {
            let mut choice = cpa_json::parse_str(
                r#"{"index":0,"message":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":null,"native_finish_reason":null}"#,
            );
            cpa_json::set(&mut choice, "index", candidate.g("index").int());

            let finish_reason = candidate.g("finishReason");
            if finish_reason.exists() {
                cpa_json::set(&mut choice, "finish_reason", finish_reason.str().to_lowercase());
                cpa_json::set(&mut choice, "native_finish_reason", finish_reason.str().to_lowercase());
            }

            let parts = candidate.g("content.parts");
            let mut has_function_call = false;
            if parts.is_array() {
                let mut tool_calls: Vec<Value> = Vec::new();
                let mut images: Vec<Value> = Vec::new();
                let mut text_content = String::new();
                let mut reasoning_content = String::new();
                let (mut has_text, mut has_reasoning) = (false, false);

                let part_raws = cpa_json::raw_children(raw, &format!("candidates.{candidate_pos}.content.parts"));
                for (part_pos, part) in parts.array().into_iter().enumerate() {
                    let part_text = part_text_of(&part);
                    let function_call = part.g("functionCall");
                    let inline_data = inline_data_of(&part);

                    if part_text.exists() {
                        if part.g("thought").bool() {
                            has_reasoning = true;
                            reasoning_content.push_str(&part_text.str());
                        } else {
                            has_text = true;
                            text_content.push_str(&part_text.str());
                        }
                    } else if function_call.exists() {
                        has_function_call = true;
                        let fc_name = restore_sanitized_tool_name(&sanitized_name_map, &function_call.g("name").str());
                        let mut call = json!({ "id": "", "type": "function", "function": { "name": "", "arguments": "" } });
                        cpa_json::set(&mut call, "id", function_call_id(&fc_name));
                        cpa_json::set(&mut call, "function.name", fc_name);
                        let args = function_call.g("args");
                        if args.exists() {
                            cpa_json::set(&mut call, "function.arguments", args_raw(part_raws.get(part_pos), &args));
                        }
                        tool_calls.push(call);
                    } else if inline_data.exists()
                        && let Some(image_url) = inline_image_url(&inline_data) {
                            let mut payload = json!({ "type": "image_url", "image_url": { "url": "" } });
                            cpa_json::set(&mut payload, "index", images.len());
                            cpa_json::set(&mut payload, "image_url.url", image_url);
                            images.push(payload);
                        }
                }

                if has_text {
                    cpa_json::set(&mut choice, "message.content", text_content);
                }
                if has_reasoning {
                    cpa_json::set(&mut choice, "message.reasoning_content", reasoning_content);
                }
                if !tool_calls.is_empty() {
                    cpa_json::set(&mut choice, "message.tool_calls", Value::Array(tool_calls));
                }
                if !images.is_empty() {
                    cpa_json::set(&mut choice, "message.images", Value::Array(images));
                }
            }

            if has_function_call {
                cpa_json::set(&mut choice, "finish_reason", "tool_calls");
                cpa_json::set(&mut choice, "native_finish_reason", "tool_calls");
            }
            choices.push(choice);
        }
        if !choices.is_empty() {
            cpa_json::set(&mut template, "choices", Value::Array(choices));
        }
    }

    Some(cpa_json::to_vec(&template))
}
