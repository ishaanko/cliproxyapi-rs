//! Antigravity reasoning replay cache (Go: cache/antigravity_reasoning_replay_cache.go): thought
//! signatures and function-call parts of Gemini turns, replayed to keep native signature ordering.
//!
//! Writers use a snapshot protocol: a request reads state with
//! [`AntigravityReasoningReplayCache::get_items_with_snapshot_required`] (a miss installs a
//! tombstone with a fresh revision), and later publishes with `replace_items_if_unchanged`, which
//! succeeds only if the entry is still the snapshotted revision (or a descendant chain on the same
//! branch whose items extend the current ones). An eviction epoch fences snapshots of absent keys
//! so unrelated eviction does not block them but eviction of their own tombstone does.

use std::collections::HashMap;
use std::fmt;
use std::sync::LazyLock;
use std::time::Duration;

use cpa_json::{J, Res, Value};
use parking_lot::Mutex;

use super::kv::{KvBackend, KvError, KvResult, Store, decode_items, encode_items, scoped_kv_key, store};
use super::{Clock, Timestamp, elapsed, ensure_cleanup_started, oldest_keys, scoped_key};
use crate::signature::GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR;

/// How long encrypted reasoning replay items stay in process memory.
pub const ANTIGRAVITY_REASONING_REPLAY_CACHE_TTL: Duration = Duration::from_secs(3600);
/// Bounds process memory for replay continuity; oldest entries are evicted first.
pub const ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_ENTRIES: usize = 10240;
/// Entries evicted at once after reaching capacity.
pub const ANTIGRAVITY_REASONING_REPLAY_CACHE_EVICT_BATCH_SIZE: usize = 128;
/// Bounds one logical conversation. Oversized chains are not partially cached, because dropping an
/// arbitrary prefix would break native signature ordering.
pub const ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_ITEMS_PER_ENTRY: usize = 4096;
pub const ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY: usize = 16 << 20;

const MIN_ANTIGRAVITY_THOUGHT_SIGNATURE_REPLAY_LEN: usize = 16;

/// Failure of a conditional replace: the items normalize to nothing, or Home KV failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AntigravityReplayError {
    InvalidItems,
    Kv(KvError),
}

