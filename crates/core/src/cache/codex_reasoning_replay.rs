//! Codex (GPT) reasoning replay cache (Go: cache/codex_reasoning_replay_cache.go): the final
//! assistant output items of stateless Responses turns, normalized to the minimal shape accepted
//! by Responses input replay, with turn-boundary markers bounding cumulative state.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use cpa_json::{J, Res, Value};
use parking_lot::Mutex;

use super::kv::{KvBackend, KvResult, Store, decode_items, encode_items, scoped_kv_key, store};
use super::{Clock, Timestamp, elapsed, ensure_cleanup_started, oldest_keys, scoped_key};
use crate::signature::inspect_gpt_reasoning_signature;

/// Item type of an internal turn-boundary marker.
pub const CODEX_REASONING_REPLAY_TURN_TYPE: &str = "cpa_codex_replay_turn";
/// How long encrypted reasoning replay items stay in process memory.
pub const CODEX_REASONING_REPLAY_CACHE_TTL: Duration = Duration::from_secs(3600);
/// Bounds process memory for replay continuity; oldest entries are evicted first.
pub const CODEX_REASONING_REPLAY_CACHE_MAX_ENTRIES: usize = 10240;
/// Bounds cumulative state for one agent.
pub const CODEX_REASONING_REPLAY_CACHE_MAX_TURNS_PER_ENTRY: usize = 256;
/// Bounds cumulative serialized items for one agent.
pub const CODEX_REASONING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY: usize = 16 << 20;
/// Entries evicted at once after reaching capacity, so high write volume does not rescan the map
/// every turn.
pub const CODEX_REASONING_REPLAY_CACHE_EVICT_BATCH_SIZE: usize = 128;

struct Entry {
    items: Vec<Vec<u8>>,
    timestamp: Timestamp,
}

pub struct CodexReasoningReplayCache {
    entries: Mutex<HashMap<String, Entry>>,
    clock: Clock,
}

static GLOBAL: LazyLock<CodexReasoningReplayCache> =
    LazyLock::new(|| CodexReasoningReplayCache::new(Clock::real()));

fn cache_key(model_name: &str, session_key: &str) -> Option<String> {
    // The session key is the continuity boundary, independent of the upstream credential so auth
    // failover preserves replay.
    scoped_key("codex-reasoning-replay", model_name, session_key)
}

/// Trimmed string at `path` of an item (`""` when absent).
fn trimmed(item: &Value, path: &str) -> String {
    item.g(path).str().trim().to_string()
}

fn is_turn_item(item: &[u8]) -> bool {
    trimmed(&cpa_json::parse(item), "type") == CODEX_REASONING_REPLAY_TURN_TYPE
}

/// Appends one completed turn to existing replay state. A turn whose id already exists is not
/// added twice; state that does not start with a turn marker is discarded.
fn append_turn(existing: Vec<Vec<u8>>, turn: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut existing = existing;
    if existing
        .first()
        .is_some_and(|first| !is_turn_item(first))
    {
        existing.clear();
    }
    let mut turn_id = String::new();
    if let Some(first) = turn.first() {
        let parsed = cpa_json::parse(first);
        if trimmed(&parsed, "type") == CODEX_REASONING_REPLAY_TURN_TYPE {
            turn_id = trimmed(&parsed, "id");
        }
    }
    if !turn_id.is_empty() {
        for item in &existing {
            let parsed = cpa_json::parse(item);
            if trimmed(&parsed, "type") == CODEX_REASONING_REPLAY_TURN_TYPE && trimmed(&parsed, "id") == turn_id {
                return trim_items(existing);
            }
        }
    }
    existing.extend(turn.iter().cloned());
    trim_items(existing)
}

/// Drops whole oldest turns until the turn-count and byte bounds hold; a single oversized turn
/// leaves nothing.
fn trim_items(mut items: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    loop {
        let mut turn_starts = vec![0usize];
        let mut total_bytes = 0usize;
        for (index, item) in items.iter().enumerate() {
            total_bytes += item.len();
            if index > 0 && is_turn_item(item) {
                turn_starts.push(index);
            }
        }
        if turn_starts.len() <= CODEX_REASONING_REPLAY_CACHE_MAX_TURNS_PER_ENTRY
            && total_bytes <= CODEX_REASONING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY
        {
            return items;
        }
        if turn_starts.len() <= 1 {
            return Vec::new();
        }
        items.drain(..turn_starts[1]);
    }
}

