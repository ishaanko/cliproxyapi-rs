//! Editing credentials: `PATCH /credentials/status` and `PATCH /credentials/fields`
//! (Go: `auth_files_fields.go`, `config_apikey_disable.go`).

use std::collections::{BTreeMap, BTreeSet};

use axum::extract::State;
use bytes::Bytes;
use chrono::Utc;
use cpa_auth::credmeta::{self, ATTRIBUTE_FILE_PRIORITY, ATTRIBUTE_WEIGHT};
use cpa_auth::types::{ATTRIBUTE_SOURCE_BACKEND, AUTH_SOURCE_FILE};
use cpa_auth::{Auth, Status};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::credentials::{ERR_NOT_FOUND, ERR_PLUGIN_VIRTUAL, lookup_auth_file};
use crate::http::{ApiError, ApiResult, blocking, detached, ok_json};
use crate::state::ManagementState;

/// Excluded-model pattern that disables a config-defined API-key credential.
const CONFIG_API_KEY_DISABLE_PATTERN: &str = "*";

#[derive(Deserialize)]
struct StatusRequest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    auth_index: String,
    disabled: Option<bool>,
}

/// `PATCH /credentials/status`.
pub(crate) async fn patch_status(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    detached(patch_status_inner(st, body)).await
}

async fn patch_status_inner(st: ManagementState, body: Bytes) -> ApiResult {
    let req: StatusRequest =
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("invalid request body"))?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }
    let Some(disabled) = req.disabled else {
        return Err(ApiError::bad_request("disabled is required"));
    };
    let Some(mut target) = lookup_auth_file(&st, name, &req.auth_index) else {
        return Err(ApiError::new(404, ERR_NOT_FOUND));
    };
    if target.is_plugin_virtual() {
        // Status changes are allowed only for the source file name, like delete; the expanded
        // virtual auths cannot be modified on their own.
        if !crate::credentials::is_plugin_virtual_source_delete(name, &target) {
            return Err(ApiError::new(409, ERR_PLUGIN_VIRTUAL));
        }
        patch_plugin_virtual_source_status(&st, &target, disabled).await?;
        return Ok(ok_json(&json!({"status": "ok", "disabled": disabled})));
    }

    if target.is_config_api_key() {
        let _guard = st.shared.config_lock.clone().lock_owned().await;
        let mut cfg = (*st.cfg()).clone();
        if !toggle_config_api_key_excluded_all(&mut cfg, &target, disabled) {
            return Err(ApiError::new(404, "config api key entry not found"));
        }
        let path = st.config_path.clone();
        blocking(move || {
            cpa_config::save_config_preserve_comments(&path, &mut cfg, true)
                .map_err(|e| ApiError::new(500, format!("failed to save config: {e}")))
        })
        .await?;
        st.reload_config().await;
        return Ok(ok_json(&json!({
            "status": "ok",
            "disabled": disabled,
            "via": "config:excluded-models",
            "excluded_pattern": CONFIG_API_KEY_DISABLE_PATTERN,
        })));
    }

    apply_disabled_state(&mut target, disabled);
    st.registry
        .update(target)
        .await
        .map_err(|e| ApiError::new(500, format!("failed to update auth: {e}")))?;
    Ok(ok_json(&json!({"status": "ok", "disabled": disabled})))
}

/// `patchPluginVirtualSourceStatus`: toggles `disabled` on the source file of a plugin multi-auth
/// group and on every runtime auth expanded from it.
async fn patch_plugin_virtual_source_status(st: &ManagementState, target: &Auth, disabled: bool) -> ApiResult<()> {
    let mut source = target.attr(cpa_auth::types::ATTRIBUTE_VIRTUAL_SOURCE).trim().to_string();
    if source.is_empty() {
        source = target.attr(cpa_auth::types::ATTRIBUTE_PATH).trim().to_string();
    }
    if source.is_empty() {
        return Err(ApiError::new(409, ERR_PLUGIN_VIRTUAL));
    }
    let path = source.clone();
    let written = blocking(move || Ok(set_source_auth_file_disabled(&path, disabled))).await?;
    if let Err(e) = written {
        return Err(match e {
            SourceError::NotFound => ApiError::new(404, ERR_NOT_FOUND),
            SourceError::Other(m) => ApiError::new(500, m),
        });
    }
    for mut auth in st.registry.list() {
        let same = |attr: &str| crate::credentials::same_path(&auth.attr(attr), &source);
        if !same(cpa_auth::types::ATTRIBUTE_PATH) && !same(cpa_auth::types::ATTRIBUTE_VIRTUAL_SOURCE) {
            continue;
        }
        let id = auth.id.clone();
        apply_disabled_state(&mut auth, disabled);
        st.registry.update(auth).await.map_err(|e| ApiError::new(500, format!("failed to update auth {id}: {e}")))?;
    }
    Ok(())
}

