//! mDNS advertiser lifecycle driven by config (Go: sdk/cliproxy/discovery_advertiser.go).
//!
//! [`DiscoveryManager::apply`] reconciles the running advertiser with a config snapshot: it starts,
//! restarts (when the interface set, addresses or TXT change), or stops it, and a background
//! refresh re-applies the last config every 15 seconds so interface changes are picked up. The
//! listen endpoint (host, port, TLS) is captured on the first apply and kept across reloads,
//! because a config reload never rebinds the HTTP listener.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use cpa_config::Config;
use cpa_discovery::{Advertiser, Ctx, Error, ServiceSpec, ZeroconfAdvertiser, build_service_spec};
use parking_lot::Mutex;
use tokio::sync::watch;

const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(15);

type NewAdvertiser = Arc<dyn Fn() -> Option<Arc<dyn Advertiser>> + Send + Sync>;
type BuildSpec = Arc<dyn Fn(&Config, i64, bool) -> Result<ServiceSpec, Error> + Send + Sync>;

/// How often the last config is re-applied (Go: negative disables, zero means the default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshPolicy {
    Disabled,
    Default,
    Every(Duration),
}

/// One-shot "finished" flag that any number of tasks can wait on (Go: a closed channel).
#[derive(Clone)]
pub(super) struct Signal(Arc<watch::Sender<bool>>);

impl Signal {
    pub(super) fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    pub(super) fn fire(&self) {
        self.0.send_replace(true);
    }

    pub(super) async fn wait(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|fired| *fired).await;
    }
}

/// The background re-apply loop.
struct Refresh {
    id: u64,
    stop: Signal,
    done: Signal,
}

impl Refresh {
    /// Idempotent: stops the loop and waits for it to exit.
    async fn stop_and_wait(&self) {
        self.stop.fire();
        self.done.wait().await;
    }
}

/// A start in flight, cancelable by a newer apply or shutdown.
struct ActiveStart {
    id: u64,
    cancel: Ctx,
    done: Signal,
}

#[derive(Default)]
struct State {
    advertiser: Option<Arc<dyn Advertiser>>,
    enabled: bool,
    last_spec: ServiceSpec,
    last_build_error: String,
    last_cfg: Option<Arc<Config>>,
    last_port: i64,
    last_tls: bool,
    generation: u64,
    refresh: Option<Arc<Refresh>>,
    active_start: Option<Arc<ActiveStart>>,
    closed: bool,
    bound_host: String,
    bound_port: i64,
    bound_tls: bool,
    bound_endpoint: bool,
}

struct Shared {
    state: Mutex<State>,
    refresh_policy: RefreshPolicy,
    new_advertiser: NewAdvertiser,
    build_spec: BuildSpec,
    next_id: AtomicU64,
    /// Number of "failed to build service spec" warnings emitted (a repeated message warns once).
    build_warnings: AtomicUsize,
}

/// Owns the advertiser for one service and reacts to config reloads.
#[derive(Clone)]
pub struct DiscoveryManager {
    shared: Arc<Shared>,
}

impl Default for DiscoveryManager {
    fn default() -> Self {
        Self::new()
    }
}

fn same_cfg(a: &Option<Arc<Config>>, b: &Arc<Config>) -> bool {
    a.as_ref().is_some_and(|a| Arc::ptr_eq(a, b))
}

impl DiscoveryManager {
    /// The production manager: mDNS advertiser, config-built specs, 15s refresh.
    pub fn new() -> Self {
        Self::with_parts(
            Arc::new(|| Some(Arc::new(ZeroconfAdvertiser::new()) as Arc<dyn Advertiser>)),
            Arc::new(|cfg, port, tls| build_service_spec(cfg, port, tls)),
            RefreshPolicy::Default,
        )
    }

