//! The read loop of an xAI websocket request (Go: the stream goroutine of
//! `XAIWebsocketsExecutor.ExecuteStream`).
//!
//! Each upstream text frame is checked for error frames, normalized like the HTTP stream
//! (reasoning summaries, namespaced and aliased tools, X Search filtering, apply_patch bridge),
//! recorded in the id state transcript, and delivered either as the raw event (client on a
//! Responses websocket) or translated to the client's format.

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use cpa_config::Config;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ExecError, StreamResult};
use cpa_translator::Param;
use http::HeaderMap;
use tokio::sync::{mpsc, oneshot};
use tokio::sync::OwnedMutexGuard;

use super::conn::Read;
use super::errors::{map_read_error, parse_error_frame};
use super::ids::RequestIdMapper;
use super::WsCall;
use crate::helps::apply_patch::{
    APPLY_PATCH_UPSTREAM_ERROR_MESSAGE, gateway_error, record_apply_patch_stream_failure, stop_apply_patch_stream,
};
use crate::helps::claude_input_tokens::ClaudeInputTokenState;
use crate::helps::logging::ApiLogHandle;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::usage::{StreamUsageBuffer, UsageReporter, parse_codex_usage};
use crate::xai::replay::cache_reasoning_replay_from_completed;
use crate::xai::request::PreparedRequest;
use crate::xai::response::{
    InternalXSearchResponseFilter, NamespaceRestorer, collect_output_item_done, normalize_reasoning_summary_data,
    normalize_reasoning_summary_data_events, patch_completed_output, restore_client_web_search_name,
};
use crate::xai::stream::Event;
use crate::xai::util::s;

const STREAM_CHANNEL_CAPACITY: usize = 16;
const DEFAULT_USAGE: &str =
    r#"{"input_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens":0,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":0}"#;

/// Everything the stream task owns.
pub(super) struct Start {
    pub cfg: Arc<Config>,
    pub api_log: ApiLogHandle,
    /// Plugin observer of the upstream frames.
    pub observer: Option<crate::helps::websocket_observer::WsFrameObserver>,
    pub downstream_ws: bool,
    pub req_model: String,
    pub prepared: PreparedRequest,
    pub call: WsCall,
    /// The `response.create` frame that was sent.
    pub frame: Vec<u8>,
    pub mapper: Option<RequestIdMapper>,
    pub warmup: bool,
    pub transcript_reset: bool,
    pub execution_session: String,
    pub ws_url: String,
    pub auth_id: String,
    pub reporter: UsageReporter,
    /// Serializes sessionless requests per id state until the stream ends.
    pub state_guard: Option<OwnedMutexGuard<()>>,
    pub headers: HeaderMap,
}

enum Flow {
    Continue,
    Stop,
}

struct WsStream {
    observer: Option<crate::helps::websocket_observer::WsFrameObserver>,
    cfg: Arc<Config>,
    api_log: ApiLogHandle,
    downstream_ws: bool,
    req_model: String,
    prepared: PreparedRequest,
    call: WsCall,
    frame: Vec<u8>,
    mapper: Option<RequestIdMapper>,
    warmup: bool,
    transcript_reset: bool,
    execution_session: String,
    ws_url: String,
    auth_id: String,
    reporter: UsageReporter,
    tx: mpsc::Sender<Result<Bytes, ExecError>>,
    usage: StreamUsageBuffer,
    param: Param,
    claude: ClaudeInputTokenState,
    items_by_index: BTreeMap<i64, Value>,
    items_fallback: Vec<Value>,
    filter: InternalXSearchResponseFilter,
    restorer: NamespaceRestorer,
    recorded_transcript: bool,
}

