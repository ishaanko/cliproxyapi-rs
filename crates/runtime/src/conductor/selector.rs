//! Credential selection strategies and session-affinity binding (Go: auth/selector.go,
//! scheduler.go pick semantics).
//!
//! The manager narrows candidates (availability, model support, priority tier) and hands the
//! selector ID-sorted slices; the selector only decides *which* of them to use:
//!
//! - round-robin: identity-based successor of the previous pick, so shrinking candidate sets
//!   (retries, cooldowns) do not skew the rotation,
//! - weighted round-robin: smooth (nginx style) accumulators, reset only when a weight changes,
//! - fill-first: lowest id,
//! - smart-quota (Rust-only): highest usage-aware score, see [`super::quota_windows`].
//!
//! With session affinity enabled, a [`SessionAffinity`] wrapper binds a session to a credential
//! (TTL cache) and falls back to the configured strategy for cold or failed bindings.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use super::clock::Clock;
use super::errors::should_skip_credential_cooldown;
use super::quota_windows::Score;
use super::session::{self, SessionCache};
use super::util::canonical_model_key;
use crate::executor::{Metadata, meta};

const MAX_ROTATION_KEYS: usize = 4096;
const MAX_WEIGHT_STATE_ENTRIES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    #[default]
    RoundRobin,
    WeightedRoundRobin,
    FillFirst,
    /// Rust-only: prefer credentials with 5h headroom, expiring weekly allowance and low load.
    SmartQuota,
}

impl Strategy {
    /// Case-insensitive config parse; anything unknown is round-robin.
    pub fn parse(s: &str) -> Strategy {
        match s.trim().to_lowercase().as_str() {
            "weighted-round-robin" | "weightedroundrobin" | "wrr" => Strategy::WeightedRoundRobin,
            "fill-first" | "fillfirst" | "ff" => Strategy::FillFirst,
            "smart-quota" | "smartquota" | "sq" => Strategy::SmartQuota,
            _ => Strategy::RoundRobin,
        }
    }
}

/// Default `routing.smart-quota-reserve-percent`.
pub const DEFAULT_SMART_QUOTA_RESERVE: u8 = 30;

/// The normalized selector configuration; the manager rebuilds the selector only on change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectorConfig {
    pub strategy: Strategy,
    /// 5h headroom percent smart-quota tries to keep free; other strategies keep the default so a
    /// setting they ignore never rebuilds the selector.
    pub smart_quota_reserve: u8,
    pub session_affinity: bool,
    pub affinity_ttl: Duration,
    pub subagent_affinity: bool,
}

impl Default for SelectorConfig {
    fn default() -> Self {
        SelectorConfig {
            strategy: Strategy::RoundRobin,
            smart_quota_reserve: DEFAULT_SMART_QUOTA_RESERVE,
            session_affinity: false,
            affinity_ttl: Duration::from_secs(3600),
            subagent_affinity: true,
        }
    }
}

/// A selectable credential as seen by the strategies.
#[derive(Debug, Clone, Copy)]
pub struct Cand<'a> {
    pub id: &'a str,
    /// Executor key of the credential.
    pub provider: &'a str,
    pub weight: i64,
    /// Smart-quota score; `None` scores like a credential without data.
    pub smart: Option<Score>,
}

#[derive(Debug, Default, Clone)]
struct SmoothWeightedState {
    current: HashMap<String, i64>,
    weights: HashMap<String, i64>,
}

impl SmoothWeightedState {
    /// Syncs configured weights; accumulated credits are reset only when a credential present in
    /// both vectors changes weight (a smaller candidate set is not a config change).
    fn prepare(&mut self, weights: &HashMap<String, i64>) {
        if self.weights_config_changed(weights) {
            self.current.clear();
        }
        for (id, w) in weights {
            self.weights.insert(id.clone(), *w);
        }
        if self.current.len() > MAX_WEIGHT_STATE_ENTRIES
            || self.weights.len() > MAX_WEIGHT_STATE_ENTRIES
        {
            self.current.retain(|id, _| weights.contains_key(id));
            self.weights.retain(|id, _| weights.contains_key(id));
        }
    }

    fn weights_config_changed(&self, right: &HashMap<String, i64>) -> bool {
        if self.weights.is_empty() {
            return false;
        }
        right
            .iter()
            .any(|(id, w)| self.weights.get(id).is_some_and(|prev| prev != w))
    }

