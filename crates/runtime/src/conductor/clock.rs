//! Time source for cooldown math and waits. Production uses [`SystemClock`]; tests use
//! [`ManualClock`], whose `sleep` advances virtual time instead of waiting.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;

#[async_trait]
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
    /// Wait between retry rounds (Go: waitForCooldown timer).
    async fn sleep(&self, d: Duration);
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    async fn sleep(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

/// Controllable clock: `sleep` returns immediately after moving `now` forward by the duration.
pub struct ManualClock {
    now: Mutex<DateTime<Utc>>,
    sleeps: Mutex<Vec<Duration>>,
}

impl ManualClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(start),
            sleeps: Mutex::new(Vec::new()),
        }
    }

    pub fn advance(&self, d: Duration) {
        let mut now = self.now.lock();
        *now += chrono::Duration::from_std(d).unwrap_or(chrono::Duration::zero());
    }

    /// Durations the conductor asked to wait, in order (jitter included).
    pub fn sleeps(&self) -> Vec<Duration> {
        self.sleeps.lock().clone()
    }
}

#[async_trait]
impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock()
    }

    async fn sleep(&self, d: Duration) {
        self.sleeps.lock().push(d);
        self.advance(d);
        tokio::task::yield_now().await;
    }
}
