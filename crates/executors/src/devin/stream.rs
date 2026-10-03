//! Devin response frames to interactions events (Go: devin_executor.go `streamDevinFrames` and
//! `consumeDevinFramesToInteractions`).
//!
//! Streaming turns Connect frames into interactions events (`step.start`, `step.delta`,
//! `step.stop`, `interaction.completed`), translates each to the client format and forwards the
//! resulting chunks. Non-stream execution folds all frames into one interactions response.

use std::collections::{BTreeMap, HashMap};

use bytes::Bytes;
use cpa_json::{J, Value, json};
use cpa_runtime::executor::ExecError;
use cpa_translator::{Format, Param};
use futures_util::Stream;
use tokio::sync::oneshot;

use crate::helps::claude_input_tokens::ClaudeInputTokenState;
use super::wire::{
    CONNECT_FLAG_END_STREAM, ConnectFrameReader, FrameError, FrameResult, ToolCallDelta, Usage,
    Utf8SplitBuffer, go_lossy, parse_frame, parse_response_dimension_groups, parse_trailer_error,
};
use crate::helps::apply_patch::{
    ChunkSender, end_apply_patch_stream, gateway_error, initialize_apply_patch_stream, is_apply_patch_upstream_tool,
    record_apply_patch_stream_failure, stop_apply_patch_stream,
};
use crate::helps::status::status_err;
use crate::helps::usage::{UsageReporter, parse_interactions_stream_usage};

/// Upper bound of distinct tool calls per response; extra calls are dropped.
pub const MAX_TOOL_CALLS: usize = 128;

/// Error without an HTTP status (stream read failures, truncated streams).
fn plain_error(message: impl Into<String>) -> ExecError {
    ExecError::new(0, message)
}

fn new_interaction_id() -> String {
    let id = uuid::Uuid::new_v4().to_string();
    format!("interaction_{}", &id[..12])
}

/// `(status, finish_reason)` for an upstream stop reason.
fn completion_status(stop_reason: u64) -> (&'static str, Option<&'static str>) {
    match stop_reason {
        1 | 3 => ("incomplete", Some("length")),
        11 => ("incomplete", Some("content_filter")),
        _ => ("completed", None),
    }
}

/// Folds a usage frame into the running usage: positive counters and non-empty ids overwrite.
/// Returns true when the frame carried a model name.
fn merge_usage(final_usage: &mut Option<Usage>, frame: Usage) -> bool {
    let Some(cur) = final_usage else {
        let has_model = !frame.model_name.is_empty();
        *final_usage = Some(frame);
        return has_model;
    };
    if frame.prompt_tokens > 0 {
        cur.prompt_tokens = frame.prompt_tokens;
    }
    if frame.completion_tokens > 0 {
        cur.completion_tokens = frame.completion_tokens;
    }
    if frame.cached_tokens > 0 {
        cur.cached_tokens = frame.cached_tokens;
    }
    if frame.cache_write_tokens > 0 {
        cur.cache_write_tokens = frame.cache_write_tokens;
    }
    if !frame.request_id.is_empty() {
        cur.request_id = frame.request_id;
    }
    let has_model = !frame.model_name.is_empty();
    if has_model {
        cur.model_name = frame.model_name;
    }
    cur.headers.extend(frame.headers);
    has_model
}

/// Token usage from field 28 groups fills counters that are still zero.
fn merge_dimension_groups(final_usage: &mut Option<Usage>, groups: &[Vec<u8>]) {
    let missing = final_usage
        .as_ref()
        .is_none_or(|u| u.prompt_tokens == 0 || u.completion_tokens == 0 || u.cached_tokens == 0);
    if groups.is_empty() || !missing {
        return;
    }
    let (input, output, cached, ok) = parse_response_dimension_groups(groups);
    if !ok {
        return;
    }
    let u = final_usage.get_or_insert_with(Usage::default);
    if u.prompt_tokens == 0 {
        u.prompt_tokens = input;
    }
    if u.completion_tokens == 0 {
        u.completion_tokens = output;
    }
    if u.cached_tokens == 0 {
        u.cached_tokens = cached;
    }
}

