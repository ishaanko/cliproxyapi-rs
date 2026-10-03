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
pub struct KvError {
    message: String,
    cas_unsupported: bool,
}

impl KvError {
    pub fn new(message: impl fmt::Display) -> Self {
        KvError { message: message.to_string(), cas_unsupported: false }
    }

    /// Home predates compare-and-swap (Go: `homekv.ErrCompareAndSwapUnsupported`).
    pub fn compare_and_swap_unsupported(message: impl fmt::Display) -> Self {
        KvError { message: message.to_string(), cas_unsupported: true }
    }

    pub fn is_compare_and_swap_unsupported(&self) -> bool {
        self.cas_unsupported
    }
}

impl fmt::Display for KvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
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

/// `<prefix>:<sha256(a)>:<sha256(b)>` with both parts trimmed, the Home key of a replay cache
/// entry (`prefix` such as `cpa:kimi:thinking-replay`).
pub(crate) fn scoped_kv_key(prefix: &str, a: &str, b: &str) -> String {
    format!("{prefix}:{}:{}", hash_key_part(a.trim()), hash_key_part(b.trim()))
}

/// Compact JSON text of `raw` as Go's `json.Marshal` emits a `json.RawMessage` (whitespace
/// dropped, key order and number text kept, `<`, `>`, `&`, U+2028 and U+2029 escaped).
pub(crate) fn compact_raw_json(raw: &[u8]) -> KvResult<String> {
    let value: serde_json::Value = serde_json::from_slice(raw).map_err(KvError::new)?;
    let text = serde_json::to_string(&value).map_err(KvError::new)?;
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    Ok(out)
}

/// Home value of a `[][]byte`: a JSON array of base64 strings (how Go's `json.Marshal` stores
/// byte slices), used by the Codex, xAI and Antigravity caches.
pub(crate) fn encode_items(items: &[Vec<u8>]) -> KvResult<Vec<u8>> {
    use base64::Engine as _;
    let encoded: Vec<String> = items.iter().map(|item| base64::engine::general_purpose::STANDARD.encode(item)).collect();
    serde_json::to_vec(&encoded).map_err(KvError::new)
}

/// Inverse of [`encode_items`]; JSON `null` (or a null element) reads as empty.
pub(crate) fn decode_items(raw: &[u8]) -> KvResult<Vec<Vec<u8>>> {
    use base64::Engine as _;
    let encoded: Option<Vec<Option<String>>> = serde_json::from_slice(raw).map_err(KvError::new)?;
    encoded
        .unwrap_or_default()
        .into_iter()
        .map(|item| match item {
            Some(text) => base64::engine::general_purpose::STANDARD.decode(text).map_err(KvError::new),
            None => Ok(Vec::new()),
        })
        .collect()
}

/// Reads `key`, first installing `tombstone` when it is absent (up to four attempts), so a stale
/// writer can never publish over a state that a newer request has already observed as a miss.
/// Values over `max_bytes` are refused. `label` names the cache in error messages.
pub(crate) fn read_or_reserve(
    backend: &dyn KvBackend,
    key: &str,
    ttl: Duration,
    max_bytes: usize,
    label: &str,
    tombstone: impl Fn() -> KvResult<Vec<u8>>,
) -> KvResult<Vec<u8>> {
    for _ in 0..4 {
        if let Some(raw) = backend.get(key)? {
            if raw.len() > max_bytes {
                return Err(KvError::new(format!("{label} value exceeds size limit")));
            }
            return Ok(raw);
        }
        let reservation = tombstone()?;
        if backend.compare_and_swap(key, None, &reservation, ttl)? {
            return Ok(reservation);
        }
    }
    Err(KvError::new(format!("could not reserve absent {label} state")))
}
