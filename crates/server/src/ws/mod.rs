//! Client-facing Responses websocket: `GET /v1/responses` and `GET /backend-api/codex/responses`
//! (Go: openai_responses_websocket*.go).
//!
//! Turns are executed through the auth manager. While the pinned credential speaks the upstream
//! websocket (Codex / xAI with `websockets: true`) frames pass through unchanged and continuations
//! must reuse that credential's live socket ([`upstream`]); otherwise frames are normalized into
//! full Responses requests ([`requests`]) and each upstream SSE event is written back as one JSON
//! text frame. Duplex steering (`codex-response-steering`) is not implemented.

mod conn;
pub mod requests;
pub mod toolcache;
pub mod upstream;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::http::HeaderValue;
use axum::response::Response;
use bytes::Bytes;
use cpa_core::format::Format;
use cpa_core::registry::global_registry;
use cpa_core::util::{get_provider_name, resolve_auto_model};
use cpa_json::J;
use cpa_runtime::conductor::Manager;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{Instant, interval_at};

use crate::error::{ErrorMessage, build_error_response_body_with_error, is_request_fault, status_text};
use crate::exec::{ExecArgs, Pipeline};
use crate::req::ReqInfo;
use crate::reqlog::ApiLog;
use crate::state::AppState;
use crate::thinking::parse_suffix;
use conn::{Conn, Disconnects};
use requests::WS_REQUEST_TYPE_CREATE;
use toolcache::{ToolCacheTurn, is_complete_tool_call};

const WS_EVENT_TYPE_ERROR: &str = "error";
const WS_CLOSE_REASON_MAX_BYTES: usize = 123;
const CLOSE_MESSAGE_TOO_BIG: u16 = 1009;

/// `GET /v1/responses` upgrade.
pub async fn responses_websocket(
    State(st): State<AppState>,
    info: ReqInfo,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    // gorilla's failed upgrade: 400 `Bad Request` with `Sec-Websocket-Version: 13`.
    let Ok(ws) = ws else {
        return upgrade_failed();
    };
    let turn_state = info.header("x-codex-turn-state");
    // A failed handshake would otherwise leave the deferred request log waiting forever.
    let api_log = info.api_log.clone();
    let mut resp = ws
        .max_message_size(1 << 30)
        .max_frame_size(1 << 30)
        .on_failed_upgrade(move |_| api_log.ws_finished())
        .on_upgrade(move |socket| session(socket, st, info));
    // gorilla writes `Connection: Upgrade` (axum lowercases the token).
    resp.headers_mut().insert(axum::http::header::CONNECTION, HeaderValue::from_static("Upgrade"));
    // The sticky turn state is echoed so reconnects keep their affinity.
    if !turn_state.is_empty()
        && let Ok(v) = HeaderValue::from_str(&turn_state)
    {
        resp.headers_mut().insert("x-codex-turn-state", v);
    }
    resp
}

fn upgrade_failed() -> Response {
    crate::reply::Reply::new(400)
        .content_type("text/plain; charset=utf-8")
        .with_header(axum::http::HeaderName::from_static("sec-websocket-version"), "13")
        .with_header(axum::http::HeaderName::from_static("x-content-type-options"), "nosniff")
        .with_body("Bad Request\n")
        .into_response()
}

/// Result of one forwarded turn.
struct TurnOutcome {
    output: Vec<u8>,
    response_id: String,
    pending_tool_call_ids: Vec<String>,
}