/// Writes the usage block shared by `interaction.completed` and non-stream responses.
fn set_usage(target: &mut Value, prefix: &str, usage: &Usage) {
    let total_input = usage.prompt_tokens + usage.cached_tokens;
    let total_output = usage.completion_tokens;
    cpa_json::set(
        target,
        &format!("{prefix}usage.total_input_tokens"),
        total_input,
    );
    cpa_json::set(
        target,
        &format!("{prefix}usage.total_output_tokens"),
        total_output,
    );
    cpa_json::set(
        target,
        &format!("{prefix}usage.total_cached_tokens"),
        usage.cached_tokens,
    );
    if usage.cache_write_tokens > 0 {
        cpa_json::set(
            target,
            &format!("{prefix}usage.cache_write_tokens"),
            usage.cache_write_tokens,
        );
    }
    cpa_json::set(
        target,
        &format!("{prefix}usage.total_tokens"),
        total_input + total_output,
    );
}

fn arguments_chunk(tc: &ToolCallDelta) -> &str {
    if tc.arguments.is_empty() {
        &tc.invalid_json_str
    } else {
        &tc.arguments
    }
}

// ------------------------------------------------------------------ streaming

/// Inputs of [`stream_frames`].
pub struct StreamParams {
    /// The requested model as the client named it (`req.Model`).
    pub model: String,
    /// `req.Payload`, the translated request handed to response translators.
    pub request: Bytes,
    /// Request used by the apply_patch bridge (`opts.original_request`, else the payload).
    pub original: Bytes,
    /// `opts.original_request`, for Claude input token estimates.
    pub client_original: Bytes,
    pub source_format: Format,
    pub response_format: Format,
    pub chat_model_uid: String,
    pub reporter: UsageReporter,
}

/// A tool call being streamed: its step index and the identity seen so far.
struct ToolSlot {
    id: String,
    name: String,
}

/// Content that arrived while a thought step was open and is replayed after it closes.
enum PendingAction {
    Tool(ToolCallDelta),
    Content(String),
}

struct StreamState {
    p: StreamParams,
    out: ChunkSender,
    interaction_id: String,
    claude_tokens: ClaudeInputTokenState,
    param: Param,
    translation_failed: bool,
    created_sent: bool,

    step_index: i64,
    thought_started: bool,
    content_started: bool,
    thought_step_index: i64,
    /// Open tool steps by step index (iterated in order when closing).
    active_tool_slots: BTreeMap<i64, ToolSlot>,
    active_call_by_id: HashMap<String, i64>,
    active_call_slot: Option<i64>,
    tool_call_count: usize,
    pending_actions: Vec<PendingAction>,
    /// Text after tool calls, flushed once the tools are closed so tool items precede the
    /// assistant message in Responses clients.
    post_tool_buffered_content: Vec<String>,
}

impl StreamState {
    async fn send_chunk(&mut self, chunk: Vec<u8>) -> bool {
        self.out.send(Ok(Bytes::from(chunk))).await.is_ok()
    }

    /// Emits one interactions event: synthesizes `interaction.created` first, suppresses failures
    /// before any content (so the stream fails at the bootstrap layer with an HTTP error),
    /// then forwards the event in the response format. False means stop (client gone or the
    /// bridge failed).
    async fn emit(&mut self, event: Value) -> bool {
        if self.translation_failed {
            return false;
        }
        let event_type = event.g("event_type").str();
        let is_failed = event_type == "response.failed" || event_type == "interaction.failed";
        if is_failed && !self.created_sent {
            return true;
        }
        if !self.created_sent && event_type != "interaction.created" {
            self.created_sent = true;
            let created = json!({
                "event_type": "interaction.created",
                "interaction": {"id": self.interaction_id, "model": self.p.model},
            });
            if !self.deliver(&created).await {
                return false;
            }
        }
        if event_type == "interaction.created" {
            self.created_sent = true;
        }
        self.deliver(&event).await
    }

