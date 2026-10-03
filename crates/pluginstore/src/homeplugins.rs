//! Plugin sync/delete for CLIProxyAPIHome mode (Go `internal/homeplugins/sync.go`).
//!
//! Reports are the JSON payloads sent back to Home. The plugin host is reached through
//! [`PluginRuntime`] / [`PluginLoadInspector`], and store clients through
//! [`ClientFactory`] so the embedder can supply proxy-aware transports.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use cpa_config::{Config, PluginInstanceConfig};

use crate::auth::{AuthConfig, ResolvedAuthConfig};
use crate::error::{Context, Error, Result};
use crate::errf;
use crate::home_sync::PluginSyncItem;
use crate::http::{HttpDoer, ReqwestDoer};
use crate::install::{InstallOptions, InstallResult, plugin_extension, runtime_goarch, runtime_goos};
use crate::manifest::Manifest;
use crate::registry::{Platform, normalize_goarch, normalize_goos};
use crate::sdk::{Client, new_client_with_auth, new_client_with_resolved_auth_expiry};
use crate::version::update_available;

/// Plugin host operations needed by sync and delete.
pub trait PluginRuntime: Send + Sync {
    fn plugin_busy(&self, id: &str) -> bool;
    fn unload_plugin(&self, id: &str) -> bool;

    /// Context-aware unload (Go's optional `UnloadPluginContext`). Return `None` when the
    /// host has no such variant; delete then falls back to [`PluginRuntime::unload_plugin`].
    fn unload_plugin_context(&self, _ctx: &Context, _id: &str) -> Option<bool> {
        None
    }
}

/// Answers whether a plugin ended up registered after a sync.
pub trait PluginLoadInspector {
    fn plugin_registered(&self, id: &str) -> bool;
}

/// Builds the store clients used by sync. The default honors `proxy-url` and
/// `plugins.store-auth`; embedders can override to share their own transports.
pub trait ClientFactory: Send + Sync {
    /// Client for locally configured (`store-auth`, environment-backed) credentials.
    fn plugin_store_client(&self, cfg: &Config) -> Client;
    /// Client for temporary credentials sent by Home.
    fn resolved_plugin_store_client(
        &self,
        cfg: &Config,
        auth: Vec<ResolvedAuthConfig>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Client;
}

/// [`ClientFactory`] using reqwest clients routed through the configured `proxy-url`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultClientFactory;

/// Transport for the proxy setting: empty inherits environment proxies, `direct`/`none`
/// bypasses them, anything else must be a valid proxy URL (invalid values are logged and
/// ignored like Go's `SetProxy`).
fn build_doer(proxy_url: &str) -> Arc<dyn HttpDoer> {
    let mut builder = ReqwestDoer::client_builder();
    let trimmed = proxy_url.trim();
    if trimmed.eq_ignore_ascii_case("direct") || trimmed.eq_ignore_ascii_case("none") {
        builder = builder.no_proxy();
    } else if !trimmed.is_empty() {
        match reqwest::Proxy::all(trimmed) {
            Ok(proxy) => builder = builder.proxy(proxy),
            Err(err) => tracing::error!("parse proxy URL failed: {err}"),
        }
    }
    let client = builder.build().unwrap_or_default();
    Arc::new(ReqwestDoer::new(client))
}

impl ClientFactory for DefaultClientFactory {
    fn plugin_store_client(&self, cfg: &Config) -> Client {
        let proxy_url = cfg.proxy_url.trim().to_string();
        let store_auth: Vec<AuthConfig> = cfg.plugins.store_auth.iter().map(AuthConfig::from).collect();
        new_client_with_auth(Some(build_doer(&proxy_url)), "", &store_auth).with_network_scope(&proxy_url)
    }

