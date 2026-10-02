//! Codex Responses -> Claude Messages response (Go: codex_claude_response.go).
//!
//! The stream converter is a state machine over Codex SSE events producing Claude SSE frames.
//! Function calls are tracked in an arena (`State::calls`); the alias table, the queue and the
//! active/last pointers all refer to calls by index.

use std::collections::{HashMap, HashSet};

use cpa_core::util::sanitize_claude_tool_id;
use cpa_json::{J, Res, Value, json};

use super::request::{build_reverse_map_short_to_original, shorten_call_id_if_needed};
use super::web_search::{append_web_search_non_stream_blocks, append_web_search_tool_result};
use crate::codex::util::go_lossy;
use crate::common::{append_sse_event_bytes, claude_input_tokens_json};
use crate::registry::{Ctx, Param};

/// Joins consecutive reasoning summary parts inside the single thinking block that represents
/// one Codex reasoning item.
const THINKING_SUMMARY_PART_SEPARATOR: &str = "\n\n";

/// One streamed function call (Go: codexFunctionCallStream).
#[derive(Default)]
struct FunctionCall {
    call_id: String,
    name: String,
    block_index: i64,
    arguments: String,
    emitted_arguments_length: usize,
    has_received_arguments_delta: bool,
    emit_initial_empty_delta: bool,
    started: bool,
    done: bool,
    closed: bool,
}

/// Per-stream state (Go: ConvertCodexResponseToClaudeParams).
#[derive(Default)]
pub(super) struct State {
    has_emitted_tool_use: bool,
    pub block_index: i64,
    has_text_delta: bool,
    text_block_open: bool,
    thinking_block_open: bool,
    thinking_signature: String,
    thinking_summary_seen: bool,
    pub web_search_tool_use_ids: HashSet<String>,
    pub web_search_tool_result_ids: HashSet<String>,
    pub last_web_search_tool_use_id: String,
    calls: Vec<FunctionCall>,
    function_calls: HashMap<String, usize>,
    function_call_queue: Vec<usize>,
    active_function_call: Option<usize>,
    last_function_call: Option<usize>,
    deferred_stream_events: Vec<Vec<u8>>,
}

/// Go: ConvertCodexResponseToClaude. One `data:` line in; the output is one chunk of Claude SSE
/// frames (possibly empty), or nothing when the event is deferred behind an open function call.
pub fn convert_codex_response_to_claude(
    _ctx: &Ctx,
    _model_name: &str,
    original_request: &[u8],
    _request: &[u8],
    raw: &[u8],
    param: &mut Param,
) -> Vec<Vec<u8>> {
    let st = param.state(State::default);
    match st.process(original_request, raw) {
        Some(out) => vec![out],
        None => vec![],
    }
}

