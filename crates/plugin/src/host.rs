//! The plugin host: loading, hot reload, fusing and the runtime snapshot (Go: `host.go`,
//! `snapshot.go`, `rpc_client.go` registration).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use cpa_config::Config;
use cpa_runtime::conductor::SharedManager;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;

use cpa_pluginapi::abi;
use cpa_pluginapi::api::PluginMetadata;
use crate::bridge::Bridges;
use crate::callbacks::ModelExecutor;
use crate::caps::{PluginIdentifier, PluginInfo, Record, Registration};
use crate::client::{CallbackInstance, Empty, GuardedClient, PluginError, RawClient, call_plugin, empty_request};
use crate::config::{RuntimeConfig, RuntimeItem, default_runtime_item, desired_versions, runtime_config_from_config};
use crate::ctx::CallCtx;
use crate::loader::{DynClient, HostCallbacks};
use crate::platform::{PluginFile, clean_path, cleanup_unselected_files, select_plugin_files};

/// Opens plugin binaries (Go: `pluginLoader`); replaceable in tests.
pub trait PluginLoader: Send + Sync {
    fn open(
        &self,
        file: &PluginFile,
        host: Arc<dyn HostCallbacks>,
        instance: Arc<CallbackInstance>,
    ) -> Result<Arc<dyn RawClient>, PluginError>;
}

/// The dlopen loader.
pub struct DynLoader;

impl PluginLoader for DynLoader {
    fn open(
        &self,
        file: &PluginFile,
        host: Arc<dyn HostCallbacks>,
        instance: Arc<CallbackInstance>,
    ) -> Result<Arc<dyn RawClient>, PluginError> {
        let client = DynClient::open(&file.path, &file.id, host, instance)?;
        Ok(client)
    }
}

/// A plugin library that is open in this process.
pub(crate) struct LoadedPlugin {
    pub id: String,
    pub path: PathBuf,
    pub client: Arc<GuardedClient>,
    st: Mutex<LoadedState>,
}

#[derive(Default)]
struct LoadedState {
    version: String,
    name: String,
    config_yaml: Vec<u8>,
    plugin: Option<PluginInfo>,
    registered: bool,
}

impl LoadedPlugin {
    fn version(&self) -> String {
        self.st.lock().version.clone()
    }
    fn name(&self) -> String {
        self.st.lock().name.clone()
    }
}

/// The active plugin set; replaced atomically on every apply (Go: `Snapshot`).
pub struct Snapshot {
    pub enabled: bool,
    pub records: Vec<Arc<Record>>,
    /// `DescribeQuota.supported_providers` per plugin, cached for this snapshot.
    pub(crate) quota_supported: Mutex<HashMap<String, Vec<String>>>,
}

impl Snapshot {
    pub fn empty() -> Arc<Snapshot> {
        Arc::new(Snapshot { enabled: false, records: Vec::new(), quota_supported: Mutex::new(HashMap::new()) })
    }
}

/// Plugin summary for the management API (Go: `RegisteredPluginInfo`).
#[derive(Debug, Clone, Default)]
pub struct RegisteredPluginInfo {
    pub id: String,
    pub priority: i64,
    pub metadata: PluginMetadata,
    pub supports_oauth: bool,
    pub oauth_provider: String,
    pub supports_quota: bool,
    pub quota_provider: String,
    pub menus: Vec<RegisteredPluginMenu>,
}

#[derive(Debug, Clone, Default)]
pub struct RegisteredPluginMenu {
    pub path: String,
    pub menu: String,
    pub description: String,
}

#[derive(Default)]
pub(crate) struct LoadRequest {
    pub closed: bool,
    pub cleanup_started: bool,
    pub instances: Vec<Arc<CallbackInstance>>,
}

