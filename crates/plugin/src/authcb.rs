//! `host.auth.*` callbacks (Go: `auth_callbacks.go`): credential listing, physical JSON access and
//! saving auth files on behalf of plugins.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use cpa_auth::credmeta::{apply_custom_headers_from_metadata, normalize_credential_metadata, validate_auth_weight};
use cpa_auth::{Auth, Status};
use cpa_pluginapi::api::{
    HostAuthFileEntry, HostAuthGetRequest, HostAuthGetResponse, HostAuthGetRuntimeResponse, HostAuthSaveRequest,
    HostAuthSaveResponse, HostRecentRequestEntry,
};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::callbacks::{decode, marshal_result};
use crate::convert::{
    auth_attribute, is_runtime_only_auth, parse_bool_value, parse_go_bool, parse_priority_value, status_str,
};
use crate::error::HostError;
use crate::host::Host;

#[derive(Serialize)]
struct HostAuthListResponse {
    files: Vec<HostAuthFileEntry>,
}

fn trimmed(raw: &[u8]) -> &[u8] {
    let start = raw.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(raw.len());
    let end = raw.iter().rposition(|b| !b.is_ascii_whitespace()).map_or(start, |i| i + 1);
    &raw[start..end]
}

fn to_time(t: SystemTime) -> Option<DateTime<Utc>> {
    Some(DateTime::<Utc>::from(t))
}

impl Host {
    pub(crate) fn cb_auth_list(self: &Arc<Self>, req: &[u8]) -> Result<Vec<u8>, HostError> {
        if !trimmed(req).is_empty() {
            let _: Map<String, Value> = decode(req, "host auth list request")?;
        }
        let files = self.list_auth_files()?;
        marshal_result(&HostAuthListResponse { files })
    }

    pub(crate) fn cb_auth_get(self: &Arc<Self>, req: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: HostAuthGetRequest = decode(req, "host auth get request")?;
        let auth_index = req.auth_index.trim().to_string();
        if auth_index.is_empty() {
            return Err(HostError::msg("auth_index is required"));
        }
        let (auth, raw_json) = self.auth_physical_json_by_index(&auth_index)?;
        let mut name = auth.file_name.trim().to_string();
        if name.is_empty() {
            name = auth.id.trim().to_string();
        }
        let path = auth_attribute(&auth, "path").trim().to_string();
        let json = serde_json::value::RawValue::from_string(String::from_utf8_lossy(&raw_json).into_owned())
            .map_err(|e| HostError::msg(e.to_string()))?;
        marshal_result(&HostAuthGetResponse { auth_index, name, path, json: Some(json) })
    }

    pub(crate) fn cb_auth_get_runtime(self: &Arc<Self>, req: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: HostAuthGetRequest = decode(req, "host auth get runtime request")?;
        let auth_index = req.auth_index.trim().to_string();
        if auth_index.is_empty() {
            return Err(HostError::msg("auth_index is required"));
        }
        let auth = self.auth_by_index(&auth_index)?;
        let Some(entry) = self.build_host_auth_file_entry(auth) else {
            return Err(HostError::msg(format!("auth runtime info not found for auth_index {auth_index}")));
        };
        marshal_result(&HostAuthGetRuntimeResponse { auth: entry })
    }

    pub(crate) async fn cb_auth_save(self: &Arc<Self>, req: &[u8]) -> Result<Vec<u8>, HostError> {
        let req: HostAuthSaveRequest = decode(req, "host auth save request")?;
        let (name, raw) = validate_host_auth_save_request(&req)?;
        let path = self.save_auth_file(&name, &raw).await?;
        marshal_result(&HostAuthSaveResponse { name, path: path.to_string_lossy().into_owned() })
    }

    fn list_auth_files(&self) -> Result<Vec<HostAuthFileEntry>, HostError> {
        if let Some(manager) = self.auth_manager() {
            let mut entries: Vec<HostAuthFileEntry> = manager.list().into_iter().filter_map(|a| self.build_host_auth_file_entry(a)).collect();
            entries.sort_by_key(|e| e.name.to_lowercase());
            return Ok(entries);
        }
        self.list_auth_files_from_disk()
    }

