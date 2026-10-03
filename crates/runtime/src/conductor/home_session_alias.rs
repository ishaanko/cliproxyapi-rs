//! Home session ids (Go: `home_session_alias.go`). Home knows one session id per request, while
//! clients may present several identifiers for the same conversation. The alias cache maps all
//! identifiers seen together to one canonical id, so Home's affinity stays stable.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::Manager;
use super::home::trimmed_meta;
use super::session::{bound_session_identity, identity};
use crate::executor::{Options, meta};

const DEFAULT_ALIAS_TTL: Duration = Duration::from_secs(3600);
const CLEANUP_OPS: u64 = 256;
const SOFT_LIMIT: usize = 4096;
const MAX_STABLE_ALIASES: usize = 64;

#[derive(Clone, PartialEq, Eq)]
struct Entry {
    canonical: String,
    expires_at: DateTime<Utc>,
    aliases: Vec<String>,
}

/// Reconciles multiple client identifiers of one Home session.
#[derive(Default)]
pub(crate) struct AliasCache {
    entries: HashMap<String, Entry>,
    groups: HashMap<String, Entry>,
    eviction_order: VecDeque<String>,
    ops: u64,
}

fn merge_aliases(existing: &[String], candidates: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(existing.len() + candidates.len());
    for alias in existing.iter().chain(candidates) {
        if !alias.is_empty() && !out.contains(alias) {
            out.push(alias.clone());
        }
    }
    out
}

/// At most one `pck:` alias and 64 stable aliases (Go: `compactHomeSessionAliases`).
fn compact_aliases(aliases: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(aliases.len());
    let (mut has_pck, mut stable) = (false, 0);
    for alias in aliases {
        if alias.starts_with("pck:") {
            if has_pck {
                continue;
            }
            has_pck = true;
        } else {
            if stable >= MAX_STABLE_ALIASES {
                continue;
            }
            stable += 1;
        }
        out.push(alias);
    }
    out
}

impl AliasCache {
    pub fn clear(&mut self) {
        *self = AliasCache::default();
    }

