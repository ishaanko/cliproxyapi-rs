//! Upstream Interactions response -> client OpenAI chat response
//! (Go: openai_interactions_response.go). Stream output elements are bare chat chunk JSON.

use std::collections::HashMap;

use cpa_json::{Res, Value, J};

use super::{first_non_empty, is_antigravity_model, json_string_value, set_items, sse_payload, tmpl, unix_nanos};
use crate::common;
use crate::registry::{Ctx, Param};

/// Per-stream state (Go: `interactionsToOpenAIChatStreamState`).
#[derive(Default)]
struct StreamState {
    id: String,
    model: String,
    environment_id: String,
    created: i64,
    started: bool,
    completed: bool,
    saw_tool_call: bool,
    tool_ids: HashMap<i64, String>,
    tool_names: HashMap<i64, String>,
    tool_call_index_by_step: HashMap<i64, i64>,
    next_tool_call_index: i64,
}

pub(super) fn convert_interactions_response_to_openai(
    _ctx: &Ctx,
    model_name: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let st = param.state(|| StreamState { model: model_name.to_string(), ..Default::default() });
    st.model = first_non_empty(&[&st.model, model_name]);
    convert_event(model_name, raw, st).into_iter().map(|c| cpa_json::to_vec(&c)).collect()
}

pub(super) fn convert_interactions_response_to_openai_non_stream(
    _ctx: &Ctx,
    model_name: &str,
    _original: &[u8],
    _translated: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let root = cpa_json::parse(raw);
    let nested = root.g("interaction");
    let interaction = if nested.exists() { nested } else { Res::of(&root) };
    let mut out = tmpl(
        r#"{"id":"","object":"chat.completion","created":0,"model":"","choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"stop"}]}"#,
    );
    let id = first_non_empty(&[&interaction.g("id").str(), &root.g("id").str(), &format!("chatcmpl_{}", unix_nanos())]);
    cpa_json::set(&mut out, "id", id);
    cpa_json::set(&mut out, "created", chrono::Utc::now().timestamp());
    let interaction_model = interaction.g("model").str();
    cpa_json::set(&mut out, "model", first_non_empty(&[&interaction_model, model_name]));
    let mut steps = interaction.g("steps");
    if !steps.exists() {
        steps = root.g("steps");
    }
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut saw_tool_call = false;
    let mut tool_calls: Vec<Value> = Vec::new();
    steps.for_each(|_, step| {
        match step.g("type").str().as_str() {
            "model_output" => content_texts(&step.g("content")).iter().for_each(|t| text.push_str(t)),
            "thought" => content_texts(&step.g("content")).iter().for_each(|t| reasoning.push_str(t)),
            "function_call" => {
                saw_tool_call = true;
                let for_antigravity = is_antigravity_model(&first_non_empty(&[&interaction_model, model_name]));
                tool_calls.push(tool_call_from_interactions(&step, &Res::NONE, for_antigravity));
            }
            _ => {}
        }
        true
    });
    if !text.is_empty() {
        cpa_json::set(&mut out, "choices.0.message.content", text);
    }
    if !reasoning.is_empty() {
        cpa_json::set(&mut out, "choices.0.message.reasoning_content", reasoning);
    }
    set_items(&mut out, "choices.0.message.tool_calls", tool_calls);
    if saw_tool_call {
        cpa_json::set(&mut out, "choices.0.message.content", Value::Null);
        cpa_json::set(&mut out, "choices.0.finish_reason", "tool_calls");
    }
    let status = first_non_empty(&[&interaction.g("status").str(), &root.g("status").str()]);
    let finish_reason = first_non_empty(&[&interaction.g("finish_reason").str(), &root.g("finish_reason").str()]);
    if finish_reason == "content_filter" {
        cpa_json::set(&mut out, "choices.0.finish_reason", "content_filter");
    } else if status == "incomplete" || finish_reason == "length" || finish_reason == "max_tokens" {
        cpa_json::set(&mut out, "choices.0.finish_reason", "length");
    }
    let env_id = first_non_empty(&[
        &interaction.g("environment_id").str(),
        &root.g("environment_id").str(),
        &interaction.g("environment.id").str(),
        &root.g("environment.id").str(),
        &root.g("interaction.environment_id").str(),
    ]);
    if !env_id.is_empty() {
        cpa_json::set(&mut out, "environment_id", env_id);
    }
    set_usage(&mut out, "usage", &common::interactions_usage(&Res::of(&root)));
    Some(cpa_json::to_vec(&out))
}

fn environment_id_of(root: &Value) -> String {
    let interaction = root.g("interaction");
    first_non_empty(&[
        &interaction.g("environment_id").str(),
        &root.g("environment_id").str(),
        &interaction.g("environment.id").str(),
        &root.g("environment.id").str(),
    ])
}

