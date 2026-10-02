//! OpenAI Chat Completions response -> OpenAI Responses response
//! (Go: openai_openai-responses_response.go).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use cpa_json::{Value, J};

use super::tools::{unwrap_custom_tool_input, ToolIndex};
use super::{go_any, pick_request_json};
use crate::common::{self, ApplyPatchCallState, ApplyPatchErrorState};
use crate::registry::{Ctx, Param};

struct Reasoning {
    reasoning_id: String,
    reasoning_data: String,
    output_index: i64,
}

/// Streaming state (Go: oaiToResponsesState).
#[derive(Default)]
struct State {
    err: ApplyPatchErrorState,
    apply_patch_calls: HashMap<String, ApplyPatchCallState>,
    request_json: Vec<u8>,
    tool_index: ToolIndex,
    request_initialized: bool,
    seq: i64,
    response_id: String,
    created: i64,
    started: bool,
    completed_emitted: bool,
    reasoning_id: String,
    reasoning_index: i64,
    // Aggregation buffers for response.output.
    msg_text_buf: HashMap<i64, String>,
    reasoning_buf: String,
    reasonings: Vec<Reasoning>,
    func_args_buf: HashMap<String, String>,
    func_names: HashMap<String, String>,
    func_call_ids: HashMap<String, String>,
    func_identity_conflicts: HashSet<String>,
    func_output_ix: HashMap<String, i64>,
    func_args_sent: HashMap<String, usize>,
    msg_output_ix: HashMap<i64, i64>,
    next_output_ix: i64,
    // Message item state per output index.
    msg_item_added: HashSet<i64>,
    msg_content_added: HashSet<i64>,
    msg_item_done: HashSet<i64>,
    // Function item state.
    func_item_added: HashSet<String>,
    func_item_custom: HashSet<String>,
    func_args_done: HashSet<String>,
    func_item_done: HashSet<String>,
    finish_reason: String,
    // Usage aggregation.
    prompt_tokens: i64,
    cached_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    reasoning_tokens: i64,
    usage_seen: bool,
}

type Out = Vec<Vec<u8>>;

/// Synthesized response identifiers need a process-wide unique counter.
static RESPONSE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tpl(s: &str) -> Value {
    cpa_json::parse_str(s)
}

fn emit(out: &mut Out, event: &str, payload: &Value) {
    out.push(common::sse_event_data(event, &cpa_json::to_vec(payload)));
}

fn emit_bytes(out: &mut Out, event: &str, payload: &[u8]) {
    out.push(common::sse_event_data(event, payload));
}

fn next_seq(st: &mut State) -> i64 {
    st.seq += 1;
    st.seq
}

fn alloc_output_index(st: &mut State) -> i64 {
    let ix = st.next_output_ix;
    st.next_output_ix += 1;
    ix
}

