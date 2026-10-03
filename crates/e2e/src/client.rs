//! Client side of the harness: sends scenario requests to the server under test and captures
//! status, relevant headers and the body (JSON, SSE event list or WebSocket frames).

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::config::{CLIENT_KEY, MGMT_SECRET};

// ---------------------------------------------------------------- request model

#[derive(Clone, Debug)]
pub enum Auth {
    /// `Authorization: Bearer <client key>`.
    Client,
    None,
    /// `Authorization: Bearer <management secret>`.
    Mgmt,
    Bearer(&'static str),
    /// Raw `Authorization` header value, no scheme added.
    RawAuthorization(&'static str),
    XApiKey(&'static str),
    GoogKey(&'static str),
}

#[derive(Clone, Debug)]
pub enum Body {
    None,
    Json(Value),
    /// Sent verbatim with `Content-Type: application/json` (for malformed bodies).
    Text(String),
    /// Sent verbatim with the given Content-Type (multipart and form bodies).
    Raw { content_type: String, bytes: Vec<u8> },
    /// Sent verbatim with the given content type (SDP, multipart).
    Typed(&'static str, String),
}

#[derive(Clone, Debug)]
pub struct HttpReq {
    pub method: &'static str,
    /// Path including any query string.
    pub path: String,
    pub auth: Auth,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl HttpReq {
    fn new(method: &'static str, path: &str, body: Body) -> Self {
        HttpReq { method, path: path.to_string(), auth: Auth::Client, headers: vec![], body }
    }

    pub fn get(path: &str) -> Self {
        Self::new("GET", path, Body::None)
    }

    pub fn post(path: &str, body: Value) -> Self {
        Self::new("POST", path, Body::Json(body))
    }

    pub fn put(path: &str, body: Value) -> Self {
        Self::new("PUT", path, Body::Json(body))
    }

    pub fn patch(path: &str, body: Value) -> Self {
        Self::new("PATCH", path, Body::Json(body))
    }

    pub fn delete(path: &str) -> Self {
        Self::new("DELETE", path, Body::None)
    }

    pub fn options(path: &str) -> Self {
        Self::new("OPTIONS", path, Body::None)
    }

    /// `multipart/form-data` body with a fixed boundary; `files` are `(field, filename, content type, bytes)`.
    pub fn multipart(path: &str, fields: &[(&str, &str)], files: &[(&str, &str, &str, &[u8])]) -> Self {
        const BOUNDARY: &str = "e2eboundary0123456789";
        let mut bytes: Vec<u8> = Vec::new();
        for (name, value) in fields {
            bytes.extend_from_slice(format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
        }
        for (name, filename, content_type, data) in files {
            bytes.extend_from_slice(
                format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n").as_bytes(),
            );
            bytes.extend_from_slice(data);
            bytes.extend_from_slice(b"\r\n");
        }
        bytes.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        Self::new("POST", path, Body::Raw { content_type: format!("multipart/form-data; boundary={BOUNDARY}"), bytes })
    }

    /// `application/x-www-form-urlencoded` body.
    pub fn form(path: &str, pairs: &[(&str, &str)]) -> Self {
        let text = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
        Self::new("POST", path, Body::Raw { content_type: "application/x-www-form-urlencoded".into(), bytes: text.into_bytes() })
    }

    /// Sends `bytes` with exactly this Content-Type (none when empty).
    pub fn raw_typed(mut self, content_type: &str, bytes: &[u8]) -> Self {
        self.body = Body::Raw { content_type: content_type.to_string(), bytes: bytes.to_vec() };
        self
    }

    pub fn raw(mut self, text: &str) -> Self {
        self.body = Body::Text(text.to_string());
        self
    }

    /// Verbatim body with its own content type.
    pub fn typed(mut self, content_type: &'static str, text: &str) -> Self {
        self.body = Body::Typed(content_type, text.to_string());
        self
    }

    pub fn auth(mut self, auth: Auth) -> Self {
        self.auth = auth;
        self
    }

    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
}

#[derive(Clone, Debug)]
pub struct WsReq {
    pub path: String,
    pub auth: Auth,
    pub headers: Vec<(String, String)>,
    /// JSON text frames sent in order; after each one the client reads until a terminal event.
    pub messages: Vec<Value>,
}

impl WsReq {
    pub fn new(path: &str, messages: Vec<Value>) -> Self {
        WsReq { path: path.to_string(), auth: Auth::Client, headers: vec![], messages }
    }

    pub fn auth(mut self, auth: Auth) -> Self {
        self.auth = auth;
        self
    }

    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
}

#[derive(Clone, Debug)]
pub enum Step {
    Http(HttpReq),
    Ws(WsReq),
    /// Sleep this many ms (lets async config reloads settle); produces no capture.
    Pause(u64),
}

impl From<HttpReq> for Step {
    fn from(r: HttpReq) -> Self {
        Step::Http(r)
    }
}

impl From<WsReq> for Step {
    fn from(r: WsReq) -> Self {
        Step::Ws(r)
    }
}

// --------------------------------------------------------------- captured output

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ObsBody {
    Empty,
    Json {
        value: Value,
        /// Blank lines written before the JSON (non-stream keep-alives).
        #[serde(default, skip_serializing_if = "is_zero")]
        lead_newlines: usize,
    },
    Text { value: String },
    /// SSE events in order; comment lines and transport errors appear as their own entries.
    Sse { events: Vec<Value> },
    /// WebSocket frames received (JSON when parseable) plus how the socket ended.
    Ws { frames: Vec<Value>, close: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Observed {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: ObsBody,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Response headers that carry no behavior: derived from the body or the connection, or random.
const DROP_RESPONSE_HEADERS: &[&str] = &["date", "content-length", "transfer-encoding", "sec-websocket-accept"];

fn header_map(headers: &axum::http::HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter(|(k, _)| !DROP_RESPONSE_HEADERS.contains(&k.as_str()))
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect()
}

fn filtered_headers(headers: &reqwest::header::HeaderMap) -> BTreeMap<String, String> {
    header_map(headers)
}

// ------------------------------------------------------------------- execution

/// How long to keep reading after a terminal websocket event.
const WS_TRAILING_MS: u64 = 250;

pub struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    pub fn new(port: u16) -> Result<Self> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Client { http, base: format!("127.0.0.1:{port}") })
    }

    pub async fn run(&self, step: &Step) -> Result<Observed> {
        match step {
            Step::Http(r) => self.http_step(r).await,
            Step::Ws(r) => self.ws_step(r).await,
            Step::Pause(_) => unreachable!("pauses are handled by the runner"),
        }
    }

    async fn http_step(&self, r: &HttpReq) -> Result<Observed> {
        let url = format!("http://{}{}", self.base, r.path);
        let method = reqwest::Method::from_bytes(r.method.as_bytes())?;
        let mut req = self.http.request(method, url).headers(request_headers(&r.auth, &r.headers)?);
        req = match &r.body {
            Body::None => req,
            Body::Json(v) => req.header("content-type", "application/json").body(v.to_string()),
            Body::Text(t) => req.header("content-type", "application/json").body(t.clone()),
            Body::Raw { content_type, bytes } if content_type.is_empty() => req.body(bytes.clone()),
            Body::Raw { content_type, bytes } => req.header("content-type", content_type.as_str()).body(bytes.clone()),
            Body::Typed(content_type, t) => req.header("content-type", *content_type).body(t.clone()),
        };
        let resp = req.send().await.with_context(|| format!("{} {}", r.method, r.path))?;
        let status = resp.status().as_u16();
        let headers = filtered_headers(resp.headers());
        let is_sse = headers.get("content-type").is_some_and(|c| c.contains("text/event-stream"));
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = vec![];
        let mut transport_error = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(b) => buf.extend_from_slice(&b),
                Err(_) => {
                    transport_error = true;
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        let body = if is_sse {
            let mut events = parse_sse(&text);
            if transport_error {
                events.push(json!({"transport_error": true}));
            }
            ObsBody::Sse { events }
        } else if buf.is_empty() && !transport_error {
            ObsBody::Empty
        } else {
            let lead_newlines = text.len() - text.trim_start_matches('\n').len();
            let mut body = match serde_json::from_str::<Value>(text.trim_matches(['\n', '\r', ' '])) {
                Ok(v) => ObsBody::Json { value: v, lead_newlines },
                Err(_) => ObsBody::Text { value: text },
            };
            if transport_error && let ObsBody::Text { value } = &mut body {
                value.push_str("<transport error>");
            }
            body
        };
        Ok(Observed { status, headers, body })
    }

    async fn ws_step(&self, r: &WsReq) -> Result<Observed> {
        let url = format!("ws://{}{}", self.base, r.path);
        let mut request = url.into_client_request()?;
        for (k, v) in request_headers(&r.auth, &r.headers)? {
            if let Some(k) = k {
                request.headers_mut().insert(k, v);
            }
        }
        let (mut socket, resp) = match tokio_tungstenite::connect_async(request).await {
            Ok(ok) => ok,
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                let status = resp.status().as_u16();
                let text = resp.body().as_ref().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
                let body = match serde_json::from_str::<Value>(&text) {
                    Ok(value) => ObsBody::Json { value, lead_newlines: 0 },
                    Err(_) if text.is_empty() => ObsBody::Empty,
                    Err(_) => ObsBody::Text { value: text },
                };
                return Ok(Observed { status, headers: header_map(resp.headers()), body });
            }
            Err(e) => return Err(e.into()),
        };
        let headers = header_map(resp.headers());
        let mut frames: Vec<Value> = vec![];
        let mut close: Option<String> = None;
        'messages: for msg in &r.messages {
            socket.send(Message::Text(msg.to_string().into())).await?;
            let mut terminal_seen = false;
            loop {
                // After a terminal event keep listening briefly for trailing frames or a close.
                let wait = if terminal_seen { WS_TRAILING_MS } else { 10_000 };
                match tokio::time::timeout(Duration::from_millis(wait), socket.next()).await {
                    Err(_) if terminal_seen => break,
                    Err(_) => {
                        frames.push(json!({"timeout": true}));
                        break 'messages;
                    }
                    Ok(None) => {
                        close = Some("eof".into());
                        break 'messages;
                    }
                    Ok(Some(Err(_))) => {
                        close = Some("abnormal".into());
                        break 'messages;
                    }
                    Ok(Some(Ok(Message::Close(frame)))) => {
                        close = Some(match frame {
                            Some(f) => format!("{} {}", u16::from(f.code), f.reason),
                            None => "no-frame".into(),
                        });
                        break 'messages;
                    }
                    Ok(Some(Ok(Message::Text(t)))) => {
                        let v = serde_json::from_str::<Value>(t.as_str()).unwrap_or_else(|_| Value::String(t.to_string()));
                        terminal_seen |= matches!(
                            v["type"].as_str(),
                            Some("response.completed" | "response.done" | "response.failed" | "response.incomplete" | "error")
                        );
                        frames.push(v);
                    }
                    Ok(Some(Ok(Message::Binary(b)))) => {
                        let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
                        frames.push(json!({"binary": hex}));
                    }
                    Ok(Some(Ok(_))) => {}
                }
            }
        }
        if close.is_none() {
            let _ = socket.send(Message::Close(None)).await;
        }
        Ok(Observed { status: 101, headers, body: ObsBody::Ws { frames, close } })
    }
}

/// Default user agent, auth headers, then the scenario's own headers (which override).
fn request_headers(auth: &Auth, extra: &[(String, String)]) -> Result<reqwest::header::HeaderMap> {
    let mut hm = reqwest::header::HeaderMap::new();
    hm.insert("user-agent", "cpa-e2e/1".parse()?);
    for (k, v) in auth_headers(auth) {
        hm.insert(reqwest::header::HeaderName::from_bytes(k.as_bytes())?, v.parse()?);
    }
    for (k, v) in extra {
        hm.insert(reqwest::header::HeaderName::from_bytes(k.as_bytes())?, v.parse()?);
    }
    Ok(hm)
}

fn auth_headers(auth: &Auth) -> Vec<(&'static str, String)> {
    match auth {
        Auth::Client => vec![("authorization", format!("Bearer {CLIENT_KEY}"))],
        Auth::None => vec![],
        Auth::Mgmt => vec![("authorization", format!("Bearer {MGMT_SECRET}"))],
        Auth::Bearer(k) => vec![("authorization", format!("Bearer {k}"))],
        Auth::RawAuthorization(v) => vec![("authorization", v.to_string())],
        Auth::XApiKey(k) => vec![("x-api-key", k.to_string())],
        Auth::GoogKey(k) => vec![("x-goog-api-key", k.to_string())],
    }
}

/// Parses an SSE body into events. Each event is a JSON object with any of `event`, `data`
/// (parsed JSON when valid, else a string), `id`, `retry`; comment lines become `{"comment": ..}`.
pub fn parse_sse(text: &str) -> Vec<Value> {
    let text = text.replace("\r\n", "\n");
    let mut events = vec![];
    let blocks: Vec<&str> = text.split("\n\n").collect();
    for (i, block) in blocks.iter().enumerate() {
        if block.is_empty() {
            continue;
        }
        let complete = i + 1 < blocks.len();
        if block.trim().is_empty() {
            // Whitespace-only residue after the last event (some servers end with a bare newline).
            events.push(json!({"trailing_whitespace": block}));
            continue;
        }
        let mut ev = Map::new();
        let mut data: Vec<&str> = vec![];
        for line in block.split('\n') {
            if let Some(c) = line.strip_prefix(':') {
                events.push(json!({"comment": c.trim_start()}));
            } else if let Some((field, value)) = line.split_once(':') {
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "data" => data.push(value),
                    other => {
                        ev.insert(other.to_string(), Value::String(value.to_string()));
                    }
                }
            } else if !line.is_empty() {
                ev.insert(line.to_string(), Value::String(String::new()));
            }
        }
        if !data.is_empty() {
            let joined = data.join("\n");
            let v = serde_json::from_str::<Value>(&joined).unwrap_or(Value::String(joined));
            ev.insert("data".into(), v);
        }
        if !complete {
            ev.insert("unterminated".into(), Value::Bool(true));
        }
        if !ev.is_empty() {
            events.push(Value::Object(ev));
        }
    }
    events
}
