//! Pluggable remote KV store for the global caches (Go: the `internal/home` KV client the
//! `internal/cache` files switch to when Home mode is on).
//!
//! `cpa-core` cannot depend on the Home client (the config crate depends on core), so the
//! executors crate installs a [`KvBackend`] at startup. The caches are synchronous, hence the
//! backend is too: its implementation bridges to the async Home client. With no backend, or one
//! reporting "not Home mode", the caches use their in-process maps exactly as before.

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Failure of a KV operation (Home mode on, but the store errored or is unavailable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvError(pub String);

impl KvError {
    pub fn new(message: impl fmt::Display) -> Self {
        KvError(message.to_string())
    }
}

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KvError {}

pub type KvResult<T> = Result<T, KvError>;

/// The remote store behind Home mode. A zero `ttl` means no expiry.
pub trait KvBackend: Send + Sync {
    /// `Ok(false)`: not in Home mode (use the in-process cache). `Ok(true)`: usable. `Err`: Home
    /// mode is on but the store is unavailable (Go: `CurrentKVClient`'s error).
    fn status(&self) -> KvResult<bool>;
    fn get(&self, key: &str) -> KvResult<Option<Vec<u8>>>;
    /// Plain `SET` with an optional expiry; `Ok(false)` when Home did not write.
    fn set(&self, key: &str, value: &[u8], ttl: Duration) -> KvResult<bool>;
    /// Atomic replace when the stored value still equals `expected` (`None`: key must be absent).
    fn compare_and_swap(&self, key: &str, expected: Option<&[u8]>, value: &[u8], ttl: Duration) -> KvResult<bool>;
    fn del(&self, key: &str) -> KvResult<()>;
    fn expire(&self, key: &str, ttl: Duration) -> KvResult<()>;
}

static BACKEND: OnceLock<Arc<dyn KvBackend>> = OnceLock::new();

/// Installs the process-wide backend (first call wins).
pub fn install_kv_backend(backend: Arc<dyn KvBackend>) {
    let _ = BACKEND.set(backend);
}

/// Where a cache operation runs.
pub(crate) enum Store {
    Local,
    Home(&'static dyn KvBackend),
}

/// The active store; `Err` when Home mode is on but the KV store is unavailable.
pub(crate) fn store() -> KvResult<Store> {
    let Some(backend) = BACKEND.get() else { return Ok(Store::Local) };
    Ok(if backend.status()? { Store::Home(backend.as_ref()) } else { Store::Local })
}

/// Hex SHA-256 of a key component, so keys never carry raw session or credential text (Go:
/// `homekv.HashKeyPart`).
pub(crate) fn hash_key_part(value: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(value.as_bytes()))
}
