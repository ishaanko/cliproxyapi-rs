//! Behavioral tests for the conductor: a scripted mock executor, a controllable clock and a
//! private model registry per test. Expected behavior follows the Go tests
//! (conductor_retry_round_test, selector_test, conductor_unauthorized_refresh_test, ...).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use cpa_auth::Auth;
use cpa_config::{Config, OAuthModelAlias};
use cpa_core::registry::{ModelInfo, ModelRegistry};
use cpa_translator::Format;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::cooldown::is_auth_blocked_for_model;
use super::*;
use crate::executor::{ErrorCode, Executor, meta};

// Scripted outcomes of a test executor; `ExecError` is large by design.
#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
enum Step {
    Ok(&'static str),
    Err(ExecError),
    /// Stream chunks; an `Err` item is delivered as an error chunk.
    Stream(Vec<Result<&'static str, ExecError>>),
    /// One chunk, then the upstream stays open and idle (sender kept in `held`).
    Idle(&'static str),
}

/// Scripted executor: per-credential queues of steps; credentials without a script succeed with
/// their own id as payload.
struct Mock {
    id: String,
    steps: Mutex<HashMap<String, VecDeque<Step>>>,
    calls: Mutex<Vec<(String, String)>>,
    refreshes: Mutex<Vec<String>>,
    /// Whether each execute call carried the Antigravity credits flag.
    credit_flags: Mutex<Vec<bool>>,
    /// Senders of idle streams, kept alive so tests can watch them close.
    held: Mutex<Vec<mpsc::Sender<Result<Bytes, ExecError>>>>,
}

impl Mock {
    fn new(id: &str) -> Arc<Self> {
        Arc::new(Mock {
            id: id.into(),
            steps: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
            refreshes: Mutex::new(Vec::new()),
            credit_flags: Mutex::new(Vec::new()),
            held: Mutex::new(Vec::new()),
        })
    }

    fn script(&self, auth_id: &str, steps: Vec<Step>) {
        self.steps.lock().insert(auth_id.into(), steps.into());
    }

    fn next(&self, auth: &Auth, model: &str) -> Step {
        self.calls.lock().push((auth.id.clone(), model.to_string()));
        let mut steps = self.steps.lock();
        match steps.get_mut(&auth.id).and_then(VecDeque::pop_front) {
            Some(s) => s,
            None => Step::Ok(""),
        }
    }

    fn call_ids(&self) -> Vec<String> {
        self.calls.lock().iter().map(|(a, _)| a.clone()).collect()
    }

    fn count(&self, id: &str) -> usize {
        self.calls.lock().iter().filter(|(a, _)| a == id).count()
    }
}

fn payload_for(step: &'static str, auth: &Auth) -> Bytes {
    if step.is_empty() {
        Bytes::from(auth.id.clone())
    } else {
        Bytes::from(step)
    }
}

#[async_trait]
impl Executor for Mock {
    fn identifier(&self) -> &str {
        &self.id
    }

    async fn execute(
        &self,
        auth: &Auth,
        req: Request,
        opts: Options,
    ) -> Result<Response, ExecError> {
        self.credit_flags
            .lock()
            .push(opts.metadata.contains_key(ANTIGRAVITY_CREDITS_METADATA_KEY));
        match self.next(auth, &req.model) {
            Step::Ok(p) => Ok(Response {
                payload: payload_for(p, auth),
                ..Default::default()
            }),
            Step::Err(e) => Err(e),
            Step::Stream(_) | Step::Idle(_) => panic!("stream step used for execute"),
        }
    }

    async fn execute_stream(
        &self,
        auth: &Auth,
        req: Request,
        _opts: Options,
    ) -> Result<StreamResult, ExecError> {
        match self.next(auth, &req.model) {
            Step::Err(e) => Err(e),
            Step::Ok(p) => {
                let (tx, rx) = mpsc::channel(8);
                tx.try_send(Ok(payload_for(p, auth))).unwrap();
                Ok(StreamResult::new(Default::default(), rx))
            }
            Step::Idle(first) => {
                let (tx, rx) = mpsc::channel(4);
                tx.try_send(Ok(Bytes::from_static(first.as_bytes())))
                    .unwrap();
                self.held.lock().push(tx);
                Ok(StreamResult::new(Default::default(), rx))
            }
            Step::Stream(items) => {
                let (tx, rx) = mpsc::channel(items.len().max(1) + 1);
                for item in items {
                    tx.try_send(item.map(|s| Bytes::from_static(s.as_bytes())))
                        .unwrap();
                }
                Ok(StreamResult::new(Default::default(), rx))
            }
        }
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        self.refreshes.lock().push(auth.id.clone());
        let mut updated = auth.clone();
        updated
            .metadata
            .insert("access_token".into(), serde_json::json!("fresh-token"));
        Ok(updated)
    }

    async fn count_tokens(
        &self,
        auth: &Auth,
        req: Request,
        _opts: Options,
    ) -> Result<Response, ExecError> {
        match self.next(auth, &req.model) {
            Step::Ok(p) => Ok(Response {
                payload: payload_for(p, auth),
                ..Default::default()
            }),
            Step::Err(e) => Err(e),
            Step::Stream(_) | Step::Idle(_) => panic!("stream step used for count"),
        }
    }
}

struct Harness {
    mgr: Manager,
    clock: Arc<ManualClock>,
    exec: Arc<Mock>,
    registry: &'static ModelRegistry,
}

fn t0() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).unwrap()
}

impl Harness {
    fn new() -> Self {
        Self::with_executor("mock")
    }

    fn with_executor(provider: &str) -> Self {
        let registry: &'static ModelRegistry = Box::leak(Box::new(ModelRegistry::new()));
        let clock = Arc::new(ManualClock::new(t0()));
        let mgr = Manager::with_parts(clock.clone(), registry);
        let exec = Mock::new(provider);
        mgr.register_executor(exec.clone());
        Harness {
            mgr,
            clock,
            exec,
            registry,
        }
    }

    fn config(&self, edit: impl FnOnce(&mut Config)) {
        let mut cfg = Config::default();
        edit(&mut cfg);
        self.mgr.set_config(Arc::new(cfg));
    }

    /// Registers a credential and the models the registry knows for it.
    async fn add(&self, id: &str, models: &[&str], edit: impl FnOnce(&mut Auth)) -> Auth {
        let provider = self.exec.id.clone();
        let mut auth = Auth::new(id, provider.clone());
        edit(&mut auth);
        let infos: Vec<ModelInfo> = models
            .iter()
            .map(|m| ModelInfo {
                id: (*m).into(),
                ..Default::default()
            })
            .collect();
        self.registry.register_client(id, &provider, &infos);
        self.mgr.register(auth).await.unwrap()
    }

    async fn run(&self, model: &str) -> Result<Response, ExecError> {
        self.mgr
            .execute(
                &["mock".to_string()],
                request(model),
                Options::new(Format::OpenAI),
            )
            .await
    }

    async fn run_with(&self, model: &str, opts: Options) -> Result<Response, ExecError> {
        self.mgr
            .execute(&["mock".to_string()], request(model), opts)
            .await
    }