impl State {
    fn process(&mut self, original: &[u8], raw: &[u8]) -> Option<Vec<u8>> {
        if !raw.starts_with(b"data:") {
            return None;
        }
        let stream_event = raw.to_vec();
        let raw = raw[5..].trim_ascii();

        let mut output: Vec<u8> = Vec::with_capacity(512);
        let root = cpa_json::parse(raw);
        let type_str = root.g("type").str();
        if self.active_function_call.is_some() && should_defer_stream_event(&type_str, &root) {
            self.deferred_stream_events.push(stream_event);
            return None;
        }

        match type_str.as_str() {
            "error" => output.extend(stream_error_to_claude_error(&root)),
            "response.created" => {
                let mut template = cpa_json::parse_str(
                    r#"{"type":"message_start","message":{"id":"","type":"message","role":"assistant","model":"claude-opus-4-1-20250805","stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0},"content":[],"stop_reason":null}}"#,
                );
                cpa_json::set(
                    &mut template,
                    "message.model",
                    root.g("response.model").str(),
                );
                cpa_json::set(&mut template, "message.id", root.g("response.id").str());
                append_sse_event_bytes(
                    &mut output,
                    "message_start",
                    &cpa_json::to_vec(&template),
                    2,
                );
            }
            "response.reasoning_summary_part.added" => {
                output.extend(self.stop_text_block());
                // Codex splits one reasoning item into several summary parts, but only
                // output_item.done carries the item's final encrypted_content. One thinking
                // block stays open for the whole item with parts separated by a blank line, so
                // the only signature ever emitted is the final one.
                if self.thinking_block_open {
                    output.extend(self.thinking_delta(THINKING_SUMMARY_PART_SEPARATOR));
                } else {
                    output.extend(self.start_thinking_block());
                }
                self.thinking_summary_seen = true;
            }
            "response.reasoning_summary_text.delta" => {
                output.extend(self.stop_text_block());
                output.extend(self.start_thinking_block());
                output.extend(self.thinking_delta(&root.g("delta").str()));
            }
            // The thinking block stays open until output_item.done delivers the final signature.
            "response.reasoning_summary_part.done" => {}
            "response.content_part.added" => {
                output.extend(self.finalize_thinking_block());
                if root.g("part.type").str() == "output_text" {
                    output.extend(self.start_text_block());
                }
            }
            "response.output_text.delta" => {
                self.has_text_delta = true;
                output.extend(self.finalize_thinking_block());
                output.extend(self.start_text_block());
                let template = json!({"type": "content_block_delta", "index": self.block_index, "delta": {"type": "text_delta", "text": root.g("delta").str()}});
                append_sse_event_bytes(
                    &mut output,
                    "content_block_delta",
                    &cpa_json::to_vec(&template),
                    2,
                );
            }
            "response.content_part.done" => {
                if root.g("part.type").str() == "output_text" {
                    output.extend(self.stop_text_block());
                }
            }
            // Wait for populated web_search_call items on output_item.done.
            "response.web_search_call.searching"
            | "response.web_search_call.completed"
            | "response.web_search_call.in_progress" => {}
            "response.completed" | "response.incomplete" => {
                let mut template = cpa_json::parse_str(
                    r#"{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":0,"output_tokens":0}}"#,
                );
                let response_data = root.g("response");
                output.extend(self.finalize_thinking_block());
                output.extend(self.stop_text_block());
                self.append_function_calls_from_terminal(&mut output, original, &response_data);
                self.append_deferred_stream_events(&mut output, original);
                output.extend(self.finalize_thinking_block());
                output.extend(self.stop_text_block());
                cpa_json::set(
                    &mut template,
                    "delta.stop_reason",
                    map_stop_reason_to_claude(
                        &stop_reason(&response_data),
                        self.has_emitted_tool_use,
                    ),
                );
                set_stop_sequence(&mut template, "delta.stop_sequence", &response_data);
                let (input_tokens, output_tokens, cached_tokens, cache_write_tokens) =
                    extract_responses_usage(&response_data.g("usage"));
                cpa_json::set(&mut template, "usage.input_tokens", input_tokens);
                cpa_json::set(&mut template, "usage.output_tokens", output_tokens);
                if cached_tokens > 0 {
                    cpa_json::set(
                        &mut template,
                        "usage.cache_read_input_tokens",
                        cached_tokens,
                    );
                }
                if cache_write_tokens > 0 {
                    cpa_json::set(
                        &mut template,
                        "usage.cache_creation_input_tokens",
                        cache_write_tokens,
                    );
                }
                set_reasoning_usage(&mut template, &response_data.g("usage"));

                append_sse_event_bytes(
                    &mut output,
                    "message_delta",
                    &cpa_json::to_vec(&template),
                    2,
                );
                append_sse_event_bytes(
                    &mut output,
                    "message_stop",
                    br#"{"type":"message_stop"}"#,
                    2,
                );
            }
            "response.output_item.added" => {
                let item = root.g("item");
                match item.g("type").str().as_str() {
                    "function_call" => {
                        output.extend(self.finalize_thinking_block());
                        output.extend(self.stop_text_block());

                        let call = self.record_function_call(&root, &item);
                        self.update_function_call_identity(call, &root, &item);
                        if !self.calls[call].name.is_empty() {
                            self.calls[call].emit_initial_empty_delta = true;
                        }
                        self.append_function_call_queue(&mut output, original);
                    }
                    "reasoning" => {
                        output.extend(self.stop_text_block());
                        // A previous reasoning item that never reported output_item.done must
                        // not leak its still-open block into this one.
                        output.extend(self.finalize_thinking_block());
                        self.thinking_summary_seen = false;
                        // Fallback for streams whose output_item.done omits encrypted_content;
                        // a pre-content snapshot, never the final value.
                        self.thinking_signature = item.g("encrypted_content").str();
                    }
                    // server_tool_use is deferred until output_item.done carries action/query.
                    _ => {}
                }
            }
            "response.output_item.done" => {
                let item = root.g("item");
                match item.g("type").str().as_str() {
                    "message" => {
                        if self.has_text_delta {
                            return Some(output);
                        }
                        let content = item.g("content");
                        if !content.exists() || !content.is_array() {
                            return Some(output);
                        }
                        let mut text = String::new();
                        for part in content.array() {
                            if part.g("type").str() != "output_text" {
                                continue;
                            }
                            text.push_str(&part.g("text").str());
                        }
                        if text.is_empty() {
                            return Some(output);
                        }

                        output.extend(self.finalize_thinking_block());
                        output.extend(self.start_text_block());
                        let template = json!({"type": "content_block_delta", "index": self.block_index, "delta": {"type": "text_delta", "text": text}});
                        append_sse_event_bytes(
                            &mut output,
                            "content_block_delta",
                            &cpa_json::to_vec(&template),
                            2,
                        );
                        output.extend(self.stop_text_block());
                        self.has_text_delta = true;
                    }
                    "function_call" => {
                        output.extend(self.finalize_thinking_block());
                        output.extend(self.stop_text_block());
                        let call = match self.function_call_for_event(&root, &item) {
                            Some(call) => call,
                            None => self.record_function_call(&root, &item),
                        };
                        self.update_function_call_identity(call, &root, &item);
                        update_function_call_arguments(
                            &mut self.calls[call],
                            &item.g("arguments").str(),
                            false,
                        );
                        self.calls[call].done = true;
                        self.append_function_call_queue(&mut output, original);
                    }
                    "reasoning" => {
                        output.extend(self.stop_text_block());
                        let signature = item.g("encrypted_content").str();
                        if !signature.is_empty() {
                            self.thinking_signature = signature;
                        }
                        if self.thinking_summary_seen {
                            output.extend(self.finalize_thinking_block());
                        } else {
                            output.extend(self.finalize_signature_only_thinking_block());
                        }
                        self.thinking_signature.clear();
                        self.thinking_summary_seen = false;
                    }
                    "web_search_call" => {
                        append_web_search_tool_result(&mut output, self, &root, &item)
                    }
                    _ => {}
                }
            }
            "response.function_call_arguments.delta" => {
                let call = match self.function_call_for_event(&root, &Res::NONE) {
                    Some(call) => call,
                    None => self.record_function_call(&root, &Res::NONE),
                };
                update_function_call_arguments(&mut self.calls[call], &root.g("delta").str(), true);
                self.append_function_call_buffered_arguments(&mut output, call);
            }
            "response.function_call_arguments.done" => {
                let call = match self.function_call_for_event(&root, &Res::NONE) {
                    Some(call) => call,
                    None => self.record_function_call(&root, &Res::NONE),
                };
                update_function_call_arguments(
                    &mut self.calls[call],
                    &root.g("arguments").str(),
                    false,
                );
                self.append_function_call_buffered_arguments(&mut output, call);
            }
            _ => {}
        }

        if self.function_call_queue.is_empty() {
            self.append_deferred_stream_events(&mut output, original);
        }
        Some(output)
    }

