//! Executor-level tests against a local mock of the Codeium server (the Go httptest servers).

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::response::Response as HttpResponse;
use cpa_config::Config;
use cpa_json::J;
use tokio::net::TcpListener;
use tokio::sync::watch;

use super::wire::{
    CONNECT_FLAG_END_STREAM, FINGERPRINT_HEX_LEN, wrap_connect_envelope,
    wrap_connect_envelope_with_flag,
};
use super::*;
use crate::devin::test_support::golden;

// ------------------------------------------------------------------ helpers

fn field_bytes(num: u32, v: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    pb::put_bytes(&mut b, num, v);
    b
}

fn field_varint(num: u32, v: u64) -> Vec<u8> {
    let mut b = Vec::new();
    pb::put_varint_field(&mut b, num, v);
    b
}

/// A recorded upstream call.
#[derive(Clone, Debug)]
struct Received {
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

struct Mock {
    base_url: String,
    received: Arc<Mutex<Vec<Received>>>,
}

/// A canned upstream reply.
#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn ok(body: Vec<u8>) -> Self {
        Reply {
            status: 200,
            headers: vec![("content-type", "application/connect+proto".into())],
            body,
        }
    }
}

#[derive(Clone)]
struct MockState {
    reply: Reply,
    received: Arc<Mutex<Vec<Received>>>,
}

async fn mock_handler(State(st): State<MockState>, req: HttpRequest<Body>) -> HttpResponse {
    let (parts, body) = req.into_parts();
    let body = to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default()
        .to_vec();
    st.received.lock().push(Received {
        path: parts.uri.path().to_string(),
        headers: parts.headers,
        body,
    });
    let mut resp = HttpResponse::builder().status(StatusCode::from_u16(st.reply.status).unwrap());
    for (k, v) in &st.reply.headers {
        resp = resp.header(*k, v);
    }
    resp.body(Body::from(st.reply.body.clone())).unwrap()
}

async fn spawn_mock(reply: Reply) -> Mock {
    let received = Arc::new(Mutex::new(Vec::new()));
    let state = MockState {
        reply,
        received: received.clone(),
    };
    let app = Router::new().fallback(mock_handler).with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Mock {
        base_url: format!("http://{addr}"),
        received,
    }
}

fn executor(cfg: Config) -> (DevinExecutor, watch::Sender<Arc<Config>>) {
    let (tx, rx) = watch::channel(Arc::new(cfg));
    let mut exec = DevinExecutor::new(rx);
    exec.test_client = Some(
        reqwest::Client::builder()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .build()
            .unwrap(),
    );
    (exec, tx)
}

fn devin_auth(base_url: &str) -> Auth {
    let mut auth = Auth::new("devin-test.json", "devin");
    auth.attributes.insert("api_key".into(), "tok-123".into());
    auth.attributes.insert("base_url".into(), base_url.into());
    auth.attributes
        .insert("header:X-Custom".into(), "custom-value".into());
    auth
}

/// Frames of a normal reply: text, usage and the EOS trailer.
fn text_reply(text: &str) -> Vec<u8> {
    let mut usage = field_varint(2, 100);
    usage.extend(field_varint(3, 50));
    usage.extend(field_varint(5, 20));
    let mut frame = field_bytes(3, text.as_bytes());
    frame.extend(field_bytes(7, &usage));
    let mut body = wrap_connect_envelope(&frame);
    body.extend(wrap_connect_envelope_with_flag(
        CONNECT_FLAG_END_STREAM,
        b"{}",
    ));
    body
}

fn request(model: &str, payload: &str) -> Request {
    Request {
        model: model.into(),
        payload: Bytes::from(payload.to_string()),
        format: Format::OpenAI,
        metadata: Default::default(),
    }
}

fn opts(format: Format, stream: bool, original: &str) -> Options {
    let mut o = Options::new(format);
    o.stream = stream;
    o.original_request = Bytes::from(original.to_string());
    o
}

