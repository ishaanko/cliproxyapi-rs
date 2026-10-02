//! Upstream OpenAI chat response -> client Interactions response
//! (Go: interactions_openai_response.go). Stream output elements are Interactions SSE frames.

use std::collections::HashMap;

use cpa_json::{Res, Value, J};

use super::{
    first_non_empty, interactions_text_step, is_antigravity_model, openai_reasoning_texts, set_items, sse_payload, tmpl, unix_nanos,
};
use crate::common;
use crate::registry::{Ctx, Param};

/// Per-stream state (Go: `openAIToInteractionsStreamState`).
#[derive(Default)]
struct StreamState {
    created: bool,
    status_updated: bool,
    completed: bool,
    done: bool,
    current_step_type: String,
    current_step_id: String,
    tool_call_ids: HashMap<i64, String>,
    tool_call_names: HashMap<i64, String>,
    id: String,
    step_index: i64,
    active_step_index: i64,
    active_step_open: bool,
    usage: Option<Value>,
}

pub(super) fn convert_openai_response_to_interactions(
    _ctx: &Ctx,
    model_name: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let st = param.state(StreamState::default);
    convert_stream(model_name, raw, st)
}

pub(super) fn convert_openai_response_to_interactions_non_stream(
    _ctx: &Ctx,
    model_name: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let root = cpa_json::parse(raw);
    let mut out = tmpl(r#"{"id":"","status":"completed","object":"interaction","model":"","steps":[]}"#);
    let id = first_non_empty(&[&root.g("id").str(), &format!("interaction_{}", unix_nanos())]);
    cpa_json::set(&mut out, "id", id);
    cpa_json::set(&mut out, "model", first_non_empty(&[model_name, &root.g("model").str()]));
    let mut steps: Vec<Value> = Vec::new();
    root.g("choices").for_each(|_, choice| {
        let message = choice.g("message");
        let reasoning = message.g("reasoning_content");
        if reasoning.exists() {
            for text in openai_reasoning_texts(&reasoning) {
                steps.push(interactions_text_step("thought", &text));
            }
        }
        let content = message.g("content");
        if content.exists() && !content.str().is_empty() {
            steps.push(interactions_text_step("model_output", &content.str()));
        }
        let tool_calls = message.g("tool_calls");
        if tool_calls.is_array() {
            let for_antigravity = is_antigravity_model(model_name);
            steps.extend(tool_calls.array().iter().filter_map(|tc| tool_call_to_interactions_step(tc, for_antigravity)));
        }
        let finish_reason = choice.g("finish_reason");
        if finish_reason.exists() {
            cpa_json::set(&mut out, "finish_reason", finish_reason.str());
        }
        true
    });
    set_items(&mut out, "steps", steps);
    set_usage(&mut out, "usage", &root.g("usage"));
    Some(cpa_json::to_vec(&out))
}

fn convert_stream(model_name: &str, raw: &[u8], st: &mut StreamState) -> Vec<Vec<u8>> {
    let payload = sse_payload(raw);
    if payload.is_empty() {
        return vec![];
    }
    let mut out: Vec<Vec<u8>> = Vec::new();
    if payload.trim_ascii() == b"[DONE]" {
        step_stop(&mut out, st);
        if !st.completed {
            completed(&mut out, st, model_name, &Value::Null);
        }
        done(&mut out, st);
        return out;
    }
    let root = cpa_json::parse(&payload);
    let usage = root.g("usage");
    if usage.exists() {
        st.usage = Some(usage.value());
    }
    let choices = root.g("choices");
    if !choices.is_array() {
        return out;
    }
    if choices.array().is_empty() {
        if usage.exists() {
            step_stop(&mut out, st);
            completed(&mut out, st, model_name, &root);
        }
        return out;
    }
    for choice in choices.array() {
        let delta = choice.g("delta");
        let reasoning = delta.g("reasoning_content");
        if reasoning.exists() {
            for text in openai_reasoning_texts(&reasoning) {
                ensure_step(&mut out, st, model_name, "thought", &root);
                text_delta(&mut out, st, &text, true);
            }
        }
        let content = delta.g("content");
        if content.exists() && !content.str().is_empty() {
            ensure_step(&mut out, st, model_name, "model_output", &root);
            text_delta(&mut out, st, &content.str(), false);
        }
        let tool_calls = delta.g("tool_calls");
        if tool_calls.is_array() {
            for tool_call in tool_calls.array() {
                tool_call_delta(&mut out, st, model_name, &root, &tool_call);
            }
        }
        if choice.g("finish_reason").exists() {
            step_stop(&mut out, st);
        }
    }
    out
}

fn tool_call_delta(out: &mut Vec<Vec<u8>>, st: &mut StreamState, model_name: &str, root: &Value, tool_call: &Res<'_>) {
    let index = tool_call.g("index").int();
    let id = tool_call.g("id").str();
    if !id.is_empty() {
        st.tool_call_ids.insert(index, id);
    }
    let function = tool_call.g("function");
    let mut name = function.g("name").str();
    if !name.is_empty() {
        if is_antigravity_model(model_name) {
            name = common::antigravity_tool_name_to_upstream(&name);
        }
        st.tool_call_names.insert(index, name);
    }
    let step_id = first_non_empty(&[
        st.tool_call_ids.get(&index).map(String::as_str).unwrap_or_default(),
        &format!("call_{index}"),
    ]);
    let step_name = st.tool_call_names.get(&index).cloned().unwrap_or_default();
    if st.current_step_type != "function_call" || st.current_step_id != step_id {
        step_stop(out, st);
        let mut step = tmpl(r#"{"type":"function_call","id":"","name":"","arguments":{}}"#);
        cpa_json::set(&mut step, "id", step_id);
        cpa_json::set(&mut step, "name", step_name);
        created(out, st, model_name, root);
        step_start(out, st, "function_call", &step);
    }
    let args = function.g("arguments");
    if args.exists() && !args.str().is_empty() {
        arguments_delta(out, st, &args.str());
    }
}

fn created(out: &mut Vec<Vec<u8>>, st: &mut StreamState, model_name: &str, root: &Value) {
    if st.created {
        return;
    }
    st.id = first_non_empty(&[&root.g("id").str(), &st.id, &format!("interaction_{}", unix_nanos())]);
    let mut payload = tmpl(
        r#"{"interaction":{"id":"","status":"in_progress","object":"interaction","model":""},"event_type":"interaction.created"}"#,
    );
    cpa_json::set(&mut payload, "interaction.id", st.id.as_str());
    cpa_json::set(&mut payload, "interaction.model", first_non_empty(&[model_name, &root.g("model").str()]));
    out.push(common::sse_event_data("interaction.created", &cpa_json::to_vec(&payload)));
    st.created = true;
    status_update(out, st);
}

fn status_update(out: &mut Vec<Vec<u8>>, st: &mut StreamState) {
    if st.status_updated {
        return;
    }
    let mut payload = tmpl(r#"{"interaction_id":"","status":"in_progress","event_type":"interaction.status_update"}"#);
    cpa_json::set(&mut payload, "interaction_id", st.id.as_str());
    out.push(common::sse_event_data("interaction.status_update", &cpa_json::to_vec(&payload)));
    st.status_updated = true;
}

/// Makes sure a step of `step_type` is open, closing the previous one if it differs.
fn ensure_step(out: &mut Vec<Vec<u8>>, st: &mut StreamState, model_name: &str, step_type: &str, chunk: &Value) {
    created(out, st, model_name, chunk);
    if st.active_step_open && st.current_step_type == step_type {
        return;
    }
    step_stop(out, st);
    step_start(out, st, step_type, chunk);
}

fn step_start(out: &mut Vec<Vec<u8>>, st: &mut StreamState, step_type: &str, step: &Value) {
    let index = st.step_index;
    st.step_index += 1;
    st.active_step_index = index;
    st.current_step_type = step_type.to_string();
    st.active_step_open = true;
    let mut payload = tmpl(r#"{"index":0,"step":{"type":""},"event_type":"step.start"}"#);
    cpa_json::set(&mut payload, "index", index);
    cpa_json::set(&mut payload, "step.type", step_type);
    if step_type == "function_call" {
        let id = first_non_empty(&[&step.g("id").str(), &step.g("call_id").str(), &st.current_step_id]);
        st.current_step_id = id.clone();
        if !id.is_empty() {
            cpa_json::set(&mut payload, "step.id", id);
        }
        cpa_json::set(&mut payload, "step.name", step.g("name").str());
        cpa_json::set(&mut payload, "step.arguments", tmpl("{}"));
    } else {
        st.current_step_id.clear();
    }
    out.push(common::sse_event_data("step.start", &cpa_json::to_vec(&payload)));
}

fn text_delta(out: &mut Vec<Vec<u8>>, st: &StreamState, text: &str, thought: bool) {
    let payload = if thought {
        let mut p = tmpl(
            r#"{"index":0,"delta":{"content":{"text":"","type":"text"},"type":"thought_summary"},"event_type":"step.delta"}"#,
        );
        cpa_json::set(&mut p, "index", st.active_step_index);
        cpa_json::set(&mut p, "delta.content.text", text);
        p
    } else {
        let mut p = tmpl(r#"{"index":0,"delta":{"text":"","type":"text"},"event_type":"step.delta"}"#);
        cpa_json::set(&mut p, "index", st.active_step_index);
        cpa_json::set(&mut p, "delta.text", text);
        p
    };
    out.push(common::sse_event_data("step.delta", &cpa_json::to_vec(&payload)));
}

fn arguments_delta(out: &mut Vec<Vec<u8>>, st: &StreamState, arguments: &str) {
    let mut payload = tmpl(r#"{"index":0,"delta":{"arguments":"","type":"arguments_delta"},"event_type":"step.delta"}"#);
    cpa_json::set(&mut payload, "index", st.active_step_index);
    cpa_json::set(&mut payload, "delta.arguments", arguments);
    out.push(common::sse_event_data("step.delta", &cpa_json::to_vec(&payload)));
}

fn step_stop(out: &mut Vec<Vec<u8>>, st: &mut StreamState) {
    if !st.active_step_open {
        return;
    }
    let mut payload = tmpl(r#"{"index":0,"event_type":"step.stop"}"#);
    cpa_json::set(&mut payload, "index", st.active_step_index);
    out.push(common::sse_event_data("step.stop", &cpa_json::to_vec(&payload)));
    st.active_step_open = false;
    st.current_step_type.clear();
    st.current_step_id.clear();
}

fn completed(out: &mut Vec<Vec<u8>>, st: &mut StreamState, model_name: &str, root: &Value) {
    if st.completed {
        return;
    }
    if !st.created {
        created(out, st, model_name, root);
    }
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut payload = tmpl(
        r#"{"interaction":{"id":"","status":"completed","usage":{},"created":"","updated":"","service_tier":"standard","object":"interaction","model":""},"event_type":"interaction.completed"}"#,
    );
    cpa_json::set(&mut payload, "interaction.id", st.id.as_str());
    cpa_json::set(&mut payload, "interaction.created", now.as_str());
    cpa_json::set(&mut payload, "interaction.updated", now);
    cpa_json::set(&mut payload, "interaction.model", first_non_empty(&[model_name, &root.g("model").str()]));
    let mut usage = root.g("usage");
    if !usage.exists() {
        usage = st.usage.as_ref().map(Res::of).unwrap_or(Res::NONE);
    }
    set_usage(&mut payload, "interaction.usage", &usage);
    out.push(common::sse_event_data("interaction.completed", &cpa_json::to_vec(&payload)));
    st.completed = true;
}

fn done(out: &mut Vec<Vec<u8>>, st: &mut StreamState) {
    if st.done {
        return;
    }
    out.push(common::sse_event_data("done", b"[DONE]"));
    st.done = true;
}

/// An Interactions `function_call` step from an OpenAI tool call; `None` for non-function calls.
pub(super) fn tool_call_to_interactions_step(tool_call: &Res<'_>, for_antigravity: bool) -> Option<Value> {
    let tool_type = tool_call.g("type").str();
    if !tool_type.is_empty() && tool_type != "function" {
        return None;
    }
    let function = tool_call.g("function");
    if !function.exists() {
        return None;
    }
    let mut step = tmpl(r#"{"type":"function_call","name":"","arguments":{}}"#);
    let id = tool_call.g("id").str();
    if !id.is_empty() {
        cpa_json::set(&mut step, "id", id);
    }
    let mut name = function.g("name").str();
    if for_antigravity {
        name = common::antigravity_tool_name_to_upstream(&name);
    }
    cpa_json::set(&mut step, "name", name);
    set_raw_json_value(&mut step, "arguments", &function.g("arguments"), "{}");
    Some(step)
}

/// Maps OpenAI chat usage onto Interactions usage fields under `path`.
fn set_usage(out: &mut Value, path: &str, usage: &Res<'_>) {
    if !usage.exists() {
        return;
    }
    let mappings: [(&str, &[&str]); 5] = [
        ("prompt_tokens", &["input_tokens", "total_input_tokens"]),
        ("completion_tokens", &["output_tokens", "total_output_tokens"]),
        ("total_tokens", &["total_tokens"]),
        ("prompt_tokens_details.cached_tokens", &["cached_tokens", "total_cached_tokens"]),
        ("completion_tokens_details.reasoning_tokens", &["reasoning_tokens", "total_thought_tokens"]),
    ];
    for (source, targets) in mappings {
        let value = usage.g(source);
        if value.exists() {
            for target in targets {
                cpa_json::set(out, &format!("{path}.{target}"), value.int());
            }
        }
    }
}

/// Sets `path` from a tool-call arguments value: JSON text is stored parsed, other strings as
/// strings, non-strings as-is, `fallback` JSON when missing.
fn set_raw_json_value(out: &mut Value, path: &str, value: &Res<'_>, fallback: &str) {
    if !value.exists() {
        cpa_json::set(out, path, tmpl(fallback));
        return;
    }
    if let Some(s) = value.as_str() {
        let trimmed = s.trim();
        if cpa_json::valid(trimmed.as_bytes()) {
            cpa_json::set(out, path, cpa_json::parse_str(trimmed));
        } else {
            cpa_json::set(out, path, s);
        }
        return;
    }
    cpa_json::set(out, path, value.value());
}
