//! Behavior of the live handlers against a mock upstream (Go: live_test.go, capabilities_test.go,
//! client_secret_test.go, websocket_test.go).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use common::*;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_live::{Caller, ClientSecretCaller, LiveSession, MediaError, MediaRelayFactory, MediaRelaySession, MediaRoute};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn json_of(reply: &cpa_live::Reply) -> Value {
    serde_json::from_slice(&reply.body).unwrap_or_else(|_| panic!("not json: {}", reply.text()))
}

fn caller(principal: &str, provider: &str) -> Caller {
    Caller { principal: principal.into(), provider: provider.into(), ..Caller::default() }
}

fn multipart_headers(boundary: &str) -> Vec<(String, String)> {
    vec![("content-type".into(), format!("multipart/form-data; boundary={boundary}"))]
}

fn parts_with(path: &str, headers: Vec<(String, String)>) -> cpa_live::RequestParts {
    let refs: Vec<(&str, &str)> = headers.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    parts(path, &refs)
}

// ---------------------------------------------------------------- call bootstrap

#[tokio::test]
async fn call_rewrites_live_request_and_schedules_oauth() {
    let mut api_key = Auth::new("codex-api-key", "codex");
    api_key.attributes.insert("api_key".into(), "must-not-be-used".into());
    let env = env(vec![api_key, oauth_auth("codex-oauth", "oauth-token", Some("account-123"))]).await;
    let upstream = Arc::new(UpstreamState::default());
    point_at(&env, &serve_upstream(upstream.clone()).await);

    let boundary = "codex-realtime-call-boundary";
    let body = multipart_body(boundary, "v=0\r\na=setup:actpass", r#"{"model":"gpt-live-1-codex"}"#);
    let mut headers = multipart_headers(boundary);
    for (k, v) in [
        ("authorization", "Bearer downstream-api-key"),
        ("originator", "Codex Desktop"),
        ("thread-id", "thread-123"),
        ("session-id", "session-123"),
        ("openai-alpha", "quicksilver=v2"),
        ("x-oai-attestation", "attestation-token"),
    ] {
        headers.push((k.into(), v.into()));
    }
    let reply = env.handler.handle_call(&caller("k", "static"), &parts_with("/v1/live", headers), Bytes::from(body)).await;
    assert_eq!(reply.status, 201, "{}", reply.text());

    let seen = upstream.last();
    assert_eq!(seen.method, "POST");
    let payload: Value = serde_json::from_slice(&seen.body).unwrap();
    assert_eq!(payload["sdp"], "v=0\r\na=setup:actpass");
    assert_eq!(payload["session"]["model"], "gpt-live-1-codex");
    assert_eq!(seen.header("content-type"), "application/json");
    assert_eq!(seen.header("authorization"), "Bearer oauth-token");
    assert_eq!(seen.header("chatgpt-account-id"), "account-123");
    for (name, want) in [
        ("openai-alpha", "quicksilver=v2"),
        ("originator", "Codex Desktop"),
        ("session-id", "session-123"),
        ("thread-id", "thread-123"),
        ("x-oai-attestation", "attestation-token"),
    ] {
        assert_eq!(seen.header(name), want, "{name}");
    }
    assert_eq!(reply.text(), "v=0\r\na=ice-lite\r\n");
    assert_eq!(reply.headers.get("location").unwrap(), "/v1/live/call-123");
    for blocked in ["set-cookie", "x-live-session"] {
        assert!(reply.headers.get(blocked).is_none(), "{blocked} leaked");
    }
    assert_eq!(reply.headers.get("x-request-id").unwrap(), "req-1");
    let stored = env.handler.sessions().peek("call-123").expect("stored session");
    assert_eq!((stored.auth_id.as_str(), stored.model.as_str()), ("codex-oauth", "gpt-live-1-codex"));
    assert_eq!((stored.owner_principal.as_str(), stored.owner_provider.as_str()), ("k", "static"));
}

#[tokio::test]
async fn standard_realtime_call_maps_model_and_location() {
    let env = env(vec![oauth_auth("codex-oauth", "oauth-token", None)]).await;
    let upstream = Arc::new(UpstreamState::default());
    upstream.reply.lock().body = "v=0\r\n";
    point_at(&env, &serve_upstream(upstream.clone()).await);
    let boundary = "standard-realtime-boundary";
    let body = multipart_body(boundary, "v=0\r\n", r#"{"type":"realtime","model":"gpt-realtime"}"#);
    let reply =
        env.handler.handle_call(&Caller::default(), &parts_with("/v1/realtime/calls", multipart_headers(boundary)), Bytes::from(body)).await;
    assert_eq!(reply.status, 201, "{}", reply.text());
    assert_eq!(reply.headers.get("location").unwrap(), "/v1/realtime/calls/call-123");
    let payload: Value = serde_json::from_slice(&upstream.last().body).unwrap();
    assert_eq!(payload["session"]["model"], "gpt-live-1-codex");
}

#[tokio::test]
async fn raw_sdp_is_forwarded_untouched_without_a_relay() {
    let env = env(vec![oauth_auth("codex-oauth", "oauth-token", None)]).await;
    let upstream = Arc::new(UpstreamState::default());
    point_at(&env, &serve_upstream(upstream.clone()).await);
    let reply = env
        .handler
        .handle_call(&Caller::default(), &parts("/v1/live", &[("content-type", "application/sdp")]), Bytes::from_static(b"v=0\r\no=raw-offer\r\n"))
        .await;
    assert_eq!(reply.status, 201);
    let seen = upstream.last();
    assert_eq!(seen.header("content-type"), "application/sdp");
    assert_eq!(seen.body.as_ref(), b"v=0\r\no=raw-offer\r\n");
}

#[tokio::test]
async fn call_errors_before_the_upstream() {
    let env = env(vec![]).await;
    // No credential: the selection error is relayed (live shape on /v1/live).
    let reply = env.handler.handle_call(&Caller::default(), &parts("/v1/live", &[]), Bytes::new()).await;
    assert_eq!(reply.status, 503, "{}", reply.text());
    assert!(json_of(&reply)["error"].as_str().unwrap().starts_with("auth_not_found"));
    // Realtime-shaped on /v1/realtime paths.
    let reply = env.handler.handle_call(&Caller::default(), &parts("/v1/realtime/calls", &[]), Bytes::new()).await;
    assert_eq!(json_of(&reply)["error"]["code"], "realtime_request_failed");
    // Invalid JSON body.
    let reply = env
        .handler
        .handle_call(&Caller::default(), &parts("/v1/live", &[("content-type", "application/json")]), Bytes::from_static(b"{broken"))
        .await;
    assert_eq!(reply.status, 400);
    assert!(json_of(&reply)["error"].as_str().unwrap().starts_with("failed to decode Realtime call request: invalid character"));
    // Oversized body.
    let reply = env.handler.handle_call(&Caller::default(), &parts("/v1/live", &[]), Bytes::from(vec![0u8; (16 << 20) + 1])).await;
    assert_eq!(reply.status, 413);
}

#[tokio::test]
async fn upstream_rejection_is_relayed_and_no_session_is_stored() {
    let env = env(vec![oauth_auth("codex-oauth", "oauth-token", None)]).await;
    let upstream = Arc::new(UpstreamState::default());
    *upstream.reply.lock() = Reply { status: 429, headers: vec![("retry-after", "7"), ("x-secret", "no")], body: "slow down" };
    point_at(&env, &serve_upstream(upstream.clone()).await);
    let reply = env.handler.handle_call(&Caller::default(), &parts("/v1/live", &[("content-type", "application/sdp")]), Bytes::from_static(b"v=0\r\n")).await;
    assert_eq!(reply.status, 429);
    assert_eq!(reply.text(), "slow down");
    assert_eq!(reply.headers.get("retry-after").unwrap(), "7");
    assert!(reply.headers.get("x-secret").is_none());
    assert!(env.handler.sessions().peek("call-123").is_none());
}

// ---------------------------------------------------------------- media relay fakes

#[derive(Default)]
struct FakeSession {
    upstream_answer: Mutex<String>,
    call_id: Mutex<String>,
    call_id_at_accept: Mutex<String>,
    downstream_sdp: String,
    close_handler: Mutex<Option<Box<dyn Fn(&str) + Send + Sync>>>,
    close_reason: Mutex<String>,
    closed: AtomicBool,
    fail: Option<&'static str>,
}

#[async_trait]
impl MediaRelaySession for FakeSession {
    async fn accept_upstream_answer(&self, answer: &str) -> Result<String, MediaError> {
        *self.upstream_answer.lock() = answer.to_string();
        *self.call_id_at_accept.lock() = self.call_id.lock().clone();
        match self.fail {
            Some(e) => Err(MediaError::new(e)),
            None => Ok(self.downstream_sdp.clone()),
        }
    }
    fn set_call_id(&self, call_id: &str) {
        *self.call_id.lock() = call_id.to_string();
    }
    fn set_close_handler(&self, handler: Box<dyn Fn(&str) + Send + Sync>) {
        *self.close_handler.lock() = Some(handler);
    }
    fn close_with_reason(&self, reason: &str) {
        *self.close_reason.lock() = reason.to_string();
        self.closed.store(true, Ordering::SeqCst);
    }
}

struct FakeRelay {
    client_offer: Mutex<String>,
    route: Mutex<Option<MediaRoute>>,
    upstream_offer: &'static str,
    session: Arc<FakeSession>,
    error: Option<&'static str>,
}

#[async_trait]
impl MediaRelayFactory for FakeRelay {
    async fn new_session(&self, offer: &str, route: MediaRoute) -> Result<(Arc<dyn MediaRelaySession>, String), MediaError> {
        *self.client_offer.lock() = offer.to_string();
        *self.route.lock() = Some(route);
        match self.error {
            Some(e) => Err(MediaError::new(e)),
            None => Ok((self.session.clone() as Arc<dyn MediaRelaySession>, self.upstream_offer.to_string())),
        }
    }
}

fn fake_relay(session: Arc<FakeSession>) -> Arc<FakeRelay> {
    Arc::new(FakeRelay {
        client_offer: Mutex::default(),
        route: Mutex::default(),
        upstream_offer: "v=0\r\no=gateway-offer\r\n",
        session,
        error: None,
    })
}

#[tokio::test]
async fn call_relays_webrtc_media_sdp() {
    let mut auth = oauth_auth("codex-oauth", "oauth-token", None);
    auth.label = "Voice credential".into();
    auth.proxy_url = "direct".into();
    let mut cfg = Config::default();
    cfg.proxy_url = "http://global-proxy.example:8080".into();
    let env = env_with_config(cfg, vec![auth]).await;
    let upstream = Arc::new(UpstreamState::default());
    upstream.reply.lock().body = "v=0\r\no=upstream-answer\r\n";
    point_at(&env, &serve_upstream(upstream.clone()).await);
    let session = Arc::new(FakeSession { downstream_sdp: "v=0\r\no=downstream-answer\r\n".into(), ..Default::default() });
    let relay = fake_relay(session.clone());
    env.handler.set_media_relay(Some(relay.clone()));

    let boundary = "media-relay-boundary";
    let body = multipart_body(boundary, "v=0\r\no=desktop-offer\r\n", r#"{"model":"gpt-live-1-codex"}"#);
    let reply = env.handler.handle_call(&Caller::default(), &parts_with("/v1/live", multipart_headers(boundary)), Bytes::from(body)).await;
    assert_eq!(reply.status, 201, "{}", reply.text());
    assert_eq!(*relay.client_offer.lock(), "v=0\r\no=desktop-offer\r\n");
    let route = relay.route.lock().clone().unwrap();
    assert_eq!(route.proxy_url, "direct");
    assert_eq!(route.credential, "Voice credential");
    assert!(!route.auth_index.is_empty());
    let payload: Value = serde_json::from_slice(&upstream.last().body).unwrap();
    assert_eq!(payload["sdp"], relay.upstream_offer);
    assert_eq!(*session.upstream_answer.lock(), "v=0\r\no=upstream-answer\r\n");
    assert_eq!(*session.call_id.lock(), "call-123");
    assert_eq!(*session.call_id_at_accept.lock(), "call-123");
    assert_eq!(reply.text(), "v=0\r\no=downstream-answer\r\n");
    assert_eq!(reply.headers.get("content-type").unwrap(), "application/sdp");
    assert!(!session.closed.load(Ordering::SeqCst), "retained media session was closed early");
    let handler = session.close_handler.lock().take().expect("close handler installed");
    handler("test_closed");
    assert!(session.closed.load(Ordering::SeqCst));
    assert_eq!(*session.close_reason.lock(), "test_closed");
    assert!(env.handler.sessions().peek("call-123").is_none(), "completed media session remained stored");
}

#[tokio::test]
async fn unretained_media_sessions_are_closed() {
    for (name, status, answer_error, want) in [("upstream rejection", 401u16, None, 401u16), ("invalid upstream answer", 201, Some("invalid answer"), 502)] {
        let env = env(vec![oauth_auth("codex-oauth", "oauth-token", None)]).await;
        let upstream = Arc::new(UpstreamState::default());
        {
            let mut reply = upstream.reply.lock();
            reply.status = status;
            reply.body = "v=0\r\no=upstream-answer\r\n";
        }
        point_at(&env, &serve_upstream(upstream).await);
        let session = Arc::new(FakeSession { downstream_sdp: "x".into(), fail: answer_error, ..Default::default() });
        env.handler.set_media_relay(Some(fake_relay(session.clone())));
        let boundary = "media-error-boundary";
        let body = multipart_body(boundary, "v=0\r\no=desktop-offer\r\n", r#"{"model":"gpt-live-1-codex"}"#);
        let reply = env.handler.handle_call(&Caller::default(), &parts_with("/v1/live", multipart_headers(boundary)), Bytes::from(body)).await;
        assert_eq!(reply.status, want, "{name}: {}", reply.text());
        assert!(session.closed.load(Ordering::SeqCst), "{name}: media session retained");
        assert_eq!(*session.close_reason.lock(), "request_not_retained", "{name}");
        assert!(env.handler.sessions().peek("call-123").is_none(), "{name}");
    }
}

#[tokio::test]
async fn media_setup_failure_maps_to_bad_gateway() {
    let env = env(vec![oauth_auth("codex-oauth", "oauth-token", None)]).await;
    let relay = Arc::new(FakeRelay {
        client_offer: Mutex::default(),
        route: Mutex::default(),
        upstream_offer: "",
        session: Arc::new(FakeSession::default()),
        error: Some("media setup failed"),
    });
    env.handler.set_media_relay(Some(relay));
    let reply = env.handler.handle_call(&Caller::default(), &parts("/v1/live", &[("content-type", "application/sdp")]), Bytes::from_static(b"v=0\r\n")).await;
    assert_eq!(reply.status, 502);
    assert_eq!(json_of(&reply)["error"], "media setup failed");
}

// ---------------------------------------------------------------- client secrets

#[tokio::test]
async fn client_secret_maps_realtime_model_and_authenticates() {
    let env = env(vec![]).await;
    let body = br#"{"session":{"type":"realtime","model":"gpt-realtime","instructions":"help"},"expires_after":{"anchor":"created_at","seconds":60}}"#;
    let reply = env.handler.create_client_secret(&caller("issuer-key", "static"), body);
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.headers.get("cache-control").unwrap(), "no-store");
    let response = json_of(&reply);
    let value = response["value"].as_str().unwrap().to_string();
    assert!(value.starts_with("ek_"));
    let session = &response["session"];
    assert_eq!(session["object"], "realtime.session");
    assert_eq!((session["type"].as_str(), session["model"].as_str(), session["instructions"].as_str()), (Some("realtime"), Some("gpt-realtime"), Some("help")));
    let mut headers = http::HeaderMap::new();
    headers.insert("authorization", format!("Bearer {value}").parse().unwrap());
    let (authorization, matched, error) = env.handler.authenticate_client_secret(&headers);
    assert!(matched && error.is_none());
    let authorization = authorization.unwrap();
    assert_eq!(authorization.principal, session["id"].as_str().unwrap());
    assert_eq!((authorization.issuer_principal.as_str(), authorization.issuer_provider.as_str()), ("issuer-key", "static"));
    let upstream_session: Value = serde_json::from_str(&authorization.session).unwrap();
    assert_eq!(upstream_session["model"], "gpt-live-1-codex");

    // Unknown ek_ tokens match but fail; other bearers do not match.
    headers.insert("authorization", "Bearer ek_unknown".parse().unwrap());
    assert_eq!(env.handler.authenticate_client_secret(&headers).2.as_deref(), Some("Realtime client secret is invalid or expired"));
    headers.insert("authorization", "Bearer sk-regular".parse().unwrap());
    assert!(!env.handler.authenticate_client_secret(&headers).1);
}

#[tokio::test]
async fn client_secret_validation_errors() {
    let env = env(vec![]).await;
    let c = Caller::default();
    let reply = env.handler.create_client_secret(&c, br#"{"session":{"type":"transcription","model":"gpt-4o-transcribe"}}"#);
    assert_eq!(reply.status, 501);
    assert_eq!(json_of(&reply)["error"]["code"], "realtime_capability_not_supported");
    let reply = env.handler.create_client_secret(&c, br#"{"expires_after":{"seconds":5}}"#);
    assert_eq!((reply.status, json_of(&reply)["error"]["code"].as_str().unwrap().to_string()), (400, "invalid_expires_after".into()));
    let reply = env.handler.create_client_secret(&c, b"[1]");
    assert_eq!(json_of(&reply)["error"]["message"], "Invalid Realtime client secret request");
    let reply = env.handler.create_client_secret(&c, br#"{"session":[]}"#);
    assert_eq!(json_of(&reply)["error"]["code"], "invalid_session");
    let reply = env.handler.create_client_secret(&c, &vec![b' '; (64 << 10) + 1]);
    assert_eq!(reply.status, 413);
    // Empty body issues a default session.
    let reply = env.handler.create_client_secret(&c, b"");
    assert_eq!(reply.status, 200);
    assert_eq!(json_of(&reply)["session"]["model"], "gpt-realtime");
}

#[tokio::test]
async fn legacy_session_embeds_the_client_secret() {
    let env = env(vec![]).await;
    let reply = env.handler.create_legacy_session(&Caller::default(), br#"{"model":"gpt-realtime-mini","voice":"alloy"}"#);
    assert_eq!(reply.status, 200, "{}", reply.text());
    let response = json_of(&reply);
    assert_eq!(response["model"], "gpt-realtime-mini");
    assert_eq!(response["voice"], "alloy");
    assert_eq!(response["object"], "realtime.session");
    assert!(response["client_secret"]["value"].as_str().unwrap().starts_with("ek_"));
    assert!(response["client_secret"]["expires_at"].is_number());
}

// ---------------------------------------------------------------- hangup and stubs

#[tokio::test]
async fn hangup_forwards_pinned_call_and_ends_the_session() {
    let env = env(vec![oauth_auth("codex-oauth", "oauth-token", Some("acct"))]).await;
    let upstream = Arc::new(UpstreamState::default());
    *upstream.reply.lock() = Reply { status: 200, headers: vec![("content-type", "application/json")], body: r#"{"status":"ok"}"# };
    point_at(&env, &serve_upstream(upstream.clone()).await);
    env.handler.sessions().put(
        "call-123",
        LiveSession { auth_id: "codex-oauth".into(), model: "gpt-live-1-codex".into(), owner_principal: "owner-key".into(), owner_provider: "static".into(), ..Default::default() },
    );
    let p = parts_with_call("/v1/realtime/calls/call-123/hangup", "call-123", &[]);
    // Another principal is refused.
    let denied = env.handler.handle_hangup(&caller("other-key", "static"), &p, Bytes::new()).await;
    assert_eq!(denied.status, 403);
    assert_eq!(json_of(&denied)["error"]["code"], "realtime_call_scope_mismatch");
    assert!(env.handler.sessions().peek("call-123").is_some());

    let reply = env.handler.handle_hangup(&caller("owner-key", "static"), &p, Bytes::new()).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.text(), r#"{"status":"ok"}"#);
    let seen = upstream.last();
    assert_eq!((seen.method.as_str(), seen.path.as_str()), ("POST", "/v1/realtime/calls/call-123/hangup"));
    assert_eq!(seen.header("authorization"), "Bearer oauth-token");
    assert_eq!(seen.header("chatgpt-account-id"), "acct");
    assert!(env.handler.sessions().peek("call-123").is_none(), "successful hangup retained session");
}

#[tokio::test]
async fn hangup_validation() {
    let env = env(vec![]).await;
    let bad = env.handler.handle_hangup(&Caller::default(), &parts_with_call("/x", "bad id", &[]), Bytes::new()).await;
    assert_eq!(json_of(&bad)["error"]["code"], "invalid_call_id");
    let missing = env.handler.handle_hangup(&Caller::default(), &parts_with_call("/x", "call-x", &[]), Bytes::new()).await;
    assert_eq!((missing.status, json_of(&missing)["error"]["code"].as_str().unwrap().to_string()), (404, "realtime_call_not_found".into()));
}

#[tokio::test]
async fn unsupported_capabilities_use_the_standard_error() {
    let env = env(vec![]).await;
    for (reply, message) in [
        (env.handler.handle_transcription_session(), "Realtime transcription-only sessions are not supported by the ChatGPT/Codex OAuth upstream"),
        (env.handler.handle_translation(), "Realtime translation sessions are not supported by the ChatGPT/Codex OAuth upstream"),
        (env.handler.handle_sip_control("/v1/realtime/calls/call-123/accept"), "Realtime SIP accept are not supported by the ChatGPT/Codex OAuth upstream"),
        (env.handler.handle_sip_control("/v1/realtime/calls/call-123/refer"), "Realtime SIP refer are not supported by the ChatGPT/Codex OAuth upstream"),
    ] {
        assert_eq!(reply.status, 501);
        let error = &json_of(&reply)["error"];
        assert_eq!((error["type"].as_str(), error["code"].as_str(), error["message"].as_str()), (Some("not_supported_error"), Some("realtime_capability_not_supported"), Some(message)));
    }
}

// ---------------------------------------------------------------- sideband

async fn ws_connect(url: &str, headers: &[(&str, &str)]) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, tokio_tungstenite::tungstenite::Error> {
    let mut request = url.into_client_request().unwrap();
    for (k, v) in headers {
        request.headers_mut().insert(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
    }
    tokio_tungstenite::connect_async(request).await.map(|(s, _)| s)
}

async fn next_text(stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin)) -> String {
    match tokio::time::timeout(Duration::from_secs(5), stream.next()).await.expect("timed out").expect("closed").expect("error") {
        Message::Text(t) => t.to_string(),
        other => panic!("unexpected message {other:?}"),
    }
}

#[tokio::test]
async fn sideband_pins_auth_and_relays_bidirectionally() {
    let env = env(vec![oauth_auth("other-oauth", "other-token", Some("other-account")), oauth_auth("pinned-oauth", "pinned-token", Some("pinned-account"))]).await;
    let upstream = Arc::new(UpstreamState::default());
    point_at(&env, &serve_upstream(upstream.clone()).await);
    env.handler.sessions().put("call-sideband", LiveSession { auth_id: "pinned-oauth".into(), model: "gpt-live-1-codex".into(), ..Default::default() });
    let downstream = serve_downstream(env.handler.clone(), Caller::default()).await;

    let mut client = ws_connect(&format!("ws://{downstream}/v1/live/call-sideband"), &[("openai-alpha", "quicksilver=v2"), ("x-oai-attestation", "attestation-token")])
        .await
        .expect("dial downstream sideband");
    client.send(Message::text("ping")).await.unwrap();
    assert_eq!(next_text(&mut client).await, "echo:ping");
    let seen = upstream.last();
    assert_eq!(seen.path, "/v1/live/call-sideband");
    assert_eq!(seen.header("authorization"), "Bearer pinned-token");
    assert_eq!(seen.header("chatgpt-account-id"), "pinned-account");
    assert_eq!(seen.header("openai-alpha"), "quicksilver=v2");
    assert_eq!(seen.header("x-oai-attestation"), "attestation-token");

    // The call is consumed while the sideband is joined.
    let second = ws_connect(&format!("ws://{downstream}/v1/live/call-sideband"), &[]).await;
    assert!(second.is_err(), "a second sideband joined the same call");
    drop(client);
}

#[tokio::test]
async fn sideband_query_style_uses_the_realtime_endpoint() {
    let env = env(vec![oauth_auth("codex-oauth", "t", None)]).await;
    let upstream = Arc::new(UpstreamState::default());
    point_at(&env, &serve_upstream(upstream.clone()).await);
    env.handler.sessions().put("rtc_9", LiveSession { auth_id: "codex-oauth".into(), ..Default::default() });
    let downstream = serve_downstream(env.handler.clone(), Caller::default()).await;
    let mut client = ws_connect(&format!("ws://{downstream}/v1/realtime?call_id=rtc_9"), &[]).await.unwrap();
    client.send(Message::text("hello")).await.unwrap();
    assert_eq!(next_text(&mut client).await, "echo:hello");
    let seen = upstream.last();
    assert_eq!((seen.path.as_str(), seen.query.as_str()), ("/v1/realtime", "intent=quicksilver&call_id=rtc_9"));
}

#[tokio::test]
async fn sideband_rejections() {
    let env = env(vec![oauth_auth("codex-oauth", "t", None)]).await;
    let upgrade = [("connection", "Upgrade"), ("upgrade", "websocket")];
    // Not an upgrade.
    let response = env.handler.handle_sideband(&Caller::default(), &parts_with_call("/v1/live/x", "x", &[]), None).await;
    assert_eq!(response.status().as_u16(), 426);
    assert_eq!(response.headers().get("upgrade").unwrap(), "websocket");
    // Unknown call.
    let response = env.handler.handle_sideband(&Caller::default(), &parts_with_call("/v1/live/x", "x", &upgrade), None).await;
    assert_eq!(response.status().as_u16(), 404);
    // Client secret scope mismatch releases the claim.
    env.handler.sessions().put("call-123", LiveSession { auth_id: "codex-oauth".into(), client_secret_principal: "sess_expected".into(), ..Default::default() });
    let mut c = Caller::default();
    c.client_secret = Some(ClientSecretCaller { principal: "sess_other".into(), session: String::new() });
    let response = env.handler.handle_sideband(&c, &parts_with_call("/v1/realtime/calls/call-123", "call-123", &upgrade), None).await;
    assert_eq!(response.status().as_u16(), 403);
    let (_, claim) = env.handler.sessions().claim("call-123");
    assert_eq!(claim, cpa_live::Claim::Acquired);
    // Standard principal mismatch.
    env.handler.sessions().put("call-456", LiveSession { auth_id: "codex-oauth".into(), owner_principal: "owner-key".into(), owner_provider: "static".into(), ..Default::default() });
    let response = env.handler.handle_sideband(&caller("other-key", "static"), &parts_with_call("/v1/realtime/calls/call-456", "call-456", &upgrade), None).await;
    assert_eq!(response.status().as_u16(), 403);
}

#[tokio::test]
async fn sideband_dial_errors_forward_only_unauthorized_bodies() {
    let env = env(vec![oauth_auth("codex-oauth", "t", None)]).await;
    let upstream = Arc::new(UpstreamState::default());
    point_at(&env, &serve_upstream(upstream.clone()).await);
    let downstream = serve_downstream(env.handler.clone(), Caller::default()).await;
    let client = reqwest::Client::new();
    let get = |url: String| {
        client
            .get(url)
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .header("sec-websocket-version", "13")
            .send()
    };

    let unauthorized = r#"{"error":{"message":"access token expired"}}"#;
    *upstream.ws_reject.lock() = Some((401, "application/json", unauthorized));
    env.handler.sessions().put("call-a", LiveSession { auth_id: "codex-oauth".into(), ..Default::default() });
    let response = get(format!("http://{downstream}/v1/live/call-a")).await.unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(response.headers().get("content-type").unwrap(), "application/json");
    assert_eq!(response.text().await.unwrap(), unauthorized);

    let proxy_html = "<html>proxy-01.internal authentication failed</html>";
    *upstream.ws_reject.lock() = Some((502, "text/html", proxy_html));
    env.handler.sessions().put("call-b", LiveSession { auth_id: "codex-oauth".into(), ..Default::default() });
    let response = get(format!("http://{downstream}/v1/live/call-b")).await.unwrap();
    assert_eq!(response.status(), 502);
    let body = response.text().await.unwrap();
    assert!(!body.contains("proxy-01") && body.contains("Codex live sideband upstream unavailable"), "{body}");
}

// ---------------------------------------------------------------- direct websocket

#[tokio::test]
async fn direct_websocket_rejects_client_secret_model_mismatch() {
    let env = env(vec![]).await;
    let mut c = Caller::default();
    c.client_secret = Some(ClientSecretCaller { principal: "sess_123".into(), session: r#"{"type":"realtime","model":"gpt-live-1-codex"}"#.into() });
    let upgrade = [("connection", "Upgrade"), ("upgrade", "websocket")];
    let response = env.handler.handle_realtime_websocket(&c, &parts("/v1/realtime?model=another-live-model", &upgrade), None).await;
    assert_eq!(response.status().as_u16(), 403);
    // Without an upgrade the standard 426 is returned.
    let response = env.handler.handle_realtime_websocket(&Caller::default(), &parts("/v1/realtime", &[]), None).await;
    assert_eq!(response.status().as_u16(), 426);
}

#[tokio::test]
async fn direct_websocket_relays_standard_realtime_frames() {
    let env = env(vec![oauth_auth("codex-oauth", "oauth-token", Some("account-123"))]).await;
    let upstream = Arc::new(UpstreamState::default());
    upstream.ws_greeting.lock().push(r#"{"type":"session.created"}"#.into());
    point_at(&env, &serve_upstream(upstream.clone()).await);
    let downstream = serve_downstream(env.handler.clone(), Caller::default()).await;
    let mut client = ws_connect(&format!("ws://{downstream}/v1/realtime?model=gpt-realtime"), &[("openai-alpha", "quicksilver=v2")]).await.unwrap();
    assert_eq!(next_text(&mut client).await, r#"{"type":"session.created"}"#);
    let event = r#"{"type":"response.create"}"#;
    client.send(Message::text(event)).await.unwrap();
    assert_eq!(next_text(&mut client).await, format!("echo:{event}"));
    let seen = upstream.last();
    assert_eq!(seen.header("authorization"), "Bearer oauth-token");
    assert_eq!(seen.header("chatgpt-account-id"), "account-123");
    assert_eq!(seen.header("openai-alpha"), "", "OpenAI-Alpha must not be forwarded");
    assert_eq!(seen.header("originator"), "Codex Desktop");
    assert_eq!(seen.query, "model=gpt-realtime");
    assert_eq!(upstream.ws_received.lock().as_slice(), [event]);
}

#[tokio::test]
async fn direct_websocket_applies_client_secret_session() {
    let env = env(vec![oauth_auth("codex-oauth", "oauth-token", None)]).await;
    let upstream = Arc::new(UpstreamState::default());
    point_at(&env, &serve_upstream(upstream.clone()).await);
    let mut c = Caller::default();
    c.client_secret =
        Some(ClientSecretCaller { principal: "sess_123".into(), session: r#"{"type":"realtime","model":"gpt-live-1-codex","instructions":"help"}"#.into() });
    let downstream = serve_downstream(env.handler.clone(), c).await;
    let mut client = ws_connect(&format!("ws://{downstream}/v1/realtime?model=gpt-realtime"), &[]).await.unwrap();
    // The proxy's session.update reaches the upstream, which echoes it back through the relay.
    let echoed = next_text(&mut client).await;
    let echoed = echoed.strip_prefix("echo:").expect("echo of the session update");
    let update: Value = serde_json::from_str(echoed).unwrap();
    assert_eq!(update["type"], "session.update");
    assert_eq!(update["session"]["instructions"], "help");
    assert!(update["session"].get("model").is_none());
    assert_eq!(update, json!({"type": "session.update", "session": {"instructions": "help", "type": "realtime"}}));
}

#[tokio::test]
async fn direct_websocket_unsupported_upstream_reports_not_supported() {
    let env = env(vec![oauth_auth("codex-oauth", "oauth-token", None)]).await;
    let upstream = Arc::new(UpstreamState::default());
    *upstream.ws_reject.lock() = Some((404, "text/plain", "nope"));
    point_at(&env, &serve_upstream(upstream).await);
    let downstream = serve_downstream(env.handler.clone(), Caller::default()).await;
    let response = reqwest::Client::new()
        .get(format!("http://{downstream}/v1/realtime"))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("sec-websocket-version", "13")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 501);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "realtime_capability_not_supported");
}

// ---------------------------------------------------------------- config reload

#[tokio::test]
async fn media_relay_follows_config_changes() {
    let env = env(vec![]).await;
    assert!(env.handler.update_config().is_ok());
    let mut cfg = Config::default();
    cfg.codex.live_media_relay.enabled = true;
    cfg.codex.live_media_relay.max_sessions = 1;
    env.cfg_tx.send(Arc::new(cfg.clone())).unwrap();
    assert!(env.handler.update_config().is_ok());
    // An invalid relay config is reported on every call.
    cfg.codex.live_media_relay.public_ip = "not-an-ip".into();
    env.cfg_tx.send(Arc::new(cfg)).unwrap();
    assert!(env.handler.update_config().is_err());
    let reply = env.handler.handle_call(&Caller::default(), &parts("/v1/live", &[]), Bytes::new()).await;
    assert_eq!(reply.status, 503);
    assert!(json_of(&reply)["error"].as_str().unwrap().contains("public-ip is invalid"));
    // Disabling clears the error.
    env.cfg_tx.send(Arc::new(Config::default())).unwrap();
    assert!(env.handler.update_config().is_ok());
}