    fn resolved_plugin_store_client(
        &self,
        cfg: &Config,
        auth: Vec<ResolvedAuthConfig>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Client {
        let proxy_url = cfg.proxy_url.trim().to_string();
        new_client_with_resolved_auth_expiry(Some(build_doer(&proxy_url)), "", auth, expires_at)
            .with_network_scope(&proxy_url)
    }
}

/// Task report sent to Home after a plugin sync or delete.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncReport {
    pub schema_version: i64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub task_id: u64,
    pub task: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub node_id: String,
    pub status: String,
    pub phase: String,
    pub ok: bool,
    #[serde(with = "crate::gotime::required")]
    pub started_at: DateTime<Utc>,
    /// `None` is the Go zero time (task not finished).
    #[serde(default, with = "crate::gotime")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(with = "crate::gotime::required")]
    pub updated_at: DateTime<Utc>,
    pub platform: Platform,
    pub plugins: Vec<PluginInstallStatus>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginInstallStatus {
    pub id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub release_tag: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub repository: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub install_type: String,
    pub install_status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub load_status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(skip_serializing_if = "is_false")]
    pub skipped: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub overwritten: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

pub const PLUGIN_TASK_NAME: &str = "plugin-sync";
pub const PLUGIN_DELETE_TASK_NAME: &str = "plugin-delete";
pub const PLUGIN_TASK_STATUS_OK: &str = "success";
pub const PLUGIN_TASK_STATUS_ERROR: &str = "failed";
pub const PLUGIN_TASK_PHASE_INSTALL: &str = "install";
pub const PLUGIN_TASK_PHASE_LOAD: &str = "load";
pub const PLUGIN_TASK_PHASE_DELETE: &str = "delete";

pub const PLUGIN_INSTALL_STATUS_INSTALLED: &str = "installed";
pub const PLUGIN_INSTALL_STATUS_SKIPPED: &str = "skipped";
pub const PLUGIN_INSTALL_STATUS_FAILED: &str = "failed";
pub const PLUGIN_INSTALL_STATUS_DELETED: &str = "deleted";
pub const PLUGIN_INSTALL_STATUS_MISSING: &str = "missing";
pub const PLUGIN_LOAD_STATUS_LOADED: &str = "loaded";
pub const PLUGIN_LOAD_STATUS_FAILED: &str = "failed";

/// Platform used by plugin discovery on this host.
pub fn current_platform() -> Platform {
    Platform { goos: runtime_goos(), goarch: runtime_goarch() }
}

pub fn normalize_platform(platform: &Platform) -> Platform {
    Platform { goos: normalize_goos(&platform.goos), goarch: normalize_goarch(&platform.goarch) }
}

/// Syncs configured store plugins for this host's platform.
pub async fn sync(ctx: &Context, cfg: &Config, runtime: Option<Arc<dyn PluginRuntime>>) -> Result<()> {
    sync_platform_with_report(ctx, cfg, runtime, &current_platform()).await.1.map_or(Ok(()), Err)
}

pub async fn sync_platform(
    ctx: &Context,
    cfg: &Config,
    runtime: Option<Arc<dyn PluginRuntime>>,
    platform: &Platform,
) -> Result<()> {
    sync_platform_with_report(ctx, cfg, runtime, platform).await.1.map_or(Ok(()), Err)
}

pub async fn sync_with_report(
    ctx: &Context,
    cfg: &Config,
    runtime: Option<Arc<dyn PluginRuntime>>,
) -> (SyncReport, Option<Error>) {
    sync_platform_with_report(ctx, cfg, runtime, &current_platform()).await
}

/// Installs every enabled config entry that carries a `store` manifest. Returns the
/// report and the joined error (None on success).
pub async fn sync_platform_with_report(
    ctx: &Context,
    cfg: &Config,
    runtime: Option<Arc<dyn PluginRuntime>>,
    platform: &Platform,
) -> (SyncReport, Option<Error>) {
    sync_platform_with_report_using(&DefaultClientFactory, ctx, cfg, runtime, platform).await
}

/// [`sync_platform_with_report`] with an explicit client factory.
pub async fn sync_platform_with_report_using(
    factory: &dyn ClientFactory,
    ctx: &Context,
    cfg: &Config,
    runtime: Option<Arc<dyn PluginRuntime>>,
    platform: &Platform,
) -> (SyncReport, Option<Error>) {
    if !cfg.home.enabled || !cfg.plugins.enabled {
        return (new_sync_report(platform), None);
    }
    let platform = normalize_platform(platform);
    let mut report = new_sync_report(&platform);
    if platform.goos.is_empty() {
        let err = errf!("home plugins: goos is required");
        finish_report(&mut report, Some(&err));
        return (report, Some(err));
    }
    if platform.goarch.is_empty() {
        let err = errf!("home plugins: goarch is required");
        finish_report(&mut report, Some(&err));
        return (report, Some(err));
    }
    report.platform = platform.clone();
    let root = match cpa_config::resolve_plugins_dir(&cfg.plugins.dir) {
        Ok(root) => root,
        Err(err) => {
            let err = errf!("home plugins: {err}");
            finish_report(&mut report, Some(&err));
            return (report, Some(err));
        }
    };
    let client = factory.plugin_store_client(cfg);
    let mut sync_errors: Vec<Error> = Vec::new();
    // BTreeMap iteration is already sorted by id.
    for (id, item) in &cfg.plugins.configs {
        if !plugin_config_enabled(item) {
            continue;
        }
        let manifest = match store_manifest_from_plugin_config(id, item) {
            Ok(Some(manifest)) => manifest,
            Ok(None) => continue,
            Err(err) => {
                report.plugins.push(PluginInstallStatus {
                    id: id.trim().to_string(),
                    install_status: PLUGIN_INSTALL_STATUS_FAILED.to_string(),
                    error: err.to_string(),
                    ..Default::default()
                });
                sync_errors.push(err);
                continue;
            }
        };
        let mut status = plugin_status_from_manifest(&manifest);
        match install_manifest(ctx, &client, &manifest, &root, &platform, runtime.clone()).await {
            Err(err) => {
                status.install_status = PLUGIN_INSTALL_STATUS_FAILED.to_string();
                status.error = err.to_string();
                report.plugins.push(status);
                sync_errors.push(err);
            }
            Ok(result) => {
                apply_install_result(&mut status, &result);
                report.plugins.push(status);
            }
        }
    }
    let err_sync = Error::join(sync_errors);
    finish_report(&mut report, err_sync.as_ref());
    (report, err_sync)
}

fn apply_install_result(status: &mut PluginInstallStatus, result: &InstallResult) {
    status.path = result.path.trim().to_string();
    status.skipped = result.skipped;
    status.overwritten = result.overwritten;
    status.install_status = if result.skipped {
        PLUGIN_INSTALL_STATUS_SKIPPED.to_string()
    } else {
        PLUGIN_INSTALL_STATUS_INSTALLED.to_string()
    };
}

/// Installs the items Home sent (with their temporary credentials), clearing the items
/// as it goes. `installed_versions` seeds the report with unchanged plugins.
pub async fn sync_resolved_with_report(
    ctx: &Context,
    cfg: &Config,
    items: &mut [PluginSyncItem],
    expires_at: Option<DateTime<Utc>>,
    installed_versions: &HashMap<String, String>,
    runtime: Option<Arc<dyn PluginRuntime>>,
) -> (SyncReport, Option<Error>) {
    sync_resolved_with_report_using(&DefaultClientFactory, ctx, cfg, items, expires_at, installed_versions, runtime)
        .await
}

/// [`sync_resolved_with_report`] with an explicit client factory.
pub async fn sync_resolved_with_report_using(
    factory: &dyn ClientFactory,
    ctx: &Context,
    cfg: &Config,
    items: &mut [PluginSyncItem],
    expires_at: Option<DateTime<Utc>>,
    installed_versions: &HashMap<String, String>,
    runtime: Option<Arc<dyn PluginRuntime>>,
) -> (SyncReport, Option<Error>) {
    let outcome = sync_resolved_inner(factory, ctx, cfg, items, expires_at, installed_versions, runtime).await;
    for item in items.iter_mut() {
        item.clear();
    }
    outcome
}

async fn sync_resolved_inner(
    factory: &dyn ClientFactory,
    ctx: &Context,
    cfg: &Config,
    items: &mut [PluginSyncItem],
    expires_at: Option<DateTime<Utc>>,
    installed_versions: &HashMap<String, String>,
    runtime: Option<Arc<dyn PluginRuntime>>,
) -> (SyncReport, Option<Error>) {
    let platform = normalize_platform(&current_platform());
    let mut report = new_sync_report(&platform);
    if !cfg.home.enabled || !cfg.plugins.enabled {
        finish_report(&mut report, None);
        return (report, None);
    }
    let root = match cpa_config::resolve_plugins_dir(&cfg.plugins.dir) {
        Ok(root) => root,
        Err(err) => {
            let err = errf!("home plugins: {err}");
            finish_report(&mut report, Some(&err));
            return (report, Some(err));
        }
    };
    add_installed_version_statuses(&mut report, cfg, &root, installed_versions);
    let mut sync_errors: Vec<Error> = Vec::new();
    for item in items.iter_mut() {
        let expired = match expires_at {
            Some(at) => Utc::now() >= at,
            None => true,
        };
        if expired {
            sync_errors.push(errf!("home plugins: plugin sync response expired"));
            break;
        }
        let manifest = item.manifest.clone();
        let mut status = plugin_status_from_manifest(&manifest);
        let mut client = factory.resolved_plugin_store_client(cfg, item.auth.clone(), expires_at);
        let outcome = install_manifest(ctx, &client, &manifest, &root, &platform, runtime.clone()).await;
        client.clear_auth();
        item.clear();
        match outcome {
            Err(err) => {
                status.install_status = PLUGIN_INSTALL_STATUS_FAILED.to_string();
                status.error = err.to_string();
                upsert_plugin_install_status(&mut report, status);
                sync_errors.push(err);
            }
            Ok(result) => {
                apply_install_result(&mut status, &result);
                upsert_plugin_install_status(&mut report, status);
            }
        }
    }
    let err_sync = Error::join(sync_errors);
    finish_report(&mut report, err_sync.as_ref());
    (report, err_sync)
}

/// Reports plugins that Home did not ask to change as skipped, at their installed version.
fn add_installed_version_statuses(
    report: &mut SyncReport,
    cfg: &Config,
    root: &Path,
    installed_versions: &HashMap<String, String>,
) {
    if installed_versions.is_empty() {
        return;
    }
    for (id, item) in &cfg.plugins.configs {
        if !plugin_config_enabled(item) {
            continue;
        }
        let id = id.trim().to_string();
        let Some(version) = installed_versions.get(&id) else {
            continue;
        };
        let mut status = PluginInstallStatus {
            id: id.clone(),
            version: version.trim().to_string(),
            install_status: PLUGIN_INSTALL_STATUS_SKIPPED.to_string(),
            skipped: true,
            ..Default::default()
        };
        if let Ok(files) = plugin_file_infos(root, &id) {
            if let Some(file) = files.iter().find(|file| file.version.trim() == status.version) {
                status.path = file.path.trim().to_string();
            }
        }
        if let Ok(Some(manifest)) = store_manifest_from_plugin_config(&id, item) {
            if plugin_versions_equal(&status.version, &manifest.version) {
                status.release_tag = manifest.release_tag.trim().to_string();
                status.repository = manifest.repository.trim().to_string();
                status.install_type = manifest.install_type();
            }
        }
        report.plugins.push(status);
    }
}

fn plugin_versions_equal(left: &str, right: &str) -> bool {
    let (left, right) = (left.trim(), right.trim());
    if left.is_empty() || right.is_empty() {
        return false;
    }
    !update_available(left, right) && !update_available(right, left)
}

fn upsert_plugin_install_status(report: &mut SyncReport, status: PluginInstallStatus) {
    let id = status.id.trim().to_string();
    match report.plugins.iter_mut().find(|existing| existing.id.trim() == id) {
        Some(existing) => *existing = status,
        None => report.plugins.push(status),
    }
}

/// Versions on disk for each configured plugin id (highest preferred version wins).
pub fn installed_versions(cfg: &Config) -> Result<HashMap<String, String>> {
    let root = cpa_config::resolve_plugins_dir(&cfg.plugins.dir).map_err(|err| errf!("home plugins: {err}"))?;
    let mut versions = HashMap::with_capacity(cfg.plugins.configs.len());
    for id in cfg.plugins.configs.keys() {
        let files = plugin_file_infos(&root, id)
            .map_err(|err| errf!("home plugins: discover installed plugin {id}: {err}"))?;
        let Some(first) = files.first() else { continue };
        let version = first.version.trim();
        if !version.is_empty() {
            versions.insert(id.trim().to_string(), version.to_string());
        }
    }
    Ok(versions)
}

async fn install_manifest(
    ctx: &Context,
    client: &Client,
    manifest: &Manifest,
    root: &Path,
    platform: &Platform,
    runtime: Option<Arc<dyn PluginRuntime>>,
) -> Result<InstallResult> {
    let id = manifest.id.trim().to_string();
    if id.is_empty() {
        return Err(errf!("home plugins: manifest plugin id is empty"));
    }
    let busy_id = id.clone();
    let plugin_busy: Arc<dyn Fn() -> bool + Send + Sync> =
        Arc::new(move || runtime.as_ref().is_some_and(|runtime| runtime.plugin_busy(&busy_id)));
    let options = InstallOptions {
        plugins_dir: root.to_string_lossy().into_owned(),
        goos: platform.goos.clone(),
        goarch: platform.goarch.clone(),
        plugin_loaded: Some(plugin_busy),
        before_write: None,
    };
    client
        .install_manifest(ctx, manifest, &options)
        .await
        .map_err(|err| err.wrap(format!("home plugins: install {id}")))
}

/// Unloads (when busy) and removes every on-disk version of the plugin for this platform.
pub fn delete_with_report(
    ctx: &Context,
    cfg: &Config,
    runtime: Option<&dyn PluginRuntime>,
    task_id: u64,
    plugin_id: &str,
) -> SyncReport {
    let platform = current_platform();
    let mut report = new_sync_report(&platform);
    report.task_id = task_id;
    report.task = PLUGIN_DELETE_TASK_NAME.to_string();
    report.phase = PLUGIN_TASK_PHASE_DELETE.to_string();
    let plugin_id = plugin_id.trim().to_string();
    let mut status = PluginInstallStatus { id: plugin_id.clone(), ..Default::default() };
    fn fail(report: &mut SyncReport, mut status: PluginInstallStatus, err: Error) {
        status.install_status = PLUGIN_INSTALL_STATUS_FAILED.to_string();
        status.error = err.to_string();
        report.plugins.push(status);
        finish_report(report, Some(&err));
    }
    if let Some(err) = ctx.err() {
        fail(&mut report, status, err);
        return report;
    }
    let root = match cpa_config::resolve_plugins_dir(&cfg.plugins.dir) {
        Ok(root) => root,
        Err(err) => {
            fail(&mut report, status, errf!("home plugins: {err}"));
            return report;
        }
    };
    if let Some(err) = ctx.err() {
        fail(&mut report, status, err);
        return report;
    }
    let (path, deleted, err_delete) = delete_plugin_artifact(ctx, &root, &plugin_id, runtime);
    status.path = path.trim().to_string();
    match &err_delete {
        Some(err) => {
            status.install_status = PLUGIN_INSTALL_STATUS_FAILED.to_string();
            status.error = err.to_string();
        }
        None if deleted => status.install_status = PLUGIN_INSTALL_STATUS_DELETED.to_string(),
        None => status.install_status = PLUGIN_INSTALL_STATUS_MISSING.to_string(),
    }
    report.plugins.push(status);
    finish_report(&mut report, err_delete.as_ref());
    report
}

/// Returns the representative path, whether anything was removed, and any error.
fn delete_plugin_artifact(
    ctx: &Context,
    root: &Path,
    id: &str,
    runtime: Option<&dyn PluginRuntime>,
) -> (String, bool, Option<Error>) {
    if let Some(err) = ctx.err() {
        return (String::new(), false, Some(err));
    }
    let id = id.trim();
    if !valid_plugin_file_id(id) {
        return (String::new(), false, Some(errf!("invalid plugin id {id:?}")));
    }
    let paths = match plugin_file_paths(root, id) {
        Ok(paths) => paths,
        Err(err) => return (String::new(), false, Some(errf!("{err}"))),
    };
    if let Some(err) = ctx.err() {
        return (String::new(), false, Some(err));
    }
    let Some(first) = paths.first().cloned() else {
        return (String::new(), false, None);
    };
    if let Some(runtime) = runtime {
        if runtime.plugin_busy(id) {
            if let Some(err) = ctx.err() {
                return (first, false, Some(err));
            }
            let unloaded = match runtime.unload_plugin_context(ctx, id) {
                Some(unloaded) => unloaded,
                None => runtime.unload_plugin(id),
            };
            if !unloaded && runtime.plugin_busy(id) {
                return (first, false, Some(Error::LoadedPluginLocked));
            }
        }
    }
    let mut deleted = false;
    for path in &paths {
        if let Some(err) = ctx.err() {
            return (first, deleted, Some(err));
        }
        if let Err(err) = std::fs::remove_file(path) {
            if err.kind() == std::io::ErrorKind::NotFound {
                continue;
            }
            return (first, deleted, Some(errf!("remove {path}: {err}")));
        }
        deleted = true;
        if let Some(err) = ctx.err() {
            return (first, deleted, Some(err));
        }
    }
    (first, deleted, None)
}

fn plugin_file_paths(root: &Path, id: &str) -> std::io::Result<Vec<String>> {
    Ok(plugin_file_infos(root, id)?.into_iter().map(|file| file.path).collect())
}

#[derive(Debug, Clone)]
struct PluginFileInfo {
    id: String,
    path: String,
    version: String,
}

/// Installed library files for the id under `root/<goos>/<goarch>` then `root`, best
/// (highest) version first, then the rest in discovery order.
fn plugin_file_infos(root: &Path, id: &str) -> std::io::Result<Vec<PluginFileInfo>> {
    let root = if root.as_os_str().is_empty() { PathBuf::from("plugins") } else { root.to_path_buf() };
    let id = id.trim();
    let goos = runtime_goos();
    let extension = plugin_extension(&goos);
    let mut candidates: Vec<PluginFileInfo> = Vec::new();
    for dir in plugin_candidate_dirs(&root, &goos, &runtime_goarch()) {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        let mut files: Vec<String> = Vec::new();
        for entry in entries {
            let entry = entry?;
            // Like Go's `entry.Type().IsRegular()`: symlinks are not followed.
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.to_lowercase().ends_with(extension) {
                files.push(cpa_config::clean_path(&dir.join(&name).to_string_lossy()));
            }
        }
        files.sort();
        for file_path in files {
            let Some(file) = plugin_file_from_path(&file_path, extension) else {
                continue;
            };
            if file.id != id {
                continue;
            }
            candidates.push(file);
        }
    }
    if candidates.len() <= 1 {
        return Ok(candidates);
    }
    let mut best = 0;
    for index in 1..candidates.len() {
        if plugin_file_preferred(&candidates[index], &candidates[best]) {
            best = index;
        }
    }
    if best == 0 {
        return Ok(candidates);
    }
    let best_file = candidates.remove(best);
    let mut out = Vec::with_capacity(candidates.len() + 1);
    out.push(best_file);
    out.append(&mut candidates);
    Ok(out)
}

fn plugin_candidate_dirs(root: &Path, goos: &str, goarch: &str) -> Vec<PathBuf> {
    vec![root.join(goos).join(goarch), root.to_path_buf()]
}

/// Splits `<id>[-v<version>]<ext>` into id and version.
fn plugin_file_from_path(file_path: &str, required_extension: &str) -> Option<PluginFileInfo> {
    let base = file_path.rsplit(['/', std::path::MAIN_SEPARATOR]).next().unwrap_or(file_path);
    let lower_base = base.to_lowercase();
    let extension = required_extension.trim();
    let extension = if !extension.is_empty() {
        if !lower_base.ends_with(&extension.to_lowercase()) {
            return None;
        }
        extension.to_string()
    } else {
        [".so", ".dylib", ".dll"].iter().find(|ext| lower_base.ends_with(**ext))?.to_string()
    };
    let name = &base[..base.len().checked_sub(extension.len())?];
    let mut id = name.to_string();
    let mut version = String::new();
    if let Some(version_index) = name.rfind("-v") {
        if version_index > 0 {
            let candidate_id = &name[..version_index];
            let candidate_version = &name[version_index + 2..];
            if valid_plugin_file_id(candidate_id) && valid_plugin_file_version(candidate_version) {
                id = candidate_id.to_string();
                version = candidate_version.to_string();
            }
        }
    }
    if !valid_plugin_file_id(&id) {
        return None;
    }
    Some(PluginFileInfo { id, path: file_path.to_string(), version })
}

fn plugin_file_preferred(candidate: &PluginFileInfo, current: &PluginFileInfo) -> bool {
    if current.path.trim().is_empty() {
        return true;
    }
    if candidate.version.is_empty() {
        return false;
    }
    if current.version.is_empty() {
        return true;
    }
    update_available(&current.version, &candidate.version)
}

fn valid_plugin_file_id(id: &str) -> bool {
    let id = id.trim();
    if id.is_empty() || id == "." || id == ".." || id.contains(['/', '\\']) {
        return false;
    }
    id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn valid_plugin_file_version(version: &str) -> bool {
    let version = version.trim();
    if version.is_empty() || version.starts_with('v') {
        return false;
    }
    version.as_bytes().first().is_some_and(u8::is_ascii_digit)
}

/// Fills in load results after the host tried to load synced plugins. Install failures
/// and a global sync failure stay fatal and are preserved in the joined error.
pub fn mark_load_results(report: &mut SyncReport, inspector: Option<&dyn PluginLoadInspector>) -> Option<Error> {
    report.phase = PLUGIN_TASK_PHASE_LOAD.to_string();
    let mut load_errors: Vec<Error> = Vec::new();
    let preserve_sync_error = !report.ok && !report.error.trim().is_empty();
    if preserve_sync_error {
        load_errors.push(Error::msg(report.error.clone()));
    }
    for status in &mut report.plugins {
        if status.install_status == PLUGIN_INSTALL_STATUS_FAILED {
            if status.load_status.is_empty() {
                status.load_status = PLUGIN_INSTALL_STATUS_SKIPPED.to_string();
            }
            if !preserve_sync_error {
                if !status.error.trim().is_empty() {
                    load_errors.push(Error::msg(status.error.clone()));
                } else {
                    load_errors.push(errf!("home plugins: plugin {} install failed", status.id));
                }
            }
            continue;
        }
        if inspector.is_some_and(|inspector| inspector.plugin_registered(&status.id)) {
            status.load_status = PLUGIN_LOAD_STATUS_LOADED.to_string();
            continue;
        }
        status.load_status = PLUGIN_LOAD_STATUS_FAILED.to_string();
        let err_load = errf!("home plugins: plugin {} installed but not loaded", status.id);
        if status.error.trim().is_empty() {
            status.error = err_load.to_string();
        }
        load_errors.push(err_load);
    }
    let err_load = Error::join(load_errors);
    finish_report(report, err_load.as_ref());
    err_load
}

fn new_sync_report(platform: &Platform) -> SyncReport {
    let now = Utc::now();
    SyncReport {
        schema_version: 1,
        task_id: 0,
        task: PLUGIN_TASK_NAME.to_string(),
        node_id: String::new(),
        status: PLUGIN_TASK_STATUS_OK.to_string(),
        phase: PLUGIN_TASK_PHASE_INSTALL.to_string(),
        ok: true,
        started_at: now,
        finished_at: None,
        updated_at: now,
        platform: normalize_platform(platform),
        plugins: Vec::new(),
        error: String::new(),
    }
}

/// Completed report for outcomes before plugin installation starts.
pub fn completed_sync_report(platform: &Platform, err_sync: Option<&Error>) -> SyncReport {
    let mut report = new_sync_report(platform);
    finish_report(&mut report, err_sync);
    report
}

fn finish_report(report: &mut SyncReport, err_task: Option<&Error>) {
    let now = Utc::now();
    report.finished_at = Some(now);
    report.updated_at = now;
    report.ok = err_task.is_none();
    match err_task {
        Some(err) => {
            report.status = PLUGIN_TASK_STATUS_ERROR.to_string();
            report.error = err.to_string();
        }
        None => {
            report.status = PLUGIN_TASK_STATUS_OK.to_string();
            report.error.clear();
        }
    }
}

fn plugin_status_from_manifest(manifest: &Manifest) -> PluginInstallStatus {
    PluginInstallStatus {
        id: manifest.id.trim().to_string(),
        version: manifest.version.trim().to_string(),
        release_tag: manifest.release_tag.trim().to_string(),
        repository: manifest.repository.trim().to_string(),
        install_type: manifest.install_type(),
        install_status: PLUGIN_INSTALL_STATUS_FAILED.to_string(),
        ..Default::default()
    }
}

/// Reads and validates `store:` from a plugin config entry. `Ok(None)` when absent.
fn store_manifest_from_plugin_config(id: &str, item: &PluginInstanceConfig) -> Result<Option<Manifest>> {
    let Some(store) = yaml_mapping_value(&item.raw, "store") else {
        return Ok(None);
    };
    if store.is_null() {
        return Ok(None);
    }
    let mut manifest = Manifest::from_yaml(store)
        .map_err(|err| errf!("home plugins: decode store manifest for {id}: {err}"))?;
    if manifest.id.trim().is_empty() {
        manifest.id = id.trim().to_string();
    }
    manifest
        .validate()
        .map_err(|err| errf!("home plugins: invalid store manifest for {id}: {err}"))?;
    Ok(Some(manifest))
}

fn yaml_mapping_value<'a>(node: &'a serde_yaml_ng::Value, key: &str) -> Option<&'a serde_yaml_ng::Value> {
    match node {
        serde_yaml_ng::Value::Mapping(map) => map.get(key),
        _ => None,
    }
}

fn plugin_config_enabled(item: &PluginInstanceConfig) -> bool {
    item.enabled == Some(true)
}
