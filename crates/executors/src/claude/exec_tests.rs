//! Executor-level tests against a local mock Anthropic server: the request that reaches the
//! upstream (path, headers, body) and the response the client gets back.

use std::sync::Arc;

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_json::J;
use serde_json::Value;
use cpa_runtime::executor::{Executor, Options, Request};
use cpa_translator::Format;
use http::HeaderMap;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Owned elements of the array at `path`.
fn arr(v: &Value, path: &str) -> Vec<Value> {
    v.g(path).array().iter().map(|r| r.value()).collect()
}

/// One request captured by the mock upstream.
#[derive(Debug, Clone)]
struct Captured {
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

struct MockUpstream {
    base_url: String,
    captured: Arc<Mutex<Vec<Captured>>>,
}

/// Starts a one-route mock that answers every request with `status` and `body`
/// (`content_type` decides JSON vs SSE).
async fn mock_upstream(status: u16, content_type: &'static str, body: String) -> MockUpstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (sink, body) = (Arc::clone(&sink), body.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head_end, content_length) = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        break (pos + 4, len);
                    }
                };
                while buf.len() < head_end + content_length {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.lines();
                let path = lines.next().unwrap_or("").split(' ').nth(1).unwrap_or("").to_string();
                let mut headers = HeaderMap::new();
                for line in lines {
                    if let Some((k, v)) = line.split_once(':')
                        && let (Ok(k), Ok(v)) = (http::HeaderName::from_bytes(k.trim().as_bytes()), http::HeaderValue::from_str(v.trim()))
                    {
                        headers.append(k, v);
                    }
                }
                sink.lock().push(Captured { path, headers, body: buf[head_end..].to_vec() });
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    MockUpstream { base_url: format!("http://{addr}"), captured }
}

fn executor() -> cpa_runtime::executor::DynExecutor {
    let (_tx, rx) = watch::channel(Arc::new(Config::default()));
    // Keep the sender alive for the executor's lifetime.
    std::mem::forget(_tx);
    super::new(rx)
}

fn api_key_auth(base_url: &str) -> Auth {
    let mut auth = Auth::new("claude-key", "claude");
    auth.attributes.insert("api_key".into(), "sk-ant-api-test".into());
    auth.attributes.insert("base_url".into(), base_url.into());
    auth
}

fn oauth_auth(base_url: &str) -> Auth {
    let mut auth = Auth::new("claude-oauth", "claude");
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.metadata.insert("access_token".into(), "sk-ant-oat01-test".into());
    auth.metadata.insert("account_uuid".into(), "3c9a1f3e-6e2b-4d57-9a53-0a6a4cf1d5aa".into());
    auth.metadata.insert("claude_device_ids".into(), serde_json::json!([format!("{:064x}", 7)]));
    auth
}

const MESSAGE: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-6","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}}"#;

fn claude_request(payload: &str) -> (Request, Options) {
    let req = Request {
        model: "claude-opus-4-6".into(),
        payload: Bytes::from(payload.to_string()),
        format: Format::Claude,
        metadata: Default::default(),
    };
    (req, Options::new(Format::Claude))
}

#[tokio::test]
async fn api_key_passthrough_forwards_caller_body_to_custom_base_url() {
    let upstream = mock_upstream(200, "application/json", MESSAGE.to_string()).await;
    let payload = r#"{"model":"claude-opus-4-6","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#;
    let (req, opts) = claude_request(payload);
    let resp = executor().execute(&api_key_auth(&upstream.base_url), req, opts).await.unwrap();

    let captured = upstream.captured.lock().clone();
    assert_eq!(captured.len(), 1);
    let c = &captured[0];
    assert_eq!(c.path, "/v1/messages?beta=true");
    // Third-party base URLs always authenticate with Bearer, never x-api-key.
    assert_eq!(c.headers.get("authorization").unwrap(), "Bearer sk-ant-api-test");
    assert!(c.headers.get("x-api-key").is_none());
    assert_eq!(c.headers.get("anthropic-version").unwrap(), "2023-06-01");
    // Caller-owned passthrough: no cloaking, no billing header.
    let body = cpa_json::parse(&c.body);
    assert_eq!(body.g("model").str(), "claude-opus-4-6");
    assert!(!body.g("system").exists());
    assert_eq!(body.g("stream").bool(), false);
    // CPA still owns cache_control placement for non-native callers (latest user block).
    assert_eq!(body.g("messages.0.content.0.text").str(), "hi");
    assert_eq!(body.g("messages.0.content.0.cache_control.type").str(), "ephemeral");
    // Response is the upstream message and carries executor-measured usage.
    assert_eq!(cpa_json::parse(&resp.payload).g("id").str(), "msg_1");
    assert_eq!(resp.metadata["usage"]["input_tokens"], 3);
    assert_eq!(resp.metadata["usage"]["output_tokens"], 2);
}

