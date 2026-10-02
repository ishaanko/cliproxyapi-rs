//! Websocket relay for AI Studio (Go: internal/wsrelay).
//!
//! A browser page driven by a userscript connects to `/v1/ws` and acts as an HTTP client: the
//! proxy sends it `http_request` envelopes and the page relays the responses back, correlated by
//! message id. The relay is transport agnostic: the HTTP layer upgrades the socket and hands
//! [`Manager::attach`] a frame sink and a frame stream, so this module has no web-server
//! dependency.
//!
//! Wire format: JSON text frames `{"id","type","payload"}` with types `http_request`,
//! `http_response`, `stream_start`, `stream_chunk`, `stream_end`, `error`, `ping`, `pong`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use http::{HeaderMap, HeaderName, HeaderValue};
use parking_lot::{Mutex, RwLock};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, mpsc, watch};

pub const MESSAGE_TYPE_HTTP_REQ: &str = "http_request";
pub const MESSAGE_TYPE_HTTP_RESP: &str = "http_response";
pub const MESSAGE_TYPE_STREAM_START: &str = "stream_start";
pub const MESSAGE_TYPE_STREAM_CHUNK: &str = "stream_chunk";
pub const MESSAGE_TYPE_STREAM_END: &str = "stream_end";
pub const MESSAGE_TYPE_ERROR: &str = "error";
pub const MESSAGE_TYPE_PING: &str = "ping";
pub const MESSAGE_TYPE_PONG: &str = "pong";

/// A socket that sends nothing (not even a pong) for this long is dropped.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Messages buffered per request before the read loop blocks on a slow consumer.
const PENDING_BUFFER: usize = 64;
const CLOSED_CAUSE: &str = "websocket session closed";

/// JSON envelope exchanged with the page.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default, skip_serializing_if = "payload_is_empty")]
    pub payload: Option<Map<String, Value>>,
}

fn payload_is_empty(payload: &Option<Map<String, Value>>) -> bool {
    payload.as_ref().is_none_or(Map::is_empty)
}

impl Message {
    fn is_terminal(&self) -> bool {
        matches!(self.kind.as_str(), MESSAGE_TYPE_HTTP_RESP | MESSAGE_TYPE_ERROR | MESSAGE_TYPE_STREAM_END)
    }
}

/// A frame read from the socket.
#[derive(Debug)]
pub enum Inbound {
    Text(String),
    /// A websocket-level pong; the only frame that extends the read deadline.
    Pong,
}

/// A frame to write to the socket.
#[derive(Debug)]
pub enum Outbound {
    Text(String),
    /// A websocket-level ping with payload `ping`.
    Ping,
}

/// Proxied HTTP request delivered to the page.
#[derive(Debug, Clone, Default)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    /// Canonical header names mapped to their values.
    pub headers: BTreeMap<String, Vec<String>>,
    pub body: Vec<u8>,
}

/// Response relayed back by the page.
#[derive(Debug, Clone, Default)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

/// One event of a streaming response.
#[derive(Debug, Clone, Default)]
pub struct StreamEvent {
    /// One of the `MESSAGE_TYPE_*` constants (empty for the synthetic "stream closed" error).
    pub kind: String,
    pub payload: Vec<u8>,
    pub status: u16,
    pub headers: HeaderMap,
    pub err: Option<String>,
}

/// A relay failure; `attempted` is false when the request never reached the socket (so the
/// conductor may retry it elsewhere as if nothing was sent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayError {
    pub message: String,
    pub attempted: bool,
}

impl RelayError {
    fn not_sent(message: impl Into<String>) -> Self {
        Self { message: message.into(), attempted: false }
    }

    fn sent(message: impl Into<String>) -> Self {
        Self { message: message.into(), attempted: true }
    }
}

impl std::fmt::Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

type ConnectedHook = Arc<dyn Fn(&str) + Send + Sync>;
type DisconnectedHook = Arc<dyn Fn(&str, &str) + Send + Sync>;

#[derive(Default)]
struct Hooks {
    on_connected: Option<ConnectedHook>,
    on_disconnected: Option<DisconnectedHook>,
}

/// The relay: one session per connected page, addressed by provider name (the auth id).
pub struct Manager {
    path: String,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    hooks: RwLock<Hooks>,
}

/// Resolves once the session is closed. A helper because `watch::Ref` is not `Send` and must not
/// appear in a `select!` arm of a spawned task.
async fn wait_closed(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|closed| *closed).await;
}

