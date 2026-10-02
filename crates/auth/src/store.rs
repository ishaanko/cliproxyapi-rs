//! Credential persistence: the `Store` trait (sdk/cliproxy/auth/store.go) and `FileTokenStore`
//! (sdk/auth/filestore.go). Files are `*.json` anywhere under the auth dir; the auth id is the path
//! relative to that dir, so an auth dir written by the Go app loads unchanged.
//!
//! Not ported: the plugin auth parser hook (plugins are out of scope for this crate) and the git /
//! postgres / object stores.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;

use crate::credmeta::{
    Metadata, apply_custom_headers_from_metadata, normalize_credential_metadata, validate_auth_weight,
    validate_metadata_weight,
};
use crate::storage::{StorageError, mkdir_all_private, write_file_in_place};
use crate::types::{
    ATTRIBUTE_PATH, ATTRIBUTE_SOURCE, ATTRIBUTE_SOURCE_BACKEND, AUTH_SOURCE_FILE, Auth, Status,
};
use crate::util::marshal_compact;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("auth filestore: {0}")]
    Invalid(String),
    #[error("auth filestore: {context}: {source}")]
    Io {
        context: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl StoreError {
    /// True when the underlying failure is "no such file or directory".
    pub fn is_not_found(&self) -> bool {
        matches!(self, StoreError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
    }
}

/// Per-save flags (Go carries these on the context).
#[derive(Debug, Clone, Copy, Default)]
pub struct SaveOptions {
    /// `WithAuthCreationIntent`: logins and migrations may create a missing disabled credential.
    pub creation_intent: bool,
}

/// Persistence for `Auth` records.
pub trait Store: Send + Sync {
    fn list(&self) -> Result<Vec<Auth>, StoreError>;
    /// Persists the auth; `Ok(None)` when the save was intentionally skipped.
    fn save(&self, auth: &mut Auth, opts: SaveOptions) -> Result<Option<PathBuf>, StoreError>;
    fn delete(&self, id: &str) -> Result<(), StoreError>;
    /// Directory the store mirrors credentials into, when it has one (watcher target).
    fn base_dir(&self) -> Option<PathBuf> {
        None
    }
}

/// Filesystem-backed store rooted at the configured `auth-dir`.
#[derive(Default)]
pub struct FileTokenStore {
    base_dir: RwLock<String>,
    write_lock: Mutex<()>,
}

impl FileTokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_dir(dir: impl AsRef<Path>) -> Self {
        let s = Self::new();
        s.set_base_dir(dir.as_ref().to_string_lossy().as_ref());
        s
    }

    /// `SetBaseDir`: default directory for auth persistence.
    pub fn set_base_dir(&self, dir: &str) {
        *self.base_dir.write() = dir.trim().to_string();
    }

    fn base_dir_snapshot(&self) -> String {
        self.base_dir.read().clone()
    }

    /// Reads one auth JSON file into an `Auth` (single-auth path of `readAuthFiles`).
    /// `Ok(None)` for empty files and legacy `type: gemini` files.
    pub fn read_auth_file(&self, path: &Path, base_dir: &Path) -> Result<Option<Auth>, StoreError> {
        let data = fs::read(path).map_err(|source| StoreError::Io { context: "read file", source })?;
        if data.is_empty() {
            return Ok(None);
        }
        let parsed: Value =
            serde_json::from_slice(&data).map_err(|e| StoreError::Invalid(format!("unmarshal auth json: {e}")))?;
        let Value::Object(mut metadata) = parsed else {
            return Err(StoreError::Invalid("unmarshal auth json: not an object".into()));
        };
        normalize_credential_metadata(&mut metadata);
        validate_metadata_weight(&metadata).map_err(StoreError::Invalid)?;

        let provider = metadata.get("type").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if provider.eq_ignore_ascii_case("gemini") {
            return Ok(None);
        }
        let mtime = fs::metadata(path)
            .and_then(|m| m.modified())
            .map_err(|source| StoreError::Io { context: "stat file", source })?;
        let mtime: DateTime<Utc> = SystemTime::into(mtime);

        let provider = if provider.is_empty() { "unknown".to_string() } else { provider };
        let id = id_for(path, base_dir);
        let disabled = metadata.get("disabled").and_then(Value::as_bool).unwrap_or(false);
        let proxy_url = metadata.get("proxy_url").and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default();
        let prefix = metadata
            .get("prefix")
            .and_then(Value::as_str)
            .map(|p| p.trim().trim_matches('/').to_string())
            .filter(|p| !p.is_empty() && !p.contains('/'))
            .unwrap_or_default();

        let path_str = path.to_string_lossy().into_owned();
        let mut auth = Auth {
            id: id.clone(),
            provider,
            file_name: id,
            label: label_for(&metadata),
            prefix,
            proxy_url,
            status: if disabled { Status::Disabled } else { Status::Active },
            disabled,
            created_at: Some(mtime),
            updated_at: Some(mtime),
            ..Default::default()
        };
        auth.attributes.insert(ATTRIBUTE_PATH.into(), path_str.clone());
        auth.attributes.insert(ATTRIBUTE_SOURCE.into(), path_str);
        auth.attributes.insert(ATTRIBUTE_SOURCE_BACKEND.into(), AUTH_SOURCE_FILE.into());
        if let Some(email) = metadata.get("email").and_then(Value::as_str).filter(|e| !e.is_empty()) {
            auth.attributes.insert("email".into(), email.to_string());
        }
        auth.metadata = metadata;
        apply_custom_headers_from_metadata(&mut auth);
        Ok(Some(auth))
    }

    fn resolve_auth_path(&self, auth: &Auth) -> Result<PathBuf, StoreError> {
        let p = auth.attr(ATTRIBUTE_PATH);
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
        let dir = self.base_dir_snapshot();
        let file_name = auth.file_name.trim();
        if !file_name.is_empty() {
            if Path::new(file_name).is_absolute() || dir.is_empty() {
                return Ok(PathBuf::from(file_name));
            }
            return Ok(Path::new(&dir).join(file_name));
        }
        if auth.id.is_empty() {
            return Err(StoreError::Invalid("missing id".into()));
        }
        if Path::new(&auth.id).is_absolute() {
            return Ok(PathBuf::from(&auth.id));
        }
        if dir.is_empty() {
            return Err(StoreError::Invalid("directory not configured".into()));
        }
        Ok(Path::new(&dir).join(&auth.id))
    }

    fn resolve_delete_path(&self, id: &str) -> Result<PathBuf, StoreError> {
        if id.contains(std::path::MAIN_SEPARATOR) || Path::new(id).is_absolute() {
            return Ok(PathBuf::from(id));
        }
        let dir = self.base_dir_snapshot();
        if dir.is_empty() {
            return Err(StoreError::Invalid("directory not configured".into()));
        }
        Ok(Path::new(&dir).join(id))
    }
}

impl Store for FileTokenStore {
    /// Walks the auth dir recursively (lexical order), skipping unreadable/invalid/empty files.
    fn list(&self) -> Result<Vec<Auth>, StoreError> {
        let dir = self.base_dir_snapshot();
        if dir.is_empty() {
            return Err(StoreError::Invalid("directory not configured".into()));
        }
        let base = PathBuf::from(&dir);
        let mut entries = Vec::new();
        walk_json_files(&base, &mut |path| {
            if let Ok(Some(auth)) = self.read_auth_file(path, &base) {
                entries.push(auth);
            }
        })?;
        Ok(entries)
    }

    fn save(&self, auth: &mut Auth, opts: SaveOptions) -> Result<Option<PathBuf>, StoreError> {
        normalize_credential_metadata(&mut auth.metadata);
        validate_auth_weight(auth).map_err(StoreError::Invalid)?;
        let path = self.resolve_auth_path(auth)?;
        if path.as_os_str().is_empty() {
            return Err(StoreError::Invalid(format!("missing file path attribute for {}", auth.id)));
        }

        // A runtime update must not resurrect a disabled credential whose file was removed.
        if auth.disabled && !opts.creation_intent && !path.exists() {
            return Ok(None);
        }

        let _guard = self.write_lock.lock();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            mkdir_all_private(dir).map_err(|source| StoreError::Io { context: "create dir failed", source })?;
        }

        if let Some(storage) = auth.storage.clone() {
            auth.metadata.insert("disabled".into(), Value::Bool(auth.disabled));
            storage.save_to_file(&path, &auth.metadata)?;
        } else if !auth.metadata.is_empty() {
            auth.metadata.insert("disabled".into(), Value::Bool(auth.disabled));
            let raw = marshal_compact(&Value::Object(auth.metadata.clone()))
                .map_err(|e| StoreError::Invalid(format!("marshal metadata failed: {e}")))?;
            write_metadata_only(&path, &raw)?;
        } else {
            return Err(StoreError::Invalid(format!("nothing to persist for {}", auth.id)));
        }

        let path_str = path.to_string_lossy().into_owned();
        auth.attributes.insert(ATTRIBUTE_PATH.into(), path_str.clone());
        auth.attributes.insert(ATTRIBUTE_SOURCE.into(), path_str);
        auth.attributes.insert(ATTRIBUTE_SOURCE_BACKEND.into(), AUTH_SOURCE_FILE.into());
        if auth.file_name.trim().is_empty() {
            auth.file_name = auth.id.clone();
        }
        Ok(Some(path))
    }

    fn delete(&self, id: &str) -> Result<(), StoreError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(StoreError::Invalid("id is empty".into()));
        }
        let path = self.resolve_delete_path(id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Io { context: "delete failed", source }),
        }
    }

    fn base_dir(&self) -> Option<PathBuf> {
        let d = self.base_dir_snapshot();
        if d.is_empty() { None } else { Some(PathBuf::from(d)) }
    }
}

