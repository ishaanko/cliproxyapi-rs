//! Credential files: list, models, download, upload, delete (Go: `auth_files.go`,
//! `auth_files_crud.go`). v8 paths `/credentials*`.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use axum::extract::{FromRequest, Multipart, Request, State};
use axum::http::{HeaderValue, Uri, header};
use axum::response::Response;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use cpa_auth::credmeta::{self, ATTRIBUTE_WEIGHT, Metadata};
use cpa_auth::types::{
    ATTRIBUTE_PATH, ATTRIBUTE_RUNTIME_ONLY, ATTRIBUTE_VIRTUAL_SOURCE, QuotaState,
};
use cpa_auth::{Auth, Status};
use serde_json::{Map, Value, json};

use crate::cooldown::{cooldown_snapshot, reconcile_cooldown_state};
use crate::http::{
    ApiError, ApiResult, blocking, content_type_is, detached, json_response, ok_json, query_get,
    query_pairs, query_trim, read_body, rfc3339,
};
use crate::state::{ManagementState, abs_path};

pub(crate) const DEFAULT_PAGE_SIZE: usize = 50;

pub(crate) const ERR_NOT_FOUND: &str = "auth file not found";
pub(crate) const ERR_PLUGIN_VIRTUAL: &str =
    "plugin virtual auth cannot be modified directly; edit or delete the source auth file";

/// `isUnsafeAuthFileName`: empty, or containing a path separator.
pub(crate) fn is_unsafe_name(name: &str) -> bool {
    name.trim().is_empty() || name.contains(['/', '\\'])
}

