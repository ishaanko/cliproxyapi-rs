//! TTL session -> credential binding cache (Go: auth/session_cache.go).
//!
//! A binding may be reachable under several alias keys (primary and parent/fallback session);
//! aliases move, refresh and expire together. Eviction is insertion-ordered (oldest group first)
//! once the entry cap is exceeded. Expiry is lazy; `cleanup` can be called periodically.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::conductor::clock::Clock;
use crate::conductor::util::add_duration;

const MAX_STABLE_SESSION_ALIASES: usize = 64;
const DEFAULT_MAX_SESSION_ENTRIES: usize = 65536;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    auth_id: String,
    expires_at: DateTime<Utc>,
    aliases: Vec<String>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    groups: HashMap<String, Entry>,
    /// Eviction order: sequence -> primary key; `seq_of` is the reverse index.
    order: BTreeMap<u64, String>,
    seq_of: HashMap<String, u64>,
    next_seq: u64,
}

pub struct SessionCache {
    inner: Mutex<Inner>,
    max_entries: usize,
    ttl: Duration,
    clock: Arc<dyn Clock>,
}

impl SessionCache {
    pub fn new(ttl: Duration, clock: Arc<dyn Clock>) -> Self {
        Self::with_capacity(ttl, DEFAULT_MAX_SESSION_ENTRIES, clock)
    }

