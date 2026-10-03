//! Tests of how executor usage reports become usage events: stream hang-ups, the requested-model
//! alias and attempt-level failures that a report alone would hide.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use cpa_auth::Auth;
use cpa_core::registry::{ModelInfo, ModelRegistry};
use cpa_translator::Format;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::{UsageFacts, build_usage_records};
use crate::conductor::{ExecResult, Manager, ManualClock};
use crate::executor::{ExecError, Executor, Options, Request, Response, StreamResult};
use crate::usage::UsageTracker;
use crate::usage_accounting::Detail;
use crate::usage_report::{Failure, Record, UsageSink};

fn report(model: &str, alias: &str, failed: bool, input_tokens: i64) -> Record {
    Record {
        request_id: "exec-1".into(),
        trace_id: String::new(),
        provider: "codex".into(),
        base_url: String::new(),
        executor_type: "CodexExecutor".into(),
        model: model.into(),
        alias: alias.into(),
        api_key: String::new(),
        session_id: String::new(),
        parent_session_id: String::new(),
        auth_id: "a".into(),
        auth_index: String::new(),
        access_token_sha256: String::new(),
        auth_type: String::new(),
        source: String::new(),
        reasoning_effort: String::new(),
        service_tier: "default".into(),
        response_service_tier: String::new(),
        response_model: String::new(),
        generate: true,
        stream: false,
        requested_at: Utc::now(),
        latency: Duration::from_millis(5),
        ttft: Duration::ZERO,
        failed,
        fail: Failure::default(),
        detail: Detail { input_tokens, total_tokens: input_tokens, ..Default::default() },
    }
}

fn publish(opts: &Options, rec: Record) {
    let c = opts.usage_collector.clone().expect("conductor attaches a collector");
    c.mark_reporter_attached();
    c.publish(rec);
}

/// Publishes one successful report per call, then either idles (stream) or fails with 502 (unary).
struct ReportExec {
    provider: &'static str,
    held: Mutex<Vec<mpsc::Sender<Result<Bytes, ExecError>>>>,
}

#[async_trait]
impl Executor for ReportExec {
    fn identifier(&self) -> &str {
        self.provider
    }

    async fn execute(&self, _auth: &Auth, _req: Request, opts: Options) -> Result<Response, ExecError> {
        // The Codex image tool path: the reporter is ensured published, then the call fails.
        publish(&opts, report("gpt-5", "", false, 0));
        Err(ExecError::new(502, "upstream returned no image"))
    }

    async fn execute_stream(&self, _auth: &Auth, _req: Request, opts: Options) -> Result<StreamResult, ExecError> {
        // One response completed: its record is published, its last frame sent, the socket stays open.
        publish(&opts, report("gpt-5", "", false, 7));
        let (tx, rx) = mpsc::channel(4);
        tx.try_send(Ok(Bytes::from_static(b"data: {\"type\":\"response.completed\"}\n\n"))).expect("room for one chunk");
        self.held.lock().push(tx);
        Ok(StreamResult::new(Default::default(), rx))
    }

    async fn refresh(&self, auth: &Auth) -> Result<Auth, ExecError> {
        Ok(auth.clone())
    }

    async fn count_tokens(&self, _auth: &Auth, _req: Request, _opts: Options) -> Result<Response, ExecError> {
        Ok(Response::default())
    }
}

async fn manager_with(provider: &'static str, edit: impl FnOnce(&mut Auth)) -> (Manager, Arc<UsageTracker>, Arc<ReportExec>) {
    let registry: &'static ModelRegistry = Box::leak(Box::new(ModelRegistry::new()));
    let mgr = Manager::with_parts(Arc::new(ManualClock::new(Utc::now())), registry);
    let exec = Arc::new(ReportExec { provider, held: Mutex::new(Vec::new()) });
    mgr.register_executor(exec.clone());
    let tracker = Arc::new(UsageTracker::new());
    mgr.set_usage_tracker(Some(tracker.clone()));
    let mut auth = Auth::new("a", provider);
    edit(&mut auth);
    registry.register_client("a", provider, &[ModelInfo { id: "gpt-5".into(), ..Default::default() }]);
    mgr.register(auth).await.expect("register credential");
    (mgr, tracker, exec)
}

fn request() -> Request {
    Request { model: "gpt-5".into(), payload: Bytes::from_static(b"{}"), format: Format::OpenAI, metadata: Default::default() }
}

/// A duplex-style response publishes its record when it completes; the client leaving afterwards
/// must not lose it, and the record must appear while the socket is still open.
#[tokio::test]
async fn record_published_at_response_completed_survives_client_hang_up() {
    let (mgr, tracker, exec) = manager_with("claude", |a| {
        a.attributes.insert("auth_kind".into(), "oauth".into());
    })
    .await;
    let mut stream = mgr.execute_stream(&["claude".into()], request(), Options::new(Format::OpenAI)).await.expect("stream");
    assert!(stream.chunks.recv().await.is_some());
    for _ in 0..100 {
        if !tracker.requests(10, None).events.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(tracker.requests(10, None).events.len(), 1, "recorded before the stream ended");
    let upstream = exec.held.lock().pop().expect("held upstream");
    drop(stream);
    tokio::time::timeout(Duration::from_secs(1), upstream.closed()).await.expect("upstream closed after hang-up");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let events = tracker.requests(10, None).events;
    assert_eq!(events.len(), 1, "no fallback record after the hang-up");
    let rec = &events[0].record;
    assert_eq!((rec.tokens.input_tokens, rec.failed, rec.stream), (7, false, true));
    let st = mgr.get("a").expect("credential");
    assert_eq!((st.success, st.failed), (0, 0), "an abandoned Claude OAuth stream still records no result");
}

/// The requested model with its thinking suffix stays the alias of the upstream model's record.
#[test]
fn alias_of_suffixed_request_survives_the_report_overlay() {
    let result = ExecResult {
        auth_id: "a".into(),
        provider: "codex".into(),
        model: "gpt-5(high)".into(),
        route_model: "gpt-5(high)".into(),
        success: true,
        retry_after: None,
        credential_scope: false,
        error: None,
        options: Options::new(Format::OpenAI),
        skip_quota_observation: false,
        response_headers: Default::default(),
    };
    let facts = UsageFacts {
        upstream_model: "gpt-5(high)".into(),
        requested_model: "gpt-5(high)".into(),
        reports: vec![report("gpt-5", "gpt-5(high)", false, 3)],
        ..Default::default()
    };
    let recs = build_usage_records(&result, None, facts, Utc::now());
    assert_eq!((recs[0].model.as_str(), recs[0].alias.as_str()), ("gpt-5", "gpt-5(high)"));
}

/// A report published before the attempt failed (image tool without an image) must not mask the
/// attempt's failure.
#[tokio::test]
async fn failed_unary_attempt_stays_failed_despite_a_success_report() {
    let (mgr, tracker, _exec) = manager_with("mock", |_| {}).await;
    let err = mgr
        .execute(&["mock".into()], request(), Options::new(Format::OpenAI))
        .await
        .expect_err("502 from the executor");
    assert_eq!(err.status, 502);
    let events = tracker.requests(10, None).events;
    assert_eq!(events.len(), 1);
    let rec = &events[0].record;
    assert!(rec.failed);
    assert_eq!(rec.fail.status_code, 502);
}
