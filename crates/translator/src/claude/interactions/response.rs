//! Claude Messages response -> Interactions response (Go: interactions_claude_response.go).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Utc;
use cpa_json::{json, Res, Value};

use crate::common::sse_event_data;
use crate::registry::{Ctx, Param};

const DATA_TAG: &[u8] = b"data:";

/// Per-stream state. Claude block indexes are mapped to Interactions steps, which are numbered
/// by a separate monotonic counter (`step_index`).
#[derive(Default)]
struct StreamState {
    id: String,
    model: String,
    created: bool,
    status_updated: bool,
    completed: bool,
    done: bool,
    /// Merged usage object (last value of each key wins).
    usage_raw: Option<Value>,
    step_index: i64,
    active_step_index: i64,
    active_step_open: bool,
    current_step_by_index: HashMap<i64, String>,
    tool_names: HashMap<i64, String>,
    tool_ids: HashMap<i64, String>,
    /// Tool arguments for function_call blocks; text for the non-stream SSE replay.
    tool_args: HashMap<i64, String>,
}

type Chunks = Vec<Vec<u8>>;

/// Converts one Claude SSE line (`data: {...}` or `[DONE]`) into Interactions SSE frames.
pub fn convert_claude_response_to_interactions(
    _ctx: &Ctx,
    model_name: &str,
    _original_request_raw_json: &[u8],
    _request_raw_json: &[u8],
    raw_json: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let st = param.state(|| StreamState { model: model_name.to_string(), ..Default::default() });
    st.model = first_non_empty(&[&st.model, model_name]);
    convert_event(model_name, raw_json, st)
}

/// Converts a non-stream Claude response: a Claude message object, or a buffered SSE body.
pub fn convert_claude_response_to_interactions_non_stream(
    _ctx: &Ctx,
    model_name: &str,
    _original_request_raw_json: &[u8],
    _request_raw_json: &[u8],
    raw_json: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let root = cpa_json::parse(raw_json);
    let root_res = Res::of(&root);
    if root_res.exists() && root_res.g("content").exists() {
        return Some(convert_message(model_name, &root_res));
    }
    Some(convert_sse_non_stream(model_name, raw_json))
}

fn now_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

fn generated_interaction_id() -> String {
    format!("interaction_{}", now_nanos())
}

/// Go `bytes.TrimSpace`.
fn trim_space(b: &[u8]) -> &[u8] {
    match std::str::from_utf8(b) {
        Ok(s) => s.trim().as_bytes(),
        Err(_) => b.trim_ascii(),
    }
}

fn first_non_empty(values: &[&str]) -> String {
    values.iter().find(|v| !v.is_empty()).map(|v| v.to_string()).unwrap_or_default()
}

fn convert_message(model_name: &str, root: &Res<'_>) -> Vec<u8> {
    let mut out = json!({"id": "", "object": "interaction", "status": "completed", "model": "", "steps": []});
    cpa_json::set(&mut out, "id", first_non_empty(&[&root.g("id").str(), &generated_interaction_id()]));
    cpa_json::set(&mut out, "model", first_non_empty(&[&root.g("model").str(), model_name]));
    let mut steps: Vec<Value> = Vec::with_capacity(4);
    root.g("content").for_each(|_, part| {
        if let Some(step) = content_block_to_step(&part) {
            steps.push(step);
        }
        true
    });
    if !steps.is_empty() {
        cpa_json::set(&mut out, "steps", Value::Array(steps));
    }
    set_usage_from_claude(&mut out, "usage", &root.g("usage"));
    cpa_json::to_vec(&out)
}

