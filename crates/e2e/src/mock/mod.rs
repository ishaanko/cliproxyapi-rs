//! Mock upstream server emulating the provider APIs used by API-key credentials.
//!
//! Every inbound upstream request is logged and answered according to the active `Script`.
//! The harness drives it through `/__control/*`: `POST /__control/script` installs a script
//! (and clears the log), `GET /__control/log` returns the requests seen since.

pub mod media;
pub mod replies;
pub mod script;
pub mod store;

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequest, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use replies::{Family, Op, RBody, Rendered, ReqCtx};
use script::{Chunking, Pick, Reply, Script, ScriptState};

/// One request observed by the mock.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LoggedRequest {
    pub family: String,
    pub method: String,
    /// Path with the family prefix stripped, e.g. `/v1/messages`.
    pub path: String,
    pub query: String,
    /// API key the request authenticated with.
    pub credential: String,
    /// Lower-cased header names; repeated headers joined with `, `.
    pub headers: BTreeMap<String, String>,
    /// Parsed JSON body, the raw text if it is not JSON, or null when empty.
    pub body: Value,
    /// Text frames received after a WebSocket upgrade.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ws_frames: Vec<Value>,
    #[serde(default)]
    pub websocket: bool,
}

struct Inner {
    script: ScriptState,
    log: Vec<LoggedRequest>,
}

#[derive(Clone)]
pub struct Mock {
    inner: Arc<Mutex<Inner>>,
}

pub struct MockHandle {
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Binds the mock on `127.0.0.1:port` and serves it in the background.
pub async fn start(port: u16) -> Result<MockHandle> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("bind mock upstream on 127.0.0.1:{port} (pass --mock-port to change)"))?;
    let port = listener.local_addr()?.port();
    let mock = Mock { inner: Arc::new(Mutex::new(Inner { script: ScriptState::new(Script::default()), log: vec![] })) };
    let app = axum::Router::new().fallback(handle).with_state(mock);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(MockHandle { port, task })
}

fn headers_map(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in headers {
        let v = String::from_utf8_lossy(value.as_bytes()).into_owned();
        out.entry(name.as_str().to_string()).and_modify(|e| *e = format!("{e}, {v}")).or_insert(v);
    }
    out
}

fn credential(headers: &BTreeMap<String, String>) -> String {
    if let Some(a) = headers.get("authorization") {
        return a.strip_prefix("Bearer ").unwrap_or(a).to_string();
    }
    headers.get("x-api-key").or_else(|| headers.get("x-goog-api-key")).cloned().unwrap_or_default()
}