fn base_name(name: &str) -> String {
    Path::new(name.trim())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn has_json_suffix(name: &str) -> bool {
    name.to_lowercase().ends_with(".json")
}

fn is_runtime_only(auth: &Auth) -> bool {
    auth.attr(ATTRIBUTE_RUNTIME_ONLY)
        .eq_ignore_ascii_case("true")
}

/// Raw (untrimmed) attribute, like Go's `authAttribute`.
fn attribute(auth: &Auth, key: &str) -> String {
    auth.attributes.get(key).cloned().unwrap_or_default()
}

fn auth_list_name(auth: &Auth) -> String {
    let name = auth.file_name.trim();
    if name.is_empty() {
        auth.id.trim().to_string()
    } else {
        name.to_string()
    }
}

fn compare_list_order(a: &Auth, b: &Auth) -> std::cmp::Ordering {
    let (an, bn) = (auth_list_name(a), auth_list_name(b));
    an.to_lowercase()
        .cmp(&bn.to_lowercase())
        .then_with(|| an.cmp(&bn))
        .then_with(|| a.id.trim().cmp(b.id.trim()))
        .then_with(|| a.index.trim().cmp(b.index.trim()))
}

/// `isAuthFileListable`.
fn is_listable(auth: &Auth) -> bool {
    let runtime_only = is_runtime_only(auth);
    if runtime_only && (auth.disabled || auth.status == Status::Disabled) {
        return false;
    }
    let path = attribute(auth, ATTRIBUTE_PATH);
    let path = path.trim();
    if path.is_empty() {
        return runtime_only;
    }
    let missing = !Path::new(path).exists();
    !(missing
        && !runtime_only
        && (auth.disabled || auth.status == Status::Disabled || removed_via_management(auth)))
}

fn removed_via_management(auth: &Auth) -> bool {
    auth.status_message
        .trim()
        .eq_ignore_ascii_case("removed via management api")
}

fn matches_lookup(auth: &mut Auth, name: &str, auth_index: &str) -> bool {
    if !name.is_empty() && auth.id.trim() != name && auth.file_name.trim() != name {
        return false;
    }
    auth_index.is_empty() || auth.ensure_index().trim() == auth_index
}

/// `lookupAuthFile`: by id or file name, optionally pinned to an `auth_index`.
pub(crate) fn lookup_auth_file(st: &ManagementState, name: &str, auth_index: &str) -> Option<Auth> {
    let (name, auth_index) = (name.trim(), auth_index.trim());
    if name.is_empty() {
        return None;
    }
    if auth_index.is_empty() {
        if let Some(a) = st.registry.get(name) {
            return Some(a);
        }
        return st
            .registry
            .list()
            .into_iter()
            .find(|a| a.file_name.trim() == name);
    }
    st.registry
        .list()
        .into_iter()
        .find_map(|mut a| matches_lookup(&mut a, name, auth_index).then_some(a))
}

/// `authByIndex`.
pub(crate) fn auth_by_index(st: &ManagementState, auth_index: &str) -> Option<Auth> {
    let auth_index = auth_index.trim();
    if auth_index.is_empty() {
        return None;
    }
    st.registry
        .list()
        .into_iter()
        .find_map(|mut a| (a.ensure_index() == auth_index).then_some(a))
}

// ---- list ----

struct Pagination {
    page: usize,
    page_size: usize,
}

fn parse_pagination(uri: &Uri) -> ApiResult<Option<Pagination>> {
    let page_raw = query_get(uri, "page");
    let size_raw = query_get(uri, "page_size");
    if page_raw.is_none() && size_raw.is_none() {
        return Ok(None);
    }
    let positive = |raw: &str| raw.trim().parse::<usize>().ok().filter(|n| *n > 0);
    let page = match page_raw {
        Some(raw) => positive(&raw)
            .ok_or_else(|| ApiError::bad_request("page must be a positive integer"))?,
        None => 1,
    };
    let page_size = match size_raw {
        Some(raw) => positive(&raw)
            .ok_or_else(|| ApiError::bad_request("page_size must be a positive integer"))?,
        None => DEFAULT_PAGE_SIZE,
    };
    Ok(Some(Pagination { page, page_size }))
}

impl Pagination {
    /// `[start, end)` of the requested page within `total` items.
    fn bounds(&self, total: usize) -> (usize, usize) {
        if total == 0 {
            return (0, total);
        }
        if self.page > 1 && self.page - 1 > total / self.page_size {
            return (total, total);
        }
        let start = (self.page - 1).saturating_mul(self.page_size);
        if start >= total {
            return (total, total);
        }
        let remaining = total - start;
        if self.page_size >= remaining {
            (start, total)
        } else {
            (start, start + self.page_size)
        }
    }
}

/// `GET /credentials`.
pub(crate) async fn list(State(st): State<ManagementState>, req: Request) -> ApiResult {
    let pagination = parse_pagination(req.uri())?;
    let name_filter = query_trim(req.uri(), "name");
    let index_filter = query_trim(req.uri(), "auth_index");
    let observed_at = Utc::now();
    let cooldowns_known = !st.cfg().home.enabled;
    let quota_supported = match &st.plugins {
        Some(host) => Some(host.quota_supported_providers_set(&cpa_plugin::CallCtx::background()).await),
        None => None,
    };
    let auths = st.registry.list();
    // Building entries stats credential files.
    let body = blocking(move || {
        Ok(list_blocking(
            auths,
            pagination,
            name_filter,
            index_filter,
            observed_at,
            cooldowns_known,
            quota_supported.as_ref(),
        ))
    })
    .await?;
    Ok(ok_json(&body))
}

fn list_blocking(
    mut auths: Vec<Auth>,
    pagination: Option<Pagination>,
    name_filter: String,
    index_filter: String,
    observed_at: DateTime<Utc>,
    cooldowns_known: bool,
    quota_supported: Option<&std::collections::HashSet<String>>,
) -> Value {
    let entry = |auth: &mut Auth| -> Option<Value> {
        let mut entry = build_entry(auth, observed_at, quota_supported)?;
        let cooldowns = if cooldowns_known {
            serde_json::to_value(cooldown_snapshot(auth, observed_at)).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        entry.insert("cooldowns".into(), cooldowns);
        Some(crate::http::sort_top(Value::Object(entry)))
    };

    if let Some(p) = pagination {
        let mut matching: Vec<Auth> = auths
            .into_iter()
            .filter_map(|mut a| {
                (matches_lookup(&mut a, &name_filter, &index_filter) && is_listable(&a))
                    .then_some(a)
            })
            .collect();
        for a in &mut matching {
            a.ensure_index();
        }
        matching.sort_by(compare_list_order);
        let total = matching.len();
        let (start, end) = p.bounds(total);
        let files: Vec<Value> = matching[start..end].iter_mut().filter_map(entry).collect();
        return json!({
            "observed_at": rfc3339(observed_at),
            "files": files,
            "total": total,
            "page": p.page,
            "page_size": p.page_size,
            "has_more": end < total,
        });
    }

    let mut files: Vec<Value> = auths
        .iter_mut()
        .filter_map(|a| {
            if matches_lookup(a, &name_filter, &index_filter) {
                entry(a)
            } else {
                None
            }
        })
        .collect();
    files.sort_by_key(|f| {
        f.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase()
    });
    json!({"observed_at": rfc3339(observed_at), "files": files})
}

fn quota_observation(provider: &str, quota: &QuotaState) -> Value {
    let supported = matches!(
        provider.trim().to_lowercase().as_str(),
        "claude" | "codex" | "devin"
    );
    let mut observed = Map::new();
    let signals: BTreeMap<&String, &String> = if supported {
        quota.signals.iter().collect()
    } else {
        BTreeMap::new()
    };
    if supported && let Some(t) = quota.observed_at {
        observed.insert("observed_at".into(), rfc3339(t).into());
    }
    observed.insert(
        "signals".into(),
        serde_json::to_value(signals).unwrap_or_default(),
    );
    Value::Object(observed)
}

fn project_id(auth: &Auth) -> String {
    let m = auth.meta_str("project_id");
    if !m.is_empty() {
        m
    } else {
        auth.attr("project_id")
    }
}

fn email(auth: &Auth) -> String {
    if let Some(Value::String(v)) = auth.metadata.get("email") {
        return v.trim().to_string();
    }
    let e = auth.attr("email");
    if !e.is_empty() {
        e
    } else {
        auth.attr("account_email")
    }
}

fn weight_value(auth: &Auth) -> Option<i64> {
    let raw = auth.attr(ATTRIBUTE_WEIGHT);
    if !raw.is_empty() {
        return credmeta::parse_weight_string(&raw).ok();
    }
    match auth.metadata.get(ATTRIBUTE_WEIGHT) {
        None | Some(Value::Null) => None,
        Some(v) => credmeta::parse_weight_value(v).ok(),
    }
}

fn bool_value(auth: &Auth, key: &str) -> Option<bool> {
    if let Ok(b) = auth.attr(key).parse::<bool>() {
        return Some(b);
    }
    match auth.metadata.get(key) {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Integer attribute `priority`, falling back to metadata.
fn priority_value(auth: &Auth) -> Option<i64> {
    let p = auth.attr("priority");
    if !p.is_empty() {
        return p.parse().ok();
    }
    match auth.metadata.get("priority") {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

fn note_value(auth: &Auth) -> Option<String> {
    let n = auth.attr("note");
    if !n.is_empty() {
        return Some(n);
    }
    match auth.metadata.get("note") {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// Plan type and subscription window claims of a Codex `id_token`.
fn codex_id_token_claims(auth: &Auth) -> Option<Value> {
    if !auth.provider.trim().eq_ignore_ascii_case("codex") {
        return None;
    }
    let token = auth.meta_str("id_token");
    let claims = cpa_auth::jwt::parse_codex_id_token(&token).ok()?;
    let info = &claims.codex_auth_info;
    let mut out = Map::new();
    if !info.chatgpt_account_id.trim().is_empty() {
        out.insert(
            "chatgpt_account_id".into(),
            info.chatgpt_account_id.trim().into(),
        );
    }
    if !info.chatgpt_plan_type.trim().is_empty() {
        out.insert("plan_type".into(), info.chatgpt_plan_type.trim().into());
    }
    if !info.chatgpt_subscription_active_start.is_null() {
        out.insert(
            "chatgpt_subscription_active_start".into(),
            info.chatgpt_subscription_active_start.clone(),
        );
    }
    if !info.chatgpt_subscription_active_until.is_null() {
        out.insert(
            "chatgpt_subscription_active_until".into(),
            info.chatgpt_subscription_active_until.clone(),
        );
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// `buildAuthFileEntry` (without `cooldowns`). `None` hides the credential from the list.
pub(crate) fn build_entry(
    auth: &mut Auth,
    now: DateTime<Utc>,
    quota_supported: Option<&std::collections::HashSet<String>>,
) -> Option<Map<String, Value>> {
    auth.ensure_index();
    let runtime_only = is_runtime_only(auth);
    if runtime_only && (auth.disabled || auth.status == Status::Disabled) {
        return None;
    }
    let path = attribute(auth, ATTRIBUTE_PATH).trim().to_string();
    if path.is_empty() && !runtime_only {
        return None;
    }
    let name = auth_list_name(auth);
    let rec = reconcile_cooldown_state(auth, now);
    let provider = auth.provider.trim().to_string();

    let mut e = Map::new();
    e.insert("id".into(), auth.id.clone().into());
    e.insert("auth_index".into(), auth.index.clone().into());
    e.insert("name".into(), name.into());
    e.insert("type".into(), provider.clone().into());
    e.insert("provider".into(), provider.clone().into());
    e.insert("label".into(), auth.label.clone().into());
    e.insert(
        "status".into(),
        serde_json::to_value(rec.status).unwrap_or_default(),
    );
    e.insert("status_message".into(), rec.status_message.into());
    e.insert("disabled".into(), auth.disabled.into());
    e.insert("unavailable".into(), rec.unavailable.into());
    e.insert("runtime_only".into(), runtime_only.into());
    e.insert("source".into(), "memory".into());
    e.insert("size".into(), 0.into());
    e.insert("success".into(), auth.success.into());
    e.insert("failed".into(), auth.failed.into());
    e.insert(
        "recent_requests".into(),
        serde_json::to_value(auth.recent_requests_snapshot(Utc::now())).unwrap_or_default(),
    );
    e.insert("quota".into(), quota_observation(&provider, &auth.quota));
    let mut model_quotas = Map::new();
    for (model, state) in &auth.model_states {
        if state.quota.observed_at.is_none() && state.quota.signals.is_empty() {
            continue;
        }
        model_quotas.insert(model.clone(), quota_observation(&provider, &state.quota));
    }
    if matches!(
        provider.to_lowercase().as_str(),
        "claude" | "codex" | "devin"
    ) && !model_quotas.is_empty()
    {
        e.insert("model_quotas".into(), Value::Object(model_quotas));
    }
    if quota_supported.is_some_and(|set| set.contains(&provider.to_lowercase())) {
        e.insert("supports_quota".into(), true.into());
        e.insert("quota_provider".into(), provider.clone().into());
    }
    if let Some(probe) = auth.metadata.get("quota_probe").filter(|p| !p.is_null()) {
        e.insert("supports_quota".into(), true.into());
        e.insert("quota_probe".into(), probe.clone());
    }
    let em = email(auth);
    if !em.is_empty() {
        e.insert("email".into(), em.into());
    }
    let pid = project_id(auth);
    if !pid.is_empty() {
        e.insert("project_id".into(), pid.into());
    }
    let (account_type, account) = auth.account_info();
    if !account_type.is_empty() {
        e.insert("account_type".into(), account_type.into());
    }
    if !account.is_empty() {
        e.insert("account".into(), account.into());
    }
    if let Some(t) = auth.created_at {
        e.insert("created_at".into(), rfc3339(t).into());
    }
    if let Some(t) = auth.updated_at {
        e.insert("modtime".into(), rfc3339(t).into());
        e.insert("updated_at".into(), rfc3339(t).into());
    }
    if let Some(t) = auth.last_refreshed_at {
        e.insert("last_refresh".into(), rfc3339(t).into());
    }
    if let Some(t) = rec.next_retry_after {
        e.insert("next_retry_after".into(), rfc3339(t).into());
    }
    if !path.is_empty() {
        e.insert("path".into(), path.clone().into());
        e.insert("source".into(), "file".into());
        match std::fs::metadata(&path) {
            Ok(meta) => {
                e.insert("size".into(), meta.len().into());
                if let Ok(m) = meta.modified() {
                    e.insert("modtime".into(), rfc3339(DateTime::<Utc>::from(m)).into());
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // Hide credentials removed from disk but still lingering in memory.
                if !runtime_only
                    && (auth.disabled
                        || auth.status == Status::Disabled
                        || removed_via_management(auth))
                {
                    return None;
                }
                e.insert("source".into(), "memory".into());
            }
            Err(err) => tracing::warn!("failed to stat auth file {path}: {err}"),
        }
    }
    if let Some(claims) = codex_id_token_claims(auth) {
        e.insert("id_token".into(), claims);
    }
    if let Some(p) = priority_value(auth) {
        e.insert("priority".into(), p.into());
    }
    if let Some(n) = note_value(auth) {
        e.insert("note".into(), n.into());
    }
    if let Some(w) = weight_value(auth) {
        e.insert(ATTRIBUTE_WEIGHT.into(), w.into());
    }
    if let Some(w) = bool_value(auth, "websockets") {
        e.insert("websockets".into(), w.into());
    }
    if let Some(r) = auth.request_retry_override() {
        e.insert("request_retry".into(), r.into());
    }
    Some(e)
}

// ---- models ----

/// `GET /credentials/models?name=`: models the registry holds for the credential.
pub(crate) async fn models(State(st): State<ManagementState>, req: Request) -> ApiResult {
    let name = query_get(req.uri(), "name").unwrap_or_default();
    if name.is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }
    let auth_id = st
        .registry
        .list()
        .into_iter()
        .find(|a| a.file_name == name || a.id == name)
        .map(|a| a.id)
        .unwrap_or(name);
    let models: Vec<Value> = cpa_core::registry::global_registry()
        .get_models_for_client(&auth_id)
        .into_iter()
        .map(|m| {
            let mut entry = Map::new();
            entry.insert("id".into(), m.id.into());
            if !m.display_name.is_empty() {
                entry.insert("display_name".into(), m.display_name.into());
            }
            if !m.r#type.is_empty() {
                entry.insert("type".into(), m.r#type.into());
            }
            if !m.owned_by.is_empty() {
                entry.insert("owned_by".into(), m.owned_by.into());
            }
            // Go builds each entry as a `gin.H`, which marshals with sorted keys.
            crate::http::sort_top(Value::Object(entry))
        })
        .collect();
    Ok(ok_json(&json!({"models": models})))
}

// ---- download ----

/// `GET /credentials/download?name=x.json`.
pub(crate) async fn download(State(st): State<ManagementState>, req: Request) -> ApiResult {
    let name = query_trim(req.uri(), "name");
    if is_unsafe_name(&name) {
        return Err(ApiError::bad_request("invalid name"));
    }
    if !has_json_suffix(&name) {
        return Err(ApiError::bad_request("name must end with .json"));
    }
    let dir = st
        .auth_dir()
        .ok_or_else(|| ApiError::new(500, "auth directory not configured"))?;
    let data = match tokio::fs::read(dir.join(&name)).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::new(404, "file not found"));
        }
        Err(e) => return Err(ApiError::new(500, format!("failed to read file: {e}"))),
    };
    let mut resp = Response::new(axum::body::Body::from(data));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

// ---- upload ----

struct UploadedFile {
    filename: String,
    data: Bytes,
}

/// All file parts of a multipart body, grouped by field name in sorted order (each field keeps
/// its part order).
async fn multipart_files(req: Request) -> Result<Vec<UploadedFile>, String> {
    let mut multipart = Multipart::from_request(req, &())
        .await
        .map_err(|e| e.to_string())?;
    let mut by_field: BTreeMap<String, Vec<UploadedFile>> = BTreeMap::new();
    while let Some(field) = multipart.next_field().await.map_err(|e| e.to_string())? {
        let Some(filename) = field.file_name().map(str::to_string) else {
            continue;
        };
        let key = field.name().unwrap_or("").to_string();
        let data = field.bytes().await.map_err(|e| e.to_string())?;
        by_field
            .entry(key)
            .or_default()
            .push(UploadedFile { filename, data });
    }
    Ok(by_field.into_values().flatten().collect())
}

/// `POST /credentials`: multipart `.json` files, or a raw JSON body with `?name=x.json`.
pub(crate) async fn upload(State(st): State<ManagementState>, req: Request) -> ApiResult {
    let is_multipart = content_type_is(req.headers(), "multipart/form-data");
    let uri = req.uri().clone();
    let files = if is_multipart {
        multipart_files(req)
            .await
            .map_err(|e| ApiError::bad_request(format!("invalid multipart form: {e}")))?
    } else {
        let body = read_body(req.into_body())
            .await
            .map_err(|_| ApiError::bad_request("failed to read body"));
        return detached(async move { upload_raw(&st, &uri, body).await }).await;
    };
    detached(async move { upload_files(&st, files).await }).await
}

async fn upload_files(st: &ManagementState, files: Vec<UploadedFile>) -> ApiResult {
    match files.len() {
        0 => Err(ApiError::bad_request("no files uploaded")),
        1 => match store_uploaded(st, &files[0]).await {
            Ok(_) => Ok(ok_json(&json!({"status": "ok"}))),
            Err(UploadError::NotJson) => Err(ApiError::bad_request("file must be .json")),
            Err(UploadError::Other(msg)) => Err(ApiError::new(500, msg)),
        },
        _ => {
            let mut uploaded = Vec::new();
            let mut failed = Vec::new();
            for file in &files {
                match store_uploaded(st, file).await {
                    Ok(name) => uploaded.push(name),
                    Err(e) => {
                        let msg = match e {
                            UploadError::NotJson => "file must be .json".to_string(),
                            UploadError::Other(m) => m,
                        };
                        failed.push(json!({"name": base_name(&file.filename), "error": msg}));
                    }
                }
            }
            if failed.is_empty() {
                Ok(ok_json(
                    &json!({"status": "ok", "uploaded": uploaded.len(), "files": uploaded}),
                ))
            } else {
                Ok(json_response(
                    207,
                    &json!({"status": "partial", "uploaded": uploaded.len(), "files": uploaded, "failed": failed}),
                ))
            }
        }
    }
}

async fn upload_raw(st: &ManagementState, uri: &Uri, body: ApiResult<Bytes>) -> ApiResult {
    let name = query_trim(uri, "name");
    if is_unsafe_name(&name) {
        return Err(ApiError::bad_request("invalid name"));
    }
    if !has_json_suffix(&name) {
        return Err(ApiError::bad_request("name must end with .json"));
    }
    let data = body?;
    write_auth_file(st, &base_name(&name), &data)
        .await
        .map_err(|e| ApiError::new(500, e))?;
    Ok(ok_json(&json!({"status": "ok"})))
}

enum UploadError {
    NotJson,
    Other(String),
}

async fn store_uploaded(st: &ManagementState, file: &UploadedFile) -> Result<String, UploadError> {
    let name = base_name(&file.filename);
    if !has_json_suffix(&name) {
        return Err(UploadError::NotJson);
    }
    write_auth_file(st, &name, &file.data)
        .await
        .map_err(UploadError::Other)?;
    Ok(name)
}

/// `0600` file write that keeps the mode of an existing file.
fn write_private_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(data)
}

/// `writeAuthFile`: validate, write `auth-dir/<name>` and register the credential.
async fn write_auth_file(st: &ManagementState, name: &str, data: &[u8]) -> Result<(), String> {
    let dir = st
        .auth_dir()
        .ok_or_else(|| "auth directory not configured".to_string())?;
    let dir = abs_path(&dir);
    let dst = dir.join(name);
    let metadata = parse_auth_metadata(data)?;
    let (store, bytes) = (st.store.clone(), data.to_vec());
    let (dir_w, dst_w) = (dir.clone(), dst.clone());
    let read = tokio::task::spawn_blocking(move || -> Result<Option<Auth>, String> {
        // Create the directory so a fresh install can take its first upload.
        std::fs::create_dir_all(&dir_w).map_err(|e| format!("failed to write file: {e}"))?;
        write_private_file(&dst_w, &bytes).map_err(|e| format!("failed to write file: {e}"))?;
        Ok(store.read_auth_file(&dst_w, &dir_w).ok().flatten())
    })
    .await
    .map_err(|e| format!("failed to write file: {e}"))??;
    let auth = build_auth_from_file(st, &dst, &dir, read, metadata);
    upsert_auth(st, auth).await
}

fn parse_auth_metadata(data: &[u8]) -> Result<Metadata, String> {
    crate::go_json::check_valid(data).map_err(|e| format!("invalid auth file: {e}"))?;
    let parsed: Value =
        serde_json::from_slice(data).map_err(|e| format!("invalid auth file: {e}"))?;
    let mut metadata = match parsed {
        Value::Object(m) => m,
        // `json.Unmarshal` of `null` into a map leaves it nil without an error.
        Value::Null => Metadata::new(),
        other => {
            let kind = match other {
                Value::Array(_) => "array",
                Value::String(_) => "string",
                Value::Number(_) => "number",
                _ => "bool",
            };
            return Err(format!(
                "invalid auth file: json: cannot unmarshal {kind} into Go value of type map[string]interface {{}}"
            ));
        }
    };
    credmeta::normalize_credential_metadata(&mut metadata);
    credmeta::validate_metadata_weight(&metadata).map_err(|e| format!("invalid auth file: {e}"))?;
    Ok(metadata)
}

/// `extractLastRefreshTimestamp`.
fn last_refresh_timestamp(meta: &Metadata) -> Option<DateTime<Utc>> {
    for key in [
        "last_refresh",
        "lastRefresh",
        "last_refreshed_at",
        "lastRefreshedAt",
    ] {
        let Some(v) = meta.get(key) else { continue };
        let ts = match v {
            Value::String(s) => {
                let s = s.trim();
                DateTime::parse_from_rfc3339(s)
                    .map(|t| t.with_timezone(&Utc))
                    .ok()
                    .or_else(|| {
                        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                            .ok()
                            .map(|t| t.and_utc())
                    })
                    .or_else(|| {
                        s.parse::<i64>()
                            .ok()
                            .filter(|u| *u > 0)
                            .and_then(|u| DateTime::from_timestamp(u, 0))
                    })
            }
            Value::Number(n) => n
                .as_i64()
                .or_else(|| n.as_f64().map(|f| f as i64))
                .filter(|u| *u > 0)
                .and_then(|u| DateTime::from_timestamp(u, 0)),
            _ => None,
        };
        if ts.is_some() {
            return ts;
        }
    }
    None
}

/// `buildAuthFromFileData` for a file that was just written.
fn build_auth_from_file(
    st: &ManagementState,
    path: &Path,
    dir: &Path,
    read: Option<Auth>,
    metadata: Metadata,
) -> Auth {
    let mut auth = match read {
        Some(a) => a,
        None => {
            let provider = metadata
                .get("type")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown")
                .to_string();
            let label = metadata
                .get("email")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(&provider)
                .to_string();
            let mut a = Auth::default();
            a.provider = provider;
            a.label = label;
            a.status = Status::Active;
            let p = path.to_string_lossy().into_owned();
            a.attributes.insert(ATTRIBUTE_PATH.into(), p.clone());
            a.attributes.insert("source".into(), p);
            a.metadata = metadata.clone();
            let now = Utc::now();
            a.created_at = Some(now);
            a.updated_at = Some(now);
            credmeta::apply_custom_headers_from_metadata(&mut a);
            a
        }
    };
    auth.id = cpa_auth::store::id_for(path, dir);
    auth.file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let last_refresh = last_refresh_timestamp(&metadata);
    if let Some(t) = last_refresh {
        auth.last_refreshed_at = Some(t);
    }
    if let Some(existing) = st.registry.get(&auth.id) {
        auth.created_at = existing.created_at;
        if last_refresh.is_none() {
            auth.last_refreshed_at = existing.last_refreshed_at;
        }
        auth.next_refresh_after = existing.next_refresh_after;
        auth.runtime = existing.runtime;
    }
    auth
}

async fn upsert_auth(st: &ManagementState, mut auth: Auth) -> Result<(), String> {
    if let Some(existing) = st.registry.get(&auth.id) {
        auth.created_at = existing.created_at;
    }
    st.registry.update(auth).await.map(|_| ())
}

// ---- delete ----

/// Names to delete: repeated `?name=`, else a JSON body `{"name"}`, `{"names"}` or `["a","b"]`.
fn requested_delete_names(uri: &Uri, body: &[u8]) -> Result<Vec<String>, String> {
    let from_query: Vec<String> = query_pairs(uri)
        .into_iter()
        .filter(|(k, _)| k == "name")
        .map(|(_, v)| v)
        .collect();
    let names = unique_names(from_query);
    if !names.is_empty() {
        return Ok(names);
    }
    let body = body.trim_ascii();
    if body.is_empty() {
        return Ok(Vec::new());
    }
    if body[0] == b'[' {
        let list: Vec<String> =
            serde_json::from_slice(body).map_err(|_| "invalid request body".to_string())?;
        return Ok(unique_names(list));
    }
    #[derive(serde::Deserialize, Default)]
    struct Body {
        #[serde(default)]
        name: String,
        #[serde(default)]
        names: Vec<String>,
    }
    let parsed: Body =
        serde_json::from_slice(body).map_err(|_| "invalid request body".to_string())?;
    let mut out = Vec::new();
    if !parsed.name.trim().is_empty() {
        out.push(parsed.name);
    }
    out.extend(parsed.names);
    Ok(unique_names(out))
}

fn unique_names(names: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    names
        .into_iter()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty() && seen.insert(n.clone()))
        .collect()
}

/// `DELETE /credentials`: `?all=true|1|*`, or one or more names.
pub(crate) async fn delete(State(st): State<ManagementState>, req: Request) -> ApiResult {
    let uri = req.uri().clone();
    let body = read_body(req.into_body())
        .await
        .map_err(|_| ApiError::bad_request("failed to read body"));
    detached(delete_inner(st, uri, body)).await
}

async fn delete_inner(st: ManagementState, uri: Uri, body: ApiResult<Bytes>) -> ApiResult {
    let st = &st;
    let dir = st
        .auth_dir()
        .ok_or_else(|| ApiError::new(500, "auth directory not configured"))?;

    if matches!(query_get(&uri, "all").as_deref(), Some("true" | "1" | "*")) {
        let dir_scan = dir.clone();
        let removed = blocking(move || {
            let entries = std::fs::read_dir(&dir_scan)
                .map_err(|e| ApiError::new(500, format!("failed to read auth dir: {e}")))?;
            let mut removed = Vec::new();
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type().is_ok_and(|t| t.is_dir()) || !has_json_suffix(&name) {
                    continue;
                }
                let full = abs_path(&dir_scan.join(&name));
                if std::fs::remove_file(&full).is_ok() {
                    removed.push(full);
                }
            }
            Ok(removed)
        })
        .await?;
        let deleted = removed.len();
        for full in removed {
            remove_auth(st, &full.to_string_lossy()).await;
        }
        return Ok(ok_json(&json!({"status": "ok", "deleted": deleted})));
    }

    let body = body?;
    let names = requested_delete_names(&uri, &body).map_err(ApiError::bad_request)?;
    match names.len() {
        0 => Err(ApiError::bad_request("invalid name")),
        1 => match delete_by_name(st, &dir, &names[0]).await {
            Ok(_) => Ok(ok_json(&json!({"status": "ok"}))),
            Err((status, msg)) => Err(ApiError::new(status, msg)),
        },
        _ => {
            let mut deleted = Vec::new();
            let mut failed = Vec::new();
            for name in &names {
                match delete_by_name(st, &dir, name).await {
                    Ok(n) => deleted.push(n),
                    Err((_, msg)) => failed.push(json!({"name": name, "error": msg})),
                }
            }
            if failed.is_empty() {
                Ok(ok_json(
                    &json!({"status": "ok", "deleted": deleted.len(), "files": deleted}),
                ))
            } else {
                Ok(json_response(
                    207,
                    &json!({"status": "partial", "deleted": deleted.len(), "files": deleted, "failed": failed}),
                ))
            }
        }
    }
}

/// `isPluginVirtualSourceDelete`: non-virtual auths and the source file of a virtual group.
pub(crate) fn is_plugin_virtual_source_delete(name: &str, auth: &Auth) -> bool {
    if !auth.is_plugin_virtual() {
        return true;
    }
    let mut source = attribute(auth, ATTRIBUTE_VIRTUAL_SOURCE).trim().to_string();
    if source.is_empty() {
        source = attribute(auth, ATTRIBUTE_PATH).trim().to_string();
    }
    if source.is_empty() {
        return false;
    }
    base_name(name).eq_ignore_ascii_case(&base_name(&source))
}

fn find_auth_for_delete(st: &ManagementState, name: &str) -> Option<Auth> {
    let name = name.trim();
    if let Some(a) = st.registry.get(name) {
        return Some(a);
    }
    st.registry
        .list()
        .into_iter()
        .find(|a| a.file_name.trim() == name || base_name(&attribute(a, ATTRIBUTE_PATH)) == name)
}

pub(crate) fn same_path(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    !a.is_empty() && !b.is_empty() && abs_path(Path::new(a)) == abs_path(Path::new(b))
}

/// `authIDForPath`: path relative to the auth dir.
fn auth_id_for_path(st: &ManagementState, path: &str) -> String {
    let path = abs_path(Path::new(path));
    match st.auth_dir() {
        Some(dir) => cpa_auth::store::id_for(&path, &abs_path(&dir)),
        None => path.to_string_lossy().into_owned(),
    }
}

/// `removeAuth`: by id, else by the id derived from a path.
async fn remove_auth(st: &ManagementState, id: &str) {
    let id = id.trim();
    if id.is_empty() {
        return;
    }
    if st.registry.get(id).is_some() {
        st.registry.remove(id).await;
        return;
    }
    let derived = auth_id_for_path(st, id);
    if !derived.is_empty() {
        st.registry.remove(&derived).await;
    }
}

async fn remove_auths_for_path(st: &ManagementState, path: &str, fallback_id: &str) {
    let mut removed = false;
    for a in st.registry.list() {
        if same_path(&attribute(&a, ATTRIBUTE_PATH), path)
            || same_path(&attribute(&a, ATTRIBUTE_VIRTUAL_SOURCE), path)
        {
            remove_auth(st, &a.id).await;
            removed = true;
        }
    }
    if removed {
        return;
    }
    if fallback_id.trim().is_empty() {
        remove_auth(st, path).await;
    } else {
        remove_auth(st, fallback_id).await;
    }
}

/// `deleteAuthFileByName`: `(deleted file name)` or `(status, message)`.
async fn delete_by_name(
    st: &ManagementState,
    dir: &Path,
    name: &str,
) -> Result<String, (u16, String)> {
    let name = name.trim();
    if is_unsafe_name(name) {
        return Err((400, "invalid name".into()));
    }
    let base = base_name(name);
    let mut target = dir.join(&base);
    let mut target_id = String::new();
    if let Some(auth) = find_auth_for_delete(st, name) {
        if !is_plugin_virtual_source_delete(name, &auth) {
            return Err((409, ERR_PLUGIN_VIRTUAL.into()));
        }
        target_id = auth.id.trim().to_string();
        let path = attribute(&auth, ATTRIBUTE_PATH);
        if !path.trim().is_empty() {
            target = PathBuf::from(path.trim());
        }
    }
    let target = abs_path(&target);
    if let Err(e) = tokio::fs::remove_file(&target).await {
        return Err(if e.kind() == std::io::ErrorKind::NotFound {
            (404, ERR_NOT_FOUND.into())
        } else {
            (500, format!("failed to remove file: {e}"))
        });
    }
    remove_auths_for_path(st, &target.to_string_lossy(), &target_id).await;
    Ok(base)
}
