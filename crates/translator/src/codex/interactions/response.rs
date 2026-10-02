//! Codex Responses -> Interactions response (Go: interactions_codex_response.go).

use cpa_json::{json, Res, Value, J};

use crate::codex::util::{mime_type_from_output_format, rfc3339_utc, unix_nano_now, unix_now, values};
use crate::common::sse_event_data;
use crate::registry::{Ctx, Param};

/// Per-stream state (Go: codexToInteractionsStreamState).
struct State {
    started: bool,
    completed: bool,
    done: bool,
    active_step_open: bool,
    active_step_type: String,
    active_step_index: i64,
    step_index: i64,
    id: String,
    model: String,
    created_at: i64,
    has_output_text: bool,
    function_call_name: String,
    function_call_id: String,
}

type Out = Vec<Vec<u8>>;

/// Go: ConvertCodexResponseToInteractions. One SSE line in, Interactions SSE frames out.
pub fn convert_codex_response_to_interactions(
    _ctx: &Ctx,
    model_name: &str,
    _original_request: &[u8],
    _request: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let st = param.state(|| State {
        started: false,
        completed: false,
        done: false,
        active_step_open: false,
        active_step_type: String::new(),
        active_step_index: 0,
        step_index: 0,
        id: format!("interaction_{}", unix_nano_now()),
        model: model_name.to_string(),
        created_at: 0,
        has_output_text: false,
        function_call_name: String::new(),
        function_call_id: String::new(),
    });
    let payload = stream_payload(raw);
    if payload == b"[DONE]" {
        let mut out = Vec::new();
        append_step_stop(&mut out, st);
        if !st.completed {
            append_completed(&mut out, st, &Res::NONE);
        }
        append_done(&mut out, st);
        return out;
    }
    if payload.is_empty() {
        return vec![];
    }
    let root = cpa_json::parse(payload);
    match root.g("type").str().as_str() {
        "response.created" => {
            let mut out = Vec::new();
            append_created(&mut out, st, &root.g("response"));
            out
        }
        "response.output_item.added" => output_item_added(st, &root),
        "response.output_text.delta" => output_text_delta(st, &root),
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => reasoning_delta(st, &root),
        "response.function_call_arguments.delta" => function_arguments_delta(st, &root),
        "response.output_item.done" => output_item_done(st, &root.g("item")),
        "response.completed" | "response.incomplete" => {
            let mut out = Vec::new();
            append_created(&mut out, st, &root.g("response"));
            append_step_stop(&mut out, st);
            append_completed(&mut out, st, &root.g("response"));
            append_done(&mut out, st);
            out
        }
        _ => vec![],
    }
}

