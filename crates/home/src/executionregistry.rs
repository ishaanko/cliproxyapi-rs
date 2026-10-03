//! Execution registry (Go: `sdk/cliproxy/executionregistry`): tracks Home-dispatched executions
//! for one subscriber lifetime. A dispatch reserves a pending slot, installs a [`Scope`] once Home
//! answered, and ending the scope releases the credential's concurrency slot through the
//! registry's release sink.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use parking_lot::{Condvar, Mutex};
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("execution registry is not accepting dispatches")]
    NotAccepting,
    #[error("execution registry is closed")]
    Closed,
    #[error("invalid pending dispatch")]
    InvalidPendingDispatch,
    #[error("invalid execution resource")]
    InvalidExecutionResource,
    #[error("execution resource is already bound")]
    ResourceAlreadyBound,
    #[error("context deadline exceeded")]
    DeadlineExceeded,
}

const STATE_ACCEPTING: u32 = 0;
const STATE_DRAINING: u32 = 1;
const STATE_CLOSED: u32 = 2;

/// Describes a Home-dispatched execution.
#[derive(Debug, Clone, Default)]
pub struct ScopeSpec {
    pub request_id: String,
    pub credential_id: String,
    pub model: String,
    pub kind: String,
    pub started_at: Option<SystemTime>,
    pub accounted: bool,
}

/// Cumulative release sequence key: one accounted credential and model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReleaseGroup {
    pub credential_id: String,
    pub model: String,
}

/// One-shot completion signal.
#[derive(Default)]
pub struct Done {
    flag: AtomicBool,
    notify: Notify,
}

impl Done {
    pub fn complete(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_done(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_done() {
                return;
            }
            notified.await;
        }
    }
}

/// Completes after Home acknowledges a cumulative release sequence.
#[derive(Clone)]
pub struct ReleaseTicket {
    pub group: ReleaseGroup,
    pub sequence: i64,
    done: Arc<Done>,
}

impl ReleaseTicket {
    /// `None` for a non-positive sequence (Go: `NewReleaseTicket`).
    pub fn new(group: ReleaseGroup, sequence: i64, done: Arc<Done>) -> Option<ReleaseTicket> {
        (sequence > 0).then_some(ReleaseTicket { group, sequence, done })
    }

    /// Waits for the acknowledgement; `Err` when `timeout` expires first.
    pub async fn wait(&self, timeout: Duration) -> Result<(), RegistryError> {
        tokio::time::timeout(timeout, self.done.wait())
            .await
            .map_err(|_| RegistryError::DeadlineExceeded)
    }

    pub fn is_done(&self) -> bool {
        self.done.is_done()
    }
}

/// Receives the latest cumulative sequence of a release group, optionally returning an
/// acknowledgement ticket.
pub type ReleaseSink = Arc<dyn Fn(ReleaseGroup, i64) -> Option<ReleaseTicket> + Send + Sync>;

/// Closes the execution resource bound to a scope.
pub type CloseFn = Box<dyn FnOnce() -> Result<(), String> + Send>;

#[derive(Default)]
struct Locked {
    next: u64,
    snapshot_revision: i64,
    observed_barrier: i64,
    pending_barrier_sequence: u64,
    published_barrier: i64,
    pending: HashSet<u64>,
    scopes: HashMap<u64, Arc<ScopeInner>>,
    release_sequences: HashMap<ReleaseGroup, i64>,
    release_sink: Option<ReleaseSink>,
}

struct RegInner {
    state: AtomicU32,
    locked: Mutex<Locked>,
    changed: Notify,
}

/// Owns all dispatches accepted during one Home subscriber lifetime.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<RegInner>,
}

/// Reserves an execution slot until it is installed or ended.
pub struct PendingDispatch {
    id: u64,
    reg: Arc<RegInner>,
    done: Mutex<bool>,
}