    /// Replays events held back while a function call was open.
    fn append_deferred_stream_events(&mut self, output: &mut Vec<u8>, original: &[u8]) {
        if self.deferred_stream_events.is_empty() {
            return;
        }
        let events = std::mem::take(&mut self.deferred_stream_events);
        for event in events {
            if let Some(chunk) = self.process(original, &event) {
                output.extend(chunk);
            }
        }
    }

    // ---- function call tracking

    fn function_call_for_keys(&self, keys: &[String]) -> Option<usize> {
        keys.iter()
            .find_map(|key| self.function_calls.get(key).copied())
    }

    fn function_call_for_event(&self, root: &Value, item: &Res<'_>) -> Option<usize> {
        let keys = function_call_keys(&Res::of(root), item);
        if !keys.is_empty() {
            return self.function_call_for_keys(&keys);
        }
        self.last_function_call
    }

    fn new_call(&mut self) -> usize {
        self.calls.push(FunctionCall {
            block_index: -1,
            ..Default::default()
        });
        let call = self.calls.len() - 1;
        self.function_call_queue.push(call);
        call
    }

    fn record_function_call(&mut self, root: &Value, item: &Res<'_>) -> usize {
        let keys = function_call_keys(&Res::of(root), item);
        let call = match self.function_call_for_keys(&keys) {
            Some(call) => call,
            None => self.new_call(),
        };
        self.add_function_call_aliases(call, &keys);
        self.last_function_call = Some(call);
        call
    }

