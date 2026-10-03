//! Plugin store catalog and installation (Go: `plugin_store.go`, `plugin_store_release.go`) over
//! `cpa-pluginstore`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::http::{HeaderValue, Uri, header};
use axum::response::Response;
use chrono::{DateTime, Utc};
use cpa_auth::http::{ProxySetting, parse_proxy};
use cpa_config::PluginInstanceConfig;
use cpa_plugin::Host;
use cpa_pluginstore::auth::{AuthConfig, plugin_auth_configured};
use cpa_pluginstore::http::ReqwestDoer;
use cpa_pluginstore::version::update_available;
use cpa_pluginstore::{
    Client, Context, Error as StoreError, InstallOptions, InstallResult, Manifest, Plugin, Source, normalize_sources,
    plugin_install_type, plugin_platforms, release_version,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use crate::http::{ApiError, ApiResult, blocking, detached, json_response, ok_struct};
use crate::state::ManagementState;

const RELEASE_CACHE_TTL: Duration = Duration::from_secs(3600);
const RELEASE_FAILURE_CACHE_TTL: Duration = Duration::from_secs(30);
const RELEASE_CONCURRENCY: usize = 2;

/// Go: `html.EscapeString`, applied to every string the store endpoints return.
pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&#39;"),
            '"' => out.push_str("&#34;"),
            c => out.push(c),
        }
    }
    out
}

// ---- response shapes ----

#[derive(Serialize)]
struct SourceInfo {
    id: String,
    name: String,
    url: String,
}

#[derive(Serialize)]
struct SourceErrorInfo {
    source_id: String,
    source_name: String,
    source_url: String,
    message: String,
}

#[derive(Serialize)]
struct PlatformInfo {
    goos: String,
    goarch: String,
}

#[derive(Serialize)]
struct StoreEntry {
    store_id: String,
    source_id: String,
    source_name: String,
    source_url: String,
    id: String,
    name: String,
    description: String,
    author: String,
    version: String,
    repository: String,
    install_type: String,
    auth_required: bool,
    auth_configured: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    platforms: Vec<PlatformInfo>,
    #[serde(skip_serializing_if = "String::is_empty")]
    logo: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    homepage: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    license: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    installed: bool,
    installed_version: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    installed_source_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    install_source_status: String,
    path: String,
    configured: bool,
    registered: bool,
    enabled: bool,
    effective_enabled: bool,
    update_available: bool,
}

/// `pluginLocalStatus`.
#[derive(Default, Clone)]
struct LocalStatus {
    installed: bool,
    installed_version: String,
    store_managed: bool,
    installed_source_id: String,
    installed_source_url: String,
    path: String,
    configured: bool,
    registered: bool,
    enabled: bool,
    effective_enabled: bool,
}

struct SourcedPlugin {
    source: Source,
    plugin: Plugin,
}

struct SourceError {
    source: Source,
    message: String,
    error: StoreError,
}

/// Everything the store handlers read from the live config.
struct Snapshot {
    plugins_enabled: bool,
    plugins_dir: String,
    proxy_url: String,
    source_configs: Vec<String>,
    store_auth: Vec<AuthConfig>,
    configs: BTreeMap<String, PluginInstanceConfig>,
    host: Option<Arc<Host>>,
}

fn snapshot(st: &ManagementState) -> ApiResult<Snapshot> {
    let cfg = st.cfg();
    let dir = cfg.plugins.dir.trim();
    let dir = if dir.is_empty() { "plugins" } else { dir };
    let plugins_dir = cpa_config::resolve_plugins_dir(dir)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| ApiError::with_message(500, "plugin_directory_invalid", e.to_string()))?;
    Ok(Snapshot {
        plugins_enabled: cfg.plugins.enabled,
        plugins_dir,
        proxy_url: cfg.proxy_url.trim().to_string(),
        source_configs: cfg.plugins.store_sources.clone(),
        store_auth: cfg.plugins.store_auth.iter().map(AuthConfig::from).collect(),
        configs: cfg.plugins.configs.clone(),
        host: st.plugins.clone(),
    })
}

fn sources_of(snap: &Snapshot) -> ApiResult<Vec<Source>> {
    normalize_sources(&snap.source_configs).map_err(|e| ApiError::with_message(500, "plugin_store_source_invalid", e.to_string()))
}

