//! xAI over the upstream Responses websocket (Go: xai_websockets_executor.go,
//! `XAIWebsocketsExecutor` and the routing of `XAIAutoExecutor`).
//!
//! The websocket is used only when the client itself is on a Responses websocket and the
//! credential enables `websockets`; everything else takes the HTTP path. A client execution
//! session owns at most one upstream connection ([`session`]); requests on a session are
//! serialized and every request is a `response.create` frame. Because a new upstream
//! connection forgets earlier responses, [`ids`] keeps the client-visible response chain
//! monotone by dropping unknown `previous_response_id`s and replaying the recorded transcript.
//!
//! The transport pieces (`conn`, `session`, `transport`) are the Codex port's, copied because
//! that port keeps them private; `codec` is compiled from the Codex source file unchanged.

mod compact;
mod conn;
mod errors;
mod ids;
mod session;
mod stream;
mod transport;

// The frame codec is shared with the Codex websocket port without editing its module tree.
#[path = "../../codex/ws/codec.rs"]
mod codec;

use std::sync::Arc;

use cpa_auth::Auth;
use cpa_auth::http::ProxySetting;
use cpa_auth::xai::DEFAULT_API_BASE_URL;
use cpa_config::Config;
use cpa_json::J;
use cpa_runtime::executor::{ExecError, Options, Request, StreamResult};
use http::header::{AUTHORIZATION, CONTENT_TYPE};
use http::{HeaderMap, HeaderName, HeaderValue};
use tokio::sync::{OwnedMutexGuard, mpsc};

use self::conn::{Read, WsConn};
use self::errors::{map_write_error, should_retry_send};
use self::ids::RequestIdMapper;
use self::session::Session;
use self::transport::DialFailure;
use super::XaiExecutor;
use super::request::{
    IDENTIFIER, PreparedRequest, apply_custom_headers, creds, execution_session_id, prepare_responses_request,
};
use super::response::status_err_for_body;
use crate::codex::{META_DOWNSTREAM_WEBSOCKET, META_REQUIRED_UPSTREAM_WEBSOCKET, upstream_websocket_replay_required};
use crate::helps::logging::{UpstreamRequestLog, websocket_upgrade_request_url};
use crate::helps::proxy::{effective_proxy_setting, effective_proxy_url};
use crate::helps::session::ensure_session_id;
use crate::helps::status::status_err;
use crate::helps::usage::UsageReporter;

pub use self::session::{
    close_execution_session, close_sessions_for_auth_id as close_xai_websocket_sessions_for_auth_id,
    upstream_disconnect_receiver,
};

/// Handshake headers as written on the wire: canonical `Title-Case` names like Go's `http.Header`.
#[derive(Debug, Clone, Default)]
pub struct WireHeaders(pub Vec<(String, String)>);

