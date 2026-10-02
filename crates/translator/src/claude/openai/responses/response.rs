//! Claude Messages responses to OpenAI Responses events and objects
//! (Go: claude/openai/responses/claude_openai-responses_response.go).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use cpa_core::applypatch;
use cpa_json::{J, Res, Value};

use super::tool_names::{
    ClaudeToolNames, ToolDescriptor, ToolWinners, build_claude_tool_names_with_winners, responses_tool_winners,
    split_responses_qualified_function_call,
};
use super::web_search::{
    CLAUDE_WEB_SEARCH_TOOL_NAME, build_responses_web_search_call_item, claude_web_search_query,
    claude_web_search_results_to_responses, responses_web_search_call_id,
};
use crate::claude::openai::chat_completions::raw_at;
use crate::common::{self, ApplyPatchCallState, ApplyPatchErrorState};
use crate::registry::{Ctx, Param};

const DATA_TAG: &[u8] = b"data:";

/// Marks a Responses reasoning item whose `encrypted_content` carries an Anthropic
/// `redacted_thinking` payload instead of a thinking signature. Responses has no redacted
/// reasoning item, and Anthropic needs redacted blocks replayed verbatim, so the payload rides in
/// `encrypted_content` behind this marker. The marker is not a valid signature for any provider,
/// so a foreign upstream drops the block instead of replaying an unusable value.
pub const CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX: &str = "claude-redacted-thinking:";

const CONFLICTING_IDENTITY: &str = "conflicting apply_patch call identity";

/// Streaming conversion state, kept across the lines of one response. Maps keyed by `i64` are
/// keyed by Claude content block index and ordered, so iteration is deterministic.
struct State {
    err: ApplyPatchErrorState,
    apply_patch_calls: BTreeMap<i64, ApplyPatchCallState>,
    tool_winners: ToolWinners,
    tool_names: ClaudeToolNames,
    func_item_added: BTreeSet<i64>,
    func_args_sent: BTreeMap<i64, usize>,
    func_block_stopped: BTreeSet<i64>,
    func_input_snapshot: BTreeMap<i64, String>,
    func_input_snapshot_errors: BTreeMap<i64, String>,
    func_identity_conflicts: BTreeSet<i64>,
    completed_emitted: bool,
    seq: i64,
    response_id: String,
    created_at: i64,
    next_output_index: i64,
    current_msg_id: String,
    current_fc_id: String,
    in_text_block: bool,
    in_func_block: bool,
    message_open: bool,
    content_part_open: bool,
    /// -1 until the open message gets an output index.
    message_output_index: i64,
    func_args_buf: BTreeMap<i64, String>,
    func_args_done: BTreeSet<i64>,
    func_item_done: BTreeSet<i64>,
    func_item_status: BTreeMap<i64, String>,
    func_names: BTreeMap<i64, String>,
    func_call_ids: BTreeMap<i64, String>,
    func_custom: BTreeSet<i64>,
    func_output_indices: BTreeMap<i64, i64>,
    text_buf: String,
    current_text_buf: String,
    message_annotations: Vec<Value>,
    message_items: Vec<MessageItem>,
    reasoning_active: bool,
    reasoning_deltas_done: bool,
    reasoning_item_id: String,
    reasoning_buf: String,
    reasoning_signature: String,
    reasoning_index: i64,
    reasoning_items: Vec<ReasoningItem>,
    // Server-side web search: a `server_tool_use` block and its `web_search_tool_result` block
    // fold into one `web_search_call` item. Maps point into `web_search_items`.
    web_search_by_block: BTreeMap<i64, usize>,
    web_search_by_tool_id: HashMap<String, usize>,
    web_search_items: Vec<WebSearchItem>,
    stop_reason: String,
    usage: UsageTokens,
}

struct WebSearchItem {
    tool_use_id: String,
    output_index: i64,
    input_buf: String,
    results: Option<Value>,
    emitted: bool,
    status: String,
}

impl WebSearchItem {
    fn render(&self) -> Value {
        build_responses_web_search_call_item(
            &self.tool_use_id,
            &claude_web_search_query(&self.input_buf),
            self.results.as_ref(),
        )
    }
}

struct MessageItem {
    id: String,
    output_index: i64,
    text: String,
    annotations: Vec<Value>,
    status: String,
}

struct ReasoningItem {
    id: String,
    output_index: i64,
    text: String,
    signature: String,
    status: String,
}

#[derive(Default)]
struct UsageTokens {
    input_tokens: i64,
    output_tokens: i64,
    cache_creation_input_tokens: i64,
    cache_read_input_tokens: i64,
    has_usage: bool,
}

impl UsageTokens {
    /// Folds a Claude `usage` object in; fields that are present replace earlier values.
    fn merge(&mut self, usage: &Res<'_>) {
        if !usage.exists() {
            return;
        }
        self.has_usage = true;
        for (field, slot) in [
            ("input_tokens", &mut self.input_tokens),
            ("output_tokens", &mut self.output_tokens),
            ("cache_creation_input_tokens", &mut self.cache_creation_input_tokens),
            ("cache_read_input_tokens", &mut self.cache_read_input_tokens),
        ] {
            let v = usage.g(field);
            if v.exists() {
                *slot = v.int();
            }
        }
    }

    /// Responses usage: input (including cache creation and reads), output, total, cached tokens.
    fn openai_responses_usage(&self) -> (i64, i64, i64, i64) {
        let cached = self.cache_read_input_tokens;
        let input = self.input_tokens + self.cache_creation_input_tokens + cached;
        (input, self.output_tokens, input + self.output_tokens, cached)
    }
}

/// The `encrypted_content` value for the Responses reasoning item mirroring a Claude thinking or
/// redacted_thinking block. Streaming thinking blocks announce an empty signature and fill it in
/// through `signature_delta`, so an empty result is expected and later replaced.
fn claude_reasoning_carrier(content_block: &Res<'_>) -> String {
    if content_block.g("type").str() == "redacted_thinking" {
        let data = content_block.g("data");
        if data.exists() && !data.str().is_empty() {
            return format!("{CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX}{}", data.str());
        }
        return String::new();
    }
    let signature = content_block.g("signature");
    if signature.exists() { signature.str() } else { String::new() }
}

