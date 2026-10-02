//! Claude thinking replay cache (Go: cache/claude_thinking_replay_cache.go): the signed assistant
//! turns of a session, appended turn by turn under a generation snapshot.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use cpa_json::Value;
use parking_lot::Mutex;

use super::kimi_thinking_replay::{KimiThinkingReplaySnapshot, valid_replay_content};
use super::{Clock, Timestamp, elapsed, ensure_cleanup_started, new_uuid, oldest_keys, scoped_key};
use crate::util::{GoJsonStyle, go_json_sorted};

/// How long signed assistant turns stay replayable.
pub const CLAUDE_THINKING_REPLAY_CACHE_TTL: Duration = Duration::from_secs(3600);
/// Bounds process memory used by Claude replay continuity.
pub const CLAUDE_THINKING_REPLAY_CACHE_MAX_ENTRIES: usize = 10240;
/// Entries evicted at once after reaching capacity.
pub const CLAUDE_THINKING_REPLAY_CACHE_EVICT_BATCH_SIZE: usize = 128;
/// Bounds all cached assistant turns for one session.
pub const CLAUDE_THINKING_REPLAY_CACHE_MAX_BYTES_PER_SESSION: usize = 8 << 20;
/// Bounds the number of assistant turns per session.
pub const CLAUDE_THINKING_REPLAY_CACHE_MAX_TURNS_PER_SESSION: usize = 64;
/// Prevents pathological content arrays.
pub const CLAUDE_THINKING_REPLAY_CACHE_MAX_BLOCKS_PER_TURN: usize = 512;
/// Bounds aggregate in-process Claude replay content.
pub const CLAUDE_THINKING_REPLAY_CACHE_MAX_TOTAL_BYTES: usize = 256 << 20;

/// Identifies the exact replay generation read for one request (shared with the Kimi cache).
pub type ClaudeThinkingReplaySnapshot = KimiThinkingReplaySnapshot;

struct Entry {
    contents: Vec<Vec<u8>>,
    timestamp: Timestamp,
    generation: String,
    deleted: bool,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    total_bytes: usize,
}

pub struct ClaudeThinkingReplayCache {
    state: Mutex<State>,
    clock: Clock,
}

static GLOBAL: LazyLock<ClaudeThinkingReplayCache> =
    LazyLock::new(|| ClaudeThinkingReplayCache::new(Clock::real()));

fn entry_bytes(contents: &[Vec<u8>]) -> usize {
    contents.iter().map(Vec::len).sum()
}

fn cache_key(model_family: &str, session_key: &str) -> Option<String> {
    scoped_key("claude-thinking-replay", model_family, session_key)
}

fn content_is_valid(content: &[u8]) -> bool {
    valid_replay_content(
        content,
        CLAUDE_THINKING_REPLAY_CACHE_MAX_BYTES_PER_SESSION,
        CLAUDE_THINKING_REPLAY_CACHE_MAX_BLOCKS_PER_TURN,
    )
}

/// Canonical form of the first JSON value in `raw` (Go: Decoder with UseNumber, then Marshal:
/// sorted keys, numbers kept as written, HTML escaping), or None when it does not parse.
fn canonical_json(raw: &[u8]) -> Option<String> {
    let value: Value = serde_json::Deserializer::from_slice(raw)
        .into_iter::<Value>()
        .next()?
        .ok()?;
    go_json_sorted(&value, GoJsonStyle::MARSHAL_USE_NUMBER)
}

fn json_equal(left: &[u8], right: &[u8]) -> bool {
    match (canonical_json(left), canonical_json(right)) {
        (Some(l), Some(r)) => l == r,
        _ => false,
    }
}

