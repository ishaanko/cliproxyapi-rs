//! Rust-side executor tests for flows the Go tests pin with httptest servers: token refresh and
//! project discovery, the credits probe and failure handling, cooldown opt-outs, stream
//! failures, usage reporting and pool/scope bookkeeping. A local axum server stands in for
//! Google; the `token`, `api` and `daily_api` endpoints of the executor point at it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request as HttpRequest, Response as HttpResponse, StatusCode};
use cpa_auth::Auth;
use cpa_auth::antigravity::AntigravityEndpoints;
use cpa_config::Config;
use cpa_json::J;
use cpa_runtime::conductor::{ANTIGRAVITY_CREDITS_METADATA_KEY, antigravity_credits_hint};
use cpa_runtime::executor::{ExecError, Executor, Options, Request};
use cpa_translator::Format;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::watch;

use super::AntigravityExecutor;
use super::tests::SERIAL;

/// One request seen by the fake Google.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

type Handler = Arc<dyn Fn(&Seen) -> (u16, String) + Send + Sync>;

struct Fake {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    fn count(&self, path_part: &str) -> usize {
        self.seen.lock().iter().filter(|s| s.path.contains(path_part)).count()
    }
    fn last(&self, path_part: &str) -> Option<Seen> {
        self.seen.lock().iter().rev().find(|s| s.path.contains(path_part)).cloned()
    }
}

