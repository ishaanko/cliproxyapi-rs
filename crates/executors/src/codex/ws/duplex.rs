//! Duplex streaming with response steering (Go: codex_websockets_duplex.go `streamCodexDuplex`).
//!
//! Once the first `response.create` is written, this module owns the already authenticated
//! upstream socket until the downstream disconnects: a response terminal event does not end the
//! stream, because accepted steering may produce an automatic successor or wait for client tool
//! results. A writer task forwards downstream frames (`response.steer`, `response.create`,
//! `response.append`), a reader task relays upstream events. There is no redial, credential
//! selection, local acknowledgement or replay: only the upstream can take ownership of a steering
//! submission, and connection failures never cool the shared credential.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ErrorCode, ExecError, Options, Request, StreamResult, WebsocketInput};
use parking_lot::Mutex;
use tokio::sync::{Notify, mpsc, oneshot, watch};

use super::conn::Read;
use super::errors::{clear_replay_on_error_frame, map_read_error, map_write_error, parse_error_frame};
use super::session::Session;
use super::{SESSION_READ_CLOSED, WS_EXECUTOR_TYPE, WsCall, WsPlan, build_request_frame};
use crate::codex::CodexExecutor;
use crate::codex::multi_agent_v2::restore_response;
use crate::codex::reasoning::{ReplayScope, cache_replay_from_completed, clear_replay_on_invalid_signature};
use crate::codex::request::Mode;
use crate::codex::terminal::{OutputItems, normalize_completion, patch_completed_output, terminal_failure_err};
use crate::codex::upstream_websocket_replay_required;
use crate::helps::logging::UpstreamRequestLog;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::ttft::observe_responses_token_event;
use crate::helps::usage::{parse::parse_codex_usage, reporter::UsageReporter};
use cpa_translator::Format;

/// Outstanding creates and queued creates are capped (Go: 16).
const MAX_OUTSTANDING: usize = 16;
/// Response settings retained for automatic successors and steering (Go: 16).
const MAX_RETAINED_RESPONSES: usize = 16;

/// What the duplex stream needs to remember about one `response.create`.
#[derive(Clone)]
struct Settings {
    replay_scope: ReplayScope,
    native: bool,
    optimize_multi_agent_v2: bool,
    multi_agent_v2_conflict: bool,
    ws_url: String,
    /// The translated request body (Go: clientBody); snapshots keep only reasoning and
    /// instructions.
    client_body: Vec<u8>,
    original_payload: Bytes,
}

impl Settings {
    fn from_plan(plan: &WsPlan) -> Self {
        Settings {
            replay_scope: plan.prepared.replay_scope.clone(),
            native: plan.prepared.native,
            optimize_multi_agent_v2: plan.prepared.optimize_multi_agent_v2,
            multi_agent_v2_conflict: plan.prepared.multi_agent_v2_conflict,
            ws_url: plan.ws_url.clone(),
            client_body: plan.body.clone(),
            original_payload: plan.prepared.original_payload.clone(),
        }
    }

    /// Response settings without request history or authorization headers: the later responses
    /// of a chain inherit these, as they do upstream.
    fn snapshot(&self) -> Self {
        let mut body = cpa_json::parse_str("{}");
        let source = cpa_json::parse(&self.client_body);
        for key in ["reasoning", "instructions"] {
            let node = source.g(key);
            if node.exists() {
                cpa_json::set(&mut body, key, node.value());
            }
        }
        Settings { client_body: cpa_json::to_vec(&body), original_payload: Bytes::new(), ..self.clone() }
    }

    /// The `instructions` an explicit settings holder carries.
    fn instructions(&self) -> Option<Value> {
        let body = cpa_json::parse(&self.client_body);
        if body.g("instructions").exists() {
            return Some(body.g("instructions").value());
        }
        let original = cpa_json::parse(&self.original_payload);
        original.g("instructions").exists().then(|| original.g("instructions").value())
    }
}

/// Bookkeeping shared by the writer and reader tasks (Go: the variables guarded by
/// `metadataMu`).
struct State {
    /// Creates written upstream whose `response.created` has not arrived yet.
    pending: VecDeque<Arc<Settings>>,
    /// Parents of steering submissions the upstream has not acknowledged yet.
    unacknowledged_steers: Vec<String>,
    /// Accepted steering ids mapped to their parent response.
    accepted_steers: HashMap<String, String>,
    current: Arc<Settings>,
    response_id: String,
    response_settings: HashMap<String, Arc<Settings>>,
    steering_settings: HashMap<String, Arc<Settings>>,
    response_order: Vec<String>,
    waiting_parent: String,
    automatic_active: bool,
}