struct ScopeInner {
    id: u64,
    reg: Arc<RegInner>,
    spec: ScopeSpec,
    /// Guarded by the registry lock for the accepting/active check in `bind`.
    active: AtomicBool,
    /// The bound resource and, once its close started, the completion other closers wait on.
    close: Mutex<CloseState>,
    /// `Some(ticket)` once ended; held during the end procedure so callers wait for it.
    ended: Mutex<Option<Option<ReleaseTicket>>>,
}

#[derive(Default)]
struct CloseState {
    close_fn: Option<CloseFn>,
    done: Option<Arc<CloseDone>>,
}

/// Blocking completion latch of one resource close.
#[derive(Default)]
struct CloseDone {
    finished: Mutex<bool>,
    cv: Condvar,
}

impl CloseDone {
    fn finish(&self) {
        *self.finished.lock() = true;
        self.cv.notify_all();
    }

    fn wait(&self) {
        let mut finished = self.finished.lock();
        while !*finished {
            self.cv.wait(&mut finished);
        }
    }
}

/// Completes the latch even if the closer panics, so waiters never hang.
struct FinishOnDrop(Arc<CloseDone>);

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        self.0.finish();
    }
}

/// Outcome of trying to start the resource close.
enum Claim {
    /// No resource was ever bound.
    Nothing,
    /// Another caller already runs (or ran) the close.
    Running(Arc<CloseDone>),
    /// This caller owns the close and must run it.
    Run(CloseFn, Arc<CloseDone>),
}

/// Owns the resource of one installed execution.
#[derive(Clone)]
pub struct Scope {
    inner: Arc<ScopeInner>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl RegInner {
    fn signal(&self) {
        self.changed.notify_waiters();
    }

    fn state(&self) -> u32 {
        self.state.load(Ordering::SeqCst)
    }
}

impl Registry {
    pub fn new() -> Registry {
        Registry {
            inner: Arc::new(RegInner {
                state: AtomicU32::new(STATE_ACCEPTING),
                locked: Mutex::new(Locked::default()),
                changed: Notify::new(),
            }),
        }
    }

    pub fn is_accepting(&self) -> bool {
        self.inner.state() == STATE_ACCEPTING
    }

    /// Reserves a dispatch token while the registry accepts traffic.
    pub fn begin_dispatch(&self) -> Result<PendingDispatch, RegistryError> {
        if self.inner.state() != STATE_ACCEPTING {
            return Err(RegistryError::NotAccepting);
        }
        let mut l = self.inner.locked.lock();
        if self.inner.state() != STATE_ACCEPTING {
            return Err(RegistryError::NotAccepting);
        }
        l.next += 1;
        let id = l.next;
        l.pending.insert(id);
        Ok(PendingDispatch { id, reg: self.inner.clone(), done: Mutex::new(false) })
    }