    /// The canonical id for `primary` (and `fallback`), registering the group.
    pub fn canonical(&mut self, primary: &str, fallback: &str, ttl: Duration, now: DateTime<Utc>) -> String {
        let (primary, fallback) = (primary.trim(), fallback.trim());
        if primary.is_empty() {
            return String::new();
        }
        let ttl = if ttl.is_zero() { DEFAULT_ALIAS_TTL } else { ttl };
        self.ops += 1;
        if self.ops.is_multiple_of(CLEANUP_OPS) {
            self.cleanup(now);
        }

        let mut canonical = primary.to_string();
        let mut aliases = merge_aliases(&[], &[primary.to_string(), fallback.to_string()]);
        let mut previous: HashMap<String, Entry> = HashMap::new();

        let mut primary_found = false;
        let mut from_live_alias = false;
        if let Some(existing) = self.entry(primary, now) {
            primary_found = true;
            from_live_alias = true;
            canonical = existing.canonical.clone();
            aliases = merge_aliases(&aliases, &existing.aliases);
            previous.insert(existing.canonical.clone(), existing);
        }
        if !fallback.is_empty()
            && fallback != primary
            && let Some(existing) = self.entry(fallback, now)
        {
            from_live_alias = true;
            if !primary_found {
                canonical = existing.canonical.clone();
            }
            aliases = merge_aliases(&aliases, &existing.aliases);
            previous.insert(existing.canonical.clone(), existing);
        }
        if from_live_alias {
            if let Some(existing) = self.group(&canonical, now) {
                aliases = merge_aliases(&aliases, &existing.aliases);
                previous.insert(existing.canonical.clone(), existing);
            }
        } else if self.group(&canonical, now).is_some() {
            return canonical;
        }
        let aliases = compact_aliases(merge_aliases(&aliases, std::slice::from_ref(&canonical)));
        for entry in previous.into_values() {
            self.remove_group(&entry);
        }
        self.set_group(Entry {
            canonical: canonical.clone(),
            expires_at: now + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::hours(1)),
            aliases,
        });
        self.enforce_limit(SOFT_LIMIT);
        canonical
    }

    fn entry(&mut self, alias: &str, now: DateTime<Utc>) -> Option<Entry> {
        let entry = self.entries.get(alias)?.clone();
        if now < entry.expires_at {
            return Some(entry);
        }
        if let Some(group) = self.groups.get(&entry.canonical).cloned()
            && group == entry
        {
            self.remove_group(&group);
        } else {
            self.entries.remove(alias);
        }
        None
    }

    fn group(&mut self, canonical: &str, now: DateTime<Utc>) -> Option<Entry> {
        let entry = self.groups.get(canonical)?.clone();
        if now < entry.expires_at {
            return Some(entry);
        }
        self.remove_group(&entry);
        None
    }

    fn set_group(&mut self, entry: Entry) {
        if let Some(existing) = self.groups.get(&entry.canonical).cloned() {
            self.remove_group(&existing);
        }
        self.groups.insert(entry.canonical.clone(), entry.clone());
        for alias in &entry.aliases {
            self.entries.insert(alias.clone(), entry.clone());
        }
        self.eviction_order.push_back(entry.canonical);
    }

    fn remove_group(&mut self, entry: &Entry) {
        let Some(current) = self.groups.get(&entry.canonical).cloned() else { return };
        if current != *entry {
            return;
        }
        for alias in &current.aliases {
            if self.entries.get(alias).is_some_and(|m| *m == current) {
                self.entries.remove(alias);
            }
        }
        self.groups.remove(&current.canonical);
        self.eviction_order.retain(|c| *c != current.canonical);
    }

    fn enforce_limit(&mut self, limit: usize) {
        while self.entries.len() > limit {
            let Some(oldest) = self.eviction_order.front().cloned() else { return };
            match self.groups.get(&oldest).cloned() {
                Some(entry) => self.remove_group(&entry),
                None => {
                    self.eviction_order.pop_front();
                }
            }
        }
    }

    fn cleanup(&mut self, now: DateTime<Utc>) {
        let expired: Vec<Entry> = self.groups.values().filter(|e| now >= e.expires_at).cloned().collect();
        for entry in expired {
            self.remove_group(&entry);
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Go: `isHierarchyParent`.
fn is_hierarchy_parent(primary: &str, fallback: &str) -> bool {
    if fallback.is_empty() || primary.is_empty() || primary == fallback {
        return false;
    }
    if primary.contains(":agent:") {
        return true;
    }
    let (idx1, idx2) = (primary.find(':'), fallback.find(':'));
    if let (Some(i1), Some(i2)) = (idx1, idx2)
        && i1 > 0
        && i2 > 0
        && primary[..i1] == fallback[..i2]
    {
        return true;
    }
    idx1.is_none() && idx2.is_none()
}

/// Go: `homeSessionAliasTTL`.
fn alias_ttl(cfg: &cpa_config::Config) -> Duration {
    let raw = cfg.routing.session_affinity_ttl.trim();
    if raw.is_empty() {
        return DEFAULT_ALIAS_TTL;
    }
    match cpa_config::GoDuration::parse(raw) {
        Ok(d) if d.0 > 0 => d.to_std(),
        _ => DEFAULT_ALIAS_TTL,
    }
}

pub(crate) fn home_alias_ttl_changed(previous: &cpa_config::Config, next: &cpa_config::Config) -> bool {
    alias_ttl(previous) != alias_ttl(next)
}

impl Manager {
    /// Go: `homeDispatchSessionIDs`: the canonical session id and parent session id sent to Home.
    /// The LCP prefix matcher is not ported, so a request without any explicit, canonical or
    /// derived identity falls back to the message-hash ids only.
    pub(crate) fn home_dispatch_session_ids(&self, opts: &mut Options) -> (String, String) {
        let (mut primary, mut fallback) =
            identity::explicit_session_ids(&opts.headers, &opts.original_request, &mut opts.metadata);
        let mut has_authoritative_input = !primary.is_empty();
        if primary.is_empty() {
            primary = trimmed_meta(&opts.metadata, meta::CANONICAL_SESSION_ID);
            if primary.is_empty() {
                primary = trimmed_meta(&opts.metadata, "lcp_affinity_session_id");
            }
        }
        if primary.is_empty() {
            let (p, f) = identity::session_ids(&opts.headers, &opts.original_request, &mut opts.metadata);
            primary = p;
            fallback = f;
            has_authoritative_input = !primary.is_empty();
        }
        if primary.is_empty() {
            return (primary, String::new());
        }

        let (mut parent, mut alias_fallback) = (String::new(), String::new());
        if !fallback.is_empty() && fallback != primary {
            if is_hierarchy_parent(&primary, &fallback) {
                parent = fallback.clone();
            } else {
                alias_fallback = fallback.clone();
            }
        }
        if !has_authoritative_input && parent.is_empty() {
            parent = trimmed_meta(&opts.metadata, meta::PARENT_SESSION_ID);
        }

        let ttl = alias_ttl(&self.cfg());
        let now = self.now();
        let mut aliases = self.home.aliases.lock();
        let mut canonical = aliases.canonical(&primary, &alias_fallback, ttl, now);
        if !parent.is_empty() {
            if parent == canonical || parent == primary || (!alias_fallback.is_empty() && parent == alias_fallback) {
                parent.clear();
            } else {
                parent = aliases.canonical(&parent, "", ttl, now);
                if parent == canonical {
                    parent.clear();
                }
            }
        }
        drop(aliases);
        canonical = bound_session_identity(&canonical);
        if !parent.is_empty() {
            parent = bound_session_identity(&parent);
        }
        if canonical == parent {
            parent.clear();
        }
        (canonical, parent)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap()
    }

    #[test]
    fn aliases_share_one_canonical_id_and_expire() {
        let mut c = AliasCache::default();
        let ttl = Duration::from_secs(60);
        assert_eq!(c.canonical("a", "", ttl, now()), "a");
        // `b` seen together with `a` joins a's group.
        assert_eq!(c.canonical("b", "a", ttl, now()), "a");
        assert_eq!(c.canonical("b", "", ttl, now()), "a");
        assert_eq!(c.canonical("a", "", ttl, now()), "a");
        let later = now() + chrono::Duration::seconds(61);
        assert_eq!(c.canonical("b", "", ttl, later), "b");
        assert_eq!(c.canonical("", "x", ttl, later), "");
    }

    #[test]
    fn prompt_cache_aliases_are_compacted_to_one() {
        assert_eq!(
            compact_aliases(vec!["pck:1".into(), "pck:2".into(), "s1".into()]),
            vec!["pck:1".to_string(), "s1".to_string()]
        );
    }

    #[test]
    fn size_is_bounded_by_evicting_the_oldest_groups() {
        let mut c = AliasCache::default();
        for i in 0..(SOFT_LIMIT + 10) {
            c.canonical(&format!("s{i}"), "", Duration::from_secs(60), now());
        }
        assert!(c.len() <= SOFT_LIMIT);
        assert_eq!(c.canonical("s0", "", Duration::from_secs(60), now()), "s0");
    }

    #[test]
    fn hierarchy_parent_rules() {
        assert!(is_hierarchy_parent("claude:agent:x", "claude:y"));
        assert!(is_hierarchy_parent("cc:1", "cc:2"));
        assert!(is_hierarchy_parent("plain1", "plain2"));
        assert!(!is_hierarchy_parent("a:1", "b:2"));
        assert!(!is_hierarchy_parent("same", "same"));
        assert!(!is_hierarchy_parent("x", ""));
    }
}