/// Replays a buffered Claude SSE body into a single interaction.
fn convert_sse_non_stream(model_name: &str, raw_json: &[u8]) -> Vec<u8> {
    let mut out = json!({"id": "", "object": "interaction", "status": "completed", "model": "", "steps": []});
    cpa_json::set(&mut out, "id", generated_interaction_id());
    cpa_json::set(&mut out, "model", model_name);
    let mut st = StreamState { model: model_name.to_string(), ..Default::default() };
    let mut steps: Vec<Value> = Vec::with_capacity(8);
    for line in raw_json.split(|&b| b == b'\n') {
        let line = trim_space(line);
        if !line.starts_with(DATA_TAG) {
            continue;
        }
        let payload = trim_space(&line[DATA_TAG.len()..]);
        if payload == b"[DONE]" {
            continue;
        }
        let root_value = cpa_json::parse(payload);
        let root = Res::of(&root_value);
        match root.g("type").str().as_str() {
            "message_start" => {
                let msg = root.g("message");
                let id = msg.g("id").str();
                if !id.is_empty() {
                    cpa_json::set(&mut out, "id", id);
                }
                let model = msg.g("model").str();
                if !model.is_empty() {
                    cpa_json::set(&mut out, "model", model);
                }
                merge_usage(&mut st, &msg.g("usage"));
            }
            "content_block_start" => non_stream_block_start(&root, &mut st),
            "content_block_delta" => non_stream_block_delta(&root, &mut st),
            "content_block_stop" => steps.push(non_stream_block_stop(&root, &mut st)),
            "message_delta" => merge_usage(&mut st, &root.g("usage")),
            _ => {}
        }
    }
    if !steps.is_empty() {
        cpa_json::set(&mut out, "steps", Value::Array(steps));
    }
    set_usage_from_claude(&mut out, "usage", &merged_usage(&st));
    cpa_json::to_vec(&out)
}

fn convert_event(model_name: &str, raw_json: &[u8], st: &mut StreamState) -> Chunks {
    let mut out = Chunks::new();
    let Some(payload) = sse_payload(raw_json) else {
        return out;
    };
    if payload.is_empty() {
        return out;
    }
    if trim_space(&payload) == b"[DONE]" {
        append_done(&mut out, st);
        return out;
    }
    let root_value = cpa_json::parse(&payload);
    let root = Res::of(&root_value);
    match root.g("type").str().as_str() {
        "message_start" => {
            let msg = root.g("message");
            st.id = first_non_empty(&[&msg.g("id").str(), &st.id, &generated_interaction_id()]);
            st.model = first_non_empty(&[&msg.g("model").str(), &st.model, model_name]);
            merge_usage(st, &msg.g("usage"));
            let model = st.model.clone();
            append_created(&mut out, st, &model);
        }
        "content_block_start" => block_start(model_name, &root, st, &mut out),
        "content_block_delta" => block_delta(model_name, &root, st, &mut out),
        "content_block_stop" => {
            let index = root.g("index").int();
            append_step_stop(&mut out, st);
            st.current_step_by_index.remove(&index);
            st.tool_names.remove(&index);
            st.tool_ids.remove(&index);
            st.tool_args.remove(&index);
        }
        "message_delta" => {
            merge_usage(st, &root.g("usage"));
            append_step_stop(&mut out, st);
            append_completed(&mut out, st, model_name, &root);
        }
        "message_stop" => {
            if !st.completed {
                append_completed(&mut out, st, model_name, &root);
            }
        }
        "error" => {
            append_created(&mut out, st, model_name);
            append_completed(&mut out, st, model_name, &root);
        }
        _ => {}
    }
    out
}

fn block_start(model_name: &str, root: &Res<'_>, st: &mut StreamState, out: &mut Chunks) {
    append_created(out, st, model_name);
    append_step_stop(out, st);
    let index = root.g("index").int();
    let block = root.g("content_block");
    let step_type = block_step_type(&block.g("type").str());
    st.current_step_by_index.insert(index, step_type.to_string());
    if step_type == "function_call" {
        let name = block.g("name").str();
        if !name.is_empty() {
            st.tool_names.insert(index, name);
        }
        let id = block.g("id").str();
        if !id.is_empty() {
            st.tool_ids.insert(index, id);
        }
        let input = block.g("input");
        if input.is_object() && input.raw() != "{}" {
            st.tool_args.insert(index, input.raw());
        }
    }
    let step = block_to_step(&block, step_type);
    append_step_start(out, st, step_type, step);
}

