//! Store selection from the environment (Go: the PGSTORE_* / GITSTORE_* / OBJECTSTORE_* block of
//! cmd/server/main.go). Precedence is Postgres, then object storage, then git.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cpa_runtime::service::StoreBackend;

use crate::object::{ObjectStoreConfig, ObjectTokenStore};
use crate::postgres::{PostgresStore, PostgresStoreConfig};

/// Go `lookupEnv`: first non-blank value among the keys (upper and lower case spellings).
fn lookup_env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        let v = std::env::var(k).ok()?;
        let t = v.trim();
        (!t.is_empty()).then(|| t.to_string())
    })
}

fn env(name: &str) -> Option<String> {
    lookup_env(&[name, &name.to_lowercase()])
}

/// A remote store opened and bootstrapped: where its spool config lives and how the service
/// should use it.
pub struct OpenedStore {
    /// Spool config file the service loads and the watcher persists.
    pub config_path: PathBuf,
    /// Spool auth directory (the config's `auth-dir` is pinned to it).
    pub auth_dir: PathBuf,
    pub backend: StoreBackend,
}

/// Opens the store the environment asks for, if any. `wd` is the working directory and
/// `home_mode` disables stores (Go: local stores are off when config comes from home).
/// Errors are the Go log lines (`failed to initialize postgres token store: ...`).
pub fn open_from_env(wd: &Path, home_mode: bool) -> Result<Option<OpenedStore>, String> {
    if home_mode {
        return Ok(None);
    }
    let writable = cpa_core::util::writable_path();
    let local_base = |explicit: Option<String>| -> PathBuf {
        match explicit {
            Some(p) => PathBuf::from(p),
            None if !writable.is_empty() => PathBuf::from(&writable),
            None => wd.to_path_buf(),
        }
    };
    let example = wd.join("config.example.yaml");

    if let Some(dsn) = env("PGSTORE_DSN") {
        let spool = local_base(env("PGSTORE_LOCAL_PATH")).join("pgstore");
        let store = PostgresStore::new(PostgresStoreConfig {
            dsn,
            schema: env("PGSTORE_SCHEMA").unwrap_or_default(),
            spool_dir: spool.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .map_err(|e| format!("failed to initialize postgres token store: {e}"))?;
        store
            .bootstrap(&example.to_string_lossy())
            .map_err(|e| format!("failed to bootstrap postgres-backed config: {e}"))?;
        tracing::info!("postgres-backed token store enabled, workspace path: {}", store.work_dir().display());
        let store = Arc::new(store);
        return Ok(Some(OpenedStore {
            config_path: store.config_path().to_path_buf(),
            auth_dir: store.auth_dir().to_path_buf(),
            backend: StoreBackend {
                store: store.clone(),
                persister: store.clone(),
                auth_dir: store.auth_dir().to_path_buf(),
                cooldown: Some(store.cooldown_store()),
            },
        }));
    }

    if let Some(endpoint) = env("OBJECTSTORE_ENDPOINT") {
        let root = local_base(env("OBJECTSTORE_LOCAL_PATH")).join("objectstore");
        let (resolved, use_ssl) = parse_object_endpoint(&endpoint)?;
        let bucket = env("OBJECTSTORE_BUCKET").unwrap_or_default();
        let store = ObjectTokenStore::new(ObjectStoreConfig {
            endpoint: resolved,
            bucket: bucket.clone(),
            access_key: env("OBJECTSTORE_ACCESS_KEY").unwrap_or_default(),
            secret_key: env("OBJECTSTORE_SECRET_KEY").unwrap_or_default(),
            local_root: root.to_string_lossy().into_owned(),
            use_ssl,
            path_style: true,
            ..Default::default()
        })
        .map_err(|e| format!("failed to initialize object token store: {e}"))?;
        store
            .bootstrap(&example.to_string_lossy())
            .map_err(|e| format!("failed to bootstrap object-backed config: {e}"))?;
        tracing::info!("object-backed token store enabled, bucket: {bucket}");
        let store = Arc::new(store);
        return Ok(Some(OpenedStore {
            config_path: store.config_path().to_path_buf(),
            auth_dir: store.auth_dir().to_path_buf(),
            backend: StoreBackend {
                store: store.clone(),
                persister: store.clone(),
                auth_dir: store.auth_dir().to_path_buf(),
                cooldown: None,
            },
        }));
    }

    Ok(None)
}

/// Go endpoint handling: `scheme://host[/path]` selects TLS and is reduced to `host[/path]`; a
/// bare endpoint defaults to TLS.
fn parse_object_endpoint(raw: &str) -> Result<(String, bool), String> {
    let mut resolved = raw.trim().to_string();
    let mut use_ssl = true;
    if resolved.contains("://") {
        let parsed =
            url::Url::parse(&resolved).map_err(|e| format!("failed to parse object store endpoint {raw:?}: {e}"))?;
        match parsed.scheme().to_lowercase().as_str() {
            "http" => use_ssl = false,
            "https" => use_ssl = true,
            other => {
                return Err(format!("unsupported object store scheme {other:?} (only http and https are allowed)"));
            }
        }
        let Some(host) = parsed.host_str() else {
            return Err(format!("object store endpoint {raw:?} is missing host information"));
        };
        let authority = match parsed.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        resolved = authority;
        let path = parsed.path();
        if !path.is_empty() && path != "/" {
            resolved = format!("{resolved}{path}").trim_end_matches('/').to_string();
        }
    }
    Ok((resolved.trim_end_matches('/').to_string(), use_ssl))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parsing() {
        assert_eq!(parse_object_endpoint("http://minio:9000").unwrap(), ("minio:9000".into(), false));
        assert_eq!(parse_object_endpoint("https://s3.example.com/base/").unwrap(), ("s3.example.com/base".into(), true));
        assert_eq!(parse_object_endpoint("play.min.io/").unwrap(), ("play.min.io".into(), true));
        assert!(parse_object_endpoint("ftp://x").unwrap_err().contains("unsupported object store scheme"));
    }
}
