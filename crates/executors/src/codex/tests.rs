//! Executor-level tests against local mock upstreams (ports of the Go codex executor tests that
//! pin stream handling, bootstrap buffering, error classification and the websocket protocol).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_runtime::executor::{ExecError, Executor, Options, Request, StreamResult, meta};
use cpa_translator::Format;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use super::{CodexExecutor, META_DOWNSTREAM_WEBSOCKET, META_REQUIRED_UPSTREAM_WEBSOCKET};
use crate::codex::terminal::BOOTSTRAP_MAX_BUFFERED_FRAMES;

const OVERLOAD: &str = r#"{"type":"error","error":{"type":"service_unavailable_error","code":"server_is_overloaded","message":"Our servers are currently overloaded. Please try again later.","param":null},"sequence_number":2}"#;
const CAPACITY: &str = r#"{"type":"error","error":{"message":"Selected model is at capacity. Please try a different model."},"sequence_number":2}"#;
const INVALID: &str = r#"{"type":"error","error":{"type":"invalid_request_error","code":"invalid_value","message":"Invalid input."},"sequence_number":2}"#;
const CREATED: &str = r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5.6-terra"}}"#;
const IN_PROGRESS: &str = r#"{"type":"response.in_progress","response":{"id":"resp_1"}}"#;
const DELTA: &str = r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"hi"}"#;
const COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}"#;

// ---------------------------------------------------------------- mock servers

#[derive(Clone, Debug)]
struct Recorded {
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct HttpReply {
    status: u16,
    body: String,
}

fn sse(events: &[&str]) -> HttpReply {
    let mut body = String::new();
    for event in events {
        let kind = event.split("\"type\":\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("message");
        body.push_str(&format!("event: {kind}\ndata: {event}\n\n"));
    }
    HttpReply { status: 200, body }
}

fn raw_sse(body: String) -> HttpReply {
    HttpReply { status: 200, body }
}

async fn read_head(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(pos + 4);
            return Some((String::from_utf8_lossy(&buf).into_owned(), rest));
        }
    }
}

fn parse_headers(head: &str) -> HashMap<String, String> {
    head.lines().skip(1).filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string())).collect()
}