/// Appends a turn unless an equal turn already exists, then drops the oldest turns until the
/// per-session turn and byte bounds hold.
fn append_content(contents: &[Vec<u8>], content: &[u8]) -> Vec<Vec<u8>> {
    let mut cloned = contents.to_vec();
    if cloned.iter().any(|existing| json_equal(existing, content)) {
        return cloned;
    }
    cloned.push(content.to_vec());
    while cloned.len() > CLAUDE_THINKING_REPLAY_CACHE_MAX_TURNS_PER_SESSION
        || entry_bytes(&cloned) > CLAUDE_THINKING_REPLAY_CACHE_MAX_BYTES_PER_SESSION
    {
        if cloned.is_empty() {
            break;
        }
        cloned.remove(0);
    }
    cloned
}

impl State {
    fn remove_entry(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.total_bytes -= entry_bytes(&entry.contents);
        }
    }

    fn enforce_limits(&mut self) {
        while self.entries.len() > CLAUDE_THINKING_REPLAY_CACHE_MAX_ENTRIES
            || self.total_bytes > CLAUDE_THINKING_REPLAY_CACHE_MAX_TOTAL_BYTES
        {
            if self.entries.is_empty() {
                self.total_bytes = 0;
                return;
            }
            for key in oldest_keys(&self.entries, CLAUDE_THINKING_REPLAY_CACHE_EVICT_BATCH_SIZE, |e| e.timestamp) {
                self.remove_entry(&key);
            }
        }
    }

    fn reserve_locked(&mut self, key: &str, now: Timestamp) {
        self.entries.insert(
            key.to_string(),
            Entry {
                contents: Vec::new(),
                timestamp: now,
                generation: new_uuid(),
                deleted: true,
            },
        );
        self.enforce_limits();
    }
}

impl ClaudeThinkingReplayCache {
    pub fn new(clock: Clock) -> Self {
        Self {
            state: Mutex::new(State::default()),
            clock,
        }
    }

    /// The process-wide cache (real clock); starts the background purge thread on first use.
    pub fn global() -> &'static ClaudeThinkingReplayCache {
        ensure_cleanup_started();
        &GLOBAL
    }

    /// Seeds the session with one complete signed assistant content array (a JSON array),
    /// replacing any previous turns.
    pub fn cache_best_effort(&self, model_family: &str, session_key: &str, content: &[u8]) -> bool {
        let Some(key) = cache_key(model_family, session_key) else {
            return false;
        };
        if !content_is_valid(content) {
            return false;
        }
        let contents = vec![content.to_vec()];
        let now = self.clock.now();
        let mut state = self.state.lock();
        state.remove_entry(&key);
        state.total_bytes += entry_bytes(&contents);
        state.entries.insert(
            key,
            Entry {
                contents,
                timestamp: now,
                generation: new_uuid(),
                deleted: false,
            },
        );
        state.enforce_limits();
        true
    }

    /// All cached assistant turns for request-time replay (None when absent or empty).
    pub fn get_required(&self, model_family: &str, session_key: &str) -> Option<Vec<Vec<u8>>> {
        self.get_with_snapshot_required(model_family, session_key).0
    }

    /// Replay turns and the exact cache state read. A miss installs a tombstone and snapshots it.
    pub fn get_with_snapshot_required(
        &self,
        model_family: &str,
        session_key: &str,
    ) -> (Option<Vec<Vec<u8>>>, ClaudeThinkingReplaySnapshot) {
        let Some(key) = cache_key(model_family, session_key) else {
            return (None, ClaudeThinkingReplaySnapshot::default());
        };
        let now = self.clock.now();
        let mut state = self.state.lock();
        let expired_or_missing = match state.entries.get(&key) {
            None => true,
            Some(entry) => elapsed(now, entry.timestamp) > CLAUDE_THINKING_REPLAY_CACHE_TTL,
        };
        if expired_or_missing {
            state.remove_entry(&key);
            state.reserve_locked(&key, now);
        }
        let Some(entry) = state.entries.get_mut(&key) else {
            return (None, ClaudeThinkingReplaySnapshot::default());
        };
        entry.timestamp = now;
        let snapshot = ClaudeThinkingReplaySnapshot {
            generation: entry.generation.clone(),
            loaded: true,
            found: true,
        };
        if entry.deleted {
            return (None, snapshot);
        }
        let contents = entry.contents.clone();
        let found = !contents.is_empty();
        (found.then_some(contents), snapshot)
    }

    /// Appends a completed assistant turn only if the request snapshot is still current.
    pub fn replace_if_unchanged(
        &self,
        model_family: &str,
        session_key: &str,
        snapshot: &ClaudeThinkingReplaySnapshot,
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
        let previous = state.entries.get(&key).map(|e| e.contents.clone()).unwrap_or_default();
        let contents = append_content(&previous, content);
        state.total_bytes -= entry_bytes(&previous);
        state.total_bytes += entry_bytes(&contents);
        state.entries.insert(
            key,
            Entry {
                contents,
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
        snapshot: &ClaudeThinkingReplaySnapshot,
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
                contents: Vec::new(),
                timestamp: now,
                generation: new_uuid(),
                deleted: true,
            },
        );
        true
    }

    /// Removes stale replay state unconditionally.
    pub fn delete_required(&self, model_family: &str, session_key: &str) {
        let Some(key) = cache_key(model_family, session_key) else {
            return;
        };
        self.state.lock().remove_entry(&key);
    }

    /// Clears all Claude replay state (and nothing else: Kimi state is a separate cache).
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
            .filter(|(_, e)| elapsed(now, e.timestamp) > CLAUDE_THINKING_REPLAY_CACHE_TTL)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            state.remove_entry(&key);
        }
    }

    #[cfg(test)]
    pub(crate) fn entry_count(&self) -> usize {
        self.state.lock().entries.len()
    }
}

