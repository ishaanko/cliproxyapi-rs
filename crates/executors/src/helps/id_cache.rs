//! Stable per-API-key identifiers and the Codex prompt cache (Go: helps/session_id_cache.go,
//! user_id_cache.go, cache_helpers.go, plus the fake Claude `user_id` generators of
//! cloak_utils.go that the user-id cache needs).
//!
//! Entries live one hour from their last access and are purged lazily every 15 minutes (Go uses
//! a cleanup goroutine). In Home mode the ids and the Codex prompt cache live in Home KV instead
//! (keys `cpa:claude:session-id:*`, `cpa:claude:user-id:*`, `cpa:codex:prompt-cache:*`), shared by
//! every node. The `*_required` functions surface Home failures, the plain ones fall back like Go.
//! The `async` functions suit async callers; the `*_blocking` and plain sync ones bridge through
//! [`home_kv`] and only touch the runtime when Home mode is on.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use cpa_home::HomeError;
use cpa_home::kv::{Kv, hash_key_part, is_home_mode, kv_del_required, kv_get_json_required, kv_set_json_best_effort, kv_set_json_required};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::helps::home_kv;

/// Lifetime of a session/user id after its last use.
pub const ID_TTL: Duration = Duration::from_secs(3600);
/// How often expired entries are purged (the Codex prompt cache sets its own `expire`).
pub const CACHE_CLEANUP_INTERVAL: Duration = Duration::from_secs(15 * 60);

struct Entry<V> {
    value: V,
    expire: Instant,
}

/// String-keyed map with per-entry expiry and lazy purging.
struct TtlMap<V> {
    inner: Mutex<(HashMap<String, Entry<V>>, Instant)>,
}

impl<V: Clone> TtlMap<V> {
    fn new() -> Self {
        Self { inner: Mutex::new((HashMap::new(), Instant::now())) }
    }

    fn purge_if_due(state: &mut (HashMap<String, Entry<V>>, Instant), now: Instant) {
        if now.saturating_duration_since(state.1) >= CACHE_CLEANUP_INTERVAL {
            state.0.retain(|_, e| e.expire > now);
            state.1 = now;
        }
    }

    /// Valid (unexpired, accepted by `valid`) value for `key`, expiry pushed to `now + ttl`
    /// when `refresh`; otherwise stores `make()` under `key`.
    fn get_or_insert(
        &self,
        key: &str,
        now: Instant,
        ttl: Duration,
        valid: impl Fn(&V) -> bool,
        make: impl FnOnce() -> V,
    ) -> V {
        let mut state = self.inner.lock();
        Self::purge_if_due(&mut state, now);
        if let Some(e) = state.0.get_mut(key)
            && e.expire > now
            && valid(&e.value)
        {
            e.expire = now + ttl;
            return e.value.clone();
        }
        let value = make();
        state.0.insert(key.to_string(), Entry { value: value.clone(), expire: now + ttl });
        value
    }

    fn get(&self, key: &str, now: Instant) -> Option<V> {
        let mut state = self.inner.lock();
        Self::purge_if_due(&mut state, now);
        state.0.get(key).filter(|e| e.expire > now).map(|e| e.value.clone())
    }

    fn set(&self, key: &str, value: V, expire: Instant, now: Instant) {
        let mut state = self.inner.lock();
        Self::purge_if_due(&mut state, now);
        state.0.insert(key.to_string(), Entry { value, expire });
    }
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Per-API-key stable id caches with an injectable clock.
pub struct IdCaches {
    session_ids: TtlMap<String>,
    user_ids: TtlMap<String>,
}

impl IdCaches {
    pub fn new() -> Self {
        Self { session_ids: TtlMap::new(), user_ids: TtlMap::new() }
    }

    /// Stable session UUID per API key (TTL refreshed on access); a fresh UUID for an empty key.
    pub fn session_id_at(&self, api_key: &str, now: Instant) -> String {
        if api_key.is_empty() {
            return Uuid::new_v4().to_string();
        }
        self.session_ids.get_or_insert(&sha256_hex(api_key), now, ID_TTL, |v| !v.is_empty(), || Uuid::new_v4().to_string())
    }