impl fmt::Display for AntigravityReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidItems => f.write_str("invalid antigravity reasoning replay items"),
            Self::Kv(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for AntigravityReplayError {}

impl From<KvError> for AntigravityReplayError {
    fn from(e: KvError) -> Self {
        Self::Kv(e)
    }
}

struct Entry {
    items: Vec<Vec<u8>>,
    timestamp: Timestamp,
    revision: u64,
    branch: String,
    deleted: bool,
}

/// The exact replay state read for one request. Opaque outside the cache; the default snapshot
/// (`loaded == false`) makes conditional operations behave as unconditional. In Home mode `raw`
/// is the stored value read, the compare-and-swap guard.
#[derive(Debug, Clone, Default)]
pub struct AntigravityReasoningReplaySnapshot {
    raw: Vec<u8>,
    items: Vec<Vec<u8>>,
    loaded: bool,
    found: bool,
    revision: u64,
    branch: String,
    eviction_epoch: u64,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    next_revision: u64,
    eviction_epoch: u64,
}

pub struct AntigravityReasoningReplayCache {
    state: Mutex<State>,
    clock: Clock,
}

static GLOBAL: LazyLock<AntigravityReasoningReplayCache> =
    LazyLock::new(|| AntigravityReasoningReplayCache::new(Clock::real()));

fn cache_key(model_name: &str, session_key: &str) -> Option<String> {
    // The session key is the continuity boundary, independent of the upstream credential so auth
    // failover preserves replay.
    scoped_key("antigravity-reasoning-replay", model_name, session_key)
}

/// A random 16 byte hex identifier for a replay branch.
fn new_generation() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

fn items_prefix(prefix: &[Vec<u8>], items: &[Vec<u8>]) -> bool {
    prefix.len() <= items.len() && prefix.iter().zip(items).all(|(a, b)| a == b)
}

fn trimmed(item: &Value, path: &str) -> String {
    item.g(path).str().trim().to_string()
}

impl State {
    fn evict_oldest(&mut self, count: usize) {
        for key in oldest_keys(&self.entries, count, |e| e.timestamp) {
            self.eviction_epoch += 1;
            self.entries.remove(&key);
        }
    }

    fn evict_if_over_capacity(&mut self) {
        if self.entries.len() > ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_ENTRIES {
            self.evict_oldest(ANTIGRAVITY_REASONING_REPLAY_CACHE_EVICT_BATCH_SIZE);
        }
    }

    fn put_tombstone(&mut self, key: &str, now: Timestamp) -> (u64, String) {
        self.next_revision += 1;
        let (revision, branch) = (self.next_revision, new_generation());
        self.entries.insert(
            key.to_string(),
            Entry {
                items: Vec::new(),
                timestamp: now,
                revision,
                branch: branch.clone(),
                deleted: true,
            },
        );
        (revision, branch)
    }

    /// Fences a local miss with a per-key tombstone so eviction of an unrelated key cannot
    /// invalidate it.
    fn reserve_absent(&mut self, key: &str, now: Timestamp) -> AntigravityReasoningReplaySnapshot {
        if self.entries.len() >= ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_ENTRIES {
            self.evict_oldest(ANTIGRAVITY_REASONING_REPLAY_CACHE_EVICT_BATCH_SIZE);
        }
        let (revision, branch) = self.put_tombstone(key, now);
        AntigravityReasoningReplaySnapshot {
            raw: Vec::new(),
            items: Vec::new(),
            loaded: true,
            found: true,
            revision,
            branch,
            eviction_epoch: self.eviction_epoch,
        }
    }
}

fn normalize_items(items: &[Vec<u8>]) -> Option<Vec<Vec<u8>>> {
    if items.len() > ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_ITEMS_PER_ENTRY {
        return None;
    }
    let mut normalized = Vec::with_capacity(items.len());
    let mut total_bytes = 0usize;
    for item in items {
        if let Some(normalized_item) = normalize_item(item) {
            total_bytes += normalized_item.len();
            if total_bytes > ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_BYTES_PER_ENTRY {
                return None;
            }
            normalized.push(normalized_item);
        }
    }
    (!normalized.is_empty()).then_some(normalized)
}

fn normalize_item(item: &[u8]) -> Option<Vec<u8>> {
    let item = cpa_json::parse(item);
    match trimmed(&item, "type").as_str() {
        "thought_signature" => normalize_thought_signature_item(&item),
        "function_call_part" => normalize_function_call_part_item(&item),
        _ => None,
    }
}

/// Sets `key` to the integer value of `item.key` when it is a JSON number (and, with
/// `non_negative`, at least zero).
fn copy_number(normalized: &mut Value, item: &Value, key: &str, non_negative: bool) {
    let number = item.g(key);
    if number.is_number() && (!non_negative || number.int() >= 0) {
        cpa_json::set(normalized, key, number.int());
    }
}

fn copy_trimmed(normalized: &mut Value, item: &Value, key: &str) {
    let value = trimmed(item, key);
    if !value.is_empty() {
        cpa_json::set(normalized, key, value);
    }
}

fn normalize_thought_signature_item(item: &Value) -> Option<Vec<u8>> {
    let mut sig = trimmed(item, "thoughtSignature");
    if sig.is_empty() {
        sig = trimmed(item, "thought_signature");
    }
    if sig.is_empty()
        || sig == GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        || sig.len() < MIN_ANTIGRAVITY_THOUGHT_SIGNATURE_REPLAY_LEN
    {
        return None;
    }
    let mut normalized = cpa_json::parse_str(r#"{"type":"thought_signature"}"#);
    cpa_json::set(&mut normalized, "thoughtSignature", sig);
    copy_number(&mut normalized, item, "contentIndex", false);
    copy_number(&mut normalized, item, "partIndex", false);
    let target_kind = trimmed(item, "targetKind");
    if target_kind == "text" || target_kind == "thought" {
        cpa_json::set(&mut normalized, "targetKind", target_kind);
    }
    copy_trimmed(&mut normalized, item, "targetHash");
    copy_number(&mut normalized, item, "targetOccurrence", true);
    copy_trimmed(&mut normalized, item, "contextHash");
    Some(cpa_json::to_vec(&normalized))
}

fn normalize_function_call_part_item(item: &Value) -> Option<Vec<u8>> {
    let mut call_id = trimmed(item, "call_id");
    if call_id.is_empty() {
        call_id = trimmed(item, "id");
    }
    let mut name = trimmed(item, "name");
    let mut args: Res<'_> = item.g("args");
    if (name.is_empty() || !args.exists()) && item.g("functionCall").exists() {
        if call_id.is_empty() {
            call_id = trimmed(item, "functionCall.id");
        }
        if name.is_empty() {
            name = trimmed(item, "functionCall.name");
        }
        if !args.exists() {
            args = item.g("functionCall.args");
        }
    }
    if name.is_empty() || !args.exists() {
        return None;
    }
    let mut normalized = cpa_json::parse_str(r#"{"type":"function_call_part"}"#);
    if !call_id.is_empty() {
        cpa_json::set(&mut normalized, "call_id", call_id);
    }
    cpa_json::set(&mut normalized, "name", name);
    if args.is_string() {
        cpa_json::set(&mut normalized, "args", args.str());
    } else {
        cpa_json::set(&mut normalized, "args", args.value());
    }
    let sig = trimmed(item, "thoughtSignature");
    if !sig.is_empty() && sig != GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR {
        cpa_json::set(&mut normalized, "thoughtSignature", sig);
    }
    copy_number(&mut normalized, item, "contentIndex", false);
    copy_number(&mut normalized, item, "partIndex", false);
    copy_number(&mut normalized, item, "targetOccurrence", true);
    copy_trimmed(&mut normalized, item, "contextHash");
    Some(cpa_json::to_vec(&normalized))
}

impl AntigravityReasoningReplayCache {
    pub fn new(clock: Clock) -> Self {
        Self {
            state: Mutex::new(State::default()),
            clock,
        }
    }

    /// The process-wide cache (real clock); starts the background purge thread on first use.
    pub fn global() -> &'static AntigravityReasoningReplayCache {
        ensure_cleanup_started();
        &GLOBAL
    }

    /// Stores replay items (normalized), replacing any previous state with a fresh branch.
    /// False when the key is empty or nothing replayable remains.
    pub fn cache_items(&self, model_name: &str, session_key: &str, items: &[Vec<u8>]) -> bool {
        let Some(key) = cache_key(model_name, session_key) else {
            return false;
        };
        let Some(normalized) = normalize_items(items) else {
            return false;
        };
        let now = self.clock.now();
        let mut state = self.state.lock();
        state.next_revision += 1;
        let revision = state.next_revision;
        state.entries.insert(
            key,
            Entry {
                items: normalized,
                timestamp: now,
                revision,
                branch: new_generation(),
                deleted: false,
            },
        );
        state.evict_if_over_capacity();
        true
    }

    /// The first normalized replay item.
    pub fn get_item(&self, model_name: &str, session_key: &str) -> Option<Vec<u8>> {
        self.get_items(model_name, session_key)?.into_iter().next()
    }

    /// Normalized replay items (None when absent or empty).
    pub fn get_items(&self, model_name: &str, session_key: &str) -> Option<Vec<Vec<u8>>> {
        self.get_items_with_snapshot_required(model_name, session_key).0
    }

    /// Replay items and the exact state that guards this request. Refreshes the TTL; a miss
    /// installs a tombstone and snapshots it.
    pub fn get_items_with_snapshot_required(
        &self,
        model_name: &str,
        session_key: &str,
    ) -> (Option<Vec<Vec<u8>>>, AntigravityReasoningReplaySnapshot) {
        let Some(key) = cache_key(model_name, session_key) else {
            return (None, AntigravityReasoningReplaySnapshot::default());
        };
        let now = self.clock.now();
        let mut state = self.state.lock();
        match state.entries.get(&key).map(|e| e.timestamp) {
            None => return (None, state.reserve_absent(&key, now)),
            Some(ts) if elapsed(now, ts) > ANTIGRAVITY_REASONING_REPLAY_CACHE_TTL => {
                state.eviction_epoch += 1;
                state.entries.remove(&key);
                return (None, state.reserve_absent(&key, now));
            }
            Some(_) => {}
        }
        let eviction_epoch = state.eviction_epoch;
        let Some(entry) = state.entries.get_mut(&key) else {
            return (None, AntigravityReasoningReplaySnapshot::default());
        };
        entry.timestamp = now;
        let mut snapshot = AntigravityReasoningReplaySnapshot {
            raw: Vec::new(),
            items: Vec::new(),
            loaded: true,
            found: true,
            revision: entry.revision,
            branch: entry.branch.clone(),
            eviction_epoch,
        };
        if entry.deleted || entry.items.is_empty() {
            return (None, snapshot);
        }
        snapshot.items = entry.items.clone();
        (Some(entry.items.clone()), snapshot)
    }

    /// Publishes a completed chain only when no newer request has changed the state read by this
    /// request.
    pub fn replace_items_if_unchanged(
        &self,
        model_name: &str,
        session_key: &str,
        snapshot: &AntigravityReasoningReplaySnapshot,
        items: &[Vec<u8>],
    ) -> Result<bool, AntigravityReplayError> {
        let Some(key) = cache_key(model_name, session_key) else {
            return Ok(false);
        };
        let normalized = normalize_items(items).ok_or(AntigravityReplayError::InvalidItems)?;
        if !snapshot.loaded {
            return Ok(self.cache_items(model_name, session_key, &normalized));
        }
        let now = self.clock.now();
        let mut state = self.state.lock();
        let current = state.entries.get(&key);
        let found = current.is_some();
        let matches_snapshot = found == snapshot.found
            && match current {
                Some(entry) => entry.revision == snapshot.revision,
                None => snapshot.eviction_epoch == state.eviction_epoch,
            };
        let is_descendant = current.is_some_and(|entry| {
            !entry.deleted
                && !snapshot.branch.is_empty()
                && entry.branch == snapshot.branch
                && items_prefix(&entry.items, &normalized)
        });
        if !matches_snapshot && !is_descendant {
            return Ok(false);
        }
        let mut branch = snapshot.branch.clone();
        if branch.is_empty() || (matches_snapshot && !items_prefix(&snapshot.items, &normalized)) {
            branch = new_generation();
        }
        state.next_revision += 1;
        let revision = state.next_revision;
        state.entries.insert(
            key,
            Entry {
                items: normalized,
                timestamp: now,
                revision,
                branch,
                deleted: false,
            },
        );
        state.evict_if_over_capacity();
        Ok(true)
    }

    /// Clears replay state (leaving a tombstone) only when it still matches the state read for
    /// this request.
    pub fn delete_items_if_unchanged(
        &self,
        model_name: &str,
        session_key: &str,
        snapshot: &AntigravityReasoningReplaySnapshot,
    ) -> bool {
        let Some(key) = cache_key(model_name, session_key) else {
            return false;
        };
        if !snapshot.loaded {
            self.delete_item(model_name, session_key);
            return true;
        }
        let now = self.clock.now();
        let mut state = self.state.lock();
        let current = state.entries.get(&key);
        let found = current.is_some();
        if found != snapshot.found
            || current.is_some_and(|entry| entry.revision != snapshot.revision)
            || (!found && snapshot.eviction_epoch != state.eviction_epoch)
        {
            return false;
        }
        state.put_tombstone(&key, now);
        state.evict_if_over_capacity();
        true
    }

    /// Removes one replay item after upstream rejects it or the caller knows it is stale. Leaves a
    /// tombstone so stale writers are fenced.
    pub fn delete_item(&self, model_name: &str, session_key: &str) {
        let Some(key) = cache_key(model_name, session_key) else {
            return;
        };
        let now = self.clock.now();
        let mut state = self.state.lock();
        state.put_tombstone(&key, now);
        state.evict_if_over_capacity();
    }

    /// Clears all Antigravity reasoning replay state (bumping the eviction epoch).
    pub fn clear(&self) {
        let mut state = self.state.lock();
        state.entries.clear();
        state.eviction_epoch += 1;
    }

    /// Drops entries older than the TTL.
    pub fn purge_expired(&self) {
        let now = self.clock.now();
        let mut state = self.state.lock();
        let expired: Vec<String> = state
            .entries
            .iter()
            .filter(|(_, e)| elapsed(now, e.timestamp) > ANTIGRAVITY_REASONING_REPLAY_CACHE_TTL)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            state.eviction_epoch += 1;
            state.entries.remove(&key);
        }
    }

    #[cfg(test)]
    pub(crate) fn entry_count(&self) -> usize {
        self.state.lock().entries.len()
    }

    /// Test hook: evicts the `count` oldest entries (Go tests call the helper directly).
    #[cfg(test)]
    pub(crate) fn evict_oldest_for_test(&self, count: usize) {
        self.state.lock().evict_oldest(count);
    }

    #[cfg(test)]
    pub(crate) fn is_tombstone_for_test(&self, model_name: &str, session_key: &str) -> Option<bool> {
        let key = cache_key(model_name, session_key)?;
        self.state.lock().entries.get(&key).map(|e| e.deleted)
    }
}

// ---- global API: Home KV when Home mode is on, otherwise the in-process cache

const KV_PREFIX: &str = "cpa:antigravity:reasoning-replay";
const GENERATION_ITEM_TYPE: &str = "cpa_antigravity_replay_generation";
/// Attempts to fence an absent key, and to re-validate a lost compare-and-swap.
const HOME_ATTEMPTS: usize = 4;
/// JSON encodes each normalized item as base64; leaves room for that expansion while rejecting
/// oversized Home values before decoding.
const MAX_SERIALIZED_BYTES: usize = 24 << 20;

fn kv_key(model_name: &str, session_key: &str) -> String {
    scoped_kv_key(KV_PREFIX, model_name, session_key)
}

/// The leading marker item of a stored value: `{"type","generation","branch"[,"deleted":true]}`.
fn marker_item(deleted: bool, branch: &str) -> Vec<u8> {
    let template = if deleted {
        r#"{"type":"","generation":"","branch":"","deleted":true}"#
    } else {
        r#"{"type":"","generation":"","branch":""}"#
    };
    let mut marker = cpa_json::parse_str(template);
    cpa_json::set(&mut marker, "type", GENERATION_ITEM_TYPE);
    cpa_json::set(&mut marker, "generation", new_generation());
    cpa_json::set(&mut marker, "branch", branch);
    cpa_json::to_vec(&marker)
}

fn new_tombstone() -> KvResult<Vec<u8>> {
    encode_items(&[marker_item(true, &new_generation())])
}

/// Stored value of `items` on `branch` (a fresh branch when empty).
fn marshal_home_value(items: &[Vec<u8>], branch: &str) -> KvResult<Vec<u8>> {
    let branch = if branch.is_empty() { new_generation() } else { branch.to_string() };
    let mut stored = Vec::with_capacity(items.len() + 1);
    stored.push(marker_item(false, &branch));
    stored.extend_from_slice(items);
    encode_items(&stored)
}

/// A decoded stored value.
struct HomeValue {
    items: Vec<Vec<u8>>,
    deleted: bool,
    branch: String,
}

/// Splits the leading marker off a stored value; a value without one decodes as plain items.
fn decode_home_value(raw: &[u8]) -> Option<HomeValue> {
    let mut items = decode_items(raw).ok()?;
    let is_marker = items
        .first()
        .is_some_and(|first| trimmed(&cpa_json::parse(first), "type") == GENERATION_ITEM_TYPE);
    if !is_marker {
        return Some(HomeValue { items, deleted: false, branch: String::new() });
    }
    let marker = cpa_json::parse(&items.remove(0));
    Some(HomeValue { items, deleted: marker.g("deleted").bool(), branch: trimmed(&marker, "branch") })
}

/// Reads the stored value of `key`, reserving a tombstone first when absent.
fn read_or_fence(backend: &dyn KvBackend, key: &str) -> KvResult<Vec<u8>> {
    for _ in 0..HOME_ATTEMPTS {
        if let Some(raw) = backend.get(key)? {
            return Ok(raw);
        }
        let reservation = new_tombstone()?;
        if backend.compare_and_swap(key, None, &reservation, ANTIGRAVITY_REASONING_REPLAY_CACHE_TTL)? {
            return Ok(reservation);
        }
    }
    Err(KvError::new("could not fence absent antigravity reasoning replay state"))
}

/// Reads the Home state into `snapshot`, which is already marked loaded so a failed read still
/// guards later conditional writes (as in Go, where the error comes with a loaded snapshot).
fn home_get(
    backend: &dyn KvBackend,
    model_name: &str,
    session_key: &str,
    snapshot: &mut AntigravityReasoningReplaySnapshot,
) -> KvResult<Option<Vec<Vec<u8>>>> {
    let key = kv_key(model_name, session_key);
    let raw = read_or_fence(backend, &key)?;
    snapshot.found = true;
    if raw.len() > MAX_SERIALIZED_BYTES {
        return Ok(None);
    }
    snapshot.raw.clone_from(&raw);
    let Some(value) = decode_home_value(&raw) else {
        return Ok(None);
    };
    snapshot.branch = value.branch;
    if value.deleted || value.items.is_empty() || value.items.len() > ANTIGRAVITY_REASONING_REPLAY_CACHE_MAX_ITEMS_PER_ENTRY {
        return Ok(None);
    }
    let Some(normalized) = normalize_items(&value.items).filter(|n| n.len() == value.items.len()) else {
        return Ok(None);
    };
    snapshot.items.clone_from(&normalized);
    backend.expire(&key, ANTIGRAVITY_REASONING_REPLAY_CACHE_TTL)?;
    Ok(Some(normalized))
}

/// Publishes `normalized` by compare-and-swap, following a chain of descendants of the snapshot
/// that only extend what this request read.
fn home_replace(
    backend: &dyn KvBackend,
    key: &str,
    snapshot: &AntigravityReasoningReplaySnapshot,
    normalized: &[Vec<u8>],
) -> KvResult<bool> {
    let mut expected_raw = snapshot.raw.clone();
    let mut expected_found = snapshot.found;
    let branch = if snapshot.branch.is_empty() || !items_prefix(&snapshot.items, normalized) {
        new_generation()
    } else {
        snapshot.branch.clone()
    };
    for _ in 0..HOME_ATTEMPTS {
        let raw = marshal_home_value(normalized, &branch)?;
        let expected = expected_found.then_some(expected_raw.as_slice());
        if backend.compare_and_swap(key, expected, &raw, ANTIGRAVITY_REASONING_REPLAY_CACHE_TTL)? {
            return Ok(true);
        }
        let Some(current_raw) = backend.get(key)? else {
            return Ok(false);
        };
        if current_raw.len() > MAX_SERIALIZED_BYTES {
            return Ok(false);
        }
        let Some(current) = decode_home_value(&current_raw) else {
            return Ok(false);
        };
        if current.deleted || snapshot.branch.is_empty() || current.branch != snapshot.branch {
            return Ok(false);
        }
        let descends = normalize_items(&current.items)
            .is_some_and(|n| n.len() == current.items.len() && items_prefix(&n, normalized));
        if !descends {
            return Ok(false);
        }
        expected_raw = current_raw;
        expected_found = true;
    }
    Ok(false)
}

/// Go: CacheAntigravityReasoningReplayItem.
pub fn cache_antigravity_reasoning_replay_item(model_name: &str, session_key: &str, item: &[u8]) -> bool {
    cache_antigravity_reasoning_replay_items(model_name, session_key, &[item.to_vec()])
}

/// Go: CacheAntigravityReasoningReplayItems / CacheAntigravityReasoningReplayItemsBestEffort.
pub fn cache_antigravity_reasoning_replay_items(model_name: &str, session_key: &str, items: &[Vec<u8>]) -> bool {
    if cache_key(model_name, session_key).is_none() {
        return false;
    }
    let Some(normalized) = normalize_items(items) else {
        return false;
    };
    let result = store().and_then(|store| match store {
        Store::Local => Ok(None),
        Store::Home(backend) => {
            let raw = marshal_home_value(&normalized, "")?;
            backend
                .set(&kv_key(model_name, session_key), &raw, ANTIGRAVITY_REASONING_REPLAY_CACHE_TTL)
                .map(Some)
        }
    });
    match result {
        Ok(Some(written)) => written,
        Ok(None) => AntigravityReasoningReplayCache::global().cache_items(model_name, session_key, items),
        Err(e) => {
            tracing::error!("home kv best-effort antigravity reasoning replay set failed prefix=cpa:antigravity:*: {e}");
            false
        }
    }
}

/// Go: GetAntigravityReasoningReplayItem (failures read as a miss).
pub fn get_antigravity_reasoning_replay_item(model_name: &str, session_key: &str) -> Option<Vec<u8>> {
    get_antigravity_reasoning_replay_items(model_name, session_key)?.into_iter().next()
}

/// Go: GetAntigravityReasoningReplayItems (failures read as a miss).
pub fn get_antigravity_reasoning_replay_items(model_name: &str, session_key: &str) -> Option<Vec<Vec<u8>>> {
    get_antigravity_reasoning_replay_items_with_snapshot_required(model_name, session_key)
        .0
        .ok()
        .flatten()
}

/// Go: GetAntigravityReasoningReplayItemsRequired.
pub fn get_antigravity_reasoning_replay_items_required(
    model_name: &str,
    session_key: &str,
) -> KvResult<Option<Vec<Vec<u8>>>> {
    get_antigravity_reasoning_replay_items_with_snapshot_required(model_name, session_key).0
}

/// Go: GetAntigravityReasoningReplayItemsWithSnapshotRequired. In Home mode a miss reserves a
/// tombstone, and the snapshot carries the stored value as the compare-and-swap guard. After a
/// failed Home read the snapshot is still returned (loaded), except when the store is unavailable.
pub fn get_antigravity_reasoning_replay_items_with_snapshot_required(
    model_name: &str,
    session_key: &str,
) -> (KvResult<Option<Vec<Vec<u8>>>>, AntigravityReasoningReplaySnapshot) {
    if cache_key(model_name, session_key).is_none() {
        return (Ok(None), AntigravityReasoningReplaySnapshot::default());
    }
    match store() {
        Err(e) => (Err(e), AntigravityReasoningReplaySnapshot::default()),
        Ok(Store::Home(backend)) => {
            let mut snapshot = AntigravityReasoningReplaySnapshot { loaded: true, ..Default::default() };
            let items = home_get(backend, model_name, session_key, &mut snapshot);
            (items, snapshot)
        }
        Ok(Store::Local) => {
            let (items, snapshot) =
                AntigravityReasoningReplayCache::global().get_items_with_snapshot_required(model_name, session_key);
            (Ok(items), snapshot)
        }
    }
}

/// Go: ReplaceAntigravityReasoningReplayItemsIfUnchanged.
pub fn replace_antigravity_reasoning_replay_items_if_unchanged(
    model_name: &str,
    session_key: &str,
    snapshot: &AntigravityReasoningReplaySnapshot,
    items: &[Vec<u8>],
) -> Result<bool, AntigravityReplayError> {
    if cache_key(model_name, session_key).is_none() {
        return Ok(false);
    }
    let normalized = normalize_items(items).ok_or(AntigravityReplayError::InvalidItems)?;
    if !snapshot.loaded {
        return Ok(cache_antigravity_reasoning_replay_items(model_name, session_key, &normalized));
    }
    match store()? {
        Store::Home(backend) => {
            Ok(home_replace(backend, &kv_key(model_name, session_key), snapshot, &normalized)?)
        }
        Store::Local => {
            AntigravityReasoningReplayCache::global().replace_items_if_unchanged(model_name, session_key, snapshot, items)
        }
    }
}

/// Go: DeleteAntigravityReasoningReplayItemsIfUnchanged.
pub fn delete_antigravity_reasoning_replay_items_if_unchanged(
    model_name: &str,
    session_key: &str,
    snapshot: &AntigravityReasoningReplaySnapshot,
) -> KvResult<bool> {
    if cache_key(model_name, session_key).is_none() {
        return Ok(false);
    }
    if !snapshot.loaded {
        delete_antigravity_reasoning_replay_item_required(model_name, session_key)?;
        return Ok(true);
    }
    match store()? {
        Store::Home(backend) => backend.compare_and_swap(
            &kv_key(model_name, session_key),
            snapshot.found.then_some(snapshot.raw.as_slice()),
            &new_tombstone()?,
            ANTIGRAVITY_REASONING_REPLAY_CACHE_TTL,
        ),
        Store::Local => Ok(AntigravityReasoningReplayCache::global().delete_items_if_unchanged(
            model_name,
            session_key,
            snapshot,
        )),
    }
}

/// Go: DeleteAntigravityReasoningReplayItem (failures ignored).
pub fn delete_antigravity_reasoning_replay_item(model_name: &str, session_key: &str) {
    let _ = delete_antigravity_reasoning_replay_item_required(model_name, session_key);
}

/// Go: DeleteAntigravityReasoningReplayItemRequired. Home mode writes a tombstone, which keeps
/// stale writers fenced.
pub fn delete_antigravity_reasoning_replay_item_required(model_name: &str, session_key: &str) -> KvResult<()> {
    if cache_key(model_name, session_key).is_none() {
        return Ok(());
    }
    match store()? {
        Store::Home(backend) => backend
            .set(&kv_key(model_name, session_key), &new_tombstone()?, ANTIGRAVITY_REASONING_REPLAY_CACHE_TTL)
            .map(|_| ()),
        Store::Local => {
            AntigravityReasoningReplayCache::global().delete_item(model_name, session_key);
            Ok(())
        }
    }
}

/// Go: ClearAntigravityReasoningReplayCache.
pub fn clear_antigravity_reasoning_replay_cache() {
    AntigravityReasoningReplayCache::global().clear();
}
