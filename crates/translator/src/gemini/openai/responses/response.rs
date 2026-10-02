//! Gemini SSE chunks -> OpenAI Responses SSE events (Go: gemini_openai-responses_response.go,
//! streaming half).
//!
//! A line-oriented state machine: [`GeminiToResponsesState`] lives in the stream's `Param` and
//! each upstream chunk may open/close reasoning, message, web search and function call items.
//! Go's nested closures over `st` and `out` are methods of `Stream`.

use crate::common::{parse_create_time, unix_nano_now, unix_now};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use cpa_core::util::{
    restore_sanitized_tool_name, responses_tool_reverse_identity_map, sanitized_tool_name_map, unwrap_responses_custom_tool_input,
    ResponsesToolIdentity,
};
use cpa_json::{json, Res, Value, J};

use super::lenient::{gjson_valid, parse_gjson};
use super::function_evidence::{pending_identity_error, record_function_evidence, EvidenceStore};
use super::signature_carrier::{encode_gemini_responses_carrier, CARRIER_ANY, CARRIER_FUNCTION, CARRIER_NEXT, CARRIER_PREVIOUS, CARRIER_STANDALONE, CARRIER_TEXT};
use super::trailing_signature::cache_gemini_responses_text_signatures;
use super::web_search::{
    allows_responses_web_search_tool_choice, build_responses_url_citations_for_messages, build_responses_web_search_call_item,
    extract_grounding_metadata, extract_grounding_queries, extract_grounding_sources, extract_responses_web_search_query,
    go_rune_count, has_responses_web_search_tool, has_valid_web_grounding, merge_citation_annotations, merge_grounding_metadata,
    model_supports_web_search, GeminiPartMapping,
};
use super::request::GEMINI_RESPONSES_THOUGHT_SIGNATURE;
use crate::common::{
    apply_patch_failure, apply_patch_input_delta, apply_patch_input_done, request_model_name, set_responses_tool_call_identity,
    sse_event_data, ApplyPatchCallState, ApplyPatchErrorState,
};
use crate::registry::{Ctx, Param};

/// Process-wide counters for synthesized ids (Go: responseIDCounter, funcCallIDCounter).
static RESPONSE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
static FUNC_CALL_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(super) fn next_response_id_counter() -> u64 {
    RESPONSE_ID_COUNTER.fetch_add(1, Ordering::SeqCst) + 1
}

pub(super) fn next_func_call_id_counter() -> u64 {
    FUNC_CALL_ID_COUNTER.fetch_add(1, Ordering::SeqCst) + 1
}

#[derive(Debug, Clone)]
struct DetachedReasoningItem {
    id: String,
    signature: String,
}

#[derive(Debug, Clone)]
struct CompletedMessageItem {
    id: String,
    text: String,
    annotations: Vec<Value>,
}

#[derive(Debug, Clone)]
struct CompletedReasoningItem {
    id: String,
    signature: String,
    text: String,
}

#[derive(Debug, Clone)]
struct StreamBufferedPart {
    part_index: i64,
    text: String,
}

/// Per-stream state (Go: geminiToResponsesState).
#[derive(Default)]
struct GeminiToResponsesState {
    err: ApplyPatchErrorState,
    seq: i64,
    response_id: String,
    created_at: i64,
    started: bool,
    completed: bool,

    // message aggregation
    msg_opened: bool,
    msg_closed: bool,
    msg_index: usize,
    current_msg_id: String,
    item_text_buf: String,

    // reasoning aggregation
    reasoning_opened: bool,
    reasoning_index: usize,
    reasoning_item_id: String,
    reasoning_enc: String,
    reasoning_direction: String,
    reasoning_target_kind: String,
    reasoning_buf: String,
    reasoning_pending_deltas: Vec<String>,
    reasoning_closed: bool,
    pending_reasoning_signature: String,
    detached_reasoning: HashMap<usize, DetachedReasoningItem>,
    completed_messages: HashMap<usize, CompletedMessageItem>,
    completed_reasoning: HashMap<usize, CompletedReasoningItem>,
    seen_reasoning_signatures: HashSet<String>,
    last_semantic_kind: String,
    hidden_text_signatures: HashMap<String, Vec<String>>,

    // function call aggregation (keyed by output_index)
    next_index: usize,
    func_args_buf: HashMap<usize, String>,
    func_input_buf: HashMap<usize, String>,
    func_custom: HashMap<usize, bool>,
    func_names: HashMap<usize, String>,
    func_namespaces: HashMap<usize, String>,
    func_call_ids: HashMap<usize, String>,
    func_done: HashMap<usize, bool>,
    sanitized_name_map: HashMap<String, String>,
    tool_identity_map: HashMap<String, ResponsesToolIdentity>,
    evidence: EvidenceStore,

    // web search aggregation
    web_search_stream_mode: bool,
    web_search_opened: bool,
    web_search_done: bool,
    web_search_index: usize,
    web_search_item_id: String,
    web_search_query: String,
    web_search_queries: Vec<String>,
    web_search_sources: Vec<Value>,
    web_search_buffered_deltas: Vec<String>,
    web_search_buffered_parts: Vec<StreamBufferedPart>,
    raw_grounding_metadata: Option<Value>,
    part_mappings: Vec<GeminiPartMapping>,
    current_logical_part_index: i64,
    current_part_kind: String,
    has_seen_first_part: bool,
    text_part_run_active: bool,
    current_msg_rune_offset: i64,
    emitted_annotation_count: HashMap<usize, usize>,
}

impl GeminiToResponsesState {
    fn new(original_request_raw_json: &[u8], req_json: Option<&[u8]>) -> Self {
        Self {
            sanitized_name_map: sanitized_tool_name_map(original_request_raw_json),
            tool_identity_map: responses_tool_reverse_identity_map(req_json.unwrap_or_default()),
            ..Default::default()
        }
    }
}

/// The original request when it is valid JSON, else the translated one, else `None`.
pub(super) fn pick_request_json<'a>(original: &'a [u8], request: &'a [u8]) -> Option<&'a [u8]> {
    if !original.is_empty() && cpa_json::valid(original) {
        return Some(original);
    }
    if !request.is_empty() && cpa_json::valid(request) {
        return Some(request);
    }
    None
}

/// A Codex-style `{"request": {...}}` envelope is unwrapped when the inner object looks like a
/// Responses request.
pub(super) fn unwrap_request_root(root: &Value) -> &Value {
    match root.get("request") {
        Some(req) if req.g("model").exists() || req.g("input").exists() || req.g("instructions").exists() => req,
        _ => root,
    }
}

/// A Vertex-style `{"response": {...}}` wrapper is unwrapped when the inner payload looks like a
/// Gemini response.
pub(super) fn unwrap_gemini_response_root(root: Value) -> (Value, bool) {
    let resp = root.g("response");
    if !resp.exists() {
        return (root, false);
    }
    if resp.g("candidates").exists() || resp.g("responseId").exists() || resp.g("usageMetadata").exists() {
        return (resp.value(), true);
    }
    (root, false)
}

fn emit_event(event: &str, payload: &Value) -> Vec<u8> {
    sse_event_data(event, &cpa_json::to_vec(payload))
}

fn has_effective_google_search_tool(raw_json: &[u8]) -> bool {
    if raw_json.is_empty() {
        return false;
    }
    let root = cpa_json::parse(raw_json);
    if root.g("requestType").str() == "web_search" {
        return true;
    }
    for path in ["request.tools", "tools"] {
        let tools = root.g(path);
        if !tools.is_array() {
            continue;
        }
        if tools.array().iter().any(|t| t.g("googleSearch").exists()) {
            return true;
        }
    }
    false
}

