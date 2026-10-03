//! Process-wide Home client and KV helpers (Go: `internal/home/global.go`, `kv_helpers.go`).
//!
//! Caches that live in memory without Home use Home's key/value store when Home mode is on. The
//! `*_required` helpers surface failures; the `*_best_effort` helpers log them and carry on.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::client::{Client, KvSetOptions};
use crate::error::HomeError;

static CURRENT: RwLock<Option<Arc<Client>>> = RwLock::new(None);

/// Sets the active Home client used by runtime integrations.
pub fn set_current(client: Arc<Client>) {
    *CURRENT.write() = Some(client);
}

/// The active Home client, if any.
pub fn current() -> Option<Arc<Client>> {
    CURRENT.read().clone()
}

pub fn clear_current() {
    *CURRENT.write() = None;
}

/// Removes the active client only when it is `client`.
pub fn clear_current_if(client: &Arc<Client>) {
    let mut cur = CURRENT.write();
    if cur.as_ref().is_some_and(|c| Arc::ptr_eq(c, client)) {
        *cur = None;
    }
}

/// Result of a KV helper: not in Home mode, or the outcome of the Home call.
#[derive(Debug)]
pub enum Kv<T> {
    NotHome,
    Home(Result<T, HomeError>),
}

impl<T> Kv<T> {
    pub fn is_home_mode(&self) -> bool {
        matches!(self, Kv::Home(_))
    }
}

