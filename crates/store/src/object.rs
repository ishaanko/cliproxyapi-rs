//! S3-compatible object-storage token store (Go: internal/store/objectstore.go).
//!
//! Config lives at `<prefix>/config/config.yaml` and credentials at `<prefix>/auths/<rel>` in the
//! bucket; both are mirrored into a local spool (`<root>/config/config.yaml`, `<root>/auths/**`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cpa_auth::credmeta::{
    apply_custom_headers_from_metadata, normalize_credential_metadata, validate_auth_weight,
};
use cpa_auth::store::{SaveOptions, Store, StoreError};
use cpa_auth::types::{ATTRIBUTE_PATH, ATTRIBUTE_SOURCE_BACKEND, AUTH_SOURCE_OBJECT_STORE, Status};
use cpa_auth::util::clean_path;
use cpa_auth::Auth;
use cpa_runtime::service::StorePersister;
use parking_lot::Mutex;
use serde_json::Value;

use crate::common::{
    Written, backend_err, copy_config_template, label_for, mkdir_all_private, normalize_auth_id,
    normalize_line_endings_bytes, rel_path, skip_disabled_recreate, stamp_saved, value_as_string, walk_json_files,
    write_auth_file, write_file_private,
};
use crate::rt;
use crate::s3::{S3Client, S3Config, S3Error};

const CONFIG_KEY: &str = "config/config.yaml";
const AUTH_PREFIX: &str = "auths";

/// Configuration for the object storage-backed token store.
#[derive(Clone, Default)]
pub struct ObjectStoreConfig {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: String,
    pub prefix: String,
    pub local_root: String,
    pub use_ssl: bool,
    pub path_style: bool,
}

impl std::fmt::Debug for ObjectStoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectStoreConfig")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .field("region", &self.region)
            .field("prefix", &self.prefix)
            .field("local_root", &self.local_root)
            .field("use_ssl", &self.use_ssl)
            .field("path_style", &self.path_style)
            .finish()
    }
}

/// Persists configuration and credentials in an S3-compatible bucket with a local mirror.
pub struct ObjectTokenStore {
    client: Arc<S3Client>,
    cfg: ObjectStoreConfig,
    config_path: PathBuf,
    auth_dir: PathBuf,
    mu: Mutex<()>,
}

fn s3_err(prefix: &str, err: &S3Error) -> StoreError {
    backend_err(format!("object store: {prefix}: {err}"))
}

impl ObjectTokenStore {
    /// `NewObjectTokenStore`.
    pub fn new(mut cfg: ObjectStoreConfig) -> Result<Self, StoreError> {
        cfg.endpoint = cfg.endpoint.trim().to_string();
        cfg.bucket = cfg.bucket.trim().to_string();
        cfg.access_key = cfg.access_key.trim().to_string();
        cfg.secret_key = cfg.secret_key.trim().to_string();
        cfg.prefix = cfg.prefix.trim_matches('/').to_string();

        if cfg.endpoint.is_empty() {
            return Err(backend_err("object store: endpoint is required"));
        }
        if cfg.bucket.is_empty() {
            return Err(backend_err("object store: bucket is required"));
        }
        if cfg.access_key.is_empty() {
            return Err(backend_err("object store: access key is required"));
        }
        if cfg.secret_key.is_empty() {
            return Err(backend_err("object store: secret key is required"));
        }

        let root = cfg.local_root.trim();
        let root = if root.is_empty() {
            match std::env::current_dir() {
                Ok(cwd) => cwd.join("objectstore"),
                Err(_) => std::env::temp_dir().join("objectstore"),
            }
        } else {
            PathBuf::from(root)
        };
        let abs_root = std::path::absolute(&root)
            .map_err(|e| backend_err(format!("object store: resolve spool directory: {e}")))?;
        let abs_root = clean_path(&abs_root);
        let config_dir = abs_root.join("config");
        let auth_dir = abs_root.join("auths");
        mkdir_all_private(&config_dir)
            .map_err(|e| backend_err(format!("object store: create config directory: {e}")))?;
        mkdir_all_private(&auth_dir)
            .map_err(|e| backend_err(format!("object store: create auth directory: {e}")))?;

        let client = S3Client::new(S3Config {
            endpoint: cfg.endpoint.clone(),
            bucket: cfg.bucket.clone(),
            access_key: cfg.access_key.clone(),
            secret_key: cfg.secret_key.clone(),
            region: cfg.region.clone(),
            use_ssl: cfg.use_ssl,
        })
        .map_err(|e| backend_err(format!("object store: create client: {e}")))?;

        Ok(Self { client, cfg, config_path: config_dir.join("config.yaml"), auth_dir, mu: Mutex::new(()) })
    }

