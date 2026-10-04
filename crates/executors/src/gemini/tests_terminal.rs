//! Responses streaming terminal behavior of the Gemini-family executors (Go: issue6258 tests):
//! split usage frames, a read error or cancellation without a synthetic terminal event, and a
//! clean EOF, for Gemini, Vertex (API key and service account) and Antigravity.

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_json::J;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use cpa_runtime::executor::{DynExecutor, ExecError, Executor, Options, Request};
use cpa_translator::Format;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use super::test_support::{config_rx, key_auth};
use super::{GeminiExecutor, GeminiVertexExecutor};
use crate::antigravity::AntigravityExecutor;

const CONTENT: &str = r#"{"responseId":"executor-6258","candidates":[{"content":{"parts":[{"text":"answer"}]}}]}"#;
const USAGE: &str = r#"{"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":10,"thoughtsTokenCount":50,"totalTokenCount":160}}"#;
const PAYLOAD: &str = r#"{"input":"hello","stream":true}"#;
const PATCH_REQUEST: &str = r#"{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","definition":"start: patch"}}]}],"input":"patch a file"}"#;
const PATCH_RESPONSE: &str = r#"{"responseId":"patch","candidates":[{"content":{"parts":[{"functionCall":{"name":"functions__apply_patch","args":{"input":"  *** Begin Patch\n*** End Patch\n "}}}]},"finishReason":"STOP"}]}"#;
const PATCH_INPUT: &str = "  *** Begin Patch\n*** End Patch\n ";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Provider {
    Gemini,
    VertexApiKey,
    VertexServiceAccount,
    Antigravity,
}

const PROVIDERS: [Provider; 4] =
    [Provider::Gemini, Provider::VertexApiKey, Provider::VertexServiceAccount, Provider::Antigravity];

/// An RSA key for the Vertex service account; `None` without openssl (those cases skip).
fn service_account_key() -> Option<&'static str> {
    static KEY: OnceLock<Option<String>> = OnceLock::new();
    KEY.get_or_init(|| {
        let out = Command::new("openssl").args(["genrsa", "-traditional", "2048"]).stderr(Stdio::null()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    })
    .as_deref()
}

/// The executor and a credential pointed at `base`, or `None` when the provider cannot run here.
fn setup(provider: Provider, base: &str) -> Option<(DynExecutor, Auth)> {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let id = format!("issue6258-{}", SEQ.fetch_add(1, Ordering::SeqCst));
    Some(match provider {
        Provider::Gemini => (Arc::new(GeminiExecutor::new(config_rx())), key_auth("gemini", base)),
        Provider::VertexApiKey => (Arc::new(GeminiVertexExecutor::new(config_rx())), key_auth("vertex", base)),
        Provider::VertexServiceAccount => {
            let key = service_account_key()?;
            // The service-account host is fixed in production; the test hook maps a unique location to `base`.
            super::vertex::TEST_BASE_URLS.lock().insert(id.clone(), base.to_string());
            let mut auth = Auth::new(&id, "vertex");
            auth.metadata.insert("project_id".into(), json!("proxy-test"));
            auth.metadata.insert("location".into(), json!(id));
            auth.metadata.insert(
                "service_account".into(),
                json!({
                    "type": "service_account", "project_id": "proxy-test", "private_key_id": "kid", "private_key": key,
                    "client_email": "proxy-test@proxy-test.iam.gserviceaccount.com", "token_uri": format!("{base}/token"),
                }),
            );
            (Arc::new(GeminiVertexExecutor::new(config_rx())), auth)
        }
        Provider::Antigravity => {
            let mut auth = Auth::new(&id, "antigravity");
            auth.attributes.insert("base_url".into(), base.into());
            auth.metadata.insert("access_token".into(), json!("test-token"));
            auth.metadata.insert("expired".into(), json!((chrono::Utc::now() + chrono::Duration::hours(24)).to_rfc3339()));
            auth.metadata.insert("project_id".into(), json!("proxy-test"));
            (Arc::new(AntigravityExecutor::new(config_rx())), auth)
        }
    })
}

