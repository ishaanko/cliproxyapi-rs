//! The registry itself (Go: sdk/cliproxy/executionregistry/registry.go).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use tokio::sync::{OnceCell, watch};

use crate::signal::{FireOnDrop, Signal};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
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
}

/// Lifecycle state of a [`Registry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Accepting,
    Draining,
    Closed,
}

const STATE_ACCEPTING: u32 = 0;
const STATE_DRAINING: u32 = 1;
const STATE_CLOSED: u32 = 2;

/// A Home-dispatched execution.
#[derive(Debug, Clone, Default)]
pub struct ScopeSpec {
    pub request_id: String,
    pub credential_id: String,
    pub model: String,
    pub kind: String,
    pub started_at: Option<SystemTime>,
    pub accounted: bool,
}

/// Identifies the cumulative release sequence for one accounted credential and model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReleaseGroup {
    pub credential_id: String,
    pub model: String,
}

/// Completes after Home acknowledges a cumulative release sequence.
#[derive(Debug, Clone)]
pub struct ReleaseTicket {
    pub group: ReleaseGroup,
    pub sequence: i64,
    done: Signal,
}

impl ReleaseTicket {
    /// A ticket backed by `done`. `None` when the sequence is not positive or the sink does not
    /// support acknowledgements (`done` is `None`).
    pub fn new(group: ReleaseGroup, sequence: i64, done: Option<Signal>) -> Option<Self> {
        match done {
            Some(done) if sequence > 0 => Some(Self { group, sequence, done }),
            _ => None,
        }
    }

    /// Waits until Home acknowledges the release. Bound the wait with `tokio::time::timeout`.
    pub async fn wait(&self) {
        self.done.wait().await;
    }
}

/// Receives the latest cumulative sequence for a release group and optionally returns an
/// acknowledgement ticket.
pub type ReleaseSink = Arc<dyn Fn(ReleaseGroup, i64) -> Option<ReleaseTicket> + Send + Sync>;

/// Closes the execution resource bound to a scope; runs once on a blocking thread.
pub type CloseFn = Box<dyn FnOnce() -> Result<(), String> + Send>;

#[derive(Default)]
pub(crate) struct Core {
    pub(crate) next: u64,
    pub(crate) snapshot_revision: i64,
    pub(crate) observed_barrier: i64,
    pub(crate) pending_barrier_sequence: u64,
    pub(crate) published_barrier: i64,
    pub(crate) pending: HashSet<u64>,
    pub(crate) scopes: HashMap<u64, Scope>,
    release_sequences: HashMap<ReleaseGroup, i64>,
    release_sink: Option<ReleaseSink>,
}

#[derive(Default)]
struct CloseState {
    started: bool,
    done: Option<Signal>,
}

struct Inner {
    state: AtomicU32,
    core: Mutex<Core>,
    /// Bumped whenever pending dispatches or scopes change; waiters re-check their condition.
    changed: watch::Sender<u64>,
    close: Mutex<CloseState>,
}

