//! Home mode of the service (Go: `sdk/cliproxy/service_home.go`).
//!
//! In Home mode the config comes from Home and is pushed over the subscription: one supervisor
//! task runs subscriber lifetimes back to back. Each lifetime owns a fresh [`Client`], an
//! execution registry and a release flusher; the first config of a lifetime publishes the
//! dispatch bundle the conductor selects credentials through, and starts the usage forwarder and
//! the in-flight publisher. A lifetime that ends with an error is replaced after the registry
//! settled (or was drained when the failure makes ongoing executions unsafe).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use cpa_config::{Config, CredentialConcurrencyConfig, HomeConfig};
use cpa_home::concurrency_release::{ReleaseFlusher, SendFn};
use cpa_home::conn::Kill;
use cpa_home::executionregistry::Registry;
use cpa_home::{Client, HomeError, kv, queue};
use parking_lot::Mutex;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use super::Inner;
use crate::conductor::home_publisher::PublisherConfig;

/// Back-off between lifetimes that failed before their first config was published.
const PRE_ACK_RETRY_BACKOFF: Duration = Duration::from_millis(100);
const USAGE_BATCH: usize = 64;

/// Callbacks of the Home lifetime for components outside the runtime (e.g. the app-log
/// forwarder, Go: `homeLogForwarder`).
pub trait HomeHooks: Send + Sync {
    /// A client became the active Home lifetime.
    fn bind(&self, client: Arc<Client>);
    /// The lifetime of `client` ended.
    fn deactivate(&self, client: &Arc<Client>);
}

/// Plugin work riding on Home configs (Go: `homePluginFinalization`): the plugin host installs
/// what Home assigns before a config is applied and reports once it took effect.
#[async_trait::async_trait]
pub trait HomePlugins: Send + Sync {
    /// Before `merged` is applied: installs the plugins Home assigns and stages the status
    /// reports and delete tasks. `sync_cfg` is `merged` with the `plugins.store-auth` Home sent.
    /// An `Err` keeps the config from being applied; it is retried.
    async fn stage(&self, client: &Arc<Client>, sync_cfg: &Config, merged: &Config) -> Result<Box<dyn HomePluginWork>, String>;
}

/// The part of [`HomePlugins::stage`] that runs after the config was applied.
#[async_trait::async_trait]
pub trait HomePluginWork: Send {
    /// Records load results, reports statuses and processes delete tasks. An `Err` is retried.
    async fn finalize(&mut self, client: &Arc<Client>) -> Result<(), String>;
}

/// A running supervisor.
pub(super) struct HomeSupervisor {
    cancel: Arc<Kill>,
    handle: JoinHandle<()>,
}

impl HomeSupervisor {
    pub(super) fn stop(self) {
        self.cancel.kill();
        self.handle.abort();
    }
}

/// Go: `forceHomeRuntimeConfig`: settings Home owns, forced on the local config.
pub fn force_home_runtime_config(cfg: &mut Config) {
    cfg.api_keys.clear();
    cfg.usage_statistics_enabled = true;
    cfg.disable_cooling = true;
    cfg.save_cooldown_status = false;
    cfg.websocket_auth = false;
    cfg.remote_management.allow_remote = false;
    cfg.remote_management.disable_control_panel = true;
    cfg.plugins.store_auth.clear();
}

/// Overlays what stays local (listener, TLS, Home itself) on the config Home sent (Go:
/// `stageHomeOverlayWithClient`).
pub fn merge_home_config(base: &Config, mut remote: Config) -> Config {
    remote.host.clone_from(&base.host);
    remote.port = base.port;
    remote.tls = base.tls.clone();
    remote.home = base.home.clone();
    force_home_runtime_config(&mut remote);
    remote
}

impl Inner {
    /// Starts the supervisor when the config enables Home.
    pub(super) fn start_home(self: &Arc<Self>) {
        let cfg = self.config();
        if !cfg.home.enabled {
            return;
        }
        queue::set_usage_statistics_enabled(true);
        queue::set_enabled(true);
        let cancel = Arc::new(Kill::default());
        let handle = tokio::spawn(run_supervisor(Arc::downgrade(self), cancel.clone(), cfg.home.clone()));
        if let Some(previous) = self.home_supervisor.lock().replace(HomeSupervisor { cancel, handle }) {
            previous.stop();
        }
    }

