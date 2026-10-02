//! Interactions response -> Claude Messages response (Go: interactions_claude_response.go).

use crate::common::{first_non_blank, trim_space, unix_nano_now};
use std::collections::HashMap;

use cpa_json::{json, Res, Value};

use crate::common::{append_sse_event_bytes, append_sse_event_string, interactions_usage, set_raw_array_items};
use crate::registry::{Ctx, Param};

/// Per-stream state. Interactions step indexes map to Claude content blocks, which are numbered
/// by their own counter (`block_index`).
#[derive(Default)]
struct StreamState {
    id: String,
    model: String,
    started: bool,
    active_block: bool,
    active_block_type: String,
    block_index: i64,
    saw_tool_call: bool,
    completed: bool,
    stopped: bool,
    done: bool,
    tool_names: HashMap<i64, String>,
    tool_ids: HashMap<i64, String>,
    tool_signatures: HashMap<i64, String>,
}

type Chunks = Vec<Vec<u8>>;

/// One SSE frame as Claude clients expect it: `event:` and `data:` lines plus three newlines.
fn frame(event: &str, payload: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    append_sse_event_bytes(&mut out, event, &cpa_json::to_vec(payload), 3);
    out
}

/// Converts one Interactions SSE event (or `[DONE]`) into Claude SSE frames.
pub fn convert_interactions_response_to_claude(
    _ctx: &Ctx,
    model_name: &str,
    _original_request_raw_json: &[u8],
    _request_raw_json: &[u8],
    raw_json: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let st = param.state(|| StreamState { model: model_name.to_string(), ..Default::default() });
    st.model = first_non_blank(&[&st.model, model_name]);
    convert_event(model_name, raw_json, st)
}

/// Converts a complete Interactions response (optionally wrapped in `interaction`) into a
/// Claude message.
pub fn convert_interactions_response_to_claude_non_stream(
    _ctx: &Ctx,
    model_name: &str,
    _original_request_raw_json: &[u8],
    _request_raw_json: &[u8],
    raw_json: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let root_value = cpa_json::parse(raw_json);
    let root = Res::of(&root_value);
    let nested = root.g("interaction");
    let interaction = if nested.exists() { nested } else { root.clone() };
    let mut out = cpa_json::parse_str(
        r#"{"id":"","type":"message","role":"assistant","model":"","content":[],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}"#,
    );
    cpa_json::set(
        &mut out,
        "id",
        first_non_blank(&[&interaction.g("id").str(), &root.g("id").str(), &format!("msg_{}", unix_nano_now())]),
    );
    cpa_json::set(&mut out, "model", first_non_blank(&[&interaction.g("model").str(), model_name]));
    let mut steps = interaction.g("steps");
    if !steps.exists() {
        steps = root.g("steps");
    }
    let mut saw_tool_call = false;
    let mut content_blocks: Vec<Vec<u8>> = Vec::new();
    steps.for_each(|_, step| {
        match step.g("type").str().as_str() {
            "thought" => {
                for text in content_texts(&step.g("content")) {
                    let mut block = json!({"type": "thinking", "thinking": text});
                    let signature = signature_of(&step);
                    if !signature.is_empty() {
                        cpa_json::set(&mut block, "signature", signature);
                    }
                    content_blocks.push(cpa_json::to_vec(&block));
                }
            }
            "function_call" => {
                saw_tool_call = true;
                let mut block = json!({"type": "tool_use", "id": "", "name": "", "input": {}});
                cpa_json::set(&mut block, "id", tool_id_of(&step));
                cpa_json::set(&mut block, "name", step.g("name").str());
                let signature = signature_of(&step);
                if !signature.is_empty() {
                    cpa_json::set(&mut block, "signature", signature);
                }
                let args = first_existing(&step, &["arguments", "args"]);
                if args.is_object() {
                    cpa_json::set(&mut block, "input", args.value());
                }
                content_blocks.push(cpa_json::to_vec(&block));
            }
            _ => {
                for text in content_texts(&step.g("content")) {
                    content_blocks.push(cpa_json::to_vec(&json!({"type": "text", "text": text})));
                }
            }
        }
        true
    });
    if !content_blocks.is_empty() {
        out = cpa_json::parse(&set_raw_array_items(&cpa_json::to_vec(&out), "content", &content_blocks));
    }
    if saw_tool_call {
        cpa_json::set(&mut out, "stop_reason", "tool_use");
    }
    if is_max_tokens(&interaction, &root) {
        cpa_json::set(&mut out, "stop_reason", "max_tokens");
    }
    set_usage_from_interactions(&mut out, "usage", &interactions_usage(&root));
    Some(cpa_json::to_vec(&out))
}