    async fn deliver(&mut self, event: &Value) -> bool {
        let raw = cpa_json::to_vec(event);
        if self.p.response_format == Format::Interactions {
            let mut frame = Vec::with_capacity(raw.len() + 8);
            frame.extend_from_slice(b"data: ");
            frame.extend_from_slice(&raw);
            frame.extend_from_slice(b"\n\n");
            return self.send_chunk(frame).await;
        }
        let lines = self.translate(&raw);
        record_apply_patch_stream_failure(
            &self.param,
            &self.p.reporter,
            &gateway_error(),
        );
        for line in lines {
            if !self.send_chunk(line).await {
                return false;
            }
        }
        if self.stop_if_apply_patch_failed().await {
            self.translation_failed = true;
            return false;
        }
        true
    }

    /// EOF check before any synthetic success: delivers the bridge's finalize frames, then the
    /// gateway error if the stream failed. True means the caller must stop.
    async fn end_apply_patch(&mut self) -> bool {
        end_apply_patch_stream(&mut self.param, &self.p.reporter, &self.out, gateway_error()).await
    }

    /// Forwards the sanitized gateway error when the bridge recorded a tool input failure.
    async fn stop_if_apply_patch_failed(&mut self) -> bool {
        stop_apply_patch_stream(&self.param, &self.p.reporter, &self.out, gateway_error()).await
    }

    fn translate(&mut self, raw: &[u8]) -> Vec<Vec<u8>> {
        self.claude_tokens.translate_stream(
            Format::Interactions,
            self.p.response_format,
            &self.p.model,
            &self.p.original,
            &self.p.request,
            raw,
            &mut self.param,
        )
    }

    async fn emit_stream_error(&mut self, err: ExecError) {
        let _ = self.out.send(Err(err)).await;
    }

    async fn stop_step(&mut self, index: i64) -> bool {
        self.emit(json!({"event_type": "step.stop", "index": index}))
            .await
    }

    async fn start_model_output(&mut self) -> bool {
        let idx = self.step_index;
        self.emit(
            json!({"event_type": "step.start", "index": idx, "step": {"type": "model_output"}}),
        )
        .await
    }

    async fn text_delta(&mut self, text: &str) -> bool {
        let idx = self.step_index;
        self.emit(json!({
            "event_type": "step.delta", "index": idx, "delta": {"type": "text", "text": text}
        }))
        .await
    }

    async fn start_thought(&mut self) -> bool {
        self.thought_step_index = self.step_index;
        let idx = self.step_index;
        let ok = self
            .emit(json!({"event_type": "step.start", "index": idx, "step": {"type": "thought"}}))
            .await;
        self.thought_started = true;
        ok
    }

    /// Closes the thought step and replays everything buffered behind it.
    async fn flush_pending_actions(&mut self) -> bool {
        if self.thought_started {
            let idx = self.thought_step_index;
            if !self.stop_step(idx).await {
                return false;
            }
            self.thought_started = false;
            self.step_index += 1;
        }
        let actions = std::mem::take(&mut self.pending_actions);
        for action in actions {
            let ok = match action {
                PendingAction::Tool(tc) => self.emit_tool_call(tc).await,
                PendingAction::Content(chunk) => self.emit_content_chunk(&chunk).await,
            };
            if !ok {
                return false;
            }
        }
        true
    }

    async fn emit_content_chunk(&mut self, chunk: &str) -> bool {
        if self.thought_started {
            let stop_idx = if self.thought_step_index < 0 {
                self.step_index
            } else {
                self.thought_step_index
            };
            if !self.stop_step(stop_idx).await {
                return false;
            }
            self.thought_started = false;
            self.step_index += 1;
        }
        if self.tool_call_count > 0 {
            self.post_tool_buffered_content.push(chunk.to_string());
            return true;
        }
        if !self.content_started {
            if !self.start_model_output().await {
                return false;
            }
            self.content_started = true;
        }
        self.text_delta(chunk).await
    }

