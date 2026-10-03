//! Forwards application log lines to Home (Go: internal/logging/home_app_log_forwarder.go).
//!
//! A [`HomeAppLogForwarder`] owns a bounded queue and a background sender. Forwarders register with
//! a process-wide [`AppLogMux`] that the [`HomeAppLogLayer`] tracing layer feeds, so one layer
//! serves every forwarder (the Go code does the same with one logrus hook). The service layer
//! drives the lifecycle: `start` once, `bind` when Home's control connection is healthy,
//! `deactivate` when it goes away, `stop` on shutdown. A forwarder without a bound client drops
//! every line; a full queue drops lines instead of blocking the logger.

use std::cell::OnceCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Local};
use cpa_home::{Client, HomeError};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

use crate::logging::{go_json_html_escape, render_line};

/// Queue size used when `start` is given zero.
pub const DEFAULT_QUEUE_SIZE: usize = 1024;

type PushFuture<'a> = Pin<Box<dyn Future<Output = Result<(), HomeError>> + Send + 'a>>;

/// The Home calls the forwarder needs (Go: `homeAppLogClient`). [`Client`] implements it; unit
/// tests substitute stubs.
pub(crate) trait AppLogClient: Send + Sync {
    fn heartbeat_ok(&self) -> bool;
    fn rpush_app_log<'a>(&'a self, payload: &'a [u8]) -> PushFuture<'a>;
}

impl AppLogClient for Client {
    fn heartbeat_ok(&self) -> bool {
        Client::heartbeat_ok(self)
    }

    fn rpush_app_log<'a>(&'a self, payload: &'a [u8]) -> PushFuture<'a> {
        Box::pin(Client::rpush_app_log(self, payload))
    }
}

/// JSON pushed to Home's `app-log` list (Go: `homeAppLogPayload`).
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct AppLogRecord {
    pub line: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub level: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub timestamp: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub request_id: String,
}

/// A record plus the client that was bound when it was logged.
struct Queued {
    record: AppLogRecord,
    client: Arc<dyn AppLogClient>,
}

fn same_client(a: &Arc<dyn AppLogClient>, b: &Arc<dyn AppLogClient>) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
}

/// State shared between the logger side (`fire`) and the background sender.
struct Inner {
    enabled: AtomicBool,
    stopped: AtomicBool,
    owner: Mutex<Option<Arc<dyn AppLogClient>>>,
    tx: mpsc::Sender<Queued>,
    stop: CancellationToken,
}

impl Inner {
    fn new(queue_size: usize) -> (Arc<Inner>, mpsc::Receiver<Queued>) {
        let size = if queue_size == 0 { DEFAULT_QUEUE_SIZE } else { queue_size };
        let (tx, rx) = mpsc::channel(size);
        let inner = Inner {
            enabled: AtomicBool::new(true),
            stopped: AtomicBool::new(false),
            owner: Mutex::new(None),
            tx,
            stop: CancellationToken::new(),
        };
        (Arc::new(inner), rx)
    }

    fn client(&self) -> Option<Arc<dyn AppLogClient>> {
        self.owner.lock().clone()
    }

    fn bind(&self, client: Arc<dyn AppLogClient>) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let mut owner = self.owner.lock();
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        *owner = Some(client);
        self.enabled.store(true, Ordering::SeqCst);
    }

    fn deactivate(&self, client: &Arc<dyn AppLogClient>) {
        let mut owner = self.owner.lock();
        if owner.as_ref().is_some_and(|o| same_client(o, client)) {
            *owner = None;
        }
    }

    /// Logger-side entry (Go: `Fire`): queues a record when a healthy owner is bound. `build` runs
    /// only after those checks so unbound forwarders cost nothing per log line.
    fn fire(&self, build: impl FnOnce() -> AppLogRecord) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }
        let Some(client) = self.client() else { return };
        if !client.heartbeat_ok() {
            return;
        }
        let record = build();
        if record.line.trim().is_empty() {
            return;
        }
        let _ = self.tx.try_send(Queued { record, client });
    }

    /// Sends one record to Home if its client still owns the forwarder (Go: `forward`).
    async fn forward(&self, record: &AppLogRecord, client: Option<Arc<dyn AppLogClient>>) {
        let Some(client) = client.or_else(|| self.client()) else { return };
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }
        if !self.client().is_some_and(|owner| same_client(&owner, &client)) {
            return;
        }
        if !client.heartbeat_ok() {
            return;
        }
        let Ok(raw) = serde_json::to_string(record) else { return };
        let raw = go_json_html_escape(raw);
        if let Err(err) = client.rpush_app_log(raw.as_bytes()).await
            && is_home_app_log_unsupported(&err)
        {
            self.disable_if_current_owner(&client);
        }
    }

    /// An old owner's late "unsupported" reply must not disable a newer owner.
    fn disable_if_current_owner(&self, client: &Arc<dyn AppLogClient>) {
        let owner = self.owner.lock();
        if owner.as_ref().is_some_and(|o| same_client(o, client)) {
            self.enabled.store(false, Ordering::SeqCst);
        }
    }

    async fn run(self: Arc<Self>, mut rx: mpsc::Receiver<Queued>) {
        loop {
            tokio::select! {
                biased;
                () = self.stop.cancelled() => return,
                item = rx.recv() => match item {
                    Some(Queued { record, client }) => self.forward(&record, Some(client)).await,
                    None => return,
                },
            }
        }
    }
}