const OPENAI_REQUEST: &str = r#"{"model":"devin/swe-2","messages":[{"role":"system","content":"be brief"},{"role":"user","content":"hi"}]}"#;

/// The protobuf payload of the (single) chat request the mock received.
fn chat_payload(received: &Received) -> Vec<u8> {
    assert_eq!(received.body[0], 0, "data frame flag");
    let len = u32::from_be_bytes(received.body[1..5].try_into().unwrap()) as usize;
    assert_eq!(received.body.len(), 5 + len);
    received.body[5..].to_vec()
}

/// First value of a top-level field.
fn top_field(msg: &[u8], wanted: u32) -> Option<Vec<u8>> {
    let mut pos = 0;
    while pos < msg.len() {
        let (num, typ, n) = pb::get_tag(&msg[pos..])?;
        pos += n;
        match typ {
            pb::BYTES => {
                let (v, m) = pb::get_bytes(&msg[pos..])?;
                pos += m;
                if num == wanted {
                    return Some(v.to_vec());
                }
            }
            other => pos += pb::skip_field(num, other, &msg[pos..])?,
        }
    }
    None
}

/// First varint value of a field in a message.
fn varint_field_of(msg: &[u8], wanted: u32) -> Option<u64> {
    let mut pos = 0;
    while pos < msg.len() {
        let (num, typ, n) = pb::get_tag(&msg[pos..])?;
        pos += n;
        if typ == pb::VARINT {
            let (v, m) = pb::get_varint(&msg[pos..])?;
            pos += m;
            if num == wanted {
                return Some(v);
            }
        } else {
            pos += pb::skip_field(num, typ, &msg[pos..])?;
        }
    }
    None
}

/// Drops per-request random values: prompt message ids (3.1), the session id inside field 15
/// (15.1) and the cascade id (field 16). Mirrors the mask the golden was recorded with.
fn mask_proto(msg: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < msg.len() {
        let (num, typ, n) = pb::get_tag(&msg[pos..]).unwrap();
        let vn = pb::skip_field(num, typ, &msg[pos + n..]).unwrap();
        let raw = &msg[pos..pos + n + vn];
        match (num, typ) {
            (16, pb::BYTES) => {}
            (3 | 15, pb::BYTES) => {
                let (sub, _) = pb::get_bytes(&msg[pos + n..]).unwrap();
                let mut masked = Vec::new();
                let mut sp = 0;
                while sp < sub.len() {
                    let (sn, st, tn) = pb::get_tag(&sub[sp..]).unwrap();
                    let sv = pb::skip_field(sn, st, &sub[sp + tn..]).unwrap();
                    if sn != 1 {
                        masked.extend_from_slice(&sub[sp..sp + tn + sv]);
                    }
                    sp += tn + sv;
                }
                pb::put_bytes(&mut out, num, &masked);
            }
            _ => out.extend_from_slice(raw),
        }
        pos += n + vn;
    }
    out
}

// ------------------------------------------------------------------ credentials and headers

#[test]
fn credentials_prefer_attributes_then_metadata() {
    let mut auth = Auth::new("a", "devin");
    auth.attributes
        .insert("session_token".into(), "token-xyz".into());
    auth.attributes
        .insert("base_url".into(), "https://custom.endpoint.com".into());
    auth.attributes
        .insert("device_seed".into(), "seed-456".into());
    auth.metadata.insert("api_key".into(), "meta-key".into());
    assert_eq!(
        credentials(Some(&auth)),
        Credentials {
            api_key: "token-xyz".into(),
            base_url: "https://custom.endpoint.com".into(),
            device_seed: "seed-456".into(),
        }
    );

    // Metadata fills what attributes lack; its base_url only applies over the default.
    let mut auth = Auth::new("b", "devin");
    auth.metadata
        .insert("session_token".into(), "  meta-token ".into());
    auth.metadata
        .insert("base_url".into(), "https://meta.example".into());
    auth.metadata
        .insert("device_seed".into(), "meta-seed".into());
    let c = credentials(Some(&auth));
    assert_eq!(
        (
            c.api_key.as_str(),
            c.base_url.as_str(),
            c.device_seed.as_str()
        ),
        ("meta-token", "https://meta.example", "meta-seed")
    );
    auth.attributes
        .insert("base_url".into(), "https://attr.example".into());
    assert_eq!(credentials(Some(&auth)).base_url, "https://attr.example");

    assert_eq!(
        credentials(None),
        Credentials {
            api_key: String::new(),
            base_url: DEFAULT_BASE_URL.into(),
            device_seed: String::new()
        }
    );
}