    fn tool_start_event(index: i64, name: &str, id: &str) -> Value {
        json!({
            "event_type": "step.start",
            "index": index,
            "step": {"type": "function_call", "name": name, "id": id, "call_id": id, "arguments": {}},
        })
    }

    async fn emit_tool_call(&mut self, tc: ToolCallDelta) -> bool {
        if self.thought_started {
            let idx = self.step_index;
            if !self.stop_step(idx).await {
                return false;
            }
            self.thought_started = false;
            self.step_index += 1;
        }
        if self.content_started {
            let idx = self.step_index;
            if !self.stop_step(idx).await {
                return false;
            }
            self.content_started = false;
            self.step_index += 1;
        }

        let args_chunk = arguments_chunk(&tc).to_string();
        let found = if !tc.id.is_empty() {
            self.active_call_by_id.get(&tc.id).copied()
        } else {
            self.active_call_slot
        };

        let slot_index = match found {
            None => {
                if self.tool_call_count >= MAX_TOOL_CALLS {
                    tracing::warn!(
                        "devin executor: total tool calls exceeded max {MAX_TOOL_CALLS}, dropping"
                    );
                    return true;
                }
                self.tool_call_count += 1;
                let s_idx = self.step_index;
                self.step_index += 1;
                self.active_tool_slots.insert(
                    s_idx,
                    ToolSlot {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                    },
                );
                if !tc.id.is_empty() {
                    self.active_call_by_id.insert(tc.id.clone(), s_idx);
                }
                self.active_call_slot = Some(s_idx);
                if !self
                    .emit(Self::tool_start_event(s_idx, &tc.name, &tc.id))
                    .await
                {
                    return false;
                }
                s_idx
            }
            Some(s_idx) => {
                self.active_call_slot = Some(s_idx);
                let mut update = None;
                if let Some(slot) = self.active_tool_slots.get_mut(&s_idx) {
                    let mut updated = false;
                    if slot.id.is_empty() && !tc.id.is_empty() {
                        slot.id = tc.id.clone();
                        updated = true;
                    }
                    if slot.name.is_empty() && !tc.name.is_empty() {
                        slot.name = tc.name.clone();
                        updated = true;
                    }
                    if updated {
                        update = Some((slot.name.clone(), slot.id.clone()));
                    }
                }
                if let Some((name, id)) = update {
                    if !tc.id.is_empty() {
                        self.active_call_by_id.insert(tc.id.clone(), s_idx);
                    }
                    if !self.emit(Self::tool_start_event(s_idx, &name, &id)).await {
                        return false;
                    }
                }
                s_idx
            }
        };

        if !args_chunk.is_empty() {
            let mut delta = json!({
                "event_type": "step.delta",
                "index": slot_index,
                "delta": {"type": "arguments_delta", "arguments": args_chunk},
            });
            if tc.arguments.is_empty() && !tc.invalid_json_str.is_empty() {
                cpa_json::set(&mut delta, "delta.invalid_json_str", true);
            }
            if !self.emit(delta).await {
                return false;
            }
        }
        true
    }

    /// Closes thought, tool and text steps in order and flushes buffered post-tool text.
    async fn close_open_steps(&mut self) {
        if !self.pending_actions.is_empty() || self.thought_started {
            let _ = self.flush_pending_actions().await;
        }
        if !self.active_tool_slots.is_empty() {
            let indices: Vec<i64> = self.active_tool_slots.keys().copied().collect();
            for idx in indices {
                let _ = self.stop_step(idx).await;
            }
            self.active_tool_slots.clear();
            self.active_call_by_id.clear();
            self.active_call_slot = None;
        }
        if !self.post_tool_buffered_content.is_empty() {
            let _ = self.start_model_output().await;
            self.content_started = true;
            for chunk in std::mem::take(&mut self.post_tool_buffered_content) {
                let _ = self.text_delta(&chunk).await;
            }
        }
        if self.content_started {
            let idx = self.step_index;
            let _ = self.stop_step(idx).await;
            self.content_started = false;
        }
    }

