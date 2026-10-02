//! Thinking-signature cache (Go: cache/signature_cache.go): model group -> text hash -> signature,
//! with a sliding 3 hour TTL, used by the Antigravity Claude translator.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use super::{Clock, Timestamp, elapsed, ensure_cleanup_started};
use crate::signature::GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR;

/// How long signatures are valid.
pub const SIGNATURE_CACHE_TTL: Duration = Duration::from_secs(3 * 3600);
/// Length of the hash key (16 hex chars = 64-bit key space).
pub const SIGNATURE_TEXT_HASH_LEN: usize = 16;
/// Minimum length for a signature to be considered valid.
pub const MIN_VALID_SIGNATURE_LEN: usize = 50;

/// A cached thinking signature with its last-touched time.
#[derive(Debug, Clone)]
pub struct SignatureEntry {
    pub signature: String,
    pub timestamp: Timestamp,
}

type Group = HashMap<String, SignatureEntry>;

/// Signature cache. Use [`SignatureCache::global`] (or the free functions) in production code.
pub struct SignatureCache {
    groups: Mutex<HashMap<String, Group>>,
    clock: Clock,
}

static GLOBAL: LazyLock<SignatureCache> = LazyLock::new(|| SignatureCache::new(Clock::real()));

/// Stable, Unicode-safe key from text content: first 16 hex chars of its SHA-256.
fn hash_text(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = hex::encode(digest);
    hex.truncate(SIGNATURE_TEXT_HASH_LEN);
    hex
}

/// Model group a model's signatures are cached under: `gpt`, `claude`, `gemini`, or the model
/// name itself.
pub fn get_model_group(model_name: &str) -> String {
    if model_name.contains("gpt") {
        "gpt".to_string()
    } else if model_name.contains("claude") {
        "claude".to_string()
    } else if model_name.contains("gemini") {
        "gemini".to_string()
    } else {
        model_name.to_string()
    }
}

/// A signature is valid when it is long enough, or the Gemini skip sentinel for a Gemini model.
pub fn has_valid_signature(model_name: &str, signature: &str) -> bool {
    (!signature.is_empty() && signature.len() >= MIN_VALID_SIGNATURE_LEN)
        || (signature == GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR && get_model_group(model_name) == "gemini")
}

impl SignatureCache {
    pub fn new(clock: Clock) -> Self {
        Self {
            groups: Mutex::new(HashMap::new()),
            clock,
        }
    }

    /// The process-wide cache (real clock); starts the background purge thread on first use.
    pub fn global() -> &'static SignatureCache {
        ensure_cleanup_started();
        &GLOBAL
    }

    /// Stores a signature for a model group and thinking text. Ignored (returns false) when the
    /// text is empty or the signature is shorter than [`MIN_VALID_SIGNATURE_LEN`].
    pub fn cache_signature_best_effort(&self, model_name: &str, text: &str, signature: &str) -> bool {
        if text.is_empty() || signature.is_empty() || signature.len() < MIN_VALID_SIGNATURE_LEN {
            return false;
        }
        let group_key = get_model_group(model_name);
        let text_hash = hash_text(text);
        let now = self.clock.now();
        self.groups.lock().entry(group_key).or_default().insert(
            text_hash,
            SignatureEntry {
                signature: signature.to_string(),
                timestamp: now,
            },
        );
        true
    }

    /// The cached signature, refreshing its TTL (sliding expiration). Misses and expired entries
    /// return `""`, except for the `gemini` group, which returns the skip sentinel (also when the
    /// text is empty).
    pub fn get_cached_signature_required(&self, model_name: &str, text: &str) -> String {
        let group_key = get_model_group(model_name);
        let miss = || {
            if group_key == "gemini" {
                GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string()
            } else {
                String::new()
            }
        };
        if text.is_empty() {
            return miss();
        }

        let text_hash = hash_text(text);
        let now = self.clock.now();
        let mut groups = self.groups.lock();
        let Some(group) = groups.get_mut(&group_key) else {
            return miss();
        };
        let Some(entry) = group.get_mut(&text_hash) else {
            return miss();
        };
        if elapsed(now, entry.timestamp) > SIGNATURE_CACHE_TTL {
            group.remove(&text_hash);
            return miss();
        }
        entry.timestamp = now;
        entry.signature.clone()
    }

    /// Clears one model group, or every group when `model_name` is empty.
    pub fn clear(&self, model_name: &str) {
        let mut groups = self.groups.lock();
        if model_name.is_empty() {
            groups.clear();
        } else {
            groups.remove(&get_model_group(model_name));
        }
    }

    /// Removes one exact cached signature (dropping the group when it becomes empty).
    pub fn delete_cached_signature_required(&self, model_name: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        let group_key = get_model_group(model_name);
        let text_hash = hash_text(text);
        let mut groups = self.groups.lock();
        if let Some(group) = groups.get_mut(&group_key) {
            group.remove(&text_hash);
            if group.is_empty() {
                groups.remove(&group_key);
            }
        }
    }

    /// Drops expired entries and empty groups.
    pub fn purge_expired(&self) {
        let now = self.clock.now();
        let mut groups = self.groups.lock();
        groups.retain(|_, group| {
            group.retain(|_, entry| elapsed(now, entry.timestamp) <= SIGNATURE_CACHE_TTL);
            !group.is_empty()
        });
    }

    #[cfg(test)]
    pub(crate) fn group_len(&self, group: &str) -> Option<usize> {
        self.groups.lock().get(group).map(HashMap::len)
    }
}

