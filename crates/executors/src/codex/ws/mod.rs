//! Codex over the upstream Responses websocket (Go: codex_websockets_*.go).
//!
//! A client execution session owns at most one upstream connection (see [`session`]); requests on
//! a session are serialized, every request is a `response.create` frame, and continuation relies
//! on the upstream remembering the previous response on the same connection. A request flagged as
//! requiring the existing upstream socket fails with the replay-required error when there is none.

pub(crate) mod codec;
pub(crate) mod conn;
mod duplex;
mod errors;
pub(crate) mod session;
mod stream;
pub(crate) mod transport;

use std::sync::Arc;

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_auth::http::ProxySetting;
use cpa_config::Config;
use cpa_json::{J, Value};
use cpa_runtime::executor::{ExecError, Metadata, Options, Request, Response};
use cpa_translator::{Ctx, Format, Param};
use http::HeaderMap;
use tokio::sync::{OwnedMutexGuard, mpsc};

use self::conn::{Read, ReadError, WsConn};
use self::errors::{clear_replay_on_error_frame, map_read_error, map_write_error, parse_error_frame, should_retry_send};
use self::session::{Provider, Session};
use crate::helps::logging::{ApiLogHandle, UpstreamRequestLog};
use crate::helps::websocket_observer::WsFrameObserver;
use self::transport::DialFailure;
use super::headers::{WireHeaders, apply_model_header_overrides, apply_routing_hint, apply_websocket_headers, websocket_cache_headers};
use super::multi_agent_v2::restore_response;
use super::reasoning::{cache_replay_from_completed, clear_replay_on_invalid_signature};
use super::request::{Mode, Prepared, prepare, prompt_cache_id};
use super::terminal::{
    OutputItems, has_meaningful_output_delta, is_terminal_empty_incomplete, new_empty_incomplete_stream_error,
    new_status_err_with_cooling, normalize_completion, patch_completed_output, status_error, terminal_failure_err,
};
use super::{CodexExecutor, META_DOWNSTREAM_WEBSOCKET, META_REQUIRED_UPSTREAM_WEBSOCKET, creds, execution_session_id, upstream_websocket_replay_required};
use crate::helps::apply_patch::APPLY_PATCH_UPSTREAM_ERROR_MESSAGE;
use crate::helps::proxy::effective_proxy_setting;
use crate::helps::responses_usage::ensure_responses_usage_details;
use crate::helps::usage::{parse::parse_codex_usage, reporter::UsageReporter};

pub use self::session::{close_execution_session, close_sessions_for_auth_id as close_codex_websocket_sessions_for_auth_id, upstream_disconnect_receiver};

/// Everything a websocket request needs once the shared pipeline ran.
pub(super) struct WsPlan {
    pub prepared: Prepared,
    pub ws_url: String,
    pub wire: WireHeaders,
    /// Translated body with the prompt cache key (Go: clientBody).
    pub body: Vec<u8>,
    /// The `response.create` frame.
    pub frame: Vec<u8>,
    pub auth_id: String,
    pub proxy_url: String,
    pub proxy: ProxySetting,
    pub required_upstream: bool,
    pub model_level_cooling: bool,
    pub cfg: Arc<Config>,
    /// The request's log handle (a session outlives one request, the log of the request being
    /// served receives the timeline).
    pub api_log: ApiLogHandle,
    /// Handshake request details for `api.websocket.request` events (body is the frame).
    pub req_log: UpstreamRequestLog,
    /// Plugin observer of the upstream frames (`EmitWebSocketResponseEvent`).
    pub observer: Option<WsFrameObserver>,
}

impl WsPlan {
    /// `RecordAPIWebsocketRequest` of the current frame.
    pub(super) fn log_request(&self) {
        self.api_log.record_api_websocket_request(&self.cfg, &self.req_log);
    }

    /// `RecordAPIWebsocketError`.
    pub(super) fn log_error(&self, stage: &str, err: &str) {
        self.api_log.record_api_websocket_error(&self.cfg, stage, err);
    }

    /// `AppendCodexAPIWebsocketResponse`: merges quota headers carried by the frame, then logs it.
    pub(super) fn log_frame(&self, payload: &[u8]) {
        self.api_log.merge_response_headers(&crate::codex::quota::parse_codex_quota_event_headers(payload));
        self.api_log.append_api_websocket_response(&self.cfg, payload);
        if let Some(observer) = &self.observer {
            observer.emit(payload);
        }
    }