    /// A manager with injected advertiser and spec factories (tests).
    pub fn with_parts(new_advertiser: NewAdvertiser, build_spec: BuildSpec, refresh_policy: RefreshPolicy) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                refresh_policy,
                new_advertiser,
                build_spec,
                next_id: AtomicU64::new(1),
                build_warnings: AtomicUsize::new(0),
            }),
        }
    }

    /// Reconciles the advertiser with `cfg`. Returns false when nothing was committed (closed,
    /// canceled, superseded by a newer apply, spec build failure or start failure).
    pub async fn apply(&self, ctx: &Ctx, cfg: &Arc<Config>, port: i64, tls_enabled: bool) -> bool {
        self.shared.clone().apply_inner(ctx, cfg, port, tls_enabled, None).await
    }

    /// Stops the advertiser and the refresh loop; later applies are rejected.
    pub async fn shutdown(&self) -> Result<(), Error> {
        let (old, active_start, refresh) = {
            let mut st = self.shared.state.lock();
            st.closed = true;
            let old = st.advertiser.take();
            let active_start = st.active_start.clone();
            let refresh = st.refresh.take();
            st.enabled = false;
            st.last_spec = ServiceSpec::default();
            st.last_build_error.clear();
            st.last_cfg = None;
            st.generation += 1;
            (old, active_start, refresh)
        };
        if let Some(active) = active_start {
            active.cancel.cancel();
            active.done.wait().await;
        }
        if let Some(refresh) = refresh {
            refresh.stop_and_wait().await;
        }
        match old {
            Some(adv) => adv.stop().await,
            None => Ok(()),
        }
    }

    #[cfg(test)]
    fn build_warnings(&self) -> usize {
        self.shared.build_warnings.load(Ordering::SeqCst)
    }
}

impl Shared {
    async fn apply_inner(self: Arc<Self>, ctx: &Ctx, cfg: &Arc<Config>, port: i64, tls_enabled: bool, expected_refresh: Option<u64>) -> bool {
        if ctx.err().is_some() {
            return false;
        }

        let (active_start, apply_generation, bound_cfg, bound_port, bound_tls) = {
            let mut st = self.state.lock();
            if st.closed {
                return false;
            }
            if let Some(expected) = expected_refresh
                && (st.refresh.as_ref().map(|r| r.id) != Some(expected) || !same_cfg(&st.last_cfg, cfg))
            {
                return false;
            }
            let active_start = st.active_start.clone();
            st.generation += 1;
            let apply_generation = st.generation;
            if !st.bound_endpoint {
                st.bound_host = cfg.host.clone();
                st.bound_port = port;
                st.bound_tls = tls_enabled;
                st.bound_endpoint = true;
            }
            let mut bound_cfg = (**cfg).clone();
            bound_cfg.host = st.bound_host.clone();
            if !same_cfg(&st.last_cfg, cfg) {
                st.last_build_error.clear();
            }
            st.last_cfg = Some(cfg.clone());
            st.last_port = st.bound_port;
            st.last_tls = st.bound_tls;
            (active_start, apply_generation, bound_cfg, st.bound_port, st.bound_tls)
        };
        if let Some(active) = active_start {
            active.cancel.cancel();
            active.done.wait().await;
        }

        // Disabled: tear everything down (state changes under the lock, stops after it).
        let enabled_in_cfg = cfg.discovery.enabled;
        let teardown = {
            let mut st = self.state.lock();
            if st.closed || st.generation != apply_generation || !same_cfg(&st.last_cfg, cfg) {
                return false;
            }
            if enabled_in_cfg {
                self.ensure_refresh_locked(&mut st);
                None
            } else {
                let old = st.advertiser.take();
                let refresh = st.refresh.take();
                st.enabled = false;
                st.last_spec = ServiceSpec::default();
                st.last_build_error.clear();
                Some((old, refresh))
            }
        };
        if let Some((old, refresh)) = teardown {
            if let Some(refresh) = refresh {
                refresh.stop_and_wait().await;
            }
            if let Some(old) = old {
                tracing::info!("discovery: stopping mDNS advertisement (disabled by config)");
                let _ = old.stop().await;
            }
            return true;
        }

        let spec = match (self.build_spec)(&bound_cfg, bound_port, bound_tls) {
            Ok(spec) => spec,
            Err(err) => {
                let (old, should_warn) = {
                    let mut st = self.state.lock();
                    if st.closed || st.generation != apply_generation || !same_cfg(&st.last_cfg, cfg) || !enabled_in_cfg {
                        return false;
                    }
                    let old = st.advertiser.take();
                    st.enabled = false;
                    st.last_spec = ServiceSpec::default();
                    let message = err.to_string();
                    let should_warn = st.last_build_error != message;
                    st.last_build_error = message;
                    st.generation += 1;
                    (old, should_warn)
                };
                if let Some(old) = old {
                    tracing::info!("discovery: stopping stale mDNS advertisement after spec build failure");
                    let _ = old.stop().await;
                }
                if should_warn {
                    self.build_warnings.fetch_add(1, Ordering::SeqCst);
                    tracing::warn!("discovery: failed to build service spec: {err}");
                }
                return false;
            }
        };

        let (old, gen_after, start_ctx, start) = {
            let mut st = self.state.lock();
            if st.closed
                || st.generation != apply_generation
                || !same_cfg(&st.last_cfg, cfg)
                || !enabled_in_cfg
                || st.active_start.is_some()
            {
                return false;
            }
            st.last_build_error.clear();
            if st.enabled && st.advertiser.is_some() && spec_equal(&st.last_spec, &spec) {
                return true;
            }
            let old = st.advertiser.take();
            st.generation += 1;
            let gen_after = st.generation;
            let start_ctx = ctx.with_cancel();
            let start = Arc::new(ActiveStart { id: self.next_id.fetch_add(1, Ordering::SeqCst), cancel: start_ctx.clone(), done: Signal::new() });
            st.active_start = Some(start.clone());
            (old, gen_after, start_ctx, start)
        };

        if let Some(old) = old {
            let _ = old.stop().await;
        }
        let adv = (self.new_advertiser)();
        let mut err_start: Option<Error> = None;
        match &adv {
            None => err_start = Some(Error::new("discovery: advertiser factory returned nil")),
            Some(adv) => {
                if let Err(err) = adv.start(&start_ctx, spec.clone()).await {
                    err_start = Some(err);
                }
            }
        }
        if err_start.is_none()
            && let Some(err) = start_ctx.err()
        {
            err_start = Some(err.into());
        }

        let valid = {
            let mut st = self.state.lock();
            let valid = !st.closed
                && st.generation == gen_after
                && st.active_start.as_ref().is_some_and(|a| a.id == start.id)
                && same_cfg(&st.last_cfg, cfg)
                && enabled_in_cfg
                && start_ctx.err().is_none();
            if valid && err_start.is_none() {
                st.advertiser = adv.clone();
                st.enabled = true;
                st.last_spec = spec.clone();
            }
            valid
        };
        start_ctx.cancel();

        if (!valid || err_start.is_some())
            && let Some(adv) = &adv
        {
            let _ = adv.stop().await;
        }
        {
            let mut st = self.state.lock();
            if st.active_start.as_ref().is_some_and(|a| a.id == start.id) {
                st.active_start = None;
            }
        }
        start.done.fire();

        if !valid || err_start.is_some() {
            if let Some(err) = err_start {
                {
                    let mut st = self.state.lock();
                    if st.generation == gen_after {
                        st.enabled = false;
                    }
                }
                tracing::warn!("discovery: failed to start mDNS advertiser: {err} (degraded, HTTP intact)");
            }
            return false;
        }

        tracing::info!("discovery: advertising as '{}.{}' on port {}", spec.instance_name, spec.service_type, bound_port);
        true
    }