/// Home answers `unsupported key` / `unknown command` / `unsupported command` when it predates
/// app-log forwarding; the error chain is searched like Go's `errors.Unwrap` loop.
fn is_home_app_log_unsupported(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = current {
        let msg = e.to_string().trim().to_lowercase();
        if !msg.is_empty()
            && ["unsupported key", "unknown command", "unsupported command"].iter().any(|m| msg.contains(m))
        {
            return true;
        }
        current = e.source();
    }
    false
}

/// Process-wide set of forwarders fed by [`HomeAppLogLayer`] (Go: `homeAppLogMux`).
#[derive(Default)]
pub struct AppLogMux {
    targets: RwLock<Vec<Arc<Inner>>>,
}

impl AppLogMux {
    /// The mux the production layer and [`HomeAppLogForwarder::start`] share.
    pub fn global() -> Arc<AppLogMux> {
        static GLOBAL: OnceLock<Arc<AppLogMux>> = OnceLock::new();
        GLOBAL.get_or_init(Arc::default).clone()
    }

    fn register(&self, target: Arc<Inner>) {
        self.targets.write().push(target);
    }

    fn unregister(&self, target: &Arc<Inner>) {
        self.targets.write().retain(|t| !Arc::ptr_eq(t, target));
    }

    pub fn target_count(&self) -> usize {
        self.targets.read().len()
    }

    /// Formats the event once and offers it to every target.
    fn fire(&self, event: &Event<'_>) {
        let targets = self.targets.read().clone();
        if targets.is_empty() {
            return;
        }
        let record: OnceCell<AppLogRecord> = OnceCell::new();
        for target in &targets {
            target.fire(|| record.get_or_init(|| record_from_event(event)).clone());
        }
    }
}

fn record_from_event(event: &Event<'_>) -> AppLogRecord {
    let line = render_line(event);
    AppLogRecord {
        line: line.text,
        level: line.level.to_string(),
        timestamp: rfc3339_nano(line.time),
        request_id: app_log_request_id(line.request_id.as_deref()),
    }
}

/// `appLogRequestID`: trimmed, with the `--------` placeholder treated as absent.
fn app_log_request_id(id: Option<&str>) -> String {
    match id.map(str::trim) {
        Some("--------") | None => String::new(),
        Some(id) => id.to_string(),
    }
}

/// Go's `time.RFC3339Nano`: trailing zeros trimmed from the fraction, `Z` for UTC.
fn rfc3339_nano(t: DateTime<Local>) -> String {
    use std::fmt::Write as _;
    let mut out = t.format("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = t.timestamp_subsec_nanos() % 1_000_000_000;
    if nanos != 0 {
        let frac = format!("{nanos:09}");
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    let offset = t.offset().local_minus_utc();
    if offset == 0 {
        out.push('Z');
    } else {
        let sign = if offset < 0 { '-' } else { '+' };
        let abs = offset.abs();
        let _ = write!(out, "{sign}{:02}:{:02}", abs / 3600, abs % 3600 / 60);
    }
    out
}

/// Tracing layer that offers every event to the forwarders registered on its mux. Installed once
/// by [`crate::logging::init`]; it sits behind the level filter, like the logrus hook.
pub struct HomeAppLogLayer {
    mux: Arc<AppLogMux>,
}

impl HomeAppLogLayer {
    pub fn global() -> Self {
        Self::new(AppLogMux::global())
    }

    pub fn new(mux: Arc<AppLogMux>) -> Self {
        HomeAppLogLayer { mux }
    }
}

impl<S: Subscriber> Layer<S> for HomeAppLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        self.mux.fire(event);
    }
}

