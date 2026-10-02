//! Upstream OpenAI Responses response -> client Interactions response
//! (Go: interactions_openai_responses_response.go, second half). Stream output elements are
//! Interactions SSE frames.

use std::collections::HashSet;

use cpa_json::{Res, Value, J};

use super::raw_text::parse_lenient;
use super::request::{responses_content_part_to_interactions, responses_function_call_to_interactions};
use super::{
    first_non_empty, is_antigravity_model, json_string_value, response_model, set_items, sse_payload, tmpl, unix_nanos,
};
use crate::common;
use crate::registry::{Ctx, Param};

type Events = Vec<Vec<u8>>;

/// Per-stream state (Go: `responsesToInteractionsStreamState`).
#[derive(Default)]
struct StreamState {
    id: String,
    created: bool,
    status_updated: bool,
    completed: bool,
    done: bool,
    step_index: i64,
    active_step_index: i64,
    active_step_type: String,
    active_step_open: bool,
    /// Dedupe keys of text already streamed, so the completed-message fallback does not replay it.
    sent_text: HashSet<String>,
    unkeyed_text_delta: bool,
    function_args_sent: HashSet<String>,
}

impl StreamState {
    fn mark_text_sent(&mut self, keys: Vec<String>) {
        if keys.is_empty() {
            self.unkeyed_text_delta = true;
        }
        self.sent_text.extend(keys);
    }

    fn has_sent_text(&self, keys: &[String], has_content_index: bool) -> bool {
        (!has_content_index && self.unkeyed_text_delta) || keys.iter().any(|k| self.sent_text.contains(k))
    }

    fn has_sent_unkeyed_text(&self, keys: &[String]) -> bool {
        if keys.is_empty() {
            return self.unkeyed_text_delta;
        }
        keys.iter().any(|k| self.sent_text.contains(k))
    }

    fn has_sent_function_args(&self, keys: &[String]) -> bool {
        keys.iter().any(|k| self.function_args_sent.contains(k))
    }
}

