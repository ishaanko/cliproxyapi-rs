//! Antigravity response -> Interactions response (Go: interactions_antigravity_response.go).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use cpa_core::util;
use cpa_json::{json, J, Res, Value};

use crate::common;
use crate::registry::{Ctx, Param};

/// Per-stream conversion state.
#[derive(Default)]
struct StreamState {
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
    tool_name_map: HashMap<String, String>,
}

fn unix_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

/// Go: `ConvertAntigravityResponseToInteractions`. One raw line yields zero or more SSE frames.
pub fn convert_antigravity_response_to_interactions(
    _ctx: &Ctx,
    model: &str,
    original_request_raw_json: &[u8],
    _request_raw_json: &[u8],
    raw_json: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let st = param.state(|| StreamState {
        id: format!("interaction_{}", unix_nanos()),
        tool_name_map: util::disambiguated_tool_name_map(original_request_raw_json),
        ..Default::default()
    });
    let mut out: Vec<Vec<u8>> = Vec::new();
    for payload in stream_payloads(raw_json) {
        if payload.trim_ascii() == b"[DONE]" {
            if !st.completed {
                append_step_stop(&mut out, st);
                append_completed(&mut out, st, model, None);
            }
            append_done(&mut out, st);
            continue;
        }
        let parsed = cpa_json::parse(&payload);
        if parsed.is_null() && payload.trim_ascii() != b"null" {
            continue;
        }
        let raw_prefix = if parsed.g("response").exists() { "response." } else { "" };
        let root = restore_function_names(unwrap_response(parsed), &st.tool_name_map);
        if !st.started {
            append_created(&mut out, st, model);
            append_status_update(&mut out, st);
            st.started = true;
        }
        let part_raws = cpa_json::raw_children(&payload, &format!("{raw_prefix}candidates.0.content.parts"));
        root.g("candidates.0.content.parts").for_each(|index, part| {
            // Go copies `args.Raw` into the delta: keep the upstream text as sent.
            let raw_args = crate::common::raw_in(usize::try_from(index.int()).ok().and_then(|i| part_raws.get(i)), "functionCall.args");
            append_part_to_stream(&mut out, st, &part, raw_args);
            true
        });
        let has_finish = root.g("candidates.0.finishReason").exists();
        let has_usage = has_stream_usage(&root);
        if has_finish && !st.finished {
            append_step_stop(&mut out, st);
            st.finished = true;
        }
        if has_usage && st.finished && !st.completed {
            append_completed(&mut out, st, model, Some(&root));
        }
    }
    out
}

/// Go: `ConvertAntigravityResponseToInteractionsNonStream`.
pub fn convert_antigravity_response_to_interactions_non_stream(
    _ctx: &Ctx,
    model: &str,
    original_request_raw_json: &[u8],
    _request_raw_json: &[u8],
    raw_json: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let root = restore_function_names(
        unwrap_response(cpa_json::parse(raw_json)),
        &util::disambiguated_tool_name_map(original_request_raw_json),
    );
    let mut out = json!({"id": "", "object": "interaction", "status": "completed", "model": "", "steps": []});
    let mut id = root.g("responseId").str();
    if id.is_empty() {
        id = format!("interaction_{}", unix_nanos());
    }
    cpa_json::set(&mut out, "id", id);
    cpa_json::set(&mut out, "model", model);
    let mut steps: Vec<Value> = Vec::new();
    root.g("candidates.0.content.parts").for_each(|_, part| {
        steps.extend(part_to_steps(&part));
        true
    });
    if !steps.is_empty() {
        cpa_json::set(&mut out, "steps", Value::Array(steps));
    }
    set_usage(&mut out, "usage", &root);
    Some(cpa_json::to_vec(&out))
}

