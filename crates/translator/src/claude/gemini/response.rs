//! Claude Messages response -> Gemini response (Go: claude_gemini_response.go).

use crate::common::{trim_space, unix_now};
use std::collections::HashMap;

use chrono::{DateTime, Local, SecondsFormat};
use cpa_core::signature::{gemini_replay_signature_or_bypass, SignatureBlockKind};
use cpa_json::{json, Res, Value, J};

use crate::registry::{Ctx, Param};

const DATA_TAG: &[u8] = b"data:";

const STREAM_TEMPLATE: &str = r#"{"candidates":[{"content":{"role":"model","parts":[]}}],"usageMetadata":{"trafficType":"PROVISIONED_THROUGHPUT"},"modelVersion":"","createTime":"","responseId":""}"#;
const NON_STREAM_TEMPLATE: &str = r#"{"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP"}],"usageMetadata":{"trafficType":"PROVISIONED_THROUGHPUT"},"modelVersion":"","createTime":"","responseId":""}"#;

/// Per-stream state. Tool-use blocks are assembled by Claude content block index, because
/// `content_block_stop` carries no payload.
#[derive(Default)]
struct StreamState {
    model: String,
    created_at: i64,
    response_id: String,
    tool_use_names: HashMap<i64, String>,
    tool_use_args: HashMap<i64, String>,
    tool_use_ids: HashMap<i64, String>,
}

impl StreamState {
    /// Handles the state-only events shared by both modes. Returns the finished functionCall
    /// part when `content_block_stop` closes a tool_use block.
    fn finish_tool_use(&mut self, idx: i64) -> Option<Value> {
        let name = self.tool_use_names.get(&idx).cloned().unwrap_or_default();
        let args_trim = self.tool_use_args.get(&idx).map(|b| b.trim().to_string()).unwrap_or_default();
        let tool_id = self.tool_use_ids.get(&idx).cloned().unwrap_or_default();
        if name.is_empty() && args_trim.is_empty() {
            return None;
        }
        let mut function_call = json!({"functionCall": {"name": "", "args": {}}});
        if !name.is_empty() {
            cpa_json::set(&mut function_call, "functionCall.name", name);
        }
        if !args_trim.is_empty() {
            set_raw_lenient(&mut function_call, "functionCall.args", &args_trim);
        }
        if !tool_id.is_empty() {
            cpa_json::set(&mut function_call, "functionCall.id", tool_id);
        }
        self.tool_use_args.remove(&idx);
        self.tool_use_names.remove(&idx);
        self.tool_use_ids.remove(&idx);
        Some(function_call)
    }

    /// Records tool_use name/id from a `content_block_start` block.
    fn start_tool_use(&mut self, idx: i64, cb: &Res<'_>) {
        let name = cb.g("name");
        if name.exists() {
            self.tool_use_names.insert(idx, name.str());
        }
        let tool_id = cb.g("id").str();
        if !tool_id.is_empty() {
            self.tool_use_ids.insert(idx, tool_id);
        }
    }
}

/// sjson.SetRaw with a payload that may be invalid JSON (truncated tool arguments). Go splices
/// the text into the output verbatim, which can corrupt the surrounding document in ways a
/// `Value` cannot reproduce; here invalid text is stored as a string instead.
fn set_raw_lenient(v: &mut Value, path: &str, raw: &str) {
    let _ = cpa_json::set_raw(v, path, raw);
}

fn thought_signature_part(signature: &str) -> Value {
    json!({
        "thought": true,
        "thoughtSignature": gemini_replay_signature_or_bypass(signature, SignatureBlockKind::GeminiModelPart),
    })
}

/// Go `time.Unix(secs, 0).Format(time.RFC3339Nano)`: local zone, `Z` for UTC.
fn format_create_time(secs: i64) -> String {
    DateTime::from_timestamp(secs, 0)
        .map(|t| t.with_timezone(&Local).to_rfc3339_opts(SecondsFormat::AutoSi, true))
        .unwrap_or_default()
}

