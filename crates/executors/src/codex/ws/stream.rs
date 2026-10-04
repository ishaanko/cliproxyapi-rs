//! Streaming over the upstream websocket, with optional bootstrap buffering (Go:
//! codex_websockets_stream.go `ExecuteStream`).

use std::time::{Duration, Instant};

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ExecError, Options, Request, StreamResult};
use cpa_translator::Param;
use tokio::sync::{mpsc, oneshot};

use super::conn::Read;
use super::errors::{clear_replay_on_error_frame, encode_as_sse, map_read_error, parse_error_frame};
use super::{SESSION_READ_CLOSED, WS_EXECUTOR_TYPE, WsCall, WsPlan, connect_and_send, is_downstream_websocket, read_error_stage};
use crate::codex::CodexExecutor;
use crate::codex::multi_agent_v2::restore_response;
use crate::codex::reasoning::{cache_replay_from_completed, clear_replay_on_invalid_signature};
use crate::codex::request::Mode;
use crate::codex::terminal::{
    BOOTSTRAP_MAX_BUFFERED_BYTES, BOOTSTRAP_MAX_BUFFERED_FRAMES, OutputItems, has_meaningful_output_delta,
    is_bootstrap_bufferable_event, is_overload_bootstrap_failure, is_terminal_empty_incomplete, new_bootstrap_overload_err,
    new_empty_incomplete_stream_error, normalize_completion, patch_completed_output, status_error, terminal_failure_err,
};
use crate::helps::claude_input_tokens::ClaudeInputTokenState;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::ttft::observe_responses_token_event;
use crate::helps::usage::{accounting::Detail, parse::parse_codex_usage, reporter::UsageReporter};

const STREAM_CHANNEL_CAPACITY: usize = 64;

/// What one upstream websocket message turned into.
enum Step {
    Skip,
    /// `error` frame with a status.
    ErrorFrame { err: ExecError, frame: Value },
    /// `response.failed` or a status-less `error` event.
    Failure { err: ExecError, body: Vec<u8> },
    EmptyIncomplete(ExecError),
    Frame { chunks: Vec<Vec<u8>>, payload_len: usize, bufferable: bool, terminal_event: bool },
}

struct WsStream {
    plan: WsPlan,
    model: String,
    param: Param,
    claude_tokens: ClaudeInputTokenState,
    items: OutputItems,
    saw_output_delta: bool,
    downstream_ws: bool,
    usage: Option<Detail>,
    reporter: UsageReporter,
}

fn is_completion_type(event_type: &str) -> bool {
    matches!(event_type, "response.completed" | "response.done" | "response.incomplete")
}