/// `{"type":"text","text":<text>}` Interactions content part.
fn interactions_text_content_part(text: &str) -> Value {
    let mut part = tmpl(r#"{"type":"text","text":""}"#);
    cpa_json::set(&mut part, "text", text);
    part
}

fn emit(event: &str, payload: &Value) -> Vec<u8> {
    common::sse_event_data(event, &cpa_json::to_vec(payload))
}

pub(super) fn convert_openai_responses_response_to_interactions(
    _ctx: &Ctx,
    model_name: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let st = param.state(StreamState::default);
    convert_event(model_name, raw, st)
}

pub(super) fn convert_openai_responses_response_to_interactions_non_stream(
    _ctx: &Ctx,
    model_name: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let root = parse_lenient(raw);
    let mut out = tmpl(r#"{"id":"","object":"interaction","status":"completed","model":"","steps":[]}"#);
    let status = root.g("status").str();
    if !status.is_empty() {
        cpa_json::set(&mut out, "status", status);
    }
    cpa_json::set(&mut out, "id", root.g("id").str());
    cpa_json::set(&mut out, "model", response_model(model_name, &root));
    let for_antigravity = is_antigravity_model(model_name);
    let mut step_items: Vec<Value> = Vec::new();
    root.g("output").for_each(|_, item| {
        step_items.extend(output_item_to_interactions_step(&item, for_antigravity));
        true
    });
    set_items(&mut out, "steps", step_items);
    set_usage(&mut out, "usage", &root.g("usage"));
    Some(cpa_json::to_vec(&out))
}

fn convert_event(model_name: &str, raw: &[u8], st: &mut StreamState) -> Events {
    let payload = sse_payload(raw);
    if payload.is_empty() {
        return vec![];
    }
    if payload.trim_ascii() == b"[DONE]" {
        let mut out = Vec::new();
        append_done(&mut out, st);
        return out;
    }
    let root = parse_lenient(&payload);
    match root.g("type").str().as_str() {
        "response.created" => {
            let mut out = Vec::new();
            append_created(&mut out, st, model_name, &root.g("response"), true);
            out
        }
        "response.output_text.delta" => {
            let mut out = Vec::new();
            ensure_step(&mut out, st, model_name, "model_output");
            append_text_delta(&mut out, st, &root.g("delta").str(), false);
            st.mark_text_sent(text_keys_from_event(&root));
            out
        }
        "response.reasoning_summary_text.delta" => {
            let mut out = Vec::new();
            ensure_step(&mut out, st, model_name, "thought");
            append_text_delta(&mut out, st, &root.g("delta").str(), true);
            out
        }
        "response.output_item.added" => output_item_added(model_name, &root, st),
        "response.function_call_arguments.delta" => {
            let mut out = Vec::new();
            ensure_function_call_step(&mut out, st, model_name, &root);
            append_arguments_delta(&mut out, st, &root.g("delta").str());
            let keys = function_args_keys(&root);
            st.function_args_sent.extend(keys);
            out
        }
        "response.output_item.done" => output_item_done(model_name, &root, st),
        "response.completed" | "response.incomplete" => completed(model_name, &root.g("response"), st),
        _ => vec![],
    }
}

/// An Interactions step for a non-stream Responses output item.
fn output_item_to_interactions_step(item: &Res<'_>, for_antigravity: bool) -> Option<Value> {
    match item.g("type").str().as_str() {
        "message" => {
            let mut step = tmpl(r#"{"type":"model_output","content":[]}"#);
            item.g("content").for_each(|_, part| {
                if let Some(converted) = responses_content_part_to_interactions(&part) {
                    cpa_json::set(&mut step, "content.-1", converted);
                }
                true
            });
            Some(step)
        }
        "function_call" => Some(responses_function_call_to_interactions(item, for_antigravity)),
        "reasoning" => {
            let mut step = tmpl(r#"{"type":"thought","content":[]}"#);
            item.g("summary").for_each(|_, summary| {
                let text = summary.g("text").str();
                if !text.is_empty() {
                    cpa_json::set(&mut step, "content.-1", interactions_text_content_part(&text));
                }
                true
            });
            Some(step)
        }
        _ => None,
    }
}

fn output_item_added(model_name: &str, root: &Value, st: &mut StreamState) -> Events {
    let item = root.g("item");
    let mut out = Vec::new();
    match item.g("type").str().as_str() {
        "function_call" => {
            ensure_created(&mut out, st, model_name);
            append_step_stop(&mut out, st);
            let mut step = tmpl(r#"{"type":"function_call","name":"","arguments":{}}"#);
            let mut name = item.g("name").str();
            if is_antigravity_model(model_name) {
                name = common::antigravity_tool_name_to_upstream(&name);
            }
            cpa_json::set(&mut step, "name", name);
            let call_id = first_non_empty(&[&item.g("call_id").str(), &item.g("id").str()]);
            if !call_id.is_empty() {
                cpa_json::set(&mut step, "id", call_id.as_str());
                cpa_json::set(&mut step, "call_id", call_id);
            }
            append_step_start(&mut out, st, "function_call", &step);
        }
        "message" => ensure_step(&mut out, st, model_name, "model_output"),
        "reasoning" => ensure_step(&mut out, st, model_name, "thought"),
        _ => {}
    }
    out
}

fn output_item_done(model_name: &str, root: &Value, st: &mut StreamState) -> Events {
    let item = root.g("item");
    let mut out = Vec::new();
    match item.g("type").str().as_str() {
        "function_call" => {
            ensure_function_call_step(&mut out, st, model_name, root);
            let args = item.g("arguments");
            if args.exists() && !args.str().is_empty() && !st.has_sent_function_args(&function_args_keys(root)) {
                append_arguments_delta(&mut out, st, &json_string_value(&args, "{}"));
            }
            append_step_stop(&mut out, st);
        }
        "reasoning" => {
            ensure_step(&mut out, st, model_name, "thought");
            for summary in item.g("summary").array() {
                let text = summary.g("text").str();
                if !text.is_empty() {
                    append_text_delta(&mut out, st, &text, true);
                }
            }
            append_step_stop(&mut out, st);
        }
        "message" => append_message_fallback(&mut out, model_name, &item, root, st, true),
        _ => {}
    }
    out
}

fn completed(model_name: &str, response: &Res<'_>, st: &mut StreamState) -> Events {
    let mut out = Vec::new();
    response.g("output").for_each(|output_index, item| {
        if item.g("type").str() == "message" {
            let mut index_root = tmpl(r#"{"output_index":0}"#);
            cpa_json::set(&mut index_root, "output_index", output_index.int());
            let id = item.g("id").str();
            if !id.is_empty() {
                cpa_json::set(&mut index_root, "item_id", id);
            }
            append_message_fallback(&mut out, model_name, &item, &index_root, st, false);
        }
        true
    });
    append_step_stop(&mut out, st);
    append_completed(&mut out, st, model_name, response);
    append_done(&mut out, st);
    out
}

/// Emits a completed message's text unless equivalent text was already streamed.
fn append_message_fallback(out: &mut Events, model_name: &str, item: &Res<'_>, root: &Value, st: &mut StreamState, stop: bool) {
    let item_id = item.g("id").str();
    let output_index = root.g("output_index");
    let has_output_index = output_index.exists();
    let output_index = output_index.int();
    item.g("content").for_each(|content_index, part| {
        let part_type = part.g("type").str();
        if part_type != "output_text" && part_type != "text" {
            return true;
        }
        let has_content_index = content_index.exists();
        let keys = text_keys(&item_id, output_index, has_output_index, content_index.int(), has_content_index);
        let unkeyed_keys = unkeyed_text_keys(&item_id, output_index, has_output_index);
        if st.has_sent_text(&keys, has_content_index) || st.has_sent_unkeyed_text(&unkeyed_keys) {
            return true;
        }
        let text = part.g("text").str();
        if text.is_empty() {
            return true;
        }
        ensure_step(out, st, model_name, "model_output");
        append_text_delta(out, st, &text, false);
        st.mark_text_sent(keys);
        true
    });
    if stop {
        append_step_stop(out, st);
    }
}

// ---- Interactions event builders

fn append_created(out: &mut Events, st: &mut StreamState, model_name: &str, response: &Res<'_>, mark_status: bool) {
    if st.created {
        return;
    }
    st.id = first_non_empty(&[&response.g("id").str(), &st.id, &format!("interaction_{}", unix_nanos())]);
    let mut created = tmpl(
        r#"{"interaction":{"id":"","status":"in_progress","object":"interaction","model":""},"event_type":"interaction.created"}"#,
    );
    cpa_json::set(&mut created, "interaction.id", st.id.as_str());
    cpa_json::set(&mut created, "interaction.model", response_model(model_name, &response.value()));
    out.push(emit("interaction.created", &created));
    st.created = true;
    if mark_status {
        append_status_update(out, st);
    }
}

fn append_status_update(out: &mut Events, st: &mut StreamState) {
    if st.status_updated {
        return;
    }
    let mut status_update = tmpl(r#"{"interaction_id":"","status":"in_progress","event_type":"interaction.status_update"}"#);
    cpa_json::set(&mut status_update, "interaction_id", st.id.as_str());
    out.push(emit("interaction.status_update", &status_update));
    st.status_updated = true;
}

fn ensure_created(out: &mut Events, st: &mut StreamState, model_name: &str) {
    append_created(out, st, model_name, &Res::NONE, true);
}

/// Makes sure a step of `step_type` is open, closing the previous one if it differs.
fn ensure_step(out: &mut Events, st: &mut StreamState, model_name: &str, step_type: &str) {
    ensure_created(out, st, model_name);
    if st.active_step_open && st.active_step_type == step_type {
        return;
    }
    append_step_stop(out, st);
    append_step_start(out, st, step_type, &Value::Null);
}

fn append_step_start(out: &mut Events, st: &mut StreamState, step_type: &str, step: &Value) {
    let index = st.step_index;
    st.step_index += 1;
    st.active_step_index = index;
    st.active_step_type = step_type.to_string();
    st.active_step_open = true;
    let mut payload = tmpl(r#"{"index":0,"step":{"type":""},"event_type":"step.start"}"#);
    cpa_json::set(&mut payload, "index", index);
    cpa_json::set(&mut payload, "step.type", step_type);
    if step_type == "function_call" {
        let id = first_non_empty(&[&step.g("call_id").str(), &step.g("id").str()]);
        if !id.is_empty() {
            cpa_json::set(&mut payload, "step.id", id.as_str());
            cpa_json::set(&mut payload, "step.call_id", id);
        }
        cpa_json::set(&mut payload, "step.name", step.g("name").str());
        cpa_json::set(&mut payload, "step.arguments", tmpl("{}"));
    }
    out.push(emit("step.start", &payload));
}

fn append_text_delta(out: &mut Events, st: &StreamState, text: &str, thought: bool) {
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
    out.push(emit("step.delta", &payload));
}

fn append_arguments_delta(out: &mut Events, st: &StreamState, arguments: &str) {
    let mut payload = tmpl(r#"{"index":0,"delta":{"arguments":"","type":"arguments_delta"},"event_type":"step.delta"}"#);
    cpa_json::set(&mut payload, "index", st.active_step_index);
    cpa_json::set(&mut payload, "delta.arguments", arguments);
    out.push(emit("step.delta", &payload));
}

fn append_step_stop(out: &mut Events, st: &mut StreamState) {
    if !st.active_step_open {
        return;
    }
    let mut payload = tmpl(r#"{"index":0,"event_type":"step.stop"}"#);
    cpa_json::set(&mut payload, "index", st.active_step_index);
    out.push(emit("step.stop", &payload));
    st.active_step_open = false;
    st.active_step_type.clear();
}

fn append_completed(out: &mut Events, st: &mut StreamState, model_name: &str, response: &Res<'_>) {
    if st.completed {
        return;
    }
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut payload = tmpl(
        r#"{"interaction":{"id":"","status":"completed","usage":{},"created":"","updated":"","service_tier":"standard","object":"interaction","model":""},"event_type":"interaction.completed"}"#,
    );
    cpa_json::set(&mut payload, "interaction.id", st.id.as_str());
    cpa_json::set(&mut payload, "interaction.created", now.as_str());
    cpa_json::set(&mut payload, "interaction.updated", now);
    cpa_json::set(&mut payload, "interaction.model", response_model(model_name, &response.value()));
    let status = response.g("status").str();
    if !status.is_empty() {
        cpa_json::set(&mut payload, "interaction.status", status);
    }
    set_usage(&mut payload, "interaction.usage", &response.g("usage"));
    out.push(emit("interaction.completed", &payload));
    st.completed = true;
}

fn append_done(out: &mut Events, st: &mut StreamState) {
    if st.done {
        return;
    }
    out.push(common::sse_event_data("done", b"[DONE]"));
    st.done = true;
}

/// Opens a `function_call` step for the current event unless one is already open.
fn ensure_function_call_step(out: &mut Events, st: &mut StreamState, model_name: &str, root: &Value) {
    if st.active_step_open && st.active_step_type == "function_call" {
        return;
    }
    let mut item = root.g("item");
    if !item.exists() {
        item = Res::of(root);
    }
    let mut step = tmpl(r#"{"type":"function_call","name":"","arguments":{}}"#);
    let mut name = item.g("name").str();
    if is_antigravity_model(model_name) {
        name = common::antigravity_tool_name_to_upstream(&name);
    }
    cpa_json::set(&mut step, "name", name);
    let call_id = first_non_empty(&[
        &item.g("call_id").str(),
        &item.g("id").str(),
        &root.g("call_id").str(),
        &root.g("item_id").str(),
    ]);
    if !call_id.is_empty() {
        cpa_json::set(&mut step, "id", call_id.as_str());
        cpa_json::set(&mut step, "call_id", call_id);
    }
    ensure_created(out, st, model_name);
    append_step_stop(out, st);
    append_step_start(out, st, "function_call", &step);
}

/// Interactions usage from Responses usage under `path`.
fn set_usage(out: &mut Value, path: &str, usage: &Res<'_>) {
    if !usage.exists() {
        return;
    }
    let mappings: [(&str, &[&str]); 5] = [
        ("input_tokens", &["input_tokens", "total_input_tokens"]),
        ("output_tokens", &["output_tokens", "total_output_tokens"]),
        ("total_tokens", &["total_tokens"]),
        ("input_tokens_details.cached_tokens", &["cached_tokens", "total_cached_tokens"]),
        ("output_tokens_details.reasoning_tokens", &["reasoning_tokens", "total_thought_tokens"]),
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

// ---- dedupe keys

fn text_keys_from_event(root: &Value) -> Vec<String> {
    let item_id = root.g("item_id").str();
    let output_index = root.g("output_index");
    let has_output_index = output_index.exists();
    let output_index = output_index.int();
    let content_index = root.g("content_index");
    if !content_index.exists() {
        return unkeyed_text_keys(&item_id, output_index, has_output_index);
    }
    text_keys(&item_id, output_index, has_output_index, content_index.int(), true)
}

fn function_args_keys(root: &Value) -> Vec<String> {
    let item = root.g("item");
    let output_index = root.g("output_index");
    let mut keys: Vec<String> = Vec::with_capacity(5);
    for id in [root.g("item_id").str(), root.g("call_id").str(), item.g("call_id").str(), item.g("id").str()] {
        if id.is_empty() {
            continue;
        }
        let key = format!("item:{id}");
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    if output_index.exists() {
        keys.push(format!("output:{}", output_index.int()));
    }
    keys
}

fn text_keys(item_id: &str, output_index: i64, has_output_index: bool, content_index: i64, has_content_index: bool) -> Vec<String> {
    if !has_content_index {
        return vec![];
    }
    let mut keys = Vec::with_capacity(3);
    if !item_id.is_empty() {
        keys.push(format!("item:{item_id}:content:{content_index}"));
    }
    if has_output_index {
        keys.push(format!("output:{output_index}:content:{content_index}"));
    }
    keys.push(format!("content:{content_index}"));
    keys
}

fn unkeyed_text_keys(item_id: &str, output_index: i64, has_output_index: bool) -> Vec<String> {
    let mut keys = Vec::with_capacity(2);
    if !item_id.is_empty() {
        keys.push(format!("item:{item_id}"));
    }
    if has_output_index {
        keys.push(format!("output:{output_index}"));
    }
    keys
}