    /// Smooth WRR step over `cands` (first max wins ties, in slice order).
    fn pick(&mut self, cands: &[Cand<'_>]) -> Option<usize> {
        let weights: HashMap<String, i64> = cands
            .iter()
            .filter(|c| c.weight > 0)
            .map(|c| (c.id.to_string(), c.weight))
            .collect();
        self.prepare(&weights);
        let mut picked: Option<usize> = None;
        let mut picked_current = 0i64;
        let mut total = 0i64;
        for (i, c) in cands.iter().enumerate() {
            if c.weight <= 0 {
                continue;
            }
            let cur = self.current.entry(c.id.to_string()).or_insert(0);
            *cur = cur.saturating_add(c.weight);
            total = total.saturating_add(c.weight);
            if picked.is_none() || *cur > picked_current {
                picked = Some(i);
                picked_current = *cur;
            }
        }
        let i = picked?;
        let cur = self.current.entry(cands[i].id.to_string()).or_insert(0);
        *cur = cur.saturating_add(-total);
        Some(i)
    }
}

#[derive(Default)]
struct Rotation {
    last_picked: HashMap<String, String>,
    weighted: HashMap<String, SmoothWeightedState>,
    mixed_cursors: HashMap<String, usize>,
}

/// Index of the first candidate ordered after `last_id`, wrapping to the start (candidates are
/// sorted by id).
fn successor_index(cands: &[Cand<'_>], last_id: Option<&str>) -> usize {
    let Some(last) = last_id.filter(|l| !l.is_empty()) else {
        return 0;
    };
    let idx = cands.partition_point(|c| c.id <= last);
    if idx >= cands.len() { 0 } else { idx }
}

pub struct Selector {
    pub config: SelectorConfig,
    rotation: Mutex<Rotation>,
    affinity: Option<SessionAffinity>,
}

impl Selector {
    pub fn new(config: SelectorConfig, clock: Arc<dyn Clock>) -> Self {
        let affinity = config
            .session_affinity
            .then(|| SessionAffinity::new(config.affinity_ttl, config.subagent_affinity, clock));
        Selector {
            config,
            rotation: Mutex::new(Rotation::default()),
            affinity,
        }
    }

    pub fn affinity(&self) -> Option<&SessionAffinity> {
        self.affinity.as_ref()
    }

    /// Strategy pick over one provider's top-tier candidates (sorted by id). `key` identifies
    /// the rotation (provider/model/priority).
    pub fn pick_ordered(&self, key: &str, cands: &[Cand<'_>]) -> Option<usize> {
        self.pick_ordered_with(self.config.strategy, key, cands)
    }

    /// [`Self::pick_ordered`] with an explicit strategy (a plugin scheduler delegating to a
    /// built-in strategy).
    pub fn pick_ordered_with(&self, strategy: Strategy, key: &str, cands: &[Cand<'_>]) -> Option<usize> {
        if cands.is_empty() {
            return None;
        }
        match strategy {
            Strategy::FillFirst => Some(0),
            Strategy::SmartQuota => self.pick_smart(key, cands),
            Strategy::WeightedRoundRobin => {
                let mut rot = self.rotation.lock();
                if !rot.weighted.contains_key(key) && rot.weighted.len() >= MAX_ROTATION_KEYS {
                    rot.weighted.clear();
                }
                rot.weighted.entry(key.to_string()).or_default().pick(cands)
            }
            Strategy::RoundRobin => {
                let mut rot = self.rotation.lock();
                if !rot.last_picked.contains_key(key) && rot.last_picked.len() >= MAX_ROTATION_KEYS
                {
                    rot.last_picked.clear();
                }
                let i = successor_index(cands, rot.last_picked.get(key).map(String::as_str));
                rot.last_picked
                    .insert(key.to_string(), cands[i].id.to_string());
                Some(i)
            }
        }
    }

    /// Smart-quota pick over `cands` (sorted by id). With a reserve configured and at least one
    /// candidate known to have that much 5h headroom, only those compete; otherwise everyone
    /// does. The highest weight wins; exact ties rotate by id like round-robin under `key`.
    fn pick_smart(&self, key: &str, cands: &[Cand<'_>]) -> Option<usize> {
        const TIE: f64 = 1e-9;
        let score = |c: &Cand<'_>| c.smart.unwrap_or(Score::UNKNOWN);
        let reserve = f64::from(self.config.smart_quota_reserve);
        let has_reserve = |c: &Cand<'_>| {
            score(c)
                .five_hour_headroom
                .is_some_and(|h| h + TIE >= reserve)
        };
        let restrict = reserve > 0.0 && cands.iter().any(has_reserve);
        let eligible = |c: &Cand<'_>| !restrict || has_reserve(c);
        let best = cands
            .iter()
            .filter(|c| eligible(c))
            .map(|c| score(c).weight)
            .max_by(f64::total_cmp)?;
        let tied: Vec<(usize, Cand<'_>)> = cands
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, c)| eligible(c) && score(c).weight >= best - TIE)
            .collect();
        let tied_cands: Vec<Cand<'_>> = tied.iter().map(|(_, c)| *c).collect();
        let mut rot = self.rotation.lock();
        if !rot.last_picked.contains_key(key) && rot.last_picked.len() >= MAX_ROTATION_KEYS {
            rot.last_picked.clear();
        }
        let i = successor_index(&tied_cands, rot.last_picked.get(key).map(String::as_str));
        rot.last_picked
            .insert(key.to_string(), tied_cands[i].id.to_string());
        Some(tied[i].0)
    }

    /// Multi-provider pick (scheduler semantics): `cands` are the best-priority-tier candidates
    /// of all requested `providers`, sorted by id. Fill-first uses the first provider with a
    /// ready credential; weighted uses one accumulator over the union; round-robin rotates a
    /// provider cursor weighted by credential count, then each provider's own rotation.
    pub fn pick_mixed(
        &self,
        providers: &[String],
        model_key: &str,
        priority: i64,
        cands: &[Cand<'_>],
    ) -> Option<usize> {
        self.pick_mixed_with(self.config.strategy, providers, model_key, priority, cands)
    }

    /// [`Self::pick_mixed`] with an explicit strategy.
    pub fn pick_mixed_with(
        &self,
        strategy: Strategy,
        providers: &[String],
        model_key: &str,
        priority: i64,
        cands: &[Cand<'_>],
    ) -> Option<usize> {
        if cands.is_empty() {
            return None;
        }
        let cursor_key = format!("{}:{model_key}", providers.join(","));
        match strategy {
            Strategy::FillFirst => providers
                .iter()
                .find_map(|p| cands.iter().position(|c| c.provider == p)),
            Strategy::SmartQuota => self.pick_smart(&format!("smart:{cursor_key}"), cands),
            Strategy::WeightedRoundRobin => {
                let mut rot = self.rotation.lock();
                rot.weighted
                    .entry(format!("mixed:{cursor_key}"))
                    .or_default()
                    .pick(cands)
            }
            Strategy::RoundRobin => {
                let mut rot = self.rotation.lock();
                let weights: Vec<usize> = providers
                    .iter()
                    .map(|p| cands.iter().filter(|c| c.provider == p).count())
                    .collect();
                let total: usize = weights.iter().sum();
                if total == 0 {
                    return None;
                }
                let mut seg_starts = Vec::with_capacity(providers.len());
                let mut seg_ends = Vec::with_capacity(providers.len());
                let mut acc = 0;
                for w in &weights {
                    seg_starts.push(acc);
                    acc += w;
                    seg_ends.push(acc);
                }
                let start_slot = rot.mixed_cursors.get(&cursor_key).copied().unwrap_or(0) % total;
                let start_idx =
                    (0..providers.len()).find(|&i| weights[i] > 0 && start_slot < seg_ends[i])?;
                let mut slot = start_slot;
                for offset in 0..providers.len() {
                    let pi = (start_idx + offset) % providers.len();
                    if weights[pi] == 0 {
                        continue;
                    }
                    if pi != start_idx {
                        slot = seg_starts[pi];
                    }
                    let provider = &providers[pi];
                    let shard: Vec<(usize, Cand<'_>)> = cands
                        .iter()
                        .copied()
                        .enumerate()
                        .filter(|(_, c)| c.provider == provider)
                        .collect();
                    let shard_cands: Vec<Cand<'_>> = shard.iter().map(|(_, c)| *c).collect();
                    let key = format!("{provider}:{model_key}:{priority}");
                    if !rot.last_picked.contains_key(&key)
                        && rot.last_picked.len() >= MAX_ROTATION_KEYS
                    {
                        rot.last_picked.clear();
                    }
                    let i = successor_index(
                        &shard_cands,
                        rot.last_picked.get(&key).map(String::as_str),
                    );
                    rot.last_picked.insert(key, shard_cands[i].id.to_string());
                    rot.mixed_cursors.insert(cursor_key.clone(), slot + 1);
                    return Some(shard[i].0);
                }
                None
            }
        }
    }
}

// ---- Session affinity ----

/// Metadata key holding the session ids derived once per request (see [`resolve_affinity_ids`]).
pub const AFFINITY_IDS_KEY: &str = "cpa.session_affinity_ids";

/// Session ids affinity binds on: explicit client ids, and the primary/fallback pair (explicit,
/// else derived/hash).
#[derive(Debug, Clone, Default)]
pub struct AffinityIds {
    pub explicit: (String, String),
    pub primary: String,
    pub fallback: String,
}

/// Parses the request for session ids and stores the result in `metadata` so later picks and the
/// result bookkeeping do not parse the body again. Also records fork/parent hints.
pub fn resolve_affinity_ids(
    headers: &http::HeaderMap,
    body: &[u8],
    metadata: &mut Metadata,
) -> AffinityIds {
    let explicit = session::explicit_session_ids(headers, body, metadata);
    let (primary, fallback) = if explicit.0.is_empty() {
        session::session_ids(headers, body, metadata)
    } else {
        explicit.clone()
    };
    let ids = AffinityIds {
        explicit,
        primary,
        fallback,
    };
    metadata.insert(
        AFFINITY_IDS_KEY.into(),
        serde_json::json!([ids.explicit.0, ids.explicit.1, ids.primary, ids.fallback]),
    );
    ids
}

fn affinity_ids(headers: &http::HeaderMap, body: &[u8], metadata: &mut Metadata) -> AffinityIds {
    if let Some(serde_json::Value::Array(v)) = metadata.get(AFFINITY_IDS_KEY)
        && v.len() == 4
    {
        let s = |i: usize| v[i].as_str().unwrap_or("").to_string();
        return AffinityIds {
            explicit: (s(0), s(1)),
            primary: s(2),
            fallback: s(3),
        };
    }
    resolve_affinity_ids(headers, body, metadata)
}

/// Result of the affinity decision for one pick.
pub enum AffinityPick {
    /// Use this candidate id (from the all-tier list).
    Bound(String),
    /// No usable binding: use the fallback strategy over the top tier, then call `bind`.
    Fallback(AffinityBinder),
    /// No session identity could be derived: plain fallback, nothing to bind.
    Unbound,
}

/// Remembers how to record the binding once the fallback strategy picked a credential.
pub struct AffinityBinder {
    cache_key: String,
    fallback_key: Option<String>,
    is_subagent: bool,
    is_fork: bool,
}

pub struct SessionAffinity {
    cache: SessionCache,
    subagent_affinity: bool,
}

impl SessionAffinity {
    pub fn new(ttl: Duration, subagent_affinity: bool, clock: Arc<dyn Clock>) -> Self {
        SessionAffinity {
            cache: SessionCache::new(ttl, clock),
            subagent_affinity,
        }
    }

    pub fn cache(&self) -> &SessionCache {
        &self.cache
    }

    pub fn invalidate_auth(&self, auth_id: &str) {
        self.cache.invalidate_auth(auth_id);
    }

    /// Decides the credential for a session. `available_ids` are all available credentials
    /// across priority tiers; an existing binding outranks priority. Updates `metadata` with the
    /// affinity namespace and canonical/parent session ids.
    pub fn decide(
        &self,
        provider: &str,
        model: &str,
        headers: &http::HeaderMap,
        original_request: &[u8],
        metadata: &mut Metadata,
        available_ids: &[&str],
    ) -> AffinityPick {
        metadata.insert(meta::SESSION_AFFINITY_PROVIDER.into(), provider.into());
        metadata.insert(meta::SESSION_AFFINITY_MODEL.into(), model.into());

        let ids = affinity_ids(headers, original_request, metadata);
        let (explicit_id, explicit_fallback) = ids.explicit.clone();
        if !explicit_id.is_empty() {
            for k in [
                meta::IS_COMPACTION,
                "node_kind",
                super::session::info::LCP_AFFINITY_SESSION_ID,
                "lcp_access_generation",
            ] {
                metadata.remove(k);
            }
            if explicit_fallback.is_empty() {
                metadata.remove(meta::PARENT_SESSION_ID);
            } else {
                metadata.insert(
                    meta::PARENT_SESSION_ID.into(),
                    session::bound_session_identity(&explicit_fallback).into(),
                );
            }
            let is_fork = metadata
                .get(meta::IS_FORK)
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !is_fork {
                metadata.remove(meta::IS_FORK);
            }
        }

        let (mut primary, mut fallback) = (ids.primary, ids.fallback);
        if primary.is_empty() {
            return AffinityPick::Unbound;
        }
        primary = session::bound_session_identity(&primary);
        if !fallback.is_empty() {
            fallback = session::bound_session_identity(&fallback);
        }
        metadata.insert(meta::CANONICAL_SESSION_ID.into(), primary.clone().into());

        let model_key = canonical_model_key(model);
        let cache_key = format!("{provider}::{primary}::{model_key}");
        let is_fork = metadata
            .get(meta::IS_FORK)
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let is_subagent = !is_fork && session::is_subagent_session(&primary, &fallback);
        let fallback_key = (!fallback.is_empty() && fallback != primary)
            .then(|| format!("{provider}::{fallback}::{model_key}"));
        let binder = AffinityBinder {
            cache_key: cache_key.clone(),
            fallback_key: fallback_key.clone(),
            is_subagent,
            is_fork,
        };

        if let Some(cached) = self.cache.get_and_refresh(&cache_key) {
            if available_ids.contains(&cached.as_str()) {
                self.bind(&binder, &cached);
                return AffinityPick::Bound(cached);
            }
            // Bound credential unavailable: reselect via the fallback strategy.
            return AffinityPick::Fallback(binder);
        }
        if let Some(fkey) = &fallback_key
            && let Some(cached) = self.cache.get(fkey)
            && available_ids.contains(&cached.as_str())
            && (!is_subagent || self.subagent_affinity)
        {
            // A child/fork inherits its parent's credential.
            self.bind(&binder, &cached);
            return AffinityPick::Bound(cached);
        }
        AffinityPick::Fallback(binder)
    }

    pub fn bind(&self, binder: &AffinityBinder, auth_id: &str) {
        match &binder.fallback_key {
            Some(fkey) if !binder.is_subagent && !binder.is_fork => {
                self.cache
                    .set_aliases(auth_id, &[binder.cache_key.clone(), fkey.clone()]);
            }
            _ => self.cache.set(&binder.cache_key, auth_id),
        }
    }

    /// Records the execution outcome: success touches the binding, a credential-attributed
    /// failure drops it so the next pick can rebind (failover).
    pub fn on_result(&self, res: &super::cooldown::ExecResult) {
        if res.auth_id.is_empty() {
            return;
        }
        if res
            .error
            .as_ref()
            .is_some_and(|e| should_skip_credential_cooldown(Some(e)))
        {
            return;
        }
        let mut md = res.options.metadata.clone();
        let ids = affinity_ids(&res.options.headers, &res.options.original_request, &mut md);
        let (mut primary, mut fallback) = (ids.primary, ids.fallback);
        if primary.is_empty() && fallback.is_empty() {
            return;
        }
        let ns = res
            .options
            .metadata
            .get(meta::SESSION_AFFINITY_PROVIDER)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(&res.provider)
            .to_string();
        let ns_model = canonical_model_key(
            res.options
                .metadata
                .get(meta::SESSION_AFFINITY_MODEL)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&res.model),
        );
        if !primary.is_empty() {
            primary = session::bound_session_identity(&primary);
        }
        if !fallback.is_empty() {
            fallback = session::bound_session_identity(&fallback);
        }
        let cache_key = format!("{ns}::{primary}::{ns_model}");
        let fallback_key = (!fallback.is_empty()
            && fallback != primary
            && !session::is_subagent_session(&primary, &fallback))
        .then(|| format!("{ns}::{fallback}::{ns_model}"));
        if res.success {
            self.cache.touch(&cache_key, &res.auth_id);
            if let Some(f) = &fallback_key {
                self.cache.touch(f, &res.auth_id);
            }
            return;
        }
        self.cache.compare_and_delete(&cache_key, &res.auth_id);
        if let Some(f) = &fallback_key {
            self.cache.compare_and_delete(f, &res.auth_id);
        }
    }

    /// Side-effect free lookup of the binding for a session id (Go: LookupAffinity). Returns
    /// `(auth_id, status)` with status `bound`, `unbound` or `ambiguous`.
    pub fn lookup(
        &self,
        provider: &str,
        model: &str,
        session_id: &str,
        auth_filter: Option<&dyn Fn(&str) -> bool>,
    ) -> (String, &'static str) {
        let (provider, model, session_id) = (provider.trim(), model.trim(), session_id.trim());
        if provider.is_empty() || model.is_empty() || session_id.is_empty() {
            return (String::new(), "unbound");
        }
        let mut model_key = canonical_model_key(model);
        if model_key.is_empty() {
            model_key = model.to_string();
        }
        let mut providers = vec![provider];
        if provider != "mixed" {
            providers.push("mixed");
        }
        let known = session::CANDIDATE_SESSION_PREFIXES
            .iter()
            .any(|p| session_id.starts_with(p));
        let mut candidates = vec![session_id.to_string()];
        if !known {
            candidates.extend(
                session::CANDIDATE_SESSION_PREFIXES
                    .iter()
                    .map(|p| format!("{p}{session_id}")),
            );
        }
        let mut found: Vec<String> = Vec::new();
        for prov in providers {
            for cand in &candidates {
                let bounded = session::bound_session_identity(cand);
                let key = format!("{prov}::{bounded}::{model_key}");
                if let Some(auth_id) = self.cache.get(&key)
                    && !auth_id.is_empty()
                    && auth_filter.is_none_or(|f| f(&auth_id))
                    && !found.contains(&auth_id)
                {
                    found.push(auth_id);
                }
            }
        }
        match found.len() {
            0 => (String::new(), "unbound"),
            1 => (found.remove(0), "bound"),
            _ => (String::new(), "ambiguous"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::clock::ManualClock;
    use chrono::DateTime;

    fn clock() -> Arc<ManualClock> {
        Arc::new(ManualClock::new(
            DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        ))
    }

    fn cands<'a>(ids: &'a [&'a str], weights: &[i64]) -> Vec<Cand<'a>> {
        ids.iter()
            .zip(weights)
            .map(|(id, w)| Cand {
                id,
                provider: "p",
                weight: *w,
                smart: None,
            })
            .collect()
    }

    #[test]
    fn round_robin_is_identity_based_and_survives_shrinking_sets() {
        let sel = Selector::new(SelectorConfig::default(), clock());
        let ids = ["a", "b", "c"];
        let all = cands(&ids, &[1, 1, 1]);
        let picks: Vec<usize> = (0..4)
            .map(|_| sel.pick_ordered("k", &all).unwrap())
            .collect();
        assert_eq!(picks, [0, 1, 2, 0]);
        // After picking "a" the set shrinks to [a, c]: rotation continues at the successor of "a".
        let shrunk = cands(&["a", "c"], &[1, 1]);
        assert_eq!(sel.pick_ordered("k", &shrunk), Some(1));
        assert_eq!(sel.pick_ordered("k", &shrunk), Some(0));
    }

    #[test]
    fn fill_first_always_takes_lowest_id() {
        let sel = Selector::new(
            SelectorConfig {
                strategy: Strategy::FillFirst,
                ..Default::default()
            },
            clock(),
        );
        let all = cands(&["a", "b"], &[1, 1]);
        assert!((0..5).all(|_| sel.pick_ordered("k", &all) == Some(0)));
    }

    #[test]
    fn smooth_weighted_distribution_is_proportional_and_interleaved() {
        let sel = Selector::new(
            SelectorConfig {
                strategy: Strategy::WeightedRoundRobin,
                ..Default::default()
            },
            clock(),
        );
        let ids = ["a", "b"];
        let all = cands(&ids, &[5, 1]);
        let picks: Vec<&str> = (0..6)
            .map(|_| ids[sel.pick_ordered("k", &all).unwrap()])
            .collect();
        assert_eq!(picks.iter().filter(|p| **p == "a").count(), 5);
        assert_eq!(picks.iter().filter(|p| **p == "b").count(), 1);
        // nginx-style smooth sequence for weights 5:1 (ties go to the first credential).
        assert_eq!(picks, ["a", "a", "a", "b", "a", "a"]);
    }

    #[test]
    fn mixed_round_robin_rotates_providers_by_count() {
        let sel = Selector::new(SelectorConfig::default(), clock());
        let list = [
            Cand {
                id: "a1",
                provider: "pa",
                weight: 1,
                smart: None,
            },
            Cand {
                id: "a2",
                provider: "pa",
                weight: 1,
                smart: None,
            },
            Cand {
                id: "b1",
                provider: "pb",
                weight: 1,
                smart: None,
            },
        ];
        let providers = vec!["pa".to_string(), "pb".to_string()];
        let picks: Vec<&str> = (0..6)
            .map(|_| list[sel.pick_mixed(&providers, "m", 0, &list).unwrap()].id)
            .collect();
        assert_eq!(picks, ["a1", "a2", "b1", "a1", "a2", "b1"]);
        let ff = Selector::new(
            SelectorConfig {
                strategy: Strategy::FillFirst,
                ..Default::default()
            },
            clock(),
        );
        let reversed = vec!["pb".to_string(), "pa".to_string()];
        assert_eq!(
            list[ff.pick_mixed(&reversed, "m", 0, &list).unwrap()].id,
            "b1"
        );
    }

    #[test]
    fn affinity_binds_then_fails_over_and_rebinds() {
        let aff = SessionAffinity::new(Duration::from_secs(60), true, clock());
        let mut md = Metadata::new();
        let headers = http::HeaderMap::new();
        let body = br#"{"prompt_cache_key":"s1"}"#;
        let AffinityPick::Fallback(binder) =
            aff.decide("mixed", "m", &headers, body, &mut md, &["a", "b"])
        else {
            panic!("cold session must use the fallback strategy");
        };
        aff.bind(&binder, "b");
        assert_eq!(md[meta::CANONICAL_SESSION_ID], "pck:s1");
        // Same session sticks to b even when a would be next in rotation.
        let AffinityPick::Bound(id) =
            aff.decide("mixed", "m", &headers, body, &mut md, &["a", "b"])
        else {
            panic!("expected binding");
        };
        assert_eq!(id, "b");
        // b unavailable -> fallback; a failure result drops the binding.
        assert!(matches!(
            aff.decide("mixed", "m", &headers, body, &mut md, &["a"]),
            AffinityPick::Fallback(_)
        ));
        let mut opts = crate::executor::Options::new(cpa_translator::Format::OpenAI);
        opts.original_request = bytes::Bytes::from_static(body);
        opts.metadata = md.clone();
        let res = super::super::cooldown::ExecResult {
            auth_id: "b".into(),
            provider: "mixed".into(),
            model: "m".into(),
            route_model: "m".into(),
            success: false,
            retry_after: None,
            credential_scope: false,
            error: Some(cpa_auth::types::AuthError {
                http_status: 500,
                message: "x".into(),
                ..Default::default()
            }),
            options: opts,
            skip_quota_observation: true,
            response_headers: Default::default(),
        };
        aff.on_result(&res);
        assert!(aff.cache().get("mixed::pck:s1::m").is_none());
    }

    #[test]
    fn affinity_ttl_expires_bindings() {
        let c = clock();
        let aff = SessionAffinity::new(Duration::from_secs(10), true, c.clone());
        let mut md = Metadata::new();
        let body = br#"{"prompt_cache_key":"s2"}"#;
        let headers = http::HeaderMap::new();
        let AffinityPick::Fallback(binder) =
            aff.decide("mixed", "m", &headers, body, &mut md, &["a"])
        else {
            panic!()
        };
        aff.bind(&binder, "a");
        c.advance(Duration::from_secs(11));
        assert!(matches!(
            aff.decide("mixed", "m", &headers, body, &mut md, &["a"]),
            AffinityPick::Fallback(_)
        ));
    }
}
