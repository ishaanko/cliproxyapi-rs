//! Ports of sdk/cliproxy/executionregistry/{registry,concurrency_release,observation}_test.go.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::time::{sleep, timeout};

use crate::*;

const ONE_SECOND: Duration = Duration::from_secs(1);
const SHORT: Duration = Duration::from_millis(20);

fn install(registry: &Registry, spec: ScopeSpec) -> Scope {
    let pending = registry.begin_dispatch().unwrap();
    registry.install(&pending, spec).unwrap()
}

fn secs(n: u64) -> Option<SystemTime> {
    Some(UNIX_EPOCH + Duration::from_secs(n))
}

/// A bound resource whose close blocks until `release` is sent; `started` flips when it runs.
fn blocking_close(scope: &Scope) -> (mpsc::Sender<()>, Arc<AtomicBool>) {
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let started = Arc::new(AtomicBool::new(false));
    let started_in = started.clone();
    scope
        .bind(move || {
            started_in.store(true, Ordering::SeqCst);
            let _ = release_rx.recv();
            Ok(())
        })
        .unwrap();
    (release_tx, started)
}

async fn wait_started(started: &AtomicBool) {
    timeout(ONE_SECOND, async {
        while !started.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("resource close did not start");
}

#[tokio::test]
async fn drain_rejects_late_install_and_cancels_bound_scopes() {
    let registry = Registry::new();
    let scope = install(
        &registry,
        ScopeSpec { request_id: "req-1".into(), credential_id: "cred-1".into(), model: "gpt".into(), kind: "http".into(), ..Default::default() },
    );
    let closed = Arc::new(AtomicI32::new(0));
    let (closed_in, scope_in) = (closed.clone(), scope.clone());
    scope
        .bind(move || {
            closed_in.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move { scope_in.end("canceled").await });
            Ok(())
        })
        .unwrap();
    timeout(ONE_SECOND, registry.drain()).await.unwrap().unwrap();
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    assert_eq!(registry.begin_dispatch().err(), Some(Error::NotAccepting));
}

#[tokio::test]
async fn scope_end_is_exactly_once() {
    let registry = Registry::new();
    let scope = install(&registry, ScopeSpec::default());
    let closed = Arc::new(AtomicI32::new(0));
    let closed_in = closed.clone();
    scope
        .bind(move || {
            closed_in.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    let other = scope.clone();
    let task = tokio::spawn(async move { other.end("complete").await });
    scope.end("duplicate").await;
    task.await.unwrap();
    assert_eq!(closed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn drain_waits_for_pending_dispatch() {
    let registry = Registry::new();
    let pending = registry.begin_dispatch().unwrap();
    let r = registry.clone();
    let drain = tokio::spawn(async move { timeout(ONE_SECOND, r.drain()).await });
    sleep(SHORT).await;
    assert!(!drain.is_finished(), "drain returned before pending dispatch ended");
    pending.end();
    drain.await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn wait_pending_does_not_drain_active_scope() {
    let registry = Registry::new();
    let scope = install(&registry, ScopeSpec::default());
    let pending = registry.begin_dispatch().unwrap();

    let r = registry.clone();
    let wait = tokio::spawn(async move { timeout(ONE_SECOND, r.wait_pending()).await });
    sleep(SHORT).await;
    assert!(!wait.is_finished(), "wait_pending returned before pending dispatch ended");
    pending.end();
    wait.await.unwrap().unwrap();
    registry.begin_dispatch().expect("wait_pending stopped registry acceptance").end();
    scope.end("test cleanup").await;
}

#[tokio::test]
async fn drain_returns_when_blocking_resource_close_exceeds_timeout() {
    let registry = Registry::new();
    let scope = install(&registry, ScopeSpec::default());
    let (release, started) = blocking_close(&scope);

    assert!(timeout(SHORT, registry.drain()).await.is_err(), "drain should outlive its timeout");
    wait_started(&started).await;
    assert_eq!(registry.state(), State::Draining);

    let ended = {
        let scope = scope.clone();
        tokio::spawn(async move { scope.end("canceled").await })
    };
    release.send(()).unwrap();
    timeout(ONE_SECOND, ended).await.expect("Scope::end did not wait for resource close completion").unwrap();
    timeout(ONE_SECOND, registry.drain()).await.unwrap().unwrap();
}

#[tokio::test]
async fn drain_waits_for_blocking_resource_close() {
    let registry = Registry::new();
    let scope = install(&registry, ScopeSpec::default());
    let (release, started) = blocking_close(&scope);
    let r = registry.clone();
    let drain = tokio::spawn(async move { timeout(ONE_SECOND, r.drain()).await });

    wait_started(&started).await;
    let s = scope.clone();
    tokio::spawn(async move { s.end("canceled").await });
    sleep(SHORT).await;
    assert!(!drain.is_finished(), "drain returned before the resource close completed");
    release.send(()).unwrap();
    drain.await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn concurrent_drain_waits_for_blocking_resource_close() {
    let registry = Registry::new();
    let scope = install(&registry, ScopeSpec::default());
    let (release, started) = blocking_close(&scope);
    let r = registry.clone();
    let first = tokio::spawn(async move { timeout(ONE_SECOND, r.drain()).await });
    wait_started(&started).await;

    let s = scope.clone();
    let ended = tokio::spawn(async move { s.end("canceled").await });
    sleep(SHORT).await;
    assert!(!ended.is_finished(), "Scope::end returned before the resource close completed");

    let r = registry.clone();
    let second = tokio::spawn(async move { timeout(ONE_SECOND, r.drain()).await });
    sleep(SHORT).await;
    assert!(!second.is_finished(), "second drain returned before resource close completed");

    release.send(()).unwrap();
    timeout(ONE_SECOND, ended).await.expect("Scope::end did not complete after the resource close").unwrap();
    first.await.unwrap().unwrap().unwrap();
    second.await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn concurrent_close_waits_for_blocking_resource_close() {
    let registry = Registry::new();
    let scope = install(&registry, ScopeSpec::default());
    let (release, started) = blocking_close(&scope);

    let r = registry.clone();
    let first = tokio::spawn(async move { r.close().await });
    wait_started(&started).await;

    let r = registry.clone();
    let second = tokio::spawn(async move { r.close().await });
    sleep(SHORT).await;
    assert!(!second.is_finished(), "second close returned before resource close completed");

    release.send(()).unwrap();
    timeout(ONE_SECOND, first).await.unwrap().unwrap().unwrap();
    timeout(ONE_SECOND, second).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn drain_rejects_late_bind() {
    let registry = Registry::new();
    let scope = install(&registry, ScopeSpec::default());
    let r = registry.clone();
    let drain = tokio::spawn(async move { timeout(ONE_SECOND, r.drain()).await });
    timeout(ONE_SECOND, async {
        while registry.state() == State::Accepting {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("registry did not begin draining");
    assert_eq!(scope.bind(|| Ok(())).err(), Some(Error::NotAccepting));
    scope.end("canceled").await;
    drain.await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn drain_rejects_late_install() {
    let registry = Registry::new();
    let pending = registry.begin_dispatch().unwrap();
    let r = registry.clone();
    let drain = tokio::spawn(async move { timeout(ONE_SECOND, r.drain()).await });
    timeout(ONE_SECOND, async {
        while registry.state() == State::Accepting {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("registry did not begin draining");
    assert_eq!(registry.install(&pending, ScopeSpec::default()).err(), Some(Error::NotAccepting));
    drain.await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn bind_is_single_use_and_install_rejects_foreign_tokens() {
    let registry = Registry::new();
    let scope = install(&registry, ScopeSpec::default());
    scope.bind(|| Ok(())).unwrap();
    assert_eq!(scope.bind(|| Ok(())).err(), Some(Error::ResourceAlreadyBound));
    scope.end("done").await;

    let other = Registry::new();
    let pending = registry.begin_dispatch().unwrap();
    assert_eq!(other.install(&pending, ScopeSpec::default()).err(), Some(Error::InvalidPendingDispatch));
    pending.end();
    pending.end();
    assert_eq!(registry.install(&pending, ScopeSpec::default()).err(), Some(Error::InvalidPendingDispatch));
}

// ---- release sequences --------------------------------------------------------------------

#[derive(Default, Clone)]
struct RecordingSink(Arc<Mutex<std::collections::HashMap<ReleaseGroup, i64>>>);

impl RecordingSink {
    fn mark_dirty(&self, group: ReleaseGroup, sequence: i64) {
        let mut map = self.0.lock().unwrap();
        let entry = map.entry(group).or_insert(0);
        if sequence > *entry {
            *entry = sequence;
        }
    }

    fn sequence(&self, credential: &str, model: &str) -> i64 {
        let group = ReleaseGroup { credential_id: credential.into(), model: model.into() };
        self.0.lock().unwrap().get(&group).copied().unwrap_or(0)
    }
}

fn accounted(registry: &Registry, credential: &str, model: &str) -> Scope {
    install(registry, ScopeSpec { credential_id: credential.into(), model: model.into(), accounted: true, ..Default::default() })
}

#[tokio::test]
async fn end_marks_one_dirty_group() {
    let sink = RecordingSink::default();
    let registry = Registry::new();
    let sink_in = sink.clone();
    registry.set_legacy_release_sink(move |g, s| sink_in.mark_dirty(g, s));
    let scope = accounted(&registry, "cred-1", "gpt");
    scope.end("complete").await;
    scope.end("duplicate").await;
    assert_eq!(sink.sequence("cred-1", "gpt"), 1);
}

#[tokio::test]
async fn unaccounted_scope_does_not_release() {
    let sink = RecordingSink::default();
    let registry = Registry::new();
    let sink_in = sink.clone();
    registry.set_legacy_release_sink(move |g, s| sink_in.mark_dirty(g, s));
    let scope = install(&registry, ScopeSpec { credential_id: "cred-1".into(), model: "gpt".into(), accounted: false, ..Default::default() });
    scope.end("observation_complete").await;
    assert_eq!(sink.sequence("cred-1", "gpt"), 0);
}

#[tokio::test]
async fn set_release_sink_replays_existing_sequences() {
    let registry = Registry::new();
    accounted(&registry, "cred-1", "gpt").end("complete").await;
    let sink = RecordingSink::default();
    let sink_in = sink.clone();
    registry.set_legacy_release_sink(move |g, s| sink_in.mark_dirty(g, s));
    assert_eq!(sink.sequence("cred-1", "gpt"), 1);
}

#[tokio::test]
async fn end_with_release_returns_the_sinks_ticket_and_it_completes_on_ack() {
    let registry = Registry::new();
    let ack = Signal::new();
    let ack_in = ack.clone();
    registry.set_release_sink(Some(Arc::new(move |group, sequence| ReleaseTicket::new(group, sequence, Some(ack_in.clone())))));
    let ticket = accounted(&registry, "cred-1", "gpt").end_with_release("complete").await.expect("expected a ticket");
    assert_eq!((ticket.sequence, ticket.group.credential_id.as_str()), (1, "cred-1"));
    assert!(timeout(SHORT, ticket.wait()).await.is_err(), "ticket completed before the acknowledgement");
    ack.fire();
    timeout(ONE_SECOND, ticket.wait()).await.unwrap();
    assert!(ReleaseTicket::new(ticket.group.clone(), 0, Some(Signal::new())).is_none());
    assert!(ReleaseTicket::new(ticket.group.clone(), 1, None).is_none());
}

// ---- observation --------------------------------------------------------------------------

#[tokio::test]
async fn freeze_in_flight_waits_for_pending_barrier_and_copies_scopes() {
    let registry = Registry::new();
    let pending = registry.begin_dispatch().unwrap();
    registry.observe_barrier(14);

    let before = registry.freeze_in_flight(secs(12).unwrap());
    assert_eq!(before.barrier_revision, 0, "barrier published before install");

    let scope = registry
        .install(
            &pending,
            ScopeSpec {
                request_id: "req-a".into(),
                credential_id: "cred".into(),
                model: "gpt-5".into(),
                kind: "http".into(),
                started_at: secs(10),
                accounted: true,
            },
        )
        .unwrap();

    let mut after = registry.freeze_in_flight(secs(13).unwrap());
    assert!(after.barrier_revision == 14 && after.executions.len() == 1 && after.executions[0].accounted, "{after:?}");
    assert_eq!(after.executions[0].started_at, secs(10));
    after.executions[0].request_id = "mutated".into();

    let copied = registry.freeze_in_flight(secs(13).unwrap());
    assert!(copied.executions.len() == 1 && copied.executions[0].request_id == "req-a", "freeze did not copy scope: {copied:?}");

    scope.end("completed").await;
    let ended = registry.freeze_in_flight(secs(14).unwrap());
    assert!(ended.executions.is_empty() && ended.revision > after.revision, "{ended:?}");
}

#[tokio::test]
async fn observe_barrier_ignores_stale_and_non_positive_revisions() {
    let registry = Registry::new();
    registry.observe_barrier(0);
    registry.observe_barrier(-3);
    assert_eq!(registry.freeze_in_flight(UNIX_EPOCH).barrier_revision, 0);
    registry.observe_barrier(5);
    registry.observe_barrier(4);
    assert_eq!(registry.freeze_in_flight(UNIX_EPOCH).barrier_revision, 5);
}