    /// Waits until every dispatch with an unresolved Home response has ended or been installed.
    pub async fn wait_pending(&self, timeout: Duration) -> Result<(), RegistryError> {
        let wait = async {
            loop {
                let notified = self.inner.changed.notified();
                if self.inner.locked.lock().pending.is_empty() {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(timeout, wait).await.map_err(|_| RegistryError::DeadlineExceeded)
    }

    /// Turns a pending dispatch token into an active execution scope.
    pub fn install(&self, pending: &PendingDispatch, spec: ScopeSpec) -> Result<Scope, RegistryError> {
        if !Arc::ptr_eq(&pending.reg, &self.inner) {
            return Err(RegistryError::InvalidPendingDispatch);
        }
        let mut done = pending.done.lock();
        let mut l = self.inner.locked.lock();
        if self.inner.state() != STATE_ACCEPTING {
            *done = true;
            l.pending.remove(&pending.id);
            drop(l);
            self.inner.signal();
            return Err(RegistryError::NotAccepting);
        }
        if !l.pending.contains(&pending.id) {
            return Err(RegistryError::InvalidPendingDispatch);
        }
        *done = true;
        l.pending.remove(&pending.id);
        let inner = Arc::new(ScopeInner {
            id: pending.id,
            reg: self.inner.clone(),
            spec,
            active: AtomicBool::new(true),
            close: Mutex::new(CloseState::default()),
            ended: Mutex::new(None),
        });
        l.scopes.insert(inner.id, inner.clone());
        drop(l);
        self.inner.signal();
        Ok(Scope { inner })
    }

    /// Replaces the cumulative release sink and replays every known group.
    pub fn set_release_sink(&self, sink: Option<ReleaseSink>) {
        let sequences: Vec<(ReleaseGroup, i64)> = {
            let mut l = self.inner.locked.lock();
            l.release_sink = sink.clone();
            l.release_sequences.iter().map(|(g, s)| (g.clone(), *s)).collect()
        };
        if let Some(sink) = sink {
            for (group, sequence) in sequences {
                if sequence > 0 {
                    sink(group, sequence);
                }
            }
        }
    }

    /// Rejects new work, cancels active resources and waits for all owners to end.
    pub async fn drain(&self, timeout: Duration) -> Result<(), RegistryError> {
        let inner = &self.inner;
        let swapped = inner
            .state
            .compare_exchange(STATE_ACCEPTING, STATE_DRAINING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if !swapped && inner.state() != STATE_DRAINING {
            return Err(RegistryError::Closed);
        }
        let scopes: Vec<Arc<ScopeInner>> = inner.locked.lock().scopes.values().cloned().collect();
        for scope in scopes {
            scope.close_resource_in_background();
        }
        let wait = async {
            loop {
                let notified = inner.changed.notified();
                {
                    let l = inner.locked.lock();
                    if l.pending.is_empty() && l.scopes.is_empty() {
                        inner.state.store(STATE_CLOSED, Ordering::SeqCst);
                        return;
                    }
                }
                notified.await;
            }
        };
        tokio::time::timeout(timeout, wait).await.map_err(|_| RegistryError::DeadlineExceeded)
    }

    /// Permanently rejects new work and closes every currently bound resource.
    pub fn close(&self) -> Result<(), RegistryError> {
        if self.inner.state() == STATE_CLOSED {
            return Ok(());
        }
        self.inner.state.store(STATE_CLOSED, Ordering::SeqCst);
        let scopes: Vec<Arc<ScopeInner>> = self.inner.locked.lock().scopes.values().cloned().collect();
        for scope in scopes {
            scope.close_resource_and_wait();
        }
        Ok(())
    }

    /// Records the latest Home observation barrier.
    pub fn observe_barrier(&self, revision: i64) {
        if revision <= 0 {
            return;
        }
        let mut l = self.inner.locked.lock();
        if revision > l.observed_barrier {
            l.observed_barrier = revision;
            l.pending_barrier_sequence = l.next;
        }
    }

    /// Copies all active executions into an immutable snapshot.
    pub fn freeze_in_flight(&self) -> Freeze {
        let mut l = self.inner.locked.lock();
        if l.observed_barrier > l.published_barrier {
            let blocked = l.pending.iter().any(|seq| *seq <= l.pending_barrier_sequence);
            if !blocked {
                l.published_barrier = l.observed_barrier;
            }
        }
        l.snapshot_revision += 1;
        Freeze {
            revision: l.snapshot_revision,
            barrier_revision: l.published_barrier,
            executions: l
                .scopes
                .values()
                .map(|s| Observation {
                    request_id: s.spec.request_id.clone(),
                    credential_id: s.spec.credential_id.clone(),
                    model: s.spec.model.clone(),
                    request_kind: s.spec.kind.clone(),
                    started_at: s.spec.started_at,
                    accounted: s.spec.accounted,
                })
                .collect(),
        }
    }
}

/// Immutable in-flight execution snapshot entry.
#[derive(Debug, Clone)]
pub struct Observation {
    pub request_id: String,
    pub credential_id: String,
    pub model: String,
    pub request_kind: String,
    pub started_at: Option<SystemTime>,
    pub accounted: bool,
}

/// Immutable in-flight execution snapshot.
#[derive(Debug, Clone, Default)]
pub struct Freeze {
    pub revision: i64,
    pub barrier_revision: i64,
    pub executions: Vec<Observation>,
}

impl PendingDispatch {
    /// Releases a dispatch token that was not installed.
    pub fn end(&self) {
        let mut done = self.done.lock();
        if *done {
            return;
        }
        *done = true;
        self.reg.locked.lock().pending.remove(&self.id);
        self.reg.signal();
    }
}

impl Drop for PendingDispatch {
    fn drop(&mut self) {
        self.end();
    }
}

impl ScopeInner {
    /// Takes ownership of the bound resource's close, never running it (callers run it without
    /// any registry or scope lock held).
    fn claim_close(&self) -> Claim {
        let mut st = self.close.lock();
        if let Some(done) = &st.done {
            return Claim::Running(done.clone());
        }
        let Some(f) = st.close_fn.take() else { return Claim::Nothing };
        let done = Arc::new(CloseDone::default());
        st.done = Some(done.clone());
        Claim::Run(f, done)
    }

    /// Runs a claimed close and completes its latch.
    fn run_close(f: CloseFn, done: Arc<CloseDone>) {
        let _finish = FinishOnDrop(done);
        if let Err(e) = f() {
            tracing::warn!("Home execution resource close failed: {e}");
        }
    }

    /// Closes the bound resource (inline, off-lock) and waits for any concurrent close of it.
    fn close_resource_and_wait(&self) {
        match self.claim_close() {
            Claim::Nothing => {}
            Claim::Running(done) => done.wait(),
            Claim::Run(f, done) => Self::run_close(f, done),
        }
    }

    /// Starts the close on its own thread so a slow closer cannot stall a drain past its deadline.
    fn close_resource_in_background(&self) {
        if let Claim::Run(f, done) = self.claim_close() {
            std::thread::spawn(move || Self::run_close(f, done));
        }
    }
}

impl Scope {
    pub fn spec(&self) -> &ScopeSpec {
        &self.inner.spec
    }

    /// Attaches the execution resource. A scope accepts exactly one resource.
    pub fn bind(&self, close: CloseFn) -> Result<(), RegistryError> {
        let _l = self.inner.reg.locked.lock();
        if self.inner.reg.state() != STATE_ACCEPTING || !self.inner.active.load(Ordering::SeqCst) {
            return Err(RegistryError::NotAccepting);
        }
        let mut st = self.inner.close.lock();
        if st.close_fn.is_some() || st.done.is_some() {
            return Err(RegistryError::ResourceAlreadyBound);
        }
        st.close_fn = Some(close);
        Ok(())
    }

    /// Closes the bound resource and releases this scope exactly once.
    pub fn end(&self, reason: &str) {
        let _ = self.end_with_release(reason);
    }

    /// Like [`Scope::end`], returning the release acknowledgement ticket. The release sink runs
    /// without the registry lock held.
    pub fn end_with_release(&self, _reason: &str) -> Option<ReleaseTicket> {
        let inner = &self.inner;
        let mut ended = inner.ended.lock();
        if let Some(ticket) = &*ended {
            return ticket.clone();
        }
        {
            let _l = inner.reg.locked.lock();
            inner.active.store(false, Ordering::SeqCst);
        }
        inner.close_resource_and_wait();

        let (sink, group, sequence) = {
            let mut l = inner.reg.locked.lock();
            if inner.spec.accounted {
                let group = ReleaseGroup {
                    credential_id: inner.spec.credential_id.clone(),
                    model: inner.spec.model.clone(),
                };
                let seq = {
                    let seq = l.release_sequences.entry(group.clone()).or_insert(0);
                    *seq += 1;
                    *seq
                };
                (l.release_sink.clone(), group, seq)
            } else {
                (None, ReleaseGroup { credential_id: String::new(), model: String::new() }, 0)
            }
        };
        let ticket = match sink {
            Some(sink) if sequence > 0 => sink(group, sequence),
            _ => None,
        };
        *ended = Some(ticket.clone());
        {
            let mut l = inner.reg.locked.lock();
            l.scopes.remove(&inner.id);
        }
        inner.reg.signal();
        ticket
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn spec(accounted: bool) -> ScopeSpec {
        ScopeSpec {
            request_id: "r".into(),
            credential_id: "cred".into(),
            model: "m".into(),
            kind: "http".into(),
            started_at: Some(SystemTime::now()),
            accounted,
        }
    }

    #[test]
    fn end_releases_once_and_sequences_accumulate_per_group() {
        let reg = Registry::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        reg.set_release_sink(Some(Arc::new(move |g, s| {
            seen2.lock().push((g, s));
            None
        })));
        for _ in 0..2 {
            let p = reg.begin_dispatch().unwrap();
            let scope = reg.install(&p, spec(true)).unwrap();
            scope.end("done");
            scope.end("again");
        }
        let p = reg.begin_dispatch().unwrap();
        reg.install(&p, spec(false)).unwrap().end("unaccounted");
        let seen = seen.lock();
        assert_eq!(seen.iter().map(|(_, s)| *s).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn end_closes_bound_resource_once() {
        let reg = Registry::new();
        let closes = Arc::new(AtomicUsize::new(0));
        let p = reg.begin_dispatch().unwrap();
        let scope = reg.install(&p, spec(true)).unwrap();
        let c = closes.clone();
        scope.bind(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }))
        .unwrap();
        assert_eq!(scope.bind(Box::new(|| Ok(()))).unwrap_err(), RegistryError::ResourceAlreadyBound);
        scope.end("a");
        scope.end("b");
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert_eq!(scope.bind(Box::new(|| Ok(()))).unwrap_err(), RegistryError::NotAccepting);
    }

    #[tokio::test]
    async fn drain_cancels_resources_and_waits_for_scopes() {
        let reg = Registry::new();
        let closes = Arc::new(AtomicUsize::new(0));
        let p = reg.begin_dispatch().unwrap();
        let scope = reg.install(&p, spec(true)).unwrap();
        let c = closes.clone();
        scope.bind(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }))
        .unwrap();
        assert_eq!(reg.drain(Duration::from_millis(30)).await, Err(RegistryError::DeadlineExceeded));
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert!(reg.begin_dispatch().is_err());
        scope.end("done");
        assert_eq!(reg.drain(Duration::from_millis(100)).await, Ok(()));
    }

    #[tokio::test]
    async fn pending_dispatches_block_wait_pending_until_ended_or_installed() {
        let reg = Registry::new();
        let p = reg.begin_dispatch().unwrap();
        assert!(reg.wait_pending(Duration::from_millis(20)).await.is_err());
        drop(p);
        assert!(reg.wait_pending(Duration::from_millis(20)).await.is_ok());
        let p = reg.begin_dispatch().unwrap();
        let scope = reg.install(&p, spec(false)).unwrap();
        assert!(reg.wait_pending(Duration::from_millis(20)).await.is_ok());
        drop(scope);
    }

    #[test]
    fn freeze_publishes_barrier_only_after_older_pending_dispatches_resolve() {
        let reg = Registry::new();
        let p = reg.begin_dispatch().unwrap();
        reg.observe_barrier(5);
        assert_eq!(reg.freeze_in_flight().barrier_revision, 0);
        let scope = reg.install(&p, spec(true)).unwrap();
        let f = reg.freeze_in_flight();
        assert_eq!((f.barrier_revision, f.revision, f.executions.len()), (5, 2, 1));
        scope.end("x");
        assert!(reg.freeze_in_flight().executions.is_empty());
    }

    #[tokio::test]
    async fn release_ticket_waits_for_acknowledgement() {
        let done = Arc::new(Done::default());
        let ticket = ReleaseTicket::new(ReleaseGroup { credential_id: "c".into(), model: "m".into() }, 1, done.clone()).unwrap();
        assert!(ticket.wait(Duration::from_millis(10)).await.is_err());
        done.complete();
        assert!(ticket.wait(Duration::from_millis(10)).await.is_ok());
        assert!(ReleaseTicket::new(ticket.group.clone(), 0, done).is_none());
    }
}