/// `truncateWebsocketCloseReason`: at most `max_bytes`, never splitting a character.
fn truncate_close_reason(reason: &str, max_bytes: usize) -> String {
    if reason.len() <= max_bytes {
        return reason.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_string()
}

/// `websocketClosePayloadForUpstreamError`: `message_too_big` becomes close code 1009.
fn close_frame_for_upstream_error(err: &ErrorMessage) -> Option<CloseFrame> {
    if let Some(frame) = upstream::replay_required_close_frame(err) {
        return Some(frame);
    }
    let status = err.status_or_500();
    if status != 413 {
        return None;
    }
    let body = cpa_json::parse_str(&err.text);
    if body.g("error.code").str() != "message_too_big" {
        return None;
    }
    let mut reason = body.g("error.message").str().trim().to_string();
    if reason.is_empty() {
        reason = "message too big".into();
    }
    Some(CloseFrame {
        code: CLOSE_MESSAGE_TOO_BIG,
        reason: truncate_close_reason(&reason, WS_CLOSE_REASON_MAX_BYTES).into(),
    })
}

/// `shouldExposeResponsesUpstreamError`: only request-shape failures reach the client; others
/// close the socket so the client reconnects and retries.
fn should_expose_upstream_error(err: &ErrorMessage) -> bool {
    err.terminal_auth || is_request_fault(err.status_or_500(), &err.text)
}

fn canonical_header_name(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// `buildResponsesWebsocketErrorPayload`: `{"type":"error","status":N,["headers":{..},]"error":..}`.
fn build_error_payload(err: &ErrorMessage) -> Vec<u8> {
    let status = err.status_or_500();
    let text = err.text_or_status_exact();
    let body = build_error_response_body_with_error(status, &text, err.terminal_auth);
    let mut payload = serde_json::json!({"type": WS_EVENT_TYPE_ERROR, "status": status});
    let mut headers = serde_json::Map::new();
    for name in err.addon.keys() {
        if let Some(first) = err.addon.get_all(name).iter().next()
            && let Ok(v) = first.to_str()
        {
            headers.insert(canonical_header_name(name.as_str()), Value::String(v.to_string()));
        }
    }
    if !headers.is_empty() {
        payload["headers"] = Value::Object(headers);
    }
    if cpa_json::valid(&body) {
        let parsed = cpa_json::parse(&body);
        let node = parsed.g("error");
        payload["error"] = if node.exists() { node.value() } else { parsed };
    }
    if !payload.g("error").exists() {
        cpa_json::set(&mut payload, "error.type", "server_error");
        cpa_json::set(&mut payload, "error.message", text);
    }
    cpa_json::to_vec(&payload)
}

impl ErrorMessage {
    /// Error text without trimming (`errMsg.Error.Error()`), falling back to the status text.
    fn text_or_status_exact(&self) -> String {
        if self.text.trim().is_empty() {
            status_text(self.status_or_500()).to_string()
        } else {
            self.text.clone()
        }
    }
}

/// `websocketJSONPayloadsFromChunk`: JSON objects found in an SSE chunk (event lines and
/// `[DONE]` are skipped).
fn json_payloads_from_chunk(chunk: &[u8]) -> Vec<Vec<u8>> {
    let mut payloads = Vec::new();
    for line in chunk.split(|b| *b == b'\n') {
        let mut line = line.trim_ascii();
        if line.is_empty() || line.starts_with(b"event:") {
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"data:") {
            line = rest.trim_ascii();
        }
        if line.is_empty() || line == b"[DONE]" {
            continue;
        }
        if cpa_json::valid(line) {
            payloads.push(line.to_vec());
        }
    }
    if !payloads.is_empty() {
        return payloads;
    }
    let mut trimmed = chunk.trim_ascii();
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        trimmed = rest.trim_ascii();
    }
    if !trimmed.is_empty() && trimmed != b"[DONE]" && cpa_json::valid(trimmed) {
        payloads.push(trimmed.to_vec());
    }
    payloads
}

fn is_completion_event(event_type: &str) -> bool {
    event_type == "response.completed" || event_type == "response.done"
}

/// Items collected from `response.output_item.done`, used to rebuild an empty completion output.
#[derive(Default)]
struct OutputCollector {
    by_index: BTreeMap<i64, Value>,
    fallback: Vec<Value>,
}

impl OutputCollector {
    fn clear(&mut self) {
        self.by_index.clear();
        self.fallback.clear();
    }

    fn is_empty(&self) -> bool {
        self.by_index.is_empty() && self.fallback.is_empty()
    }

    fn collect(&mut self, payload: &Value) {
        if payload.g("type").str() != "response.output_item.done" {
            return;
        }
        let item = payload.g("item");
        if !item.exists() || !item.is_object() {
            return;
        }
        let index = payload.g("output_index");
        if index.exists() {
            self.by_index.insert(index.int(), item.value());
        } else {
            self.fallback.push(item.value());
        }
    }

    fn collected_tool_calls(&self) -> std::collections::HashMap<String, Value> {
        let mut out = std::collections::HashMap::new();
        for item in self.by_index.values().chain(self.fallback.iter()) {
            if is_complete_tool_call(item) {
                out.insert(item.g("call_id").str().trim().to_string(), item.clone());
            }
        }
        out
    }

    /// `responseCompletedOutputFromPayload`.
    fn completed_output(&self, payload: &Value) -> Vec<u8> {
        let output = payload.g("response.output");
        if output.exists() && output.is_array() && !output.array().is_empty() {
            return cpa_json::to_vec(&output.value());
        }
        if self.is_empty() {
            return b"[]".to_vec();
        }
        let mut items = Vec::new();
        for item in self.by_index.values().chain(self.fallback.iter()) {
            if requests::is_tool_call_type(&item.g("type").str()) && !is_complete_tool_call(item) {
                continue;
            }
            items.push(item.clone());
        }
        cpa_json::to_vec(&Value::Array(items))
    }

    /// `restoreResponsesWebsocketCompletionOutput`: `Some(new payload)` when it changed.
    fn restore_completion(&self, payload: &Value) -> Option<Vec<u8>> {
        let output = payload.g("response.output");
        if output.exists() && output.is_array() && !output.array().is_empty() {
            let collected = self.collected_tool_calls();
            if collected.is_empty() {
                return None;
            }
            let mut changed = false;
            let items: Vec<Value> = output
                .array()
                .into_iter()
                .map(|item| {
                    let value = item.value();
                    if requests::is_tool_call_type(&item.g("type").str()) {
                        let call_id = item.g("call_id").str().trim().to_string();
                        if let Some(c) = collected.get(&call_id)
                            && *c != value
                        {
                            changed = true;
                            return c.clone();
                        }
                    }
                    value
                })
                .collect();
            if !changed {
                return None;
            }
            let mut out = payload.clone();
            cpa_json::set(&mut out, "response.output", Value::Array(items));
            return Some(cpa_json::to_vec(&out));
        }
        if self.is_empty() {
            return None;
        }
        let rebuilt = self.completed_output(payload);
        let mut out = payload.clone();
        let _ = cpa_json::set_raw(&mut out, "response.output", &String::from_utf8_lossy(&rebuilt));
        Some(cpa_json::to_vec(&out))
    }
}

/// `recordPendingToolCallIDsFromPayload`.
fn record_pending_tool_calls(pending: &mut BTreeSet<String>, payload: &Value) {
    let mut update = |item: &Value| match item.get("type").and_then(Value::as_str).map(str::trim) {
        Some("function_call" | "custom_tool_call") => {
            if is_complete_tool_call(item) {
                pending.insert(item.g("call_id").str().trim().to_string());
            }
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            let id = item.g("call_id").str().trim().to_string();
            if !id.is_empty() {
                pending.remove(&id);
            }
        }
        _ => {}
    };
    let item = payload.g("item");
    if item.exists() {
        update(&item.value());
    }
    let output = payload.g("response.output");
    if output.is_array() {
        for item in output.array() {
            update(&item.value());
        }
    }
}

/// `responsesWebsocketErrorMessageFromPayload`.
fn error_message_from_payload(payload: &[u8]) -> ErrorMessage {
    let root = cpa_json::parse(payload);
    let mut status = root.g("status").int();
    if status <= 0 {
        status = root.g("status_code").int();
    }
    if !(100..=599).contains(&status) {
        status = 500;
    }
    let text = String::from_utf8_lossy(payload.trim_ascii()).into_owned();
    ErrorMessage::new(status as u16, if text.is_empty() { status_text(status as u16).to_string() } else { text })
}

// ------------------------------------------------------------------ provider/auth helpers

/// `responsesWebsocketResolvedModelName`.
fn resolved_model_name(model: &str) -> String {
    let initial = parse_suffix(model);
    if initial.model_name == "auto" {
        let base = resolve_auto_model(&initial.model_name);
        if initial.has_suffix {
            return format!("{base}({})", initial.raw_suffix);
        }
        return base;
    }
    resolve_auto_model(model)
}

/// `responsesWebsocketProviderSetForModel`: (providers, model key).
fn provider_set_for_model(resolved: &str) -> (BTreeSet<String>, String) {
    let parsed = parse_suffix(resolved);
    let base = parsed.model_name.trim().to_string();
    let mut providers = get_provider_name(&base);
    if providers.is_empty() && base != resolved {
        providers = get_provider_name(resolved);
    }
    let set = providers
        .iter()
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    let key = if base.is_empty() { resolved.trim().to_string() } else { base };
    (set, key)
}

/// `responsesWebsocketAuthAvailableForModel`.
fn auth_available_for_model(auth: &cpa_auth::Auth, model: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
    use cpa_auth::Status;
    if auth.disabled || auth.status == Status::Disabled {
        return false;
    }
    if !model.is_empty() && !auth.model_states.is_empty() {
        let mut state = auth.model_states.get(model);
        if state.is_none() {
            let base = parse_suffix(model).model_name.trim().to_string();
            if !base.is_empty() && base != model {
                state = auth.model_states.get(&base);
            }
        }
        if let Some(state) = state {
            if state.status == Status::Disabled {
                return false;
            }
            if state.unavailable && state.next_retry_after.is_some_and(|t| t > now) {
                return false;
            }
            return true;
        }
    }
    !(auth.unavailable && auth.next_retry_after.is_some_and(|t| t > now))
}

/// Credentials able to serve `model` right now (`responsesWebsocketAvailableAuthsForModel`).
fn available_auths_for_model(manager: &Manager, model: &str) -> Vec<cpa_auth::Auth> {
    let resolved = resolved_model_name(model);
    let (providers, model_key) = provider_set_for_model(&resolved);
    if providers.is_empty() {
        return Vec::new();
    }
    let registry = global_registry();
    let now = chrono::Utc::now();
    manager
        .list()
        .into_iter()
        .filter(|auth| {
            let provider = auth.provider.trim().to_lowercase();
            providers.contains(&provider)
                && (model_key.is_empty() || registry.client_supports_model(&auth.id, &model_key))
                && auth_available_for_model(auth, &model_key, now)
        })
        .collect()
}

/// `websocketUpstreamSupportsCompactionReplayForModel`: every candidate credential is Codex.
fn supports_compaction_replay_for_model(manager: &Manager, model: &str) -> bool {
    let auths = available_auths_for_model(manager, model);
    !auths.is_empty() && auths.iter().all(|a| a.provider.trim().eq_ignore_ascii_case("codex"))
}

// ------------------------------------------------------------------ session

struct Writer<'a> {
    socket: &'a mut Conn,
    api_log: &'a ApiLog,
    timeline: bool,
}