/// Forwards application logs to Home once a healthy client is bound (Go: `HomeAppLogForwarder`).
pub struct HomeAppLogForwarder {
    inner: Arc<Inner>,
    mux: Arc<AppLogMux>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl HomeAppLogForwarder {
    /// Registers a forwarder on the process-wide mux and starts its sender. Must run inside a
    /// Tokio runtime. `queue_size == 0` selects [`DEFAULT_QUEUE_SIZE`].
    pub fn start(queue_size: usize) -> HomeAppLogForwarder {
        Self::start_in(AppLogMux::global(), queue_size)
    }

    pub fn start_in(mux: Arc<AppLogMux>, queue_size: usize) -> HomeAppLogForwarder {
        let (inner, rx) = Inner::new(queue_size);
        let task = tokio::spawn(inner.clone().run(rx));
        mux.register(inner.clone());
        HomeAppLogForwarder { inner, mux, task: Mutex::new(Some(task)) }
    }

    /// Activates forwarding to `client` (the previous owner, if any, is replaced).
    pub fn bind(&self, client: Arc<Client>) {
        self.inner.bind(client);
    }

    /// Stops forwarding only when `client` is the current owner.
    pub fn deactivate(&self, client: &Arc<Client>) {
        let client: Arc<dyn AppLogClient> = client.clone();
        self.inner.deactivate(&client);
    }

    /// Disables forwarding and waits for the sender, including a push already in flight.
    pub async fn stop(&self) {
        self.shutdown();
        let task = self.task.lock().take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    fn shutdown(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        *self.inner.owner.lock() = None;
        self.inner.enabled.store(false, Ordering::SeqCst);
        self.mux.unregister(&self.inner);
        self.inner.stop.cancel();
    }
}

impl Drop for HomeAppLogForwarder {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use tokio::sync::Notify;
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    struct StubClient {
        heartbeat: AtomicBool,
        err: Option<HomeError>,
        gate: Option<Arc<Notify>>,
        started: Notify,
        pushed: Mutex<Vec<Vec<u8>>>,
        calls: AtomicUsize,
    }

    impl StubClient {
        fn new(heartbeat: bool) -> Arc<StubClient> {
            Arc::new(StubClient {
                heartbeat: AtomicBool::new(heartbeat),
                err: None,
                gate: None,
                started: Notify::new(),
                pushed: Mutex::default(),
                calls: AtomicUsize::new(0),
            })
        }

        fn failing(err: HomeError) -> Arc<StubClient> {
            Arc::new(StubClient { err: Some(err), ..Arc::into_inner(Self::new(true)).expect("unique") })
        }

        fn count(&self) -> usize {
            self.pushed.lock().len()
        }
    }

    impl AppLogClient for StubClient {
        fn heartbeat_ok(&self) -> bool {
            self.heartbeat.load(Ordering::SeqCst)
        }

        fn rpush_app_log<'a>(&'a self, payload: &'a [u8]) -> PushFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.started.notify_one();
                if let Some(gate) = &self.gate {
                    gate.notified().await;
                }
                if let Some(err) = &self.err {
                    return Err(err.clone());
                }
                self.pushed.lock().push(payload.to_vec());
                Ok(())
            })
        }
    }

    fn as_client(c: &Arc<StubClient>) -> Arc<dyn AppLogClient> {
        c.clone()
    }

    fn record(line: &str) -> AppLogRecord {
        AppLogRecord { line: line.into(), ..Default::default() }
    }

    async fn wait_for(client: &StubClient, want: usize) {
        for _ in 0..1000 {
            if client.count() >= want {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(client.count(), want, "pushed records");
    }

    #[tokio::test]
    async fn forwards_formatted_log_when_bound_owner_is_healthy() {
        let stub = StubClient::new(true);
        let mux = Arc::new(AppLogMux::default());
        let forwarder = HomeAppLogForwarder::start_in(mux.clone(), 4);
        forwarder.inner.bind(as_client(&stub));

        let subscriber = tracing_subscriber::registry().with(HomeAppLogLayer::new(mux));
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(request_id = "req-app1", "debug details");
        });
        wait_for(&stub, 1).await;

        let got: serde_json::Value = serde_json::from_slice(&stub.pushed.lock()[0]).expect("json payload");
        assert_eq!(got["level"], "debug");
        assert_eq!(got["request_id"], "req-app1");
        let line = got["line"].as_str().unwrap_or_default();
        assert!(line.contains("debug details") && line.contains("[req-app1]"), "{line}");
        assert!(!got["timestamp"].as_str().unwrap_or_default().trim().is_empty());
        forwarder.stop().await;
    }

    #[tokio::test]
    async fn stop_unregisters_mux_target() {
        let mux = Arc::new(AppLogMux::default());
        let forwarder = HomeAppLogForwarder::start_in(mux.clone(), 1);
        assert_eq!(mux.target_count(), 1);
        forwarder.stop().await;
        assert_eq!(mux.target_count(), 0);
    }

    #[tokio::test]
    async fn rebinds_only_to_current_owner() {
        let (first, second) = (StubClient::new(true), StubClient::new(true));
        let forwarder = HomeAppLogForwarder::start_in(Arc::default(), 4);
        let inner = forwarder.inner.clone();

        inner.bind(as_client(&first));
        inner.fire(|| record("one"));
        wait_for(&first, 1).await;

        inner.bind(as_client(&second));
        inner.deactivate(&as_client(&first));
        inner.fire(|| record("two"));
        wait_for(&second, 1).await;
        assert_eq!(first.count(), 1, "stale owner received a record");

        inner.deactivate(&as_client(&first));
        inner.fire(|| record("three"));
        wait_for(&second, 2).await;

        inner.deactivate(&as_client(&second));
        inner.fire(|| record("four"));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(second.count(), 2, "detached owner received a record");
        forwarder.stop().await;
    }

    #[tokio::test]
    async fn delayed_old_owner_unsupported_does_not_disable_new_owner() {
        let gate = Arc::new(Notify::new());
        let old = Arc::new(StubClient {
            err: Some(HomeError::Redis("ERR unsupported key".into())),
            gate: Some(gate.clone()),
            ..Arc::into_inner(StubClient::new(true)).expect("unique")
        });
        let new = StubClient::new(true);
        let forwarder = HomeAppLogForwarder::start_in(Arc::default(), 1);
        let inner = forwarder.inner.clone();
        inner.bind(as_client(&old));

        let old_client = as_client(&old);
        let sender = {
            let inner = inner.clone();
            tokio::spawn(async move { inner.forward(&record("old owner"), Some(old_client)).await })
        };
        tokio::time::timeout(Duration::from_secs(1), old.started.notified()).await.expect("old owner started");

        inner.bind(as_client(&new));
        gate.notify_one();
        tokio::time::timeout(Duration::from_secs(1), sender).await.expect("forward finished").expect("join");
        assert!(inner.enabled.load(Ordering::SeqCst), "old owner disabled the new owner");

        inner.forward(&record("new owner"), Some(as_client(&new))).await;
        assert_eq!(new.count(), 1);
        forwarder.stop().await;
    }

    #[test]
    fn unbound_and_gap_logs_are_dropped() {
        let (inner, mut rx) = Inner::new(2);
        inner.fire(|| record("pre-ack"));
        assert!(rx.try_recv().is_err());

        let stub = StubClient::new(true);
        inner.bind(as_client(&stub));
        inner.deactivate(&as_client(&stub));
        inner.fire(|| record("reconnect gap"));
        assert!(rx.try_recv().is_err());
        assert!(inner.client().is_none());
    }

    #[test]
    fn placeholder_request_id_is_omitted() {
        assert_eq!(app_log_request_id(Some("--------")), "");
        assert_eq!(app_log_request_id(Some(" abc ")), "abc");
        assert_eq!(app_log_request_id(None), "");
    }

    #[test]
    fn unhealthy_owner_keeps_logs_local() {
        let (inner, mut rx) = Inner::new(2);
        inner.bind(as_client(&StubClient::new(false)));
        inner.fire(|| record("should stay local"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn unsupported_reply_disables_forwarding_until_rebound() {
        let stub = StubClient::failing(HomeError::Redis("ERR unsupported key".into()));
        let (inner, _rx) = Inner::new(2);
        inner.bind(as_client(&stub));
        inner.forward(&record("legacy home cannot receive app logs"), None).await;
        assert!(!inner.enabled.load(Ordering::SeqCst));

        inner.bind(as_client(&stub));
        assert!(inner.enabled.load(Ordering::SeqCst), "bind re-enables forwarding");
    }

    #[tokio::test]
    async fn full_queue_drops_instead_of_blocking() {
        let (inner, mut rx) = Inner::new(1);
        inner.bind(as_client(&StubClient::new(true)));
        inner.fire(|| record("kept"));
        inner.fire(|| record("dropped"));
        assert_eq!(rx.try_recv().map(|q| q.record.line).ok().as_deref(), Some("kept"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn timestamp_trims_zero_fraction() {
        let t = DateTime::parse_from_rfc3339("2026-05-29T08:00:00.120+00:00").expect("time").with_timezone(&Local);
        let text = rfc3339_nano(t);
        assert!(text.starts_with("2026-05-29T") && text.contains(".12") && !text.contains(".120"), "{text}");
    }
}