static GLOBAL: OnceLock<Arc<Manager>> = OnceLock::new();

/// The process-wide relay used by the AI Studio executor and the `/v1/ws` route.
pub fn global() -> Arc<Manager> {
    GLOBAL.get_or_init(|| Arc::new(Manager::new(""))).clone()
}

impl Manager {
    /// `path` is the route the HTTP layer mounts (default `/v1/ws`).
    pub fn new(path: &str) -> Self {
        let path = path.trim();
        let path = if path.is_empty() {
            "/v1/ws".to_string()
        } else if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        Manager { path, sessions: Mutex::new(HashMap::new()), hooks: RwLock::new(Hooks::default()) }
    }

    /// HTTP path the relay expects websocket upgrades on.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Installs the connect and disconnect callbacks (`on_disconnected` gets the cause text).
    pub fn set_hooks(
        &self,
        on_connected: Option<impl Fn(&str) + Send + Sync + 'static>,
        on_disconnected: Option<impl Fn(&str, &str) + Send + Sync + 'static>,
    ) {
        let mut hooks = self.hooks.write();
        hooks.on_connected = on_connected.map(|f| Arc::new(f) as ConnectedHook);
        hooks.on_disconnected = on_disconnected.map(|f| Arc::new(f) as DisconnectedHook);
    }

    /// Names of the currently connected sessions.
    pub fn connected(&self) -> Vec<String> {
        self.sessions.lock().keys().cloned().collect()
    }

    /// Registers a freshly upgraded socket and runs it until it closes. `outbound` receives the
    /// frames to write; `inbound` yields the frames read (an `Err` or the end of the stream ends
    /// the session). Returns the session's provider name (`aistudio-<16 chars>`).
    pub fn attach(
        self: &Arc<Self>,
        outbound: mpsc::Sender<Outbound>,
        inbound: impl Stream<Item = Result<Inbound, String>> + Send + 'static,
    ) -> String {
        let provider = random_provider_name();
        let (closed_tx, _) = watch::channel(false);
        let session = Arc::new(Session {
            provider: provider.clone(),
            outbound,
            pending: Mutex::new(HashMap::new()),
            closed: closed_tx,
            cleaned: AtomicBool::new(false),
            manager: Arc::downgrade(self),
        });
        let replaced = self.sessions.lock().insert(provider.clone(), session.clone());
        if let Some(old) = replaced {
            old.cleanup("replaced by new connection");
        }
        let connected = self.hooks.read().on_connected.clone();
        if let Some(hook) = connected {
            hook(&provider);
        }
        tokio::spawn(session.clone().heartbeat());
        tokio::spawn(session.run(Box::pin(inbound)));
        provider
    }

    /// Closes every session (Go: Manager.Stop).
    pub fn stop(&self) {
        let sessions: Vec<_> = self.sessions.lock().drain().map(|(_, s)| s).collect();
        for session in sessions {
            session.cleanup("wsrelay: manager stopped");
        }
    }

    fn session(&self, provider: &str) -> Option<Arc<Session>> {
        self.sessions.lock().get(provider.trim().to_lowercase().as_str()).cloned()
    }

    fn session_closed(&self, session: &Arc<Session>, cause: &str) {
        {
            let mut sessions = self.sessions.lock();
            if sessions.get(&session.provider).is_some_and(|cur| Arc::ptr_eq(cur, session)) {
                sessions.remove(&session.provider);
            }
        }
        let hook = self.hooks.read().on_disconnected.clone();
        if let Some(hook) = hook {
            hook(&session.provider, cause);
        }
    }

    /// Sends `msg` to the provider's page and returns the stream of responses for its id.
    pub async fn send(&self, provider: &str, msg: Message) -> Result<PendingRx, RelayError> {
        match self.session(provider) {
            Some(session) => session.request(msg).await,
            None => Err(RelayError::not_sent(format!("wsrelay: provider {provider} not connected"))),
        }
    }