    fn add_function_call_aliases(&mut self, call: usize, keys: &[String]) {
        for key in keys {
            self.function_calls.insert(key.clone(), call);
        }
    }

    fn update_function_call_identity(&mut self, call: usize, root: &Value, item: &Res<'_>) {
        let call_id = item.g("call_id").str();
        if !call_id.is_empty() {
            self.calls[call].call_id = call_id;
        }
        let name = item.g("name").str();
        if !name.is_empty() {
            self.calls[call].name = name;
        }
        let keys = function_call_keys(&Res::of(root), item);
        self.add_function_call_aliases(call, &keys);
    }

    fn append_function_call_buffered_arguments(&mut self, output: &mut Vec<u8>, call: usize) {
        let c = &mut self.calls[call];
        if self.active_function_call != Some(call) || !c.started || c.closed {
            return;
        }
        if c.emitted_arguments_length >= c.arguments.len() {
            return;
        }
        let pending = go_slice_from(&c.arguments, c.emitted_arguments_length);
        append_function_call_argument_delta(output, &pending, c.block_index);
        c.emitted_arguments_length = c.arguments.len();
    }

    /// Starts, streams and closes queued calls in order; stops at the first call that is still
    /// open or has no name yet.
    fn append_function_call_queue(&mut self, output: &mut Vec<u8>, original: &[u8]) {
        loop {
            if let Some(active) = self.active_function_call {
                self.append_function_call_buffered_arguments(output, active);
                if !self.calls[active].done {
                    return;
                }
                append_function_call_stop(output, self.calls[active].block_index);
                if self.block_index <= self.calls[active].block_index {
                    self.block_index = self.calls[active].block_index + 1;
                }
                self.calls[active].closed = true;
                self.active_function_call = None;
                self.function_call_queue.retain(|&queued| queued != active);
            }

            while self
                .function_call_queue
                .first()
                .is_some_and(|&c| self.calls[c].closed)
            {
                self.function_call_queue.remove(0);
            }
            let Some(&call) = self.function_call_queue.first() else {
                return;
            };
            if self.calls[call].name.is_empty() {
                return;
            }

            self.calls[call].block_index = self.block_index;
            append_function_call_start(
                output,
                original,
                &self.calls[call].call_id,
                &self.calls[call].name,
                self.block_index,
            );
            if self.calls[call].emit_initial_empty_delta {
                append_function_call_argument_delta(output, "", self.block_index);
            }
            self.calls[call].started = true;
            self.active_function_call = Some(call);
            self.has_emitted_tool_use = true;
            self.append_function_call_buffered_arguments(output, call);
        }
    }

    /// The terminal response lists every function call: finish the ones still open and flush.
    fn append_function_calls_from_terminal(
        &mut self,
        output: &mut Vec<u8>,
        original: &[u8],
        response_data: &Res<'_>,
    ) {
        let outputs = response_data.g("output");
        let entries: Vec<(Option<String>, Res<'_>)> = match outputs.v() {
            Some(Value::Array(a)) => a
                .iter()
                .enumerate()
                .map(|(i, v)| (Some(i.to_string()), Res::of(v)))
                .collect(),
            Some(Value::Object(m)) => m
                .iter()
                .map(|(k, v)| (Some(k.clone()), Res::of(v)))
                .collect(),
            Some(v) => vec![(None, Res::of(v))],
            None => vec![],
        };
        for (index, item) in entries {
            if item.g("type").str() != "function_call" {
                continue;
            }
            let mut keys = function_call_keys(&Res::NONE, &item);
            let item_output_index = item.g("output_index");
            if item_output_index.exists() {
                append_unique_key(&mut keys, format!("output:{}", item_output_index.raw()));
            }
            if let Some(index) = index {
                append_unique_key(&mut keys, format!("output:{index}"));
            }
            let call = match self.function_call_for_keys(&keys) {
                Some(call) => call,
                None => self.new_call(),
            };
            self.add_function_call_aliases(call, &keys);
            let empty = Value::Null;
            self.update_function_call_identity(call, &empty, &item);
            update_function_call_arguments(
                &mut self.calls[call],
                &item.g("arguments").str(),
                false,
            );
            self.calls[call].done = true;
        }

        let queue = std::mem::take(&mut self.function_call_queue);
        for call in queue {
            let c = &mut self.calls[call];
            if c.closed {
                continue;
            }
            if c.name.is_empty() {
                c.closed = true;
                continue;
            }
            c.done = true;
            self.function_call_queue.push(call);
        }
        self.append_function_call_queue(output, original);

        self.function_calls.clear();
        self.function_call_queue.clear();
        self.active_function_call = None;
        self.last_function_call = None;
    }

