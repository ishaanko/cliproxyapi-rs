//! Postgres-backed config and credential store (Go: internal/store/postgresstore.go).
//!
//! Config and auth JSON live in Postgres and are mirrored into a local spool directory
//! (`<spool>/config/config.yaml`, `<spool>/auths/**`) so file-based flows and the watcher keep
//! working; the watcher pushes spool changes back through [`StorePersister`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cpa_auth::credmeta::{
    Metadata, apply_custom_headers_from_metadata, normalize_credential_metadata, validate_auth_weight,
};
use cpa_auth::store::{SaveOptions, Store, StoreError};
use cpa_auth::types::{AUTH_SOURCE_POSTGRES, ATTRIBUTE_PATH, ATTRIBUTE_SOURCE_BACKEND, Status};
use cpa_auth::util::clean_path;
use cpa_auth::Auth;
use cpa_runtime::conductor::CooldownStateStore;
use cpa_runtime::service::StorePersister;
use parking_lot::Mutex;
use serde_json::Value;

use crate::common::{
    Written, backend_err, copy_config_template, label_for, mkdir_all_private, normalize_auth_id,
    normalize_line_endings, rel_path, skip_disabled_recreate, stamp_saved, value_as_string, write_auth_file,
    write_file_private,
};
use crate::pgconn::{Conn, ErrText};
use crate::postgres_cooldown::PostgresCooldownStore;
use crate::rt;

const DEFAULT_CONFIG_TABLE: &str = "config_store";
const DEFAULT_AUTH_TABLE: &str = "auth_store";
const DEFAULT_COOLDOWN_TABLE: &str = "cooldown_store";
const DEFAULT_CONFIG_KEY: &str = "config";

/// Configuration required to initialize a Postgres-backed store.
#[derive(Clone, Default)]
pub struct PostgresStoreConfig {
    pub dsn: String,
    pub schema: String,
    pub config_table: String,
    pub auth_table: String,
    pub cooldown_table: String,
    pub spool_dir: String,
}

/// The DSN carries the password, so Debug leaves it out.
impl std::fmt::Debug for PostgresStoreConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresStoreConfig")
            .field("dsn", &"<redacted>")
            .field("schema", &self.schema)
            .field("config_table", &self.config_table)
            .field("auth_table", &self.auth_table)
            .field("cooldown_table", &self.cooldown_table)
            .field("spool_dir", &self.spool_dir)
            .finish()
    }
}

pub(crate) struct Shared {
    pub(crate) db: tokio::sync::Mutex<Conn>,
    pub(crate) cfg: PostgresStoreConfig,
}

impl Shared {
    /// `fullTableName`: schema-qualified, quoted.
    pub(crate) fn full_table_name(&self, name: &str) -> String {
        if self.cfg.schema.trim().is_empty() {
            quote_identifier(name)
        } else {
            format!("{}.{}", quote_identifier(&self.cfg.schema), quote_identifier(name))
        }
    }
}

pub(crate) fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Persists configuration and credentials in PostgreSQL while mirroring them to a local spool.
pub struct PostgresStore {
    shared: Arc<Shared>,
    spool_root: PathBuf,
    config_path: PathBuf,
    auth_dir: PathBuf,
    cooldown: Arc<PostgresCooldownStore>,
    mu: Mutex<()>,
}

fn db_err(prefix: &str, err: impl ErrText) -> StoreError {
    backend_err(format!("postgres store: {prefix}: {}", err.err_text()))
}

