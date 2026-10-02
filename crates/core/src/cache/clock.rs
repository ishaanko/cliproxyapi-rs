//! Controllable time source for cache TTL logic (Go uses `time.Now()` directly; AGENTS.md asks
//! for controllable clocks in expiry tests).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

/// Monotonic time as a duration since an arbitrary origin.
pub type Timestamp = Duration;

static PROCESS_START: LazyLock<Instant> = LazyLock::new(Instant::now);

#[derive(Debug)]
enum Source {
    Real,
    /// Nanoseconds since the mock origin; only moves via [`Clock::advance`].
    Mock(AtomicU64),
}

/// A shareable clock: real monotonic time, or a mock that only moves when advanced.
#[derive(Debug, Clone)]
pub struct Clock(Arc<Source>);

impl Clock {
    pub fn real() -> Self {
        Self(Arc::new(Source::Real))
    }

    /// A mock clock starting one day after its origin (so "older than the TTL" timestamps are
    /// representable without underflow).
    pub fn mock() -> Self {
        Self(Arc::new(Source::Mock(AtomicU64::new(
            Duration::from_secs(24 * 3600).as_nanos() as u64,
        ))))
    }

    pub fn now(&self) -> Timestamp {
        match &*self.0 {
            Source::Real => PROCESS_START.elapsed() + Duration::from_secs(24 * 3600),
            Source::Mock(nanos) => Duration::from_nanos(nanos.load(Ordering::SeqCst)),
        }
    }

    /// Moves a mock clock forward; a no-op on the real clock.
    pub fn advance(&self, by: Duration) {
        if let Source::Mock(nanos) = &*self.0 {
            nanos.fetch_add(by.as_nanos() as u64, Ordering::SeqCst);
        }
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::real()
    }
}