/// Go: CacheClaudeThinkingReplayBestEffort.
pub fn cache_claude_thinking_replay_best_effort(model_family: &str, session_key: &str, content: &[u8]) -> bool {
    ClaudeThinkingReplayCache::global().cache_best_effort(model_family, session_key, content)
}

/// Go: GetClaudeThinkingReplayRequired.
pub fn get_claude_thinking_replay_required(model_family: &str, session_key: &str) -> Option<Vec<Vec<u8>>> {
    ClaudeThinkingReplayCache::global().get_required(model_family, session_key)
}

/// Go: GetClaudeThinkingReplayWithSnapshotRequired.
pub fn get_claude_thinking_replay_with_snapshot_required(
    model_family: &str,
    session_key: &str,
) -> (Option<Vec<Vec<u8>>>, ClaudeThinkingReplaySnapshot) {
    ClaudeThinkingReplayCache::global().get_with_snapshot_required(model_family, session_key)
}

/// Go: ReplaceClaudeThinkingReplayIfUnchanged.
pub fn replace_claude_thinking_replay_if_unchanged(
    model_family: &str,
    session_key: &str,
    snapshot: &ClaudeThinkingReplaySnapshot,
    content: &[u8],
) -> bool {
    ClaudeThinkingReplayCache::global().replace_if_unchanged(model_family, session_key, snapshot, content)
}

/// Go: DeleteClaudeThinkingReplayIfUnchanged.
pub fn delete_claude_thinking_replay_if_unchanged(
    model_family: &str,
    session_key: &str,
    snapshot: &ClaudeThinkingReplaySnapshot,
) -> bool {
    ClaudeThinkingReplayCache::global().delete_if_unchanged(model_family, session_key, snapshot)
}

/// Go: DeleteClaudeThinkingReplayRequired.
pub fn delete_claude_thinking_replay_required(model_family: &str, session_key: &str) {
    ClaudeThinkingReplayCache::global().delete_required(model_family, session_key);
}

/// Go: ClearClaudeThinkingReplayCache.
pub fn clear_claude_thinking_replay_cache() {
    ClaudeThinkingReplayCache::global().clear();
}
