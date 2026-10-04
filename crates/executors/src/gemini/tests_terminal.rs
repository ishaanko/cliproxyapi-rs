//! Responses streaming terminal behavior of the Gemini-family executors (Go: issue6258 tests):
//! split usage frames, a read error without a synthetic terminal event, and a clean EOF.

use bytes::Bytes;
use cpa_json::J;
use std::sync::Arc;

use cpa_runtime::executor::{DynExecutor, ExecError, Executor, Options, Request};
use cpa_translator::Format;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::test_support::{config_rx, key_auth, mock_upstream, sse_reply};
use super::{GeminiExecutor, GeminiVertexExecutor};

const CONTENT: &str = r#"{"responseId":"executor-6258","candidates":[{"content":{"parts":[{"text":"answer"}]}}]}"#;
const USAGE: &str = r#"{"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":10,"thoughtsTokenCount":50,"totalTokenCount":160}}"#;
const PAYLOAD: &str = r#"{"input":"hello","stream":true}"#;

/// Providers sharing the Gemini streaming loop (service-account Vertex and Antigravity have their
/// own credential flows and are covered elsewhere).
fn executors() -> Vec<(&'static str, DynExecutor)> {
    vec![("gemini", Arc::new(GeminiExecutor::new(config_rx()))), ("vertex", Arc::new(GeminiVertexExecutor::new(config_rx())))]
}

fn sse(body: &str, trace: bool) -> String {
    let body = if trace { format!("{},\"traceId\":\"trace-6258\"}}", body.trim_end_matches('}')) } else { body.to_string() };
    format!("data: {body}\n\n")
}

async fn start(exec: &DynExecutor, provider: &str, base: &str, payload: &str) -> cpa_runtime::executor::StreamResult {
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
    exec.execute_stream(&key_auth(provider, base), req, opts).await.expect("execute_stream")
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

#[tokio::test]
async fn split_usage_frames_complete_once_with_cumulative_usage() {
    for (provider, exec) in executors() {
        for trace in [false, true] {
            let body: String =
                [CONTENT, r#"{"candidates":[{"finishReason":"STOP"}]}"#, USAGE].iter().map(|b| sse(b, trace)).collect();
            let (base, _seen) = mock_upstream(vec![sse_reply(&body)]).await;
            let response = clean_terminal(start(&exec, provider, &base, PAYLOAD).await).await;
            for (path, want) in [
                ("input_tokens", 100),
                ("output_tokens", 60),
                ("total_tokens", 160),
                ("output_tokens_details.reasoning_tokens", 50),
            ] {
                assert_eq!(response.g(&format!("usage.{path}")).int(), want, "{provider} trace={trace} usage.{path}: {response}");
            }
        }
    }
}

#[tokio::test]
async fn clean_eof_without_finish_still_completes_partial_output() {
    for (provider, exec) in executors() {
        let (base, _seen) = mock_upstream(vec![sse_reply(&sse(CONTENT, false))]).await;
        let response = clean_terminal(start(&exec, provider, &base, PAYLOAD).await).await;
        assert_eq!(response.g("output.0.content.0.text").str(), "answer", "{provider}: {response}");
    }
}

/// A body shorter than its Content-Length: the executor reports the read error and must not
/// synthesize a terminal event for the truncated stream.
async fn truncated_upstream(body: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let mut buf = vec![0u8; 8192];
            let mut seen = Vec::new();
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut buf).await {
                    Ok(n) if n > 0 => seen.extend_from_slice(&buf[..n]),
                    _ => break,
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len() + 128
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    base
}

#[tokio::test]
async fn read_error_has_no_terminal() {
    for (provider, exec) in executors() {
        let base = truncated_upstream(sse(CONTENT, false)).await;
        let mut stream = start(&exec, provider, &base, PAYLOAD).await;
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
                        assert!(!is_terminal(kind), "{provider}: read-error stream synthesized {kind} before unexpected EOF");
                    }
                }
            }
        }
        assert!(read_errors == 1 && deltas > 0, "{provider}: errors={read_errors} deltas={deltas}, want 1/>0");
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