/// `incomplete` status or a length-like finish reason maps to Claude `max_tokens`.
fn is_max_tokens(interaction: &Res<'_>, root: &Res<'_>) -> bool {
    let status = first_non_blank(&[&interaction.g("status").str(), &root.g("status").str()]);
    let finish_reason = first_non_blank(&[&interaction.g("finish_reason").str(), &root.g("finish_reason").str()]);
    status == "incomplete" || finish_reason == "length" || finish_reason == "max_tokens"
}

fn convert_event(model_name: &str, raw_json: &[u8], st: &mut StreamState) -> Chunks {
    let mut out = Chunks::new();
    let payload = sse_payload(raw_json);
    if payload.is_empty() {
        return out;
    }
    if trim_space(&payload) == b"[DONE]" {
        append_message_stop(&mut out, st);
        return out;
    }
    let root_value = cpa_json::parse(&payload);
    if root_value.is_null() {
        return out;
    }
    let root = Res::of(&root_value);
    match root.g("event_type").str().as_str() {
        "interaction.created" => {
            let interaction = root.g("interaction");
            st.id = first_non_blank(&[&interaction.g("id").str(), &st.id]);
            st.model = first_non_blank(&[&interaction.g("model").str(), &st.model, model_name]);
            append_message_start(&mut out, st);
        }
        "step.start" => step_start(&root, st, &mut out),
        "step.delta" => step_delta(&root, st, &mut out),
        "step.stop" => append_content_block_stop(&mut out, st),
        "interaction.completed" | "finish" => append_message_delta(&mut out, &root, st),
        "response.failed" | "interaction.failed" => append_error(&mut out, &root, st),
        "done" => append_message_stop(&mut out, st),
        _ => {}
    }
    out
}

fn step_start(root: &Res<'_>, st: &mut StreamState, out: &mut Chunks) {
    append_message_start(out, st);
    append_content_block_stop(out, st);
    let index = root.g("index").int();
    let step = root.g("step");
    match step.g("type").str().as_str() {
        "function_call" => {
            st.saw_tool_call = true;
            st.tool_names.insert(index, step.g("name").str());
            st.tool_ids.insert(index, tool_id_of(&step));
            st.tool_signatures.insert(index, signature_of(&step));
            append_tool_block_start(out, index, st);
        }
        "thought" => append_content_block_start(out, "thinking", st),
        _ => append_content_block_start(out, "text", st),
    }
}

fn step_delta(root: &Res<'_>, st: &mut StreamState, out: &mut Chunks) {
    let index = root.g("index").int();
    let delta = root.g("delta");
    match delta.g("type").str().as_str() {
        "thought_summary" => {
            append_message_start(out, st);
            ensure_content_block(out, "thinking", st);
            let text = first_non_blank(&[&delta.g("content.text").str(), &delta.g("text").str()]);
            append_content_delta(out, "thinking_delta", "thinking", &text, st);
        }
        "thought_signature" => {
            if st.active_block && st.active_block_type == "thinking" {
                append_content_delta(out, "signature_delta", "signature", &delta.g("signature").str(), st);
            }
        }
        "arguments_delta" => {
            append_message_start(out, st);
            if !st.active_block || st.active_block_type != "tool_use" {
                append_content_block_stop(out, st);
                if st.tool_names.get(&index).is_none_or(|n| n.is_empty()) {
                    st.tool_names.insert(index, root.g("step.name").str());
                }
                if st.tool_ids.get(&index).is_none_or(|n| n.is_empty()) {
                    st.tool_ids.insert(index, format!("toolu_{index}"));
                }
                append_tool_block_start(out, index, st);
            }
            append_content_delta(out, "input_json_delta", "partial_json", &delta.g("arguments").str(), st);
        }
        _ => {
            append_message_start(out, st);
            ensure_content_block(out, "text", st);
            append_content_delta(out, "text_delta", "text", &delta.g("text").str(), st);
        }
    }
}

fn append_message_start(out: &mut Chunks, st: &mut StreamState) {
    if st.started {
        return;
    }
    let mut msg = cpa_json::parse_str(
        r#"{"type":"message_start","message":{"id":"","type":"message","role":"assistant","content":[],"model":"","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}}"#,
    );
    cpa_json::set(&mut msg, "message.id", first_non_blank(&[&st.id, &format!("msg_{}", unix_nano_now())]));
    cpa_json::set(&mut msg, "message.model", st.model.as_str());
    st.started = true;
    out.push(frame("message_start", &msg));
}