    async fn send_failed_event(&mut self, message: &str, code: &str) {
        let _ = self.emit(json!({"event_type": "response.failed", "error": {"message": message, "code": code}})).await;
    }
}

/// Streams the Connect frames of `reader` to `out` as client-format chunks. `usage_tx` receives
/// the final usage when the stream completes. Returns when the stream ends or the receiver is
/// dropped.
pub async fn stream_frames<S, E>(
    mut reader: ConnectFrameReader<S>,
    p: StreamParams,
    out: ChunkSender,
    usage_tx: oneshot::Sender<Value>,
) where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    p.reporter.set_upstream_model(&p.chat_model_uid);
    let claude_tokens = ClaudeInputTokenState::new(
        p.source_format,
        Format::Interactions,
        p.response_format,
        &p.client_original,
    );
    let mut param = Param::default();
    initialize_apply_patch_stream(
        Format::Interactions,
        p.response_format,
        &p.model,
        &p.original,
        &p.request,
        &mut param,
    );
    let mut st = StreamState {
        out,
        interaction_id: new_interaction_id(),
        claude_tokens,
        param,
        translation_failed: false,
        created_sent: false,
        step_index: 0,
        thought_started: false,
        content_started: false,
        thought_step_index: -1,
        active_tool_slots: BTreeMap::new(),
        active_call_by_id: HashMap::new(),
        active_call_slot: None,
        tool_call_count: 0,
        pending_actions: Vec::new(),
        post_tool_buffered_content: Vec::new(),
        p,
    };

    let mut thinking_buf = Utf8SplitBuffer::default();
    let mut content_buf = Utf8SplitBuffer::default();
    let mut final_usage: Option<Usage> = None;
    let mut stream_err: Option<FrameError> = None;
    let mut last_stop_reason = 0u64;
    let mut saw_eos = false;

    // 1. Consume frames.
    loop {
        let frame = match reader.read_frame().await {
            Ok(f) => f,
            Err(FrameError::Eof) => break,
            Err(e) => {
                tracing::warn!("devin executor: stream read error: {e}");
                stream_err = Some(e);
                break;
            }
        };

        if frame.flag & CONNECT_FLAG_END_STREAM != 0 {
            if let Some(trailer) = parse_trailer_error(&frame.payload) {
                if st.end_apply_patch().await {
                    return;
                }
                st.close_open_steps().await;
                if st.translation_failed {
                    return;
                }
                let err = status_err(trailer.status, trailer.message.clone());
                st.p.reporter.publish_failure(&err);
                tracing::warn!(
                    "devin executor: trailer error ({}): {}",
                    trailer.status,
                    trailer.message
                );
                st.send_failed_event(&trailer.message, &trailer.status.to_string())
                    .await;
                st.emit_stream_error(err).await;
                return;
            }
            saw_eos = true;
            break;
        }

        let Ok(res) = parse_frame(&frame.payload) else {
            continue;
        };
        if res.stop_reason != 0 {
            last_stop_reason = res.stop_reason;
        }
        if let Some(u) = res.usage.clone()
            && merge_usage(&mut final_usage, u)
            && let Some(name) = final_usage.as_ref().map(|u| u.model_name.clone())
        {
            st.p.reporter.set_response_model(&name);
        }
        merge_dimension_groups(&mut final_usage, &res.response_dimension_groups);

        if !handle_frame(&mut st, &res, &mut thinking_buf, &mut content_buf).await {
            return;
        }
    }

    if (!saw_eos || stream_err.is_some()) && st.end_apply_patch().await {
        return;
    }
    // 2. Close open steps.
    st.close_open_steps().await;
    if st.translation_failed {
        return;
    }

    // Abnormal read failure mid-flight.
    if let Some(err) = stream_err {
        let msg = err.to_string();
        st.send_failed_event(&msg, "stream_read_error").await;
        st.emit_stream_error(plain_error(msg)).await;
        return;
    }
    // A Connect stream must end with an EOS trailer; a bare close is a truncated response.
    if !saw_eos {
        let msg = "devin stream terminated prematurely before EOS trailer";
        st.send_failed_event(msg, "stream_truncated").await;
        st.emit_stream_error(plain_error(msg)).await;
        return;
    }

    // 3. interaction.completed with final usage.
    let (status, finish_reason) = completion_status(last_stop_reason);
    let mut completed = json!({
        "event_type": "interaction.completed",
        "interaction": {
            "id": st.interaction_id,
            "model": st.p.model,
            "status": status,
            "usage": {"total_input_tokens": 0, "total_output_tokens": 0, "total_cached_tokens": 0},
        },
    });
    if let Some(reason) = finish_reason {
        cpa_json::set(&mut completed, "interaction.finish_reason", reason);
    }
    if let Some(u) = &final_usage {
        set_usage(&mut completed, "interaction.", u);
        if !u.model_name.is_empty() {
            st.p.reporter.set_response_model(&u.model_name);
        }
    }
    if !st.emit(completed.clone()).await {
        return;
    }
    if let Some(detail) = parse_interactions_stream_usage(&cpa_json::to_vec(&completed)) {
        let _ = usage_tx.send(UsageReporter::usage_metadata(&detail));
        st.p.reporter.publish(detail);
    }

    // 4. Terminate the stream with [DONE].
    if st.p.response_format == Format::Interactions {
        let _ = st.send_chunk(b"data: [DONE]\n\n".to_vec()).await;
    } else {
        let lines = st.translate(b"[DONE]");
        record_apply_patch_stream_failure(&st.param, &st.p.reporter, &gateway_error());
        for line in lines {
            if !st.send_chunk(line).await {
                break;
            }
        }
    }
}

