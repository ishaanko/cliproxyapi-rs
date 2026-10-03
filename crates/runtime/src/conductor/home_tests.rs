//! Home-mode conductor tests against a scripted Home dispatcher and a scripted executor (ports
//! of the Go `home_*_test.go` behaviors: dispatch, concurrency accounting, retry rounds, force
//! mapping, websocket retention).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_config::Config;
use cpa_core::registry::ModelRegistry;
use cpa_home::executionregistry::{Registry, ReleaseGroup};
use cpa_home::{DispatchParams, HomeError};
use cpa_translator::Format;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::home::{HomeAuthDispatcher, HomeDispatchBundle};
use super::*;
use crate::executor::{Executor, meta};

/// Scripted Home: pops one reply per dispatch and records the request keys.
struct FakeHome {
    replies: Mutex<VecDeque<Result<Vec<u8>, HomeError>>>,
    requests: Mutex<Vec<Value>>,
    aborts: AtomicUsize,
    heartbeat: AtomicBool,
}

impl FakeHome {
    fn new(replies: Vec<Result<Vec<u8>, HomeError>>) -> Arc<Self> {
        Arc::new(FakeHome {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
            aborts: AtomicUsize::new(0),
            heartbeat: AtomicBool::new(true),
        })
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().clone()
    }
}

#[async_trait]
impl HomeAuthDispatcher for FakeHome {
    fn heartbeat_ok(&self) -> bool {
        self.heartbeat.load(Ordering::SeqCst)
    }

    async fn rpop_auth(&self, params: &DispatchParams<'_>) -> Result<Vec<u8>, HomeError> {
        let req = cpa_home::client::new_auth_dispatch_request(params);
        self.requests.lock().push(serde_json::to_value(req).unwrap());
        self.replies.lock().pop_front().unwrap_or(Err(HomeError::AuthNotFound))
    }

    fn abort_ambiguous_dispatch(&self) {
        self.aborts.fetch_add(1, Ordering::SeqCst);
    }
}

fn dispatch_reply(auth_id: &str, model: &str, extra: Value) -> Result<Vec<u8>, HomeError> {
    let mut v = json!({
        "model": model,
        "provider": "mock",
        "auth_index": auth_id,
        "auth": {"id": auth_id, "provider": "mock", "status": "active"},
    });
    if let (Value::Object(base), Value::Object(more)) = (&mut v, extra) {
        base.extend(more);
    }
    Ok(serde_json::to_vec(&v).unwrap())
}

fn accounted(auth_id: &str, model: &str) -> Value {
    json!({"concurrency": {"accounted": true, "credential_id": auth_id, "model": model}})
}

fn error_reply(kind: &str, extra: Value) -> Result<Vec<u8>, HomeError> {
    let mut detail = json!({"type": kind, "message": format!("{kind} from home")});
    if let (Value::Object(base), Value::Object(more)) = (&mut detail, extra) {
        base.extend(more);
    }
    Ok(serde_json::to_vec(&json!({"error": detail})).unwrap())
}