/// Name of the first tool declared in a request body, in any dialect.
fn first_tool_name(body: &Value) -> String {
    let tools = body["tools"].as_array().cloned().unwrap_or_default();
    tools
        .iter()
        .find_map(|t| {
            t["name"]
                .as_str()
                .or_else(|| t["function"]["name"].as_str())
                .or_else(|| t["functionDeclarations"][0]["name"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "get_weather".to_string())
}

/// Splits `/v1beta/models/<model>:<method>` into (model, method).
fn gemini_route(path: &str) -> Option<(&str, &str)> {
    path.strip_prefix("/v1beta/models/")?.split_once(':')
}

/// Maps (family, path) to the operation and request context.
fn classify(family: Family, path: &str, body: &Value) -> Option<(Op, ReqCtx)> {
    let tool = first_tool_name(body);
    let model = body["model"].as_str().unwrap_or_default().to_string();
    let stream = body["stream"].as_bool() == Some(true);
    let ctx = ReqCtx { model, stream, tool };
    match (family, path) {
        (Family::Anthropic, "/v1/messages") => Some((Op::Generate, ctx)),
        (Family::Anthropic, "/v1/messages/count_tokens") => Some((Op::CountTokens, ctx)),
        (Family::Compat, "/chat/completions") => Some((Op::Generate, ctx)),
        (Family::Compat | Family::Codex, "/responses") => Some((Op::Generate, ctx)),
        (Family::Compat | Family::Codex, "/responses/compact") => Some((Op::Compact, ctx)),
        (Family::Gemini, _) => {
            let (model, method) = gemini_route(path)?;
            let op = if method == "countTokens" { Op::CountTokens } else { Op::Generate };
            if !matches!(method, "generateContent" | "streamGenerateContent" | "countTokens") {
                return None;
            }
            Some((op, ReqCtx { model: model.to_string(), stream: method == "streamGenerateContent", tool: ctx.tool }))
        }
        (_, "/models" | "/v1/models" | "/v1beta/models") => Some((Op::Models, ctx)),
        _ => None,
    }
}

/// Re-splits per-event chunks so streams arrive with different write boundaries.
fn rechunk(chunks: Vec<Bytes>, mode: Chunking) -> Vec<Bytes> {
    match mode {
        Chunking::Whole => chunks,
        Chunking::Split => chunks.into_iter().flat_map(|c| [c.slice(..c.len() / 2), c.slice(c.len() / 2..)]).filter(|c| !c.is_empty()).collect(),
        Chunking::Merged => chunks
            .chunks(3)
            .map(|group| Bytes::from(group.iter().flat_map(|b| b.iter().copied()).collect::<Vec<u8>>()))
            .collect(),
    }
}

/// `stall_ms` pauses a stream after its first chunk.
fn to_response(r: Rendered, stall_ms: u64, chunking: Chunking) -> Response {
    let mut builder = Response::builder().status(StatusCode::from_u16(r.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    for (k, v) in &r.headers {
        if let Ok(v) = HeaderValue::from_str(v) {
            builder = builder.header(k.as_str(), v);
        }
    }
    let body = match r.body {
        RBody::Full(b) => Body::from(b),
        RBody::Chunks { chunks, abort } => {
            let chunks = rechunk(chunks, chunking);
            let items = chunks.into_iter().map(Ok::<Bytes, io::Error>).chain(abort.then(|| Err(io::Error::other("mock abort"))));
            let stall = Duration::from_millis(stall_ms);
            let paced = stream::iter(items.enumerate()).then(move |(i, item)| async move {
                tokio::time::sleep(if i == 1 { stall } else { Duration::from_millis(1) }).await;
                item
            });
            Body::from_stream(paced)
        }
    };
    builder.body(body).unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn handle(State(mock): State<Mock>, req: Request) -> Response {
    if req.method() == axum::http::Method::CONNECT {
        return store::connect(req);
    }
    let path = req.uri().path().to_string();
    if let Some(ctl) = path.strip_prefix("/__control/") {
        return control(&mock, ctl, req).await;
    }
    let method = req.method().to_string();
    let query = req.uri().query().unwrap_or_default().to_string();
    let headers = headers_map(req.headers());
    let host_header = headers.get("host").cloned().unwrap_or_default();
    let is_ws = headers.get("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"));

    let trimmed = path.trim_start_matches('/');
    let (prefix, rest) = trimmed.split_once('/').map(|(a, b)| (a, format!("/{b}"))).unwrap_or((trimmed, "/".into()));
    let family = Family::from_prefix(prefix);
    let media_op = media::route(prefix, &method, &rest);
    let cred = credential(&headers);
    let mut entry = LoggedRequest {
        family: family
            .map(|f| f.prefix().to_string())
            .or_else(|| media_op.as_ref().map(|_| prefix.to_string()))
            .unwrap_or_else(|| "unknown".into()),
        method,
        path: rest.clone(),
        query,
        credential: cred.clone(),
        headers,
        websocket: is_ws,
        ..Default::default()
    };

    if is_ws {
        let Ok(upgrade) = WebSocketUpgrade::from_request(req, &()).await else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let idx = push_log(&mock, entry).await;
        let mock = mock.clone();
        return upgrade.on_upgrade(move |socket| ws_session(mock, idx, cred, socket));
    }

    let bytes = axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024).await.unwrap_or_default();
    let mut body: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        if bytes.is_empty() { Value::Null } else { Value::String(String::from_utf8_lossy(&bytes).into_owned()) }
    });
    // Multipart bodies are logged as a sorted description; their random boundary is masked.
    let content_type = entry.headers.get("content-type").cloned().unwrap_or_default();
    if content_type.starts_with("multipart/form-data")
        && let Some(described) = media::describe_multipart(&content_type, &bytes)
    {
        body = described;
        entry.headers.insert("content-type".into(), "multipart/form-data; boundary=<boundary>".into());
    }
    if let Some(op) = &media_op {
        media::stabilize_body(op, &mut body);
    }
    entry.body = body.clone();
    push_log(&mock, entry).await;

    if let Some(op) = media_op {
        let host = host_header.clone();
        // The video file is a static download: it never consumes a scripted step.
        let pick = if matches!(op, media::MediaOp::VideoFile(_)) {
            Pick { reply: Reply::ok(script::Content::Text), delay_ms: 0, stall_ms: 0, chunking: Chunking::Whole, headers: vec![] }
        } else {
            mock.inner.lock().await.script.next(&cred)
        };
        if pick.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(pick.delay_ms)).await;
        }
        let mut rendered = media::render(&op, &pick.reply, &body, &host);
        rendered.headers.extend(pick.headers);
        return to_response(rendered, pick.stall_ms, pick.chunking);
    }

    let Some((family, (op, ctx))) = family.and_then(|f| classify(f, &rest, &body).map(|c| (f, c))) else {
        let rendered = Rendered {
            status: 404,
            headers: vec![("content-type".into(), "application/json".into())],
            body: RBody::Full(Bytes::from(json!({"error":"mock: unknown route"}).to_string())),
        };
        return to_response(rendered, 0, Chunking::Whole);
    };
    // Model listings are not part of any scenario script.
    let pick = if op == Op::Models {
        Pick { reply: Reply::ok(script::Content::Text), delay_ms: 0, stall_ms: 0, chunking: Chunking::Whole, headers: vec![] }
    } else {
        mock.inner.lock().await.script.next(&cred)
    };
    if pick.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(pick.delay_ms)).await;
    }
    let mut rendered = replies::render(family, op, &ctx, &pick.reply);
    rendered.headers.extend(pick.headers);
    to_response(rendered, pick.stall_ms, pick.chunking)
}

