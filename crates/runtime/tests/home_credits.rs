//! Home-mode behavior of the Antigravity credits hint store and the credits fallback candidate
//! lookup (Go: antigravity_credits.go, conductor_credits_candidates_test.go). The Home client is
//! process-wide, so every test serializes on `SERIAL` and clears it on exit.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::{Config, HomeConfig};
use cpa_core::registry::{ModelInfo, ModelRegistry};
use cpa_home::testing::install_fake_home;
use cpa_runtime::conductor::{
    ANTIGRAVITY_CREDITS_METADATA_KEY, AntigravityCreditsHint, Manager, SystemClock,
    antigravity_credits_hint, get_antigravity_credits_hint_required, set_antigravity_credits_hint,
    set_antigravity_credits_hint_async,
};
use cpa_runtime::executor::{ExecError, Executor, Options, Request, Response, StreamResult};
use cpa_translator::Format;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

static SERIAL: AsyncMutex<()> = AsyncMutex::const_new(());

/// Clears the process-wide Home client when a test ends, pass or fail.
struct ClearHome;

impl Drop for ClearHome {
    fn drop(&mut self) {
        cpa_home::kv::clear_current();
    }
}

fn hint_key(id: &str) -> String {
    format!("cpa:antigravity:credits-hint:{id}")
}

