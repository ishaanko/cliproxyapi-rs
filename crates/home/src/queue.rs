//! In-memory usage and error queues behind the Redis-protocol outputs (Go: `internal/redisqueue`
//! queue.go and usage_toggle.go).
//!
//! Usage records are buffered for `redis-usage-queue-retention-seconds` until a consumer pops
//! them (`LPOP`/`RPOP usage`, `GET /usage-queue`) or hands them straight to subscribers
//! (`SUBSCRIBE usage`). Error events only reach live `SUBSCRIBE errors` subscribers. The queues
//! are process-wide, like the Go package variables.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::mpsc;

pub const DEFAULT_RETENTION_SECONDS: i64 = 60;
pub const MAX_RETENTION_SECONDS: i64 = 3600;
const USAGE_SUBSCRIBER_BUFFER: usize = 256;
const ERROR_SUBSCRIBER_BUFFER: usize = 256;

/// First payload every `SUBSCRIBE usage` subscriber receives.
pub const USAGE_SUPPORT_REFRESH_PAYLOAD: &[u8] = br#"{"support_refresh":true}"#;
/// Broadcast to usage subscribers when credentials change.
pub const USAGE_REFRESH_PAYLOAD: &[u8] = br#"{"refresh":true}"#;

static ENABLED: AtomicBool = AtomicBool::new(false);
static RETENTION_SECONDS: AtomicI64 = AtomicI64::new(DEFAULT_RETENTION_SECONDS);
static USAGE_STATISTICS_ENABLED: AtomicBool = AtomicBool::new(true);
static GLOBAL: Queue = Queue::new();
static ERROR_GLOBAL: Queue = Queue::new();

struct Inner {
    items: VecDeque<(Instant, Vec<u8>)>,
    subscribers: BTreeMap<u64, mpsc::Sender<Vec<u8>>>,
    next_id: u64,
}

/// One retention buffer with live subscribers.
pub struct Queue {
    inner: Mutex<Inner>,
}

/// Receiving end of a subscription. Dropping it unsubscribes. [`Subscription::recv`] yields
/// `None` once the queue dropped the subscriber (slow consumer, queue disabled) or it was closed.
pub struct Subscription {
    rx: mpsc::Receiver<Vec<u8>>,
    queue: &'static Queue,
    id: u64,
}

impl Subscription {
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.queue.unsubscribe(self.id);
    }
}

impl Queue {
    const fn new() -> Self {
        Queue {
            inner: Mutex::new(Inner { items: VecDeque::new(), subscribers: BTreeMap::new(), next_id: 0 }),
        }
    }

    fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.items.clear();
        // Dropping the senders closes every subscription.
        inner.subscribers.clear();
    }

    fn enqueue(&self, payload: &[u8]) {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        prune(&mut inner.items, now);
        inner.items.push_back((now, payload.to_vec()));
    }

    /// Sends `payload` to every subscriber; one whose buffer is full is dropped. Returns whether
    /// any subscriber existed.
    fn publish_to_subscribers(&self, payload: &[u8]) -> bool {
        let mut inner = self.inner.lock();
        if inner.subscribers.is_empty() {
            return false;
        }
        inner.subscribers.retain(|_, tx| tx.try_send(payload.to_vec()).is_ok());
        true
    }

    fn subscribe(&'static self, buffer: usize, initial: Option<&[u8]>) -> Subscription {
        let (tx, rx) = mpsc::channel(buffer);
        if let Some(initial) = initial {
            let _ = tx.try_send(initial.to_vec());
        }
        let mut inner = self.inner.lock();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.subscribers.insert(id, tx);
        Subscription { rx, queue: self, id }
    }

    fn unsubscribe(&self, id: u64) {
        self.inner.lock().subscribers.remove(&id);
    }

    fn pop_oldest(&self, count: usize) -> Vec<Vec<u8>> {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        prune(&mut inner.items, now);
        let n = count.min(inner.items.len());
        inner.items.drain(..n).map(|(_, p)| p).collect()
    }
}

fn prune(items: &mut VecDeque<(Instant, Vec<u8>)>, now: Instant) {
    let mut window = RETENTION_SECONDS.load(Ordering::Relaxed);
    if window <= 0 {
        window = DEFAULT_RETENTION_SECONDS;
    }
    let Some(cutoff) = now.checked_sub(Duration::from_secs(window as u64)) else {
        return;
    };
    while items.front().is_some_and(|(at, _)| *at < cutoff) {
        items.pop_front();
    }
}

