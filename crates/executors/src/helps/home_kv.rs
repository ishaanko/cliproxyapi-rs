//! Home KV for synchronous executor code (Go: `internal/home` KV helpers called from the executor
//! and cache packages).
//!
//! The Home client is async while the caches and request preparation are synchronous.
//! [`run_blocking`] runs one Home operation to completion from a worker thread of the multi-thread
//! runtime (the server's); that thread blocks for the duration, with its queued tasks moved to
//! other workers. Where that is impossible (no runtime, a current-thread runtime) it reports the
//! store as unavailable, which callers treat like any other Home failure. [`install`] plugs the
//! Home client into the `cpa_core` caches.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use cpa_core::cache::{KvBackend, KvError, KvResult, install_kv_backend};
use cpa_home::KvSetOptions;
use cpa_home::{Client, HomeError};
use cpa_runtime::executor::ExecError;

/// Drives `fut` to completion on the calling thread.
pub fn run_blocking<F: Future>(fut: F) -> Result<F::Output, HomeError> {
    cpa_home::kv::run_blocking(fut)
}

/// [`run_blocking`] for operations that themselves return `Result<_, HomeError>`.
pub fn call<T>(fut: impl Future<Output = Result<T, HomeError>>) -> Result<T, HomeError> {
    run_blocking(fut)?
}

/// The usable Home client: `Ok(None)` outside Home mode, `Err` when Home mode is on but the KV
/// store is unavailable.
pub fn client() -> Result<Option<Arc<Client>>, HomeError> {
    cpa_home::kv::current_kv()
}

/// Executor error for a Home KV failure while preparing a request (Go returns the plain error
/// before any upstream attempt).
pub fn exec_error(err: &HomeError) -> ExecError {
    let mut e = ExecError::new(0, err.to_string());
    e.upstream_attempted = false;
    e
}

/// [`exec_error`] for a failure reported by the `cpa_core` caches.
pub fn kv_exec_error(err: &KvError) -> ExecError {
    let mut e = ExecError::new(0, err.to_string());
    e.upstream_attempted = false;
    e
}

fn kv_error(err: HomeError) -> KvError {
    match err {
        HomeError::CompareAndSwapUnsupported => KvError::compare_and_swap_unsupported(err),
        err => KvError::new(err),
    }
}

/// `cpa_core` cache backend over the process-wide Home client.
struct HomeBackend;

impl HomeBackend {
    fn client() -> KvResult<Arc<Client>> {
        match client() {
            Ok(Some(client)) => Ok(client),
            Ok(None) => Err(KvError::new("home kv store unavailable: home mode is off")),
            Err(e) => Err(kv_error(e)),
        }
    }
}

impl KvBackend for HomeBackend {
    fn status(&self) -> KvResult<bool> {
        client().map(|c| c.is_some()).map_err(kv_error)
    }

    fn get(&self, key: &str) -> KvResult<Option<Vec<u8>>> {
        let client = Self::client()?;
        call(client.kv_get(key)).map_err(kv_error)
    }

    fn set(&self, key: &str, value: &[u8], ttl: Duration) -> KvResult<bool> {
        let client = Self::client()?;
        call(client.kv_set(key, value, KvSetOptions { ex: ttl, ..Default::default() })).map_err(kv_error)
    }

    fn compare_and_swap(&self, key: &str, expected: Option<&[u8]>, value: &[u8], ttl: Duration) -> KvResult<bool> {
        let client = Self::client()?;
        call(client.kv_compare_and_swap(key, expected.unwrap_or_default(), expected.is_some(), value, ttl))
            .map_err(kv_error)
    }

    fn del(&self, key: &str) -> KvResult<()> {
        let client = Self::client()?;
        call(client.kv_del(&[key.to_string()])).map(|_| ()).map_err(kv_error)
    }

    fn expire(&self, key: &str, ttl: Duration) -> KvResult<()> {
        let client = Self::client()?;
        call(client.kv_expire(key, ttl)).map(|_| ()).map_err(kv_error)
    }
}

/// Makes the `cpa_core` caches use Home KV whenever Home mode is on. Idempotent; called from
/// [`crate::all_executors`].
pub fn install() {
    install_kv_backend(Arc::new(HomeBackend));
}