    pub(super) fn stop_home(&self) {
        if let Some(supervisor) = self.home_supervisor.lock().take() {
            supervisor.stop();
        }
    }
}

/// State shared by the pieces of one lifetime.
struct Lifetime {
    client: Arc<Client>,
    registry: Registry,
    cancel: Arc<Kill>,
    published: AtomicBool,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

fn cancel_bound(cfg: &CredentialConcurrencyConfig) -> u64 {
    cfg.clone().with_defaults().cpa_cancel_bound.0.max(0) as u64
}

fn default_cancel_bound() -> Duration {
    CredentialConcurrencyConfig::default().with_defaults().cpa_cancel_bound.to_std()
}

fn sender_for(client: Arc<Client>) -> SendFn {
    Arc::new(move |frame| {
        let client = client.clone();
        Box::pin(async move { client.push_concurrency_release(&frame).await })
    })
}

fn new_flusher(registry: &Registry) -> ReleaseFlusher {
    let flusher = ReleaseFlusher::new();
    let f = flusher.clone();
    registry.set_release_sink(Some(Arc::new(move |group, seq| f.mark_dirty(group, seq))));
    flusher
}

async fn run_supervisor(inner: Weak<Inner>, cancel: Arc<Kill>, home_cfg: HomeConfig) {
    let mut registry = Registry::new();
    let mut flusher = new_flusher(&registry);
    let bound = Arc::new(AtomicU64::new(default_cancel_bound().as_nanos() as u64));
    let mut previous: Option<Arc<Client>> = None;
    loop {
        if cancel.is_dead() {
            break;
        }
        let Some(service) = inner.upgrade() else { return };
        let client = Arc::new(match &previous {
            Some(p) => p.new_lifetime(),
            None => Client::new(home_cfg.clone()),
        });
        client.set_managed_lifetime(true);
        let config_client = client.clone();
        flusher.set_config_provider(Some(Arc::new(move || config_client.limiter_config())));
        flusher.set_sender(Some(sender_for(client.clone())));
        let release_cancel = Arc::new(Kill::default());
        let release_task = {
            let (flusher, release_cancel) = (flusher.clone(), release_cancel.clone());
            tokio::spawn(async move { flusher.run(&release_cancel).await })
        };

        let life = Arc::new(Lifetime {
            client: client.clone(),
            registry: registry.clone(),
            cancel: Arc::new(Kill::default()),
            published: AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
        });
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let ready = Arc::new(Notify::new());
        let ready_flag = Arc::new(AtomicBool::new(false));
        let worker = tokio::spawn(config_worker(
            Arc::downgrade(&service),
            life.clone(),
            rx,
            ready.clone(),
            ready_flag.clone(),
            cancel.clone(),
        ));

        let on_config_inner = Arc::downgrade(&service);
        let on_config_client = client.clone();
        let barrier_registry = registry.clone();
        let on_config_bound = bound.clone();
        let mut on_config = move |raw: &[u8]| -> Result<(), HomeError> {
            let parsed = cpa_config::parse_config_bytes(raw).map_err(|e| {
                tracing::warn!("failed to parse home config payload: {e}");
                HomeError::other(e)
            })?;
            on_config_client.set_lifecycle_config(parsed.credential_concurrency.clone()).inspect_err(|e| {
                tracing::warn!("failed to apply Home lifecycle config: {e}");
            })?;
            match PublisherConfig::from_config(&parsed.credential_in_flight) {
                Ok(cfg) => {
                    if let Some(service) = on_config_inner.upgrade() {
                        service.manager.apply_home_in_flight_publisher_config(cfg);
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to apply Home in-flight publisher config: {e}");
                    return Err(HomeError::other(e));
                }
            }
            barrier_registry.observe_barrier(parsed.credential_concurrency.observation_barrier_revision);
            on_config_bound.store(cancel_bound(&parsed.credential_concurrency), Ordering::SeqCst);
            let _ = tx.send(raw.to_vec());
            Ok(())
        };
        let (ready_n, ready_f) = (ready.clone(), ready_flag.clone());
        let mut on_ready = move || {
            ready_f.store(true, Ordering::SeqCst);
            ready_n.notify_waiters();
            ready_n.notify_one();
        };
        let err_run = client.run_config_subscriber_lifetime(&life.cancel, &mut on_config, &mut on_ready).await;
        life.cancel.kill();
        let _ = worker.await;
        let tasks: Vec<JoinHandle<()>> = std::mem::take(&mut *life.tasks.lock());
        for t in tasks {
            let _ = t.await;
        }

        detach_lifetime(&service, &life, &client);
        let retry = err_run.is_err() && !cancel.is_dead();
        if retry {
            release_cancel.kill();
            let _ = release_task.await;
            client.close();
            let settle = Duration::from_nanos(bound.load(Ordering::SeqCst));
            if let Err(e) = registry.wait_pending(settle).await {
                tracing::error!("failed to settle pending Home dispatches before subscriber replacement: {e}");
                service.home_fatal.notify_one();
                return;
            }
            let err = err_run.err().unwrap_or(HomeError::Disabled);
            let legacy = err.is_legacy_membership_protocol();
            if legacy {
                client.enable_legacy_membership();
            }
            if client.ambiguous_dispatch() || err.is_membership_takeover_unavailable() || legacy || client.legacy_membership() {
                registry.set_release_sink(None);
                if let Err(e) = registry.drain(settle).await {
                    tracing::error!("failed to drain Home executions after unsafe subscriber replacement: {e}");
                    service.home_fatal.notify_one();
                    return;
                }
                client.suppress_takeover();
                registry = Registry::new();
                flusher = new_flusher(&registry);
            }
            tracing::warn!("home config subscription lifetime ended: {err}");
            if !life.published.load(Ordering::SeqCst) {
                tokio::select! {
                    _ = cancel.wait() => return,
                    _ = tokio::time::sleep(PRE_ACK_RETRY_BACKOFF) => {}
                }
            }
            previous = Some(client);
            continue;
        }

        let bound_now = Duration::from_nanos(bound.load(Ordering::SeqCst));
        let drained = registry.drain(bound_now).await;
        let flushed = if drained.is_ok() { flusher.flush_all(bound_now).await.map_err(|e| e.to_string()) } else { Ok(()) };
        release_cancel.kill();
        let _ = release_task.await;
        client.close();
        match (drained, flushed) {
            (Err(e), _) => {
                if !cancel.is_dead() {
                    tracing::error!("failed to drain Home execution registry: {e}");
                    service.home_fatal.notify_one();
                }
            }
            (_, Err(e)) if !cancel.is_dead() => {
                tracing::error!("failed to flush Home concurrency releases: {e}");
                service.home_fatal.notify_one();
            }
            _ => {}
        }
        return;
    }
    // Cancelled between lifetimes: drain what is left.
    registry.set_release_sink(None);
    let _ = registry.drain(Duration::from_nanos(bound.load(Ordering::SeqCst))).await;
}

/// Applies each config of a lifetime in order; the first one publishes the lifetime (Go:
/// `runHomeConfigWorker`).
async fn config_worker(
    inner: Weak<Inner>,
    life: Arc<Lifetime>,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    ready: Arc<Notify>,
    ready_flag: Arc<AtomicBool>,
    supervisor_cancel: Arc<Kill>,
) {
    // Wait for the subscription to be established.
    loop {
        let notified = ready.notified();
        if ready_flag.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            _ = life.cancel.wait() => return,
            _ = notified => {}
        }
    }
    loop {
        let raw = tokio::select! {
            _ = life.cancel.wait() => return,
            raw = rx.recv() => match raw {
                Some(raw) => raw,
                None => return,
            },
        };
        let Some(service) = inner.upgrade() else { return };
        let parsed = match cpa_config::parse_config_bytes(&raw) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("failed to stage home config; retrying: {e}");
                continue;
            }
        };
        let base = service.config();
        let remote_store_auth = parsed.plugins.store_auth.clone();
        let merged = merge_home_config(&base, parsed);
        let mut plugin_work = None;
        if let Some(plugins) = &service.home_plugins {
            let mut sync_cfg = merged.clone();
            sync_cfg.plugins.store_auth = remote_store_auth;
            match plugins.stage(&life.client, &sync_cfg, &merged).await {
                Ok(work) => plugin_work = Some(work),
                Err(e) => {
                    tracing::warn!("failed to stage home config; retrying: {e}");
                    continue;
                }
            }
        }
        let merged = Arc::new(merged);
        if life.cancel.is_dead() || supervisor_cancel.is_dead() {
            return;
        }
        let outcome = service.apply_config(merged).await;
        if !outcome.accepted {
            tracing::warn!("failed to apply config update from home control center");
            continue;
        }
        if let Some(mut work) = plugin_work {
            loop {
                if life.cancel.is_dead() {
                    return;
                }
                match work.finalize(&life.client).await {
                    Ok(()) => break,
                    Err(e) => {
                        tracing::warn!("failed to finalize home plugins; retrying: {e}");
                        tokio::select! {
                            _ = life.cancel.wait() => return,
                            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                        }
                    }
                }
            }
        }
        if life.cancel.is_dead() {
            return;
        }
        if !life.published.swap(true, Ordering::SeqCst) {
            publish_lifetime(&service, &life);
        }
    }
}