/// Owns all dispatches accepted during one Home subscriber lifetime. Cloning shares the registry.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Inner>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Registry {
    /// Creates an accepting registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: AtomicU32::new(STATE_ACCEPTING),
                core: Mutex::new(Core::default()),
                changed: watch::channel(0).0,
                close: Mutex::new(CloseState::default()),
            }),
        }
    }

    pub(crate) fn core(&self) -> MutexGuard<'_, Core> {
        lock(&self.inner.core)
    }

    pub fn state(&self) -> State {
        match self.inner.state.load(Ordering::SeqCst) {
            STATE_ACCEPTING => State::Accepting,
            STATE_DRAINING => State::Draining,
            _ => State::Closed,
        }
    }

    fn signal_locked(&self) {
        self.inner.changed.send_modify(|v| *v += 1);
    }

    /// Waits until `done` holds. `done` runs under the registry lock and may mutate the core.
    async fn wait_until(&self, mut done: impl FnMut(&mut Core) -> bool) {
        loop {
            let mut changed = {
                let mut core = self.core();
                if done(&mut core) {
                    return;
                }
                // Subscribing under the lock cannot miss a change signalled after the check.
                self.inner.changed.subscribe()
            };
            let _ = changed.changed().await;
        }
    }

    /// Reserves a dispatch token while the registry accepts traffic.
    pub fn begin_dispatch(&self) -> Result<PendingDispatch, Error> {
        if self.state() != State::Accepting {
            return Err(Error::NotAccepting);
        }
        let mut core = self.core();
        if self.state() != State::Accepting {
            return Err(Error::NotAccepting);
        }
        core.next += 1;
        let id = core.next;
        core.pending.insert(id);
        Ok(PendingDispatch { id, registry: self.clone(), ended: Mutex::new(false) })
    }

    /// Waits until every dispatch with an unresolved Home response has ended or been installed.
    pub async fn wait_pending(&self) {
        self.wait_until(|core| core.pending.is_empty()).await;
    }

    /// Atomically turns a pending dispatch token into an active execution scope.
    pub fn install(&self, pending: &PendingDispatch, spec: ScopeSpec) -> Result<Scope, Error> {
        if !Arc::ptr_eq(&pending.registry.inner, &self.inner) {
            return Err(Error::InvalidPendingDispatch);
        }
        let mut ended = lock(&pending.ended);
        let mut core = self.core();

        if self.state() != State::Accepting {
            *ended = true;
            core.pending.remove(&pending.id);
            self.signal_locked();
            return Err(Error::NotAccepting);
        }
        if !core.pending.contains(&pending.id) {
            return Err(Error::InvalidPendingDispatch);
        }

        *ended = true;
        core.pending.remove(&pending.id);
        let scope = Scope(Arc::new(ScopeInner {
            id: pending.id,
            registry: self.clone(),
            spec,
            state: Mutex::new(ScopeState { active: true, ..ScopeState::default() }),
            ended: OnceCell::new(),
        }));
        core.scopes.insert(pending.id, scope.clone());
        self.signal_locked();
        Ok(scope)
    }

    /// Replaces the cumulative release sink and replays every known group to it.
    pub fn set_release_sink(&self, sink: Option<ReleaseSink>) {
        let sequences = {
            let mut core = self.core();
            core.release_sink = sink.clone();
            let mut sequences: Vec<_> = core.release_sequences.iter().map(|(g, s)| (g.clone(), *s)).collect();
            sequences.sort_by(|a, b| (&a.0.credential_id, &a.0.model).cmp(&(&b.0.credential_id, &b.0.model)));
            sequences
        };
        let Some(sink) = sink else { return };
        for (group, sequence) in sequences {
            if sequence > 0 {
                sink(group, sequence);
            }
        }
    }

    /// Go's legacy callback form: a sink that cannot return acknowledgement tickets.
    pub fn set_legacy_release_sink(&self, sink: impl Fn(ReleaseGroup, i64) + Send + Sync + 'static) {
        self.set_release_sink(Some(Arc::new(move |group, sequence| {
            sink(group, sequence);
            None
        })));
    }

    fn mark_released_locked(core: &mut Core, scope: &ScopeInner) -> (Option<ReleaseSink>, ReleaseGroup, i64) {
        let group = ReleaseGroup { credential_id: scope.spec.credential_id.clone(), model: scope.spec.model.clone() };
        if !scope.spec.accounted {
            return (None, group, 0);
        }
        let sequence = core.release_sequences.entry(group.clone()).or_insert(0);
        *sequence += 1;
        let sequence = *sequence;
        (core.release_sink.clone(), group, sequence)
    }

    fn scopes_snapshot(&self) -> Vec<Scope> {
        let core = self.core();
        let mut scopes: Vec<_> = core.scopes.values().cloned().collect();
        scopes.sort_by_key(|s| s.0.id);
        scopes
    }

    /// Rejects new work, cancels active resources, and waits for all owners to end. Returns
    /// `Closed` if the registry already finished closing. Bound the wait with a timeout; the
    /// registry stays draining if it expires.
    pub async fn drain(&self) -> Result<(), Error> {
        let moved = self.inner.state.compare_exchange(STATE_ACCEPTING, STATE_DRAINING, Ordering::SeqCst, Ordering::SeqCst);
        if moved.is_err() && self.state() != State::Draining {
            return Err(Error::Closed);
        }
        for scope in self.scopes_snapshot() {
            scope.start_bound_resource_close();
        }
        let state = &self.inner.state;
        self.wait_until(|core| {
            let finished = core.pending.is_empty() && core.scopes.is_empty();
            if finished {
                state.store(STATE_CLOSED, Ordering::SeqCst);
            }
            finished
        })
        .await;
        Ok(())
    }

    /// Permanently rejects new work and closes every currently bound resource.
    pub async fn close(&self) -> Result<(), Error> {
        enum Role {
            /// Someone else is closing: wait for them.
            Wait(Option<Signal>),
            AlreadyClosed,
            Closer(Signal),
        }
        let role = {
            let mut close = lock(&self.inner.close);
            if close.started {
                Role::Wait(close.done.clone())
            } else if self.state() == State::Closed {
                Role::AlreadyClosed
            } else {
                close.started = true;
                let done = Signal::new();
                close.done = Some(done.clone());
                Role::Closer(done)
            }
        };
        let done = match role {
            Role::Wait(done) => {
                if let Some(done) = done {
                    done.wait().await;
                }
                return Ok(());
            }
            Role::AlreadyClosed => return Ok(()),
            Role::Closer(done) => done,
        };
        let _release_waiters = FireOnDrop(done);

        self.inner.state.store(STATE_CLOSED, Ordering::SeqCst);
        for scope in self.scopes_snapshot() {
            scope.wait_for_bound_resource_close().await;
        }
        Ok(())
    }
}

