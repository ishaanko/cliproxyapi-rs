//! Home KV for synchronous executor code (Go: `internal/home` KV helpers called from the executor
//! and cache packages).
//!
//! The Home client is async while the caches and request preparation are synchronous.
//! [`run_blocking`] runs one Home operation to completion from a worker thread of the multi-thread
//! runtime (the server's) without stalling other tasks. Where that is impossible (no runtime, a
//! current-thread runtime) it reports the store as unavailable, which callers treat like any
//! other Home failure. [`install`] plugs the Home client into the `cpa_core` caches.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use cpa_core::cache::{KvBackend, KvError, KvResult, install_kv_backend};
use cpa_home::KvSetOptions;
use cpa_home::{Client, HomeError};
use tokio::runtime::{Handle, RuntimeFlavor};

/// Drives `fut` to completion on the calling thread.
pub fn run_blocking<F: Future>(fut: F) -> Result<F::Output, HomeError> {
    let handle = Handle::try_current().map_err(|_| HomeError::other("home kv unavailable: no async runtime"))?;
    match handle.runtime_flavor() {
        RuntimeFlavor::MultiThread => Ok(tokio::task::block_in_place(|| handle.block_on(fut))),
        _ => Err(HomeError::other("home kv unavailable: needs a multi-thread async runtime")),
    }
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

fn kv_error(err: HomeError) -> KvError {
    KvError::new(err)
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