/// Spawns the read loop and returns the client-facing stream.
pub(super) fn spawn(start: Start) -> StreamResult {
    let (tx, rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
    let (usage_tx, usage_rx) = oneshot::channel();
    let headers = start.headers.clone();
    let claude = ClaudeInputTokenState::new(
        start.prepared.from,
        start.prepared.to,
        start.prepared.response_format,
        &start.prepared.original_payload,
    );
    let filter = InternalXSearchResponseFilter::new(
        start.prepared.filter_internal_x_search,
        start.prepared.client_declared_tools.clone(),
    );
    let restorer = NamespaceRestorer::new(start.prepared.namespace_tools.clone());
    let state_guard = start.state_guard;
    let mut stream = WsStream {
        cfg: start.cfg,
        api_log: start.api_log,
        observer: start.observer,
        downstream_ws: start.downstream_ws,
        req_model: start.req_model,
        prepared: start.prepared,
        call: start.call,
        frame: start.frame,
        mapper: start.mapper,
        warmup: start.warmup,
        transcript_reset: start.transcript_reset,
        execution_session: start.execution_session,
        ws_url: start.ws_url,
        auth_id: start.auth_id,
        reporter: start.reporter,
        tx,
        usage: StreamUsageBuffer::default(),
        param: Param::default(),
        claude,
        items_by_index: BTreeMap::new(),
        items_fallback: Vec::new(),
        filter,
        restorer,
        recorded_transcript: false,
    };
    tokio::spawn(async move {
        stream.run_loop().await;
        stream.reporter.publish_buffer(&stream.usage);
        if let Some(detail) = stream.usage.detail() {
            let _ = usage_tx.send(UsageReporter::usage_metadata(&detail));
        }
        // The session connection is released (and an ephemeral one closed) before the id state
        // lock, like Go's defer order.
        drop(stream);
        drop(state_guard);
    });
    let mut result = StreamResult::new(headers, rx);
    result.usage = Some(usage_rx);
    result
}

impl WsStream {
    async fn send(&self, chunk: Vec<u8>) -> bool {
        self.tx.send(Ok(Bytes::from(chunk))).await.is_ok()
    }

    async fn send_err(&self, err: ExecError) {
        let _ = self.tx.send(Err(err)).await;
    }

    /// Go: invalidatePatchAttempt. A validation failure ends the upstream attempt before any
    /// downstream delivery or session reuse.
    fn invalidate_patch_attempt(&mut self) {
        let err = gateway_error();
        self.call.set_close_reason("invalid_tool_arguments");
        self.call.invalidate("invalid_tool_arguments", Some(APPLY_PATCH_UPSTREAM_ERROR_MESSAGE), false);
        self.reporter.publish_failure(&err);
    }

    /// An apply_patch bridge failure event as the client's transport carries it.
    async fn send_bridge_events(&self, events: Vec<Vec<u8>>) {
        for event in events {
            if self.downstream_ws {
                let _ = self.send(event).await;
            } else {
                let _ = self.send(sse_line(&event)).await;
            }
        }
    }

    async fn run_loop(&mut self) {
        loop {
            let read = tokio::select! {
                _ = self.tx.closed() => {
                    self.call.set_close_reason("context_done");
                    return;
                }
                read = self.call.next_read() => read,
            };
            let payload = match read {
                Some(Read::Text(payload)) => payload,
                Some(Read::Err(err)) => return self.handle_read_error(map_read_error(&err)).await,
                None => {
                    let err = ExecError::new(0, "xai websockets executor: session read channel closed");
                    return self.handle_read_error(err).await;
                }
            };
            if payload.is_empty() {
                continue;
            }
            if let Flow::Stop = self.handle_payload(payload).await {
                return;
            }
        }
    }

    /// The upstream ended or failed mid-request.
    async fn handle_read_error(&mut self, mapped: ExecError) {
        if let Err(err_finish) = self.prepared.apply_patch.finish() {
            let (events, _) = self.prepared.apply_patch.bridge.fail(err_finish);
            self.invalidate_patch_attempt();
            self.send_bridge_events(events).await;
            self.send_err(gateway_error()).await;
            return;
        }
        self.call.set_close_reason("read_error");
        self.api_log.record_api_websocket_error(&self.cfg, "read", &mapped.message);
        self.reporter.publish_failure(&mapped);
        self.send_err(mapped).await;
    }

    /// Handles one upstream text frame (Go: the body of the read loop).
    async fn handle_payload(&mut self, payload: Vec<u8>) -> Flow {
        self.reporter.mark_first_response_byte();
        self.api_log.append_api_websocket_response(&self.cfg, &payload);
        if let Some(observer) = &self.observer {
            observer.emit(&payload);
        }

        let valid = cpa_json::valid(&payload);
        let frame = cpa_json::parse(&payload);
        if let Some(ws_err) = parse_error_frame(&payload, &frame) {
            self.call.set_close_reason("upstream_error");
            self.api_log.record_api_websocket_error(&self.cfg, "upstream_error", &ws_err.message);
            self.reporter.publish_failure(&ws_err);
            self.call.invalidate("upstream_error", Some(&ws_err.message), false);
            self.send_err(ws_err).await;
            return Flow::Stop;
        }

        let list: Vec<Event> = if valid {
            normalize_reasoning_summary_data_events(frame).into_iter().map(Event::Json).collect()
        } else {
            vec![Event::Raw(payload)]
        };
        for mut event in list {
            self.prepared.apply_patch.remember_dispatcher_event(&event.bytes());
            if let Event::Json(v) = &mut event {
                self.restorer.restore(v);
                if !self.prepared.web_search_alias.is_empty() {
                    restore_client_web_search_name(v, &self.prepared.web_search_alias);
                }
                if !self.filter.apply(v) {
                    continue;
                }
            }
            let bytes = event.bytes();
            if bytes.is_empty() {
                continue;
            }
            let (events, err_bridge) = self.prepared.apply_patch.transform(&bytes);
            if err_bridge.is_some() {
                self.invalidate_patch_attempt();
                self.send_bridge_events(events).await;
                let err = gateway_error();
                self.reporter.publish_failure(&err);
                self.send_err(err).await;
                return Flow::Stop;
            }
            for event in events {
                if let Flow::Stop = self.handle_event(event).await {
                    return Flow::Stop;
                }
            }
        }
        Flow::Continue
    }

    /// Records the turn in the id state transcript once per request.
    fn record_turn(&mut self, completed: &[u8]) {
        if let Some(mapper) = &self.mapper
            && !self.recorded_transcript
        {
            mapper.state.record_transcript_turn(&self.frame, completed, self.transcript_reset);
            self.recorded_transcript = true;
        }
    }

    /// One event after the bridge (Go: the inner loop over `events`).
    async fn handle_event(&mut self, mut payload: Vec<u8>) -> Flow {
        let parsed = cpa_json::parse(&payload);
        let event_type = s(&parsed, "type");
        let patch_terminal = self.prepared.apply_patch.active() && (event_type == "response.incomplete" || event_type == "response.failed");
        let terminal_event = matches!(event_type.as_str(), "response.completed" | "response.done" | "error") || patch_terminal;
        self.reporter.observe_response_model(&payload);
        let mut warmup_completed: Vec<u8> = Vec::new();
        match event_type.as_str() {
            "response.created" => {
                if self.warmup {
                    warmup_completed = build_warmup_completed_payload(&parsed);
                    self.record_turn(&warmup_completed);
                    tracing::info!(
                        "xai websockets: upstream warmup completed session={} auth={} url={} response_id={}",
                        self.execution_session.trim(),
                        self.auth_id.trim(),
                        self.ws_url.trim(),
                        s(&parsed, "response.id").trim()
                    );
                }
            }
            "response.output_item.done" => {
                collect_output_item_done(&parsed, &mut self.items_by_index, &mut self.items_fallback);
            }
            "response.completed" => {
                self.log_terminal(&event_type, &parsed);
                if let Some(detail) = parse_codex_usage(&payload) {
                    self.usage.observe(detail, true);
                }
                let mut completed = patch_completed_output(&parsed, &self.items_by_index, &self.items_fallback);
                normalize_reasoning_summary_data(&mut completed);
                cache_reasoning_replay_from_completed(&self.prepared.replay_scope, &completed);
                payload = cpa_json::to_vec(&completed);
                if !self.warmup {
                    self.record_turn(&payload);
                }
            }
            "response.done" => {
                self.log_terminal(&event_type, &parsed);
                if let Some(detail) = parse_codex_usage(&payload) {
                    self.usage.observe(detail, true);
                }
                if !self.warmup {
                    self.record_turn(&payload);
                }
            }
            _ => {}
        }

        if self.downstream_ws {
            let mut downstream = ensure_responses_usage_details(&payload);
            let mut downstream_warmup = ensure_responses_usage_details(&warmup_completed);
            if let Some(mapper) = self.mapper.as_mut() {
                downstream = mapper.downstream_response_payload(downstream);
                if !warmup_completed.is_empty() {
                    downstream_warmup = mapper.downstream_response_payload(downstream_warmup);
                }
            }
            if !self.send(downstream).await {
                self.call.set_close_reason("context_done");
                return Flow::Stop;
            }
            if !downstream_warmup.is_empty() {
                if !self.send(downstream_warmup).await {
                    self.call.set_close_reason("context_done");
                }
                return Flow::Stop;
            }
            return if terminal_event { Flow::Stop } else { Flow::Continue };
        }

        // Go: normalizeCodexWebsocketCompletion, then the SSE translation path.
        if s(&cpa_json::parse(&payload), "type").trim() == "response.done" {
            let mut value = cpa_json::parse(&payload);
            if cpa_json::set(&mut value, "type", "response.completed") {
                payload = cpa_json::to_vec(&value);
            }
        }
        if let Flow::Stop = self.deliver_translated(&payload).await {
            return Flow::Stop;
        }
        if !warmup_completed.is_empty() {
            if let Flow::Continue = self.deliver_translated(&warmup_completed).await {
                stop_apply_patch_stream(&self.param, &self.reporter, &self.tx, gateway_error()).await;
            }
            return Flow::Stop;
        }
        if matches!(event_type.as_str(), "response.completed" | "response.done") || patch_terminal {
            return Flow::Stop;
        }
        Flow::Continue
    }

    /// Translates one event to the client's format and delivers it; `Stop` when delivery ended
    /// the stream (client gone or an apply_patch failure).
    async fn deliver_translated(&mut self, payload: &[u8]) -> Flow {
        let line = sse_line(payload);
        let chunks = self.claude.translate_stream(
            self.prepared.to,
            self.prepared.response_format,
            &self.req_model,
            &self.prepared.original_payload,
            &self.prepared.body,
            &line,
            &mut self.param,
        );
        if record_apply_patch_stream_failure(&self.param, &self.reporter, &gateway_error()) {
            self.invalidate_patch_attempt();
        }
        for chunk in chunks {
            if !self.send(chunk).await {
                self.call.set_close_reason("context_done");
                return Flow::Stop;
            }
        }
        if stop_apply_patch_stream(&self.param, &self.reporter, &self.tx, gateway_error()).await {
            self.call.set_close_reason("invalid_tool_arguments");
            return Flow::Stop;
        }
        Flow::Continue
    }

    fn log_terminal(&self, event_type: &str, parsed: &Value) {
        tracing::info!(
            "xai websockets: upstream terminal response session={} auth={} url={} event={} response_id={} previous_response_id={}",
            self.execution_session.trim(),
            self.auth_id.trim(),
            self.ws_url.trim(),
            event_type.trim(),
            s(parsed, "response.id").trim(),
            s(parsed, "response.previous_response_id").trim()
        );
    }
}

/// Go: encodeCodexWebsocketAsSSE. A frame as one SSE `data:` line for the translators.
fn sse_line(payload: &[u8]) -> Vec<u8> {
    if payload.is_empty() {
        return Vec::new();
    }
    let mut line = Vec::with_capacity(payload.len() + 6);
    line.extend_from_slice(b"data: ");
    line.extend_from_slice(payload);
    line
}

/// Go: buildXAIWebsocketWarmupCompletedPayload. The synthetic `response.completed` that ends a
/// `generate:false` warmup right after its `response.created`.
pub(super) fn build_warmup_completed_payload(created: &Value) -> Vec<u8> {
    let mut completed = cpa_json::parse_str(&format!(
        r#"{{"type":"response.completed","response":{{"output":[],"usage":{DEFAULT_USAGE}}}}}"#
    ));
    let sequence = created.g("sequence_number");
    if sequence.exists() {
        cpa_json::set(&mut completed, "sequence_number", sequence.int() + 1);
    }
    let response = created.g("response");
    if response.exists() && response.is_object() {
        let mut payload = response.value();
        cpa_json::set(&mut payload, "status", "completed");
        if !payload.g("output").exists() {
            cpa_json::set(&mut payload, "output", Value::Array(Vec::new()));
        }
        if !payload.g("usage").exists() {
            cpa_json::set(&mut payload, "usage", cpa_json::parse_str(DEFAULT_USAGE));
        }
        cpa_json::set(&mut completed, "response", payload);
    }
    ensure_responses_usage_details(&cpa_json::to_vec(&completed))
}
