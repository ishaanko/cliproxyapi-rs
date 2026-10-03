//! xAI (Grok) reasoning replay cache (Go: cache/xai_reasoning_replay_cache.go): the final
//! assistant output items of stateless Responses turns, normalized for replay. Unlike the Codex
//! cache, assistant messages are kept, but a batch is only stored when it contains a replay
//! anchor (reasoning, function_call or custom_tool_call).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use cpa_json::{J, Value};
use parking_lot::Mutex;

use super::codex_reasoning_replay::{normalize_custom_tool_call_item, normalize_function_call_item};
use super::kv::{KvResult, Store, decode_items, encode_items, scoped_kv_key, store};
use super::{Clock, Timestamp, elapsed, ensure_cleanup_started, oldest_keys, scoped_key};
use crate::signature::inspect_grok_encrypted_content;

/// How long encrypted reasoning replay items stay in process memory.
pub const XAI_REASONING_REPLAY_CACHE_TTL: Duration = Duration::from_secs(3600);
/// Bounds process memory for replay continuity; oldest entries are evicted first.
pub const XAI_REASONING_REPLAY_CACHE_MAX_ENTRIES: usize = 10240;
/// Entries evicted at once after reaching capacity.
pub const XAI_REASONING_REPLAY_CACHE_EVICT_BATCH_SIZE: usize = 128;

/// Why a completed-turn cache write succeeded or failed, so callers can decide whether to keep
/// prior entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XaiReasoningReplayStoreStatus {
    /// Model or session was empty.
    InvalidArgs,
    /// A valid reasoning batch was written.
    Stored,
    /// The completed output had no cacheable reasoning batch (for example reasoning disabled).
    NoReplayableState,
    /// Normalization succeeded but the storage backend failed. Only Home KV mode can
    /// produce this; the in-memory cache never returns it.
    BackendError,
}

struct Entry {
    items: Vec<Vec<u8>>,
    timestamp: Timestamp,
}

pub struct XaiReasoningReplayCache {
    entries: Mutex<HashMap<String, Entry>>,
    clock: Clock,
}

static GLOBAL: LazyLock<XaiReasoningReplayCache> =
    LazyLock::new(|| XaiReasoningReplayCache::new(Clock::real()));

fn cache_key(model_name: &str, session_key: &str) -> Option<String> {
    // The session key is the continuity boundary, independent of the upstream credential so auth
    // failover preserves replay.
    scoped_key("xai-reasoning-replay", model_name, session_key)
}

fn trimmed(item: &Value, path: &str) -> String {
    item.g(path).str().trim().to_string()
}

/// Normalized items plus whether any of them is a replay anchor.
fn normalize_items(items: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    let mut normalized = Vec::with_capacity(items.len());
    let mut has_replay_anchor = false;
    for item in items {
        if let Some(normalized_item) = normalize_item(item) {
            let kind = trimmed(&cpa_json::parse(&normalized_item), "type");
            if matches!(kind.as_str(), "reasoning" | "function_call" | "custom_tool_call") {
                has_replay_anchor = true;
            }
            normalized.push(normalized_item);
        }
    }
    has_replay_anchor.then_some(normalized)
}

fn normalize_item(item: &[u8]) -> Option<Vec<u8>> {
    let item = cpa_json::parse(item);
    match trimmed(&item, "type").as_str() {
        "reasoning" => normalize_reasoning_item(&item),
        "message" => normalize_message_item(&item),
        "function_call" => normalize_function_call_item(&item),
        "custom_tool_call" => normalize_custom_tool_call_item(&item),
        _ => None,
    }
}