impl PostgresStore {
    /// `NewPostgresStore`: validates the config, prepares the spool and pings the database.
    pub fn new(mut cfg: PostgresStoreConfig) -> Result<Self, StoreError> {
        let dsn = cfg.dsn.trim().to_string();
        if dsn.is_empty() {
            return Err(backend_err("postgres store: DSN is required"));
        }
        cfg.dsn = dsn;
        if cfg.config_table.is_empty() {
            cfg.config_table = DEFAULT_CONFIG_TABLE.into();
        }
        if cfg.auth_table.is_empty() {
            cfg.auth_table = DEFAULT_AUTH_TABLE.into();
        }
        if cfg.cooldown_table.is_empty() {
            cfg.cooldown_table = DEFAULT_COOLDOWN_TABLE.into();
        }

        let spool = cfg.spool_dir.trim();
        let spool_root = if spool.is_empty() {
            match std::env::current_dir() {
                Ok(cwd) => cwd.join("pgstore"),
                Err(_) => std::env::temp_dir().join("pgstore"),
            }
        } else {
            PathBuf::from(spool)
        };
        let abs_spool = std::path::absolute(&spool_root)
            .map_err(|e| backend_err(format!("postgres store: resolve spool directory: {e}")))?;
        let abs_spool = clean_path(&abs_spool);
        let config_dir = abs_spool.join("config");
        let auth_dir = abs_spool.join("auths");
        mkdir_all_private(&config_dir)
            .map_err(|e| backend_err(format!("postgres store: create config directory: {e}")))?;
        mkdir_all_private(&auth_dir)
            .map_err(|e| backend_err(format!("postgres store: create auth directory: {e}")))?;

        let conn = Conn::new(&cfg.dsn).map_err(|e| db_err("open database connection", e))?;
        let shared = Arc::new(Shared { db: tokio::sync::Mutex::new(conn), cfg });
        let ping = shared.clone();
        rt::block_on_deadline(async move {
            let mut db = ping.db.lock().await;
            let client = db.client().await?;
            client.simple_query("SELECT 1").await.map(|_| ())
        })
        .map_err(|e| db_err("ping database", e))?;

        let cooldown = Arc::new(PostgresCooldownStore::new(shared.clone()));
        Ok(Self {
            shared,
            config_path: config_dir.join("config.yaml"),
            spool_root: abs_spool,
            auth_dir,
            cooldown,
            mu: Mutex::new(()),
        })
    }

    /// `ConfigPath`: the managed config file inside the spool.
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// `AuthDir`: local directory with the mirrored auth files.
    pub fn auth_dir(&self) -> &Path {
        &self.auth_dir
    }

    /// `WorkDir`: the spool root.
    pub fn work_dir(&self) -> &Path {
        &self.spool_root
    }

    /// `CooldownStateStore`: the Postgres-backed runtime cooldown store.
    pub fn cooldown_store(&self) -> Arc<dyn CooldownStateStore> {
        self.cooldown.clone()
    }

    /// `EnsureSchema`: creates the schema (when set) and the three tables.
    pub fn ensure_schema(&self) -> Result<(), StoreError> {
        let shared = self.shared.clone();
        rt::block_on(async move {
            tokio::time::timeout(rt::DEADLINE, ensure_schema(&shared))
                .await
                .unwrap_or_else(|_| Err(db_err("create schema", "context deadline exceeded".to_string())))
        })
    }

    /// `Bootstrap`: syncs config and auth records between Postgres and the spool.
    pub fn bootstrap(&self, example_config_path: &str) -> Result<(), StoreError> {
        self.ensure_schema()?;
        self.sync_config_from_database(example_config_path)?;
        self.sync_auth_from_database()
    }

    fn query(&self, sql: String, params: Vec<DbParam>) -> Result<(), StoreError> {
        let shared = self.shared.clone();
        rt::block_on_deadline(async move {
            let mut db = shared.db.lock().await;
            let client = db.client().await?;
            let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params.iter().map(DbParam::as_sql).collect();
            client.execute(sql.as_str(), &refs).await.map(|_| ())
        })
        .map_err(backend_err)
    }

