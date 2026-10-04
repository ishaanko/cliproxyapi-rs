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

/// Every upstream websocket frame is also handed to the plugin observer (Go:
/// `EmitWebSocketResponseEvent`).
#[tokio::test]
async fn websocket_frames_reach_the_plugin_observer() {
    let mock = ws_server(vec![frames(&[CREATED, DELTA, COMPLETED])], None).await;
    let (exec, _keep) = executor(Config::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let mut opts = ws_opts("observer-session");
    opts.websocket_response_observer =
        Some(cpa_runtime::executor::WebSocketResponseObserver(Arc::new(move |e| sink.lock().push((e.event_type, e.provider, e.auth_id)))));
    let (req, _) = request(HELLO_ITEMS, true);
    drain(exec.execute_stream(&api_key_auth(&mock.url, true), req, opts).await.unwrap()).await;
    let events: Vec<String> = seen.lock().iter().map(|e| e.0.clone()).collect();
    assert_eq!(events, ["response.created", "response.output_text.delta", "response.completed"]);
    assert!(seen.lock().iter().all(|e| e.1 == "codex" && e.2 == "codex-key-1"));
}

// ---------------------------------------------------------------- duplex steering

fn created(id: &str, parent: &str) -> String {
    format!(r#"{{"type":"response.created","response":{{"id":"{id}","previous_response_id":"{parent}"}}}}"#)
}

fn completed(id: &str) -> String {
    format!(r#"{{"type":"response.completed","response":{{"id":"{id}","status":"completed","output":[]}}}}"#)
}

fn steering_executor() -> (CodexExecutor, watch::Sender<Arc<Config>>) {
    let mut cfg = Config::default();
    cfg.codex.response_steering = true;
    executor(cfg)
}

/// Opens a duplex stream whose downstream frames are pushed through the returned sender.
async fn duplex_stream(exec: &CodexExecutor, mock: &WsMock) -> (StreamResult, tokio::sync::mpsc::Sender<Result<Vec<u8>, ExecError>>) {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let mut opts = ws_opts("duplex-session");
    opts.ws_input = Some(cpa_runtime::executor::WebsocketInput::new(rx));
    let (req, _) = request(HELLO_ITEMS, true);
    let result = exec.execute_stream(&api_key_auth(&mock.url, true), req, opts).await.expect("duplex stream");
    (result, tx)
}

async fn next_chunk(result: &mut StreamResult) -> Result<String, ExecError> {
    let chunk = tokio::time::timeout(Duration::from_secs(5), result.chunks.recv()).await.expect("chunk in time");
    chunk.expect("stream still open").map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// A completion does not end the stream: steering and explicit creates reach the same socket,
/// acknowledgements pass through untouched and automatic successors are relayed.
#[tokio::test]
async fn duplex_stream_outlives_completion_and_forwards_steering_and_creates() {
    let accepted = r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"},"sequence_number":7}"#;
    let mock = ws_server(
        vec![
            frames(&[&created("r1", ""), &completed("r1")]),
            frames(&[accepted, &created("r2", "r1"), &completed("r2")]),
            frames(&[&created("r3", "r2"), &completed("r3")]),
        ],
        None,
    )
    .await;
    let (exec, _keep) = steering_executor();
    let (mut result, input) = duplex_stream(&exec, &mock).await;

    assert!(next_chunk(&mut result).await.unwrap().contains(r#""id":"r1""#));
    assert!(next_chunk(&mut result).await.unwrap().contains("response.completed"));

    let steer = br#"{"type":"response.steer","previous_response_id":"r1","input":"Use the tool result"}"#;
    input.send(Ok(steer.to_vec())).await.unwrap();
    // The acknowledgement is relayed byte for byte, then the automatic successor follows.
    assert_eq!(next_chunk(&mut result).await.unwrap(), accepted);
    assert!(next_chunk(&mut result).await.unwrap().contains(r#""id":"r2""#));
    assert!(next_chunk(&mut result).await.unwrap().contains("response.completed"));

    // Local validation answers without touching the upstream.
    input.send(Ok(b"not json".to_vec())).await.unwrap();
    assert_eq!(
        next_chunk(&mut result).await.unwrap(),
        r#"{"error":{"message":"invalid websocket request JSON","type":"invalid_request_error"},"status":400,"type":"error"}"#
    );
    input.send(Ok(br#"{"type":"response.bogus"}"#.to_vec())).await.unwrap();
    assert!(next_chunk(&mut result).await.unwrap().contains("unsupported websocket request type: response.bogus"));

    // An explicit continuation is prepared like any create and keeps its parent id.
    let create = br#"{"type":"response.create","previous_response_id":"r2","input":[{"type":"function_call_output","call_id":"c1","output":"ok"}]}"#;
    input.send(Ok(create.to_vec())).await.unwrap();
    assert!(next_chunk(&mut result).await.unwrap().contains(r#""id":"r3""#));
    assert!(next_chunk(&mut result).await.unwrap().contains("response.completed"));

    let received = mock.received.lock().clone();
    assert_eq!(received.len(), 3, "{received:?}");
    assert_eq!(received[1].as_bytes(), steer, "steering bypasses every create translation");
    let forwarded: Value = serde_json::from_str(&received[2]).unwrap();
    assert_eq!(forwarded["type"], "response.create");
    assert_eq!(forwarded["previous_response_id"], "r2");
    assert_eq!(forwarded["input"][0]["call_id"], "c1");
    assert_eq!(forwarded["model"], "gpt-5.6-terra");
    assert_eq!(mock.connections.load(Ordering::SeqCst), 1);

    // Dropping the downstream ends the stream and releases the socket.
    drop(input);
    drop(result);
    for _ in 0..100 {
        if mock.closed.load(Ordering::SeqCst) == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("upstream socket was not closed");
}

fn completed_with_usage(id: &str, input: i64, output: i64) -> String {
    format!(
        r#"{{"type":"response.completed","response":{{"id":"{id}","status":"completed","output":[],"usage":{{"input_tokens":{input},"output_tokens":{output},"total_tokens":{}}}}}}}"#,
        input + output
    )
}

/// Go publishes one usage record per response of a steering socket (a new reporter on every
/// later `response.created`), and a client that goes away closes the stream without an error or
/// a failed record.
#[tokio::test]
async fn duplex_socket_records_usage_per_response_and_client_close_is_not_a_failure() {
    let accepted = r#"{"type":"response.steer.accepted","steer":{"id":"s1","previous_response_id":"r1"},"sequence_number":7}"#;
    let mock = ws_server(
        vec![
            frames(&[&created("r1", ""), &completed_with_usage("r1", 3, 4)]),
            frames(&[accepted, &created("r2", "r1"), &completed_with_usage("r2", 5, 6)]),
        ],
        None,
    )
    .await;
    let (exec, _keep) = steering_executor();
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let collector = cpa_runtime::usage_report::UsageCollector::new();
    let mut opts = ws_opts("duplex-usage");
    opts.ws_input = Some(cpa_runtime::executor::WebsocketInput::new(rx));
    opts.usage_collector = Some(collector.clone());
    let (req, _) = request(HELLO_ITEMS, true);
    let mut result = exec.execute_stream(&api_key_auth(&mock.url, true), req, opts).await.expect("duplex stream");

    assert!(next_chunk(&mut result).await.unwrap().contains(r#""id":"r1""#));
    assert!(next_chunk(&mut result).await.unwrap().contains("response.completed"));
    tx.send(Ok(br#"{"type":"response.steer","previous_response_id":"r1","input":"go"}"#.to_vec())).await.unwrap();
    assert_eq!(next_chunk(&mut result).await.unwrap(), accepted);
    assert!(next_chunk(&mut result).await.unwrap().contains(r#""id":"r2""#));
    assert!(next_chunk(&mut result).await.unwrap().contains("response.completed"));

    let records = collector.take();
    let tokens: Vec<_> = records.iter().map(|r| (r.detail.input_tokens, r.detail.output_tokens, r.failed)).collect();
    assert_eq!(tokens, vec![(3, 4, false), (5, 6, false)]);
    assert_ne!(records[0].request_id, records[1].request_id);
    assert!(records.iter().all(|r| r.executor_type == "CodexWebsocketsExecutor"));

    // The client goes away: the stream just ends.
    drop(tx);
    let end = tokio::time::timeout(Duration::from_secs(5), result.chunks.recv()).await.expect("stream ends");
    assert!(end.is_none(), "client close must not surface an error: {end:?}");
    assert!(collector.take().is_empty(), "client close must not publish a failure record");
}

/// A response created with no pending create and no retained parent settings cannot be
/// attributed to any request: the stream fails as a request-scoped connection error.
#[tokio::test]
async fn duplex_unattributable_automatic_response_is_a_request_scoped_failure() {
    let mock = ws_server(vec![frames(&[&created("r1", ""), &completed("r1"), &created("r9", "ghost")])], None).await;
    let (exec, _keep) = steering_executor();
    let (mut result, _input) = duplex_stream(&exec, &mock).await;
    assert!(next_chunk(&mut result).await.unwrap().contains(r#""id":"r1""#));
    assert!(next_chunk(&mut result).await.unwrap().contains("response.completed"));
    let err = next_chunk(&mut result).await.unwrap_err();
    assert!(err.message.contains("automatic successor has no retained parent settings"), "{}", err.message);
    assert!(err.is_request_scoped());
}

// ---------------------------------------------------------------- direct OpenAI images

fn image_options(stream: bool) -> Options {
    let mut opts = Options::new(Format::OpenAI);
    opts.stream = stream;
    opts.metadata.insert(crate::openai_compat::META_HANDLER_TYPE.into(), json!("openai-image"));
    opts.metadata.insert("request_path".into(), json!("/v1/images/generations"));
    opts
}

/// Go `TestCodexExecutorDirectOpenAIImageGenerationUsesImagesEndpoint`: the request goes to
/// `/images/generations` with the Codex headers, and the usage object of the answer is reported.
#[tokio::test]
async fn direct_image_generation_uses_images_endpoint_and_reports_usage() {
    let upstream = r#"{"created":1713833628,"data":[{"b64_json":"AA=="}],"usage":{"total_tokens":100,"input_tokens":50,"output_tokens":50}}"#;
    let (url, recorded) = http_server(move |_| HttpReply { status: 200, body: upstream.to_string() }).await;
    let (exec, _keep) = executor(Config::default());
    let req = Request {
        model: "codex/gpt-image-1.5".into(),
        payload: Bytes::from_static(br#"{"model":"codex/gpt-image-1.5","prompt":"A cute baby sea otter","n":1,"extra":{"preserve":true},"stream":false}"#),
        format: Format::OpenAI,
        metadata: Default::default(),
    };
    let mut opts = image_options(false);
    opts.headers.insert("version", "0.135.0".parse().unwrap());
    opts.headers.insert("x-client-request-id", "client-request-1".parse().unwrap());
    let resp = exec.execute(&api_key_auth(&url, false), req, opts).await.unwrap();

    assert_eq!(&resp.payload[..], upstream.as_bytes());
    assert_eq!(resp.metadata["usage"], json!({"input_tokens": 50, "output_tokens": 50, "reasoning_tokens": 0, "cached_tokens": 0, "total_tokens": 100}));
    let seen = recorded.lock();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].headers["authorization"], "Bearer test");
    assert_eq!(seen[0].headers["accept"], "application/json");
    assert_eq!(seen[0].headers["version"], "0.135.0");
    assert_eq!(seen[0].headers["x-client-request-id"], "client-request-1");
    let body: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(body["model"], "gpt-image-1.5");
    assert_eq!(body["extra"]["preserve"], true);
    assert!(body.get("stream").is_none());
}

/// Go `TestCodexExecutorDirectOpenAIImageGenerationStreamsImagesEndpoint` plus the usage the
/// stream task reports from the `image_generation.completed` event.
#[tokio::test]
async fn direct_image_stream_relays_events_and_reports_usage() {
    let body = "event: image_generation.partial_image\ndata: {\"type\":\"image_generation.partial_image\",\"b64_json\":\"AA==\",\"partial_image_index\":0}\n\n\
event: image_generation.completed\ndata: {\"type\":\"image_generation.completed\",\"b64_json\":\"BB==\",\"usage\":{\"total_tokens\":10,\"input_tokens\":4,\"output_tokens\":6}}\n\n";
    let (url, recorded) = http_server(move |_| HttpReply { status: 200, body: body.to_string() }).await;
    let (exec, _keep) = executor(Config::default());
    let req = Request {
        model: "gpt-image-2".into(),
        payload: Bytes::from_static(br#"{"model":"gpt-image-2","prompt":"A cute baby sea otter","partial_images":2}"#),
        format: Format::OpenAI,
        metadata: Default::default(),
    };
    let mut result = exec.execute_stream(&api_key_auth(&url, false), req, image_options(true)).await.unwrap();
    let usage = result.usage.take().expect("usage channel");
    let (out, err) = drain(result).await;
    assert!(err.is_none());
    assert!(out.contains("event: image_generation.partial_image") && out.contains("event: image_generation.completed"), "{out}");
    let usage = usage.await.expect("usage reported");
    assert_eq!(usage, json!({"input_tokens": 4, "output_tokens": 6, "reasoning_tokens": 0, "cached_tokens": 0, "total_tokens": 10}));
    let seen = recorded.lock();
    assert_eq!(seen[0].headers["accept"], "text/event-stream");
    let sent: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["partial_images"], 2);
}

// ---------------------------------------------------------------- image usage order

fn completed_with_tool_usage(main_usage: &str, image_usage: &str) -> String {
    format!(
        r#"{{"type":"response.completed","response":{{"id":"resp_usage","object":"response","status":"completed","model":"gpt-5.5","output":[{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"ok"}}]}}]{main_usage}{image_usage}}}}}"#
    )
}

/// Go `TestCodexExecutorExecutePublishesMainUsageBeforeImageUsage` (upstream a3b7756): the main
/// model's record comes first and keeps its tokens; the image tool only adds a record of its own
/// model when it used tokens.
#[tokio::test]
async fn execute_publishes_main_usage_before_image_usage() {
    let main = r#","usage":{"input_tokens":100,"output_tokens":40,"total_tokens":140,"input_tokens_details":{"cached_tokens":25}}"#;
    let zero = r#","tool_usage":{"image_gen":{"input_tokens":0,"output_tokens":0,"total_tokens":0}}"#;
    let nonzero = r#","tool_usage":{"image_gen":{"input_tokens":10,"output_tokens":20,"total_tokens":30,"input_tokens_details":{"cached_tokens":3}}}"#;
    let cases = [(main, zero, false), (main, nonzero, true), (main, "", false), ("", zero, false), ("", nonzero, true)];
    for (main_usage, image_usage, expect_image_record) in cases {
        let body = format!("data: {}\n\n", completed_with_tool_usage(main_usage, image_usage));
        let (url, _) = http_server(move |_| raw_sse(body.clone())).await;
        let (exec, _keep) = executor(Config::default());
        let collector = cpa_runtime::usage_report::UsageCollector::new();
        let (mut req, mut opts) = request(r#"{"model":"gpt-5.5","input":"hi"}"#, false);
        req.model = "gpt-5.5".into();
        opts.usage_collector = Some(collector.clone());
        exec.execute(&api_key_auth(&url, false), req, opts).await.expect("execute");

        let records = collector.take();
        assert_eq!(records.len(), 1 + usize::from(expect_image_record), "{main_usage} {image_usage}: {records:?}");
        let main_record = &records[0];
        assert!(main_record.model == "gpt-5.5" && !main_record.failed);
        let want_main = if main_usage.is_empty() { (0, 0, 0, 0) } else { (100, 40, 140, 25) };
        let d = &main_record.detail;
        assert_eq!((d.input_tokens, d.output_tokens, d.total_tokens, d.cached_tokens), want_main, "{main_usage} {image_usage}");
        if expect_image_record {
            let image = &records[1];
            assert!(image.model == "gpt-image-2" && !image.failed);
            let d = &image.detail;
            assert_eq!((d.input_tokens, d.output_tokens, d.total_tokens, d.cached_tokens, d.cache_read_tokens), (10, 20, 30, 3, 3));
        }
    }
}

// ---------------------------------------------------------------- Responses image tool

/// Image call item as the upstream sends it in `response.output_item.done`.
const IMAGE_ITEM: &str = r#"{"id":"ig_1","type":"image_generation_call","status":"completed","result":"QUJD","revised_prompt":"revised","output_format":"jpeg","size":"1024x1024","background":"opaque","quality":"high"}"#;

fn image_tool_sse(with_image: bool) -> String {
    let done = format!(r#"{{"type":"response.output_item.done","output_index":0,"item":{IMAGE_ITEM}}}"#);
    let partial = r#"{"type":"response.image_generation_call.partial_image","item_id":"ig_1","output_index":0,"partial_image_index":1,"partial_image_b64":"UEFSVA==","output_format":"webp"}"#;
    let completed = r#"{"type":"response.completed","response":{"created_at":1700000001,"output":[],"usage":{"input_tokens":3,"output_tokens":4,"total_tokens":7},"tool_usage":{"image_gen":{"input_tokens":5,"output_tokens":6,"total_tokens":11}}}}"#;
    let mut events = vec![partial.to_string()];
    if with_image {
        events.push(done);
    }
    events.push(completed.to_string());
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

fn image_tool_request(path: &str, model: &str, payload: &str, stream: bool) -> (Request, Options) {
    let req = Request { model: model.into(), payload: Bytes::from(payload.to_string()), format: Format::OpenAI, metadata: Default::default() };
    let mut opts = image_options(stream);
    opts.metadata.insert("request_path".into(), json!(path));
    (req, opts)
}

/// A model that is not a direct image model takes the Responses path: forced `image_generation`
/// tool on the base model, answer rebuilt as an images response, usage of the base model and of
/// the tool published as two records.
#[tokio::test]
async fn image_tool_generation_builds_an_images_response_and_reports_both_usages() {
    let body = image_tool_sse(true);
    let (url, recorded) = http_server(move |_| raw_sse(body.clone())).await;
    let mut cfg = Config::default();
    cfg.gpt_image_2_base_model = "gpt-5.5".into();
    let (exec, _keep) = executor(cfg);
    let collector = cpa_runtime::usage_report::UsageCollector::new();
    let payload = r#"{"model":"custom-image","prompt":" a lighthouse ","size":"512x512","output_compression":80,"partial_images":2,"response_format":"URL","n":3}"#;
    let (req, mut opts) = image_tool_request("/v1/images/generations", "custom-image", payload, false);
    opts.usage_collector = Some(collector.clone());
    let resp = exec.execute(&api_key_auth(&url, false), req, opts).await.expect("image tool call");

    let out: Value = serde_json::from_slice(&resp.payload).unwrap();
    assert_eq!(
        out,
        json!({"created": 1700000001, "background": "opaque", "output_format": "jpeg", "quality": "high", "size": "1024x1024",
            "usage": {"input_tokens": 5, "output_tokens": 6, "total_tokens": 11},
            "data": [{"revised_prompt": "revised", "url": "data:image/jpeg;base64,QUJD"}]})
    );
    let seen = recorded.lock();
    assert_eq!(seen[0].headers["accept"], "text/event-stream");
    let sent: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(sent["model"], "gpt-5.5");
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["tool_choice"], json!({"type": "image_generation"}));
    assert_eq!(
        sent["tools"],
        json!([{"type": "image_generation", "action": "generate", "model": "custom-image", "size": "512x512", "output_compression": 80, "partial_images": 2}])
    );
    assert_eq!(sent["input"], json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "a lighthouse"}]}]));
    drop(seen);

    let records = collector.take();
    let summary: Vec<_> = records.iter().map(|r| (r.model.as_str(), r.detail.total_tokens, r.failed)).collect();
    assert_eq!(summary, vec![("gpt-5.5", 7, false), ("custom-image", 11, false)]);
}

#[tokio::test]
async fn image_tool_edit_forwards_images_and_mask() {
    let body = image_tool_sse(true);
    let (url, recorded) = http_server(move |_| raw_sse(body.clone())).await;
    let (exec, _keep) = executor(Config::default());
    let payload = r#"{"model":"custom-image","prompt":"p","images":[{"image_url":" data:image/png;base64,AA== "},{"file_id":"f"},{"image_url":""}],"mask":{"image_url":"data:image/png;base64,BB=="},"input_fidelity":"high"}"#;
    let (req, opts) = image_tool_request("/v1/images/edits", "custom-image", payload, false);
    let resp = exec.execute(&api_key_auth(&url, false), req, opts).await.expect("image tool edit");
    let out: Value = serde_json::from_slice(&resp.payload).unwrap();
    assert_eq!(out["data"], json!([{"revised_prompt": "revised", "b64_json": "QUJD"}]));
    let sent: Value = serde_json::from_slice(&recorded.lock()[0].body).unwrap();
    assert_eq!(sent["model"], "gpt-5.4-mini");
    assert_eq!(
        sent["tools"],
        json!([{"type": "image_generation", "action": "edit", "model": "custom-image", "input_fidelity": "high", "input_image_mask": {"image_url": "data:image/png;base64,BB=="}}])
    );
    assert_eq!(
        sent["input"][0]["content"],
        json!([{"type": "input_text", "text": "p"}, {"type": "input_image", "image_url": "data:image/png;base64,AA=="}])
    );
}

#[tokio::test]
async fn image_tool_stream_emits_partial_and_completed_events() {
    let body = image_tool_sse(true);
    let (url, _) = http_server(move |_| raw_sse(body.clone())).await;
    let (exec, _keep) = executor(Config::default());
    let (req, opts) = image_tool_request("/v1/images/edits", "custom-image", r#"{"model":"custom-image","prompt":"p","response_format":"url"}"#, true);
    let (out, err) = drain(exec.execute_stream(&api_key_auth(&url, false), req, opts).await.expect("stream")).await;
    assert!(err.is_none());
    assert_eq!(
        out,
        "event: image_edit.partial_image\ndata: {\"type\":\"image_edit.partial_image\",\"partial_image_index\":1,\"url\":\"data:image/webp;base64,UEFSVA==\"}\n\n\n\
event: image_edit.completed\ndata: {\"type\":\"image_edit.completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":6,\"total_tokens\":11},\"url\":\"data:image/jpeg;base64,QUJD\"}\n\n"
    );
}

#[tokio::test]
async fn image_tool_without_image_output_is_a_502_and_without_completion_a_504() {
    let body = image_tool_sse(false);
    let (url, _) = http_server(move |_| raw_sse(body.clone())).await;
    let (exec, _keep) = executor(Config::default());
    let (req, opts) = image_tool_request("/v1/images/generations", "custom-image", r#"{"prompt":"p"}"#, false);
    let err = exec.execute(&api_key_auth(&url, false), req, opts).await.unwrap_err();
    assert_eq!((err.status, err.message.as_str()), (502, "upstream did not return image output"));

    let (url, _) = http_server(|_| raw_sse("data: {\"type\":\"response.in_progress\"}\n\n".into())).await;
    let (req, opts) = image_tool_request("/v1/images/generations", "custom-image", r#"{"prompt":"p"}"#, false);
    let err = exec.execute(&api_key_auth(&url, false), req, opts).await.unwrap_err();
    assert_eq!((err.status, err.message.as_str()), (504, "stream error: stream disconnected before completion"));
}