    async fn payload(&self, model: &str) -> String {
        String::from_utf8(self.run(model).await.unwrap().payload.to_vec()).unwrap()
    }
}

fn request(model: &str) -> Request {
    Request {
        model: model.into(),
        payload: Bytes::from_static(b"{}"),
        format: Format::OpenAI,
        metadata: Default::default(),
    }
}

fn status_err(status: u16, body: &str) -> ExecError {
    ExecError::new(status, body)
}

fn opts_with_body(body: &'static str) -> Options {
    let mut o = Options::new(Format::OpenAI);
    o.original_request = Bytes::from_static(body.as_bytes());
    o
}

// ---- Selection ----

#[tokio::test]
async fn round_robin_rotates_by_identity_and_ties_go_to_lowest_id() {
    let h = Harness::new();
    for id in ["c", "a", "b"] {
        h.add(id, &["m"], |_| {}).await;
    }
    let mut seen = Vec::new();
    for _ in 0..6 {
        seen.push(h.payload("m").await);
    }
    assert_eq!(seen, ["a", "b", "c", "a", "b", "c"]);
}

#[tokio::test]
async fn fill_first_burns_one_credential_until_it_cools() {
    let h = Harness::new();
    h.config(|c| c.routing.strategy = "FILL-FIRST".into());
    for id in ["a", "b"] {
        h.add(id, &["m"], |_| {}).await;
    }
    for _ in 0..3 {
        assert_eq!(h.payload("m").await, "a");
    }
    h.exec.script("a", vec![Step::Err(status_err(503, "down"))]);
    // The failing credential fails over inside the same call and is skipped afterwards.
    assert_eq!(h.payload("m").await, "b");
    assert_eq!(h.payload("m").await, "b");
    // After the 60s transient cooldown fill-first returns to a.
    h.clock.advance(Duration::from_secs(61));
    assert_eq!(h.payload("m").await, "a");
}

#[tokio::test]
async fn weighted_round_robin_is_proportional() {
    let h = Harness::new();
    h.config(|c| c.routing.strategy = "wrr".into());
    h.add("a", &["m"], |a| {
        a.attributes.insert("weight".into(), "2".into());
    })
    .await;
    h.add("b", &["m"], |_| {}).await;
    h.add("z", &["m"], |a| {
        a.attributes.insert("weight".into(), "0".into());
    })
    .await;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for _ in 0..9 {
        *counts.entry(h.payload("m").await).or_default() += 1;
    }
    assert_eq!((counts["a"], counts["b"]), (6, 3));
    assert!(!counts.contains_key("z"), "weight 0 is never selected");
}

#[tokio::test]
async fn priority_tiers_use_lower_tier_only_when_higher_is_blocked() {
    let h = Harness::new();
    h.add("hi", &["m"], |a| {
        a.attributes.insert("priority".into(), "10".into());
    })
    .await;
    h.add("lo", &["m"], |_| {}).await;
    for _ in 0..3 {
        assert_eq!(h.payload("m").await, "hi");
    }
    h.exec
        .script("hi", vec![Step::Err(status_err(500, "boom"))]);
    // hi fails (transient cooldown) and the request falls to the lower tier.
    assert_eq!(h.payload("m").await, "lo");
    assert_eq!(h.payload("m").await, "lo");
    h.clock.advance(Duration::from_secs(61));
    assert_eq!(h.payload("m").await, "hi");
}

#[tokio::test]
async fn model_support_comes_from_the_registry_so_excluded_models_are_not_routable() {
    let h = Harness::new();
    h.add("a", &["m", "other"], |_| {}).await;
    // "b" had `m` excluded at registration time: it only serves `other`.
    h.add("b", &["other"], |_| {}).await;
    for _ in 0..4 {
        assert_eq!(h.payload("m").await, "a");
    }
    let err = h.run("nonexistent").await.unwrap_err();
    assert_eq!(err.auth_code.as_deref(), Some("auth_not_found"));
}

#[tokio::test]
async fn pinned_auth_and_disallow_free_metadata_filter_candidates() {
    let h = Harness::new();
    h.add("a", &["m"], |_| {}).await;
    h.add("b", &["m"], |_| {}).await;
    let mut o = Options::new(Format::OpenAI);
    o.metadata.insert(meta::PINNED_AUTH_ID.into(), "b".into());
    for _ in 0..3 {
        assert_eq!(
            String::from_utf8(h.run_with("m", o.clone()).await.unwrap().payload.to_vec()).unwrap(),
            "b"
        );
    }
}

// ---- Cooldowns ----

#[tokio::test]
async fn quota_429_cools_with_retry_after_floor_then_recovers() {
    let h = Harness::new();
    h.add("a", &["m"], |_| {}).await;
    h.exec.script(
        "a",
        vec![Step::Err(
            status_err(429, "slow down").with_retry_after(Duration::from_secs(30)),
        )],
    );
    let err = h.run("m").await.unwrap_err();
    assert_eq!(err.status, 429);
    assert_eq!(h.exec.count("a"), 1);

    // Still cooling: no upstream call, a model_cooldown error with Retry-After.
    h.clock.advance(Duration::from_secs(5));
    let err = h.run("m").await.unwrap_err();
    assert_eq!(err.auth_code.as_deref(), Some("model_cooldown"));
    assert_eq!(err.status, 429);
    assert_eq!(err.headers.get("retry-after").unwrap(), "25");
    assert_eq!(h.exec.count("a"), 1);
    let v: serde_json::Value = serde_json::from_str(&err.message).unwrap();
    assert_eq!(v["error"]["model"], "m");

    let state = h.mgr.get("a").unwrap();
    assert!(state.model_states["m"].quota.exceeded);
    assert!(h.registry.is_model_quota_exceeded_for_client("a", "m"));

    // After the window the credential is used again and its state clears.
    h.clock.advance(Duration::from_secs(26));
    assert_eq!(h.payload("m").await, "a");
    let state = h.mgr.get("a").unwrap();
    assert!(!state.model_states["m"].quota.exceeded && state.last_error.is_none());
    assert!(!h.registry.is_model_quota_exceeded_for_client("a", "m"));
}

#[tokio::test]
async fn backoff_ladder_escalates_per_failure_window() {
    let h = Harness::new();
    h.add("a", &["m"], |_| {}).await;
    let mut windows = Vec::new();
    for _ in 0..4 {
        h.exec.script("a", vec![Step::Err(status_err(429, "q"))]);
        let before = h.clock.now();
        let _ = h.run("m").await;
        let st = h.mgr.get("a").unwrap();
        let next = st.model_states["m"].next_retry_after.unwrap();
        windows.push((next - before).num_seconds());
        h.clock.advance(Duration::from_secs(
            (next - before).num_seconds() as u64 + 1,
        ));
    }
    assert_eq!(windows, [1, 2, 4, 8]);
}

#[tokio::test]
async fn disable_cooling_keeps_credential_eligible() {
    let h = Harness::new();
    h.add("a", &["m"], |a| {
        a.metadata
            .insert("disable_cooling".into(), serde_json::json!(true));
    })
    .await;
    h.exec.script(
        "a",
        vec![
            Step::Err(status_err(429, "q")),
            Step::Err(status_err(429, "q")),
        ],
    );
    assert!(h.run("m").await.is_err());
    assert!(h.run("m").await.is_err());
    assert_eq!(h.payload("m").await, "a");
    assert_eq!(h.exec.count("a"), 3);
}

#[tokio::test]
async fn cooldown_state_survives_restart_via_file_store() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn CooldownStateStore> = Arc::new(FileCooldownStateStore::new(dir.path()));
    let h = Harness::new();
    h.mgr.set_cooldown_state_store(Some(store.clone()));
    h.add("a", &["m"], |_| {}).await;
    h.exec.script(
        "a",
        vec![Step::Err(
            status_err(429, "q").with_retry_after(Duration::from_secs(120)),
        )],
    );
    assert!(h.run("m").await.is_err());
    h.mgr.persist_cooldown_states().await;

