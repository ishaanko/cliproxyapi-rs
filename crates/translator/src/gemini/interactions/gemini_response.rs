//! Gemini response to Interactions response (Go: interactions_gemini_common.go and
//! interactions_gemini_response.go, Gemini upstream with an Interactions client).

use crate::common::unix_nano_now;
use cpa_json::{json, Value, J};

use super::shared::{gemini_part_to_interactions_steps, interactions_thought_signature};
use crate::common::sse_event_data;
use crate::registry::{Ctx, Param};

/// Per-stream conversion state.
#[derive(Default)]
pub struct StreamState {
    started: bool,
    finished: bool,
    completed: bool,
    done: bool,
    active_step_open: bool,
    id: String,
    step_id: String,
    active_step_type: String,
    active_step_index: i64,
    step_index: i64,
}

fn frame(event: &str, payload: &Value) -> Vec<u8> {
    sse_event_data(event, &cpa_json::to_vec(payload))
}

/// Translates one Gemini stream chunk into Interactions SSE events.
pub fn convert_gemini_response_to_interactions(
    ctx: &Ctx,
    model: &str,
    original: &[u8],
    translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    convert_gemini_response_to_interactions_stream(ctx, model, original, translated, raw, param)
}

/// Streaming converter (named separately in Go; [`convert_gemini_response_to_interactions`] wraps it).
pub fn convert_gemini_response_to_interactions_stream(
    _ctx: &Ctx,
    model_name: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let st = param.state(|| StreamState { id: format!("interaction_{}", unix_nano_now()), ..Default::default() });
    let mut out: Vec<Vec<u8>> = Vec::new();
    if raw.trim_ascii() == b"[DONE]" {
        if !st.completed {
            append_step_stop(&mut out, st);
            append_completed(&mut out, st, model_name, None);
        }
        append_done(&mut out, st);
        return out;
    }
    let root = cpa_json::parse(raw);
    if !st.started {
        append_created(&mut out, st, model_name);
        append_status_update(&mut out, st);
        st.started = true;
    }
    let part_raws = cpa_json::raw_children(raw, "candidates.0.content.parts");
    for (part_index, part) in root.g("candidates.0.content.parts").array().into_iter().enumerate() {
        // Go copies `functionCall.args` as source text into the arguments delta.
        let args_text = crate::common::raw_in(part_raws.get(part_index), "functionCall.args");
        append_gemini_part_to_stream(&mut out, st, &part.value(), args_text);
    }
    let has_finish = root.g("candidates.0.finishReason").exists();
    let has_usage = has_stream_usage(&root);
    if has_finish && !st.finished {
        append_step_stop(&mut out, st);
        st.finished = true;
    }
    if has_usage && st.finished && !st.completed {
        append_completed(&mut out, st, model_name, Some(&root));
    }
    out
}

/// Converts a complete Gemini response into an Interactions `interaction` object.
pub fn convert_gemini_response_to_interactions_non_stream(
    _ctx: &Ctx,
    model_name: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let root = cpa_json::parse(raw);
    let mut out = json!({ "id": "", "object": "interaction", "status": "completed", "model": "", "steps": [] });
    let mut id = root.g("responseId").str();
    if id.is_empty() {
        id = format!("interaction_{}", unix_nano_now());
    }
    cpa_json::set(&mut out, "id", id);
    cpa_json::set(&mut out, "model", model_name);
    let mut steps: Vec<Value> = Vec::new();
    for part in root.g("candidates.0.content.parts").array() {
        steps.extend(gemini_part_to_interactions_steps(&part.value()));
    }
    if !steps.is_empty() {
        cpa_json::set(&mut out, "steps", Value::Array(steps));
    }
    set_usage_from_gemini(&mut out, "usage", &root);
    Some(cpa_json::to_vec(&out))
}

fn usage_of(root: &Value) -> Option<Value> {
    let usage = root.g("usageMetadata");
    if usage.exists() {
        return usage.into_value();
    }
    root.g("usage_metadata").into_value()
}

fn first_usage(usage: &Value, paths: &[&str]) -> Option<Value> {
    paths.iter().find_map(|path| usage.g(path).into_value())
}

fn first_usage_int(usage: &Value, paths: &[&str]) -> i64 {
    first_usage(usage, paths).map(|v| cpa_json::Res::owned(v).int()).unwrap_or(0)
}

fn has_stream_usage(root: &Value) -> bool {
    let Some(usage) = usage_of(root) else { return false };
    [
        "promptTokenCount",
        "candidatesTokenCount",
        "totalTokenCount",
        "thoughtsTokenCount",
        "cachedContentTokenCount",
        "prompt_token_count",
        "candidates_token_count",
        "total_token_count",
        "thoughts_token_count",
        "cached_content_token_count",
    ]
    .iter()
    .any(|path| usage.g(path).exists())
}