enum SourceError {
    NotFound,
    Other(String),
}

/// `setSourceAuthFileDisabled`.
fn set_source_auth_file_disabled(path: &str, disabled: bool) -> Result<(), SourceError> {
    let path = path.trim();
    if path.is_empty() {
        return Err(SourceError::Other("source auth path is empty".into()));
    }
    let data = std::fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound { SourceError::NotFound } else { SourceError::Other(format!("failed to update source auth file: {e}")) }
    })?;
    let mut metadata: Map<String, Value> = Map::new();
    if !data.iter().all(u8::is_ascii_whitespace) {
        match serde_json::from_slice::<Value>(&data) {
            Ok(Value::Object(m)) => metadata = m,
            Ok(Value::Null) => {}
            Ok(_) | Err(_) => return Err(SourceError::Other("failed to update source auth file: invalid auth file".into())),
        }
    }
    credmeta::normalize_credential_metadata(&mut metadata);
    metadata.insert("disabled".into(), Value::Bool(disabled));
    let raw = cpa_auth::util::marshal_compact(&Value::Object(metadata)).map_err(|e| SourceError::Other(format!("marshal auth file: {e}")))?;
    write_private(path, raw.as_bytes()).map_err(|e| SourceError::Other(format!("failed to update source auth file: {e}")))
}

/// `os.WriteFile(path, data, 0o600)`.
fn write_private(path: &str, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(data)
}

fn apply_disabled_state(auth: &mut Auth, disabled: bool) {
    auth.disabled = disabled;
    if disabled {
        auth.status = Status::Disabled;
        auth.status_message = "disabled via management API".into();
    } else {
        auth.status = Status::Active;
        auth.status_message.clear();
    }
    auth.updated_at = Some(Utc::now());
    auth.metadata
        .insert("disabled".into(), Value::Bool(disabled));
}

// ---- config API-key credentials ----

fn set_excluded_all(models: &[String], disable: bool) -> Vec<String> {
    let mut list = models.to_vec();
    if disable {
        if !list
            .iter()
            .any(|m| m.trim() == CONFIG_API_KEY_DISABLE_PATTERN)
        {
            list.push(CONFIG_API_KEY_DISABLE_PATTERN.into());
        }
    } else {
        list.retain(|m| m.trim() != CONFIG_API_KEY_DISABLE_PATTERN);
    }
    cpa_config::normalize_excluded_models(&list)
}

/// Adds or removes `*` from the excluded models of the config entry behind a config-defined
/// API-key credential. Entries are matched on provider, API key, base URL and, when several
/// entries share those, proxy URL and prefix (Go matches the synthesized stable auth id).
/// Returns whether an entry was found.
fn toggle_config_api_key_excluded_all(
    cfg: &mut cpa_config::Config,
    auth: &Auth,
    disable: bool,
) -> bool {
    let key = auth.attr("api_key");
    let base = auth.attr("base_url");
    let proxy = auth.proxy_url.trim().to_string();
    let prefix = auth.prefix.trim().to_string();
    let matches = |k: &str, b: &str, p: &str, x: &str| {
        k.trim() == key && b.trim() == base && (p.trim() == proxy) && (x.trim() == prefix)
    };
    let matches_loose = |k: &str, b: &str| k.trim() == key && b.trim() == base;

    macro_rules! toggle {
        ($list:expr) => {{
            let list = &mut $list;
            let idx = list
                .iter()
                .position(|e| matches(&e.api_key, &e.base_url, &e.proxy_url, &e.prefix))
                .or_else(|| {
                    list.iter()
                        .position(|e| matches_loose(&e.api_key, &e.base_url))
                });
            match idx {
                Some(i) => {
                    list[i].excluded_models = set_excluded_all(&list[i].excluded_models, disable);
                    true
                }
                None => false,
            }
        }};
    }
    match auth.provider.trim().to_lowercase().as_str() {
        "gemini" => toggle!(cfg.gemini_key),
        "gemini-interactions" => toggle!(cfg.interactions_key),
        "claude" => toggle!(cfg.claude_key),
        "codex" => toggle!(cfg.codex_key),
        "xai" => toggle!(cfg.xai_key),
        "meta" => toggle!(cfg.meta_key),
        "vertex" => toggle!(cfg.vertex_compat_api_key),
        _ => false,
    }
}