    /// Runs a request and returns the complete response; a streamed reply is aggregated. Drop the
    /// future to cancel (Go: context cancellation).
    pub async fn non_stream(&self, provider: &str, req: &HttpRequest) -> Result<HttpResponse, RelayError> {
        let msg = new_request_message(req);
        let mut rx = self.send(provider, msg).await?;
        let mut stream_mode = false;
        let mut stream_resp: Option<HttpResponse> = None;
        let mut stream_body: Vec<u8> = Vec::new();
        loop {
            let Some(msg) = rx.recv().await else {
                if stream_mode {
                    let mut resp = stream_resp.unwrap_or_else(ok_response);
                    resp.body = stream_body;
                    return Ok(resp);
                }
                return Err(RelayError::sent("wsrelay: connection closed during response"));
            };
            match msg.kind.as_str() {
                MESSAGE_TYPE_HTTP_RESP => {
                    let mut resp = decode_response(msg.payload.as_ref());
                    if stream_mode && !stream_body.is_empty() && resp.body.is_empty() {
                        resp.body = stream_body;
                    }
                    return Ok(resp);
                }
                MESSAGE_TYPE_ERROR => return Err(RelayError::sent(decode_error(msg.payload.as_ref()))),
                MESSAGE_TYPE_STREAM_START => {
                    stream_mode = true;
                    stream_resp = Some(decode_response(msg.payload.as_ref()));
                    stream_body.clear();
                }
                MESSAGE_TYPE_STREAM_CHUNK => {
                    if !stream_mode {
                        stream_mode = true;
                        stream_resp = Some(ok_response());
                    }
                    stream_body.extend_from_slice(&decode_chunk(msg.payload.as_ref()));
                }
                MESSAGE_TYPE_STREAM_END => {
                    if !stream_mode {
                        return Ok(ok_response());
                    }
                    let mut resp = stream_resp.unwrap_or_else(ok_response);
                    resp.body = stream_body;
                    return Ok(resp);
                }
                _ => {}
            }
        }
    }

    /// Runs a request and yields its events. Dropping the receiver cancels the request.
    pub async fn stream(&self, provider: &str, req: &HttpRequest) -> Result<mpsc::Receiver<StreamEvent>, RelayError> {
        let msg = new_request_message(req);
        let mut rx = self.send(provider, msg).await?;
        let (tx, out) = mpsc::channel(1);
        tokio::spawn(async move {
            loop {
                let msg = tokio::select! {
                    _ = tx.closed() => return,
                    msg = rx.recv() => msg,
                };
                let Some(msg) = msg else {
                    let _ =
                        tx.send(StreamEvent { err: Some("wsrelay: stream closed".into()), ..Default::default() }).await;
                    return;
                };
                let (event, done) = match msg.kind.as_str() {
                    MESSAGE_TYPE_STREAM_START => {
                        let resp = decode_response(msg.payload.as_ref());
                        (
                            StreamEvent {
                                kind: msg.kind,
                                status: resp.status,
                                headers: resp.headers,
                                ..Default::default()
                            },
                            false,
                        )
                    }
                    MESSAGE_TYPE_STREAM_CHUNK => (
                        StreamEvent {
                            kind: msg.kind,
                            payload: decode_chunk(msg.payload.as_ref()),
                            ..Default::default()
                        },
                        false,
                    ),
                    MESSAGE_TYPE_STREAM_END => (StreamEvent { kind: msg.kind, ..Default::default() }, true),
                    MESSAGE_TYPE_ERROR => (
                        StreamEvent {
                            kind: msg.kind,
                            err: Some(decode_error(msg.payload.as_ref())),
                            ..Default::default()
                        },
                        true,
                    ),
                    MESSAGE_TYPE_HTTP_RESP => {
                        let resp = decode_response(msg.payload.as_ref());
                        (
                            StreamEvent {
                                kind: msg.kind,
                                payload: resp.body,
                                status: resp.status,
                                headers: resp.headers,
                                err: None,
                            },
                            true,
                        )
                    }
                    _ => continue,
                };
                if tx.send(event).await.is_err() || done {
                    return;
                }
            }
        });
        Ok(out)
    }
}

// ---------------------------------------------------------------- request/response codecs

fn new_request_message(req: &HttpRequest) -> Message {
    Message {
        id: uuid::Uuid::new_v4().to_string(),
        kind: MESSAGE_TYPE_HTTP_REQ.into(),
        payload: Some(encode_request(req)),
    }
}

/// `{"method","url","headers":{name:[values]},"body":"<string>","sent_at":<RFC3339Nano UTC>}`.
fn encode_request(req: &HttpRequest) -> Map<String, Value> {
    let headers: Map<String, Value> = req.headers.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    let mut payload = Map::new();
    payload.insert("method".into(), json!(req.method));
    payload.insert("url".into(), json!(req.url));
    payload.insert("headers".into(), Value::Object(headers));
    payload.insert("body".into(), json!(String::from_utf8_lossy(&req.body)));
    payload.insert("sent_at".into(), json!(rfc3339_nano_utc()));
    payload
}

