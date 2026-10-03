//! Remote-backed token stores (Postgres, repository, object storage) as seen by the service.
//!
//! Go: `storePersister` in internal/watcher plus the `AuthDir()` provider and
//! `CooldownStateStoreProvider` interfaces. Each backend mirrors config and credentials into a
//! local spool directory; the watcher keeps working on that directory and calls the persister
//! after every accepted config reload and auth file change.

use std::path::PathBuf;
use std::sync::Arc;

use cpa_auth::Store;

use crate::conductor::CooldownStateStore;

/// Pushes spool changes to the remote backend (Go `PersistConfig` / `PersistAuthFiles`).
pub trait StorePersister: Send + Sync {
    fn persist_config(&self) -> Result<(), String>;
    /// `message` is the watcher's commit message (`Sync auth x.json`, `Remove auth x.json`).
    fn persist_auth_files(&self, message: &str, paths: &[String]) -> Result<(), String>;
}

/// A remote-backed token store registered with the service in place of the plain file store.
#[derive(Clone)]
pub struct StoreBackend {
    /// Saves from the credential manager and CLI logins go through this store.
    pub store: Arc<dyn Store>,
    pub persister: Arc<dyn StorePersister>,
    /// Spool auth directory; the service pins `auth-dir` to it (Go `mirroredAuthDir`).
    pub auth_dir: PathBuf,
    /// Backend-owned cooldown state store (Postgres only).
    pub cooldown: Option<Arc<dyn CooldownStateStore>>,
}