fn convert_event(model_name: &str, raw: &[u8], st: &mut StreamState) -> Vec<Value> {
    let payload = sse_payload(raw);
    if payload.is_empty() || payload.trim_ascii() == b"[DONE]" {
        return vec![];
    }
    let root = cpa_json::parse(&payload);
    match root.g("event_type").str().as_str() {
        "interaction.created" => {
            let interaction = root.g("interaction");
            st.id = first_non_empty(&[&interaction.g("id").str(), &st.id]);
            st.model = first_non_empty(&[&interaction.g("model").str(), &st.model, model_name]);
            let env_id = environment_id_of(&root);
            if !env_id.is_empty() {
                st.environment_id = env_id;
            }
            ensure_started(vec![], st)
        }
        "step.start" => step_start(model_name, &root, st),
        "step.delta" => step_delta(&root, st),
        "interaction.completed" | "finish" => {
            let env_id = environment_id_of(&root);
            if !env_id.is_empty() {
                st.environment_id = env_id;
            }
            append_completed(vec![], &root, st)
        }
        "response.failed" | "interaction.failed" => failed(&root),
        _ => vec![],
    }
}

fn step_start(model_name: &str, root: &Value, st: &mut StreamState) -> Vec<Value> {
    let mut out = ensure_started(vec![], st);
    let index = root.g("index").int();
    let step = root.g("step");
    if step.g("type").str() != "function_call" {
        return out;
    }
    st.saw_tool_call = true;
    let tool_call_index = match st.tool_call_index_by_step.get(&index) {
        Some(existing) => *existing,
        None => {
            let next = st.next_tool_call_index;
            st.tool_call_index_by_step.insert(index, next);
            st.next_tool_call_index += 1;
            next
        }
    };
    st.tool_ids.insert(
        index,
        first_non_empty(&[&step.g("call_id").str(), &step.g("id").str(), &format!("call_{tool_call_index}")]),
    );
    let mut name = step.g("name").str();
    if is_antigravity_model(model_name) || is_antigravity_model(&st.model) {
        name = common::antigravity_upstream_tool_name_to_client(&name);
    }
    st.tool_names.insert(index, name);
    out.push(tool_call_start_chunk(st, index));
    out
}

fn step_delta(root: &Value, st: &mut StreamState) -> Vec<Value> {
    let index = root.g("index").int();
    let delta = root.g("delta");
    let mut out = ensure_started(vec![], st);
    match delta.g("type").str().as_str() {
        "thought_summary" => {
            let text = first_non_empty(&[&delta.g("content.text").str(), &delta.g("text").str()]);
            if !text.is_empty() {
                out.push(delta_chunk(st, "reasoning_content", &text));
            }
        }
        "arguments_delta" => {
            let args = delta.g("arguments").str();
            out.push(tool_call_arguments_chunk(st, index, &args));
        }
        _ => {
            let text = delta.g("text").str();
            if !text.is_empty() {
                out.push(delta_chunk(st, "content", &text));
            }
        }
    }
    out
}

fn ensure_started(mut out: Vec<Value>, st: &mut StreamState) -> Vec<Value> {
    if st.started {
        return out;
    }
    let mut chunk = base_chunk(st);
    cpa_json::set(&mut chunk, "choices.0.delta.role", "assistant");
    st.started = true;
    out.push(chunk);
    out
}

fn append_completed(out: Vec<Value>, root: &Value, st: &mut StreamState) -> Vec<Value> {
    if st.completed {
        return out;
    }
    let mut out = ensure_started(out, st);
    let mut chunk = base_chunk(st);
    let mut finish_reason = if st.saw_tool_call { "tool_calls" } else { "stop" };
    let interaction = root.g("interaction");
    let status = first_non_empty(&[&interaction.g("status").str(), &root.g("status").str()]);
    let interaction_finish = first_non_empty(&[&interaction.g("finish_reason").str(), &root.g("finish_reason").str()]);
    if interaction_finish == "content_filter" {
        finish_reason = "content_filter";
    } else if status == "incomplete" || interaction_finish == "length" || interaction_finish == "max_tokens" {
        finish_reason = "length";
    }
    cpa_json::set(&mut chunk, "choices.0.finish_reason", finish_reason);
    set_usage(&mut chunk, "usage", &common::interactions_usage(&Res::of(root)));
    st.completed = true;
    out.push(chunk);
    out
}

