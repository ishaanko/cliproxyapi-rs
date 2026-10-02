//! Upstream Interactions response -> client OpenAI Responses response
//! (Go: interactions_openai_responses_response.go, first half).
//!
//! The stream converter carries the fail-closed `apply_patch` bridge: when the client request
//! declares the Codex custom `apply_patch` tool, matching `function_call` steps are re-emitted as
//! `response.custom_tool_call_input.*` events, and any identity, JSON or ordering conflict ends the
//! stream with `response.failed` and records a tool input error on the [`Param`].

use std::collections::{BTreeMap, BTreeSet, HashMap};

use cpa_core::signature::is_recognized_reasoning_signature;
use cpa_core::util::{self, ResponsesToolIdentity};
use cpa_core::applypatch;
use cpa_json::{Res, Value, J};

use super::raw_text::restore_step_arguments;
use super::request::{
    interactions_content_part_to_responses, interactions_content_texts, interactions_function_call_to_responses_with_identity,
};
use super::{
    first_existing, first_non_empty, is_antigravity_model, json_string_value, response_model, set_items, sse_payload, tmpl,
};
use crate::common::{self, ApplyPatchCallState};
use crate::registry::{Ctx, Param};

type Events = Vec<Vec<u8>>;
type IdentityMap = HashMap<String, ResponsesToolIdentity>;

/// Per-step `function_call` state (Go: `interactionsFunctionCallState`).
#[derive(Default)]
struct FunctionCallState {
    id: String,
    call_id: String,
    item_id_seen: bool,
    call_id_seen: bool,
    initial_arguments: String,
    raw_name: String,
    added: bool,
    patch_call: Option<ApplyPatchCallState>,
    pending_error: Option<String>,
    snapshot_arguments: String,
    snapshot_input: String,
    has_snapshot: bool,
    name: String,
    namespace: String,
    is_custom: bool,
    arguments: String,
    argument_fragments: Vec<String>,
    source_stopped: bool,
    stop_pending: bool,
    identity_finalized: bool,
    arguments_done_emitted: bool,
    item_done_emitted: bool,
}

/// Per-stream state (Go: `interactionsToResponsesStreamState`). Maps keyed by step index are
/// ordered so "lowest index wins" scans are deterministic.
#[derive(Default)]
struct StreamState {
    tool_input_error: Option<String>,
    id: String,
    environment_id: String,
    function_calls: BTreeMap<i64, FunctionCallState>,
    item_ids: HashMap<i64, String>,
    item_types: HashMap<i64, String>,
    reasoning_encrypted: HashMap<i64, String>,
    reasoning_summaries: HashMap<i64, Vec<String>>,
    text_outputs: HashMap<i64, String>,
    seq: i64,
    done: bool,
    terminal: bool,
    source_failed: bool,
    tool_identity_map: Option<IdentityMap>,
    pending_envelope_error: Option<String>,
    pending_identity_errors: HashMap<i64, String>,
    item_identity_indexes: HashMap<String, i64>,
    call_identity_indexes: HashMap<String, i64>,
    for_antigravity: bool,
}

fn is_patch(map: &Option<IdentityMap>, raw_name: &str) -> bool {
    map.as_ref().and_then(|m| m.get(raw_name)).is_some_and(|identity| identity.apply_patch)
}

impl StreamState {
    fn patch_for(&self, raw_name: &str) -> bool {
        is_patch(&self.tool_identity_map, raw_name)
    }

    /// True when the request declares the apply_patch custom tool.
    fn has_patch_bridge(&self) -> bool {
        self.tool_identity_map.as_ref().is_some_and(|m| m.values().any(|identity| identity.apply_patch))
    }

    fn item_id(&self, index: i64) -> String {
        self.item_ids.get(&index).cloned().unwrap_or_default()
    }
}

fn next_seq(seq: &mut i64) -> i64 {
    *seq += 1;
    *seq
}

fn emit(event: &str, payload: &Value) -> Vec<u8> {
    common::sse_event_data(event, &cpa_json::to_vec(payload))
}