/// Emits the events of one decoded frame. False means the stream must stop.
async fn handle_frame(
    st: &mut StreamState,
    res: &FrameResult,
    thinking_buf: &mut Utf8SplitBuffer,
    content_buf: &mut Utf8SplitBuffer,
) -> bool {
    // Thinking delta. Content and tools arriving while a thought is open are buffered so
    // late or split signatures can still close the thinking block.
    if !res.thinking_text.is_empty() {
        if !st.pending_actions.is_empty() && !st.flush_pending_actions().await {
            return false;
        }
        let chunk = thinking_buf.feed(&res.thinking_text);
        if !chunk.is_empty() {
            if st.content_started {
                let idx = st.step_index;
                if !st.stop_step(idx).await {
                    return false;
                }
                st.content_started = false;
                st.step_index += 1;
            }
            if !st.thought_started && !st.start_thought().await {
                return false;
            }
            let idx = st.thought_step_index;
            let delta = json!({
                "event_type": "step.delta",
                "index": idx,
                "delta": {"type": "thought_summary", "text": chunk, "content": {"type": "text", "text": chunk}},
            });
            if !st.emit(delta).await {
                return false;
            }
        }
    }

    // Thinking signature delta, targeting the thought step.
    if !res.delta_signature.is_empty() {
        if st.thought_step_index == -1 && !st.content_started && !st.start_thought().await {
            return false;
        }
        let target = st.thought_step_index.max(0);
        let mut sig = json!({
            "event_type": "step.delta",
            "index": target,
            "delta": {"type": "thought_signature", "signature": go_lossy(&res.delta_signature)},
        });
        if !res.delta_signature_type.is_empty() {
            cpa_json::set(
                &mut sig,
                "delta.signature_type",
                res.delta_signature_type.as_str(),
            );
        }
        if !st.emit(sig).await {
            return false;
        }
    }

    // Tool call deltas.
    for tc in &res.tool_call_deltas {
        if st.thought_started {
            st.pending_actions.push(PendingAction::Tool(tc.clone()));
        } else if !st.emit_tool_call(tc.clone()).await {
            return false;
        }
    }

    // Content text delta.
    if !res.content_text.is_empty() {
        let chunk = content_buf.feed(&res.content_text);
        if !chunk.is_empty() {
            if st.thought_started {
                st.pending_actions.push(PendingAction::Content(chunk));
            } else if !st.emit_content_chunk(&chunk).await {
                return false;
            }
        }
    }
    true
}