#[test]
fn headers_match_the_native_client() {
    let mut auth = Auth::new("a", "devin");
    auth.attributes
        .insert("api_key".into(), "my-secret-key".into());
    auth.attributes
        .insert("header:X-Extra".into(), "extra".into());
    auth.attributes
        .insert("header:Accept".into(), "application/json".into());
    let h = prepare_headers(Some(&auth), CHAT_PATH, None, None);
    assert_eq!(h["authorization"], "Basic my-secret-key-my-secret-key");
    assert_eq!(h["content-type"], "application/connect+proto");
    assert_eq!(h["connect-protocol-version"], "1");
    assert_eq!(h["user-agent"], "");
    assert_eq!(h["x-extra"], "extra");
    // Custom headers are applied last and override the built-in value.
    assert_eq!(h["accept"], "application/json");
    let trace: Vec<usize> = h["sentry-trace"]
        .to_str()
        .unwrap()
        .split('-')
        .map(str::len)
        .collect();
    assert_eq!(trace, [32, 16, 1]);

    // Unary status and catalog calls carry no Sentry-Trace.
    let unary = prepare_headers(
        Some(&auth),
        "/exa.seat_management_pb.SeatManagementService/GetUserStatus",
        None,
        None,
    );
    assert!(!unary.contains_key("sentry-trace"));
}

#[test]
fn status_errors_carry_retry_after_only_on_429() {
    let mut h = HeaderMap::new();
    h.insert(http::header::RETRY_AFTER, HeaderValue::from_static("30"));
    let e = new_status_error(429, &h, b"rate limited");
    assert_eq!(
        (e.status, e.message.as_str(), e.retry_after),
        (429, "rate limited", Some(Duration::from_secs(30)))
    );

    let future = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(60));
    h.insert(
        http::header::RETRY_AFTER,
        HeaderValue::from_str(&future).unwrap(),
    );
    let delay = new_status_error(429, &h, b"x").retry_after.unwrap();
    assert!(delay > Duration::ZERO && delay <= Duration::from_secs(61));

    // Past dates and negative seconds give no hint; other statuses never do.
    let past = httpdate::fmt_http_date(SystemTime::now() - Duration::from_secs(60));
    h.insert(
        http::header::RETRY_AFTER,
        HeaderValue::from_str(&past).unwrap(),
    );
    assert_eq!(new_status_error(429, &h, b"x").retry_after, None);
    h.insert(http::header::RETRY_AFTER, HeaderValue::from_static("-5"));
    assert_eq!(new_status_error(429, &h, b"x").retry_after, None);
    h.insert(http::header::RETRY_AFTER, HeaderValue::from_static("30"));
    assert_eq!(new_status_error(500, &h, b"server error").retry_after, None);
    assert_eq!(new_status_error(500, &h, b"").message, "status 500");
}

// ------------------------------------------------------------------ request building