fn normalize_items(items: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    let normalized: Vec<Vec<u8>> = items.iter().filter_map(|item| normalize_item(item)).collect();
    let normalized = trim_items(normalized);
    (!normalized.is_empty()).then_some(normalized)
}

fn normalize_item(item: &[u8]) -> Option<Vec<u8>> {
    let item = cpa_json::parse(item);
    match trimmed(&item, "type").as_str() {
        CODEX_REASONING_REPLAY_TURN_TYPE => normalize_turn(&item),
        "reasoning" => normalize_reasoning_item(&item),
        "function_call" => normalize_function_call_item(&item),
        "custom_tool_call" => normalize_custom_tool_call_item(&item),
        _ => None,
    }
}

fn normalize_turn(item: &Value) -> Option<Vec<u8>> {
    let turn_id = trimmed(item, "id");
    if turn_id.is_empty() {
        return None;
    }
    let mut normalized = cpa_json::parse_str(&format!(r#"{{"type":"{CODEX_REASONING_REPLAY_TURN_TYPE}"}}"#));
    cpa_json::set(&mut normalized, "id", turn_id);
    for key in ["assistant_fingerprint", "request_fingerprint"] {
        let fingerprint = trimmed(item, key);
        if !fingerprint.is_empty() {
            cpa_json::set(&mut normalized, key, fingerprint);
        }
    }
    let call_ids = item.g("call_ids");
    if call_ids.is_array() {
        for call_id in call_ids.array() {
            let call_id = call_id.str().trim().to_string();
            if !call_id.is_empty() {
                cpa_json::set(&mut normalized, "call_ids.-1", call_id);
            }
        }
    }
    Some(cpa_json::to_vec(&normalized))
}

fn normalize_reasoning_item(item: &Value) -> Option<Vec<u8>> {
    let encrypted = item.g("encrypted_content");
    let encrypted_content = encrypted.as_str()?;
    if encrypted_content != encrypted_content.trim() {
        return None;
    }
    inspect_gpt_reasoning_signature(encrypted_content).ok()?;

    let mut normalized = cpa_json::parse_str(r#"{"type":"reasoning","summary":[],"content":null}"#);
    cpa_json::set(&mut normalized, "encrypted_content", encrypted_content);
    Some(cpa_json::to_vec(&normalized))
}

/// `function_call` -> `{type, call_id, name, arguments}` (string arguments required); also used
/// by the xAI cache.
pub(super) fn normalize_function_call_item(item: &Value) -> Option<Vec<u8>> {
    let call_id = trimmed(item, "call_id");
    let name = trimmed(item, "name");
    let arguments = item.g("arguments");
    if call_id.is_empty() || name.is_empty() || !arguments.is_string() {
        return None;
    }
    let mut normalized = cpa_json::parse_str(r#"{"type":"function_call"}"#);
    cpa_json::set(&mut normalized, "call_id", call_id);
    cpa_json::set(&mut normalized, "name", name);
    cpa_json::set(&mut normalized, "arguments", arguments.str());
    Some(cpa_json::to_vec(&normalized))
}

/// `custom_tool_call` -> `{type, status, call_id, name, input}`; also used by the xAI cache.
pub(super) fn normalize_custom_tool_call_item(item: &Value) -> Option<Vec<u8>> {
    let call_id = trimmed(item, "call_id");
    let name = trimmed(item, "name");
    let input = item.g("input");
    if call_id.is_empty() || name.is_empty() || !input.exists() {
        return None;
    }
    let mut normalized = cpa_json::parse_str(r#"{"type":"custom_tool_call","status":"completed"}"#);
    let status = trimmed(item, "status");
    if !status.is_empty() {
        cpa_json::set(&mut normalized, "status", status);
    }
    cpa_json::set(&mut normalized, "call_id", call_id);
    cpa_json::set(&mut normalized, "name", name);
    set_input(&mut normalized, &input);
    Some(cpa_json::to_vec(&normalized))
}

/// Strings are set as strings, any other JSON value verbatim.
fn set_input(normalized: &mut Value, input: &Res<'_>) {
    if input.is_string() {
        cpa_json::set(normalized, "input", input.str());
    } else {
        cpa_json::set(normalized, "input", input.value());
    }
}

impl CodexReasoningReplayCache {
    pub fn new(clock: Clock) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            clock,
        }
    }

    /// The process-wide cache (real clock); starts the background purge thread on first use.
    pub fn global() -> &'static CodexReasoningReplayCache {
        ensure_cleanup_started();
        &GLOBAL
    }

    /// Stores the final assistant output items needed to replay a stateless next turn, replacing
    /// any previous state. False when nothing replayable remains after normalization.
    pub fn cache_items(&self, model_name: &str, session_key: &str, items: &[Vec<u8>]) -> bool {
        let Some(key) = cache_key(model_name, session_key) else {
            return false;
        };
        let Some(normalized) = normalize_items(items) else {
            return false;
        };
        let now = self.clock.now();
        let mut entries = self.entries.lock();
        entries.insert(key, Entry { items: normalized, timestamp: now });
        if entries.len() > CODEX_REASONING_REPLAY_CACHE_MAX_ENTRIES {
            evict_oldest(&mut entries, CODEX_REASONING_REPLAY_CACHE_EVICT_BATCH_SIZE);
        }
        true
    }

    /// Appends one completed turn to existing replay state (expired state is discarded first).
    pub fn append_items_best_effort(&self, model_name: &str, session_key: &str, items: &[Vec<u8>]) -> bool {
        let Some(key) = cache_key(model_name, session_key) else {
            return false;
        };
        let Some(normalized) = normalize_items(items) else {
            return false;
        };
        let now = self.clock.now();
        let mut entries = self.entries.lock();
        let existing = match entries.remove(&key) {
            Some(entry) if elapsed(now, entry.timestamp) <= CODEX_REASONING_REPLAY_CACHE_TTL => entry.items,
            _ => Vec::new(),
        };
        let combined = append_turn(existing, &normalized);
        entries.insert(key, Entry { items: combined, timestamp: now });
        if entries.len() > CODEX_REASONING_REPLAY_CACHE_MAX_ENTRIES {
            evict_oldest(&mut entries, CODEX_REASONING_REPLAY_CACHE_EVICT_BATCH_SIZE);
        }
        true
    }

    /// The first normalized upstream replay item (turn markers skipped).
    pub fn get_item(&self, model_name: &str, session_key: &str) -> Option<Vec<u8>> {
        self.get_items(model_name, session_key)?
            .into_iter()
            .find(|item| !is_turn_item(item))
    }

    /// Normalized assistant output items; refreshes the TTL (sliding expiration).
    pub fn get_items(&self, model_name: &str, session_key: &str) -> Option<Vec<Vec<u8>>> {
        let key = cache_key(model_name, session_key)?;
        let now = self.clock.now();
        let mut entries = self.entries.lock();
        let entry = entries.get_mut(&key)?;
        if elapsed(now, entry.timestamp) > CODEX_REASONING_REPLAY_CACHE_TTL {
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

    /// Clears all Codex reasoning replay state.
    pub fn clear(&self) {
        self.entries.lock().clear();
    }

    /// Drops entries older than the TTL.
    pub fn purge_expired(&self) {
        let now = self.clock.now();
        self.entries
            .lock()
            .retain(|_, entry| elapsed(now, entry.timestamp) <= CODEX_REASONING_REPLAY_CACHE_TTL);
    }

    #[cfg(test)]
    pub(crate) fn entry_count(&self) -> usize {
        self.entries.lock().len()
    }
}

fn evict_oldest(entries: &mut HashMap<String, Entry>, count: usize) {
    for key in oldest_keys(entries, count, |e| e.timestamp) {
        entries.remove(&key);
    }
}

// ---- global API: Home KV when Home mode is on, otherwise the in-process cache

const KV_PREFIX: &str = "cpa:codex:reasoning-replay";

/// Attempts of the Home append loop before giving up on contention.
const MAX_CAS_ATTEMPTS: usize = 32;

fn kv_key(model_name: &str, session_key: &str) -> String {
    scoped_kv_key(KV_PREFIX, model_name, session_key)
}

/// Go: CacheCodexReasoningReplayItem.
pub fn cache_codex_reasoning_replay_item(model_name: &str, session_key: &str, item: &[u8]) -> bool {
    cache_codex_reasoning_replay_items(model_name, session_key, &[item.to_vec()])
}

/// Go: CacheCodexReasoningReplayItems / CacheCodexReasoningReplayItemsBestEffort.
pub fn cache_codex_reasoning_replay_items(model_name: &str, session_key: &str, items: &[Vec<u8>]) -> bool {
    if cache_key(model_name, session_key).is_none() {
        return false;
    }
    let Some(normalized) = normalize_items(items) else {
        return false;
    };
    let result = store().and_then(|store| match store {
        Store::Local => Ok(None),
        Store::Home(backend) => {
            let raw = encode_items(&normalized)?;
            backend
                .set(&kv_key(model_name, session_key), &raw, CODEX_REASONING_REPLAY_CACHE_TTL)
                .map(Some)
        }
    });
    match result {
        Ok(Some(written)) => written,
        Ok(None) => CodexReasoningReplayCache::global().cache_items(model_name, session_key, items),
        Err(e) => {
            tracing::error!("home kv best-effort codex reasoning replay set failed prefix=cpa:codex:*: {e}");
            false
        }
    }
}

/// Appends one completed turn through compare-and-swap on the stored value.
fn home_append(backend: &dyn KvBackend, key: &str, normalized: &[Vec<u8>]) -> KvResult<bool> {
    for _ in 0..MAX_CAS_ATTEMPTS {
        let existing_raw = backend.get(key)?;
        let existing = match &existing_raw {
            Some(raw) => decode_items(raw)?,
            None => Vec::new(),
        };
        let raw = encode_items(&append_turn(existing, normalized))?;
        if backend.compare_and_swap(key, existing_raw.as_deref(), &raw, CODEX_REASONING_REPLAY_CACHE_TTL)? {
            return Ok(true);
        }
    }
    tracing::warn!("home kv best-effort codex reasoning replay append exhausted compare-and-swap attempts");
    Ok(false)
}

/// Go: AppendCodexReasoningReplayItemsBestEffort.
pub fn append_codex_reasoning_replay_items_best_effort(model_name: &str, session_key: &str, items: &[Vec<u8>]) -> bool {
    if cache_key(model_name, session_key).is_none() {
        return false;
    }
    let Some(normalized) = normalize_items(items) else {
        return false;
    };
    match store() {
        Ok(Store::Local) => {
            CodexReasoningReplayCache::global().append_items_best_effort(model_name, session_key, items)
        }
        Ok(Store::Home(backend)) => home_append(backend, &kv_key(model_name, session_key), &normalized)
            .unwrap_or_else(|e| {
                tracing::error!("home kv best-effort codex reasoning replay append failed prefix=cpa:codex:*: {e}");
                false
            }),
        Err(e) => {
            tracing::error!("home kv best-effort codex reasoning replay append failed prefix=cpa:codex:*: {e}");
            false
        }
    }
}

/// Go: GetCodexReasoningReplayItem (failures read as a miss).
pub fn get_codex_reasoning_replay_item(model_name: &str, session_key: &str) -> Option<Vec<u8>> {
    get_codex_reasoning_replay_items(model_name, session_key)?
        .into_iter()
        .find(|item| !is_turn_item(item))
}

/// Go: GetCodexReasoningReplayItems (failures read as a miss).
pub fn get_codex_reasoning_replay_items(model_name: &str, session_key: &str) -> Option<Vec<Vec<u8>>> {
    get_codex_reasoning_replay_items_required(model_name, session_key).ok().flatten()
}

/// Go: GetCodexReasoningReplayItemsRequired.
pub fn get_codex_reasoning_replay_items_required(
    model_name: &str,
    session_key: &str,
) -> KvResult<Option<Vec<Vec<u8>>>> {
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
            backend.expire(&key, CODEX_REASONING_REPLAY_CACHE_TTL)?;
            Ok(Some(items))
        }
        Store::Local => Ok(CodexReasoningReplayCache::global().get_items(model_name, session_key)),
    }
}

/// Go: DeleteCodexReasoningReplayItem (failures ignored).
pub fn delete_codex_reasoning_replay_item(model_name: &str, session_key: &str) {
    let _ = delete_codex_reasoning_replay_item_required(model_name, session_key);
}

/// Go: DeleteCodexReasoningReplayItemRequired.
pub fn delete_codex_reasoning_replay_item_required(model_name: &str, session_key: &str) -> KvResult<()> {
    if cache_key(model_name, session_key).is_none() {
        return Ok(());
    }
    match store()? {
        Store::Home(backend) => backend.del(&kv_key(model_name, session_key)),
        Store::Local => {
            CodexReasoningReplayCache::global().delete_item(model_name, session_key);
            Ok(())
        }
    }
}

/// Go: ClearCodexReasoningReplayCache.
pub fn clear_codex_reasoning_replay_cache() {
    CodexReasoningReplayCache::global().clear();
}
