//! Home-mode behaviour of the Antigravity executor: short cooldowns, the credits balance and the
//! refresh lock live in Home KV (Go: antigravity_executor_credits_test.go, the `homekv` cases).
//!
//! The Home client is process-global, so every test holds `SERIAL` and runs against its own
//! in-memory Home (a mock RESP server over `FakeKv`) that can inject failures per command. A
//! local axum server stands in for Google; credentials point at it through `base_url`.

// ExecError is the runtime contract's error type; its size is not this file's to change.
#![allow(clippy::result_large_err)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::http::{Request as HttpRequest, Response as HttpResponse, StatusCode};
use cpa_auth::Auth;
use cpa_config::{Config, HomeConfig};
use cpa_executors::antigravity::{AntigravityExecutor, auth_has_credits_required};
use cpa_home::kv::{clear_current, hash_key_part, set_current};
use cpa_home::testing::{FakeKv, MockHome, err};
use cpa_home::Client;
use cpa_runtime::conductor::ANTIGRAVITY_CREDITS_METADATA_KEY;
use cpa_runtime::executor::{ExecError, Executor, Options, Request};
use cpa_translator::Format;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard, watch};

static SERIAL: AsyncMutex<()> = AsyncMutex::const_new(());

/// A fake Home installed as the process-wide client for the length of one test.
struct Home {
    mock: MockHome,
    kv: FakeKv,
    _serial: MutexGuard<'static, ()>,
}

impl Drop for Home {
    fn drop(&mut self) {
        clear_current();
    }
}

fn is_cmd(args: &[String], name: &str) -> bool {
    args.first().is_some_and(|a| a.eq_ignore_ascii_case(name))
}

/// Installs a fake Home; commands for which `fail` returns true get an error reply.
async fn home_with(fail: impl Fn(&[String]) -> bool + Send + Sync + 'static) -> Home {
    let serial = SERIAL.lock().await;
    let kv = FakeKv::new();
    let handler_kv = kv.clone();
    let mock = MockHome::start(move |args| {
        if fail(args) { err("ERR injected failure") } else { handler_kv.handle(args) }
    })
    .await;
    let cfg = HomeConfig { enabled: true, host: "127.0.0.1".into(), port: i64::from(mock.port()), ..Default::default() };
    let client = Arc::new(Client::new(cfg));
    client.set_test_operation_timeout(Duration::from_secs(2));
    client.set_heartbeat_ok_for_tests(true);
    set_current(client);
    Home { mock, kv, _serial: serial }
}

async fn home() -> Home {
    home_with(|_| false).await
}

/// True for `cmd` commands whose key contains `part`.
fn on_key(args: &[String], cmd: &str, part: &str) -> bool {
    is_cmd(args, cmd) && args.get(1).is_some_and(|k| k.contains(part))
}

struct Fake {
    url: String,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn count(&self, path_part: &str) -> usize {
        self.seen.lock().iter().filter(|p| p.contains(path_part)).count()
    }
}