#[test]
fn max_tokens_are_clamped_to_the_model_limit() {
    use cpa_core::registry::{ModelInfo, global_registry};
    let registry = global_registry();
    let client = "test-devin-clamp-client";
    registry.register_client(
        client,
        "devin",
        &[ModelInfo {
            id: "devin/swe-2-clamp-test".into(),
            max_completion_tokens: 64_000,
            context_length: 262_000,
            ..Default::default()
        }],
    );
    let (exec, _tx) = executor(Config::default());
    let auth = devin_auth("http://unused.invalid");
    let payload = |tokens: u64| {
        format!(
            r#"{{"generation_config":{{"max_output_tokens":{tokens}}},"input":[{{"type":"user_input","content":[{{"type":"text","text":"hello"}}]}}]}}"#
        )
    };
    let max_tokens = |tokens: u64| {
        let prepared = exec
            .prepare_request(
                &auth,
                &request("devin/swe-2-clamp-test", &payload(tokens)),
                &opts(Format::Interactions, false, ""),
            )
            .unwrap();
        // Completion config is field 8; its sub-field 2 is the token limit.
        let f8 = top_field(&prepared.body[5..], 8).unwrap();
        varint_field_of(&f8, 2).unwrap()
    };
    assert_eq!(
        max_tokens(100_000),
        64_000,
        "oversized request clamps to the registered limit"
    );
    assert_eq!(max_tokens(1_000), 1_000, "smaller request is kept");
    registry.unregister_client(client);
}

#[test]
fn client_requests_encode_like_go() {
    // Client payload in each format through translation, parsing and encoding, with the
    // random ids masked on both sides.
    let (exec, _tx) = executor(Config::default());
    let mut auth = Auth::new("a", "devin");
    auth.attributes.insert("api_key".into(), "tok".into());
    auth.attributes.insert("device_seed".into(), "seed".into());
    for case in golden()["e2e"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let payload = case["payload"].as_str().unwrap();
        let format =
            Format::parse(case["source_format"].as_str().unwrap()).unwrap_or(Format::Interactions);
        let req = request(case["model"].as_str().unwrap(), payload);
        let prepared = exec
            .prepare_request(&auth, &req, &opts(format, false, payload))
            .unwrap();
        assert_eq!(prepared.url, case["url"].as_str().unwrap(), "case {name}");
        assert_eq!(
            hex::encode(mask_proto(&prepared.body[5..])),
            case["hex"].as_str().unwrap(),
            "case {name}"
        );
    }
}

#[test]
fn missing_credentials_fail_before_any_request() {
    let (exec, _tx) = executor(Config::default());
    let auth = Auth::new("devin-empty.json", "devin");
    let err = exec
        .prepare_request(
            &auth,
            &request("swe-2", "{}"),
            &opts(Format::Interactions, false, ""),
        )
        .err()
        .unwrap();
    assert_eq!(
        err.message,
        "devin credentials missing: api_key or session_token required"
    );
    assert!(!err.upstream_attempted);
}

#[tokio::test]
async fn configured_sensitive_words_drop_matching_system_prompt_lines() {
    let mock = spawn_mock(Reply::ok(text_reply("ok"))).await;
    let mut cfg = Config::default();
    cfg.devin.sensitive_words = vec!["Project Zeus".into()];
    let (exec, tx) = executor(cfg);
    let auth = devin_auth(&mock.base_url);
    let payload = r#"{"model":"devin/swe-2","messages":[{"role":"system","content":"keep this\nsecret Project Zeus plan"},{"role":"user","content":"hi"}]}"#;
    exec.execute(
        &auth,
        request("devin/swe-2", payload),
        opts(Format::OpenAI, false, payload),
    )
    .await
    .unwrap();
    let body = chat_payload(&mock.received.lock()[0]);
    let system = String::from_utf8(top_field(&body, 2).unwrap()).unwrap();
    assert_eq!(system, "keep this");

    // The live config is re-read per request: clearing the words keeps the line.
    tx.send(Arc::new(Config::default())).unwrap();
    exec.execute(
        &auth,
        request("devin/swe-2", payload),
        opts(Format::OpenAI, false, payload),
    )
    .await
    .unwrap();
    let body = chat_payload(&mock.received.lock()[1]);
    let system = String::from_utf8(top_field(&body, 2).unwrap()).unwrap();
    assert_eq!(system, "keep this\nsecret Project Zeus plan");
}