impl State {
    /// Go: releaseSteeringSettings. Settings pinned for in-flight steering stay until it settles.
    fn release_steering_settings(&mut self, parent: &str) {
        if self.unacknowledged_steers.iter().any(|t| t == parent) || self.accepted_steers.values().any(|t| t == parent) {
            return;
        }
        self.steering_settings.remove(parent);
    }

    /// Go: readyForCreate. An explicit create waits for steering to settle.
    fn ready_for_create(&self) -> bool {
        if !self.unacknowledged_steers.is_empty() || self.automatic_active {
            return false;
        }
        self.accepted_steers.values().all(|parent| *parent == self.waiting_parent)
    }
}

struct Shared {
    state: Mutex<State>,
    /// Go: stateChanged. A stored wake-up for the writer.
    changed: Notify,
    cancel: watch::Sender<bool>,
    /// The first failure the writer hit, preferred by the reader as the connection error.
    write_error: Mutex<Option<ExecError>>,
    /// The downstream input ended: the client went away (Go: the request context is done), so
    /// the stream closes without an error or a failed usage record.
    client_gone: std::sync::atomic::AtomicBool,
}

impl Shared {
    fn client_gone(&self) {
        self.client_gone.store(true, std::sync::atomic::Ordering::Release);
        self.cancel.send_replace(true);
    }

    fn is_client_gone(&self) -> bool {
        self.client_gone.load(std::sync::atomic::Ordering::Acquire)
    }

    fn fail(&self, err: ExecError) {
        self.write_error.lock().get_or_insert(err);
        self.cancel.send_replace(true);
    }

    fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    /// Go: waitFor. Waits until `ready(state)`; false when the stream was cancelled first.
    async fn wait_for(&self, ready: impl Fn(&State) -> bool) -> bool {
        let mut cancel = self.cancel.subscribe();
        loop {
            if ready(&self.state.lock()) {
                return !self.is_cancelled();
            }
            tokio::select! {
                _ = self.changed.notified() => {}
                () = wait_cancelled(&mut cancel) => return false,
            }
        }
    }
}

async fn wait_cancelled(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|cancelled| *cancelled).await;
}

/// Request-scoped wrapper keeping a connection failure from cooling the shared credential (Go:
/// `codexDuplexConnectionError`). Auth and quota failures of the initial handshake keep the normal
/// policy; this wrapper applies only after the stream started.
fn connection_error(cause: ExecError) -> ExecError {
    cause.with_code(ErrorCode::RequestScoped)
}

/// Makes the reporter of each response of the socket (Go: `NewExecutorUsageReporter(ctx, e, req.Model, auth)`
/// per `response.created` after the first).
struct ReporterFactory {
    auth: Auth,
    model: String,
    opts: Options,
}

impl ReporterFactory {
    /// A reporter for a response created with `settings`, its reasoning effort read from the
    /// translated request body.
    fn reporter(&self, settings: &Settings, to: Format) -> UsageReporter {
        let reporter = UsageReporter::new("codex", WS_EXECUTOR_TYPE, &self.model, Some(&self.auth), Some(&self.opts));
        reporter.set_translated_reasoning_effort(&settings.client_body, to.as_str());
        reporter
    }
}

/// A locally generated rejection frame: `{"error":{"message":..,"type":"invalid_request_error"},"status":400,"type":"error"}`
/// (keys sorted, as Go marshals the map).
fn rejection_payload(message: &str) -> Vec<u8> {
    format!(
        r#"{{"error":{{"message":{},"type":"invalid_request_error"}},"status":400,"type":"error"}}"#,
        cpa_core::util::go_json_string(message)
    )
    .into_bytes()
}

/// `RecordAPIWebsocketRequest` of a forwarded frame (url, method and body only).
fn log_forwarded(plan: &WsPlan, frame: &[u8]) {
    let entry = UpstreamRequestLog {
        url: plan.ws_url.clone(),
        method: "WEBSOCKET".to_string(),
        body: frame.to_vec(),
        provider: "codex".to_string(),
        auth_id: plan.auth_id.clone(),
        ..Default::default()
    };
    plan.api_log.record_api_websocket_request(&plan.cfg, &entry);
}