/// The payloads in one raw upstream line: a `data:` line is one payload, a JSON array yields each
/// item's `response` (or the item itself), anything else is a single payload.
fn stream_payloads(raw_json: &[u8]) -> Vec<Vec<u8>> {
    let trimmed = raw_json.trim_ascii();
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        return vec![rest.trim_ascii().to_vec()];
    }
    // Go quirk: gjson parses a bare `[DONE]` line as an array holding one garbage number, so the
    // line becomes a single unparseable payload and never reaches the `[DONE]` branch. Only a
    // `data: [DONE]` line does.
    if trimmed.starts_with(b"[") && !cpa_json::valid(trimmed) {
        return vec![vec![]];
    }
    let root = cpa_json::parse(trimmed);
    if let Value::Array(items) = &root {
        let payloads: Vec<Vec<u8>> = items
            .iter()
            .map(|item| match item.g("response").v() {
                Some(response) => cpa_json::to_vec(response),
                None => cpa_json::to_vec(item),
            })
            .collect();
        if !payloads.is_empty() {
            return payloads;
        }
    }
    vec![trimmed.to_vec()]
}

/// The `response` object when present, else the root, with `cpaUsageMetadata` restored.
fn unwrap_response(root: Value) -> Value {
    let response = root.g("response");
    if response.exists() {
        let response = response.value();
        return restore_usage_metadata(response);
    }
    restore_usage_metadata(root)
}

fn restore_function_names(mut root: Value, name_map: &HashMap<String, String>) -> Value {
    if name_map.is_empty() {
        return root;
    }
    let candidates = root.g("candidates").array().len();
    for candidate_index in 0..candidates {
        let parts = root.g(&format!("candidates.{candidate_index}.content.parts")).array().len();
        for part_index in 0..parts {
            for field in ["functionCall", "functionResponse"] {
                let path = format!("candidates.{candidate_index}.content.parts.{part_index}.{field}.name");
                let name_result = root.g(&path);
                let name = name_result.str();
                if name.is_empty() {
                    continue;
                }
                let restored = util::restore_sanitized_tool_name(name_map, &name);
                if name_result.is_string() && restored == name {
                    continue;
                }
                cpa_json::set(&mut root, &path, restored);
            }
        }
    }
    root
}

fn restore_usage_metadata(mut root: Value) -> Value {
    if !root.g("usageMetadata").exists() {
        let cpa_usage = root.g("cpaUsageMetadata");
        if cpa_usage.exists() {
            let v = cpa_usage.value();
            cpa_json::set(&mut root, "usageMetadata", v);
            cpa_json::delete(&mut root, "cpaUsageMetadata");
        }
    }
    root
}

fn sse(out: &mut Vec<Vec<u8>>, event: &str, payload: &Value) {
    out.push(common::sse_event_data(event, &cpa_json::to_vec(payload)));
}

fn append_created(out: &mut Vec<Vec<u8>>, st: &StreamState, model: &str) {
    let mut created = json!({"interaction": {"id": "", "status": "in_progress", "object": "interaction", "model": ""}, "event_type": "interaction.created"});
    cpa_json::set(&mut created, "interaction.id", st.id.clone());
    cpa_json::set(&mut created, "interaction.model", model);
    sse(out, "interaction.created", &created);
}

fn append_status_update(out: &mut Vec<Vec<u8>>, st: &StreamState) {
    let mut update = json!({"interaction_id": "", "status": "in_progress", "event_type": "interaction.status_update"});
    cpa_json::set(&mut update, "interaction_id", st.id.clone());
    sse(out, "interaction.status_update", &update);
}

fn append_completed(out: &mut Vec<Vec<u8>>, st: &mut StreamState, model: &str, root: Option<&Value>) {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let mut completed = json!({
        "interaction": {
            "id": "", "status": "completed", "usage": {}, "created": "", "updated": "",
            "service_tier": "standard", "object": "interaction", "model": ""
        },
        "event_type": "interaction.completed"
    });
    cpa_json::set(&mut completed, "interaction.id", st.id.clone());
    cpa_json::set(&mut completed, "interaction.created", now.clone());
    cpa_json::set(&mut completed, "interaction.updated", now);
    cpa_json::set(&mut completed, "interaction.model", model);
    if let Some(root) = root {
        set_stream_usage(&mut completed, "interaction.usage", root);
    }
    sse(out, "interaction.completed", &completed);
    st.completed = true;
}