fn normalize_reasoning_item(item: &Value) -> Option<Vec<u8>> {
    let encrypted = item.g("encrypted_content");
    let encrypted_content = encrypted.as_str()?;
    if encrypted_content != encrypted_content.trim() {
        return None;
    }
    inspect_grok_encrypted_content(encrypted_content).ok()?;

    let mut normalized = cpa_json::parse_str(r#"{"type":"reasoning","summary":[],"content":null}"#);
    cpa_json::set(&mut normalized, "encrypted_content", encrypted_content);
    Some(cpa_json::to_vec(&normalized))
}

/// Assistant message with only `output_text` / `refusal` parts kept (Responses API refusal parts
/// use the `refusal` field, not `text`).
fn normalize_message_item(item: &Value) -> Option<Vec<u8>> {
    if !item.g("role").str().trim().eq_ignore_ascii_case("assistant") {
        return None;
    }
    let content = item.g("content");
    if !content.is_array() || content.array().is_empty() {
        return None;
    }

    let mut normalized = cpa_json::parse_str(r#"{"type":"message","role":"assistant","content":[]}"#);
    for part in content.array() {
        let next_part = match part.g("type").str().trim() {
            "output_text" => {
                let text = part.g("text");
                if !text.is_string() {
                    continue;
                }
                let mut next = cpa_json::parse_str(r#"{"type":"output_text","text":""}"#);
                cpa_json::set(&mut next, "text", text.str());
                next
            }
            "refusal" => {
                let refusal = part.g("refusal");
                if !refusal.is_string() {
                    continue;
                }
                let mut next = cpa_json::parse_str(r#"{"type":"refusal","refusal":""}"#);
                cpa_json::set(&mut next, "refusal", refusal.str());
                next
            }
            _ => continue,
        };
        cpa_json::set(&mut normalized, "content.-1", next_part);
    }
    if normalized.g("content").array().is_empty() {
        return None;
    }
    Some(cpa_json::to_vec(&normalized))
}

impl XaiReasoningReplayCache {
    pub fn new(clock: Clock) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            clock,
        }
    }

    /// The process-wide cache (real clock); starts the background purge thread on first use.
    pub fn global() -> &'static XaiReasoningReplayCache {
        ensure_cleanup_started();
        &GLOBAL
    }

    /// Stores replay items and distinguishes empty completed state from backend failures.
    pub fn store_items(&self, model_name: &str, session_key: &str, items: &[Vec<u8>]) -> XaiReasoningReplayStoreStatus {
        let Some(key) = cache_key(model_name, session_key) else {
            return XaiReasoningReplayStoreStatus::InvalidArgs;
        };
        let Some(normalized) = normalize_items(items) else {
            return XaiReasoningReplayStoreStatus::NoReplayableState;
        };
        let now = self.clock.now();
        let mut entries = self.entries.lock();
        entries.insert(key, Entry { items: normalized, timestamp: now });
        if entries.len() > XAI_REASONING_REPLAY_CACHE_MAX_ENTRIES {
            for key in oldest_keys(&entries, XAI_REASONING_REPLAY_CACHE_EVICT_BATCH_SIZE, |e| e.timestamp) {
                entries.remove(&key);
            }
        }
        XaiReasoningReplayStoreStatus::Stored
    }

    /// Stores replay items for completed response paths; true when written.
    pub fn cache_items(&self, model_name: &str, session_key: &str, items: &[Vec<u8>]) -> bool {
        self.store_items(model_name, session_key, items) == XaiReasoningReplayStoreStatus::Stored
    }

    /// The first normalized replay item.
    pub fn get_item(&self, model_name: &str, session_key: &str) -> Option<Vec<u8>> {
        self.get_items(model_name, session_key)?.into_iter().next()
    }

    /// Normalized assistant output items; refreshes the TTL (sliding expiration).
    pub fn get_items(&self, model_name: &str, session_key: &str) -> Option<Vec<Vec<u8>>> {
        let key = cache_key(model_name, session_key)?;
        let now = self.clock.now();
        let mut entries = self.entries.lock();
        let entry = entries.get_mut(&key)?;
        if elapsed(now, entry.timestamp) > XAI_REASONING_REPLAY_CACHE_TTL {
            entries.remove(&key);
            return None;
        }
        entry.timestamp = now;
        Some(entry.items.clone())
    }

    /// Removes replay state after upstream rejects it or the caller knows it is stale.
    pub fn delete_item(&self, model_name: &str, session_key: &str) {
        if let Some(key) = cache_key(model_name, session_key) {
            self.entries.lock().remove(&key);
        }
    }

    /// Clears all xAI reasoning replay state.
    pub fn clear(&self) {
        self.entries.lock().clear();
    }

    /// Drops entries older than the TTL.
    pub fn purge_expired(&self) {
        let now = self.clock.now();
        self.entries
            .lock()
            .retain(|_, entry| elapsed(now, entry.timestamp) <= XAI_REASONING_REPLAY_CACHE_TTL);
    }
}

// ---- global API: Home KV when Home mode is on, otherwise the in-process cache

fn kv_key(model_name: &str, session_key: &str) -> String {
    scoped_kv_key("cpa:xai:reasoning-replay", model_name, session_key)
}

/// Go: CacheXAIReasoningReplayItem.
pub fn cache_xai_reasoning_replay_item(model_name: &str, session_key: &str, item: &[u8]) -> bool {
    cache_xai_reasoning_replay_items(model_name, session_key, &[item.to_vec()])
}

/// Go: CacheXAIReasoningReplayItems / CacheXAIReasoningReplayItemsBestEffort.
pub fn cache_xai_reasoning_replay_items(model_name: &str, session_key: &str, items: &[Vec<u8>]) -> bool {
    store_xai_reasoning_replay_items(model_name, session_key, items) == XaiReasoningReplayStoreStatus::Stored
}

/// Go: StoreXAIReasoningReplayItems. A Home failure reports `BackendError` so callers keep the
/// previous entry.
pub fn store_xai_reasoning_replay_items(
    model_name: &str,
    session_key: &str,
    items: &[Vec<u8>],
) -> XaiReasoningReplayStoreStatus {
    use XaiReasoningReplayStoreStatus as Status;
    if cache_key(model_name, session_key).is_none() {
        return Status::InvalidArgs;
    }
    let Some(normalized) = normalize_items(items) else {
        return Status::NoReplayableState;
    };
    let written = store().and_then(|store| match store {
        Store::Local => Ok(None),
        Store::Home(backend) => {
            let raw = encode_items(&normalized)?;
            backend
                .set(&kv_key(model_name, session_key), &raw, XAI_REASONING_REPLAY_CACHE_TTL)
                .map(Some)
        }
    });
    match written {
        Ok(None) => XaiReasoningReplayCache::global().store_items(model_name, session_key, items),
        Ok(Some(true)) => Status::Stored,
        Ok(Some(false)) => Status::BackendError,
        Err(e) => {
            tracing::error!("home kv best-effort xai reasoning replay set failed prefix=cpa:xai:*: {e}");
            Status::BackendError
        }
    }
}

/// Go: GetXAIReasoningReplayItem (failures read as a miss).
pub fn get_xai_reasoning_replay_item(model_name: &str, session_key: &str) -> Option<Vec<u8>> {
    get_xai_reasoning_replay_items(model_name, session_key)?.into_iter().next()
}

/// Go: GetXAIReasoningReplayItems (failures read as a miss).
pub fn get_xai_reasoning_replay_items(model_name: &str, session_key: &str) -> Option<Vec<Vec<u8>>> {
    get_xai_reasoning_replay_items_required(model_name, session_key).ok().flatten()
}

/// Go: GetXAIReasoningReplayItemsRequired.
pub fn get_xai_reasoning_replay_items_required(model_name: &str, session_key: &str) -> KvResult<Option<Vec<Vec<u8>>>> {
    if cache_key(model_name, session_key).is_none() {
        return Ok(None);
    }
    match store()? {
        Store::Home(backend) => {
            let key = kv_key(model_name, session_key);
            let Some(raw) = backend.get(&key)? else {
                return Ok(None);
            };
            let items = decode_items(&raw)?;
            if let Err(e) = backend.expire(&key, XAI_REASONING_REPLAY_CACHE_TTL) {
                tracing::warn!("home kv xai reasoning replay expire failed prefix=cpa:xai:*: {e}");
            }
            Ok(Some(items))
        }
        Store::Local => Ok(XaiReasoningReplayCache::global().get_items(model_name, session_key)),
    }
}

/// Go: DeleteXAIReasoningReplayItem (failures ignored).
pub fn delete_xai_reasoning_replay_item(model_name: &str, session_key: &str) {
    let _ = delete_xai_reasoning_replay_item_required(model_name, session_key);
}

/// Go: DeleteXAIReasoningReplayItemRequired.
pub fn delete_xai_reasoning_replay_item_required(model_name: &str, session_key: &str) -> KvResult<()> {
    if cache_key(model_name, session_key).is_none() {
        return Ok(());
    }
    match store()? {
        Store::Home(backend) => backend.del(&kv_key(model_name, session_key)),
        Store::Local => {
            XaiReasoningReplayCache::global().delete_item(model_name, session_key);
            Ok(())
        }
    }
}

/// Go: ClearXAIReasoningReplayCache.
pub fn clear_xai_reasoning_replay_cache() {
    XaiReasoningReplayCache::global().clear();
}