async fn fake_google(handler: impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static) -> Fake {
    let handler: Handler = Arc::new(handler);
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = Router::new().fallback(move |req: HttpRequest<Body>| {
        let (handler, log) = (handler.clone(), log.clone());
        async move {
            let (parts, body) = req.into_parts();
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
            let seen = Seen { path: parts.uri.path().to_string(), headers: parts.headers, body: bytes.to_vec() };
            let (status, text) = handler(&seen);
            log.lock().push(seen);
            let mut resp = HttpResponse::new(Body::from(text));
            *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            resp
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Fake { url: format!("http://{addr}"), seen }
}

fn executor(fake: &Fake, cfg: Config) -> AntigravityExecutor {
    let (tx, rx) = watch::channel(Arc::new(cfg));
    std::mem::forget(tx);
    let mut ex = AntigravityExecutor::new(rx);
    ex.endpoints = AntigravityEndpoints {
        token: format!("{}/token", fake.url),
        user_info: format!("{}/userinfo", fake.url),
        api: fake.url.clone(),
        daily_api: fake.url.clone(),
    };
    ex.client_secret = Some("test-secret".into());
    ex
}

fn auth_with(id: &str, base_url: Option<&str>, expires_in_minutes: i64) -> Auth {
    let mut auth = Auth::new(id, "antigravity");
    if let Some(url) = base_url {
        auth.attributes.insert("base_url".into(), url.into());
    }
    auth.metadata.insert("access_token".into(), json!("access-old"));
    auth.metadata.insert("refresh_token".into(), json!("refresh-old"));
    auth.metadata.insert("project_id".into(), json!("project-1"));
    let expiry = chrono::Utc::now() + chrono::Duration::minutes(expires_in_minutes);
    auth.metadata.insert("expired".into(), json!(expiry.to_rfc3339()));
    auth
}

fn token_ok() -> String {
    json!({"access_token": "access-new", "refresh_token": "refresh-new", "token_type": "Bearer", "expires_in": 3600}).to_string()
}

fn request(model: &str, payload: Value, format: Format) -> (Request, Options) {
    let req = Request {
        model: model.into(),
        payload: serde_json::to_vec(&payload).unwrap_or_default().into(),
        format,
        metadata: Default::default(),
    };
    (req, Options::new(format))
}

const SSE_OK: &str = "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2,\"totalTokenCount\":5}}}\n\n";
const OPENAI_HELLO: &str = r#"{"messages":[{"role":"user","content":"hello"}]}"#;

// ---------------------------------------------------------------- token refresh

#[tokio::test]
async fn token_inside_five_minute_window_is_refreshed_outside_is_reused() {
    let fake = fake_google(|s| if s.path == "/token" { (200, token_ok()) } else { (200, "{}".into()) }).await;
    let ex = executor(&fake, Config::default());
    let cfg = ex.cfg();

    let warm = auth_with("auth-warm", None, 6);
    let (token, updated) = ex.ensure_access_token(&cfg, &warm).await.expect("warm token");
    assert_eq!((token.as_str(), updated.is_none()), ("access-old", true));
    assert_eq!(fake.count("/token"), 0);

    let expiring = auth_with("auth-expiring", None, 4);
    let (token, updated) = ex.ensure_access_token(&cfg, &expiring).await.expect("refresh");
    assert_eq!(token, "access-new");
    let updated = updated.expect("updated auth");
    assert_eq!(updated.meta_str("refresh_token"), "refresh-new");
    assert_eq!(updated.meta_str("type"), "antigravity");
    assert_eq!(updated.metadata["expires_in"], 3600);
    assert!(updated.metadata.contains_key("timestamp") && updated.metadata.contains_key("expired"));

    // The refresh request carries the OAuth form and the Go default User-Agent.
    let seen = fake.last("/token").expect("token request");
    let form = String::from_utf8_lossy(&seen.body).into_owned();
    assert!(form.contains("grant_type=refresh_token") && form.contains("refresh_token=refresh-old"), "{form}");
    assert!(form.contains("client_secret=test-secret"), "{form}");
    assert_eq!(seen.headers.get("user-agent").and_then(|v| v.to_str().ok()), Some("Go-http-client/2.0"));
}

#[tokio::test]
async fn missing_refresh_token_is_401_and_upstream_429_keeps_retry_hint() {
    let body = json!({"error": {"details": [{"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "7s"}]}}).to_string();
    let fake = fake_google(move |_| (429, body.clone())).await;
    let ex = executor(&fake, Config::default());
    let cfg = ex.cfg();

    let mut no_refresh = auth_with("auth-no-rt", None, 1);
    no_refresh.metadata.remove("refresh_token");
    let err = ex.ensure_access_token(&cfg, &no_refresh).await.expect_err("no refresh token");
    assert_eq!((err.status, err.message.as_str()), (401, "missing refresh token"));

    let err = ex.ensure_access_token(&cfg, &auth_with("auth-429", None, 1)).await.expect_err("429");
    assert_eq!(err.status, 429);
    assert_eq!(err.retry_after, Some(Duration::from_secs(7)));
}

#[tokio::test]
async fn concurrent_refreshes_of_one_refresh_token_share_a_single_exchange() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let fake = fake_google(move |s| {
        if s.path == "/token" {
            counter.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(300));
            (200, token_ok())
        } else {
            (200, r#"{"paidTier":{"id":"t","availableCredits":[]}}"#.into())
        }
    })
    .await;
    let ex = executor(&fake, Config::default());
    let cfg = ex.cfg();
    let mut a = auth_with("auth-dedup-a", None, 1);
    let mut b = auth_with("auth-dedup-b", None, 1);
    for auth in [&mut a, &mut b] {
        auth.metadata.insert("refresh_token".into(), json!("shared-refresh"));
    }
    let (ra, rb) = tokio::join!(ex.ensure_access_token(&cfg, &a), ex.ensure_access_token(&cfg, &b));
    assert_eq!(ra.expect("a").0, "access-new");
    assert_eq!(rb.expect("b").0, "access-new");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one token exchange for both callers");
}

// ---------------------------------------------------------------- project discovery

#[tokio::test]
async fn prepare_request_auth_discovers_a_missing_project_id() {
    let fake = fake_google(|s| {
        if s.path.ends_with(":loadCodeAssist") {
            (200, r#"{"cloudaicompanionProject":"proj-discovered"}"#.into())
        } else {
            (200, token_ok())
        }
    })
    .await;
    let ex = executor(&fake, Config::default());
    let mut auth = auth_with("auth-prep", None, 60);
    auth.metadata.remove("project_id");
    assert!(ex.should_prepare_request_auth(&auth));
    let updated = ex.prepare_request_auth(&auth).await.expect("prepared").expect("updated auth");
    assert_eq!(updated.meta_str("project_id"), "proj-discovered");
    assert!(!ex.should_prepare_request_auth(&updated));
    let seen = fake.last(":loadCodeAssist").expect("loadCodeAssist call");
    assert_eq!(String::from_utf8_lossy(&seen.body), r#"{"metadata":{"ideType":"ANTIGRAVITY"}}"#);
    assert_eq!(seen.headers.get("authorization").and_then(|v| v.to_str().ok()), Some("Bearer access-old"));
}

#[tokio::test]
async fn project_discovery_failure_keeps_the_upstream_status() {
    let fake = fake_google(|_| (403, r#"{"error":"forbidden"}"#.into())).await;
    let ex = executor(&fake, Config::default());
    let mut auth = auth_with("auth-403", None, 60);
    auth.metadata.remove("project_id");
    let err = ex.prepare_request_auth(&auth).await.expect_err("403");
    assert_eq!(err.status, 403);
    assert!(err.message.starts_with("antigravity auth missing project_id: "), "{}", err.message);

    // Without discovery data the request itself fails with 400.
    let (req, opts) = request("gemini-3.7-flash", serde_json::from_str(OPENAI_HELLO).unwrap(), Format::OpenAI);
    let err = ex.execute(&auth, req, opts).await.expect_err("missing project");
    assert_eq!((err.status, err.upstream_attempted), (400, false));
    assert_eq!(err.message, "antigravity auth missing project_id");
}

// ---------------------------------------------------------------- credits

#[tokio::test]
async fn credits_probe_reads_balance_with_the_short_user_agent() {
    let fake = fake_google(|_| {
        (200, r#"{"paidTier":{"id":"tier-1","availableCredits":[{"creditType":"GOOGLE_ONE_AI","creditAmount":"25000","minimumCreditAmountForUsage":"50"}]}}"#.into())
    })
    .await;
    let ex = executor(&fake, Config::default());
    let mut auth = auth_with("auth-probe-ua", Some(&fake.url), 60);
    auth.attributes.insert("user_agent".into(), "antigravity/hub/1.23.2 windows/amd64 google-api-nodejs-client/10.3.0".into());
    ex.update_credits_balance(&ex.cfg(), &auth, "token", None).await;
    let seen = fake.last("/v1internal:loadCodeAssist").expect("probe");
    assert_eq!(seen.headers.get("user-agent").and_then(|v| v.to_str().ok()), Some("antigravity/hub/1.23.2 windows/amd64"));
    assert!(seen.headers.get("x-goog-api-client").is_none());
    assert_eq!(String::from_utf8_lossy(&seen.body), r#"{"metadata":{"ideType":"ANTIGRAVITY"}}"#);
    let hint = antigravity_credits_hint("auth-probe-ua").expect("hint");
    assert!(hint.known && hint.available);
    assert_eq!((hint.credit_amount, hint.min_credit_amount, hint.paid_tier_id.as_str()), (25000.0, 50.0, "tier-1"));
}

#[tokio::test]
async fn credits_probe_marks_low_or_missing_balances_unavailable() {
    let bodies = [
        ("auth-low", r#"{"paidTier":{"id":"t","availableCredits":[{"creditType":"GOOGLE_ONE_AI","creditAmount":"10","minimumCreditAmountForUsage":"50"}]}}"#),
        ("auth-none", r#"{"paidTier":{"id":"t"}}"#),
    ];
    for (id, body) in bodies {
        let fake = fake_google(move |_| (200, body.to_string())).await;
        let ex = executor(&fake, Config::default());
        ex.update_credits_balance(&ex.cfg(), &auth_with(id, Some(&fake.url), 60), "token", None).await;
        let hint = antigravity_credits_hint(id).expect("hint");
        assert!(hint.known && !hint.available, "{id}: {hint:?}");
    }
}

#[tokio::test]
async fn conductor_credits_flag_injects_enabled_credit_types_only_when_configured() {
    let _guard = SERIAL.lock().await;
    let fake = fake_google(|s| {
        if s.path.contains("generateContent") || s.path.contains("streamGenerateContent") {
            (200, SSE_OK.into())
        } else {
            (200, r#"{"paidTier":{"id":"t","availableCredits":[{"creditType":"GOOGLE_ONE_AI","creditAmount":"1000","minimumCreditAmountForUsage":"50"}]}}"#.into())
        }
    })
    .await;
    for (enabled, flagged, want) in [(true, true, true), (true, false, false), (false, true, false)] {
        let mut cfg = Config::default();
        cfg.quota_exceeded.antigravity_credits = enabled;
        let ex = executor(&fake, cfg);
        let auth = auth_with(&format!("auth-credits-{enabled}-{flagged}"), Some(&fake.url), 60);
        let (req, mut opts) = request("claude-sonnet-4-6", json!({"request": {"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}}), Format::Antigravity);
        if flagged {
            opts.metadata.insert(ANTIGRAVITY_CREDITS_METADATA_KEY.into(), json!(true));
        }
        ex.execute(&auth, req, opts).await.expect("execute");
        let seen = fake.last("enerateContent").expect("generate call");
        let body: Value = serde_json::from_slice(&seen.body).expect("json body");
        assert_eq!(body.g("enabledCreditTypes").exists(), want, "enabled={enabled} flagged={flagged}: {body}");
        if want {
            assert_eq!(body["enabledCreditTypes"], json!(["GOOGLE_ONE_AI"]));
        }
    }
}

#[tokio::test]
async fn explicit_credit_balance_exhaustion_disables_credits_for_the_auth() {
    let _guard = SERIAL.lock().await;
    // The reason alone is not a quota signal; the "quota exhausted" text makes it a full exhaustion.
    let body = json!({"error": {"status": "RESOURCE_EXHAUSTED", "message": "Quota exhausted", "details": [
        {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "INSUFFICIENT_G1_CREDITS_BALANCE"}]}})
    .to_string();
    let fake = fake_google(move |s| if s.path.contains("loadCodeAssist") { (200, "{}".into()) } else { (429, body.clone()) }).await;
    let mut cfg = Config::default();
    cfg.quota_exceeded.antigravity_credits = true;
    let ex = executor(&fake, cfg);
    let auth = auth_with("auth-credits-exhausted", Some(&fake.url), 60);
    let (req, mut opts) = request("claude-sonnet-4-6", json!({"request": {"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}}), Format::Antigravity);
    opts.metadata.insert(ANTIGRAVITY_CREDITS_METADATA_KEY.into(), json!(true));
    let err = ex.execute(&auth, req, opts).await.expect_err("429");
    assert_eq!(err.status, 429);
    let hint = antigravity_credits_hint("auth-credits-exhausted").expect("hint");
    assert!(hint.known && !hint.available);
}

// ---------------------------------------------------------------- cooldowns

#[tokio::test]
async fn disabled_cooling_never_short_circuits_on_a_short_rate_limit() {
    let _guard = SERIAL.lock().await;
    let short = json!({"error": {"status": "RESOURCE_EXHAUSTED", "details": [
        {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "RATE_LIMIT_EXCEEDED"},
        {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "30s"}]}})
    .to_string();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let fake = fake_google(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        (429, short.clone())
    })
    .await;

    // Global flag and per-auth override both skip recording and the precheck.
    for (id, global, override_) in [("auth-cool-global", true, None), ("auth-cool-override", false, Some(true))] {
        let mut cfg = Config::default();
        cfg.disable_cooling = global;
        let ex = executor(&fake, cfg);
        let mut auth = auth_with(id, Some(&fake.url), 60);
        if let Some(o) = override_ {
            auth.metadata.insert("disable_cooling".into(), json!(o));
        }
        let before = hits.load(Ordering::SeqCst);
        for _ in 0..2 {
            let (req, opts) = request("gemini-2.5-flash", serde_json::from_str(OPENAI_HELLO).unwrap(), Format::OpenAI);
            let err = ex.execute(&auth, req, opts).await.expect_err("429");
            assert_eq!(err.status, 429);
        }
        assert_eq!(hits.load(Ordering::SeqCst) - before, 2, "{id}: both calls reach upstream");
    }

    // With cooling enabled the second call is answered locally.
    let ex = executor(&fake, Config::default());
    let auth = auth_with("auth-cool-on", Some(&fake.url), 60);
    let before = hits.load(Ordering::SeqCst);
    for _ in 0..2 {
        let (req, opts) = request("gemini-2.5-flash", serde_json::from_str(OPENAI_HELLO).unwrap(), Format::OpenAI);
        let err = ex.execute(&auth, req, opts).await.expect_err("429");
        assert_eq!(err.status, 429);
    }
    assert_eq!(hits.load(Ordering::SeqCst) - before, 1);
}

// ---------------------------------------------------------------- streams and usage

#[tokio::test]
async fn read_error_mid_stream_ends_with_an_error_and_no_terminal_event() {
    // Upstream sends one chunk and then breaks the connection.
    let app = Router::new().fallback(|| async {
        use futures_util::StreamExt;
        let first: Result<Bytes, std::io::Error> =
            Ok(Bytes::from_static(b"data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"par\"}]}}]}}\n\n"));
        let broken = futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Err::<Bytes, std::io::Error>(std::io::Error::other("connection reset"))
        });
        HttpResponse::new(Body::from_stream(futures_util::stream::iter([first]).chain(broken)))
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let ex = AntigravityExecutor::new(watch::channel(Arc::new(Config::default())).1);
    let auth = auth_with("auth-read-error", Some(&url), 60);
    for format in [Format::Claude, Format::OpenAI] {
        let payload = if format == Format::Claude {
            json!({"model": "m", "max_tokens": 10, "messages": [{"role": "user", "content": "hi"}]})
        } else {
            serde_json::from_str(OPENAI_HELLO).unwrap()
        };
        let (req, mut opts) = request("gemini-3.7-flash", payload, format);
        opts.stream = true;
        let mut stream = ex.execute_stream(&auth, req, opts).await.expect("stream starts");
        let mut texts = Vec::new();
        let mut error: Option<ExecError> = None;
        while let Some(item) = stream.chunks.recv().await {
            match item {
                Ok(b) => texts.push(String::from_utf8_lossy(&b).into_owned()),
                Err(e) => error = Some(e),
            }
        }
        assert!(error.is_some(), "{format:?}: read failure must surface");
        let all = texts.join("");
        assert!(!all.contains("message_stop") && !all.contains("[DONE]") && !all.contains("\"finish_reason\":\"stop\""), "{format:?}: {all}");
    }
}

#[tokio::test]
async fn split_terminal_usage_is_reported_once_with_the_final_counts() {
    let _guard = SERIAL.lock().await;
    let sse = concat!(
        "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"first\"}]},\"finishReason\":\"STOP\"}],\"modelVersion\":\"gemini-3.7-flash\",\"responseId\":\"resp-split\"},\"traceId\":\"trace-split\"}\n\n",
        "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"\"}]}}],\"usageMetadata\":{\"promptTokenCount\":11,\"candidatesTokenCount\":22,\"totalTokenCount\":33},\"modelVersion\":\"gemini-3.7-flash\",\"responseId\":\"resp-split\"},\"traceId\":\"trace-split\"}\n\n",
    );
    let fake = fake_google(move |_| (200, sse.to_string())).await;
    let ex = executor(&fake, Config::default());
    let auth = auth_with("auth-split-usage", Some(&fake.url), 60);
    let (req, mut opts) = request("gemini-3.7-flash", serde_json::from_str(OPENAI_HELLO).unwrap(), Format::OpenAI);
    opts.stream = true;
    let mut stream = ex.execute_stream(&auth, req, opts).await.expect("stream");
    let usage_rx = stream.usage.take().expect("usage channel");
    let mut chunks = Vec::new();
    while let Some(item) = stream.chunks.recv().await {
        chunks.push(String::from_utf8_lossy(&item.expect("chunk")).into_owned());
    }
    let finishes: Vec<&String> = chunks.iter().filter(|c| cpa_json::parse(c.as_bytes()).g("choices.0.finish_reason").str() == "stop").collect();
    assert_eq!(finishes.len(), 1, "exactly one terminal chunk: {chunks:?}");
    assert!(std::ptr::eq(finishes[0], chunks.last().expect("chunks")), "terminal chunk is the last");
    let terminal = cpa_json::parse(finishes[0].as_bytes());
    assert_eq!((terminal.g("usage.total_tokens").int(), terminal.g("usage.prompt_tokens").int()), (33, 11));
    let usage = usage_rx.await.expect("usage");
    assert_eq!((usage["input_tokens"].as_i64(), usage["output_tokens"].as_i64(), usage["total_tokens"].as_i64()), (Some(11), Some(22), Some(33)));
}

#[tokio::test]
async fn non_stream_response_carries_usage_metadata() {
    let fake = fake_google(|_| (200, json!({"response": {"candidates": [{"content": {"role": "model", "parts": [{"text": "hi"}]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 3, "totalTokenCount": 10, "thoughtsTokenCount": 2}}}).to_string())).await;
    let ex = executor(&fake, Config::default());
    let auth = auth_with("auth-usage-meta", Some(&fake.url), 60);
    let (req, opts) = request("gemini-2.5-flash", serde_json::from_str(OPENAI_HELLO).unwrap(), Format::OpenAI);
    let resp = ex.execute(&auth, req, opts).await.expect("execute");
    let usage = &resp.metadata["usage"];
    assert_eq!((usage["input_tokens"].as_i64(), usage["total_tokens"].as_i64(), usage["reasoning_tokens"].as_i64()), (Some(7), Some(10), Some(2)));
}

// ---------------------------------------------------------------- pools and scopes

#[test]
fn pool_settings_follow_the_connection_pool_config() {
    use super::transport::resolve_pool_settings;
    let cfg_with = |enabled: Option<bool>, idle: &str, max_idle: Option<i64>| {
        let mut cfg = Config::default();
        let pool = &mut cfg.antigravity.connection_pool;
        pool.enabled = enabled;
        pool.idle_conn_timeout = idle.into();
        pool.max_idle_conns_per_host = max_idle;
        cfg
    };
    // Pooling is off unless explicitly enabled.
    for cfg in [cfg_with(None, "", None), cfg_with(Some(false), "5s", Some(10))] {
        let s = resolve_pool_settings(&cfg);
        assert!(s.short_mode && s.max_idle_conns_per_host == -1);
    }
    let s = resolve_pool_settings(&cfg_with(Some(true), "", None));
    assert_eq!((s.short_mode, s.idle_conn_timeout, s.max_idle_conns_per_host), (false, Duration::from_secs(30), 2));
    // The idle timeout is capped below the 240 s frontend cutoff; limits are clamped.
    let s = resolve_pool_settings(&cfg_with(Some(true), "1h", Some(500)));
    assert_eq!((s.idle_conn_timeout, s.max_idle_conns_per_host), (Duration::from_secs(210), 100));
    // Invalid durations fall back to the default; non-positive or negative values turn pooling off.
    assert_eq!(resolve_pool_settings(&cfg_with(Some(true), "bogus", None)).idle_conn_timeout, Duration::from_secs(30));
    assert!(resolve_pool_settings(&cfg_with(Some(true), "0s", None)).short_mode);
    assert!(resolve_pool_settings(&cfg_with(Some(true), "", Some(-1))).short_mode);
    assert_eq!(resolve_pool_settings(&cfg_with(Some(true), "45s", Some(7))).idle_conn_timeout, Duration::from_secs(45));
}

#[test]
fn transport_scope_prefers_stable_markers_and_never_leaks_tokens() {
    use super::transport::transport_scope;
    let mut a = Auth::new("  id-1  ", "antigravity");
    assert_eq!(transport_scope(&a), "id:id-1");
    a.id.clear();
    a.attributes.insert("path".into(), "/auths/x.json".into());
    assert_eq!(transport_scope(&a), "path:/auths/x.json");
    a.attributes.clear();
    a.attributes.insert("source".into(), "file".into());
    assert_eq!(transport_scope(&a), "source:file");
    a.attributes.clear();
    a.label = "shared-label".into();
    a.metadata.insert("access_token".into(), json!("access-secret-1"));
    a.metadata.insert("refresh_token".into(), json!("refresh-secret"));
    let scope = transport_scope(&a);
    assert!(scope.starts_with("refresh:") && !scope.contains("secret"), "{scope}");
    // Token rotation keeps the pool: the refresh token is the identity.
    a.metadata.insert("access_token".into(), json!("access-secret-2"));
    assert_eq!(transport_scope(&a), scope);
    a.metadata.remove("refresh_token");
    assert!(transport_scope(&a).starts_with("token:"));
    a.metadata.clear();
    assert_eq!(transport_scope(&a), "anonymous");
}

#[test]
fn tls_profile_advertises_no_alpn() {
    let config = super::transport::tls_config_for_test().expect("tls config");
    assert!(config.alpn_protocols.is_empty());
}