    /// `RecordAPIWebsocketUpgradeRejection` for a refused upgrade.
    fn log_upgrade_rejection(&self, status: u16, headers: &HeaderMap, body: &[u8]) {
        self.api_log.record_api_websocket_upgrade_rejection(&self.cfg, websocket_upgrade_request_log(&self.req_log), status, headers, body);
    }

    /// `recordAPIWebsocketHandshake` for a fresh connection.
    fn log_handshake(&self, headers: &HeaderMap) {
        self.api_log.record_api_websocket_handshake(&self.cfg, 101, headers);
    }
}

/// The handshake as an HTTP request (Go: websocketUpgradeRequestLog).
fn websocket_upgrade_request_log(info: &UpstreamRequestLog) -> UpstreamRequestLog {
    let mut upgrade = info.clone();
    upgrade.url = crate::helps::logging::websocket_upgrade_request_url(&info.url);
    upgrade.method = "GET".to_string();
    upgrade.body = Vec::new();
    for (name, value) in [("connection", "Upgrade"), ("upgrade", "websocket")] {
        if upgrade.headers.get(name).and_then(|v| v.to_str().ok()).is_none_or(|v| v.trim().is_empty()) {
            upgrade.headers.insert(http::HeaderName::from_static(name), http::HeaderValue::from_static(value));
        }
    }
    upgrade
}

fn metadata_flag(metadata: &Metadata, key: &str) -> bool {
    matches!(metadata.get(key), Some(Value::Bool(true)))
}

/// `ws(s)://` form of an HTTP(S) responses URL (Go: buildCodexResponsesWebsocketURL).
pub(super) fn websocket_url(http_url: &str) -> Result<String, ExecError> {
    let mut parsed = url::Url::parse(http_url.trim()).map_err(|e| ExecError::new(0, e.to_string()))?;
    let scheme = match parsed.scheme().to_lowercase().as_str() {
        "http" => "ws",
        "https" => "wss",
        other => return Err(ExecError::new(0, format!("codex websockets executor: unsupported responses websocket URL scheme \"{other}\""))),
    };
    if parsed.host_str().is_none_or(|h| h.trim().is_empty()) {
        return Err(ExecError::new(0, "codex websockets executor: responses websocket URL host is empty"));
    }
    parsed.set_scheme(scheme).map_err(|()| ExecError::new(0, "codex websockets executor: cannot set websocket scheme"))?;
    Ok(parsed.to_string())
}

/// `response.create` frame: every upstream request is a create, input item ids sanitized (Go:
/// buildCodexWebsocketRequestBody).
fn build_request_frame(body: &[u8]) -> Vec<u8> {
    if body.is_empty() {
        return Vec::new();
    }
    let body = super::input_ids::sanitize_codex_input_item_ids(body);
    let mut value = cpa_json::parse(&body);
    if cpa_json::set(&mut value, "type", "response.create") { cpa_json::to_vec(&value) } else { body }
}

impl CodexExecutor {
    /// Runs the shared pipeline and builds the handshake headers and request frame.
    pub(super) fn prepare_ws(&self, cfg: &Arc<Config>, auth: &Auth, req: &Request, opts: &Options, mode: Mode) -> Result<WsPlan, ExecError> {
        let prepared = prepare(cfg, auth, req, opts, mode)?;
        let (api_key, configured_base) = creds::codex_creds(auth);
        let ws_url = websocket_url(&format!("{}/responses", creds::base_url(&configured_base)))?;

        let cache_id = prompt_cache_id(prepared.from, req, opts, &prepared.body, false);
        let body = if cache_id.is_empty() {
            prepared.body.clone()
        } else {
            let mut value = cpa_json::parse(&prepared.body);
            if value.g("prompt_cache_key").as_str() == Some(cache_id.as_str()) {
                prepared.body.clone()
            } else {
                cpa_json::set(&mut value, "prompt_cache_key", cache_id.as_str());
                cpa_json::to_vec(&value)
            }
        };
        let session_id = self.session_id(opts, &req.payload);
        let mut headers = websocket_cache_headers(&cache_id);
        apply_websocket_headers(&mut headers, auth, &api_key, cfg, prepared.native, &opts.headers, session_id.as_deref());
        apply_routing_hint(&mut headers, auth, &prepared.base_model, &body, &opts.headers, session_id.as_deref());
        apply_model_header_overrides(&mut headers, &prepared.base_model);

        let proxy_url = crate::helps::proxy::effective_proxy_url(&opts.proxy_url, Some(auth), Some(cfg));
        let proxy = effective_proxy_setting(&opts.proxy_url, Some(auth), Some(cfg)).unwrap_or_else(|err| {
            tracing::error!("codex websockets executor: {err}");
            ProxySetting::Inherit
        });
        let frame = build_request_frame(&body);
        // Only a logged request needs the handshake details (and the frame copy).
        let req_log = if opts.api_log.get().is_some() {
            UpstreamRequestLog::from_auth("codex", Some(auth), "WEBSOCKET", &ws_url, &headers, &frame)
        } else {
            UpstreamRequestLog::default()
        };
        Ok(WsPlan {
            prepared,
            ws_url,
            wire: WireHeaders::from_map(&headers),
            body,
            frame,
            auth_id: auth.id.clone(),
            proxy_url,
            proxy,
            required_upstream: metadata_flag(&opts.metadata, META_REQUIRED_UPSTREAM_WEBSOCKET),
            model_level_cooling: cfg.codex.model_level_cooling,
            cfg: Arc::clone(cfg),
            api_log: opts.api_log.clone(),
            req_log,
            observer: WsFrameObserver::new(opts, Some(auth), "codex", &req.model),
        })
    }