    fn list_auth_files_from_disk(&self) -> Result<Vec<HostAuthFileEntry>, HostError> {
        let dir = self.resolved_auth_dir();
        if dir.as_os_str().is_empty() {
            return Err(HostError::msg("auth directory is unavailable"));
        }
        let read = std::fs::read_dir(&dir).map_err(|e| HostError::msg(format!("failed to read auth dir: {e}")))?;
        let mut files = Vec::new();
        for entry in read.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.to_lowercase().ends_with(".json") {
                continue;
            }
            let full = dir.join(&name);
            let mut fe = HostAuthFileEntry {
                name: name.clone(),
                source: "file".into(),
                path: full.to_string_lossy().into_owned(),
                ..Default::default()
            };
            if let Ok(info) = entry.metadata() {
                fe.size = info.len() as i64;
                fe.mod_time = info.modified().ok().and_then(to_time);
            }
            if let Ok(data) = std::fs::read(&full)
                && let Ok(Value::Object(meta)) = serde_json::from_slice::<Value>(&data)
            {
                if let Some(Value::String(p)) = meta.get("type") {
                    fe.kind = p.trim().to_string();
                    fe.provider = fe.kind.clone();
                }
                if let Some(Value::String(v)) = meta.get("email") {
                    fe.email = v.trim().to_string();
                }
                if let Some(Value::String(v)) = meta.get("project_id") {
                    fe.project_id = v.trim().to_string();
                }
                if let Some(p) = meta.get("priority").and_then(parse_priority_value) {
                    fe.priority = p;
                }
                if let Some(Value::String(v)) = meta.get("note") {
                    fe.note = v.trim().to_string();
                }
                if let Some(Value::String(v)) = meta.get("base_url") {
                    fe.base_url = v.trim().to_string();
                }
                if let Some(ws) = parse_bool_value(meta.get("websockets")) {
                    fe.websockets = ws;
                }
                if parse_bool_value(meta.get("disabled")) == Some(true) {
                    fe.disabled = true;
                    fe.status = status_str(Status::Disabled).into();
                } else {
                    fe.status = status_str(Status::Active).into();
                }
            }
            files.push(fe);
        }
        files.sort_by_key(|e| e.name.to_lowercase());
        Ok(files)
    }

    pub(crate) fn auth_by_index(&self, auth_index: &str) -> Result<Auth, HostError> {
        let auth_index = auth_index.trim();
        if auth_index.is_empty() {
            return Err(HostError::msg("auth_index is required"));
        }
        let Some(manager) = self.auth_manager() else {
            return Err(HostError::msg("core auth manager unavailable"));
        };
        for mut auth in manager.list() {
            auth.ensure_index();
            if auth.index == auth_index {
                return Ok(auth);
            }
        }
        Err(HostError::msg(format!("auth not found for auth_index {auth_index}")))
    }

    /// Go `authPhysicalJSONByIndex`: the auth plus the bytes of its backing file.
    pub(crate) fn auth_physical_json_by_index(&self, auth_index: &str) -> Result<(Auth, Vec<u8>), HostError> {
        let auth = self.auth_by_index(auth_index)?;
        let path = auth_attribute(&auth, "path").trim().to_string();
        if path.is_empty() {
            return Err(HostError::msg(format!("auth file path not found for auth_index {auth_index}")));
        }
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(HostError::msg(format!("auth file not found for auth_index {auth_index}")));
            }
            Err(e) => return Err(HostError::msg(format!("failed to read auth file: {e}"))),
        };
        if trimmed(&data).is_empty() {
            return Err(HostError::msg(format!("auth file is empty for auth_index {auth_index}")));
        }
        if let Err(e) = serde_json::from_slice::<Map<String, Value>>(&data) {
            return Err(HostError::msg(format!("invalid auth file for auth_index {auth_index}: {e}")));
        }
        Ok((auth, data))
    }

    async fn save_auth_file(&self, name: &str, data: &[u8]) -> Result<PathBuf, HostError> {
        let dir = self.resolved_auth_dir();
        if dir.as_os_str().is_empty() {
            return Err(HostError::msg("auth directory is unavailable"));
        }
        let base = Path::new(name).file_name().map(|b| b.to_os_string()).unwrap_or_default();
        let mut dst = dir.join(base);
        if !dst.is_absolute()
            && let Ok(cwd) = std::env::current_dir()
        {
            dst = cwd.join(dst);
        }
        let auth = self.build_auth_from_file_data(&dst, Some(data))?;
        write_private_file(&dst, data).map_err(|e| HostError::msg(format!("failed to write auth file: {e}")))?;
        self.upsert_auth_record(auth).await?;
        Ok(dst)
    }

    /// Go `buildAuthFromFileData`.
    pub(crate) fn build_auth_from_file_data(&self, path: &Path, data: Option<&[u8]>) -> Result<Auth, HostError> {
        if path.as_os_str().is_empty() {
            return Err(HostError::msg("auth path is empty"));
        }
        let bytes = match data {
            Some(d) => d.to_vec(),
            None => std::fs::read(path).map_err(|e| HostError::msg(format!("failed to read auth file: {e}")))?,
        };
        let mut metadata: Map<String, Value> =
            serde_json::from_slice(&bytes).map_err(|e| HostError::msg(format!("invalid auth file: {e}")))?;
        normalize_credential_metadata(&mut metadata);
        let provider = match metadata.get("type") {
            Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
            _ => "unknown".to_string(),
        };
        let mut label = provider.clone();
        if let Some(Value::String(email)) = metadata.get("email")
            && !email.trim().is_empty()
        {
            label = email.trim().to_string();
        }
        let mut auth_id = self.auth_id_for_path(path);
        if auth_id.is_empty() {
            auth_id = path.to_string_lossy().into_owned();
        }
        let disabled = parse_bool_value(metadata.get("disabled")) == Some(true);
        let status = if disabled { Status::Disabled } else { Status::Active };
        let now = Utc::now();
        let path_str = path.to_string_lossy().into_owned();
        let mut auth = Auth::default();
        auth.id = auth_id.clone();
        auth.provider = provider;
        auth.file_name = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        auth.label = label;
        auth.status = status;
        auth.disabled = disabled;
        auth.metadata = metadata;
        auth.created_at = Some(now);
        auth.updated_at = Some(now);
        auth.attributes.insert("path".into(), path_str.clone());
        auth.attributes.insert("source".into(), path_str);
        if let Some(manager) = self.auth_manager()
            && let Some(existing) = manager.get(&auth_id)
        {
            auth.created_at = existing.created_at;
            auth.last_refreshed_at = existing.last_refreshed_at;
            auth.next_retry_after = existing.next_retry_after;
            auth.runtime = existing.runtime.clone();
        }
        if let Err(e) = validate_auth_weight(&auth) {
            return Err(HostError::msg(format!("invalid auth weight: {e}")));
        }
        apply_custom_headers_from_metadata(&mut auth);
        Ok(auth)
    }

    async fn upsert_auth_record(&self, mut auth: Auth) -> Result<(), HostError> {
        let Some(manager) = self.auth_manager() else { return Ok(()) };
        if let Some(existing) = manager.get(&auth.id) {
            auth.created_at = existing.created_at;
            manager.update(auth).await.map(|_| ()).map_err(|e| HostError::msg(e.to_string()))
        } else {
            manager.register(auth).await.map(|_| ()).map_err(|e| HostError::msg(e.to_string()))
        }
    }

    /// Go `buildHostAuthFileEntry`; `None` for credentials that are not exposed to plugins.
    pub(crate) fn build_host_auth_file_entry(&self, mut auth: Auth) -> Option<HostAuthFileEntry> {
        auth.ensure_index();
        let runtime_only = is_runtime_only_auth(&auth);
        if runtime_only && (auth.disabled || auth.status == Status::Disabled) {
            return None;
        }
        let path = auth_attribute(&auth, "path").trim().to_string();
        if path.is_empty() && !runtime_only {
            return None;
        }
        let mut name = auth.file_name.trim().to_string();
        if name.is_empty() {
            name = auth.id.clone();
        }
        let provider = auth.provider.trim().to_string();
        let mut entry = HostAuthFileEntry {
            id: auth.id.clone(),
            auth_index: auth.index.clone(),
            name,
            kind: provider.clone(),
            provider,
            label: auth.label.clone(),
            status: status_str(auth.status).into(),
            status_message: auth.status_message.clone(),
            disabled: auth.disabled,
            unavailable: auth.unavailable,
            runtime_only,
            source: "memory".into(),
            success: auth.success,
            failed: auth.failed,
            recent_requests: host_recent_requests(&auth),
            ..Default::default()
        };
        let email = auth_email(&auth);
        if !email.is_empty() {
            entry.email = email;
        }
        let project = auth_project_id(&auth);
        if !project.is_empty() {
            entry.project_id = project;
        }
        let (account_type, account) = auth.account_info();
        if !account_type.is_empty() || !account.is_empty() {
            entry.account_type = account_type.into();
            entry.account = account;
        }
        entry.created_at = auth.created_at;
        if auth.updated_at.is_some() {
            entry.mod_time = auth.updated_at;
            entry.updated_at = auth.updated_at;
        }
        entry.last_refresh = auth.last_refreshed_at;
        entry.next_retry_after = auth.next_retry_after;
        if !path.is_empty() {
            entry.path = path.clone();
            entry.source = "file".into();
            match std::fs::metadata(&path) {
                Ok(info) => {
                    entry.size = info.len() as i64;
                    entry.mod_time = info.modified().ok().and_then(to_time);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if !runtime_only
                        && (auth.disabled
                            || auth.status == Status::Disabled
                            || auth.status_message.trim().eq_ignore_ascii_case("removed via management api"))
                    {
                        return None;
                    }
                    entry.source = "memory".into();
                }
                Err(_) => {}
            }
        }
        let attr_priority = auth_attribute(&auth, "priority").trim().to_string();
        if !attr_priority.is_empty() {
            if let Ok(p) = attr_priority.parse::<i64>() {
                entry.priority = p;
            }
        } else if let Some(p) = auth.metadata.get("priority").and_then(parse_priority_value) {
            entry.priority = p;
        }
        let note = auth_attribute(&auth, "note").trim().to_string();
        if !note.is_empty() {
            entry.note = note;
        } else if let Some(Value::String(n)) = auth.metadata.get("note") {
            entry.note = n.trim().to_string();
        }
        let base = auth_attribute(&auth, "base_url").trim().to_string();
        if !base.is_empty() {
            entry.base_url = base;
        } else if let Some(Value::String(b)) = auth.metadata.get("base_url") {
            entry.base_url = b.trim().to_string();
        }
        if let Some(ws) = auth_websockets_value(&auth) {
            entry.websockets = ws;
        }
        Some(entry)
    }

    /// Go `resolvedAuthDir`: the configured auth dir made absolute.
    pub(crate) fn resolved_auth_dir(&self) -> PathBuf {
        let Some(cfg) = self.runtime_config() else { return PathBuf::new() };
        let dir = cfg.auth_dir.trim();
        if dir.is_empty() {
            return PathBuf::new();
        }
        let cleaned = crate::platform::clean_path(Path::new(dir));
        if cleaned.is_absolute() {
            cleaned
        } else {
            std::env::current_dir().map(|c| c.join(&cleaned)).unwrap_or(cleaned)
        }
    }

    /// Go `(*Host).authIDForPath`.
    fn auth_id_for_path(&self, path: &Path) -> String {
        if path.as_os_str().is_empty() {
            return String::new();
        }
        let mut path = crate::platform::clean_path(path);
        if !path.is_absolute()
            && let Ok(cwd) = std::env::current_dir()
        {
            path = cwd.join(path);
        }
        let mut id = path.to_string_lossy().into_owned();
        let dir = self.resolved_auth_dir();
        if !dir.as_os_str().is_empty()
            && let Some(rel) = crate::convert::relative_path(&dir.to_string_lossy(), &id)
            && !rel.is_empty()
        {
            id = rel;
        }
        id
    }
}