    /// Stable fake Claude `user_id` per API key; its `session_id` is the key's cached session id.
    pub fn user_id_at(&self, api_key: &str, now: Instant) -> String {
        let make = || generate_fake_user_id_with_session_id(&self.session_id_at(api_key, now));
        if api_key.is_empty() {
            return make();
        }
        self.user_ids.get_or_insert(&sha256_hex(api_key), now, ID_TTL, |v| is_valid_user_id(v), make)
    }
}

impl Default for IdCaches {
    fn default() -> Self {
        Self::new()
    }
}

static GLOBAL: LazyLock<IdCaches> = LazyLock::new(IdCaches::new);

/// Home KV key of an API key's session id (Go: claudeSessionIDKVKey).
fn session_id_kv_key(api_key: &str) -> String {
    format!("cpa:claude:session-id:{}", hash_key_part(api_key))
}

/// Home KV key of an API key's fake user id (Go: claudeUserIDKVKey).
fn user_id_kv_key(api_key: &str) -> String {
    format!("cpa:claude:user-id:{}", hash_key_part(api_key))
}

/// Trimmed text of a stored value, `None` when absent or blank.
fn stored_text(raw: Option<Vec<u8>>) -> Option<String> {
    let raw = raw?;
    let text = String::from_utf8_lossy(&raw);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Home branch of [`cached_session_id_required`]: reuse and refresh the stored id, else
/// `SET NX` a new one and read back the winner.
async fn session_id_home(client: &cpa_home::Client, api_key: &str) -> Result<String, HomeError> {
    let key = session_id_kv_key(api_key);
    if let Some(id) = stored_text(client.kv_get(&key).await?) {
        client.kv_expire(&key, ID_TTL).await?;
        return Ok(id);
    }
    let new_id = Uuid::new_v4().to_string();
    client.kv_set_nx(&key, new_id.as_bytes(), ID_TTL).await?;
    stored_text(client.kv_get(&key).await?).ok_or_else(|| HomeError::other("home kv session id missing after set"))
}

/// Home branch of [`cached_user_id_required`]; a new id embeds the shared session id.
async fn user_id_home(client: &cpa_home::Client, api_key: &str) -> Result<String, HomeError> {
    let key = user_id_kv_key(api_key);
    if let Some(id) = stored_text(client.kv_get(&key).await?).filter(|id| is_valid_user_id(id)) {
        client.kv_expire(&key, ID_TTL).await?;
        return Ok(id);
    }
    let session_id = cached_session_id_required(api_key).await?;
    let new_id = generate_fake_user_id_with_session_id(&session_id);
    client.kv_set_nx(&key, new_id.as_bytes(), ID_TTL).await?;
    stored_text(client.kv_get(&key).await?)
        .filter(|id| is_valid_user_id(id))
        .ok_or_else(|| HomeError::other("home kv user id missing after set"))
}

/// Stable session UUID per API key for request-time paths (Go: CachedSessionIDRequired). A fresh
/// UUID for an empty key; in Home mode the id is shared through Home KV and failures are errors.
pub async fn cached_session_id_required(api_key: &str) -> Result<String, HomeError> {
    if api_key.is_empty() {
        return Ok(Uuid::new_v4().to_string());
    }
    match home_kv::client()? {
        Some(client) => session_id_home(&client, api_key).await,
        None => Ok(GLOBAL.session_id_at(api_key, Instant::now())),
    }
}

/// [`cached_session_id_required`] for synchronous callers.
pub fn cached_session_id_required_blocking(api_key: &str) -> Result<String, HomeError> {
    if api_key.is_empty() {
        return Ok(Uuid::new_v4().to_string());
    }
    match home_kv::client()? {
        Some(client) => home_kv::call(session_id_home(&client, api_key)),
        None => Ok(GLOBAL.session_id_at(api_key, Instant::now())),
    }
}

/// Stable fake Claude `user_id` per API key for request-time paths (Go: CachedUserIDRequired).
pub async fn cached_user_id_required(api_key: &str) -> Result<String, HomeError> {
    if api_key.is_empty() {
        return Ok(generate_fake_user_id());
    }
    match home_kv::client()? {
        Some(client) => user_id_home(&client, api_key).await,
        None => Ok(GLOBAL.user_id_at(api_key, Instant::now())),
    }
}

/// [`cached_user_id_required`] for synchronous callers.
pub fn cached_user_id_required_blocking(api_key: &str) -> Result<String, HomeError> {
    if api_key.is_empty() {
        return Ok(generate_fake_user_id());
    }
    match home_kv::client()? {
        Some(client) => home_kv::call(user_id_home(&client, api_key)),
        None => Ok(GLOBAL.user_id_at(api_key, Instant::now())),
    }
}

/// Stable session UUID per API key (Go: CachedSessionID); a fresh UUID when Home fails.
pub fn cached_session_id(api_key: &str) -> String {
    match cached_session_id_required_blocking(api_key) {
        Ok(id) if !id.is_empty() => id,
        _ => Uuid::new_v4().to_string(),
    }
}

/// Stable fake Claude `user_id` per API key (Go: CachedUserID); a fresh one when Home fails.
pub fn cached_user_id(api_key: &str) -> String {
    match cached_user_id_required_blocking(api_key) {
        Ok(id) if !id.is_empty() => id,
        _ => generate_fake_user_id(),
    }
}

/// `metadata.user_id` in the JSON-string format of Claude Code 2.1.78+ with a random device id.
pub fn generate_fake_user_id() -> String {
    generate_fake_user_id_with_session_id(&Uuid::new_v4().to_string())
}

/// [`generate_fake_user_id`] with the given session id (replaced by a fresh UUID when it is not
/// a valid UUID).
pub fn generate_fake_user_id_with_session_id(session_id: &str) -> String {
    let session_id = match Uuid::parse_str(session_id) {
        Ok(_) => session_id.to_string(),
        Err(_) => Uuid::new_v4().to_string(),
    };
    let device: [u8; 32] = rand::random();
    format!(
        r#"{{"device_id":"{}","account_uuid":"","session_id":"{}"}}"#,
        hex::encode(device),
        session_id
    )
}

/// Checks the Claude Code 2.1.220 `metadata.user_id` shape: 64 lowercase hex `device_id`, UUID
/// `session_id`, empty or UUID `account_uuid`.
pub fn is_valid_user_id(user_id: &str) -> bool {
    let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(user_id) else {
        return false;
    };
    // Go decodes into a struct of strings: a wrong type is an error, null or absent is "".
    let field = |name: &str| match obj.get(name) {
        None | Some(serde_json::Value::Null) => Some(""),
        Some(serde_json::Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    let (Some(device), Some(account), Some(session)) = (field("device_id"), field("account_uuid"), field("session_id")) else {
        return false;
    };
    let device_ok = device.len() == 64 && device.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    device_ok && Uuid::parse_str(session).is_ok() && (account.is_empty() || Uuid::parse_str(account).is_ok())
}

// ---------------------------------------------------------------- Codex prompt cache

/// Cached Codex `prompt_cache_key` id and its expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexCache {
    pub id: String,
    pub expire: Instant,
}

static CODEX_CACHE: LazyLock<TtlMap<CodexCache>> = LazyLock::new(TtlMap::new);

/// Home KV value of a [`CodexCache`], with Go's field names and a wall-clock expiry
/// (`{"ID":"...","Expire":"2026-01-02T03:04:05.678Z"}`). A missing `Expire` reads as expired.
#[derive(Serialize, Deserialize)]
struct CodexCacheWire {
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(rename = "Expire", default)]
    expire: Option<DateTime<Utc>>,
}

impl CodexCacheWire {
    fn new(cache: &CodexCache, now: Instant) -> Self {
        let remaining = chrono::Duration::from_std(cache.expire.saturating_duration_since(now)).unwrap_or_default();
        Self { id: cache.id.clone(), expire: Some(Utc::now() + remaining) }
    }

    /// The entry with a monotonic expiry, `None` when its wall-clock expiry has passed.
    fn into_live(self, now: Instant) -> Option<CodexCache> {
        let remaining = (self.expire? - Utc::now()).to_std().ok()?;
        Some(CodexCache { id: self.id, expire: now + remaining })
    }
}

/// The cached entry for `key` for request-time paths (Go: GetCodexCacheRequired); `Ok(None)` when
/// absent or expired. In Home mode an expired entry is deleted and failures are errors.
pub async fn get_codex_cache_required(key: &str) -> Result<Option<CodexCache>, HomeError> {
    match kv_get_json_required::<CodexCacheWire>(key).await {
        Kv::NotHome => Ok(CODEX_CACHE.get(key, Instant::now())),
        Kv::Home(Err(e)) => Err(e),
        Kv::Home(Ok(None)) => Ok(None),
        Kv::Home(Ok(Some(wire))) => {
            let live = wire.into_live(Instant::now());
            if live.is_none() {
                let _ = kv_del_required(&[key.to_string()]).await;
            }
            Ok(live)
        }
    }
}

/// The cached entry for `key`, `None` when absent, expired or (Home mode) unreadable (Go:
/// GetCodexCache).
pub fn get_codex_cache(key: &str) -> Option<CodexCache> {
    if !is_home_mode() {
        return CODEX_CACHE.get(key, Instant::now());
    }
    home_kv::call(get_codex_cache_required(key)).ok().flatten()
}

/// Stores a cache entry for request-time paths (Go: SetCodexCacheRequired); an already expired
/// entry is dropped. In Home mode the entry is written with its remaining lifetime as TTL and a
/// failure is an error.
pub async fn set_codex_cache_required(key: &str, cache: CodexCache) -> Result<(), HomeError> {
    let now = Instant::now();
    let ttl = cache.expire.saturating_duration_since(now);
    if ttl.is_zero() {
        return Ok(());
    }
    match kv_set_json_required(key, &CodexCacheWire::new(&cache, now), ttl).await {
        Kv::NotHome => {
            CODEX_CACHE.set(key, cache.clone(), cache.expire, now);
            Ok(())
        }
        Kv::Home(result) => result,
    }
}

/// Stores a cache entry without failing completed responses (Go: SetCodexCacheBestEffort); false
/// when nothing was stored (already expired, or a Home failure that is logged).
pub async fn set_codex_cache_best_effort(key: &str, cache: CodexCache) -> bool {
    let now = Instant::now();
    let ttl = cache.expire.saturating_duration_since(now);
    if ttl.is_zero() {
        return false;
    }
    if is_home_mode() {
        return kv_set_json_best_effort(key, &CodexCacheWire::new(&cache, now), ttl).await;
    }
    CODEX_CACHE.set(key, cache.clone(), cache.expire, now);
    true
}

/// Stores a cache entry; false (nothing stored) when it is already expired or Home failed (Go:
/// SetCodexCache).
pub fn set_codex_cache(key: &str, cache: CodexCache) -> bool {
    if !is_home_mode() {
        let now = Instant::now();
        if cache.expire <= now {
            return false;
        }
        CODEX_CACHE.set(key, cache.clone(), cache.expire, now);
        return true;
    }
    home_kv::run_blocking(set_codex_cache_best_effort(key, cache)).unwrap_or(false)
}

/// Key of the prompt cache for a model and user scope (matches Go's Home KV key layout).
pub fn codex_prompt_cache_key(model_name: &str, user_scope: &str) -> String {
    format!("cpa:codex:prompt-cache:{}:{}", sha256_hex(model_name), sha256_hex(user_scope))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_stable_until_ttl_lapses() {
        let c = IdCaches::new();
        let t0 = Instant::now();
        let a = c.session_id_at("key-1", t0);
        assert!(Uuid::parse_str(&a).is_ok());
        assert_eq!(c.session_id_at("key-1", t0 + Duration::from_secs(1800)), a);
        // Access at 30m refreshed the TTL, so 80m is still within an hour of the last use.
        assert_eq!(c.session_id_at("key-1", t0 + Duration::from_secs(4800)), a);
        assert_ne!(c.session_id_at("key-1", t0 + Duration::from_secs(4800 + 3601)), a);
        assert_ne!(c.session_id_at("key-2", t0), a);
        // An empty key is never cached.
        assert_ne!(c.session_id_at("", t0), c.session_id_at("", t0));
    }

    #[test]
    fn user_id_embeds_cached_session_and_validates() {
        let c = IdCaches::new();
        let t0 = Instant::now();
        let uid = c.user_id_at("key-1", t0);
        assert!(is_valid_user_id(&uid));
        assert_eq!(c.user_id_at("key-1", t0), uid);
        let parsed: serde_json::Value = serde_json::from_str(&uid).unwrap();
        assert_eq!(parsed["session_id"], c.session_id_at("key-1", t0));
        assert_eq!(parsed["account_uuid"], "");
        assert!(!is_valid_user_id("not json"));
        assert!(!is_valid_user_id(r#"{"device_id":"abc","session_id":"x"}"#));
        assert!(!is_valid_user_id(&uid.replace("account_uuid\":\"\"", "account_uuid\":\"nope\"")));
        // A non-UUID session id is replaced.
        let fixed = generate_fake_user_id_with_session_id("nope");
        assert!(is_valid_user_id(&fixed));
    }

    #[test]
    fn codex_cache_respects_expiry() {
        assert!(!set_codex_cache("k-expired", CodexCache { id: "x".into(), expire: Instant::now() }));
        assert!(get_codex_cache("k-expired").is_none());
        let key = codex_prompt_cache_key("gpt-5", "user");
        assert!(set_codex_cache(&key, CodexCache { id: "id1".into(), expire: Instant::now() + Duration::from_secs(60) }));
        assert_eq!(get_codex_cache(&key).unwrap().id, "id1");
        assert_eq!(key.len(), "cpa:codex:prompt-cache:".len() + 64 + 1 + 64);
    }
}