    fn refresh_period(&self) -> Option<Duration> {
        match self.refresh_policy {
            RefreshPolicy::Disabled => None,
            RefreshPolicy::Default => Some(DEFAULT_REFRESH_INTERVAL),
            RefreshPolicy::Every(d) => Some(d),
        }
    }

    /// Starts the periodic re-apply task if enabled and not already running.
    fn ensure_refresh_locked(self: &Arc<Self>, st: &mut State) {
        let Some(interval) = self.refresh_period() else { return };
        if st.refresh.is_some() {
            return;
        }
        let refresh = Arc::new(Refresh { id: self.next_id.fetch_add(1, Ordering::SeqCst), stop: Signal::new(), done: Signal::new() });
        st.refresh = Some(refresh.clone());
        let shared = self.clone();
        tokio::spawn(async move {
            shared.refresh_loop(&refresh, interval).await;
            refresh.done.fire();
        });
    }

    async fn refresh_loop(self: &Arc<Self>, refresh: &Refresh, interval: Duration) {
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        loop {
            tokio::select! {
                _ = refresh.stop.wait() => return,
                _ = ticker.tick() => {
                    let (cfg, port, tls) = {
                        let st = self.state.lock();
                        (st.last_cfg.clone(), st.last_port, st.last_tls)
                    };
                    let Some(cfg) = cfg else { continue };
                    if !cfg.discovery.enabled {
                        continue;
                    }
                    self.clone().apply_inner(&Ctx::background(), &cfg, port, tls, Some(refresh.id)).await;
                }
            }
        }
    }
}