    /// Non-stream request over the upstream websocket: returns on the first terminal response event.
    pub(super) async fn execute_ws(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        let cfg = self.config();
        if opts.alt == "responses/compact" {
            return self.execute_http(auth, req, opts).await;
        }
        let plan = self.prepare_ws(&cfg, auth, &req, &opts, Mode::WsExecute)?;
        let mut call = connect_and_send(&opts, &plan, false).await?;
        let prepared = &plan.prepared;

        let mut items = OutputItems::default();
        let mut saw_output_delta = false;
        loop {
            let read = call.next_read().await;
            let payload = match read {
                Some(Read::Text(payload)) => payload,
                Some(Read::Err(err)) => {
                    let mapped = map_read_error(&err);
                    plan.log_error(read_error_stage(&err), &mapped.message);
                    return Err(mapped);
                }
                None => {
                    let err = ExecError::new(0, SESSION_READ_CLOSED);
                    plan.log_error("read", &err.message);
                    return Err(err);
                }
            };
            if payload.is_empty() {
                continue;
            }
            plan.log_frame(&payload);
            let payload = restore_response(&payload, call.restore_multi_agent);
            let frame = cpa_json::parse(&payload);
            if let Some(ws_err) = parse_error_frame(&frame, plan.model_level_cooling) {
                call.invalidate("upstream_error", true);
                if let Err(replay_err) = clear_replay_on_error_frame(&prepared.replay_scope, &frame) {
                    plan.log_error("replay_clear_error", &replay_err.message);
                    return Err(replay_err);
                }
                plan.log_error("upstream_error", &ws_err.message);
                return Err(ws_err);
            }
            if let Some((stream_err, terminal_body)) = terminal_failure_err(&frame, plan.model_level_cooling) {
                call.unlock();
                call.invalidate("terminal_failure", true);
                clear_replay_on_invalid_signature(&prepared.replay_scope, stream_err.status, &terminal_body)?;
                return Err(stream_err);
            }
            let payload = normalize_completion(&payload);
            let frame = cpa_json::parse(&payload);
            let event_type = frame.g("type").str();
            if has_meaningful_output_delta(&frame) {
                saw_output_delta = true;
            }
            match event_type.as_str() {
                "response.output_item.done" => items.collect(&frame, &payload),
                "response.completed" | "response.done" | "response.incomplete" => {
                    if is_terminal_empty_incomplete(&frame, items.len(), saw_output_delta) {
                        call.invalidate("terminal_empty_incomplete", true);
                        call.unlock();
                        let err = new_empty_incomplete_stream_error();
                        plan.api_log.record_api_response_error(&plan.cfg, &err.message);
                        return Err(err);
                    }
                    let payload = patch_completed_output(&payload, &items);
                    if event_type != "response.incomplete" {
                        cache_replay_from_completed(&prepared.replay_scope, &cpa_json::parse(&payload));
                    }
                    let detail = parse_codex_usage(&payload);
                    let mut param = Param::default();
                    let out = cpa_translator::translate_non_stream(
                        &Ctx::default(),
                        prepared.to,
                        prepared.response_format,
                        &req.model,
                        &prepared.original_payload,
                        &plan.body,
                        &payload,
                        &mut param,
                    );
                    let Some(out) = out else {
                        return Err(status_error(502, APPLY_PATCH_UPSTREAM_ERROR_MESSAGE));
                    };
                    let out = if prepared.response_format == Format::OpenAIResponse { ensure_responses_usage_details(&out) } else { out };
                    let mut metadata = Metadata::new();
                    if let Some(detail) = detail {
                        metadata.insert("usage".to_string(), UsageReporter::usage_metadata(&detail));
                    }
                    return Ok(Response { payload: Bytes::from(out), metadata, headers: HeaderMap::new() });
                }
                _ => {}
            }
        }
    }
}