    /// `ConfigPath`: the managed config file inside the spool.
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// `AuthDir`: local directory with the mirrored auth files.
    pub fn auth_dir(&self) -> &Path {
        &self.auth_dir
    }

    /// `Bootstrap`: ensures the bucket exists, then pulls config and auths into the spool.
    pub fn bootstrap(&self, example_config_path: &str) -> Result<(), StoreError> {
        self.ensure_bucket()?;
        self.sync_config_from_bucket(example_config_path)?;
        self.sync_auth_from_bucket()
    }

    fn ensure_bucket(&self) -> Result<(), StoreError> {
        let client = self.client.clone();
        let exists = rt::block_on(async move { client.bucket_exists().await }).map_err(|e| s3_err("check bucket", &e))?;
        if exists {
            return Ok(());
        }
        let client = self.client.clone();
        rt::block_on(async move { client.make_bucket().await }).map_err(|e| s3_err("create bucket", &e))
    }

    fn sync_config_from_bucket(&self, example: &str) -> Result<(), StoreError> {
        let key = self.prefixed_key(CONFIG_KEY);
        let client = self.client.clone();
        let k = key.clone();
        let stat = rt::block_on(async move { client.stat_object(&k).await });
        match stat {
            Ok(()) => {
                let client = self.client.clone();
                let data = rt::block_on(async move { client.get_object(&key).await })
                    .map_err(|e| s3_err("fetch config", &e))?;
                write_file_private(&self.config_path, &normalize_line_endings_bytes(&data))
                    .map_err(|e| backend_err(format!("object store: write config: {e}")))
            }
            Err(e) if e.is_not_found() => {
                if !self.config_path.exists() {
                    if !example.is_empty() {
                        copy_config_template(Path::new(example), &self.config_path)
                            .map_err(|e| backend_err(format!("object store: copy example config: {e}")))?;
                    } else {
                        if let Some(dir) = self.config_path.parent() {
                            mkdir_all_private(dir)
                                .map_err(|e| backend_err(format!("object store: prepare config directory: {e}")))?;
                        }
                        write_file_private(&self.config_path, b"")
                            .map_err(|e| backend_err(format!("object store: create empty config: {e}")))?;
                    }
                }
                let data = fs::read(&self.config_path)
                    .map_err(|e| backend_err(format!("object store: read local config: {e}")))?;
                if !data.is_empty() {
                    self.put_object(CONFIG_KEY, data, "application/x-yaml")?;
                }
                Ok(())
            }
            Err(e) => Err(s3_err("stat config", &e)),
        }
    }

    /// Downloads every auth object into the spool without wiping it first (a wipe would fire
    /// watcher delete events that propagate back to the bucket).
    fn sync_auth_from_bucket(&self) -> Result<(), StoreError> {
        mkdir_all_private(&self.auth_dir)
            .map_err(|e| backend_err(format!("object store: create auth directory: {e}")))?;
        let prefix = self.prefixed_key(&format!("{AUTH_PREFIX}/"));
        let client = self.client.clone();
        let p = prefix.clone();
        let keys = rt::block_on(async move { client.list_objects(&p).await })
            .map_err(|e| s3_err("list auth objects", &e))?;
        for key in keys {
            let rel = key.strip_prefix(&prefix).unwrap_or(&key);
            if rel.is_empty() || rel.ends_with('/') {
                continue;
            }
            if Path::new(rel).is_absolute() {
                tracing::warn!(key = %key, "object store: skip auth outside mirror");
                continue;
            }
            let clean = clean_path(Path::new(rel));
            let clean_str = clean.to_string_lossy();
            if clean_str == "." || clean_str == ".." || clean_str.starts_with("../") {
                tracing::warn!(key = %key, "object store: skip auth outside mirror");
                continue;
            }
            let local = self.auth_dir.join(&clean);
            if let Some(dir) = local.parent() {
                mkdir_all_private(dir).map_err(|e| backend_err(format!("object store: prepare auth subdir: {e}")))?;
            }
            let client = self.client.clone();
            let k = key.clone();
            let data = rt::block_on(async move { client.get_object(&k).await })
                .map_err(|e| s3_err(&format!("download auth {key}"), &e))?;
            write_file_private(&local, &data)
                .map_err(|e| backend_err(format!("object store: write auth {}: {e}", local.display())))?;
        }
        Ok(())
    }

    fn prefixed_key(&self, key: &str) -> String {
        let key = key.trim_start_matches('/');
        if self.cfg.prefix.is_empty() {
            return key.to_string();
        }
        format!("{}/{key}", self.cfg.prefix).trim_start_matches('/').to_string()
    }