/// Everything the writer task needs.
struct Writer {
    exec: CodexExecutor,
    cfg: Arc<Config>,
    auth: Auth,
    req: Request,
    opts: Options,
    input: WebsocketInput,
    initial: Arc<Settings>,
    plan: Arc<WsPlan>,
    sess: Arc<Session>,
    conn: Arc<super::conn::WsConn>,
    shared: Arc<Shared>,
    out: mpsc::Sender<Result<Bytes, ExecError>>,
    /// Creates held back until steering settles.
    pending_creates: VecDeque<Vec<u8>>,
}

impl Writer {
    /// Go: reject. Sends a local error frame downstream; false when the stream was cancelled.
    async fn reject(&self, message: &str) -> bool {
        let mut cancel = self.shared.cancel.subscribe();
        tokio::select! {
            sent = self.out.send(Ok(Bytes::from(rejection_payload(message)))) => sent.is_ok(),
            () = wait_cancelled(&mut cancel) => false,
        }
    }

    fn auth_enabled(&self) -> bool {
        self.opts.websocket_auth_enabled(&self.auth.id)
    }

    /// Writes one frame upstream, mapping failures like Go's `mapCodexWebsocketWriteError`.
    async fn write_frame(&self, frame: Vec<u8>) -> Result<(), ExecError> {
        self.conn.write_text(frame).await.map_err(|text| map_write_error(text, self.sess.upstream_close(&self.conn)))
    }

    /// Go: processCreatePayload. False ends the writer (the failure was already reported).
    async fn process_create(&self, mut payload: Vec<u8>) -> bool {
        let root = cpa_json::parse(&payload);
        let is_append = root.g("type").str() == "response.append";
        let mut prev_id = root.g("previous_response_id").str().trim().to_string();
        let wrong_parent = {
            let st = self.shared.state.lock();
            if is_append && prev_id.is_empty() && !st.response_id.is_empty() {
                prev_id = st.response_id.clone();
                let mut v = cpa_json::parse(&payload);
                cpa_json::set(&mut v, "previous_response_id", prev_id.as_str());
                payload = cpa_json::to_vec(&v);
            }
            !st.accepted_steers.is_empty() && prev_id != st.waiting_parent
        };
        if wrong_parent {
            return self.reject("response.create must continue the response waiting for required input").await;
        }
        let model = cpa_json::parse(&payload).g("model").str().trim().to_string();
        let initial_model = cpa_json::parse(&self.initial.original_payload).g("model").str();
        if !model.is_empty() && model != self.req.model && model != initial_model {
            self.shared.fail(upstream_websocket_replay_required());
            return false;
        }
        if model.is_empty() {
            let mut model = self.req.model.clone();
            if model.is_empty() {
                model = initial_model.trim().to_string();
            }
            let mut v = cpa_json::parse(&payload);
            cpa_json::set(&mut v, "model", model);
            payload = cpa_json::to_vec(&v);
        }
        if is_append && !cpa_json::parse(&payload).g("instructions").exists() {
            let parent_settings = self.shared.state.lock().response_settings.get(&prev_id).cloned();
            let target = parent_settings.unwrap_or_else(|| Arc::clone(&self.initial));
            let instructions = target.instructions().or_else(|| self.initial.instructions());
            if let Some(instructions) = instructions {
                let mut v = cpa_json::parse(&payload);
                cpa_json::set(&mut v, "instructions", instructions);
                payload = cpa_json::to_vec(&v);
            }
        }
        let mut next_req = self.req.clone();
        next_req.payload = Bytes::from(payload.clone());
        let mut next_opts = self.opts.clone();
        next_opts.original_request = Bytes::from(payload);
        let prepared = match self.exec.prepare_ws(&self.cfg, &self.auth, &next_req, &next_opts, Mode::WsStream) {
            Ok(p) => p,
            Err(err) => {
                self.shared.fail(err);
                return false;
            }
        };
        if prepared.ws_url != self.initial.ws_url {
            self.shared.fail(upstream_websocket_replay_required());
            return false;
        }
        let frame = build_request_frame(&prepared.body);
        {
            let mut st = self.shared.state.lock();
            if st.pending.len() >= MAX_OUTSTANDING {
                drop(st);
                self.shared.fail(ExecError::new(0, "too many outstanding response.create requests"));
                return false;
            }
            st.pending.push_back(Arc::new(Settings::from_plan(&prepared)));
        }
        if prepared.prepared.optimize_multi_agent_v2 || prepared.prepared.multi_agent_v2_conflict {
            self.sess.set_multi_agent_v2_optimized(
                &self.conn,
                prepared.prepared.optimize_multi_agent_v2 && !prepared.prepared.multi_agent_v2_conflict,
            );
        }
        if !self.auth_enabled() {
            self.shared.fail(ExecError::new(0, "websocket credential is no longer enabled"));
            return false;
        }
        log_forwarded(&self.plan, &frame);
        if let Err(err) = self.write_frame(frame.clone()).await {
            self.shared.fail(err);
            return false;
        }
        tracing::info!(
            "codex websockets: request forwarded session={} auth={} url={} event={}",
            self.sess.id,
            self.auth.id,
            self.plan.ws_url,
            cpa_json::parse(&frame).g("type").str()
        );
        true
    }