fn is_upstream_gemini_request(raw_json: &[u8]) -> bool {
    if raw_json.is_empty() {
        return false;
    }
    let root = cpa_json::parse(raw_json);
    if root.g("requestType").exists() {
        return true;
    }
    ["contents", "request.contents"].iter().any(|p| root.g(p).exists())
}

/// Whether text should be buffered until the web search item is finalized.
fn determine_web_search_stream_mode(model_name: &str, request_model_name: &str, original: &[u8], request: &[u8]) -> bool {
    if !original.is_empty() {
        let orig = cpa_json::parse(original);
        if !allows_responses_web_search_tool_choice(unwrap_request_root(&orig)) {
            return false;
        }
    }
    if !request.is_empty() {
        let req = cpa_json::parse(request);
        let req_root = unwrap_request_root(&req);
        if req_root.g("tool_choice").exists() && !allows_responses_web_search_tool_choice(req_root) {
            return false;
        }
        if is_upstream_gemini_request(request) || has_effective_google_search_tool(request) {
            return has_effective_google_search_tool(request);
        }
    }
    if let Some(req_json) = pick_request_json(original, request) {
        let parsed = cpa_json::parse(req_json);
        let req_root = unwrap_request_root(&parsed);
        return has_responses_web_search_tool(req_root)
            && allows_responses_web_search_tool_choice(req_root)
            && (model_supports_web_search(model_name) || model_supports_web_search(request_model_name));
    }
    false
}

/// Echo fields copied from the request into the response object.
pub(super) fn echo_request_fields(target: &mut Value, prefix: &str, req: &Value, include_model: bool) {
    let p = |name: &str| if prefix.is_empty() { name.to_string() } else { format!("{prefix}.{name}") };
    let get = |name: &str| req.g(name);
    let v = get("instructions");
    if v.exists() {
        cpa_json::set(target, &p("instructions"), v.str());
    }
    let v = get("max_output_tokens");
    if v.exists() {
        cpa_json::set(target, &p("max_output_tokens"), v.int());
    }
    let v = get("max_tool_calls");
    if v.exists() {
        cpa_json::set(target, &p("max_tool_calls"), v.int());
    }
    if include_model {
        let v = get("model");
        if v.exists() {
            cpa_json::set(target, &p("model"), v.str());
        }
    }
    let v = get("parallel_tool_calls");
    if v.exists() {
        cpa_json::set(target, &p("parallel_tool_calls"), v.bool());
    }
    for name in ["previous_response_id", "prompt_cache_key"] {
        let v = get(name);
        if v.exists() {
            cpa_json::set(target, &p(name), v.str());
        }
    }
    let v = get("reasoning");
    if v.exists() {
        cpa_json::set(target, &p("reasoning"), v.value());
    }
    for name in ["safety_identifier", "service_tier"] {
        let v = get(name);
        if v.exists() {
            cpa_json::set(target, &p(name), v.str());
        }
    }
    let v = get("store");
    if v.exists() {
        cpa_json::set(target, &p("store"), v.bool());
    }
    let v = get("temperature");
    if v.exists() {
        cpa_json::set(target, &p("temperature"), cpa_json::num_f64(v.float()));
    }
    for name in ["text", "tool_choice", "tools"] {
        let v = get(name);
        if v.exists() {
            cpa_json::set(target, &p(name), v.value());
        }
    }
    let v = get("top_logprobs");
    if v.exists() {
        cpa_json::set(target, &p("top_logprobs"), v.int());
    }
    let v = get("top_p");
    if v.exists() {
        cpa_json::set(target, &p("top_p"), cpa_json::num_f64(v.float()));
    }
    let v = get("truncation");
    if v.exists() {
        cpa_json::set(target, &p("truncation"), v.str());
    }
    for name in ["user", "metadata"] {
        let v = get(name);
        if v.exists() {
            cpa_json::set(target, &p(name), v.value());
        }
    }
}

/// Maps `usageMetadata` into a Responses `usage` object at `prefix`. The stream variant writes
/// zero defaults for missing thought and total counts, the non-stream one omits them.
pub(super) fn set_usage(target: &mut Value, prefix: &str, um: &Res<'_>, zero_defaults: bool) {
    let p = |name: &str| format!("{prefix}.{name}");
    // Input tokens are the prompt only (thoughts go to output).
    cpa_json::set(target, &p("input_tokens"), um.g("promptTokenCount").int());
    cpa_json::set(target, &p("input_tokens_details.cached_tokens"), um.g("cachedContentTokenCount").int());
    cpa_json::set(target, &p("output_tokens"), um.g("candidatesTokenCount").int() + um.g("thoughtsTokenCount").int());
    let thoughts = um.g("thoughtsTokenCount");
    if thoughts.exists() {
        cpa_json::set(target, &p("output_tokens_details.reasoning_tokens"), thoughts.int());
    } else if zero_defaults {
        cpa_json::set(target, &p("output_tokens_details.reasoning_tokens"), 0);
    }
    let total = um.g("totalTokenCount");
    if total.exists() {
        cpa_json::set(target, &p("total_tokens"), total.int());
    } else if zero_defaults {
        cpa_json::set(target, &p("total_tokens"), 0);
    }
}

/// Converts Gemini SSE chunks into OpenAI Responses SSE events. Also used by the antigravity
/// translator after unwrapping its envelope.
pub fn convert_gemini_response_to_openai_responses(
    _ctx: &Ctx,
    model_name: &str,
    original_request_raw_json: &[u8],
    request_raw_json: &[u8],
    raw_json: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let req_json = pick_request_json(original_request_raw_json, request_raw_json);
    let st = param.state(|| GeminiToResponsesState::new(original_request_raw_json, req_json));

    let mut raw = raw_json;
    if raw.starts_with(b"data:") {
        raw = raw[5..].trim_ascii();
    }
    raw = raw.trim_ascii();
    if raw.is_empty() || st.completed {
        return Vec::new();
    }
    let done_chunk;
    if raw == b"[DONE]" {
        if !st.started {
            return Vec::new();
        }
        done_chunk = br#"{"candidates":[{"finishReason":"STOP"}]}"#;
        raw = done_chunk;
    }

    let Some(parsed) = parse_gjson(raw) else { return Vec::new() };
    let valid_json = gjson_valid(raw);
    let (root, wrapped) = unwrap_gemini_response_root(parsed);
    let root_raw: &[u8] = if wrapped { cpa_json::raw_at(raw, "response").map(str::as_bytes).unwrap_or(raw) } else { raw };

    let out = {
        let mut stream = Stream {
            st: &mut *st,
            out: Vec::new(),
            model_name,
            original: original_request_raw_json,
            request: request_raw_json,
            req_json,
        };
        stream.run(&root, root_raw, valid_json);
        stream.out
    };
    let err = st.err.tool_input_error().map(str::to_string);
    if let Some(err) = err {
        param.tool_input_error = Some(err);
    }
    out
}

/// Rejects a patch-enabled stream that ended without its source terminator (Go:
/// FinalizeToolInput, found by the executor through an interface). Returns the failure event, if
/// any.
pub(crate) fn finalize_tool_input(param: &mut Param) -> Vec<Vec<u8>> {
    let Some(st) = param.get::<GeminiToResponsesState>() else { return Vec::new() };
    if st.err.tool_input_error().is_some() || st.completed {
        return Vec::new();
    }
    if !st.tool_identity_map.values().any(|i| i.apply_patch) {
        return Vec::new();
    }
    st.err.set_tool_input_error("upstream apply_patch stream ended before protocol completion");
    st.completed = true;
    st.seq += 1;
    let event = emit_event("response.failed", &cpa_json::parse(&apply_patch_failure(&st.response_id, st.seq)));
    let err = st.err.tool_input_error().map(str::to_string);
    if let Some(err) = err {
        param.tool_input_error = Some(err);
    }
    vec![event]
}