/// One SSE data frame; Antigravity wraps the Gemini body in `{"response": ...}`.
fn sse(provider: Provider, body: &str, trace: bool) -> String {
    let body = if provider == Provider::Antigravity { format!(r#"{{"response":{body}}}"#) } else { body.to_string() };
    let body = if trace { format!("{},\"traceId\":\"trace-6258\"}}", body.trim_end_matches('}')) } else { body };
    format!("data: {body}\n\n")
}

async fn start(exec: &DynExecutor, auth: &Auth, payload: &str) -> cpa_runtime::executor::StreamResult {
    let req = Request {
        model: "gemini-3.7-flash".into(),
        payload: Bytes::from(payload.to_string()),
        format: Format::OpenAIResponse,
        metadata: Default::default(),
    };
    let mut opts = Options::new(Format::OpenAIResponse);
    opts.response_format = Some(Format::OpenAIResponse);
    opts.original_request = req.payload.clone();
    opts.stream = true;
    exec.execute_stream(auth, req, opts).await.expect("execute_stream")
}

fn events(chunk: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(chunk)
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        .filter(|e| e.get("type").is_some())
        .collect()
}

fn is_terminal(kind: &str) -> bool {
    matches!(kind, "response.completed" | "response.incomplete" | "response.failed")
}

/// Drains a clean stream: exactly one `response.completed`, every output item done once.
async fn clean_terminal(mut stream: cpa_runtime::executor::StreamResult) -> Value {
    let (mut response, mut terminals) = (Value::Null, 0);
    let mut done: std::collections::HashMap<String, usize> = Default::default();
    while let Some(chunk) = stream.chunks.recv().await {
        let chunk = chunk.expect("clean stream returned error");
        for event in events(&chunk) {
            match event["type"].as_str().unwrap_or("") {
                kind if is_terminal(kind) => {
                    terminals += 1;
                    assert_eq!(kind, "response.completed");
                    response = event["response"].clone();
                }
                "response.output_item.done" => {
                    *done.entry(event["item"]["id"].as_str().unwrap_or("").to_string()).or_default() += 1;
                }
                _ => {}
            }
        }
    }
    assert_eq!((terminals, response["status"].as_str()), (1, Some("completed")), "one completed terminal");
    for item in response["output"].as_array().into_iter().flatten() {
        assert_eq!(done.get(item["id"].as_str().unwrap_or("")), Some(&1), "item done count: {item}");
    }
    response
}

/// What the upstream does with a stream request (`/token` always answers a bearer token).
enum Serve {
    /// The whole body, then a clean close.
    Full(String),
    /// A body shorter than its Content-Length: the client sees an unexpected EOF.
    Truncated(String),
    /// The body so far, then silence until the client hangs up (reported on the channel).
    Hang(String, oneshot::Sender<()>),
}

async fn serve(mode: Serve) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let mode = Arc::new(parking_lot::Mutex::new(Some(mode)));
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let mode = mode.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    match socket.read(&mut chunk).await {
                        Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                        _ => return,
                    }
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let want: usize = head
                    .lines()
                    .filter_map(|l| l.split_once(':'))
                    .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.trim().parse().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + want {
                    match socket.read(&mut chunk).await {
                        Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                        _ => break,
                    }
                }
                if head.split_whitespace().nth(1).is_some_and(|t| t.starts_with("/token")) {
                    let body = r#"{"access_token":"local-token","token_type":"Bearer","expires_in":3600}"#;
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                    let _ = socket.shutdown().await;
                    return;
                }
                let Some(mode) = mode.lock().take() else { return };
                match mode {
                    Serve::Full(body) => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                        let _ = socket.write_all(body.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    }
                    Serve::Truncated(body) => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len() + 128
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                        let _ = socket.write_all(body.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    }
                    Serve::Hang(body, closed) => {
                        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                        let _ = socket.write_all(head.as_bytes()).await;
                        let _ = socket.write_all(body.as_bytes()).await;
                        // Returns on EOF or a reset: the client dropped the request.
                        while matches!(socket.read(&mut chunk).await, Ok(n) if n > 0) {}
                        let _ = closed.send(());
                    }
                }
            });
        }
    });
    base
}