impl Writer<'_> {
    async fn text(&mut self, payload: &[u8]) -> Result<(), axum::Error> {
        if self.timeline {
            self.api_log.ws_timeline_append("response", payload);
        }
        let text = String::from_utf8_lossy(payload).into_owned();
        self.socket.send(Message::Text(text.into())).await
    }
}

/// How a turn ended.
enum TurnEnd {
    Completed(TurnOutcome),
    /// The socket is finished (closed with or without a frame, write failure, client gone).
    Terminate(String),
    /// The upstream error was withheld from the client (`suppressError`); the caller decides.
    Failed(ErrorMessage),
}

/// The per-turn switches of `responsesWebsocketForwardOptions` that depend on the selected
/// credential (read at use, like Go's closures).
struct ForwardOptions<'a> {
    /// Keep the upstream completion output as is instead of rebuilding it from the items.
    preserve_completion_output: &'a (dyn Fn() -> bool + Sync),
    /// A Codex steering stream owns connection termination.
    duplex_stream: &'a (dyn Fn() -> bool + Sync),
}

#[allow(clippy::too_many_arguments)]
async fn forward_turn(
    socket: &mut Conn,
    disconnects: &mut Disconnects,
    info: &ReqInfo,
    keepalive: Duration,
    timeline: bool,
    rx: &mut mpsc::Receiver<Result<Bytes, ErrorMessage>>,
    tool_turn: &mut Option<ToolCacheTurn>,
    session_key: &str,
    session_id: &str,
    suppress: &(dyn Fn(&ErrorMessage) -> bool + Sync),
    options: ForwardOptions<'_>,
) -> TurnEnd {
    let mut completed = false;
    let mut response_started = false;
    let mut completed_output: Vec<u8> = b"[]".to_vec();
    let mut completed_response_id = String::new();
    let mut collector = OutputCollector::default();
    let mut pending: BTreeSet<String> = BTreeSet::new();
    let mut ticker = (!keepalive.is_zero()).then(|| interval_at(Instant::now() + keepalive, keepalive));
    let api_log = info.api_log.clone();

    macro_rules! outcome {
        () => {
            TurnOutcome {
                output: completed_output.clone(),
                response_id: completed_response_id.clone(),
                pending_tool_call_ids: pending.iter().cloned().collect(),
            }
        };
    }

    loop {
        tokio::select! {
            biased;
            item = rx.recv() => {
                let Some(item) = item else {
                    if (options.duplex_stream)() {
                        // A duplex stream ends with its socket, not an individual response.
                        return TurnEnd::Terminate("websocket: close sent".into());
                    }
                    if !completed {
                        let err = ErrorMessage::new(408, "stream closed before response.completed");
                        api_log.record_error(err.status, &err.text);
                        api_log.mark_response_timestamp();
                        if timeline {
                            api_log.ws_timeline_append("disconnect", err.text.as_bytes());
                        }
                        return TurnEnd::Terminate(err.text);
                    }
                    return TurnEnd::Completed(outcome!());
                };
                let chunk = match item {
                    Ok(chunk) => chunk,
                    Err(err) => {
                        api_log.record_error(err.status, &err.text);
                        if suppress(&err) {
                            return TurnEnd::Failed(err);
                        }
                        api_log.mark_response_timestamp();
                        return end_with_error(socket, &api_log, timeline, &err, None).await;
                    }
                };
                if let Some(t) = ticker.as_mut() {
                    t.reset();
                }
                for payload_bytes in json_payloads_from_chunk(&chunk) {
                    let mut payload = cpa_json::parse(&payload_bytes);
                    let mut bytes = payload_bytes;
                    let event_type = payload.g("type").str();
                    if event_type == "response.created" {
                        response_started = true;
                        completed = false;
                        collector.clear();
                        pending.clear();
                    }
                    collector.collect(&payload);
                    if is_completion_event(&event_type)
                        && !(options.preserve_completion_output)()
                        && let Some(restored) = collector.restore_completion(&payload)
                    {
                        payload = cpa_json::parse(&restored);
                        bytes = restored;
                    }
                    match tool_turn.as_mut() {
                        Some(turn) => turn.record_response(&payload),
                        None => toolcache::record_calls_from_payload(session_key, &payload),
                    }
                    record_pending_tool_calls(&mut pending, &payload);

                    let mut payload_err: Option<ErrorMessage> = None;
                    // In Codex duplex mode the executor owns connection termination: payload errors
                    // after response.created are recoverable events; the stream's own error still
                    // closes the socket.
                    let preserve_error_event = response_started && (options.duplex_stream)();
                    if event_type == WS_EVENT_TYPE_ERROR && !preserve_error_event {
                        let err = error_message_from_payload(&bytes);
                        api_log.record_error(err.status, &err.text);
                        payload_err = Some(err);
                    } else if is_completion_event(&event_type) {
                        completed = true;
                        completed_output = collector.completed_output(&payload);
                        completed_response_id = payload.g("response.id").str().trim().to_string();
                    }
                    if let Some(err) = &payload_err
                        && suppress(err)
                    {
                        return TurnEnd::Failed(err.clone());
                    }
                    api_log.mark_response_timestamp();
                    if let Some(err) = payload_err {
                        return end_with_error(socket, &api_log, timeline, &err, Some(&bytes)).await;
                    }
                    let mut writer = Writer { socket, api_log: &api_log, timeline };
                    if let Err(e) = writer.text(&bytes).await {
                        tracing::warn!(
                            "responses websocket: downstream_out write failed id={session_id} event={event_type} error={e}"
                        );
                        return TurnEnd::Terminate(e.to_string());
                    }
                }
            }
            _ = async { ticker.as_mut().expect("guarded by the branch condition").tick().await }, if ticker.is_some() => {
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    return TurnEnd::Terminate("ping failed".into());
                }
            }
            (provider, text) = disconnects.fired() => {
                if provider == "codex" && (options.duplex_stream)() {
                    // The steering stream drains its acknowledgements and pending events in order.
                    disconnects.disarm();
                    continue;
                }
                return TurnEnd::Terminate(close_for_upstream_disconnect(socket, session_id, &text).await);
            }
        }
    }
}

