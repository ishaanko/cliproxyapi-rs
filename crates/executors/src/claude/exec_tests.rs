//! Executor-level tests against a local mock Anthropic server: the request that reaches the
//! upstream (path, headers, body) and the response the client gets back.

use std::sync::Arc;

use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_json::J;
use serde_json::Value;
use cpa_runtime::executor::{Options, Request};
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
    mock_upstream_with(status, content_type, move |_| body.clone()).await
}

/// Like [`mock_upstream`], but the response body is computed from the request body.
async fn mock_upstream_with(
    status: u16,
    content_type: &'static str,
    respond: impl Fn(&[u8]) -> String + Send + Sync + 'static,
) -> MockUpstream {
    let respond = Arc::new(respond);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (sink, respond) = (Arc::clone(&sink), Arc::clone(&respond));
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
                let request_body = buf[head_end..].to_vec();
                let body = respond(&request_body);
                sink.lock().push(Captured { path, headers, body: request_body });
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
    eprintln!("HEADERS {:?}", c.headers);
    // Caller-owned passthrough: no cloaking, no billing header.
    let body = cpa_json::parse(&c.body);
    assert_eq!(body.g("model").str(), "claude-opus-4-6");
    assert!(!body.g("system").exists());
    assert!(!body.g("stream").bool());
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

#[tokio::test]
async fn claude_code_cli_profile_on_third_party_gateway() {
    let upstream = mock_upstream_with(200, "application/json", |request| {
        let tool = cpa_json::parse(request).g("tools.0.name").str();
        format!(
            r#"{{"id":"msg_1","type":"message","model":"claude-sonnet-5","role":"assistant","content":[{{"type":"tool_use","id":"toolu_1","name":"{tool}","input":{{}}}}],"stop_reason":"tool_use","usage":{{"input_tokens":1,"output_tokens":1}}}}"#
        )
    })
    .await;
    let mut auth = Auth::new("claude-code-cli-api-key", "claude");
    auth.attributes.insert("api_key".into(), "key-claude-code-cli-fp".into());
    auth.attributes.insert("base_url".into(), upstream.base_url.clone());
    auth.attributes.insert("fingerprint_profile".into(), "claude-code-cli".into());
    let payload = r#"{"model":"claude-sonnet-5","messages":[{"role":"user","content":[{"type":"text","text":"What can you do?"}]}],"tools":[{"name":"read_file","description":"Read a file","input_schema":{"type":"object"}}]}"#;
    let mut req = claude_request(payload).0;
    req.model = "claude-sonnet-5".into();
    let resp = executor().execute(&auth, req, Options::new(Format::Claude)).await.unwrap();

    let c = upstream.captured.lock()[0].clone();
    assert_eq!(c.headers.get("authorization").unwrap(), "Bearer key-claude-code-cli-fp");
    let want_betas = super::request::claude_code_cli_betas(payload.as_bytes(), &Default::default(), true);
    assert_eq!(c.headers.get("anthropic-beta").unwrap().to_str().unwrap(), want_betas);

    let body = cpa_json::parse(&c.body);
    let billing = body.g("system.0.text").str();
    assert!(billing.starts_with("x-anthropic-billing-header:"), "{billing}");
    // Native only signs for first-party; a gateway gets the billing header without cch.
    assert!(!billing.contains("cch="), "{billing}");
    assert_eq!(body.g("system.1.text").str(), "You are Claude Code, Anthropic's official CLI for Claude.");

    let user_id_text = body.g("metadata.user_id").str();
    assert!(crate::helps::id_cache::is_valid_user_id(&user_id_text), "{user_id_text}");
    let user_id = cpa_json::parse(user_id_text.as_bytes());
    assert!(!user_id.g("account_uuid").str().is_empty());
    let session_header = c.headers.get("x-claude-code-session-id").unwrap().to_str().unwrap().to_string();
    assert_eq!(user_id.g("session_id").str(), session_header);

    let upstream_tool = body.g("tools.0.name").str();
    assert!(upstream_tool.starts_with("mcp__"), "{upstream_tool}");
    assert_eq!(cpa_json::parse(&resp.payload).g("content.0.name").str(), "read_file");
}

#[tokio::test]
async fn openai_chat_client_gets_translated_non_stream_response_from_sse_upstream() {
    let sse = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-opus-4-6\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let upstream = mock_upstream(200, "text/event-stream", sse.to_string()).await;
    let req = Request {
        model: "claude-opus-4-6".into(),
        payload: Bytes::from_static(br#"{"model":"claude-opus-4-6","messages":[{"role":"user","content":"hi"}]}"#),
        format: Format::OpenAI,
        metadata: Default::default(),
    };
    let resp = executor().execute(&api_key_auth(&upstream.base_url), req, Options::new(Format::OpenAI)).await.unwrap();
    // A non-Claude client always streams upstream, even for a non-stream call.
    let c = upstream.captured.lock()[0].clone();
    assert!(cpa_json::parse(&c.body).g("stream").bool());
    let out = cpa_json::parse(&resp.payload);
    assert_eq!(out.g("choices.0.message.content").str(), "hello");
    assert_eq!(resp.metadata["usage"]["output_tokens"], 3);
}

#[tokio::test]
async fn empty_upstream_stream_is_a_502() {
    let upstream = mock_upstream(200, "text/event-stream", String::new()).await;
    let req = Request {
        model: "claude-opus-4-6".into(),
        payload: Bytes::from_static(br#"{"model":"claude-opus-4-6","messages":[{"role":"user","content":"hi"}]}"#),
        format: Format::OpenAI,
        metadata: Default::default(),
    };
    let err = executor().execute(&api_key_auth(&upstream.base_url), req, Options::new(Format::OpenAI)).await.unwrap_err();
    assert_eq!(err.status, 502);
    assert!(err.message.contains("empty stream response"), "{}", err.message);
}

async fn prepared(url: &str, auth: &Auth) -> reqwest::Request {
    let mut req = reqwest::Request::new(reqwest::Method::POST, url.parse().unwrap());
    req.headers_mut().insert("authorization", "stale".parse().unwrap());
    executor().prepare_request(&mut req, auth).await.unwrap();
    req
}

/// Go PrepareRequest: `x-api-key` only for API-key credentials on the first-party origin,
/// bearer elsewhere, custom attribute headers last.
#[tokio::test]
async fn prepare_request_picks_header_by_origin_and_credential() {
    let mut key = api_key_auth("http://unused");
    key.attributes.insert("header:X-Team".into(), "blue".into());
    let first_party = prepared("https://api.anthropic.com/v1/messages", &key).await;
    assert_eq!(first_party.headers()["x-api-key"], "sk-ant-api-test");
    assert!(!first_party.headers().contains_key("authorization"));
    assert_eq!(first_party.headers()["x-team"], "blue");
    let third_party = prepared("https://proxy.example/v1/messages", &key).await;
    assert_eq!(third_party.headers()["authorization"], "Bearer sk-ant-api-test");
    assert!(!third_party.headers().contains_key("x-api-key"));
}

/// An embedded executor (Kimi's Anthropic-compatible path) sends the normalized model upstream
/// and writes the client's model back into the response.
#[tokio::test]
async fn embedded_executor_normalizes_the_upstream_model_and_restores_the_client_model() {
    let upstream = mock_upstream(200, "application/json", MESSAGE.to_string()).await;
    let (_tx, rx) = watch::channel(Arc::new(Config::default()));
    let embedded = super::new_embedded(
        rx,
        super::Embedding { request_log_provider: "kimi", upstream_model: |m| m.replace("alias-", "claude-") },
    );
    let req = Request {
        model: "alias-opus-4-6".into(),
        payload: Bytes::from_static(br#"{"model":"alias-opus-4-6","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}"#),
        format: Format::Claude,
        metadata: Default::default(),
    };
    let resp = embedded.execute(&api_key_auth(&upstream.base_url), req, Options::new(Format::Claude)).await.unwrap();
    let sent = cpa_json::parse(&upstream.captured.lock()[0].body);
    assert_eq!(sent.g("model").str(), "claude-opus-4-6");
    assert_eq!(cpa_json::parse(&resp.payload).g("model").str(), "alias-opus-4-6");
}

// ---------------------------------------------------------------- cloaked date pin

/// Calendar date of the currentDate reminder in the first user message of a captured body.
fn cloak_date(body: &[u8]) -> String {
    let root = cpa_json::parse(body);
    let content = root.g("messages.0.content");
    let texts: Vec<String> =
        if content.is_array() { content.array().iter().map(|b| b.g("text").str()).collect() } else { vec![content.str()] };
    for text in texts {
        if let Some(rest) = text.split("Today's date is ").nth(1) {
            return rest.chars().take(10).collect();
        }
    }
    panic!("no currentDate reminder in first user message: {}", String::from_utf8_lossy(body));
}

fn date_pin_auth(id: &str, base_url: &str) -> Auth {
    let mut auth = Auth::new(id, "claude");
    auth.attributes.insert("api_key".into(), "sk-ant-oat-test-oauth-key-date-pin".into());
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes.insert("cloak_mode".into(), "always".into());
    // The test clock is UTC wall time.
    auth.attributes.insert("timezone".into(), "UTC".into());
    auth.metadata.insert("account_uuid".into(), "11111111-2222-4333-8444-555555555555".into());
    auth.metadata.insert("claude_device_ids".into(), serde_json::json!([format!("{:064x}", 0xd1)]));
    auth
}

fn utc_unix(day: u32, hour: u32, minute: u32) -> i64 {
    chrono::NaiveDate::from_ymd_opt(2026, 8, day).and_then(|d| d.and_hms_opt(hour, minute, 0)).map_or(0, |t| t.and_utc().timestamp())
}

fn session_opts(session: &str, stream: bool) -> Options {
    let mut opts = Options::new(Format::Claude);
    opts.headers.insert("x-session-id", session.parse().unwrap());
    opts.stream = stream;
    opts
}

/// Two requests (Execute and ExecuteStream) of one session across simulated local midnight carry
/// byte-identical first messages, and a fresh session re-anchors to the new date.
#[tokio::test]
async fn cloaked_date_reminder_session_byte_stability() {
    let upstream = mock_upstream(200, "application/json", MESSAGE.to_string()).await;
    let auth = date_pin_auth("test-date-pin-exec-oauth", &upstream.base_url);
    let payload = r#"{"model":"claude-opus-4-6","system":[{"type":"text","text":"You are an expert software engineer.","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":"Say OK."}],"max_tokens":100}"#;
    let exec = executor();
    let (req, _) = claude_request(payload);

    super::tz::test_clock::set(&auth.id, utc_unix(1, 23, 59));
    exec.execute(&auth, req.clone(), session_opts("date-pin-session-1", false)).await.unwrap();

    super::tz::test_clock::set(&auth.id, utc_unix(2, 0, 1));
    exec.execute(&auth, req.clone(), session_opts("date-pin-session-1", false)).await.unwrap();
    let mut streamed = exec.execute_stream(&auth, req.clone(), session_opts("date-pin-session-1", true)).await.unwrap();
    while streamed.chunks.recv().await.is_some() {}

    let bodies: Vec<Vec<u8>> = upstream.captured.lock().iter().map(|c| c.body.clone()).collect();
    assert_eq!(bodies.len(), 3);
    let first = cpa_json::parse(&bodies[0]).g("messages.0").raw();
    for body in &bodies[1..] {
        assert_eq!(cpa_json::parse(body).g("messages.0").raw(), first, "messages[0] changed between requests of one session");
    }
    for body in &bodies {
        assert_eq!(cloak_date(body), "2026-08-01");
    }

    exec.execute(&auth, req, session_opts("date-pin-session-2", false)).await.unwrap();
    let last = upstream.captured.lock().last().unwrap().body.clone();
    assert_eq!(cloak_date(&last), "2026-08-02", "a fresh session re-anchors to the current date");
}

/// A probe declassified by a payload override across midnight keeps the session's pinned date.
#[tokio::test]
async fn cloaked_date_reminder_declassified_probe_pins_session_date() {
    let upstream = mock_upstream(200, "application/json", MESSAGE.to_string()).await;
    let auth = date_pin_auth("test-date-pin-exec-oauth-declass", &upstream.base_url);
    // A payload override turns the max_tokens: 1 probe into a normal request.
    let cfg = cpa_config::parse_config_bytes(
        b"payload:\n  override:\n    - models:\n        - name: claude-opus-4-6\n          protocol: claude\n      params:\n        max_tokens: 100\n",
    )
    .unwrap();
    let (_tx, rx) = watch::channel(Arc::new(cfg));
    std::mem::forget(_tx);
    let exec = super::new(rx);

    super::tz::test_clock::set(&auth.id, utc_unix(1, 23, 59));
    let normal = claude_request(r#"{"model":"claude-opus-4-6","messages":[{"role":"user","content":"normal request"}],"max_tokens":100}"#).0;
    exec.execute(&auth, normal, session_opts("declass-session-1", false)).await.unwrap();

    super::tz::test_clock::set(&auth.id, utc_unix(2, 0, 1));
    let probe = claude_request(r#"{"model":"claude-opus-4-6","messages":[{"role":"user","content":"probe"}],"max_tokens":1}"#).0;
    exec.execute(&auth, probe.clone(), session_opts("declass-session-1", false)).await.unwrap();
    let mut streamed = exec.execute_stream(&auth, probe, session_opts("declass-session-1", true)).await.unwrap();
    while streamed.chunks.recv().await.is_some() {}

    let bodies: Vec<Vec<u8>> = upstream.captured.lock().iter().map(|c| c.body.clone()).collect();
    assert_eq!(bodies.len(), 3);
    assert_eq!(cloak_date(&bodies[1]), "2026-08-01", "declassified Execute request");
    assert_eq!(cloak_date(&bodies[2]), "2026-08-01", "declassified ExecuteStream request");
}