#[tokio::test]
async fn split_usage_frames_complete_once_with_cumulative_usage() {
    for provider in PROVIDERS {
        for trace in [false, true] {
            let body: String =
                [CONTENT, r#"{"candidates":[{"finishReason":"STOP"}]}"#, USAGE].iter().map(|b| sse(provider, b, trace)).collect();
            let base = serve(Serve::Full(body)).await;
            let Some((exec, auth)) = setup(provider, &base) else { continue };
            let response = clean_terminal(start(&exec, &auth, PAYLOAD).await).await;
            for (path, want) in [
                ("input_tokens", 100),
                ("output_tokens", 60),
                ("total_tokens", 160),
                ("output_tokens_details.reasoning_tokens", 50),
            ] {
                assert_eq!(response.g(&format!("usage.{path}")).int(), want, "{provider:?} trace={trace} usage.{path}: {response}");
            }
        }
    }
}

/// Go `TestIssue6258ExecutorCleanEOFControl`: a clean EOF completes the partial output, and with
/// apply_patch a valid source STOP must not become an invalid patch.
#[tokio::test]
async fn clean_eof_completes_partial_output_and_valid_patch() {
    for provider in PROVIDERS {
        for patch in [false, true] {
            let (body, payload) = if patch { (PATCH_RESPONSE, PATCH_REQUEST) } else { (CONTENT, PAYLOAD) };
            let base = serve(Serve::Full(sse(provider, body, false))).await;
            let Some((exec, auth)) = setup(provider, &base) else { continue };
            let response = clean_terminal(start(&exec, &auth, payload).await).await;
            if patch {
                let item = response["output"]
                    .as_array()
                    .and_then(|o| o.iter().find(|i| i["type"] == "custom_tool_call"))
                    .unwrap_or_else(|| panic!("{provider:?}: wrong patch output: {response}"));
                assert_eq!(
                    (item["input"].as_str(), item["name"].as_str(), item["namespace"].as_str()),
                    (Some(PATCH_INPUT), Some("apply_patch"), Some("functions")),
                    "{provider:?}: {response}"
                );
            } else {
                assert_eq!(response.g("output.0.content.0.text").str(), "answer", "{provider:?}: clean EOF lost partial output: {response}");
            }
        }
    }
}

/// A body shorter than its Content-Length: the executor reports the read error and must not
/// synthesize a terminal event for the truncated stream.
#[tokio::test]
async fn read_error_has_no_terminal() {
    for provider in PROVIDERS {
        let base = serve(Serve::Truncated(sse(provider, CONTENT, false))).await;
        let Some((exec, auth)) = setup(provider, &base) else { continue };
        let mut stream = start(&exec, &auth, PAYLOAD).await;
        let (mut read_errors, mut deltas) = (0, 0);
        while let Some(chunk) = stream.chunks.recv().await {
            match chunk {
                Err(ExecError { .. }) => read_errors += 1,
                Ok(chunk) => {
                    for event in events(&chunk) {
                        let kind = event["type"].as_str().unwrap_or("");
                        if kind == "response.output_text.delta" {
                            deltas += 1;
                        }
                        assert!(!is_terminal(kind), "{provider:?}: read-error stream synthesized {kind} before unexpected EOF");
                    }
                }
            }
        }
        assert!(read_errors == 1 && deltas > 0, "{provider:?}: errors={read_errors} deltas={deltas}, want 1/>0");
    }
}

/// Go `TestIssue6258ExecutorCancellationHasNoTerminal`: the caller going away (the receiver is
/// dropped, Rust's context cancellation) after content was established ends the upstream request
/// and never produces a synthesized terminal event.
#[tokio::test]
async fn cancellation_has_no_terminal() {
    for provider in PROVIDERS {
        let (closed_tx, closed_rx) = oneshot::channel();
        let base = serve(Serve::Hang(sse(provider, CONTENT, false), closed_tx)).await;
        let Some((exec, auth)) = setup(provider, &base) else { continue };
        let mut stream = start(&exec, &auth, PAYLOAD).await;
        let mut started = false;
        'read: while let Some(chunk) = tokio::time::timeout(Duration::from_secs(10), stream.chunks.recv()).await.expect("stream stalled") {
            let chunk = chunk.unwrap_or_else(|e| panic!("{provider:?}: stream error before cancellation: {e:?}"));
            for event in events(&chunk) {
                let kind = event["type"].as_str().unwrap_or("");
                assert!(!is_terminal(kind), "{provider:?}: canceled stream synthesized {kind}");
                if kind == "response.output_text.delta" {
                    started = true;
                    break 'read;
                }
            }
        }
        assert!(started, "{provider:?}: cancellation did not exercise an established content stream");
        drop(stream);
        tokio::time::timeout(Duration::from_secs(10), closed_rx)
            .await
            .unwrap_or_else(|_| panic!("{provider:?}: upstream request outlived the canceled stream"))
            .expect("hang server alive");
    }
}