fn block_delta(model_name: &str, root: &Res<'_>, st: &mut StreamState, out: &mut Chunks) {
    let index = root.g("index").int();
    let step_type = st.current_step_by_index.get(&index).cloned().unwrap_or_default();
    if step_type.is_empty() {
        let step_type = delta_step_type(&root.g("delta.type").str());
        append_created(out, st, model_name);
        append_step_stop(out, st);
        append_step_start(out, st, step_type, json!({"type": step_type}));
        st.current_step_by_index.insert(index, step_type.to_string());
        append_delta(out, st, &root.g("delta"), index);
        return;
    }
    if !st.active_step_open || st.active_step_index != index {
        append_created(out, st, model_name);
        append_step_stop(out, st);
        let step = step_for_known_index(&step_type, index, st);
        append_step_start(out, st, &step_type, step);
        append_delta(out, st, &root.g("delta"), index);
        return;
    }
    append_delta(out, st, &root.g("delta"), index);
}

fn append_delta(out: &mut Chunks, st: &mut StreamState, delta: &Res<'_>, index: i64) {
    match delta.g("type").str().as_str() {
        "text_delta" => append_text_delta(out, st, &delta.g("text").str(), false),
        "thinking_delta" => append_text_delta(out, st, &delta.g("thinking").str(), true),
        "input_json_delta" => {
            let partial = delta.g("partial_json").str();
            st.tool_args.entry(index).or_default().push_str(&partial);
            append_arguments_delta(out, st, &partial);
        }
        _ => {}
    }
}

fn content_block_to_step(part: &Res<'_>) -> Option<Value> {
    match part.g("type").str().as_str() {
        "text" => Some(json!({
            "type": "model_output",
            "content": [{"type": "text", "text": part.g("text").str()}],
        })),
        "thinking" => Some(json!({
            "type": "thought",
            "content": [{"type": "text", "text": part.g("thinking").str()}],
        })),
        "tool_use" => Some(tool_use_to_step(
            &part.g("name").str(),
            &part.g("id").str(),
            part.g("input").raw().trim(),
        )),
        _ => None,
    }
}

/// A `function_call` step; `args_raw` is used as `arguments` when it is valid JSON.
fn tool_use_to_step(name: &str, id: &str, args_raw: &str) -> Value {
    let mut step = json!({"type": "function_call", "name": name, "arguments": {}});
    if !id.is_empty() {
        cpa_json::set(&mut step, "id", id);
        cpa_json::set(&mut step, "call_id", id);
    }
    if !args_raw.is_empty() && cpa_json::valid(args_raw.as_bytes()) {
        let _ = cpa_json::set_raw(&mut step, "arguments", args_raw);
    }
    step
}

fn block_to_step(block: &Res<'_>, step_type: &str) -> Value {
    let mut step = json!({"type": step_type});
    if step_type == "function_call" {
        cpa_json::set(&mut step, "name", block.g("name").str());
        let id = block.g("id").str();
        if !id.is_empty() {
            cpa_json::set(&mut step, "id", id.as_str());
            cpa_json::set(&mut step, "call_id", id);
        }
        cpa_json::set(&mut step, "arguments", json!({}));
    }
    step
}

fn step_for_known_index(step_type: &str, index: i64, st: &StreamState) -> Value {
    let mut step = json!({"type": step_type});
    if step_type == "function_call" {
        cpa_json::set(&mut step, "name", st.tool_names.get(&index).cloned().unwrap_or_default());
        let id = st.tool_ids.get(&index).cloned().unwrap_or_default();
        if !id.is_empty() {
            cpa_json::set(&mut step, "id", id.as_str());
            cpa_json::set(&mut step, "call_id", id);
        }
        cpa_json::set(&mut step, "arguments", json!({}));
    }
    step
}

fn non_stream_block_start(root: &Res<'_>, st: &mut StreamState) {
    let index = root.g("index").int();
    let block = root.g("content_block");
    st.current_step_by_index.insert(index, block_step_type(&block.g("type").str()).to_string());
    if block.g("type").str() != "tool_use" {
        return;
    }
    st.tool_names.insert(index, block.g("name").str());
    st.tool_ids.insert(index, block.g("id").str());
    let input = block.g("input");
    if input.is_object() && input.raw() != "{}" {
        st.tool_args.insert(index, input.raw());
    }
}

fn non_stream_block_delta(root: &Res<'_>, st: &mut StreamState) {
    let index = root.g("index").int();
    let delta = root.g("delta");
    let delta_type = delta.g("type").str();
    let piece = match delta_type.as_str() {
        "text_delta" => delta.g("text").str(),
        "thinking_delta" => delta.g("thinking").str(),
        "input_json_delta" => delta.g("partial_json").str(),
        _ => return,
    };
    st.tool_args.entry(index).or_default().push_str(&piece);
}