    fn put_object(&self, key: &str, data: Vec<u8>, content_type: &str) -> Result<(), StoreError> {
        if data.is_empty() {
            return self.delete_object(key);
        }
        let full = self.prefixed_key(key);
        let client = self.client.clone();
        let (k, ct) = (full.clone(), content_type.to_string());
        rt::block_on(async move { client.put_object(&k, data, &ct).await })
            .map_err(|e| s3_err(&format!("put object {full}"), &e))
    }

    fn delete_object(&self, key: &str) -> Result<(), StoreError> {
        let full = self.prefixed_key(key);
        let client = self.client.clone();
        let k = full.clone();
        match rt::block_on(async move { client.remove_object(&k).await }) {
            Ok(()) => Ok(()),
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(s3_err(&format!("delete object {full}"), &e)),
        }
    }

    /// Object key of a spool auth path: `auths/<slash-separated path relative to the auth dir>`.
    fn auth_key(&self, path: &Path) -> Result<String, StoreError> {
        let rel = rel_path(&self.auth_dir, path)
            .ok_or_else(|| backend_err("object store: resolve auth relative path: no common root"))?;
        Ok(format!("{AUTH_PREFIX}/{}", rel.to_string_lossy().replace('\\', "/")))
    }

    fn upload_auth(&self, path: &Path) -> Result<(), StoreError> {
        if path.as_os_str().is_empty() {
            return Ok(());
        }
        let key = self.auth_key(path)?;
        match fs::read(path) {
            Ok(data) if data.is_empty() => self.delete_object(&key),
            Ok(data) => self.put_object(&key, data, "application/json"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.delete_object(&key),
            Err(e) => Err(backend_err(format!("object store: read auth file: {e}"))),
        }
    }

    fn delete_auth_object(&self, path: &Path) -> Result<(), StoreError> {
        if path.as_os_str().is_empty() {
            return Ok(());
        }
        let key = self.auth_key(path)?;
        self.delete_object(&key)
    }

    fn resolve_auth_path(&self, auth: &Auth) -> Result<PathBuf, StoreError> {
        let p = auth.attr(ATTRIBUTE_PATH);
        if !p.is_empty() {
            if Path::new(&p).is_absolute() {
                return Ok(PathBuf::from(p));
            }
            return Ok(self.auth_dir.join(p));
        }
        let mut file_name = auth.file_name.trim().to_string();
        if file_name.is_empty() {
            file_name = auth.id.trim().to_string();
        }
        if file_name.is_empty() {
            return Err(backend_err(format!("object store: auth {} missing filename", auth.id)));
        }
        if !file_name.to_lowercase().ends_with(".json") {
            file_name.push_str(".json");
        }
        Ok(self.auth_dir.join(file_name))
    }

    fn resolve_delete_path(&self, id: &str) -> Result<PathBuf, StoreError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(backend_err("object store: id is empty"));
        }
        if Path::new(id).is_absolute() {
            return Ok(PathBuf::from(id));
        }
        let clean = clean_path(Path::new(id));
        let mut clean = clean.to_string_lossy().into_owned();
        if clean == "." || clean == ".." || clean.starts_with("../") {
            return Err(backend_err(format!("object store: invalid auth identifier {id}")));
        }
        if !clean.to_lowercase().ends_with(".json") {
            clean.push_str(".json");
        }
        Ok(self.auth_dir.join(clean))
    }

    /// `readAuthFile`: one spool file as an `Auth`; `Ok(None)` for empty files.
    fn read_auth_file(&self, path: &Path, base_dir: &Path) -> Result<Option<Auth>, StoreError> {
        let data = fs::read(path).map_err(|e| backend_err(format!("read file: {e}")))?;
        if data.is_empty() {
            return Ok(None);
        }
        let mut metadata = match serde_json::from_slice::<Value>(&data) {
            Ok(Value::Object(m)) => m,
            Ok(_) => return Err(backend_err("unmarshal auth json: not an object")),
            Err(e) => return Err(backend_err(format!("unmarshal auth json: {e}"))),
        };
        normalize_credential_metadata(&mut metadata);
        let mut probe = Auth::default();
        probe.metadata = metadata.clone();
        validate_auth_weight(&probe).map_err(backend_err)?;
        let mut provider = value_as_string(metadata.get("type")).trim().to_string();
        if provider.is_empty() {
            provider = "unknown".into();
        }
        let mtime = fs::metadata(path)
            .and_then(|m| m.modified())
            .map_err(|e| backend_err(format!("stat auth file: {e}")))?;
        let mtime: chrono::DateTime<chrono::Utc> = mtime.into();
        let rel = match rel_path(base_dir, path) {
            Some(r) => r.to_string_lossy().into_owned(),
            None => path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        };
        let rel = normalize_auth_id(&rel);
        let mut auth = Auth::default();
        auth.id = rel.clone();
        auth.provider = provider;
        auth.file_name = rel;
        auth.label = label_for(&metadata);
        auth.status = Status::Active;
        auth.created_at = Some(mtime);
        auth.updated_at = Some(mtime);
        auth.attributes.insert(ATTRIBUTE_PATH.into(), path.to_string_lossy().into_owned());
        auth.attributes.insert(ATTRIBUTE_SOURCE_BACKEND.into(), AUTH_SOURCE_OBJECT_STORE.into());
        let email = value_as_string(metadata.get("email")).trim().to_string();
        if !email.is_empty() {
            auth.attributes.insert("email".into(), email);
        }
        let disabled = metadata.get("disabled").and_then(Value::as_bool).unwrap_or(false);
        auth.metadata = metadata;
        apply_custom_headers_from_metadata(&mut auth);
        if disabled {
            auth.disabled = true;
            auth.status = Status::Disabled;
        }
        Ok(Some(auth))
    }
}

