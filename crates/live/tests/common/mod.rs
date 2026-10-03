//! Shared harness: a real `Manager` with OAuth Codex credentials, a mock upstream (HTTP calls and
//! websockets) and helpers to serve the sideband/direct handlers over real sockets.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::{any, get};
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_live::{Caller, Handler, RequestParts, endpoints};
use cpa_runtime::conductor::Manager;
use cpa_runtime::executor::{ExecError, Executor, Options, Request as ExecRequest, Response as ExecResponse, StreamResult};
use http::{HeaderMap, StatusCode};
use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::watch;

/// Registers as the `codex` provider so credential selection works; never executes anything.
struct NoopCodex;

#[async_trait]
impl Executor for NoopCodex {
    fn identifier(&self) -> &str {
        "codex"
    }
    async fn execute(&self, _: &Auth, _: ExecRequest, _: Options) -> Result<ExecResponse, ExecError> {
        Err(ExecError::new(500, "unused"))
    }
    async fn execute_stream(&self, _: &Auth, _: ExecRequest, _: Options) -> Result<StreamResult, ExecError> {
        Err(ExecError::new(500, "unused"))
    }
    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        Ok(auth.clone())
    }
    async fn count_tokens(&self, _: &Auth, _: ExecRequest, _: Options) -> Result<ExecResponse, ExecError> {
        Err(ExecError::new(500, "unused"))
    }
}

pub struct Env {
    pub handler: Handler,
    pub manager: Arc<Manager>,
    pub cfg_tx: watch::Sender<Arc<Config>>,
}

pub fn oauth_auth(id: &str, token: &str, account: Option<&str>) -> Auth {
    let mut auth = Auth::new(id, "codex");
    auth.metadata.insert("access_token".into(), json!(token));
    if let Some(account) = account {
        auth.metadata.insert("account_id".into(), json!(account));
    }
    auth
}

pub async fn env(auths: Vec<Auth>) -> Env {
    env_with_config(Config::default(), auths).await
}

pub async fn env_with_config(cfg: Config, auths: Vec<Auth>) -> Env {
    let manager = Arc::new(Manager::default());
    manager.register_executor(Arc::new(NoopCodex));
    for auth in auths {
        manager.update(auth).await.expect("register auth");
    }
    let (cfg_tx, cfg_rx) = watch::channel(Arc::new(cfg));
    Env { handler: Handler::new(manager.clone(), cfg_rx), manager, cfg_tx }
}

/// One request the mock upstream saw.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Seen {
    pub fn header(&self, name: &str) -> String {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
    }
}

/// What the mock upstream answers to plain HTTP requests.
#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, &'static str)>,
    pub body: &'static str,
}

impl Default for Reply {
    fn default() -> Self {
        Reply {
            status: 201,
            headers: vec![
                ("content-type", "application/sdp"),
                ("location", "/v1/live/call-123"),
                ("set-cookie", "session=secret"),
                ("x-live-session", "live-session-123"),
                ("x-request-id", "req-1"),
            ],
            body: "v=0\r\na=ice-lite\r\n",
        }
    }
}

#[derive(Default)]
pub struct UpstreamState {
    pub seen: Mutex<VecDeque<Seen>>,
    pub reply: Mutex<Reply>,
    /// Frames the websocket upstream sends right after the handshake.
    pub ws_greeting: Mutex<Vec<String>>,
    /// Handshake rejection (status, content type, body); when set the upgrade is refused.
    pub ws_reject: Mutex<Option<(u16, &'static str, &'static str)>>,
    pub ws_received: Mutex<Vec<String>>,
}

impl UpstreamState {
    pub fn last(&self) -> Seen {
        self.seen.lock().back().cloned().expect("no upstream request seen")
    }
}

fn record(state: &UpstreamState, req: &Request) -> Seen {
    let seen = Seen {
        method: req.method().to_string(),
        path: req.uri().path().to_string(),
        query: req.uri().query().unwrap_or("").to_string(),
        headers: req.headers().clone(),
        body: Bytes::new(),
    };
    state.seen.lock().push_back(seen.clone());
    seen
}

async fn http_handler(State(state): State<Arc<UpstreamState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 32 << 20).await.unwrap_or_default();
    let seen = Seen {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        query: parts.uri.query().unwrap_or("").to_string(),
        headers: parts.headers,
        body: bytes,
    };
    state.seen.lock().push_back(seen);
    let reply = state.reply.lock().clone();
    let mut builder = Response::builder().status(StatusCode::from_u16(reply.status).unwrap());
    for (name, value) in reply.headers {
        builder = builder.header(name, value);
    }
    builder.body(Body::from(reply.body)).unwrap()
}