// ------------------------------------------------------------------ execution

#[tokio::test]
async fn execute_translates_a_chat_completion_and_reports_usage() {
    let mock = spawn_mock(Reply::ok(text_reply("Hello from Devin"))).await;
    let (exec, _tx) = executor(Config::default());
    let auth = devin_auth(&mock.base_url);

    let resp = exec
        .execute(
            &auth,
            request("devin/swe-2", OPENAI_REQUEST),
            opts(Format::OpenAI, false, OPENAI_REQUEST),
        )
        .await
        .unwrap();
    let body = cpa_json::parse(&resp.payload);
    assert_eq!(body.g("object").str(), "chat.completion");
    assert_eq!(
        body.g("choices.0.message.content").str(),
        "Hello from Devin"
    );
    assert_eq!(body.g("usage.prompt_tokens").int(), 120);
    assert_eq!(body.g("usage.completion_tokens").int(), 50);
    assert_eq!(resp.metadata["usage"]["input_tokens"], 120);
    assert_eq!(resp.metadata["usage"]["cached_tokens"], 20);
    assert_eq!(resp.metadata["usage"]["total_tokens"], 170);
    assert_eq!(resp.headers["content-type"], "application/connect+proto");

    let received = mock.received.lock()[0].clone();
    assert_eq!(received.path, CHAT_PATH);
    assert_eq!(received.headers["authorization"], "Basic tok-123-tok-123");
    assert_eq!(
        received.headers["content-type"],
        "application/connect+proto"
    );
    assert_eq!(received.headers["connect-protocol-version"], "1");
    assert_eq!(received.headers["x-custom"], "custom-value");
    assert_eq!(received.headers["sentry-trace"].len(), 32 + 1 + 16 + 2);
    assert!(
        !received.headers.contains_key("user-agent"),
        "User-Agent must be absent on the wire"
    );
    assert!(
        !received.headers.contains_key("accept-encoding"),
        "compression negotiation is disabled"
    );

    let proto = chat_payload(&received);
    assert_eq!(top_field(&proto, 21).unwrap(), b"swe-2-high");
    assert_eq!(top_field(&proto, 2).unwrap(), b"be brief");
    let meta = top_field(&proto, 1).unwrap();
    assert_eq!(top_field(&meta, 3).unwrap(), b"tok-123");
    assert_eq!(top_field(&meta, 31).unwrap().len(), FINGERPRINT_HEX_LEN);
    // The user turn is the only history prompt: message uuid, source 1, content.
    let prompt = top_field(&proto, 3).unwrap();
    assert_eq!(top_field(&prompt, 3).unwrap(), b"hi");
}

#[tokio::test]
async fn execute_in_interactions_format_returns_the_interactions_response() {
    let mock = spawn_mock(Reply::ok(text_reply("direct"))).await;
    let (exec, _tx) = executor(Config::default());
    let payload = r#"{"input":[{"type":"user_input","content":[{"type":"text","text":"hi"}]}]}"#;
    let resp = exec
        .execute(
            &devin_auth(&mock.base_url),
            request("swe-2", payload),
            opts(Format::Interactions, false, ""),
        )
        .await
        .unwrap();
    let body = cpa_json::parse(&resp.payload);
    assert_eq!(body.g("steps.0.content.0.text").str(), "direct");
    assert_eq!(body.g("usage.total_tokens").int(), 170);
}