/// Usage mapping shared by stream and non-stream: the Gemini usage fields written at `prefix`
/// ("usageMetadata." for a full response, "" for a standalone object).
fn set_usage(target: &mut Value, prefix: &str, usage: &Res<'_>) {
    let input_tokens = usage.g("input_tokens").int();
    let output_tokens = usage.g("output_tokens").int();
    let p = |name: &str| format!("{prefix}{name}");

    cpa_json::set(target, &p("promptTokenCount"), input_tokens);
    cpa_json::set(target, &p("candidatesTokenCount"), output_tokens);
    cpa_json::set(target, &p("totalTokenCount"), input_tokens.wrapping_add(output_tokens));

    let cache_creation = usage.g("cache_creation_input_tokens");
    if cache_creation.exists() {
        cpa_json::set(target, &p("cachedContentTokenCount"), cache_creation.int());
    }
    let cache_read = usage.g("cache_read_input_tokens");
    if cache_read.exists() {
        // Cache reads add to the cached content count.
        let total = cache_creation.int().wrapping_add(cache_read.int());
        cpa_json::set(target, &p("cachedContentTokenCount"), total);
    }
    let thinking_tokens = usage.g("thinking_tokens");
    if thinking_tokens.exists() {
        cpa_json::set(target, &p("thoughtsTokenCount"), thinking_tokens.int());
    }
    cpa_json::set(target, &p("trafficType"), "PROVISIONED_THROUGHPUT");
}

/// Converts one Claude SSE line into zero or more Gemini JSON chunks (no SSE framing; the caller
/// frames them). Output is a Gemini `GenerateContentResponse` per chunk.
pub fn convert_claude_response_to_gemini(
    _ctx: &Ctx,
    model_name: &str,
    _original_request_raw_json: &[u8],
    _request_raw_json: &[u8],
    raw_json: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let st = param.state(|| StreamState { model: model_name.to_string(), ..Default::default() });

    if !raw_json.starts_with(DATA_TAG) {
        return Vec::new();
    }
    let raw_json = trim_space(&raw_json[5..]);

    let root = cpa_json::parse(raw_json);
    let event_type = root.g("type").str();

    let mut template = cpa_json::parse_str(STREAM_TEMPLATE);

    if !st.model.is_empty() {
        cpa_json::set(&mut template, "modelVersion", st.model.as_str());
    }
    if !st.response_id.is_empty() {
        cpa_json::set(&mut template, "responseId", st.response_id.as_str());
    }
    if st.created_at == 0 {
        st.created_at = unix_now();
    }
    cpa_json::set(&mut template, "createTime", format_create_time(st.created_at));


    match event_type.as_str() {
        "message_start" => {
            let message = root.g("message");
            if message.exists() {
                st.response_id = message.g("id").str();
                st.model = message.g("model").str();
            }
            Vec::new()
        }
        "content_block_start" => {
            let cb = root.g("content_block");
            if cb.exists() {
                if cb.g("type").str() == "tool_use" {
                    let idx = root.g("index").int();
                    st.start_tool_use(idx, &cb);
                } else if cb.g("type").str() == "thinking" {
                    let sig = cb.g("signature");
                    if sig.exists() && !sig.str().is_empty() {
                        cpa_json::set(&mut template, "candidates.0.content.parts.-1", thought_signature_part(&sig.str()));
                        return vec![cpa_json::to_vec(&template)];
                    }
                }
            }
            Vec::new()
        }
        "content_block_delta" => {
            let delta = root.g("delta");
            if delta.exists() {
                match delta.g("type").str().as_str() {
                    "text_delta" => {
                        let text = delta.g("text");
                        if text.exists() && !text.str().is_empty() {
                            cpa_json::set(&mut template, "candidates.0.content.parts.-1", json!({"text": text.str()}));
                        }
                    }
                    "thinking_delta" => {
                        let text = delta.g("thinking");
                        if text.exists() && !text.str().is_empty() {
                            cpa_json::set(
                                &mut template,
                                "candidates.0.content.parts.-1",
                                json!({"thought": true, "text": text.str()}),
                            );
                        }
                    }
                    "signature_delta" => {
                        let sig = delta.g("signature");
                        if sig.exists() && !sig.str().is_empty() {
                            cpa_json::set(&mut template, "candidates.0.content.parts.-1", thought_signature_part(&sig.str()));
                        }
                    }
                    "input_json_delta" => {
                        // Accumulated by block index; the functionCall is emitted at block stop.
                        let idx = root.g("index").int();
                        let buf = st.tool_use_args.entry(idx).or_default();
                        let pj = delta.g("partial_json");
                        if pj.exists() {
                            buf.push_str(&pj.str());
                        }
                        return Vec::new();
                    }
                    _ => {}
                }
            }
            vec![cpa_json::to_vec(&template)]
        }
        "content_block_stop" => {
            let idx = root.g("index").int();
            match st.finish_tool_use(idx) {
                Some(function_call) => {
                    cpa_json::set(&mut template, "candidates.0.content.parts.-1", function_call);
                    cpa_json::set(&mut template, "candidates.0.finishReason", "STOP");
                    vec![cpa_json::to_vec(&template)]
                }
                None => Vec::new(),
            }
        }
        "message_delta" => {
            let delta = root.g("delta");
            if delta.exists() {
                let stop_reason = delta.g("stop_reason");
                if stop_reason.exists() {
                    let finish = if stop_reason.str() == "max_tokens" { "MAX_TOKENS" } else { "STOP" };
                    cpa_json::set(&mut template, "candidates.0.finishReason", finish);
                }
            }
            let usage = root.g("usage");
            if usage.exists() {
                set_usage(&mut template, "usageMetadata.", &usage);
            }
            // Go overwrites the finish reason unconditionally, so MAX_TOKENS never survives.
            cpa_json::set(&mut template, "candidates.0.finishReason", "STOP");
            vec![cpa_json::to_vec(&template)]
        }
        "message_stop" => Vec::new(),
        "error" => {
            let mut error_msg = root.g("error.message").str();
            if error_msg.is_empty() {
                error_msg = "Unknown error occurred".into();
            }
            let error_response = json!({"error": {"code": 400, "message": error_msg, "status": "INVALID_ARGUMENT"}});
            vec![cpa_json::to_vec(&error_response)]
        }
        _ => Vec::new(),
    }
}