/// Metadata-only write: skipped when semantically identical to the existing file.
fn write_metadata_only(path: &Path, raw: &str) -> Result<(), StoreError> {
    match fs::read(path) {
        Ok(existing) => {
            if json_equal(&existing, raw.as_bytes()) {
                return Ok(());
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(StoreError::Io { context: "read existing failed", source }),
    }
    write_file_in_place(path, raw.as_bytes())?;
    Ok(())
}

/// Semantic JSON equality (key order and number formatting insensitive).
fn json_equal(a: &[u8], b: &[u8]) -> bool {
    match (serde_json::from_slice::<Value>(a), serde_json::from_slice::<Value>(b)) {
        (Ok(a), Ok(b)) => deep_equal_json(&a, &b),
        _ => false,
    }
}

fn deep_equal_json(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| deep_equal_json(v, w)))
        }
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| deep_equal_json(p, q)),
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (x, y) => x == y,
    }
}

/// `label`, else `email`, else `project_id`.
fn label_for(metadata: &Metadata) -> String {
    for key in ["label", "email", "project_id"] {
        if let Some(v) = metadata.get(key).and_then(Value::as_str).filter(|v| !v.is_empty()) {
            return v.to_string();
        }
    }
    String::new()
}

/// Path relative to the base dir (lowercased on Windows), else the full path.
pub fn id_for(path: &Path, base_dir: &Path) -> String {
    let id = if base_dir.as_os_str().is_empty() {
        path.to_string_lossy().into_owned()
    } else {
        path.strip_prefix(base_dir)
            .ok()
            .filter(|r| !r.as_os_str().is_empty())
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned())
    };
    if cfg!(windows) { id.to_lowercase() } else { id }
}