    let h2 = Harness::new();
    h2.mgr.set_cooldown_state_store(Some(store));
    h2.add("a", &["m"], |_| {}).await;
    h2.mgr.restore_cooldown_states().await.unwrap();
    let err = h2.run("m").await.unwrap_err();
    assert_eq!(err.auth_code.as_deref(), Some("model_cooldown"));
    assert_eq!(h2.exec.count("a"), 0);
}

#[tokio::test]
async fn reset_quota_clears_cooldown_and_resumes_registry() {
    let h = Harness::new();
    h.add("a", &["m"], |_| {}).await;
    h.exec.script("a", vec![Step::Err(status_err(429, "q"))]);
    assert!(h.run("m").await.is_err());
    assert!(h.run("m").await.is_err());
    let (auth, models) = h.mgr.reset_quota("a").unwrap().unwrap();
    assert!(models.contains(&"m".to_string()));
    assert!(!auth.unavailable);
    assert_eq!(h.payload("m").await, "a");
    let views = h.mgr.cooldown_snapshot("a").unwrap();
    assert!(views.is_empty());
}

// ---- Refresh ----

#[tokio::test]
async fn unauthorized_refreshes_once_then_retries_same_credential() {
    let h = Harness::new();
    h.add("a", &["m"], |a| {
        a.metadata
            .insert("access_token".into(), serde_json::json!("stale"));
        a.metadata
            .insert("refresh_token".into(), serde_json::json!("rt"));
    })
    .await;
    h.exec.script(
        "a",
        vec![
            Step::Err(status_err(401, "expired")),
            Step::Ok("ok-after-refresh"),
        ],
    );
    assert_eq!(h.payload("m").await, "ok-after-refresh");
    assert_eq!(h.exec.refreshes.lock().len(), 1);
    assert_eq!(h.exec.count("a"), 2);
    let st = h.mgr.get("a").unwrap();
    assert_eq!(st.access_token(), "fresh-token");
    assert!(st.last_refreshed_at.is_some());
    assert_eq!(
        st.failed, 0,
        "the 401 was recovered before a failure was recorded"
    );
}

#[tokio::test]
async fn second_unauthorized_after_refresh_fails_over_and_cools() {
    let h = Harness::new();
    h.add("a", &["m"], |a| {
        a.metadata
            .insert("access_token".into(), serde_json::json!("stale"));
        a.metadata
            .insert("refresh_token".into(), serde_json::json!("rt"));
    })
    .await;
    h.add("b", &["m"], |_| {}).await;
    h.exec.script(
        "a",
        vec![
            Step::Err(status_err(401, "bad")),
            Step::Err(status_err(401, "still bad")),
        ],
    );
    assert_eq!(h.payload("m").await, "b");
    assert_eq!(
        h.exec.refreshes.lock().len(),
        1,
        "refresh is attempted once per attempt"
    );
    let st = h.mgr.get("a").unwrap();
    assert!(is_auth_blocked_for_model(&st, "m", h.clock.now()).blocked);
}

#[tokio::test]
async fn due_credentials_refresh_by_provider_lead() {
    let h = Harness::with_executor("claude");
    let soon = (t0() + chrono::Duration::hours(1)).to_rfc3339();
    let later = (t0() + chrono::Duration::hours(10)).to_rfc3339();
    for (id, exp) in [("due", soon), ("fresh", later)] {
        let mut auth = Auth::new(id, "claude");
        auth.metadata
            .insert("access_token".into(), serde_json::json!("opaque"));
        auth.metadata
            .insert("refresh_token".into(), serde_json::json!("rt"));
        auth.metadata
            .insert("expired".into(), serde_json::json!(exp));
        h.mgr.register(auth).await.unwrap();
    }
    assert_eq!(h.mgr.refresh_due_auths().await, 1);
    assert_eq!(h.exec.refreshes.lock().as_slice(), ["due"]);
    assert!(h.mgr.get("due").unwrap().last_refreshed_at.is_some());
    assert!(h.mgr.get("fresh").unwrap().last_refreshed_at.is_none());
}

// ---- Error handling / retry ----

#[tokio::test]
async fn request_invalid_errors_return_without_failover_or_cooldown() {
    let h = Harness::new();
    h.add("a", &["m"], |_| {}).await;
    h.add("b", &["m"], |_| {}).await;
    h.config(|c| c.request_retry = 3);
    h.exec.script(
        "a",
        vec![Step::Err(status_err(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"bad"}}"#,
        ))],
    );
    let err = h.run("m").await.unwrap_err();
    assert_eq!(err.status, 400);
    assert_eq!(h.exec.call_ids(), ["a"]);
    let st = h.mgr.get("a").unwrap();
    assert!(
        !st.unavailable && st.model_states.is_empty(),
        "request-scoped failure must not cool the credential"
    );
    assert_eq!(st.failed, 1);
}

#[tokio::test]
async fn request_scoped_stop_rule_ends_the_call_without_retry_rounds() {
    let h = Harness::new();
    h.config(|c| c.request_retry = 3);
    h.add("a", &["m"], |a| {
        a.metadata.insert(
            "request_scoped_errors".into(),
            serde_json::json!([{"status": 500, "match": ["content filter"], "action": "stop"}]),
        );
    })
    .await;
    h.add("b", &["m"], |_| {}).await;
    h.exec.script(
        "a",
        vec![Step::Err(status_err(500, "blocked by content filter"))],
    );
    let err = h.run("m").await.unwrap_err();
    assert_eq!(err.status, 500);
    assert_eq!(h.exec.call_ids(), ["a"]);
    assert!(h.mgr.get("a").unwrap().model_states.is_empty());
}

#[tokio::test]
async fn continue_rule_rotates_without_cooling_and_cooldown_variant_cools() {
    let h = Harness::new();
    h.add("a", &["m"], |a| {
        a.metadata.insert(
            "request_scoped_errors".into(),
            serde_json::json!([
                {"status": 500, "match": ["soft"], "action": "continue"},
                {"status": 500, "match": ["hard"], "action": "continue-and-cooldown"}
            ]),
        );
    })
    .await;
    h.add("b", &["m"], |_| {}).await;
    h.exec.script(
        "a",
        vec![
            Step::Err(status_err(500, "soft failure")),
            Step::Err(status_err(500, "hard failure")),
        ],
    );
    assert_eq!(h.payload("m").await, "b");
    assert!(
        h.mgr.get("a").unwrap().model_states.is_empty(),
        "continue does not cool"
    );
    // b is next in rotation, then a again (hard failure forces a cooldown).
    assert_eq!(h.payload("m").await, "b");
    assert_eq!(h.payload("m").await, "b");
    assert!(is_auth_blocked_for_model(&h.mgr.get("a").unwrap(), "m", h.clock.now()).blocked);
}

