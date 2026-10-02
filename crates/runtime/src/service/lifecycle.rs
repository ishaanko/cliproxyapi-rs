//! The service object (Go: sdk/cliproxy builder.go, service.go, service_lifecycle.go,
//! service_auth.go, service_config.go, service_executors.go).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use cpa_auth::types::{ATTRIBUTE_PATH, ATTRIBUTE_SOURCE};
use cpa_auth::{Auth, AuthFileEvent, AuthWatcher, FileTokenStore, Status, Store, WatchOptions};
use cpa_config::diff::ReloadPlan;
use cpa_config::watcher::ConfigWatcher;
use cpa_config::{Config, ConfigError, load_config, load_dotenv, resolve_auth_dir};
use cpa_core::registry::{ModelRegistry, global_registry};
use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::antigravity::{Prober, reverse_alias_map, resolve_upstream_model_id};
use super::models::{apply_registration, openai_compat_info_from_auth, resolve_models_for_auth};
use super::sync::{AuthSync, AuthUpdate, AuthUpdateAction};
use crate::conductor::{Manager, SharedManager};
use crate::executor::{DynExecutor, ExecError};

/// Builds an executor for a provider key no registered executor handles (Go: the default branch
/// of `registerExecutorForAuth` creates an OpenAI-compatible executor per provider key). Return
/// `None` to leave the provider unhandled.
pub type ExecutorFactory = Arc<dyn Fn(&str) -> Option<DynExecutor> + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("cliproxy: failed to resolve config path: {0}")]
    ConfigPath(std::io::Error),
    #[error("cliproxy: failed to create auth directory {path}: {source}")]
    AuthDir { path: String, source: std::io::Error },
    #[error("cliproxy: auth path exists but is not a directory: {0}")]
    AuthDirNotDirectory(String),
    #[error("cliproxy: failed to start watcher: {0}")]
    Watcher(String),
    #[error("cliproxy: service already started")]
    AlreadyStarted,
}

/// The conductor operations the service drives. [`Manager`] implements it by delegation; the
/// hooks with default bodies exist so tests can stub them.
#[async_trait]
pub trait ManagerPort: Send + Sync {
    fn register_executor(&self, executor: DynExecutor);
    /// Insert or update (Go: `Manager.Register` / `Manager.Update`).
    async fn update(&self, auth: Auth) -> Result<Auth, ExecError>;
    async fn remove(&self, id: &str);
    fn list(&self) -> Vec<Auth>;
    fn get(&self, id: &str) -> Option<Auth>;

    /// A new config snapshot was committed (Go: `SetConfig`, `SetOAuthModelAlias`,
    /// `SetRetryConfig`, selector rebuild when `routing` changed).
    fn config_changed(&self, _config: &Arc<Config>) {}

    /// Models of `auth_id` were (re)registered (Go: `ReconcileRegistryModelStates`; there is no
    /// scheduler index to refresh).
    async fn models_registered(&self, _auth_id: &str) {}

    /// A batch of auth updates finished (Go: `RefreshAPIKeyModelAlias`).
    fn auth_batch_applied(&self) {}
}

#[async_trait]
impl ManagerPort for Manager {
    fn register_executor(&self, executor: DynExecutor) {
        Manager::register_executor(self, executor);
    }
    async fn update(&self, auth: Auth) -> Result<Auth, ExecError> {
        Manager::update(self, auth).await
    }
    async fn remove(&self, id: &str) {
        Manager::remove(self, id).await;
    }
    fn list(&self) -> Vec<Auth> {
        Manager::list(self)
    }
    fn get(&self, id: &str) -> Option<Auth> {
        Manager::get(self, id)
    }
    fn config_changed(&self, config: &Arc<Config>) {
        Manager::set_config(self, config.clone());
    }
    async fn models_registered(&self, auth_id: &str) {
        Manager::reconcile_registry_model_states(self, auth_id);
    }
    fn auth_batch_applied(&self) {
        Manager::refresh_api_key_model_alias(self);
    }
}

/// Configures and creates a [`Service`] (Go: `cliproxy.Builder`).
pub struct ServiceBuilder {
    config_path: PathBuf,
    executors: Vec<DynExecutor>,
    executor_factory: Option<ExecutorFactory>,
    manager: Option<SharedManager>,
    port: Option<Arc<dyn ManagerPort>>,
    registry: &'static ModelRegistry,
    dotenv_dir: Option<PathBuf>,
    watch: bool,
    antigravity_probe: bool,
}