/// Go: ConvertCodexResponseToInteractionsNonStream.
pub fn convert_codex_response_to_interactions_non_stream(
    _ctx: &Ctx,
    model_name: &str,
    _original_request: &[u8],
    _request: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let root = cpa_json::parse(raw);
    let mut response = root.g("response");
    if !response.exists() {
        response = Res::of(&root);
    }
    let mut out = cpa_json::parse_str(r#"{"id":"","object":"interaction","status":"completed","model":"","steps":[]}"#);
    let status = response.g("status").str();
    if !status.is_empty() {
        cpa_json::set(&mut out, "status", status);
    }
    let mut id = response.g("id").str();
    if id.is_empty() {
        id = format!("interaction_{}", unix_nano_now());
    }
    cpa_json::set(&mut out, "id", id);
    let model = response.g("model").str();
    cpa_json::set(&mut out, "model", if model.is_empty() { model_name.to_string() } else { model });
    let mut steps: Vec<Value> = Vec::new();
    let output_items = response.g("output");
    for item in values(&output_items) {
        let step = match item.g("type").str().as_str() {
            "message" => build_message_item(&item),
            "reasoning" => build_reasoning_item(&item),
            "function_call" | "tool_call" => build_function_call_item(&item),
            "image_generation_call" => build_image_item(&item),
            _ => None,
        };
        steps.extend(step);
    }
    if !steps.is_empty() {
        cpa_json::set(&mut out, "steps", Value::Array(steps));
    }
    set_usage(&mut out, "usage", &response.g("usage"), false);
    Some(cpa_json::to_vec(&out))
}

/// Go `bytes.TrimSpace` plus an optional `data:` prefix and a second trim.
fn stream_payload(raw: &[u8]) -> &[u8] {
    let raw = raw.trim_ascii();
    match raw.strip_prefix(b"data:") {
        Some(rest) => rest.trim_ascii(),
        None => raw,
    }
}

fn append_created(out: &mut Out, st: &mut State, response: &Res<'_>) {
    if st.started {
        return;
    }
    let id = response.g("id").str();
    if !id.is_empty() {
        st.id = id;
    }
    let model = response.g("model").str();
    if !model.is_empty() {
        st.model = model;
    }
    let created_at = response.g("created_at");
    if created_at.exists() {
        st.created_at = created_at.int();
    }
    let created = json!({"interaction": {"id": st.id, "status": "in_progress", "object": "interaction", "model": st.model}, "event_type": "interaction.created"});
    out.push(sse_event_data("interaction.created", &cpa_json::to_vec(&created)));
    let status_update = json!({"interaction_id": st.id, "status": "in_progress", "event_type": "interaction.status_update"});
    out.push(sse_event_data("interaction.status_update", &cpa_json::to_vec(&status_update)));
    st.started = true;
}

fn append_completed(out: &mut Out, st: &mut State, response: &Res<'_>) {
    if st.completed {
        return;
    }
    let created = if st.created_at > 0 { st.created_at } else { unix_now() };
    let mut completed = cpa_json::parse_str(
        r#"{"interaction":{"id":"","status":"completed","usage":{},"created":"","updated":"","service_tier":"standard","object":"interaction","model":""},"event_type":"interaction.completed"}"#,
    );
    cpa_json::set(&mut completed, "interaction.id", st.id.clone());
    cpa_json::set(&mut completed, "interaction.created", rfc3339_utc(created));
    cpa_json::set(&mut completed, "interaction.updated", rfc3339_utc(unix_now()));
    cpa_json::set(&mut completed, "interaction.model", st.model.clone());
    let status = response.g("status").str();
    if !status.is_empty() {
        cpa_json::set(&mut completed, "interaction.status", status);
    }
    set_usage(&mut completed, "interaction.usage", &response.g("usage"), true);
    out.push(sse_event_data("interaction.completed", &cpa_json::to_vec(&completed)));
    st.completed = true;
}

fn append_done(out: &mut Out, st: &mut State) {
    if st.done {
        return;
    }
    out.push(sse_event_data("done", b"[DONE]"));
    st.done = true;
}

fn output_item_added(st: &mut State, root: &Value) -> Out {
    let mut out = Vec::new();
    append_created(&mut out, st, &root.g("response"));
    let item = root.g("item");
    match item.g("type").str().as_str() {
        "message" => ensure_step(&mut out, st, "model_output", &item),
        "reasoning" => ensure_step(&mut out, st, "thought", &item),
        "function_call" | "tool_call" => {
            st.function_call_name = item.g("name").str();
            st.function_call_id = item_call_id(&item);
            ensure_step(&mut out, st, "function_call", &item);
        }
        _ => {}
    }
    out
}

fn output_text_delta(st: &mut State, root: &Value) -> Out {
    let mut out = Vec::new();
    append_created(&mut out, st, &root.g("response"));
    ensure_step(&mut out, st, "model_output", &Res::NONE);
    let delta = json!({"index": st.active_step_index, "delta": {"text": root.g("delta").str(), "type": "text"}, "event_type": "step.delta"});
    st.has_output_text = true;
    out.push(sse_event_data("step.delta", &cpa_json::to_vec(&delta)));
    out
}

fn reasoning_delta(st: &mut State, root: &Value) -> Out {
    let mut out = Vec::new();
    append_created(&mut out, st, &root.g("response"));
    ensure_step(&mut out, st, "thought", &Res::NONE);
    out.push(thought_delta(st, &root.g("delta").str()));
    out
}

fn thought_delta(st: &State, text: &str) -> Vec<u8> {
    let delta = json!({"index": st.active_step_index, "delta": {"content": {"text": text, "type": "text"}, "type": "thought_summary"}, "event_type": "step.delta"});
    sse_event_data("step.delta", &cpa_json::to_vec(&delta))
}

fn function_arguments_delta(st: &mut State, root: &Value) -> Out {
    let mut out = Vec::new();
    append_created(&mut out, st, &root.g("response"));
    ensure_step(&mut out, st, "function_call", &root.g("item"));
    out.push(arguments_delta(st, &root.g("delta").str()));
    out
}

fn arguments_delta(st: &State, arguments: &str) -> Vec<u8> {
    let delta = json!({"index": st.active_step_index, "delta": {"arguments": arguments, "type": "arguments_delta"}, "event_type": "step.delta"});
    sse_event_data("step.delta", &cpa_json::to_vec(&delta))
}

fn output_item_done(st: &mut State, item: &Res<'_>) -> Out {
    let mut out = Vec::new();
    append_created(&mut out, st, &Res::NONE);
    match item.g("type").str().as_str() {
        "message" => {
            if !st.has_output_text {
                append_message_item_stream(&mut out, st, item);
            }
            append_step_stop(&mut out, st);
        }
        "reasoning" => {
            let text = reasoning_text(item);
            if !text.is_empty() {
                ensure_step(&mut out, st, "thought", item);
                out.push(thought_delta(st, &text));
            }
            append_step_stop(&mut out, st);
        }
        "function_call" | "tool_call" => {
            ensure_step(&mut out, st, "function_call", item);
            out.push(arguments_delta(st, &item.g("arguments").str()));
            append_step_stop(&mut out, st);
        }
        "image_generation_call" => {
            let result = item.g("result").str();
            if !result.is_empty() {
                ensure_step(&mut out, st, "model_output", item);
                let mime = mime_type_from_output_format(&item.g("output_format").str());
                let delta = json!({"index": st.active_step_index, "delta": {"content": {"type": "image", "mime_type": mime, "data": result}, "type": "content"}, "event_type": "step.delta"});
                out.push(sse_event_data("step.delta", &cpa_json::to_vec(&delta)));
            }
            append_step_stop(&mut out, st);
        }
        _ => {}
    }
    out
}

fn ensure_step(out: &mut Out, st: &mut State, step_type: &str, item: &Res<'_>) {
    if st.active_step_open && st.active_step_type == step_type {
        return;
    }
    append_step_stop(out, st);
    append_step_start(out, st, step_type, item);
}

fn append_step_start(out: &mut Out, st: &mut State, step_type: &str, item: &Res<'_>) {
    st.active_step_index = st.step_index;
    st.step_index += 1;
    st.active_step_open = true;
    st.active_step_type = step_type.to_string();
    let mut step_start = json!({"index": st.active_step_index, "step": {"type": step_type}, "event_type": "step.start"});
    if step_type == "function_call" {
        let mut name = item.g("name").str();
        if name.is_empty() {
            name = st.function_call_name.clone();
        }
        let mut call_id = item_call_id(item);
        if call_id.is_empty() {
            call_id = st.function_call_id.clone();
        }
        if call_id.is_empty() {
            call_id = format!("step_{}", unix_nano_now());
        }
        cpa_json::set(&mut step_start, "step.id", call_id.clone());
        cpa_json::set(&mut step_start, "step.call_id", call_id);
        cpa_json::set(&mut step_start, "step.name", name);
        cpa_json::set(&mut step_start, "step.arguments", json!({}));
    }
    out.push(sse_event_data("step.start", &cpa_json::to_vec(&step_start)));
}

fn append_step_stop(out: &mut Out, st: &mut State) {
    if !st.active_step_open {
        return;
    }
    let step_stop = json!({"index": st.active_step_index, "event_type": "step.stop"});
    out.push(sse_event_data("step.stop", &cpa_json::to_vec(&step_stop)));
    st.active_step_open = false;
    st.active_step_type.clear();
}

fn build_message_item(item: &Res<'_>) -> Option<Value> {
    let content_items = item.g("content");
    let contents: Vec<Value> = values(&content_items)
        .iter()
        .filter_map(|c| {
            let text = content_text(c);
            (!text.is_empty()).then(|| json!({"type": "text", "text": text}))
        })
        .collect();
    if contents.is_empty() {
        return None;
    }
    Some(json!({"type": "model_output", "content": contents}))
}

fn build_reasoning_item(item: &Res<'_>) -> Option<Value> {
    let text = reasoning_text(item);
    if text.is_empty() {
        return None;
    }
    Some(json!({"type": "thought", "content": [{"type": "text", "text": text}]}))
}

fn build_function_call_item(item: &Res<'_>) -> Option<Value> {
    let mut step = json!({"type": "function_call", "name": "", "arguments": {}});
    cpa_json::set(&mut step, "name", item.g("name").str());
    let call_id = item_call_id(item);
    if !call_id.is_empty() {
        cpa_json::set(&mut step, "call_id", call_id);
    }
    if let Some(args) = arguments_json(&item.g("arguments")) {
        cpa_json::set(&mut step, "arguments", args);
    }
    Some(step)
}

fn build_image_item(item: &Res<'_>) -> Option<Value> {
    let result = item.g("result").str();
    if result.is_empty() {
        return None;
    }
    let mime = mime_type_from_output_format(&item.g("output_format").str());
    Some(json!({"type": "model_output", "content": [{"type": "image", "mime_type": mime, "data": result}]}))
}

fn append_message_item_stream(out: &mut Out, st: &mut State, item: &Res<'_>) {
    let content_items = item.g("content");
    for content in values(&content_items) {
        let text = content_text(&content);
        if !text.is_empty() {
            ensure_step(out, st, "model_output", item);
            let delta = json!({"index": st.active_step_index, "delta": {"text": text, "type": "text"}, "event_type": "step.delta"});
            out.push(sse_event_data("step.delta", &cpa_json::to_vec(&delta)));
        }
    }
}

/// First string at `text` or `content`.
fn content_text(content: &Res<'_>) -> String {
    for path in ["text", "content"] {
        let value = content.g(path);
        if value.exists() && value.is_string() {
            return value.str();
        }
    }
    String::new()
}

fn reasoning_text(item: &Res<'_>) -> String {
    let content = item.g("content");
    if content.exists() {
        if content.is_string() {
            return content.str();
        }
        if content.is_array() {
            let mut parts: Vec<String> = Vec::new();
            for part in content.array() {
                let mut text = content_text(&part);
                if text.is_empty() {
                    text = part.g("summary_text").str();
                }
                if !text.is_empty() {
                    parts.push(text);
                }
            }
            return parts.join("\n");
        }
    }
    let summary = item.g("summary");
    if summary.exists() {
        if summary.is_string() {
            return summary.str();
        }
        if summary.is_array() {
            let parts: Vec<String> = summary.array().iter().map(content_text).filter(|t| !t.is_empty()).collect();
            return parts.join("\n");
        }
    }
    String::new()
}

fn item_call_id(item: &Res<'_>) -> String {
    let call_id = item.g("call_id").str().trim().to_string();
    if !call_id.is_empty() {
        return call_id;
    }
    item.g("id").str().trim().to_string()
}

/// Arguments as a JSON object: strings must parse to an object (else `{}`), objects pass through.
fn arguments_json(arguments: &Res<'_>) -> Option<Value> {
    if !arguments.exists() {
        return None;
    }
    if let Some(s) = arguments.as_str() {
        let parsed = cpa_json::parse_str(s);
        if parsed.is_object() {
            return Some(parsed);
        }
        return Some(json!({}));
    }
    if arguments.is_object() {
        return Some(arguments.value());
    }
    None
}

fn set_usage(out: &mut Value, path: &str, usage: &Res<'_>, stream: bool) {
    if !usage.exists() {
        return;
    }
    let mut input = usage.g("input_tokens").int();
    let mut output = usage.g("output_tokens").int();
    if input == 0 {
        input = usage.g("prompt_tokens").int();
    }
    if output == 0 {
        output = usage.g("completion_tokens").int();
    }
    let mut total = usage.g("total_tokens").int();
    if total == 0 {
        total = input + output;
    }
    let mut reasoning = usage.g("output_tokens_details.reasoning_tokens").int();
    if reasoning == 0 {
        reasoning = usage.g("reasoning_tokens").int();
    }
    let mut cached = usage.g("input_tokens_details.cached_tokens").int();
    if cached == 0 {
        cached = usage.g("cached_tokens").int();
    }
    if stream {
        cpa_json::set(out, &format!("{path}.total_tokens"), total);
        cpa_json::set(out, &format!("{path}.total_input_tokens"), input);
        cpa_json::set(out, &format!("{path}.input_tokens_by_modality"), json!([{"modality": "text", "tokens": input}]));
        cpa_json::set(out, &format!("{path}.total_cached_tokens"), cached);
        cpa_json::set(out, &format!("{path}.total_output_tokens"), output);
        cpa_json::set(out, &format!("{path}.total_tool_use_tokens"), 0);
        cpa_json::set(out, &format!("{path}.total_thought_tokens"), reasoning);
        return;
    }
    cpa_json::set(out, &format!("{path}.input_tokens"), input);
    cpa_json::set(out, &format!("{path}.output_tokens"), output);
    cpa_json::set(out, &format!("{path}.total_tokens"), total);
    if reasoning > 0 {
        cpa_json::set(out, &format!("{path}.reasoning_tokens"), reasoning);
    }
    if cached > 0 {
        cpa_json::set(out, &format!("{path}.cached_tokens"), cached);
    }
}