pub fn hash_key_part(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

/// `Ok(None)`: no Home client (not Home mode). `Err`: Home mode but the store is unavailable.
fn current_kv_client() -> Result<Option<Arc<Client>>, HomeError> {
    let Some(client) = current() else { return Ok(None) };
    if !client.enabled() {
        return Err(HomeError::Other(format!("home kv store unavailable: {}", HomeError::Disabled)));
    }
    if !client.heartbeat_ok() {
        return Err(HomeError::Other(format!("home kv store unavailable: {}", HomeError::NotConnected)));
    }
    Ok(Some(client))
}

/// Home mode (a Home client exists)? Go: `_, homeMode, _ := CurrentKVClient()`.
pub fn is_home_mode() -> bool {
    current().is_some()
}

/// Whether the store is usable: `(client, home_mode)` or the unavailability error.
pub fn current_kv() -> Result<Option<Arc<Client>>, HomeError> {
    current_kv_client()
}

macro_rules! home_client {
    ($client:ident) => {
        let $client = match current_kv_client() {
            Ok(Some(c)) => c,
            Ok(None) => return Kv::NotHome,
            Err(e) => return Kv::Home(Err(e)),
        };
    };
}

/// `Home(Ok(None))` is a miss.
pub async fn kv_get_json_required<T: DeserializeOwned>(key: &str) -> Kv<Option<T>> {
    home_client!(client);
    let raw = match client.kv_get(key).await {
        Ok(Some(raw)) => raw,
        Ok(None) => return Kv::Home(Ok(None)),
        Err(e) => return Kv::Home(Err(e)),
    };
    match serde_json::from_slice(&raw) {
        Ok(v) => Kv::Home(Ok(Some(v))),
        Err(e) => Kv::Home(Err(HomeError::other(e))),
    }
}

pub async fn kv_set_json_required<T: Serialize>(key: &str, value: &T, ttl: Duration) -> Kv<()> {
    match serde_json::to_vec(value) {
        Ok(raw) => kv_set_bytes_required(key, &raw, ttl).await,
        Err(e) => Kv::Home(Err(HomeError::other(e))),
    }
}

pub async fn kv_set_bytes_required(key: &str, value: &[u8], ttl: Duration) -> Kv<()> {
    home_client!(client);
    match client.kv_set(key, value, set_options_for_ttl(ttl)).await {
        Err(e) => Kv::Home(Err(e)),
        Ok(false) => Kv::Home(Err(HomeError::other("home kv store unavailable"))),
        Ok(true) => Kv::Home(Ok(())),
    }
}

pub async fn kv_set_nx_required(key: &str, value: &[u8], ttl: Duration) -> Kv<bool> {
    home_client!(client);
    Kv::Home(client.kv_set_nx(key, value, ttl).await)
}

pub async fn kv_del_required(keys: &[String]) -> Kv<i64> {
    home_client!(client);
    Kv::Home(client.kv_del(keys).await)
}

pub async fn kv_expire_required(key: &str, ttl: Duration) -> Kv<()> {
    home_client!(client);
    Kv::Home(client.kv_expire(key, ttl).await.map(|_| ()))
}

/// `(home_mode, value)`; failures are logged and read as a miss.
pub async fn kv_get_json_best_effort<T: DeserializeOwned>(key: &str) -> (bool, Option<T>) {
    match kv_get_json_required::<T>(key).await {
        Kv::NotHome => (false, None),
        Kv::Home(Ok(v)) => (true, v),
        Kv::Home(Err(e)) => {
            tracing::error!("home kv best-effort get failed prefix={}: {e}", log_prefix(key));
            (true, None)
        }
    }
}

pub async fn kv_set_json_best_effort<T: Serialize>(key: &str, value: &T, ttl: Duration) -> bool {
    match serde_json::to_vec(value) {
        Ok(raw) => kv_set_bytes_best_effort(key, &raw, ttl).await,
        Err(e) => {
            tracing::error!("home kv best-effort set failed prefix={}: {e}", log_prefix(key));
            false
        }
    }
}

pub async fn kv_set_bytes_best_effort(key: &str, value: &[u8], ttl: Duration) -> bool {
    match kv_set_bytes_required(key, value, ttl).await {
        Kv::NotHome => false,
        Kv::Home(Ok(())) => true,
        Kv::Home(Err(e)) => {
            tracing::error!("home kv best-effort set failed prefix={}: {e}", log_prefix(key));
            false
        }
    }
}

pub async fn kv_set_nx_best_effort(key: &str, value: &[u8], ttl: Duration) -> bool {
    match kv_set_nx_required(key, value, ttl).await {
        Kv::NotHome => false,
        Kv::Home(Ok(written)) => written,
        Kv::Home(Err(e)) => {
            tracing::error!("home kv best-effort setnx failed prefix={}: {e}", log_prefix(key));
            false
        }
    }
}

pub async fn kv_del_best_effort(keys: &[String]) -> bool {
    match kv_del_required(keys).await {
        Kv::NotHome => false,
        Kv::Home(Ok(_)) => true,
        Kv::Home(Err(e)) => {
            tracing::error!(
                "home kv best-effort del failed prefix={}: {e}",
                log_prefix(keys.first().map_or("", String::as_str))
            );
            false
        }
    }
}

pub async fn kv_expire_best_effort(key: &str, ttl: Duration) -> bool {
    match kv_expire_required(key, ttl).await {
        Kv::NotHome => false,
        Kv::Home(Ok(())) => true,
        Kv::Home(Err(e)) => {
            tracing::error!("home kv best-effort expire failed prefix={}: {e}", log_prefix(key));
            false
        }
    }
}

fn set_options_for_ttl(ttl: Duration) -> KvSetOptions {
    if ttl.is_zero() {
        KvSetOptions::default()
    } else {
        KvSetOptions { ex: ttl, ..Default::default() }
    }
}

/// `a:b:*` of `a:b:c...`, so logs never carry key material.
fn log_prefix(key: &str) -> String {
    let key = key.trim();
    if key.is_empty() {
        return "unknown".into();
    }
    let parts: Vec<&str> = key.split(':').collect();
    if parts.len() >= 2 {
        format!("{}:{}:*", parts[0], parts[1])
    } else {
        format!("{}:*", parts[0])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_prefix_hides_the_key_tail() {
        assert_eq!(log_prefix("kimi:thinking:abc"), "kimi:thinking:*");
        assert_eq!(log_prefix("solo"), "solo:*");
        assert_eq!(log_prefix("  "), "unknown");
    }

    #[test]
    fn hash_key_part_is_sha256_hex() {
        assert_eq!(hash_key_part("a"), "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb");
    }
}