/// Upstream failure handling: `message_too_big` becomes a close frame, request-shape faults are
/// written as one `error` frame, everything else closes the socket without a frame.
async fn end_with_error(
    socket: &mut Conn,
    api_log: &ApiLog,
    timeline: bool,
    err: &ErrorMessage,
    payload: Option<&[u8]>,
) -> TurnEnd {
    if let Some(frame) = close_frame_for_upstream_error(err) {
        let _ = socket.send(Message::Close(Some(frame))).await;
        return TurnEnd::Terminate(err.text.clone());
    }
    if !should_expose_upstream_error(err) {
        // Keep the reason in the request-log timeline even though the client only sees a close.
        if timeline {
            api_log.ws_timeline_append("disconnect", err.text.as_bytes());
        }
        return TurnEnd::Terminate(err.text.clone());
    }
    let body = match payload {
        Some(p) if !p.is_empty() => p.to_vec(),
        _ => build_error_payload(err),
    };
    let mut writer = Writer { socket, api_log, timeline };
    let _ = writer.text(&body).await;
    tracing::info!(
        "responses websocket: downstream_out event={} payload={}",
        cpa_json::parse(&body).g("type").str(),
        String::from_utf8_lossy(&body)
    );
    TurnEnd::Terminate(err.text.clone())
}

struct SessionState {
    last_request: Vec<u8>,
    last_response_output: Vec<u8>,
    last_response_id: String,
    pending_tool_call_ids: Vec<String>,
    pending_prewarm_id: String,
    pinned: upstream::PinnedAuths,
    passthrough_model: String,
    mode: upstream::UpstreamMode,
    upstream_ws_auth_id: String,
    observed_compaction: upstream::ObservedCompaction,
}