    /// Go: flushPendingCreates.
    async fn flush_pending_creates(&mut self) -> bool {
        while !self.pending_creates.is_empty() && self.shared.state.lock().ready_for_create() {
            let Some(item) = self.pending_creates.pop_front() else { break };
            if !self.process_create(item).await {
                return false;
            }
        }
        true
    }

    /// Go: the `response.steer` case. Control frames bypass every create translation and default;
    /// unknown fields and unsupported input are left to upstream validation.
    async fn forward_steer(&self, payload: Vec<u8>) -> bool {
        let parent = cpa_json::parse(&payload).g("previous_response_id").str();
        let mut settings = self.shared.state.lock().response_settings.get(&parent).cloned();
        if settings.is_none() {
            if !self.shared.wait_for(|st| st.pending.is_empty()).await {
                return false;
            }
            settings = self.shared.state.lock().response_settings.get(&parent).cloned();
        }
        {
            let mut st = self.shared.state.lock();
            st.unacknowledged_steers.push(parent.clone());
            if let Some(settings) = settings {
                st.steering_settings.insert(parent, settings);
            }
        }
        if !self.auth_enabled() {
            self.shared.fail(ExecError::new(0, "websocket credential is no longer enabled"));
            return false;
        }
        log_forwarded(&self.plan, &payload);
        if let Err(err) = self.write_frame(payload).await {
            self.shared.fail(err);
            return false;
        }
        tracing::info!(
            "codex websockets: request forwarded session={} auth={} url={} event=response.steer",
            self.sess.id,
            self.auth.id,
            self.plan.ws_url
        );
        true
    }

    /// Go: the writer goroutine. `ready` fires after the first `response.created`: follow-ups
    /// are not consumed before bootstrap succeeds, because a rejected initial request may retry
    /// on another credential using the same input.
    async fn run(mut self, ready: oneshot::Receiver<()>) {
        let mut cancel = self.shared.cancel.subscribe();
        tokio::select! {
            () = wait_cancelled(&mut cancel) => return,
            ready = ready => if ready.is_err() { return },
        }
        loop {
            if !self.flush_pending_creates().await {
                return;
            }
            let message = tokio::select! {
                () = wait_cancelled(&mut cancel) => return,
                () = self.shared.changed.notified() => {
                    if !self.flush_pending_creates().await {
                        return;
                    }
                    continue;
                }
                message = self.input.recv() => message,
            };
            let payload = match message {
                None => {
                    self.shared.client_gone();
                    return;
                }
                Some(Err(err)) => {
                    self.shared.fail(err);
                    return;
                }
                Some(Ok(payload)) => payload,
            };
            if !self.auth_enabled() {
                // A fresh client connection can select an enabled credential. Do not send this
                // frame on a disabled account or replay it on a different socket.
                self.shared.fail(ExecError::new(0, "websocket credential is no longer enabled"));
                return;
            }
            if !cpa_json::valid(&payload) {
                if !self.reject("invalid websocket request JSON").await {
                    return;
                }
                continue;
            }
            let kind = cpa_json::parse(&payload).g("type").str();
            match kind.as_str() {
                "response.steer" => {
                    if !self.forward_steer(payload).await {
                        return;
                    }
                }
                "response.create" | "response.append" => {
                    if self.pending_creates.is_empty() && self.shared.state.lock().ready_for_create() {
                        if !self.process_create(payload).await {
                            return;
                        }
                    } else {
                        if self.pending_creates.len() >= MAX_OUTSTANDING {
                            self.shared.fail(ExecError::new(0, "too many outstanding response.create requests"));
                            return;
                        }
                        self.pending_creates.push_back(payload);
                    }
                }
                other => {
                    if !self.reject(&format!("unsupported websocket request type: {other}")).await {
                        return;
                    }
                }
            }
        }
    }
}