async fn push_log(mock: &Mock, entry: LoggedRequest) -> usize {
    let mut inner = mock.inner.lock().await;
    inner.log.push(entry);
    inner.log.len() - 1
}

async fn control(mock: &Mock, ctl: &str, req: Request) -> Response {
    match ctl {
        "script" => {
            let bytes = axum::body::to_bytes(req.into_body(), 16 * 1024 * 1024).await.unwrap_or_default();
            match serde_json::from_slice::<Script>(&bytes) {
                Ok(script) => {
                    let mut inner = mock.inner.lock().await;
                    inner.script = ScriptState::new(script);
                    inner.log.clear();
                    StatusCode::OK.into_response()
                }
                Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
            }
        }
        "log" => {
            let inner = mock.inner.lock().await;
            axum::Json(inner.log.clone()).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Gap between upstream websocket frames; lets the server under test forward each frame before
/// the next one (or a terminal close) arrives, keeping captures deterministic.
const WS_FRAME_PACE_MS: u64 = 15;

/// Codex upstream WebSocket: each `response.create` frame consumes one script step and is
/// answered with Responses events as text frames.
async fn ws_session(mock: Mock, idx: usize, cred: String, mut socket: WebSocket) {
    while let Some(Ok(msg)) = socket.recv().await {
        let Message::Text(text) = msg else {
            if matches!(msg, Message::Close(_)) {
                break;
            }
            continue;
        };
        let frame: Value = serde_json::from_str(text.as_str()).unwrap_or(Value::String(text.to_string()));
        let Pick { reply, delay_ms, .. } = {
            let mut inner = mock.inner.lock().await;
            if let Some(e) = inner.log.get_mut(idx) {
                e.ws_frames.push(frame.clone());
            }
            inner.script.next(&cred)
        };
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        let ctx = ReqCtx {
            model: frame["model"].as_str().unwrap_or_default().to_string(),
            stream: true,
            tool: first_tool_name(&frame),
        };
        let (frames, close): (Vec<Value>, Option<bool>) = match &reply {
            Reply::Ok { content } => (replies::codex_ws_frames(&ctx, *content), None),
            Reply::Cut { content, after, abort } => {
                let mut f = replies::codex_ws_frames(&ctx, *content);
                f.truncate(*after);
                (f, Some(*abort))
            }
            Reply::StreamError { content, after } => {
                let mut f = replies::codex_ws_frames(&ctx, *content);
                f.truncate(*after);
                f.push(json!({"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"mock mid-stream error"}}}));
                (f, None)
            }
            Reply::Error { status, headers, body } => {
                let body = body.clone().unwrap_or_else(|| replies::error_body(Family::Codex, *status));
                let mut f = json!({"type":"error","status":status,"error":body["error"].clone()});
                if !headers.is_empty() {
                    f["headers"] = Value::Object(headers.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect());
                }
                (vec![f], None)
            }
            Reply::Raw { body, .. } => (vec![Value::String(body.clone())], None),
        };
        for f in frames {
            let text = match f {
                Value::String(s) => s,
                v => v.to_string(),
            };
            tokio::time::sleep(Duration::from_millis(WS_FRAME_PACE_MS)).await;
            if socket.send(Message::Text(text.into())).await.is_err() {
                return;
            }
        }
        if close.is_some() {
            // Let the server forward the last frame before the socket goes away.
            tokio::time::sleep(Duration::from_millis(WS_FRAME_PACE_MS * 4)).await;
        }
        match close {
            Some(true) => return,
            Some(false) => {
                let _ = socket.send(Message::Close(Some(CloseFrame { code: 1000, reason: "mock done".into() }))).await;
                return;
            }
            None => {}
        }
    }
}
