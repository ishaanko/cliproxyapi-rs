//! v0 plugin and quota endpoints (Go: `plugins.go`, `plugin_store.go`, `plugin_quota.go`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::Uri;
use bytes::Bytes;
use cpa_auth::Auth;
use cpa_plugin::{CallCtx, Host};
use cpa_pluginapi::api::{ConfigField, PluginMetadata, QuotaFetchRequest, QuotaResetRequest};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::credentials::auth_by_index;
use crate::http::{ApiError, ApiResult, blocking, detached, ok_json, ok_struct, query_trim};
use crate::plugin_store::esc;
use crate::state::ManagementState;
use crate::v0_util::{first_json, persist};

/// Go: `pluginhost.ValidatePluginID`: `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`.
fn valid_plugin_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

fn plugin_id(raw: &str) -> ApiResult<String> {
    let id = raw.trim();
    if valid_plugin_id(id) {
        Ok(id.to_string())
    } else {
        Err(ApiError::with_message(
            400,
            "invalid_plugin_id",
            "invalid plugin id",
        ))
    }
}

fn plugin_not_found() -> ApiError {
    ApiError::with_message(404, "plugin_not_found", "plugin not found")
}

/// The JSON object of `plugins.configs.<id>`: the raw YAML subtree, or the host fields.
fn instance_json(item: &cpa_config::PluginInstanceConfig) -> Value {
    let raw = serde_json::to_value(&item.raw).unwrap_or(Value::Null);
    match raw {
        Value::Object(_) => raw,
        _ => {
            let mut m = Map::new();
            if let Some(e) = item.enabled {
                m.insert("enabled".into(), e.into());
            }
            if item.priority != 0 {
                m.insert("priority".into(), item.priority.into());
            }
            Value::Object(m)
        }
    }
}

/// `yamlNodeFromJSONValue`: integers stay integers, other numbers become floats.
fn json_to_yaml(v: &Value) -> serde_yaml_ng::Value {
    use serde_yaml_ng::Value as Y;
    match v {
        Value::Null => Y::Null,
        Value::Bool(b) => Y::Bool(*b),
        Value::String(s) => Y::String(s.clone()),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => Y::Number(i.into()),
            (_, Some(u), _) => Y::Number(u.into()),
            (_, _, Some(f)) => Y::Number(f.into()),
            _ => Y::Null,
        },
        Value::Array(items) => Y::Sequence(items.iter().map(json_to_yaml).collect()),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Y::Mapping(keys.into_iter().map(|k| (Y::String(k.clone()), json_to_yaml(&map[k]))).collect())
        }
    }
}

pub(crate) fn instance_mapping(item: &cpa_config::PluginInstanceConfig) -> serde_yaml_ng::Mapping {
    match &item.raw {
        serde_yaml_ng::Value::Mapping(m) => m.clone(),
        _ => {
            let mut m = serde_yaml_ng::Mapping::new();
            if let Some(e) = item.enabled {
                m.insert("enabled".into(), e.into());
            }
            if item.priority != 0 {
                m.insert("priority".into(), item.priority.into());
            }
            m
        }
    }
}

fn instance_from_mapping(m: serde_yaml_ng::Mapping) -> ApiResult<cpa_config::PluginInstanceConfig> {
    cpa_config::PluginInstanceConfig::from_yaml(serde_yaml_ng::Value::Mapping(m))
        .map_err(|e| ApiError::with_message(400, "invalid_config", e))
}

/// Go: `readPluginConfigObject`.
fn config_object(body: &[u8]) -> ApiResult<Map<String, Value>> {
    match first_json(body) {
        Some(Value::Object(o)) => Ok(o),
        Some(Value::Null) => Err(ApiError::with_message(
            400,
            "invalid_body",
            "body must be a JSON object",
        )),
        Some(_) => Err(ApiError::with_message(
            400,
            "invalid_body",
            "json: cannot unmarshal into Go value of type map[string]interface {}",
        )),
        None => Err(ApiError::with_message(400, "invalid_body", "EOF")),
    }
}