/// `newPluginStoreClient`: the store client for one registry URL behind the configured proxy.
fn new_client(proxy_url: &str, registry_url: &str, auth: &[AuthConfig]) -> Client {
    let mut client = Client::default();
    client.network_scope = proxy_url.trim().to_string();
    client.registry_url = registry_url.trim().to_string();
    client.auth = auth.to_vec();
    let mut builder = ReqwestDoer::client_builder();
    match parse_proxy(proxy_url) {
        Ok(ProxySetting::Direct) => builder = builder.no_proxy(),
        Ok(ProxySetting::Proxy(p)) => {
            if let Ok(proxy) = reqwest::Proxy::all(p) {
                builder = builder.proxy(proxy);
            }
        }
        _ => {}
    }
    if !proxy_url.trim().is_empty()
        && let Ok(http) = builder.build()
    {
        client.http_client = Some(Arc::new(ReqwestDoer::new(http)));
    }
    client
}

async fn fetch_sourced_plugins(
    ctx: &Context,
    proxy_url: &str,
    auth: &[AuthConfig],
    sources: &[Source],
) -> (Vec<SourcedPlugin>, Vec<SourceError>) {
    let mut plugins = Vec::new();
    let mut errors = Vec::new();
    for source in sources {
        let client = new_client(proxy_url, &source.url, auth);
        match client.fetch_registry(ctx).await {
            Ok(registry) => {
                for plugin in registry.plugins {
                    plugins.push(SourcedPlugin { source: source.clone(), plugin });
                }
            }
            Err(error) => errors.push(SourceError { source: source.clone(), message: error.to_string(), error }),
        }
    }
    (plugins, errors)
}

// ---- latest release cache ----

struct CacheEntry {
    version: String,
    next_check_at: DateTime<Utc>,
}

struct ReleaseCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
    slots: Semaphore,
}

static RELEASES: LazyLock<ReleaseCache> =
    LazyLock::new(|| ReleaseCache { entries: Mutex::new(HashMap::new()), slots: Semaphore::new(RELEASE_CONCURRENCY) });

fn release_key(client: &Client, plugin: &Plugin) -> String {
    if plugin_install_type(plugin) != cpa_pluginstore::INSTALL_TYPE_GITHUB_RELEASE || plugin.repository.is_empty() {
        return String::new();
    }
    client.latest_release_cache_key(plugin).unwrap_or_default()
}

impl ReleaseCache {
    /// The last successful result, without network activity.
    fn cached(&self, client: &Client, plugin: &Plugin) -> String {
        let key = release_key(client, plugin);
        self.entries.lock().get(&key).map(|e| e.version.clone()).unwrap_or_default()
    }

    fn fresh(&self, key: &str) -> Option<String> {
        let entries = self.entries.lock();
        entries.get(key).filter(|e| Utc::now() < e.next_check_at).map(|e| e.version.clone())
    }

    /// `latestPluginVersion`.
    async fn latest(&self, ctx: &Context, client: &Client, plugin: &Plugin) -> String {
        if plugin_install_type(plugin) != cpa_pluginstore::INSTALL_TYPE_GITHUB_RELEASE {
            return String::new();
        }
        let Ok((client, key)) = client.prepare_latest_release(plugin) else { return String::new() };
        if let Some(v) = self.fresh(&key) {
            return v;
        }
        let Ok(_permit) = self.slots.acquire().await else { return String::new() };
        // Another listing may have refreshed the entry while this one waited for a slot.
        if let Some(v) = self.fresh(&key) {
            return v;
        }
        let previous = self.entries.lock().get(&key).map(|e| e.version.clone()).unwrap_or_default();
        let result = match client.fetch_latest_release(ctx, plugin).await {
            Ok(release) => release_version(&release),
            Err(e) => Err(e),
        };
        let (version, ttl) = match &result {
            Ok(v) => (v.clone(), RELEASE_CACHE_TTL),
            Err(e) => {
                tracing::warn!(plugin_id = %plugin.id, "pluginstore: failed to fetch latest release: {e}");
                (previous, RELEASE_FAILURE_CACHE_TTL)
            }
        };
        let mut next = Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_default();
        if let Err(e) = &result
            && let Some(rl) = e.rate_limit()
        {
            next = rl.retry_at;
        }
        self.entries.lock().insert(key, CacheEntry { version: version.clone(), next_check_at: next });
        version
    }
}

