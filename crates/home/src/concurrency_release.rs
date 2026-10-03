//! Cumulative concurrency release flusher (Go: `internal/home/concurrency_release.go`).
//!
//! Ending an accounted execution bumps a per-(credential, model) release sequence. The flusher
//! sends the latest sequence of every dirty group to Home, backing off after failures, and
//! completes the tickets of everything Home acknowledged.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cpa_config::CredentialConcurrencyConfig;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::client::Cancel;
use crate::error::HomeError;
use crate::executionregistry::{Done, ReleaseGroup, ReleaseTicket};
use crate::requests::ConcurrencyReleaseFrame;

/// Sends one release frame to Home.
pub type SendFn = Arc<
    dyn Fn(ConcurrencyReleaseFrame) -> Pin<Box<dyn Future<Output = Result<(), HomeError>> + Send>> + Send + Sync,
>;
/// Supplies the current limiter timings.
pub type ConfigProvider = Arc<dyn Fn() -> CredentialConcurrencyConfig + Send + Sync>;

#[derive(Default)]
struct ReleaseState {
    latest: i64,
    acked: i64,
    waiters: BTreeMap<i64, Vec<Arc<Done>>>,
}

#[derive(Default)]
struct FState {
    groups: HashMap<ReleaseGroup, ReleaseState>,
    config_provider: Option<ConfigProvider>,
    send: Option<SendFn>,
}

struct Inner {
    state: Mutex<FState>,
    flush_interval: Duration,
    max_backoff: Duration,
    wake: Notify,
    force: AtomicBool,
}

#[derive(Clone)]
pub struct ReleaseFlusher {
    inner: Arc<Inner>,
}

#[derive(Clone, Copy)]
struct Timings {
    flush_interval: Duration,
    max_backoff: Duration,
}

impl ReleaseFlusher {
    /// A flusher that reads timing updates from the limiter configuration once a provider is set.
    pub fn new() -> ReleaseFlusher {
        Self::with_timings(Duration::ZERO, Duration::ZERO)
    }

    pub fn with_timings(flush_interval: Duration, max_backoff: Duration) -> ReleaseFlusher {
        ReleaseFlusher {
            inner: Arc::new(Inner {
                state: Mutex::new(FState::default()),
                flush_interval,
                max_backoff,
                wake: Notify::new(),
                force: AtomicBool::new(false),
            }),
        }
    }

    pub fn set_config_provider(&self, provider: Option<ConfigProvider>) {
        self.inner.state.lock().config_provider = provider;
        self.inner.wake.notify_one();
    }

    /// Replaces the Home lifetime used for subsequent release attempts.
    pub fn set_sender(&self, send: Option<SendFn>) {
        self.inner.state.lock().send = send;
        self.inner.wake.notify_one();
    }

    /// Records the latest cumulative sequence of a release group and returns a ticket completed
    /// when Home acknowledges it.
    pub fn mark_dirty(&self, group: ReleaseGroup, sequence: i64) -> Option<ReleaseTicket> {
        if sequence <= 0 || group.credential_id.is_empty() || group.model.is_empty() {
            return None;
        }
        let done = Arc::new(Done::default());
        {
            let mut s = self.inner.state.lock();
            let state = s.groups.entry(group.clone()).or_default();
            if sequence <= state.acked {
                done.complete();
            } else {
                state.waiters.entry(sequence).or_default().push(done.clone());
                if sequence > state.latest {
                    state.latest = sequence;
                }
            }
        }
        self.inner.wake.notify_one();
        ReleaseTicket::new(group, sequence, done)
    }

    fn timings(&self) -> Timings {
        let defaults = CredentialConcurrencyConfig::default().with_defaults();
        let mut t = Timings { flush_interval: self.inner.flush_interval, max_backoff: self.inner.max_backoff };
        let provider = self.inner.state.lock().config_provider.clone();
        if let Some(provider) = provider {
            let cfg = provider().with_defaults();
            t.flush_interval = cfg.release_flush_interval.to_std();
            t.max_backoff = cfg.release_max_backoff.to_std();
        }
        if t.flush_interval.is_zero() {
            t.flush_interval = defaults.release_flush_interval.to_std();
        }
        if t.max_backoff < t.flush_interval {
            t.max_backoff = t.flush_interval;
        }
        t
    }

    fn next_delay(&self, delay: Duration, failed: bool) -> (Duration, bool) {
        let t = self.timings();
        if !failed {
            return (t.flush_interval, false);
        }
        let delay = (delay * 2).max(t.flush_interval).min(t.max_backoff);
        (delay, true)
    }

    /// Sends dirty groups until `cancel` fires.
    pub async fn run(&self, cancel: &Cancel) {
        let mut delay = self.timings().flush_interval;
        let mut backing_off = false;
        let timer = tokio::time::sleep(Duration::ZERO);
        tokio::pin!(timer);
        loop {
            tokio::select! {
                _ = cancel.wait() => return,
                _ = self.inner.wake.notified() => {
                    if self.inner.force.swap(false, Ordering::SeqCst) {
                        let failed = self.flush().await;
                        (delay, backing_off) = self.next_delay(delay, failed);
                        timer.as_mut().reset(Instant::now() + delay);
                    } else if !backing_off {
                        timer.as_mut().reset(Instant::now());
                    }
                }
                _ = &mut timer => {
                    let failed = self.flush().await;
                    (delay, backing_off) = self.next_delay(delay, failed);
                    timer.as_mut().reset(Instant::now() + delay);
                }
            }
        }
    }