impl WsStream {
    /// Handles one text message (Go: the body of the read loop after the transport checks).
    fn step(&mut self, payload: Vec<u8>, restore_multi_agent: bool, window_open: bool) -> Step {
        if payload.is_empty() {
            return Step::Skip;
        }
        observe_responses_token_event(&self.reporter, &payload);
        self.plan.log_frame(&payload);
        let payload = restore_response(&payload, restore_multi_agent);
        let frame = cpa_json::parse(&payload);
        let modelc = self.plan.model_level_cooling;
        if let Some(err) = parse_error_frame(&frame, modelc) {
            return Step::ErrorFrame { err, frame };
        }
        if let Some((err, body)) = terminal_failure_err(&frame, modelc) {
            return Step::Failure { err, body };
        }
        let event_type = frame.g("type").str();
        let terminal_event = is_completion_type(&event_type) || event_type == "response.failed" || event_type == "error";
        if has_meaningful_output_delta(&frame) {
            self.saw_output_delta = true;
        }
        if is_terminal_empty_incomplete(&frame, self.items.len(), self.saw_output_delta) {
            return Step::EmptyIncomplete(new_empty_incomplete_stream_error());
        }
        if event_type == "response.output_item.done" {
            self.items.collect(&frame, &payload);
        }
        let mut completed = payload.clone();
        if is_completion_type(&event_type) {
            completed = normalize_completion(&completed);
            if !self.plan.prepared.native {
                completed = patch_completed_output(&completed, &self.items);
            }
            if event_type != "response.incomplete" {
                cache_replay_from_completed(&self.plan.prepared.replay_scope, &cpa_json::parse(&completed));
            }
            self.usage = parse_codex_usage(&completed);
            match &self.usage {
                Some(detail) => self.reporter.publish(detail.clone()),
                None => self.reporter.ensure_published(),
            }
        }
        let completion = is_completion_type(&event_type);
        let (chunks, payload) = if self.downstream_ws {
            let out = if completion { completed } else { payload };
            (vec![ensure_responses_usage_details(&out)], out)
        } else {
            let mut out = normalize_completion(&payload);
            if completion {
                out = completed;
            }
            let line = encode_as_sse(&out);
            let chunks = self.claude_tokens.translate_stream(
                self.plan.prepared.to,
                self.plan.prepared.response_format,
                &self.model,
                &self.plan.prepared.original_payload,
                &self.plan.body,
                &line,
                &mut self.param,
            );
            (chunks, out)
        };
        let bufferable = window_open && !terminal_event && is_bootstrap_bufferable_event(&event_type, &payload, &frame);
        Step::Frame { chunks, payload_len: payload.len(), bufferable, terminal_event }
    }

    fn usage_value(&self) -> Option<Value> {
        self.usage.as_ref().map(UsageReporter::usage_metadata)
    }
}

impl CodexExecutor {
    /// Streaming request over the upstream websocket.
    pub(in crate::codex) async fn execute_stream_ws(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        let cfg = self.config();
        if opts.alt == "responses/compact" {
            return Err(status_error(400, "streaming not supported for /responses/compact"));
        }
        let reporter = self.reporter(WS_EXECUTOR_TYPE, auth, &req, &opts);
        let result = self.execute_stream_ws_reported(cfg, auth, req, opts, reporter.clone()).await;
        reporter.track_failure(&result);
        result
    }