impl ServiceBuilder {
    /// A builder for the config at `config_path`. `.env` is read from the current directory (as
    /// the Go entrypoint does), file watching and the Antigravity probe are on.
    pub fn new(config_path: impl Into<PathBuf>) -> Self {
        Self {
            config_path: config_path.into(),
            executors: Vec::new(),
            executor_factory: None,
            manager: None,
            port: None,
            registry: global_registry(),
            dotenv_dir: std::env::current_dir().ok(),
            watch: true,
            antigravity_probe: true,
        }
    }

    /// An executor registered with the manager when the service starts.
    pub fn executor(mut self, executor: DynExecutor) -> Self {
        self.executors.push(executor);
        self
    }

    pub fn executors(mut self, executors: impl IntoIterator<Item = DynExecutor>) -> Self {
        self.executors.extend(executors);
        self
    }

    /// Creates executors on demand for providers none of the registered executors handles.
    pub fn executor_factory(mut self, factory: ExecutorFactory) -> Self {
        self.executor_factory = Some(factory);
        self
    }

    /// Uses an existing manager instead of creating one (Go: `WithCoreAuthManager`).
    pub fn manager(mut self, manager: SharedManager) -> Self {
        self.manager = Some(manager);
        self
    }

    /// Registry to register models in; defaults to the process-wide one.
    pub fn registry(mut self, registry: &'static ModelRegistry) -> Self {
        self.registry = registry;
        self
    }

    /// Directory whose `.env` is loaded before the config; `None` skips it.
    pub fn dotenv_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.dotenv_dir = dir;
        self
    }

    /// Whether `start` watches the config file and auth directory (default true).
    pub fn watch(mut self, watch: bool) -> Self {
        self.watch = watch;
        self
    }

    /// Whether Antigravity auths probe `fetchAvailableModels` for web-search support (default true).
    pub fn antigravity_probe(mut self, enabled: bool) -> Self {
        self.antigravity_probe = enabled;
        self
    }

    /// Replaces the conductor operations the service calls (tests). The `manager()` accessor still
    /// returns the real manager.
    #[cfg(test)]
    pub(crate) fn manager_port(mut self, port: Arc<dyn ManagerPort>) -> Self {
        self.port = Some(port);
        self
    }

    /// Loads `.env` and the config, validates credential weights, resolves `auth-dir` and opens
    /// the auth store. Nothing is registered or watched until [`Service::start`].
    pub fn build(self) -> Result<Service, ServiceError> {
        if let Some(dir) = &self.dotenv_dir
            && let Err(err) = load_dotenv(dir)
        {
            tracing::warn!("failed to load .env file: {err}");
        }
        let config_path = std::path::absolute(&self.config_path).map_err(ServiceError::ConfigPath)?;
        let mut config = load_config(&config_path)?;
        config.validate_credential_weights()?;
        match resolve_auth_dir(&config.auth_dir) {
            Ok(dir) => config.auth_dir = dir.to_string_lossy().into_owned(),
            Err(err) => tracing::error!("failed to resolve auth directory: {err}"),
        }

        let store = Arc::new(FileTokenStore::with_dir(&config.auth_dir));
        let manager = self.manager.unwrap_or_default();
        let port: Arc<dyn ManagerPort> = match self.port {
            Some(port) => port,
            None => manager.clone(),
        };
        let registered: HashSet<String> = self.executors.iter().map(|e| e.identifier().to_string()).collect();
        let config = Arc::new(config);
        let (config_tx, _) = watch::channel(config.clone());
        let (applied_tx, _) = watch::channel(0u64);
        let inner = Inner {
            config_path,
            config_tx,
            applied_tx,
            store,
            manager,
            port,
            registry: self.registry,
            sync: Mutex::new(AuthSync::new(config.clone(), config.auth_dir.clone())),
            apply_lock: tokio::sync::Mutex::new(()),
            pending_executors: Mutex::new(self.executors),
            registered_executors: Mutex::new(registered),
            executor_factory: self.executor_factory,
            prober: self.antigravity_probe.then(Prober::default),
            probes: Mutex::new(Vec::new()),
            watch: self.watch,
            config_watcher: Mutex::new(None),
            task: Mutex::new(None),
        };
        Ok(Service { inner: Arc::new(inner) })
    }
}