    // ---- text and thinking blocks

    fn start_text_block(&mut self) -> Vec<u8> {
        if self.text_block_open {
            return Vec::new();
        }
        let template = json!({"type": "content_block_start", "index": self.block_index, "content_block": {"type": "text", "text": ""}});
        self.text_block_open = true;
        sse("content_block_start", &template)
    }

    pub(super) fn stop_text_block(&mut self) -> Vec<u8> {
        if !self.text_block_open {
            return Vec::new();
        }
        let template = json!({"type": "content_block_stop", "index": self.block_index});
        self.text_block_open = false;
        self.block_index += 1;
        sse("content_block_stop", &template)
    }

    fn start_thinking_block(&mut self) -> Vec<u8> {
        if self.thinking_block_open {
            return Vec::new();
        }
        let template = json!({"type": "content_block_start", "index": self.block_index, "content_block": {"type": "thinking", "thinking": ""}});
        self.thinking_block_open = true;
        sse("content_block_start", &template)
    }

    /// A `thinking_delta` for the currently open thinking block.
    fn thinking_delta(&self, text: &str) -> Vec<u8> {
        if text.is_empty() {
            return Vec::new();
        }
        let template = json!({"type": "content_block_delta", "index": self.block_index, "delta": {"type": "thinking_delta", "thinking": text}});
        sse("content_block_delta", &template)
    }

    fn finalize_signature_only_thinking_block(&mut self) -> Vec<u8> {
        if self.thinking_signature.is_empty() {
            return Vec::new();
        }
        let mut output = self.start_thinking_block();
        output.extend(self.finalize_thinking_block());
        output
    }

    pub(super) fn finalize_thinking_block(&mut self) -> Vec<u8> {
        if !self.thinking_block_open {
            return Vec::new();
        }
        let mut output = Vec::with_capacity(256);
        if !self.thinking_signature.is_empty() {
            let signature_delta = json!({"type": "content_block_delta", "index": self.block_index, "delta": {"type": "signature_delta", "signature": self.thinking_signature}});
            append_sse_event_bytes(
                &mut output,
                "content_block_delta",
                &cpa_json::to_vec(&signature_delta),
                2,
            );
        }
        let stop = json!({"type": "content_block_stop", "index": self.block_index});
        append_sse_event_bytes(
            &mut output,
            "content_block_stop",
            &cpa_json::to_vec(&stop),
            2,
        );
        self.block_index += 1;
        self.thinking_block_open = false;
        output
    }
}

fn sse(event: &str, payload: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    append_sse_event_bytes(&mut out, event, &cpa_json::to_vec(payload), 2);
    out
}

/// Go's `s[from:]` on a string (byte offset), invalid bytes becoming U+FFFD.
fn go_slice_from(s: &str, from: usize) -> String {
    let bytes = s.as_bytes();
    go_lossy(&bytes[from.min(bytes.len())..])
}

fn should_defer_stream_event(type_str: &str, root: &Value) -> bool {
    match type_str {
        "error"
        | "response.completed"
        | "response.incomplete"
        | "response.function_call_arguments.delta"
        | "response.function_call_arguments.done" => false,
        "response.output_item.added" | "response.output_item.done" => {
            root.g("item.type").str() != "function_call"
        }
        _ => true,
    }
}