/// What the credential-selection callback saw during one turn (Go: the variables captured by
/// `WithSelectedAuthIDCallback`).
#[derive(Default)]
struct SelectionObserved {
    last_attempted: String,
    mode: upstream::UpstreamMode,
    observed: bool,
    pinned_attempted: bool,
    /// Native Codex clients on Codex credentials keep the completion output untouched.
    preserve_native_output: bool,
}

async fn session(socket: WebSocket, st: AppState, info: ReqInfo) {
    let session_id = uuid::Uuid::new_v4().to_string();
    let session_key = toolcache::downstream_session_key(&info.headers);
    toolcache::retain_session(&session_key);
    tracing::info!("responses websocket: client connected id={session_id} remote={}", info.client_ip);

    // With response steering a dedicated reader owns the socket's input (Go: duplexInput).
    let cfg = st.cfg();
    let steering = cfg.codex.response_steering || cfg.codex_response_steering;
    let mut socket = if steering { Conn::duplex(socket) } else { Conn::direct(socket) };
    let mut disconnects = upstream_disconnects(&st.manager, &session_id);

    let reason = run_session(&mut socket, &mut disconnects, &st, &info, &session_id, &session_key).await;

    toolcache::release_session(&session_key);
    match &reason {
        Some(r) => tracing::debug!("responses websocket: session closing id={session_id} reason={r}"),
        None => tracing::info!("responses websocket: session closing id={session_id}"),
    }
    (st.close_execution_session)(&session_id);
    tracing::info!("responses websocket: upstream execution session closed id={session_id}");
    info.api_log.ws_finished();
    drop(socket);
}

/// Subscriptions to the upstream-disconnect notices of the Codex and xAI executors for this
/// session (Go: `UpstreamDisconnectChan`, only for providers with a registered executor).
fn upstream_disconnects(manager: &Manager, session_id: &str) -> Disconnects {
    let mut receivers = Vec::new();
    if manager.executor("codex").is_some()
        && let Some(rx) = cpa_executors::codex::upstream_disconnect_receiver(session_id)
    {
        receivers.push(("codex", rx));
    }
    if manager.executor("xai").is_some()
        && let Some(rx) = cpa_executors::xai::upstream_disconnect_receiver(session_id)
    {
        receivers.push(("xai", rx));
    }
    Disconnects::new(receivers)
}

/// `closeForUpstreamDisconnect`: mirror close codes 1009 and 1012 as close frames, expose only
/// request-shape faults as an error frame, otherwise close silently (the client reconnects, which
/// implies a full-context resend).
async fn close_for_upstream_disconnect(socket: &mut Conn, session_id: &str, text: &str) -> String {
    let err = disconnect_error(text);
    if let Some(frame) = close_frame_for_upstream_error(&err) {
        let _ = socket.send(Message::Close(Some(frame))).await;
        return text.to_string();
    }
    if should_expose_upstream_error(&err) {
        let body = build_error_payload(&err);
        if socket.send(Message::Text(String::from_utf8_lossy(&body).into_owned().into())).await.is_ok() {
            tracing::info!(
                "responses websocket: downstream_out disconnect_error id={session_id} event={} payload={}",
                cpa_json::parse(&body).g("type").str(),
                String::from_utf8_lossy(&body)
            );
        }
    }
    text.to_string()
}

/// The upstream disconnect notice as the error the handler classifies: the replay signal and
/// `message_too_big` bodies keep their statuses, anything else is a plain 500.
fn disconnect_error(text: &str) -> ErrorMessage {
    let status = if text.contains("upstream_http_replay_required") {
        426
    } else if cpa_json::parse_str(text).g("error.code").str() == "message_too_big" {
        413
    } else {
        500
    };
    ErrorMessage::new(status, text)
}

/// Closes the socket with the replay-required frame (the client reconnects and resends the turn).
async fn close_for_replay(socket: &mut Conn) -> Option<String> {
    let err = upstream::replay_required_error();
    if let Some(frame) = upstream::replay_required_close_frame(&err) {
        let _ = socket.send(Message::Close(Some(frame))).await;
    }
    Some(err.text)
}