    fn sync_config_from_database(&self, example: &str) -> Result<(), StoreError> {
        let table = self.shared.full_table_name(&self.shared.cfg.config_table);
        let shared = self.shared.clone();
        let sql = format!("SELECT content FROM {table} WHERE id = $1");
        let row: Option<String> = rt::block_on_deadline(async move {
            let mut db = shared.db.lock().await;
            let client = db.client().await?;
            let row = client.query_opt(sql.as_str(), &[&DEFAULT_CONFIG_KEY]).await?;
            Ok::<_, tokio_postgres::Error>(row.map(|r| r.get::<_, String>(0)))
        })
        .map_err(|e| db_err("load config from database", e))?;

        match row {
            None => {
                if !self.config_path.exists() {
                    if !example.is_empty() {
                        copy_config_template(Path::new(example), &self.config_path)
                            .map_err(|e| backend_err(format!("postgres store: copy example config: {e}")))?;
                    } else {
                        if let Some(dir) = self.config_path.parent() {
                            mkdir_all_private(dir)
                                .map_err(|e| backend_err(format!("postgres store: prepare config directory: {e}")))?;
                        }
                        write_file_private(&self.config_path, b"")
                            .map_err(|e| backend_err(format!("postgres store: create empty config: {e}")))?;
                    }
                }
                let data = fs::read(&self.config_path)
                    .map_err(|e| backend_err(format!("postgres store: read local config: {e}")))?;
                self.persist_config_data(&data)
            }
            Some(content) => {
                if let Some(dir) = self.config_path.parent() {
                    mkdir_all_private(dir)
                        .map_err(|e| backend_err(format!("postgres store: prepare config directory: {e}")))?;
                }
                write_file_private(&self.config_path, normalize_line_endings(&content).as_bytes())
                    .map_err(|e| backend_err(format!("postgres store: write config to spool: {e}")))
            }
        }
    }