/// Go's `time.RFC3339Nano` in UTC: fractional digits trimmed of trailing zeros.
fn rfc3339_nano_utc() -> String {
    let now = chrono::Utc::now();
    let mut out = now.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = format!("{:09}", now.timestamp_subsec_nanos().min(999_999_999));
    let frac = nanos.trim_end_matches('0');
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    out.push('Z');
    out
}

fn ok_response() -> HttpResponse {
    HttpResponse { status: 200, ..Default::default() }
}

/// Missing payload is a gateway failure (502); a missing status defaults to 200.
fn decode_response(payload: Option<&Map<String, Value>>) -> HttpResponse {
    let Some(payload) = payload else {
        return HttpResponse { status: 502, ..Default::default() };
    };
    let mut resp = ok_response();
    if let Some(status) = payload.get("status").and_then(Value::as_f64) {
        resp.status = status as u16;
    }
    if let Some(Value::Object(headers)) = payload.get("headers") {
        for (key, raw) in headers {
            let Ok(name) = HeaderName::from_bytes(key.as_bytes()) else { continue };
            match raw {
                Value::Array(items) => {
                    for item in items.iter().filter_map(Value::as_str) {
                        if let Ok(value) = HeaderValue::from_str(item) {
                            resp.headers.append(name.clone(), value);
                        }
                    }
                }
                Value::String(item) => {
                    if let Ok(value) = HeaderValue::from_str(item) {
                        resp.headers.insert(name, value);
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(body) = payload.get("body").and_then(Value::as_str) {
        resp.body = body.as_bytes().to_vec();
    }
    resp
}

fn decode_chunk(payload: Option<&Map<String, Value>>) -> Vec<u8> {
    payload.and_then(|p| p.get("data")).and_then(Value::as_str).map(|s| s.as_bytes().to_vec()).unwrap_or_default()
}

/// `"<msg> (status=N)"`, default message `wsrelay: upstream error`.
fn decode_error(payload: Option<&Map<String, Value>>) -> String {
    let Some(payload) = payload else {
        return "wsrelay: unknown error".into();
    };
    let message = payload.get("error").and_then(Value::as_str).unwrap_or("");
    let status = payload.get("status").and_then(Value::as_f64).unwrap_or(0.0) as i64;
    let message = if message.is_empty() { "wsrelay: upstream error" } else { message };
    format!("{message} (status={status})")
}

fn random_provider_name() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    let suffix: String = (0..16).map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char).collect();
    format!("aistudio-{suffix}")
}

/// Canonical MIME header key (`content-type` becomes `Content-Type`), like `http.Header.Set`.
pub fn canonical_header_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper = true;
    for ch in key.chars() {
        out.push(if upper { ch.to_ascii_uppercase() } else { ch.to_ascii_lowercase() });
        upper = ch == '-';
    }
    out
}

// ---------------------------------------------------------------- per-request message pipe

#[derive(Default)]
struct PipeState {
    queue: VecDeque<Message>,
    /// No more messages will arrive; the consumer drains the queue then sees the end.
    closed: bool,
    /// A terminal message was queued.
    terminal: bool,
    /// The consumer went away.
    rx_dropped: bool,
}

/// Bounded single-consumer queue with producer backpressure and a drop-oldest path used to force
/// an error frame past a full buffer (Go: pendingRequest).
struct Pipe {
    state: Mutex<PipeState>,
    not_empty: Notify,
    not_full: Notify,
}

impl Pipe {
    fn new() -> Arc<Self> {
        Arc::new(Pipe { state: Mutex::new(PipeState::default()), not_empty: Notify::new(), not_full: Notify::new() })
    }

    /// Queues `msg`, waiting while the buffer is full. False when the consumer is gone, the pipe
    /// is closed or the session closed meanwhile.
    async fn deliver(&self, session_closed: &mut watch::Receiver<bool>, msg: Message, terminal: bool) -> bool {
        let mut msg = Some(msg);
        loop {
            let notified = self.not_full.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.state.lock();
                if state.closed || state.terminal || state.rx_dropped {
                    return false;
                }
                if *session_closed.borrow() {
                    return false;
                }
                if state.queue.len() < PENDING_BUFFER {
                    if let Some(m) = msg.take() {
                        state.queue.push_back(m);
                    }
                    if terminal {
                        state.terminal = true;
                    }
                    drop(state);
                    self.not_empty.notify_one();
                    return true;
                }
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = session_closed.changed() => {}
            }
        }
    }

    /// Ends the pipe; the consumer drains what is queued.
    fn close(&self) {
        let mut state = self.state.lock();
        if state.closed {
            return;
        }
        state.closed = true;
        drop(state);
        self.not_empty.notify_one();
        self.not_full.notify_waiters();
    }

    /// Closes the pipe after queueing an error frame (replacing the oldest message when the
    /// buffer is full) unless a terminal message was already delivered.
    fn cancel_with_error(&self, cause: &str) {
        let mut state = self.state.lock();
        if state.closed {
            return;
        }
        state.closed = true;
        if !state.terminal {
            if state.queue.len() >= PENDING_BUFFER {
                state.queue.pop_front();
            }
            state.queue.push_back(Message {
                id: String::new(),
                kind: MESSAGE_TYPE_ERROR.into(),
                payload: Some(Map::from_iter([("error".to_string(), json!(cause))])),
            });
        }
        drop(state);
        self.not_empty.notify_one();
        self.not_full.notify_waiters();
    }

    async fn recv(&self) -> Option<Message> {
        loop {
            let notified = self.not_empty.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.state.lock();
                if let Some(msg) = state.queue.pop_front() {
                    drop(state);
                    self.not_full.notify_waiters();
                    return Some(msg);
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }
}

/// Consumer half of a request's response stream. Dropping it cancels the request.
pub struct PendingRx {
    pipe: Arc<Pipe>,
    session: Weak<Session>,
    id: String,
}

impl PendingRx {
    /// Next response message, `None` once the request finished or the session closed.
    pub async fn recv(&mut self) -> Option<Message> {
        self.pipe.recv().await
    }
}

impl Drop for PendingRx {
    fn drop(&mut self) {
        self.pipe.state.lock().rx_dropped = true;
        self.pipe.not_full.notify_waiters();
        if let Some(session) = self.session.upgrade() {
            session.pending.lock().remove(&self.id);
        }
    }
}

// ---------------------------------------------------------------- session

struct Session {
    provider: String,
    outbound: mpsc::Sender<Outbound>,
    pending: Mutex<HashMap<String, Arc<Pipe>>>,
    closed: watch::Sender<bool>,
    cleaned: AtomicBool,
    manager: Weak<Manager>,
}

impl Session {
    fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// Pings every 30s; a failed write ends the session.
    async fn heartbeat(self: Arc<Self>) {
        let mut closed = self.closed.subscribe();
        let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = wait_closed(&mut closed) => return,
                _ = ticker.tick() => {
                    let sent = tokio::time::timeout(WRITE_TIMEOUT, self.outbound.send(Outbound::Ping)).await;
                    if !matches!(sent, Ok(Ok(()))) {
                        self.cleanup("websocket: ping write failed");
                        return;
                    }
                }
            }
        }
    }

    /// Read loop: dispatches every message until the socket fails or the deadline passes.
    async fn run(self: Arc<Self>, mut inbound: std::pin::Pin<Box<dyn Stream<Item = Result<Inbound, String>> + Send>>) {
        let mut deadline = tokio::time::Instant::now() + READ_TIMEOUT;
        let mut closed = self.closed.subscribe();
        loop {
            let frame = tokio::select! {
                _ = wait_closed(&mut closed) => return,
                frame = tokio::time::timeout_at(deadline, inbound.next()) => frame,
            };
            match frame {
                Err(_) => return self.cleanup("websocket: read timeout (i/o timeout)"),
                Ok(None) => return self.cleanup("websocket: close 1006 (abnormal closure): unexpected EOF"),
                Ok(Some(Err(err))) => return self.cleanup(&err),
                Ok(Some(Ok(Inbound::Pong))) => deadline = tokio::time::Instant::now() + READ_TIMEOUT,
                Ok(Some(Ok(Inbound::Text(text)))) => match serde_json::from_str::<Message>(&text) {
                    Ok(msg) => self.dispatch(msg).await,
                    Err(err) => return self.cleanup(&err.to_string()),
                },
            }
        }
    }

    async fn dispatch(self: &Arc<Self>, msg: Message) {
        if msg.kind == MESSAGE_TYPE_PING {
            let _ = self.send(Message { id: msg.id, kind: MESSAGE_TYPE_PONG.into(), payload: None }).await;
            return;
        }
        let pipe = self.pending.lock().get(&msg.id).cloned();
        let Some(pipe) = pipe else {
            if msg.is_terminal() {
                tracing::debug!(
                    "wsrelay: received terminal message for unknown id {} (provider={})",
                    msg.id,
                    self.provider
                );
            }
            return;
        };
        let mut closed = self.closed.subscribe();
        if !msg.is_terminal() {
            pipe.deliver(&mut closed, msg, false).await;
            return;
        }
        let id = msg.id.clone();
        let is_error = msg.kind == MESSAGE_TYPE_ERROR;
        let error_text = msg
            .payload
            .as_ref()
            .and_then(|p| p.get("error"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("wsrelay: upstream error")
            .to_string();
        let delivered = pipe.deliver(&mut closed, msg, true).await;
        let removed = self.pending.lock().remove(&id);
        if removed.is_none() {
            return;
        }
        if delivered {
            pipe.close();
        } else if self.is_closed() {
            pipe.cancel_with_error(CLOSED_CAUSE);
        } else if pipe.state.lock().rx_dropped {
            pipe.close();
        } else if is_error {
            pipe.cancel_with_error(&error_text);
        } else {
            pipe.close();
        }
    }

    async fn send(&self, msg: Message) -> Result<(), RelayError> {
        if self.is_closed() {
            return Err(RelayError::not_sent(CLOSED_CAUSE));
        }
        let text = serde_json::to_string(&msg).map_err(|e| RelayError::sent(format!("write json: {e}")))?;
        match tokio::time::timeout(WRITE_TIMEOUT, self.outbound.send(Outbound::Text(text))).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(RelayError::sent("write json: websocket closed")),
            Err(_) => Err(RelayError::sent("write json: i/o timeout")),
        }
    }

    async fn request(self: &Arc<Self>, msg: Message) -> Result<PendingRx, RelayError> {
        if msg.id.is_empty() {
            return Err(RelayError::not_sent("wsrelay: message id is required"));
        }
        let pipe = Pipe::new();
        {
            let mut pending = self.pending.lock();
            if pending.contains_key(&msg.id) {
                return Err(RelayError::not_sent(format!("wsrelay: duplicate message id {}", msg.id)));
            }
            pending.insert(msg.id.clone(), pipe.clone());
        }
        let id = msg.id.clone();
        // Constructed before the write so a failed send or a cancelled caller removes the entry.
        let rx = PendingRx { pipe: pipe.clone(), session: Arc::downgrade(self), id: id.clone() };
        if let Err(err) = self.send(msg).await {
            self.pending.lock().remove(&id);
            pipe.close();
            return Err(err);
        }
        Ok(rx)
    }

    /// Closes the session once: fails every pending request with `cause` and tells the manager.
    fn cleanup(self: &Arc<Self>, cause: &str) {
        if self.cleaned.swap(true, Ordering::AcqRel) {
            return;
        }
        let _ = self.closed.send(true);
        let drained: Vec<_> = self.pending.lock().drain().map(|(_, p)| p).collect();
        for pipe in drained {
            pipe.cancel_with_error(cause);
        }
        if let Some(manager) = self.manager.upgrade() {
            manager.session_closed(self, cause);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream_shim::ReceiverStream;

    /// Minimal mpsc-to-Stream adapter for tests (avoids a tokio-stream dependency).
    mod tokio_stream_shim {
        use std::pin::Pin;
        use std::task::{Context, Poll};

        pub struct ReceiverStream<T>(pub tokio::sync::mpsc::Receiver<T>);

        impl<T> futures_util::Stream for ReceiverStream<T> {
            type Item = T;
            fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
                self.0.poll_recv(cx)
            }
        }
    }

    struct Page {
        manager: Arc<Manager>,
        provider: String,
        sent: mpsc::Receiver<Outbound>,
        feed: mpsc::Sender<Result<Inbound, String>>,
    }

    fn connect(manager: &Arc<Manager>) -> Page {
        let (out_tx, out_rx) = mpsc::channel(256);
        let (in_tx, in_rx) = mpsc::channel(256);
        let provider = manager.attach(out_tx, ReceiverStream(in_rx));
        Page { manager: manager.clone(), provider, sent: out_rx, feed: in_tx }
    }

    impl Page {
        async fn next_request(&mut self) -> Message {
            loop {
                match self.sent.recv().await.expect("outbound closed") {
                    Outbound::Text(text) => return serde_json::from_str(&text).unwrap(),
                    Outbound::Ping => continue,
                }
            }
        }

        async fn reply(&self, id: &str, kind: &str, payload: Value) {
            let text = json!({"id": id, "type": kind, "payload": payload}).to_string();
            self.feed.send(Ok(Inbound::Text(text))).await.unwrap();
        }
    }

    fn request() -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            url: "https://generativelanguage.googleapis.com/v1beta/models/m:generateContent".into(),
            headers: BTreeMap::from([("Content-Type".to_string(), vec!["application/json".to_string()])]),
            body: br#"{"a":1}"#.to_vec(),
        }
    }

    #[tokio::test]
    async fn request_envelope_and_http_response_roundtrip() {
        let manager = Arc::new(Manager::new(""));
        let mut page = connect(&manager);
        assert!(page.provider.starts_with("aistudio-") && page.provider.len() == "aistudio-".len() + 16);
        let provider = page.provider.clone();
        let task = tokio::spawn({
            let manager = manager.clone();
            async move { manager.non_stream(&provider, &request()).await }
        });
        let msg = page.next_request().await;
        assert_eq!(msg.kind, MESSAGE_TYPE_HTTP_REQ);
        let payload = msg.payload.unwrap();
        assert_eq!(payload["method"], "POST");
        assert_eq!(payload["headers"]["Content-Type"][0], "application/json");
        assert_eq!(payload["body"], r#"{"a":1}"#);
        assert!(payload["sent_at"].as_str().unwrap().ends_with('Z'));
        page.reply(
            &msg.id,
            MESSAGE_TYPE_HTTP_RESP,
            json!({"status": 201, "headers": {"X-A": ["1", "2"], "X-B": "b"}, "body": "ok"}),
        )
        .await;
        let resp = task.await.unwrap().unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body, b"ok");
        assert_eq!(resp.headers.get_all("x-a").iter().count(), 2);
        assert_eq!(resp.headers["x-b"], "b");
        assert!(page.manager.session(&page.provider).is_some());
    }

    #[tokio::test]
    async fn streamed_reply_is_aggregated_and_events_are_forwarded() {
        let manager = Arc::new(Manager::new(""));
        let mut page = connect(&manager);
        let provider = page.provider.clone();
        let agg = tokio::spawn({
            let manager = manager.clone();
            let provider = provider.clone();
            async move { manager.non_stream(&provider, &request()).await }
        });
        let msg = page.next_request().await;
        page.reply(&msg.id, MESSAGE_TYPE_STREAM_START, json!({"status": 200, "headers": {"X-S": "1"}})).await;
        page.reply(&msg.id, MESSAGE_TYPE_STREAM_CHUNK, json!({"data": "ab"})).await;
        page.reply(&msg.id, MESSAGE_TYPE_STREAM_CHUNK, json!({"data": "cd"})).await;
        page.reply(&msg.id, MESSAGE_TYPE_STREAM_END, json!({})).await;
        let resp = agg.await.unwrap().unwrap();
        assert_eq!((resp.status, resp.body.as_slice()), (200, &b"abcd"[..]));
        assert_eq!(resp.headers["x-s"], "1");

        let mut events = manager.stream(&provider, &request()).await.unwrap();
        let msg = page.next_request().await;
        page.reply(&msg.id, MESSAGE_TYPE_STREAM_START, json!({"status": 429})).await;
        page.reply(&msg.id, MESSAGE_TYPE_STREAM_CHUNK, json!({"data": "x"})).await;
        page.reply(&msg.id, MESSAGE_TYPE_ERROR, json!({"error": "boom", "status": 500})).await;
        let first = events.recv().await.unwrap();
        assert_eq!((first.kind.as_str(), first.status), (MESSAGE_TYPE_STREAM_START, 429));
        assert_eq!(events.recv().await.unwrap().payload, b"x");
        let last = events.recv().await.unwrap();
        assert_eq!(last.err.as_deref(), Some("boom (status=500)"));
        assert!(events.recv().await.is_none());
    }

    #[tokio::test]
    async fn app_level_ping_gets_a_pong_and_unknown_provider_errors() {
        let manager = Arc::new(Manager::new(""));
        let mut page = connect(&manager);
        page.feed.send(Ok(Inbound::Text(json!({"id": "p1", "type": "ping"}).to_string()))).await.unwrap();
        let pong = page.next_request().await;
        assert_eq!((pong.id.as_str(), pong.kind.as_str()), ("p1", MESSAGE_TYPE_PONG));
        let err = manager.non_stream("aistudio-nope", &request()).await.unwrap_err();
        assert_eq!(err.message, "wsrelay: provider aistudio-nope not connected");
        assert!(!err.attempted);
    }

    #[tokio::test]
    async fn disconnect_fails_pending_requests_and_fires_hooks() {
        let manager = Arc::new(Manager::new(""));
        let events: Arc<Mutex<Vec<String>>> = Arc::default();
        let (e1, e2) = (events.clone(), events.clone());
        manager.set_hooks(
            Some(move |p: &str| e1.lock().push(format!("up:{p}"))),
            Some(move |p: &str, cause: &str| e2.lock().push(format!("down:{p}:{cause}"))),
        );
        let mut page = connect(&manager);
        let provider = page.provider.clone();
        let task = tokio::spawn({
            let manager = manager.clone();
            let provider = provider.clone();
            async move { manager.non_stream(&provider, &request()).await }
        });
        let _ = page.next_request().await;
        page.feed.send(Err("socket reset".into())).await.unwrap();
        assert_eq!(task.await.unwrap().unwrap_err().message, "socket reset (status=0)");
        let log = events.lock().clone();
        assert_eq!(log, vec![format!("up:{provider}"), format!("down:{provider}:socket reset")]);
        assert!(manager.session(&provider).is_none());
        assert!(manager.connected().is_empty());
    }

    #[tokio::test]
    async fn terminal_error_survives_a_full_buffer_on_cleanup() {
        let manager = Arc::new(Manager::new(""));
        let mut page = connect(&manager);
        let provider = page.provider.clone();
        let mut rx = manager
            .send(&provider, Message { id: "r1".into(), kind: MESSAGE_TYPE_HTTP_REQ.into(), payload: None })
            .await
            .unwrap();
        let _ = page.next_request().await;
        for i in 0..PENDING_BUFFER {
            page.reply("r1", MESSAGE_TYPE_STREAM_CHUNK, json!({"data": i.to_string()})).await;
        }
        // Wait until the read loop buffered everything, then kill the session.
        for _ in 0..200 {
            if rx.pipe.state.lock().queue.len() == PENDING_BUFFER {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        page.feed.send(Err("gone".into())).await.unwrap();
        let mut last = None;
        while let Some(msg) = rx.recv().await {
            last = Some(msg);
        }
        let last = last.unwrap();
        assert_eq!(last.kind, MESSAGE_TYPE_ERROR);
        assert_eq!(last.payload.unwrap()["error"], "gone");
    }

    #[tokio::test]
    async fn reconnect_collision_and_dropped_consumer() {
        let manager = Arc::new(Manager::new(""));
        let mut page = connect(&manager);
        let provider = page.provider.clone();
        let rx = manager
            .send(&provider, Message { id: "r1".into(), kind: MESSAGE_TYPE_HTTP_REQ.into(), payload: None })
            .await
            .unwrap();
        let _ = page.next_request().await;
        let dup = manager
            .send(&provider, Message { id: "r1".into(), kind: MESSAGE_TYPE_HTTP_REQ.into(), payload: None })
            .await;
        assert_eq!(dup.err().unwrap().message, "wsrelay: duplicate message id r1");
        drop(rx);
        assert!(manager.session(&provider).unwrap().pending.lock().is_empty());
        let none = manager.send(&provider, Message::default()).await;
        assert_eq!(none.err().unwrap().message, "wsrelay: message id is required");
    }

    #[test]
    fn codecs() {
        assert_eq!(decode_response(None).status, 502);
        assert_eq!(decode_response(Some(&Map::new())).status, 200);
        assert_eq!(decode_error(None), "wsrelay: unknown error");
        assert_eq!(decode_error(Some(&Map::new())), "wsrelay: upstream error (status=0)");
        assert_eq!(canonical_header_key("x-goog-API-key"), "X-Goog-Api-Key");
        let json = serde_json::to_string(&Message { id: "1".into(), kind: "pong".into(), payload: None }).unwrap();
        assert_eq!(json, r#"{"id":"1","type":"pong"}"#);
    }
}