/// Converts a complete Claude SSE body (the non-stream upstream response is the buffered event
/// stream) into one Gemini response.
pub fn convert_claude_response_to_gemini_non_stream(
    _ctx: &Ctx,
    model_name: &str,
    _original_request_raw_json: &[u8],
    _request_raw_json: &[u8],
    raw_json: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let mut template = cpa_json::parse_str(NON_STREAM_TEMPLATE);
    cpa_json::set(&mut template, "modelVersion", model_name);

    let streaming_events: Vec<&[u8]> = raw_json
        .split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").map(|l| l).unwrap_or(line))
        .filter(|line| line.starts_with(DATA_TAG))
        .map(|line| trim_space(&line[5..]))
        .collect();

    let mut st = StreamState { model: model_name.to_string(), ..Default::default() };
    let mut all_parts: Vec<Value> = Vec::new();
    let mut final_usage: Option<Value> = None;
    let mut response_id = String::new();
    let mut created_at = 0i64;

    for event_data in streaming_events {
        if event_data.is_empty() {
            continue;
        }
        let root = cpa_json::parse(event_data);
        match root.g("type").str().as_str() {
            "message_start" => {
                let message = root.g("message");
                if message.exists() {
                    response_id = message.g("id").str();
                    st.response_id = response_id.clone();
                    st.model = message.g("model").str();
                    created_at = unix_now();
                    st.created_at = created_at;
                }
            }
            "content_block_start" => {
                let idx = root.g("index").int();
                let cb = root.g("content_block");
                if cb.exists() {
                    if cb.g("type").str() == "tool_use" {
                        st.start_tool_use(idx, &cb);
                    } else if cb.g("type").str() == "thinking" {
                        let sig = cb.g("signature");
                        if sig.exists() && !sig.str().is_empty() {
                            all_parts.push(thought_signature_part(&sig.str()));
                        }
                    }
                }
            }
            "content_block_delta" => {
                let delta = root.g("delta");
                if delta.exists() {
                    match delta.g("type").str().as_str() {
                        "text_delta" => {
                            let text = delta.g("text");
                            if text.exists() && !text.str().is_empty() {
                                all_parts.push(json!({"text": text.str()}));
                            }
                        }
                        "thinking_delta" => {
                            let text = delta.g("thinking");
                            if text.exists() && !text.str().is_empty() {
                                all_parts.push(json!({"thought": true, "text": text.str()}));
                            }
                        }
                        "signature_delta" => {
                            let sig = delta.g("signature");
                            if sig.exists() && !sig.str().is_empty() {
                                all_parts.push(thought_signature_part(&sig.str()));
                            }
                        }
                        "input_json_delta" => {
                            let idx = root.g("index").int();
                            let buf = st.tool_use_args.entry(idx).or_default();
                            let pj = delta.g("partial_json");
                            if pj.exists() {
                                buf.push_str(&pj.str());
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                let idx = root.g("index").int();
                if let Some(function_call) = st.finish_tool_use(idx) {
                    all_parts.push(function_call);
                }
            }
            "message_delta" => {
                let usage = root.g("usage");
                if usage.exists() {
                    let mut usage_json = json!({});
                    set_usage(&mut usage_json, "", &usage);
                    final_usage = Some(usage_json);
                }
            }
            _ => {}
        }
    }

    if !response_id.is_empty() {
        cpa_json::set(&mut template, "responseId", response_id);
    }
    if created_at > 0 {
        cpa_json::set(&mut template, "createTime", format_create_time(created_at));
    }

    let consolidated = consolidate_parts(all_parts);
    if !consolidated.is_empty() {
        cpa_json::set(&mut template, "candidates.0.content.parts", Value::Array(consolidated));
    }
    if let Some(usage) = final_usage {
        cpa_json::set(&mut template, "usageMetadata", usage);
    }

    Some(cpa_json::to_vec(&template))
}

/// Merges consecutive text parts and consecutive thought parts (keeping the last signature);
/// functionCall and other parts are kept as-is and flush both buffers.
fn consolidate_parts(parts: Vec<Value>) -> Vec<Value> {
    if parts.is_empty() {
        return parts;
    }

    let mut consolidated: Vec<Value> = Vec::new();
    let mut text = String::new();
    let mut thought = String::new();
    let mut thought_signature = String::new();
    let (mut has_text, mut has_thought) = (false, false);

    fn flush_text(out: &mut Vec<Value>, text: &mut String, has_text: &mut bool) {
        if *has_text && !text.is_empty() {
            out.push(json!({"text": std::mem::take(text)}));
            *has_text = false;
        }
    }
    fn flush_thought(out: &mut Vec<Value>, thought: &mut String, signature: &mut String, has_thought: &mut bool) {
        if *has_thought && (!thought.is_empty() || !signature.is_empty()) {
            let mut part = json!({"thought": true, "text": std::mem::take(thought)});
            if !signature.is_empty() {
                cpa_json::set(&mut part, "thoughtSignature", std::mem::take(signature));
            }
            out.push(part);
            *has_thought = false;
        }
    }

    for part_json in parts {
        if !part_json.is_object() {
            flush_text(&mut consolidated, &mut text, &mut has_text);
            flush_thought(&mut consolidated, &mut thought, &mut thought_signature, &mut has_thought);
            consolidated.push(part_json);
            continue;
        }

        if part_json.get("thought") == Some(&Value::Bool(true)) {
            flush_text(&mut consolidated, &mut text, &mut has_text);
            if let Some(Value::String(t)) = part_json.get("text") {
                thought.push_str(t);
                has_thought = true;
            }
            if let Some(Value::String(sig)) = part_json.get("thoughtSignature")
                && !sig.is_empty()
            {
                thought_signature = sig.clone();
                has_thought = true;
            }
        } else if let Some(Value::String(t)) = part_json.get("text") {
            flush_thought(&mut consolidated, &mut thought, &mut thought_signature, &mut has_thought);
            text.push_str(t);
            has_text = true;
        } else {
            flush_text(&mut consolidated, &mut text, &mut has_text);
            flush_thought(&mut consolidated, &mut thought, &mut thought_signature, &mut has_thought);
            consolidated.push(part_json);
        }
    }

    flush_thought(&mut consolidated, &mut thought, &mut thought_signature, &mut has_thought);
    flush_text(&mut consolidated, &mut text, &mut has_text);
    consolidated
}