fn append_done(out: &mut Vec<Vec<u8>>, st: &mut StreamState) {
    if st.done {
        return;
    }
    out.push(common::sse_event_data("done", b"[DONE]"));
    st.done = true;
}

fn append_step_start(out: &mut Vec<Vec<u8>>, st: &mut StreamState, step_type: &str, part: &Res<'_>) {
    st.step_id = format!("step_{}", unix_nanos());
    st.active_step_index = st.step_index;
    st.step_index += 1;
    st.active_step_type = step_type.to_string();
    st.active_step_open = true;
    let mut start = json!({"index": 0, "step": {"type": ""}, "event_type": "step.start"});
    cpa_json::set(&mut start, "index", st.active_step_index);
    cpa_json::set(&mut start, "step.type", step_type);
    if step_type == "function_call" {
        let mut id = function_part_id(part);
        if id.is_empty() {
            id = st.step_id.clone();
        }
        cpa_json::set(&mut start, "step.id", id.clone());
        cpa_json::set(&mut start, "step.call_id", id);
        cpa_json::set(&mut start, "step.name", part.g("name").str());
        cpa_json::set(&mut start, "step.arguments", json!({}));
    }
    sse(out, "step.start", &start);
}

fn append_step_stop(out: &mut Vec<Vec<u8>>, st: &mut StreamState) {
    if !st.active_step_open {
        return;
    }
    let mut stop = json!({"index": 0, "event_type": "step.stop"});
    cpa_json::set(&mut stop, "index", st.active_step_index);
    sse(out, "step.stop", &stop);
    st.active_step_open = false;
    st.active_step_type.clear();
}

fn ensure_step(out: &mut Vec<Vec<u8>>, st: &mut StreamState, step_type: &str, part: &Res<'_>) {
    if st.active_step_open && st.active_step_type == step_type {
        return;
    }
    append_step_stop(out, st);
    append_step_start(out, st, step_type, part);
}

fn append_part_to_stream(out: &mut Vec<Vec<u8>>, st: &mut StreamState, part: &Res<'_>, raw_args: Option<&str>) {
    let text = part.g("text");
    if text.exists() && !text.str().is_empty() {
        if part.g("thought").bool() {
            ensure_step(out, st, "thought", &Res::NONE);
            let mut delta = json!({"index": 0, "delta": {"content": {"text": "", "type": "text"}, "type": "thought_summary"}, "event_type": "step.delta"});
            cpa_json::set(&mut delta, "index", st.active_step_index);
            cpa_json::set(&mut delta, "delta.content.text", text.str());
            sse(out, "step.delta", &delta);
            return append_thought_signature(out, st, part);
        }
        ensure_step(out, st, "model_output", &Res::NONE);
        let mut delta = json!({"index": 0, "delta": {"text": "", "type": "text"}, "event_type": "step.delta"});
        cpa_json::set(&mut delta, "index", st.active_step_index);
        cpa_json::set(&mut delta, "delta.text", text.str());
        sse(out, "step.delta", &delta);
        return append_thought_signature(out, st, part);
    }
    let fc = part.g("functionCall");
    if fc.exists() {
        append_thought_signature(out, st, part);
        ensure_step(out, st, "function_call", &fc);
        let mut delta = json!({"index": 0, "delta": {"arguments": "", "type": "arguments_delta"}, "event_type": "step.delta"});
        cpa_json::set(&mut delta, "index", st.active_step_index);
        let args = fc.g("args");
        let arguments = if args.exists() { raw_args.map_or_else(|| args.raw(), str::to_string) } else { "{}".to_string() };
        cpa_json::set(&mut delta, "delta.arguments", arguments);
        sse(out, "step.delta", &delta);
        return append_step_stop(out, st);
    }
    let fr = part.g("functionResponse");
    if fr.exists() {
        ensure_step(out, st, "function_result", &fr);
        let mut delta = json!({"index": 0, "delta": {"type": "function_result", "name": "", "result": {}}, "event_type": "step.delta"});
        cpa_json::set(&mut delta, "index", st.active_step_index);
        cpa_json::set(&mut delta, "delta.name", fr.g("name").str());
        let response = fr.g("response");
        if response.exists() {
            cpa_json::set(&mut delta, "delta.result", response.value());
        }
        sse(out, "step.delta", &delta);
        return append_step_stop(out, st);
    }
    if !thought_signature(part).is_empty() {
        append_thought_signature(out, st, part);
    }
}