#[derive(Clone)]
enum Step {
    Ok(&'static str),
    Err(ExecError),
    Stream(Vec<Result<&'static str, ExecError>>),
}

struct Mock {
    steps: Mutex<HashMap<String, VecDeque<Step>>>,
    /// (auth id, model, home_upstream_model attribute, lifecycle present)
    calls: Mutex<Vec<(String, String, String, bool)>>,
    refreshes: AtomicUsize,
}

impl Mock {
    fn new() -> Arc<Self> {
        Arc::new(Mock { steps: Mutex::new(HashMap::new()), calls: Mutex::new(vec![]), refreshes: AtomicUsize::new(0) })
    }

    fn script(&self, auth_id: &str, steps: Vec<Step>) {
        self.steps.lock().insert(auth_id.into(), steps.into());
    }

    fn next(&self, auth: &Auth, req: &Request, opts: &Options) -> Step {
        self.calls.lock().push((
            auth.id.clone(),
            req.model.clone(),
            auth.attr("home_upstream_model"),
            opts.lifecycle.is_some(),
        ));
        self.steps.lock().get_mut(&auth.id).and_then(VecDeque::pop_front).unwrap_or(Step::Ok("ok"))
    }

    fn call_ids(&self) -> Vec<String> {
        self.calls.lock().iter().map(|c| c.0.clone()).collect()
    }
}

#[async_trait]
impl Executor for Mock {
    fn identifier(&self) -> &str {
        "mock"
    }

    async fn execute(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        match self.next(auth, &req, &opts) {
            Step::Ok(p) => Ok(Response { payload: Bytes::from_static(p.as_bytes()), ..Default::default() }),
            Step::Err(e) => Err(e),
            Step::Stream(_) => panic!("stream step used for execute"),
        }
    }

    async fn execute_stream(&self, auth: &Auth, req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        match self.next(auth, &req, &opts) {
            Step::Err(e) => Err(e),
            Step::Ok(p) => {
                let (tx, rx) = mpsc::channel(4);
                tx.try_send(Ok(Bytes::from_static(p.as_bytes()))).unwrap();
                Ok(StreamResult::new(Default::default(), rx))
            }
            Step::Stream(items) => {
                let (tx, rx) = mpsc::channel(items.len() + 1);
                for item in items {
                    tx.try_send(item.map(|s| Bytes::from_static(s.as_bytes()))).unwrap();
                }
                Ok(StreamResult::new(Default::default(), rx))
            }
        }
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(auth.clone())
    }

    async fn count_tokens(&self, auth: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        self.execute(auth, req, opts).await
    }
}

struct Harness {
    mgr: Manager,
    exec: Arc<Mock>,
    home: Arc<FakeHome>,
    registry: Registry,
    releases: Arc<Mutex<Vec<(ReleaseGroup, i64)>>>,
    bundle: Arc<HomeDispatchBundle>,
}

impl Harness {
    fn new(replies: Vec<Result<Vec<u8>, HomeError>>) -> Self {
        Self::with_config(replies, |_| {})
    }

    fn with_config(replies: Vec<Result<Vec<u8>, HomeError>>, edit: impl FnOnce(&mut Config)) -> Self {
        let model_registry: &'static ModelRegistry = Box::leak(Box::new(ModelRegistry::new()));
        let mgr = Manager::with_parts(Arc::new(SystemClock), model_registry);
        let exec = Mock::new();
        mgr.register_executor(exec.clone());
        let mut cfg = Config::default();
        cfg.home.enabled = true;
        edit(&mut cfg);
        mgr.set_config(Arc::new(cfg));
        let home = FakeHome::new(replies);
        let registry = Registry::new();
        let releases = Arc::new(Mutex::new(Vec::new()));
        let sink = releases.clone();
        registry.set_release_sink(Some(Arc::new(move |g, s| {
            sink.lock().push((g, s));
            None
        })));
        let bundle = mgr.publish_home_dispatch(home.clone(), registry.clone(), 1);
        Harness { mgr, exec, home, registry, releases, bundle }
    }

    async fn run(&self, model: &str) -> Result<Response, ExecError> {
        self.mgr.execute(&["mock".to_string()], request(model), Options::new(Format::OpenAI)).await
    }

    async fn stream(&self, model: &str) -> Result<StreamResult, ExecError> {
        self.mgr.execute_stream(&["mock".to_string()], request(model), Options::new(Format::OpenAI)).await
    }

    fn release_count(&self) -> usize {
        self.releases.lock().len()
    }
}

fn request(model: &str) -> Request {
    Request { model: model.into(), payload: Bytes::from_static(b"{}"), format: Format::OpenAI, metadata: Default::default() }
}

fn status_err(status: u16, body: &str) -> ExecError {
    ExecError::new(status, body)
}

async fn drain(mut s: StreamResult) -> Vec<Result<Bytes, ExecError>> {
    let mut out = vec![];
    while let Some(c) = s.chunks.recv().await {
        out.push(c);
    }
    out
}

async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[tokio::test]
async fn dispatch_runs_the_executor_with_the_home_auth_and_releases_the_scope() {
    let h = Harness::new(vec![dispatch_reply("auth-1", "gpt-5", accounted("auth-1", "gpt-5"))]);
    let resp = h.run("gpt-5").await.unwrap();
    assert_eq!(resp.payload, "ok");
    let calls = h.exec.calls.lock().clone();
    assert_eq!(calls, vec![("auth-1".into(), "gpt-5".into(), "gpt-5".into(), true)]);
    let req = h.home.requests()[0].clone();
    assert_eq!((req["type"].clone(), req["model"].clone(), req["count"].clone()), (json!("auth"), json!("gpt-5"), json!(1)));
    assert_eq!(req["concurrency_protocol"], 1);
    // The accounted execution is released exactly once, cumulatively.
    assert_eq!(h.release_count(), 1);
    let (group, seq) = h.releases.lock()[0].clone();
    assert_eq!((group.credential_id.as_str(), group.model.as_str(), seq), ("auth-1", "gpt-5", 1));
}

#[tokio::test]
async fn unaccounted_dispatches_release_nothing() {
    let h = Harness::new(vec![dispatch_reply("auth-1", "gpt-5", json!({}))]);
    h.run("gpt-5").await.unwrap();
    assert_eq!(h.release_count(), 0);
    assert_eq!(h.mgr.home_execution_registry().unwrap().freeze_in_flight().executions.len(), 0);
}

#[tokio::test]
async fn upstream_model_from_home_overrides_the_route_model() {
    let h = Harness::new(vec![dispatch_reply("a", "claude-real", json!({}))]);
    h.run("alias").await.unwrap();
    let call = h.exec.calls.lock()[0].clone();
    assert_eq!((call.1.as_str(), call.2.as_str()), ("claude-real", "claude-real"));
}

#[tokio::test]
async fn force_mapping_rewrites_the_response_model_back_to_the_alias() {
    let h = Harness::new(vec![dispatch_reply("a", "real-model", json!({"force_mapping": true, "original_alias": "my-alias"}))]);
    h.exec.script("a", vec![Step::Ok(r#"{"model":"real-model","x":1}"#)]);
    let resp = h.run("my-alias").await.unwrap();
    assert_eq!(resp.payload, r#"{"model":"my-alias","x":1}"#);
}

#[tokio::test]
async fn dispatch_error_replies_map_to_statuses() {
    let cases: Vec<(&str, u16)> = vec![
        ("model_not_found", 404),
        ("unauthorized", 401),
        ("user_credits_insufficient", 402),
        ("home_unavailable", 503),
        ("totally_unknown", 502),
    ];
    for (kind, status) in cases {
        let h = Harness::with_config(vec![error_reply(kind, json!({}))], |c| c.request_retry = 0);
        let err = h.run("m").await.unwrap_err();
        assert_eq!((err.status, err.auth_code.as_deref()), (status, Some(kind)), "{kind}");
        assert_eq!(h.exec.call_ids().len(), 0);
    }
}

#[tokio::test]
async fn auth_not_found_and_transport_errors_are_503() {
    let h = Harness::with_config(vec![Err(HomeError::AuthNotFound)], |c| c.request_retry = 0);
    let err = h.run("m").await.unwrap_err();
    assert_eq!((err.status, err.auth_code.as_deref()), (503, Some("auth_not_found")));
    let h = Harness::with_config(vec![Err(HomeError::Io("boom".into()))], |c| c.request_retry = 0);
    let err = h.run("m").await.unwrap_err();
    assert_eq!((err.status, err.auth_code.as_deref(), err.retryable), (503, Some("home_unavailable"), true));
}

#[tokio::test]
async fn ambiguous_dispatch_aborts_the_home_client() {
    let h = Harness::with_config(
        vec![Err(HomeError::AmbiguousDispatch(Box::new(HomeError::Io("eof".into()))))],
        |c| c.request_retry = 0,
    );
    assert!(h.run("m").await.is_err());
    assert_eq!(h.home.aborts.load(Ordering::SeqCst), 1);
    // The registry slot was released: nothing pending.
    assert!(h.registry.wait_pending(Duration::from_millis(50)).await.is_ok());
}

#[tokio::test]
async fn unhealthy_heartbeat_fails_before_any_request() {
    let h = Harness::with_config(vec![dispatch_reply("a", "m", json!({}))], |c| c.request_retry = 0);
    h.home.heartbeat.store(false, Ordering::SeqCst);
    let err = h.run("m").await.unwrap_err();
    assert_eq!((err.status, err.auth_code.as_deref()), (503, Some("home_unavailable")));
    assert!(h.home.requests().is_empty());
}

#[tokio::test]
async fn missing_bundle_is_home_unavailable() {
    let h = Harness::with_config(vec![], |c| c.request_retry = 0);
    h.mgr.clear_home_dispatch_bundle(&h.bundle);
    let err = h.run("m").await.unwrap_err();
    assert_eq!(err.auth_code.as_deref(), Some("home_unavailable"));
}

#[tokio::test]
async fn malformed_concurrency_tuple_aborts_and_is_invalid_home_concurrency() {
    for tuple in [
        json!({"concurrency": {"accounted": true, "credential_id": "", "model": "m"}}),
        json!({"concurrency": {"accounted": false}}),
        json!({"concurrency": {"accounted": true, "credential_id": "a", "model": "Upper"}}),
    ] {
        let h = Harness::with_config(vec![dispatch_reply("a", "m", tuple)], |c| c.request_retry = 0);
        let err = h.run("m").await.unwrap_err();
        assert_eq!(err.auth_code.as_deref(), Some("invalid_home_concurrency"));
        assert_eq!(h.home.aborts.load(Ordering::SeqCst), 1);
        assert!(h.exec.call_ids().is_empty());
        assert_eq!(h.release_count(), 0);
    }
}

#[tokio::test]
async fn concurrency_identity_must_match_the_dispatched_auth() {
    // auth_index differs from the accounted credential id.
    let reply = dispatch_reply("a", "m", json!({"auth_index": "other", "concurrency": {"accounted": true, "credential_id": "a", "model": "m"}}));
    let h = Harness::with_config(vec![reply], |c| c.request_retry = 0);
    let err = h.run("m").await.unwrap_err();
    assert_eq!(err.auth_code.as_deref(), Some("invalid_home_concurrency"));
    // The accounted scope was ended, releasing the slot.
    assert_eq!(h.release_count(), 1);
}

#[tokio::test]
async fn concurrency_model_must_match_the_dispatched_model() {
    let reply = dispatch_reply("a", "gpt-5", accounted("a", "other-model"));
    let h = Harness::with_config(vec![reply], |c| c.request_retry = 0);
    assert_eq!(h.run("gpt-5").await.unwrap_err().auth_code.as_deref(), Some("invalid_home_concurrency"));
}

#[tokio::test]
async fn error_together_with_a_concurrency_tuple_is_invalid() {
    let mut payload: Value = serde_json::from_slice(&error_reply("unauthorized", json!({})).unwrap()).unwrap();
    payload["concurrency"] = json!({"accounted": true, "credential_id": "a", "model": "m"});
    let h = Harness::with_config(vec![Ok(serde_json::to_vec(&payload).unwrap())], |c| c.request_retry = 0);
    assert_eq!(h.run("m").await.unwrap_err().auth_code.as_deref(), Some("invalid_home_concurrency"));
    assert_eq!(h.home.aborts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn invalid_payloads_and_unregistered_executors_are_502() {
    let h = Harness::with_config(vec![Ok(b"not json".to_vec())], |c| c.request_retry = 0);
    assert_eq!(h.run("m").await.unwrap_err().auth_code.as_deref(), Some("invalid_auth"));
    let reply = Ok(serde_json::to_vec(&json!({"auth": {"id": "a", "provider": "missing-provider"}})).unwrap());
    let h = Harness::with_config(vec![reply], |c| c.request_retry = 0);
    let err = h.run("m").await.unwrap_err();
    assert_eq!((err.status, err.auth_code.as_deref()), (502, Some("executor_not_found")));
    let reply = Ok(serde_json::to_vec(&json!({"auth": {"id": "", "provider": "mock"}})).unwrap());
    let h = Harness::with_config(vec![reply], |c| c.request_retry = 0);
    assert_eq!(h.run("m").await.unwrap_err().auth_code.as_deref(), Some("invalid_auth"));
}

#[tokio::test]
async fn failing_credential_fails_over_to_the_next_dispatch_with_exclusions() {
    let h = Harness::new(vec![
        dispatch_reply("a", "m", accounted("a", "m")),
        dispatch_reply("b", "m", accounted("b", "m")),
    ]);
    h.exec.script("a", vec![Step::Err(status_err(500, "boom"))]);
    let resp = h.run("m").await.unwrap();
    assert_eq!(resp.payload, "ok");
    assert_eq!(h.exec.call_ids(), ["a", "b"]);
    let reqs = h.home.requests();
    assert_eq!(reqs[0].get("excluded_auth_ids"), None);
    assert_eq!((reqs[1]["excluded_auth_ids"].clone(), reqs[1]["count"].clone()), (json!(["a"]), json!(1)));
    // Both scopes released: the failed attempt before redispatch, the successful one at the end.
    assert_eq!(h.release_count(), 2);
}

#[tokio::test]
async fn exhausted_round_returns_the_upstream_error_with_round_marker_and_retries_with_round_number() {
    let h = Harness::with_config(
        vec![
            dispatch_reply("a", "m", json!({})),
            error_reply("auth_unavailable", json!({})),
            dispatch_reply("a2", "m", json!({})),
        ],
        |c| c.request_retry = 1,
    );
    h.exec.script("a", vec![Step::Err(status_err(500, "upstream boom"))]);
    let resp = h.run("m").await.unwrap();
    assert_eq!(resp.payload, "ok");
    let reqs = h.home.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(reqs[2]["retry_round"], 1);
}

#[tokio::test]
async fn round_exhaustion_without_retries_surfaces_the_last_upstream_error() {
    let h = Harness::with_config(
        vec![dispatch_reply("a", "m", json!({})), error_reply("auth_unavailable", json!({}))],
        |c| c.request_retry = 0,
    );
    h.exec.script("a", vec![Step::Err(status_err(500, "upstream boom"))]);
    let err = h.run("m").await.unwrap_err();
    assert_eq!(err.status, 500);
    assert!(err.message.contains("upstream boom"));
}

#[tokio::test]
async fn model_cooldown_waits_for_the_hint_then_replays_the_round() {
    let h = Harness::with_config(
        vec![
            dispatch_reply("a", "m", json!({})),
            error_reply("model_cooldown", json!({"retry_after_ms": 20, "request_retry": 2})),
            dispatch_reply("a", "m", json!({})),
        ],
        |c| {
            c.request_retry = 3;
            c.max_retry_interval = 5;
        },
    );
    h.exec.script("a", vec![Step::Err(status_err(429, "limited")), Step::Ok("second")]);
    let started = std::time::Instant::now();
    let resp = h.run("m").await.unwrap();
    assert_eq!(resp.payload, "second");
    assert!(started.elapsed() >= Duration::from_millis(15));
    assert_eq!(h.home.requests().len(), 3);
}

#[tokio::test]
async fn concurrency_busy_is_not_retried_and_relays_retry_after() {
    let h = Harness::with_config(
        vec![error_reply("credential_concurrency_exceeded", json!({"retry_after_ms": 1500}))],
        |c| c.request_retry = 3,
    );
    let err = h.run("m").await.unwrap_err();
    assert_eq!(err.status, 429);
    assert_eq!(err.home, Some(crate::executor::HomeErrKind::ConcurrencyBusy));
    assert_eq!(err.retry_after, Some(Duration::from_millis(1500)));
    assert_eq!(h.home.requests().len(), 1);
    assert_eq!(super::errors::safe_response_headers(&err).get("retry-after").unwrap(), "2");
}

#[tokio::test]
async fn invalid_requests_end_the_call_without_rotating() {
    let h = Harness::new(vec![dispatch_reply("a", "m", json!({})), dispatch_reply("b", "m", json!({}))]);
    h.exec.script("a", vec![Step::Err(status_err(400, r#"{"error":{"message":"bad request"}}"#))]);
    let err = h.run("m").await.unwrap_err();
    assert_eq!(err.status, 400);
    assert_eq!(h.exec.call_ids(), ["a"]);
    assert_eq!(h.home.requests().len(), 1);
}

#[tokio::test]
async fn pinned_credential_mismatch_is_rejected() {
    let h = Harness::with_config(vec![dispatch_reply("b", "m", json!({}))], |c| c.request_retry = 0);
    let mut opts = Options::new(Format::OpenAI);
    opts.metadata.insert(meta::PINNED_AUTH_ID.into(), json!("a"));
    let err = h.mgr.execute(&["mock".to_string()], request("m"), opts).await.unwrap_err();
    assert_eq!(err.auth_code.as_deref(), Some("auth_not_found"));
    assert_eq!(h.home.requests()[0]["pinned_auth_id"], "a");
}

#[tokio::test]
async fn usage_is_recorded_for_home_dispatched_attempts() {
    let h = Harness::new(vec![dispatch_reply("a", "m", json!({}))]);
    let tracker = Arc::new(crate::usage::UsageTracker::new());
    h.mgr.set_usage_tracker(Some(tracker.clone()));
    h.run("m").await.unwrap();
    let page = tracker.requests(10, None);
    assert_eq!(page.events.len(), 1);
    assert_eq!((page.events[0].record.auth_index.as_str(), page.events[0].record.failed), ("a", false));
}

#[tokio::test]
async fn cancelling_a_request_mid_execution_releases_the_scope() {
    struct Hang;
    #[async_trait]
    impl Executor for Hang {
        fn identifier(&self) -> &str {
            "mock"
        }
        async fn execute(&self, _: &Auth, _: Request, _: Options) -> Result<Response, ExecError> {
            std::future::pending().await
        }
        async fn execute_stream(&self, _: &Auth, _: Request, _: Options) -> Result<StreamResult, ExecError> {
            std::future::pending().await
        }
        async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
            Ok(auth.clone())
        }
        async fn count_tokens(&self, _: &Auth, _: Request, _: Options) -> Result<Response, ExecError> {
            std::future::pending().await
        }
    }
    let h = Harness::new(vec![dispatch_reply("a", "m", accounted("a", "m"))]);
    h.mgr.register_executor(Arc::new(Hang));
    let mgr = h.mgr.clone();
    let task = tokio::spawn(async move { mgr.execute(&["mock".to_string()], request("m"), Options::new(Format::OpenAI)).await });
    for _ in 0..100 {
        if !h.registry.freeze_in_flight().executions.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(h.registry.freeze_in_flight().executions.len(), 1);
    task.abort();
    let _ = task.await;
    settle().await;
    assert_eq!(h.release_count(), 1);
    assert!(h.registry.freeze_in_flight().executions.is_empty());
}

#[tokio::test]
async fn draining_the_registry_cancels_a_running_attempt() {
    struct Hang;
    #[async_trait]
    impl Executor for Hang {
        fn identifier(&self) -> &str {
            "mock"
        }
        async fn execute(&self, _: &Auth, _: Request, _: Options) -> Result<Response, ExecError> {
            std::future::pending().await
        }
        async fn execute_stream(&self, _: &Auth, _: Request, _: Options) -> Result<StreamResult, ExecError> {
            std::future::pending().await
        }
        async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
            Ok(auth.clone())
        }
        async fn count_tokens(&self, _: &Auth, _: Request, _: Options) -> Result<Response, ExecError> {
            std::future::pending().await
        }
    }
    let h = Harness::with_config(vec![dispatch_reply("a", "m", accounted("a", "m"))], |c| c.request_retry = 0);
    h.mgr.register_executor(Arc::new(Hang));
    let mgr = h.mgr.clone();
    let task = tokio::spawn(async move { mgr.execute(&["mock".to_string()], request("m"), Options::new(Format::OpenAI)).await });
    for _ in 0..100 {
        if !h.registry.freeze_in_flight().executions.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let drain = h.registry.drain(Duration::from_secs(2)).await;
    assert!(drain.is_ok(), "{drain:?}");
    // The cancelled attempt fails over to a new dispatch, which the draining registry refuses
    // (Go: the attempt context cancelling is not the request context cancelling).
    let err = task.await.unwrap().unwrap_err();
    assert_eq!(err.auth_code.as_deref(), Some("home_unavailable"));
    assert_eq!(h.release_count(), 1);
}

// ---- streaming ----

#[tokio::test]
async fn stream_forwards_chunks_and_ends_the_selection_when_closed() {
    let h = Harness::new(vec![dispatch_reply("a", "m", accounted("a", "m"))]);
    h.exec.script("a", vec![Step::Stream(vec![Ok("one"), Ok("two")])]);
    let stream = h.stream("m").await.unwrap();
    let chunks: Vec<String> = drain(stream).await.into_iter().map(|c| String::from_utf8(c.unwrap().to_vec()).unwrap()).collect();
    assert_eq!(chunks, ["one", "two"]);
    settle().await;
    assert_eq!(h.release_count(), 1);
    assert!(h.exec.calls.lock()[0].3, "the executor sees the selection as its lifecycle");
}

#[tokio::test]
async fn stream_bootstrap_failure_fails_over_and_excludes_the_failed_credential() {
    let h = Harness::new(vec![dispatch_reply("a", "m", accounted("a", "m")), dispatch_reply("b", "m", accounted("b", "m"))]);
    h.exec.script("a", vec![Step::Err(status_err(500, "down"))]);
    h.exec.script("b", vec![Step::Stream(vec![Ok("from-b")])]);
    let stream = h.stream("m").await.unwrap();
    assert_eq!(drain(stream).await.len(), 1);
    assert_eq!(h.exec.call_ids(), ["a", "b"]);
    assert_eq!(h.home.requests()[1]["excluded_auth_ids"], json!(["a"]));
    settle().await;
    assert_eq!(h.release_count(), 2);
}

#[tokio::test]
async fn stream_error_chunk_is_forwarded_and_the_remainder_discarded() {
    let h = Harness::new(vec![dispatch_reply("a", "m", json!({}))]);
    h.exec.script("a", vec![Step::Stream(vec![Ok("one"), Err(status_err(502, "cut")), Ok("late")])]);
    let stream = h.stream("m").await.unwrap();
    let items = drain(stream).await;
    assert_eq!(items.len(), 2);
    assert!(items[1].is_err());
}

#[tokio::test]
async fn stream_total_failure_returns_the_error_stream_with_headers() {
    let h = Harness::with_config(
        vec![dispatch_reply("a", "m", json!({})), error_reply("auth_unavailable", json!({}))],
        |c| c.request_retry = 0,
    );
    h.exec.script("a", vec![Step::Err(status_err(500, "down"))]);
    let err = h.stream("m").await.map(|_| ()).unwrap_err();
    assert_eq!(err.status, 500);
}

#[tokio::test]
async fn dropping_a_stream_releases_the_selection() {
    let h = Harness::new(vec![dispatch_reply("a", "m", accounted("a", "m"))]);
    h.exec.script("a", vec![Step::Stream(vec![Ok("one")])]);
    let stream = h.stream("m").await.unwrap();
    drop(stream);
    settle().await;
    assert_eq!(h.release_count(), 1);
}

// ---- websocket session retention ----

fn ws_opts(session: &str) -> Options {
    let mut o = Options::new(Format::OpenAIResponse);
    o.metadata.insert(meta::EXECUTION_SESSION_ID.into(), json!(session));
    o.metadata.insert("downstream_websocket".into(), json!(true));
    o
}

#[tokio::test]
async fn closing_an_execution_session_ends_retained_selections() {
    let h = Harness::new(vec![dispatch_reply("a", "m", accounted("a", "m"))]);
    // Simulate an executor retaining the selection through the lifecycle handle.
    struct Retaining;
    #[async_trait]
    impl Executor for Retaining {
        fn identifier(&self) -> &str {
            "mock"
        }
        async fn execute(&self, _: &Auth, _: Request, opts: Options) -> Result<Response, ExecError> {
            if let Some(l) = &opts.lifecycle {
                l.retain();
            }
            Ok(Response { payload: Bytes::from_static(b"ok"), ..Default::default() })
        }
        async fn execute_stream(&self, _: &Auth, _: Request, _: Options) -> Result<StreamResult, ExecError> {
            unreachable!()
        }
        async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
            Ok(auth.clone())
        }
        async fn count_tokens(&self, _: &Auth, _: Request, _: Options) -> Result<Response, ExecError> {
            unreachable!()
        }
    }
    h.mgr.register_executor(Arc::new(Retaining));
    h.mgr.execute(&["mock".to_string()], request("m"), ws_opts("sess-1")).await.unwrap();
    // Retained: the scope is still held after the call returned.
    assert_eq!(h.release_count(), 0);
    assert_eq!(h.registry.freeze_in_flight().executions.len(), 1);
    h.mgr.close_execution_session("sess-1").await;
    assert_eq!(h.release_count(), 1);
    assert!(h.registry.freeze_in_flight().executions.is_empty());
}

#[tokio::test]
async fn retained_selection_is_reused_for_the_next_request_of_the_session() {
    struct Retaining;
    #[async_trait]
    impl Executor for Retaining {
        fn identifier(&self) -> &str {
            "mock"
        }
        async fn execute(&self, _: &Auth, _: Request, opts: Options) -> Result<Response, ExecError> {
            if let Some(l) = &opts.lifecycle {
                l.retain();
            }
            Ok(Response { payload: Bytes::from_static(b"ok"), ..Default::default() })
        }
        async fn execute_stream(&self, _: &Auth, _: Request, _: Options) -> Result<StreamResult, ExecError> {
            unreachable!()
        }
        async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
            Ok(auth.clone())
        }
        async fn count_tokens(&self, _: &Auth, _: Request, _: Options) -> Result<Response, ExecError> {
            unreachable!()
        }
    }
    let mut ws_auth = json!({"id": "a", "provider": "mock", "attributes": {"websockets": "true"}});
    ws_auth["status"] = json!("active");
    let reply = Ok(serde_json::to_vec(&json!({"model": "m", "auth_index": "a", "auth": ws_auth, "concurrency": {"accounted": true, "credential_id": "a", "model": "m"}})).unwrap());
    let h = Harness::new(vec![reply]);
    h.mgr.register_executor(Arc::new(Retaining));
    h.mgr.execute(&["mock".to_string()], request("m"), ws_opts("sess-2")).await.unwrap();
    h.mgr.execute(&["mock".to_string()], request("m"), ws_opts("sess-2")).await.unwrap();
    // One dispatch served both requests; the session auth is visible to later lookups.
    assert_eq!(h.home.requests().len(), 1);
    assert!(h.mgr.get_execution_session_auth_by_id("sess-2", "a").is_some());
    h.mgr.close_execution_session("sess-2").await;
    assert!(h.mgr.get_execution_session_auth_by_id("sess-2", "a").is_none());
    assert_eq!(h.release_count(), 1);
}

#[tokio::test]
async fn session_ids_and_headers_are_sent_to_home() {
    let h = Harness::new(vec![dispatch_reply("a", "m", json!({}))]);
    let mut opts = Options::new(Format::OpenAI);
    opts.headers.insert("session_id", "sess-header-1".parse().unwrap());
    opts.headers.insert("x-custom", "v".parse().unwrap());
    opts.metadata.insert("node_kind".into(), json!("fork"));
    h.mgr.execute(&["mock".to_string()], request("m"), opts).await.unwrap();
    let req = h.home.requests()[0].clone();
    assert_eq!(req["headers"]["x-custom"], "v");
    assert_eq!(req["node_kind"], "fork");
    assert!(req["session_id"].as_str().is_some_and(|s| !s.is_empty()), "{req}");
}

#[tokio::test]
async fn go_dispatch_fixtures_decode() {
    let accounted = include_str!("../../tests/fixtures/home/concurrency_dispatch_accounted.json").replace("codex", "mock");
    let h = Harness::new(vec![Ok(accounted.into_bytes())]);
    h.run("gpt").await.unwrap();
    assert_eq!(h.exec.call_ids(), ["cred-1"]);
    assert_eq!(h.release_count(), 1);

    let busy = include_str!("../../tests/fixtures/home/concurrency_dispatch_busy.json");
    let h = Harness::with_config(vec![Ok(busy.as_bytes().to_vec())], |c| c.request_retry = 0);
    let err = h.run("gpt").await.unwrap_err();
    assert_eq!((err.status, err.retry_after), (429, Some(Duration::from_millis(750))));
    assert_eq!(err.home, Some(crate::executor::HomeErrKind::ConcurrencyBusy));
}

#[tokio::test]
async fn query_credentials_are_forwarded_as_goog_api_key() {
    let h = Harness::new(vec![dispatch_reply("a", "m", json!({}))]);
    let mut opts = Options::new(Format::Gemini);
    opts.query = vec![("key".into(), " query-key ".into())];
    h.mgr.execute(&["mock".to_string()], request("m"), opts).await.unwrap();
    assert_eq!(h.home.requests()[0]["headers"]["x-goog-api-key"], "query-key");
    let h = Harness::new(vec![dispatch_reply("a", "m", json!({}))]);
    let mut opts = Options::new(Format::Gemini);
    opts.query = vec![("key".into(), "query-key".into())];
    opts.headers.insert("authorization", "Bearer real".parse().unwrap());
    h.mgr.execute(&["mock".to_string()], request("m"), opts).await.unwrap();
    assert!(h.home.requests()[0]["headers"].get("x-goog-api-key").is_none());
}

fn repeated(auth_id: &str, n: usize) -> Vec<Result<Vec<u8>, HomeError>> {
    (0..n).map(|_| dispatch_reply(auth_id, "m", json!({}))).collect()
}

// Go: TestHomeUnauthorized* (execute, count tokens, stream variants): a 401 is returned as is,
// the credential is never refreshed and the request is never replayed.
#[tokio::test]
async fn home_unauthorized_returns_the_original_error_without_refresh() {
    let h = Harness::new(repeated("a", 4));
    h.exec.script("a", vec![Step::Err(status_err(401, "access token expired")); 4]);
    let err = h.run("m").await.unwrap_err();
    assert_eq!((err.status, err.message.as_str()), (401, "access token expired"));
    assert_eq!(h.exec.call_ids(), ["a"]);
    assert_eq!(h.exec.refreshes.load(Ordering::SeqCst), 0);

    let h = Harness::new(repeated("a", 4));
    h.exec.script("a", vec![Step::Err(status_err(401, "access token expired")); 4]);
    let err = h.mgr.execute_count(&["mock".to_string()], request("m"), Options::new(Format::OpenAI)).await.unwrap_err();
    assert_eq!(err.status, 401);
    assert_eq!(h.exec.call_ids(), ["a"]);
    assert_eq!(h.exec.refreshes.load(Ordering::SeqCst), 0);
}

// Go: TestManagerExecuteHomeStopsWhenDispatchRepeatsTriedAuth.
#[tokio::test]
async fn home_stops_when_dispatch_repeats_a_tried_auth() {
    let h = Harness::new(repeated("a", 8));
    h.exec.script("a", vec![Step::Err(status_err(401, "missing access token")); 8]);
    let err = tokio::time::timeout(Duration::from_secs(1), h.run("m")).await.unwrap().unwrap_err();
    assert_eq!(err.status, 401);
    assert_eq!(h.exec.call_ids().len(), 1);
    assert_eq!(h.home.requests().len(), 2);
}

#[tokio::test]
async fn home_unauthorized_streams_are_not_refreshed_or_replayed() {
    // Synchronous failure.
    let h = Harness::new(repeated("a", 4));
    h.exec.script("a", vec![Step::Err(status_err(401, "access token expired")); 4]);
    let err = h.stream("m").await.err().expect("stream error");
    assert_eq!((err.status, err.message.as_str()), (401, "access token expired"));
    assert_eq!(h.exec.call_ids().len(), 1);
    assert_eq!(h.exec.refreshes.load(Ordering::SeqCst), 0);

    // 401 as the first chunk (bootstrap failure).
    let h = Harness::new(repeated("a", 4));
    h.exec.script("a", vec![Step::Stream(vec![Err(status_err(401, "access token expired"))]); 4]);
    let chunks = match h.stream("m").await {
        Ok(s) => drain(s).await,
        Err(e) => vec![Err(e)],
    };
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].as_ref().err().map(|e| e.status), Some(401));
    assert_eq!(h.exec.call_ids().len(), 1);
    assert_eq!(h.exec.refreshes.load(Ordering::SeqCst), 0);

    // 401 after the stream started: payload and error both surface, no replay.
    let h = Harness::new(repeated("a", 4));
    h.exec.script("a", vec![Step::Stream(vec![Ok("started"), Err(status_err(401, "access token expired"))]); 4]);
    let chunks = drain(h.stream("m").await.unwrap()).await;
    assert!(chunks.iter().any(|c| matches!(c, Ok(b) if &b[..] == b"started")));
    assert!(chunks.iter().any(|c| c.as_ref().err().is_some_and(|e| e.status == 401)));
    assert_eq!(h.exec.call_ids().len(), 1);
    assert_eq!(h.exec.refreshes.load(Ordering::SeqCst), 0);
}

// ---- retained routes, alias changes and release acknowledgement (Go: home_force_mapping_test.go) ----

type ReplyFn = Box<dyn Fn(&str, usize) -> Value + Send + Sync>;

/// Dispatcher answering through a closure `(model, call number) -> reply`.
struct DynHome {
    reply: ReplyFn,
    models: Mutex<Vec<String>>,
    /// Observed by the closure of tests that check ordering against release acknowledgements.
    on_call: Box<dyn Fn(usize) + Send + Sync>,
}

impl DynHome {
    fn new(reply: impl Fn(&str, usize) -> Value + Send + Sync + 'static) -> Arc<Self> {
        Self::with_hook(reply, |_| {})
    }

    fn with_hook(reply: impl Fn(&str, usize) -> Value + Send + Sync + 'static, on_call: impl Fn(usize) + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(DynHome { reply: Box::new(reply), models: Mutex::new(vec![]), on_call: Box::new(on_call) })
    }

    fn models(&self) -> Vec<String> {
        self.models.lock().clone()
    }
}

#[async_trait]
impl HomeAuthDispatcher for DynHome {
    fn heartbeat_ok(&self) -> bool {
        true
    }

    async fn rpop_auth(&self, params: &DispatchParams<'_>) -> Result<Vec<u8>, HomeError> {
        let call = {
            let mut models = self.models.lock();
            models.push(params.model.to_string());
            models.len()
        };
        (self.on_call)(call);
        Ok(serde_json::to_vec(&(self.reply)(params.model, call)).unwrap())
    }

    fn abort_ambiguous_dispatch(&self) {}
}

/// Retaining executor: echoes the model it was called with and records it.
struct Echo {
    id: &'static str,
    models: Mutex<Vec<String>>,
}

impl Echo {
    fn new(id: &'static str) -> Arc<Self> {
        Arc::new(Echo { id, models: Mutex::new(vec![]) })
    }

    fn models(&self) -> Vec<String> {
        self.models.lock().clone()
    }
}

#[async_trait]
impl Executor for Echo {
    fn identifier(&self) -> &str {
        self.id
    }

    async fn execute(&self, _: &Auth, req: Request, opts: Options) -> Result<Response, ExecError> {
        self.models.lock().push(req.model.clone());
        if let Some(l) = &opts.lifecycle {
            l.retain();
        }
        Ok(Response { payload: Bytes::from(serde_json::to_vec(&json!({"model": req.model})).unwrap()), ..Default::default() })
    }

    async fn execute_stream(&self, _: &Auth, _: Request, _: Options) -> Result<StreamResult, ExecError> {
        Err(ExecError::new(500, "unused"))
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        Ok(auth.clone())
    }

    async fn count_tokens(&self, _: &Auth, _: Request, _: Options) -> Result<Response, ExecError> {
        Err(ExecError::new(500, "unused"))
    }
}

fn dyn_manager(home: Arc<DynHome>, registry: Registry, edit: impl FnOnce(&mut Config)) -> Manager {
    let model_registry: &'static ModelRegistry = Box::leak(Box::new(ModelRegistry::new()));
    let mgr = Manager::with_parts(Arc::new(SystemClock), model_registry);
    let mut cfg = Config::default();
    cfg.home.enabled = true;
    edit(&mut cfg);
    mgr.set_config(Arc::new(cfg));
    mgr.publish_home_dispatch(home, registry, 1);
    mgr
}

fn session_opts(session: &str, pinned: &str) -> Options {
    let mut o = Options::new(Format::OpenAIResponse);
    o.metadata.insert(meta::EXECUTION_SESSION_ID.into(), json!(session));
    o.metadata.insert(meta::PINNED_AUTH_ID.into(), json!(pinned));
    o.metadata.insert("downstream_websocket".into(), json!(true));
    o
}

fn ws_auth(id: &str, provider: &str, extra_attrs: Value) -> Value {
    let mut attrs = json!({"websockets": "true"});
    if let (Value::Object(base), Value::Object(more)) = (&mut attrs, extra_attrs) {
        base.extend(more);
    }
    json!({"id": id, "provider": provider, "status": "active", "attributes": attrs})
}

async fn run_on(mgr: &Manager, provider: &str, model: &str, opts: Options) -> Result<Response, ExecError> {
    mgr.execute(&[provider.to_string()], request(model), opts).await
}

// Go: TestHomeForceMappingAliasResult*.
#[test]
fn force_mapping_alias_result_requires_flag_and_matching_alias() {
    use super::home_selection::{HOME_FORCE_MAPPING_ATTRIBUTE, HOME_ORIGINAL_ALIAS_ATTRIBUTE, HOME_UPSTREAM_MODEL_ATTRIBUTE};
    let mut auth = Auth::default();
    auth.provider = "xai".into();
    auth.attributes.insert(HOME_UPSTREAM_MODEL_ATTRIBUTE.into(), "grok-4.5".into());
    auth.attributes.insert(HOME_ORIGINAL_ALIAS_ATTRIBUTE.into(), "grok-latest".into());
    let none = super::routing::home_force_mapping_alias_result(&auth, "grok-latest");
    assert!(!none.force_mapping && none.original_alias.is_empty());

    auth.attributes.insert(HOME_FORCE_MAPPING_ATTRIBUTE.into(), "true".into());
    let r = super::routing::home_force_mapping_alias_result(&auth, "grok-latest");
    assert_eq!((r.upstream_model.as_str(), r.force_mapping, r.original_alias.as_str()), ("grok-4.5", true, "grok-latest"));
    assert!(super::routing::home_force_mapping_alias_result(&auth, " GROK-LATEST ").force_mapping);
    assert!(super::routing::home_force_mapping_alias_result(&auth, "grok-latest(high)").force_mapping);
    for other in ["grok-latest(custom)", "grok-other"] {
        let r = super::routing::home_force_mapping_alias_result(&auth, other);
        assert!(!r.force_mapping && r.original_alias.is_empty(), "{other}");
    }
}

// Go: TestHomeAuthSelectionRouteRetainsRequestedResponseAliasAcrossWebsocketReuse.
#[tokio::test]
async fn auth_selection_route_keeps_the_requested_response_alias_across_websocket_reuse() {
    let home = DynHome::new(|_, _| {
        json!({
            "model": "target-model", "force_mapping": true, "original_alias": "route-model",
            "auth_index": "route-auth", "auth": ws_auth("route-auth", "force-mapping", json!({})),
            "concurrency": {"accounted": true, "credential_id": "route-auth", "model": "target-model"},
        })
    });
    let mgr = dyn_manager(home.clone(), Registry::new(), |_| {});
    mgr.register_executor(Echo::new("force-mapping"));
    for _ in 0..2 {
        let mut opts = session_opts("auth-selection-route", "route-auth");
        opts.metadata.insert(meta::AUTH_SELECTION_MODEL.into(), json!("route-model"));
        opts.metadata.insert(meta::REQUESTED_MODEL.into(), json!("client-alias"));
        let resp = run_on(&mgr, "force-mapping", "execution-model", opts).await.unwrap();
        assert_eq!(resp.payload, r#"{"model":"client-alias"}"#);
    }
    assert_eq!(home.models(), ["route-model"]);
    mgr.close_execution_session("auth-selection-route").await;
}

/// Counts of releases seen by the registry sink.
fn counting_registry() -> (Registry, Arc<Mutex<Vec<ReleaseGroup>>>) {
    let registry = Registry::new();
    let groups = Arc::new(Mutex::new(Vec::new()));
    let sink = groups.clone();
    registry.set_release_sink(Some(Arc::new(move |g, _| {
        sink.lock().push(g);
        None
    })));
    (registry, groups)
}

// Go: TestHomeForceMappingAliasChangeEndsAndFlushesBeforeRedispatch.
#[tokio::test]
async fn force_mapping_alias_change_releases_before_the_second_dispatch() {
    let (registry, groups) = counting_registry();
    let seen = groups.clone();
    let released_before_second = Arc::new(AtomicBool::new(false));
    let flag = released_before_second.clone();
    let home = DynHome::with_hook(
        |_, _| {
            json!({
                "model": "upstream-a", "auth_index": "fm-auth",
                "auth": ws_auth("fm-auth", "force-mapping", json!({"home_force_mapping": "true", "home_original_alias": "alias-a"})),
                "concurrency": {"accounted": true, "credential_id": "fm-auth", "model": "upstream-a"},
            })
        },
        move |call| {
            if call == 2 {
                flag.store(seen.lock().len() == 1, Ordering::SeqCst);
            }
        },
    );
    let mgr = dyn_manager(home.clone(), registry, |_| {});
    mgr.register_executor(Echo::new("force-mapping"));
    for model in ["alias-a", "alias-b"] {
        run_on(&mgr, "force-mapping", model, session_opts("fm-alias-change", "fm-auth")).await.unwrap();
    }
    assert_eq!(home.models().len(), 2);
    assert!(released_before_second.load(Ordering::SeqCst), "previous selection must be released before the second dispatch");
    mgr.close_execution_session("fm-alias-change").await;
}

// Go: TestHomeNonForceAliasSessionReuseAndTargetChangeReleasesAccountedModel.
#[tokio::test]
async fn non_force_alias_reuses_the_session_and_releases_the_accounted_model_on_target_change() {
    let (registry, groups) = counting_registry();
    let seen = groups.clone();
    let released_before_second = Arc::new(AtomicBool::new(false));
    let flag = released_before_second.clone();
    let home = DynHome::with_hook(
        |model, _| {
            let target = if super::home_concurrency::canonical_concurrency_model_key(model) == "alias-b" { "target-b" } else { "target-a" };
            json!({
                "model": target, "auth_index": "nf-auth",
                "auth": ws_auth("nf-auth", "force-mapping", json!({})),
                "concurrency": {"accounted": true, "credential_id": "nf-auth", "model": target},
            })
        },
        move |call| {
            if call == 2 {
                flag.store(seen.lock().len() == 1, Ordering::SeqCst);
            }
        },
    );
    let mgr = dyn_manager(home.clone(), registry, |_| {});
    mgr.register_executor(Echo::new("force-mapping"));
    for model in ["alias-a(high)", "alias-a", "alias-b"] {
        run_on(&mgr, "force-mapping", model, session_opts("nf-session", "nf-auth")).await.unwrap();
    }
    assert_eq!(home.models().len(), 2, "same-route reuse then target change");
    assert!(released_before_second.load(Ordering::SeqCst));
    mgr.close_execution_session("nf-session").await;
    let want = [
        ReleaseGroup { credential_id: "nf-auth".into(), model: "target-a".into() },
        ReleaseGroup { credential_id: "nf-auth".into(), model: "target-b".into() },
    ];
    assert_eq!(*groups.lock(), want);
}

// Go: TestHomeRetainedPrefixedRouteRewritesSuffixAndResponse.
#[tokio::test]
async fn retained_prefixed_route_rewrites_suffix_and_response() {
    let home = DynHome::new(|_, _| {
        let mut auth = ws_auth("pfx-auth", "prefixed-retained-route", json!({}));
        auth["prefix"] = json!("team");
        json!({
            "model": "target-a", "force_mapping": true, "original_alias": "alias-a",
            "auth_index": "pfx-auth", "auth": auth,
            "concurrency": {"accounted": true, "credential_id": "pfx-auth", "model": "target-a"},
        })
    });
    let mgr = dyn_manager(home.clone(), Registry::new(), |_| {});
    let echo = Echo::new("prefixed-retained-route");
    mgr.register_executor(echo.clone());
    for model in ["team/alias-a", "team/alias-a(high)"] {
        let resp = run_on(&mgr, "prefixed-retained-route", model, session_opts("pfx-session", "pfx-auth")).await.unwrap();
        assert_eq!(resp.payload, format!(r#"{{"model":"{model}"}}"#).as_str());
    }
    assert_eq!(home.models(), ["team/alias-a"]);
    assert_eq!(echo.models(), ["target-a", "target-a(high)"]);
    mgr.close_execution_session("pfx-session").await;
}

fn ack_route_reply(model: &str) -> Value {
    let target = if super::home_concurrency::canonical_concurrency_model_key(model) == "alias-a" { "target-a" } else { "target-custom" };
    json!({
        "model": target, "force_mapping": true, "original_alias": model,
        "auth_index": "retained-route-auth", "auth": ws_auth("retained-route-auth", "retained-route", json!({})),
        "concurrency": {"accounted": true, "credential_id": "retained-route-auth", "model": target},
    })
}

// Go: TestHomeRetainedRouteRewritesReasoningSuffixAndWaitsForReleaseACK.
#[tokio::test]
async fn retained_route_rewrites_reasoning_suffix_and_waits_for_the_release_ack() {
    use cpa_home::concurrency_release::ReleaseFlusher;
    let registry = Registry::new();
    let flusher = ReleaseFlusher::with_timings(Duration::from_millis(1), Duration::from_millis(10));
    let acks = Arc::new(AtomicUsize::new(0));
    let counter = acks.clone();
    flusher.set_sender(Some(Arc::new(move |_frame| {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    })));
    let f = flusher.clone();
    registry.set_release_sink(Some(Arc::new(move |g, s| f.mark_dirty(g, s))));
    let cancel: cpa_home::client::Cancel = Arc::new(Default::default());
    let (run_flusher, run_cancel) = (flusher.clone(), cancel.clone());
    let task = tokio::spawn(async move { run_flusher.run(&run_cancel).await });

    let acked_before_second = Arc::new(AtomicBool::new(false));
    let (flag, seen) = (acked_before_second.clone(), acks.clone());
    let home = DynHome::with_hook(
        |model, _| ack_route_reply(model),
        move |call| {
            if call == 2 {
                flag.store(seen.load(Ordering::SeqCst) == 1, Ordering::SeqCst);
            }
        },
    );
    let mgr = dyn_manager(home.clone(), registry, |_| {});
    let echo = Echo::new("retained-route");
    mgr.register_executor(echo.clone());
    for model in ["alias-a", "alias-a(high)", "alias-a", "alias-a(custom)"] {
        let resp = run_on(&mgr, "retained-route", model, session_opts("retained-route-ack", "retained-route-auth")).await.unwrap();
        assert_eq!(resp.payload, format!(r#"{{"model":"{model}"}}"#).as_str());
    }
    assert_eq!(home.models().len(), 2, "a custom suffix must redispatch");
    assert!(acked_before_second.load(Ordering::SeqCst), "release must be acknowledged before the second dispatch");
    assert_eq!(echo.models(), ["target-a", "target-a(high)", "target-a", "target-custom"]);

    mgr.close_execution_session("retained-route-ack").await;
    for _ in 0..1000 {
        if acks.load(Ordering::SeqCst) == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(acks.load(Ordering::SeqCst), 2);
    cancel.kill();
    let _ = task.await;
}

// Go: TestHomeRedispatchStopsWhenReleaseAcknowledgementFails.
#[tokio::test]
async fn redispatch_stops_when_the_release_acknowledgement_fails() {
    use cpa_home::concurrency_release::ReleaseFlusher;
    let registry = Registry::new();
    let flusher = ReleaseFlusher::with_timings(Duration::from_millis(1), Duration::from_millis(1));
    flusher.set_sender(Some(Arc::new(|_| Box::pin(async { Err(HomeError::Io("deadline exceeded".into())) }))));
    let f = flusher.clone();
    registry.set_release_sink(Some(Arc::new(move |g, s| f.mark_dirty(g, s))));
    let cancel: cpa_home::client::Cancel = Arc::new(Default::default());
    let (run_flusher, run_cancel) = (flusher.clone(), cancel.clone());
    let task = tokio::spawn(async move { run_flusher.run(&run_cancel).await });

    let home = DynHome::new(|model, _| ack_route_reply(model));
    let mgr = dyn_manager(home.clone(), registry, |c| {
        c.credential_concurrency.cpa_cancel_bound = cpa_config::GoDuration::from_millis(20);
    });
    mgr.register_executor(Echo::new("retained-route"));
    run_on(&mgr, "retained-route", "alias-a", session_opts("release-failure", "retained-route-auth")).await.unwrap();
    assert!(run_on(&mgr, "retained-route", "alias-a(custom)", session_opts("release-failure", "retained-route-auth")).await.is_err());
    assert_eq!(home.models().len(), 1, "no second dispatch after a release failure");
    cancel.kill();
    let _ = task.await;
}