fn append_content_block_start(out: &mut Chunks, block_type: &str, st: &mut StreamState) {
    if st.active_block && st.active_block_type == block_type {
        return;
    }
    append_content_block_stop(out, st);
    let block = if block_type == "thinking" {
        json!({"type": "content_block_start", "index": st.block_index, "content_block": {"type": "thinking", "thinking": ""}})
    } else {
        json!({"type": "content_block_start", "index": st.block_index, "content_block": {"type": "text", "text": ""}})
    };
    st.active_block = true;
    st.active_block_type = block_type.to_string();
    out.push(frame("content_block_start", &block));
}

fn append_tool_block_start(out: &mut Chunks, step_index: i64, st: &mut StreamState) {
    append_content_block_stop(out, st);
    let mut block = json!({
        "type": "content_block_start",
        "index": st.block_index,
        "content_block": {"type": "tool_use", "id": "", "name": "", "input": {}},
    });
    let id = first_non_blank(&[
        st.tool_ids.get(&step_index).map(String::as_str).unwrap_or(""),
        &format!("toolu_{step_index}"),
    ]);
    cpa_json::set(&mut block, "content_block.id", id);
    cpa_json::set(&mut block, "content_block.name", st.tool_names.get(&step_index).cloned().unwrap_or_default());
    if let Some(signature) = st.tool_signatures.get(&step_index).filter(|s| !s.is_empty()) {
        cpa_json::set(&mut block, "content_block.signature", signature.as_str());
    }
    st.active_block = true;
    st.active_block_type = "tool_use".into();
    out.push(frame("content_block_start", &block));
}

fn ensure_content_block(out: &mut Chunks, block_type: &str, st: &mut StreamState) {
    if st.active_block && st.active_block_type == block_type {
        return;
    }
    append_content_block_start(out, block_type, st);
}

/// Emits a content_block_delta; empty values are skipped except for `input_json_delta`.
fn append_content_delta(out: &mut Chunks, delta_type: &str, field: &str, value: &str, st: &StreamState) {
    if value.is_empty() && delta_type != "input_json_delta" {
        return;
    }
    let mut delta = json!({"type": "content_block_delta", "index": st.block_index, "delta": {"type": delta_type}});
    cpa_json::set(&mut delta, &format!("delta.{field}"), value);
    out.push(frame("content_block_delta", &delta));
}

fn append_content_block_stop(out: &mut Chunks, st: &mut StreamState) {
    if !st.active_block {
        return;
    }
    let stop = json!({"type": "content_block_stop", "index": st.block_index});
    out.push(frame("content_block_stop", &stop));
    st.active_block = false;
    st.active_block_type.clear();
    st.block_index += 1;
}

fn append_message_delta(out: &mut Chunks, root: &Res<'_>, st: &mut StreamState) {
    if st.completed {
        return;
    }
    append_message_start(out, st);
    append_content_block_stop(out, st);
    let mut payload = cpa_json::parse_str(
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":0,"output_tokens":0}}"#,
    );
    if st.saw_tool_call {
        cpa_json::set(&mut payload, "delta.stop_reason", "tool_use");
    }
    let interaction = root.g("interaction");
    if is_max_tokens(&interaction, root) {
        cpa_json::set(&mut payload, "delta.stop_reason", "max_tokens");
    }
    set_usage_from_interactions(&mut payload, "usage", &interactions_usage(root));
    out.push(frame("message_delta", &payload));
    st.completed = true;
}