struct Inner {
    config_path: PathBuf,
    /// The committed config; `send_replace`d on every accepted reload.
    config_tx: watch::Sender<Arc<Config>>,
    /// Counts fully applied config reloads (lets `reload_config` wait for the background apply).
    applied_tx: watch::Sender<u64>,
    store: Arc<FileTokenStore>,
    manager: SharedManager,
    port: Arc<dyn ManagerPort>,
    registry: &'static ModelRegistry,
    sync: Mutex<AuthSync>,
    /// Serializes application of auth updates (Go: `authUpdateMu`).
    apply_lock: tokio::sync::Mutex<()>,
    pending_executors: Mutex<Vec<DynExecutor>>,
    registered_executors: Mutex<HashSet<String>>,
    executor_factory: Option<ExecutorFactory>,
    prober: Option<Prober>,
    probes: Mutex<Vec<JoinHandle<()>>>,
    watch: bool,
    config_watcher: Mutex<Option<Arc<ConfigWatcher>>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

/// A running (or startable) proxy core: config, auth store, manager and model registry wired
/// together. Cheap to clone.
#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}

impl Service {
    /// The current config snapshot.
    pub fn config(&self) -> Arc<Config> {
        self.inner.config_tx.borrow().clone()
    }

    /// Yields a new snapshot after every accepted config reload.
    pub fn subscribe_config(&self) -> watch::Receiver<Arc<Config>> {
        self.inner.config_tx.subscribe()
    }

    pub fn manager(&self) -> SharedManager {
        self.inner.manager.clone()
    }

    pub fn store(&self) -> Arc<FileTokenStore> {
        self.inner.store.clone()
    }

    pub fn registry(&self) -> &'static ModelRegistry {
        self.inner.registry
    }

    pub fn config_path(&self) -> &Path {
        &self.inner.config_path
    }

    /// Registers an executor now (e.g. one created after `start`).
    pub fn register_executor(&self, executor: DynExecutor) {
        self.inner.register_executor(executor);
    }

    /// Go `Service.Run` up to the watcher: creates the auth dir, registers executors, loads the
    /// store into the manager, synthesizes config and file auths, registers their models, then
    /// (unless disabled) starts watching the config file and auth dir in a background task.
    pub async fn start(&self) -> Result<(), ServiceError> {
        let inner = &self.inner;
        if inner.task.lock().is_some() || inner.config_watcher.lock().is_some() {
            return Err(ServiceError::AlreadyStarted);
        }
        let cfg = self.config();
        ensure_auth_dir(&cfg.auth_dir)?;
        inner.store.set_base_dir(&cfg.auth_dir);
        inner.port.config_changed(&cfg);

        let pending = std::mem::take(&mut *inner.pending_executors.lock());
        for executor in pending {
            inner.port.register_executor(executor);
        }

        // Go `Manager.Load`: the store's auths enter the manager first (with file mtimes as
        // `created_at`); synthesized auths then update them.
        match inner.store.list() {
            Ok(auths) => {
                for auth in auths {
                    let id = auth.id.clone();
                    if let Err(err) = inner.port.update(auth).await {
                        tracing::warn!("failed to load auth {id} from store: {err}");
                    }
                }
            }
            Err(err) => tracing::warn!("failed to load auth store: {err}"),
        }

        let updates = inner.sync.lock().reload_clients(true, &[], false);
        inner.apply_updates(updates).await;

        if inner.watch {
            self.start_watchers(&cfg)?;
        }
        Ok(())
    }

    fn start_watchers(&self, cfg: &Arc<Config>) -> Result<(), ServiceError> {
        let inner = &self.inner;
        let watcher = Arc::new(
            ConfigWatcher::start(&inner.config_path, cfg.clone(), None).map_err(|e| ServiceError::Watcher(e.to_string()))?,
        );
        let config_rx = watcher.subscribe();
        *inner.config_watcher.lock() = Some(watcher);
        let auth_watcher = start_auth_watcher(&inner.store, &cfg.auth_dir)?;
        let task = tokio::spawn(run_loop(Arc::downgrade(inner), config_rx, Some(auth_watcher)));
        *inner.task.lock() = Some(task);
        tracing::info!("file watcher started for config and auth directory changes");
        Ok(())
    }