    /// Sends every pending group once; returns whether any send failed.
    async fn flush(&self) -> bool {
        let (send, pending): (Option<SendFn>, Vec<(ReleaseGroup, i64)>) = {
            let s = self.inner.state.lock();
            (
                s.send.clone(),
                s.groups.iter().filter(|(_, st)| st.latest > st.acked).map(|(g, st)| (g.clone(), st.latest)).collect(),
            )
        };
        let Some(send) = send else { return false };
        let mut failed = false;
        for (group, sequence) in pending {
            let frame = ConcurrencyReleaseFrame {
                credential_id: group.credential_id.clone(),
                model: group.model.clone(),
                release_seq: sequence,
            };
            if send(frame).await.is_err() {
                failed = true;
                continue;
            }
            let mut s = self.inner.state.lock();
            let state = s.groups.entry(group).or_default();
            if sequence > state.acked {
                state.acked = sequence;
                let acked = state.acked;
                let ready: Vec<i64> = state.waiters.range(..=acked).map(|(k, _)| *k).collect();
                for key in ready {
                    for done in state.waiters.remove(&key).unwrap_or_default() {
                        done.complete();
                    }
                }
            }
        }
        failed
    }

    fn idle(&self) -> bool {
        self.inner.state.lock().groups.values().all(|st| st.latest <= st.acked)
    }

    /// Waits for all currently dirty groups to be acknowledged within `timeout`.
    pub async fn flush_all(&self, timeout: Duration) -> Result<(), HomeError> {
        self.inner.force.store(true, Ordering::SeqCst);
        self.inner.wake.notify_one();
        let wait = async {
            while !self.idle() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| HomeError::Other("context deadline exceeded".into()))
    }
}

impl Default for ReleaseFlusher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::Kill;
    use std::sync::atomic::AtomicUsize;

    fn group() -> ReleaseGroup {
        ReleaseGroup { credential_id: "cred".into(), model: "m".into() }
    }

    fn sender(fail_first: usize, frames: Arc<Mutex<Vec<i64>>>) -> SendFn {
        let calls = Arc::new(AtomicUsize::new(0));
        Arc::new(move |frame| {
            let calls = calls.clone();
            let frames = frames.clone();
            Box::pin(async move {
                if calls.fetch_add(1, Ordering::SeqCst) < fail_first {
                    return Err(HomeError::Io("down".into()));
                }
                frames.lock().push(frame.release_seq);
                Ok(())
            })
        })
    }

    #[tokio::test]
    async fn latest_sequence_is_sent_and_tickets_complete_on_ack() {
        let flusher = ReleaseFlusher::with_timings(Duration::from_millis(5), Duration::from_millis(20));
        let frames = Arc::new(Mutex::new(vec![]));
        flusher.set_sender(Some(sender(0, frames.clone())));
        let t1 = flusher.mark_dirty(group(), 1).unwrap();
        let t2 = flusher.mark_dirty(group(), 2).unwrap();
        let cancel = Arc::new(Kill::default());
        let run = {
            let (flusher, cancel) = (flusher.clone(), cancel.clone());
            tokio::spawn(async move { flusher.run(&cancel).await })
        };
        t1.wait(Duration::from_secs(2)).await.unwrap();
        t2.wait(Duration::from_secs(2)).await.unwrap();
        assert_eq!(*frames.lock(), vec![2]);
        // Already acknowledged sequences complete immediately.
        flusher.mark_dirty(group(), 1).unwrap().wait(Duration::from_millis(10)).await.unwrap();
        assert!(flusher.mark_dirty(ReleaseGroup { credential_id: String::new(), model: "m".into() }, 1).is_none());
        cancel.kill();
        run.await.unwrap();
    }

    #[tokio::test]
    async fn failures_back_off_and_retry() {
        let flusher = ReleaseFlusher::with_timings(Duration::from_millis(5), Duration::from_millis(20));
        let frames = Arc::new(Mutex::new(vec![]));
        flusher.set_sender(Some(sender(2, frames.clone())));
        let ticket = flusher.mark_dirty(group(), 1).unwrap();
        let cancel = Arc::new(Kill::default());
        let run = {
            let (flusher, cancel) = (flusher.clone(), cancel.clone());
            tokio::spawn(async move { flusher.run(&cancel).await })
        };
        ticket.wait(Duration::from_secs(3)).await.unwrap();
        assert_eq!(*frames.lock(), vec![1]);
        flusher.flush_all(Duration::from_millis(100)).await.unwrap();
        cancel.kill();
        run.await.unwrap();
    }

    #[test]
    fn next_delay_doubles_until_the_cap_and_resets_on_success() {
        let flusher = ReleaseFlusher::with_timings(Duration::from_millis(10), Duration::from_millis(35));
        assert_eq!(flusher.next_delay(Duration::from_millis(10), true), (Duration::from_millis(20), true));
        assert_eq!(flusher.next_delay(Duration::from_millis(20), true), (Duration::from_millis(35), true));
        assert_eq!(flusher.next_delay(Duration::from_millis(35), false), (Duration::from_millis(10), false));
    }
}