fn write_private_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(data)
}

/// Go `validateHostAuthSaveRequest`: file name and trimmed JSON object.
fn validate_host_auth_save_request(req: &HostAuthSaveRequest) -> Result<(String, Vec<u8>), HostError> {
    let name = req.name.trim().to_string();
    if is_unsafe_auth_file_name(&name) {
        return Err(HostError::msg("invalid auth file name"));
    }
    if !name.to_lowercase().ends_with(".json") {
        return Err(HostError::msg("auth file name must end with .json"));
    }
    let raw = req.json.as_ref().map(|r| r.get().as_bytes().to_vec()).unwrap_or_default();
    let raw = trimmed(&raw).to_vec();
    if raw.is_empty() {
        return Err(HostError::msg("json is required"));
    }
    if let Err(e) = serde_json::from_slice::<Map<String, Value>>(&raw) {
        return Err(HostError::msg(format!("invalid auth json: {e}")));
    }
    let base = Path::new(&name).file_name().map(|b| b.to_string_lossy().into_owned()).unwrap_or(name);
    Ok((base, raw))
}

fn is_unsafe_auth_file_name(name: &str) -> bool {
    name.trim().is_empty() || name.contains('/') || name.contains('\\') || {
        // Windows volume names ("C:").
        let b = name.as_bytes();
        b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()
    }
}