fn index_root(index: i64) -> Value {
    let mut root = tmpl(r#"{"index":0}"#);
    cpa_json::set(&mut root, "index", index);
    root
}

// ---- entry points

/// Stream converter registered for (Interactions upstream -> OpenAI Responses client).
pub(super) fn convert_interactions_response_to_openai_responses(
    _ctx: &Ctx,
    model_name: &str,
    original: &[u8],
    translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let st = param.state(StreamState::default);
    st.for_antigravity = is_antigravity_model(model_name);
    if st.tool_identity_map.is_none() {
        let req = if original.is_empty() { translated } else { original };
        if !req.is_empty() {
            st.tool_identity_map = Some(tool_identity_map(req, st.for_antigravity));
        }
    }
    let out = convert_event(model_name, original, translated, raw, st);
    let err = st.tool_input_error.clone();
    param.tool_input_error = err;
    out
}

/// Executors call this at stream EOF: a patch-enabled stream that never reached its source
/// terminator fails closed with `response.failed` (Go: `FinalizeToolInput`).
pub fn finalize_tool_input(param: &mut Param) -> Vec<Vec<u8>> {
    let Some(st) = param.get::<StreamState>() else { return vec![] };
    if st.tool_input_error.is_some() || st.terminal || !st.has_patch_bridge() {
        return vec![];
    }
    st.tool_input_error = Some("upstream apply_patch stream ended before protocol completion".to_string());
    st.terminal = true;
    let seq = next_seq(&mut st.seq);
    let out = vec![emit_bytes("response.failed", &common::apply_patch_failure(&st.id, seq))];
    let err = st.tool_input_error.clone();
    param.tool_input_error = err;
    out
}

fn emit_bytes(event: &str, payload: &[u8]) -> Vec<u8> {
    common::sse_event_data(event, payload)
}

/// Non-stream converter registered for (Interactions upstream -> OpenAI Responses client).
pub(super) fn convert_interactions_response_to_openai_responses_non_stream(
    _ctx: &Ctx,
    model_name: &str,
    original: &[u8],
    translated: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Option<Vec<u8>> {
    let mut root = cpa_json::parse(raw);
    restore_step_arguments(raw, &mut root);
    let mut out = tmpl(r#"{"id":"","object":"response","status":"completed","model":"","output":[]}"#);
    cpa_json::set(&mut out, "id", first_non_empty(&[&root.g("id").str(), &root.g("interaction.id").str()]));
    cpa_json::set(&mut out, "model", response_model(model_name, &root));
    let mut steps = root.g("steps");
    if !steps.exists() {
        steps = root.g("interaction.steps");
    }
    let for_antigravity = is_antigravity_model(&response_model(model_name, &root));
    let req = if original.is_empty() { translated } else { original };
    let identity_map: Option<IdentityMap> = (!req.is_empty()).then(|| tool_identity_map(req, for_antigravity));
    let patch_enabled = identity_map.as_ref().is_some_and(|m| m.values().any(|identity| identity.apply_patch));
    let mut tool_input_error: Option<String> = None;
    let status = first_non_empty(&[&root.g("status").str(), &root.g("interaction.status").str()]);
    let source_error = first_existing([root.g("error"), root.g("interaction.error")]);
    if patch_enabled && (status == "failed" || (source_error.exists() && !source_error.is_null())) {
        tool_input_error = Some("upstream apply_patch interaction failed".to_string());
    }
    let raw_valid = cpa_json::valid(raw);
    let mut outputs: Vec<Value> = Vec::new();
    steps.for_each(|_, step| {
        if tool_input_error.is_some() {
            return false;
        }
        let is_call = step.g("type").str() == "function_call";
        let step_name = step.g("name").str();
        if patch_enabled && is_call && step_name.is_empty() {
            tool_input_error = Some("unresolved Interactions apply_patch call identity".to_string());
            return false;
        }
        if is_call && is_patch(&identity_map, &step_name) {
            if !raw_valid {
                tool_input_error = Some("invalid Interactions apply_patch response JSON".to_string());
                return false;
            }
            let mut patch_call = ApplyPatchCallState::default();
            if let Err(e) = patch_call.finish_arguments(&json_string_value(&step.g("arguments"), "{}")) {
                tool_input_error = Some(e);
                return false;
            }
        }
        outputs.extend(step_to_responses_output(&step, for_antigravity, identity_map.as_ref()));
        true
    });
    if let Some(err) = tool_input_error {
        param.tool_input_error = Some(err);
        return None;
    }
    set_items(&mut out, "output", outputs);
    let interaction_status = first_non_empty(&[&root.g("status").str(), &root.g("interaction.status").str()]);
    let finish_reason = first_non_empty(&[&root.g("finish_reason").str(), &root.g("interaction.finish_reason").str()]);
    if finish_reason == "content_filter" {
        cpa_json::set(&mut out, "status", "incomplete");
        cpa_json::set(&mut out, "incomplete_details.reason", "content_filter");
    } else if interaction_status == "incomplete" || finish_reason == "length" || finish_reason == "max_tokens" {
        cpa_json::set(&mut out, "status", "incomplete");
        cpa_json::set(&mut out, "incomplete_details.reason", "max_output_tokens");
    }
    let env_id = first_non_empty(&[
        &root.g("environment_id").str(),
        &root.g("interaction.environment_id").str(),
        &root.g("environment.id").str(),
        &root.g("interaction.environment.id").str(),
    ]);
    if !env_id.is_empty() {
        cpa_json::set(&mut out, "environment_id", env_id);
    }
    set_usage(&mut out, "usage", &common::interactions_usage(&Res::of(&root)));
    Some(cpa_json::to_vec(&out))
}

// ---- stream event dispatch

fn convert_event(model_name: &str, original: &[u8], translated: &[u8], raw: &[u8], st: &mut StreamState) -> Events {
    if st.done || st.tool_input_error.is_some() || st.source_failed {
        return vec![];
    }
    let payload = sse_payload(raw);
    if payload.is_empty() {
        return vec![];
    }
    let payload_valid = cpa_json::valid(&payload);
    // A source sentinel remains legal after response completion, but only once.
    let is_done = payload.trim_ascii() == b"[DONE]"
        || (payload_valid && cpa_json::parse(&payload).g("event_type").str() == "done");
    if is_done {
        let mut events = finish_patch_calls(st);
        if st.tool_input_error.is_some() {
            return events;
        }
        st.done = true;
        st.terminal = true;
        events.push(b"data: [DONE]".to_vec());
        return events;
    }
    if st.terminal {
        return vec![];
    }
    let mut root = cpa_json::parse(&payload);
    restore_step_arguments(&payload, &mut root);
    if root.is_null() {
        return vec![];
    }
    if !payload_valid {
        let err = "invalid Interactions apply_patch event JSON".to_string();
        let index = match resolve_step_index(&root.g("index"), &root.g("step"), 0, st) {
            Ok(index) => index,
            Err(e) => return patch_failure(st, e),
        };
        if root.g("event_type").str().starts_with("step.") {
            let step_id = root.g("step.id").str();
            let step_call_id = root.g("step.call_id").str();
            let call = st.function_calls.entry(index).or_insert_with(|| FunctionCallState {
                item_id_seen: !step_id.is_empty(),
                call_id_seen: !step_call_id.is_empty(),
                id: step_id,
                call_id: step_call_id,
                ..Default::default()
            });
            if call.pending_error.is_none() {
                call.pending_error = Some(err.clone());
            }
            let raw_name = call.raw_name.clone();
            if st.patch_for(&raw_name) || st.patch_for(&root.g("step.name").str()) {
                return patch_failure(st, err);
            }
        } else {
            let pending = st.pending_envelope_error.get_or_insert(err).clone();
            if st.function_calls.values().any(|call| call.patch_call.is_some()) {
                return patch_failure(st, pending);
            }
        }
    }
    match root.g("event_type").str().as_str() {
        "interaction.created" => vec![created_event(model_name, original, translated, &root, st)],
        "step.start" => step_start(&root, st),
        "step.delta" => step_delta(&root, st),
        "step.stop" => step_stop(&root, st),
        "interaction.completed" | "finish" => completed(model_name, &root, st),
        "response.failed" | "interaction.failed" => {
            if st.has_patch_bridge() {
                return patch_failure(st, "upstream apply_patch interaction failed".to_string());
            }
            st.source_failed = true;
            st.terminal = true;
            vec![failed_event(model_name, &root, st)]
        }
        _ => vec![],
    }
}

/// `interaction.completed` / `finish`: reconcile the final step snapshot, then emit the terminal event.
fn completed(model_name: &str, root: &Value, st: &mut StreamState) -> Events {
    let mut events: Events = Vec::new();
    let steps = first_existing([root.g("interaction.steps"), root.g("steps")]);
    let patch_enabled = st.has_patch_bridge();
    steps.for_each(|key, step| {
        let index = match resolve_step_index(&step.g("index"), &step, key.int(), st) {
            Ok(index) => index,
            Err(e) => {
                events.extend(patch_failure(st, e));
                return false;
            }
        };
        let step_name = step.g("name").str();
        let skip = match st.function_calls.get(&index) {
            // Patch-enabled unnamed functions retain evidence before final snapshot filtering.
            Some(call) if call.patch_call.is_some() || st.patch_for(&call.raw_name) || (patch_enabled && call.raw_name.is_empty()) => false,
            call => {
                if step.g("type").str() != "function_call" {
                    true
                } else {
                    let unresolved = patch_enabled && (step_name.is_empty() || call.is_some_and(|c| c.raw_name.is_empty()));
                    !st.patch_for(&step_name) && !unresolved
                }
            }
        };
        if skip {
            return true;
        }
        events.extend(update_function_call(index, &step, st, false));
        !st.terminal
    });
    if st.terminal {
        return events;
    }
    events.extend(finish_patch_calls(st));
    if st.terminal {
        return events;
    }
    st.terminal = true;
    events.push(completed_event(model_name, root, st));
    events
}

// ---- step events

fn created_event(model_name: &str, original: &[u8], translated: &[u8], root: &Value, st: &mut StreamState) -> Vec<u8> {
    let mut payload =
        tmpl(r#"{"type":"response.created","response":{"id":"","object":"response","status":"in_progress","model":"","output":[]}}"#);
    cpa_json::set(&mut payload, "sequence_number", next_seq(&mut st.seq));
    let id = first_non_empty(&[&root.g("interaction.id").str(), &root.g("id").str()]);
    if !id.is_empty() {
        st.id = id.clone();
    }
    cpa_json::set(&mut payload, "response.id", id);
    cpa_json::set(&mut payload, "response.model", model_name);
    let env_id = first_non_empty(&[
        &root.g("interaction.environment_id").str(),
        &root.g("environment_id").str(),
        &root.g("environment.id").str(),
        &root.g("interaction.environment.id").str(),
    ]);
    if !env_id.is_empty() {
        st.environment_id = env_id.clone();
        cpa_json::set(&mut payload, "response.environment_id", env_id);
    }
    let mut request_model_name = common::request_model_name(original, translated);
    if request_model_name.is_empty() {
        request_model_name = model_name.to_string();
    }
    if !request_model_name.is_empty() {
        cpa_json::set(&mut payload, "response.model", request_model_name);
    }
    emit("response.created", &payload)
}

fn step_start(root: &Value, st: &mut StreamState) -> Events {
    let step = root.g("step");
    let index = match resolve_step_index(&root.g("index"), &step, 0, st) {
        Ok(index) => index,
        Err(e) => return patch_failure(st, e),
    };
    let step_type = step.g("type").str();
    // A repeated start must not overwrite a possible patch call's type evidence.
    if let Some(call) = st.function_calls.get(&index)
        && (call.patch_call.is_some() || st.patch_for(&call.raw_name) || (call.raw_name.is_empty() && st.has_patch_bridge()))
    {
        return update_function_call(index, &step, st, true);
    }
    let item_id = first_non_empty(&[&step.g("id").str(), &step.g("call_id").str(), &format!("item_{index}")]);
    if step_type == "function_call" {
        return update_function_call(index, &step, st, true);
    }
    st.item_ids.insert(index, item_id.clone());
    st.item_types.insert(index, step_type.clone());
    match step_type.as_str() {
        "model_output" => {
            let mut added = tmpl(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"","type":"message","status":"in_progress","role":"assistant","content":[]}}"#,
            );
            cpa_json::set(&mut added, "sequence_number", next_seq(&mut st.seq));
            cpa_json::set(&mut added, "output_index", index);
            cpa_json::set(&mut added, "item.id", item_id.as_str());
            let mut part = tmpl(
                r#"{"type":"response.content_part.added","output_index":0,"content_index":0,"item_id":"","part":{"type":"output_text","text":""}}"#,
            );
            cpa_json::set(&mut part, "sequence_number", next_seq(&mut st.seq));
            cpa_json::set(&mut part, "output_index", index);
            cpa_json::set(&mut part, "item_id", item_id);
            vec![emit("response.output_item.added", &added), emit("response.content_part.added", &part)]
        }
        "thought" => {
            let mut added = tmpl(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"","type":"reasoning","status":"in_progress","encrypted_content":"","summary":[]}}"#,
            );
            cpa_json::set(&mut added, "sequence_number", next_seq(&mut st.seq));
            cpa_json::set(&mut added, "output_index", index);
            cpa_json::set(&mut added, "item.id", item_id);
            let signature = reasoning_encrypted_content(st.reasoning_encrypted.get(&index).map(String::as_str).unwrap_or_default());
            if !signature.is_empty() {
                cpa_json::set(&mut added, "item.encrypted_content", signature);
            }
            vec![emit("response.output_item.added", &added)]
        }
        _ => vec![],
    }
}