impl CodexExecutor {
    /// Hands the socket with its first `response.create` already written over to the duplex
    /// streams. `call` keeps the session locked and routed until both tasks are done.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn stream_duplex(
        &self,
        cfg: Arc<Config>,
        auth: &Auth,
        req: Request,
        opts: Options,
        input: WebsocketInput,
        mut call: WsCall,
        plan: WsPlan,
        reporter: UsageReporter,
    ) -> StreamResult {
        let headers = std::mem::take(&mut call.handshake_headers);
        // This first frame was successfully written before the handoff.
        tracing::info!(
            "codex websockets: request forwarded session={} auth={} url={} event=response.create",
            call.sess.id,
            auth.id,
            plan.ws_url
        );
        let initial = Arc::new(Settings::from_plan(&plan));
        let (cancel, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                pending: VecDeque::from([Arc::clone(&initial)]),
                unacknowledged_steers: Vec::new(),
                accepted_steers: HashMap::new(),
                current: Arc::clone(&initial),
                response_id: String::new(),
                response_settings: HashMap::new(),
                steering_settings: HashMap::new(),
                response_order: Vec::new(),
                waiting_parent: String::new(),
                automatic_active: false,
            }),
            changed: Notify::new(),
            cancel,
            write_error: Mutex::new(None),
            client_gone: std::sync::atomic::AtomicBool::new(false),
        });
        let reporters = ReporterFactory { auth: auth.clone(), model: req.model.clone(), opts: opts.clone() };
        let (out_tx, out_rx) = mpsc::channel::<Result<Bytes, ExecError>>(1);
        let (ready_tx, ready_rx) = oneshot::channel();
        let plan = Arc::new(plan);
        let writer = Writer {
            exec: self.clone(),
            cfg,
            auth: auth.clone(),
            req,
            opts,
            input,
            initial: Arc::clone(&initial),
            plan: Arc::clone(&plan),
            sess: Arc::clone(&call.sess),
            conn: Arc::clone(&call.conn),
            shared: Arc::clone(&shared),
            out: out_tx.clone(),
            pending_creates: VecDeque::new(),
        };
        let writer_task = tokio::spawn(writer.run(ready_rx));
        tokio::spawn(async move {
            read_loop(&mut call, &plan, &shared, &out_tx, ready_tx, reporter, &reporters).await;
            // Closing releases a writer blocked in the network; join it before releasing the
            // execution session so no task outlives its socket.
            shared.cancel.send_replace(true);
            call.invalidate("duplex_closed", false);
            let _ = writer_task.await;
            drop(call);
        });
        StreamResult::new(headers, out_rx)
    }
}

/// Go: the `response.created` bookkeeping. An automatic successor (no pending create) inherits
/// the settings retained for its parent; an explicit one takes the oldest pending create.
fn on_response_created(shared: &Shared, root: &Value, first_response: bool) -> Result<(), ExecError> {
    {
        let mut st = shared.state.lock();
        let mut parent = root.g("response.previous_response_id").str();
        if parent.is_empty() {
            parent = st.response_id.clone();
        }
        if !first_response && st.pending.is_empty() {
            let settings = st.steering_settings.get(&parent).or_else(|| st.response_settings.get(&parent)).cloned();
            match settings {
                Some(settings) => st.current = settings,
                None => return Err(ExecError::new(0, "automatic successor has no retained parent settings")),
            }
        }
        st.accepted_steers.retain(|_, target| *target != parent);
        st.waiting_parent.clear();
        st.automatic_active = !first_response && st.pending.is_empty();
        if let Some(next) = st.pending.pop_front() {
            st.current = next;
        }
        let response_id = root.g("response.id").str();
        st.response_id = response_id.clone();
        // Retain response settings, not request history or authorization headers. In-flight
        // steering pins its parent's settings independently of this window.
        let snapshot = Arc::new(st.current.snapshot());
        st.response_settings.insert(response_id.clone(), snapshot);
        st.response_order.push(response_id);
        if st.response_order.len() > MAX_RETAINED_RESPONSES {
            let oldest = st.response_order.remove(0);
            st.response_settings.remove(&oldest);
        }
        st.release_steering_settings(&parent);
    }
    shared.changed.notify_one();
    Ok(())
}