/// `response.incomplete` details when the stop reason is `max_tokens`.
fn incomplete_details(stop_reason: &str) -> Option<Value> {
    stop_reason
        .trim()
        .eq_ignore_ascii_case("max_tokens")
        .then(|| cpa_json::parse_str(r#"{"reason":"max_output_tokens"}"#))
}

fn output_status(stop_reason: &str) -> &'static str {
    if incomplete_details(stop_reason).is_some() { "incomplete" } else { "completed" }
}

/// Terminal event type, response status and incomplete details for a stop reason.
fn terminal_state(stop_reason: &str) -> (&'static str, &'static str, Option<Value>) {
    match incomplete_details(stop_reason) {
        Some(details) => ("response.incomplete", "incomplete", Some(details)),
        None => ("response.completed", "completed", None),
    }
}

/// The original request when it is valid JSON, else the translated request, else nothing.
fn pick_request_json<'a>(original: &'a [u8], request: &'a [u8]) -> &'a [u8] {
    if !original.is_empty() && cpa_json::valid(original) {
        return original;
    }
    if !request.is_empty() && cpa_json::valid(request) {
        return request;
    }
    &[]
}

fn emit_event(event: &str, payload: &Value) -> Vec<u8> {
    common::sse_event_data(event, &cpa_json::to_vec(payload))
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

fn template(json: &str) -> Value {
    cpa_json::parse_str(json)
}

/// Rejects a patch tool input snapshot unless it parses as a consistent complete patch: equivalent
/// JSON spellings are fine, but one complete input never silently replaces another.
fn validate_apply_patch_snapshots(previous: &str, current: &str) -> Result<(), String> {
    let mut call = ApplyPatchCallState::default();
    if !previous.is_empty() {
        call.finish_arguments(previous)?;
    }
    call.finish_arguments(current).map(|_| ())
}

/// Treats complete streamed JSON as a complete snapshot, not a prefix a later snapshot may extend.
/// A partial source can still be completed by a consistent full snapshot. Returns the unsent tail
/// and the full decoded input.
fn finish_claude_apply_patch_arguments(
    call: &mut ApplyPatchCallState,
    arguments: &str,
    snapshot: &str,
) -> Result<(String, String), String> {
    if snapshot.is_empty() {
        return call.finish_arguments(arguments);
    }
    if cpa_json::valid(arguments.as_bytes()) {
        call.finish_arguments(arguments)?;
    }
    call.finish_arguments(snapshot)
}

impl State {
    fn new(request_json: &[u8]) -> Self {
        let root = cpa_json::parse(request_json);
        let winners = responses_tool_winners(&root);
        State {
            err: ApplyPatchErrorState::default(),
            apply_patch_calls: BTreeMap::new(),
            tool_names: build_claude_tool_names_with_winners(&root, &winners),
            tool_winners: winners,
            func_item_added: BTreeSet::new(),
            func_args_sent: BTreeMap::new(),
            func_block_stopped: BTreeSet::new(),
            func_input_snapshot: BTreeMap::new(),
            func_input_snapshot_errors: BTreeMap::new(),
            func_identity_conflicts: BTreeSet::new(),
            completed_emitted: false,
            seq: 0,
            response_id: String::new(),
            created_at: 0,
            next_output_index: 0,
            current_msg_id: String::new(),
            current_fc_id: String::new(),
            in_text_block: false,
            in_func_block: false,
            message_open: false,
            content_part_open: false,
            message_output_index: -1,
            func_args_buf: BTreeMap::new(),
            func_args_done: BTreeSet::new(),
            func_item_done: BTreeSet::new(),
            func_item_status: BTreeMap::new(),
            func_names: BTreeMap::new(),
            func_call_ids: BTreeMap::new(),
            func_custom: BTreeSet::new(),
            func_output_indices: BTreeMap::new(),
            text_buf: String::new(),
            current_text_buf: String::new(),
            message_annotations: Vec::new(),
            message_items: Vec::new(),
            reasoning_active: false,
            reasoning_deltas_done: false,
            reasoning_item_id: String::new(),
            reasoning_buf: String::new(),
            reasoning_signature: String::new(),
            reasoning_index: -1,
            reasoning_items: Vec::new(),
            web_search_by_block: BTreeMap::new(),
            web_search_by_tool_id: HashMap::new(),
            web_search_items: Vec::new(),
            stop_reason: String::new(),
            usage: UsageTokens::default(),
        }
    }

    fn next_seq(&mut self) -> i64 {
        self.seq += 1;
        self.seq
    }

    fn has_error(&self) -> bool {
        self.err.tool_input_error().is_some()
    }

    fn allocate_output_index(&mut self) -> i64 {
        let index = self.next_output_index;
        self.next_output_index += 1;
        index
    }

    fn message_output_index(&mut self) -> i64 {
        if self.message_output_index < 0 {
            self.message_output_index = self.allocate_output_index();
        }
        self.message_output_index
    }

    fn function_output_index(&mut self, block_index: i64) -> i64 {
        if let Some(index) = self.func_output_indices.get(&block_index) {
            return *index;
        }
        let index = self.allocate_output_index();
        self.func_output_indices.insert(block_index, index);
        index
    }

    fn func_name(&self, idx: i64) -> String {
        self.func_names.get(&idx).cloned().unwrap_or_default()
    }

    fn func_call_id(&self, idx: i64) -> String {
        self.func_call_ids.get(&idx).cloned().unwrap_or_default()
    }

    /// The winning declaration for a Claude tool name, if the original request declared one.
    fn winner(&self, claude_name: &str) -> Option<&ToolDescriptor> {
        self.tool_winners.get(&self.tool_names.identity(claude_name))
    }

    /// Whether the Claude tool name is the bridged custom `apply_patch` tool, judged by the
    /// original request's winning declaration.
    fn is_apply_patch(&self, name: &str) -> bool {
        self.winner(name).is_some_and(|d| d.tool_type == "custom" && applypatch::is_custom_tool(&d.tool))
    }

    /// Opens the Responses item standing for a Claude server-side search block.
    fn start_web_search(&mut self, block_index: i64, tool_use_id: &str) -> usize {
        let output_index = self.allocate_output_index();
        self.web_search_items.push(WebSearchItem {
            tool_use_id: tool_use_id.to_string(),
            output_index,
            input_buf: String::new(),
            results: None,
            emitted: false,
            status: String::new(),
        });
        let slot = self.web_search_items.len() - 1;
        self.web_search_by_block.insert(block_index, slot);
        self.web_search_by_tool_id.insert(tool_use_id.to_string(), slot);
        slot
    }

    /// Emits `output_item.done` once the result block has been seen, or at message_stop when the
    /// turn ended without one.
    fn finalize_web_search(&mut self, slot: usize, status: &str) -> Vec<Vec<u8>> {
        if self.web_search_items[slot].emitted {
            return vec![];
        }
        let seq = self.next_seq();
        let item = &mut self.web_search_items[slot];
        item.emitted = true;
        item.status = status.to_string();
        let mut done =
            template(r#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{}}"#);
        cpa_json::set(&mut done, "sequence_number", seq);
        cpa_json::set(&mut done, "output_index", item.output_index);
        let mut rendered = item.render();
        cpa_json::set(&mut rendered, "status", status);
        cpa_json::set(&mut done, "item", rendered);
        vec![emit_event("response.output_item.done", &done)]
    }

    /// Records a tool input error and emits the terminal `response.failed` event (once).
    fn fail_tool_input(&mut self, err: &str) -> Vec<Vec<u8>> {
        if self.has_error() {
            return vec![];
        }
        self.err.set_tool_input_error(err);
        let seq = self.next_seq();
        vec![common::sse_event_data("response.failed", &common::apply_patch_failure(&self.response_id, seq))]
    }

    /// Sets the client-declared `name` and `namespace` on a function/custom call item.
    fn apply_function_call_namespace_fields(&self, item: Value, qualified_name: &str, item_path: &str) -> Value {
        let (name, namespace) =
            split_responses_qualified_function_call(&self.tool_winners, &self.tool_names, qualified_name);
        cpa_json::parse(&common::set_responses_tool_call_identity(
            &cpa_json::to_vec(&item),
            &name,
            &namespace,
            item_path,
        ))
    }

    fn emit_func_item(&mut self, idx: i64, force: bool) -> Vec<Vec<u8>> {
        if self.func_item_added.contains(&idx) || self.has_error() {
            return vec![];
        }
        let mut name = self.func_name(idx);
        let mut call_id = self.func_call_id(idx);
        if force && name.is_empty() && self.tool_winners.len() == 1 {
            let identities: Vec<String> = self.tool_winners.keys().cloned().collect();
            for identity in identities {
                if self.is_apply_patch(&identity) {
                    name = self.tool_names.claude_name(&identity);
                    self.func_names.insert(idx, name.clone());
                }
            }
        }
        if self.is_apply_patch(&name) {
            if self.func_identity_conflicts.contains(&idx) {
                return self.fail_tool_input(CONFLICTING_IDENTITY);
            }
            if let Some(err) = self.func_input_snapshot_errors.get(&idx).cloned() {
                return self.fail_tool_input(&err);
            }
        }
        if !force && (name.is_empty() || call_id.is_empty()) {
            return vec![];
        }
        if call_id.is_empty() {
            call_id = format!("call_{}_{idx}", self.response_id);
            self.func_call_ids.insert(idx, call_id.clone());
        }
        let d = self.winner(&name).cloned();
        let is_custom = d.as_ref().is_some_and(|d| d.tool_type == "custom");
        if is_custom {
            self.func_custom.insert(idx);
        } else {
            self.func_custom.remove(&idx);
        }
        let output_index = self.function_output_index(idx);
        let mut item;
        let item_id;
        if is_custom {
            item = template(
                r#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"custom_tool_call","status":"in_progress","input":"","call_id":"","name":""}}"#,
            );
            item_id = format!("ctc_{call_id}");
            if self.is_apply_patch(&name)
                && let Some(d) = &d
            {
                let local_name = if d.direct { d.name.clone() } else { d.child_name.clone() };
                self.apply_patch_calls.insert(
                    idx,
                    ApplyPatchCallState {
                        item_id: item_id.clone(),
                        call_id: call_id.clone(),
                        name: local_name,
                        namespace: d.namespace.clone(),
                        output_index,
                        ..Default::default()
                    },
                );
            }
        } else {
            item = template(
                r#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"function_call","status":"in_progress","arguments":"","call_id":"","name":""}}"#,
            );
            item_id = format!("fc_{call_id}");
        }
        cpa_json::set(&mut item, "item.id", item_id);
        cpa_json::set(&mut item, "item.call_id", call_id);
        let mut item = self.apply_function_call_namespace_fields(item, &name, "item");
        let seq = self.next_seq();
        cpa_json::set(&mut item, "sequence_number", seq);
        cpa_json::set(&mut item, "output_index", output_index);
        self.func_item_added.insert(idx);
        vec![emit_event("response.output_item.added", &item)]
    }

    fn emit_pending_func_args(&mut self, idx: i64) -> Vec<Vec<u8>> {
        if !self.func_item_added.contains(&idx) || self.has_error() {
            return vec![];
        }
        let sent = self.func_args_sent.get(&idx).copied().unwrap_or(0);
        let Some(buf) = self.func_args_buf.get(&idx) else { return vec![] };
        if buf.len() <= sent {
            return vec![];
        }
        let fragment = buf[sent..].to_string();
        self.func_args_sent.insert(idx, buf.len());
        if self.func_custom.contains(&idx) {
            if let Some(patch_call) = self.apply_patch_calls.get_mut(&idx) {
                match patch_call.push_arguments(&fragment) {
                    Err(err) => return self.fail_tool_input(&err),
                    Ok(delta) if !delta.is_empty() => {
                        let seq = self.next_seq();
                        let patch_call = &self.apply_patch_calls[&idx];
                        return vec![common::sse_event_data(
                            "response.custom_tool_call_input.delta",
                            &common::apply_patch_input_delta(patch_call, &delta, seq),
                        )];
                    }
                    Ok(_) => {}
                }
            }
            return vec![];
        }
        let seq = self.next_seq();
        let output_index = self.function_output_index(idx);
        let mut msg = template(
            r#"{"type":"response.function_call_arguments.delta","sequence_number":0,"item_id":"","output_index":0,"delta":""}"#,
        );
        cpa_json::set(&mut msg, "sequence_number", seq);
        cpa_json::set(&mut msg, "item_id", format!("fc_{}", self.func_call_id(idx)));
        cpa_json::set(&mut msg, "output_index", output_index);
        cpa_json::set(&mut msg, "delta", fragment);
        vec![emit_event("response.function_call_arguments.delta", &msg)]
    }

    fn finalize_func_item(&mut self, idx: i64, status: &str) -> Vec<Vec<u8>> {
        if self.func_item_done.contains(&idx) || self.has_error() {
            return vec![];
        }
        let mut out = self.emit_func_item(idx, true);
        out.extend(self.emit_pending_func_args(idx));
        if self.has_error() {
            return out;
        }
        self.func_item_done.insert(idx);
        self.func_item_status.insert(idx, status.to_string());

        let output_index = self.function_output_index(idx);
        let buf = self.func_args_buf.get(&idx).cloned().unwrap_or_default();
        let is_custom = self.func_custom.contains(&idx);
        let mut args = buf;
        if !is_custom && args.is_empty() && status == "completed" {
            args = "{}".to_string();
        }
        let mut call_id = self.func_call_id(idx);
        if call_id.is_empty() {
            call_id = self.current_fc_id.clone();
        }
        let name = self.func_name(idx);

        if is_custom {
            let snapshot = self.func_input_snapshot.get(&idx).cloned().unwrap_or_default();
            let finished = self
                .apply_patch_calls
                .get_mut(&idx)
                .map(|call| finish_claude_apply_patch_arguments(call, &args, &snapshot));
            let input = match finished {
                Some(Err(err)) => {
                    out.extend(self.fail_tool_input(&err));
                    return out;
                }
                Some(Ok((tail, full_input))) => {
                    if !tail.is_empty() {
                        let seq = self.next_seq();
                        out.push(common::sse_event_data(
                            "response.custom_tool_call_input.delta",
                            &common::apply_patch_input_delta(&self.apply_patch_calls[&idx], &tail, seq),
                        ));
                    }
                    full_input
                }
                None => unwrap_custom_tool_input(&args),
            };
            if self.func_args_done.insert(idx) {
                let seq = self.next_seq();
                if let Some(patch_call) = self.apply_patch_calls.get(&idx) {
                    out.push(common::sse_event_data(
                        "response.custom_tool_call_input.done",
                        &common::apply_patch_input_done(patch_call, &input, seq),
                    ));
                } else {
                    let mut input_done = template(
                        r#"{"type":"response.custom_tool_call_input.done","sequence_number":0,"item_id":"","output_index":0,"input":""}"#,
                    );
                    cpa_json::set(&mut input_done, "sequence_number", seq);
                    cpa_json::set(&mut input_done, "item_id", format!("ctc_{call_id}"));
                    cpa_json::set(&mut input_done, "output_index", output_index);
                    cpa_json::set(&mut input_done, "input", input.as_str());
                    out.push(emit_event("response.custom_tool_call_input.done", &input_done));
                }
            }

            let mut item_done = template(
                r#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}}"#,
            );
            let seq = self.next_seq();
            cpa_json::set(&mut item_done, "sequence_number", seq);
            cpa_json::set(&mut item_done, "output_index", output_index);
            cpa_json::set(&mut item_done, "item.id", format!("ctc_{call_id}"));
            cpa_json::set(&mut item_done, "item.status", status);
            cpa_json::set(&mut item_done, "item.input", input);
            cpa_json::set(&mut item_done, "item.call_id", call_id);
            let item_done = self.apply_function_call_namespace_fields(item_done, &name, "item");
            out.push(emit_event("response.output_item.done", &item_done));
        } else {
            if self.func_args_done.insert(idx) {
                let mut fc_done = template(
                    r#"{"type":"response.function_call_arguments.done","sequence_number":0,"item_id":"","output_index":0,"arguments":""}"#,
                );
                let seq = self.next_seq();
                cpa_json::set(&mut fc_done, "sequence_number", seq);
                cpa_json::set(&mut fc_done, "item_id", format!("fc_{call_id}"));
                cpa_json::set(&mut fc_done, "output_index", output_index);
                cpa_json::set(&mut fc_done, "arguments", args.as_str());
                out.push(emit_event("response.function_call_arguments.done", &fc_done));
            }

            let mut item_done = template(
                r#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}}"#,
            );
            let seq = self.next_seq();
            cpa_json::set(&mut item_done, "sequence_number", seq);
            cpa_json::set(&mut item_done, "output_index", output_index);
            cpa_json::set(&mut item_done, "item.id", format!("fc_{call_id}"));
            cpa_json::set(&mut item_done, "item.status", status);
            cpa_json::set(&mut item_done, "item.arguments", args);
            cpa_json::set(&mut item_done, "item.call_id", call_id);
            let item_done = self.apply_function_call_namespace_fields(item_done, &name, "item");
            out.push(emit_event("response.output_item.done", &item_done));
        }
        self.in_func_block = false;
        out
    }

    fn finalize_reasoning_deltas(&mut self) -> Vec<Vec<u8>> {
        if !self.reasoning_active || self.reasoning_deltas_done {
            return vec![];
        }
        self.reasoning_deltas_done = true;
        let full = self.reasoning_buf.clone();
        let mut out = Vec::new();
        let mut text_done = template(
            r#"{"type":"response.reasoning_summary_text.done","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"text":""}"#,
        );
        let seq = self.next_seq();
        cpa_json::set(&mut text_done, "sequence_number", seq);
        cpa_json::set(&mut text_done, "item_id", self.reasoning_item_id.as_str());
        cpa_json::set(&mut text_done, "output_index", self.reasoning_index);
        cpa_json::set(&mut text_done, "text", full.as_str());
        out.push(emit_event("response.reasoning_summary_text.done", &text_done));
        let mut part_done = template(
            r#"{"type":"response.reasoning_summary_part.done","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}"#,
        );
        let seq = self.next_seq();
        cpa_json::set(&mut part_done, "sequence_number", seq);
        cpa_json::set(&mut part_done, "item_id", self.reasoning_item_id.as_str());
        cpa_json::set(&mut part_done, "output_index", self.reasoning_index);
        cpa_json::set(&mut part_done, "part.text", full);
        out.push(emit_event("response.reasoning_summary_part.done", &part_done));
        out
    }

    fn finalize_reasoning_item(&mut self, status: &str) -> Vec<Vec<u8>> {
        if !self.reasoning_active && self.reasoning_item_id.is_empty() {
            return vec![];
        }
        let mut out = self.finalize_reasoning_deltas();

        let full = std::mem::take(&mut self.reasoning_buf);
        let mut item_done = template(
            r#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"reasoning","status":"completed","encrypted_content":"","summary":[]}}"#,
        );
        let seq = self.next_seq();
        cpa_json::set(&mut item_done, "sequence_number", seq);
        cpa_json::set(&mut item_done, "output_index", self.reasoning_index);
        cpa_json::set(&mut item_done, "item.id", self.reasoning_item_id.as_str());
        cpa_json::set(&mut item_done, "item.status", status);
        cpa_json::set(&mut item_done, "item.encrypted_content", self.reasoning_signature.as_str());
        let mut summary = template(r#"{"type":"summary_text","text":""}"#);
        cpa_json::set(&mut summary, "text", full.as_str());
        cpa_json::set(&mut item_done, "item.summary", Value::Array(vec![summary]));
        out.push(emit_event("response.output_item.done", &item_done));
        self.reasoning_items.push(ReasoningItem {
            id: std::mem::take(&mut self.reasoning_item_id),
            output_index: self.reasoning_index,
            text: full,
            signature: std::mem::take(&mut self.reasoning_signature),
            status: status.to_string(),
        });
        self.reasoning_active = false;
        self.reasoning_index = -1;
        out
    }

    fn finalize_assistant_message(&mut self) -> Vec<Vec<u8>> {
        if !self.message_open {
            return vec![];
        }
        let full_text = self.text_buf.clone();
        let output_index = self.message_output_index();
        let status = output_status(&self.stop_reason);
        let mut out = Vec::new();

        let mut done = template(
            r#"{"type":"response.output_text.done","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"text":"","logprobs":[]}"#,
        );
        let seq = self.next_seq();
        cpa_json::set(&mut done, "sequence_number", seq);
        cpa_json::set(&mut done, "item_id", self.current_msg_id.as_str());
        cpa_json::set(&mut done, "output_index", output_index);
        cpa_json::set(&mut done, "text", full_text.as_str());
        out.push(emit_event("response.output_text.done", &done));

        let mut part_done = template(
            r#"{"type":"response.content_part.done","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}"#,
        );
        let seq = self.next_seq();
        cpa_json::set(&mut part_done, "sequence_number", seq);
        cpa_json::set(&mut part_done, "item_id", self.current_msg_id.as_str());
        cpa_json::set(&mut part_done, "output_index", output_index);
        cpa_json::set(&mut part_done, "part.text", full_text.as_str());
        if !self.message_annotations.is_empty() {
            cpa_json::set(&mut part_done, "part.annotations", Value::Array(self.message_annotations.clone()));
        }
        out.push(emit_event("response.content_part.done", &part_done));

        let mut fin = template(
            r#"{"type":"response.output_item.done","sequence_number":0,"output_index":0,"item":{"id":"","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":""}],"role":"assistant"}}"#,
        );
        let seq = self.next_seq();
        cpa_json::set(&mut fin, "sequence_number", seq);
        cpa_json::set(&mut fin, "output_index", output_index);
        cpa_json::set(&mut fin, "item.id", self.current_msg_id.as_str());
        cpa_json::set(&mut fin, "item.status", status);
        cpa_json::set(&mut fin, "item.content.0.text", full_text.as_str());
        if !self.message_annotations.is_empty() {
            cpa_json::set(&mut fin, "item.content.0.annotations", Value::Array(self.message_annotations.clone()));
        }
        out.push(emit_event("response.output_item.done", &fin));

        self.message_items.push(MessageItem {
            id: std::mem::take(&mut self.current_msg_id),
            output_index,
            text: full_text,
            annotations: std::mem::take(&mut self.message_annotations),
            status: status.to_string(),
        });
        self.in_text_block = false;
        self.message_open = false;
        self.content_part_open = false;
        self.message_output_index = -1;
        self.text_buf.clear();
        self.current_text_buf.clear();
        out
    }

    /// Resets per-message aggregation at `message_start` (tool declarations, sequence counter,
    /// error and web search bookkeeping persist).
    fn reset_for_message(&mut self) {
        self.text_buf.clear();
        self.current_text_buf.clear();
        self.message_annotations.clear();
        self.message_items.clear();
        self.reasoning_buf.clear();
        self.reasoning_active = false;
        self.reasoning_deltas_done = false;
        self.next_output_index = 0;
        self.in_text_block = false;
        self.in_func_block = false;
        self.message_open = false;
        self.content_part_open = false;
        self.current_msg_id.clear();
        self.current_fc_id.clear();
        self.message_output_index = -1;
        self.reasoning_item_id.clear();
        self.reasoning_signature.clear();
        self.reasoning_index = -1;
        self.reasoning_items.clear();
        self.stop_reason.clear();
        self.apply_patch_calls.clear();
        self.func_item_added.clear();
        self.func_args_sent.clear();
        self.func_block_stopped.clear();
        self.func_input_snapshot.clear();
        self.func_input_snapshot_errors.clear();
        self.func_identity_conflicts.clear();
        self.func_args_buf.clear();
        self.func_args_done.clear();
        self.func_item_done.clear();
        self.func_item_status.clear();
        self.func_names.clear();
        self.func_call_ids.clear();
        self.func_custom.clear();
        self.func_output_indices.clear();
        self.usage = UsageTokens::default();
    }

    /// The `tool_use` content block start: records identity, queues input evidence and emits the
    /// function item once its name and id are known.
    fn start_tool_use(&mut self, idx: i64, cb: &Res<'_>, line: &[u8]) -> Result<Vec<Vec<u8>>, Vec<Vec<u8>>> {
        self.in_func_block = true;
        let call_id = cb.g("id").str();
        let name = cb.g("name").str();
        let old_id = self.func_call_id(idx);
        let old_name = self.func_name(idx);
        // Pending identity evidence must survive later matching updates.
        if !call_id.is_empty() && !old_id.is_empty() && call_id != old_id {
            self.func_identity_conflicts.insert(idx);
        }
        if (self.is_apply_patch(&old_name) || self.is_apply_patch(&name))
            && (self.func_identity_conflicts.contains(&idx)
                || (!name.is_empty()
                    && !old_name.is_empty()
                    && self.tool_names.identity(&name) != self.tool_names.identity(&old_name)))
        {
            return Err(self.fail_tool_input(CONFLICTING_IDENTITY));
        }
        if !self.func_item_added.contains(&idx) {
            // Keep the block key even before its ID arrives so terminal validation cannot
            // overlook an unnamed or ID-less call.
            if !call_id.is_empty() || old_id.is_empty() {
                self.func_call_ids.insert(idx, call_id);
            }
            if !name.is_empty() {
                self.func_names.insert(idx, name);
            }
        }
        self.current_fc_id = self.func_call_id(idx);
        self.function_output_index(idx);
        self.func_args_buf.entry(idx).or_default();
        // Empty start input is a Claude placeholder, not an arguments fragment. Populated
        // snapshot evidence is retained without fabricating progress.
        let input = cb.g("input");
        if input.exists() && (!input.is_object() || !input.entries().is_empty()) {
            // The input as sent: a duplicated key must stay visible to the patch decoder.
            let raw = raw_at(line, "content_block.input").map_or_else(|| input.raw(), str::to_string);
            let previous = self.func_input_snapshot.get(&idx).cloned().unwrap_or_default();
            if let Err(err) = validate_apply_patch_snapshots(&previous, &raw)
                && !self.func_input_snapshot_errors.contains_key(&idx)
            {
                self.func_input_snapshot_errors.insert(idx, err);
            }
            // Item completion does not seal the response: late snapshots are compared against
            // the finished decoder without emitting more input.
            if self.func_item_done.contains(&idx)
                && let Some(patch_call) = self.apply_patch_calls.get_mut(&idx)
                && let Err(err) = patch_call.finish_arguments(&raw)
            {
                return Err(self.fail_tool_input(&err));
            }
            self.func_input_snapshot.insert(idx, raw);
        }
        if self.is_apply_patch(&self.func_name(idx))
            && let Some(err) = self.func_input_snapshot_errors.get(&idx).cloned()
        {
            return Err(self.fail_tool_input(&err));
        }
        let mut out = self.emit_func_item(idx, false);
        out.extend(self.emit_pending_func_args(idx));
        Ok(out)
    }

    fn convert_line(&mut self, model_name: &str, original: &[u8], request: &[u8], raw: &[u8]) -> Vec<Vec<u8>> {
        if self.has_error() || self.completed_emitted {
            return vec![];
        }
        if !raw.starts_with(DATA_TAG) {
            return vec![];
        }
        let raw = raw[DATA_TAG.len()..].trim_ascii();
        let root = cpa_json::parse(raw);
        let ev = root.g("type").str();
        let mut out: Vec<Vec<u8>> = Vec::new();

        match ev.as_str() {
            "message_start" => {
                let msg = root.g("message");
                if msg.exists() {
                    self.response_id = msg.g("id").str();
                    self.created_at = now_unix();
                    self.reset_for_message();
                    self.usage.merge(&msg.g("usage"));

                    let mut created = template(
                        r#"{"type":"response.created","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"in_progress","background":false,"error":null,"output":[]}}"#,
                    );
                    let seq = self.next_seq();
                    cpa_json::set(&mut created, "sequence_number", seq);
                    cpa_json::set(&mut created, "response.id", self.response_id.as_str());
                    cpa_json::set(&mut created, "response.created_at", self.created_at);
                    let mut request_model_name = common::request_model_name(original, request);
                    if request_model_name.is_empty() {
                        request_model_name = model_name.to_string();
                    }
                    if !request_model_name.is_empty() {
                        cpa_json::set(&mut created, "response.model", request_model_name.as_str());
                    }
                    out.push(emit_event("response.created", &created));

                    let mut inprog = template(
                        r#"{"type":"response.in_progress","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"in_progress","output":[]}}"#,
                    );
                    let seq = self.next_seq();
                    cpa_json::set(&mut inprog, "sequence_number", seq);
                    cpa_json::set(&mut inprog, "response.id", self.response_id.as_str());
                    cpa_json::set(&mut inprog, "response.created_at", self.created_at);
                    if !request_model_name.is_empty() {
                        cpa_json::set(&mut inprog, "response.model", request_model_name.as_str());
                    }
                    out.push(emit_event("response.in_progress", &inprog));
                }
            }

            "content_block_start" => {
                let cb = root.g("content_block");
                if !cb.exists() {
                    return out;
                }
                let idx = root.g("index").int();
                let typ = cb.g("type").str();

                // Adjacent text blocks stay in the same assistant message.
                if typ != "text" {
                    out.extend(self.finalize_assistant_message());
                }
                if self.reasoning_active || !self.reasoning_item_id.is_empty() {
                    out.extend(self.finalize_reasoning_item("completed"));
                }
                // Finalize earlier function calls. Patch calls may interleave: a new block is not
                // a completion snapshot for a still-open (or not yet named) call.
                let prev_indices: Vec<i64> = self.func_call_ids.keys().copied().collect();
                for prev_idx in prev_indices {
                    if !self.func_item_done.contains(&prev_idx) && prev_idx != idx {
                        let prev_name = self.func_name(prev_idx);
                        if (self.is_apply_patch(&prev_name) || prev_name.is_empty())
                            && !self.func_block_stopped.contains(&prev_idx)
                        {
                            continue;
                        }
                        out.extend(self.finalize_func_item(prev_idx, "completed"));
                        if self.has_error() {
                            return out;
                        }
                    }
                }
                for slot in 0..self.web_search_items.len() {
                    let item = &self.web_search_items[slot];
                    if !item.emitted && item.results.is_some() {
                        out.extend(self.finalize_web_search(slot, "completed"));
                    }
                }

                match typ.as_str() {
                    "text" => {
                        self.in_text_block = true;
                        let output_index = self.message_output_index();
                        if self.current_msg_id.is_empty() {
                            self.current_msg_id = format!("msg_{}_{}", self.response_id, self.message_items.len());
                        }
                        if !self.message_open {
                            let mut item = template(
                                r#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"message","status":"in_progress","content":[],"role":"assistant"}}"#,
                            );
                            let seq = self.next_seq();
                            cpa_json::set(&mut item, "sequence_number", seq);
                            cpa_json::set(&mut item, "output_index", output_index);
                            cpa_json::set(&mut item, "item.id", self.current_msg_id.as_str());
                            out.push(emit_event("response.output_item.added", &item));
                            self.message_open = true;
                        }
                        if !self.content_part_open {
                            let mut part = template(
                                r#"{"type":"response.content_part.added","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"part":{"type":"output_text","annotations":[],"logprobs":[],"text":""}}"#,
                            );
                            let seq = self.next_seq();
                            cpa_json::set(&mut part, "sequence_number", seq);
                            cpa_json::set(&mut part, "item_id", self.current_msg_id.as_str());
                            cpa_json::set(&mut part, "output_index", output_index);
                            out.push(emit_event("response.content_part.added", &part));
                            self.content_part_open = true;
                        }
                    }
                    "tool_use" => match self.start_tool_use(idx, &cb, raw) {
                        Ok(events) => out.extend(events),
                        Err(events) => {
                            out.extend(events);
                            return out;
                        }
                    },
                    "server_tool_use" => {
                        if cb.g("name").str() == CLAUDE_WEB_SEARCH_TOOL_NAME {
                            let slot = self.start_web_search(idx, &cb.g("id").str());
                            let item = &self.web_search_items[slot];
                            let (output_index, tool_use_id) = (item.output_index, item.tool_use_id.clone());
                            let mut added = template(
                                r#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"web_search_call","status":"in_progress","action":{"type":"search","query":""}}}"#,
                            );
                            let seq = self.next_seq();
                            cpa_json::set(&mut added, "sequence_number", seq);
                            cpa_json::set(&mut added, "output_index", output_index);
                            cpa_json::set(&mut added, "item.id", responses_web_search_call_id(&tool_use_id));
                            out.push(emit_event("response.output_item.added", &added));
                        } else {
                            tracing::debug!(
                                "claude->responses: unmapped server_tool_use {:?} at block {idx}",
                                cb.g("name").str()
                            );
                        }
                    }
                    "web_search_tool_result" => {
                        // A result block carries its full content up front and has no deltas. The
                        // item is closed when the next block starts or at message_stop so the
                        // final stop_reason is respected.
                        match self.web_search_by_tool_id.get(&cb.g("tool_use_id").str()).copied() {
                            Some(slot) => {
                                self.web_search_items[slot].results =
                                    claude_web_search_results_to_responses(&cb.g("content"))
                            }
                            None => tracing::debug!(
                                "claude->responses: web_search_tool_result without matching server_tool_use at block {idx}"
                            ),
                        }
                    }
                    "thinking" | "redacted_thinking" => {
                        self.reasoning_active = true;
                        self.reasoning_deltas_done = false;
                        self.reasoning_index = self.allocate_output_index();
                        self.reasoning_buf.clear();
                        self.reasoning_signature = claude_reasoning_carrier(&cb);
                        self.reasoning_item_id = format!("rs_{}_{idx}", self.response_id);
                        let mut item = template(
                            r#"{"type":"response.output_item.added","sequence_number":0,"output_index":0,"item":{"id":"","type":"reasoning","status":"in_progress","encrypted_content":"","summary":[]}}"#,
                        );
                        let seq = self.next_seq();
                        cpa_json::set(&mut item, "sequence_number", seq);
                        cpa_json::set(&mut item, "output_index", self.reasoning_index);
                        cpa_json::set(&mut item, "item.id", self.reasoning_item_id.as_str());
                        cpa_json::set(&mut item, "item.encrypted_content", self.reasoning_signature.as_str());
                        out.push(emit_event("response.output_item.added", &item));
                        // A summary part placeholder.
                        let mut part = template(
                            r#"{"type":"response.reasoning_summary_part.added","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}"#,
                        );
                        let seq = self.next_seq();
                        cpa_json::set(&mut part, "sequence_number", seq);
                        cpa_json::set(&mut part, "item_id", self.reasoning_item_id.as_str());
                        cpa_json::set(&mut part, "output_index", self.reasoning_index);
                        out.push(emit_event("response.reasoning_summary_part.added", &part));
                    }
                    _ => {}
                }
            }

            "content_block_delta" => {
                let d = root.g("delta");
                if !d.exists() {
                    return out;
                }
                match d.g("type").str().as_str() {
                    "text_delta" => {
                        let t = d.g("text");
                        if t.exists() {
                            let mut msg = template(
                                r#"{"type":"response.output_text.delta","sequence_number":0,"item_id":"","output_index":0,"content_index":0,"delta":"","logprobs":[]}"#,
                            );
                            let seq = self.next_seq();
                            let output_index = self.message_output_index();
                            cpa_json::set(&mut msg, "sequence_number", seq);
                            cpa_json::set(&mut msg, "item_id", self.current_msg_id.as_str());
                            cpa_json::set(&mut msg, "output_index", output_index);
                            cpa_json::set(&mut msg, "delta", t.str());
                            out.push(emit_event("response.output_text.delta", &msg));
                            self.text_buf.push_str(&t.str());
                            self.current_text_buf.push_str(&t.str());
                        }
                    }
                    "input_json_delta" => {
                        let idx = root.g("index").int();
                        if let Some(slot) = self.web_search_by_block.get(&idx).copied() {
                            let pj = d.g("partial_json");
                            if pj.exists() {
                                self.web_search_items[slot].input_buf.push_str(&pj.str());
                            }
                            return vec![];
                        }
                        let pj = d.g("partial_json");
                        if pj.exists() {
                            self.func_args_buf.entry(idx).or_default().push_str(&pj.str());
                            out.extend(self.emit_pending_func_args(idx));
                        }
                    }
                    "thinking_delta" => {
                        if self.reasoning_active {
                            let t = d.g("thinking");
                            if t.exists() {
                                self.reasoning_buf.push_str(&t.str());
                                let mut msg = template(
                                    r#"{"type":"response.reasoning_summary_text.delta","sequence_number":0,"item_id":"","output_index":0,"summary_index":0,"delta":""}"#,
                                );
                                let seq = self.next_seq();
                                cpa_json::set(&mut msg, "sequence_number", seq);
                                cpa_json::set(&mut msg, "item_id", self.reasoning_item_id.as_str());
                                cpa_json::set(&mut msg, "output_index", self.reasoning_index);
                                cpa_json::set(&mut msg, "delta", t.str());
                                out.push(emit_event("response.reasoning_summary_text.delta", &msg));
                            }
                        }
                    }
                    "signature_delta" => {
                        if self.reasoning_active {
                            let signature = d.g("signature");
                            if signature.exists() && !signature.str().is_empty() {
                                self.reasoning_signature = signature.str();
                            }
                        }
                        return vec![];
                    }
                    "citations_delta" => {
                        let citation = d.g("citation");
                        if citation.exists() {
                            self.message_annotations.push(citation.value());
                        }
                        return vec![];
                    }
                    _ => {}
                }
            }

            "content_block_stop" => {
                self.func_block_stopped.insert(root.g("index").int());
                if self.in_text_block {
                    self.in_text_block = false;
                } else if self.in_func_block {
                    self.in_func_block = false;
                } else if self.reasoning_active {
                    out.extend(self.finalize_reasoning_deltas());
                }
                return out;
            }

            "message_delta" => {
                self.usage.merge(&root.g("usage"));
                let stop_reason = root.g("delta.stop_reason");
                if stop_reason.exists() {
                    self.stop_reason = stop_reason.str();
                }
                return vec![];
            }

            "message_stop" => {
                let tool_status = output_status(&self.stop_reason);
                if self.reasoning_active || !self.reasoning_item_id.is_empty() {
                    out.extend(self.finalize_reasoning_item(tool_status));
                }
                out.extend(self.finalize_assistant_message());
                let indices: Vec<i64> = self.func_call_ids.keys().copied().collect();
                for idx in indices {
                    if !self.func_item_done.contains(&idx) {
                        out.extend(self.finalize_func_item(idx, tool_status));
                        if self.has_error() {
                            return out;
                        }
                    }
                }
                for slot in 0..self.web_search_items.len() {
                    if !self.web_search_items[slot].emitted {
                        out.extend(self.finalize_web_search(slot, tool_status));
                    }
                }

                let (event_type, response_status, details) = terminal_state(&self.stop_reason);
                let mut completed = template(
                    r#"{"type":"","sequence_number":0,"response":{"id":"","object":"response","created_at":0,"status":"","background":false,"error":null}}"#,
                );
                cpa_json::set(&mut completed, "type", event_type);
                let seq = self.next_seq();
                cpa_json::set(&mut completed, "sequence_number", seq);
                cpa_json::set(&mut completed, "response.id", self.response_id.as_str());
                cpa_json::set(&mut completed, "response.created_at", self.created_at);
                cpa_json::set(&mut completed, "response.status", response_status);
                if let Some(details) = details {
                    cpa_json::set(&mut completed, "response.incomplete_details", details);
                }

                // Echo the original request fields into the response.
                let req_bytes = pick_request_json(original, request);
                if !req_bytes.is_empty() {
                    echo_request_fields(&cpa_json::parse(req_bytes), &mut completed, "response.");
                }

                // response.output from the aggregated state, placed by output index.
                let mut outputs = template(r#"{"arr":[]}"#);
                for reasoning in &self.reasoning_items {
                    let status = if reasoning.status.is_empty() { "completed" } else { reasoning.status.as_str() };
                    let mut item = template(
                        r#"{"id":"","type":"reasoning","status":"completed","encrypted_content":"","summary":[]}"#,
                    );
                    cpa_json::set(&mut item, "id", reasoning.id.as_str());
                    cpa_json::set(&mut item, "status", status);
                    cpa_json::set(&mut item, "encrypted_content", reasoning.signature.as_str());
                    let mut summary = template(r#"{"type":"summary_text","text":""}"#);
                    cpa_json::set(&mut summary, "text", reasoning.text.as_str());
                    cpa_json::set(&mut item, "summary", Value::Array(vec![summary]));
                    cpa_json::set(&mut outputs, &format!("arr.{}", reasoning.output_index), item);
                }
                for message in &self.message_items {
                    let mut item = template(
                        r#"{"id":"","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":""}],"role":"assistant"}"#,
                    );
                    cpa_json::set(&mut item, "id", message.id.as_str());
                    cpa_json::set(&mut item, "status", message.status.as_str());
                    cpa_json::set(&mut item, "content.0.text", message.text.as_str());
                    if !message.annotations.is_empty() {
                        cpa_json::set(&mut item, "content.0.annotations", Value::Array(message.annotations.clone()));
                    }
                    cpa_json::set(&mut outputs, &format!("arr.{}", message.output_index), item);
                }
                for item in &self.web_search_items {
                    let status = if item.status.is_empty() { "completed" } else { item.status.as_str() };
                    let mut rendered = item.render();
                    cpa_json::set(&mut rendered, "status", status);
                    cpa_json::set(&mut outputs, &format!("arr.{}", item.output_index), rendered);
                }
                // Function calls in ascending block order.
                for idx in self.func_args_buf.keys().copied().collect::<Vec<_>>() {
                    let status = self
                        .func_item_status
                        .get(&idx)
                        .filter(|s| !s.is_empty())
                        .cloned()
                        .unwrap_or_else(|| "completed".to_string());
                    let is_custom = self.func_custom.contains(&idx);
                    let mut args = if !is_custom && status == "completed" { "{}".to_string() } else { String::new() };
                    if let Some(b) = self.func_args_buf.get(&idx).filter(|b| !b.is_empty()) {
                        args = b.clone();
                    }
                    let mut call_id = self.func_call_id(idx);
                    let name = self.func_name(idx);
                    if call_id.is_empty() && !self.current_fc_id.is_empty() {
                        call_id = self.current_fc_id.clone();
                    }
                    let output_index = self.func_output_indices.get(&idx).copied().unwrap_or(0);
                    let item = if is_custom {
                        let mut item = template(
                            r#"{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}"#,
                        );
                        cpa_json::set(&mut item, "id", format!("ctc_{call_id}"));
                        cpa_json::set(&mut item, "status", status.as_str());
                        let input = match self.apply_patch_calls.get(&idx) {
                            Some(patch_call) => patch_call.decoder.input().to_string(),
                            None => unwrap_custom_tool_input(&args),
                        };
                        cpa_json::set(&mut item, "input", input);
                        cpa_json::set(&mut item, "call_id", call_id.as_str());
                        self.apply_function_call_namespace_fields(item, &name, "")
                    } else {
                        let mut item = template(
                            r#"{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}"#,
                        );
                        cpa_json::set(&mut item, "id", format!("fc_{call_id}"));
                        cpa_json::set(&mut item, "status", status.as_str());
                        cpa_json::set(&mut item, "arguments", args);
                        cpa_json::set(&mut item, "call_id", call_id.as_str());
                        self.apply_function_call_namespace_fields(item, &name, "")
                    };
                    cpa_json::set(&mut outputs, &format!("arr.{output_index}"), item);
                }
                if outputs.g("arr.#").int() > 0 {
                    cpa_json::set(&mut completed, "response.output", outputs.g("arr").value());
                }

                let reasoning_length: usize = self.reasoning_items.iter().map(|r| r.text.len()).sum();
                let reasoning_tokens = (reasoning_length / 4) as i64;
                if self.usage.has_usage || reasoning_tokens > 0 {
                    let (input_tokens, output_tokens, total_tokens, cached_tokens) =
                        self.usage.openai_responses_usage();
                    cpa_json::set(&mut completed, "response.usage.input_tokens", input_tokens);
                    cpa_json::set(&mut completed, "response.usage.input_tokens_details.cached_tokens", cached_tokens);
                    cpa_json::set(&mut completed, "response.usage.output_tokens", output_tokens);
                    cpa_json::set(
                        &mut completed,
                        "response.usage.output_tokens_details.reasoning_tokens",
                        reasoning_tokens,
                    );
                    if total_tokens > 0 || self.usage.has_usage {
                        cpa_json::set(&mut completed, "response.usage.total_tokens", total_tokens);
                    }
                }
                self.completed_emitted = true;
                out.push(emit_event(event_type, &completed));
            }

            _ => {}
        }
        out
    }
}

/// How an echoed request field is converted when copied into a response.
#[derive(Clone, Copy)]
enum Echo {
    Str,
    Int,
    Bool,
    Float,
    Value,
}

/// Copies the request fields a Responses object echoes (under `prefix`) when present.
fn echo_request_fields(req: &Value, target: &mut Value, prefix: &str) {
    const FIELDS: [(&str, Echo); 20] = [
        ("instructions", Echo::Str),
        ("max_output_tokens", Echo::Int),
        ("max_tool_calls", Echo::Int),
        ("model", Echo::Str),
        ("parallel_tool_calls", Echo::Bool),
        ("previous_response_id", Echo::Str),
        ("prompt_cache_key", Echo::Str),
        ("reasoning", Echo::Value),
        ("safety_identifier", Echo::Str),
        ("service_tier", Echo::Str),
        ("store", Echo::Bool),
        ("temperature", Echo::Float),
        ("text", Echo::Value),
        ("tool_choice", Echo::Value),
        ("tools", Echo::Value),
        ("top_logprobs", Echo::Int),
        ("top_p", Echo::Float),
        ("truncation", Echo::Str),
        ("user", Echo::Value),
        ("metadata", Echo::Value),
    ];
    for (key, kind) in FIELDS {
        let v = req.g(key);
        if !v.exists() {
            continue;
        }
        let path = format!("{prefix}{key}");
        match kind {
            Echo::Str => cpa_json::set(target, &path, v.str()),
            Echo::Int => cpa_json::set(target, &path, v.int()),
            Echo::Bool => cpa_json::set(target, &path, v.bool()),
            Echo::Float => cpa_json::set(target, &path, cpa_json::num_f64(v.float())),
            Echo::Value => cpa_json::set(target, &path, v.value()),
        };
    }
}

/// Converts one Claude streaming line (`data: {...}`) into Responses SSE events.
pub fn convert_claude_response_to_openai_responses(
    _ctx: &Ctx,
    model_name: &str,
    original: &[u8],
    request: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let st = param.state(|| State::new(pick_request_json(original, request)));
    let out = st.convert_line(model_name, original, request, raw);
    param.tool_input_error = param.get::<State>().and_then(|st| st.err.tool_input_error().map(str::to_string));
    out
}

/// For executors at stream end: a patch-enabled stream that lacks its source terminator fails the
/// response. Returns the `response.failed` event, if any.
pub fn finalize_tool_input(param: &mut Param) -> Vec<Vec<u8>> {
    let Some(st) = param.get::<State>() else { return vec![] };
    if st.has_error() || st.completed_emitted {
        return vec![];
    }
    let enabled = st.tool_winners.keys().any(|name| st.is_apply_patch(name));
    if !enabled {
        return vec![];
    }
    st.err.set_tool_input_error("upstream apply_patch stream ended before protocol completion");
    st.seq += 1;
    let event = common::sse_event_data("response.failed", &common::apply_patch_failure(&st.response_id, st.seq));
    param.tool_input_error = param.get::<State>().and_then(|st| st.err.tool_input_error().map(str::to_string));
    vec![event]
}

/// Recovers the `input` text of a custom tool call from its function arguments: the `input` field
/// of the JSON object, or, for malformed JSON, a tolerant scan of the `"input"` string value.
fn unwrap_custom_tool_input(arguments: &str) -> String {
    let trimmed = arguments.trim();
    let parsed = cpa_json::parse_str(trimmed);
    let v = parsed.g("input");
    if v.exists() {
        // Non-strings come back as the original text of the value (gjson `Raw`).
        return if v.is_string() {
            v.str()
        } else {
            cpa_json::raw_at(trimmed.as_bytes(), "input").map_or_else(|| v.raw(), str::to_string)
        };
    }
    if let Some(idx) = trimmed.find("\"input\"") {
        let rest = trimmed[idx + 7..].trim();
        if let Some(rest) = rest.strip_prefix(':') {
            let rest = rest.trim();
            if let Some(content) = rest.strip_prefix('"') {
                return scan_json_string_tolerant(content);
            }
        }
    }
    arguments.to_string()
}

/// Unescapes the body of a JSON string literal up to its closing quote, keeping unknown or
/// incomplete escapes literally (the input may be a truncated stream).
fn scan_json_string_tolerant(content: &str) -> String {
    let bytes = content.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut in_escape = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_escape {
            match c {
                b'"' | b'\\' | b'/' => out.push(c),
                b'b' => out.push(0x08),
                b'f' => out.push(0x0c),
                b'n' => out.push(b'\n'),
                b'r' => out.push(b'\r'),
                b't' => out.push(b'\t'),
                b'u' => {
                    if i + 4 < bytes.len()
                        && let Some(r) = parse_hex4(&bytes[i + 1..i + 5])
                    {
                        if (0xD800..0xE000).contains(&r)
                            && i + 10 < bytes.len()
                            && &bytes[i + 5..i + 7] == b"\\u"
                            && let Some(r2) = parse_hex4(&bytes[i + 7..i + 11])
                        {
                            push_rune(&mut out, decode_utf16_pair(r, r2));
                            i += 11;
                            in_escape = false;
                            continue;
                        }
                        push_rune(&mut out, r as u32);
                        i += 5;
                        in_escape = false;
                        continue;
                    }
                    out.extend_from_slice(b"\\u");
                }
                _ => {
                    out.push(b'\\');
                    out.push(c);
                }
            }
            in_escape = false;
        } else if c == b'\\' {
            in_escape = true;
        } else if c == b'"' {
            break;
        } else {
            out.push(c);
        }
        i += 1;
    }
    if in_escape {
        out.push(b'\\');
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_hex4(b: &[u8]) -> Option<u16> {
    if !b.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u16::from_str_radix(std::str::from_utf8(b).ok()?, 16).ok()
}

/// Go `utf16.DecodeRune`: a valid surrogate pair combines, anything else is U+FFFD.
fn decode_utf16_pair(r1: u16, r2: u16) -> u32 {
    if (0xD800..0xDC00).contains(&r1) && (0xDC00..0xE000).contains(&r2) {
        0x10000 + (((r1 as u32) - 0xD800) << 10 | ((r2 as u32) - 0xDC00))
    } else {
        0xFFFD
    }
}

/// Writes a code point as UTF-8; surrogates and out-of-range values become U+FFFD like Go's
/// `WriteRune`.
fn push_rune(out: &mut Vec<u8>, r: u32) {
    let ch = char::from_u32(r).unwrap_or('\u{FFFD}');
    let mut buf = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
}

// ---------------------------------------------------------------- non-stream

/// One output item of the non-stream aggregation, in content block order.
#[derive(Default)]
struct OutputItem {
    item_type: String,
    id: String,
    call_id: String,
    name: String,
    text: String,
    signature: String,
    annotations: Vec<Value>,
    args: String,
    input_snapshot: String,
    results: Option<Value>,
}

/// Appends an output item for a content block and returns its slot.
fn new_output_item(
    items: &mut Vec<OutputItem>,
    block_to_item: &mut HashMap<i64, usize>,
    item_type: &str,
    block_index: i64,
) -> usize {
    items.push(OutputItem { item_type: item_type.to_string(), ..Default::default() });
    block_to_item.insert(block_index, items.len() - 1);
    items.len() - 1
}

/// Aggregates a complete Claude SSE body into one Responses object.
pub fn convert_claude_response_to_openai_responses_non_stream(
    _ctx: &Ctx,
    _model_name: &str,
    original: &[u8],
    request: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Option<Vec<u8>> {
    let req_bytes = pick_request_json(original, request);
    let mut st = State::new(req_bytes);
    let out = non_stream(&mut st, req_bytes, raw);
    param.tool_input_error = st.err.tool_input_error().map(str::to_string);
    *param.state(|| State::new(&[])) = st;
    out
}

/// The `response` object of the apply_patch failure event, returned after a tool input error.
fn failure_response(response_id: &str) -> Vec<u8> {
    cpa_json::parse(&common::apply_patch_failure(response_id, 0)).g("response").raw().into_bytes()
}

fn non_stream(st: &mut State, req_bytes: &[u8], raw: &[u8]) -> Option<Vec<u8>> {
    let chunks: Vec<&[u8]> = raw
        .split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| line.starts_with(DATA_TAG))
        .map(|line| &line[DATA_TAG.len()..])
        .collect();

    let mut out = template(
        r#"{"id":"","object":"response","created_at":0,"status":"completed","background":false,"error":null,"incomplete_details":null,"output":[],"usage":{"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"output_tokens_details":{},"total_tokens":0}}"#,
    );

    let mut response_id = String::new();
    let mut created_at = 0i64;
    let mut stop_reason = String::new();
    let mut usage_tokens = UsageTokens::default();

    let mut items: Vec<OutputItem> = Vec::new();
    let mut block_to_item: HashMap<i64, usize> = HashMap::new();
    let mut web_search_by_tool_id: HashMap<String, usize> = HashMap::new();
    let mut message_count = 0usize;
    let mut active_message_item: Option<usize> = None;
    let mut pending_annotations: Vec<Value> = Vec::new();

    for ch in chunks {
        let root = cpa_json::parse(ch);
        let ev = root.g("type").str();
        // Match the streaming path: no input after the protocol terminator.
        if ev == "message_stop" {
            break;
        }

        match ev.as_str() {
            "message_start" => {
                let msg = root.g("message");
                if msg.exists() {
                    response_id = msg.g("id").str();
                    created_at = now_unix();
                    usage_tokens.merge(&msg.g("usage"));
                }
            }

            "content_block_start" => {
                let cb = root.g("content_block");
                if !cb.exists() {
                    continue;
                }
                let idx = root.g("index").int();
                let typ = cb.g("type").str();
                if typ != "text" {
                    active_message_item = None;
                }
                match typ.as_str() {
                    "text" => {
                        let slot = match active_message_item {
                            Some(slot) => {
                                block_to_item.insert(idx, slot);
                                slot
                            }
                            None => {
                                let slot = new_output_item(&mut items, &mut block_to_item, "message", idx);
                                items[slot].id = format!("msg_{response_id}_{message_count}");
                                message_count += 1;
                                slot
                            }
                        };
                        if !pending_annotations.is_empty() {
                            items[slot].annotations.append(&mut pending_annotations);
                        }
                        active_message_item = Some(slot);
                    }
                    "tool_use" => {
                        let mut item_type = "function_call";
                        if st.winner(&cb.g("name").str()).is_some_and(|d| d.tool_type == "custom") {
                            item_type = "custom_tool_call";
                        }
                        let slot = match block_to_item.get(&idx).copied() {
                            Some(slot) => slot,
                            None => new_output_item(&mut items, &mut block_to_item, item_type, idx),
                        };
                        let name = cb.g("name").str();
                        let call_id = cb.g("id").str();
                        if !call_id.is_empty() && !items[slot].call_id.is_empty() && call_id != items[slot].call_id {
                            st.func_identity_conflicts.insert(idx);
                        }
                        if (st.is_apply_patch(&items[slot].name) || st.is_apply_patch(&name))
                            && (st.func_identity_conflicts.contains(&idx)
                                || (!name.is_empty()
                                    && !items[slot].name.is_empty()
                                    && st.tool_names.identity(&name) != st.tool_names.identity(&items[slot].name)))
                        {
                            st.err.set_tool_input_error(CONFLICTING_IDENTITY);
                            return Some(failure_response(&response_id));
                        }
                        if !name.is_empty() {
                            items[slot].name = name;
                            items[slot].item_type = item_type.to_string();
                        }
                        if !call_id.is_empty() {
                            items[slot].call_id = call_id;
                        }
                        let input = cb.g("input");
                        if input.exists() && (!input.is_object() || !input.entries().is_empty()) {
                            let raw_input =
                                raw_at(ch, "content_block.input").map_or_else(|| input.raw(), str::to_string);
                            if let Err(err) = validate_apply_patch_snapshots(&items[slot].input_snapshot, &raw_input)
                                && !st.func_input_snapshot_errors.contains_key(&idx)
                            {
                                st.func_input_snapshot_errors.insert(idx, err);
                            }
                            items[slot].input_snapshot = raw_input;
                        }
                        if st.is_apply_patch(&items[slot].name)
                            && let Some(err) = st.func_input_snapshot_errors.get(&idx).cloned()
                        {
                            st.err.set_tool_input_error(err);
                            return Some(failure_response(&response_id));
                        }
                        items[slot].id = if items[slot].item_type == "custom_tool_call" {
                            format!("ctc_{}", items[slot].call_id)
                        } else {
                            format!("fc_{}", items[slot].call_id)
                        };
                    }
                    "server_tool_use" => {
                        let name = cb.g("name").str();
                        if name != CLAUDE_WEB_SEARCH_TOOL_NAME {
                            tracing::debug!("claude->responses: unmapped server_tool_use {name:?} at block {idx}");
                            continue;
                        }
                        let tool_use_id = cb.g("id").str();
                        let slot = new_output_item(&mut items, &mut block_to_item, "web_search_call", idx);
                        items[slot].id = responses_web_search_call_id(&tool_use_id);
                        items[slot].call_id = tool_use_id.clone();
                        web_search_by_tool_id.insert(tool_use_id, slot);
                        // Streaming announces an empty input and fills it through
                        // input_json_delta; only seed when the query is already present.
                        let input = cb.g("input");
                        if input.is_object() && !claude_web_search_query(&input.raw()).is_empty() {
                            items[slot].args.push_str(&input.raw());
                        }
                    }
                    "web_search_tool_result" => match web_search_by_tool_id.get(&cb.g("tool_use_id").str()).copied() {
                        Some(slot) => items[slot].results = claude_web_search_results_to_responses(&cb.g("content")),
                        None => tracing::debug!(
                            "claude->responses: web_search_tool_result without matching server_tool_use at block {idx}"
                        ),
                    },
                    "thinking" | "redacted_thinking" => {
                        let slot = new_output_item(&mut items, &mut block_to_item, "reasoning", idx);
                        items[slot].id = format!("rs_{response_id}_{idx}");
                        items[slot].signature = claude_reasoning_carrier(&cb);
                    }
                    _ => {}
                }
            }

            "content_block_delta" => {
                let d = root.g("delta");
                if !d.exists() {
                    continue;
                }
                let idx = root.g("index").int();
                let slot = block_to_item.get(&idx).copied();
                match d.g("type").str().as_str() {
                    "text_delta" => {
                        if let Some(slot) = slot.filter(|&s| items[s].item_type == "message") {
                            let t = d.g("text");
                            if t.exists() {
                                items[slot].text.push_str(&t.str());
                            }
                        }
                    }
                    "input_json_delta" => {
                        if let Some(slot) = slot.filter(|&s| {
                            matches!(
                                items[s].item_type.as_str(),
                                "function_call" | "custom_tool_call" | "web_search_call"
                            )
                        }) {
                            let pj = d.g("partial_json");
                            if pj.exists() {
                                items[slot].args.push_str(&pj.str());
                            }
                        }
                    }
                    "thinking_delta" => {
                        if let Some(slot) = slot.filter(|&s| items[s].item_type == "reasoning") {
                            let t = d.g("thinking");
                            if t.exists() {
                                items[slot].text.push_str(&t.str());
                            }
                        }
                    }
                    "signature_delta" => {
                        if let Some(slot) = slot.filter(|&s| items[s].item_type == "reasoning") {
                            let signature = d.g("signature");
                            if signature.exists() && !signature.str().is_empty() {
                                items[slot].signature = signature.str();
                            }
                        }
                    }
                    "citations_delta" => {
                        let citation = d.g("citation");
                        if citation.exists() {
                            if let Some(slot) = slot.filter(|&s| items[s].item_type == "message") {
                                items[slot].annotations.push(citation.value());
                            } else if let Some(active) = active_message_item {
                                items[active].annotations.push(citation.value());
                            } else {
                                pending_annotations.push(citation.value());
                            }
                        }
                    }
                    _ => {}
                }
            }

            // Output items are finalized after all deltas have been aggregated.
            "content_block_stop" => {}

            "message_delta" => {
                usage_tokens.merge(&root.g("usage"));
                let value = root.g("delta.stop_reason");
                if value.exists() {
                    stop_reason = value.str();
                }
            }
            _ => {}
        }
    }

    let (_, response_status, details) = terminal_state(&stop_reason);
    cpa_json::set(&mut out, "id", response_id.as_str());
    cpa_json::set(&mut out, "created_at", created_at);
    cpa_json::set(&mut out, "status", response_status);
    if let Some(details) = details {
        cpa_json::set(&mut out, "incomplete_details", details);
    }

    // Echo the request fields at the top level, like the streaming terminal event.
    if !req_bytes.is_empty() {
        echo_request_fields(&cpa_json::parse(req_bytes), &mut out, "");
    }

    // Output array in content block order.
    let mut outputs: Vec<Value> = Vec::with_capacity(items.len());
    let last = items.len().wrapping_sub(1);
    for (i, output_item) in items.iter().enumerate() {
        let item_status = if response_status == "incomplete" && i == last { "incomplete" } else { "completed" };
        let item = match output_item.item_type.as_str() {
            "reasoning" => {
                let mut item = template(
                    r#"{"id":"","type":"reasoning","status":"completed","encrypted_content":"","summary":[]}"#,
                );
                cpa_json::set(&mut item, "id", output_item.id.as_str());
                cpa_json::set(&mut item, "status", item_status);
                cpa_json::set(&mut item, "encrypted_content", output_item.signature.as_str());
                let mut summary = template(r#"{"type":"summary_text","text":""}"#);
                cpa_json::set(&mut summary, "text", output_item.text.as_str());
                cpa_json::set(&mut item, "summary", Value::Array(vec![summary]));
                Some(item)
            }
            "web_search_call" => {
                let mut item = build_responses_web_search_call_item(
                    &output_item.call_id,
                    &claude_web_search_query(&output_item.args),
                    output_item.results.as_ref(),
                );
                cpa_json::set(&mut item, "status", item_status);
                Some(item)
            }
            "message" => {
                let mut item = template(
                    r#"{"id":"","type":"message","status":"completed","content":[{"type":"output_text","annotations":[],"logprobs":[],"text":""}],"role":"assistant"}"#,
                );
                cpa_json::set(&mut item, "id", output_item.id.as_str());
                cpa_json::set(&mut item, "status", item_status);
                cpa_json::set(&mut item, "content.0.text", output_item.text.as_str());
                if !output_item.annotations.is_empty() {
                    cpa_json::set(&mut item, "content.0.annotations", Value::Array(output_item.annotations.clone()));
                }
                Some(item)
            }
            "custom_tool_call" => {
                let mut item = template(
                    r#"{"id":"","type":"custom_tool_call","status":"completed","input":"","call_id":"","name":""}"#,
                );
                cpa_json::set(&mut item, "id", output_item.id.as_str());
                cpa_json::set(&mut item, "status", item_status);
                let input = if st.is_apply_patch(&output_item.name) {
                    let mut patch_call = ApplyPatchCallState::default();
                    if let Err(err) = patch_call.push_arguments(&output_item.args) {
                        st.err.set_tool_input_error(err);
                        return Some(failure_response(&response_id));
                    }
                    match finish_claude_apply_patch_arguments(
                        &mut patch_call,
                        &output_item.args,
                        &output_item.input_snapshot,
                    ) {
                        Ok((_, full_input)) => full_input,
                        Err(err) => {
                            st.err.set_tool_input_error(err);
                            return Some(failure_response(&response_id));
                        }
                    }
                } else {
                    unwrap_custom_tool_input(&output_item.args)
                };
                cpa_json::set(&mut item, "input", input);
                cpa_json::set(&mut item, "call_id", output_item.call_id.as_str());
                Some(st.apply_function_call_namespace_fields(item, &output_item.name, ""))
            }
            "function_call" => {
                let mut args = output_item.args.clone();
                if args.is_empty() && item_status == "completed" {
                    args = "{}".to_string();
                }
                let mut item = template(
                    r#"{"id":"","type":"function_call","status":"completed","arguments":"","call_id":"","name":""}"#,
                );
                cpa_json::set(&mut item, "id", output_item.id.as_str());
                cpa_json::set(&mut item, "status", item_status);
                cpa_json::set(&mut item, "arguments", args);
                cpa_json::set(&mut item, "call_id", output_item.call_id.as_str());
                Some(st.apply_function_call_namespace_fields(item, &output_item.name, ""))
            }
            _ => None,
        };
        outputs.extend(item);
    }
    if !outputs.is_empty() {
        cpa_json::set(&mut out, "output", Value::Array(outputs));
    }

    let (input_tokens, output_tokens, total_tokens, cached_tokens) = usage_tokens.openai_responses_usage();
    if input_tokens != 0 {
        cpa_json::set(&mut out, "usage.input_tokens", input_tokens);
    }
    if cached_tokens != 0 {
        cpa_json::set(&mut out, "usage.input_tokens_details.cached_tokens", cached_tokens);
    }
    if output_tokens != 0 {
        cpa_json::set(&mut out, "usage.output_tokens", output_tokens);
    }
    if total_tokens != 0 {
        cpa_json::set(&mut out, "usage.total_tokens", total_tokens);
    }
    let reasoning_length: usize = items.iter().filter(|i| i.item_type == "reasoning").map(|i| i.text.len()).sum();
    if reasoning_length > 0 {
        // Rough estimate, like chat completions.
        let reasoning_tokens = (reasoning_length / 4) as i64;
        if reasoning_tokens > 0 {
            cpa_json::set(&mut out, "usage.output_tokens_details.reasoning_tokens", reasoning_tokens);
        }
    }

    Some(cpa_json::to_vec(&out))
}