/// Recursive walk in lexical order; symlinks are not followed into directories (Go `WalkDir`).
fn walk_json_files(dir: &Path, visit: &mut dyn FnMut(&Path)) -> Result<(), StoreError> {
    let read = fs::read_dir(dir).map_err(|source| StoreError::Io { context: "walk dir", source })?;
    let mut entries: Vec<_> = read
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| StoreError::Io { context: "walk dir", source })?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| StoreError::Io { context: "walk dir", source })?;
        if file_type.is_dir() {
            walk_json_files(&path, visit)?;
        } else if entry.file_name().to_string_lossy().to_lowercase().ends_with(".json") {
            visit(&path);
        }
    }
    Ok(())
}

/// Whether `path` looks like a credential file (case-insensitive `.json`).
pub fn is_auth_json_path(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().ends_with(".json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{ClaudeTokenStorage, TokenStorage};
    use serde_json::json;

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn lists_go_written_auth_dir() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "claude-a@b.com.json", r#"{"type":"claude","email":"a@b.com","access_token":"t","prefix":"/team/","proxy-url":"socks5://h:1","headers":{"X-A":"1"}}"#);
        write(dir.path(), "sub/codex-x.json", r#"{"type":"codex","email":"x@y","disabled":true,"weight":3}"#);
        write(dir.path(), "legacy-gemini.json", r#"{"type":"gemini","email":"g@g"}"#);
        write(dir.path(), "empty.json", "");
        write(dir.path(), "broken.json", "{not json");
        write(dir.path(), "notes.txt", "{}");
        write(dir.path(), "untyped.json", r#"{"project_id":"p1"}"#);

        let store = FileTokenStore::with_dir(dir.path());
        let mut auths = store.list().unwrap();
        auths.sort_by(|a, b| a.id.cmp(&b.id));
        let ids: Vec<&str> = auths.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["claude-a@b.com.json", "sub/codex-x.json", "untyped.json"]);

        let claude = &auths[0];
        assert_eq!(claude.provider, "claude");
        assert_eq!(claude.label, "a@b.com");
        assert_eq!(claude.prefix, "team");
        assert_eq!(claude.proxy_url, "socks5://h:1");
        assert_eq!(claude.status, Status::Active);
        assert_eq!(claude.attr("header:X-A"), "1");
        assert_eq!(claude.attr("email"), "a@b.com");
        assert_eq!(claude.attr("source_backend"), "file");
        assert!(claude.metadata.contains_key("proxy_url") && !claude.metadata.contains_key("proxy-url"));

        let codex = &auths[1];
        assert!(codex.disabled);
        assert_eq!(codex.status, Status::Disabled);

        let untyped = &auths[2];
        assert_eq!(untyped.provider, "unknown");
        assert_eq!(untyped.label, "p1");
    }

    #[test]
    fn invalid_weight_skips_file_on_list_and_rejects_on_save() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "bad.json", r#"{"type":"claude","weight":2000000}"#);
        let store = FileTokenStore::with_dir(dir.path());
        assert!(store.list().unwrap().is_empty());
        let mut a = Auth { id: "x.json".into(), ..Default::default() };
        a.metadata.insert("weight".into(), json!("nope"));
        assert!(store.save(&mut a, SaveOptions::default()).is_err());
    }

    #[test]
    fn save_with_storage_merges_metadata_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTokenStore::with_dir(dir.path());
        let mut a = Auth {
            id: "claude-e@x.json".into(),
            provider: "claude".into(),
            file_name: "claude-e@x.json".into(),
            storage: Some(TokenStorage::Claude(ClaudeTokenStorage {
                access_token: "at".into(),
                email: "e@x".into(),
                ..Default::default()
            })),
            ..Default::default()
        };
        a.metadata.insert("proxy_url".into(), json!("http://p"));
        let path = store.save(&mut a, SaveOptions::default()).unwrap().unwrap();
        assert_eq!(path, dir.path().join("claude-e@x.json"));
        assert_eq!(a.attr("path"), path.to_string_lossy());

        let loaded = store.list().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].metadata.get("access_token"), Some(&json!("at")));
        assert_eq!(loaded[0].metadata.get("proxy_url"), Some(&json!("http://p")));
        assert_eq!(loaded[0].metadata.get("disabled"), Some(&json!(false)));
    }

    #[test]
    fn metadata_only_save_skips_identical_rewrite_and_disabled_missing_is_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTokenStore::with_dir(dir.path());
        let mut a = Auth { id: "antigravity-a.json".into(), provider: "antigravity".into(), ..Default::default() };
        a.metadata.insert("type".into(), json!("antigravity"));
        a.metadata.insert("access_token".into(), json!("t"));
        let path = store.save(&mut a, SaveOptions::default()).unwrap().unwrap();
        let first = fs::read_to_string(&path).unwrap();
        assert_eq!(first, r#"{"access_token":"t","disabled":false,"type":"antigravity"}"#);

        // Same content in a different key order: file must be left untouched.
        fs::write(&path, r#"{"type":"antigravity","disabled":false,"access_token":"t"}"#).unwrap();
        store.save(&mut a, SaveOptions::default()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"type":"antigravity","disabled":false,"access_token":"t"}"#);

        // Disabled auth whose file was deleted is not recreated without creation intent.
        store.delete("antigravity-a.json").unwrap();
        a.disabled = true;
        assert!(store.save(&mut a, SaveOptions::default()).unwrap().is_none());
        assert!(!path.exists());
        assert!(store.save(&mut a, SaveOptions { creation_intent: true }).unwrap().is_some());
        assert!(path.exists());
        // Deleting a missing file is not an error.
        store.delete("nope.json").unwrap();
    }
}