fn append_created(out: &mut Vec<Vec<u8>>, st: &StreamState, model_name: &str) {
    let mut created = json!({
        "interaction": { "id": "", "status": "in_progress", "object": "interaction", "model": "" },
        "event_type": "interaction.created",
    });
    cpa_json::set(&mut created, "interaction.id", st.id.as_str());
    cpa_json::set(&mut created, "interaction.model", model_name);
    out.push(frame("interaction.created", &created));
}

fn append_status_update(out: &mut Vec<Vec<u8>>, st: &StreamState) {
    let mut update = json!({ "interaction_id": "", "status": "in_progress", "event_type": "interaction.status_update" });
    cpa_json::set(&mut update, "interaction_id", st.id.as_str());
    out.push(frame("interaction.status_update", &update));
}

fn append_completed(out: &mut Vec<Vec<u8>>, st: &mut StreamState, model_name: &str, root: Option<&Value>) {
    let now = crate::common::utc_now_rfc3339();
    let mut completed = json!({
        "interaction": {
            "id": "", "status": "completed", "usage": {}, "created": "", "updated": "",
            "service_tier": "standard", "object": "interaction", "model": "",
        },
        "event_type": "interaction.completed",
    });
    cpa_json::set(&mut completed, "interaction.id", st.id.as_str());
    cpa_json::set(&mut completed, "interaction.created", now.as_str());
    cpa_json::set(&mut completed, "interaction.updated", now.as_str());
    cpa_json::set(&mut completed, "interaction.model", model_name);
    if let Some(root) = root {
        set_stream_usage_from_gemini(&mut completed, "interaction.usage", root);
    }
    out.push(frame("interaction.completed", &completed));
    st.completed = true;
}

fn append_done(out: &mut Vec<Vec<u8>>, st: &mut StreamState) {
    if st.done {
        return;
    }
    out.push(sse_event_data("done", b"[DONE]"));
    st.done = true;
}

/// Non-stream usage: input/output/reasoning/total/cached token counts.
fn set_usage_from_gemini(out: &mut Value, path: &str, root: &Value) {
    let Some(usage) = usage_of(root) else { return };
    cpa_json::set(out, &format!("{path}.input_tokens"), first_usage_int(&usage, &["promptTokenCount", "prompt_token_count"]));
    cpa_json::set(out, &format!("{path}.output_tokens"), first_usage_int(&usage, &["candidatesTokenCount", "candidates_token_count"]));
    if let Some(reasoning) = first_usage(&usage, &["thoughtsTokenCount", "thoughts_token_count"]) {
        cpa_json::set(out, &format!("{path}.reasoning_tokens"), cpa_json::Res::owned(reasoning).int());
    }
    cpa_json::set(out, &format!("{path}.total_tokens"), first_usage_int(&usage, &["totalTokenCount", "total_token_count"]));
    let cached = usage.g("cachedContentTokenCount");
    let cached = if cached.exists() { cached } else { usage.g("cached_content_token_count") };
    if cached.exists() {
        cpa_json::set(out, &format!("{path}.cached_tokens"), cached.int());
    }
}

/// Stream completion usage in the Interactions `total_*` shape.
fn set_stream_usage_from_gemini(out: &mut Value, path: &str, root: &Value) {
    let Some(usage) = usage_of(root) else { return };
    let input_tokens = first_usage_int(&usage, &["promptTokenCount", "prompt_token_count"]);
    let output_tokens = first_usage_int(&usage, &["candidatesTokenCount", "candidates_token_count"]);
    let total_tokens = first_usage_int(&usage, &["totalTokenCount", "total_token_count"]);
    let thought_tokens = first_usage_int(&usage, &["thoughtsTokenCount", "thoughts_token_count"]);
    let mut cached_tokens = usage.g("cachedContentTokenCount").int();
    if cached_tokens == 0 {
        cached_tokens = usage.g("cached_content_token_count").int();
    }
    cpa_json::set(out, &format!("{path}.total_tokens"), total_tokens);
    cpa_json::set(out, &format!("{path}.total_input_tokens"), input_tokens);
    cpa_json::set(
        out,
        &format!("{path}.input_tokens_by_modality"),
        json!([{ "modality": "text", "tokens": input_tokens }]),
    );
    cpa_json::set(out, &format!("{path}.total_cached_tokens"), cached_tokens);
    cpa_json::set(out, &format!("{path}.total_output_tokens"), output_tokens);
    cpa_json::set(out, &format!("{path}.total_tool_use_tokens"), 0);
    cpa_json::set(out, &format!("{path}.total_thought_tokens"), thought_tokens);
}