fn step_delta(root: &Value, st: &mut StreamState) -> Events {
    let index = match resolve_step_index(&root.g("index"), &root.g("step"), 0, st) {
        Ok(index) => index,
        Err(e) => return patch_failure(st, e),
    };
    // Check the source barrier before a same-event snapshot or identity update can replay
    // fragments and publish completion. Unknown names retain the violation.
    let delta_is_args = root.g("delta.type").str() == "arguments_delta" && !root.g("delta.arguments").str().is_empty();
    if let Some(call) = st.function_calls.get(&index)
        && call.source_stopped
        && delta_is_args
    {
        let err = "apply_patch delta after source stop".to_string();
        if call.patch_call.is_some() || st.patch_for(&call.raw_name) || st.patch_for(&root.g("step.name").str()) {
            return patch_failure(st, err);
        }
        let retain = call.raw_name.is_empty() && st.has_patch_bridge() && call.pending_error.is_none();
        if retain && let Some(call) = st.function_calls.get_mut(&index) {
            call.pending_error = Some(err);
        }
    }
    let step = root.g("step");
    if step.is_object() {
        let mut events = update_function_call(index, &step, st, false);
        if st.terminal {
            return events;
        }
        // Process the same real delta after its late identity update, without replaying the
        // update or turning its snapshot into parameter fragments.
        let mut stripped = root.clone();
        cpa_json::delete(&mut stripped, "step");
        cpa_json::set(&mut stripped, "index", index);
        events.extend(step_delta(&stripped, st));
        return events;
    }
    let delta = root.g("delta");
    match delta.g("type").str().as_str() {
        "thought_summary" => {
            let text = first_non_empty(&[&delta.g("content.text").str(), &delta.g("text").str()]);
            if !text.is_empty() {
                st.reasoning_summaries.entry(index).or_default().push(text.clone());
            }
            let mut payload = tmpl(r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":""}"#);
            cpa_json::set(&mut payload, "sequence_number", next_seq(&mut st.seq));
            cpa_json::set(&mut payload, "output_index", index);
            cpa_json::set(&mut payload, "delta", text);
            vec![emit("response.reasoning_summary_text.delta", &payload)]
        }
        "thought_signature" => {
            let signature = reasoning_encrypted_content(&delta.g("signature").str());
            if !signature.is_empty() {
                st.reasoning_encrypted.insert(index, signature);
            }
            vec![]
        }
        "arguments_delta" => arguments_delta(index, &delta, st),
        _ => {
            let mut payload =
                tmpl(r#"{"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"","delta":""}"#);
            cpa_json::set(&mut payload, "sequence_number", next_seq(&mut st.seq));
            cpa_json::set(&mut payload, "output_index", index);
            cpa_json::set(&mut payload, "item_id", st.item_id(index));
            let text = delta.g("text").str();
            if !text.is_empty() {
                st.text_outputs.entry(index).or_default().push_str(&text);
            }
            cpa_json::set(&mut payload, "delta", text);
            vec![emit("response.output_text.delta", &payload)]
        }
    }
}

fn arguments_delta(index: i64, delta: &Res<'_>, st: &mut StreamState) -> Events {
    let arguments = delta.g("arguments").str();
    let call = st.function_calls.entry(index).or_default();
    if delta.g("invalid_json_str").exists() {
        let err = "legacy freeform arguments are invalid for apply_patch".to_string();
        call.pending_error = Some(err.clone());
        let raw_name = call.raw_name.clone();
        if call.patch_call.is_some() || st.patch_for(&raw_name) {
            return patch_failure(st, err);
        }
    }
    let Some(call) = st.function_calls.get_mut(&index) else { return vec![] };
    if call.patch_call.is_some() {
        if call.item_done_emitted && !arguments.is_empty() {
            return patch_failure(st, "apply_patch delta after item completion".to_string());
        }
        call.arguments.push_str(&arguments);
        let pushed = call.patch_call.as_mut().map(|patch| patch.push_arguments(&arguments));
        return match pushed {
            Some(Ok(delta)) => match st.function_calls.get(&index).and_then(|c| c.patch_call.as_ref()) {
                Some(patch) => patch_delta(&mut st.seq, patch, &delta),
                None => vec![],
            },
            Some(Err(e)) => patch_failure(st, e),
            None => vec![],
        };
    }
    if call.item_done_emitted {
        return vec![];
    }
    call.arguments.push_str(&arguments);
    if !call.source_stopped && (call.raw_name.is_empty() || is_patch(&st.tool_identity_map, &call.raw_name)) {
        call.argument_fragments.push(arguments.clone());
    }
    if call.raw_name.is_empty() || call.is_custom {
        return vec![];
    }
    let item_id = st.item_id(index);
    vec![function_call_arguments_delta(index, &item_id, &arguments, &mut st.seq)]
}

fn function_call_arguments_delta(index: i64, item_id: &str, arguments: &str, seq: &mut i64) -> Vec<u8> {
    let mut payload = tmpl(r#"{"type":"response.function_call_arguments.delta","output_index":0,"item_id":"","delta":""}"#);
    cpa_json::set(&mut payload, "sequence_number", next_seq(seq));
    cpa_json::set(&mut payload, "output_index", index);
    cpa_json::set(&mut payload, "item_id", item_id);
    cpa_json::set(&mut payload, "delta", arguments);
    emit("response.function_call_arguments.delta", &payload)
}

fn function_call_arguments_done(index: i64, item_id: &str, arguments: &str, seq: &mut i64) -> Vec<u8> {
    let mut payload = tmpl(r#"{"type":"response.function_call_arguments.done","output_index":0,"item_id":"","arguments":""}"#);
    cpa_json::set(&mut payload, "sequence_number", next_seq(seq));
    cpa_json::set(&mut payload, "output_index", index);
    cpa_json::set(&mut payload, "item_id", item_id);
    cpa_json::set(&mut payload, "arguments", arguments);
    emit("response.function_call_arguments.done", &payload)
}

fn custom_tool_call_input_done(index: i64, item_id: &str, input: &str, seq: &mut i64) -> Vec<u8> {
    let mut payload = tmpl(r#"{"type":"response.custom_tool_call_input.done","output_index":0,"item_id":"","input":""}"#);
    cpa_json::set(&mut payload, "sequence_number", next_seq(seq));
    cpa_json::set(&mut payload, "output_index", index);
    cpa_json::set(&mut payload, "item_id", item_id);
    cpa_json::set(&mut payload, "input", input);
    emit("response.custom_tool_call_input.done", &payload)
}

fn step_stop(root: &Value, st: &mut StreamState) -> Events {
    let index = match resolve_step_index(&root.g("index"), &root.g("step"), 0, st) {
        Ok(index) => index,
        Err(e) => return patch_failure(st, e),
    };
    // Source completion is independent of downstream identity and publication, including
    // candidates established by arguments before their first start.
    if let Some(call) = st.function_calls.get_mut(&index) {
        call.source_stopped = true;
        if call.raw_name.is_empty() {
            call.stop_pending = true;
        }
    }
    let mut updates: Events = Vec::new();
    let step = root.g("step");
    if st.item_types.get(&index).map(String::as_str) == Some("function_call") && step.is_object() {
        updates = update_function_call(index, &step, st, false);
        if st.terminal {
            return updates;
        }
    }
    let item_id = st.item_id(index);
    let item_type = st.item_types.get(&index).cloned().unwrap_or_default();
    match item_type.as_str() {
        "model_output" => {
            let text = st.text_outputs.get(&index).cloned().unwrap_or_default();
            let mut text_done = tmpl(
                r#"{"type":"response.output_text.done","output_index":0,"content_index":0,"item_id":"","text":"","logprobs":[]}"#,
            );
            cpa_json::set(&mut text_done, "sequence_number", next_seq(&mut st.seq));
            cpa_json::set(&mut text_done, "output_index", index);
            cpa_json::set(&mut text_done, "item_id", item_id.as_str());
            cpa_json::set(&mut text_done, "text", text.as_str());
            let mut part = tmpl(
                r#"{"type":"response.content_part.done","output_index":0,"content_index":0,"item_id":"","part":{"type":"output_text","text":""}}"#,
            );
            cpa_json::set(&mut part, "sequence_number", next_seq(&mut st.seq));
            cpa_json::set(&mut part, "output_index", index);
            cpa_json::set(&mut part, "item_id", item_id.as_str());
            cpa_json::set(&mut part, "part.text", text.as_str());
            let mut done = tmpl(
                r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"","type":"message","status":"completed","role":"assistant","content":[]}}"#,
            );
            cpa_json::set(&mut done, "sequence_number", next_seq(&mut st.seq));
            cpa_json::set(&mut done, "output_index", index);
            cpa_json::set(&mut done, "item.id", item_id);
            let mut output_text = tmpl(r#"{"type":"output_text","text":""}"#);
            cpa_json::set(&mut output_text, "text", text);
            cpa_json::set(&mut done, "item.content.-1", output_text);
            vec![
                emit("response.output_text.done", &text_done),
                emit("response.content_part.done", &part),
                emit("response.output_item.done", &done),
            ]
        }
        "function_call" => function_call_stop(index, &item_id, st, updates),
        _ => {
            let mut done = tmpl(r#"{"type":"response.output_item.done","output_index":0,"item":{}}"#);
            cpa_json::set(&mut done, "sequence_number", next_seq(&mut st.seq));
            cpa_json::set(&mut done, "output_index", index);
            cpa_json::set(&mut done, "item", reasoning_item(index, st));
            vec![emit("response.output_item.done", &done)]
        }
    }
}

/// The `function_call` arm of [`step_stop`]: publishes the done events for a finished call.
fn function_call_stop(index: i64, item_id: &str, st: &mut StreamState, updates: Events) -> Events {
    let call = st.function_calls.entry(index).or_insert_with(|| FunctionCallState {
        id: item_id.to_string(),
        source_stopped: true,
        ..Default::default()
    });
    if call.raw_name.is_empty() {
        call.stop_pending = true;
        return updates;
    }
    if is_patch(&st.tool_identity_map, &call.raw_name) && call.patch_call.is_none() {
        call.stop_pending = true;
        return updates;
    }
    if call.item_done_emitted {
        return updates;
    }
    let mut events = updates;
    if call.patch_call.is_some() {
        let (tail, input) = match prepare_patch_finish(call) {
            Ok(v) => v,
            Err(e) => return patch_failure(st, e),
        };
        if let Some(patch) = call.patch_call.as_ref() {
            events.extend(patch_delta(&mut st.seq, patch, &tail));
            let seq = next_seq(&mut st.seq);
            events.push(emit_bytes("response.custom_tool_call_input.done", &common::apply_patch_input_done(patch, &input, seq)));
        }
        let item = completed_output_item(index, "function_call", st).unwrap_or(Value::Null);
        let mut done = tmpl(r#"{"type":"response.output_item.done"}"#);
        cpa_json::set(&mut done, "sequence_number", next_seq(&mut st.seq));
        cpa_json::set(&mut done, "output_index", index);
        cpa_json::set(&mut done, "item", item);
        if let Some(call) = st.function_calls.get_mut(&index) {
            call.item_done_emitted = true;
            call.arguments_done_emitted = true;
        }
        events.push(emit("response.output_item.done", &done));
        return events;
    }
    if call.is_custom {
        let input = util::unwrap_responses_custom_tool_input(&call.arguments);
        if !call.arguments_done_emitted {
            events.push(custom_tool_call_input_done(index, item_id, &input, &mut st.seq));
            call.arguments_done_emitted = true;
        }
        let mut done = tmpl(
            r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"","type":"custom_tool_call","call_id":"","name":"","input":"","status":"completed"}}"#,
        );
        cpa_json::set(&mut done, "sequence_number", next_seq(&mut st.seq));
        cpa_json::set(&mut done, "output_index", index);
        cpa_json::set(&mut done, "item.id", item_id);
        cpa_json::set(&mut done, "item.call_id", call.call_id.as_str());
        if !call.namespace.is_empty() {
            cpa_json::set(&mut done, "item.namespace", call.namespace.as_str());
        }
        cpa_json::set(&mut done, "item.name", call.name.as_str());
        cpa_json::set(&mut done, "item.input", input);
        call.item_done_emitted = true;
        events.push(emit("response.output_item.done", &done));
        return events;
    }
    let arguments = function_call_arguments(call);
    if !call.arguments_done_emitted {
        events.push(function_call_arguments_done(index, item_id, &arguments, &mut st.seq));
        call.arguments_done_emitted = true;
    }
    let mut done = tmpl(
        r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"","type":"function_call","call_id":"","name":"","arguments":"","status":"completed"}}"#,
    );
    cpa_json::set(&mut done, "sequence_number", next_seq(&mut st.seq));
    cpa_json::set(&mut done, "output_index", index);
    cpa_json::set(&mut done, "item.id", item_id);
    cpa_json::set(&mut done, "item.call_id", call.call_id.as_str());
    if !call.namespace.is_empty() {
        cpa_json::set(&mut done, "item.namespace", call.namespace.as_str());
    }
    cpa_json::set(&mut done, "item.name", call.name.as_str());
    cpa_json::set(&mut done, "item.arguments", arguments);
    call.item_done_emitted = true;
    events.push(emit("response.output_item.done", &done));
    events
}

/// Validates the source arguments against the snapshot and finishes the patch decoder: the unsent
/// tail and the full decoded input.
fn prepare_patch_finish(call: &mut FunctionCallState) -> Result<(String, String), String> {
    let args = if call.has_snapshot { call.snapshot_arguments.clone() } else { call.arguments.clone() };
    if call.has_snapshot && cpa_json::valid(call.arguments.as_bytes()) {
        let mut source = ApplyPatchCallState::default();
        let (_, input) = source.finish_arguments(&call.arguments)?;
        if input != call.snapshot_input {
            return Err("apply_patch complete source conflicts with snapshot".to_string());
        }
    }
    match call.patch_call.as_mut() {
        Some(patch) => patch.finish_arguments(&args),
        None => Err("apply_patch call state missing".to_string()),
    }
}

// ---- terminal events

fn completed_event(model_name: &str, root: &Value, st: &mut StreamState) -> Vec<u8> {
    let mut event_type = "response.completed";
    let mut status = "completed";
    let interaction = root.g("interaction");
    let interaction_status = first_non_empty(&[&interaction.g("status").str(), &root.g("status").str()]);
    let finish_reason = first_non_empty(&[&interaction.g("finish_reason").str(), &root.g("finish_reason").str()]);
    let mut incomplete_reason = "";
    if finish_reason == "content_filter" {
        event_type = "response.incomplete";
        status = "incomplete";
        incomplete_reason = "content_filter";
    } else if interaction_status == "incomplete" || finish_reason == "length" || finish_reason == "max_tokens" {
        event_type = "response.incomplete";
        status = "incomplete";
        incomplete_reason = "max_output_tokens";
    }
    let mut payload = tmpl(
        r#"{"type":"response.completed","response":{"id":"","object":"response","status":"completed","model":"","output":[],"usage":{}}}"#,
    );
    cpa_json::set(&mut payload, "type", event_type);
    cpa_json::set(&mut payload, "response.status", status);
    if !incomplete_reason.is_empty() {
        cpa_json::set(&mut payload, "response.incomplete_details.reason", incomplete_reason);
    }
    cpa_json::set(&mut payload, "sequence_number", next_seq(&mut st.seq));
    cpa_json::set(&mut payload, "response.id", first_non_empty(&[&interaction.g("id").str(), &root.g("id").str()]));
    cpa_json::set(&mut payload, "response.model", first_non_empty(&[&interaction.g("model").str(), model_name]));
    let mut env_id = first_non_empty(&[
        &interaction.g("environment_id").str(),
        &root.g("environment_id").str(),
        &interaction.g("environment.id").str(),
        &root.g("environment.id").str(),
    ]);
    if env_id.is_empty() {
        env_id = st.environment_id.clone();
    }
    if !env_id.is_empty() {
        cpa_json::set(&mut payload, "response.environment_id", env_id);
    }
    set_completed_output(&mut payload, st);
    set_usage(&mut payload, "response.usage", &common::interactions_usage(&Res::of(root)));
    emit(event_type, &payload)
}

fn failed_event(model_name: &str, root: &Value, st: &mut StreamState) -> Vec<u8> {
    let mut payload = tmpl(
        r#"{"type":"response.failed","response":{"id":"","object":"response","status":"failed","model":"","output":[],"error":{"message":"","code":"","type":"server_error"}}}"#,
    );
    cpa_json::set(&mut payload, "sequence_number", next_seq(&mut st.seq));
    let interaction = root.g("interaction");
    let mut id = first_non_empty(&[&interaction.g("id").str(), &root.g("id").str()]);
    if id.is_empty() {
        id = st.id.clone();
    }
    cpa_json::set(&mut payload, "response.id", id);
    cpa_json::set(&mut payload, "response.model", first_non_empty(&[&interaction.g("model").str(), model_name]));
    let mut err_node = root.g("error");
    if !err_node.exists() && interaction.exists() {
        err_node = interaction.g("error");
    }
    let mut msg = err_node.g("message").str();
    if msg.is_empty() {
        msg = "upstream execution failed".to_string();
    }
    let code = err_node.g("code").str();
    cpa_json::set(&mut payload, "response.error.message", msg);
    if code.is_empty() {
        cpa_json::delete(&mut payload, "response.error.code");
    } else {
        cpa_json::set(&mut payload, "response.error.code", code);
    }
    let mut err_type = err_node.g("type").str();
    if err_type.is_empty() {
        err_type = "server_error".to_string();
    }
    cpa_json::set(&mut payload, "response.error.type", err_type);
    emit("response.failed", &payload)
}

// ---- reasoning signatures and output assembly

fn thought_signature(step: &Res<'_>) -> String {
    for path in ["encrypted_content", "signature", "thought_signature", "thoughtSignature", "extra_content.google.thought_signature"] {
        let signature = reasoning_encrypted_content(&step.g(path).str());
        if !signature.is_empty() {
            return signature;
        }
    }
    let content = step.g("content");
    if content.is_array() {
        for part in content.array() {
            let candidate = first_non_empty(&[
                &part.g("signature").str(),
                &part.g("thought_signature").str(),
                &part.g("thoughtSignature").str(),
                &part.g("extra_content.google.thought_signature").str(),
            ]);
            let valid = reasoning_encrypted_content(&candidate);
            if !valid.is_empty() {
                return valid;
            }
        }
    }
    String::new()
}

/// The trimmed signature when it is a recognized reasoning signature, else "".
fn reasoning_encrypted_content(raw_signature: &str) -> String {
    let candidate = raw_signature.trim();
    if candidate.is_empty() || !is_recognized_reasoning_signature(candidate) {
        return String::new();
    }
    candidate.to_string()
}

fn set_completed_output(payload: &mut Value, st: &StreamState) {
    let Some(max_index) = st.item_types.keys().max().copied() else { return };
    let mut output_items: Vec<Value> = Vec::new();
    for index in 0..=max_index {
        if let Some(item_type) = st.item_types.get(&index) {
            output_items.extend(completed_output_item(index, item_type, st));
        }
    }
    set_items(payload, "response.output", output_items);
}

fn function_call_arguments(call: &FunctionCallState) -> String {
    if call.arguments.is_empty() { "{}".to_string() } else { call.arguments.clone() }
}

fn completed_output_item(index: i64, item_type: &str, st: &StreamState) -> Option<Value> {
    match item_type {
        "model_output" => {
            let mut item = tmpl(r#"{"id":"","type":"message","status":"completed","role":"assistant","content":[]}"#);
            cpa_json::set(&mut item, "id", st.item_id(index));
            if let Some(text) = st.text_outputs.get(&index).filter(|t| !t.is_empty()) {
                let mut part = tmpl(r#"{"type":"output_text","text":""}"#);
                cpa_json::set(&mut part, "text", text.as_str());
                set_items(&mut item, "content", vec![part]);
            }
            Some(item)
        }
        "thought" => Some(reasoning_item(index, st)),
        "function_call" => {
            let call = st.function_calls.get(&index);
            let item_id = st.item_id(index);
            if let Some(call) = call.filter(|c| c.is_custom) {
                let mut item = tmpl(r#"{"id":"","type":"custom_tool_call","call_id":"","name":"","input":"","status":"completed"}"#);
                cpa_json::set(&mut item, "id", item_id);
                cpa_json::set(&mut item, "call_id", call.call_id.as_str());
                if !call.namespace.is_empty() {
                    cpa_json::set(&mut item, "namespace", call.namespace.as_str());
                }
                cpa_json::set(&mut item, "name", call.name.as_str());
                let mut input = util::unwrap_responses_custom_tool_input(&function_call_arguments(call));
                if let Some(patch) = &call.patch_call {
                    input = patch.decoder.input().to_string();
                }
                cpa_json::set(&mut item, "input", input);
                return Some(item);
            }
            let mut item = tmpl(r#"{"id":"","type":"function_call","call_id":"","name":"","arguments":"{}","status":"completed"}"#);
            cpa_json::set(&mut item, "id", item_id.as_str());
            cpa_json::set(&mut item, "call_id", item_id);
            if let Some(call) = call {
                cpa_json::set(&mut item, "call_id", call.call_id.as_str());
                if !call.namespace.is_empty() {
                    cpa_json::set(&mut item, "namespace", call.namespace.as_str());
                }
                cpa_json::set(&mut item, "name", call.name.as_str());
                cpa_json::set(&mut item, "arguments", function_call_arguments(call));
            }
            Some(item)
        }
        _ => None,
    }
}

fn reasoning_item(index: i64, st: &StreamState) -> Value {
    let mut item = tmpl(r#"{"id":"","type":"reasoning","encrypted_content":"","summary":[]}"#);
    cpa_json::set(&mut item, "id", st.item_id(index));
    let signature = reasoning_encrypted_content(st.reasoning_encrypted.get(&index).map(String::as_str).unwrap_or_default());
    if !signature.is_empty() {
        cpa_json::set(&mut item, "encrypted_content", signature);
    }
    let summaries: Vec<Value> = st
        .reasoning_summaries
        .get(&index)
        .map(|texts| {
            texts
                .iter()
                .map(|text| {
                    let mut part = tmpl(r#"{"type":"summary_text","text":""}"#);
                    cpa_json::set(&mut part, "text", text.as_str());
                    part
                })
                .collect()
        })
        .unwrap_or_default();
    set_items(&mut item, "summary", summaries);
    item
}

/// Responses usage from Interactions usage; token counts default to 0 and the total to the sum.
fn set_usage(out: &mut Value, path: &str, usage: &Res<'_>) {
    let first_int = |paths: &[&str]| paths.iter().map(|p| usage.g(p)).find(Res::exists).map(|v| v.int());
    let (mut input, mut output, mut total) = (0, 0, 0);
    if usage.exists() {
        input = first_int(&["input_tokens", "total_input_tokens"]).unwrap_or(0);
        output = first_int(&["output_tokens", "total_output_tokens"]).unwrap_or(0);
        total = first_int(&["total_tokens"]).unwrap_or(input + output);
    }
    cpa_json::set(out, &format!("{path}.input_tokens"), input);
    cpa_json::set(out, &format!("{path}.output_tokens"), output);
    cpa_json::set(out, &format!("{path}.total_tokens"), total);
    if usage.exists() {
        if let Some(v) = first_int(&["cached_tokens", "total_cached_tokens"]) {
            cpa_json::set(out, &format!("{path}.input_tokens_details.cached_tokens"), v);
        }
        if let Some(v) = first_int(&["reasoning_tokens", "total_thought_tokens"]) {
            cpa_json::set(out, &format!("{path}.output_tokens_details.reasoning_tokens"), v);
        }
    }
}

/// An Interactions step as a non-stream Responses output item.
fn step_to_responses_output(step: &Res<'_>, for_antigravity: bool, identity_map: Option<&IdentityMap>) -> Option<Value> {
    match step.g("type").str().as_str() {
        "model_output" => {
            let mut item = tmpl(r#"{"type":"message","role":"assistant","content":[]}"#);
            let id = first_non_empty(&[&step.g("id").str(), &step.g("step_id").str()]);
            if !id.is_empty() {
                cpa_json::set(&mut item, "id", id);
            }
            let content = step.g("content");
            if let Some(text) = content.as_str() {
                let mut part = tmpl(r#"{"type":"output_text","text":""}"#);
                cpa_json::set(&mut part, "text", text);
                set_items(&mut item, "content", vec![part]);
            } else {
                let mut parts: Vec<Value> = Vec::new();
                content.for_each(|_, part| {
                    parts.extend(interactions_content_part_to_responses(&part, "assistant"));
                    true
                });
                set_items(&mut item, "content", parts);
            }
            Some(item)
        }
        "thought" => {
            let mut item = tmpl(r#"{"type":"reasoning","summary":[]}"#);
            let signature = thought_signature(step);
            if !signature.is_empty() {
                cpa_json::set(&mut item, "encrypted_content", signature);
            }
            let summaries: Vec<Value> = interactions_content_texts(&step.g("content"))
                .iter()
                .map(|text| {
                    let mut part = tmpl(r#"{"type":"summary_text","text":""}"#);
                    cpa_json::set(&mut part, "text", text.as_str());
                    part
                })
                .collect();
            set_items(&mut item, "summary", summaries);
            Some(item)
        }
        "function_call" => {
            let mut item = interactions_function_call_to_responses_with_identity(step, for_antigravity, identity_map);
            cpa_json::set(&mut item, "status", "completed");
            Some(item)
        }
        _ => None,
    }
}

// ---- apply_patch bridge helpers

/// Winning tool identities of the original request, keyed by qualified name (Interactions uses
/// qualified names directly), plus Antigravity upstream-name aliases.
fn tool_identity_map(raw_json: &[u8], for_antigravity: bool) -> IdentityMap {
    let parsed = cpa_json::parse(raw_json);
    let root = parsed.get("request").unwrap_or(&parsed);
    let winners = util::collect_responses_tool_winners(root);
    let identity_of = |descriptor: &util::ResponsesToolDescriptor| ResponsesToolIdentity {
        name: descriptor.local_name.clone(),
        namespace: descriptor.namespace.clone(),
        custom: descriptor.tool_type == "custom",
        apply_patch: applypatch::is_custom_tool(&descriptor.tool),
    };
    let mut identities: IdentityMap = winners.iter().map(|(name, d)| (name.clone(), identity_of(d))).collect();
    if for_antigravity {
        let mut names: Vec<&String> = winners.keys().collect();
        names.sort();
        for name in names {
            let upstream_name = common::antigravity_tool_name_to_upstream(name);
            identities.entry(upstream_name).or_insert_with(|| identity_of(&winners[name]));
        }
    }
    identities
}

/// Ends the stream with `response.failed` and records the tool input error (once).
fn patch_failure(st: &mut StreamState, err: String) -> Events {
    if st.terminal {
        return vec![];
    }
    st.tool_input_error = Some(err);
    st.terminal = true;
    let seq = next_seq(&mut st.seq);
    vec![emit_bytes("response.failed", &common::apply_patch_failure(&st.id, seq))]
}

fn patch_delta(seq: &mut i64, patch: &ApplyPatchCallState, delta: &str) -> Events {
    if delta.is_empty() {
        return vec![];
    }
    let seq = next_seq(seq);
    vec![emit_bytes("response.custom_tool_call_input.delta", &common::apply_patch_input_delta(patch, delta, seq))]
}

/// Reconciles every supplied index and ID before snapshot types are inspected. Returns the step
/// index to use, or the pending identity conflict error for patch-related steps.
fn resolve_step_index(explicit_index: &Res<'_>, step: &Res<'_>, fallback: i64, st: &mut StreamState) -> Result<i64, String> {
    let step_index = step.g("index");
    let indexed = explicit_index.exists() || step_index.exists();
    let mut index = fallback;
    if explicit_index.exists() {
        index = explicit_index.int();
    } else if step_index.exists() {
        index = step_index.int();
    }
    let item_id = step.g("id").str();
    let call_id = step.g("call_id").str();
    let mut matched: BTreeSet<i64> = BTreeSet::new();
    if !item_id.is_empty()
        && let Some(&call_index) = st.item_identity_indexes.get(&item_id)
    {
        matched.insert(call_index);
    }
    if !call_id.is_empty()
        && let Some(&call_index) = st.call_identity_indexes.get(&call_id)
    {
        matched.insert(call_index);
    }
    if !indexed && let Some(&lowest) = matched.first() {
        // The fallback array position is not evidence when a supplied alias matches.
        index = lowest;
    }
    for (&call_index, call) in &st.function_calls {
        let id_match = !item_id.is_empty() && (call.item_id_seen || call.added) && item_id == call.id;
        let call_id_match = !call_id.is_empty() && (call.call_id_seen || call.added) && call_id == call.call_id;
        if id_match || call_id_match {
            if !indexed && (matched.is_empty() || call_index < index) {
                index = call_index;
            }
            matched.insert(call_index);
        }
    }
    if !indexed && matched.is_empty() {
        let mut found = false;
        for (&item_index, id) in &st.item_ids {
            if !item_id.is_empty() && item_id == *id && (!found || item_index < index) {
                index = item_index;
                found = true;
            }
        }
        // A final array position must not rebind an unrelated item with a different supplied ID.
        if !found && (!item_id.is_empty() || !call_id.is_empty()) {
            while st.function_calls.contains_key(&index) || st.item_types.get(&index).is_some_and(|t| !t.is_empty()) {
                index += 1;
            }
        }
    }
    let mut related: BTreeSet<i64> = BTreeSet::from([index]);
    if explicit_index.exists() {
        related.insert(explicit_index.int());
    }
    if step_index.exists() {
        related.insert(step_index.int());
    }
    let mut conflict = matched.len() > 1 || (explicit_index.exists() && step_index.exists() && explicit_index.int() != step_index.int());
    for &call_index in &matched {
        related.insert(call_index);
        if (explicit_index.exists() && explicit_index.int() != call_index) || (step_index.exists() && step_index.int() != call_index) {
            conflict = true;
        }
    }
    let mut patch_related = st.patch_for(&step.g("name").str());
    for call_index in &related {
        if let Some(call) = st.function_calls.get(call_index) {
            patch_related = patch_related || call.patch_call.is_some() || st.patch_for(&call.raw_name);
            if (!item_id.is_empty() && call.item_id_seen && item_id != call.id) || (!call_id.is_empty() && call.call_id_seen && call_id != call.call_id) {
                conflict = true;
            }
        }
    }
    if conflict {
        let err = "conflicting Interactions apply_patch step identity".to_string();
        // Retain both sides even when an unnamed non-function snapshot is skipped.
        for &call_index in &related {
            st.pending_identity_errors.entry(call_index).or_insert_with(|| err.clone());
            if let Some(call) = st.function_calls.get_mut(&call_index)
                && call.pending_error.is_none()
            {
                call.pending_error = Some(err.clone());
            }
        }
    }
    // Keep unmatched aliases even on conflicts or partial invalid snapshots, so later provenance
    // through any supplied key cannot erase the contradiction.
    if !item_id.is_empty() {
        st.item_identity_indexes.entry(item_id).or_insert(index);
    }
    if !call_id.is_empty() {
        st.call_identity_indexes.entry(call_id).or_insert(index);
    }
    if patch_related {
        for call_index in &related {
            if let Some(err) = st.pending_identity_errors.get(call_index) {
                return Err(err.clone());
            }
        }
    }
    // Ordinary functions retain their explicit-index behavior; only patch identities fail closed.
    Ok(index)
}

/// Records function-call evidence from a step even before the upstream name identifies the winning
/// declaration, and announces the item once its identity is ready.
fn update_function_call(index: i64, step: &Res<'_>, st: &mut StreamState, initial: bool) -> Events {
    let mut call = st.function_calls.remove(&index).unwrap_or_default();
    let (mut events, stop_after) = update_function_call_inner(index, step, st, initial, &mut call);
    st.function_calls.insert(index, call);
    if stop_after {
        events.extend(step_stop(&index_root(index), st));
    }
    events
}

fn record_error(call: &mut FunctionCallState, err: &str) {
    if call.pending_error.is_none() {
        call.pending_error = Some(err.to_string());
    }
}

/// Merges a supplied item/call ID into the call, recording a conflict with an already-seen value.
fn merge_id(call: &mut FunctionCallState, raw_is_patch: bool, item_id: bool, value: &str) {
    if value.is_empty() {
        return;
    }
    let (current, seen) = if item_id { (call.id.clone(), call.item_id_seen) } else { (call.call_id.clone(), call.call_id_seen) };
    if (seen || (call.added && !raw_is_patch)) && current != value {
        record_error(call, "conflicting apply_patch call identity");
        return;
    }
    if item_id {
        call.id = value.to_string();
        call.item_id_seen = true;
    } else {
        call.call_id = value.to_string();
        call.call_id_seen = true;
    }
}

/// Body of [`update_function_call`]; the bool is true when the pending source stop must be
/// replayed once `call` is back in the state.
fn update_function_call_inner(index: i64, step: &Res<'_>, st: &mut StreamState, initial: bool, call: &mut FunctionCallState) -> (Events, bool) {
    let step_type = step.g("type");
    if step_type.exists() && step_type.str() != "function_call" {
        record_error(call, "conflicting apply_patch item type");
    }
    let raw_is_patch = st.patch_for(&call.raw_name);
    merge_id(call, raw_is_patch, true, &step.g("id").str());
    merge_id(call, raw_is_patch, false, &step.g("call_id").str());
    let name = step.g("name").str();
    if !name.is_empty() {
        if !call.raw_name.is_empty() && call.raw_name != name {
            record_error(call, "conflicting apply_patch call name");
        } else {
            call.raw_name = name.clone();
        }
    }
    if let Some(err) = &call.pending_error
        && st.patch_for(&name)
    {
        return (patch_failure(st, err.clone()), false);
    }
    let args = step.g("arguments");
    let skip_empty_initial = initial
        && !call.has_snapshot
        && call.arguments.is_empty()
        && !call.item_done_emitted
        && json_string_value(&args, "").trim() == "{}";
    if args.exists() && !skip_empty_initial {
        let arguments = json_string_value(&args, "{}");
        // A later complete snapshot is not a new prefix for already buffered fragments.
        if !call.added && call.initial_arguments.is_empty() && call.arguments.is_empty() {
            call.initial_arguments = arguments.clone();
        }
        let mut snapshot = ApplyPatchCallState::default();
        match snapshot.finish_arguments(&arguments) {
            Err(e) => record_error(call, &e),
            Ok((_, input)) => {
                if call.has_snapshot && input != call.snapshot_input {
                    record_error(call, "conflicting apply_patch full snapshots");
                }
                if let Some(patch) = &call.patch_call
                    && call.item_done_emitted
                    && input != patch.decoder.input()
                {
                    record_error(call, "apply_patch snapshot conflicts with completed input");
                }
                call.has_snapshot = true;
                call.snapshot_input = input;
                call.snapshot_arguments = arguments;
            }
        }
    }
    st.item_ids.insert(index, call.id.clone());
    st.item_types.insert(index, "function_call".to_string());
    if call.raw_name.is_empty() {
        return (vec![], false);
    }
    let known_identity = st.tool_identity_map.as_ref().and_then(|m| m.get(&call.raw_name)).cloned();
    let identity = known_identity.clone().unwrap_or_default();
    if let Some(known) = known_identity {
        call.name = known.name;
        call.namespace = known.namespace;
        call.is_custom = known.custom;
    } else {
        call.name = call.raw_name.clone();
        if st.for_antigravity {
            call.name = common::antigravity_upstream_tool_name_to_client(&call.raw_name);
        }
    }
    if identity.apply_patch {
        if let Some(err) = st.pending_envelope_error.clone() {
            return (patch_failure(st, err), false);
        }
        if let Some(err) = call.pending_error.clone() {
            return (patch_failure(st, err), false);
        }
        // Upstream evidence and downstream readiness are independent. A first late ID may still
        // be adopted; no provisional patch identity has escaped.
        if !(call.item_id_seen && call.call_id_seen) && !call.identity_finalized {
            return (vec![], false);
        }
    } else {
        call.argument_fragments.clear();
        if !call.item_id_seen && !call.added {
            call.id = first_non_empty(&[&call.call_id, &format!("item_{index}")]);
        }
        if !call.call_id_seen && !call.added {
            call.call_id = call.id.clone();
        }
    }
    st.item_ids.insert(index, call.id.clone());
    let mut events: Events = Vec::new();
    // Replay buffered ordinary arguments only when the item is first announced.
    let announced = !call.added;
    if announced {
        if !identity.apply_patch && !call.initial_arguments.is_empty() {
            let fragments = std::mem::take(&mut call.arguments);
            call.arguments = format!("{}{}", call.initial_arguments, fragments);
        }
        let (item_type, input_key) = if call.is_custom { ("custom_tool_call", "input") } else { ("function_call", "arguments") };
        let mut added = tmpl(r#"{"type":"response.output_item.added","item":{"status":"in_progress"}}"#);
        cpa_json::set(&mut added, "sequence_number", next_seq(&mut st.seq));
        cpa_json::set(&mut added, "output_index", index);
        cpa_json::set(&mut added, "item.type", item_type);
        cpa_json::set(&mut added, &format!("item.{input_key}"), "");
        cpa_json::set(&mut added, "item.id", call.id.as_str());
        cpa_json::set(&mut added, "item.call_id", call.call_id.as_str());
        let added = common::set_responses_tool_call_identity(&cpa_json::to_vec(&added), &call.name, &call.namespace, "item");
        events.push(emit_bytes("response.output_item.added", &added));
        call.added = true;
    }
    if identity.apply_patch && call.patch_call.is_none() {
        let mut patch = ApplyPatchCallState {
            item_id: call.id.clone(),
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            namespace: call.namespace.clone(),
            output_index: index,
            ..Default::default()
        };
        let fragments = std::mem::take(&mut call.argument_fragments);
        for fragment in &fragments {
            match patch.push_arguments(fragment) {
                Ok(delta) => events.extend(patch_delta(&mut st.seq, &patch, &delta)),
                Err(e) => {
                    call.patch_call = Some(patch);
                    events.extend(patch_failure(st, e));
                    return (events, false);
                }
            }
        }
        call.patch_call = Some(patch);
    } else if !call.is_custom && announced && !call.arguments.is_empty() {
        events.push(function_call_arguments_delta(index, &call.id, &call.arguments, &mut st.seq));
    }
    let mut stop_after = false;
    if call.stop_pending && call.patch_call.is_some() {
        call.stop_pending = false;
        stop_after = true;
    }
    (events, stop_after)
}

/// Completes every declared apply_patch call that has not yet published its done events.
fn finish_patch_calls(st: &mut StreamState) -> Events {
    if st.has_patch_bridge() && st.function_calls.values().any(|call| call.raw_name.is_empty()) {
        return patch_failure(st, "unresolved Interactions apply_patch call identity".to_string());
    }
    let indexes: Vec<i64> = st
        .function_calls
        .iter()
        .filter(|(_, call)| st.patch_for(&call.raw_name) && !call.item_done_emitted)
        .map(|(index, _)| *index)
        .collect();
    let mut events: Events = Vec::new();
    for index in indexes {
        let needs_patch_state = st.function_calls.get(&index).is_some_and(|call| call.patch_call.is_none());
        if needs_patch_state {
            // Interactions already maps an absent call_id to id (and an absent id to
            // call_id/item_<index>). Freeze that compatibility mapping only at the response
            // terminal, after all supplied snapshot IDs were reconciled.
            if let Some(call) = st.function_calls.get_mut(&index) {
                if !call.item_id_seen {
                    call.id = first_non_empty(&[&call.call_id, &format!("item_{index}")]);
                }
                if !call.call_id_seen {
                    call.call_id = call.id.clone();
                }
                call.identity_finalized = true;
            }
            events.extend(update_function_call(index, &Res::NONE, st, false));
            if st.terminal {
                break;
            }
        }
        events.extend(step_stop(&index_root(index), st));
        if st.terminal {
            break;
        }
    }
    events
}