/// Reserves an execution slot until it is installed or ended.
pub struct PendingDispatch {
    id: u64,
    registry: Registry,
    /// Held while the token is resolved, so `end` and `install` cannot interleave.
    ended: Mutex<bool>,
}

impl PendingDispatch {
    /// Releases a dispatch token that was not installed. Idempotent.
    pub fn end(&self) {
        let mut ended = lock(&self.ended);
        if *ended {
            return;
        }
        *ended = true;
        let mut core = self.registry.core();
        core.pending.remove(&self.id);
        self.registry.signal_locked();
    }
}

#[derive(Default)]
struct ScopeState {
    close_fn: Option<CloseFn>,
    close_done: Option<Signal>,
    active: bool,
}

struct ScopeInner {
    id: u64,
    registry: Registry,
    spec: ScopeSpec,
    state: Mutex<ScopeState>,
    /// Runs the end sequence once; concurrent `end` calls wait for it and share its ticket.
    ended: OnceCell<Option<ReleaseTicket>>,
}

/// Owns the resource for one installed execution. Cloning shares the scope.
#[derive(Clone)]
pub struct Scope(Arc<ScopeInner>);

impl Scope {
    pub(crate) fn spec(&self) -> &ScopeSpec {
        &self.0.spec
    }

    /// Attaches the execution resource. A scope accepts exactly one resource.
    pub fn bind(&self, close_fn: impl FnOnce() -> Result<(), String> + Send + 'static) -> Result<(), Error> {
        let registry = &self.0.registry;
        let _core = registry.core();
        let mut state = lock(&self.0.state);
        if registry.state() != State::Accepting || !state.active {
            return Err(Error::NotAccepting);
        }
        if state.close_fn.is_some() || state.close_done.is_some() {
            return Err(Error::ResourceAlreadyBound);
        }
        state.close_fn = Some(Box::new(close_fn));
        Ok(())
    }

    /// Closes the bound resource and releases this execution scope exactly once.
    pub async fn end(&self, reason: &str) {
        let _ = self.end_with_release(reason).await;
    }

    /// Like [`Scope::end`], returning the release acknowledgement ticket. The release sink runs
    /// without the registry lock held.
    pub async fn end_with_release(&self, _reason: &str) -> Option<ReleaseTicket> {
        let inner = &self.0;
        inner
            .ended
            .get_or_init(|| async {
                {
                    let _core = inner.registry.core();
                    lock(&inner.state).active = false;
                }
                self.wait_for_bound_resource_close().await;

                let (sink, group, sequence) = {
                    let mut core = inner.registry.core();
                    Registry::mark_released_locked(&mut core, inner)
                };
                let ticket = match sink {
                    Some(sink) if sequence > 0 => sink(group, sequence),
                    _ => None,
                };

                let mut core = inner.registry.core();
                core.scopes.remove(&inner.id);
                inner.registry.signal_locked();
                ticket
            })
            .await
            .clone()
    }

    /// Starts closing the bound resource on a blocking thread (once) and returns its completion.
    pub(crate) fn start_bound_resource_close(&self) -> Option<Signal> {
        let mut state = lock(&self.0.state);
        if let Some(done) = &state.close_done {
            return Some(done.clone());
        }
        let close_fn = state.close_fn.take()?;
        let done = Signal::new();
        state.close_done = Some(done.clone());
        drop(state);
        let finished = FireOnDrop(done.clone());
        tokio::task::spawn_blocking(move || {
            let _finished = finished;
            if let Err(err) = close_fn() {
                tracing::warn!("Home execution resource close failed: {err}");
            }
        });
        Some(done)
    }

    async fn wait_for_bound_resource_close(&self) {
        if let Some(done) = self.start_bound_resource_close() {
            done.wait().await;
        }
    }
}
