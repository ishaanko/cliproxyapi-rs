//! Kimi thinking replay cache (Go: cache/kimi_thinking_replay_cache.go): one complete signed
//! assistant content array per (model family, session), guarded by a generation snapshot so a
//! slow request cannot overwrite or delete newer state.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::value::RawValue;

use super::kv::{KvBackend, KvError, KvResult, Store, compact_raw_json, read_or_reserve, scoped_kv_key, store};
use super::{Clock, Timestamp, elapsed, ensure_cleanup_started, new_uuid, oldest_keys, scoped_key};

/// How long signed assistant content stays replayable.
pub const KIMI_THINKING_REPLAY_CACHE_TTL: Duration = Duration::from_secs(3600);
/// Bounds process memory used for replay continuity.
pub const KIMI_THINKING_REPLAY_CACHE_MAX_ENTRIES: usize = 10240;
/// Entries evicted at once after reaching capacity.
pub const KIMI_THINKING_REPLAY_CACHE_EVICT_BATCH_SIZE: usize = 128;
/// Bounds one complete assistant content array.
pub const KIMI_THINKING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY: usize = 8 << 20;
/// Prevents pathological content arrays.
pub const KIMI_THINKING_REPLAY_CACHE_MAX_BLOCKS_PER_ENTRY: usize = 512;
/// Bounds aggregate in-process replay content.
pub const KIMI_THINKING_REPLAY_CACHE_MAX_TOTAL_BYTES: usize = 256 << 20;

struct Entry {
    content: Vec<u8>,
    timestamp: Timestamp,
    generation: String,
    deleted: bool,
}

/// Identifies the exact replay generation read for one request. Opaque outside the cache; a
/// default snapshot (`loaded == false`) makes conditional operations behave as unconditional.
/// In Home mode `raw` is the stored value read, the compare-and-swap guard.
#[derive(Debug, Clone, Default)]
pub struct KimiThinkingReplaySnapshot {
    pub(super) raw: Vec<u8>,
    pub(super) generation: String,
    pub(super) loaded: bool,
    pub(super) found: bool,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    total_bytes: usize,
}

pub struct KimiThinkingReplayCache {
    state: Mutex<State>,
    clock: Clock,
}

static GLOBAL: LazyLock<KimiThinkingReplayCache> =
    LazyLock::new(|| KimiThinkingReplayCache::new(Clock::real()));

/// A valid content array: JSON, at most 8 MiB, and a non-empty array of at most 512 blocks.
pub(super) fn valid_replay_content(content: &[u8], max_bytes: usize, max_blocks: usize) -> bool {
    if content.is_empty() || content.len() > max_bytes || !cpa_json::valid(content) {
        return false;
    }
    match cpa_json::parse(content) {
        cpa_json::Value::Array(items) => !items.is_empty() && items.len() <= max_blocks,
        _ => false,
    }
}

impl State {
    fn remove_entry(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.total_bytes -= entry.content.len();
        }
    }

    fn enforce_limits(&mut self) {
        while self.entries.len() > KIMI_THINKING_REPLAY_CACHE_MAX_ENTRIES
            || self.total_bytes > KIMI_THINKING_REPLAY_CACHE_MAX_TOTAL_BYTES
        {
            if self.entries.is_empty() {
                self.total_bytes = 0;
                return;
            }
            for key in oldest_keys(&self.entries, KIMI_THINKING_REPLAY_CACHE_EVICT_BATCH_SIZE, |e| e.timestamp) {
                self.remove_entry(&key);
            }
        }
    }

    /// Fences a local miss with a tombstone so a stale first writer cannot publish over it.
    fn reserve_locked(&mut self, key: &str, now: Timestamp) -> String {
        let entry = Entry {
            content: Vec::new(),
            timestamp: now,
            generation: new_uuid(),
            deleted: true,
        };
        let generation = entry.generation.clone();
        self.entries.insert(key.to_string(), entry);
        self.enforce_limits();
        generation
    }

    fn store(&mut self, key: &str, content: Vec<u8>, generation: String, deleted: bool, now: Timestamp) {
        self.remove_entry(key);
        self.total_bytes += content.len();
        self.entries.insert(
            key.to_string(),
            Entry {
                content,
                timestamp: now,
                generation,
                deleted,
            },
        );
        self.enforce_limits();
    }
}

fn cache_key(model_family: &str, session_key: &str) -> Option<String> {
    scoped_key("kimi-thinking-replay", model_family, session_key)
}