struct Stream<'a> {
    st: &'a mut GeminiToResponsesState,
    out: Vec<Vec<u8>>,
    model_name: &'a str,
    original: &'a [u8],
    request: &'a [u8],
    /// Request used for tool and web search lookups (Go: reqJSON), parsed on demand.
    req_json: Option<&'a [u8]>,
}

impl Stream<'_> {
    fn next_seq(&mut self) -> i64 {
        self.st.seq += 1;
        self.st.seq
    }

    fn push(&mut self, event: &str, payload: &Value) {
        self.out.push(emit_event(event, payload));
    }

    fn reasoning_encrypted_content(&self) -> String {
        let st = &self.st;
        if st.reasoning_enc.is_empty() || st.reasoning_direction.is_empty() {
            return st.reasoning_enc.clone();
        }
        encode_gemini_responses_carrier(&st.reasoning_enc, &st.reasoning_direction, &st.reasoning_target_kind)
    }

    fn fill_web_search_query_from_request(&mut self) {
        if self.st.web_search_query.is_empty() && !self.st.web_search_queries.is_empty() {
            self.st.web_search_query = self.st.web_search_queries[0].clone();
        }
        if self.st.web_search_query.is_empty()
            && let Some(req) = self.req_json.map(cpa_json::parse) {
                self.st.web_search_query = extract_responses_web_search_query(unwrap_request_root(&req));
            }
    }

    fn finalize_web_search(&mut self) {
        if !self.st.web_search_opened || self.st.web_search_done {
            return;
        }
        self.fill_web_search_query_from_request();

        let seq = self.next_seq();
        let completed = json!({"type": "response.web_search_call.completed", "sequence_number": seq, "output_index": self.st.web_search_index, "item_id": self.st.web_search_item_id});
        self.push("response.web_search_call.completed", &completed);

        let done_item = build_responses_web_search_call_item(&self.st.web_search_item_id, &self.st.web_search_query, &self.st.web_search_queries, &self.st.web_search_sources);
        let seq = self.next_seq();
        let done_event = json!({"type": "response.output_item.done", "sequence_number": seq, "output_index": self.st.web_search_index, "item": done_item});
        self.push("response.output_item.done", &done_event);
        self.st.web_search_done = true;
    }

    fn open_reasoning(&mut self) {
        let st = &self.st;
        if st.reasoning_opened || st.reasoning_closed || (st.reasoning_buf.is_empty() && st.reasoning_enc.is_empty()) {
            return;
        }
        self.finalize_web_search();
        self.st.reasoning_opened = true;
        self.st.reasoning_index = self.st.next_index;
        self.st.next_index += 1;
        self.st.reasoning_item_id = format!("rs_{}_{}", self.st.response_id, self.st.reasoning_index);
        let enc = self.reasoning_encrypted_content();
        let (item_id, index) = (self.st.reasoning_item_id.clone(), self.st.reasoning_index);
        let seq = self.next_seq();
        let item = json!({"type": "response.output_item.added", "sequence_number": seq, "output_index": index, "item": {"id": item_id, "type": "reasoning", "status": "in_progress", "encrypted_content": enc, "summary": []}});
        self.push("response.output_item.added", &item);
        let seq = self.next_seq();
        let part_added = json!({"type": "response.reasoning_summary_part.added", "sequence_number": seq, "item_id": item_id, "output_index": index, "summary_index": 0, "part": {"type": "summary_text", "text": ""}});
        self.push("response.reasoning_summary_part.added", &part_added);
        for delta in std::mem::take(&mut self.st.reasoning_pending_deltas) {
            let seq = self.next_seq();
            let msg = json!({"type": "response.reasoning_summary_text.delta", "sequence_number": seq, "item_id": item_id, "output_index": index, "summary_index": 0, "delta": delta});
            self.push("response.reasoning_summary_text.delta", &msg);
        }
    }

    /// Emits reasoning_summary_text.done, reasoning_summary_part.done and output_item.done once.
    fn finalize_reasoning(&mut self) {
        self.open_reasoning();
        if !self.st.reasoning_opened || self.st.reasoning_closed {
            return;
        }
        let full = self.st.reasoning_buf.clone();
        let (item_id, index) = (self.st.reasoning_item_id.clone(), self.st.reasoning_index);
        let enc = self.reasoning_encrypted_content();
        let seq = self.next_seq();
        let text_done = json!({"type": "response.reasoning_summary_text.done", "sequence_number": seq, "item_id": item_id, "output_index": index, "summary_index": 0, "text": full});
        self.push("response.reasoning_summary_text.done", &text_done);
        let seq = self.next_seq();
        let part_done = json!({"type": "response.reasoning_summary_part.done", "sequence_number": seq, "item_id": item_id, "output_index": index, "summary_index": 0, "part": {"type": "summary_text", "text": full}});
        self.push("response.reasoning_summary_part.done", &part_done);
        let seq = self.next_seq();
        let item_done = json!({"type": "response.output_item.done", "sequence_number": seq, "output_index": index, "item": {"id": item_id, "type": "reasoning", "encrypted_content": enc, "summary": [{"type": "summary_text", "text": full}]}});
        self.push("response.output_item.done", &item_done);

        self.st.completed_reasoning.insert(index, CompletedReasoningItem { id: item_id, signature: enc, text: full });
        self.st.reasoning_closed = true;
    }

    fn reset_reasoning(&mut self) {
        let st = &mut *self.st;
        st.reasoning_opened = false;
        st.reasoning_closed = false;
        st.reasoning_index = 0;
        st.reasoning_item_id.clear();
        st.reasoning_enc.clear();
        st.reasoning_direction.clear();
        st.reasoning_target_kind.clear();
        st.reasoning_buf.clear();
        st.reasoning_pending_deltas.clear();
    }

    fn open_web_search(&mut self) {
        if self.st.web_search_opened {
            return;
        }
        self.finalize_reasoning();
        self.st.web_search_opened = true;
        self.st.web_search_index = self.st.next_index;
        self.st.next_index += 1;
        self.st.web_search_item_id = format!("ws_{}", self.st.response_id.strip_prefix("resp_").unwrap_or(&self.st.response_id));
        self.fill_web_search_query_from_request();

        let seq = self.next_seq();
        let added = json!({"type": "response.output_item.added", "sequence_number": seq, "output_index": self.st.web_search_index, "item": {"id": self.st.web_search_item_id, "type": "web_search_call", "status": "in_progress", "action": {"type": "search", "query": self.st.web_search_query}}});
        self.push("response.output_item.added", &added);
        let seq = self.next_seq();
        let searching = json!({"type": "response.web_search_call.searching", "sequence_number": seq, "output_index": self.st.web_search_index, "item_id": self.st.web_search_item_id});
        self.push("response.web_search_call.searching", &searching);
    }

    /// Opens a new assistant message item (output_item.added and content_part.added).
    fn open_message(&mut self) {
        self.st.msg_opened = true;
        self.st.msg_index = self.st.next_index;
        self.st.next_index += 1;
        self.st.current_msg_id = format!("msg_{}_{}", self.st.response_id, self.st.msg_index);
        let (msg_id, msg_index) = (self.st.current_msg_id.clone(), self.st.msg_index);
        let seq = self.next_seq();
        let item = json!({"type": "response.output_item.added", "sequence_number": seq, "output_index": msg_index, "item": {"id": msg_id, "type": "message", "status": "in_progress", "content": [], "role": "assistant"}});
        self.push("response.output_item.added", &item);
        let seq = self.next_seq();
        let part_added = json!({"type": "response.content_part.added", "sequence_number": seq, "item_id": msg_id, "output_index": msg_index, "content_index": 0, "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": ""}});
        self.push("response.content_part.added", &part_added);
        self.st.item_text_buf.clear();
        self.st.current_msg_rune_offset = 0;
    }

    fn push_text_delta(&mut self, delta: &str) {
        let (msg_id, msg_index) = (self.st.current_msg_id.clone(), self.st.msg_index);
        let seq = self.next_seq();
        let msg = json!({"type": "response.output_text.delta", "sequence_number": seq, "item_id": msg_id, "output_index": msg_index, "content_index": 0, "delta": delta, "logprobs": []});
        self.push("response.output_text.delta", &msg);
    }

    fn flush_web_search_buffered_text(&mut self) {
        self.finalize_web_search();
        if self.st.web_search_buffered_deltas.is_empty() {
            return;
        }
        if self.st.msg_closed {
            self.st.msg_opened = false;
            self.st.msg_closed = false;
            self.st.item_text_buf.clear();
            self.st.current_msg_rune_offset = 0;
        }
        if !self.st.msg_opened {
            self.open_message();
        }
        for delta in std::mem::take(&mut self.st.web_search_buffered_deltas) {
            self.st.item_text_buf.push_str(&delta);
            self.push_text_delta(&delta);
        }
        for bp in std::mem::take(&mut self.st.web_search_buffered_parts) {
            self.record_part_mapping(bp.part_index, &bp.text);
        }
    }

    /// Records where a text part sits in the current message (merging with the previous mapping
    /// of the same part and message) and advances the rune offset.
    fn record_part_mapping(&mut self, part_index: i64, text: &str) {
        let msg_index = self.st.msg_index as i64;
        match self.st.part_mappings.last_mut() {
            Some(last) if last.part_index == part_index && last.message_index == msg_index => last.part_text.push_str(text),
            _ => {
                let start = self.st.current_msg_rune_offset;
                self.st.part_mappings.push(GeminiPartMapping { part_index, message_index: msg_index, start_rune_in_msg: start, part_text: text.to_string() });
            }
        }
        self.st.current_msg_rune_offset += go_rune_count(text.as_bytes());
    }

    fn emit_new_citation_annotations(&mut self, msg_index: usize, item_id: &str, annotations: &[Value]) {
        let emitted = self.st.emitted_annotation_count.get(&msg_index).copied().unwrap_or(0);
        for (ann_idx, annotation) in annotations.iter().enumerate().skip(emitted) {
            let seq = self.next_seq();
            let ann_event = json!({"type": "response.output_text.annotation.added", "sequence_number": seq, "response_id": self.st.response_id, "item_id": item_id, "output_index": msg_index, "content_index": 0, "annotation_index": ann_idx, "annotation": annotation});
            self.push("response.output_text.annotation.added", &ann_event);
        }
        if annotations.len() > emitted {
            self.st.emitted_annotation_count.insert(msg_index, annotations.len());
        }
    }

    /// Closes the assistant message: new citation annotations, then output_text.done,
    /// content_part.done and output_item.done, exactly once.
    fn finalize_message(&mut self) {
        self.finalize_web_search();
        if !self.st.web_search_buffered_deltas.is_empty() {
            self.flush_web_search_buffered_text();
        }
        if !self.st.msg_opened || self.st.msg_closed {
            return;
        }
        let full_text = self.st.item_text_buf.clone();
        let mut msg_citations: Vec<Value> = Vec::new();
        if let Some(gm) = &self.st.raw_grounding_metadata {
            let c_map = build_responses_url_citations_for_messages(gm, &self.st.part_mappings, std::slice::from_ref(&full_text));
            msg_citations = c_map.get(&(self.st.msg_index as i64)).cloned().unwrap_or_default();
            if msg_citations.is_empty() && self.st.completed_messages.is_empty()
                && let Some(first) = c_map.get(&0).filter(|c| !c.is_empty()) {
                    msg_citations = first.clone();
                }
        }
        let (msg_id, msg_index) = (self.st.current_msg_id.clone(), self.st.msg_index);
        self.emit_new_citation_annotations(msg_index, &msg_id, &msg_citations);
        let seq = self.next_seq();
        let done = json!({"type": "response.output_text.done", "sequence_number": seq, "item_id": msg_id, "output_index": msg_index, "content_index": 0, "text": full_text, "logprobs": []});
        self.push("response.output_text.done", &done);
        let seq = self.next_seq();
        let mut part_done = json!({"type": "response.content_part.done", "sequence_number": seq, "item_id": msg_id, "output_index": msg_index, "content_index": 0, "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": full_text}});
        if !msg_citations.is_empty() {
            cpa_json::set(&mut part_done, "part.annotations", Value::Array(msg_citations.clone()));
        }
        self.push("response.content_part.done", &part_done);
        let seq = self.next_seq();
        let mut final_event = json!({"type": "response.output_item.done", "sequence_number": seq, "output_index": msg_index, "item": {"id": msg_id, "type": "message", "status": "completed", "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": full_text}], "role": "assistant"}});
        if !msg_citations.is_empty() {
            cpa_json::set(&mut final_event, "item.content.0.annotations", Value::Array(msg_citations.clone()));
        }
        self.push("response.output_item.done", &final_event);

        self.st.completed_messages.insert(msg_index, CompletedMessageItem { id: msg_id, text: full_text, annotations: msg_citations });
        self.st.msg_closed = true;
        self.st.current_msg_rune_offset = 0;
    }

    /// Citations that arrive after a message is closed are emitted as late annotations.
    fn emit_late_citations(&mut self) {
        let Some(gm) = self.st.raw_grounding_metadata.clone() else { return };
        if self.st.completed_messages.is_empty() {
            return;
        }
        let mut msg_texts: Vec<String> = Vec::with_capacity(self.st.completed_messages.len());
        for idx in 0..self.st.next_index {
            if let Some(msg) = self.st.completed_messages.get(&idx) {
                msg_texts.push(msg.text.clone());
            }
        }
        let late_map = build_responses_url_citations_for_messages(&gm, &self.st.part_mappings, &msg_texts);
        if late_map.is_empty() {
            return;
        }
        for idx in 0..self.st.next_index {
            let Some(completed_message) = self.st.completed_messages.get(&idx).cloned() else { continue };
            let mut late_cites = late_map.get(&(idx as i64)).cloned().unwrap_or_default();
            if late_cites.is_empty() && self.st.completed_messages.len() == 1
                && let Some(first) = late_map.get(&0).filter(|c| !c.is_empty()) {
                    late_cites = first.clone();
                }
            let annotations = merge_citation_annotations(&completed_message.annotations, &late_cites);
            self.emit_new_citation_annotations(idx, &completed_message.id, &annotations);
            if !annotations.is_empty()
                && let Some(m) = self.st.completed_messages.get_mut(&idx) {
                    m.annotations = annotations;
                }
        }
    }

    fn emit_detached_reasoning(&mut self, signature: &str, direction: &str, target_kind: &str) {
        let signature = signature.trim();
        if signature.is_empty() || self.st.seen_reasoning_signatures.contains(signature) {
            return;
        }
        self.finalize_reasoning();
        self.finalize_message();
        let idx = self.st.next_index;
        self.st.next_index += 1;
        let placement = if direction == CARRIER_PREVIOUS { "after" } else { "before" };
        let item_id = format!("rs_{}_detached_{}_{}", self.st.response_id, placement, idx);
        let carrier_signature = encode_gemini_responses_carrier(signature, direction, target_kind);

        let seq = self.next_seq();
        let added = json!({"type": "response.output_item.added", "sequence_number": seq, "output_index": idx, "item": {"id": item_id, "type": "reasoning", "status": "in_progress", "encrypted_content": carrier_signature, "summary": []}});
        self.push("response.output_item.added", &added);
        let seq = self.next_seq();
        let done = json!({"type": "response.output_item.done", "sequence_number": seq, "output_index": idx, "item": {"id": item_id, "type": "reasoning", "encrypted_content": carrier_signature, "summary": []}});
        self.push("response.output_item.done", &done);

        self.st.detached_reasoning.insert(idx, DetachedReasoningItem { id: item_id, signature: carrier_signature });
        self.st.seen_reasoning_signatures.insert(signature.to_string());
    }

    fn emit_trailing_detached_reasoning(&mut self, signature: &str) {
        match self.st.last_semantic_kind.as_str() {
            CARRIER_TEXT => {
                let signature = signature.trim();
                if signature.is_empty() || self.st.seen_reasoning_signatures.contains(signature) {
                    return;
                }
                self.finalize_reasoning();
                self.finalize_message();
                // last_semantic_kind also includes thought text. Never bind a later thought
                // signature to a visible message from before that thought.
                if !self.st.msg_opened || (self.st.reasoning_opened && self.st.reasoning_index > self.st.msg_index) {
                    self.emit_detached_reasoning(signature, CARRIER_PREVIOUS, CARRIER_TEXT);
                    return;
                }
                // Keep failed writes in the prefix so a later successful write cannot move a
                // newer signature ahead of an earlier fallback carrier.
                let msg_id = self.st.current_msg_id.clone();
                let signatures = {
                    let list = self.st.hidden_text_signatures.entry(msg_id.clone()).or_default();
                    list.push(signature.to_string());
                    list.clone()
                };
                if cache_gemini_responses_text_signatures(self.model_name, &msg_id, &self.st.item_text_buf, &signatures) {
                    self.st.seen_reasoning_signatures.insert(signature.to_string());
                    return;
                }
                // Preserve replay continuity if the cache cannot accept the signature.
                self.emit_detached_reasoning(signature, CARRIER_PREVIOUS, CARRIER_TEXT);
            }
            CARRIER_FUNCTION => self.emit_detached_reasoning(signature, CARRIER_PREVIOUS, CARRIER_FUNCTION),
            _ => self.emit_detached_reasoning(signature, CARRIER_STANDALONE, CARRIER_ANY),
        }
    }

    fn fail(&mut self, err: String) {
        self.st.err.set_tool_input_error(err);
        self.st.completed = true;
        let seq = self.next_seq();
        let failure = cpa_json::parse(&apply_patch_failure(&self.st.response_id, seq));
        self.push("response.failed", &failure);
    }

    fn run(&mut self, root: &Value, root_raw: &[u8], valid_json: bool) {
        // Initialize per-response fields and emit created/in_progress once.
        if !self.st.started {
            self.st.response_id = root.g("responseId").str();
            if self.st.response_id.is_empty() {
                self.st.response_id = format!("resp_{:x}_{}", unix_nano_now(), next_response_id_counter());
            }
            if !self.st.response_id.starts_with("resp_") {
                self.st.response_id = format!("resp_{}", self.st.response_id);
            }
            let create_time = root.g("createTime");
            if create_time.exists()
                && let Some(t) = parse_create_time(&create_time.str()) {
                    self.st.created_at = t;
                }
            if self.st.created_at == 0 {
                self.st.created_at = unix_now();
            }

            let seq = self.next_seq();
            let mut created = json!({"type": "response.created", "sequence_number": seq, "response": {"id": self.st.response_id, "object": "response", "created_at": self.st.created_at, "status": "in_progress", "background": false, "error": null, "output": []}});
            let mut req_model = request_model_name(self.original, self.request);
            if req_model.is_empty() {
                req_model = self.model_name.to_string();
            }
            if !req_model.is_empty() {
                cpa_json::set(&mut created, "response.model", req_model.as_str());
            }
            self.push("response.created", &created);

            let seq = self.next_seq();
            let mut in_progress = json!({"type": "response.in_progress", "sequence_number": seq, "response": {"id": self.st.response_id, "object": "response", "created_at": self.st.created_at, "status": "in_progress", "output": []}});
            if !req_model.is_empty() {
                cpa_json::set(&mut in_progress, "response.model", req_model.as_str());
            }
            self.push("response.in_progress", &in_progress);

            self.st.started = true;
            self.st.next_index = 0;
            self.st.web_search_stream_mode = determine_web_search_stream_mode(self.model_name, &req_model, self.original, self.request);
        }

        // groundingMetadata for web search.
        if let Some(gm) = extract_grounding_metadata(root) {
            let merged = merge_grounding_metadata(self.st.raw_grounding_metadata.as_ref(), Some(&gm));
            self.st.raw_grounding_metadata = merged;
            let merged_gm = self.st.raw_grounding_metadata.clone().unwrap_or(Value::Null);
            let queries = extract_grounding_queries(&merged_gm);
            if !queries.is_empty() {
                if self.st.web_search_query.is_empty() {
                    self.st.web_search_query = queries[0].clone();
                }
                self.st.web_search_queries = queries;
            }
            let sources = extract_grounding_sources(&merged_gm);
            if !sources.is_empty() {
                self.st.web_search_sources = sources;
            }
            if !self.st.web_search_opened && has_valid_web_grounding(&merged_gm) {
                self.open_web_search();
            }

            self.emit_late_citations();
        }

        // Parts (text / thought / functionCall).
        let parts = root.g("candidates.0.content.parts");
        if parts.exists() && parts.is_array() {
            let part_raws = cpa_json::raw_children(root_raw, "candidates.0.content.parts");
            for (part_idx_in_chunk, part) in parts.array().iter().enumerate() {
                let args_raw = crate::common::raw_in(part_raws.get(part_idx_in_chunk), "functionCall.args");
                if !self.process_part(part_idx_in_chunk as i64, part, args_raw, valid_json) {
                    break;
                }
            }
        }

        if self.st.completed {
            return;
        }

        // Finalization on finishReason.
        let fr = root.g("candidates.0.finishReason");
        if fr.exists() && !fr.str().is_empty() {
            self.finish(root);
        }
    }

    fn finish(&mut self, root: &Value) {
        if let Some(err) = pending_identity_error(&self.st.evidence, &self.st.tool_identity_map) {
            self.fail(err);
            return;
        }
        if !self.st.pending_reasoning_signature.is_empty() {
            let pending = std::mem::take(&mut self.st.pending_reasoning_signature);
            self.emit_trailing_detached_reasoning(&pending);
        }
        // Finalize web search with the complete incremental sources, then reasoning, then the
        // message so web_search_call precedes later output items.
        self.finalize_web_search();
        self.finalize_reasoning();
        self.finalize_message();

        // Close function calls in index order.
        let mut idxs: Vec<usize> = self.st.func_args_buf.keys().copied().collect();
        idxs.sort_unstable();
        for idx in idxs {
            if self.st.func_done.get(&idx).copied().unwrap_or(false) {
                continue;
            }
            let call_id = self.st.func_call_ids.get(&idx).cloned().unwrap_or_default();
            let name = self.st.func_names.get(&idx).cloned().unwrap_or_default();
            let namespace = self.st.func_namespaces.get(&idx).cloned().unwrap_or_default();
            if self.st.func_custom.get(&idx).copied().unwrap_or(false) {
                let input_str = self.st.func_input_buf.get(&idx).cloned().unwrap_or_default();
                let seq = self.next_seq();
                let input_done = json!({"type": "response.custom_tool_call_input.done", "sequence_number": seq, "item_id": format!("ctc_{call_id}"), "output_index": idx, "input": input_str});
                self.push("response.custom_tool_call_input.done", &input_done);
                let seq = self.next_seq();
                let item_done = self.tool_item_done_event(seq, idx, "custom_tool_call", &call_id, "input", &input_str, &name, &namespace);
                self.push("response.output_item.done", &item_done);
            } else {
                let args = match self.st.func_args_buf.get(&idx) {
                    Some(b) if !b.is_empty() => b.clone(),
                    _ => "{}".to_string(),
                };
                let seq = self.next_seq();
                let fc_done = json!({"type": "response.function_call_arguments.done", "sequence_number": seq, "item_id": format!("fc_{call_id}"), "output_index": idx, "arguments": args});
                self.push("response.function_call_arguments.done", &fc_done);
                let seq = self.next_seq();
                let item_done = self.tool_item_done_event(seq, idx, "function_call", &call_id, "arguments", &args, &name, &namespace);
                self.push("response.output_item.done", &item_done);
            }
            self.st.func_done.insert(idx, true);
        }

        // response.completed with aggregated outputs and request echo fields.
        let seq = self.next_seq();
        let mut completed = json!({"type": "response.completed", "sequence_number": seq, "response": {"id": self.st.response_id, "object": "response", "created_at": self.st.created_at, "status": "completed", "background": false, "error": null}});

        if let Some(req_json) = pick_request_json(self.original, self.request) {
            let parsed = cpa_json::parse(req_json);
            echo_request_fields(&mut completed, "response", unwrap_request_root(&parsed), true);
        }

        self.emit_late_citations();

        // Outputs in output_index order.
        let mut outputs: Vec<Value> = Vec::with_capacity(self.st.next_index);
        for idx in 0..self.st.next_index {
            let st = &self.st;
            if st.web_search_done && idx == st.web_search_index {
                outputs.push(build_responses_web_search_call_item(&st.web_search_item_id, &st.web_search_query, &st.web_search_queries, &st.web_search_sources));
                continue;
            }
            if let Some(r) = st.completed_reasoning.get(&idx) {
                outputs.push(json!({"id": r.id, "type": "reasoning", "encrypted_content": r.signature, "summary": [{"type": "summary_text", "text": r.text}]}));
                continue;
            }
            if let Some(m) = st.completed_messages.get(&idx) {
                let mut item = json!({"id": m.id, "type": "message", "status": "completed", "content": [{"type": "output_text", "annotations": [], "logprobs": [], "text": m.text}], "role": "assistant"});
                if !m.annotations.is_empty() {
                    cpa_json::set(&mut item, "content.0.annotations", Value::Array(m.annotations.clone()));
                }
                outputs.push(item);
                continue;
            }
            if let Some(d) = st.detached_reasoning.get(&idx) {
                outputs.push(json!({"id": d.id, "type": "reasoning", "encrypted_content": d.signature, "summary": []}));
                continue;
            }
            let call_id = st.func_call_ids.get(&idx).cloned().unwrap_or_default();
            if !call_id.is_empty() {
                let name = st.func_names.get(&idx).cloned().unwrap_or_default();
                let namespace = st.func_namespaces.get(&idx).cloned().unwrap_or_default();
                if st.func_custom.get(&idx).copied().unwrap_or(false) {
                    let input_str = st.func_input_buf.get(&idx).cloned().unwrap_or_default();
                    let item = json!({"id": format!("ctc_{call_id}"), "type": "custom_tool_call", "status": "completed", "input": input_str, "call_id": call_id, "name": ""});
                    outputs.push(with_tool_identity(item, &name, &namespace));
                } else {
                    let args = match st.func_args_buf.get(&idx) {
                        Some(b) if !b.is_empty() => b.clone(),
                        _ => "{}".to_string(),
                    };
                    let item = json!({"id": format!("fc_{call_id}"), "type": "function_call", "status": "completed", "arguments": args, "call_id": call_id, "name": ""});
                    outputs.push(with_tool_identity(item, &name, &namespace));
                }
            }
        }
        if !outputs.is_empty() {
            cpa_json::set(&mut completed, "response.output", Value::Array(outputs));
        }
        if self.st.web_search_done {
            cpa_json::set(&mut completed, "response.tool_usage.web_search.num_requests", 1);
        }

        let um = root.g("usageMetadata");
        if um.exists() {
            set_usage(&mut completed, "response.usage", &um, true);
        }

        self.push("response.completed", &completed);
        self.st.completed = true;
    }

    /// `response.output_item.done` for a function or custom tool call.
    #[allow(clippy::too_many_arguments)]
    fn tool_item_done_event(&self, seq: i64, idx: usize, item_type: &str, call_id: &str, payload_key: &str, payload: &str, name: &str, namespace: &str) -> Value {
        let id_prefix = if item_type == "custom_tool_call" { "ctc" } else { "fc" };
        let mut item = json!({"id": format!("{id_prefix}_{call_id}"), "type": item_type, "status": "completed"});
        cpa_json::set(&mut item, payload_key, payload);
        cpa_json::set(&mut item, "call_id", call_id);
        cpa_json::set(&mut item, "name", "");
        let item = with_tool_identity(item, name, namespace);
        json!({"type": "response.output_item.done", "sequence_number": seq, "output_index": idx, "item": item})
    }

    /// Handles one Gemini part. Returns false to stop processing the remaining parts.
    fn process_part(&mut self, part_idx_in_chunk: i64, part: &Res<'_>, args_raw: Option<&str>, valid_json: bool) -> bool {
        let mut explicit_part_index: i64 = -1;
        let p = part.g("partIndex");
        if p.exists() {
            explicit_part_index = p.int();
        } else {
            let p = part.g("index");
            if p.exists() {
                explicit_part_index = p.int();
            }
        }

        let mut signature = part.g("thoughtSignature").str().trim().to_string();
        if signature.is_empty() {
            signature = part.g("thought_signature").str().trim().to_string();
        }
        let function_call = part.g("functionCall");
        let text = part.g("text");
        let is_thought = part.g("thought").bool();

        let part_kind = if is_thought {
            "thought"
        } else if function_call.exists() {
            "function"
        } else if text.exists() {
            "text"
        } else {
            "unknown"
        };

        let current_part_index: i64;
        if explicit_part_index >= 0 {
            current_part_index = explicit_part_index;
            self.st.current_logical_part_index = explicit_part_index;
            self.st.current_part_kind = part_kind.to_string();
            self.st.has_seen_first_part = true;
            self.st.text_part_run_active = part_kind == "text";
        } else if !self.st.has_seen_first_part {
            self.st.has_seen_first_part = true;
            self.st.current_logical_part_index = 0;
            self.st.current_part_kind = part_kind.to_string();
            current_part_index = 0;
            if part_kind == "text" {
                self.st.text_part_run_active = true;
            }
        } else {
            if part_idx_in_chunk > 0 || part_kind != self.st.current_part_kind {
                self.st.current_logical_part_index += 1;
                self.st.current_part_kind = part_kind.to_string();
                self.st.text_part_run_active = part_kind == "text";
            } else if part_kind == "function" {
                self.st.current_logical_part_index += 1;
                self.st.current_part_kind = part_kind.to_string();
                self.st.text_part_run_active = false;
            } else if part_kind == "text" && !self.st.text_part_run_active {
                self.st.current_logical_part_index += 1;
                self.st.current_part_kind = part_kind.to_string();
                self.st.text_part_run_active = true;
            }
            current_part_index = self.st.current_logical_part_index;
        }

        let text_str = text.str();
        if function_call.exists() && !self.st.pending_reasoning_signature.is_empty() {
            let pending = self.st.pending_reasoning_signature.clone();
            if signature.is_empty() {
                self.emit_detached_reasoning(&pending, CARRIER_NEXT, CARRIER_FUNCTION);
            } else {
                self.emit_trailing_detached_reasoning(&pending);
            }
            self.st.pending_reasoning_signature.clear();
        }
        let reasoning_active = (self.st.reasoning_opened && !self.st.reasoning_closed)
            || (!self.st.reasoning_opened && (!self.st.reasoning_buf.is_empty() || !self.st.reasoning_enc.is_empty()));
        if !signature.is_empty() && !is_thought {
            if reasoning_active {
                if self.st.reasoning_enc.is_empty() || self.st.reasoning_enc == signature {
                    self.st.reasoning_enc = signature.clone();
                    if function_call.exists() {
                        self.st.reasoning_direction = CARRIER_NEXT.to_string();
                        self.st.reasoning_target_kind = CARRIER_FUNCTION.to_string();
                    } else if text.exists() && !text_str.is_empty() {
                        self.st.reasoning_direction = CARRIER_NEXT.to_string();
                        self.st.reasoning_target_kind = CARRIER_TEXT.to_string();
                    } else {
                        self.st.reasoning_direction = CARRIER_STANDALONE.to_string();
                        self.st.reasoning_target_kind = CARRIER_TEXT.to_string();
                    }
                    self.st.seen_reasoning_signatures.insert(signature.clone());
                } else {
                    self.finalize_reasoning();
                    if function_call.exists() {
                        self.emit_detached_reasoning(&signature, CARRIER_NEXT, CARRIER_FUNCTION);
                    } else if !self.st.seen_reasoning_signatures.contains(&signature) {
                        self.st.pending_reasoning_signature = signature.clone();
                    }
                }
                if text.exists() && text_str.is_empty() && !function_call.exists() {
                    self.finalize_reasoning();
                    return true;
                }
            } else if function_call.exists() {
                self.emit_detached_reasoning(&signature, CARRIER_NEXT, CARRIER_FUNCTION);
            } else if text.exists() && !text_str.is_empty() {
                if !self.st.pending_reasoning_signature.is_empty() && self.st.pending_reasoning_signature != signature {
                    let pending = std::mem::take(&mut self.st.pending_reasoning_signature);
                    self.emit_trailing_detached_reasoning(&pending);
                }
                if !self.st.seen_reasoning_signatures.contains(&signature) {
                    self.st.pending_reasoning_signature = signature.clone();
                }
            } else if text.exists() && text_str.is_empty() {
                if !self.st.pending_reasoning_signature.is_empty() {
                    let pending_signature = std::mem::take(&mut self.st.pending_reasoning_signature);
                    if pending_signature != signature {
                        self.emit_trailing_detached_reasoning(&pending_signature);
                    }
                }
                if self.st.msg_opened || !self.st.func_done.is_empty() || !self.st.web_search_buffered_deltas.is_empty() {
                    self.emit_trailing_detached_reasoning(&signature);
                } else if !self.st.seen_reasoning_signatures.contains(&signature) {
                    self.st.pending_reasoning_signature = signature.clone();
                }
                return true;
            }
        }

        // Reasoning text.
        if is_thought {
            self.process_thought(&signature, &text, &text_str);
            return true;
        }

        // Assistant visible text.
        if text.exists() && !text_str.is_empty() {
            self.process_visible_text(&signature, &text_str, current_part_index);
            return true;
        }

        // Function call.
        if function_call.exists() {
            return self.process_function_call(&function_call, args_raw, explicit_part_index, valid_json);
        }

        true
    }

    fn process_thought(&mut self, signature: &str, text: &Res<'_>, text_str: &str) {
        if !self.st.web_search_buffered_deltas.is_empty() {
            self.finalize_message();
        }
        if !self.st.pending_reasoning_signature.is_empty() && self.st.msg_opened && !self.st.msg_closed {
            let pending = std::mem::take(&mut self.st.pending_reasoning_signature);
            self.emit_trailing_detached_reasoning(&pending);
        }
        let mut incoming_signature = String::new();
        if !signature.is_empty() && signature != GEMINI_RESPONSES_THOUGHT_SIGNATURE {
            if !self.st.pending_reasoning_signature.is_empty() {
                if self.st.pending_reasoning_signature != signature {
                    let pending = self.st.pending_reasoning_signature.clone();
                    self.emit_detached_reasoning(&pending, CARRIER_STANDALONE, CARRIER_ANY);
                }
                self.st.pending_reasoning_signature.clear();
            }
            incoming_signature = signature.to_string();
        } else if !self.st.pending_reasoning_signature.is_empty() {
            incoming_signature = std::mem::take(&mut self.st.pending_reasoning_signature);
        }
        if self.st.reasoning_opened
            && !self.st.reasoning_closed
            && !incoming_signature.is_empty()
            && !self.st.reasoning_enc.is_empty()
            && incoming_signature != self.st.reasoning_enc
        {
            self.finalize_reasoning();
            self.reset_reasoning();
        }
        if self.st.reasoning_closed {
            self.finalize_message();
            self.reset_reasoning();
        } else if !self.st.reasoning_opened && self.st.reasoning_buf.is_empty() && self.st.msg_opened && !self.st.msg_closed {
            self.finalize_message();
        }
        if !incoming_signature.is_empty() {
            self.st.reasoning_enc = incoming_signature.clone();
            self.st.reasoning_direction = CARRIER_STANDALONE.to_string();
            self.st.reasoning_target_kind = CARRIER_TEXT.to_string();
            self.st.seen_reasoning_signatures.insert(incoming_signature);
        }
        if text.exists() && !text_str.is_empty() {
            self.st.last_semantic_kind = CARRIER_TEXT.to_string();
            self.st.reasoning_buf.push_str(text_str);
            if self.st.reasoning_opened {
                let (item_id, index) = (self.st.reasoning_item_id.clone(), self.st.reasoning_index);
                let seq = self.next_seq();
                let msg = json!({"type": "response.reasoning_summary_text.delta", "sequence_number": seq, "item_id": item_id, "output_index": index, "summary_index": 0, "delta": text_str});
                self.push("response.reasoning_summary_text.delta", &msg);
            } else {
                self.st.reasoning_pending_deltas.push(text_str.to_string());
            }
        }
        if !self.st.reasoning_opened && !self.st.reasoning_enc.is_empty() {
            self.open_reasoning();
        }
    }

    fn process_visible_text(&mut self, signature: &str, text: &str, current_part_index: i64) {
        if signature.is_empty()
            && !self.st.pending_reasoning_signature.is_empty()
            && ((self.st.msg_opened && !self.st.msg_closed) || !self.st.web_search_buffered_deltas.is_empty())
        {
            let pending = std::mem::take(&mut self.st.pending_reasoning_signature);
            self.emit_trailing_detached_reasoning(&pending);
        }
        // Responses output items are sequential: finish reasoning before opening the visible
        // message. A signature that arrives later is cached with the message and recombined on
        // replay.
        self.finalize_reasoning();

        if self.st.msg_closed {
            self.st.msg_opened = false;
            self.st.msg_closed = false;
            self.st.item_text_buf.clear();
            self.st.current_msg_rune_offset = 0;
        }

        // In web search stream mode, buffer deltas until web_search_call is finalized (stream end
        // or a later output item) so the completed search item includes incremental sources and
        // strictly precedes the message.
        if self.st.web_search_stream_mode && !self.st.web_search_done {
            self.st.last_semantic_kind = CARRIER_TEXT.to_string();
            self.st.web_search_buffered_deltas.push(text.to_string());
            match self.st.web_search_buffered_parts.last_mut() {
                Some(last) if last.part_index == current_part_index => last.text.push_str(text),
                _ => self.st.web_search_buffered_parts.push(StreamBufferedPart { part_index: current_part_index, text: text.to_string() }),
            }
            self.st.text_part_run_active = true;
            return;
        }

        if !self.st.msg_opened {
            self.open_message();
        }
        self.st.last_semantic_kind = CARRIER_TEXT.to_string();
        self.st.item_text_buf.push_str(text);
        self.record_part_mapping(current_part_index, text);
        self.push_text_delta(text);
        self.st.text_part_run_active = true;
    }

    fn process_function_call(&mut self, fc: &Res<'_>, args_raw: Option<&str>, explicit_part_index: i64, valid_json: bool) -> bool {
        // Before emitting function-call outputs, finalize reasoning, web search, and the message
        // (if open): Responses streaming requires message done events before the next
        // output_item.added.
        self.finalize_reasoning();
        self.finalize_web_search();
        if !self.st.web_search_buffered_deltas.is_empty() {
            self.flush_web_search_buffered_text();
        }
        self.finalize_message();
        self.st.last_semantic_kind = CARRIER_FUNCTION.to_string();

        let args = fc.g("args");
        // Raw argument text as sent (gjson `Raw`): whitespace and duplicate keys preserved.
        let args_text = if args.exists() { args_raw.map(str::to_string).unwrap_or_else(|| args.raw()) } else { String::new() };
        let evidence_idx = record_function_evidence(&mut self.st.evidence, &self.st.tool_identity_map, fc, &args_text, explicit_part_index, valid_json);
        {
            let evidence = &self.st.evidence.entries[evidence_idx];
            if evidence.apply_patch && evidence.err.is_some() {
                let err = evidence.err.clone().unwrap_or_default();
                self.fail(err);
                return false;
            }
            if evidence.raw_name.is_empty() {
                return true;
            }
        }
        let (evidence_apply_patch, evidence_raw_name, evidence_upstream_id) = {
            let e = &self.st.evidence.entries[evidence_idx];
            (e.apply_patch, e.raw_name.clone(), e.upstream_id.clone())
        };
        let mut raw_name = fc.g("name").str();
        if evidence_apply_patch {
            raw_name = evidence_raw_name;
        }
        let identity = match self.st.tool_identity_map.get(&raw_name) {
            Some(i) => i.clone(),
            None => ResponsesToolIdentity { name: restore_sanitized_tool_name(&self.st.sanitized_name_map, &raw_name), ..Default::default() },
        };
        let name = identity.name.clone();
        let namespace = identity.namespace.clone();
        let is_custom = identity.custom;
        if evidence_apply_patch
            && let Some(patch_call) = self.st.evidence.entries[evidence_idx].patch_call.as_mut() {
                if let Err(err) = patch_call.finish_arguments(&args_text) {
                    self.fail(err);
                    return false;
                }
                return true;
            }

        let idx = self.st.next_index;
        self.st.next_index += 1;
        self.st.func_args_buf.entry(idx).or_default();
        if identity.apply_patch {
            self.st.func_call_ids.insert(idx, evidence_upstream_id);
        }
        if self.st.func_call_ids.get(&idx).is_none_or(|id| id.is_empty()) {
            self.st.func_call_ids.insert(idx, format!("call_{}_{}", unix_nano_now(), next_func_call_id_counter()));
        }
        self.st.func_names.insert(idx, name.clone());
        self.st.func_namespaces.insert(idx, namespace.clone());
        self.st.func_custom.insert(idx, is_custom);
        let call_id = self.st.func_call_ids.get(&idx).cloned().unwrap_or_default();

        let args_json = if args.exists() { args_text.clone() } else { "{}".to_string() };
        if let Some(buf) = self.st.func_args_buf.get_mut(&idx)
            && buf.is_empty() && !args_json.is_empty() {
                buf.push_str(&args_json);
            }

        if is_custom {
            let mut input_str = unwrap_responses_custom_tool_input(&args_json);
            let mut patch_call: Option<ApplyPatchCallState> = None;
            if identity.apply_patch {
                let mut pc = ApplyPatchCallState {
                    item_id: format!("ctc_{call_id}"),
                    call_id: call_id.clone(),
                    name: name.clone(),
                    namespace: namespace.clone(),
                    output_index: idx as i64,
                    ..Default::default()
                };
                let finished = pc.finish_arguments(&args_json);
                let finished = if !valid_json { Err("invalid Gemini apply_patch response JSON".to_string()) } else { finished };
                match finished {
                    Err(err) => {
                        self.fail(err);
                        return false;
                    }
                    Ok((_, input)) => input_str = input,
                }
                patch_call = Some(pc);
            }
            self.st.func_input_buf.insert(idx, input_str.clone());

            // item.added for the custom tool call.
            let seq = self.next_seq();
            let item = with_tool_identity(
                json!({"id": format!("ctc_{call_id}"), "type": "custom_tool_call", "status": "in_progress", "input": "", "call_id": call_id, "name": ""}),
                &name,
                &namespace,
            );
            let added = json!({"type": "response.output_item.added", "sequence_number": seq, "output_index": idx, "item": item});
            self.push("response.output_item.added", &added);

            // Gemini delivers complete arguments; this delta is not an early preview.
            if let Some(pc) = &patch_call
                && !input_str.is_empty() {
                    let seq = self.next_seq();
                    let delta = cpa_json::parse(&apply_patch_input_delta(pc, &input_str, seq));
                    self.push("response.custom_tool_call_input.delta", &delta);
                }
            if !self.st.func_done.get(&idx).copied().unwrap_or(false) {
                let seq = self.next_seq();
                let input_done = match &patch_call {
                    Some(pc) => cpa_json::parse(&apply_patch_input_done(pc, &input_str, seq)),
                    None => json!({"type": "response.custom_tool_call_input.done", "sequence_number": seq, "item_id": format!("ctc_{call_id}"), "output_index": idx, "input": input_str}),
                };
                self.push("response.custom_tool_call_input.done", &input_done);

                let seq = self.next_seq();
                let item_done = self.tool_item_done_event(seq, idx, "custom_tool_call", &call_id, "input", &input_str, &name, &namespace);
                self.push("response.output_item.done", &item_done);
                self.st.func_done.insert(idx, true);
            }
            if patch_call.is_some() {
                self.st.evidence.entries[evidence_idx].patch_call = patch_call;
            }
        } else {
            let seq = self.next_seq();
            let item = with_tool_identity(
                json!({"id": format!("fc_{call_id}"), "type": "function_call", "status": "in_progress", "arguments": "", "call_id": call_id, "name": ""}),
                &name,
                &namespace,
            );
            let added = json!({"type": "response.output_item.added", "sequence_number": seq, "output_index": idx, "item": item});
            self.push("response.output_item.added", &added);

            // Full args in one chunk; "{}" when Gemini omits args keeps the Responses event
            // order consistent.
            if !args_json.is_empty() {
                let seq = self.next_seq();
                let ad = json!({"type": "response.function_call_arguments.delta", "sequence_number": seq, "item_id": format!("fc_{call_id}"), "output_index": idx, "delta": args_json});
                self.push("response.function_call_arguments.delta", &ad);
            }

            // Gemini emits the full function call payload at once, so it is finalized now.
            if !self.st.func_done.get(&idx).copied().unwrap_or(false) {
                let seq = self.next_seq();
                let fc_done = json!({"type": "response.function_call_arguments.done", "sequence_number": seq, "item_id": format!("fc_{call_id}"), "output_index": idx, "arguments": args_json});
                self.push("response.function_call_arguments.done", &fc_done);

                let seq = self.next_seq();
                let item_done = self.tool_item_done_event(seq, idx, "function_call", &call_id, "arguments", &args_json, &name, &namespace);
                self.push("response.output_item.done", &item_done);
                self.st.func_done.insert(idx, true);
            }
        }
        true
    }
}

/// Sets the resolved tool name and namespace on a tool call item (Go:
/// SetResponsesToolCallIdentity with an empty item path).
pub(super) fn with_tool_identity(item: Value, name: &str, namespace: &str) -> Value {
    cpa_json::parse(&set_responses_tool_call_identity(&cpa_json::to_vec(&item), name, namespace, ""))
}