fn non_stream_block_stop(root: &Res<'_>, st: &mut StreamState) -> Value {
    let index = root.g("index").int();
    let step_type = st.current_step_by_index.get(&index).cloned().unwrap_or_default();
    let text = st.tool_args.get(&index).cloned().unwrap_or_default();
    let step = match step_type.as_str() {
        "function_call" => {
            let id = st.tool_ids.get(&index).cloned().unwrap_or_default();
            let name = st.tool_names.get(&index).cloned().unwrap_or_default();
            tool_use_to_step(&name, &id, text.trim())
        }
        other => {
            let kind = if other == "thought" { "thought" } else { "model_output" };
            json!({"type": kind, "content": [{"type": "text", "text": text}]})
        }
    };
    st.current_step_by_index.remove(&index);
    st.tool_names.remove(&index);
    st.tool_ids.remove(&index);
    st.tool_args.remove(&index);
    step
}

/// Merges the token counters of `usage` into the stream's usage object (last value wins).
fn merge_usage(st: &mut StreamState, usage: &Res<'_>) {
    if !usage.exists() {
        return;
    }
    let merged = st.usage_raw.get_or_insert_with(|| json!({}));
    for key in [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "thinking_tokens",
    ] {
        let value = usage.g(key);
        if value.exists() {
            cpa_json::set(merged, key, value.value());
        }
    }
}

fn merged_usage(st: &StreamState) -> Res<'_> {
    match &st.usage_raw {
        Some(v) => Res::of(v),
        None => Res::NONE,
    }
}

/// Writes Interactions usage fields under `path` from Claude usage counters.
fn set_usage_from_claude(out: &mut Value, path: &str, usage: &Res<'_>) {
    if !usage.exists() {
        return;
    }
    let input_tokens = usage.g("input_tokens").int();
    let output_tokens = usage.g("output_tokens").int();
    let cache_read = usage.g("cache_read_input_tokens").int();
    let cache_creation = usage.g("cache_creation_input_tokens").int();
    let thinking_tokens = usage.g("thinking_tokens").int();
    let has_input = usage.g("input_tokens").exists();
    let has_output = usage.g("output_tokens").exists();
    if has_input {
        cpa_json::set(out, &format!("{path}.input_tokens"), input_tokens);
        cpa_json::set(out, &format!("{path}.total_input_tokens"), input_tokens);
    }
    if has_output {
        cpa_json::set(out, &format!("{path}.output_tokens"), output_tokens);
        cpa_json::set(out, &format!("{path}.total_output_tokens"), output_tokens);
    }
    if has_input || has_output {
        cpa_json::set(out, &format!("{path}.total_tokens"), input_tokens.wrapping_add(output_tokens));
    }
    if cache_read != 0 || cache_creation != 0 {
        let cached = cache_read.wrapping_add(cache_creation);
        cpa_json::set(out, &format!("{path}.cached_tokens"), cached);
        cpa_json::set(out, &format!("{path}.total_cached_tokens"), cached);
    }
    if thinking_tokens != 0 {
        cpa_json::set(out, &format!("{path}.reasoning_tokens"), thinking_tokens);
        cpa_json::set(out, &format!("{path}.total_thought_tokens"), thinking_tokens);
    }
}

fn append_created(out: &mut Chunks, st: &mut StreamState, model_name: &str) {
    if st.created {
        return;
    }
    st.id = first_non_empty(&[&st.id, &generated_interaction_id()]);
    let created = json!({
        "interaction": {
            "id": st.id,
            "status": "in_progress",
            "object": "interaction",
            "model": first_non_empty(&[&st.model, model_name]),
        },
        "event_type": "interaction.created",
    });
    out.push(sse_event_data("interaction.created", &cpa_json::to_vec(&created)));
    st.created = true;
    append_status_update(out, st);
}

fn append_status_update(out: &mut Chunks, st: &mut StreamState) {
    if st.status_updated {
        return;
    }
    let status_update = json!({
        "interaction_id": st.id,
        "status": "in_progress",
        "event_type": "interaction.status_update",
    });
    out.push(sse_event_data("interaction.status_update", &cpa_json::to_vec(&status_update)));
    st.status_updated = true;
}

