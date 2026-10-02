//! Media relay contracts (Go: `mediaRelaySession` / `mediaRelayFactory`) and the shared session
//! limiter. The WebRTC implementation lives in [`crate::relay`].

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

/// Where a media session's upstream leg connects through and how it is labelled in logs.
#[derive(Debug, Clone, Default)]
pub struct MediaRoute {
    pub proxy_url: String,
    pub credential: String,
    pub auth_index: String,
}

/// Media failure with an optional HTTP status (0 when none applies).
#[derive(Debug, Clone)]
pub struct MediaError {
    pub status: u16,
    pub message: String,
}

impl MediaError {
    pub fn new(message: impl Into<String>) -> Self {
        MediaError { status: 0, message: message.into() }
    }
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// One relayed call: the client leg terminated locally plus a second leg to the upstream.
#[async_trait]
pub trait MediaRelaySession: Send + Sync {
    /// Applies the upstream SDP answer and returns the SDP answer for the client.
    async fn accept_upstream_answer(&self, upstream_answer: &str) -> Result<String, MediaError>;
    fn set_call_id(&self, call_id: &str);
    /// Registers a callback invoked with the reason when the session ends on its own (failure).
    fn set_close_handler(&self, handler: Box<dyn Fn(&str) + Send + Sync>);
    /// Closes the session; the peer connections are torn down in the background.
    fn close_with_reason(&self, reason: &str);
}

#[async_trait]
pub trait MediaRelayFactory: Send + Sync {
    /// Creates a session for the client's SDP offer; returns it with the SDP offer for the upstream.
    async fn new_session(&self, client_offer: &str, route: MediaRoute) -> Result<(Arc<dyn MediaRelaySession>, String), MediaError>;
}

/// Concurrent media session cap shared across config reloads (Go: `mediaSessionLimiter`).
#[derive(Default)]
pub struct MediaLimiter {
    state: Mutex<LimiterState>,
}

#[derive(Default)]
struct LimiterState {
    limit: i64,
    active: i64,
}

impl MediaLimiter {
    pub fn set_limit(&self, limit: i64) {
        self.state.lock().limit = limit;
    }

    pub fn acquire(&self) -> bool {
        let mut state = self.state.lock();
        if state.limit <= 0 || state.active >= state.limit {
            return false;
        }
        state.active += 1;
        true
    }

    pub fn release(&self) {
        let mut state = self.state.lock();
        if state.active > 0 {
            state.active -= 1;
        }
    }
}