    fn sync_auth_from_database(&self) -> Result<(), StoreError> {
        let rows = self.fetch_auth_rows(false)?;
        fs::remove_dir_all(&self.auth_dir)
            .or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
            .map_err(|e| backend_err(format!("postgres store: reset auth directory: {e}")))?;
        mkdir_all_private(&self.auth_dir)
            .map_err(|e| backend_err(format!("postgres store: recreate auth directory: {e}")))?;
        for (id, payload, _, _) in rows {
            let path = match self.absolute_auth_path(&id) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("postgres store: skipping auth {id} outside spool: {e}");
                    continue;
                }
            };
            if let Some(dir) = path.parent() {
                mkdir_all_private(dir).map_err(|e| backend_err(format!("postgres store: create auth subdir: {e}")))?;
            }
            write_file_private(&path, payload.as_bytes())
                .map_err(|e| backend_err(format!("postgres store: write auth file: {e}")))?;
        }
        Ok(())
    }

    /// Auth rows as `(id, content text, created_at, updated_at)`, optionally ordered by id.
    #[allow(clippy::type_complexity)]
    fn fetch_auth_rows(
        &self,
        ordered: bool,
    ) -> Result<Vec<(String, String, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>, StoreError> {
        let table = self.shared.full_table_name(&self.shared.cfg.auth_table);
        let order = if ordered { " ORDER BY id" } else { "" };
        let sql = format!("SELECT id, content::text, created_at, updated_at FROM {table}{order}");
        let shared = self.shared.clone();
        rt::block_on_deadline(async move {
            let mut db = shared.db.lock().await;
            let client = db.client().await?;
            let rows = client.query(sql.as_str(), &[]).await?;
            Ok::<_, tokio_postgres::Error>(
                rows.iter()
                    .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
                    .collect::<Vec<_>>(),
            )
        })
        .map_err(|e| db_err(if ordered { "list auth" } else { "load auth from database" }, e))
    }

    fn persist_config_data(&self, data: &[u8]) -> Result<(), StoreError> {
        let table = self.shared.full_table_name(&self.shared.cfg.config_table);
        let sql = format!(
            "INSERT INTO {table} (id, content, created_at, updated_at) VALUES ($1, $2, NOW(), NOW()) \
             ON CONFLICT (id) DO UPDATE SET content = EXCLUDED.content, updated_at = NOW()"
        );
        let normalized = normalize_line_endings(&String::from_utf8_lossy(data));
        self.query(sql, vec![DbParam::Text(DEFAULT_CONFIG_KEY.into()), DbParam::Text(normalized)])
            .map_err(|e| backend_err(format!("postgres store: upsert config: {e}")))
    }

    fn delete_config_record(&self) -> Result<(), StoreError> {
        let table = self.shared.full_table_name(&self.shared.cfg.config_table);
        self.query(
            format!("DELETE FROM {table} WHERE id = $1"),
            vec![DbParam::Text(DEFAULT_CONFIG_KEY.into())],
        )
        .map_err(|e| backend_err(format!("postgres store: delete config: {e}")))
    }

    fn persist_auth(&self, rel_id: &str, data: &[u8]) -> Result<(), StoreError> {
        let table = self.shared.full_table_name(&self.shared.cfg.auth_table);
        // The raw bytes go to Postgres as text so it parses them exactly like Go's RawMessage.
        let sql = format!(
            "INSERT INTO {table} (id, content, created_at, updated_at) VALUES ($1, $2::text::jsonb, NOW(), NOW()) \
             ON CONFLICT (id) DO UPDATE SET content = EXCLUDED.content, updated_at = NOW()"
        );
        let text = String::from_utf8(data.to_vec()).map_err(|_| {
            backend_err("postgres store: upsert auth record: invalid byte sequence for encoding \"UTF8\"")
        })?;
        self.query(sql, vec![DbParam::Text(rel_id.into()), DbParam::Text(text)])
            .map_err(|e| backend_err(format!("postgres store: upsert auth record: {e}")))
    }

    fn delete_auth_record(&self, rel_id: &str) -> Result<(), StoreError> {
        let table = self.shared.full_table_name(&self.shared.cfg.auth_table);
        self.query(format!("DELETE FROM {table} WHERE id = $1"), vec![DbParam::Text(rel_id.into())])
            .map_err(|e| backend_err(format!("postgres store: delete auth record: {e}")))
    }

    /// Upserts the spool file as the record for `rel_id`; empty files delete the record.
    fn upsert_auth_record(&self, rel_id: &str, path: &Path) -> Result<(), StoreError> {
        let data = fs::read(path).map_err(|e| backend_err(format!("postgres store: read auth file: {e}")))?;
        if data.is_empty() {
            return self.delete_auth_record(rel_id);
        }
        self.persist_auth(rel_id, &data)
    }

    /// `syncAuthFile`: like upsert, but a missing file deletes the record.
    fn sync_auth_file(&self, rel_id: &str, path: &Path) -> Result<(), StoreError> {
        match fs::read(path) {
            Ok(data) if data.is_empty() => self.delete_auth_record(rel_id),
            Ok(data) => self.persist_auth(rel_id, &data),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.delete_auth_record(rel_id),
            Err(e) => Err(backend_err(format!("postgres store: read auth file: {e}"))),
        }
    }

    fn resolve_auth_path(&self, auth: &Auth) -> Result<PathBuf, StoreError> {
        let p = auth.attr(ATTRIBUTE_PATH);
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
        let file_name = auth.file_name.trim();
        if !file_name.is_empty() {
            if Path::new(file_name).is_absolute() {
                return Ok(PathBuf::from(file_name));
            }
            return Ok(self.auth_dir.join(file_name));
        }
        if auth.id.is_empty() {
            return Err(backend_err("postgres store: missing id"));
        }
        if Path::new(&auth.id).is_absolute() {
            return Ok(PathBuf::from(&auth.id));
        }
        Ok(self.auth_dir.join(&auth.id))
    }

    fn resolve_delete_path(&self, id: &str) -> PathBuf {
        if id.contains(std::path::MAIN_SEPARATOR) || Path::new(id).is_absolute() {
            return PathBuf::from(id);
        }
        self.auth_dir.join(id)
    }

    /// `relativeAuthID`: slash-separated path relative to the spool auth dir.
    fn relative_auth_id(&self, path: &Path) -> Result<String, StoreError> {
        let path = if path.is_absolute() { path.to_path_buf() } else { self.auth_dir.join(path) };
        let rel = rel_path(&self.auth_dir, &path)
            .ok_or_else(|| backend_err("postgres store: compute relative path: no common root"))?;
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.starts_with("..") {
            return Err(backend_err(format!(
                "postgres store: path {} outside managed directory",
                path.display()
            )));
        }
        Ok(rel)
    }

    /// `absoluteAuthPath`: record id to spool path, rejecting escapes.
    fn absolute_auth_path(&self, id: &str) -> Result<PathBuf, StoreError> {
        let clean = clean_path(Path::new(id));
        if clean.to_string_lossy().starts_with("..") {
            return Err(backend_err(format!("postgres store: invalid auth identifier {id}")));
        }
        let path = self.auth_dir.join(&clean);
        let rel = rel_path(&self.auth_dir, &path)
            .ok_or_else(|| backend_err("postgres store: resolved auth path escapes auth directory"))?;
        if rel.to_string_lossy().starts_with("..") {
            return Err(backend_err("postgres store: resolved auth path escapes auth directory"));
        }
        Ok(path)
    }
}