fn content_is_valid(content: &[u8]) -> bool {
    valid_replay_content(
        content,
        KIMI_THINKING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY,
        KIMI_THINKING_REPLAY_CACHE_MAX_BLOCKS_PER_ENTRY,
    )
}

impl KimiThinkingReplayCache {
    pub fn new(clock: Clock) -> Self {
        Self {
            state: Mutex::new(State::default()),
            clock,
        }
    }

    /// The process-wide cache (real clock); starts the background purge thread on first use.
    pub fn global() -> &'static KimiThinkingReplayCache {
        ensure_cleanup_started();
        &GLOBAL
    }

    /// Stores one complete signed assistant content array (a JSON array).
    pub fn cache_best_effort(&self, model_family: &str, session_key: &str, content: &[u8]) -> bool {
        let Some(key) = cache_key(model_family, session_key) else {
            return false;
        };
        if !content_is_valid(content) {
            return false;
        }
        let now = self.clock.now();
        self.state.lock().store(&key, content.to_vec(), new_uuid(), false, now);
        true
    }

    /// Complete assistant content for request-time replay.
    pub fn get_required(&self, model_family: &str, session_key: &str) -> Option<Vec<u8>> {
        self.get_with_snapshot_required(model_family, session_key).0
    }

    /// Replay content (None when absent or tombstoned) and the exact cache state read. A miss
    /// installs a tombstone and snapshots it.
    pub fn get_with_snapshot_required(
        &self,
        model_family: &str,
        session_key: &str,
    ) -> (Option<Vec<u8>>, KimiThinkingReplaySnapshot) {
        let Some(key) = cache_key(model_family, session_key) else {
            return (None, KimiThinkingReplaySnapshot::default());
        };
        let now = self.clock.now();
        let mut state = self.state.lock();
        let expired_or_missing = match state.entries.get(&key) {
            None => true,
            Some(entry) => elapsed(now, entry.timestamp) > KIMI_THINKING_REPLAY_CACHE_TTL,
        };
        if expired_or_missing {
            state.remove_entry(&key);
            state.reserve_locked(&key, now);
        }
        // Reservation can trigger eviction; the entry just written is the newest, so it survives.
        let Some(entry) = state.entries.get_mut(&key) else {
            return (None, KimiThinkingReplaySnapshot::default());
        };
        entry.timestamp = now;
        let snapshot = KimiThinkingReplaySnapshot {
            generation: entry.generation.clone(),
            loaded: true,
            found: true,
            ..Default::default()
        };
        if entry.deleted {
            return (None, snapshot);
        }
        (Some(entry.content.clone()), snapshot)
    }

    /// Stores completed content only if the request snapshot is still current.
    pub fn replace_if_unchanged(
        &self,
        model_family: &str,
        session_key: &str,
        snapshot: &KimiThinkingReplaySnapshot,
        content: &[u8],
    ) -> bool {
        let Some(key) = cache_key(model_family, session_key) else {
            return false;
        };
        if !content_is_valid(content) {
            return false;
        }
        if !snapshot.loaded {
            return self.cache_best_effort(model_family, session_key, content);
        }
        let now = self.clock.now();
        let mut state = self.state.lock();
        let found = state.entries.get(&key).map(|e| e.generation.as_str());
        if found.is_some() != snapshot.found || found.is_some_and(|g| g != snapshot.generation) {
            return false;
        }
        state.remove_entry(&key);
        state.total_bytes += content.len();
        state.entries.insert(
            key,
            Entry {
                content: content.to_vec(),
                timestamp: now,
                generation: new_uuid(),
                deleted: false,
            },
        );
        state.enforce_limits();
        true
    }

    /// Clears replay state only if the request snapshot is still current (leaving a tombstone).
    pub fn delete_if_unchanged(
        &self,
        model_family: &str,
        session_key: &str,
        snapshot: &KimiThinkingReplaySnapshot,
    ) -> bool {
        let Some(key) = cache_key(model_family, session_key) else {
            return false;
        };
        if !snapshot.loaded {
            self.delete_required(model_family, session_key);
            return true;
        }
        let now = self.clock.now();
        let mut state = self.state.lock();
        let found = state.entries.get(&key).map(|e| e.generation.as_str());
        if found.is_some() != snapshot.found || found.is_some_and(|g| g != snapshot.generation) {
            return false;
        }
        state.remove_entry(&key);
        state.entries.insert(
            key,
            Entry {
                content: Vec::new(),
                timestamp: now,
                generation: new_uuid(),
                deleted: true,
            },
        );
        true
    }

    /// Removes replay state unconditionally.
    pub fn delete_required(&self, model_family: &str, session_key: &str) {
        let Some(key) = cache_key(model_family, session_key) else {
            return;
        };
        self.state.lock().remove_entry(&key);
    }

    /// Clears all Kimi replay state.
    pub fn clear(&self) {
        *self.state.lock() = State::default();
    }

    /// Drops entries older than the TTL.
    pub fn purge_expired(&self) {
        let now = self.clock.now();
        let mut state = self.state.lock();
        let expired: Vec<String> = state
            .entries
            .iter()
            .filter(|(_, e)| elapsed(now, e.timestamp) > KIMI_THINKING_REPLAY_CACHE_TTL)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            state.remove_entry(&key);
        }
    }

    #[cfg(test)]
    pub(crate) fn total_bytes(&self) -> usize {
        self.state.lock().total_bytes
    }

    #[cfg(test)]
    pub(crate) fn entry_count(&self) -> usize {
        self.state.lock().entries.len()
    }
}