/// AI Studio relay: a clean `stream_end` (or an `http_response` 2xx) completes the Responses
/// stream; a non-2xx `http_response` is exactly one error and never a synthesized terminal.
#[tokio::test]
async fn aistudio_relay_endings() {
    use super::wsrelay::{Inbound, Manager, Message, Outbound};
    use tokio::sync::mpsc;

    struct Rx(mpsc::Receiver<Result<Inbound, String>>);
    impl futures_util::Stream for Rx {
        type Item = Result<Inbound, String>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            self.0.poll_recv(cx)
        }
    }

    let stop = r#"{"responseId":"executor-6258","candidates":[{"content":{"parts":[{"text":"answer"}]},"finishReason":"STOP"}]}"#;
    // (name, body, http status of an http_response reply or 0 for a chunked stream)
    let cases = [
        ("no_finish", CONTENT, 0),
        ("stop_without_usage", stop, 0),
        ("http_response_success", CONTENT, 200),
        ("http_response_201_success", CONTENT, 201),
        ("http_response_error_after_start", CONTENT, 503),
    ];
    for (name, body, http_status) in cases {
        let relay = Arc::new(Manager::new(""));
        let (out_tx, mut out_rx) = mpsc::channel(16);
        let (in_tx, in_rx) = mpsc::channel(16);
        let channel = relay.attach(out_tx, Rx(in_rx));
        let exec = super::AiStudioExecutor::new(config_rx(), relay.clone());
        let auth = cpa_auth::Auth::new(channel, "aistudio");
        let page = tokio::spawn(async move {
            let Some(Outbound::Text(text)) = out_rx.recv().await else { return };
            let msg: Message = serde_json::from_str(&text).expect("relay request");
            let frame = |kind: &str, payload: Value| {
                Inbound::Text(serde_json::json!({"id": msg.id, "type": kind, "payload": payload}).to_string())
            };
            let mut frames = if http_status == 0 {
                vec![
                    frame("stream_start", serde_json::json!({"status": 200})),
                    frame("stream_chunk", serde_json::json!({"data": format!("data: {body}\n\n")})),
                    frame("stream_end", serde_json::json!({})),
                ]
            } else {
                vec![frame("http_response", serde_json::json!({"status": http_status, "body": body}))]
            };
            if http_status >= 400 {
                frames.insert(0, frame("stream_start", serde_json::json!({"status": 200})));
            }
            for f in frames {
                let _ = in_tx.send(Ok(f)).await;
            }
        });
        let req = Request {
            model: "gemini-3.7-flash".into(),
            payload: Bytes::from(PAYLOAD.to_string()),
            format: Format::OpenAIResponse,
            metadata: Default::default(),
        };
        let mut opts = Options::new(Format::OpenAIResponse);
        opts.response_format = Some(Format::OpenAIResponse);
        opts.original_request = req.payload.clone();
        opts.stream = true;
        let started = exec.execute_stream(&auth, req, opts).await;
        if http_status >= 400 {
            // The error may surface when the stream starts or as the only chunk.
            let mut errors = usize::from(started.is_err());
            if let Ok(mut stream) = started {
                while let Some(chunk) = stream.chunks.recv().await {
                    match chunk {
                        Err(_) => errors += 1,
                        Ok(chunk) => {
                            for event in events(&chunk) {
                                assert!(!is_terminal(event["type"].as_str().unwrap_or("")), "{name}: terminal after an error");
                            }
                        }
                    }
                }
            }
            assert_eq!(errors, 1, "{name}: unsuccessful http_response errors");
        } else {
            let response = clean_terminal(started.expect("stream")).await;
            assert_eq!(response.g("output.0.content.0.text").str(), "answer", "{name}: {response}");
        }
        let _ = page.await;
    }
}