/// A bind parameter owned across the `block_on` boundary.
enum DbParam {
    Text(String),
}

impl DbParam {
    fn as_sql(&self) -> &(dyn tokio_postgres::types::ToSql + Sync) {
        match self {
            DbParam::Text(s) => s,
        }
    }
}

pub(crate) async fn ensure_schema(shared: &Shared) -> Result<(), StoreError> {
    let mut db = shared.db.lock().await;
    let client = db.client().await.map_err(|e| db_err("create schema", e))?;
    let schema = shared.cfg.schema.trim();
    if !schema.is_empty() {
        client
            .batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {}", quote_identifier(schema)))
            .await
            .map_err(|e| db_err("create schema", e))?;
    }
    let config_table = shared.full_table_name(&shared.cfg.config_table);
    client
        .batch_execute(&format!(
            "CREATE TABLE IF NOT EXISTS {config_table} (
			id TEXT PRIMARY KEY,
			content TEXT NOT NULL,
			created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
		)"
        ))
        .await
        .map_err(|e| db_err("create config table", e))?;
    let auth_table = shared.full_table_name(&shared.cfg.auth_table);
    client
        .batch_execute(&format!(
            "CREATE TABLE IF NOT EXISTS {auth_table} (
			id TEXT PRIMARY KEY,
			content JSONB NOT NULL,
			created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
		)"
        ))
        .await
        .map_err(|e| db_err("create auth table", e))?;
    let cooldown_table = shared.full_table_name(&shared.cfg.cooldown_table);
    client
        .batch_execute(&format!(
            "CREATE TABLE IF NOT EXISTS {cooldown_table} (
			auth_id TEXT NOT NULL,
			model TEXT NOT NULL DEFAULT '',
			content JSONB NOT NULL,
			deleted BOOLEAN NOT NULL DEFAULT FALSE,
			created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			PRIMARY KEY (auth_id, model)
		)"
        ))
        .await
        .map_err(|e| db_err("create cooldown table", e))?;
    Ok(())
}