    pub fn with_capacity(ttl: Duration, max_entries: usize, clock: Arc<dyn Clock>) -> Self {
        SessionCache {
            inner: Mutex::new(Inner::default()),
            max_entries: if max_entries == 0 {
                DEFAULT_MAX_SESSION_ENTRIES
            } else {
                max_entries
            },
            ttl: if ttl.is_zero() {
                Duration::from_secs(30 * 60)
            } else {
                ttl
            },
            clock,
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    fn expiry(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        add_duration(now, self.ttl)
    }

    /// Credential bound to the session if still valid. Does not refresh the TTL.
    pub fn get(&self, session_id: &str) -> Option<String> {
        if session_id.is_empty() {
            return None;
        }
        let now = self.clock.now();
        let mut inner = self.inner.lock();
        let entry = inner.entries.get(session_id)?.clone();
        if now < entry.expires_at {
            return Some(entry.auth_id);
        }
        inner.remove_group(&entry);
        None
    }

    /// Like `get` but refreshes the TTL of every alias of the logical session.
    pub fn get_and_refresh(&self, session_id: &str) -> Option<String> {
        if session_id.is_empty() {
            return None;
        }
        let now = self.clock.now();
        let mut inner = self.inner.lock();
        let entry = inner.entries.get(session_id)?.clone();
        if now >= entry.expires_at {
            inner.remove_group(&entry);
            return None;
        }
        let aliases = compact_aliases(merge_aliases(&[session_id.to_string()], &entry.aliases));
        let auth_id = entry.auth_id.clone();
        inner.replace_groups(
            &auth_id,
            self.expiry(now),
            aliases,
            &[entry],
            self.max_entries,
        );
        Some(auth_id)
    }

    /// Binds a session to a credential, keeping aliases of the same logical session attached.
    pub fn set(&self, session_id: &str, auth_id: &str) {
        self.set_aliases(auth_id, &[session_id.to_string()]);
    }

    pub fn set_aliases(&self, auth_id: &str, session_ids: &[String]) {
        if auth_id.is_empty() {
            return;
        }
        let now = self.clock.now();
        let mut inner = self.inner.lock();
        let mut aliases = merge_aliases(&[], session_ids);
        let mut previous = Vec::new();
        for id in session_ids {
            let Some(entry) = inner.entries.get(id).cloned() else {
                continue;
            };
            if now >= entry.expires_at {
                inner.remove_group(&entry);
                continue;
            }
            aliases = merge_aliases(&aliases, &entry.aliases);
            previous.push(entry);
        }
        let aliases = compact_aliases(aliases);
        if aliases.is_empty() {
            return;
        }
        inner.replace_groups(
            auth_id,
            self.expiry(now),
            aliases,
            &previous,
            self.max_entries,
        );
    }

    /// Refreshes the binding only while it still points at `expected_auth_id`.
    pub fn touch(&self, session_id: &str, expected_auth_id: &str) -> bool {
        if session_id.is_empty() || expected_auth_id.is_empty() {
            return false;
        }
        let now = self.clock.now();
        let mut inner = self.inner.lock();
        let Some(entry) = inner.entries.get(session_id).cloned() else {
            return false;
        };
        if entry.auth_id != expected_auth_id || now >= entry.expires_at {
            return false;
        }
        let aliases = compact_aliases(merge_aliases(&[session_id.to_string()], &entry.aliases));
        inner.replace_groups(
            expected_auth_id,
            self.expiry(now),
            aliases,
            &[entry],
            self.max_entries,
        );
        true
    }

    /// Removes the binding only if it points at `expected_auth_id`; sibling aliases survive.
    pub fn compare_and_delete(&self, session_id: &str, expected_auth_id: &str) -> bool {
        if session_id.is_empty() || expected_auth_id.is_empty() {
            return false;
        }
        let mut inner = self.inner.lock();
        let Some(entry) = inner.entries.get(session_id).cloned() else {
            return false;
        };
        if entry.auth_id != expected_auth_id {
            return false;
        }
        inner.remove_group(&entry);
        let surviving: Vec<String> = entry
            .aliases
            .iter()
            .filter(|a| *a != session_id)
            .cloned()
            .collect();
        if !surviving.is_empty() {
            let max = self.max_entries;
            inner.replace_groups(&entry.auth_id, entry.expires_at, surviving, &[], max);
        }
        true
    }

    /// Removes every binding of a credential (used when it is removed or its credentials change).
    pub fn invalidate_auth(&self, auth_id: &str) {
        if auth_id.is_empty() {
            return;
        }
        let mut inner = self.inner.lock();
        let doomed: Vec<Entry> = inner
            .groups
            .values()
            .filter(|g| g.auth_id == auth_id)
            .cloned()
            .collect();
        for g in doomed {
            inner.remove_group(&g);
        }
    }

    /// Number of tracked alias keys.
    pub fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drops expired groups.
    pub fn cleanup(&self) {
        let now = self.clock.now();
        let mut inner = self.inner.lock();
        let doomed: Vec<Entry> = inner
            .groups
            .values()
            .filter(|g| now >= g.expires_at)
            .cloned()
            .collect();
        for g in doomed {
            inner.remove_group(&g);
        }
    }
}

impl Inner {
    fn remove_group(&mut self, entry: &Entry) {
        let Some(primary) = entry.aliases.first() else {
            return;
        };
        if self.groups.get(primary).is_some_and(|g| g == entry) {
            self.groups.remove(primary);
            if let Some(seq) = self.seq_of.remove(primary) {
                self.order.remove(&seq);
            }
        }
        for alias in &entry.aliases {
            if self.entries.get(alias).is_some_and(|cur| cur == entry) {
                self.entries.remove(alias);
            }
        }
    }

    fn replace_groups(
        &mut self,
        auth_id: &str,
        expires_at: DateTime<Utc>,
        aliases: Vec<String>,
        previous: &[Entry],
        max_entries: usize,
    ) {
        for p in previous {
            self.remove_group(p);
        }
        let Some(primary) = aliases.first().cloned() else {
            return;
        };
        if let Some(existing) = self.groups.get(&primary).cloned() {
            self.remove_group(&existing);
        }
        let entry = Entry {
            auth_id: auth_id.to_string(),
            expires_at,
            aliases: aliases.clone(),
        };
        self.groups.insert(primary.clone(), entry.clone());
        for alias in &aliases {
            self.entries.insert(alias.clone(), entry.clone());
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.order.insert(seq, primary.clone());
        self.seq_of.insert(primary, seq);
        while self.entries.len() > max_entries {
            let Some((&oldest, _)) = self.order.iter().next() else {
                break;
            };
            let key = self.order.remove(&oldest).unwrap_or_default();
            self.seq_of.remove(&key);
            if let Some(group) = self.groups.get(&key).cloned() {
                self.remove_group(&group);
            }
        }
    }
}

fn is_local_prompt_cache_alias(alias: &str) -> bool {
    if alias.starts_with("pck:") {
        return true;
    }
    alias
        .split_once("::")
        .is_some_and(|(_, rest)| rest.starts_with("pck:"))
}

/// At most one prompt-cache-key alias and 64 stable aliases per group.
fn compact_aliases(aliases: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(aliases.len());
    let mut has_pck = false;
    let mut stable = 0;
    for alias in aliases {
        if is_local_prompt_cache_alias(&alias) {
            if has_pck {
                continue;
            }
            has_pck = true;
        } else {
            if stable >= MAX_STABLE_SESSION_ALIASES {
                continue;
            }
            stable += 1;
        }
        out.push(alias);
    }
    out
}

fn merge_aliases(existing: &[String], candidates: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(existing.len() + candidates.len());
    for alias in existing.iter().chain(candidates) {
        if alias.is_empty() || out.contains(alias) {
            continue;
        }
        out.push(alias.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::clock::ManualClock;

    fn cache(ttl_secs: u64, cap: usize) -> (SessionCache, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(
            DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        ));
        (
            SessionCache::with_capacity(Duration::from_secs(ttl_secs), cap, clock.clone()),
            clock,
        )
    }

    #[test]
    fn ttl_get_does_not_refresh_but_get_and_refresh_does() {
        let (c, clock) = cache(10, 100);
        c.set("s", "auth-a");
        clock.advance(Duration::from_secs(6));
        assert_eq!(c.get("s").as_deref(), Some("auth-a"));
        assert_eq!(c.get_and_refresh("s").as_deref(), Some("auth-a"));
        clock.advance(Duration::from_secs(6));
        assert_eq!(
            c.get("s").as_deref(),
            Some("auth-a"),
            "refresh extended the deadline"
        );
        clock.advance(Duration::from_secs(5));
        assert_eq!(c.get("s"), None);
        assert!(c.is_empty());
    }

    #[test]
    fn aliases_move_together_and_compare_and_delete_is_scoped() {
        let (c, _) = cache(60, 100);
        c.set_aliases("a", &["k1".into(), "k2".into()]);
        assert_eq!(c.get("k2").as_deref(), Some("a"));
        c.set_aliases("b", &["k1".into()]);
        assert_eq!(
            c.get("k2").as_deref(),
            Some("b"),
            "alias group is rebound as a whole"
        );
        assert!(!c.compare_and_delete("k1", "a"));
        assert!(c.compare_and_delete("k1", "b"));
        assert_eq!(c.get("k1"), None);
        assert_eq!(c.get("k2").as_deref(), Some("b"), "sibling alias survives");
    }

    #[test]
    fn eviction_drops_oldest_and_invalidate_auth_removes_all() {
        let (c, _) = cache(60, 2);
        c.set("s1", "a");
        c.set("s2", "a");
        c.set("s3", "b");
        assert_eq!(c.get("s1"), None);
        assert_eq!(c.get("s2").as_deref(), Some("a"));
        c.invalidate_auth("a");
        assert_eq!(c.get("s2"), None);
        assert_eq!(c.get("s3").as_deref(), Some("b"));
    }

    #[test]
    fn touch_requires_matching_auth() {
        let (c, clock) = cache(10, 10);
        c.set("s", "a");
        clock.advance(Duration::from_secs(8));
        assert!(!c.touch("s", "b"));
        assert!(c.touch("s", "a"));
        clock.advance(Duration::from_secs(8));
        assert!(c.get("s").is_some());
    }
}