// ---- global API: Home KV when Home mode is on, otherwise the in-process cache

const KV_PREFIX: &str = "cpa:kimi:thinking-replay";
const LABEL: &str = "kimi thinking replay";
const MAX_SERIALIZED_BYTES: usize = KIMI_THINKING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY + 1024;

/// Home value: `{"generation", "deleted"?, "content"?}`; a tombstone has no content.
#[derive(Deserialize)]
struct HomeValue {
    #[serde(default)]
    generation: String,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    content: Option<Box<RawValue>>,
}

fn kv_key(model_family: &str, session_key: &str) -> String {
    scoped_kv_key(KV_PREFIX, model_family, session_key)
}

fn marshal_home_value(generation: &str, deleted: bool, content: &[u8]) -> KvResult<Vec<u8>> {
    let mut out = format!("{{\"generation\":\"{generation}\"");
    if deleted {
        out.push_str(",\"deleted\":true");
    } else if !content.is_empty() {
        out.push_str(",\"content\":");
        out.push_str(&compact_raw_json(content)?);
    }
    out.push('}');
    Ok(out.into_bytes())
}

/// `(content, generation, deleted)` of a stored value; a bare content array is the legacy format.
fn decode_home_value(raw: &[u8]) -> Option<(Vec<u8>, String, bool)> {
    if raw.is_empty() || raw.len() > MAX_SERIALIZED_BYTES || !cpa_json::valid(raw) {
        return None;
    }
    if raw.trim_ascii_start().first() == Some(&b'[') {
        return content_is_valid(raw).then(|| (raw.to_vec(), "legacy".to_string(), false));
    }
    let value: HomeValue = serde_json::from_slice(raw).ok()?;
    if value.generation.trim().is_empty() {
        return None;
    }
    if value.deleted {
        return Some((Vec::new(), value.generation, true));
    }
    let content = value.content?.get().as_bytes().to_vec();
    content_is_valid(&content).then_some((content, value.generation, false))
}

fn home_get(
    backend: &dyn KvBackend,
    model_family: &str,
    session_key: &str,
) -> KvResult<(Option<Vec<u8>>, KimiThinkingReplaySnapshot)> {
    let key = kv_key(model_family, session_key);
    let raw = read_or_reserve(backend, &key, KIMI_THINKING_REPLAY_CACHE_TTL, MAX_SERIALIZED_BYTES, LABEL, || {
        marshal_home_value(&new_uuid(), true, &[])
    })?;
    let mut snapshot = KimiThinkingReplaySnapshot { raw: raw.clone(), loaded: true, found: true, ..Default::default() };
    let Some((content, generation, deleted)) = decode_home_value(&raw) else {
        return Err(KvError::new("invalid kimi thinking replay content"));
    };
    snapshot.generation = generation;
    if let Err(e) = backend.expire(&key, KIMI_THINKING_REPLAY_CACHE_TTL) {
        tracing::warn!("home kv kimi thinking replay expire failed prefix=cpa:kimi:*: {e}");
    }
    Ok(((!deleted).then_some(content), snapshot))
}