// ---- PATCH /credentials/fields ----

type Fields = Vec<(String, Value)>;

/// `normalizeAuthFilePatchFields`: canonical root keys; a canonical spelling wins over its legacy
/// alias and two spellings of the same field in one request is an error.
fn normalize_patch_fields(fields: Fields) -> Result<Fields, String> {
    let mut out: BTreeMap<String, (Value, String, bool)> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for (key, value) in fields {
        let mut parts: Vec<String> = key
            .trim()
            .split('.')
            .map(|p| p.trim().to_string())
            .collect();
        let original_root = parts[0].clone();
        parts[0] = credmeta::canonical_credential_metadata_key(&original_root).to_string();
        let canonical_path = parts.join(".");
        let current_canonical = original_root == parts[0];
        if let Some((_, original, was_canonical)) = out.get(&canonical_path) {
            if *was_canonical != current_canonical {
                if current_canonical {
                    out.insert(canonical_path, (value, key, true));
                }
                continue;
            }
            return Err(format!(
                "auth file fields {original:?} and {key:?} refer to the same field"
            ));
        }
        order.push(canonical_path.clone());
        out.insert(canonical_path, (value, key, current_canonical));
    }
    Ok(order
        .into_iter()
        .filter_map(|p| out.remove(&p).map(|(v, _, _)| (p, v)))
        .collect())
}

fn root_field(path: &str) -> &str {
    path.trim().split('.').next().unwrap_or("").trim()
}

/// `request_retry` patch: `None` means untouched, `Some(None)` removes the field.
fn decode_request_retry(fields: &Fields) -> Result<Option<Option<i64>>, String> {
    let mut raw = None;
    for (key, value) in fields {
        let path = key.trim();
        if root_field(path) == "request_retry" && path != "request_retry" {
            return Err("request_retry does not support nested fields".into());
        }
        if path == "request_retry" {
            raw = Some(value);
        }
    }
    let Some(value) = raw else { return Ok(None) };
    let invalid = || "request_retry must be an integer or null".to_string();
    match value {
        Value::Null => Ok(Some(None)),
        Value::Number(n) => {
            let v = n.to_string().parse::<i64>().map_err(|_| invalid())?;
            let normalized = i32::try_from(v).map_err(|_| invalid())?;
            Ok(Some((normalized >= 0).then_some(i64::from(normalized))))
        }
        _ => Err(invalid()),
    }
}

/// `setAuthFileMetadataValue`: dotted path into nested objects, creating them.
fn set_metadata_value(
    metadata: &mut Map<String, Value>,
    path: &str,
    value: Value,
) -> Result<(), String> {
    let parts: Vec<&str> = path.split('.').map(str::trim).collect();
    let mut current = metadata;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            return Err(format!("invalid field path: {path}"));
        }
        if i == parts.len() - 1 {
            current.insert((*part).to_string(), value);
            return Ok(());
        }
        let entry = current
            .entry((*part).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        let Value::Object(next) = entry else {
            return Ok(());
        };
        current = next;
    }
    Ok(())
}

/// `applyAuthFileHeadersPatch`: a string map merges into the existing headers (empty value
/// deletes a header); anything else replaces the field.
fn apply_headers_patch(auth: &mut Auth, value: Value) {
    let Value::Object(patch) = &value else {
        auth.metadata.insert("headers".into(), value);
        return;
    };
    let mut strings: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in patch {
        match v {
            Value::String(s) => {
                strings.insert(k.clone(), s.clone());
            }
            _ => {
                auth.metadata.insert("headers".into(), value);
                return;
            }
        }
    }
    let mut next = credmeta::extract_custom_headers(&auth.metadata);
    for (k, v) in strings {
        let name = k.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let v = v.trim().to_string();
        if v.is_empty() {
            next.remove(&name);
        } else {
            next.insert(name, v);
        }
    }
    if next.is_empty() {
        auth.metadata.shift_remove("headers");
    } else {
        auth.metadata.insert(
            "headers".into(),
            Value::Object(
                next.into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect(),
            ),
        );
    }
}