/// Makes the lifetime the active one: dispatch bundle, global client, usage forwarding and
/// in-flight publishing.
fn publish_lifetime(service: &Arc<Inner>, life: &Arc<Lifetime>) {
    let client = life.client.clone();
    kv::set_current(client.clone());
    *service.home_state.lock() = Some(service.manager.publish_home_dispatch(client.clone(), life.registry.clone(), 0));
    if let Some(hooks) = &service.home_hooks {
        hooks.bind(client.clone());
    }
    let mut tasks = life.tasks.lock();
    let manager = service.manager.clone();
    {
        let (cancel, client, registry) = (life.cancel.clone(), client.clone(), life.registry.clone());
        let manager = manager.clone();
        tasks.push(tokio::spawn(async move {
            manager.start_home_in_flight_publisher(&cancel, client, registry).await;
        }));
    }
    tasks.push(tokio::spawn(usage_forwarder(life.cancel.clone(), client)));
}

/// Clears everything `publish_lifetime` installed for `client`.
fn detach_lifetime(service: &Arc<Inner>, _life: &Arc<Lifetime>, client: &Arc<Client>) {
    if let Some(bundle) = service.home_state.lock().take() {
        service.manager.clear_home_dispatch_bundle(&bundle);
    }
    kv::clear_current_if(client);
    if let Some(hooks) = &service.home_hooks {
        hooks.deactivate(client);
    }
}

/// Forwards queued usage records to Home (Go: `startHomeUsageForwarder`); a failed push puts the
/// unsent records back.
async fn usage_forwarder(cancel: Arc<Kill>, client: Arc<Client>) {
    let sleep = |d: Duration| {
        let cancel = cancel.clone();
        async move {
            tokio::select! {
                _ = cancel.wait() => false,
                _ = tokio::time::sleep(d) => true,
            }
        }
    };
    loop {
        if cancel.is_dead() {
            return;
        }
        if !client.heartbeat_ok() {
            if !sleep(Duration::from_secs(1)).await {
                return;
            }
            continue;
        }
        let items = queue::pop_oldest(USAGE_BATCH);
        if items.is_empty() {
            if !sleep(Duration::from_millis(500)).await {
                return;
            }
            continue;
        }
        for (i, item) in items.iter().enumerate() {
            let pushed = tokio::select! {
                _ = cancel.wait() => Err(HomeError::Other("context canceled".into())),
                r = client.lpush_usage(item) => r,
            };
            if pushed.is_err() {
                for rest in &items[i..] {
                    queue::enqueue(rest);
                }
                if !sleep(Duration::from_secs(1)).await {
                    return;
                }
                break;
            }
        }
    }
}