fn auth_email(auth: &Auth) -> String {
    if let Some(Value::String(v)) = auth.metadata.get("email") {
        return v.trim().to_string();
    }
    for key in ["email", "account_email"] {
        let v = auth.attributes.get(key).map(|v| v.trim()).unwrap_or("");
        if !v.is_empty() {
            return v.to_string();
        }
    }
    String::new()
}

fn auth_project_id(auth: &Auth) -> String {
    if let Some(Value::String(v)) = auth.metadata.get("project_id")
        && !v.trim().is_empty()
    {
        return v.trim().to_string();
    }
    auth.attributes.get("project_id").map(|v| v.trim().to_string()).unwrap_or_default()
}

fn auth_websockets_value(auth: &Auth) -> Option<bool> {
    if let Some(raw) = auth.attributes.get("websockets") {
        let raw = raw.trim();
        if !raw.is_empty()
            && let Some(b) = parse_go_bool(raw)
        {
            return Some(b);
        }
    }
    parse_bool_value(auth.metadata.get("websockets"))
}

fn host_recent_requests(auth: &Auth) -> Vec<HostRecentRequestEntry> {
    auth.recent_requests_snapshot(Utc::now())
        .into_iter()
        .map(|b| HostRecentRequestEntry { time: b.time, success: b.success, failed: b.failed })
        .collect()
}