#[derive(Default)]
pub(crate) struct State {
    pub loaded: HashMap<String, Arc<LoadedPlugin>>,
    pub retired: HashMap<String, Vec<Arc<LoadedPlugin>>>,
    pub loading: HashMap<String, Arc<Mutex<LoadRequest>>>,
    pub fused: HashMap<String, String>,
    pub plugin_file_versions: HashMap<PathBuf, String>,
    pub active_versions: HashMap<String, String>,
    pub active_paths: HashMap<String, PathBuf>,
    pub cleanup_files_pending: bool,
    pub runtime_config: Option<Arc<Config>>,
    pub auth_manager: Option<SharedManager>,
    pub model_executor: Option<Arc<dyn ModelExecutor>>,
    pub model_client_ids: HashSet<String>,
    pub executor_model_client_ids: HashSet<String>,
    pub model_providers: HashMap<String, String>,
    pub model_registrations: HashMap<String, crate::adapters::models::ModelRegistration>,
    pub provider_models: HashMap<String, Vec<cpa_core::registry::ModelInfo>>,
    pub executor_providers: HashSet<String>,
    pub access_provider_keys: HashSet<String>,
    pub usage_listener_keys: HashSet<String>,
    pub executor_adapters: HashMap<String, Arc<crate::adapters::executors::ExecutorAdapter>>,
    pub command_line_flags: HashMap<String, crate::cli::FlagRecord>,
    pub command_line_hits: HashSet<String>,
    pub management_routes: HashMap<String, crate::management::ManagementRouteRecord>,
    pub resource_routes: HashMap<String, crate::management::ResourceRouteRecord>,
}

pub struct Host {
    me: Weak<Host>,
    loader: Mutex<Arc<dyn PluginLoader>>,
    pub(crate) state: Mutex<State>,
    apply: tokio::sync::Mutex<()>,
    snapshot: RwLock<Arc<Snapshot>>,
    pub(crate) bridges: Bridges,
    pub(crate) access: Mutex<crate::adapters::access::AccessRegistry>,
    runtime: Mutex<Option<tokio::runtime::Handle>>,
}

#[derive(Serialize)]
struct LifecycleRequest<'a> {
    #[serde(with = "cpa_pluginapi::wire::b64")]
    config_yaml: &'a [u8],
    schema_version: u32,
}

impl Host {
    pub fn new() -> Arc<Host> {
        Self::with_loader(Arc::new(DynLoader))
    }

    pub fn with_loader(loader: Arc<dyn PluginLoader>) -> Arc<Host> {
        Arc::new_cyclic(|me| Host {
            me: me.clone(),
            loader: Mutex::new(loader),
            state: Mutex::new(State { cleanup_files_pending: true, ..Default::default() }),
            apply: tokio::sync::Mutex::new(()),
            snapshot: RwLock::new(Snapshot::empty()),
            bridges: Bridges::default(),
            access: Mutex::new(Default::default()),
            runtime: Mutex::new(None),
        })
    }

    pub(crate) fn arc(&self) -> Arc<Host> {
        self.me.upgrade().expect("host is alive while borrowed")
    }