fn append_step_start(out: &mut Vec<Vec<u8>>, st: &mut StreamState, step_type: &str, part: &Value) {
    st.step_id = format!("step_{}", unix_nano_now());
    st.active_step_index = st.step_index;
    st.step_index += 1;
    st.active_step_type = step_type.to_string();
    st.active_step_open = true;
    let mut step_start = json!({ "index": 0, "step": { "type": "" }, "event_type": "step.start" });
    cpa_json::set(&mut step_start, "index", st.active_step_index);
    cpa_json::set(&mut step_start, "step.type", step_type);
    if step_type == "function_call" {
        let mut id = function_part_id(part);
        if id.is_empty() {
            id = st.step_id.clone();
        }
        cpa_json::set(&mut step_start, "step.id", id);
        cpa_json::set(&mut step_start, "step.name", part.g("name").str());
        cpa_json::set(&mut step_start, "step.arguments", json!({}));
    }
    out.push(frame("step.start", &step_start));
}

fn append_step_stop(out: &mut Vec<Vec<u8>>, st: &mut StreamState) {
    if !st.active_step_open {
        return;
    }
    let mut step_stop = json!({ "index": 0, "event_type": "step.stop" });
    cpa_json::set(&mut step_stop, "index", st.active_step_index);
    out.push(frame("step.stop", &step_stop));
    st.active_step_open = false;
    st.active_step_type.clear();
}

fn ensure_step(out: &mut Vec<Vec<u8>>, st: &mut StreamState, step_type: &str, part: &Value) {
    if st.active_step_open && st.active_step_type == step_type {
        return;
    }
    append_step_stop(out, st);
    append_step_start(out, st, step_type, part);
}

fn append_gemini_part_to_stream(out: &mut Vec<Vec<u8>>, st: &mut StreamState, part: &Value, args_text: Option<&str>) {
    let text = part.g("text");
    if text.exists() && !text.str().is_empty() {
        if part.g("thought").bool() {
            ensure_step(out, st, "thought", &Value::Null);
            let mut delta = json!({
                "index": 0,
                "delta": { "content": { "text": "", "type": "text" }, "type": "thought_summary" },
                "event_type": "step.delta",
            });
            cpa_json::set(&mut delta, "index", st.active_step_index);
            cpa_json::set(&mut delta, "delta.content.text", text.str());
            out.push(frame("step.delta", &delta));
            return append_thought_signature(out, st, part);
        }
        ensure_step(out, st, "model_output", &Value::Null);
        let mut delta = json!({ "index": 0, "delta": { "text": "", "type": "text" }, "event_type": "step.delta" });
        cpa_json::set(&mut delta, "index", st.active_step_index);
        cpa_json::set(&mut delta, "delta.text", text.str());
        out.push(frame("step.delta", &delta));
        return append_thought_signature(out, st, part);
    }
    let fc = part.g("functionCall");
    if fc.exists() {
        append_thought_signature(out, st, part);
        ensure_step(out, st, "function_call", &fc.value());
        let mut delta = json!({
            "index": 0,
            "delta": { "arguments": "", "type": "arguments_delta" },
            "event_type": "step.delta",
        });
        cpa_json::set(&mut delta, "index", st.active_step_index);
        let args = fc.g("args");
        let arguments = if args.exists() {
            args_text.map(str::to_string).unwrap_or_else(|| args.raw())
        } else {
            "{}".to_string()
        };
        cpa_json::set(&mut delta, "delta.arguments", arguments);
        out.push(frame("step.delta", &delta));
        return append_step_stop(out, st);
    }
    let fr = part.g("functionResponse");
    if fr.exists() {
        ensure_step(out, st, "function_result", &fr.value());
        let mut delta = json!({
            "index": 0,
            "delta": { "type": "function_result", "name": "", "result": {} },
            "event_type": "step.delta",
        });
        cpa_json::set(&mut delta, "index", st.active_step_index);
        cpa_json::set(&mut delta, "delta.name", fr.g("name").str());
        let response = fr.g("response");
        if response.exists() {
            cpa_json::set(&mut delta, "delta.result", response.value());
        }
        out.push(frame("step.delta", &delta));
        return append_step_stop(out, st);
    }
    if !interactions_thought_signature(part).is_empty() {
        append_thought_signature(out, st, part);
    }
}

fn append_thought_signature(out: &mut Vec<Vec<u8>>, st: &mut StreamState, part: &Value) {
    let signature = interactions_thought_signature(part);
    if signature.is_empty() {
        return;
    }
    ensure_step(out, st, "thought", &Value::Null);
    let mut delta = json!({
        "index": 0,
        "delta": { "signature": "", "type": "thought_signature" },
        "event_type": "step.delta",
    });
    cpa_json::set(&mut delta, "index", st.active_step_index);
    cpa_json::set(&mut delta, "delta.signature", signature);
    out.push(frame("step.delta", &delta));
}

fn function_part_id(part: &Value) -> String {
    let id = part.g("id");
    if id.exists() {
        return id.str();
    }
    let call_id = part.g("call_id");
    if call_id.exists() {
        return call_id.str();
    }
    String::new()
}