/// Stores a thinking signature for a model group and text (Go: CacheSignature).
pub fn cache_signature(model_name: &str, text: &str, signature: &str) {
    SignatureCache::global().cache_signature_best_effort(model_name, text, signature);
}

/// Go: CacheSignatureBestEffort.
pub fn cache_signature_best_effort(model_name: &str, text: &str, signature: &str) -> bool {
    SignatureCache::global().cache_signature_best_effort(model_name, text, signature)
}

/// Go: GetCachedSignature.
pub fn get_cached_signature(model_name: &str, text: &str) -> String {
    SignatureCache::global().get_cached_signature_required(model_name, text)
}

/// Go: GetCachedSignatureRequired.
pub fn get_cached_signature_required(model_name: &str, text: &str) -> String {
    SignatureCache::global().get_cached_signature_required(model_name, text)
}

/// Go: ClearSignatureCache.
pub fn clear_signature_cache(model_name: &str) {
    SignatureCache::global().clear(model_name);
}

/// Go: DeleteCachedSignatureRequired.
pub fn delete_cached_signature_required(model_name: &str, text: &str) {
    SignatureCache::global().delete_cached_signature_required(model_name, text);
}

static SIGNATURE_CACHE_ENABLED: AtomicBool = AtomicBool::new(true);
static SIGNATURE_BYPASS_STRICT_MODE: AtomicBool = AtomicBool::new(false);

/// Switches Antigravity signature handling between cache mode (true, default) and bypass mode.
pub fn set_signature_cache_enabled(enabled: bool) {
    let previous = SIGNATURE_CACHE_ENABLED.swap(enabled, Ordering::SeqCst);
    if previous == enabled {
        return;
    }
    if !enabled {
        tracing::info!(
            "antigravity signature cache DISABLED - bypass mode active, cached signatures will not be used for request translation"
        );
    }
}

/// Whether signature cache validation is enabled.
pub fn signature_cache_enabled() -> bool {
    SIGNATURE_CACHE_ENABLED.load(Ordering::SeqCst)
}

/// Controls whether bypass mode uses strict protobuf-tree validation.
pub fn set_signature_bypass_strict_mode(strict: bool) {
    let previous = SIGNATURE_BYPASS_STRICT_MODE.swap(strict, Ordering::SeqCst);
    if previous == strict {
        return;
    }
    if strict {
        tracing::debug!("antigravity bypass signature validation: strict mode (protobuf tree)");
    } else {
        tracing::debug!("antigravity bypass signature validation: basic mode (R/E + 0x12)");
    }
}

/// Whether bypass mode uses strict protobuf-tree validation.
pub fn signature_bypass_strict_mode() -> bool {
    SIGNATURE_BYPASS_STRICT_MODE.load(Ordering::SeqCst)
}
