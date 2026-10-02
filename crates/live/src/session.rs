//! Call sessions kept for sideband and hangup (Go: `sessionStore` in sideband.go).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::task::AbortHandle;

use crate::media::MediaRelaySession;
use crate::util::is_call_id;

/// How long an unclaimed call session is kept.
pub const SESSION_LIFETIME: Duration = Duration::from_secs(3600);

/// Closers run once when a session ends (sideband sockets).
#[derive(Default)]
pub struct SessionResources {
    state: Mutex<ResourceState>,
}

#[derive(Default)]
struct ResourceState {
    closed: bool,
    closers: Vec<Box<dyn FnOnce() + Send>>,
}

impl SessionResources {
    /// Registers closers; runs them immediately when the session already ended.
    pub fn add(&self, closers: Vec<Box<dyn FnOnce() + Send>>) {
        let mut state = self.state.lock();
        if !state.closed {
            state.closers.extend(closers);
            return;
        }
        drop(state);
        closers.into_iter().for_each(|c| c());
    }

    pub fn close(&self) {
        let closers = {
            let mut state = self.state.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            std::mem::take(&mut state.closers)
        };
        closers.into_iter().for_each(|c| c());
    }
}

/// A WebRTC call known to this proxy.
#[derive(Clone, Default)]
pub struct LiveSession {
    pub call_id: String,
    pub auth_id: String,
    pub model: String,
    pub owner_principal: String,
    pub owner_provider: String,
    pub client_secret_principal: String,
    pub media: Option<Arc<dyn MediaRelaySession>>,
    pub resources: Option<Arc<SessionResources>>,
    pub token: u64,
}

struct Stored {
    session: LiveSession,
    claimed: bool,
    timer: Option<AbortHandle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    Missing,
    Busy,
    Acquired,
}

struct StoreInner {
    state: Mutex<StoreState>,
    lifetime: Mutex<Duration>,
}

#[derive(Default)]
struct StoreState {
    next: u64,
    sessions: HashMap<String, Stored>,
}

/// Cheaply cloneable handle to the session map.
#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<StoreInner>,
}

impl Default for SessionStore {
    fn default() -> Self {
        SessionStore {
            inner: Arc::new(StoreInner { state: Mutex::new(StoreState::default()), lifetime: Mutex::new(SESSION_LIFETIME) }),
        }
    }
}

/// `endLiveSession`: releases sideband sockets and the media relay.
pub fn end_live_session(session: &LiveSession, reason: &str) {
    if let Some(resources) = &session.resources {
        resources.close();
    }
    if let Some(media) = &session.media {
        media.close_with_reason(reason);
    }
}

impl SessionStore {
    #[cfg(test)]
    pub(crate) fn set_lifetime(&self, lifetime: Duration) {
        *self.inner.lifetime.lock() = lifetime;
    }

    fn expiry(&self) -> Duration {
        let lifetime = *self.inner.lifetime.lock();
        if lifetime.is_zero() { SESSION_LIFETIME } else { lifetime }
    }

    fn arm_timer(&self, call_id: &str, token: u64) -> Option<AbortHandle> {
        let handle = tokio::runtime::Handle::try_current().ok()?;
        let store = self.clone();
        let call_id = call_id.to_string();
        let delay = self.expiry();
        Some(
            handle
                .spawn(async move {
                    tokio::time::sleep(delay).await;
                    store.expire(&call_id, token);
                })
                .abort_handle(),
        )
    }

    /// `put`: stores a session under its call id, replacing (and closing) a previous one. Returns
    /// the stored copy (with `call_id` and `token` set), or an empty session when the id is invalid.
    pub fn put(&self, call_id: &str, mut session: LiveSession) -> LiveSession {
        if !is_call_id(call_id) {
            end_live_session(&session, "invalid_call_id");
            return LiveSession::default();
        }
        if session.resources.is_none() {
            session.resources = Some(Arc::new(SessionResources::default()));
        }
        let previous = {
            let mut state = self.inner.state.lock();
            state.next += 1;
            session.call_id = call_id.to_string();
            session.token = state.next;
            let timer = self.arm_timer(call_id, session.token);
            state.sessions.insert(call_id.to_string(), Stored { session: session.clone(), claimed: false, timer })
        };
        if let Some(previous) = previous {
            if let Some(timer) = &previous.timer {
                timer.abort();
            }
            if let (Some(old), Some(new)) = (&previous.session.resources, &session.resources)
                && !Arc::ptr_eq(old, new)
            {
                old.close();
            }
            if let Some(old) = &previous.session.media {
                let same = session.media.as_ref().is_some_and(|new| Arc::ptr_eq(old, new));
                if !same {
                    old.close_with_reason("session_replaced");
                }
            }
        }
        session
    }

    /// `claim`: exclusive use of a session by one sideband connection.
    pub fn claim(&self, call_id: &str) -> (LiveSession, Claim) {
        if !is_call_id(call_id) {
            return (LiveSession::default(), Claim::Missing);
        }
        let mut state = self.inner.state.lock();
        let Some(entry) = state.sessions.get_mut(call_id) else {
            return (LiveSession::default(), Claim::Missing);
        };
        if entry.claimed {
            return (LiveSession::default(), Claim::Busy);
        }
        entry.claimed = true;
        if let Some(timer) = entry.timer.take() {
            timer.abort();
        }
        (entry.session.clone(), Claim::Acquired)
    }

    /// `release`: returns a claimed session to the pool and restarts its expiry timer.
    pub fn release(&self, session: &LiveSession) {
        if session.call_id.is_empty() {
            return;
        }
        let mut state = self.inner.state.lock();
        let timer = self.arm_timer(&session.call_id, session.token);
        let Some(entry) = state.sessions.get_mut(&session.call_id) else {
            return;
        };
        if entry.session.token != session.token || !entry.claimed {
            if let Some(timer) = timer {
                timer.abort();
            }
            return;
        }
        entry.claimed = false;
        entry.timer = timer;
    }

    /// `complete`: removes the session and ends it with `reason`.
    pub fn complete(&self, session: &LiveSession, reason: &str) {
        if session.call_id.is_empty() {
            end_live_session(session, reason);
            return;
        }
        let entry = {
            let mut state = self.inner.state.lock();
            match state.sessions.get(&session.call_id) {
                Some(e) if e.session.token == session.token => state.sessions.remove(&session.call_id),
                _ => None,
            }
        };
        if let Some(entry) = entry {
            if let Some(timer) = &entry.timer {
                timer.abort();
            }
            end_live_session(&entry.session, reason);
        }
    }

    /// `closeAll`: ends every session (server shutdown).
    pub fn close_all(&self, reason: &str) {
        let entries: Vec<Stored> = {
            let mut state = self.inner.state.lock();
            state.sessions.drain().map(|(_, e)| e).collect()
        };
        for entry in entries {
            if let Some(timer) = &entry.timer {
                timer.abort();
            }
            end_live_session(&entry.session, reason);
        }
    }

    fn expire(&self, call_id: &str, token: u64) {
        let entry = {
            let mut state = self.inner.state.lock();
            match state.sessions.get(call_id) {
                Some(e) if e.session.token == token && !e.claimed => state.sessions.remove(call_id),
                _ => None,
            }
        };
        if let Some(entry) = entry {
            end_live_session(&entry.session, "session_expired");
        }
    }

    /// `peek`: the stored session without claiming it.
    pub fn peek(&self, call_id: &str) -> Option<LiveSession> {
        self.inner.state.lock().sessions.get(call_id).map(|e| e.session.clone())
    }
}