    async fn execute_stream_ws_reported(
        &self,
        cfg: std::sync::Arc<cpa_config::Config>,
        auth: &Auth,
        req: Request,
        opts: Options,
        reporter: UsageReporter,
    ) -> Result<StreamResult, ExecError> {
        let plan = self.prepare_ws(&cfg, auth, &req, &opts, Mode::WsStream)?;
        reporter.set_translated_reasoning_effort(&plan.body, plan.prepared.to.as_str());
        let mut call = connect_and_send(&opts, &plan, true).await?;
        reporter.start_response_ttft();
        if let Some(input) = opts.ws_input.clone()
            && (cfg.codex.response_steering || cfg.codex_response_steering)
        {
            return Ok(self.stream_duplex(cfg, auth, req, opts, input, call, plan, reporter));
        }
        let headers = std::mem::take(&mut call.handshake_headers);

        let buffering = cfg.codex.stream_bootstrap_buffering;
        let bootstrap_timeout = Duration::from_nanos(cfg.codex.stream_bootstrap_timeout_duration().0.max(0) as u64);
        let claude_tokens = ClaudeInputTokenState::new(plan.prepared.from, plan.prepared.to, plan.prepared.response_format, &plan.prepared.original_payload);
        let mut stream = WsStream {
            plan,
            model: req.model.clone(),
            param: Param::default(),
            claude_tokens,
            items: OutputItems::default(),
            saw_output_delta: false,
            downstream_ws: is_downstream_websocket(&opts),
            usage: None,
            reporter,
        };
        let (usage_tx, usage_rx) = oneshot::channel::<Value>();

        let mut buffered: Vec<Vec<u8>> = Vec::new();
        let mut initial: Vec<Vec<u8>> = Vec::new();
        let mut bootstrap_terminal_err: Option<ExecError> = None;
        let mut immediate_terminal = false;
        if buffering {
            let start = Instant::now();
            let (mut frames_read, mut buffered_bytes) = (0usize, 0usize);
            loop {
                let read = call.next_read().await;
                let payload = match read {
                    Some(Read::Text(payload)) => payload,
                    Some(Read::Err(err)) => {
                        let mapped = map_read_error(&err);
                        call.invalidate_with("read_error", &mapped, true);
                        stream.plan.log_error(read_error_stage(&err), &mapped.message);
                        return Err(mapped);
                    }
                    None => {
                        stream.plan.log_error("read", SESSION_READ_CLOSED);
                        return Err(ExecError::new(0, SESSION_READ_CLOSED));
                    }
                };
                frames_read += 1;
                let time_reached = !bootstrap_timeout.is_zero() && start.elapsed() >= bootstrap_timeout;
                let window_open = frames_read <= BOOTSTRAP_MAX_BUFFERED_FRAMES && !time_reached;
                let restore = call.restore_multi_agent;
                match stream.step(payload, restore, window_open) {
                    Step::Skip => {
                        if !window_open {
                            break;
                        }
                    }
                    Step::ErrorFrame { err, frame } => {
                        call.invalidate_with("upstream_error", &err, true);
                        call.unlock();
                        if let Err(replay_err) = clear_replay_on_error_frame(&stream.plan.prepared.replay_scope, &frame) {
                            stream.plan.log_error("replay_clear_error", &replay_err.message);
                            return Err(replay_err);
                        }
                        stream.plan.log_error("upstream_error", &err.message);
                        if time_reached {
                            stream.reporter.publish_failure(&err);
                            bootstrap_terminal_err = Some(err);
                            break;
                        }
                        return Err(err);
                    }
                    Step::Failure { err, body } => {
                        let mut failover = is_overload_bootstrap_failure(&body);
                        if failover && time_reached {
                            failover = false;
                        }
                        call.unlock();
                        call.invalidate_with("terminal_failure", &err, !failover);
                        if let Err(replay_err) = clear_replay_on_invalid_signature(&stream.plan.prepared.replay_scope, err.status, &body) {
                            stream.plan.log_error("replay_clear_error", &replay_err.message);
                            return Err(replay_err);
                        }
                        stream.plan.log_error("upstream_error", &err.message);
                        if failover {
                            call.set_close_reason("bootstrap_overload");
                            return Err(new_bootstrap_overload_err(&body));
                        }
                        stream.reporter.publish_failure(&err);
                        bootstrap_terminal_err = Some(err);
                        break;
                    }
                    Step::EmptyIncomplete(err) => {
                        stream.plan.log_error("upstream_error", &err.message);
                        call.invalidate_with("terminal_empty_incomplete", &err, true);
                        call.unlock();
                        stream.reporter.publish_failure(&err);
                        bootstrap_terminal_err = Some(err);
                        break;
                    }
                    Step::Frame { chunks, payload_len, bufferable, terminal_event } => {
                        if bufferable {
                            let frame_bytes = payload_len + chunks.iter().map(Vec::len).sum::<usize>();
                            if buffered_bytes + frame_bytes <= BOOTSTRAP_MAX_BUFFERED_BYTES {
                                buffered_bytes += frame_bytes;
                                buffered.extend(chunks);
                                continue;
                            }
                        }
                        initial = chunks;
                        immediate_terminal = terminal_event;
                        break;
                    }
                }
            }
        }

        let capacity = (buffered.len() + initial.len() + 1).max(STREAM_CHANNEL_CAPACITY);
        let (tx, rx) = mpsc::channel(capacity);
        for chunk in buffered.into_iter().chain(initial) {
            let _ = tx.try_send(Ok(Bytes::from(chunk)));
        }
        let mut result = StreamResult::new(headers, rx);
        result.usage = Some(usage_rx);
        if let Some(err) = bootstrap_terminal_err {
            call.set_close_reason("bootstrap_terminal_error");
            let _ = tx.try_send(Err(err));
            return Ok(result);
        }
        if immediate_terminal {
            if let Some(meta) = stream.usage_value() {
                let _ = usage_tx.send(meta);
            }
            return Ok(result);
        }
        tokio::spawn(async move {
            stream.run(call, tx, usage_tx).await;
        });
        Ok(result)
    }
}