    /// Re-reads the config file now and waits until the new snapshot has been applied (Go:
    /// `reloadConfigFromWatcher`, used by the management API after it writes the config).
    /// Returns false when nothing changed or the new config was rejected.
    pub async fn reload_config(&self) -> bool {
        let inner = &self.inner;
        let Some(watcher) = inner.config_watcher.lock().clone() else {
            return false;
        };
        let mut applied = inner.applied_tx.subscribe();
        applied.mark_unchanged();
        if !watcher.reload_now().await {
            return false;
        }
        applied.changed().await.is_ok()
    }

    /// Go `runtimeAuthSyncHook` / `DispatchPersistedAuthUpdate`: registers an auth that a login or
    /// management call just persisted, without waiting for the file event.
    pub async fn sync_persisted_auth(&self, auth: Auth) {
        let inner = &self.inner;
        if !inner.sync.lock().note_persisted(&auth) {
            return;
        }
        let action = if inner.port.get(&auth.id).is_some() { AuthUpdateAction::Modify } else { AuthUpdateAction::Add };
        let update = AuthUpdate { action, id: auth.id.clone(), auth: Some(auth) };
        inner.apply_updates(vec![update]).await;
    }

    /// Waits for in-flight Antigravity capability probes (Go: `WaitAntigravityProbes`).
    pub async fn wait_antigravity_probes(&self) {
        let handles = std::mem::take(&mut *self.inner.probes.lock());
        for handle in handles {
            let _ = handle.await;
        }
    }

    /// Stops the watchers. Registered auths and models stay as they are.
    pub fn shutdown(&self) {
        if let Some(task) = self.inner.task.lock().take() {
            task.abort();
        }
        self.inner.config_watcher.lock().take();
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().take() {
            task.abort();
        }
    }
}

/// Go `ensureAuthDir`.
fn ensure_auth_dir(dir: &str) -> Result<(), ServiceError> {
    match fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(ServiceError::AuthDirNotDirectory(dir.to_string())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(dir).map_err(|source| ServiceError::AuthDir { path: dir.to_string(), source })?;
            tracing::info!("created missing auth directory: {dir}");
            Ok(())
        }
        Err(source) => Err(ServiceError::AuthDir { path: dir.to_string(), source }),
    }
}

fn start_auth_watcher(store: &Arc<FileTokenStore>, dir: &str) -> Result<AuthWatcher, ServiceError> {
    // The initial scan is replayed so removals of files present at start are recognized; the sync
    // state's content hashes turn those replayed events into no-ops.
    AuthWatcher::start(store.clone(), Path::new(dir), WatchOptions::default())
        .map_err(|e| ServiceError::Watcher(e.to_string()))
}

/// Background loop: applies committed config reloads and auth-dir file events in order.
async fn run_loop(inner: Weak<Inner>, mut config_rx: watch::Receiver<Arc<Config>>, mut auth_watcher: Option<AuthWatcher>) {
    loop {
        enum Event {
            Config,
            Auth(Option<Box<AuthFileEvent>>),
        }
        let event = tokio::select! {
            changed = config_rx.changed() => match changed {
                Ok(()) => Event::Config,
                Err(_) => return,
            },
            event = async {
                match auth_watcher.as_mut() {
                    Some(watcher) => watcher.next().await,
                    None => std::future::pending().await,
                }
            } => Event::Auth(event.map(Box::new)),
        };
        let Some(inner) = inner.upgrade() else { return };
        match event {
            Event::Config => {
                let cfg = config_rx.borrow_and_update().clone();
                if inner.apply_config(cfg.clone()).await {
                    // The auth dir moved: watch the new one.
                    match start_auth_watcher(&inner.store, &cfg.auth_dir) {
                        Ok(watcher) => auth_watcher = Some(watcher),
                        Err(err) => {
                            tracing::error!("{err}");
                            auth_watcher = None;
                        }
                    }
                }
                inner.applied_tx.send_modify(|n| *n += 1);
            }
            Event::Auth(None) => auth_watcher = None,
            Event::Auth(Some(event)) => inner.handle_auth_file_event(*event).await,
        }
    }
}

impl Inner {
    fn config(&self) -> Arc<Config> {
        self.config_tx.borrow().clone()
    }