fn append_thought_signature(out: &mut Vec<Vec<u8>>, st: &mut StreamState, part: &Res<'_>) {
    let signature = thought_signature(part);
    if signature.is_empty() {
        return;
    }
    ensure_step(out, st, "thought", &Res::NONE);
    let mut delta = json!({"index": 0, "delta": {"signature": "", "type": "thought_signature"}, "event_type": "step.delta"});
    cpa_json::set(&mut delta, "index", st.active_step_index);
    cpa_json::set(&mut delta, "delta.signature", signature);
    sse(out, "step.delta", &delta);
}

/// Non-stream steps for one part (function calls and text are preceded/followed by a thought step
/// when the part carries a signature).
fn part_to_steps(part: &Res<'_>) -> Vec<Value> {
    let sig = thought_signature(part);
    let fc = part.g("functionCall");
    if fc.exists() {
        let mut steps = Vec::new();
        if !sig.is_empty() {
            steps.push(thought_step(&sig, ""));
        }
        let mut step = json!({"type": "function_call", "name": "", "arguments": {}});
        cpa_json::set(&mut step, "name", fc.g("name").str());
        let id = fc.g("id");
        let call_id = fc.g("call_id");
        if id.exists() {
            cpa_json::set(&mut step, "call_id", id.str());
        } else if call_id.exists() {
            cpa_json::set(&mut step, "call_id", call_id.str());
        }
        let args = fc.g("args");
        if args.exists() {
            cpa_json::set(&mut step, "arguments", args.value());
        }
        steps.push(step);
        return steps;
    }
    let fr = part.g("functionResponse");
    if fr.exists() {
        let mut step = json!({"type": "function_result", "name": "", "result": {}});
        cpa_json::set(&mut step, "name", fr.g("name").str());
        let id = fr.g("id");
        let call_id = fr.g("call_id");
        if id.exists() {
            cpa_json::set(&mut step, "call_id", id.str());
        } else if call_id.exists() {
            cpa_json::set(&mut step, "call_id", call_id.str());
        }
        let response = fr.g("response");
        if response.exists() {
            cpa_json::set(&mut step, "result", response.value());
        }
        return vec![step];
    }
    let text = part.g("text");
    if text.exists() {
        if part.g("thought").bool() {
            return vec![thought_step(&sig, &text.str())];
        }
        if text.str().is_empty() {
            return if sig.is_empty() { vec![] } else { vec![thought_step(&sig, "")] };
        }
        let step = json!({"type": "model_output", "content": [{"type": "text", "text": text.str()}]});
        let mut steps = vec![step];
        if !sig.is_empty() {
            steps.push(thought_step(&sig, ""));
        }
        return steps;
    }
    for key in ["inlineData", "inline_data"] {
        let inline = part.g(key);
        if inline.exists() {
            if let Some(step) = inline_data_to_step(&inline) {
                let mut steps = vec![step];
                if !sig.is_empty() {
                    steps.push(thought_step(&sig, ""));
                }
                return steps;
            }
        }
    }
    if !sig.is_empty() {
        return vec![thought_step(&sig, "")];
    }
    vec![]
}

fn thought_step(sig: &str, text: &str) -> Value {
    let mut step = json!({"type": "thought"});
    if !sig.is_empty() {
        cpa_json::set(&mut step, "signature", sig);
    }
    if !text.is_empty() {
        cpa_json::set(&mut step, "content", json!([{"type": "text", "text": text}]));
    }
    step
}

fn inline_data_to_step(inline: &Res<'_>) -> Option<Value> {
    let mut mime_type = inline.g("mimeType").str();
    if mime_type.is_empty() {
        mime_type = inline.g("mime_type").str();
    }
    let data = inline.g("data").str();
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    let lower = mime_type.to_lowercase();
    let content_type = if lower.starts_with("image/") {
        "image"
    } else if lower.starts_with("audio/") {
        "audio"
    } else if lower.starts_with("video/") {
        "video"
    } else {
        "document"
    };
    Some(json!({"type": "model_output", "content": [{"type": content_type, "mime_type": mime_type, "data": data}]}))
}