#[tokio::test]
async fn oauth_request_is_cloaked_and_signed() {
    let upstream = mock_upstream(200, "application/json", MESSAGE.to_string()).await;
    let payload = r#"{"model":"claude-opus-4-6","max_tokens":64,"system":"be brief","messages":[{"role":"user","content":"hello there world"}]}"#;
    let (req, opts) = claude_request(payload);
    executor().execute(&oauth_auth(&upstream.base_url), req, opts).await.unwrap();

    let c = upstream.captured.lock()[0].clone();
    assert_eq!(c.headers.get("authorization").unwrap(), "Bearer sk-ant-oat01-test");
    assert_eq!(c.headers.get("x-app").unwrap(), "cli");
    let beta = c.headers.get("anthropic-beta").unwrap().to_str().unwrap().to_string();
    assert!(beta.starts_with("claude-code-20250219,oauth-2025-04-20,"), "{beta}");
    assert!(c.headers.get("x-claude-code-session-id").is_some());

    let body = cpa_json::parse(&c.body);
    let billing = body.g("system.0.text").str();
    assert!(billing.starts_with("x-anthropic-billing-header: cc_version=2.1.280."), "{billing}");
    // The placeholder digits are replaced by the signature.
    let cch = billing.split("cch=").nth(1).unwrap().split(';').next().unwrap();
    assert_eq!(cch.len(), 5);
    assert_ne!(cch, "00000");
    assert_eq!(body.g("system.1.text").str(), "You are Claude Code, Anthropic's official CLI for Claude.");
    // The caller's system prompt moved into a mid-conversation system message (legacy model:
    // opus-4-6 uses the reminder path instead).
    let first_user = arr(&body, "messages.0.content");
    assert!(first_user.iter().any(|b| b.g("text").str().contains("be brief")), "{body}");
    // Identity: metadata.user_id is a JSON string with the credential device and account.
    let user_id = cpa_json::parse(body.g("metadata.user_id").str().as_bytes());
    assert_eq!(user_id.g("account_uuid").str(), "3c9a1f3e-6e2b-4d57-9a53-0a6a4cf1d5aa");
    assert_eq!(user_id.g("device_id").str(), format!("{:064x}", 7));
}

#[tokio::test]
async fn upstream_429_is_classified_and_keeps_body() {
    let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
    let upstream = mock_upstream(429, "application/json", body.to_string()).await;
    let (req, opts) = claude_request(r#"{"model":"claude-opus-4-6","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#);
    let err = executor().execute(&api_key_auth(&upstream.base_url), req, opts).await.unwrap_err();
    assert_eq!(err.status, 429);
    assert_eq!(err.message, body);
    assert!(!err.is_request_scoped());
}

#[tokio::test]
async fn stream_claude_client_receives_whole_events() {
    let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-opus-4-6\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let upstream = mock_upstream(200, "text/event-stream", sse.to_string()).await;
    let (req, mut opts) = claude_request(r#"{"model":"claude-opus-4-6","max_tokens":8,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#);
    opts.stream = true;
    let mut result = executor().execute_stream(&api_key_auth(&upstream.base_url), req, opts).await.unwrap();
    let mut events = Vec::new();
    while let Some(chunk) = result.chunks.recv().await {
        events.push(String::from_utf8(chunk.unwrap().to_vec()).unwrap());
    }
    assert_eq!(events.len(), 3, "{events:?}");
    assert!(events[0].starts_with("event: message_start\ndata: "));
    assert!(events[2].contains("message_stop"));
    let usage = result.usage.take().unwrap().await.unwrap();
    assert_eq!(usage["input_tokens"], 5);
    assert_eq!(usage["output_tokens"], 4);
}

#[tokio::test]
async fn count_tokens_is_local_on_third_party_base_url() {
    let upstream = mock_upstream(200, "application/json", "{}".to_string()).await;
    let (req, opts) = claude_request(r#"{"model":"claude-opus-4-6","messages":[{"role":"user","content":"hello world"}]}"#);
    let resp = executor().count_tokens(&api_key_auth(&upstream.base_url), req, opts).await.unwrap();
    assert!(upstream.captured.lock().is_empty(), "local estimate must not call the upstream");
    assert!(cpa_json::parse(&resp.payload).g("input_tokens").int() > 0);

    let (req, opts) = claude_request(r#"{"model":"claude-opus-4-6","messages":[]}"#);
    let err = executor().count_tokens(&api_key_auth(&upstream.base_url), req, opts).await.unwrap_err();
    assert_eq!(err.status, 400);
    assert!(err.is_request_scoped());
}