#[tokio::test]
async fn retry_rounds_respect_per_credential_limits() {
    let h = Harness::new();
    h.config(|c| c.request_retry = 3);
    for (id, limit) in [("retry-a", 3), ("retry-b", 2), ("retry-c", 2)] {
        h.add(id, &["m"], |a| {
            a.metadata
                .insert("request_retry".into(), serde_json::json!(limit));
            a.metadata
                .insert("disable_cooling".into(), serde_json::json!(true));
        })
        .await;
        h.exec.script(
            id,
            (0..10)
                .map(|_| Step::Err(status_err(500, "fail")))
                .collect(),
        );
    }
    assert!(h.run("m").await.is_err());
    assert_eq!(
        (
            h.exec.count("retry-a"),
            h.exec.count("retry-b"),
            h.exec.count("retry-c")
        ),
        (4, 3, 3)
    );
}

#[tokio::test]
async fn max_retry_credentials_bounds_each_round_and_ages_skipped_credentials() {
    let h = Harness::new();
    h.config(|c| {
        c.request_retry = 2;
        c.max_retry_credentials = 3;
    });
    for (id, limit) in [("cap-a", 1), ("cap-b", 1), ("cap-c", 1), ("cap-d", 2)] {
        h.add(id, &["m"], |a| {
            a.metadata
                .insert("request_retry".into(), serde_json::json!(limit));
            a.metadata
                .insert("disable_cooling".into(), serde_json::json!(true));
        })
        .await;
        h.exec.script(
            id,
            (0..10)
                .map(|_| Step::Err(status_err(500, "fail")))
                .collect(),
        );
    }
    assert!(h.run("m").await.is_err());
    let calls = h.exec.call_ids();
    assert_eq!(calls.len(), 7, "{calls:?}");
    assert_eq!(calls.last().map(String::as_str), Some("cap-d"));
}

#[tokio::test]
async fn retry_round_waits_for_cooldown_within_max_retry_interval() {
    let h = Harness::new();
    h.config(|c| {
        c.request_retry = 1;
        c.max_retry_interval = 30;
    });
    h.add("a", &["m"], |_| {}).await;
    h.exec.script(
        "a",
        vec![
            Step::Err(status_err(429, "q").with_retry_after(Duration::from_secs(12))),
            Step::Ok("recovered"),
        ],
    );
    let resp = h.run("m").await.unwrap();
    assert_eq!(resp.payload, "recovered");
    let sleeps = h.clock.sleeps();
    assert_eq!(sleeps.len(), 1);
    // A 10s floor / 12s retry-after plus jitter bounded by 2s and by max-retry-interval.
    assert!(
        sleeps[0] >= Duration::from_secs(12) && sleeps[0] <= Duration::from_secs(14),
        "{sleeps:?}"
    );
}

#[tokio::test]
async fn retry_round_does_not_wait_longer_than_max_retry_interval() {
    let h = Harness::new();
    h.config(|c| {
        c.request_retry = 2;
        c.max_retry_interval = 5;
    });
    h.add("a", &["m"], |_| {}).await;
    h.exec.script(
        "a",
        vec![Step::Err(
            status_err(429, "q").with_retry_after(Duration::from_secs(60)),
        )],
    );
    let err = h.run("m").await.unwrap_err();
    assert_eq!(err.status, 429);
    assert!(h.clock.sleeps().is_empty());
    assert_eq!(h.exec.count("a"), 1);
}

#[tokio::test]
async fn pick_failure_after_an_attempt_reports_the_upstream_error_not_no_auth() {
    let h = Harness::new();
    h.add("a", &["m"], |_| {}).await;
    h.exec
        .script("a", vec![Step::Err(status_err(503, "upstream overloaded"))]);
    let err = h.run("m").await.unwrap_err();
    assert_eq!(err.status, 503);
    assert!(
        err.message.contains("upstream overloaded"),
        "{}",
        err.message
    );
    assert!(err.auth_code.is_none());
}

#[tokio::test]
async fn count_tokens_uses_the_same_selection_and_failover() {
    let h = Harness::new();
    h.add("a", &["m"], |_| {}).await;
    h.add("b", &["m"], |_| {}).await;
    h.exec.script("a", vec![Step::Err(status_err(500, "x"))]);
    let r = h
        .mgr
        .execute_count(&["mock".into()], request("m"), Options::new(Format::OpenAI))
        .await
        .unwrap();
    assert_eq!(r.payload, "b");
}

// ---- Streams ----

async fn run_session(h: &Harness, body: &'static str) -> Result<Response, ExecError> {
    h.run_with("m", opts_with_body(body)).await
}

async fn drain(mut s: StreamResult) -> Vec<Result<String, String>> {
    let mut out = Vec::new();
    while let Some(c) = s.chunks.recv().await {
        out.push(
            c.map(|b| String::from_utf8(b.to_vec()).unwrap())
                .map_err(|e| e.message),
        );
    }
    out
}

#[tokio::test]
async fn stream_fails_over_only_before_the_first_payload() {
    let h = Harness::new();
    h.add("a", &["m"], |_| {}).await;
    h.add("b", &["m"], |_| {}).await;
    // Bootstrap failure on a (error before any payload): b serves the stream.
    h.exec.script(
        "a",
        vec![Step::Stream(vec![Err(status_err(502, "bad gateway"))])],
    );
    let stream = h
        .mgr
        .execute_stream(&["mock".into()], request("m"), Options::new(Format::OpenAI))
        .await
        .unwrap();
    assert_eq!(drain(stream).await, vec![Ok("b".to_string())]);
    assert_eq!(h.exec.call_ids(), ["a", "b"]);

    // Empty stream (closed without payload) also fails over.
    let h = Harness::new();
    h.add("a", &["m"], |_| {}).await;
    h.add("b", &["m"], |_| {}).await;
    h.exec.script("a", vec![Step::Stream(vec![])]);
    let stream = h
        .mgr
        .execute_stream(&["mock".into()], request("m"), Options::new(Format::OpenAI))
        .await
        .unwrap();
    assert_eq!(drain(stream).await, vec![Ok("b".to_string())]);
}

#[tokio::test]
async fn stream_error_after_first_chunk_is_in_band_and_never_replayed() {
    let h = Harness::new();
    h.add("a", &["m"], |_| {}).await;
    h.add("b", &["m"], |_| {}).await;
    h.exec.script(
        "a",
        vec![Step::Stream(vec![
            Ok("first"),
            Ok("second"),
            Err(status_err(500, "mid-stream failure")),
        ])],
    );
    let stream = h
        .mgr
        .execute_stream(&["mock".into()], request("m"), Options::new(Format::OpenAI))
        .await
        .unwrap();
    let chunks = drain(stream).await;
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0], Ok("first".to_string()));
    assert_eq!(chunks[2], Err("mid-stream failure".to_string()));
    assert_eq!(h.exec.call_ids(), ["a"], "no replay on another credential");
    // The failure is recorded (once) against a after the stream finished.
    tokio::task::yield_now().await;
    let st = h.mgr.get("a").unwrap();
    assert_eq!(st.failed, 1);
    assert!(st.model_states["m"].unavailable);
}