fn failed(root: &Value) -> Vec<Value> {
    let mut err_node = root.g("error");
    if !err_node.exists() {
        err_node = root.g("interaction.error");
    }
    let mut msg = err_node.g("message").str();
    if msg.is_empty() {
        msg = "upstream error occurred".to_string();
    }
    let code = err_node.g("code").str();
    let mut err_type = err_node.g("type").str();
    if err_type.is_empty() {
        err_type = "server_error".to_string();
    }
    let mut error_json = tmpl(r#"{"error":{"message":"","type":"","code":""}}"#);
    cpa_json::set(&mut error_json, "error.message", msg);
    cpa_json::set(&mut error_json, "error.type", err_type);
    if code.is_empty() {
        cpa_json::delete(&mut error_json, "error.code");
    } else {
        cpa_json::set(&mut error_json, "error.code", code);
    }
    vec![error_json]
}

fn base_chunk(st: &mut StreamState) -> Value {
    let mut chunk = tmpl(
        r#"{"id":"","object":"chat.completion.chunk","created":0,"model":"","choices":[{"index":0,"delta":{},"finish_reason":null}]}"#,
    );
    cpa_json::set(&mut chunk, "id", first_non_empty(&[&st.id, &format!("chatcmpl_{}", unix_nanos())]));
    if st.created == 0 {
        st.created = chrono::Utc::now().timestamp();
    }
    cpa_json::set(&mut chunk, "created", st.created);
    cpa_json::set(&mut chunk, "model", st.model.as_str());
    if !st.environment_id.is_empty() {
        cpa_json::set(&mut chunk, "environment_id", st.environment_id.as_str());
    }
    chunk
}

fn delta_chunk(st: &mut StreamState, field: &str, value: &str) -> Value {
    let mut chunk = base_chunk(st);
    cpa_json::set(&mut chunk, &format!("choices.0.delta.{field}"), value);
    chunk
}

/// The chat tool-call index for an Interactions step index.
fn tool_call_index(st: &StreamState, index: i64) -> i64 {
    st.tool_call_index_by_step.get(&index).copied().unwrap_or(index)
}

fn tool_call_start_chunk(st: &mut StreamState, index: i64) -> Value {
    let mut chunk = base_chunk(st);
    let tool_call_index = tool_call_index(st, index);
    let mut tool_call = tmpl(r#"{"index":0,"id":"","type":"function","function":{"name":"","arguments":""}}"#);
    cpa_json::set(&mut tool_call, "index", tool_call_index);
    let id = st.tool_ids.get(&index).cloned().unwrap_or_default();
    cpa_json::set(&mut tool_call, "id", first_non_empty(&[&id, &format!("call_{tool_call_index}")]));
    cpa_json::set(&mut tool_call, "function.name", st.tool_names.get(&index).cloned().unwrap_or_default());
    cpa_json::set(&mut chunk, "choices.0.delta.tool_calls.-1", tool_call);
    chunk
}

fn tool_call_arguments_chunk(st: &mut StreamState, index: i64, arguments: &str) -> Value {
    let mut chunk = base_chunk(st);
    let mut tool_call = tmpl(r#"{"index":0,"function":{"arguments":""}}"#);
    cpa_json::set(&mut tool_call, "index", tool_call_index(st, index));
    cpa_json::set(&mut tool_call, "function.arguments", arguments);
    cpa_json::set(&mut chunk, "choices.0.delta.tool_calls.-1", tool_call);
    chunk
}

/// An OpenAI `tool_calls` entry from a `function_call` step; `fallback_args` is used when the step
/// has no `arguments`.
pub(super) fn tool_call_from_interactions(step: &Res<'_>, fallback_args: &Res<'_>, for_antigravity: bool) -> Value {
    let mut tool_call = tmpl(r#"{"id":"","type":"function","function":{"name":"","arguments":"{}"}}"#);
    let call_id = first_non_empty(&[&step.g("call_id").str(), &step.g("id").str(), "call_0"]);
    cpa_json::set(&mut tool_call, "id", call_id);
    let mut name = step.g("name").str();
    if for_antigravity {
        name = common::antigravity_upstream_tool_name_to_client(&name);
    }
    cpa_json::set(&mut tool_call, "function.name", name);
    let mut args = step.g("arguments");
    if !args.exists() {
        args = fallback_args.clone();
    }
    cpa_json::set(&mut tool_call, "function.arguments", json_string_value(&args, "{}"));
    tool_call
}

/// Maps an Interactions usage object onto OpenAI chat usage fields under `path`.
fn set_usage(out: &mut Value, path: &str, usage: &Res<'_>) {
    if !usage.exists() {
        return;
    }
    let mappings: [(&[&str], &str); 5] = [
        (&["input_tokens", "total_input_tokens"], "prompt_tokens"),
        (&["output_tokens", "total_output_tokens"], "completion_tokens"),
        (&["total_tokens"], "total_tokens"),
        (&["cached_tokens", "total_cached_tokens"], "prompt_tokens_details.cached_tokens"),
        (&["reasoning_tokens", "total_thought_tokens"], "completion_tokens_details.reasoning_tokens"),
    ];
    for (sources, target) in mappings {
        if let Some(value) = sources.iter().map(|p| usage.g(p)).find(Res::exists) {
            cpa_json::set(out, &format!("{path}.{target}"), value.int());
        }
    }
}

/// Text parts of Interactions step content: the string itself, or each part's `text` /
/// `content.text`.
fn content_texts(content: &Res<'_>) -> Vec<String> {
    if !content.exists() {
        return vec![];
    }
    if let Some(s) = content.as_str() {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    content.for_each(|_, part| {
        let text = first_non_empty(&[&part.g("text").str(), &part.g("content.text").str()]);
        if !text.is_empty() {
            out.push(text);
        }
        true
    });
    out
}