fn stream_error_to_claude_error(root: &Value) -> Vec<u8> {
    let error = root.g("error");
    let mut err_type = error.g("type").str().trim().to_string();
    if err_type.is_empty() {
        err_type = root.g("error_type").str().trim().to_string();
    }
    if err_type.is_empty() {
        err_type = "api_error".into();
    }

    let code = error.g("code").str().trim().to_string();
    let mut message = error.g("message").str().trim().to_string();
    if message.is_empty() {
        message = root.g("message").str().trim().to_string();
    }
    if message.is_empty() {
        message = code.clone();
    }
    if message.is_empty() {
        message = err_type.clone();
    }

    if code == "cyber_policy" || err_type == "invalid_request" {
        err_type = "invalid_request_error".into();
    }

    let out = json!({"type": "error", "error": {"type": err_type, "message": message}});
    let mut frame = Vec::new();
    append_sse_event_bytes(&mut frame, "error", &cpa_json::to_vec(&out), 2);
    frame
}

/// Go: ConvertCodexResponseToClaudeNonStream. Builds one Claude message from a terminal event.
pub fn convert_codex_response_to_claude_non_stream(
    _ctx: &Ctx,
    _model_name: &str,
    original_request: &[u8],
    _request: &[u8],
    raw: &[u8],
    _param: &mut Param,
) -> Option<Vec<u8>> {
    let rev_names = build_reverse_map_short_to_original(original_request);

    let root = cpa_json::parse(raw);
    let type_str = root.g("type").str();
    if type_str != "response.completed" && type_str != "response.incomplete" {
        return Some(Vec::new());
    }

    let response_data = root.g("response");
    if !response_data.exists() {
        return Some(Vec::new());
    }

    let mut out = cpa_json::parse_str(
        r#"{"id":"","type":"message","role":"assistant","model":"","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}"#,
    );
    cpa_json::set(&mut out, "id", response_data.g("id").str());
    cpa_json::set(&mut out, "model", response_data.g("model").str());
    let (input_tokens, output_tokens, cached_tokens, cache_write_tokens) =
        extract_responses_usage(&response_data.g("usage"));
    cpa_json::set(&mut out, "usage.input_tokens", input_tokens);
    cpa_json::set(&mut out, "usage.output_tokens", output_tokens);
    if cached_tokens > 0 {
        cpa_json::set(&mut out, "usage.cache_read_input_tokens", cached_tokens);
    }
    if cache_write_tokens > 0 {
        cpa_json::set(
            &mut out,
            "usage.cache_creation_input_tokens",
            cache_write_tokens,
        );
    }
    set_reasoning_usage(&mut out, &response_data.g("usage"));

    let mut has_tool_call = false;
    let mut web_search_seen: HashSet<String> = HashSet::new();
    let mut content_blocks: Vec<Value> = Vec::new();

    let output = response_data.g("output");
    if output.exists() && output.is_array() {
        for item in output.array() {
            match item.g("type").str().as_str() {
                "reasoning" => {
                    let mut thinking_text = String::new();
                    let signature = item.g("encrypted_content").str();
                    let summary = item.g("summary");
                    if summary.exists() {
                        if summary.is_array() {
                            for part in summary.array() {
                                push_part_text(&mut thinking_text, &part);
                            }
                        } else {
                            thinking_text.push_str(&summary.str());
                        }
                    }
                    if thinking_text.is_empty() {
                        let content = item.g("content");
                        if content.exists() {
                            if content.is_array() {
                                for part in content.array() {
                                    push_part_text(&mut thinking_text, &part);
                                }
                            } else {
                                thinking_text.push_str(&content.str());
                            }
                        }
                    }
                    if !thinking_text.is_empty() || !signature.is_empty() {
                        let mut block = json!({"type": "thinking", "thinking": thinking_text});
                        if !signature.is_empty() {
                            cpa_json::set(&mut block, "signature", signature);
                        }
                        content_blocks.push(block);
                    }
                }
                "message" => {
                    let content = item.g("content");
                    if content.exists() {
                        if content.is_array() {
                            for part in content.array() {
                                if part.g("type").str() == "output_text" {
                                    let text = part.g("text").str();
                                    if !text.is_empty() {
                                        content_blocks.push(json!({"type": "text", "text": text}));
                                    }
                                }
                            }
                        } else {
                            let text = content.str();
                            if !text.is_empty() {
                                content_blocks.push(json!({"type": "text", "text": text}));
                            }
                        }
                    }
                }
                "web_search_call" => append_web_search_non_stream_blocks(
                    &mut content_blocks,
                    &item,
                    &mut web_search_seen,
                ),
                "function_call" => {
                    has_tool_call = true;
                    let mut name = item.g("name").str();
                    if let Some(original) = rev_names.get(&name) {
                        name = original.clone();
                    }
                    let mut input = json!({});
                    let args_str = item.g("arguments").str();
                    if !args_str.is_empty() && cpa_json::valid(args_str.as_bytes()) {
                        let args = cpa_json::parse_str(&args_str);
                        if args.is_object() {
                            input = args;
                        }
                    }
                    content_blocks.push(json!({
                        "type": "tool_use",
                        "id": shorten_call_id_if_needed(&sanitize_claude_tool_id(&item.g("call_id").str())),
                        "name": name,
                        "input": input,
                    }));
                }
                _ => {}
            }
        }
    }

    if !content_blocks.is_empty() {
        cpa_json::set(&mut out, "content", Value::Array(content_blocks));
    }

    cpa_json::set(
        &mut out,
        "stop_reason",
        map_stop_reason_to_claude(&stop_reason(&response_data), has_tool_call),
    );
    set_stop_sequence(&mut out, "stop_sequence", &response_data);

    Some(cpa_json::to_vec(&out))
}