impl Store for ObjectTokenStore {
    /// `List`: auth JSON files of the mirrored workspace (not the bucket).
    fn list(&self) -> Result<Vec<Auth>, StoreError> {
        if self.auth_dir.as_os_str().is_empty() {
            return Err(backend_err("object store: auth directory not configured"));
        }
        let mut entries = Vec::new();
        walk_json_files(&self.auth_dir, &mut |path| match self.read_auth_file(path, &self.auth_dir) {
            Ok(Some(auth)) => entries.push(auth),
            Ok(None) => {}
            Err(e) => tracing::warn!("object store: skip auth {}: {e}", path.display()),
        })
        .map_err(|e| backend_err(format!("object store: walk auth directory: {e}")))?;
        Ok(entries)
    }

    /// `Save`: writes the spool file, then uploads it.
    fn save(&self, auth: &mut Auth, opts: SaveOptions) -> Result<Option<PathBuf>, StoreError> {
        normalize_credential_metadata(&mut auth.metadata);
        validate_auth_weight(auth).map_err(|e| backend_err(format!("object store: {e}")))?;
        let path = self.resolve_auth_path(auth)?;
        if path.as_os_str().is_empty() {
            return Err(backend_err(format!("object store: missing file path attribute for {}", auth.id)));
        }
        if skip_disabled_recreate(auth, opts.creation_intent, &path) {
            return Ok(None);
        }

        let _guard = self.mu.lock();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            mkdir_all_private(dir).map_err(|e| backend_err(format!("object store: create auth directory: {e}")))?;
        }
        if let Written::Unchanged = write_auth_file("object store", auth, &path)? {
            return Ok(Some(path));
        }
        stamp_saved(auth, &path, AUTH_SOURCE_OBJECT_STORE);
        self.upload_auth(&path)?;
        Ok(Some(path))
    }

    /// `Delete`: removes the spool file and the object.
    fn delete(&self, id: &str) -> Result<(), StoreError> {
        let path = self.resolve_delete_path(id)?;
        let _guard = self.mu.lock();
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(backend_err(format!("object store: delete auth file: {e}"))),
        }
        self.delete_auth_object(&path)
    }

    fn base_dir(&self) -> Option<PathBuf> {
        Some(self.auth_dir.clone())
    }
}

impl StorePersister for ObjectTokenStore {
    /// `PersistAuthFiles`: uploads each path (relative paths resolve under the auth dir).
    fn persist_auth_files(&self, _message: &str, paths: &[String]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let _guard = self.mu.lock();
        for p in paths {
            let trimmed = p.trim();
            if trimmed.is_empty() {
                continue;
            }
            let abs = if Path::new(trimmed).is_absolute() {
                PathBuf::from(trimmed)
            } else {
                self.auth_dir.join(trimmed)
            };
            self.upload_auth(&abs).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// `PersistConfig`: uploads the spool config (deleting the object when it is missing/empty).
    fn persist_config(&self) -> Result<(), String> {
        let _guard = self.mu.lock();
        let result = match fs::read(&self.config_path) {
            Ok(data) if data.is_empty() => self.delete_object(CONFIG_KEY),
            Ok(data) => self.put_object(CONFIG_KEY, data, "application/x-yaml"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.delete_object(CONFIG_KEY),
            Err(e) => Err(backend_err(format!("object store: read config file: {e}"))),
        };
        result.map_err(|e| e.to_string())
    }
}