#[tokio::test]
async fn successful_stream_records_success_when_it_ends() {
    let h = Harness::new();
    h.add("a", &["m"], |_| {}).await;
    h.exec
        .script("a", vec![Step::Stream(vec![Ok("x"), Ok("y")])]);
    let stream = h
        .mgr
        .execute_stream(&["mock".into()], request("m"), Options::new(Format::OpenAI))
        .await
        .unwrap();
    assert_eq!(
        drain(stream).await,
        vec![Ok("x".to_string()), Ok("y".to_string())]
    );
    tokio::task::yield_now().await;
    assert_eq!(h.mgr.get("a").unwrap().success, 1);
}

#[tokio::test]
async fn all_bootstrap_failures_surface_as_an_error_stream_with_upstream_headers() {
    let h = Harness::new();
    h.add("a", &["m"], |_| {}).await;
    h.exec.script(
        "a",
        vec![Step::Stream(vec![Err(status_err(502, "bad gateway"))])],
    );
    let stream = h
        .mgr
        .execute_stream(&["mock".into()], request("m"), Options::new(Format::OpenAI))
        .await
        .unwrap();
    assert_eq!(drain(stream).await, vec![Err("bad gateway".to_string())]);
}

// ---- Models ----

#[tokio::test]
async fn oauth_alias_routes_to_upstream_model_and_force_mapping_rewrites_response() {
    let h = Harness::new();
    h.config(|c| {
        c.oauth_model_alias.insert(
            "mock".into(),
            vec![OAuthModelAlias {
                name: "up-model".into(),
                alias: "friendly".into(),
                force_mapping: true,
                ..Default::default()
            }],
        );
    });
    h.add("a", &["friendly"], |_| {}).await;
    h.exec
        .script("a", vec![Step::Ok(r#"{"model":"up-model","id":1}"#)]);
    let resp = h.run("friendly(8192)").await;
    // The registry knows `friendly`; a thinking suffix is stripped for matching and kept upstream.
    let resp = resp.unwrap();
    assert_eq!(h.exec.calls.lock()[0].1, "up-model(8192)");
    assert_eq!(
        String::from_utf8(resp.payload.to_vec()).unwrap(),
        r#"{"model":"friendly(8192)","id":1}"#.replace("friendly(8192)", "friendly")
    );
}

#[tokio::test]
async fn prefix_is_stripped_before_the_executor_and_state_is_keyed_by_upstream_name() {
    let h = Harness::new();
    h.add("a", &["team/m"], |a| a.prefix = "team".into()).await;
    assert_eq!(h.payload("team/m").await, "a");
    assert_eq!(h.exec.calls.lock()[0].1, "m");
    // Cooldown state follows the (route model) key, shared across thinking suffixes.
    h.exec.script("a", vec![Step::Err(status_err(429, "q"))]);
    assert!(h.run("team/m").await.is_err());
    let err = h.run("team/m(high)").await.unwrap_err();
    assert_eq!(err.auth_code.as_deref(), Some("model_cooldown"));
}

#[tokio::test]
async fn api_key_alias_pool_rotates_and_falls_through_on_failure() {
    let h = Harness::with_executor("openai-compatible-mock");
    h.config(|c| {
        c.openai_compatibility
            .push(cpa_config::OpenAiCompatibility {
                name: "mock".into(),
                models: vec![
                    cpa_config::OpenAiCompatibilityModel {
                        name: "up-1".into(),
                        alias: "pooled".into(),
                        ..Default::default()
                    },
                    cpa_config::OpenAiCompatibilityModel {
                        name: "up-2".into(),
                        alias: "pooled".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            });
    });
    h.add("a", &["pooled"], |a| {
        a.provider = "openai-compatibility".into();
        a.attributes.insert("compat_name".into(), "mock".into());
        a.attributes.insert("api_key".into(), "k".into());
        a.attributes
            .insert("source".into(), "config:openai-compatibility[abc]".into());
    })
    .await;
    let providers = ["openai-compatible-mock".to_string()];
    let run = || {
        h.mgr
            .execute(&providers, request("pooled"), Options::new(Format::OpenAI))
    };
    run().await.unwrap();
    run().await.unwrap();
    let models: Vec<String> = h.exec.calls.lock().iter().map(|(_, m)| m.clone()).collect();
    assert_eq!(models, ["up-1", "up-2"], "pool rotates its starting model");
    // First pooled model fails: the second one serves within the same attempt.
    h.exec
        .script("a", vec![Step::Err(status_err(500, "pool member down"))]);
    let before = h.exec.calls.lock().len();
    run().await.unwrap();
    let models: Vec<String> = h.exec.calls.lock()[before..]
        .iter()
        .map(|(_, m)| m.clone())
        .collect();
    assert_eq!(models, ["up-1", "up-2"]);
}

// ---- Session affinity ----

#[tokio::test]
async fn session_affinity_sticks_fails_over_and_expires() {
    let h = Harness::new();
    h.config(|c| {
        c.routing.session_affinity = true;
        c.routing.session_affinity_ttl = "10s".into();
    });
    for id in ["a", "b", "c"] {
        h.add(id, &["m"], |_| {}).await;
    }
    let body = r#"{"prompt_cache_key":"sess-1","messages":[{"role":"user","content":"hi"}]}"#;
    let first = String::from_utf8(run_session(&h, body).await.unwrap().payload.to_vec()).unwrap();
    for _ in 0..4 {
        assert_eq!(
            String::from_utf8(run_session(&h, body).await.unwrap().payload.to_vec()).unwrap(),
            first,
            "session sticks"
        );
    }
    // A different session is distributed by the fallback strategy.
    let other = String::from_utf8(
        h.run_with("m", opts_with_body(r#"{"prompt_cache_key":"sess-2"}"#))
            .await
            .unwrap()
            .payload
            .to_vec(),
    )
    .unwrap();
    assert_ne!(other, first);

    // The bound credential failing rebinds the session to another credential.
    h.exec
        .script(&first, vec![Step::Err(status_err(500, "down"))]);
    let moved = String::from_utf8(run_session(&h, body).await.unwrap().payload.to_vec()).unwrap();
    assert_ne!(moved, first);
    for _ in 0..3 {
        assert_eq!(
            String::from_utf8(run_session(&h, body).await.unwrap().payload.to_vec()).unwrap(),
            moved
        );
    }
    // After the TTL the binding lapses (the fallback strategy decides again).
    h.clock.advance(Duration::from_secs(11));
    let sel = h.mgr.selector();
    assert!(
        sel.affinity()
            .unwrap()
            .cache()
            .get("mixed::pck:sess-1::m")
            .is_none()
    );
}

#[tokio::test]
async fn lookup_session_affinity_reports_bound_credential() {
    let h = Harness::new();
    h.config(|c| c.routing.session_affinity = true);
    h.add("a", &["m"], |_| {}).await;
    h.run_with("m", opts_with_body(r#"{"prompt_cache_key":"look"}"#))
        .await
        .unwrap();
    let (auth, status) = h.mgr.lookup_session_affinity("mixed", "m", "pck:look");
    assert_eq!(status, "bound");
    assert_eq!(auth.unwrap().id, "a");
    assert_eq!(
        h.mgr.lookup_session_affinity("mixed", "m", "missing").1,
        "unbound"
    );
}

// ---- Lifecycle ----

#[tokio::test]
async fn update_upserts_and_keeps_counters_and_credential_cooldown() {
    let h = Harness::new();
    let a = h.add("a", &["m"], |_| {}).await;
    assert_eq!((a.registration_epoch, a.generation), (1, 1));
    h.payload("m").await;
    let mut edited = h.mgr.get("a").unwrap();
    edited
        .metadata
        .insert("note".into(), serde_json::json!("hello"));
    let updated = h.mgr.update(edited).await.unwrap();
    assert_eq!(updated.registration_epoch, 1);
    assert!(updated.generation > 1);
    assert_eq!(
        h.mgr.get("a").unwrap().success,
        1,
        "counters survive an update"
    );
    // Unknown ids register.
    let fresh = h.mgr.update(Auth::new("new", "mock")).await.unwrap();
    assert_eq!(fresh.registration_epoch, 1);
    // Remove then re-register bumps the epoch past the tombstone.
    h.mgr.remove("a").await;
    assert!(h.mgr.get("a").is_none());
    let again = h.mgr.register(Auth::new("a", "mock")).await.unwrap();
    assert_eq!(again.registration_epoch, 3);
}

#[tokio::test]
async fn invalid_weight_is_rejected_on_register() {
    let h = Harness::new();
    let mut a = Auth::new("w", "mock");
    a.attributes.insert("weight".into(), "not-a-number".into());
    assert!(h.mgr.register(a).await.is_err());
}

#[tokio::test]
async fn error_enrichment_adds_routing_context() {
    let h = Harness::new();
    let err = h.run("nothing-registered").await.unwrap_err();
    let enriched = enrich_auth_selection_error(err, &["claude".into()], "nothing-registered");
    assert!(
        enriched
            .message
            .contains("providers=claude, model=nothing-registered"),
        "{}",
        enriched.message
    );
    assert!(enriched.message.contains("/v0/management/auth-files"));
}

#[tokio::test]
async fn config_reload_rebuilds_selector_only_when_routing_changes() {
    let h = Harness::new();
    let before = h.mgr.selector();
    h.config(|c| c.request_retry = 2);
    assert!(Arc::ptr_eq(&before, &h.mgr.selector()));
    h.config(|c| c.routing.strategy = "fill-first".into());
    assert!(!Arc::ptr_eq(&before, &h.mgr.selector()));
    assert_eq!(h.mgr.selector().config.strategy, Strategy::FillFirst);
}

#[test]
fn executor_error_code_marks_request_scoped() {
    let e = ExecError::new(500, "x").with_code(ErrorCode::RequestScoped);
    assert!(errors::Failure::of_exec(&e).is_request_invalid());
}

// ---- Alias-aware availability (Go: conductor_alias_cooldown_test) ----

fn mark_failure(h: &Harness, auth_id: &str, model: &str, status: i32, retry_after: Duration) {
    h.mgr.mark_result(cooldown::ExecResult {
        auth_id: auth_id.into(),
        provider: "mock".into(),
        model: model.into(),
        route_model: model.into(),
        success: false,
        retry_after: Some(retry_after),
        credential_scope: false,
        error: Some(cpa_auth::types::AuthError {
            http_status: status,
            message: "limited".into(),
            ..Default::default()
        }),
        options: Options::new(Format::OpenAI),
        skip_quota_observation: true,
        response_headers: Default::default(),
    });
}

#[tokio::test]
async fn alias_quota_failover_with_unobserved_target_model_for_every_strategy() {
    for strategy in ["round-robin", "weighted-round-robin", "fill-first"] {
        for stream in [false, true] {
            let h = Harness::new();
            h.config(|c| {
                c.routing.strategy = strategy.into();
                c.request_retry = 3;
                c.max_retry_interval = 30;
                c.oauth_model_alias.insert(
                    "mock".into(),
                    vec![OAuthModelAlias {
                        name: "quota-target".into(),
                        alias: "quota-route".into(),
                        fork: true,
                        ..Default::default()
                    }],
                );
            });
            for (id, prio) in [("high", "4"), ("low", "3")] {
                h.add(id, &["quota-route", "quota-target", "quota-other"], |a| {
                    a.attributes.insert("priority".into(), prio.into());
                    a.attributes.insert("weight".into(), "1".into());
                })
                .await;
            }
            // An unrelated model of the preferred credential is rate limited for an hour: that
            // flags the credential's aggregate state but not the requested target model.
            mark_failure(&h, "high", "quota-other", 429, Duration::from_secs(3600));
            let high = h.mgr.get("high").unwrap();
            assert!(high.unavailable && !high.model_states.contains_key("quota-target"));

            h.exec.script(
                "high",
                vec![Step::Err(
                    status_err(429, "account quota exhausted")
                        .with_retry_after(Duration::from_secs(3600))
                        .with_credential_scope(),
                )],
            );
            let providers = ["mock".to_string()];
            let payload = if stream {
                let s = h
                    .mgr
                    .execute_stream(
                        &providers,
                        request("quota-route"),
                        Options::new(Format::OpenAI),
                    )
                    .await
                    .unwrap();
                drain(s)
                    .await
                    .into_iter()
                    .map(|c| c.unwrap())
                    .collect::<String>()
            } else {
                h.payload("quota-route").await
            };
            assert_eq!(payload, "low", "{strategy} stream={stream}");
            assert_eq!(
                h.exec.call_ids(),
                ["high", "low"],
                "{strategy} stream={stream}"
            );
            assert!(
                h.exec.calls.lock().iter().all(|(_, m)| m == "quota-target"),
                "upstream model is the alias target"
            );
        }
    }
}

#[tokio::test]
async fn alias_request_is_not_blocked_by_other_model_cooldown_but_by_its_own_target() {
    let h = Harness::new();
    h.config(|c| {
        c.oauth_model_alias.insert(
            "mock".into(),
            vec![OAuthModelAlias {
                name: "target".into(),
                alias: "route".into(),
                fork: true,
                ..Default::default()
            }],
        );
    });
    h.add("a", &["route", "target", "image"], |_| {}).await;
    mark_failure(&h, "a", "image", 429, Duration::from_secs(3600));
    // Another model's quota cooldown does not block the alias route...
    assert_eq!(h.payload("route").await, "a");
    // ...but a cooldown of the alias target does, reported against the route model.
    mark_failure(&h, "a", "target", 429, Duration::from_secs(3600));
    let err = h.run("route").await.unwrap_err();
    assert_eq!(err.auth_code.as_deref(), Some("model_cooldown"));
    let v: serde_json::Value = serde_json::from_str(&err.message).unwrap();
    assert_eq!(v["error"]["model"], "route");
    assert_eq!(
        h.mgr
            .select_auth("mock", "route", &Options::new(Format::OpenAI))
            .unwrap_err()
            .status,
        429
    );
}

// ---- Retry-storm guards (Go: conductor_subsecond_cooldown_test) ----

fn expired_state_edit(model: &'static str, at: DateTime<Utc>) -> impl FnOnce(&mut Auth) {
    move |a: &mut Auth| {
        a.model_states.insert(
            model.into(),
            cpa_auth::types::ModelState {
                status: cpa_auth::types::Status::Error,
                unavailable: true,
                next_retry_after: Some(at),
                quota: cpa_auth::types::QuotaState {
                    exceeded: true,
                    reason: "quota".into(),
                    next_recover_at: Some(at),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
    }
}

#[tokio::test]
async fn subsecond_retry_after_is_floored_so_rounds_do_not_storm() {
    let h = Harness::new();
    h.config(|c| {
        c.request_retry = 5;
        c.max_retry_interval = 5;
        c.max_retry_credentials = 6;
    });
    for id in ["storm-1", "storm-2"] {
        h.add(id, &["m"], |_| {}).await;
        h.exec.script(
            id,
            (0..10)
                .map(|_| {
                    Step::Err(
                        status_err(429, "quota exhausted")
                            .with_retry_after(Duration::from_millis(708)),
                    )
                })
                .collect(),
        );
    }
    let err = h.run("m").await.unwrap_err();
    assert_eq!(err.status, 429);
    // Both credentials are tried once; their 10s floor exceeds the 5s max wait, so no further round.
    assert_eq!(h.exec.call_ids().len(), 2);
    assert!(h.clock.sleeps().is_empty());
}

#[tokio::test]
async fn attempted_credential_after_429_never_gets_a_zero_wait_round() {
    let h = Harness::new();
    let expired = t0() - chrono::Duration::seconds(5);
    h.add("a", &["m"], expired_state_edit("m", expired)).await;
    let elig = pick::Eligibility::default();
    let providers = ["mock".to_string()];
    // Untried credential with an expired cooldown is available immediately.
    let untried =
        h.mgr
            .closest_cooldown_wait(&providers, "m", 0, &elig, "", 5, 429, &Default::default());
    assert_eq!(untried, Some(Duration::ZERO));
    // The same credential after failing this round with 429 must wait at least the quota floor.
    let attempted: std::collections::HashSet<String> = ["a".to_string()].into();
    let wait = h
        .mgr
        .closest_cooldown_wait(&providers, "m", 0, &elig, "", 5, 429, &attempted)
        .unwrap();
    assert!(wait >= cooldown::MIN_QUOTA_COOLDOWN_FLOOR, "{wait:?}");
    // And should_retry honors a large max wait with that positive wait.
    let err = status_err(429, "RESOURCE_EXHAUSTED");
    let (wait, retry) = h.mgr.should_retry_after_error(
        &err,
        0,
        &providers,
        "m",
        Duration::from_secs(30),
        5,
        &attempted,
        &Default::default(),
    );
    assert!(retry && wait >= cooldown::MIN_QUOTA_COOLDOWN_FLOOR);
}

#[tokio::test]
async fn provider_cooling_override_allows_immediate_retry_round() {
    let h = Harness::with_executor("openai-compatibility");
    let expired = t0() - chrono::Duration::seconds(5);
    h.add("a", &["m"], |a| {
        a.attributes
            .insert("provider_key".into(), "custom-llm".into());
        expired_state_edit("m", expired)(a);
    })
    .await;
    let elig = pick::Eligibility::default();
    let providers = ["openai-compatibility".to_string()];
    let attempted: std::collections::HashSet<String> = ["a".to_string()].into();
    let wait = h
        .mgr
        .closest_cooldown_wait(&providers, "m", 0, &elig, "", 5, 429, &attempted)
        .unwrap();
    assert!(wait >= cooldown::MIN_QUOTA_COOLDOWN_FLOOR);
    h.config(|c| {
        c.openai_compatibility
            .push(cpa_config::OpenAiCompatibility {
                name: "custom-llm".into(),
                disable_cooling: Some(true),
                ..Default::default()
            });
    });
    let wait = h
        .mgr
        .closest_cooldown_wait(&providers, "m", 0, &elig, "", 5, 429, &attempted);
    assert_eq!(wait, Some(Duration::ZERO));
}

#[tokio::test]
async fn later_shorter_failure_keeps_the_longer_model_deadline() {
    let h = Harness::new();
    h.add("a", &["m"], |_| {}).await;
    mark_failure(&h, "a", "m", 429, Duration::from_secs(600));
    let long = h.mgr.get("a").unwrap().model_states["m"].next_retry_after;
    mark_failure(&h, "a", "m", 429, Duration::from_secs(15));
    assert_eq!(
        h.mgr.get("a").unwrap().model_states["m"].next_retry_after,
        long
    );
    // Credential-scope failures extend siblings but never shorten them.
    let r = cooldown::ExecResult {
        auth_id: "a".into(),
        provider: "mock".into(),
        model: "m".into(),
        route_model: "m".into(),
        success: false,
        retry_after: Some(Duration::from_secs(30)),
        credential_scope: true,
        error: Some(cpa_auth::types::AuthError {
            http_status: 429,
            message: "q".into(),
            ..Default::default()
        }),
        options: Options::new(Format::OpenAI),
        skip_quota_observation: true,
        response_headers: Default::default(),
    };
    h.mgr.mark_result(r);
    let st = h.mgr.get("a").unwrap();
    assert_eq!(st.quota.reason, "credential_quota");
    assert!(st.model_states["m"].next_retry_after >= long);
}

// ---- Compile-time guarantees ----

#[test]
fn manager_futures_are_send_so_handlers_can_spawn_them() {
    fn is_send<T: Send>(_: &T) {}
    let m = Manager::new();
    let providers: Vec<String> = Vec::new();
    is_send(&m.execute(&providers, request("m"), Options::new(Format::OpenAI)));
    is_send(&m.execute_stream(&providers, request("m"), Options::new(Format::OpenAI)));
    is_send(&m.execute_count(&providers, request("m"), Options::new(Format::OpenAI)));
    is_send(&m.update(Auth::new("a", "mock")));
    is_send(&m.remove("a"));
    is_send(&m.force_refresh_all());
    is_send(&m.refresh_due_auths());
    is_send(&m.restore_cooldown_states());
    fn is_sync<T: Sync>(_: &T) {}
    is_sync(&m);
}

#[tokio::test]
async fn payload_only_requests_still_get_session_affinity() {
    let h = Harness::new();
    h.config(|c| c.routing.session_affinity = true);
    for id in ["a", "b", "c"] {
        h.add(id, &["m"], |_| {}).await;
    }
    // No `original_request`: enrichment shares the request payload, so affinity still sees it.
    let mut req = request("m");
    req.payload = Bytes::from_static(br#"{"prompt_cache_key":"payload-only"}"#);
    let providers = ["mock".to_string()];
    let first = h
        .mgr
        .execute(&providers, req.clone(), Options::new(Format::OpenAI))
        .await
        .unwrap()
        .payload;
    for _ in 0..3 {
        assert_eq!(
            h.mgr
                .execute(&providers, req.clone(), Options::new(Format::OpenAI))
                .await
                .unwrap()
                .payload,
            first
        );
    }
}

// ---- Auto-refresh loop ----

#[tokio::test]
async fn auto_refresh_loop_refreshes_due_credentials_once_and_backs_off() {
    let h = Harness::with_executor("claude");
    let soon = (t0() + chrono::Duration::hours(1)).to_rfc3339();
    let mut auth = Auth::new("loop-due", "claude");
    auth.metadata
        .insert("access_token".into(), serde_json::json!("opaque"));
    auth.metadata
        .insert("refresh_token".into(), serde_json::json!("rt"));
    auth.metadata
        .insert("expired".into(), serde_json::json!(soon));
    h.mgr.register(auth).await.unwrap();
    h.mgr.start_auto_refresh(Duration::from_secs(1));
    for _ in 0..100 {
        if !h.exec.refreshes.lock().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Let any (incorrect) extra refresh happen before asserting there was exactly one.
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.mgr.stop_auto_refresh();
    assert_eq!(h.exec.refreshes.lock().as_slice(), ["loop-due"]);
    let st = h.mgr.get("loop-due").unwrap();
    assert!(st.last_refreshed_at.is_some());
    // The mock does not move the expiry, so the refresh is "ineffective": the loop backs off 30s
    // instead of spinning.
    assert_eq!(
        st.next_refresh_after,
        Some(t0() + chrono::Duration::seconds(30))
    );
}

// ---- Antigravity credits fallback ----

#[tokio::test]
async fn antigravity_credits_fallback_retries_claude_models_with_credits_flag() {
    let h = Harness::with_executor("antigravity");
    h.config(|c| c.quota_exceeded.antigravity_credits = true);
    h.add("ag-1", &["claude-sonnet-4-6"], |_| {}).await;
    h.add("ag-2", &["claude-sonnet-4-6"], |_| {}).await;
    set_antigravity_credits_hint(
        "ag-2",
        AntigravityCreditsHint {
            known: true,
            available: true,
            ..Default::default()
        },
    );
    for id in ["ag-1", "ag-2"] {
        h.exec.script(
            id,
            vec![
                Step::Err(status_err(429, "quota").with_retry_after(Duration::from_secs(3600))),
                Step::Ok("with-credits"),
            ],
        );
    }
    let resp = h
        .mgr
        .execute(
            &["antigravity".to_string()],
            request("claude-sonnet-4-6"),
            Options::new(Format::OpenAI),
        )
        .await
        .unwrap();
    assert_eq!(resp.payload, "with-credits");
    assert_eq!(*h.exec.credit_flags.lock().last().unwrap(), true);
    assert_eq!(
        h.exec.credit_flags.lock().iter().filter(|f| !**f).count(),
        2,
        "both credentials were tried normally first"
    );
    // The credential known to have credits is preferred for the fallback.
    assert_eq!(h.exec.call_ids().last().map(String::as_str), Some("ag-2"));

    // Non-Claude models never use the credits path.
    let h2 = Harness::with_executor("antigravity");
    h2.config(|c| c.quota_exceeded.antigravity_credits = true);
    h2.add("ag-3", &["gemini-3-flash"], |_| {}).await;
    h2.exec.script(
        "ag-3",
        vec![Step::Err(
            status_err(429, "quota").with_retry_after(Duration::from_secs(3600)),
        )],
    );
    let err = h2
        .mgr
        .execute(
            &["antigravity".to_string()],
            request("gemini-3-flash"),
            Options::new(Format::OpenAI),
        )
        .await
        .unwrap_err();
    assert_eq!(err.status, 429);
    assert_eq!(h2.exec.call_ids().len(), 1);
}

// ---- Usage records ----

#[tokio::test]
async fn every_attempt_records_usage_with_tokens_alias_and_failure_details() {
    let h = Harness::new();
    let tracker = Arc::new(crate::usage::UsageTracker::new());
    h.mgr.set_usage_tracker(Some(tracker.clone()));
    h.config(|c| {
        c.oauth_model_alias.insert(
            "mock".into(),
            vec![OAuthModelAlias {
                name: "up".into(),
                alias: "friendly".into(),
                ..Default::default()
            }],
        );
    });
    h.add("a", &["friendly"], |a| {
        a.label = "me@example.com".into();
    })
    .await;
    h.exec.script(
        "a",
        vec![
            Step::Ok(r#"{"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#),
            Step::Err(status_err(500, "upstream exploded")),
        ],
    );
    h.run("friendly").await.unwrap();
    h.run("friendly").await.unwrap_err();

    let events = tracker.requests(10, None).events;
    assert_eq!(events.len(), 2);
    let ok = &events[0].record;
    assert!(!ok.failed && !ok.stream);
    assert_eq!(
        (ok.provider.as_str(), ok.model.as_str(), ok.alias.as_str()),
        ("mock", "up", "friendly")
    );
    assert_eq!(
        (
            ok.tokens.input_tokens,
            ok.tokens.output_tokens,
            ok.tokens.total_tokens
        ),
        (3, 2, 5)
    );
    assert_eq!(ok.source, "me@example.com");
    assert!(!ok.auth_index.is_empty());
    let failed = &events[1].record;
    assert!(failed.failed);
    assert_eq!(
        (failed.fail.status_code, failed.fail.body.as_str()),
        (500, "upstream exploded")
    );
}

#[tokio::test]
async fn streams_record_usage_when_they_finish_and_count_tokens_does_not() {
    let h = Harness::new();
    let tracker = Arc::new(crate::usage::UsageTracker::new());
    h.mgr.set_usage_tracker(Some(tracker.clone()));
    h.add("a", &["m"], |_| {}).await;
    h.exec.script(
        "a",
        vec![Step::Stream(vec![
            Ok("data: {\"choices\":[]}\n\n"),
            Ok("data: {\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":6}}\n\n"),
        ])],
    );
    let providers = ["mock".to_string()];
    let s = h
        .mgr
        .execute_stream(&providers, request("m"), Options::new(Format::OpenAI))
        .await
        .unwrap();
    assert_eq!(drain(s).await.len(), 2);
    for _ in 0..50 {
        if !tracker.requests(10, None).events.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let events = tracker.requests(10, None).events;
    assert_eq!(events.len(), 1);
    let rec = &events[0].record;
    assert!(rec.stream && !rec.failed);
    assert_eq!(
        (
            rec.tokens.input_tokens,
            rec.tokens.output_tokens,
            rec.tokens.total_tokens
        ),
        (4, 6, 10)
    );
    h.mgr
        .execute_count(&providers, request("m"), Options::new(Format::OpenAI))
        .await
        .unwrap();
    assert_eq!(
        tracker.requests(10, None).events.len(),
        1,
        "token counting is not usage"
    );
}

#[tokio::test]
async fn client_disconnect_closes_idle_upstream_and_claude_oauth_records_nothing() {
    let h = Harness::with_executor("claude");
    h.add("a", &["m"], |a| {
        a.attributes.insert("auth_kind".into(), "oauth".into());
    })
    .await;
    h.exec.script("a", vec![Step::Idle("first")]);
    let mut stream = h
        .mgr
        .execute_stream(
            &["claude".into()],
            request("m"),
            Options::new(Format::OpenAI),
        )
        .await
        .unwrap();
    assert_eq!(stream.chunks.recv().await.unwrap().unwrap(), "first");
    let upstream = h.exec.held.lock().pop().unwrap();
    // The client goes away while the upstream is idle: the upstream stream must be dropped
    // promptly, not at its next chunk.
    drop(stream);
    tokio::time::timeout(Duration::from_secs(1), upstream.closed())
        .await
        .expect("upstream closed after client disconnect");
    tokio::task::yield_now().await;
    let st = h.mgr.get("a").unwrap();
    assert_eq!(
        (st.success, st.failed),
        (0, 0),
        "no result for an abandoned Claude OAuth stream"
    );
}