/// Two specs are equal when everything that reaches the wire is: names, ports, subtypes, TXT,
/// the interface set (by index and name) and the advertised addresses.
pub fn spec_equal(a: &ServiceSpec, b: &ServiceSpec) -> bool {
    a.instance_name == b.instance_name
        && a.service_type == b.service_type
        && a.domain == b.domain
        && a.port == b.port
        && a.subtypes == b.subtypes
        && a.text_records == b.text_records
        && a.interfaces.len() == b.interfaces.len()
        && a.interfaces.iter().zip(&b.interfaces).all(|(x, y)| x.index == y.index && x.name == y.name)
        && a.advertised_ips == b.advertised_ips
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use async_trait::async_trait;
    use cpa_discovery::Interface;
    use tokio::sync::Notify;

    use super::*;

    #[derive(Default)]
    struct FakeAdvertiser {
        counts: Mutex<(usize, usize, ServiceSpec)>,
        start_err: Mutex<Option<Error>>,
        entered: Notify,
        entered_flag: AtomicBool,
        block_start: Option<Arc<Notify>>,
    }

    impl FakeAdvertiser {
        fn snapshot(&self) -> (usize, usize, ServiceSpec) {
            self.counts.lock().clone()
        }
    }

    #[async_trait]
    impl Advertiser for FakeAdvertiser {
        async fn start(&self, ctx: &Ctx, spec: ServiceSpec) -> Result<(), Error> {
            self.entered_flag.store(true, Ordering::SeqCst);
            self.entered.notify_waiters();
            if let Some(block) = &self.block_start {
                tokio::select! {
                    _ = block.notified() => {}
                    err = ctx.done() => return Err(err.into()),
                }
            }
            let mut counts = self.counts.lock();
            counts.0 += 1;
            counts.2 = spec;
            match self.start_err.lock().clone() {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }

        async fn stop(&self) -> Result<(), Error> {
            self.counts.lock().1 += 1;
            Ok(())
        }
    }

    fn spec(name: &str, port: i64) -> ServiceSpec {
        ServiceSpec { instance_name: name.into(), port, ..Default::default() }
    }

    fn manager(adv: Arc<FakeAdvertiser>, build: impl Fn(&Config, i64, bool) -> Result<ServiceSpec, Error> + Send + Sync + 'static) -> DiscoveryManager {
        DiscoveryManager::with_parts(Arc::new(move || Some(adv.clone() as Arc<dyn Advertiser>)), Arc::new(build), RefreshPolicy::Disabled)
    }

    fn enabled_cfg() -> Arc<Config> {
        let mut cfg = Config::default();
        cfg.discovery.enabled = true;
        Arc::new(cfg)
    }

    async fn apply(mgr: &DiscoveryManager, cfg: &Arc<Config>) -> bool {
        mgr.apply(&Ctx::background(), cfg, 8317, false).await
    }

    #[test]
    fn spec_equal_detects_advertised_ip_change() {
        let a = ServiceSpec { instance_name: "n".into(), port: 8317, advertised_ips: vec!["192.0.2.10".into()], ..Default::default() };
        let b = ServiceSpec { advertised_ips: vec!["192.0.2.11".into()], ..a.clone() };
        assert!(spec_equal(&a, &a));
        assert!(!spec_equal(&a, &b));
    }

    #[test]
    fn spec_equal_detects_interface_index_change() {
        let iface = |index| Interface { index, name: "en0".into(), ..Default::default() };
        let a = ServiceSpec { instance_name: "n".into(), port: 8317, interfaces: vec![iface(1)], ..Default::default() };
        let b = ServiceSpec { interfaces: vec![iface(2)], ..a.clone() };
        assert!(spec_equal(&a, &a));
        assert!(!spec_equal(&a, &b));
    }

    #[tokio::test]
    async fn starts_once_for_unchanged_spec() {
        let adv = Arc::new(FakeAdvertiser::default());
        let mgr = manager(adv.clone(), |_, _, _| Ok(ServiceSpec { advertised_ips: vec!["192.0.2.10".into()], ..spec("n", 8317) }));
        let cfg = enabled_cfg();
        assert!(apply(&mgr, &cfg).await);
        assert!(apply(&mgr, &cfg).await);
        let (starts, stops, _) = adv.snapshot();
        assert_eq!((starts, stops), (1, 0));
    }

    #[tokio::test]
    async fn preserves_bound_endpoint_on_reload() {
        let adv = Arc::new(FakeAdvertiser::default());
        let seen = Arc::new(Mutex::new((String::new(), 0i64, false)));
        let seen_in = seen.clone();
        let mgr = manager(adv, move |cfg, port, tls| {
            *seen_in.lock() = (cfg.host.clone(), port, tls);
            Ok(spec("n", port))
        });
        assert!(apply(&mgr, &enabled_cfg()).await);
        let mut second = Config::default();
        second.host = "192.0.2.20".into();
        second.discovery.enabled = true;
        assert!(mgr.apply(&Ctx::background(), &Arc::new(second), 9999, true).await);
        assert_eq!(*seen.lock(), (String::new(), 8317, false));
    }

    #[tokio::test]
    async fn restarts_on_ip_change() {
        let created: Arc<Mutex<Vec<Arc<FakeAdvertiser>>>> = Arc::default();
        let created_in = created.clone();
        let ips = Arc::new(Mutex::new(vec!["192.0.2.10".to_string()]));
        let ips_in = ips.clone();
        let mgr = DiscoveryManager::with_parts(
            Arc::new(move || {
                let adv = Arc::new(FakeAdvertiser::default());
                created_in.lock().push(adv.clone());
                Some(adv as Arc<dyn Advertiser>)
            }),
            Arc::new(move |_, _, _| Ok(ServiceSpec { advertised_ips: ips_in.lock().clone(), ..spec("n", 8317) })),
            RefreshPolicy::Disabled,
        );
        let cfg = enabled_cfg();
        assert!(apply(&mgr, &cfg).await);
        *ips.lock() = vec!["192.0.2.11".into()];
        assert!(apply(&mgr, &cfg).await);
        let created = created.lock();
        assert_eq!(created.len(), 2);
        let (starts, stops, _) = created[0].snapshot();
        assert_eq!((starts, stops), (1, 1));
        let (starts, stops, second_spec) = created[1].snapshot();
        assert_eq!((starts, stops), (1, 0));
        assert_eq!(second_spec.advertised_ips, ["192.0.2.11"]);
    }

    #[tokio::test]
    async fn stops_advertiser_on_build_failure() {
        let adv = Arc::new(FakeAdvertiser::default());
        let fail = Arc::new(AtomicBool::new(false));
        let fail_in = fail.clone();
        let mgr = manager(adv.clone(), move |_, _, _| {
            if fail_in.load(Ordering::SeqCst) { Err(Error::new("context canceled")) } else { Ok(spec("n", 8317)) }
        });
        let cfg = enabled_cfg();
        assert!(apply(&mgr, &cfg).await);
        fail.store(true, Ordering::SeqCst);
        assert!(!apply(&mgr, &cfg).await);
        let (starts, stops, _) = adv.snapshot();
        assert_eq!((starts, stops), (1, 1));
    }

    #[tokio::test]
    async fn warns_once_per_build_failure_message() {
        let adv = Arc::new(FakeAdvertiser::default());
        let message = Arc::new(Mutex::new(Some("no qualified physical interfaces".to_string())));
        let calls = Arc::new(AtomicUsize::new(0));
        let (message_in, calls_in) = (message.clone(), calls.clone());
        let mgr = manager(adv.clone(), move |_, _, _| {
            calls_in.fetch_add(1, Ordering::SeqCst);
            match message_in.lock().clone() {
                Some(m) => Err(Error::new(m)),
                None => Ok(spec("n", 8317)),
            }
        });
        let cfg = enabled_cfg();
        for _ in 0..3 {
            assert!(!apply(&mgr, &cfg).await);
        }
        assert_eq!((calls.load(Ordering::SeqCst), mgr.build_warnings()), (3, 1));

        *message.lock() = Some("interface address unavailable".into());
        assert!(!apply(&mgr, &cfg).await);
        assert_eq!(mgr.build_warnings(), 2);
        *message.lock() = None;
        assert!(apply(&mgr, &cfg).await, "expected recovery to start advertiser");
        *message.lock() = Some("no qualified physical interfaces".into());
        assert!(!apply(&mgr, &cfg).await);
        assert_eq!(mgr.build_warnings(), 3);
        let (starts, stops, _) = adv.snapshot();
        assert_eq!((starts, stops), (1, 1));

        // Disabling clears the remembered failure, so the same message warns again.
        let disabled = Arc::new(Config::default());
        assert!(apply(&mgr, &disabled).await);
        let reenabled = enabled_cfg();
        assert!(!apply(&mgr, &reenabled).await);
        assert_eq!(mgr.build_warnings(), 4);
    }

    #[tokio::test]
    async fn rejects_apply_after_shutdown() {
        let called = Arc::new(AtomicBool::new(false));
        let called_in = called.clone();
        let mgr = manager(Arc::default(), move |_, _, _| {
            called_in.store(true, Ordering::SeqCst);
            Ok(ServiceSpec::default())
        });
        mgr.shutdown().await.unwrap();
        assert!(!apply(&mgr, &enabled_cfg()).await);
        assert!(!called.load(Ordering::SeqCst), "build_spec called after shutdown");
    }

    #[tokio::test]
    async fn refresh_stop_is_idempotent() {
        let refresh = Refresh { id: 1, stop: Signal::new(), done: Signal::new() };
        refresh.done.fire();
        refresh.stop_and_wait().await;
        refresh.stop_and_wait().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_shutdown_is_safe() {
        let adv = Arc::new(FakeAdvertiser::default());
        let adv_in = adv.clone();
        let mgr = DiscoveryManager::with_parts(
            Arc::new(move || Some(adv_in.clone() as Arc<dyn Advertiser>)),
            Arc::new(|_, _, _| Ok(spec("n", 8317))),
            RefreshPolicy::Every(Duration::from_millis(1)),
        );
        let cfg = enabled_cfg();
        assert!(apply(&mgr, &cfg).await);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let mgr = mgr.clone();
                tokio::spawn(async move { mgr.shutdown().await })
            })
            .collect();
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
        assert!(!apply(&mgr, &cfg).await);
    }

    #[tokio::test]
    async fn disable_stops_advertiser() {
        let adv = Arc::new(FakeAdvertiser::default());
        let mgr = manager(adv.clone(), |_, _, _| Ok(spec("n", 8317)));
        assert!(apply(&mgr, &enabled_cfg()).await);
        assert!(apply(&mgr, &Arc::new(Config::default())).await);
        let (starts, stops, _) = adv.snapshot();
        assert_eq!((starts, stops), (1, 1));
    }

    #[tokio::test]
    async fn canceled_apply_stops_in_flight_start() {
        let block = Arc::new(Notify::new());
        let adv = Arc::new(FakeAdvertiser { block_start: Some(block), ..Default::default() });
        let mgr = manager(adv.clone(), |_, _, _| Ok(spec("n", 8317)));
        let cfg = enabled_cfg();
        let ctx = Ctx::background().with_cancel();
        let task = {
            let (mgr, ctx, cfg) = (mgr.clone(), ctx.clone(), cfg.clone());
            tokio::spawn(async move { mgr.apply(&ctx, &cfg, 8317, false).await })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while !adv.entered_flag.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("start did not begin");
        ctx.cancel();
        let committed = tokio::time::timeout(Duration::from_secs(2), task).await.expect("canceled apply did not return").unwrap();
        assert!(!committed, "canceled apply should not commit");
        let (starts, stops, _) = adv.snapshot();
        assert_eq!(starts, 0);
        assert!(stops > 0);
    }

    #[tokio::test]
    async fn shutdown_abandons_in_flight_start() {
        let block = Arc::new(Notify::new());
        let adv = Arc::new(FakeAdvertiser { block_start: Some(block.clone()), ..Default::default() });
        let mgr = manager(adv.clone(), |_, _, _| Ok(spec("n", 8317)));
        let cfg = enabled_cfg();
        let task = {
            let (mgr, cfg) = (mgr.clone(), cfg.clone());
            tokio::spawn(async move { mgr.apply(&Ctx::background(), &cfg, 8317, false).await })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while !adv.entered_flag.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("start did not begin");
        mgr.shutdown().await.unwrap();
        block.notify_waiters();
        let committed = tokio::time::timeout(Duration::from_secs(2), task).await.expect("apply did not return").unwrap();
        assert!(!committed, "in-flight apply should not commit after shutdown");
        assert!(adv.snapshot().1 > 0, "expected in-flight advertiser to be stopped");
    }
}