fn append_step_start(out: &mut Chunks, st: &mut StreamState, step_type: &str, step: Value) {
    st.active_step_index = st.step_index;
    st.active_step_open = true;
    let mut payload = json!({"index": st.active_step_index, "step": {"type": ""}, "event_type": "step.start"});
    if step.is_null() {
        cpa_json::set(&mut payload, "step.type", step_type);
    } else {
        cpa_json::set(&mut payload, "step", step);
    }
    out.push(sse_event_data("step.start", &cpa_json::to_vec(&payload)));
}

fn append_text_delta(out: &mut Chunks, st: &StreamState, text: &str, thought: bool) {
    let mut payload = json!({"index": st.active_step_index, "delta": {"text": "", "type": "text"}, "event_type": "step.delta"});
    if thought {
        cpa_json::set(&mut payload, "delta.type", "thought_summary");
        cpa_json::set(&mut payload, "delta.content.type", "text");
        cpa_json::set(&mut payload, "delta.content.text", text);
        cpa_json::delete(&mut payload, "delta.text");
    } else {
        cpa_json::set(&mut payload, "delta.text", text);
    }
    out.push(sse_event_data("step.delta", &cpa_json::to_vec(&payload)));
}

fn append_arguments_delta(out: &mut Chunks, st: &StreamState, arguments: &str) {
    let payload = json!({
        "index": st.active_step_index,
        "delta": {"arguments": arguments, "type": "arguments_delta"},
        "event_type": "step.delta",
    });
    out.push(sse_event_data("step.delta", &cpa_json::to_vec(&payload)));
}

fn append_step_stop(out: &mut Chunks, st: &mut StreamState) {
    if !st.active_step_open {
        return;
    }
    let payload = json!({"index": st.active_step_index, "event_type": "step.stop"});
    out.push(sse_event_data("step.stop", &cpa_json::to_vec(&payload)));
    st.active_step_open = false;
    st.step_index += 1;
}

fn append_completed(out: &mut Chunks, st: &mut StreamState, model_name: &str, root: &Res<'_>) {
    if st.completed {
        return;
    }
    append_created(out, st, model_name);
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut completed = json!({
        "interaction": {
            "id": "",
            "status": "completed",
            "usage": {},
            "created": "",
            "updated": "",
            "service_tier": "standard",
            "object": "interaction",
            "model": "",
        },
        "event_type": "interaction.completed",
    });
    cpa_json::set(&mut completed, "interaction.id", st.id.as_str());
    cpa_json::set(&mut completed, "interaction.created", now.as_str());
    cpa_json::set(&mut completed, "interaction.updated", now);
    cpa_json::set(&mut completed, "interaction.model", first_non_empty(&[&st.model, model_name]));
    let merged = merged_usage(st);
    let root_usage;
    let usage = if merged.exists() {
        &merged
    } else {
        root_usage = root.g("usage");
        &root_usage
    };
    set_usage_from_claude(&mut completed, "interaction.usage", usage);
    out.push(sse_event_data("interaction.completed", &cpa_json::to_vec(&completed)));
    st.completed = true;
}

fn append_done(out: &mut Chunks, st: &mut StreamState) {
    if st.done {
        return;
    }
    out.push(sse_event_data("done", b"[DONE]"));
    st.done = true;
}

/// The JSON payload of an SSE line: `[DONE]` as-is, the text after `data:`, or `None` for other
/// lines.
fn sse_payload(raw_json: &[u8]) -> Option<Vec<u8>> {
    let raw = trim_space(raw_json);
    if raw == b"[DONE]" {
        return Some(raw.to_vec());
    }
    if !raw.starts_with(DATA_TAG) {
        return None;
    }
    Some(trim_space(&raw[DATA_TAG.len()..]).to_vec())
}

fn block_step_type(block_type: &str) -> &'static str {
    match block_type {
        "thinking" => "thought",
        "tool_use" => "function_call",
        _ => "model_output",
    }
}

fn delta_step_type(delta_type: &str) -> &'static str {
    match delta_type {
        "thinking_delta" => "thought",
        "input_json_delta" => "function_call",
        _ => "model_output",
    }
}