/// `GET /plugins/:id/config`.
pub(crate) async fn get_config(
    State(st): State<ManagementState>,
    Path(id): Path<String>,
) -> ApiResult {
    let id = plugin_id(&id)?;
    let cfg = st.cfg();
    if let Some(item) = cfg.plugins.configs.get(&id) {
        return Ok(ok_json(&cpa_auth::util::sort_json(&instance_json(item))));
    }
    if st.plugins.as_ref().is_some_and(|h| plugin_registered(h, &id)) {
        return Ok(ok_json(&json!({})));
    }
    let root = resolved_plugins_dir(&cfg.plugins.dir)?;
    let files = discover(&root, &HashMap::new())?;
    if files.iter().any(|f| f.id == id) {
        return Ok(ok_json(&json!({})));
    }
    Err(plugin_not_found())
}

/// `PATCH /plugins/:id/enabled`.
pub(crate) async fn patch_enabled(
    State(st): State<ManagementState>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult {
    let id = plugin_id(&id)?;
    let invalid = || ApiError::with_message(400, "invalid_body", "enabled is required");
    let enabled = match first_json(&body) {
        Some(Value::Object(o)) => crate::v0_util::field(&o, "enabled")
            .and_then(Value::as_bool)
            .ok_or_else(invalid)?,
        _ => return Err(invalid()),
    };
    persist(&st, move |c| {
        c.normalize_plugins_config();
        let item = c.plugins.configs.get(&id).cloned().unwrap_or_default();
        let mut m = instance_mapping(&item);
        m.insert("enabled".into(), enabled.into());
        c.plugins.configs.insert(id, instance_from_mapping(m)?);
        Ok(())
    })
    .await
}

/// `PUT /plugins/:id/config`: replaces the instance with the request object.
pub(crate) async fn put_config(
    State(st): State<ManagementState>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult {
    let id = plugin_id(&id)?;
    let obj = config_object(&body)?;
    let mut keys: Vec<&String> = obj.keys().collect();
    keys.sort();
    let mut m = serde_yaml_ng::Mapping::new();
    for k in keys {
        m.insert(k.as_str().into(), json_to_yaml(&obj[k]));
    }
    let updated = instance_from_mapping(m)?;
    persist(&st, move |c| {
        c.normalize_plugins_config();
        c.plugins.configs.insert(id, updated);
        Ok(())
    })
    .await
}

/// `PATCH /plugins/:id/config`: shallow merge, `null` deletes a key.
pub(crate) async fn patch_config(
    State(st): State<ManagementState>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult {
    let id = plugin_id(&id)?;
    let obj = config_object(&body)?;
    persist(&st, move |c| {
        c.normalize_plugins_config();
        let item = c.plugins.configs.get(&id).cloned().unwrap_or_default();
        let mut m = instance_mapping(&item);
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        for k in keys {
            if obj[k].is_null() {
                m.shift_remove(k.as_str());
            } else {
                m.insert(k.as_str().into(), json_to_yaml(&obj[k]));
            }
        }
        c.plugins.configs.insert(id, instance_from_mapping(m)?);
        Ok(())
    })
    .await
}

/// `DELETE /plugins/:id` (v0).
pub(crate) async fn delete(State(st): State<ManagementState>, Path(id): Path<String>) -> ApiResult {
    delete_plugin(st, id, false).await
}

/// `DELETE /v8/management/plugins/:id`.
pub(crate) async fn delete_v8(State(st): State<ManagementState>, Path(id): Path<String>) -> ApiResult {
    delete_plugin(st, id, true).await
}

/// `DeletePlugin`: unloads the plugin if possible, removes its file and its saved config.
async fn delete_plugin(st: ManagementState, id: String, v8: bool) -> ApiResult {
    let id = plugin_id(&id)?;
    detached(async move {
        let _guard = st.shared.config_lock.clone().lock_owned().await;
        let cfg = st.cfg();
        let item = cfg.plugins.configs.get(&id).cloned();
        let configured = item.is_some();
        let root = resolved_plugins_dir(&cfg.plugins.dir)?;
        let desired = item.as_ref().map(|i| desired_versions_of(&std::collections::BTreeMap::from([(id.clone(), i.clone())]))).unwrap_or_default();
        let path = discover(&root, &desired)?.into_iter().find(|f| f.id == id).map(|f| f.path.to_string_lossy().into_owned()).unwrap_or_default();
        if path.is_empty() && !configured {
            return Err(plugin_not_found());
        }
        if let Some(host) = &st.plugins
            && host.plugin_busy(&id)
            && !host.unload_plugin(&CallCtx::background(), &id).await
            && host.plugin_busy(&id)
        {
            return Err(ApiError::from_body(
                409,
                json!({
                    "error": "plugin_delete_requires_restart",
                    "message": "loaded plugin cannot be deleted while the server is running",
                    "restart_required": true,
                }),
            ));
        }
        let mut file_deleted = false;
        if !path.is_empty() {
            let target = path.clone();
            match blocking(move || Ok(std::fs::remove_file(&target))).await? {
                Ok(()) => file_deleted = true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(ApiError::with_message(500, "plugin_delete_failed", e.to_string())),
            }
        }
        let mut next = (*cfg).clone();
        next.plugins.configs.remove(&id);
        if configured {
            let cfg_path = st.config_path.clone();
            let saved = blocking(move || Ok(cpa_config::save_config_preserve_comments(&cfg_path, &mut next, v8))).await?;
            if let Err(e) = saved {
                return Err(ApiError::from_body(
                    500,
                    json!({
                        "error": "config_save_failed",
                        "message": format!("plugin deleted but saving config failed: {e}"),
                        "file_deleted": file_deleted,
                        "path": path,
                    }),
                ));
            }
        }
        st.reload_config().await;
        Ok(ok_json(&json!({
            "status": "deleted",
            "id": esc(&id),
            "path": esc(&path),
            "file_deleted": file_deleted,
            "configured_removed": configured,
            "restart_required": false,
        })))
    })
    .await
}

// ---- listing ----

#[derive(Serialize)]
struct ConfigFieldInfo {
    name: String,
    r#type: String,
    enum_values: Vec<String>,
    description: String,
}

#[derive(Serialize)]
struct MenuInfo {
    path: String,
    menu: String,
    description: String,
}

#[derive(Serialize)]
struct MetadataInfo {
    name: String,
    version: String,
    author: String,
    github_repository: String,
    logo: String,
    config_fields: Vec<ConfigFieldInfo>,
}

#[derive(Serialize)]
struct ListEntry {
    id: String,
    path: String,
    configured: bool,
    registered: bool,
    enabled: bool,
    effective_enabled: bool,
    supports_oauth: bool,
    oauth_provider: String,
    supports_quota: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    quota_provider: String,
    logo: String,
    config_fields: Vec<ConfigFieldInfo>,
    menus: Vec<MenuInfo>,
    metadata: Option<MetadataInfo>,
}

impl ListEntry {
    fn new(id: &str) -> Self {
        ListEntry {
            id: esc(id),
            path: String::new(),
            configured: false,
            registered: false,
            enabled: false,
            effective_enabled: false,
            supports_oauth: false,
            oauth_provider: String::new(),
            supports_quota: false,
            quota_provider: String::new(),
            logo: String::new(),
            config_fields: Vec::new(),
            menus: Vec::new(),
            metadata: None,
        }
    }
}

fn config_fields(fields: &[ConfigField]) -> Vec<ConfigFieldInfo> {
    fields
        .iter()
        .map(|f| ConfigFieldInfo {
            name: esc(&f.name),
            r#type: esc(&f.kind),
            enum_values: f.enum_values.iter().map(|v| esc(v)).collect(),
            description: esc(&f.description),
        })
        .collect()
}

fn metadata_info(m: &PluginMetadata) -> MetadataInfo {
    MetadataInfo {
        name: esc(&m.name),
        version: esc(&m.version),
        author: esc(&m.author),
        github_repository: esc(&m.git_hub_repository),
        logo: esc(&m.logo),
        config_fields: config_fields(&m.config_fields),
    }
}

/// `GET /plugins`: discovered, configured and registered plugins.
pub(crate) async fn list_plugins(State(st): State<ManagementState>) -> ApiResult {
    let cfg = st.cfg();
    let enabled = cfg.plugins.enabled;
    let root = resolved_plugins_dir(&cfg.plugins.dir)?;
    let files = discover(&root, &desired_versions_of(&cfg.plugins.configs))?;
    let mut entries: std::collections::BTreeMap<String, ListEntry> = Default::default();
    for file in files {
        let mut entry = ListEntry::new(&file.id);
        entry.path = esc(&file.path.to_string_lossy());
        entries.insert(file.id, entry);
    }
    for (id, item) in &cfg.plugins.configs {
        let entry = entries.entry(id.clone()).or_insert_with(|| ListEntry::new(id));
        entry.configured = true;
        entry.enabled = item.enabled.unwrap_or(false);
    }
    if let Some(host) = &st.plugins {
        for info in host.registered_plugins() {
            let entry = entries.entry(info.id.clone()).or_insert_with(|| ListEntry::new(&info.id));
            entry.registered = true;
            entry.supports_oauth = info.supports_oauth;
            entry.oauth_provider = esc(&info.oauth_provider);
            entry.supports_quota = info.supports_quota;
            entry.quota_provider = esc(&info.quota_provider);
            entry.logo = esc(&info.metadata.logo);
            entry.config_fields = config_fields(&info.metadata.config_fields);
            entry.menus = info
                .menus
                .iter()
                .map(|m| MenuInfo { path: esc(&m.path), menu: esc(&m.menu), description: esc(&m.description) })
                .collect();
            entry.metadata = Some(metadata_info(&info.metadata));
        }
    }
    let plugins: Vec<ListEntry> = entries
        .into_values()
        .map(|mut e| {
            e.effective_enabled = enabled && e.enabled && e.registered;
            e
        })
        .collect();
    Ok(ok_struct(&json!({
        "plugins_enabled": enabled,
        "plugins_dir": esc(&root),
        "plugins": plugins,
    })))
}

fn plugin_registered(host: &Host, id: &str) -> bool {
    host.registered_plugins().iter().any(|p| p.id == id)
}

/// `normalizedPluginsDir` + `config.ResolvePluginsDir`.
fn resolved_plugins_dir(dir: &str) -> ApiResult<String> {
    let dir = dir.trim();
    let dir = if dir.is_empty() { "plugins" } else { dir };
    cpa_config::resolve_plugins_dir(dir)
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| ApiError::with_message(500, "plugin_directory_invalid", e.to_string()))
}

/// `pluginhost.DiscoverPluginFiles`.
fn discover(root: &str, desired: &HashMap<String, String>) -> ApiResult<Vec<cpa_plugin::platform::PluginFile>> {
    cpa_plugin::platform::select_plugin_files(root, desired)
        .map(|(files, _)| files)
        .map_err(|e| ApiError::with_message(500, "plugin_discovery_failed", e.to_string()))
}

/// `pluginStoreDesiredVersions`: versions pinned by `plugins.configs.<id>.store`.
pub(crate) fn desired_versions_of(configs: &std::collections::BTreeMap<String, cpa_config::PluginInstanceConfig>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (id, item) in configs {
        let id = id.trim();
        let version = crate::plugin_store::desired_version(item);
        if !id.is_empty() && !version.is_empty() {
            out.insert(id.to_string(), version);
        }
    }
    out
}

/// `GET /plugin-store`.
pub(crate) async fn store(State(st): State<ManagementState>) -> ApiResult {
    crate::plugin_store::list(&st).await
}

/// `POST /plugin-store/:id/install`.
pub(crate) async fn install(
    State(st): State<ManagementState>,
    Path(id): Path<String>,
    uri: Uri,
    body: Bytes,
) -> ApiResult {
    crate::plugin_store::install(&st, &id, &uri, &body, false).await
}

/// `POST /v8/management/plugins/store/:id/install`.
pub(crate) async fn install_v8(
    State(st): State<ManagementState>,
    Path(id): Path<String>,
    uri: Uri,
    body: Bytes,
) -> ApiResult {
    crate::plugin_store::install(&st, &id, &uri, &body, true).await
}

// ---- quota ----

fn quota_auth_index(uri: &Uri) -> String {
    let idx = query_trim(uri, "auth_index");
    if idx.is_empty() {
        query_trim(uri, "authIndex")
    } else {
        idx
    }
}

/// Go: `credentialQuotaRequest.resolveAuthIndex`.
fn body_auth_index(obj: &Map<String, Value>) -> String {
    ["auth_index", "authIndex", "AuthIndex"]
        .iter()
        .filter_map(|k| obj.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn quota_body(body: &[u8]) -> ApiResult<Map<String, Value>> {
    match first_json(body) {
        Some(Value::Object(o)) => Ok(o),
        Some(Value::Null) => Ok(Map::new()),
        _ => Err(ApiError::bad_request("invalid request body")),
    }
}

fn no_quota_plugin() -> ApiError {
    ApiError::new(404, "quota provider not found for plugin")
}

/// The credential a quota request targets, with its index stamped.
fn quota_auth(st: &ManagementState, auth_index: &str) -> ApiResult<Auth> {
    let mut auth = auth_by_index(st, auth_index).ok_or_else(|| ApiError::new(404, "auth not found"))?;
    auth.ensure_index();
    Ok(auth)
}

fn fetch_request(auth: &Auth, provider: &str) -> QuotaFetchRequest {
    QuotaFetchRequest {
        auth_index: auth.index.clone(),
        auth_id: auth.id.clone(),
        provider: provider.to_string(),
        metadata: auth.metadata.clone(),
        attributes: auth.attributes.clone(),
        ..Default::default()
    }
}

fn reset_request(auth: &Auth, provider: &str) -> QuotaResetRequest {
    QuotaResetRequest {
        auth_index: auth.index.clone(),
        auth_id: auth.id.clone(),
        provider: provider.to_string(),
        metadata: auth.metadata.clone(),
        attributes: auth.attributes.clone(),
        ..Default::default()
    }
}

/// `fetchQuotaForPlugin`.
async fn fetch_quota_for_plugin(st: &ManagementState, plugin_id: &str, auth_index: &str) -> ApiResult {
    let auth = quota_auth(st, auth_index)?;
    let host = quota_host_for_plugin(st, plugin_id)?;
    let ctx = CallCtx::background();
    match host.fetch_quota_by_plugin(&ctx, plugin_id, fetch_request(&auth, &auth.provider)).await {
        Ok(Some(resp)) => Ok(ok_struct(&resp)),
        Ok(None) => Err(no_quota_plugin()),
        Err(e) => Err(ApiError::new(502, format!("failed to fetch quota: {e}"))),
    }
}

fn quota_host_for_plugin(st: &ManagementState, plugin_id: &str) -> ApiResult<Arc<Host>> {
    match &st.plugins {
        Some(h) if h.has_quota_provider_for_plugin(plugin_id) => Ok(h.clone()),
        _ => Err(no_quota_plugin()),
    }
}

/// `GET /plugins/:id/quota?auth_index=`.
pub(crate) async fn get_quota(
    State(st): State<ManagementState>,
    Path(id): Path<String>,
    uri: Uri,
) -> ApiResult {
    let index = quota_auth_index(&uri);
    if index.is_empty() {
        return Err(ApiError::bad_request("auth_index is required"));
    }
    fetch_quota_for_plugin(&st, id.trim(), &index).await
}

/// `POST /plugins/:id/quota`.
pub(crate) async fn fetch_quota(
    State(st): State<ManagementState>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult {
    let obj = quota_body(&body)?;
    let index = body_auth_index(&obj);
    if index.is_empty() {
        return Err(ApiError::bad_request("auth_index is required"));
    }
    fetch_quota_for_plugin(&st, id.trim(), &index).await
}

/// `DELETE /plugins/:id/quota`, `POST /plugins/:id/quota/reset`.
pub(crate) async fn reset_quota(
    State(st): State<ManagementState>,
    Path(id): Path<String>,
    uri: Uri,
    body: Bytes,
) -> ApiResult {
    let mut idx = quota_auth_index(&uri);
    if idx.is_empty() {
        // The body is optional here; a malformed one just leaves the index empty.
        if let Some(Value::Object(o)) = first_json(&body) {
            idx = body_auth_index(&o);
        }
    }
    if idx.is_empty() {
        return Err(ApiError::bad_request("auth_index is required"));
    }
    let plugin_id = id.trim().to_string();
    let auth = quota_auth(&st, &idx)?;
    let host = quota_host_for_plugin(&st, &plugin_id)?;
    let ctx = CallCtx::background();
    let resp = match host.reset_quota_by_plugin(&ctx, &plugin_id, reset_request(&auth, &auth.provider)).await {
        Ok(Some(r)) => r,
        Ok(None) => return Err(no_quota_plugin()),
        Err(e) => return Err(ApiError::new(502, format!("failed to reset quota: {e}"))),
    };
    if !resp.success {
        let msg = if resp.message.is_empty() { "quota reset rejected by plugin".to_string() } else { resp.message };
        return Err(ApiError::new(502, msg));
    }
    finish_reset(&st, &auth, &resp.message)
}

/// The routing-state reset and the answer shared by both reset endpoints.
fn finish_reset(st: &ManagementState, auth: &Auth, message: &str) -> ApiResult {
    if let Err(e) = st.manager.reset_quota(&auth.id) {
        return Err(ApiError::new(500, format!("failed to reset routing quota: {e}")));
    }
    let mut body = json!({"status": "ok", "auth_index": auth.index});
    if !message.is_empty() {
        body["message"] = Value::String(message.to_string());
    }
    Ok(ok_json(&body))
}

/// `GET /quota/providers`.
pub(crate) async fn quota_providers(State(st): State<ManagementState>) -> ApiResult {
    let Some(host) = &st.plugins else {
        return Ok(ok_json(&json!({"providers": []})));
    };
    let providers = host.quota_providers(&CallCtx::background()).await;
    Ok(ok_json(&json!({"providers": providers})))
}

fn resolve_provider(obj: &Map<String, Value>, auth: &Auth) -> (String, String) {
    let plugin_id = text_field(obj, "plugin_id");
    let provider = text_field(obj, "provider");
    (plugin_id, if provider.is_empty() { auth.provider.clone() } else { provider })
}

fn text_field(obj: &Map<String, Value>, name: &str) -> String {
    crate::v0_util::field(obj, name).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// `POST /quota/fetch`: a plugin provider, else the credential's declarative `quota_probe`.
pub(crate) async fn fetch_credential_quota(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    let obj = quota_body(&body)?;
    let index = body_auth_index(&obj);
    if index.is_empty() {
        return Err(ApiError::bad_request("auth_index is required"));
    }
    let auth = quota_auth(&st, &index)?;
    let (plugin_id, provider) = resolve_provider(&obj, &auth);
    if let Some(host) = &st.plugins {
        let ctx = CallCtx::background();
        let req = fetch_request(&auth, &provider);
        let result = if plugin_id.is_empty() {
            host.fetch_quota(&ctx, req).await
        } else {
            host.fetch_quota_by_plugin(&ctx, &plugin_id, req).await
        };
        match result {
            Ok(Some(resp)) => return Ok(ok_struct(&resp)),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("failed to fetch quota for credential {}: {e}", auth.index);
                return Err(ApiError::new(502, format!("failed to fetch quota: {e}")));
            }
        }
    }
    if let Some(Value::Object(probe)) = auth.metadata.get("quota_probe")
        && let Some(result) = crate::quota_probe::execute(&st, &auth, probe).await
    {
        return match result {
            Ok(resp) => Ok(ok_struct(&resp)),
            Err(e) => Err(ApiError::new(502, format!("quota probe failed: {e}"))),
        };
    }
    Err(ApiError::new(501, "no quota provider available for credential"))
}

/// `POST /quota/reset`.
pub(crate) async fn reset_credential_quota(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    let obj = quota_body(&body)?;
    let index = body_auth_index(&obj);
    if index.is_empty() {
        return Err(ApiError::bad_request("auth_index is required"));
    }
    let auth = quota_auth(&st, &index)?;
    let Some(host) = st.plugins.clone() else {
        return Err(ApiError::new(501, "plugin host unavailable"));
    };
    let (plugin_id, provider) = resolve_provider(&obj, &auth);
    let ctx = CallCtx::background();
    let req = reset_request(&auth, &provider);
    let result = if !plugin_id.is_empty() {
        if !host.has_quota_provider_for_plugin(&plugin_id) {
            return Err(no_quota_plugin());
        }
        match host.reset_quota_by_plugin(&ctx, &plugin_id, req).await {
            Ok(None) => return Err(no_quota_plugin()),
            other => other,
        }
    } else {
        if !host.has_quota_provider(&ctx, &provider).await {
            return Err(ApiError::new(501, "no quota provider available for credential to reset"));
        }
        match host.reset_quota(&ctx, req).await {
            Ok(None) => return Err(ApiError::new(502, "quota provider did not handle reset request")),
            other => other,
        }
    };
    let resp = match result {
        Ok(Some(r)) => r,
        Ok(None) => return Err(no_quota_plugin()),
        Err(e) => return Err(ApiError::new(502, format!("plugin quota reset failed: {e}"))),
    };
    if !resp.success {
        let msg = if resp.message.is_empty() { "quota reset rejected by provider".to_string() } else { resp.message };
        return Err(ApiError::new(502, msg));
    }
    finish_reset(&st, &auth, &resp.message)
}