/// `incomplete_details` for a finish reason that ends the response early.
fn incomplete_by_finish_reason(reason: &str) -> Option<&'static str> {
    match reason {
        "length" | "max_tokens" => Some(r#"{"reason":"max_output_tokens"}"#),
        "content_filter" => Some(r#"{"reason":"content_filter"}"#),
        _ => None,
    }
}

/// Copies request fields into a response object, as the Responses API echoes them. `prefix` is
/// `response.` for events and empty for the non-stream body.
fn echo_request_fields(target: &mut Value, prefix: &str, req: &Value) {
    let p = |name: &str| format!("{prefix}{name}");
    let v = req.g("instructions");
    if v.exists() {
        cpa_json::set(target, &p("instructions"), v.str());
    }
    let v = req.g("max_output_tokens");
    if v.exists() {
        cpa_json::set(target, &p("max_output_tokens"), v.int());
    }
    let v = req.g("max_tool_calls");
    if v.exists() {
        cpa_json::set(target, &p("max_tool_calls"), v.int());
    }
    let v = req.g("model");
    if v.exists() {
        cpa_json::set(target, &p("model"), v.str());
    }
    let v = req.g("parallel_tool_calls");
    if v.exists() {
        cpa_json::set(target, &p("parallel_tool_calls"), v.bool());
    }
    let v = req.g("previous_response_id");
    if v.exists() {
        cpa_json::set(target, &p("previous_response_id"), v.str());
    }
    let v = req.g("prompt_cache_key");
    if v.exists() {
        cpa_json::set(target, &p("prompt_cache_key"), v.str());
    }
    let v = req.g("reasoning");
    if v.exists() {
        cpa_json::set(target, &p("reasoning"), go_any(v.value()));
    }
    let v = req.g("safety_identifier");
    if v.exists() {
        cpa_json::set(target, &p("safety_identifier"), v.str());
    }
    let v = req.g("service_tier");
    if v.exists() {
        cpa_json::set(target, &p("service_tier"), v.str());
    }
    let v = req.g("store");
    if v.exists() {
        cpa_json::set(target, &p("store"), v.bool());
    }
    let v = req.g("temperature");
    if v.exists() {
        cpa_json::set(target, &p("temperature"), cpa_json::num_f64(v.float()));
    }
    let v = req.g("text");
    if v.exists() {
        cpa_json::set(target, &p("text"), go_any(v.value()));
    }
    let v = req.g("tool_choice");
    if v.exists() {
        cpa_json::set(target, &p("tool_choice"), go_any(v.value()));
    }
    let v = req.g("tools");
    if v.exists() {
        cpa_json::set(target, &p("tools"), go_any(v.value()));
    }
    let v = req.g("top_logprobs");
    if v.exists() {
        cpa_json::set(target, &p("top_logprobs"), v.int());
    }
    let v = req.g("top_p");
    if v.exists() {
        cpa_json::set(target, &p("top_p"), cpa_json::num_f64(v.float()));
    }
    let v = req.g("truncation");
    if v.exists() {
        cpa_json::set(target, &p("truncation"), v.str());
    }
    let v = req.g("user");
    if v.exists() {
        cpa_json::set(target, &p("user"), go_any(v.value()));
    }
    let v = req.g("metadata");
    if v.exists() {
        cpa_json::set(target, &p("metadata"), go_any(v.value()));
    }
}

/// The `response.completed` (or `response.incomplete`) event with the aggregated output.
fn build_responses_completed_event(st: &mut State, request_raw_json: &[u8]) -> Vec<u8> {
    let mut event_type = "response.completed";
    let mut status = "completed";
    let incomplete_details = incomplete_by_finish_reason(&st.finish_reason);
    if incomplete_details.is_some() {
        event_type = "response.incomplete";
        status = "incomplete";
    }

    let mut completed = tpl(
        r#"{"type":"","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"","background":false,"error":null}}"#,
    );
    cpa_json::set(&mut completed, "type", event_type);
    let seq = next_seq(st);
    cpa_json::set(&mut completed, "sequence_number", seq);
    cpa_json::set(&mut completed, "response.id", st.response_id.clone());
    cpa_json::set(&mut completed, "response.created_at", st.created);
    cpa_json::set(&mut completed, "response.status", status);
    if let Some(details) = incomplete_details {
        cpa_json::set(&mut completed, "response.incomplete_details", tpl(details));
    }
    // Inject original request fields into the response.
    if !request_raw_json.is_empty() {
        echo_request_fields(&mut completed, "response.", &cpa_json::parse(request_raw_json));
    }

    let is_incomplete = incomplete_details.is_some();
    let mut output_items: Vec<(i64, Value)> = Vec::new();
    for r in &st.reasonings {
        let mut item = tpl(r#"{"id":"","type":"reasoning","summary":[{"type":"summary_text","text":""}]}"#);
        cpa_json::set(&mut item, "id", r.reasoning_id.clone());
        cpa_json::set(&mut item, "summary.0.text", r.reasoning_data.clone());
        output_items.push((r.output_index, item));
    }
    for &i in &st.msg_item_added {
        let txt = st.msg_text_buf.get(&i).cloned().unwrap_or_default();
        let msg_status = if is_incomplete { "incomplete" } else { "completed" };
        let mut item = tpl(
            r#"{"id":"","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":""}],"role":"assistant"}"#,
        );
        cpa_json::set(&mut item, "id", format!("msg_{}_{}", st.response_id, i));
        cpa_json::set(&mut item, "status", msg_status);
        cpa_json::set(&mut item, "content.0.text", txt);
        output_items.push((st.msg_output_ix.get(&i).copied().unwrap_or(0), item));
    }
    let keys: Vec<String> = st.func_args_buf.keys().cloned().collect();
    for key in keys {
        if !st.func_item_done.contains(&key) {
            continue;
        }
        let args = st.func_args_buf.get(&key).cloned().unwrap_or_default();
        let call_id = st.func_call_ids.get(&key).cloned().unwrap_or_default();
        let name = st.func_names.get(&key).cloned().unwrap_or_default();
        let tool_status = if is_incomplete { "incomplete" } else { "completed" };
        let output_ix = st.func_output_ix.get(&key).copied().unwrap_or(0);
        if st.func_item_custom.contains(&key) {
            let mut item = tpl(r#"{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}"#);
            cpa_json::set(&mut item, "id", format!("ctc_{call_id}"));
            cpa_json::set(&mut item, "status", tool_status);
            let input = match st.apply_patch_calls.get(&key) {
                Some(patch_call) => patch_call.decoder.input().to_string(),
                None => unwrap_custom_tool_input(&args),
            };
            cpa_json::set(&mut item, "input", input);
            cpa_json::set(&mut item, "call_id", call_id);
            let item = st.tool_index.apply_identity(&cpa_json::to_vec(&item), &name, "");
            output_items.push((output_ix, cpa_json::parse(&item)));
            continue;
        }
        let mut item = tpl(r#"{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}"#);
        cpa_json::set(&mut item, "id", format!("fc_{call_id}"));
        cpa_json::set(&mut item, "status", tool_status);
        cpa_json::set(&mut item, "arguments", args);
        cpa_json::set(&mut item, "call_id", call_id);
        let item = st.tool_index.apply_identity(&cpa_json::to_vec(&item), &name, "");
        output_items.push((output_ix, cpa_json::parse(&item)));
    }
    output_items.sort_by_key(|(index, _)| *index);
    if !output_items.is_empty() {
        let outputs: Vec<Value> = output_items.into_iter().map(|(_, item)| item).collect();
        cpa_json::set(&mut completed, "response.output", Value::Array(outputs));
    }
    if st.usage_seen {
        cpa_json::set(&mut completed, "response.usage.input_tokens", st.prompt_tokens);
        cpa_json::set(&mut completed, "response.usage.input_tokens_details.cached_tokens", st.cached_tokens);
        cpa_json::set(&mut completed, "response.usage.output_tokens", st.completion_tokens);
        if st.reasoning_tokens > 0 {
            cpa_json::set(&mut completed, "response.usage.output_tokens_details.reasoning_tokens", st.reasoning_tokens);
        }
        let mut total = st.total_tokens;
        if total == 0 {
            total = st.prompt_tokens + st.completion_tokens;
        }
        cpa_json::set(&mut completed, "response.usage.total_tokens", total);
    }
    common::sse_event_data(event_type, &cpa_json::to_vec(&completed))
}

/// Records a tool-input failure once and emits `response.failed`.
fn fail_tool_input(st: &mut State, out: &mut Out, err: String) {
    if st.err.tool_input_error().is_none() {
        st.err.set_tool_input_error(err);
        let seq = next_seq(st);
        emit_bytes(out, "response.failed", &common::apply_patch_failure(&st.response_id, seq));
    }
}

/// Announces a tool call item once its call id and name are known (or `force`d at finalization).
fn emit_tool_item(st: &mut State, out: &mut Out, key: &str, force: bool) {
    if st.func_item_added.contains(key) {
        return;
    }
    let mut call_id = st.func_call_ids.get(key).cloned().unwrap_or_default();
    let mut name = st.tool_index.canonical_name(st.func_names.get(key).map_or("", String::as_str));
    st.func_names.insert(key.to_string(), name.clone());
    if !force && (call_id.is_empty() || name.is_empty()) {
        return;
    }
    if name.is_empty() {
        let (custom_tool_name, ok) = st.tool_index.single_custom_name();
        if ok {
            name = custom_tool_name.clone();
            st.func_names.insert(key.to_string(), custom_tool_name);
        }
    }
    if st.tool_index.is_apply_patch(&name) && st.func_identity_conflicts.contains(key) {
        fail_tool_input(st, out, "conflicting apply_patch call identity".into());
        return;
    }
    if call_id.is_empty() {
        call_id = format!("call_{}_{}", st.response_id, key.replace(':', "_"));
        st.func_call_ids.insert(key.to_string(), call_id.clone());
    }

    let output_index = st.func_output_ix.get(key).copied().unwrap_or(0);
    let is_custom_tool = st.tool_index.custom.contains(&name);
    if is_custom_tool {
        st.func_item_custom.insert(key.to_string());
    } else {
        st.func_item_custom.remove(key);
    }
    if is_custom_tool {
        if st.tool_index.is_apply_patch(&name)
            && let Some(d) = st.tool_index.by_chat.get(&name)
        {
            st.apply_patch_calls.insert(
                key.to_string(),
                ApplyPatchCallState {
                    item_id: format!("ctc_{call_id}"),
                    call_id: call_id.clone(),
                    name: d.local_name.clone(),
                    namespace: d.namespace.clone(),
                    output_index,
                    ..Default::default()
                },
            );
        }
        let mut o = tpl(
            r#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"custom_tool_call","status":"in_progress","input":"","call_id":"","name":""}}"#,
        );
        let seq = next_seq(st);
        cpa_json::set(&mut o, "sequence_number", seq);
        cpa_json::set(&mut o, "output_index", output_index);
        cpa_json::set(&mut o, "item.id", format!("ctc_{call_id}"));
        cpa_json::set(&mut o, "item.call_id", call_id);
        let o = st.tool_index.apply_identity(&cpa_json::to_vec(&o), &name, "item");
        emit_bytes(out, "response.output_item.added", &o);
    } else {
        let mut o = tpl(
            r#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"function_call","status":"in_progress","arguments":"","call_id":"","name":""}}"#,
        );
        let seq = next_seq(st);
        cpa_json::set(&mut o, "sequence_number", seq);
        cpa_json::set(&mut o, "output_index", output_index);
        cpa_json::set(&mut o, "item.id", format!("fc_{call_id}"));
        cpa_json::set(&mut o, "item.call_id", call_id);
        let o = st.tool_index.apply_identity(&cpa_json::to_vec(&o), &name, "item");
        emit_bytes(out, "response.output_item.added", &o);
    }
    st.func_item_added.insert(key.to_string());
}

/// Emits the not yet sent part of a tool call's buffered arguments as a delta event.
fn emit_pending_function_args(st: &mut State, out: &mut Out, key: &str) {
    if !st.func_item_added.contains(key) || st.err.tool_input_error().is_some() {
        return;
    }
    let sent = st.func_args_sent.get(key).copied().unwrap_or(0);
    let Some(args) = st.func_args_buf.get(key).filter(|b| b.len() > sent).cloned() else {
        return;
    };
    let delta = args[sent..].to_string();
    if st.func_item_custom.contains(key) {
        if let Some(patch_call) = st.apply_patch_calls.get_mut(key) {
            match patch_call.push_arguments(&delta) {
                Err(err) => fail_tool_input(st, out, err),
                Ok(patch_delta) if !patch_delta.is_empty() => {
                    let seq = next_seq(st);
                    if let Some(patch_call) = st.apply_patch_calls.get(key) {
                        emit_bytes(
                            out,
                            "response.custom_tool_call_input.delta",
                            &common::apply_patch_input_delta(patch_call, &patch_delta, seq),
                        );
                    }
                }
                Ok(_) => {}
            }
            st.func_args_sent.insert(key.to_string(), args.len());
        }
        return;
    }
    let call_id = st.func_call_ids.get(key).cloned().unwrap_or_default();
    let mut ad = tpl(r#"{"type":"response.function_call_arguments.delta","sequence_number":0,"item_id":"","output_index":0,"delta":""}"#);
    let seq = next_seq(st);
    cpa_json::set(&mut ad, "sequence_number", seq);
    cpa_json::set(&mut ad, "item_id", format!("fc_{call_id}"));
    cpa_json::set(&mut ad, "output_index", st.func_output_ix.get(key).copied().unwrap_or(0));
    cpa_json::set(&mut ad, "delta", delta);
    emit(out, "response.function_call_arguments.delta", &ad);
    st.func_args_sent.insert(key.to_string(), args.len());
}

fn stop_reasoning(st: &mut State, out: &mut Out, text: &str) {
    let mut text_done = tpl(
        r#"{"type":"response.reasoning_summary_text.done","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"text":""}"#,
    );
    let seq = next_seq(st);
    cpa_json::set(&mut text_done, "sequence_number", seq);
    cpa_json::set(&mut text_done, "item_id", st.reasoning_id.clone());
    cpa_json::set(&mut text_done, "output_index", st.reasoning_index);
    cpa_json::set(&mut text_done, "text", text);
    emit(out, "response.reasoning_summary_text.done", &text_done);

    let mut part_done = tpl(
        r#"{"type":"response.reasoning_summary_part.done","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}"#,
    );
    let seq = next_seq(st);
    cpa_json::set(&mut part_done, "sequence_number", seq);
    cpa_json::set(&mut part_done, "item_id", st.reasoning_id.clone());
    cpa_json::set(&mut part_done, "output_index", st.reasoning_index);
    cpa_json::set(&mut part_done, "part.text", text);
    emit(out, "response.reasoning_summary_part.done", &part_done);

    let mut item_done = tpl(
        r#"{"type":"response.output_item.done","item":{"id":"","type":"reasoning","encrypted_content":"","summary":[{"type":"summary_text","text":""}]},"output_index":0,"sequence_number":0}"#,
    );
    let seq = next_seq(st);
    cpa_json::set(&mut item_done, "sequence_number", seq);
    cpa_json::set(&mut item_done, "item.id", st.reasoning_id.clone());
    cpa_json::set(&mut item_done, "output_index", st.reasoning_index);
    cpa_json::set(&mut item_done, "item.summary.0.text", text);
    emit(out, "response.output_item.done", &item_done);

    st.reasonings.push(Reasoning {
        reasoning_id: std::mem::take(&mut st.reasoning_id),
        reasoning_data: text.to_string(),
        output_index: st.reasoning_index,
    });
}

fn emit_message_item_done(st: &mut State, out: &mut Out, idx: i64) {
    if !st.msg_item_added.contains(&idx) || st.msg_item_done.contains(&idx) {
        return;
    }
    let msg_output_index = st.msg_output_ix.get(&idx).copied().unwrap_or(0);
    let full_text = st.msg_text_buf.get(&idx).cloned().unwrap_or_default();
    let item_id = format!("msg_{}_{}", st.response_id, idx);

    let mut done = tpl(
        r#"{"type":"response.output_text.done","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"text":"","logprobs":[]}"#,
    );
    let seq = next_seq(st);
    cpa_json::set(&mut done, "sequence_number", seq);
    cpa_json::set(&mut done, "item_id", item_id.clone());
    cpa_json::set(&mut done, "output_index", msg_output_index);
    cpa_json::set(&mut done, "content_index", 0);
    cpa_json::set(&mut done, "text", full_text.clone());
    emit(out, "response.output_text.done", &done);

    let mut part_done = tpl(
        r#"{"type":"response.content_part.done","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}"#,
    );
    let seq = next_seq(st);
    cpa_json::set(&mut part_done, "sequence_number", seq);
    cpa_json::set(&mut part_done, "item_id", item_id.clone());
    cpa_json::set(&mut part_done, "output_index", msg_output_index);
    cpa_json::set(&mut part_done, "content_index", 0);
    cpa_json::set(&mut part_done, "part.text", full_text.clone());
    emit(out, "response.content_part.done", &part_done);

    let msg_status = if incomplete_by_finish_reason(&st.finish_reason).is_some() { "incomplete" } else { "completed" };
    let mut item_done = tpl(
        r#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":""}],"role":"assistant"}}"#,
    );
    let seq = next_seq(st);
    cpa_json::set(&mut item_done, "sequence_number", seq);
    cpa_json::set(&mut item_done, "output_index", msg_output_index);
    cpa_json::set(&mut item_done, "item.id", item_id);
    cpa_json::set(&mut item_done, "item.status", msg_status);
    cpa_json::set(&mut item_done, "item.content.0.text", full_text);
    emit(out, "response.output_item.done", &item_done);
    st.msg_item_done.insert(idx);
}

/// Closes every open message, reasoning and tool call item (on finish_reason and on `[DONE]`).
fn finalize_open_items(st: &mut State, out: &mut Out) {
    if st.err.tool_input_error().is_some() {
        return;
    }
    if !st.msg_item_added.is_empty() {
        let mut idxs: Vec<i64> = st.msg_item_added.iter().copied().collect();
        idxs.sort_by_key(|idx| st.msg_output_ix.get(idx).copied().unwrap_or(0));
        for idx in idxs {
            emit_message_item_done(st, out, idx);
        }
    }

    if !st.reasoning_id.is_empty() {
        let text = std::mem::take(&mut st.reasoning_buf);
        stop_reasoning(st, out, &text);
    }

    if st.func_args_buf.is_empty() {
        return;
    }
    let mut keys: Vec<String> = st.func_args_buf.keys().cloned().collect();
    keys.sort_by(|a, b| {
        let left = st.func_output_ix.get(a).copied().unwrap_or(0);
        let right = st.func_output_ix.get(b).copied().unwrap_or(0);
        left.cmp(&right).then_with(|| a.cmp(b))
    });
    for key in keys {
        if st.func_item_done.contains(&key) {
            continue;
        }
        let args_buf = st.func_args_buf.get(&key).cloned().unwrap_or_default();
        let has_args = !args_buf.is_empty();
        let is_incomplete = incomplete_by_finish_reason(&st.finish_reason).is_some();
        let is_explicit_tool_finish = st.finish_reason == "tool_calls" || st.finish_reason == "stop";

        // A stream that ended without finish_reason and holds no (or partial) arguments must not
        // synthesize empty arguments or complete the in-flight tool call as successful.
        let mut name = st.tool_index.canonical_name(st.func_names.get(&key).map_or("", String::as_str));
        if name.is_empty() {
            name = st.tool_index.single_custom_name().0;
        }
        if !st.tool_index.is_apply_patch(&name)
            && st.finish_reason.is_empty()
            && (!has_args || !cpa_json::valid(args_buf.as_bytes()))
        {
            continue;
        }

        emit_tool_item(st, out, &key, true);
        emit_pending_function_args(st, out, &key);
        if st.err.tool_input_error().is_some() {
            return;
        }
        let call_id = st.func_call_ids.get(&key).cloned().unwrap_or_default();
        if call_id.is_empty() || st.func_item_done.contains(&key) {
            continue;
        }

        let output_index = st.func_output_ix.get(&key).copied().unwrap_or(0);
        let mut tool_status = "completed";
        let mut args = "{}".to_string();
        if has_args {
            args = args_buf;
        } else if is_incomplete || !is_explicit_tool_finish {
            args = String::new();
        }
        if is_incomplete {
            tool_status = "incomplete";
        }
        let func_name = st.func_names.get(&key).cloned().unwrap_or_default();

        if st.func_item_custom.contains(&key) {
            let input;
            if let Some(patch_call) = st.apply_patch_calls.get_mut(&key) {
                match patch_call.finish_arguments(&args) {
                    Err(err) => {
                        fail_tool_input(st, out, err);
                        return;
                    }
                    Ok((tail, full_input)) => {
                        input = full_input;
                        if !tail.is_empty() {
                            let seq = next_seq(st);
                            if let Some(patch_call) = st.apply_patch_calls.get(&key) {
                                emit_bytes(out, "response.custom_tool_call_input.delta", &common::apply_patch_input_delta(patch_call, &tail, seq));
                            }
                        }
                        let seq = next_seq(st);
                        if let Some(patch_call) = st.apply_patch_calls.get(&key) {
                            emit_bytes(out, "response.custom_tool_call_input.done", &common::apply_patch_input_done(patch_call, &input, seq));
                        }
                    }
                }
            } else {
                input = unwrap_custom_tool_input(&args);
                let mut input_done = tpl(
                    r#"{"type":"response.custom_tool_call_input.done","sequence_number":0,"item_id":"","output_index":0,"input":""}"#,
                );
                let seq = next_seq(st);
                cpa_json::set(&mut input_done, "sequence_number", seq);
                cpa_json::set(&mut input_done, "item_id", format!("ctc_{call_id}"));
                cpa_json::set(&mut input_done, "output_index", output_index);
                cpa_json::set(&mut input_done, "input", input.clone());
                emit(out, "response.custom_tool_call_input.done", &input_done);
            }

            let mut item_done = tpl(
                r#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}}"#,
            );
            let seq = next_seq(st);
            cpa_json::set(&mut item_done, "sequence_number", seq);
            cpa_json::set(&mut item_done, "output_index", output_index);
            cpa_json::set(&mut item_done, "item.id", format!("ctc_{call_id}"));
            cpa_json::set(&mut item_done, "item.status", tool_status);
            cpa_json::set(&mut item_done, "item.input", input);
            cpa_json::set(&mut item_done, "item.call_id", call_id);
            let item_done = st.tool_index.apply_identity(&cpa_json::to_vec(&item_done), &func_name, "item");
            emit_bytes(out, "response.output_item.done", &item_done);
            st.func_item_done.insert(key.clone());
            st.func_args_done.insert(key);
            continue;
        }

        let mut fc_done = tpl(r#"{"type":"response.function_call_arguments.done","sequence_number":0,"item_id":"","output_index":0,"arguments":""}"#);
        let seq = next_seq(st);
        cpa_json::set(&mut fc_done, "sequence_number", seq);
        cpa_json::set(&mut fc_done, "item_id", format!("fc_{call_id}"));
        cpa_json::set(&mut fc_done, "output_index", output_index);
        cpa_json::set(&mut fc_done, "arguments", args.clone());
        emit(out, "response.function_call_arguments.done", &fc_done);

        let mut item_done = tpl(
            r#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}}"#,
        );
        let seq = next_seq(st);
        cpa_json::set(&mut item_done, "sequence_number", seq);
        cpa_json::set(&mut item_done, "output_index", output_index);
        cpa_json::set(&mut item_done, "item.id", format!("fc_{call_id}"));
        cpa_json::set(&mut item_done, "item.status", tool_status);
        cpa_json::set(&mut item_done, "item.arguments", args);
        cpa_json::set(&mut item_done, "item.call_id", call_id);
        let item_done = st.tool_index.apply_identity(&cpa_json::to_vec(&item_done), &func_name, "item");
        emit_bytes(out, "response.output_item.done", &item_done);
        st.func_item_done.insert(key.clone());
        st.func_args_done.insert(key);
    }
}

/// Converts one Chat Completions streaming line into Responses SSE events (`response.*`). Syncs
/// the apply_patch tool-input error to `param` so the registry suppresses fallbacks.
pub fn convert_openai_chat_completions_response_to_openai_responses(
    _ctx: &Ctx,
    model_name: &str,
    original: &[u8],
    translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let st = param.state(State::default);
    let out = convert_stream(st, model_name, original, translated, raw);
    let err = st.err.tool_input_error().map(str::to_string);
    if err.is_some() {
        param.tool_input_error = err;
    }
    out
}

fn convert_stream(st: &mut State, model_name: &str, original: &[u8], translated: &[u8], raw: &[u8]) -> Out {
    if st.err.tool_input_error().is_some() || st.completed_emitted {
        return vec![];
    }

    let mut raw = raw;
    if let Some(rest) = raw.strip_prefix(b"data:") {
        raw = rest.trim_ascii();
    }
    raw = raw.trim_ascii();
    if !st.request_initialized {
        st.request_json = pick_request_json(original, translated).map(<[u8]>::to_vec).unwrap_or_default();
        st.tool_index = ToolIndex::new(&cpa_json::parse(&st.request_json));
        st.request_initialized = true;
    }
    if raw.is_empty() {
        return vec![];
    }
    let request_for_namespace = st.request_json.clone();
    let is_done = raw == b"[DONE]";
    if is_done && (!st.started || st.completed_emitted) {
        return vec![];
    }

    let root = cpa_json::parse(raw);
    if !is_done {
        let obj = root.g("object");
        if obj.exists() && !obj.str().is_empty() && obj.str() != "chat.completion.chunk" {
            return vec![];
        }
        if !root.g("choices").is_array() {
            return vec![];
        }
    }

    let usage = root.g("usage");
    if usage.exists() {
        let v = usage.g("prompt_tokens");
        if v.exists() {
            st.prompt_tokens = v.int();
            st.usage_seen = true;
        }
        let v = usage.g("prompt_tokens_details.cached_tokens");
        if v.exists() {
            st.cached_tokens = v.int();
            st.usage_seen = true;
        }
        let v = usage.g("completion_tokens");
        let v2 = usage.g("output_tokens");
        if v.exists() {
            st.completion_tokens = v.int();
            st.usage_seen = true;
        } else if v2.exists() {
            st.completion_tokens = v2.int();
            st.usage_seen = true;
        }
        let v = usage.g("output_tokens_details.reasoning_tokens");
        let v2 = usage.g("completion_tokens_details.reasoning_tokens");
        if v.exists() {
            st.reasoning_tokens = v.int();
            st.usage_seen = true;
        } else if v2.exists() {
            st.reasoning_tokens = v2.int();
            st.usage_seen = true;
        }
        let v = usage.g("total_tokens");
        if v.exists() {
            st.total_tokens = v.int();
            st.usage_seen = true;
        }
    }

    let mut out: Out = Vec::new();

    if !st.started {
        st.response_id = root.g("id").str();
        st.created = root.g("created").int();
        // Reset aggregation state for a new streaming response.
        st.msg_text_buf.clear();
        st.reasoning_buf.clear();
        st.reasoning_id.clear();
        st.reasoning_index = 0;
        st.apply_patch_calls.clear();
        st.func_args_buf.clear();
        st.func_names.clear();
        st.func_call_ids.clear();
        st.func_identity_conflicts.clear();
        st.func_output_ix.clear();
        st.func_args_sent.clear();
        st.msg_output_ix.clear();
        st.next_output_ix = 0;
        st.msg_item_added.clear();
        st.msg_content_added.clear();
        st.msg_item_done.clear();
        st.func_item_added.clear();
        st.func_item_custom.clear();
        st.func_args_done.clear();
        st.func_item_done.clear();
        st.prompt_tokens = 0;
        st.cached_tokens = 0;
        st.completion_tokens = 0;
        st.total_tokens = 0;
        st.reasoning_tokens = 0;
        st.finish_reason.clear();
        st.usage_seen = false;
        st.completed_emitted = false;

        let mut created = tpl(
            r#"{"type":"response.created","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"in_progress","background":false,"error":null,"output":[]}}"#,
        );
        let seq = next_seq(st);
        cpa_json::set(&mut created, "sequence_number", seq);
        cpa_json::set(&mut created, "response.id", st.response_id.clone());
        cpa_json::set(&mut created, "response.created_at", st.created);
        let mut request_model_name = common::request_model_name(original, translated);
        if request_model_name.is_empty() {
            request_model_name = model_name.to_string();
        }
        if !request_model_name.is_empty() {
            cpa_json::set(&mut created, "response.model", request_model_name.clone());
        }
        emit(&mut out, "response.created", &created);

        let mut inprog = tpl(
            r#"{"type":"response.in_progress","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"in_progress","output":[]}}"#,
        );
        let seq = next_seq(st);
        cpa_json::set(&mut inprog, "sequence_number", seq);
        cpa_json::set(&mut inprog, "response.id", st.response_id.clone());
        cpa_json::set(&mut inprog, "response.created_at", st.created);
        if !request_model_name.is_empty() {
            cpa_json::set(&mut inprog, "response.model", request_model_name);
        }
        emit(&mut out, "response.in_progress", &inprog);
        st.started = true;
    }

    if is_done {
        finalize_open_items(st, &mut out);
        if st.err.tool_input_error().is_some() {
            return out;
        }
        let has_active_unfinished_tool = st.func_item_added.iter().any(|key| !st.func_item_done.contains(key));
        if has_active_unfinished_tool {
            return out;
        }
        if st.msg_item_added.is_empty() && st.func_item_added.is_empty() {
            return out;
        }
        st.completed_emitted = true;
        let completed = build_responses_completed_event(st, &request_for_namespace);
        out.push(completed);
        return out;
    }

    // choices[].delta content / tool_calls / reasoning_content
    for choice in root.g("choices").array() {
        let idx = choice.g("index").int();
        let delta = choice.g("delta");
        if delta.exists() {
            // reasoning_content (OpenAI reasoning incremental text), falling back to `reasoning`.
            let mut rc = delta.g("reasoning_content");
            if !rc.exists() || rc.str().is_empty() {
                rc = delta.g("reasoning");
            }
            if rc.exists() && !rc.str().is_empty() {
                let rc_text = rc.str();
                // On first appearance, add the reasoning item and part.
                if st.reasoning_id.is_empty() {
                    st.reasoning_id = format!("rs_{}_{}", st.response_id, idx);
                    st.reasoning_index = alloc_output_index(st);
                    let mut item = tpl(
                        r#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"reasoning","status":"in_progress","summary":[]}}"#,
                    );
                    let seq = next_seq(st);
                    cpa_json::set(&mut item, "sequence_number", seq);
                    cpa_json::set(&mut item, "output_index", st.reasoning_index);
                    cpa_json::set(&mut item, "item.id", st.reasoning_id.clone());
                    emit(&mut out, "response.output_item.added", &item);
                    let mut part = tpl(
                        r#"{"type":"response.reasoning_summary_part.added","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}"#,
                    );
                    let seq = next_seq(st);
                    cpa_json::set(&mut part, "sequence_number", seq);
                    cpa_json::set(&mut part, "item_id", st.reasoning_id.clone());
                    cpa_json::set(&mut part, "output_index", st.reasoning_index);
                    emit(&mut out, "response.reasoning_summary_part.added", &part);
                }
                st.reasoning_buf.push_str(&rc_text);
                let mut msg = tpl(
                    r#"{"type":"response.reasoning_summary_text.delta","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"delta":""}"#,
                );
                let seq = next_seq(st);
                cpa_json::set(&mut msg, "sequence_number", seq);
                cpa_json::set(&mut msg, "item_id", st.reasoning_id.clone());
                cpa_json::set(&mut msg, "output_index", st.reasoning_index);
                cpa_json::set(&mut msg, "delta", rc_text);
                emit(&mut out, "response.reasoning_summary_text.delta", &msg);
            }

            let c = delta.g("content");
            if c.exists() && !c.str().is_empty() {
                let c_text = c.str();
                // Announce the message item and its first content part before any text deltas.
                if !st.reasoning_id.is_empty() {
                    let text = std::mem::take(&mut st.reasoning_buf);
                    stop_reasoning(st, &mut out, &text);
                }
                if !st.msg_output_ix.contains_key(&idx) {
                    let ix = alloc_output_index(st);
                    st.msg_output_ix.insert(idx, ix);
                }
                let msg_output_index = st.msg_output_ix.get(&idx).copied().unwrap_or(0);
                let item_id = format!("msg_{}_{}", st.response_id, idx);
                if !st.msg_item_added.contains(&idx) {
                    let mut item = tpl(
                        r#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"message","status":"in_progress","content":[],"role":"assistant"}}"#,
                    );
                    let seq = next_seq(st);
                    cpa_json::set(&mut item, "sequence_number", seq);
                    cpa_json::set(&mut item, "output_index", msg_output_index);
                    cpa_json::set(&mut item, "item.id", item_id.clone());
                    emit(&mut out, "response.output_item.added", &item);
                    st.msg_item_added.insert(idx);
                }
                if !st.msg_content_added.contains(&idx) {
                    let mut part = tpl(
                        r#"{"type":"response.content_part.added","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}"#,
                    );
                    let seq = next_seq(st);
                    cpa_json::set(&mut part, "sequence_number", seq);
                    cpa_json::set(&mut part, "item_id", item_id.clone());
                    cpa_json::set(&mut part, "output_index", msg_output_index);
                    cpa_json::set(&mut part, "content_index", 0);
                    emit(&mut out, "response.content_part.added", &part);
                    st.msg_content_added.insert(idx);
                }

                let mut msg = tpl(
                    r#"{"type":"response.output_text.delta","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"delta":"","logprobs":[]}"#,
                );
                let seq = next_seq(st);
                cpa_json::set(&mut msg, "sequence_number", seq);
                cpa_json::set(&mut msg, "item_id", item_id);
                cpa_json::set(&mut msg, "output_index", msg_output_index);
                cpa_json::set(&mut msg, "content_index", 0);
                cpa_json::set(&mut msg, "delta", c_text.clone());
                emit(&mut out, "response.output_text.delta", &msg);
                // Aggregate for response.output.
                st.msg_text_buf.entry(idx).or_default().push_str(&c_text);
            }

            // Tool calls.
            let tcs = delta.g("tool_calls");
            if tcs.is_array() && !tcs.array().is_empty() {
                if !st.reasoning_id.is_empty() {
                    let text = std::mem::take(&mut st.reasoning_buf);
                    stop_reasoning(st, &mut out, &text);
                }
                // Close an open message for this index first, matching Codex's expected ordering.
                emit_message_item_done(st, &mut out, idx);

                for tc in tcs.array() {
                    let tool_index_n = tc.g("index").int();
                    let key = format!("{idx}:{tool_index_n}");
                    if !st.func_args_buf.contains_key(&key) {
                        st.func_args_buf.insert(key.clone(), String::new());
                        let ix = alloc_output_index(st);
                        st.func_output_ix.insert(key.clone(), ix);
                    }
                    let new_id = tc.g("id").str();
                    let new_name = st.tool_index.canonical_name(&tc.g("function.name").str());
                    let old_id = st.func_call_ids.get(&key).cloned().unwrap_or_default();
                    let old_name = st.tool_index.canonical_name(st.func_names.get(&key).map_or("", String::as_str));
                    // Retain conflicting nonempty ids until the winning tool is known.
                    if !new_id.is_empty() && !old_id.is_empty() && new_id != old_id {
                        st.func_identity_conflicts.insert(key.clone());
                    }
                    if (st.tool_index.is_apply_patch(&old_name) || st.tool_index.is_apply_patch(&new_name))
                        && (st.func_identity_conflicts.contains(&key) || (!new_name.is_empty() && !old_name.is_empty() && new_name != old_name))
                    {
                        fail_tool_input(st, &mut out, "conflicting apply_patch call identity".into());
                        break;
                    }
                    let new_call_id = tc.g("id").str();
                    if !new_call_id.is_empty() && st.func_call_ids.get(&key).is_none_or(String::is_empty) {
                        st.func_call_ids.insert(key.clone(), new_call_id);
                    }
                    let name_chunk = tc.g("function.name").str();
                    if !name_chunk.is_empty() && !st.func_item_added.contains(&key) {
                        st.func_names.insert(key.clone(), name_chunk);
                    }

                    let args = tc.g("function.arguments");
                    if args.exists() && !args.str().is_empty() {
                        st.func_args_buf.entry(key.clone()).or_default().push_str(&args.str());
                    }
                    emit_tool_item(st, &mut out, &key, false);
                    emit_pending_function_args(st, &mut out, &key);
                    if st.err.tool_input_error().is_some() {
                        break;
                    }
                }
            }
        }

        if st.err.tool_input_error().is_some() {
            break;
        }

        // finish_reason finalizes items; response.completed waits for the terminal [DONE] marker so
        // late usage-only chunks can still populate response.usage.
        let fr = choice.g("finish_reason");
        if fr.exists() && !fr.str().is_empty() {
            st.finish_reason = fr.str();
            finalize_open_items(st, &mut out);
        }

        if st.err.tool_input_error().is_some() {
            break;
        }
    }

    out
}

/// Builds a single Responses object from a complete Chat Completions response.
pub fn convert_openai_chat_completions_response_to_openai_responses_non_stream(
    _ctx: &Ctx,
    _model: &str,
    original: &[u8],
    translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Option<Vec<u8>> {
    let mut st = State::default();
    let body = convert_non_stream(&mut st, original, translated, raw);
    // Go stores the state in `*param`; keep it so a tool-input error reaches the registry.
    param.tool_input_error = st.err.tool_input_error().map(str::to_string);
    *param.state(State::default) = st;
    Some(body)
}

fn convert_non_stream(st: &mut State, original: &[u8], translated: &[u8], raw: &[u8]) -> Vec<u8> {
    let root = cpa_json::parse(raw);
    let request_for_namespace = pick_request_json(original, translated).unwrap_or_default();
    let tool_index = ToolIndex::new(&cpa_json::parse(request_for_namespace));

    let finish_reason = root.g("choices.0.finish_reason").str();
    let incomplete_details = incomplete_by_finish_reason(&finish_reason);
    let is_incomplete = incomplete_details.is_some();

    let resp_status = if is_incomplete { "incomplete" } else { "completed" };

    let mut resp = tpl(
        r#"{"id":"","object":"response","created_at":0,"status":"completed","background":false,"error":null,"incomplete_details":null}"#,
    );
    cpa_json::set(&mut resp, "status", resp_status);
    if let Some(details) = incomplete_details {
        cpa_json::set(&mut resp, "incomplete_details", tpl(details));
    }

    // id: provider id when present, otherwise synthesized.
    let mut id = root.g("id").str();
    if id.is_empty() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        id = format!("resp_{:x}_{}", nanos, RESPONSE_ID_COUNTER.fetch_add(1, Ordering::SeqCst) + 1);
    }
    cpa_json::set(&mut resp, "id", id.clone());

    // created_at: from chat.completion `created`.
    let mut created = root.g("created").int();
    if created == 0 {
        created = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    }
    cpa_json::set(&mut resp, "created_at", created);

    // Echo request fields when available (aligns with the streaming path).
    if !translated.is_empty() {
        let req = cpa_json::parse(translated);
        let v = req.g("instructions");
        if v.exists() {
            cpa_json::set(&mut resp, "instructions", v.str());
        }
        let v = req.g("max_output_tokens");
        if v.exists() {
            cpa_json::set(&mut resp, "max_output_tokens", v.int());
        } else {
            // Also support max_tokens from chat completion style.
            let v = req.g("max_tokens");
            if v.exists() {
                cpa_json::set(&mut resp, "max_output_tokens", v.int());
            }
        }
        let v = req.g("max_tool_calls");
        if v.exists() {
            cpa_json::set(&mut resp, "max_tool_calls", v.int());
        }
        let v = req.g("model");
        let rv = root.g("model");
        if v.exists() {
            cpa_json::set(&mut resp, "model", v.str());
        } else if rv.exists() {
            cpa_json::set(&mut resp, "model", rv.str());
        }
        // Remaining echoed fields (same order as Go from here on).
        let v = req.g("parallel_tool_calls");
        if v.exists() {
            cpa_json::set(&mut resp, "parallel_tool_calls", v.bool());
        }
        let v = req.g("previous_response_id");
        if v.exists() {
            cpa_json::set(&mut resp, "previous_response_id", v.str());
        }
        let v = req.g("prompt_cache_key");
        if v.exists() {
            cpa_json::set(&mut resp, "prompt_cache_key", v.str());
        }
        let v = req.g("reasoning");
        if v.exists() {
            cpa_json::set(&mut resp, "reasoning", go_any(v.value()));
        }
        let v = req.g("safety_identifier");
        if v.exists() {
            cpa_json::set(&mut resp, "safety_identifier", v.str());
        }
        let v = req.g("service_tier");
        if v.exists() {
            cpa_json::set(&mut resp, "service_tier", v.str());
        }
        let v = req.g("store");
        if v.exists() {
            cpa_json::set(&mut resp, "store", v.bool());
        }
        let v = req.g("temperature");
        if v.exists() {
            cpa_json::set(&mut resp, "temperature", cpa_json::num_f64(v.float()));
        }
        let v = req.g("text");
        if v.exists() {
            cpa_json::set(&mut resp, "text", go_any(v.value()));
        }
        let v = req.g("tool_choice");
        if v.exists() {
            cpa_json::set(&mut resp, "tool_choice", go_any(v.value()));
        }
        let v = req.g("tools");
        if v.exists() {
            cpa_json::set(&mut resp, "tools", go_any(v.value()));
        }
        let v = req.g("top_logprobs");
        if v.exists() {
            cpa_json::set(&mut resp, "top_logprobs", v.int());
        }
        let v = req.g("top_p");
        if v.exists() {
            cpa_json::set(&mut resp, "top_p", cpa_json::num_f64(v.float()));
        }
        let v = req.g("truncation");
        if v.exists() {
            cpa_json::set(&mut resp, "truncation", v.str());
        }
        let v = req.g("user");
        if v.exists() {
            cpa_json::set(&mut resp, "user", go_any(v.value()));
        }
        let v = req.g("metadata");
        if v.exists() {
            cpa_json::set(&mut resp, "metadata", go_any(v.value()));
        }
    } else {
        let v = root.g("model");
        if v.exists() {
            // Fallback model from the response.
            cpa_json::set(&mut resp, "model", v.str());
        }
    }

    // Output list from choices.
    let mut output_items: Vec<Value> = Vec::new();
    // Reasoning content, falling back to `reasoning`.
    let mut rc = root.g("choices.0.message.reasoning_content");
    if !rc.exists() || rc.str().is_empty() {
        rc = root.g("choices.0.message.reasoning");
    }
    let rc_text = rc.str();
    let mut include_reasoning = !rc_text.is_empty();
    if !include_reasoning && !translated.is_empty() {
        include_reasoning = cpa_json::parse(translated).g("reasoning").exists();
    }
    if include_reasoning {
        let rid = id.strip_prefix("resp_").unwrap_or(&id);
        let mut reasoning_item = tpl(r#"{"id":"","type":"reasoning","encrypted_content":"","summary":[]}"#);
        cpa_json::set(&mut reasoning_item, "id", format!("rs_{rid}"));
        if !rc_text.is_empty() {
            cpa_json::set(&mut reasoning_item, "summary.0.type", "summary_text");
            cpa_json::set(&mut reasoning_item, "summary.0.text", rc_text);
        }
        output_items.push(reasoning_item);
    }

    let choices = root.g("choices");
    if choices.is_array() {
        for choice in choices.array() {
            let msg = choice.g("message");
            if msg.exists() {
                // Text message part.
                let c = msg.g("content");
                if c.exists() && !c.str().is_empty() {
                    let item_status = if is_incomplete { "incomplete" } else { "completed" };
                    let mut item = tpl(
                        r#"{"id":"","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":""}],"role":"assistant"}"#,
                    );
                    cpa_json::set(&mut item, "id", format!("msg_{}_{}", id, choice.g("index").int()));
                    cpa_json::set(&mut item, "status", item_status);
                    cpa_json::set(&mut item, "content.0.text", c.str());
                    output_items.push(item);
                }

                // Function/tool calls.
                let tcs = msg.g("tool_calls");
                if tcs.is_array() {
                    for (tc_index, tc) in tcs.array().into_iter().enumerate() {
                        let mut call_id = tc.g("id").str();
                        if call_id.is_empty() {
                            // Providers may omit tool_call ids; synthesize one so the item stays
                            // usable for Codex round-trips.
                            call_id = format!("call_{}_{}_{}", id, choice.g("index").int(), tc_index);
                        }
                        let name = tool_index.canonical_name(&tc.g("function.name").str());
                        let args = tc.g("function.arguments").str();
                        let tool_status = if is_incomplete { "incomplete" } else { "completed" };
                        if tool_index.custom.contains(&name) {
                            let mut item = tpl(r#"{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}"#);
                            cpa_json::set(&mut item, "id", format!("ctc_{call_id}"));
                            cpa_json::set(&mut item, "status", tool_status);
                            let input = if tool_index.is_apply_patch(&name) {
                                let mut patch_call = ApplyPatchCallState::default();
                                match patch_call.finish_arguments(&args) {
                                    Ok((_, full_input)) => full_input,
                                    Err(err) => {
                                        st.err.set_tool_input_error(err);
                                        break;
                                    }
                                }
                            } else {
                                unwrap_custom_tool_input(&args)
                            };
                            cpa_json::set(&mut item, "input", input);
                            cpa_json::set(&mut item, "call_id", call_id);
                            let item = tool_index.apply_identity(&cpa_json::to_vec(&item), &name, "");
                            output_items.push(cpa_json::parse(&item));
                            continue;
                        }
                        let mut item = tpl(r#"{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}"#);
                        cpa_json::set(&mut item, "id", format!("fc_{call_id}"));
                        cpa_json::set(&mut item, "status", tool_status);
                        cpa_json::set(&mut item, "arguments", args);
                        cpa_json::set(&mut item, "call_id", call_id);
                        let item = tool_index.apply_identity(&cpa_json::to_vec(&item), &name, "");
                        output_items.push(cpa_json::parse(&item));
                    }
                }
            }
            if st.err.tool_input_error().is_some() {
                break;
            }
        }
    }
    if st.err.tool_input_error().is_some() {
        let failure = cpa_json::parse(&common::apply_patch_failure(&id, 0));
        return failure.g("response").raw().into_bytes();
    }
    if !output_items.is_empty() {
        cpa_json::set(&mut resp, "output", Value::Array(output_items));
    }

    // Usage mapping.
    let usage = root.g("usage");
    if usage.exists() {
        if usage.g("prompt_tokens").exists() || usage.g("completion_tokens").exists() || usage.g("total_tokens").exists() {
            cpa_json::set(&mut resp, "usage.input_tokens", usage.g("prompt_tokens").int());
            let d = usage.g("prompt_tokens_details.cached_tokens");
            if d.exists() {
                cpa_json::set(&mut resp, "usage.input_tokens_details.cached_tokens", d.int());
            }
            cpa_json::set(&mut resp, "usage.output_tokens", usage.g("completion_tokens").int());
            // Chat Completions has no reasoning token count; map it when present under output_tokens_details.
            let d = usage.g("output_tokens_details.reasoning_tokens");
            if d.exists() {
                cpa_json::set(&mut resp, "usage.output_tokens_details.reasoning_tokens", d.int());
            }
            cpa_json::set(&mut resp, "usage.total_tokens", usage.g("total_tokens").int());
        } else {
            // Fall back to the raw usage object if the structure differs.
            cpa_json::set(&mut resp, "usage", go_any(usage.value()));
        }
    }

    cpa_json::to_vec(&resp)
}

/// Rejects a patch-enabled stream that lacks its source terminator: when the request declares an
/// apply_patch custom tool and the stream ended before completion, returns the `response.failed`
/// frame (executors call this at stream EOF; Go: `FinalizeToolInput`).
pub fn finalize_tool_input(param: &mut Param) -> Vec<Vec<u8>> {
    let Some(st) = param.get::<State>() else { return vec![] };
    if st.err.tool_input_error().is_some() || st.completed_emitted {
        return vec![];
    }
    let enabled = st.tool_index.by_chat.keys().any(|name| st.tool_index.is_apply_patch(name));
    if !enabled {
        return vec![];
    }
    let message = "upstream apply_patch stream ended before protocol completion";
    st.err.set_tool_input_error(message);
    st.seq += 1;
    let frame = common::sse_event_data("response.failed", &common::apply_patch_failure(&st.response_id, st.seq));
    param.tool_input_error = Some(message.to_string());
    vec![frame]
}