async fn fake_google(handler: impl Fn(&str) -> (u16, String) + Send + Sync + 'static) -> Fake {
    let handler = Arc::new(handler);
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = Router::new().fallback(move |req: HttpRequest<Body>| {
        let (handler, log) = (handler.clone(), log.clone());
        async move {
            let path = req.uri().path().to_string();
            let (status, text) = handler(&path);
            log.lock().push(path);
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

fn executor(cfg: Config) -> AntigravityExecutor {
    let (tx, rx) = watch::channel(Arc::new(cfg));
    std::mem::forget(tx);
    AntigravityExecutor::new(rx)
}

fn credits_config() -> Config {
    let mut cfg = Config::default();
    cfg.quota_exceeded.antigravity_credits = true;
    cfg
}

fn auth_for(id: &str, fake: &Fake) -> Auth {
    let mut auth = Auth::new(id, "antigravity");
    auth.attributes.insert("base_url".into(), fake.url.clone());
    auth.metadata.insert("access_token".into(), json!("access-old"));
    auth.metadata.insert("refresh_token".into(), json!("refresh-old"));
    auth.metadata.insert("project_id".into(), json!("project-1"));
    let expiry = chrono::Utc::now() + chrono::Duration::minutes(60);
    auth.metadata.insert("expired".into(), json!(expiry.to_rfc3339()));
    auth
}

const MODEL: &str = "claude-sonnet-4-6";

fn request(credits: bool) -> (Request, Options) {
    let payload = json!({"request": {"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}});
    let req = Request {
        model: MODEL.into(),
        payload: serde_json::to_vec(&payload).unwrap_or_default().into(),
        format: Format::Antigravity,
        metadata: Default::default(),
    };
    let mut opts = Options::new(Format::Antigravity);
    if credits {
        opts.metadata.insert(ANTIGRAVITY_CREDITS_METADATA_KEY.into(), json!(true));
    }
    (req, opts)
}

async fn run(ex: &AntigravityExecutor, auth: &Auth, credits: bool) -> Result<(), ExecError> {
    let (req, opts) = request(credits);
    ex.execute(auth, req, opts).await.map(|_| ())
}

const SSE_OK: &str = "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2,\"totalTokenCount\":5}}}\n\n";

fn short_rate_limit() -> String {
    json!({"error": {"status": "RESOURCE_EXHAUSTED", "details": [
        {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "RATE_LIMIT_EXCEEDED"},
        {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "30s"}]}})
    .to_string()
}

fn cooldown_key(auth_id: &str) -> String {
    format!("cpa:antigravity:short-cooldown:{auth_id}:{}", hash_key_part(MODEL))
}

// ---------------------------------------------------------------- short cooldown

#[tokio::test(flavor = "multi_thread")]
async fn short_cooldown_is_written_to_and_read_from_home_kv() {
    let home = home().await;
    let body = short_rate_limit();
    let fake = fake_google(move |_| (429, body.clone())).await;
    let ex = executor(Config::default());
    let auth = auth_for("home-cooldown-auth", &fake);

    let first = run(&ex, &auth, false).await.expect_err("429");
    assert_eq!(first.status, 429);
    assert_eq!(fake.count("enerateContent"), 1);

    let key = cooldown_key("home-cooldown-auth");
    let until: u128 = String::from_utf8_lossy(&home.kv.get(&key).expect("cooldown stored")).trim().parse().expect("unix nanos");
    let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
    let remaining = Duration::from_nanos(u64::try_from(until.saturating_sub(now)).unwrap_or(0));
    assert!(remaining > Duration::from_secs(25) && remaining <= Duration::from_secs(30), "{remaining:?}");
    // The key outlives the cooldown by five seconds.
    assert_eq!(home.kv.ttl(&key), Some(Duration::from_secs(35)));

    // A second request is short-circuited from the Home KV entry without reaching Google.
    let second = run(&ex, &auth, false).await.expect_err("cooldown");
    assert_eq!(second.status, 429);
    assert!(second.message.starts_with("auth in short cooldown, "), "{}", second.message);
    assert!(second.retry_after.is_some_and(|d| d > Duration::from_secs(20)));
    assert_eq!(fake.count("enerateContent"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_home_cooldown_is_deleted_and_the_request_proceeds() {
    let home = home().await;
    let fake = fake_google(|_| (200, SSE_OK.into())).await;
    let ex = executor(Config::default());
    let auth = auth_for("home-expired-auth", &fake);
    let key = cooldown_key("home-expired-auth");
    home.kv.put(&key, "1");

    run(&ex, &auth, false).await.expect("expired cooldown does not block");
    assert!(home.kv.get(&key).is_none(), "expired entry removed");
    assert_eq!(fake.count("enerateContent"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn home_kv_failures_on_cooldown_become_503() {
    let fake = fake_google(|_| (429, short_rate_limit())).await;

    // Read failure before sending.
    {
        let home = home_with(|a| on_key(a, "GET", "short-cooldown")).await;
        let auth = auth_for("home-read-fail", &fake);
        let err = run(&executor(Config::default()), &auth, false).await.expect_err("read fails");
        assert_eq!(err.status, 503);
        assert!(err.message.starts_with("home kv store unavailable"), "{}", err.message);
        assert_eq!(home.mock.count("get", None), 1);
    }
    assert_eq!(fake.count("enerateContent"), 0, "nothing sent when the cooldown read failed");

    // Write failure after the upstream 429 replaces the upstream error.
    {
        let _home = home_with(|a| on_key(a, "SET", "short-cooldown")).await;
        let auth = auth_for("home-write-fail", &fake);
        let err = run(&executor(Config::default()), &auth, false).await.expect_err("write fails");
        assert_eq!(err.status, 503);
        assert!(err.message.starts_with("home kv store unavailable"), "{}", err.message);
    }
    assert_eq!(fake.count("enerateContent"), 1);

    // Deleting an expired entry can fail too.
    {
        let home = home_with(|a| is_cmd(a, "DEL")).await;
        home.kv.put(&cooldown_key("home-del-fail"), "1");
        let auth = auth_for("home-del-fail", &fake);
        let err = run(&executor(Config::default()), &auth, false).await.expect_err("delete fails");
        assert_eq!(err.status, 503);
    }
}

// ---------------------------------------------------------------- credits balance and hint

#[tokio::test(flavor = "multi_thread")]
async fn explicit_balance_exhaustion_stores_balance_and_hint_in_home_kv() {
    let home = home().await;
    // The reason alone is not a quota signal; the "quota exhausted" text makes it a full exhaustion.
    let body = json!({"error": {"status": "RESOURCE_EXHAUSTED", "message": "Quota exhausted", "details": [
        {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "INSUFFICIENT_G1_CREDITS_BALANCE"}]}})
    .to_string();
    let fake = fake_google(move |_| (429, body.clone())).await;
    let ex = executor(credits_config());
    let auth = auth_for("home-exhausted-auth", &fake);

    let err = run(&ex, &auth, true).await.expect_err("429");
    assert_eq!(err.status, 429);

    let balance_key = "cpa:antigravity:credits-balance:home-exhausted-auth";
    let balance: Value = serde_json::from_slice(&home.kv.get(balance_key).expect("balance")).expect("json");
    assert_eq!(balance, json!({"CreditAmount": 0.0, "MinCreditAmount": 1.0, "PaidTierID": "", "Known": true}));
    assert_eq!(home.kv.ttl(balance_key), Some(Duration::from_secs(30 * 60)));

    let hint_key = "cpa:antigravity:credits-hint:home-exhausted-auth";
    let hint: Value = serde_json::from_slice(&home.kv.get(hint_key).expect("hint")).expect("json");
    assert_eq!((hint["Known"].clone(), hint["Available"].clone()), (json!(true), json!(false)));
    assert_eq!(home.kv.ttl(hint_key), Some(Duration::from_secs(30 * 60)));
}

#[tokio::test(flavor = "multi_thread")]
async fn credits_check_reads_the_home_balance_and_publishes_a_hint() {
    let home = home().await;
    let auth = Auth::new("home-balance-auth", "antigravity");
    let balance_key = "cpa:antigravity:credits-balance:home-balance-auth";

    // Optimistic when nothing is stored.
    assert!(auth_has_credits_required(&auth).await.expect("miss"));

    home.kv.put(balance_key, r#"{"CreditAmount":10,"MinCreditAmount":50,"PaidTierID":"t","Known":true}"#);
    assert!(!auth_has_credits_required(&auth).await.expect("low balance"));
    // The check publishes the derived hint, which then answers on its own.
    let hint: Value = serde_json::from_slice(&home.kv.get("cpa:antigravity:credits-hint:home-balance-auth").expect("hint")).expect("json");
    assert_eq!((hint["Known"].clone(), hint["Available"].clone()), (json!(true), json!(false)));
    home.kv.put(balance_key, r#"{"CreditAmount":99,"MinCreditAmount":50,"Known":true}"#);
    assert!(!auth_has_credits_required(&auth).await.expect("hint wins"));

    // Unknown balances and corrupt values.
    let other = Auth::new("home-unknown-auth", "antigravity");
    home.kv.put("cpa:antigravity:credits-balance:home-unknown-auth", r#"{"Known":false}"#);
    assert!(!auth_has_credits_required(&other).await.expect("unknown"));
    let bad = Auth::new("home-bad-auth", "antigravity");
    home.kv.put("cpa:antigravity:credits-balance:home-bad-auth", "not json");
    assert!(auth_has_credits_required(&bad).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn home_refresh_lock_throttles_the_warm_token_credits_probe() {
    let home = home().await;
    let fake = fake_google(|path| {
        if path.contains("loadCodeAssist") {
            (200, r#"{"paidTier":{"id":"tier-1","availableCredits":[{"creditType":"GOOGLE_ONE_AI","creditAmount":"25000","minimumCreditAmountForUsage":"50"}]}}"#.into())
        } else {
            (200, SSE_OK.into())
        }
    })
    .await;
    let ex = executor(credits_config());
    let auth = auth_for("home-refresh-auth", &fake);

    run(&ex, &auth, false).await.expect("execute");
    let lock_key = "cpa:antigravity:credits-refresh-lock:home-refresh-auth";
    assert_eq!(home.kv.get(lock_key).as_deref(), Some(&b"1"[..]));
    assert_eq!(home.kv.ttl(lock_key), Some(Duration::from_secs(10 * 60)));

    // The probe runs in the background and publishes its hint to Home KV.
    let hint_key = "cpa:antigravity:credits-hint:home-refresh-auth";
    for _ in 0..100 {
        if home.kv.get(hint_key).is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let hint: Value = serde_json::from_slice(&home.kv.get(hint_key).expect("probe published a hint")).expect("json");
    assert_eq!((hint["Known"].clone(), hint["Available"].clone(), hint["CreditAmount"].clone()), (json!(true), json!(true), json!(25000.0)));
    assert!(home.kv.get("cpa:antigravity:credits-balance:home-refresh-auth").is_some());
    assert_eq!(fake.count("loadCodeAssist"), 1);

    // With the hint gone the still-held lock blocks a second probe from this or any other node.
    home.kv.put(hint_key, "{}");
    run(&ex, &auth, false).await.expect("execute again");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fake.count("loadCodeAssist"), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_failure_skips_the_warm_token_probe() {
    let home = home_with(|a| is_cmd(a, "SET") && a.iter().any(|x| x == "NX")).await;
    let fake = fake_google(|path| if path.contains("loadCodeAssist") { (200, "{}".into()) } else { (200, SSE_OK.into()) }).await;
    let ex = executor(credits_config());
    let auth = auth_for("home-lock-fail-auth", &fake);

    run(&ex, &auth, false).await.expect("execute is not affected");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fake.count("loadCodeAssist"), 0);
    assert!(home.kv.get("cpa:antigravity:credits-refresh-lock:home-lock-fail-auth").is_none());
}