const USAGE_COUNT_PATHS: [&str; 10] = [
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
];

fn has_stream_usage(root: &Value) -> bool {
    let usage = usage_node(root);
    usage.exists() && USAGE_COUNT_PATHS.iter().any(|p| usage.g(p).exists())
}

fn set_usage(out: &mut Value, path: &str, root: &Value) {
    let usage = usage_node(root);
    if !usage.exists() {
        return;
    }
    cpa_json::set(out, &format!("{path}.input_tokens"), first_usage_int(&usage, &["promptTokenCount", "prompt_token_count"]));
    cpa_json::set(out, &format!("{path}.output_tokens"), first_usage_int(&usage, &["candidatesTokenCount", "candidates_token_count"]));
    if usage_path_exists(&usage, &["thoughtsTokenCount", "thoughts_token_count"]) {
        cpa_json::set(out, &format!("{path}.reasoning_tokens"), first_usage_int(&usage, &["thoughtsTokenCount", "thoughts_token_count"]));
    }
    cpa_json::set(out, &format!("{path}.total_tokens"), first_usage_int(&usage, &["totalTokenCount", "total_token_count"]));
    if usage_path_exists(&usage, &["cachedContentTokenCount", "cached_content_token_count"]) {
        cpa_json::set(out, &format!("{path}.cached_tokens"), first_usage_int(&usage, &["cachedContentTokenCount", "cached_content_token_count"]));
    }
}

fn set_stream_usage(out: &mut Value, path: &str, root: &Value) {
    let usage = usage_node(root);
    if !usage.exists() {
        return;
    }
    let input_tokens = first_usage_int(&usage, &["promptTokenCount", "prompt_token_count"]);
    let output_tokens = first_usage_int(&usage, &["candidatesTokenCount", "candidates_token_count"]);
    let total_tokens = first_usage_int(&usage, &["totalTokenCount", "total_token_count"]);
    let thought_tokens = first_usage_int(&usage, &["thoughtsTokenCount", "thoughts_token_count"]);
    let cached_tokens = first_usage_int(&usage, &["cachedContentTokenCount", "cached_content_token_count"]);
    cpa_json::set(out, &format!("{path}.total_tokens"), total_tokens);
    cpa_json::set(out, &format!("{path}.total_input_tokens"), input_tokens);
    cpa_json::set(out, &format!("{path}.input_tokens_by_modality"), json!([{"modality": "text", "tokens": input_tokens}]));
    cpa_json::set(out, &format!("{path}.total_cached_tokens"), cached_tokens);
    cpa_json::set(out, &format!("{path}.total_output_tokens"), output_tokens);
    cpa_json::set(out, &format!("{path}.total_tool_use_tokens"), 0);
    cpa_json::set(out, &format!("{path}.total_thought_tokens"), thought_tokens);
}

fn usage_node(root: &Value) -> Res<'_> {
    for key in ["usageMetadata", "usage_metadata", "cpaUsageMetadata"] {
        let usage = root.g(key);
        if usage.exists() {
            return usage;
        }
    }
    Res::NONE
}

fn first_usage_int(usage: &Res<'_>, paths: &[&str]) -> i64 {
    paths.iter().map(|p| usage.g(p)).find(|v| v.exists()).map_or(0, |v| v.int())
}

fn usage_path_exists(usage: &Res<'_>, paths: &[&str]) -> bool {
    paths.iter().any(|p| usage.g(p).exists())
}

fn function_part_id(part: &Res<'_>) -> String {
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

fn thought_signature(part: &Res<'_>) -> String {
    ["thoughtSignature", "thought_signature", "extra_content.google.thought_signature"]
        .iter()
        .map(|p| part.g(p).str().trim().to_string())
        .find(|s| !s.is_empty())
        .unwrap_or_default()
}