async fn latest_versions(ctx: &Context, client: &Client, plugins: &[Option<&Plugin>]) -> Vec<String> {
    let futures = plugins.iter().map(|p| async move {
        match p {
            Some(plugin) if !release_key(client, plugin).is_empty() => RELEASES.latest(ctx, client, plugin).await,
            _ => String::new(),
        }
    });
    futures_util::future::join_all(futures).await
}

// ---- local status ----

/// `pluginStoreDesiredVersion`: the version pinned by `plugins.configs.<id>.store`.
pub(crate) fn desired_version(item: &PluginInstanceConfig) -> String {
    let Some(store) = store_node(item) else { return String::new() };
    for key in ["version", "release-tag"] {
        let version = normalize_desired(&yaml_scalar(store.get(key)));
        if !version.is_empty() {
            return version;
        }
    }
    String::new()
}

fn store_node(item: &PluginInstanceConfig) -> Option<&serde_yaml_ng::Value> {
    match &item.raw {
        serde_yaml_ng::Value::Mapping(m) => m.get("store"),
        _ => None,
    }
}

fn yaml_scalar(v: Option<&serde_yaml_ng::Value>) -> String {
    match v {
        Some(serde_yaml_ng::Value::String(s)) => s.trim().to_string(),
        Some(serde_yaml_ng::Value::Number(n)) => n.to_string(),
        Some(serde_yaml_ng::Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn normalize_desired(version: &str) -> String {
    let mut v = version.trim();
    if v.len() > 1 && (v.starts_with('v') || v.starts_with('V')) {
        v = &v[1..];
    }
    if v.is_empty() || !v.as_bytes()[0].is_ascii_digit() { String::new() } else { v.to_string() }
}

/// `pluginStoreConfiguredSource`: `(source id, source url, store managed)`.
fn configured_source(item: &PluginInstanceConfig) -> (String, String, bool) {
    let Some(node) = store_node(item) else { return (String::new(), String::new(), false) };
    match Manifest::from_yaml(node) {
        Ok(m) => (m.source_id.trim().to_string(), m.source_url.trim().to_string(), true),
        Err(_) => (String::new(), String::new(), true),
    }
}

/// `pluginLocalStatuses`.
fn local_statuses(snap: &Snapshot) -> ApiResult<HashMap<String, LocalStatus>> {
    let mut statuses: HashMap<String, LocalStatus> = HashMap::new();
    let desired = crate::plugins_v0::desired_versions_of(&snap.configs);
    let (files, _) = cpa_plugin::platform::select_plugin_files(&snap.plugins_dir, &desired)
        .map_err(|e| ApiError::with_message(500, "plugin_discovery_failed", e.to_string()))?;
    for file in files {
        let status = statuses.entry(file.id).or_default();
        status.installed = true;
        status.path = file.path.to_string_lossy().into_owned();
        if !file.version.trim().is_empty() {
            status.installed_version = file.version.trim().to_string();
        }
        status.enabled = true;
    }
    for (id, item) in &snap.configs {
        let status = statuses.entry(id.clone()).or_default();
        status.configured = true;
        status.enabled = item.enabled.unwrap_or(false);
        let (sid, surl, managed) = configured_source(item);
        status.installed_source_id = sid;
        status.installed_source_url = surl;
        status.store_managed = managed;
    }
    if let Some(host) = &snap.host {
        for info in host.registered_plugins() {
            let status = statuses.entry(info.id.clone()).or_default();
            status.installed = true;
            status.registered = true;
            status.installed_version = info.metadata.version.trim().to_string();
        }
    }
    for status in statuses.values_mut() {
        status.effective_enabled = snap.plugins_enabled && status.enabled && status.registered;
    }
    Ok(statuses)
}

/// `pluginStoreResolveInstalledSource`.
fn resolve_installed_source(status: &LocalStatus, sources: &[Source]) -> Option<String> {
    let source_id = status.installed_source_id.trim();
    let source_url = status.installed_source_url.trim();
    if !source_id.is_empty() {
        for source in sources {
            if source.id.trim() != source_id {
                continue;
            }
            if !source_url.is_empty() && source.url.trim() != source_url {
                return None;
            }
            return Some(source_id.to_string());
        }
        return Some(source_id.to_string());
    }
    if source_url.is_empty() {
        return None;
    }
    sources.iter().find(|s| s.url.trim() == source_url).map(|s| s.id.trim().to_string())
}

/// `pluginStoreInstallSourceStatus`: `(installed source id, status, update allowed)`.
fn install_source_status(status: &LocalStatus, sources: &[Source], entry_source: &str, source_count: usize) -> (String, String, bool) {
    if !status.installed && !status.configured && !status.registered {
        return (String::new(), String::new(), true);
    }
    if let Some(source_id) = resolve_installed_source(status, sources) {
        if source_id == entry_source.trim() {
            return (source_id, "matched".into(), true);
        }
        return (source_id, "different".into(), false);
    }
    if status.store_managed || source_count > 1 {
        return (String::new(), "unknown".into(), false);
    }
    (String::new(), "assumed".into(), true)
}

// ---- listing ----

/// `GET /plugin-store` (v0) and `GET /v8/management/plugins/store`.
pub(crate) async fn list(st: &ManagementState) -> ApiResult {
    let snap = snapshot(st)?;
    let sources = sources_of(&snap)?;
    let ctx = Context::background();
    let (plugins, source_errors) = fetch_sourced_plugins(&ctx, &snap.proxy_url, &snap.store_auth, &sources).await;
    if plugins.is_empty()
        && let Some(first) = source_errors.first()
    {
        return Err(ApiError::with_message(502, "plugin_store_registry_failed", first.message.clone()));
    }
    let statuses = local_statuses(&snap)?;

    let mut source_counts: HashMap<&str, usize> = HashMap::new();
    for item in &plugins {
        *source_counts.entry(item.plugin.id.as_str()).or_default() += 1;
    }
    // Browsing the catalog must not spend API quota on uninstalled plugins or on sources that
    // cannot update the installed plugin; positional placeholders keep the versions aligned.
    let default_status = LocalStatus::default();
    let latest_input: Vec<Option<&Plugin>> = plugins
        .iter()
        .map(|item| {
            let status = statuses.get(&item.plugin.id).unwrap_or(&default_status);
            let count = source_counts.get(item.plugin.id.as_str()).copied().unwrap_or(0);
            let (_, _, allows) = install_source_status(status, &sources, &item.source.id, count);
            (status.installed && allows).then_some(&item.plugin)
        })
        .collect();
    let client = new_client(&snap.proxy_url, "", &snap.store_auth);
    let latest = latest_versions(&ctx, &client, &latest_input).await;

    let mut entries = Vec::with_capacity(plugins.len());
    for (index, item) in plugins.iter().enumerate() {
        let plugin = &item.plugin;
        let status = statuses.get(&plugin.id).unwrap_or(&default_status);
        let count = source_counts.get(plugin.id.as_str()).copied().unwrap_or(0);
        let (installed_source, source_status, allows_update) = install_source_status(status, &sources, &item.source.id, count);
        let installed_version = status.installed_version.clone();
        // Fall back to the registry version when the latest release is unknown.
        let mut store_version = plugin.version.clone();
        if !latest[index].is_empty() {
            store_version = latest[index].clone();
        } else {
            let cached = RELEASES.cached(&client, plugin);
            if !cached.is_empty() {
                store_version = cached;
            }
        }
        entries.push(StoreEntry {
            store_id: esc(&format!("{}/{}", item.source.id, plugin.id)),
            source_id: esc(&item.source.id),
            source_name: esc(&item.source.name),
            source_url: esc(&item.source.url),
            id: esc(&plugin.id),
            name: esc(&plugin.name),
            description: esc(&plugin.description),
            author: esc(&plugin.author),
            version: esc(&store_version),
            repository: esc(&plugin.repository),
            install_type: esc(&plugin_install_type(plugin)),
            auth_required: plugin.auth_required,
            auth_configured: plugin_auth_configured(&item.source, plugin, &snap.store_auth),
            platforms: plugin_platforms(plugin).iter().map(|p| PlatformInfo { goos: esc(&p.goos), goarch: esc(&p.goarch) }).collect(),
            logo: esc(&plugin.logo),
            homepage: esc(&plugin.homepage),
            license: esc(&plugin.license),
            tags: plugin.tags.iter().map(|t| esc(t)).collect(),
            installed: status.installed,
            installed_version: esc(&installed_version),
            installed_source_id: esc(&installed_source),
            install_source_status: esc(&source_status),
            path: esc(&status.path),
            configured: status.configured,
            registered: status.registered,
            enabled: status.enabled,
            effective_enabled: status.effective_enabled,
            update_available: allows_update && update_available(&installed_version, &store_version),
        });
    }

    let mut body = serde_json::Map::new();
    body.insert("plugins_enabled".into(), snap.plugins_enabled.into());
    body.insert("plugins_dir".into(), esc(&snap.plugins_dir).into());
    body.insert("sources".into(), serde_json::to_value(source_infos(&sources)).unwrap_or(Value::Null));
    if !source_errors.is_empty() {
        let errors: Vec<SourceErrorInfo> = source_errors
            .iter()
            .map(|e| SourceErrorInfo {
                source_id: esc(&e.source.id),
                source_name: esc(&e.source.name),
                source_url: esc(&e.source.url),
                message: esc(&e.message),
            })
            .collect();
        body.insert("source_errors".into(), serde_json::to_value(errors).unwrap_or(Value::Null));
    }
    body.insert("plugins".into(), serde_json::to_value(entries).unwrap_or(Value::Null));
    Ok(ok_struct(&Value::Object(body)))
}

fn source_infos(sources: &[Source]) -> Vec<SourceInfo> {
    sources.iter().map(|s| SourceInfo { id: esc(&s.id), name: esc(&s.name), url: esc(&s.url) }).collect()
}

// ---- install ----

#[derive(Serialize)]
struct InstallResponse {
    status: &'static str,
    source_id: String,
    source_name: String,
    source_url: String,
    id: String,
    version: String,
    install_type: String,
    path: String,
    plugins_enabled: bool,
    restart_required: bool,
}

fn normalize_requested_version(version: &str) -> String {
    let v = version.trim();
    if v.to_lowercase().starts_with('v') { v[1..].trim().to_string() } else { v.to_string() }
}

/// `pluginInstallRequestedVersion`.
fn requested_version(uri: &Uri, body: &[u8]) -> Result<String, String> {
    let from_query = crate::http::query_trim(uri, "version");
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(from_query);
    }
    #[derive(Deserialize, Default)]
    struct Req {
        #[serde(default)]
        version: Option<String>,
    }
    let req: Req = serde_json::from_slice(body).map_err(|e| format!("decode install request: {e}"))?;
    let from_body = req.version.unwrap_or_default().trim().to_string();
    if from_query.is_empty() {
        return Ok(from_body);
    }
    if from_body.is_empty() || normalize_requested_version(&from_body) == normalize_requested_version(&from_query) {
        return Ok(from_query);
    }
    Err(format!("version query {from_query:?} does not match request body version {from_body:?}"))
}

fn rate_limit_response(err: &StoreError) -> Option<Response> {
    let rl = err.rate_limit()?;
    let retry_after = rl.retry_after_seconds(Utc::now());
    let mut resp = json_response(
        429,
        &json!({
            "error": "plugin_store_rate_limited",
            "message": rl.to_string(),
            "retry_after": retry_after,
            "retry_at": rl.retry_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        }),
    );
    if let Ok(v) = HeaderValue::from_str(&retry_after.to_string()) {
        resp.headers_mut().insert(header::RETRY_AFTER, v);
    }
    Some(resp)
}

/// `findPluginStoreInstallTarget`: the source, plugin and client for an install, or the error
/// response to send.
async fn find_install_target(
    ctx: &Context,
    snap: &Snapshot,
    sources: &[Source],
    id: &str,
    requested_source: &str,
) -> Result<(Source, Plugin, Client), Box<Response>> {
    let fail = |e: ApiError| -> Box<Response> { Box::new(axum::response::IntoResponse::into_response(e)) };
    let rate_limited = |e: &StoreError| rate_limit_response(e).map(Box::new);
    let requested_source = requested_source.trim();
    if !requested_source.is_empty() {
        let Some(source) = sources.iter().find(|s| s.id == requested_source) else {
            return Err(fail(ApiError::with_message(404, "plugin_store_source_not_found", "plugin store source not found")));
        };
        let client = new_client(&snap.proxy_url, &source.url, &snap.store_auth);
        let registry = match client.fetch_registry(ctx).await {
            Ok(r) => r,
            Err(e) => {
                return Err(rate_limited(&e).unwrap_or_else(|| fail(ApiError::with_message(502, "plugin_store_registry_failed", e.to_string()))));
            }
        };
        return match registry.plugin_by_id(id) {
            Some(plugin) => Ok((source.clone(), plugin.clone(), client)),
            None => Err(fail(ApiError::with_message(404, "plugin_not_found", "plugin not found in registry source"))),
        };
    }
    let (plugins, errors) = fetch_sourced_plugins(ctx, &snap.proxy_url, &snap.store_auth, sources).await;
    let matches: Vec<&SourcedPlugin> = plugins.iter().filter(|p| p.plugin.id == id).collect();
    if matches.is_empty() {
        if plugins.is_empty()
            && let Some(first) = errors.first()
        {
            return Err(rate_limited(&first.error).unwrap_or_else(|| fail(ApiError::with_message(502, "plugin_store_registry_failed", first.message.clone()))));
        }
        return Err(fail(ApiError::with_message(404, "plugin_not_found", "plugin not found in registry")));
    }
    if matches.len() > 1 {
        let listed: Vec<Source> = matches.iter().map(|m| m.source.clone()).collect();
        return Err(fail(ApiError::from_body(
            409,
            json!({
                "error": "plugin_store_source_required",
                "message": "multiple plugin store sources contain this plugin id; specify source",
                "sources": source_infos(&listed),
            }),
        )));
    }
    let m = matches[0];
    let client = new_client(&snap.proxy_url, &m.source.url, &snap.store_auth);
    Ok((m.source.clone(), m.plugin.clone(), client))
}

/// `validatePluginStoreInstallSource`.
fn validate_install_source(
    configs: &BTreeMap<String, PluginInstanceConfig>,
    sources: &[Source],
    id: &str,
    requested_source: &str,
) -> Result<(), ApiError> {
    let Some(item) = configs.get(id) else { return Ok(()) };
    let (sid, surl, managed) = configured_source(item);
    if !managed {
        return Ok(());
    }
    let status = LocalStatus { store_managed: true, installed_source_id: sid, installed_source_url: surl, ..Default::default() };
    let requested = requested_source.trim();
    match resolve_installed_source(&status, sources) {
        None => Err(ApiError::from_body(
            409,
            json!({
                "error": "plugin_store_installed_source_unknown",
                "message": "installed plugin source cannot be verified; uninstall it before reinstalling from the store",
                "requested_source_id": requested,
            }),
        )),
        Some(resolved) if resolved != requested => Err(ApiError::from_body(
            409,
            json!({
                "error": "plugin_store_source_conflict",
                "message": "installed plugin belongs to a different store source; uninstall it before switching sources",
                "installed_source_id": resolved,
                "requested_source_id": requested,
            }),
        )),
        Some(_) => Ok(()),
    }
}

/// `pluginStoreDirectManifest`.
fn direct_manifest(source: &Source, plugin: &Plugin, requested: &str) -> Result<Manifest, StoreError> {
    let mut version = normalize_requested_version(requested);
    if version.is_empty() {
        version = normalize_requested_version(&plugin.version);
    }
    let mut plugin = plugin.clone();
    if normalize_requested_version(&plugin.version) == version {
        plugin.version = version;
        return Manifest::from_plugin(source, &plugin);
    }
    for candidate in plugin.versions.clone() {
        if normalize_requested_version(&candidate.version) != version {
            continue;
        }
        plugin.version = version;
        plugin.install = candidate.install;
        if plugin.install.kind.trim().is_empty() {
            plugin.install.kind = cpa_pluginstore::INSTALL_TYPE_DIRECT.to_string();
        }
        return Manifest::from_plugin(source, &plugin);
    }
    Err(StoreError::msg(format!("direct plugin version {version:?} not found")))
}

fn release_tag_candidates(version: &str) -> Vec<String> {
    let version = version.trim();
    if version.is_empty() {
        return Vec::new();
    }
    if version.to_lowercase().starts_with('v') {
        return vec![version.to_string(), version[1..].trim().to_string()];
    }
    vec![version.to_string(), format!("v{version}")]
}

/// `installPluginStoreGitHubRelease`.
async fn install_github_release(
    ctx: &Context,
    client: &Client,
    plugin: &Plugin,
    requested: &str,
    options: &InstallOptions,
) -> Result<InstallResult, StoreError> {
    let version = normalize_requested_version(requested);
    if version.is_empty() {
        return client.install(ctx, plugin, options).await;
    }
    let mut errors = Vec::new();
    for tag in release_tag_candidates(requested) {
        match client.install_version(ctx, plugin, &tag, &version, options).await {
            Ok(r) => return Ok(r),
            Err(e) => {
                if e.rate_limit().is_some() || ctx.is_canceled() {
                    return Err(e);
                }
                errors.push(e.wrap(tag));
            }
        }
    }
    Err(StoreError::join(errors).unwrap_or_else(|| StoreError::msg("no release tag candidates")).wrap("install release by tag"))
}

/// `pluginStoreManifestForInstall`.
fn manifest_for_install(source: &Source, plugin: &Plugin, result: &InstallResult) -> Result<Manifest, StoreError> {
    let mut install_type = result.install_type.trim().to_string();
    if install_type.is_empty() {
        install_type = plugin_install_type(plugin);
    }
    match install_type.as_str() {
        cpa_pluginstore::INSTALL_TYPE_DIRECT => {
            let mut plugin = plugin.clone();
            plugin.version = result.version.trim().to_string();
            plugin.install = cpa_pluginstore::normalize_install_plan(&plugin.install);
            Manifest::from_plugin(source, &plugin)
        }
        cpa_pluginstore::INSTALL_TYPE_GITHUB_RELEASE => {
            let tag = result.release_tag.trim().to_string();
            if tag.is_empty() {
                return Err(StoreError::msg("release tag is required"));
            }
            Manifest::from_release(source, plugin, &cpa_pluginstore::Release { tag_name: tag, ..Default::default() })
        }
        other => Err(StoreError::msg(format!("unsupported install type {other:?}"))),
    }
}

/// The manifest as the YAML subtree stored under `plugins.configs.<id>.store` (kebab-case keys).
fn manifest_yaml(manifest: &Manifest) -> serde_yaml_ng::Value {
    fn convert(v: &Value, top: bool) -> Option<serde_yaml_ng::Value> {
        match v {
            Value::Object(map) => {
                let mut out = serde_yaml_ng::Mapping::new();
                for (k, item) in map {
                    let key = if top { k.replace('_', "-") } else { k.clone() };
                    if let Some(value) = convert(item, false) {
                        out.insert(key.into(), value);
                    }
                }
                (!out.is_empty()).then_some(serde_yaml_ng::Value::Mapping(out))
            }
            Value::Array(items) => Some(serde_yaml_ng::Value::Sequence(items.iter().filter_map(|i| convert(i, false)).collect())),
            other => serde_yaml_ng::to_value(other).ok(),
        }
    }
    let value = serde_json::to_value(manifest).unwrap_or(Value::Null);
    convert(&value, true).unwrap_or(serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new()))
}

/// `POST /plugin-store/:id/install` (v0) and `POST /v8/management/plugins/store/:id/install`.
pub(crate) async fn install(st: &ManagementState, id: &str, uri: &Uri, body: &[u8], v8: bool) -> ApiResult {
    let id = id.trim();
    if !cpa_plugin::platform::valid_plugin_id(id) {
        return Err(ApiError::with_message(400, "invalid_plugin_id", "invalid plugin id"));
    }
    let requested = requested_version(uri, body).map_err(|m| ApiError::with_message(400, "invalid_request", m))?;
    let requested_source = crate::http::query_trim(uri, "source");
    let st = st.clone();
    let id = id.to_string();
    detached(async move {
        let ctx = Context::background();
        let snap = snapshot(&st)?;
        let sources = sources_of(&snap)?;
        let (source, plugin, client) = match find_install_target(&ctx, &snap, &sources, &id, &requested_source).await {
            Ok(t) => t,
            Err(resp) => return Ok(*resp),
        };
        validate_install_source(&snap.configs, &sources, &id, &source.id)?;
        let host = snap.host.clone();
        let busy_id = id.clone();
        let options = InstallOptions {
            plugins_dir: snap.plugins_dir.clone(),
            goos: cpa_pluginstore::install::runtime_goos(),
            goarch: cpa_pluginstore::install::runtime_goarch(),
            plugin_loaded: Some(Arc::new(move || host.as_ref().is_some_and(|h| h.plugin_busy(&busy_id)))),
            before_write: None,
        };
        let mut manifest = Manifest::default();
        let result = match plugin_install_type(&plugin).as_str() {
            cpa_pluginstore::INSTALL_TYPE_DIRECT => match direct_manifest(&source, &plugin, &requested) {
                Ok(m) => {
                    manifest = m;
                    client.install_manifest(&ctx, &manifest, &options).await
                }
                Err(e) => return Err(ApiError::with_message(502, "plugin_manifest_invalid", e.to_string())),
            },
            cpa_pluginstore::INSTALL_TYPE_GITHUB_RELEASE => install_github_release(&ctx, &client, &plugin, &requested, &options).await,
            _ => {
                return Err(ApiError::with_message(
                    502,
                    "plugin_manifest_invalid",
                    format!("unsupported install type {:?}", plugin.install.kind),
                ));
            }
        };
        let result = match result {
            Ok(r) => r,
            Err(e) => {
                if let Some(resp) = rate_limit_response(&e) {
                    return Ok(resp);
                }
                if e.is_loaded_plugin_locked() {
                    return Err(ApiError::from_body(
                        409,
                        json!({
                            "error": "plugin_update_requires_restart",
                            "message": "loaded plugin cannot be overwritten while the server is running",
                            "restart_required": true,
                        }),
                    ));
                }
                return Err(ApiError::with_message(502, "plugin_install_failed", e.to_string()));
            }
        };
        if manifest.id.is_empty() {
            manifest = manifest_for_install(&source, &plugin, &result).map_err(|e| {
                ApiError::from_body(
                    500,
                    json!({
                        "error": "plugin_manifest_failed",
                        "message": format!("plugin file installed at {} but creating store manifest failed: {e}", result.path),
                        "path": result.path,
                    }),
                )
            })?;
        }

        let guard = st.shared.config_lock.clone().lock_owned().await;
        let mut cfg = (*st.cfg()).clone();
        cfg.normalize_plugins_config();
        let item = cfg.plugins.configs.get(&id).cloned().unwrap_or_default();
        let mut mapping = crate::plugins_v0::instance_mapping(&item);
        mapping.insert("enabled".into(), true.into());
        mapping.insert("store".into(), manifest_yaml(&manifest));
        let updated = cpa_config::PluginInstanceConfig::from_yaml(serde_yaml_ng::Value::Mapping(mapping)).map_err(|e| {
            ApiError::from_body(
                500,
                json!({
                    "error": "config_update_failed",
                    "message": format!("plugin file installed at {} but enabling it in config failed: decode plugin config: {e}", result.path),
                    "path": result.path,
                }),
            )
        })?;
        cfg.plugins.configs.insert(id.clone(), updated);
        let path = st.config_path.clone();
        let saved = blocking(move || Ok(cpa_config::save_config_preserve_comments(&path, &mut cfg, v8))).await?;
        if let Err(e) = saved {
            return Err(ApiError::from_body(
                500,
                json!({
                    "error": "config_save_failed",
                    "message": format!("plugin file installed at {} but saving config failed: {e}", result.path),
                    "path": result.path,
                }),
            ));
        }
        drop(guard);
        st.reload_config().await;
        tracing::info!(
            plugin_id = %result.id,
            plugin_name = %plugin.name,
            source_id = %source.id,
            version = %result.version,
            install_type = %result.install_type,
            path = %result.path,
            overwritten = result.overwritten,
            "pluginstore: plugin installed"
        );
        Ok(ok_struct(&InstallResponse {
            status: "installed",
            source_id: esc(&source.id),
            source_name: esc(&source.name),
            source_url: esc(&source.url),
            id: esc(&result.id),
            version: esc(&result.version),
            install_type: esc(&result.install_type),
            path: esc(&result.path),
            plugins_enabled: snap.plugins_enabled,
            restart_required: false,
        }))
    })
    .await
}