/// Go: the reader goroutine. Relays upstream events downstream and keeps the steering
/// bookkeeping; returns when the stream ends.
async fn read_loop(
    call: &mut WsCall,
    plan: &WsPlan,
    shared: &Shared,
    out: &mpsc::Sender<Result<Bytes, ExecError>>,
    ready: oneshot::Sender<()>,
    mut reporter: UsageReporter,
    reporters: &ReporterFactory,
) {
    let to = plan.prepared.to;
    let mut ready = Some(ready);
    let mut cancel = shared.cancel.subscribe();
    let model_level_cooling = plan.model_level_cooling;
    let mut first_response = true;
    let mut response_active = false;
    let mut items = OutputItems::default();

    let send = |chunk: Result<Bytes, ExecError>| async {
        out.send(chunk).await.is_ok()
    };

    loop {
        let read = tokio::select! {
            () = wait_cancelled(&mut cancel) => None,
            read = call.next_read() => Some(read),
        };
        let payload = match read {
            Some(Some(Read::Text(payload))) => payload,
            terminal => {
                let mut err = match terminal {
                    Some(Some(Read::Err(err))) => map_read_error(&err),
                    Some(None) => ExecError::new(0, SESSION_READ_CLOSED),
                    _ => ExecError::new(0, "context canceled"),
                };
                if let Some(write_err) = shared.write_error.lock().take() {
                    err = write_err;
                }
                // The client going away is not an upstream failure: nothing is published or sent.
                if !shared.is_client_gone() && !out.is_closed() {
                    let err = connection_error(err);
                    reporter.publish_failure(&err);
                    let _ = send(Err(err)).await;
                }
                return;
            }
        };
        if payload.is_empty() {
            continue;
        }
        let root = cpa_json::parse(&payload);
        let event_type = root.g("type").str();
        let establishing = first_response && event_type == "response.created";
        if event_type == "response.created" {
            if let Err(err) = on_response_created(shared, &root, first_response) {
                let err = connection_error(err);
                reporter.publish_failure(&err);
                let _ = send(Err(err)).await;
                return;
            }
            if !first_response {
                // Every later response of the socket is its own request with its own record.
                let current = Arc::clone(&shared.state.lock().current);
                reporter = reporters.reporter(&current, to);
                reporter.start_response_ttft();
            }
            first_response = false;
            response_active = true;
            items = OutputItems::default();
        }
        observe_responses_token_event(&reporter, &payload);
        plan.log_frame(&payload);

        // Steering acknowledgements, pending notifications and failures are opaque: IDs, input,
        // sequence numbers and event types are preserved byte for byte.
        if event_type.starts_with("response.steer.") {
            let id = root.g("steer.id").str();
            {
                let mut st = shared.state.lock();
                let mut parent = root.g("steer.previous_response_id").str();
                if parent.is_empty() {
                    parent = st.response_id.clone();
                }
                let consume_submission = |st: &mut State| {
                    if let Some(i) = st.unacknowledged_steers.iter().position(|t| *t == parent || t.is_empty()) {
                        st.unacknowledged_steers.remove(i);
                    }
                };
                match event_type.as_str() {
                    "response.steer.accepted" => {
                        consume_submission(&mut st);
                        st.accepted_steers.insert(id, parent.clone());
                    }
                    "response.steer.failed" => {
                        if st.accepted_steers.remove(&id).is_none() {
                            consume_submission(&mut st);
                        }
                        st.release_steering_settings(&parent);
                    }
                    "response.steer.pending" => {
                        // Tool results may already be waiting in the writer. No automatic
                        // successor can start until an explicit continuation supplies them.
                        st.waiting_parent = parent.clone();
                    }
                    _ => {}
                }
            }
            shared.changed.notify_one();
            if !send(Ok(Bytes::from(payload))).await {
                return;
            }
            continue;
        }

        if !first_response && (event_type == "error" || event_type == "response.failed") {
            let credential_err = parse_error_frame(&root, model_level_cooling)
                .or_else(|| terminal_failure_err(&root, model_level_cooling).map(|(err, _)| err));
            if let Some(err) = credential_err
                && matches!(err.status, 401 | 403 | 429)
            {
                // Account health is independent of which queued request failed. The conductor
                // records the original classification without replaying this started stream.
                reporter.publish_failure(&err);
                if send(Ok(Bytes::from(payload))).await {
                    let _ = send(Err(err)).await;
                }
                return;
            }
        }

        let mut event_settings = Arc::clone(&shared.state.lock().current);
        let mut event_reporter = reporter.clone();
        if !first_response && (event_type == "response.failed" || event_type == "error") {
            let mut failed_id = root.g("response.id").str();
            if failed_id.is_empty() {
                failed_id = root.g("response_id").str();
            }
            let ambiguous = {
                let mut st = shared.state.lock();
                // A failure for the running response must not consume a queued create. A rejection
                // before response.created instead owns the oldest pending create.
                let current_failure = !failed_id.is_empty() && failed_id == st.response_id;
                let ambiguous = failed_id.is_empty()
                    && ((!st.pending.is_empty() && response_active) || !st.unacknowledged_steers.is_empty());
                if !st.pending.is_empty() && !current_failure && !ambiguous {
                    if let Some(next) = st.pending.pop_front() {
                        event_reporter = reporters.reporter(&next, to);
                        event_settings = next;
                    }
                } else if !ambiguous {
                    response_active = false;
                    st.automatic_active = false;
                }
                ambiguous
            };
            shared.changed.notify_one();
            if ambiguous {
                // Without a response id, assigning this failure could corrupt either request.
                // Preserve the event and fail the socket without guessing a scope, replaying
                // input, or cooling the credential.
                let err = connection_error(ExecError::new(0, "cannot associate websocket failure with a response or pending create"));
                reporter.publish_failure(&err);
                if send(Ok(Bytes::from(payload))).await {
                    let _ = send(Err(err)).await;
                }
                return;
            }
        }

        let restore = !event_settings.multi_agent_v2_conflict
            && (event_settings.optimize_multi_agent_v2 || call.sess.is_multi_agent_v2_optimized(&call.conn));
        let payload = restore_response(&payload, restore);
        let frame = cpa_json::parse(&payload);
        // Invalidate replay for every rejected request using the metadata that belongs to this
        // event. Only the first rejection can enter conductor bootstrap retry; later failures
        // stay on this socket.
        let mut terminal_err: Option<ExecError> = None;
        let mut replay_err: Option<ExecError> = None;
        if let Some(ws_err) = parse_error_frame(&frame, model_level_cooling) {
            replay_err = clear_replay_on_error_frame(&event_settings.replay_scope, &frame).err();
            if replay_err.is_none() {
                plan.log_error("upstream_error", &ws_err.message);
            }
            terminal_err = Some(ws_err);
        } else if let Some((stream_err, body)) = terminal_failure_err(&frame, model_level_cooling) {
            replay_err = clear_replay_on_invalid_signature(&event_settings.replay_scope, stream_err.status, &body).err();
            if replay_err.is_none() {
                plan.log_error("upstream_error", &stream_err.message);
            }
            terminal_err = Some(stream_err);
        }
        if let Some(err) = replay_err {
            // A failed replay cleanup replaces the upstream error and ends the stream.
            plan.log_error("replay_clear_error", &err.message);
            event_reporter.publish_failure(&err);
            let _ = send(Err(err)).await;
            return;
        }
        if let Some(err) = terminal_err {
            event_reporter.publish_failure(&err);
            if first_response {
                let _ = send(Err(err)).await;
                return;
            }
        }

        if event_type == "response.output_item.done" {
            items.collect(&frame, &payload);
        }
        let mut payload = payload;
        if matches!(event_type.as_str(), "response.completed" | "response.done" | "response.incomplete") {
            response_active = false;
            let current = {
                let mut st = shared.state.lock();
                st.automatic_active = false;
                Arc::clone(&st.current)
            };
            shared.changed.notify_one();
            payload = normalize_completion(&payload);
            if !current.native {
                payload = patch_completed_output(&payload, &items);
            }
            if event_type != "response.incomplete" {
                cache_replay_from_completed(&current.replay_scope, &cpa_json::parse(&payload));
            }
            match parse_codex_usage(&payload) {
                Some(detail) => reporter.publish(detail),
                None => reporter.ensure_published(),
            }
        }
        if !send(Ok(Bytes::from(ensure_responses_usage_details(&payload)))).await {
            return;
        }
        if establishing && let Some(tx) = ready.take() {
            // response.created is delivered before any locally generated error so the handler
            // also observes a successful bootstrap first.
            let _ = tx.send(());
        }
    }
}