/// A request attached to a session connection. Dropping it stops frame routing, releases the
/// session lock and closes an ephemeral session.
pub(super) struct WsCall {
    pub sess: Arc<Session>,
    pub conn: Arc<WsConn>,
    pub rx: mpsc::Receiver<Read>,
    generation: u64,
    guard: Option<OwnedMutexGuard<()>>,
    ephemeral: bool,
    close_reason: &'static str,
    /// Handshake response headers when this request dialed.
    pub handshake_headers: HeaderMap,
    pub restore_multi_agent: bool,
}

impl WsCall {
    pub async fn next_read(&mut self) -> Option<Read> {
        self.rx.recv().await
    }

    /// Releases the session lock before the request ends (the connection is unusable or the
    /// turn is over).
    pub fn unlock(&mut self) {
        self.guard = None;
    }

    pub fn invalidate(&self, reason: &str, notify: bool) {
        self.sess.invalidate(&self.conn, reason, None, notify);
    }

    pub fn invalidate_with(&self, reason: &str, err: &ExecError, notify: bool) {
        self.sess.invalidate(&self.conn, reason, Some(&err.message), notify);
    }

    pub fn set_close_reason(&mut self, reason: &'static str) {
        self.close_reason = reason;
    }

    fn rebind(&mut self, conn: Arc<WsConn>) {
        self.sess.clear_active(self.conn.id, self.generation);
        let (generation, rx) = self.sess.activate(&conn);
        self.conn = conn;
        self.generation = generation;
        self.rx = rx;
    }
}

impl Drop for WsCall {
    fn drop(&mut self) {
        self.sess.clear_active(self.conn.id, self.generation);
        self.guard = None;
        if self.ephemeral {
            self.sess.close(self.close_reason);
        }
    }
}

/// Go's read error for a session whose channel was closed under the request.
pub(super) const SESSION_READ_CLOSED: &str = "codex websockets executor: session read channel closed";

/// Log stage of a failed read: the reader reports an unexpected binary frame as a read error,
/// Go logs it under its own stage.
pub(crate) fn read_error_stage(err: &ReadError) -> &'static str {
    match err {
        ReadError::UnexpectedBinary(_) => "unexpected_binary",
        _ => "read",
    }
}

/// Error of a failed dial, logging the rejected upgrade or the transport failure like Go's
/// Execute/ExecuteStream (the 426 and status paths log only the rejection).
fn dial_failure_error(plan: &WsPlan, failure: DialFailure) -> ExecError {
    if let Some(status) = failure.status {
        plan.log_upgrade_rejection(status, &failure.headers, &failure.body);
    }
    match failure.status {
        Some(426) => status_error(426, String::from_utf8_lossy(&failure.body).into_owned()),
        Some(status) if status > 0 => new_status_err_with_cooling(status, &failure.body, plan.model_level_cooling),
        _ => {
            plan.log_error("dial", &failure.error);
            ExecError::new(0, failure.error)
        }
    }
}