/// Appends a summary/content part's `text`, or the part itself when it has none.
fn push_part_text(builder: &mut String, part: &Res<'_>) {
    let txt = part.g("text");
    if txt.exists() {
        builder.push_str(&txt.str());
    } else {
        builder.push_str(&part.str());
    }
}

fn stop_reason(response_data: &Res<'_>) -> String {
    let stop_reason = response_data.g("stop_reason");
    if stop_reason.exists() && !stop_reason.str().is_empty() {
        if stop_reason.str() == "stop" && !response_data.g("stop_sequence").str().is_empty() {
            return "stop_sequence".into();
        }
        return stop_reason.str();
    }
    let reason = response_data.g("incomplete_details.reason");
    if reason.exists() && !reason.str().is_empty() {
        return reason.str();
    }
    if !response_data.g("stop_sequence").str().is_empty() {
        return "stop_sequence".into();
    }
    String::new()
}

fn map_stop_reason_to_claude(stop_reason: &str, has_tool_call: bool) -> String {
    if has_tool_call {
        return "tool_use".into();
    }
    match stop_reason {
        "" | "stop" | "completed" => "end_turn",
        "max_tokens" | "max_output_tokens" => "max_tokens",
        "tool_use" | "tool_calls" | "function_call" => "end_turn",
        "end_turn"
        | "stop_sequence"
        | "pause_turn"
        | "refusal"
        | "model_context_window_exceeded" => stop_reason,
        "content_filter" => "refusal",
        _ => "end_turn",
    }
    .into()
}

fn set_stop_sequence(out: &mut Value, path: &str, response_data: &Res<'_>) {
    let stop_sequence = response_data.g("stop_sequence");
    if stop_sequence.exists() && !stop_sequence.str().is_empty() {
        cpa_json::set(out, path, stop_sequence.value());
    }
}

/// Identity keys of a function call event: output index, call id and item id, each prefixed.
fn function_call_keys(root: &Res<'_>, item: &Res<'_>) -> Vec<String> {
    let mut keys = Vec::with_capacity(5);
    let output_index = root.g("output_index");
    if output_index.exists() {
        append_unique_key(&mut keys, format!("output:{}", output_index.raw()));
    }
    let call_id = item.g("call_id").str();
    if !call_id.is_empty() {
        append_unique_key(&mut keys, format!("call:{call_id}"));
    }
    let call_id = root.g("call_id").str();
    if !call_id.is_empty() {
        append_unique_key(&mut keys, format!("call:{call_id}"));
    }
    let item_id = item.g("id").str();
    if !item_id.is_empty() {
        append_unique_key(&mut keys, format!("item:{item_id}"));
    }
    let item_id = root.g("item_id").str();
    if !item_id.is_empty() {
        append_unique_key(&mut keys, format!("item:{item_id}"));
    }
    keys
}