impl Store for PostgresStore {
    /// `List`: every auth record in Postgres, skipping rows with invalid JSON or weights.
    fn list(&self) -> Result<Vec<Auth>, StoreError> {
        let rows = self.fetch_auth_rows(true)?;
        let mut auths = Vec::with_capacity(rows.len());
        for (id, payload, created_at, updated_at) in rows {
            let path = match self.absolute_auth_path(&id) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("postgres store: skipping auth {id} outside spool: {e}");
                    continue;
                }
            };
            let mut metadata: Metadata = match serde_json::from_str::<Value>(&payload) {
                Ok(Value::Object(m)) => m,
                Ok(_) | Err(_) => {
                    tracing::warn!("postgres store: skipping auth {id} with invalid json");
                    continue;
                }
            };
            normalize_credential_metadata(&mut metadata);
            let mut probe = Auth::default();
            probe.metadata = metadata.clone();
            if let Err(e) = validate_auth_weight(&probe) {
                tracing::warn!("postgres store: skipping auth {id} with invalid weight: {e}");
                continue;
            }
            let mut provider = value_as_string(metadata.get("type")).trim().to_string();
            if provider.is_empty() {
                provider = "unknown".into();
            }
            let mut auth = Auth::default();
            auth.id = normalize_auth_id(&id);
            auth.provider = provider;
            auth.file_name = normalize_auth_id(&id);
            auth.label = label_for(&metadata);
            auth.status = Status::Active;
            auth.created_at = Some(created_at);
            auth.updated_at = Some(updated_at);
            auth.attributes.insert(ATTRIBUTE_PATH.into(), path.to_string_lossy().into_owned());
            auth.attributes.insert(ATTRIBUTE_SOURCE_BACKEND.into(), AUTH_SOURCE_POSTGRES.into());
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
            auths.push(auth);
        }
        Ok(auths)
    }

    /// `Save`: writes the spool file, then upserts it into Postgres.
    fn save(&self, auth: &mut Auth, opts: SaveOptions) -> Result<Option<PathBuf>, StoreError> {
        normalize_credential_metadata(&mut auth.metadata);
        validate_auth_weight(auth).map_err(|e| backend_err(format!("postgres store: {e}")))?;
        let path = self.resolve_auth_path(auth)?;
        if path.as_os_str().is_empty() {
            return Err(backend_err(format!("postgres store: missing file path attribute for {}", auth.id)));
        }
        if skip_disabled_recreate(auth, opts.creation_intent, &path) {
            return Ok(None);
        }

        let _guard = self.mu.lock();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            mkdir_all_private(dir).map_err(|e| backend_err(format!("postgres store: create auth directory: {e}")))?;
        }
        if let Written::Unchanged = write_auth_file("postgres store", auth, &path)? {
            return Ok(Some(path));
        }
        stamp_saved(auth, &path, AUTH_SOURCE_POSTGRES);
        let rel_id = self.relative_auth_id(&path)?;
        self.upsert_auth_record(&rel_id, &path)?;
        Ok(Some(path))
    }

    /// `Delete`: removes the spool file and the record.
    fn delete(&self, id: &str) -> Result<(), StoreError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(backend_err("postgres store: id is empty"));
        }
        let path = self.resolve_delete_path(id);
        let _guard = self.mu.lock();
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(backend_err(format!("postgres store: delete auth file: {e}"))),
        }
        let rel_id = self.relative_auth_id(&path)?;
        self.delete_auth_record(&rel_id)
    }

    fn base_dir(&self) -> Option<PathBuf> {
        Some(self.auth_dir.clone())
    }
}

impl StorePersister for PostgresStore {
    /// `PersistAuthFiles`: syncs each spool path into Postgres.
    fn persist_auth_files(&self, _message: &str, paths: &[String]) -> Result<(), String> {
        if paths.is_empty() {
            return Ok(());
        }
        let _guard = self.mu.lock();
        let run = || -> Result<(), StoreError> {
            for p in paths {
                let mut trimmed = PathBuf::from(p.trim());
                if trimmed.as_os_str().is_empty() {
                    continue;
                }
                let rel_id = match self.relative_auth_id(&trimmed) {
                    Ok(id) => id,
                    Err(_) => {
                        let abs = if trimmed.is_absolute() { trimmed.clone() } else { self.auth_dir.join(&trimmed) };
                        match self.relative_auth_id(&abs) {
                            Ok(id) => {
                                trimmed = abs;
                                id
                            }
                            Err(e) => {
                                tracing::warn!("postgres store: ignoring auth path {}: {e}", trimmed.display());
                                continue;
                            }
                        }
                    }
                };
                self.sync_auth_file(&rel_id, &trimmed)?;
            }
            Ok(())
        };
        run().map_err(|e| e.to_string())
    }

    /// `PersistConfig`: mirrors the spool config file into Postgres.
    fn persist_config(&self) -> Result<(), String> {
        let _guard = self.mu.lock();
        let run = || -> Result<(), StoreError> {
            match fs::read(&self.config_path) {
                Ok(data) => self.persist_config_data(&data),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.delete_config_record(),
                Err(e) => Err(backend_err(format!("postgres store: read config file: {e}"))),
            }
        };
        run().map_err(|e| e.to_string())
    }
}