fn available(amount: f64) -> AntigravityCreditsHint {
    AntigravityCreditsHint {
        known: true,
        available: true,
        credit_amount: amount,
        min_credit_amount: 50.0,
        paid_tier_id: "tier-1".into(),
        updated_at: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn hint_is_stored_in_home_kv_with_go_json_and_30m_ttl() {
    let _serial = SERIAL.lock().await;
    let (_mock, kv, _client) = install_fake_home().await;
    let _clear = ClearHome;

    set_antigravity_credits_hint_async(" ag-home ", available(25000.0)).await;

    let raw = kv
        .get(&hint_key("ag-home"))
        .expect("hint stored under trimmed id");
    let stored: Value = serde_json::from_slice(&raw).expect("json");
    assert_eq!(stored["Known"], json!(true));
    assert_eq!(stored["Available"], json!(true));
    assert_eq!(stored["CreditAmount"], json!(25000.0));
    assert_eq!(stored["MinCreditAmount"], json!(50.0));
    assert_eq!(stored["PaidTierID"], json!("tier-1"));
    assert!(
        stored["UpdatedAt"]
            .as_str()
            .is_some_and(|t| t.ends_with('Z') && !t.starts_with("0001"))
    );
    assert_eq!(
        kv.ttl(&hint_key("ag-home")),
        Some(Duration::from_secs(30 * 60))
    );

    let read = get_antigravity_credits_hint_required("ag-home")
        .await
        .expect("read")
        .expect("hit");
    assert!(read.known && read.available);
    assert_eq!(read.credit_amount, 25000.0);
    assert!(read.updated_at.is_some());
    assert!(
        get_antigravity_credits_hint_required("ag-missing")
            .await
            .expect("read")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn hint_written_by_go_is_readable_and_corrupt_value_is_an_error() {
    let _serial = SERIAL.lock().await;
    let (_mock, kv, _client) = install_fake_home().await;
    let _clear = ClearHome;

    kv.put(
        &hint_key("ag-go"),
        r#"{"Known":true,"Available":false,"CreditAmount":3,"MinCreditAmount":50,"PaidTierID":"t","UpdatedAt":"2026-01-02T03:04:05.123456789+02:00"}"#,
    );
    let hint = get_antigravity_credits_hint_required("ag-go")
        .await
        .expect("read")
        .expect("hit");
    assert!(hint.known && !hint.available);
    assert_eq!(hint.paid_tier_id, "t");

    kv.put(&hint_key("ag-bad"), "not json");
    assert!(
        get_antigravity_credits_hint_required("ag-bad")
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_accessors_use_home_kv_from_a_worker_thread() {
    let _serial = SERIAL.lock().await;
    let (_mock, kv, _client) = install_fake_home().await;
    let _clear = ClearHome;

    // The sync bridge blocks in place, which needs a runtime worker rather than the test thread.
    tokio::spawn(async {
        set_antigravity_credits_hint("ag-sync", available(100.0));
        let hint = antigravity_credits_hint("ag-sync").expect("hint");
        assert!(hint.available);
    })
    .await
    .expect("task");
    assert!(kv.get(&hint_key("ag-sync")).is_some());
}

/// Antigravity stand-in: 429 until the credits flag is set, then success. Records call order.
struct CreditsExecutor {
    calls: Mutex<Vec<(String, bool)>>,
}

#[async_trait]
impl Executor for CreditsExecutor {
    fn identifier(&self) -> &str {
        "antigravity"
    }

    async fn execute(
        &self,
        auth: &Auth,
        _req: Request,
        opts: Options,
    ) -> Result<Response, ExecError> {
        let credits = opts.metadata.contains_key(ANTIGRAVITY_CREDITS_METADATA_KEY);
        self.calls.lock().push((auth.id.clone(), credits));
        if !credits {
            let mut err = ExecError::new(429, "quota exhausted");
            err.retry_after = Some(Duration::from_secs(3600));
            return Err(err);
        }
        Ok(Response {
            payload: Bytes::from_static(b"with-credits"),
            ..Default::default()
        })
    }

    async fn execute_stream(
        &self,
        _auth: &Auth,
        _req: Request,
        _opts: Options,
    ) -> Result<StreamResult, ExecError> {
        Err(ExecError::new(501, "stream not implemented"))
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        Ok(auth.clone())
    }

    async fn count_tokens(
        &self,
        _auth: &Auth,
        _req: Request,
        _opts: Options,
    ) -> Result<Response, ExecError> {
        Err(ExecError::new(501, "count not implemented"))
    }
}

async fn manager_with_auths(ids: &[&str], model: &str) -> (Manager, Arc<CreditsExecutor>) {
    let registry: &'static ModelRegistry = Box::leak(Box::new(ModelRegistry::new()));
    let mgr = Manager::with_parts(Arc::new(SystemClock), registry);
    let mut cfg = Config::default();
    cfg.quota_exceeded.antigravity_credits = true;
    mgr.set_config(Arc::new(cfg));
    let exec = Arc::new(CreditsExecutor {
        calls: Mutex::new(Vec::new()),
    });
    mgr.register_executor(exec.clone());
    for id in ids {
        registry.register_client(
            id,
            "antigravity",
            &[ModelInfo {
                id: model.into(),
                ..Default::default()
            }],
        );
        mgr.register(Auth::new(*id, "antigravity"))
            .await
            .expect("register");
    }
    (mgr, exec)
}

fn request(model: &str) -> Request {
    Request {
        model: model.into(),
        payload: Bytes::from_static(b"{}"),
        format: Format::OpenAI,
        metadata: Default::default(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fallback_candidates_read_hints_from_home_kv() {
    let _serial = SERIAL.lock().await;
    let (_mock, _kv, _client) = install_fake_home().await;
    let _clear = ClearHome;
    let model = "claude-sonnet-4-6";
    let (mgr, exec) = manager_with_auths(&["zz-credits", "aa-unknown", "mm-no"], model).await;

    set_antigravity_credits_hint_async("zz-credits", available(25000.0)).await;
    set_antigravity_credits_hint_async(
        "mm-no",
        AntigravityCreditsHint {
            known: true,
            available: false,
            ..Default::default()
        },
    )
    .await;

    let resp = mgr
        .execute(
            &["antigravity".to_string()],
            request(model),
            Options::new(Format::OpenAI),
        )
        .await
        .expect("credits fallback succeeds");
    assert_eq!(resp.payload, "with-credits");

    // All three were tried normally; the credits pass starts with the credential known to have
    // credits and skips the one known to be out.
    let calls = exec.calls.lock().clone();
    let credit_calls: Vec<&str> = calls
        .iter()
        .filter(|(_, credits)| *credits)
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(credit_calls, ["zz-credits"]);
    assert!(!credit_calls.contains(&"mm-no"));
}

#[tokio::test(flavor = "multi_thread")]
async fn fallback_fails_with_503_when_home_kv_is_unavailable() {
    let _serial = SERIAL.lock().await;
    // A disabled Home client reports Home mode with an unusable KV store.
    cpa_home::kv::set_current(Arc::new(cpa_home::Client::new(HomeConfig {
        enabled: false,
        ..Default::default()
    })));
    let _clear = ClearHome;
    let model = "claude-sonnet-4-6";
    let (mgr, exec) = manager_with_auths(&["ag-home-kv"], model).await;

    let err = mgr
        .execute(
            &["antigravity".to_string()],
            request(model),
            Options::new(Format::OpenAI),
        )
        .await
        .expect_err("home kv unavailable");
    assert_eq!(err.status, 503);
    assert!(
        err.message.contains("home kv store unavailable"),
        "{}",
        err.message
    );
    assert_eq!(err.auth_code.as_deref(), Some("home_kv_unavailable"));
    assert!(
        exec.calls.lock().iter().all(|(_, credits)| !credits),
        "no credits dispatch"
    );
}