fn append_unique_key(keys: &mut Vec<String>, key: String) {
    if !key.is_empty() && !keys.contains(&key) {
        keys.push(key);
    }
}

/// Arguments arrive as deltas and/or a final value; a final value replaces the buffer unless it
/// contradicts already received deltas.
fn update_function_call_arguments(call: &mut FunctionCall, arguments: &str, delta: bool) {
    if arguments.is_empty() {
        return;
    }
    if delta {
        call.arguments.push_str(arguments);
        call.has_received_arguments_delta = true;
        return;
    }
    if !call.has_received_arguments_delta {
        call.arguments = arguments.to_string();
        return;
    }
    if arguments.starts_with(&call.arguments) {
        call.arguments = arguments.to_string();
    }
}

fn append_function_call_start(
    output: &mut Vec<u8>,
    original: &[u8],
    call_id: &str,
    name: &str,
    block_index: i64,
) {
    let template = json!({
        "type": "content_block_start",
        "index": block_index,
        "content_block": {
            "type": "tool_use",
            "id": shorten_call_id_if_needed(&sanitize_claude_tool_id(call_id)),
            "name": resolve_tool_use_name(original, name),
            "input": {},
        },
    });
    append_sse_event_bytes(
        output,
        "content_block_start",
        &cpa_json::to_vec(&template),
        2,
    );
}

fn append_function_call_argument_delta(output: &mut Vec<u8>, partial_json: &str, block_index: i64) {
    let template = json!({"type": "content_block_delta", "index": block_index, "delta": {"type": "input_json_delta", "partial_json": partial_json}});
    append_sse_event_bytes(
        output,
        "content_block_delta",
        &cpa_json::to_vec(&template),
        2,
    );
}

fn append_function_call_stop(output: &mut Vec<u8>, block_index: i64) {
    let template = json!({"type": "content_block_stop", "index": block_index});
    append_sse_event_bytes(
        output,
        "content_block_stop",
        &cpa_json::to_vec(&template),
        2,
    );
}

fn resolve_tool_use_name(original: &[u8], name: &str) -> String {
    build_reverse_map_short_to_original(original)
        .get(name)
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

/// (input, output, cached, cache write) tokens; cached and cache-write tokens are deducted from
/// input like Claude reports them.
fn extract_responses_usage(usage: &Res<'_>) -> (i64, i64, i64, i64) {
    if !usage.exists() || usage.is_null() {
        return (0, 0, 0, 0);
    }
    let mut input_tokens = usage.g("input_tokens").int();
    let output_tokens = usage.g("output_tokens").int();
    let cached_tokens = usage.g("input_tokens_details.cached_tokens").int();
    let mut cache_write_tokens = usage.g("input_tokens_details.cache_write_tokens").int();
    if cache_write_tokens <= 0 {
        cache_write_tokens = usage.g("input_tokens_details.cache_creation_tokens").int();
    }

    let mut deduct = 0i64;
    if cached_tokens > 0 {
        deduct += cached_tokens;
    }
    if cache_write_tokens > 0 {
        deduct = deduct.saturating_add(cache_write_tokens);
    }
    if deduct > 0 {
        input_tokens = if input_tokens >= deduct {
            input_tokens - deduct
        } else {
            0
        };
    }
    if input_tokens < 0 {
        input_tokens = 0;
    }
    (
        input_tokens,
        output_tokens,
        cached_tokens,
        cache_write_tokens,
    )
}

fn set_reasoning_usage(out: &mut Value, usage: &Res<'_>) {
    let detail = usage.g("output_tokens_details.reasoning_tokens");
    if !detail.exists() || !detail.is_number() {
        return;
    }
    let num = detail.float();
    if detail.raw().starts_with('-') || num < 0.0 {
        return;
    }
    let output_tokens = usage.g("output_tokens").int().max(0);
    let tokens = if num >= output_tokens as f64 {
        output_tokens
    } else {
        detail.int()
    };
    cpa_json::set(out, "usage.output_tokens_details.thinking_tokens", tokens);
}

/// Go: ClaudeTokenCount.
pub fn claude_token_count(_ctx: &Ctx, count: i64) -> Vec<u8> {
    claude_input_tokens_json(count)
}