/// Go: CacheKimiThinkingReplayBestEffort.
pub fn cache_kimi_thinking_replay_best_effort(model_family: &str, session_key: &str, content: &[u8]) -> bool {
    if cache_key(model_family, session_key).is_none() || !content_is_valid(content) {
        return false;
    }
    let result = store().and_then(|store| match store {
        Store::Local => Ok(None),
        Store::Home(backend) => {
            let raw = marshal_home_value(&new_uuid(), false, content)?;
            backend
                .set(&kv_key(model_family, session_key), &raw, KIMI_THINKING_REPLAY_CACHE_TTL)
                .map(Some)
        }
    });
    match result {
        Ok(Some(written)) => written,
        Ok(None) => KimiThinkingReplayCache::global().cache_best_effort(model_family, session_key, content),
        Err(e) => {
            tracing::error!("home kv best-effort kimi thinking replay set failed prefix=cpa:kimi:*: {e}");
            false
        }
    }
}

/// Go: GetKimiThinkingReplayRequired.
pub fn get_kimi_thinking_replay_required(model_family: &str, session_key: &str) -> KvResult<Option<Vec<u8>>> {
    get_kimi_thinking_replay_with_snapshot_required(model_family, session_key).map(|(content, _)| content)
}

/// Go: GetKimiThinkingReplayWithSnapshotRequired. In Home mode a miss reserves a tombstone, and
/// the snapshot carries the stored value as the compare-and-swap guard.
pub fn get_kimi_thinking_replay_with_snapshot_required(
    model_family: &str,
    session_key: &str,
) -> KvResult<(Option<Vec<u8>>, KimiThinkingReplaySnapshot)> {
    if cache_key(model_family, session_key).is_none() {
        return Ok((None, KimiThinkingReplaySnapshot::default()));
    }
    match store()? {
        Store::Home(backend) => home_get(backend, model_family, session_key),
        Store::Local => Ok(KimiThinkingReplayCache::global().get_with_snapshot_required(model_family, session_key)),
    }
}

/// Go: ReplaceKimiThinkingReplayIfUnchanged.
pub fn replace_kimi_thinking_replay_if_unchanged(
    model_family: &str,
    session_key: &str,
    snapshot: &KimiThinkingReplaySnapshot,
    content: &[u8],
) -> KvResult<bool> {
    if cache_key(model_family, session_key).is_none() || !content_is_valid(content) {
        return Ok(false);
    }
    if !snapshot.loaded {
        return Ok(cache_kimi_thinking_replay_best_effort(model_family, session_key, content));
    }
    match store()? {
        Store::Home(backend) => {
            let raw = marshal_home_value(&new_uuid(), false, content)?;
            backend.compare_and_swap(
                &kv_key(model_family, session_key),
                snapshot.found.then_some(snapshot.raw.as_slice()),
                &raw,
                KIMI_THINKING_REPLAY_CACHE_TTL,
            )
        }
        Store::Local => {
            Ok(KimiThinkingReplayCache::global().replace_if_unchanged(model_family, session_key, snapshot, content))
        }
    }
}

/// Go: DeleteKimiThinkingReplayIfUnchanged.
pub fn delete_kimi_thinking_replay_if_unchanged(
    model_family: &str,
    session_key: &str,
    snapshot: &KimiThinkingReplaySnapshot,
) -> KvResult<bool> {
    if cache_key(model_family, session_key).is_none() {
        return Ok(false);
    }
    if !snapshot.loaded {
        delete_kimi_thinking_replay_required(model_family, session_key)?;
        return Ok(true);
    }
    match store()? {
        Store::Home(backend) => {
            let tombstone = marshal_home_value(&new_uuid(), true, &[])?;
            backend.compare_and_swap(
                &kv_key(model_family, session_key),
                snapshot.found.then_some(snapshot.raw.as_slice()),
                &tombstone,
                KIMI_THINKING_REPLAY_CACHE_TTL,
            )
        }
        Store::Local => {
            Ok(KimiThinkingReplayCache::global().delete_if_unchanged(model_family, session_key, snapshot))
        }
    }
}

/// Go: DeleteKimiThinkingReplayRequired.
pub fn delete_kimi_thinking_replay_required(model_family: &str, session_key: &str) -> KvResult<()> {
    if cache_key(model_family, session_key).is_none() {
        return Ok(());
    }
    match store()? {
        Store::Home(backend) => backend.del(&kv_key(model_family, session_key)),
        Store::Local => {
            KimiThinkingReplayCache::global().delete_required(model_family, session_key);
            Ok(())
        }
    }
}

/// Go: ClearKimiThinkingReplayCache.
pub fn clear_kimi_thinking_replay_cache() {
    KimiThinkingReplayCache::global().clear();
}