impl WireHeaders {
    fn from_map(headers: &HeaderMap) -> Self {
        let mut out = Vec::with_capacity(headers.len());
        for (name, value) in headers {
            let Ok(value) = value.to_str() else { continue };
            let mut canonical = String::with_capacity(name.as_str().len());
            let mut upper = true;
            for c in name.as_str().chars() {
                canonical.push(if upper { c.to_ascii_uppercase() } else { c });
                upper = c == '-';
            }
            out.push((canonical, value.to_string()));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        WireHeaders(out)
    }
}

fn metadata_flag(opts: &Options, key: &str) -> bool {
    matches!(opts.metadata.get(key), Some(serde_json::Value::Bool(true)))
}

/// Whether the client is on a Responses websocket (Go: `DownstreamWebsocket(ctx)`).
pub(super) fn is_downstream_websocket(opts: &Options) -> bool {
    metadata_flag(opts, META_DOWNSTREAM_WEBSOCKET)
}

/// Whether the request must continue on the session's live upstream socket (Go:
/// `RequiredUpstreamWebsocket(ctx)`).
pub(super) fn requires_upstream_websocket(opts: &Options) -> bool {
    metadata_flag(opts, META_REQUIRED_UPSTREAM_WEBSOCKET)
}

/// Go: executionSessionIDFromOptions.
fn execution_session_id_from_options(opts: &Options) -> String {
    match opts.metadata.get(cpa_runtime::executor::meta::EXECUTION_SESSION_ID) {
        Some(serde_json::Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// Go: xaiWebsocketsEnabled. Attribute `websockets` first, then metadata (bool or string).
pub fn websockets_enabled(auth: &Auth) -> bool {
    if let Some(raw) = auth.attributes.get("websockets").map(|v| v.trim())
        && !raw.is_empty()
        && let Some(parsed) = parse_bool(raw)
    {
        return parsed;
    }
    match auth.metadata.get("websockets") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => parse_bool(s.trim()).unwrap_or(false),
        _ => false,
    }
}

/// Go: buildXAIResponsesWebsocketURL. `http(s)` maps to `ws(s)`; `ws(s)` is kept.
pub(super) fn build_responses_websocket_url(http_url: &str) -> Result<String, ExecError> {
    let mut parsed = url::Url::parse(http_url.trim()).map_err(|e| ExecError::new(0, e.to_string()))?;
    let scheme = match parsed.scheme().to_lowercase().as_str() {
        "http" => Some("ws"),
        "https" => Some("wss"),
        "ws" | "wss" => None,
        other => {
            return Err(ExecError::new(
                0,
                format!("xai websockets executor: unsupported responses websocket URL scheme \"{other}\""),
            ));
        }
    };
    if parsed.host_str().is_none_or(|h| h.trim().is_empty()) {
        return Err(ExecError::new(0, "xai websockets executor: responses websocket URL host is empty"));
    }
    if let Some(scheme) = scheme {
        parsed.set_scheme(scheme).map_err(|()| ExecError::new(0, "xai websockets executor: cannot set websocket scheme"))?;
    }
    Ok(parsed.to_string())
}

/// Go: applyXAIWebsocketHeaders. No CLI identity headers: the websocket never uses the chat proxy.
fn websocket_headers(auth: &Auth, token: &str, session_id: &str, opts: &Options, session: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if !token.trim().is_empty()
        && let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}"))
    {
        headers.insert(AUTHORIZATION, value);
    }
    if !session_id.is_empty()
        && let Ok(value) = HeaderValue::from_str(session_id)
    {
        headers.insert(HeaderName::from_static("x-grok-conv-id"), value);
    }
    apply_custom_headers(&mut headers, Some(auth), opts, session);
    headers
}

/// The `response.create` frame for a prepared body (Go: buildXAIWebsocketRequestBody): always a
/// create, stored, no streaming controls, and no instructions when continuing a response.
pub(super) fn build_request_body(body: &[u8]) -> Vec<u8> {
    if body.is_empty() {
        return Vec::new();
    }
    let mut value = cpa_json::parse(body);
    cpa_json::set(&mut value, "type", "response.create");
    cpa_json::delete(&mut value, "stream");
    cpa_json::delete(&mut value, "stream_options");
    cpa_json::delete(&mut value, "background");
    cpa_json::set(&mut value, "store", true);
    if !value.g("previous_response_id").str().trim().is_empty() {
        cpa_json::delete(&mut value, "instructions");
    }
    cpa_json::to_vec(&value)
}

/// Go: xaiWebsocketGenerateFalse. A warmup request primes the upstream without generating.
pub(super) fn generate_false(frame: &[u8]) -> bool {
    let parsed = cpa_json::parse(frame);
    let generate = parsed.g("generate");
    generate.exists() && !generate.bool()
}

/// Go: websocketUpgradeRequestLog. The handshake as an HTTP GET for the request log.
fn upgrade_request_log(info: &UpstreamRequestLog) -> UpstreamRequestLog {
    let mut upgrade = info.clone();
    upgrade.url = websocket_upgrade_request_url(&info.url);
    upgrade.method = "GET".to_string();
    upgrade.body = Vec::new();
    let blank = |headers: &HeaderMap, name: &str| headers.get(name).and_then(|v| v.to_str().ok()).is_none_or(|v| v.trim().is_empty());
    if blank(&upgrade.headers, "connection") {
        upgrade.headers.insert(HeaderName::from_static("connection"), HeaderValue::from_static("Upgrade"));
    }
    if blank(&upgrade.headers, "upgrade") {
        upgrade.headers.insert(HeaderName::from_static("upgrade"), HeaderValue::from_static("websocket"));
    }
    upgrade
}

/// Everything a websocket request needs once the shared pipeline ran.
pub(super) struct WsPlan {
    pub prepared: PreparedRequest,
    pub ws_url: String,
    pub headers: HeaderMap,
    pub wire: WireHeaders,
    pub auth_id: String,
    pub proxy_url: String,
    pub proxy: ProxySetting,
    pub required_upstream: bool,
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
}

impl WsCall {
    pub async fn next_read(&mut self) -> Option<Read> {
        self.rx.recv().await
    }

    /// Invalidates the connection (Go: invalidateUpstreamConn, `notify` mirrors the variant
    /// without the disconnect notification).
    pub fn invalidate(&self, reason: &str, err: Option<&str>, notify: bool) {
        self.sess.invalidate(&self.conn, reason, err, notify);
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

/// A rejected or failed dial as the executor error (Go: `xaiStatusErr` for a response with a
/// status, else the dial error).
fn dial_failure_error(failure: &DialFailure) -> ExecError {
    match failure.status {
        Some(status) if status > 0 => status_err_for_body(status, &failure.body),
        _ => ExecError::new(0, failure.error.clone()),
    }
}

/// The upstream session an execution attaches to: the stored session with its request lock held,
/// or an ephemeral one.
struct Attached {
    sess: Arc<Session>,
    ephemeral: bool,
    guard: Option<OwnedMutexGuard<()>>,
}

async fn attach_session(execution_session_id: &str) -> Attached {
    match Session::get_or_create(execution_session_id) {
        Some(sess) => {
            let guard = Arc::clone(&sess.req_mu).lock_owned().await;
            Attached { sess, ephemeral: false, guard: Some(guard) }
        }
        None => Attached { sess: Session::ephemeral(), ephemeral: true, guard: None },
    }
}

fn log_request_sent(session_id: &str, auth_id: &str, ws_url: &str, frame: &[u8]) {
    if frame.is_empty() {
        tracing::info!("xai websockets: upstream request sent session={} auth={} url={}", session_id.trim(), auth_id.trim(), ws_url.trim());
        return;
    }
    let parsed = cpa_json::parse(frame);
    let generate = parsed.g("generate");
    let generate = if generate.exists() { generate.raw().trim().to_string() } else { "default".to_string() };
    tracing::info!(
        "xai websockets: upstream request sent session={} auth={} url={} event={} previous_response_id={} generate={} input_items={}",
        session_id.trim(),
        auth_id.trim(),
        ws_url.trim(),
        parsed.g("type").str().trim(),
        parsed.g("previous_response_id").str().trim(),
        generate,
        parsed.g("input").array().len()
    );
}

/// Outcome of attaching a request to its upstream connection.
pub(super) struct Connected {
    pub call: WsCall,
    /// The frame that was finally sent (the retry rebuilds it).
    pub frame: Vec<u8>,
}

impl XaiExecutor {
    /// Runs the shared pipeline for a websocket request and builds the handshake headers.
    fn prepare_ws_plan(&self, cfg: &Config, auth: &Auth, req: &Request, opts: &Options) -> Result<WsPlan, ExecError> {
        let (token, mut base_url) = creds(Some(auth));
        if base_url.is_empty() {
            base_url = DEFAULT_API_BASE_URL.to_string();
        }
        let mut prepared = prepare_responses_request(cfg, req, opts, true)?;
        // Go: prepareResponsesWebsocketRequest keeps the client's previous_response_id.
        let previous = cpa_json::parse(&req.payload).g("previous_response_id").str().trim().to_string();
        if !previous.is_empty() {
            let mut body = cpa_json::parse(&prepared.body);
            cpa_json::set(&mut body, "previous_response_id", previous);
            prepared.body = cpa_json::to_vec(&body);
        }
        let http_url = format!("{}/responses", base_url.trim_end_matches('/'));
        let ws_url = build_responses_websocket_url(&http_url)?;
        let session = ensure_session_id(None, "", opts, &req.payload);
        let headers = websocket_headers(auth, &token, &prepared.session_id, opts, session.as_deref());
        let proxy_url = effective_proxy_url(&opts.proxy_url, Some(auth), Some(cfg));
        let proxy = effective_proxy_setting(&opts.proxy_url, Some(auth), Some(cfg)).unwrap_or_else(|err| {
            tracing::error!("xai websockets executor: {err}");
            ProxySetting::Inherit
        });
        let plan = WsPlan {
            ws_url,
            wire: WireHeaders::from_map(&headers),
            headers,
            auth_id: auth.id.clone(),
            proxy_url,
            proxy,
            required_upstream: requires_upstream_websocket(opts),
            prepared,
        };
        Ok(plan)
    }

    /// Attaches the request to its session connection (reusing or dialing) and writes the
    /// `response.create` frame, retrying once on a fresh connection when a persistent session's
    /// first send fails (Go: the connect/send prologue of ExecuteStream). The websocket
    /// request, handshake and errors are recorded in the request log along the way.
    #[allow(clippy::too_many_arguments)]
    async fn connect_and_send(
        &self,
        cfg: &Config,
        opts: &Options,
        plan: &WsPlan,
        attached: Attached,
        frame: Vec<u8>,
        log_info: &UpstreamRequestLog,
        reporter: &UsageReporter,
    ) -> Result<Connected, ExecError> {
        let Attached { sess, ephemeral, guard } = attached;
        let log = &opts.api_log;
        let dial = || transport::dial(&plan.ws_url, &plan.wire, &plan.proxy);

        let (conn, handshake) = if plan.required_upstream {
            match sess.existing_conn(&plan.auth_id, &plan.ws_url, &plan.proxy_url) {
                Some(conn) => (conn, None),
                None => return Err(upstream_websocket_replay_required()),
            }
        } else {
            match sess.ensure_conn(&plan.auth_id, &plan.ws_url, &plan.proxy_url, dial).await {
                Ok(ok) => ok,
                Err(failure) => {
                    if let Some(status) = failure.status {
                        log.record_api_websocket_upgrade_rejection(
                            cfg,
                            upgrade_request_log(log_info),
                            status,
                            &failure.headers,
                            &failure.body,
                        );
                        if status > 0 {
                            return Err(dial_failure_error(&failure));
                        }
                    }
                    log.record_api_websocket_error(cfg, "dial", &failure.error);
                    return Err(ExecError::new(0, failure.error));
                }
            }
        };
        if let Some(headers) = &handshake {
            log.record_api_websocket_handshake(cfg, 101, headers);
        }
        reporter.start_response_ttft();
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
        };

        let mut frame = frame;
        if let Err(text) = call.conn.write_text(frame.clone()).await {
            let mapped = map_write_error(text, sess.upstream_close(&call.conn));
            log.record_api_websocket_error(cfg, "send", &mapped.message);
            if ephemeral {
                call.invalidate("send_error", Some(&mapped.message), true);
                call.set_close_reason("send_error");
                return Err(mapped);
            }
            if plan.required_upstream {
                call.invalidate("send_error", Some(&mapped.message), false);
                return Err(if should_retry_send(&mapped) { upstream_websocket_replay_required() } else { mapped });
            }
            call.invalidate("send_error", Some(&mapped.message), true);
            if !should_retry_send(&mapped) {
                return Err(mapped);
            }
            // The upstream may have closed the socket between sequential requests of this session.
            let (retry_conn, retry_handshake) = match sess.ensure_conn(&plan.auth_id, &plan.ws_url, &plan.proxy_url, dial).await {
                Ok(ok) => ok,
                Err(failure) => {
                    log.record_api_websocket_error(cfg, "dial_retry", &failure.error);
                    return Err(dial_failure_error(&failure));
                }
            };
            call.rebind(retry_conn);
            call.handshake_headers = retry_handshake.clone().unwrap_or_default();
            let retry_frame = build_request_body(&plan.prepared.body);
            log.record_api_websocket_request(cfg, &UpstreamRequestLog { body: retry_frame.clone(), ..log_info.clone() });
            log_request_sent(&execution_session_id_from_options(opts), &plan.auth_id, &plan.ws_url, &retry_frame);
            if let Some(headers) = &retry_handshake {
                log.record_api_websocket_handshake(cfg, 101, headers);
            }
            reporter.start_response_ttft();
            if let Err(text) = call.conn.write_text(retry_frame.clone()).await {
                let mapped = map_write_error(text, sess.upstream_close(&call.conn));
                log.record_api_websocket_error(cfg, "send_retry", &mapped.message);
                call.invalidate("send_error", Some(&mapped.message), true);
                return Err(mapped);
            }
            frame = retry_frame;
        }
        Ok(Connected { call, frame })
    }

    /// Go: XAIWebsocketsExecutor.ExecuteStream. Streams one request over the upstream websocket.
    pub(super) async fn execute_stream_ws(&self, auth: &Auth, req: &Request, opts: &Options) -> Result<StreamResult, ExecError> {
        if opts.alt == "responses/compact" {
            return Err(status_err(400, "streaming not supported for /responses/compact"));
        }
        let cfg = self.config();
        let execution_session = execution_session_id_from_options(opts);
        let mut state_session = execution_session_id(req, opts);
        if state_session.is_empty() {
            state_session = execution_session.clone();
        }
        let state = ids::get_state(&state_session);
        // Requests without a websocket session of their own are serialized per id state; the
        // lock moves into the stream task.
        let state_guard = match (&state, execution_session.is_empty()) {
            (Some(state), true) => Some(Arc::clone(&state.request_mu).lock_owned().await),
            _ => None,
        };

        if super::execute::input_has_item_type(&req.payload, "compaction_trigger") {
            if requires_upstream_websocket(opts) {
                return Err(upstream_websocket_replay_required());
            }
            let _session_guard = match Session::get_or_create(&execution_session) {
                Some(sess) => Some(Arc::clone(&sess.req_mu).lock_owned().await),
                None => None,
            };
            let mapper = RequestIdMapper::new(&state_session, &req.payload);
            return self.execute_compaction_trigger_from_websocket(auth, req, opts, mapper).await;
        }

        let plan = self.prepare_ws_plan(&cfg, auth, req, opts)?;
        let reporter = self.new_reporter(&plan.prepared.base_model, auth, opts);
        let result = self
            .stream_ws_prepared(&cfg, auth, req, opts, plan, &execution_session, &state_session, state_guard, &reporter)
            .await;
        reporter.track_failure(&result);
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn stream_ws_prepared(
        &self,
        cfg: &Arc<Config>,
        auth: &Auth,
        req: &Request,
        opts: &Options,
        mut plan: WsPlan,
        execution_session: &str,
        state_session: &str,
        state_guard: Option<OwnedMutexGuard<()>>,
        reporter: &UsageReporter,
    ) -> Result<StreamResult, ExecError> {
        let ws_url = plan.ws_url.clone();
        let attached = attach_session(execution_session).await;
        let mut mapper = RequestIdMapper::new(state_session, &req.payload);
        if let Some(mapper) = mapper.as_mut() {
            if attached.sess.target_changed(&plan.auth_id, &ws_url, &plan.proxy_url) {
                mapper.upstream_previous_id.clear();
            }
            plan.prepared.body = mapper.upstream_request_payload(std::mem::take(&mut plan.prepared.body));
        }
        reporter.set_translated_reasoning_effort(&plan.prepared.body, IDENTIFIER);

        let frame = build_request_body(&plan.prepared.body);
        let request_type = cpa_json::parse(&req.payload).g("type").str().trim().to_string();
        let transcript_reset = cpa_json::parse(&frame).g("previous_response_id").str().trim().is_empty()
            && (request_type != "response.append" || mapper.as_ref().is_some_and(|m| m.replayed_compacted_transcript));
        let warmup = generate_false(&frame);

        let (auth_type, auth_value) = auth.account_info();
        let log_info = UpstreamRequestLog {
            url: ws_url.clone(),
            method: "WEBSOCKET".to_string(),
            headers: plan.headers.clone(),
            body: frame.clone(),
            provider: IDENTIFIER.to_string(),
            auth_id: auth.id.clone(),
            auth_label: auth.label.clone(),
            auth_type: auth_type.to_string(),
            auth_value,
        };
        opts.api_log.record_api_websocket_request(cfg, &log_info);
        log_request_sent(execution_session, &plan.auth_id, &ws_url, &frame);

        let Connected { call, frame } = self.connect_and_send(cfg, opts, &plan, attached, frame, &log_info, reporter).await?;
        let handshake_headers = call.handshake_headers.clone();
        Ok(stream::spawn(stream::Start {
            cfg: Arc::clone(cfg),
            api_log: opts.api_log.clone(),
            downstream_ws: is_downstream_websocket(opts),
            req_model: req.model.clone(),
            prepared: plan.prepared,
            call,
            frame,
            mapper,
            warmup,
            transcript_reset,
            execution_session: execution_session.to_string(),
            ws_url,
            auth_id: plan.auth_id,
            reporter: reporter.clone(),
            state_guard,
            headers: handshake_headers,
        }))
    }
}