fn int_value(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n
            .to_string()
            .parse()
            .ok()
            .or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn bool_value(v: Option<&Value>) -> Option<bool> {
    match v? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `syncAuthFileMetadataFields`: keeps the runtime attributes in step with the edited metadata.
fn sync_metadata_fields(auth: &mut Auth, touched: &BTreeSet<String>) {
    if touched.contains("prefix")
        && let Some(Value::String(p)) = auth.metadata.get("prefix")
    {
        auth.prefix = p.trim().to_string();
    }
    if touched.contains("proxy_url")
        && let Some(Value::String(p)) = auth.metadata.get("proxy_url")
    {
        auth.proxy_url = p.trim().to_string();
    }
    if touched.contains("headers") {
        auth.attributes.retain(|k, _| !k.starts_with("header:"));
        for (name, value) in credmeta::extract_custom_headers(&auth.metadata) {
            auth.attributes.insert(format!("header:{name}"), value);
        }
    }
    if touched.contains("priority") {
        match int_value(auth.metadata.get("priority")) {
            None => {
                auth.attributes.remove("priority");
                auth.attributes.remove(ATTRIBUTE_FILE_PRIORITY);
            }
            Some(priority) => {
                if auth
                    .attributes
                    .get(ATTRIBUTE_SOURCE_BACKEND)
                    .map(String::as_str)
                    == Some(AUTH_SOURCE_FILE)
                {
                    auth.attributes
                        .insert(ATTRIBUTE_FILE_PRIORITY.into(), "true".into());
                }
                if priority == 0 {
                    auth.attributes.remove("priority");
                } else {
                    auth.attributes
                        .insert("priority".into(), priority.to_string());
                }
            }
        }
    }
    if touched.contains(ATTRIBUTE_WEIGHT) {
        match auth
            .metadata
            .get(ATTRIBUTE_WEIGHT)
            .map(credmeta::parse_weight_value)
        {
            Some(Ok(w)) => {
                auth.attributes
                    .insert(ATTRIBUTE_WEIGHT.into(), w.to_string());
            }
            _ => {
                auth.attributes.remove(ATTRIBUTE_WEIGHT);
            }
        }
    }
    if touched.contains("note") {
        match auth.metadata.get("note") {
            Some(Value::String(n)) if !n.trim().is_empty() => {
                auth.attributes.insert("note".into(), n.trim().to_string());
            }
            _ => {
                auth.attributes.remove("note");
            }
        }
    }
    if touched.contains("websockets") {
        match bool_value(auth.metadata.get("websockets")) {
            Some(b) => {
                auth.attributes.insert("websockets".into(), b.to_string());
            }
            None => {
                auth.attributes.remove("websockets");
            }
        }
    }
    if touched.contains("disabled")
        && let Some(disabled) = bool_value(auth.metadata.get("disabled"))
    {
        auth.disabled = disabled;
        if disabled {
            auth.status = Status::Disabled;
            if auth.status_message.trim().is_empty() {
                auth.status_message = "disabled via management API".into();
            }
        } else {
            auth.status = Status::Active;
            auth.status_message.clear();
        }
    }
    if (touched.contains("plan_type") || touched.contains("id_token"))
        && auth.provider.trim().eq_ignore_ascii_case("codex")
    {
        let plan = match (
            auth.metadata.get("plan_type"),
            auth.metadata.get("id_token"),
        ) {
            (Some(Value::String(p)), _) if !p.trim().is_empty() => p.trim().to_string(),
            (_, Some(Value::String(t))) if !t.trim().is_empty() => {
                cpa_auth::jwt::parse_codex_id_token(t)
                    .map(|c| c.plan_type())
                    .unwrap_or_else(|_| cpa_auth::jwt::DEFAULT_PLAN_TYPE.to_string())
            }
            _ => String::new(),
        };
        if plan.is_empty() {
            auth.attributes.remove("plan_type");
        } else {
            auth.attributes.insert("plan_type".into(), plan);
        }
    }
}

/// `PATCH /credentials/fields`: `{"name": ..., <field>: <value>, ...}`; `null` removes a field.
pub(crate) async fn patch_fields(State(st): State<ManagementState>, body: Bytes) -> ApiResult {
    detached(patch_fields_inner(st, body)).await
}

async fn patch_fields_inner(st: ManagementState, body: Bytes) -> ApiResult {
    let Ok(Value::Object(mut req)) = serde_json::from_slice::<Value>(&body) else {
        return Err(ApiError::bad_request("invalid request body"));
    };
    let name = match req.shift_remove("name") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return Err(ApiError::bad_request("name is required")),
    };
    let fields =
        normalize_patch_fields(req.into_iter().collect()).map_err(ApiError::bad_request)?;
    let retry_patch = decode_request_retry(&fields).map_err(ApiError::bad_request)?;
    let fields: Fields = fields
        .into_iter()
        .filter(|(k, _)| k.trim() != "request_retry")
        .collect();

    let target = st
        .registry
        .get(&name)
        .or_else(|| st.registry.list().into_iter().find(|a| a.file_name == name));
    let Some(mut target) = target else {
        return Err(ApiError::new(404, ERR_NOT_FOUND));
    };
    if target.is_plugin_virtual() {
        return Err(ApiError::new(409, ERR_PLUGIN_VIRTUAL));
    }
    credmeta::normalize_credential_metadata(&mut target.metadata);

    let mut changed = false;
    let mut touched = BTreeSet::new();
    for (key, value) in fields {
        let path = key.trim().to_string();
        if path.is_empty() {
            return Err(ApiError::bad_request("field name is required"));
        }
        if path == ATTRIBUTE_WEIGHT {
            if value.is_null() {
                target.metadata.shift_remove(ATTRIBUTE_WEIGHT);
            } else {
                if !value.is_number() {
                    return Err(ApiError::bad_request("weight must be an integer"));
                }
                let weight = credmeta::parse_weight_value(&value)
                    .map_err(|e| ApiError::bad_request(e.to_string()))?;
                target
                    .metadata
                    .insert(ATTRIBUTE_WEIGHT.into(), weight.into());
            }
        } else if root_field(&path) == ATTRIBUTE_WEIGHT {
            return Err(ApiError::bad_request(
                "weight does not support nested fields",
            ));
        } else if path == "headers" {
            apply_headers_patch(&mut target, value);
        } else {
            set_metadata_value(&mut target.metadata, &path, value)
                .map_err(ApiError::bad_request)?;
        }
        let root = root_field(&path);
        if !root.is_empty() {
            touched.insert(root.to_string());
        }
        changed = true;
    }
    if let Some(retry) = retry_patch {
        match retry {
            None => {
                target.metadata.shift_remove("request_retry");
            }
            Some(v) => {
                target.metadata.insert("request_retry".into(), v.into());
            }
        }
        changed = true;
    }
    if !changed {
        return Err(ApiError::bad_request("no fields to update"));
    }
    sync_metadata_fields(&mut target, &touched);
    target.updated_at = Some(Utc::now());
    st.registry
        .update(target)
        .await
        .map_err(|e| ApiError::new(500, format!("failed to update auth: {e}")))?;
    Ok(ok_json(&json!({"status": "ok"})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_and_canonical_spellings_collide_unless_one_is_canonical() {
        let ok = normalize_patch_fields(vec![
            ("proxy-url".into(), json!("a")),
            ("proxy_url".into(), json!("b")),
        ])
        .unwrap();
        assert_eq!(ok, vec![("proxy_url".to_string(), json!("b"))]);
        let err =
            normalize_patch_fields(vec![("note".into(), json!(1)), ("note".into(), json!(2))]);
        assert!(err.is_err());
    }

    #[test]
    fn request_retry_patch_rules() {
        let f = |v: Value| decode_request_retry(&vec![("request_retry".to_string(), v)]);
        assert_eq!(f(json!(3)).unwrap(), Some(Some(3)));
        assert_eq!(f(json!(-1)).unwrap(), Some(None));
        assert_eq!(f(Value::Null).unwrap(), Some(None));
        assert!(f(json!("3")).is_err());
        assert_eq!(decode_request_retry(&vec![]).unwrap(), None);
    }

    #[test]
    fn headers_merge_and_empty_value_deletes() {
        let mut a = Auth::new("a.json", "claude");
        a.metadata
            .insert("headers".into(), json!({"X-A": "1", "X-B": "2"}));
        apply_headers_patch(&mut a, json!({"X-B": "", "X-C": " 3 "}));
        assert_eq!(a.metadata["headers"], json!({"X-A": "1", "X-C": "3"}));
        apply_headers_patch(&mut a, json!({"X-A": "", "X-C": ""}));
        assert!(!a.metadata.contains_key("headers"));
    }

    #[test]
    fn sync_updates_attributes_from_metadata() {
        let mut a = Auth::new("a.json", "codex");
        a.metadata.insert("priority".into(), json!(7));
        a.metadata.insert("note".into(), json!(" hi "));
        a.metadata.insert("disabled".into(), json!(true));
        let touched: BTreeSet<String> = ["priority", "note", "disabled"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        sync_metadata_fields(&mut a, &touched);
        assert_eq!(a.attributes.get("priority").map(String::as_str), Some("7"));
        assert_eq!(a.attributes.get("note").map(String::as_str), Some("hi"));
        assert!(a.disabled && a.status == Status::Disabled);
    }
}
