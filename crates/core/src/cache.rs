//! In-process caches (Go: internal/cache): the thinking-signature cache and the replay caches
//! shared by executors and translators, plus the generic [`BoundedLru`].
//!
//! Each cache is a struct owning its state behind a `parking_lot::Mutex` and a [`Clock`], so tests
//! can drive TTLs with a mock clock instead of sleeping. The Go package-level state is exposed as
//! `Type::global()` singletons (real clock) and Go-named free functions that delegate to them,
//! re-exported flat from this module. The first access to a global cache starts one background
//! thread that purges expired entries every [`CACHE_CLEANUP_INTERVAL`], like Go's cleanup
//! goroutine.
//!
//! Not ported: the `internal/home` KV mode. Go branches to a remote KV client when the home
//! control plane is active; here only the in-memory path exists, so the Go `context` parameters and
//! the `error` results that only the KV path can produce are dropped (`*Required` variants return
//! plain values). The Go signature-cache toggles ([`set_signature_cache_enabled`],
//! [`set_signature_bypass_strict_mode`]) stay process-wide atomics.

mod antigravity_reasoning_replay;
mod bounded_lru;
mod claude_thinking_replay;
mod clock;
mod codex_reasoning_replay;
mod kv;
mod kimi_thinking_replay;
mod signature_cache;
mod xai_reasoning_replay;

pub use antigravity_reasoning_replay::*;
pub use bounded_lru::*;
pub use claude_thinking_replay::*;
pub use clock::*;
pub use codex_reasoning_replay::*;
pub use kv::{KvBackend, KvError, KvResult, install_kv_backend};
pub use kimi_thinking_replay::*;
pub use signature_cache::*;
pub use xai_reasoning_replay::*;

use std::collections::HashMap;
use std::sync::Once;
use std::time::Duration;

/// How often the background thread purges stale entries from every global cache.
pub const CACHE_CLEANUP_INTERVAL: Duration = Duration::from_secs(10 * 60);

static CLEANUP_ONCE: Once = Once::new();

/// Starts the purge thread (once per process). Called from every `global()` accessor.
pub(crate) fn ensure_cleanup_started() {
    CLEANUP_ONCE.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("cpa-cache-cleanup".into())
            .spawn(|| loop {
                std::thread::sleep(CACHE_CLEANUP_INTERVAL);
                purge_expired_caches();
            });
        if let Err(err) = spawned {
            tracing::warn!("cache cleanup thread not started: {err}");
        }
    });
}

/// Purges expired entries from every global cache (Go: purgeExpiredCaches).
pub fn purge_expired_caches() {
    SignatureCache::global().purge_expired();
    CodexReasoningReplayCache::global().purge_expired();
    XaiReasoningReplayCache::global().purge_expired();
    AntigravityReasoningReplayCache::global().purge_expired();
    KimiThinkingReplayCache::global().purge_expired();
    ClaudeThinkingReplayCache::global().purge_expired();
}

/// Keys of the `count` oldest entries by timestamp (the shared batch-eviction step of the replay
/// caches). Ties break arbitrarily, as in Go's unstable sort.
pub(crate) fn oldest_keys<E>(
    entries: &HashMap<String, E>,
    count: usize,
    timestamp: impl Fn(&E) -> Duration,
) -> Vec<String> {
    let mut candidates: Vec<(&String, Duration)> = entries
        .iter()
        .map(|(key, entry)| (key, timestamp(entry)))
        .collect();
    candidates.sort_by_key(|(_, ts)| *ts);
    candidates
        .into_iter()
        .take(count)
        .map(|(key, _)| key.clone())
        .collect()
}

/// `now - since`, saturating at zero (Go `now.Sub(t)` for a `t` that is never in the future).
pub(crate) fn elapsed(now: Duration, since: Duration) -> Duration {
    now.saturating_sub(since)
}

/// Joins the trimmed `(a, b)` into the `\0`-separated key shared by the replay caches. Empty after
/// trimming means "no scope", reported as `None`.
pub(crate) fn scoped_key(kind: &str, scope: &str, session: &str) -> Option<String> {
    let (scope, session) = (scope.trim(), session.trim());
    if scope.is_empty() || session.is_empty() {
        return None;
    }
    Some(format!("{kind}\0{scope}\0{session}"))
}

/// A fresh random identifier (Go: uuid.NewString).
pub(crate) fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests;