// ------------------------------------------------------------------ non-stream

/// Result of folding all frames of a response.
pub struct Consumed {
    /// The interactions response document.
    pub interactions: Value,
    pub usage: Option<Usage>,
}

struct ToolBuilder {
    /// Arguments arrived as `invalid_json_str` (custom tool call with non-JSON input).
    legacy: bool,
    id: String,
    name: String,
    args: String,
}

/// Reads every frame of the response into one interactions response. `original` is the request
/// whose declarations identify apply_patch tools.
pub async fn consume_frames_to_interactions<S, E>(
    mut reader: ConnectFrameReader<S>,
    model: &str,
    original: &[u8],
) -> Result<Consumed, ExecError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: std::fmt::Display,
{
    let interaction_id = new_interaction_id();
    // Text stays bytes until the end: a character may be split across frames.
    let mut pre_tool_text: Vec<u8> = Vec::new();
    let mut post_tool_text: Vec<u8> = Vec::new();
    let mut have_pre = false;
    let mut have_post = false;
    let mut thinking: Vec<u8> = Vec::new();
    let mut have_thinking = false;
    let mut builders: Vec<ToolBuilder> = Vec::new();
    let mut call_id_to_builder: HashMap<String, usize> = HashMap::new();
    let mut last_builder: Option<usize> = None;
    let mut final_usage: Option<Usage> = None;
    let mut signature: Vec<u8> = Vec::new();
    let mut last_stop_reason = 0u64;
    let mut saw_eos = false;

    loop {
        let frame = match reader.read_frame().await {
            Ok(f) => f,
            Err(FrameError::Eof) => break,
            Err(e) => return Err(plain_error(e.to_string())),
        };
        if frame.flag & CONNECT_FLAG_END_STREAM != 0 {
            if let Some(trailer) = parse_trailer_error(&frame.payload) {
                return Err(status_err(trailer.status, trailer.message));
            }
            saw_eos = true;
            break;
        }
        let Ok(res) = parse_frame(&frame.payload) else {
            continue;
        };
        if res.stop_reason != 0 {
            last_stop_reason = res.stop_reason;
        }
        if let Some(u) = res.usage.clone() {
            merge_usage(&mut final_usage, u);
        }
        merge_dimension_groups(&mut final_usage, &res.response_dimension_groups);
        signature.extend_from_slice(&res.delta_signature);
        if !res.thinking_text.is_empty() {
            have_thinking = true;
            thinking.extend_from_slice(&res.thinking_text);
        }
        for tc in &res.tool_call_deltas {
            let chunk = arguments_chunk(tc);
            let found = if !tc.id.is_empty() {
                call_id_to_builder.get(&tc.id).copied()
            } else {
                last_builder
            };
            let idx = match found {
                None => {
                    if builders.len() >= MAX_TOOL_CALLS {
                        tracing::warn!(
                            "devin executor: total tool calls exceeded max {MAX_TOOL_CALLS}, dropping"
                        );
                        continue;
                    }
                    let idx = builders.len();
                    builders.push(ToolBuilder {
                        legacy: false,
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        args: String::new(),
                    });
                    if !tc.id.is_empty() {
                        call_id_to_builder.insert(tc.id.clone(), idx);
                    }
                    idx
                }
                Some(idx) => {
                    let b = &mut builders[idx];
                    if b.id.is_empty() && !tc.id.is_empty() {
                        b.id = tc.id.clone();
                        call_id_to_builder.insert(tc.id.clone(), idx);
                    }
                    if !tc.name.is_empty() {
                        b.name = tc.name.clone();
                    }
                    idx
                }
            };
            last_builder = Some(idx);
            if tc.arguments.is_empty() && !tc.invalid_json_str.is_empty() {
                builders[idx].legacy = true;
            }
            builders[idx].args.push_str(chunk);
        }
        if !res.content_text.is_empty() {
            if builders.is_empty() {
                have_pre = true;
                pre_tool_text.extend_from_slice(&res.content_text);
            } else {
                have_post = true;
                post_tool_text.extend_from_slice(&res.content_text);
            }
        }
    }

    if !original.is_empty() {
        for b in &builders {
            if b.legacy && is_apply_patch_upstream_tool(original, &b.name) {
                return Err(gateway_error());
            }
        }
    }
    if !saw_eos {
        return Err(plain_error(
            "devin upstream stream terminated prematurely before EOS trailer",
        ));
    }

    let (status, finish_reason) = completion_status(last_stop_reason);
    let mut out = json!({
        "id": interaction_id,
        "model": model,
        "status": status,
        "steps": [],
        "usage": {"total_input_tokens": 0, "total_output_tokens": 0, "total_cached_tokens": 0},
    });
    if let Some(reason) = finish_reason {
        cpa_json::set(&mut out, "finish_reason", reason);
    }

    let mut steps: Vec<Value> = Vec::new();
    if have_thinking || !signature.is_empty() {
        let mut thought = if have_thinking {
            json!({"type": "thought", "content": [{"type": "text", "text": go_lossy(&thinking)}]})
        } else {
            json!({"type": "thought"})
        };
        if !signature.is_empty() {
            let sig = go_lossy(&signature);
            cpa_json::set(&mut thought, "signature", sig.as_str());
            cpa_json::set(&mut thought, "thought_signature", sig.as_str());
        }
        steps.push(thought);
    }
    if have_pre {
        steps.push(json!({"type": "model_output", "content": [{"type": "text", "text": go_lossy(&pre_tool_text)}]}));
    }
    for b in builders
        .iter()
        .filter(|b| !(b.id.is_empty() && b.name.is_empty() && b.args.is_empty()))
    {
        let mut step = json!({"type": "function_call", "name": b.name, "id": b.id, "call_id": b.id, "arguments": {}});
        if !b.args.is_empty() {
            // Valid JSON arguments are embedded as JSON, anything else stays a string.
            match serde_json::from_str::<Value>(&b.args) {
                Ok(v) => {
                    cpa_json::set(&mut step, "arguments", v);
                }
                Err(_) => {
                    cpa_json::set(&mut step, "arguments", b.args.as_str());
                }
            }
        }
        steps.push(step);
    }
    if have_post {
        steps.push(json!({"type": "model_output", "content": [{"type": "text", "text": go_lossy(&post_tool_text)}]}));
    }
    if !steps.is_empty() {
        cpa_json::set(&mut out, "steps", Value::Array(steps));
    }
    if let Some(u) = &final_usage {
        set_usage(&mut out, "", u);
    }
    Ok(Consumed {
        interactions: out,
        usage: final_usage,
    })
}

/// Collects a whole byte slice as a single-chunk body (tests and small mock bodies).
pub fn single_chunk_body(
    bytes: Vec<u8>,
) -> impl Stream<Item = Result<Bytes, std::convert::Infallible>> + Unpin {
    futures_util::stream::iter(vec![Ok(Bytes::from(bytes))])
}

#[cfg(test)]
mod tests;