/// The read loop; returns why the session ended (`None` for a clean client close).
async fn run_session(
    socket: &mut Conn,
    disconnects: &mut Disconnects,
    st: &AppState,
    info: &ReqInfo,
    session_id: &str,
    session_key: &str,
) -> Option<String> {
    use upstream::UpstreamMode;
    // Whether the Codex steering stream of the current turn owns the socket (Go: codexDuplexStream).
    let duplex_stream = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut state = SessionState {
        last_request: Vec::new(),
        last_response_output: b"[]".to_vec(),
        last_response_id: String::new(),
        pending_tool_call_ids: Vec::new(),
        pending_prewarm_id: String::new(),
        pinned: upstream::PinnedAuths::default(),
        passthrough_model: String::new(),
        mode: UpstreamMode::Unknown,
        upstream_ws_auth_id: String::new(),
        observed_compaction: upstream::ObservedCompaction::default(),
    };
    loop {
        let frame = tokio::select! {
            biased;
            frame = socket.recv() => frame,
            (provider, text) = disconnects.fired() => {
                if provider == "codex" && duplex_stream.load(std::sync::atomic::Ordering::Relaxed) {
                    // A steering stream owns the closure: it drains acknowledgements in order.
                    disconnects.disarm();
                    continue;
                }
                return Some(close_for_upstream_disconnect(socket, session_id, &text).await);
            }
        };
        let payload: Vec<u8> = match frame {
            None => return None,
            Some(Err(e)) => return Some(e.to_string()),
            Some(Ok(Message::Text(t))) => t.as_str().as_bytes().to_vec(),
            Some(Ok(Message::Binary(b))) => b.to_vec(),
            Some(Ok(Message::Close(_))) => {
                tracing::info!("responses websocket: client disconnected id={session_id}");
                return None;
            }
            Some(Ok(_)) => continue,
        };
        let pipeline = Pipeline::new(st, info);
        let timeline = pipeline.cfg.request_log && !pipeline.cfg.commercial_mode;
        if timeline {
            info.api_log.ws_timeline_append("request", &payload);
        }

        let root = cpa_json::parse(&payload);
        let explicit_model = root.g("model").str().trim().to_string();
        let mut request_model = explicit_model.clone();
        if request_model.is_empty() {
            request_model = state.passthrough_model.clone();
        }
        if request_model.is_empty() {
            request_model = cpa_json::parse(&state.last_request).g("model").str().trim().to_string();
        }

        // Credential affinity: keep the pinned credential only while it still serves the model.
        state.pinned.refresh(&st.manager, &request_model);
        let mut use_upstream_ws = upstream::uses_upstream_websocket_passthrough(&st.manager, &request_model);
        if !state.pinned.current.is_empty()
            && let Some(pinned_auth) = st.manager.get(&state.pinned.current)
            && upstream::supports_incremental_input(&pinned_auth)
        {
            let provider = pinned_auth.provider.trim().to_lowercase();
            use_upstream_ws = provider == "codex" || provider == "xai";
        }
        let native = upstream::native_passthrough_allowed(state.mode, use_upstream_ws, &state.pinned.current, &state.upstream_ws_auth_id);
        let requires_current_upstream = upstream::request_requires_current_upstream(&payload);
        if state.mode == UpstreamMode::Ws && !native && requires_current_upstream {
            return close_for_replay(socket).await;
        }
        if !explicit_model.is_empty() && !use_upstream_ws {
            state.passthrough_model.clear();
        }

        // A completed compaction response is evidence the credential that produced it supports
        // compaction replay.
        let mut observed_replay_auth_id = String::new();
        let mut observed_supported = false;
        let observed = &state.observed_compaction;
        if !observed.model_name.is_empty()
            && resolved_model_name(&observed.model_name) == resolved_model_name(&request_model)
            && !observed.auth_id.is_empty()
        {
            if !state.pinned.current.is_empty() {
                if state.pinned.current == observed.auth_id {
                    observed_supported = true;
                    observed_replay_auth_id = observed.auth_id.clone();
                }
            } else if let Some(auth) = st.manager.get(&observed.auth_id)
                && upstream::pinned_auth_matches_model(&auth, &request_model)
            {
                observed_supported = true;
                observed_replay_auth_id = observed.auth_id.clone();
            }
        }
        let mut allow_compaction_bypass = observed_supported;
        if !native {
            if !state.pinned.current.is_empty() {
                if let Some(auth) = st.manager.get(&state.pinned.current) {
                    allow_compaction_bypass |= auth.provider.trim().eq_ignore_ascii_case("codex");
                }
            } else {
                allow_compaction_bypass |= supports_compaction_replay_for_model(&st.manager, &request_model);
            }
        }

        let previous_response_id = root.g("previous_response_id").str().trim().to_string();
        let is_prewarm = !use_upstream_ws && requests::should_handle_prewarm_locally(&payload);
        let input_not_array = {
            let input = root.g("input");
            input.exists() && !input.is_array()
        };

        let normalized: Result<(Vec<u8>, Vec<u8>), ErrorMessage> = if !state.pending_prewarm_id.is_empty() && !previous_response_id.is_empty() {
            if previous_response_id != state.pending_prewarm_id {
                Err(requests::previous_response_not_found_error())
            } else {
                requests::normalize_prewarm_followup(&payload, &state.last_request)
            }
        } else if (is_prewarm && previous_response_id.is_empty())
            || (!state.pending_prewarm_id.is_empty() && root.g("type").str() == WS_REQUEST_TYPE_CREATE)
        {
            if input_not_array {
                Err(ErrorMessage::new(400, "websocket request requires array field: input"))
            } else {
                requests::normalize_create(&requests::transcript_replacement(&payload, &state.last_request))
            }
        } else if native {
            upstream::normalize_passthrough_request(&payload, &request_model).map(|r| (r, Vec::new()))
        } else if state.last_request.is_empty() && !previous_response_id.is_empty() {
            Err(requests::previous_response_not_found_error())
        } else {
            requests::normalize_request(
                &payload,
                &state.last_request,
                &state.last_response_output,
                &state.last_response_id,
                &state.pending_tool_call_ids,
                false,
                allow_compaction_bypass,
            )
        };

        let (mut request_json, mut updated_last_request) = match normalized {
            Ok(v) => v,
            Err(err) => {
                info.api_log.record_error(err.status, &err.text);
                info.api_log.mark_response_timestamp();
                let body = build_error_payload(&err);
                tracing::info!(
                    "responses websocket: downstream_out id={session_id} event=error payload={}",
                    String::from_utf8_lossy(&body)
                );
                let mut writer = Writer { socket, api_log: &info.api_log, timeline };
                if writer.text(&body).await.is_err() {
                    return Some("write failed".into());
                }
                continue;
            }
        };

        if is_prewarm {
            for buf in [&mut request_json, &mut updated_last_request] {
                let mut v = cpa_json::parse(buf);
                cpa_json::delete(&mut v, "generate");
                *buf = cpa_json::to_vec(&v);
            }
            state.last_request = updated_last_request;
            state.last_response_output = b"[]".to_vec();
            state.observed_compaction = upstream::ObservedCompaction::default();
            state.last_response_id.clear();
            state.pending_tool_call_ids.clear();
            let (payloads, prewarm_id) = requests::synthetic_prewarm_payloads(&request_json);
            for p in &payloads {
                info.api_log.mark_response_timestamp();
                let mut writer = Writer { socket, api_log: &info.api_log, timeline };
                if let Err(e) = writer.text(p).await {
                    return Some(e.to_string());
                }
            }
            state.pending_prewarm_id = prewarm_id;
            continue;
        }

        let mut tool_turn = None;
        let mut next_last_request = state.last_request.clone();
        if native {
            let model = cpa_json::parse(&request_json).g("model").str().trim().to_string();
            if !model.is_empty() {
                state.passthrough_model = model;
            }
        } else {
            let (prepared, turn) = toolcache::prepare_fallback_turn(session_key, &request_json);
            request_json = prepared;
            tool_turn = turn;
            next_last_request = request_json.clone();
        }

        let model = cpa_json::parse(&request_json).g("model").str();
        let native_request = cpa_core::util::is_codex_responses_lite_request(&root, &info.headers);
        duplex_stream.store(false, std::sync::atomic::Ordering::Relaxed);
        let steering_input = socket.input();
        let observed_selection = std::sync::Arc::new(std::sync::Mutex::new(SelectionObserved {
            last_attempted: state.pinned.current.clone(),
            ..Default::default()
        }));
        let on_selected: std::sync::Arc<dyn Fn(&str) + Send + Sync> = {
            let observed_selection = observed_selection.clone();
            let manager = st.manager.clone();
            let pinned = state.pinned.current.clone();
            let duplex_stream = duplex_stream.clone();
            let has_input = steering_input.is_some();
            let steering_oauth_only = pipeline.cfg.oauth_only_fields.contains("codex.response-steering");
            std::sync::Arc::new(move |auth_id: &str| {
                duplex_stream.store(false, std::sync::atomic::Ordering::Relaxed);
                let Ok(mut seen) = observed_selection.lock() else { return };
                seen.preserve_native_output = false;
                let id = auth_id.trim();
                if id.is_empty() {
                    return;
                }
                seen.last_attempted = id.to_string();
                seen.observed = true;
                seen.pinned_attempted |= !pinned.is_empty() && id == pinned;
                if let Some(auth) = manager.get(id) {
                    let provider = auth.provider.trim().to_lowercase();
                    seen.mode = if upstream::supports_incremental_input(&auth) && (provider == "codex" || provider == "xai") {
                        UpstreamMode::Ws
                    } else {
                        UpstreamMode::Http
                    };
                    // OAuth-only steering still leaves API keys in normal mode.
                    let steering_allowed = !steering_oauth_only || auth.auth_kind() != cpa_auth::types::AUTH_KIND_API_KEY;
                    duplex_stream.store(
                        has_input && steering_allowed && seen.mode == UpstreamMode::Ws && provider == "codex",
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    seen.preserve_native_output = native_request && provider == "codex";
                }
            })
        };
        let execution_auth_id = if state.pinned.current.is_empty() { observed_replay_auth_id } else { state.pinned.current.clone() };

        let mut args = ExecArgs::new(Format::OpenAIResponse, &model, Bytes::from(request_json), "");
        args.execution_session_id = Some(session_id);
        args.downstream_websocket = true;
        args.required_upstream_websocket = native && requires_current_upstream;
        args.on_selected_auth = Some(on_selected);
        if let Some(input) = steering_input {
            let manager = st.manager.clone();
            args.ws_input = Some(input);
            args.ws_auth_check = Some(cpa_runtime::executor::WebsocketAuthCheck(std::sync::Arc::new(move |auth_id: &str| {
                manager.get(auth_id).is_some_and(|a| !a.disabled && a.status != cpa_auth::Status::Disabled)
            })));
        }
        if !execution_auth_id.is_empty() {
            args.pinned_auth_id = Some(&execution_auth_id);
        }
        let mut stream = pipeline.execute_stream(args).await;

        // A connection-scoped continuation cannot rotate credentials in place: credential errors
        // are withheld and the client replays the full turn on a new socket.
        let replay_pinned_failure = |err: &ErrorMessage| {
            native
                && requires_current_upstream
                && observed_selection.lock().map(|s| s.pinned_attempted).unwrap_or(false)
                && upstream::should_replay_pinned_auth_failure(err)
        };

        let preserve_output = || observed_selection.lock().map(|s| s.preserve_native_output).unwrap_or(false);
        let is_duplex = || duplex_stream.load(std::sync::atomic::Ordering::Relaxed);
        let end = forward_turn(
            socket,
            disconnects,
            info,
            pipeline.settings.stream_keepalive,
            timeline,
            &mut stream.rx,
            &mut tool_turn,
            session_key,
            session_id,
            &replay_pinned_failure,
            ForwardOptions { preserve_completion_output: &preserve_output, duplex_stream: &is_duplex },
        )
        .await;
        let (selected_last, selected_mode, selected_seen, pinned_attempted) = match observed_selection.lock() {
            Ok(g) => (g.last_attempted.clone(), g.mode, g.observed, g.pinned_attempted),
            Err(_) => (String::new(), UpstreamMode::Unknown, false, false),
        };
        match end {
            TurnEnd::Terminate(reason) => return Some(reason),
            TurnEnd::Failed(err) => {
                if pinned_attempted && upstream::should_release_pinned_auth(&err) {
                    state.pinned.forget();
                }
                return close_for_replay(socket).await;
            }
            TurnEnd::Completed(outcome) => {
                if let Some(turn) = &tool_turn {
                    turn.commit();
                }
                state.pending_prewarm_id.clear();
                // Plugin/alternate routes bypass selection and count as HTTP.
                let attempted_mode = if selected_seen { selected_mode } else { UpstreamMode::Http };
                state.mode = attempted_mode;
                if attempted_mode == UpstreamMode::Ws {
                    state.upstream_ws_auth_id = selected_last.clone();
                    if !selected_last.is_empty() {
                        state.pinned.remember(&st.manager, &selected_last, &model);
                    }
                    state.passthrough_model = model;
                    state.last_request.clear();
                    state.last_response_output = b"[]".to_vec();
                    state.observed_compaction = upstream::ObservedCompaction::default();
                    state.last_response_id.clear();
                    state.pending_tool_call_ids.clear();
                } else {
                    state.upstream_ws_auth_id.clear();
                    state.last_request = next_last_request;
                    let full_transcript = requests::input_contains_full_transcript(&cpa_json::parse(&outcome.output));
                    if full_transcript {
                        state.observed_compaction = upstream::ObservedCompaction { model_name: model.clone(), auth_id: selected_last.clone() };
                    } else if !state.observed_compaction.model_name.is_empty() {
                        let observed = &state.observed_compaction;
                        let mismatch = resolved_model_name(&observed.model_name) != resolved_model_name(&model)
                            || (!observed.auth_id.is_empty() && !selected_last.is_empty() && observed.auth_id != selected_last);
                        if mismatch {
                            state.observed_compaction = upstream::ObservedCompaction::default();
                        }
                    }
                    state.last_response_output = outcome.output;
                    state.last_response_id = outcome.response_id.trim().to_string();
                    state.pending_tool_call_ids = outcome.pending_tool_call_ids;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_payloads_are_extracted_from_sse_chunks() {
        let chunk = b"event: response.created\ndata: {\"type\":\"response.created\"}\n\ndata: [DONE]\n\n";
        let payloads = json_payloads_from_chunk(chunk);
        assert_eq!(payloads, vec![br#"{"type":"response.created"}"#.to_vec()]);
        // a bare JSON object (no SSE framing) is accepted too
        assert_eq!(json_payloads_from_chunk(b"  {\"a\":1} ").len(), 1);
        assert!(json_payloads_from_chunk(b"data: [DONE]").is_empty());
        assert!(json_payloads_from_chunk(b"data: not json").is_empty());
    }

    #[test]
    fn completion_output_is_rebuilt_from_collected_items() {
        let mut c = OutputCollector::default();
        c.collect(&cpa_json::parse(br#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"b"}}"#));
        c.collect(&cpa_json::parse(br#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"c","name":"f","arguments":"{}"}}"#));
        c.collect(&cpa_json::parse(br#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"half"}}"#));
        let payload = cpa_json::parse(br#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#);
        let restored = c.restore_completion(&payload).unwrap();
        let v: Value = serde_json::from_slice(&restored).unwrap();
        let types: Vec<_> = v["response"]["output"].as_array().unwrap().iter().map(|i| i["type"].as_str().unwrap()).collect();
        // sorted by output_index; the incomplete fallback tool call is dropped
        assert_eq!(types, vec!["function_call", "message"]);
        assert_eq!(c.completed_output(&payload), serde_json::to_vec(&v["response"]["output"]).unwrap());
    }

    #[test]
    fn partial_tool_call_in_output_is_reconciled_with_the_collected_one() {
        let mut c = OutputCollector::default();
        c.collect(&cpa_json::parse(br#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"c","name":"f","arguments":"{\"a\":1}"}}"#));
        let payload = cpa_json::parse(br#"{"type":"response.completed","response":{"output":[{"type":"function_call","call_id":"c","name":"f","arguments":""}]}}"#);
        let restored = c.restore_completion(&payload).unwrap();
        let v: Value = serde_json::from_slice(&restored).unwrap();
        assert_eq!(v["response"]["output"][0]["arguments"], "{\"a\":1}");
        // a complete output is left alone
        let same = cpa_json::parse(br#"{"type":"response.completed","response":{"output":[{"type":"message"}]}}"#);
        assert!(c.restore_completion(&same).is_none());
    }

    #[test]
    fn error_payload_shape() {
        let err = ErrorMessage::new(400, r#"{"error":{"message":"bad","type":"invalid_request_error"}}"#);
        assert_eq!(
            String::from_utf8(build_error_payload(&err)).unwrap(),
            r#"{"type":"error","status":400,"error":{"message":"bad","type":"invalid_request_error"}}"#
        );
        let plain = ErrorMessage::new(409, "conflict");
        assert_eq!(
            String::from_utf8(build_error_payload(&plain)).unwrap(),
            r#"{"type":"error","status":409,"error":{"message":"conflict","type":"invalid_request_error"}}"#
        );
    }

    #[test]
    fn upstream_error_exposure_policy() {
        assert!(should_expose_upstream_error(&ErrorMessage::new(400, "x")));
        assert!(!should_expose_upstream_error(&ErrorMessage::new(500, "x")));
        assert!(!should_expose_upstream_error(&ErrorMessage::new(429, "x")));
        let mut terminal = ErrorMessage::new(401, "x");
        terminal.terminal_auth = true;
        assert!(should_expose_upstream_error(&terminal));
    }

    #[test]
    fn message_too_big_maps_to_close_1009() {
        let err = ErrorMessage::new(413, r#"{"error":{"code":"message_too_big","message":"too large"}}"#);
        let frame = close_frame_for_upstream_error(&err).unwrap();
        assert_eq!((frame.code, frame.reason.as_str()), (1009, "too large"));
        assert!(close_frame_for_upstream_error(&ErrorMessage::new(413, "plain")).is_none());
        assert_eq!(truncate_close_reason("ééé", 3), "é");
    }

    #[test]
    fn pending_tool_calls_follow_calls_and_outputs() {
        let mut pending = BTreeSet::new();
        record_pending_tool_calls(
            &mut pending,
            &cpa_json::parse(br#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"a","name":"f","arguments":"{}"}}"#),
        );
        assert_eq!(pending.iter().collect::<Vec<_>>(), vec!["a"]);
        record_pending_tool_calls(
            &mut pending,
            &cpa_json::parse(br#"{"response":{"output":[{"type":"function_call_output","call_id":"a"}]}}"#),
        );
        assert!(pending.is_empty());
    }
}