/// Turns the queue on or off. Turning it off drops buffered records and closes subscribers.
pub fn set_enabled(value: bool) {
    ENABLED.store(value, Ordering::SeqCst);
    if !value {
        GLOBAL.clear();
        ERROR_GLOBAL.clear();
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

/// Retention window; non-positive values mean the 60s default, values above 3600 are capped.
pub fn set_retention_seconds(value: i64) {
    let normalized = if value <= 0 {
        DEFAULT_RETENTION_SECONDS
    } else {
        value.min(MAX_RETENTION_SECONDS)
    };
    RETENTION_SECONDS.store(normalized, Ordering::SeqCst);
}

/// Mirrors `usage-statistics-enabled`: whether usage records are published at all.
pub fn set_usage_statistics_enabled(value: bool) {
    USAGE_STATISTICS_ENABLED.store(value, Ordering::SeqCst);
}

pub fn usage_statistics_enabled() -> bool {
    USAGE_STATISTICS_ENABLED.load(Ordering::SeqCst)
}

/// Publishes a usage record: to live subscribers when there are any, else into the buffer.
pub fn enqueue(payload: &[u8]) {
    if !enabled() || payload.is_empty() {
        return;
    }
    if GLOBAL.publish_to_subscribers(payload) {
        return;
    }
    GLOBAL.enqueue(payload);
}

/// Publishes an error event to `SUBSCRIBE errors` subscribers (never buffered).
pub fn enqueue_error(payload: &[u8]) {
    if !enabled() || payload.is_empty() {
        return;
    }
    ERROR_GLOBAL.publish_to_subscribers(payload);
}

/// Pops up to `count` of the oldest buffered usage records.
pub fn pop_oldest(count: usize) -> Vec<Vec<u8>> {
    if !enabled() || count == 0 {
        return Vec::new();
    }
    GLOBAL.pop_oldest(count)
}

/// Live usage feed; the first message is the support-refresh marker.
pub fn subscribe_usage() -> Subscription {
    GLOBAL.subscribe(USAGE_SUBSCRIBER_BUFFER, Some(USAGE_SUPPORT_REFRESH_PAYLOAD))
}

/// Live error-event feed.
pub fn subscribe_errors() -> Subscription {
    ERROR_GLOBAL.subscribe(ERROR_SUBSCRIBER_BUFFER, None)
}

/// Tells usage subscribers to re-read credential state (Go: `NotifyUsageRefresh`; also works while
/// the queue is disabled, like Go).
pub fn notify_usage_refresh() {
    GLOBAL.publish_to_subscribers(USAGE_REFRESH_PAYLOAD);
}

/// Serializes tests that touch the process-wide queue state.
#[cfg(test)]
pub(crate) fn test_lock() -> parking_lot::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)] // test_lock serializes tests over the global queue
mod tests {
    use super::*;

    #[test]
    fn disabled_queue_drops_and_pops_nothing() {
        let _g = test_lock();
        set_enabled(false);
        enqueue(b"a");
        set_enabled(true);
        assert!(pop_oldest(10).is_empty());
        set_enabled(false);
    }

    #[test]
    fn fifo_pop_and_clear_on_disable() {
        let _g = test_lock();
        set_enabled(true);
        set_retention_seconds(0);
        for p in [&b"a"[..], b"b", b"c"] {
            enqueue(p);
        }
        assert_eq!(pop_oldest(2), vec![b"a".to_vec(), b"b".to_vec()]);
        assert!(pop_oldest(0).is_empty());
        set_enabled(false);
        set_enabled(true);
        assert!(pop_oldest(10).is_empty());
        set_enabled(false);
    }

    #[test]
    fn retention_normalizes_and_expires_old_entries() {
        let _g = test_lock();
        set_enabled(true);
        set_retention_seconds(99_999);
        assert_eq!(RETENTION_SECONDS.load(Ordering::SeqCst), MAX_RETENTION_SECONDS);
        set_retention_seconds(-3);
        assert_eq!(RETENTION_SECONDS.load(Ordering::SeqCst), DEFAULT_RETENTION_SECONDS);
        let mut items: VecDeque<(Instant, Vec<u8>)> = VecDeque::new();
        let now = Instant::now();
        if let Some(old) = now.checked_sub(Duration::from_secs(120)) {
            items.push_back((old, b"old".to_vec()));
            items.push_back((now, b"new".to_vec()));
            prune(&mut items, now);
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].1, b"new");
        }
        set_enabled(false);
    }

    #[tokio::test]
    async fn subscribers_get_support_refresh_then_records_instead_of_the_buffer() {
        let _g = test_lock();
        set_enabled(true);
        let mut sub = subscribe_usage();
        assert_eq!(sub.recv().await.unwrap(), USAGE_SUPPORT_REFRESH_PAYLOAD);
        enqueue(b"{\"id\":1}");
        assert_eq!(sub.recv().await.unwrap(), b"{\"id\":1}");
        notify_usage_refresh();
        assert_eq!(sub.recv().await.unwrap(), USAGE_REFRESH_PAYLOAD);
        assert!(pop_oldest(10).is_empty());
        drop(sub);
        enqueue(b"buffered");
        assert_eq!(pop_oldest(10), vec![b"buffered".to_vec()]);
        set_enabled(false);
    }

    #[tokio::test]
    async fn error_events_reach_only_live_subscribers_and_slow_ones_are_dropped() {
        let _g = test_lock();
        set_enabled(true);
        enqueue_error(b"lost");
        let mut sub = subscribe_errors();
        enqueue_error(b"seen");
        assert_eq!(sub.recv().await.unwrap(), b"seen");
        // A subscriber that never reads is dropped once its buffer fills.
        for i in 0..=ERROR_SUBSCRIBER_BUFFER {
            enqueue_error(format!("{i}").as_bytes());
        }
        let mut got = 0;
        while sub.recv().await.is_some() {
            got += 1;
        }
        assert_eq!(got, ERROR_SUBSCRIBER_BUFFER);
        set_enabled(false);
    }

    #[tokio::test]
    async fn disabling_closes_subscribers() {
        let _g = test_lock();
        set_enabled(true);
        let mut sub = subscribe_usage();
        assert!(sub.recv().await.is_some());
        set_enabled(false);
        assert!(sub.recv().await.is_none());
    }
}