#[tokio::test]
async fn execute_stream_translates_chunks_and_sends_usage() {
    let mock = spawn_mock(Reply::ok(text_reply("streamed"))).await;
    let (exec, _tx) = executor(Config::default());
    let claude_request = r#"{"model":"devin/swe-2","max_tokens":100,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    let mut result = exec
        .execute_stream(
            &devin_auth(&mock.base_url),
            request("devin/swe-2", claude_request),
            opts(Format::Claude, true, claude_request),
        )
        .await
        .unwrap();
    let mut text = String::new();
    while let Some(chunk) = result.chunks.recv().await {
        text.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
    }
    assert!(text.contains("event: message_start"), "{text}");
    assert!(text.contains(r#""text":"streamed""#), "{text}");
    assert!(text.contains("event: message_stop"), "{text}");
    let usage = result.usage.take().unwrap().await.unwrap();
    assert_eq!(
        (
            usage["input_tokens"].as_i64(),
            usage["output_tokens"].as_i64()
        ),
        (Some(120), Some(50))
    );
    assert_eq!(result.headers["content-type"], "application/connect+proto");
}

#[tokio::test]
async fn upstream_rate_limit_carries_retry_after() {
    let reply = Reply {
        status: 429,
        headers: vec![("retry-after", "30".into())],
        body: b"slow down".to_vec(),
    };
    let mock = spawn_mock(reply).await;
    let (exec, _tx) = executor(Config::default());
    let auth = devin_auth(&mock.base_url);
    let err = exec
        .execute(
            &auth,
            request("devin/swe-2", OPENAI_REQUEST),
            opts(Format::OpenAI, false, OPENAI_REQUEST),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (err.status, err.message.as_str(), err.retry_after),
        (429, "slow down", Some(Duration::from_secs(30)))
    );
    let err = exec
        .execute_stream(
            &auth,
            request("devin/swe-2", OPENAI_REQUEST),
            opts(Format::OpenAI, true, OPENAI_REQUEST),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(
        (err.status, err.retry_after),
        (429, Some(Duration::from_secs(30)))
    );
}

fn trailer_error_reply(code: &str, message: &str, leading_text: Option<&str>) -> Reply {
    let mut body = Vec::new();
    if let Some(text) = leading_text {
        body.extend(wrap_connect_envelope(&field_bytes(3, text.as_bytes())));
    }
    let trailer = serde_json::json!({"error": {"code": code, "message": message}}).to_string();
    body.extend(wrap_connect_envelope_with_flag(
        CONNECT_FLAG_END_STREAM,
        trailer.as_bytes(),
    ));
    Reply::ok(body)
}

#[tokio::test]
async fn trailer_errors_become_status_errors() {
    let (exec, _tx) = executor(Config::default());

    // Non-stream: quota exhaustion is a 429 error.
    let mock = spawn_mock(trailer_error_reply(
        "failed_precondition",
        "monthly ACU quota exhausted",
        Some("partial"),
    ))
    .await;
    let err = exec
        .execute(
            &devin_auth(&mock.base_url),
            request("devin/swe-2", OPENAI_REQUEST),
            opts(Format::OpenAI, false, OPENAI_REQUEST),
        )
        .await
        .unwrap_err();
    assert_eq!(err.status, 429);
    assert_eq!(
        err.message,
        "devin upstream error (failed_precondition): monthly ACU quota exhausted"
    );

    // Stream, before any content: the only item is the error (no partial success events).
    let mock = spawn_mock(trailer_error_reply("unauthenticated", "expired", None)).await;
    let mut result = exec
        .execute_stream(
            &devin_auth(&mock.base_url),
            request("devin/swe-2", OPENAI_REQUEST),
            opts(Format::OpenAI, true, OPENAI_REQUEST),
        )
        .await
        .unwrap();
    let first = result.chunks.recv().await.unwrap().unwrap_err();
    assert_eq!(first.status, 401);
    assert!(result.chunks.recv().await.is_none());
    assert!(
        result.usage.take().unwrap().await.is_err(),
        "failed streams report no usage"
    );
}

#[tokio::test]
async fn truncated_streams_fail_instead_of_completing() {
    let body = wrap_connect_envelope(&field_bytes(3, b"cut off"));
    let mock = spawn_mock(Reply::ok(body)).await;
    let (exec, _tx) = executor(Config::default());
    let err = exec
        .execute(
            &devin_auth(&mock.base_url),
            request("devin/swe-2", OPENAI_REQUEST),
            opts(Format::OpenAI, false, OPENAI_REQUEST),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.message,
        "devin upstream stream terminated prematurely before EOS trailer"
    );
}

// ------------------------------------------------------------------ refresh and counting

fn user_status_response() -> Vec<u8> {
    let mut org = field_bytes(4, b"org-test-devin");
    org.extend(field_bytes(8, b"XCodeCLI"));
    let mut plan_info = field_bytes(2, b"Pro");
    plan_info.extend(field_bytes(33, &org));
    let mut plan_status = field_bytes(1, &plan_info);
    for (n, v) in [(14, 95), (15, 45), (17, 1_789_200_000), (18, 1_789_286_400)] {
        plan_status.extend(field_varint(n, v));
    }
    let mut user = field_bytes(3, b"refreshuser");
    user.extend(field_bytes(5, b"team-xyz"));
    user.extend(field_bytes(7, b"refreshuser@example.com"));
    user.extend(field_bytes(13, &plan_status));
    user.extend(field_bytes(36, b"user-id-999"));
    field_bytes(1, &user)
}

#[tokio::test]
async fn refresh_rereads_user_status_into_metadata_and_quota_signals() {
    let mock = spawn_mock(Reply {
        status: 200,
        headers: vec![("content-type", "application/proto".into())],
        body: user_status_response(),
    })
    .await;
    let (exec, _tx) = executor(Config::default());
    let mut auth = devin_auth(&mock.base_url);
    auth.attributes
        .insert("api_key".into(), "devin-session-token$test".into());

    let updated = exec.refresh(&auth).await.unwrap();
    assert_eq!(updated.metadata["plan"], "Pro");
    assert_eq!(updated.metadata["email"], "refreshuser@example.com");
    assert_eq!(updated.metadata["user_name"], "refreshuser");
    assert_eq!(updated.attributes["org_name"], "XCodeCLI");
    // Quota percentages live only in the signals.
    assert!(
        !updated
            .metadata
            .contains_key("daily_quota_remaining_percent")
    );
    assert_eq!(
        updated.quota.signals["daily_quota_remaining_percent"],
        "95%"
    );
    assert_eq!(
        updated.quota.signals["weekly_quota_remaining_percent"],
        "45%"
    );
    assert_eq!(
        updated.quota.signals["daily_quota_reset_at"],
        "2026-09-12T08:00:00Z"
    );
    assert!(updated.quota.observed_at.is_some() && updated.last_refreshed_at.is_some());

    let received = mock.received.lock()[0].clone();
    assert_eq!(received.path, cpa_auth::devin::GET_USER_STATUS_PATH);
    assert_eq!(
        received.headers["authorization"],
        "Basic devin-session-token$test-devin-session-token$test"
    );
    assert_eq!(received.headers["content-type"], "application/proto");

    // No credential: nothing to refresh, the auth comes back unchanged.
    let blank = Auth::new("blank", "devin");
    assert_eq!(exec.refresh(&blank).await.unwrap().id, "blank");
}

#[tokio::test]
async fn refresh_failure_is_an_error() {
    let mock = spawn_mock(Reply {
        status: 500,
        headers: vec![],
        body: b"boom".to_vec(),
    })
    .await;
    let (exec, _tx) = executor(Config::default());
    let err = exec.refresh(&devin_auth(&mock.base_url)).await.unwrap_err();
    assert!(
        err.message.contains("status 500") && err.message.contains("boom"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn count_tokens_estimates_a_quarter_of_the_payload_bytes() {
    let (exec, _tx) = executor(Config::default());
    let payload = "x".repeat(400);
    let resp = exec
        .count_tokens(
            &Auth::new("a", "devin"),
            request("swe-2", &payload),
            opts(Format::OpenAI, false, ""),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.payload.as_ref(),
        br#"{"total_tokens":100,"input_tokens":100}"#
    );
    assert_eq!(exec.identifier(), "devin");
    assert!(exec.supports_apply_patch("swe-2"));
}