/// Attaches the request to its session connection (reusing or dialing) and writes the
/// `response.create` frame, retrying once on a fresh connection when a persistent session's
/// first send fails (Go: the connect/send prologue of Execute and ExecuteStream).
pub(super) async fn connect_and_send(opts: &Options, plan: &WsPlan, stream: bool) -> Result<WsCall, ExecError> {
    let session_id = execution_session_id(opts);
    let (sess, ephemeral) = match Session::get_or_create(Provider::Codex, &session_id) {
        Some(s) => (s, false),
        None => (Session::ephemeral(Provider::Codex), true),
    };
    let guard = if ephemeral { None } else { Some(Arc::clone(&sess.req_mu).lock_owned().await) };
    plan.log_request();

    let (conn, handshake) = if plan.required_upstream {
        match sess.existing_conn(&plan.auth_id, &plan.ws_url, &plan.proxy_url) {
            Some(conn) => (conn, None),
            None => return Err(upstream_websocket_replay_required()),
        }
    } else {
        let dial = || transport::dial(&plan.ws_url, &plan.wire, &plan.proxy);
        match sess.ensure_conn(&plan.auth_id, &plan.ws_url, &plan.proxy_url, dial).await {
            Ok(ok) => ok,
            Err(failure) => return Err(dial_failure_error(plan, failure)),
        }
    };
    if let Err(message) = sess.bind_execution_lifecycle(opts.lifecycle.as_ref(), &conn) {
        drop(guard);
        close_after_bind_failure(&sess, &conn);
        return Err(ExecError::new(0, message));
    }
    if let Some(headers) = &handshake {
        plan.log_handshake(headers);
    }
    let (generation, rx) = sess.activate(&conn);
    let mut call = WsCall {
        sess: Arc::clone(&sess),
        conn,
        rx,
        generation,
        guard,
        ephemeral,
        close_reason: "completed",
        handshake_headers: handshake.unwrap_or_default(),
        restore_multi_agent: false,
    };
    call.restore_multi_agent = !plan.prepared.multi_agent_v2_conflict && (plan.prepared.optimize_multi_agent_v2 || sess.is_multi_agent_v2_optimized(&call.conn));

    if let Err(text) = call.conn.write_text(plan.frame.clone()).await {
        let mapped = map_write_error(text, sess.upstream_close(&call.conn));
        // ExecuteStream logs the send error up front, Execute only where the request ends on it.
        if stream {
            plan.log_error("send", &mapped.message);
        }
        if ephemeral {
            call.sess.invalidate(&call.conn, "send_error", Some(&mapped.message), true);
            call.set_close_reason("send_error");
            if !stream {
                plan.log_error("send", &mapped.message);
            }
            return Err(mapped);
        }
        if plan.required_upstream {
            call.sess.invalidate(&call.conn, "send_error", Some(&mapped.message), false);
            if should_retry_send(&mapped) {
                return Err(upstream_websocket_replay_required());
            }
            if !stream {
                plan.log_error("send", &mapped.message);
            }
            return Err(mapped);
        }
        call.sess.invalidate(&call.conn, "send_error", Some(&mapped.message), true);
        if !should_retry_send(&mapped) {
            if !stream {
                plan.log_error("send", &mapped.message);
            }
            return Err(mapped);
        }
        // The upstream may have closed the socket between sequential requests of this session.
        let dial = || transport::dial(&plan.ws_url, &plan.wire, &plan.proxy);
        let (retry_conn, retry_handshake) = match sess.ensure_conn(&plan.auth_id, &plan.ws_url, &plan.proxy_url, dial).await {
            Ok(ok) => ok,
            Err(failure) => {
                plan.log_error("dial_retry", &failure.error);
                return Err(ExecError::new(0, failure.error));
            }
        };
        if let Err(message) = sess.bind_execution_lifecycle(opts.lifecycle.as_ref(), &retry_conn) {
            close_after_bind_failure(&sess, &retry_conn);
            return Err(ExecError::new(0, message));
        }
        call.rebind(retry_conn);
        plan.log_request();
        if let Some(headers) = &retry_handshake {
            plan.log_handshake(headers);
        }
        call.handshake_headers = retry_handshake.unwrap_or_default();
        call.restore_multi_agent = !plan.prepared.multi_agent_v2_conflict && (plan.prepared.optimize_multi_agent_v2 || sess.is_multi_agent_v2_optimized(&call.conn));
        if let Err(text) = call.conn.write_text(plan.frame.clone()).await {
            let mapped = map_write_error(text, sess.upstream_close(&call.conn));
            call.sess.invalidate(&call.conn, "send_error", Some(&mapped.message), true);
            plan.log_error("send_retry", &mapped.message);
            return Err(mapped);
        }
    }
    if plan.prepared.optimize_multi_agent_v2 || plan.prepared.multi_agent_v2_conflict {
        sess.set_multi_agent_v2_optimized(&call.conn, plan.prepared.optimize_multi_agent_v2 && !plan.prepared.multi_agent_v2_conflict);
    }
    Ok(call)
}

/// Drops a connection whose lifecycle bind failed (Go: `closeWebsocketAfterBindFailure`).
pub(crate) fn close_after_bind_failure(sess: &Arc<Session>, conn: &Arc<WsConn>) {
    sess.invalidate(conn, "lifecycle_bind_failed", None, false);
}

/// `downstream_websocket` flag of a request.
pub(super) fn is_downstream_websocket(opts: &Options) -> bool {
    metadata_flag(&opts.metadata, META_DOWNSTREAM_WEBSOCKET)
}