    /// Runtime handle host callbacks block on; captured from the first async entry point.
    pub(crate) fn note_runtime(&self) {
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            *self.runtime.lock() = Some(h);
        }
    }

    pub(crate) fn runtime_handle(&self) -> Option<tokio::runtime::Handle> {
        self.runtime.lock().clone()
    }

    pub fn set_model_executor(&self, executor: Option<Arc<dyn ModelExecutor>>) {
        self.state.lock().model_executor = executor;
    }

    pub(crate) fn model_executor(&self) -> Option<Arc<dyn ModelExecutor>> {
        self.state.lock().model_executor.clone()
    }

    pub fn set_auth_manager(&self, manager: Option<SharedManager>) {
        self.state.lock().auth_manager = manager;
    }

    pub fn auth_manager(&self) -> Option<SharedManager> {
        self.state.lock().auth_manager.clone()
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.read().clone()
    }

    pub(crate) fn store_snapshot(&self, snap: Arc<Snapshot>) {
        *self.snapshot.write() = snap;
    }

    /// The configuration the host was last applied with.
    pub fn runtime_config(&self) -> Option<Arc<Config>> {
        self.state.lock().runtime_config.clone()
    }

    /// Whether a plugin library is still loaded (active or retired).
    pub fn plugin_loaded(&self, id: &str) -> bool {
        let id = id.trim();
        if id.is_empty() {
            return false;
        }
        let st = self.state.lock();
        st.loaded.contains_key(id) || st.retired.get(id).is_some_and(|v| !v.is_empty())
    }

    /// Whether a plugin library is loaded or being loaded.
    pub fn plugin_busy(&self, id: &str) -> bool {
        let id = id.trim();
        if id.is_empty() {
            return false;
        }
        let st = self.state.lock();
        st.loaded.contains_key(id) || st.retired.get(id).is_some_and(|v| !v.is_empty()) || st.loading.contains_key(id)
    }

    // ---- fusing ----

    /// Disables a plugin after a panic in one of its calls (Go: `fusePlugin`).
    pub(crate) fn fuse_plugin(&self, id: &str, method: &str, detail: &str) {
        self.state.lock().fused.insert(id.to_string(), format!("{method} panic: {detail}"));
        cpa_core::thinking::unregister_plugin_providers(id);
        tracing::error!(plugin_id = id, method, "pluginhost: plugin panic recovered: {detail}");
    }

    pub fn is_plugin_fused(&self, id: &str) -> bool {
        self.state.lock().fused.contains_key(id)
    }

    // ---- snapshot access ----

    /// Records of the current snapshot whose identity still matches the active maps.
    pub fn active_records(&self) -> Vec<Arc<Record>> {
        let snap = self.snapshot();
        self.active_records_from(&snap)
    }

    /// True when at least one plugin is loaded and current; the server only enters its plugin
    /// pipeline then.
    pub fn has_active_plugins(&self) -> bool {
        let snap = self.snapshot();
        snap.records.iter().any(|r| self.record_current(r))
    }

    pub fn active_records_from(&self, snap: &Snapshot) -> Vec<Arc<Record>> {
        snap.records.iter().filter(|r| self.record_current(r)).cloned().collect()
    }

    pub fn record_current(&self, record: &Record) -> bool {
        self.plugin_identity_current(&record.id, &record.path, &record.version)
    }

    pub fn plugin_identity_current(&self, id: &str, path: &Path, version: &str) -> bool {
        let id = id.trim();
        if id.is_empty() {
            return false;
        }
        let version = version.trim();
        let path = clean_path(path);
        let st = self.state.lock();
        if st.active_paths.get(id) != Some(&path) {
            return false;
        }
        match st.plugin_file_versions.get(&path) {
            Some(v) if v == version => {}
            _ => return false,
        }
        st.active_versions.get(id).map(String::as_str) == Some(version)
    }

    pub fn plugin_registered(&self, id: &str) -> bool {
        let id = id.trim();
        !id.is_empty() && self.active_records().iter().any(|r| r.id == id)
    }

    fn rebuild_active_maps(st: &mut State, records: &[Arc<Record>]) {
        st.plugin_file_versions.clear();
        st.active_versions.clear();
        st.active_paths.clear();
        for r in records {
            let id = r.id.trim();
            let path = clean_path(&r.path);
            if id.is_empty() || path.as_os_str().is_empty() {
                continue;
            }
            st.plugin_file_versions.insert(path.clone(), r.version.trim().to_string());
            st.active_versions.insert(id.to_string(), r.version.trim().to_string());
            st.active_paths.insert(id.to_string(), path);
        }
    }

    fn remove_runtime_state(st: &mut State, id: &str) {
        st.management_routes.retain(|_, r| r.plugin_id != id);
        st.resource_routes.retain(|_, r| r.plugin_id != id);
        let removed: Vec<String> =
            st.command_line_flags.iter().filter(|(_, r)| r.plugin_id == id).map(|(n, _)| n.clone()).collect();
        for name in removed {
            st.command_line_flags.remove(&name);
            st.command_line_hits.remove(&name);
        }
        if let Some(reg) = st.model_registrations.remove(id) {
            st.provider_models.remove(&reg.provider);
        }
        st.model_providers.remove(id);
    }

    fn clear_routes_and_maps(&self) {
        let mut st = self.state.lock();
        st.management_routes.clear();
        st.resource_routes.clear();
        Self::rebuild_active_maps(&mut st, &[]);
        drop(st);
        self.store_snapshot(Snapshot::empty());
    }

    async fn lock_apply(&self, ctx: &CallCtx) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        tokio::select! {
            g = self.apply.lock() => Some(g),
            () = ctx.cancelled() => None,
        }
    }

    // ---- applying configuration ----

    /// Loads, reconfigures, hot-reloads and unloads plugins to match `cfg` (Go: `ApplyConfig`).
    pub async fn apply_config(&self, ctx: &CallCtx, cfg: Option<Arc<Config>>) {
        self.note_runtime();
        let Some(_guard) = self.lock_apply(ctx).await else { return };
        if ctx.is_canceled() {
            return;
        }
        let rc = match runtime_config_from_config(cfg.as_deref()) {
            Ok(rc) => rc,
            Err(e) => {
                tracing::error!("failed to apply plugin runtime config: {e}");
                return;
            }
        };
        self.state.lock().runtime_config = cfg;

        if !rc.enabled {
            self.clear_routes_and_maps();
            self.arc().refresh_thinking_providers(&[]);
            return;
        }
        let desired = desired_versions(&rc.items);
        let files = match select_plugin_files(&rc.dir, &desired) {
            Ok((files, _)) => files,
            Err(e) => {
                tracing::warn!("pluginhost: failed to select plugin files: {e}");
                self.clear_routes_and_maps();
                self.arc().refresh_thinking_providers(&[]);
                return;
            }
        };
        let files = self.with_loaded_fallbacks(files, &rc, &desired);

        let mut records: Vec<Arc<Record>> = Vec::new();
        let mut loaded_files: Vec<PluginFile> = Vec::new();
        let mut hot_reload_logs: Vec<(String, String, PathBuf, String, PathBuf)> = Vec::new();

        for file in files {
            let item = rc.items.get(&file.id).cloned().unwrap_or_else(|| default_runtime_item(&file.id));
            if !item.enabled {
                continue;
            }
            let (mut lp, mut replaced, disabled) = {
                let st = self.state.lock();
                let lp = st.loaded.get(&file.id).cloned();
                let fused = st.fused.contains_key(&file.id);
                (lp, None::<Arc<LoadedPlugin>>, fused)
            };
            if let Some(cur) = &lp
                && clean_path(&cur.path) != clean_path(&file.path)
            {
                replaced = lp.take();
            }
            if disabled && replaced.is_none() {
                continue;
            }

            let mut loaded_now = false;
            let mut info: Option<PluginInfo> = None;
            let mut registered_now = false;
            let mut hot_reload: Option<(String, String, PathBuf, String, PathBuf)> = None;

            if lp.is_none() {
                let request = Arc::new(Mutex::new(LoadRequest::default()));
                {
                    let mut st = self.state.lock();
                    if st.loading.contains_key(&file.id) {
                        continue;
                    }
                    st.loading.insert(file.id.clone(), request.clone());
                }
                if let Some(old) = &replaced {
                    self.call_quiesce(ctx, old).await;
                    if ctx.is_canceled() {
                        self.clear_loading(&file.id, &request);
                        self.rollback_replacement(old, &item).await;
                        return;
                    }
                }
                let outcome = self.load_plugin(ctx, &file, &item, &request).await;
                let LoadOutcome { loaded, info: load_info, err, completed } = outcome;
                if !completed {
                    // Canceled mid-load: the library is discarded once the load finishes.
                    if let Some(old) = &replaced {
                        self.cleanup_load(&file.id, &request, loaded).await;
                        self.rollback_replacement(old, &item).await;
                    } else {
                        self.cleanup_load(&file.id, &request, loaded).await;
                    }
                    return;
                }
                if err.is_some() || (replaced.is_some() && load_info.is_none()) {
                    self.cleanup_load(&file.id, &request, loaded).await;
                    if let Some(old) = &replaced
                        && let Some(rec) = self.rollback_replacement(old, &item).await
                    {
                        loaded_files.push(PluginFile { id: rec.id.clone(), path: rec.path.clone(), version: rec.version.clone() });
                        records.push(rec);
                    }
                    if let Some(e) = err {
                        tracing::warn!("pluginhost: failed to load plugin {} from {}: {e}", file.id, file.path.display());
                    }
                    continue;
                }
                let Some(new_lp) = loaded else { continue };
                {
                    let mut st = self.state.lock();
                    let still_ours = st.loading.get(&file.id).is_some_and(|r| Arc::ptr_eq(r, &request));
                    if !still_ours {
                        drop(st);
                        self.discard_loaded(&new_lp);
                        if let Some(old) = &replaced {
                            self.rollback_replacement(old, &item).await;
                        }
                        return;
                    }
                    if ctx.is_canceled() {
                        drop(st);
                        self.cleanup_load(&file.id, &request, Some(new_lp)).await;
                        if let Some(old) = &replaced {
                            self.rollback_replacement(old, &item).await;
                        }
                        return;
                    }
                    st.loading.remove(&file.id);
                    if let Some(old) = &replaced {
                        hot_reload = Some((file.id.clone(), file.version.clone(), file.path.clone(), old.version(), old.path.clone()));
                        st.retired.entry(old.id.clone()).or_default().push(old.clone());
                        st.fused.remove(&file.id);
                        Self::remove_runtime_state(&mut st, &file.id);
                    }
                    st.loaded.insert(file.id.clone(), new_lp.clone());
                }
                loaded_now = true;
                registered_now = load_info.is_some();
                info = load_info;
                lp = Some(new_lp);
                tracing::info!(plugin_id = %file.id, version = %file.version, path = %file.path.display(), "pluginhost: plugin loaded");
            }

            let Some(lp) = lp else { continue };
            if !registered_now {
                if loaded_now {
                    continue;
                }
                match self.call_register(ctx, &lp, &item).await {
                    Some(i) => info = Some(i),
                    None => continue,
                }
            }
            let Some(info) = info else { continue };
            {
                let mut st = lp.st.lock();
                st.name = info.metadata.name.trim().to_string();
                if st.version.trim().is_empty() {
                    st.version = info.metadata.version.trim().to_string();
                }
            }
            if loaded_now {
                tracing::info!(plugin_id = %file.id, plugin_name = %info.metadata.name, version = %info.metadata.version, path = %file.path.display(), "pluginhost: plugin registered");
            }
            if let Some(h) = hot_reload {
                hot_reload_logs.push(h);
            }
            records.push(Arc::new(Record::new(file.id.clone(), file.path.clone(), file.version.clone(), item.priority, info, lp.client.clone())));
            loaded_files.push(file);
        }

        records.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.id.cmp(&b.id)));
        let cleanup_files = {
            let mut st = self.state.lock();
            let pending = st.cleanup_files_pending;
            if !loaded_files.is_empty() {
                st.cleanup_files_pending = false;
            }
            Self::rebuild_active_maps(&mut st, &records);
            pending
        };
        self.store_snapshot(Arc::new(Snapshot { enabled: true, records: records.clone(), quota_supported: Mutex::new(HashMap::new()) }));
        self.arc().refresh_thinking_providers(&records);
        for (id, av, ap, rv, rp) in hot_reload_logs {
            tracing::info!(plugin_id = %id, active_version = %av, active_path = %ap.display(), retired_version = %rv, retired_path = %rp.display(), "pluginhost: plugin hot reloaded");
        }
        if cleanup_files && !loaded_files.is_empty()
            && let Err(e) = cleanup_unselected_files(&rc.dir, &loaded_files)
        {
            tracing::warn!("pluginhost: failed to clean old plugin files: {e}");
        }
    }

    /// Keeps a loaded plugin whose pinned version is missing on disk (Go:
    /// `withLoadedPluginFallbacks`).
    fn with_loaded_fallbacks(&self, mut files: Vec<PluginFile>, rc: &RuntimeConfig, desired: &HashMap<String, String>) -> Vec<PluginFile> {
        if desired.is_empty() {
            return files;
        }
        let mut selected: HashSet<String> = files.iter().map(|f| f.id.trim().to_string()).filter(|i| !i.is_empty()).collect();
        let mut ids: Vec<&String> = desired.keys().collect();
        ids.sort();
        let st = self.state.lock();
        for id in ids {
            if selected.contains(id) {
                continue;
            }
            if rc.items.get(id).is_some_and(|i| !i.enabled) {
                continue;
            }
            let Some(lp) = st.loaded.get(id) else { continue };
            if lp.path.as_os_str().is_empty() {
                continue;
            }
            files.push(PluginFile { id: id.clone(), path: lp.path.clone(), version: lp.version().trim().to_string() });
            selected.insert(id.clone());
        }
        files
    }

    fn clear_loading(&self, id: &str, request: &Arc<Mutex<LoadRequest>>) {
        let mut st = self.state.lock();
        if st.loading.get(id).is_some_and(|r| Arc::ptr_eq(r, request)) {
            st.loading.remove(id);
        }
    }

    /// Opens the library and registers it (Go: `startPluginLoad` + `waitForPluginLoad`).
    async fn load_plugin(&self, ctx: &CallCtx, file: &PluginFile, item: &RuntimeItem, request: &Arc<Mutex<LoadRequest>>) -> LoadOutcome {
        let loader = self.loader.lock().clone();
        let host_cb: Arc<dyn HostCallbacks> = self.arc();
        let instance = Arc::new(CallbackInstance::default());
        {
            let mut req = request.lock();
            req.instances.push(instance.clone());
            if req.closed {
                instance.closed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let f = file.clone();
        let inst = instance.clone();
        let open = tokio::task::spawn_blocking(move || loader.open(&f, host_cb, inst));
        let raw = tokio::select! {
            r = open => r,
            () = ctx.cancelled() => {
                return LoadOutcome { completed: false, ..Default::default() };
            }
        };
        let raw = match raw {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return LoadOutcome { err: Some(e.message), completed: true, ..Default::default() },
            Err(e) => return LoadOutcome { err: Some(format!("plugin loader panicked: {e}")), completed: true, ..Default::default() },
        };
        let client = GuardedClient::new(raw);
        let lp = Arc::new(LoadedPlugin {
            id: file.id.clone(),
            path: file.path.clone(),
            client,
            st: Mutex::new(LoadedState { version: file.version.clone(), ..Default::default() }),
        });
        let info = self.call_register(ctx, &lp, item).await;
        let completed = !ctx.is_canceled();
        LoadOutcome { loaded: Some(lp), info, err: None, completed }
    }

    async fn cleanup_load(&self, id: &str, request: &Arc<Mutex<LoadRequest>>, loaded: Option<Arc<LoadedPlugin>>) {
        let instances = {
            let mut req = request.lock();
            if req.cleanup_started {
                Vec::new()
            } else {
                req.cleanup_started = true;
                req.closed = true;
                for i in &req.instances {
                    i.closed.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                req.instances.clone()
            }
        };
        for i in &instances {
            self.bridges.close_http_callback_instance(id, Some(i));
        }
        if let Some(lp) = loaded {
            self.discard_loaded(&lp);
        }
        self.clear_loading(id, request);
    }

    fn discard_loaded(&self, lp: &Arc<LoadedPlugin>) {
        self.bridges.close_http_callback_instance(&lp.id, Some(&lp.client.instance()));
        lp.client.shutdown(Some(Duration::from_secs(5)));
    }

    // ---- registration ----

    /// `plugin.register` or, on an already registered library, `plugin.reconfigure`.
    async fn call_register(&self, ctx: &CallCtx, lp: &Arc<LoadedPlugin>, item: &RuntimeItem) -> Option<PluginInfo> {
        if ctx.is_canceled() {
            return None;
        }
        let method = if lp.st.lock().registered { abi::METHOD_PLUGIN_RECONFIGURE } else { abi::METHOD_PLUGIN_REGISTER };
        let info = match register_rpc(ctx, &lp.client, method, &item.config_yaml).await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!("pluginhost: plugin {} {} failed: {}", lp.id, method, e);
                return None;
            }
        };
        lp.st.lock().registered = true;
        if !info.is_valid() {
            tracing::warn!("pluginhost: plugin {} returned invalid metadata or no capabilities", lp.id);
            return None;
        }
        let mut st = lp.st.lock();
        st.name = info.metadata.name.trim().to_string();
        if st.version.trim().is_empty() {
            st.version = info.metadata.version.trim().to_string();
        }
        st.config_yaml = item.config_yaml.clone();
        st.plugin = Some(info.clone());
        Some(info)
    }

    async fn call_quiesce(&self, ctx: &CallCtx, lp: &Arc<LoadedPlugin>) -> bool {
        if ctx.is_canceled() {
            return false;
        }
        match call_plugin::<Empty>(&lp.client, ctx, abi::METHOD_PLUGIN_QUIESCE, &empty_request()).await {
            Ok(_) => true,
            Err(e) => {
                let msg = e.message.to_lowercase();
                let unsupported = matches!(e.code.to_lowercase().as_str(), "unknown_method" | "method_not_found" | "unsupported_method")
                    || [
                        "unknown method",
                        "method not found",
                        "unsupported method",
                        "method unsupported",
                        "method is not supported",
                        "method not supported",
                    ]
                    .iter()
                    .any(|s| msg.contains(s));
                if unsupported {
                    tracing::debug!(plugin_id = %lp.id, "pluginhost: plugin quiesce unsupported: {e}");
                } else if e.is_canceled() {
                    tracing::debug!(plugin_id = %lp.id, "pluginhost: plugin quiesce canceled: {e}");
                } else {
                    tracing::warn!(plugin_id = %lp.id, "pluginhost: plugin quiesce failed: {e}");
                }
                false
            }
        }
    }

    /// Re-registers the previous library after a failed hot reload (Go: `rollbackReplacement`).
    async fn rollback_replacement(&self, lp: &Arc<LoadedPlugin>, item: &RuntimeItem) -> Option<Arc<Record>> {
        let (config_yaml, previous) = {
            let st = lp.st.lock();
            (st.config_yaml.clone(), st.plugin.clone())
        };
        let mut item = item.clone();
        if !config_yaml.is_empty() {
            item.config_yaml = config_yaml;
        }
        let ctx = CallCtx::background();
        let info = match self.call_register(&ctx, lp, &item).await {
            Some(info) => {
                self.state.lock().fused.remove(&lp.id);
                info
            }
            None => previous?,
        };
        if !info.is_valid() {
            return None;
        }
        Some(Arc::new(Record::new(lp.id.clone(), lp.path.clone(), lp.version(), item.priority, info, lp.client.clone())))
    }

    // ---- unloading ----

    /// Removes one plugin from the active runtime and closes its library (Go: `UnloadPlugin`).
    pub async fn unload_plugin(&self, ctx: &CallCtx, id: &str) -> bool {
        let id = id.trim().to_string();
        if id.is_empty() {
            return false;
        }
        let Some(_guard) = self.lock_apply(ctx).await else { return false };
        let (targets, request, records, enabled) = {
            let mut st = self.state.lock();
            let mut targets: Vec<Arc<LoadedPlugin>> = Vec::new();
            if let Some(lp) = st.loaded.get(&id) {
                targets.push(lp.clone());
            }
            if let Some(retired) = st.retired.get(&id) {
                targets.extend(retired.iter().cloned());
            }
            let request = st.loading.get(&id).cloned();
            if let Some(r) = &request {
                let mut r = r.lock();
                r.closed = true;
                for i in &r.instances {
                    i.closed.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            if targets.is_empty() && request.is_none() {
                return false;
            }
            st.loaded.remove(&id);
            st.retired.remove(&id);
            st.fused.remove(&id);
            st.active_versions.remove(&id);
            st.active_paths.remove(&id);
            for t in &targets {
                st.plugin_file_versions.remove(&clean_path(&t.path));
            }
            let snap = self.snapshot();
            let records: Vec<Arc<Record>> = snap.records.iter().filter(|r| r.id != id).cloned().collect();
            Self::remove_runtime_state(&mut st, &id);
            (targets, request, records, snap.enabled)
        };
        self.store_snapshot(Arc::new(Snapshot { enabled, records: records.clone(), quota_supported: Mutex::new(HashMap::new()) }));
        if let Some(req) = &request {
            let instances = req.lock().instances.clone();
            for i in &instances {
                self.bridges.close_http_callback_instance(&id, Some(i));
            }
        }
        if targets.is_empty() {
            self.bridges.close_http_plugin_resources(&id, None);
        }
        for t in &targets {
            self.bridges.close_http_plugin_resources(&t.id, Some(&t.client.instance()));
        }
        self.arc().refresh_thinking_providers(&records);
        self.arc().register_frontend_auth_providers();
        for t in &targets {
            t.client.shutdown(Some(Duration::from_secs(5)));
            tracing::info!(plugin_id = %t.id, plugin_name = %t.name(), version = %t.version(), path = %t.path.display(), "pluginhost: plugin unloaded");
        }
        for t in &targets {
            self.bridges.close_http_plugin_resources(&t.id, Some(&t.client.instance()));
        }
        true
    }

    /// Detaches every plugin and closes all libraries (Go: `ShutdownAllContext`).
    pub async fn shutdown_all(&self, ctx: &CallCtx) {
        let Some(_guard) = self.lock_apply(ctx).await else { return };
        let (targets, loading) = {
            let mut st = self.state.lock();
            let loading: Vec<(String, Arc<Mutex<LoadRequest>>)> = st.loading.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            for (_, r) in &loading {
                let mut r = r.lock();
                r.closed = true;
                for i in &r.instances {
                    i.closed.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let mut targets: Vec<Arc<LoadedPlugin>> = st.loaded.values().cloned().collect();
            for v in st.retired.values() {
                targets.extend(v.iter().cloned());
            }
            st.loaded.clear();
            st.retired.clear();
            st.model_client_ids.clear();
            st.executor_model_client_ids.clear();
            st.model_providers.clear();
            st.model_registrations.clear();
            st.provider_models.clear();
            st.executor_providers.clear();
            st.command_line_flags.clear();
            st.command_line_hits.clear();
            st.management_routes.clear();
            st.resource_routes.clear();
            st.plugin_file_versions.clear();
            st.active_versions.clear();
            st.active_paths.clear();
            (targets, loading)
        };
        self.store_snapshot(Snapshot::empty());
        for (id, r) in &loading {
            let instances = r.lock().instances.clone();
            for i in &instances {
                self.bridges.close_http_callback_instance(id, Some(i));
            }
        }
        for t in &targets {
            self.bridges.close_http_plugin_resources(&t.id, Some(&t.client.instance()));
        }
        self.bridges.cancel_all_http();
        self.arc().refresh_thinking_providers(&[]);
        self.arc().register_frontend_auth_providers();
        for t in &targets {
            t.client.shutdown(Some(Duration::from_secs(5)));
            tracing::info!(plugin_id = %t.id, plugin_name = %t.name(), version = %t.version(), path = %t.path.display(), "pluginhost: plugin unloaded");
        }
        self.bridges.cancel_all_http();
    }

    // ---- registered plugins ----

    /// Active plugins with their OAuth/quota/menu capabilities (Go: `RegisteredPlugins`).
    pub fn registered_plugins(&self) -> Vec<RegisteredPluginInfo> {
        let records = self.active_records();
        if records.is_empty() {
            return Vec::new();
        }
        let menus = self.registered_plugin_menus();
        records
            .iter()
            .map(|r| {
                let fused = self.is_plugin_fused(&r.id);
                RegisteredPluginInfo {
                    id: r.id.clone(),
                    priority: r.priority,
                    metadata: r.meta.clone(),
                    supports_oauth: r.caps().auth_provider,
                    oauth_provider: if r.caps().auth_provider && !fused { r.info.auth_identifier.clone() } else { String::new() },
                    supports_quota: r.caps().quota_provider,
                    quota_provider: if r.caps().quota_provider && !fused { r.info.quota_identifier.clone() } else { String::new() },
                    menus: menus.get(&r.id).cloned().unwrap_or_default(),
                }
            })
            .collect()
    }

    fn registered_plugin_menus(&self) -> HashMap<String, Vec<RegisteredPluginMenu>> {
        let st = self.state.lock();
        let mut out: HashMap<String, Vec<RegisteredPluginMenu>> = HashMap::new();
        for rec in st.resource_routes.values() {
            let menu = rec.route.menu.trim();
            if menu.is_empty() {
                continue;
            }
            out.entry(rec.plugin_id.clone()).or_default().push(RegisteredPluginMenu {
                path: rec.route.path.trim().to_string(),
                menu: menu.to_string(),
                description: rec.route.description.trim().to_string(),
            });
        }
        for v in out.values_mut() {
            v.sort_by(|a, b| a.path.cmp(&b.path));
        }
        out
    }
}

#[derive(Default)]
struct LoadOutcome {
    loaded: Option<Arc<LoadedPlugin>>,
    info: Option<PluginInfo>,
    err: Option<String>,
    completed: bool,
}

/// Performs the registration RPC and turns the response into [`PluginInfo`] (Go:
/// `registerRPCPlugin`).
pub(crate) async fn register_rpc(ctx: &CallCtx, client: &Arc<GuardedClient>, method: &str, config_yaml: &[u8]) -> Result<PluginInfo, PluginError> {
    let req = LifecycleRequest { config_yaml, schema_version: abi::SCHEMA_VERSION };
    let resp: Registration = call_plugin(client, ctx, method, &req).await?;
    if resp.schema_version > abi::SCHEMA_VERSION {
        return Err(PluginError::msg(format!("plugin schema version {} is not supported", resp.schema_version)));
    }
    let mut caps = resp.capabilities;
    caps.frontend_auth_provider_exclusive = caps.frontend_auth_provider && caps.frontend_auth_provider_exclusive;
    if !caps.scheduler {
        caps.scheduler_across_priorities = false;
    }
    let mut info = PluginInfo {
        metadata: resp.metadata,
        schema_version: if resp.schema_version == 0 { 1 } else { resp.schema_version },
        caps,
        ..Default::default()
    };
    if info.caps.auth_provider {
        if ctx.is_canceled() {
            return Err(PluginError::canceled());
        }
        info.auth_identifier = identifier_rpc(ctx, client, abi::METHOD_AUTH_IDENTIFIER).await.to_lowercase();
        if ctx.is_canceled() {
            return Err(PluginError::canceled());
        }
    }
    if info.caps.quota_provider {
        if ctx.is_canceled() {
            return Err(PluginError::canceled());
        }
        info.quota_identifier = identifier_rpc(ctx, client, abi::METHOD_QUOTA_IDENTIFIER).await.to_lowercase();
        if ctx.is_canceled() {
            return Err(PluginError::canceled());
        }
    }
    Ok(info)
}

async fn identifier_rpc(ctx: &CallCtx, client: &Arc<GuardedClient>, method: &str) -> String {
    call_plugin::<PluginIdentifier>(client, ctx, method, &empty_request()).await.map(|r| r.identifier.trim().to_string()).unwrap_or_default()
}
