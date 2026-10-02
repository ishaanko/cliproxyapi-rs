//! v0 plugin and quota endpoints (Go: `plugins.go`, `plugin_store.go`, `plugin_quota.go`).
//!
//! This build has no plugin host, so these behave like Go does with a host that has no plugin
//! registered: configuration edits work, nothing is discoverable, and quota providers are absent.

use axum::extract::{Path, State};
use axum::http::Uri;
use bytes::Bytes;
use serde_json::{Map, Value, json};

use crate::credentials::auth_by_index;
use crate::http::{ApiError, ApiResult, ok_json, query_trim};
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

fn json_to_yaml(v: &Value) -> serde_yaml_ng::Value {
    serde_yaml_ng::to_value(v).unwrap_or(serde_yaml_ng::Value::Null)
}

fn instance_mapping(item: &cpa_config::PluginInstanceConfig) -> serde_yaml_ng::Mapping {
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

fn instance_from_mapping(
    m: serde_yaml_ng::Mapping,
) -> ApiResult<cpa_config::PluginInstanceConfig> {
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
    match st.cfg().plugins.configs.get(&id) {
        Some(item) => Ok(ok_json(&cpa_auth::util::sort_json(&instance_json(item)))),
        None => Err(plugin_not_found()),
    }
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
                m.remove(k.as_str());
            } else {
                m.insert(k.as_str().into(), json_to_yaml(&obj[k]));
            }
        }
        c.plugins.configs.insert(id, instance_from_mapping(m)?);
        Ok(())
    })
    .await
}

/// `DELETE /plugins/:id`: drops the saved config (no plugin files are ever discovered here).
pub(crate) async fn delete(
    State(st): State<ManagementState>,
    Path(id): Path<String>,
) -> ApiResult {
    let id = plugin_id(&id)?;
    if !st.cfg().plugins.configs.contains_key(&id) {
        return Err(plugin_not_found());
    }
    let reply_id = id.clone();
    persist(&st, move |c| {
        c.plugins.configs.remove(&id);
        Ok(())
    })
    .await?;
    Ok(ok_json(&json!({
        "status": "deleted",
        "id": reply_id,
        "path": "",
        "file_deleted": false,
        "configured_removed": true,
        "restart_required": false,
    })))
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
    crate::plugin_store::install(&st, &id, &uri, &body).await
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

fn require_auth(st: &ManagementState, auth_index: &str) -> ApiResult<()> {
    if auth_index.is_empty() {
        return Err(ApiError::bad_request("auth_index is required"));
    }
    if auth_by_index(st, auth_index).is_none() {
        return Err(ApiError::new(404, "auth not found"));
    }
    Ok(())
}

fn no_quota_plugin() -> ApiError {
    ApiError::new(404, "quota provider not found for plugin")
}

/// `GET /plugins/:id/quota?auth_index=`.
pub(crate) async fn get_quota(
    State(st): State<ManagementState>,
    Path(_id): Path<String>,
    uri: Uri,
) -> ApiResult {
    require_auth(&st, &quota_auth_index(&uri))?;
    Err(no_quota_plugin())
}

/// `POST /plugins/:id/quota`.
pub(crate) async fn fetch_quota(
    State(st): State<ManagementState>,
    Path(_id): Path<String>,
    body: Bytes,
) -> ApiResult {
    let obj = quota_body(&body)?;
    require_auth(&st, &body_auth_index(&obj))?;
    Err(no_quota_plugin())
}

/// `DELETE /plugins/:id/quota`, `POST /plugins/:id/quota/reset`.
pub(crate) async fn reset_quota(
    State(st): State<ManagementState>,
    Path(_id): Path<String>,
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
    require_auth(&st, &idx)?;
    Err(no_quota_plugin())
}

/// `GET /quota/providers`: no quota provider is registered.
pub(crate) async fn quota_providers() -> ApiResult {
    Ok(ok_json(&json!({"providers": []})))
}

/// `POST /quota/fetch`: only plugin providers or a declarative `quota_probe` can answer.
pub(crate) async fn fetch_credential_quota(
    State(st): State<ManagementState>,
    body: Bytes,
) -> ApiResult {
    let obj = quota_body(&body)?;
    require_auth(&st, &body_auth_index(&obj))?;
    Err(ApiError::new(501, "no quota provider available for credential"))
}

/// `POST /quota/reset`.
pub(crate) async fn reset_credential_quota(
    State(st): State<ManagementState>,
    body: Bytes,
) -> ApiResult {
    let obj = quota_body(&body)?;
    require_auth(&st, &body_auth_index(&obj))?;
    let plugin = obj
        .get("plugin_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if plugin.is_empty() {
        Err(ApiError::new(
            501,
            "no quota provider available for credential to reset",
        ))
    } else {
        Err(ApiError::new(404, "quota provider not found for plugin"))
    }
}