    fn register_executor(&self, executor: DynExecutor) {
        self.registered_executors.lock().insert(executor.identifier().to_string());
        self.port.register_executor(executor);
    }

    /// Go `ensureExecutorsForAuth` for providers outside the registered set: asks the factory.
    fn ensure_executor_for_auth(&self, auth: &Auth) {
        let Some(factory) = &self.executor_factory else { return };
        // Disabled auths never (re)bind executors.
        if auth.disabled {
            return;
        }
        let key = executor_key_for_auth(auth);
        if key.is_empty() || self.registered_executors.lock().contains(&key) {
            return;
        }
        if let Some(executor) = factory(&key) {
            self.register_executor(executor);
        }
    }

    async fn handle_auth_file_event(&self, event: AuthFileEvent) {
        let updates = match event {
            AuthFileEvent::Added(auth) | AuthFileEvent::Updated(auth) => {
                let path = auth.attr(ATTRIBUTE_PATH);
                let path = if path.is_empty() { auth.attr(ATTRIBUTE_SOURCE) } else { path };
                if path.is_empty() {
                    return;
                }
                self.sync.lock().file_changed(Path::new(&path))
            }
            AuthFileEvent::Removed { path, .. } => self.sync.lock().file_removed(&path),
        };
        self.apply_updates(updates).await;
    }

    /// Go `applyConfigUpdateWithAuthSynthesis` (watcher path) plus the watcher's `reloadConfig`
    /// decisions: commits `new`, diffs it against the previous config and republishes the
    /// affected auths. Returns whether the auth directory changed. Configs with invalid
    /// credential weights are rejected.
    async fn apply_config(&self, new: Arc<Config>) -> bool {
        if let Err(err) = new.validate_credential_weights() {
            tracing::warn!("rejected config update with invalid credential weights: {err}");
            return false;
        }
        let old = self.config();
        let plan = ReloadPlan::between(Some(&old), &new);
        self.config_tx.send_replace(new.clone());
        self.port.config_changed(&new);
        if plan.auth_dir_changed {
            self.store.set_base_dir(&new.auth_dir);
            if let Err(err) = ensure_auth_dir(&new.auth_dir) {
                tracing::error!("{err}");
            }
        }
        let updates = {
            let mut sync = self.sync.lock();
            sync.set_config(new.clone());
            if plan.auth_dir_changed {
                sync.set_auth_dir(new.auth_dir.clone());
            }
            sync.reload_clients(plan.auth_dir_changed, &plan.affected_oauth_providers, plan.force_auth_refresh)
        };
        self.apply_updates(updates).await;
        plan.auth_dir_changed
    }

    /// Go `handleAuthUpdates`: add/modify updates the manager and re-registers models, delete
    /// unregisters and removes. Updates apply in order, one batch at a time.
    async fn apply_updates(&self, updates: Vec<AuthUpdate>) {
        if updates.is_empty() {
            return;
        }
        let _guard = self.apply_lock.lock().await;
        let cfg = self.config();
        for update in updates {
            match update.action {
                AuthUpdateAction::Add | AuthUpdateAction::Modify => {
                    if let Some(auth) = update.auth.filter(|a| !a.id.is_empty()) {
                        self.apply_add_or_update(&cfg, auth).await;
                    }
                }
                AuthUpdateAction::Delete => {
                    if !update.id.is_empty() {
                        self.apply_removal(&update.id).await;
                    }
                }
            }
        }
        self.port.auth_batch_applied();
    }

    /// Go `applyCoreAuthRemoval`.
    async fn apply_removal(&self, id: &str) {
        self.registry.unregister_client(id);
        self.port.remove(id).await;
    }