/// Serves `reply(request)` for every HTTP request; returns the base URL and the recorded requests.
async fn http_server(reply: impl Fn(&Recorded) -> HttpReply + Send + Sync + 'static) -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let reply = Arc::new(reply);
    let seen = Arc::clone(&recorded);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let (reply, seen) = (Arc::clone(&reply), Arc::clone(&seen));
            tokio::spawn(async move {
                let Some((head, mut body)) = read_head(&mut stream).await else { return };
                let headers = parse_headers(&head);
                let want: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                while body.len() < want {
                    let mut chunk = [0u8; 4096];
                    match stream.read(&mut chunk).await {
                        Ok(n) if n > 0 => body.extend_from_slice(&chunk[..n]),
                        _ => break,
                    }
                }
                let request = Recorded { headers, body };
                let response = reply(&request);
                seen.lock().push(request);
                let head = format!(
                    "HTTP/1.1 {} X\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                    response.status,
                    response.body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(response.body.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (url, recorded)
}

/// What the websocket mock does after each client message.
#[derive(Clone)]
enum WsStep {
    Frames(Vec<String>),
    /// Frames, then a close frame with this code.
    FramesThenClose(Vec<String>, u16),
    /// Frames sent as permessage-deflate messages.
    Compressed(Vec<String>),
}

struct WsMock {
    url: String,
    handshakes: Arc<Mutex<Vec<HashMap<String, String>>>>,
    received: Arc<Mutex<Vec<String>>>,
    connections: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
}

fn server_frame(opcode: u8, rsv1: bool, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0x80 | if rsv1 { 0x40 } else { 0 } | opcode];
    if payload.len() < 126 {
        f.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        f.push(126);
        f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        f.push(127);
        f.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    f.extend_from_slice(payload);
    f
}

async fn read_client_text(stream: &mut TcpStream, pending: &mut Vec<u8>) -> Option<String> {
    loop {
        if pending.len() >= 2 {
            let opcode = pending[0] & 0x0f;
            let (len, header) = match pending[1] & 0x7f {
                126 if pending.len() >= 4 => (u16::from_be_bytes([pending[2], pending[3]]) as usize, 4),
                126 => (usize::MAX, 0),
                127 if pending.len() >= 10 => (u64::from_be_bytes(pending[2..10].try_into().unwrap()) as usize, 10),
                127 => (usize::MAX, 0),
                n => (n as usize, 2),
            };
            if len != usize::MAX && pending.len() >= header + 4 + len {
                let mask = [pending[header], pending[header + 1], pending[header + 2], pending[header + 3]];
                let payload: Vec<u8> = pending[header + 4..header + 4 + len].iter().enumerate().map(|(i, b)| b ^ mask[i % 4]).collect();
                pending.drain(..header + 4 + len);
                if opcode == 0x8 {
                    return None;
                }
                if opcode == 0x1 {
                    return Some(String::from_utf8_lossy(&payload).into_owned());
                }
                continue;
            }
        }
        let mut chunk = [0u8; 8192];
        match stream.read(&mut chunk).await {
            Ok(n) if n > 0 => pending.extend_from_slice(&chunk[..n]),
            _ => return None,
        }
    }
}

fn deflate(text: &str) -> Vec<u8> {
    let mut c = flate2::Compress::new(flate2::Compression::default(), false);
    let mut out = Vec::with_capacity(text.len() + 64);
    c.compress_vec(text.as_bytes(), &mut out, flate2::FlushCompress::Sync).unwrap();
    out.truncate(out.len() - 4);
    out
}

/// Websocket upstream: replies to the n-th client message (across connections) with `script[n]`
/// (the last step repeats). `reject` answers the upgrade with a plain HTTP response instead.
async fn ws_server(script: Vec<WsStep>, reject: Option<(u16, &'static str)>) -> WsMock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock = WsMock {
        url: format!("http://{}", listener.local_addr().unwrap()),
        handshakes: Arc::default(),
        received: Arc::default(),
        connections: Arc::default(),
        closed: Arc::default(),
    };
    let (handshakes, received, connections, closed) =
        (Arc::clone(&mock.handshakes), Arc::clone(&mock.received), Arc::clone(&mock.connections), Arc::clone(&mock.closed));
    let counter = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let (script, handshakes, received, connections, closed, counter) =
                (script.clone(), Arc::clone(&handshakes), Arc::clone(&received), Arc::clone(&connections), Arc::clone(&closed), Arc::clone(&counter));
            tokio::spawn(async move {
                let Some((head, mut pending)) = read_head(&mut stream).await else { return };
                let headers = parse_headers(&head);
                handshakes.lock().push(headers.clone());
                if let Some((status, body)) = reject {
                    let response = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = stream.write_all(response.as_bytes()).await;
                    return;
                }
                connections.fetch_add(1, Ordering::SeqCst);
                let key = headers.get("sec-websocket-key").cloned().unwrap_or_default();
                let accept = base64::engine::general_purpose::STANDARD.encode(Sha1::digest(format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11")));
                let compress = script.iter().any(|s| matches!(s, WsStep::Compressed(_)));
                let extension = if compress { "Sec-WebSocket-Extensions: permessage-deflate; server_no_context_takeover; client_no_context_takeover\r\n" } else { "" };
                let response = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n{extension}\r\n");
                if stream.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
                while let Some(message) = read_client_text(&mut stream, &mut pending).await {
                    received.lock().push(message);
                    let index = counter.fetch_add(1, Ordering::SeqCst).min(script.len() - 1);
                    let (frames, compressed, close) = match &script[index] {
                        WsStep::Frames(f) => (f.clone(), false, None),
                        WsStep::FramesThenClose(f, code) => (f.clone(), false, Some(*code)),
                        WsStep::Compressed(f) => (f.clone(), true, None),
                    };
                    for frame in frames {
                        let bytes = if compressed { server_frame(0x1, true, &deflate(&frame)) } else { server_frame(0x1, false, frame.as_bytes()) };
                        if stream.write_all(&bytes).await.is_err() {
                            return;
                        }
                    }
                    if let Some(code) = close {
                        let _ = stream.write_all(&server_frame(0x8, false, &code.to_be_bytes())).await;
                        let _ = stream.shutdown().await;
                        return;
                    }
                }
                closed.fetch_add(1, Ordering::SeqCst);
            });
        }
    });
    mock
}

// ---------------------------------------------------------------- fixtures

fn executor(cfg: Config) -> (CodexExecutor, watch::Sender<Arc<Config>>) {
    let (tx, rx) = watch::channel(Arc::new(cfg));
    (CodexExecutor { cfg: rx, api_key_scope: false }, tx)
}

fn buffering(on: bool) -> Config {
    let mut cfg = Config::default();
    cfg.codex.stream_bootstrap_buffering = on;
    cfg
}

fn api_key_auth(base_url: &str, websockets: bool) -> Auth {
    let mut auth = Auth::new("codex-key-1", "codex");
    auth.attributes.insert("api_key".into(), "test".into());
    auth.attributes.insert("base_url".into(), base_url.into());
    if websockets {
        auth.attributes.insert("websockets".into(), "true".into());
    }
    auth
}

fn request(payload: &str, stream: bool) -> (Request, Options) {
    let req = Request { model: "gpt-5.6-terra".into(), payload: Bytes::from(payload.to_string()), format: Format::OpenAIResponse, metadata: Default::default() };
    let mut opts = Options::new(Format::OpenAIResponse);
    opts.stream = stream;
    (req, opts)
}

const HELLO: &str = r#"{"model":"gpt-5.6-terra","input":"hello"}"#;
const HELLO_ITEMS: &str = r#"{"model":"gpt-5.6-terra","input":[{"type":"message","role":"user","content":"hello"}]}"#;

fn ws_opts(session: &str) -> Options {
    let (_, mut opts) = request(HELLO_ITEMS, true);
    opts.metadata.insert(META_DOWNSTREAM_WEBSOCKET.into(), json!(true));
    if !session.is_empty() {
        opts.metadata.insert(meta::EXECUTION_SESSION_ID.into(), json!(session));
    }
    opts
}

async fn drain(result: StreamResult) -> (String, Option<ExecError>) {
    let mut rx = result.chunks;
    let mut payloads = Vec::new();
    let mut err = None;
    while let Some(chunk) = rx.recv().await {
        match chunk {
            Ok(bytes) => payloads.push(String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) => {
                err.get_or_insert(e);
            }
        }
    }
    (payloads.join("\n"), err)
}

async fn stream_http(cfg: Config, reply: HttpReply) -> Result<StreamResult, ExecError> {
    let reply = Arc::new(Mutex::new(Some(reply)));
    let (url, _) = http_server(move |_| {
        let r = reply.lock().take().unwrap_or(HttpReply { status: 200, body: String::new() });
        HttpReply { status: r.status, body: r.body }
    })
    .await;
    let (exec, _keep) = executor(cfg);
    let (req, opts) = request(HELLO, true);
    exec.execute_stream(&api_key_auth(&url, false), req, opts).await
}

// ---------------------------------------------------------------- HTTP: bootstrap buffering

#[tokio::test]
async fn bootstrap_overload_fails_the_attempt_without_leaking_the_handshake() {
    let err = stream_http(buffering(true), sse(&[CREATED, IN_PROGRESS, OVERLOAD])).await.err().expect("attempt must fail");
    assert_eq!(err.status, 503);
    let err = stream_http(buffering(true), sse(&[CREATED, IN_PROGRESS, CAPACITY])).await.err().expect("capacity must fail the attempt");
    assert_eq!(err.status, 429);
}

#[tokio::test]
async fn bootstrap_non_overload_failure_is_delivered_after_the_flushed_handshake() {
    let result = stream_http(buffering(true), sse(&[CREATED, IN_PROGRESS, INVALID])).await.unwrap();
    let (combined, err) = drain(result).await;
    assert!(combined.contains(r#""type":"response.created""#), "{combined}");
    assert_eq!(err.expect("in-stream error").status, 400);
}

#[tokio::test]
async fn bootstrap_buffer_limit_releases_the_stream() {
    let mut events: Vec<String> = (0..BOOTSTRAP_MAX_BUFFERED_FRAMES + 1).map(|i| format!(r#"{{"type":"response.in_progress","response":{{"id":"resp_{i}"}}}}"#)).collect();
    events.push(OVERLOAD.to_string());
    let refs: Vec<&str> = events.iter().map(String::as_str).collect();
    let result = stream_http(buffering(true), sse(&refs)).await.expect("stream released once the budget is exhausted");
    let (_, err) = drain(result).await;
    assert!(err.is_some(), "overload is delivered in-stream after release");
}

#[tokio::test]
async fn bootstrap_budget_boundary_is_exact() {
    let overload = format!("data: {OVERLOAD}\n\n");
    for (keepalives, fails_over) in [(BOOTSTRAP_MAX_BUFFERED_FRAMES - 1, true), (BOOTSTRAP_MAX_BUFFERED_FRAMES, true), (BOOTSTRAP_MAX_BUFFERED_FRAMES + 1, false)] {
        let body = format!("{}{overload}", ": keepalive\n".repeat(keepalives));
        let result = stream_http(buffering(true), raw_sse(body)).await;
        assert_eq!(result.is_err(), fails_over, "{keepalives} held lines");
    }
}

#[tokio::test]
async fn bootstrap_empty_data_frames_keep_the_window_open() {
    for frame in ["data:\n\n", "data: \n\n", "data:   \t\n\n"] {
        let result = stream_http(buffering(true), raw_sse(format!("{frame}data: {OVERLOAD}\n\n"))).await;
        assert!(result.is_err(), "empty data frame {frame:?} must not release the stream");
    }
}

#[tokio::test]
async fn bootstrap_content_frame_releases_before_a_later_overload() {
    let result = stream_http(buffering(true), sse(&[CREATED, DELTA, OVERLOAD])).await.expect("content commits the stream");
    let (combined, err) = drain(result).await;
    assert!(combined.contains("response.output_text.delta"), "{combined}");
    assert!(err.is_some());
}

#[tokio::test]
async fn disabled_buffering_passes_the_overload_through_the_stream() {
    let result = stream_http(buffering(false), sse(&[CREATED, OVERLOAD])).await.expect("no bootstrap probing");
    let (combined, err) = drain(result).await;
    assert!(combined.contains("response.created"));
    assert_eq!(err.expect("error chunk").status, 502);
}

// ---------------------------------------------------------------- HTTP: stream and execute

#[tokio::test]
async fn stream_without_completion_is_a_request_scoped_408() {
    let result = stream_http(buffering(false), sse(&[CREATED])).await.unwrap();
    let (_, err) = drain(result).await;
    let err = err.expect("missing completion");
    assert_eq!(err.status, 408);
    assert!(err.is_request_scoped());
}

#[tokio::test]
async fn stream_terminal_failure_is_an_error_chunk_after_prior_output() {
    let result = stream_http(buffering(false), sse(&[CREATED, INVALID])).await.unwrap();
    let (combined, err) = drain(result).await;
    assert!(combined.contains("response.created"));
    let err = err.expect("terminal failure");
    assert_eq!(err.status, 400);
    assert!(!err.is_request_scoped());
}

#[tokio::test]
async fn stream_completion_ends_the_stream_and_reports_usage() {
    let result = stream_http(buffering(false), sse(&[CREATED, DELTA, COMPLETED])).await.unwrap();
    let usage = result.usage;
    let (combined, err) = drain(StreamResult { headers: result.headers, chunks: result.chunks, usage: None }).await;
    assert!(err.is_none(), "{err:?}");
    assert!(combined.contains("response.completed"));
    let usage = usage.expect("usage receiver").await.unwrap();
    assert_eq!(usage["input_tokens"], 1);
    assert_eq!(usage["total_tokens"], 2);
}

async fn execute_http_body(body: &str) -> Result<cpa_runtime::executor::Response, ExecError> {
    let (url, _) = {
        let body = body.to_string();
        http_server(move |_| raw_sse(body.clone())).await
    };
    let (exec, _keep) = executor(Config::default());
    let (req, opts) = request(HELLO, false);
    exec.execute(&api_key_auth(&url, false), req, opts).await
}

#[tokio::test]
async fn execute_hydrates_missing_item_ids_from_output_item_done() {
    let body = concat!(
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"fc_123\",\"type\":\"function_call\",\"call_id\":\"call_123\",\"name\":\"weather\",\"arguments\":\"{}\"},\"output_index\":0}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[{\"id\":null,\"type\":\"function_call\",\"call_id\":\"call_123\",\"name\":\"weather-terminal\",\"arguments\":\"{}\"},{\"id\":\"fc_existing\",\"type\":\"function_call\",\"call_id\":\"call_existing\",\"name\":\"preserved\",\"arguments\":\"{}\"}]}}\n\n"
    );
    let resp = execute_http_body(body).await.unwrap();
    let out: Value = serde_json::from_slice(&resp.payload).unwrap();
    assert_eq!(out["output"][0]["id"], "fc_123");
    assert_eq!(out["output"][0]["name"], "weather-terminal");
    assert_eq!(out["output"][1]["id"], "fc_existing");
}

#[tokio::test]
async fn execute_rebuilds_empty_output_from_output_item_done_and_reports_usage() {
    let body = format!(
        "data: {}\n\ndata: {}\n\n",
        r#"{"type":"response.output_item.done","item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]},"output_index":0}"#,
        r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}"#
    );
    let resp = execute_http_body(&body).await.unwrap();
    let out: Value = serde_json::from_slice(&resp.payload).unwrap();
    assert_eq!(out["output"][0]["id"], "msg_1");
    assert_eq!(resp.metadata["usage"]["total_tokens"], 6);
}

#[tokio::test]
async fn execute_without_completion_is_a_request_scoped_408() {
    let err = execute_http_body(&format!("data: {CREATED}\n\n")).await.unwrap_err();
    assert_eq!(err.status, 408);
    assert!(err.is_request_scoped());
}

#[tokio::test]
async fn execute_incomplete_response_is_a_success() {
    let body = format!("data: {}\n\n", r#"{"type":"response.incomplete","response":{"id":"resp_1","status":"incomplete","output":[{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":"part"}]}],"usage":{"input_tokens":1,"output_tokens":3,"total_tokens":4}}}"#);
    let resp = execute_http_body(&body).await.unwrap();
    assert!(String::from_utf8_lossy(&resp.payload).contains("part"));
}

#[tokio::test]
async fn execute_empty_incomplete_is_a_request_scoped_502() {
    let body = format!("data: {}\n\n", r#"{"type":"response.incomplete","response":{"id":"resp_1","output":[],"usage":{"input_tokens":1,"output_tokens":0,"total_tokens":1}}}"#);
    let err = execute_http_body(&body).await.unwrap_err();
    assert_eq!(err.status, 502);
    assert!(err.is_request_scoped());
}

#[tokio::test]
async fn upstream_usage_limit_is_a_credential_scoped_429_with_reset_delay() {
    let (url, _) = http_server(|_| HttpReply { status: 429, body: r#"{"error":{"type":"usage_limit_reached","message":"limit","resets_in_seconds":90}}"#.into() }).await;
    let (exec, _keep) = executor(Config::default());
    let (req, opts) = request(HELLO, false);
    let err = exec.execute(&api_key_auth(&url, false), req, opts).await.unwrap_err();
    assert_eq!(err.status, 429);
    assert!(err.credential_scoped);
    assert_eq!(err.retry_after, Some(Duration::from_secs(90)));

    let mut cfg = Config::default();
    cfg.codex.model_level_cooling = true;
    let (exec, _keep) = executor(cfg);
    let (req, opts) = request(HELLO, false);
    let err = exec.execute(&api_key_auth(&url, false), req, opts).await.unwrap_err();
    assert!(!err.credential_scoped, "model-level cooling scopes the cooldown to the model");
}

#[tokio::test]
async fn upstream_auth_failure_is_normalized() {
    let (url, _) = http_server(|_| HttpReply { status: 401, body: "Unauthorized".into() }).await;
    let (exec, _keep) = executor(Config::default());
    let (req, opts) = request(HELLO, false);
    let err = exec.execute(&api_key_auth(&url, false), req, opts).await.unwrap_err();
    assert_eq!(err.status, 401);
    assert!(err.message.contains(r#""code":"auth_unavailable""#), "{}", err.message);
}

#[tokio::test]
async fn http_request_carries_cloaked_identity_and_stream_fields() {
    let (url, recorded) = http_server(|_| sse(&[CREATED, COMPLETED])).await;
    let (exec, _keep) = executor(Config::default());
    let (req, mut opts) = request(HELLO, true);
    opts.metadata.insert(meta::EXECUTION_SESSION_ID.into(), json!("http-session-1"));
    let result = exec.execute_stream(&api_key_auth(&url, false), req, opts).await.unwrap();
    drain(result).await;
    let seen = recorded.lock();
    let request = &seen[0];
    assert_eq!(request.headers["authorization"], "Bearer test");
    assert_eq!(request.headers["accept"], "text/event-stream");
    assert_eq!(request.headers["user-agent"], super::headers::USER_AGENT);
    assert_eq!(request.headers["originator"], super::headers::ORIGINATOR);
    assert!(request.headers.contains_key("session-id"));
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["model"], "gpt-5.6-terra");
    assert_eq!(body["instructions"], "");
    assert_eq!(body["prompt_cache_key"], request.headers["session-id"]);
}

// ---------------------------------------------------------------- websocket

async fn ws_stream(exec: &CodexExecutor, auth: &Auth, session: &str) -> Result<StreamResult, ExecError> {
    let (req, _) = request(HELLO_ITEMS, true);
    exec.execute_stream(auth, req, ws_opts(session)).await
}

fn frames(events: &[&str]) -> WsStep {
    WsStep::Frames(events.iter().map(|s| s.to_string()).collect())
}

#[tokio::test]
async fn websocket_stream_passes_frames_through_and_sends_a_response_create() {
    let mock = ws_server(vec![frames(&[CREATED, DELTA, COMPLETED])], None).await;
    let (exec, _keep) = executor(Config::default());
    let result = ws_stream(&exec, &api_key_auth(&mock.url, true), "passthrough-session").await.unwrap();
    let (combined, err) = drain(result).await;
    assert!(err.is_none(), "{err:?}");
    let lines: Vec<&str> = combined.lines().collect();
    assert_eq!(lines.len(), 3, "{combined}");
    assert!(lines[0].contains("response.created") && lines[2].contains("response.completed"));

    let received = mock.received.lock();
    let sent: Value = serde_json::from_str(&received[0]).unwrap();
    assert_eq!(sent["type"], "response.create");
    assert_eq!(sent["model"], "gpt-5.6-terra");
    let handshake = &mock.handshakes.lock()[0];
    assert_eq!(handshake["openai-beta"], "responses_websockets=2026-02-06");
    assert_eq!(handshake["authorization"], "Bearer test");
    assert!(handshake.contains_key("session_id") && handshake.contains_key("sec-websocket-extensions"));
}

#[tokio::test]
async fn websocket_session_reuses_its_connection_and_closing_the_session_closes_it() {
    let mock = ws_server(vec![frames(&[CREATED, COMPLETED])], None).await;
    let (exec, _keep) = executor(Config::default());
    let auth = api_key_auth(&mock.url, true);
    for _ in 0..2 {
        let result = ws_stream(&exec, &auth, "client-session-1").await.unwrap();
        let (_, err) = drain(result).await;
        assert!(err.is_none());
    }
    assert_eq!(mock.connections.load(Ordering::SeqCst), 1, "one upstream socket per client session");
    assert_eq!(mock.received.lock().len(), 2);
    exec.close_execution_session("client-session-1").await;
    for _ in 0..50 {
        if mock.closed.load(Ordering::SeqCst) == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream socket was not closed with the session");
}

#[tokio::test]
async fn websocket_without_session_id_uses_a_fresh_connection_per_request() {
    let mock = ws_server(vec![frames(&[CREATED, COMPLETED])], None).await;
    let (exec, _keep) = executor(Config::default());
    let auth = api_key_auth(&mock.url, true);
    for _ in 0..2 {
        drain(ws_stream(&exec, &auth, "").await.unwrap()).await;
    }
    assert_eq!(mock.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn required_upstream_websocket_without_a_live_connection_needs_replay() {
    let mock = ws_server(vec![frames(&[CREATED, COMPLETED])], None).await;
    let (exec, _keep) = executor(Config::default());
    let auth = api_key_auth(&mock.url, true);
    let (req, _) = request(HELLO_ITEMS, true);
    let mut opts = ws_opts("replay-session");
    opts.metadata.insert(META_REQUIRED_UPSTREAM_WEBSOCKET.into(), json!(true));
    let err = exec.execute_stream(&auth, req.clone(), opts.clone()).await.err().expect("replay required");
    assert!(super::is_upstream_websocket_replay_required(&err));
    assert!(err.is_request_scoped() && !err.upstream_attempted);
    assert_eq!(mock.connections.load(Ordering::SeqCst), 0, "no dial for a continuation");

    // Once the session holds a live connection the continuation reuses it.
    drain(ws_stream(&exec, &auth, "replay-session").await.unwrap()).await;
    let result = exec.execute_stream(&auth, req, opts).await.unwrap();
    assert!(drain(result).await.1.is_none());
    assert_eq!(mock.connections.load(Ordering::SeqCst), 1);

    // A non-websocket credential cannot serve it either.
    let http_auth = api_key_auth(&mock.url, false);
    let (req, _) = request(HELLO_ITEMS, true);
    let mut opts = ws_opts("replay-session");
    opts.metadata.insert(META_REQUIRED_UPSTREAM_WEBSOCKET.into(), json!(true));
    let err = exec.execute_stream(&http_auth, req, opts).await.err().expect("http fallback rejected");
    assert_eq!(err.status, 426);
}

#[tokio::test]
async fn websocket_handshake_rejections_are_classified() {
    let mock = ws_server(vec![frames(&[])], Some((426, "upgrade please"))).await;
    let (exec, _keep) = executor(Config::default());
    let err = ws_stream(&exec, &api_key_auth(&mock.url, true), "").await.err().unwrap();
    assert_eq!(err.status, 426);
    assert_eq!(err.message, "upgrade please");

    let mock = ws_server(vec![frames(&[])], Some((429, r#"{"error":{"type":"usage_limit_reached","resets_in_seconds":30}}"#))).await;
    let err = ws_stream(&exec, &api_key_auth(&mock.url, true), "").await.err().unwrap();
    assert_eq!(err.status, 429);
    assert!(err.credential_scoped);
    assert_eq!(err.retry_after, Some(Duration::from_secs(30)));
}

#[tokio::test]
async fn websocket_error_frame_is_a_status_error_and_drops_the_connection() {
    let frame = r#"{"type":"error","status":429,"error":{"type":"rate_limit_error","message":"slow"},"headers":{"x-codex-primary-used-percent":"97"}}"#;
    let mock = ws_server(vec![frames(&[frame]), frames(&[CREATED, COMPLETED])], None).await;
    let (exec, _keep) = executor(Config::default());
    let auth = api_key_auth(&mock.url, true);
    let result = ws_stream(&exec, &auth, "err-session").await.unwrap();
    let (_, err) = drain(result).await;
    let err = err.expect("error frame");
    assert_eq!(err.status, 429);
    assert_eq!(err.headers.get("x-codex-primary-used-percent").unwrap(), "97");
    // The connection was invalidated: the next request on the session redials.
    drain(ws_stream(&exec, &auth, "err-session").await.unwrap()).await;
    assert_eq!(mock.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn websocket_close_1009_maps_to_message_too_big() {
    let mock = ws_server(vec![WsStep::FramesThenClose(vec![CREATED.to_string()], 1009)], None).await;
    let (exec, _keep) = executor(Config::default());
    let result = ws_stream(&exec, &api_key_auth(&mock.url, true), "").await.unwrap();
    let (_, err) = drain(result).await;
    let err = err.expect("close error");
    assert_eq!(err.status, 413);
    assert!(err.is_request_scoped());
    assert!(err.message.contains("message_too_big"));
}

#[tokio::test]
async fn websocket_accepts_permessage_deflate_frames() {
    let mock = ws_server(vec![WsStep::Compressed(vec![CREATED.to_string(), DELTA.to_string(), COMPLETED.to_string()])], None).await;
    let (exec, _keep) = executor(Config::default());
    let result = ws_stream(&exec, &api_key_auth(&mock.url, true), "").await.unwrap();
    let (combined, err) = drain(result).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(combined.lines().count(), 3);
    assert!(combined.contains(r#""delta":"hi""#));
}

#[tokio::test]
async fn websocket_bootstrap_buffering_fails_over_on_overload_and_flushes_otherwise() {
    let mock = ws_server(vec![frames(&[CREATED, IN_PROGRESS, OVERLOAD])], None).await;
    let (exec, _keep) = executor(buffering(true));
    let err = ws_stream(&exec, &api_key_auth(&mock.url, true), "").await.err().expect("overload fails the attempt");
    assert_eq!(err.status, 503);

    let mock = ws_server(vec![frames(&[CREATED, IN_PROGRESS, INVALID])], None).await;
    let result = ws_stream(&exec, &api_key_auth(&mock.url, true), "").await.unwrap();
    let (combined, err) = drain(result).await;
    assert!(combined.contains("response.created"));
    assert_eq!(err.expect("in-stream").status, 400);

    let mock = ws_server(vec![frames(&[CREATED, IN_PROGRESS, DELTA, COMPLETED])], None).await;
    let result = ws_stream(&exec, &api_key_auth(&mock.url, true), "").await.unwrap();
    let (combined, err) = drain(result).await;
    assert!(err.is_none());
    let order: Vec<usize> = ["response.created", "response.in_progress", "output_text.delta", "response.completed"].iter().map(|k| combined.find(k).unwrap()).collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "buffered frames flush in order: {combined}");
}

#[tokio::test]
async fn websocket_bootstrap_frame_budget_releases_the_stream() {
    let mut events: Vec<&str> = vec![IN_PROGRESS; BOOTSTRAP_MAX_BUFFERED_FRAMES + 1];
    events.push(OVERLOAD);
    let mock = ws_server(vec![frames(&events)], None).await;
    let (exec, _keep) = executor(buffering(true));
    let result = ws_stream(&exec, &api_key_auth(&mock.url, true), "").await.expect("budget exhausted before the overload");
    let (_, err) = drain(result).await;
    assert!(err.is_some());
}

#[tokio::test]
async fn websocket_execute_returns_the_translated_completion() {
    let mock = ws_server(vec![frames(&[CREATED, DELTA, COMPLETED])], None).await;
    let (exec, _keep) = executor(Config::default());
    let (req, _) = request(HELLO_ITEMS, false);
    let mut opts = ws_opts("");
    opts.stream = false;
    let resp = exec.execute(&api_key_auth(&mock.url, true), req, opts).await.unwrap();
    let out: Value = serde_json::from_slice(&resp.payload).unwrap();
    assert_eq!(out["output"][0]["content"][0]["text"], "hello");
    assert_eq!(resp.metadata["usage"]["total_tokens"], 2);
}

#[tokio::test]
async fn websocket_requires_the_downstream_flag_and_a_websocket_credential() {
    let mock = ws_server(vec![frames(&[CREATED, COMPLETED])], None).await;
    let (http_url, recorded) = http_server(|_| sse(&[CREATED, COMPLETED])).await;
    let (exec, _keep) = executor(Config::default());
    // Credential without `websockets`: HTTP even for websocket clients.
    let mut auth = api_key_auth(&http_url, false);
    drain(ws_stream(&exec, &auth, "s").await.unwrap()).await;
    assert_eq!(recorded.lock().len(), 1);
    // No downstream websocket: HTTP even when the credential enables websockets.
    auth = api_key_auth(&http_url, true);
    let (req, mut opts) = request(HELLO, true);
    opts.metadata.clear();
    drain(exec.execute_stream(&auth, req, opts).await.unwrap()).await;
    assert_eq!(recorded.lock().len(), 2);
    assert_eq!(mock.connections.load(Ordering::SeqCst), 0);
}