async fn ws_handler(State(state): State<Arc<UpstreamState>>, ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>, req: Request) -> Response {
    record(&state, &req);
    if let Some((status, content_type, body)) = *state.ws_reject.lock() {
        return Response::builder()
            .status(StatusCode::from_u16(status).unwrap())
            .header("content-type", content_type)
            .body(Body::from(body))
            .unwrap();
    }
    let Ok(ws) = ws else {
        return Response::builder().status(400).body(Body::empty()).unwrap();
    };
    ws.on_upgrade(move |socket| echo(socket, state))
}

async fn echo(mut socket: WebSocket, state: Arc<UpstreamState>) {
    let greeting = state.ws_greeting.lock().clone();
    for frame in greeting {
        if socket.send(Message::Text(frame.into())).await.is_err() {
            return;
        }
    }
    while let Some(Ok(message)) = socket.recv().await {
        if let Message::Text(text) = message {
            state.ws_received.lock().push(text.to_string());
            if socket.send(Message::Text(format!("echo:{text}").into())).await.is_err() {
                return;
            }
        }
    }
}

/// Serves the mock upstream; returns its base address (`127.0.0.1:port`).
pub async fn serve_upstream(state: Arc<UpstreamState>) -> String {
    let app = Router::new()
        .route("/calls", any(http_handler))
        .route("/v1/realtime/calls/{call_id}/hangup", any(http_handler))
        .route("/v1/live/{call_id}", any(ws_handler))
        .route("/v1/realtime", any(ws_handler))
        .route("/v1/realtime/calls/{call_id}", any(ws_handler))
        .with_state(state);
    serve(app).await
}

pub async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr.to_string()
}

/// Points the handler at the mock upstream.
pub fn point_at(env: &Env, addr: &str) {
    env.handler.set_upstream_urls(&format!("http://{addr}/calls"), &format!("ws://{addr}/v1"));
}

pub fn parts(path: &str, headers: &[(&str, &str)]) -> RequestParts {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        map.insert(http::HeaderName::from_bytes(name.as_bytes()).unwrap(), http::HeaderValue::from_str(value).unwrap());
    }
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    RequestParts::new(path, query, &map, None)
}

pub fn parts_with_call(path: &str, call_id: &str, headers: &[(&str, &str)]) -> RequestParts {
    let mut p = parts(path, headers);
    p.call_id = Some(call_id.to_string());
    p
}

pub fn multipart_body(boundary: &str, sdp: &str, session: &str) -> String {
    let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"sdp\"\r\nContent-Type: application/sdp\r\n\r\n{sdp}\r\n");
    if !session.is_empty() {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"session\"\r\nContent-Type: application/json\r\n\r\n{session}\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    body
}

/// Serves the sideband and direct websocket handlers with a fixed caller.
pub async fn serve_downstream(handler: Handler, caller: Caller) -> String {
    #[derive(Clone)]
    struct S(Handler, Caller);
    async fn sideband(State(s): State<S>, axum::extract::Path(call_id): axum::extract::Path<String>, req: Request, ) -> Response {
        let (mut parts_, _) = req.into_parts();
        let ws = <WebSocketUpgrade as axum::extract::FromRequestParts<()>>::from_request_parts(&mut parts_, &()).await.ok();
        let p = RequestParts::new(parts_.uri.path(), parts_.uri.query().unwrap_or(""), &parts_.headers, Some(call_id));
        endpoints::sideband(&s.0, &s.1, p, ws).await
    }
    async fn realtime(State(s): State<S>, req: Request) -> Response {
        let (mut parts_, _) = req.into_parts();
        let ws = <WebSocketUpgrade as axum::extract::FromRequestParts<()>>::from_request_parts(&mut parts_, &()).await.ok();
        let p = RequestParts::new(parts_.uri.path(), parts_.uri.query().unwrap_or(""), &parts_.headers, None);
        endpoints::realtime_websocket(&s.0, &s.1, p, ws).await
    }
    let app = Router::new()
        .route("/v1/live/{call_id}", get(sideband))
        .route("/v1/realtime/calls/{call_id}", get(sideband))
        .route("/v1/realtime", get(realtime))
        .with_state(S(handler, caller));
    serve(app).await
}