impl WsStream {
    /// Streams the rest of the response after the bootstrap decision.
    async fn run(mut self, mut call: WsCall, tx: mpsc::Sender<Result<Bytes, ExecError>>, usage_tx: oneshot::Sender<Value>) {
        loop {
            let read = tokio::select! {
                _ = tx.closed() => {
                    call.set_close_reason("context_done");
                    // Go sends ctx.Err() and returns without recording a failure.
                    self.reporter.abandon();
                    return;
                }
                read = call.next_read() => read,
            };
            let payload = match read {
                Some(Read::Text(payload)) => payload,
                Some(Read::Err(err)) => {
                    call.set_close_reason("read_error");
                    let mapped = map_read_error(&err);
                    self.plan.log_error(read_error_stage(&err), &mapped.message);
                    self.reporter.publish_failure(&mapped);
                    let _ = tx.send(Err(mapped)).await;
                    return;
                }
                None => {
                    call.set_close_reason("read_error");
                    self.plan.log_error("read", SESSION_READ_CLOSED);
                    let closed = ExecError::new(0, SESSION_READ_CLOSED);
                    self.reporter.publish_failure(&closed);
                    let _ = tx.send(Err(closed)).await;
                    return;
                }
            };
            let restore = call.restore_multi_agent;
            match self.step(payload, restore, false) {
                Step::Skip => {}
                Step::ErrorFrame { err, frame } => {
                    call.set_close_reason("upstream_error");
                    call.invalidate_with("upstream_error", &err, true);
                    let err = match clear_replay_on_error_frame(&self.plan.prepared.replay_scope, &frame) {
                        Err(replay_err) => {
                            self.plan.log_error("replay_clear_error", &replay_err.message);
                            replay_err
                        }
                        Ok(()) => {
                            self.plan.log_error("upstream_error", &err.message);
                            err
                        }
                    };
                    self.reporter.publish_failure(&err);
                    let _ = tx.send(Err(err)).await;
                    return;
                }
                Step::Failure { err, body } => {
                    call.set_close_reason("upstream_error");
                    call.unlock();
                    call.invalidate_with("terminal_failure", &err, true);
                    let err = match clear_replay_on_invalid_signature(&self.plan.prepared.replay_scope, err.status, &body) {
                        Err(replay_err) => {
                            self.plan.log_error("replay_clear_error", &replay_err.message);
                            replay_err
                        }
                        Ok(()) => {
                            self.plan.log_error("upstream_error", &err.message);
                            err
                        }
                    };
                    self.reporter.publish_failure(&err);
                    let _ = tx.send(Err(err)).await;
                    return;
                }
                Step::EmptyIncomplete(err) => {
                    self.plan.api_log.record_api_response_error(&self.plan.cfg, &err.message);
                    call.invalidate_with("terminal_empty_incomplete", &err, true);
                    call.unlock();
                    call.set_close_reason("terminal_empty_incomplete");
                    self.reporter.publish_failure(&err);
                    let _ = tx.send(Err(err)).await;
                    return;
                }
                Step::Frame { chunks, terminal_event, .. } => {
                    for chunk in chunks {
                        if tx.send(Ok(Bytes::from(chunk))).await.is_err() {
                            call.set_close_reason("context_done");
                            return;
                        }
                    }
                    if terminal_event {
                        if let Some(meta) = self.usage_value() {
                            let _ = usage_tx.send(meta);
                        }
                        return;
                    }
                }
            }
        }
    }
}