fn append_message_stop(out: &mut Chunks, st: &mut StreamState) {
    if st.done {
        return;
    }
    append_content_block_stop(out, st);
    if !st.completed {
        append_message_delta(out, &Res::NONE, st);
    }
    if !st.stopped {
        let mut stop = Vec::new();
        append_sse_event_string(&mut stop, "message_stop", r#"{"type":"message_stop"}"#, 3);
        out.push(stop);
        st.stopped = true;
    }
    st.done = true;
}

fn append_error(out: &mut Chunks, root: &Res<'_>, st: &mut StreamState) {
    append_content_block_stop(out, st);
    let mut err_node = root.g("error");
    if !err_node.exists() {
        err_node = root.g("interaction.error");
    }
    let mut msg = err_node.g("message").str();
    if msg.is_empty() {
        msg = "upstream error occurred".into();
    }
    let mut err_type = err_node.g("type").str();
    if err_type.is_empty() {
        err_type = "api_error".into();
    }
    let payload = json!({"type": "error", "error": {"type": err_type, "message": msg}});
    out.push(frame("error", &payload));
}

/// Maps Interactions usage onto Claude usage fields at `path`. When only a prompt total is
/// present, cache tokens are subtracted from it to get Claude's uncached `input_tokens`.
fn set_usage_from_interactions(out: &mut Value, path: &str, usage: &Res<'_>) {
    if !usage.exists() {
        return;
    }
    let output_tokens = first_usage_int(usage, &["output_tokens", "total_output_tokens"]);
    let cached_tokens = first_usage_int(usage, &["cache_read_input_tokens", "cache_read_tokens", "cached_tokens", "total_cached_tokens"]);
    let cache_write_tokens =
        first_usage_int(usage, &["cache_creation_input_tokens", "cache_creation_tokens", "cache_write_tokens"]);

    let mut total_cache = 0i64;
    if let Some(c) = cached_tokens.filter(|&c| c > 0) {
        total_cache = total_cache.wrapping_add(c);
    }
    if let Some(c) = cache_write_tokens.filter(|&c| c > 0) {
        total_cache = total_cache.wrapping_add(c);
    }

    let in_node = usage.g("input_tokens");
    let input_tokens = if in_node.exists() {
        Some(in_node.int())
    } else {
        first_usage_int(usage, &["total_input_tokens", "prompt_tokens"])
            .map(|total| if total >= total_cache { total.wrapping_sub(total_cache) } else { 0 })
    };

    if let Some(v) = input_tokens {
        cpa_json::set(out, &format!("{path}.input_tokens"), v);
    }
    if let Some(v) = output_tokens {
        cpa_json::set(out, &format!("{path}.output_tokens"), v);
    }
    if let Some(v) = cached_tokens.filter(|&c| c > 0) {
        cpa_json::set(out, &format!("{path}.cache_read_input_tokens"), v);
    }
    if let Some(v) = cache_write_tokens.filter(|&c| c > 0) {
        cpa_json::set(out, &format!("{path}.cache_creation_input_tokens"), v);
    }
}

/// The JSON payload of an SSE line. `[DONE]` and a single `data:` line yield their text; a
/// multi-line chunk yields its joined `data:` lines; anything else is returned trimmed.
fn sse_payload(raw_json: &[u8]) -> Vec<u8> {
    let trimmed = trim_space(raw_json);
    if trimmed.is_empty() || trimmed == b"[DONE]" {
        return trimmed.to_vec();
    }
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        return trim_space(rest).to_vec();
    }
    let data_lines: Vec<&[u8]> = trimmed
        .split(|&b| b == b'\n')
        .map(trim_space)
        .filter_map(|line| line.strip_prefix(b"data:").map(trim_space))
        .collect();
    if !data_lines.is_empty() {
        return data_lines.join(&b'\n');
    }
    trimmed.to_vec()
}

/// Texts of a content value: a string, or each part's `text` / `content.text`.
fn content_texts(content: &Res<'_>) -> Vec<String> {
    if !content.exists() {
        return Vec::new();
    }
    if content.is_string() {
        return vec![content.str()];
    }
    let mut out = Vec::new();
    content.for_each(|_, part| {
        let text = first_non_blank(&[&part.g("text").str(), &part.g("content.text").str()]);
        if !text.is_empty() {
            out.push(text);
        }
        true
    });
    out
}

fn tool_id_of(root: &Res<'_>) -> String {
    first_non_blank(&[
        &root.g("call_id").str(),
        &root.g("id").str(),
        &root.g("tool_use_id").str(),
        "toolu_interactions",
    ])
}

fn signature_of(root: &Res<'_>) -> String {
    first_non_blank(&[
        &root.g("signature").str(),
        &root.g("thought_signature").str(),
        &root.g("thoughtSignature").str(),
        &root.g("extra_content.google.thought_signature").str(),
    ])
}

fn first_existing<'a>(root: &'a Res<'_>, paths: &[&str]) -> Res<'a> {
    for path in paths {
        let value = root.g(path);
        if value.exists() {
            return value;
        }
    }
    Res::NONE
}

fn first_usage_int(root: &Res<'_>, paths: &[&str]) -> Option<i64> {
    paths.iter().map(|path| root.g(path)).find(Res::exists).map(|v| v.int())
}