    /// Go `prepareCoreAuthForModelRegistration` + `completeModelRegistrationForAuth`.
    async fn apply_add_or_update(&self, cfg: &Arc<Config>, mut auth: Auth) {
        self.ensure_executor_for_auth(&auth);

        // The manager is updated first so proxy/prefix changes take effect before models register.
        if let Some(existing) = self.port.get(&auth.id) {
            if is_stale_core_auth(&existing, &auth) {
                tracing::debug!("skipping stale auth update for {}", auth.id);
                auth = existing;
                self.register_models(cfg, &auth).await;
                return;
            }
            auth.created_at = existing.created_at;
            let both_enabled = !existing.disabled
                && existing.status != Status::Disabled
                && !auth.disabled
                && auth.status != Status::Disabled;
            if both_enabled {
                auth.last_refreshed_at = existing.last_refreshed_at;
                auth.next_refresh_after = existing.next_refresh_after;
                if auth.model_states.is_empty() && !existing.model_states.is_empty() {
                    auth.model_states = existing.model_states;
                }
            }
        }
        if let Err(err) = self.port.update(auth.clone()).await {
            tracing::error!("failed to update auth {}: {err}", auth.id);
            match self.port.get(&auth.id) {
                Some(current) if !current.disabled => auth = current,
                _ => {
                    self.registry.unregister_client(&auth.id);
                    return;
                }
            }
        }
        if self.should_skip_model_registration(&auth.id, auth.disabled) {
            return;
        }
        self.register_models(cfg, &auth).await;
    }

    /// Go `shouldSkipModelRegistration` (without the generation check: synthesized snapshots are
    /// unversioned).
    fn should_skip_model_registration(&self, id: &str, expected_disabled: bool) -> bool {
        match self.port.get(id) {
            None => !expected_disabled,
            Some(current) => current.disabled != expected_disabled,
        }
    }

    /// Go `registerModelsForAuth` and the follow-up reconcile / scheduler refresh.
    async fn register_models(&self, cfg: &Arc<Config>, auth: &Auth) {
        if auth.disabled {
            if self.port.get(&auth.id).is_some_and(|c| !c.disabled) {
                return;
            }
            self.registry.unregister_client(&auth.id);
            return;
        }
        if self.port.get(&auth.id).is_none_or(|c| c.disabled) {
            return;
        }
        apply_registration(self.registry, &auth.id, resolve_models_for_auth(cfg, auth));
        self.probe_antigravity(cfg, auth);
        self.port.models_registered(&auth.id).await;
    }

    /// Go `asyncProbeAntigravityCapabilities`: flags web-search models after the fact, if the
    /// registration and the auth are still the ones that were probed.
    fn probe_antigravity(&self, cfg: &Arc<Config>, auth: &Auth) {
        let Some(prober) = self.prober.clone() else { return };
        if !auth.provider.trim().eq_ignore_ascii_case("antigravity") || auth.disabled || auth.id.is_empty() {
            return;
        }
        let auth = auth.clone();
        let (expected_epoch, expected_prefix) = (auth.registration_epoch, auth.prefix.clone());
        let expected_registry_epoch = self.registry.client_registration_epoch(&auth.id);
        let (registry, port, cfg) = (self.registry, self.port.clone(), cfg.clone());
        let handle = tokio::spawn(async move {
            let hints = prober.fetch_hints(&auth, &cfg.proxy_url).await;
            if hints.is_empty() {
                return;
            }
            match port.get(&auth.id) {
                Some(current)
                    if !current.disabled
                        && current.registration_epoch == expected_epoch
                        && current.prefix == expected_prefix => {}
                _ => return,
            }
            let alias_map = reverse_alias_map(&cfg, &auth);
            let updated = registry.apply_client_model_capabilities(&auth.id, expected_registry_epoch, |model_id, info| {
                let upstream = resolve_upstream_model_id(model_id, &auth.prefix, &alias_map);
                if hints.contains(&upstream) {
                    info.supports_web_search = true;
                }
            });
            if updated {
                port.models_registered(&auth.id).await;
            }
        });
        let mut probes = self.probes.lock();
        probes.retain(|h| !h.is_finished());
        probes.push(handle);
    }
}

/// Go `isStaleCoreAuth`: an update older than the manager's copy by epoch or generation.
fn is_stale_core_auth(existing: &Auth, incoming: &Auth) -> bool {
    (incoming.registration_epoch > 0 && incoming.registration_epoch < existing.registration_epoch)
        || (incoming.generation > 0 && incoming.generation < existing.generation)
}

/// The executor identifier that serves `auth` (docs/survey 1.3 `executorKeyFromAuth`).
fn executor_key_for_auth(auth: &Auth) -> String {
    if let Some((key, _)) = openai_compat_info_from_auth(auth) {
        return if key.is_empty() { "openai-compatibility".into() } else { key };
    }
    match auth.provider.trim().to_lowercase().as_str() {
        "kimi.com" => "kimi".into(),
        "kimi.ai" => "kimi-ai".into(),
        other => other.to_string(),
    }
}

