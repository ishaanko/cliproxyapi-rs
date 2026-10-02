//! Candidate selection (Go: conductor_selection.go pickNextMixed/availability, selector.go).
//!
//! Order of operations for one pick:
//! 1. filter credentials by provider/executor, pinned id, eligibility, tried set and registry
//!    model support,
//! 2. drop credentials blocked for the (alias-resolved) model; if none remain, synthesize the
//!    `model_cooldown` / terminal-auth / `auth_unavailable` error,
//! 3. keep the highest priority tier (all tiers for session affinity, which lets an existing
//!    binding outrank priority),
//! 4. let the selector strategy (or the affinity binding) choose.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use cpa_auth::Auth;
use cpa_auth::credmeta::{parse_bool_any, parse_weight_string, parse_weight_value};
use cpa_auth::types::{AUTH_KIND_API_KEY, AUTH_KIND_OAUTH};
use http::HeaderMap;
use serde_json::Value;

use super::cooldown::{BlockReason, has_unauthorized_auth_failure, is_auth_blocked_for_model};
use super::errors::{
    AuthErrorExt, auth_not_found, auth_unavailable, model_cooldown_error, terminal_auth_error,
};
use super::models::{canonical_scheduling_provider, executor_key_from_auth};
use super::selector::{AffinityPick, Cand, Strategy};
use super::util::{canonical_model_key, parse_suffix};
use super::{Manager, executor_locked, meta_trimmed};
use crate::executor::{DynExecutor, ExecError, Metadata, meta};

const WEIGHT_DEFAULT: i64 = 1;

/// Request-level credential restrictions.
#[derive(Debug, Clone, Default)]
pub struct Eligibility {
    pub required_kind: String,
    pub credential_policy: String,
    pub disallow_free_auth: bool,
}

impl Eligibility {
    pub(crate) fn from_meta(meta: &Metadata) -> Self {
        Eligibility {
            disallow_free_auth: disallow_free_auth_from_metadata(meta),
            ..Default::default()
        }
    }

    pub(crate) fn allows(&self, auth: &Auth) -> bool {
        if !self.required_kind.is_empty() && auth.auth_kind() != self.required_kind {
            return false;
        }
        if !self.credential_policy.is_empty()
            && !credential_policy_allows(&self.credential_policy, auth)
        {
            return false;
        }
        !self.disallow_free_auth || !is_free_codex_auth(auth)
    }
}

pub(crate) fn disallow_free_auth_from_metadata(meta_map: &Metadata) -> bool {
    match meta_map.get(meta::DISALLOW_FREE_AUTH) {
        Some(Value::Bool(b)) => *b,
        Some(v @ Value::String(_)) => parse_bool_any(v).unwrap_or(false),
        _ => false,
    }
}

fn is_free_codex_auth(auth: &Auth) -> bool {
    auth.provider.trim().eq_ignore_ascii_case("codex")
        && auth.attr("plan_type").eq_ignore_ascii_case("free")
}

pub fn normalize_credential_policy(policy: &str) -> String {
    match policy.trim().to_lowercase().as_str() {
        super::CREDENTIAL_POLICY_CODEX_ALPHA_SEARCH_V1 => {
            super::CREDENTIAL_POLICY_CODEX_ALPHA_SEARCH_V1.into()
        }
        _ => String::new(),
    }
}

fn credential_policy_allows(policy: &str, auth: &Auth) -> bool {
    match policy {
        super::CREDENTIAL_POLICY_CODEX_ALPHA_SEARCH_V1 => {
            if !auth.provider.trim().eq_ignore_ascii_case("codex") {
                return false;
            }
            match auth.auth_kind() {
                AUTH_KIND_OAUTH => true,
                AUTH_KIND_API_KEY => auth.attr("codex_alpha_search").eq_ignore_ascii_case("true"),
                _ => false,
            }
        }
        _ => false,
    }
}

/// Integer `priority` attribute (larger wins); invalid or absent is 0.
pub fn auth_priority(auth: &Auth) -> i64 {
    auth.attr("priority").parse::<i64>().unwrap_or(0)
}

/// Selection weight: `weight` attribute, else metadata, else 1; invalid values count as 0.
pub fn auth_weight(auth: &Auth) -> i64 {
    if let Some(raw) = auth.attributes.get("weight")
        && !raw.trim().is_empty()
    {
        return parse_weight_string(raw).unwrap_or(0);
    }
    if let Some(v) = auth.metadata.get("weight") {
        return parse_weight_value(v).unwrap_or(0);
    }
    WEIGHT_DEFAULT
}

pub(crate) fn pinned_auth_id(meta_map: &Metadata) -> String {
    meta_trimmed(meta_map, meta::PINNED_AUTH_ID)
}

/// A chosen credential with the executor that serves it.
pub(crate) struct Picked {
    pub auth: Auth,
    pub executor: DynExecutor,
    /// Executor registry key of the credential.
    pub provider: String,
}

struct Timed<'a> {
    time: Option<DateTime<Utc>>,
    id: &'a str,
    text: String,
}

fn newer(cur: &Option<Timed<'_>>, time: Option<DateTime<Utc>>, id: &str) -> bool {
    match cur {
        None => true,
        Some(c) => time > c.time || (time == c.time && id > c.id),
    }
}

/// Most recent error text among candidates for the model, preferring model-level errors (Go:
/// latestCandidateErrorForModel).
fn latest_candidate_error_for_model(
    auths: &[&Auth],
    selection_model: &dyn Fn(&Auth) -> String,
) -> Option<String> {
    let mut model_best: Option<Timed<'_>> = None;
    let mut auth_best: Option<Timed<'_>> = None;
    for c in auths {
        let check_model = selection_model(c);
        let state = c
            .model_states
            .get(&check_model)
            .or_else(|| c.model_states.get(&canonical_model_key(&check_model)));
        if let Some(s) = state {
            let found = if let Some(e) = &s.last_error {
                Some(e.go_string())
            } else if !s.status_message.trim().is_empty() {
                Some(s.status_message.clone())
            } else {
                None
            };
            if let Some(text) = found {
                let time = s.updated_at.or(c.updated_at);
                if newer(&model_best, time, &c.id) {
                    model_best = Some(Timed {
                        time,
                        id: &c.id,
                        text,
                    });
                }
            }
        }
        let found = if let Some(e) = &c.last_error {
            Some(e.go_string())
        } else if !c.status_message.trim().is_empty() {
            Some(c.status_message.clone())
        } else {
            None
        };
        if let Some(text) = found
            && newer(&auth_best, c.updated_at, &c.id)
        {
            auth_best = Some(Timed {
                time: c.updated_at,
                id: &c.id,
                text,
            });
        }
    }
    model_best.or(auth_best).map(|t| t.text)
}

fn latest_unauthorized_candidate_error(auths: &[&Auth]) -> Option<String> {
    let mut best: Option<Timed<'_>> = None;
    for c in auths {
        if !has_unauthorized_auth_failure(c) {
            continue;
        }
        if let Some(e) = &c.last_error
            && newer(&best, c.updated_at, &c.id)
        {
            best = Some(Timed {
                time: c.updated_at,
                id: &c.id,
                text: e.go_string(),
            });
        }
    }
    best.map(|t| t.text)
}

fn to_cands<'a>(list: &[(&'a Auth, &'a str)]) -> Vec<Cand<'a>> {
    list.iter()
        .map(|(a, p)| Cand {
            id: a.id.as_str(),
            provider: p,
            weight: auth_weight(a),
        })
        .collect()
}

impl Manager {
    /// Registry model support (Go: authSupportsRouteModel): the client registered the route model
    /// or its alias-resolved selection key.
    pub(crate) fn auth_supports_route_model(&self, auth: &Auth, route_model: &str) -> bool {
        let route_key = canonical_model_key(route_model);
        if route_key.is_empty() {
            return true;
        }
        if self.registry.client_supports_model(&auth.id, &route_key) {
            return true;
        }
        let selection_key = self.selection_model_key_for_auth(auth, route_model);
        !selection_key.is_empty()
            && selection_key != route_key
            && self
                .registry
                .client_supports_model(&auth.id, &selection_key)
    }

    /// One credential for the request across `providers` (Go: pickNextMixed). `meta_map` receives
    /// session-affinity bookkeeping (canonical/parent session ids, affinity namespace).
    pub(crate) fn pick_next_mixed(
        &self,
        providers: &[String],
        route_model: &str,
        headers: &HeaderMap,
        original_request: &Bytes,
        meta_map: &mut Metadata,
        tried: &HashSet<String>,
        eligibility: &Eligibility,
    ) -> Result<Picked, ExecError> {
        let now = self.now();
        let selector = self.selector();
        let affinity = selector.affinity();
        let strategy = selector.config.strategy;
        meta_map.insert(
            meta::SESSION_AFFINITY_PROVIDER.into(),
            Value::String("mixed".into()),
        );
        meta_map.insert(
            meta::SESSION_AFFINITY_MODEL.into(),
            Value::String(route_model.into()),
        );
        let pinned = pinned_auth_id(meta_map);

        let st = self.state.read();
        let mut eligible: Vec<String> = Vec::new();
        for p in providers {
            let key = canonical_scheduling_provider(p);
            if key.is_empty() || eligible.contains(&key) || executor_locked(&st, &key).is_none() {
                continue;
            }
            eligible.push(key);
        }
        if eligible.is_empty() {
            return Err(auth_not_found("no auth available"));
        }

        let model_key = {
            let trimmed = route_model.trim();
            let base = parse_suffix(trimmed).model_name;
            if base.trim().is_empty() {
                trimmed.to_string()
            } else {
                base.trim().to_string()
            }
        };
        let mut cands: Vec<(&Auth, String)> = Vec::new();
        for a in st.auths.values() {
            if a.disabled {
                continue;
            }
            if !pinned.is_empty() && a.id != pinned {
                continue;
            }
            if !eligibility.allows(a) {
                continue;
            }
            let key = canonical_scheduling_provider(&executor_key_from_auth(a));
            if key.is_empty() || !eligible.contains(&key) {
                continue;
            }
            if tried.contains(&a.id) {
                continue;
            }
            if !model_key.is_empty() && !self.auth_supports_route_model(a, route_model) {
                continue;
            }
            if strategy == Strategy::WeightedRoundRobin && auth_weight(a) <= 0 {
                continue;
            }
            cands.push((a, key));
        }
        if cands.is_empty() {
            return Err(auth_not_found("no auth available"));
        }

        // Availability by priority tier.
        let mut by_priority: BTreeMap<i64, Vec<(&Auth, &str)>> = BTreeMap::new();
        let (mut cooldown_count, mut unauthorized_count) = (0usize, 0usize);
        let mut earliest: Option<DateTime<Utc>> = None;
        for (c, key) in &cands {
            let check_model = self.selection_model_for_auth(c, route_model);
            let b = is_auth_blocked_for_model(c, &check_model, now);
            if !b.blocked {
                by_priority
                    .entry(auth_priority(c))
                    .or_default()
                    .push((c, key.as_str()));
                continue;
            }
            if b.reason == BlockReason::Cooldown {
                cooldown_count += 1;
            }
            if b.reason != BlockReason::Disabled
                && let Some(next) = b.next
                && next > now
                && earliest.is_none_or(|e| next < e)
            {
                earliest = Some(next);
            }
            if has_unauthorized_auth_failure(c) {
                unauthorized_count += 1;
            }
        }
        if by_priority.is_empty() {
            let refs: Vec<&Auth> = cands.iter().map(|(a, _)| *a).collect();
            let sel = |a: &Auth| self.selection_model_for_auth(a, route_model);
            let last_err = latest_candidate_error_for_model(&refs, &sel);
            let provider_for_error = if affinity.is_none() && eligible.len() == 1 {
                eligible[0].as_str()
            } else {
                ""
            };
            if cooldown_count == cands.len()
                && let Some(next) = earliest
            {
                let reset_in = (next - now).to_std().unwrap_or(Duration::ZERO);
                return Err(model_cooldown_error(
                    route_model,
                    provider_for_error,
                    reset_in,
                    last_err.as_deref(),
                ));
            }
            if unauthorized_count == cands.len() {
                let cause = latest_unauthorized_candidate_error(&refs).or(last_err);
                return Err(terminal_auth_error(cause.as_deref()));
            }
            return Err(auth_unavailable(earliest, now, last_err.as_deref()));
        }

        let best_priority = *by_priority.keys().next_back().unwrap_or(&0);
        let mut top: Vec<(&Auth, &str)> =
            by_priority.get(&best_priority).cloned().unwrap_or_default();
        top.sort_by(|a, b| a.0.id.cmp(&b.0.id));

        let chosen: &Auth = if let Some(aff) = affinity {
            let mut all: Vec<(&Auth, &str)> = by_priority.values().flatten().copied().collect();
            all.sort_by(|a, b| a.0.id.cmp(&b.0.id));
            let all_ids: Vec<&str> = all.iter().map(|(a, _)| a.id.as_str()).collect();
            let key = format!("mixed:{}", canonical_model_key(route_model));
            let top_cands = to_cands(&top);
            match aff.decide(
                "mixed",
                route_model,
                headers,
                original_request,
                meta_map,
                &all_ids,
            ) {
                AffinityPick::Bound(id) => match all.iter().find(|(a, _)| a.id == id) {
                    Some((a, _)) => a,
                    None => return Err(auth_not_found("selector returned no auth")),
                },
                AffinityPick::Fallback(binder) => {
                    let Some(i) = selector.pick_ordered(&key, &top_cands) else {
                        return Err(auth_not_found("selector returned no auth"));
                    };
                    aff.bind(&binder, &top[i].0.id);
                    top[i].0
                }
                AffinityPick::Unbound => {
                    let Some(i) = selector.pick_ordered(&key, &top_cands) else {
                        return Err(auth_not_found("selector returned no auth"));
                    };
                    top[i].0
                }
            }
        } else {
            let top_cands = to_cands(&top);
            let canonical = canonical_model_key(route_model);
            let idx = if eligible.len() == 1 {
                let key = format!("{}:{canonical}:{best_priority}", eligible[0]);
                selector.pick_ordered(&key, &top_cands)
            } else {
                selector.pick_mixed(&eligible, &canonical, best_priority, &top_cands)
            };
            let Some(i) = idx else {
                return Err(auth_not_found("selector returned no auth"));
            };
            top[i].0
        };

        let provider = canonical_scheduling_provider(&executor_key_from_auth(chosen));
        let Some(executor) = executor_locked(&st, &provider) else {
            return Err(super::errors::executor_not_found());
        };
        Ok(Picked {
            auth: chosen.clone(),
            executor,
            provider: executor_key_from_auth(chosen),
        })
    }

    /// Selects one credential through the configured strategy without executing anything (Go:
    /// SelectAuth). `required_kind` / `policy` narrow the candidates when non-empty.
    pub fn select_auth(
        &self,
        provider: &str,
        model: &str,
        opts: &crate::executor::Options,
    ) -> Result<Auth, ExecError> {
        self.select_with(
            provider,
            model,
            opts,
            Eligibility::from_meta(&opts.metadata),
        )
    }

    pub fn select_auth_by_kind(
        &self,
        provider: &str,
        model: &str,
        required_kind: &str,
        opts: &crate::executor::Options,
    ) -> Result<Auth, ExecError> {
        let kind = match required_kind.trim().to_lowercase().as_str() {
            "apikey" | "api_key" | "api-key" => AUTH_KIND_API_KEY,
            "oauth" | "oauth2" => AUTH_KIND_OAUTH,
            _ => {
                let mut e = super::errors::auth_error(
                    "invalid_auth_kind",
                    "required auth kind is invalid",
                    400,
                );
                e.upstream_attempted = false;
                return Err(e);
            }
        };
        let mut elig = Eligibility::from_meta(&opts.metadata);
        elig.required_kind = kind.to_string();
        self.select_with(provider, model, opts, elig)
    }

    pub fn select_auth_with_credential_policy(
        &self,
        provider: &str,
        model: &str,
        policy: &str,
        opts: &crate::executor::Options,
    ) -> Result<Auth, ExecError> {
        let policy = normalize_credential_policy(policy);
        if policy.is_empty() {
            return Err(super::errors::auth_error(
                "invalid_credential_policy",
                "credential policy is invalid",
                400,
            ));
        }
        let mut elig = Eligibility::from_meta(&opts.metadata);
        elig.credential_policy = policy.clone();
        let picked = self.select_with(provider, model, opts, elig)?;
        if !credential_policy_allows(&policy, &picked) {
            return Err(auth_not_found("selector returned no eligible auth"));
        }
        Ok(picked)
    }

    fn select_with(
        &self,
        provider: &str,
        model: &str,
        opts: &crate::executor::Options,
        elig: Eligibility,
    ) -> Result<Auth, ExecError> {
        let mut md = opts.metadata.clone();
        if self.selector().affinity().is_some() {
            super::selector::resolve_affinity_ids(&opts.headers, &opts.original_request, &mut md);
        }
        let picked = self.pick_next_mixed(
            &[provider.to_string()],
            model,
            &opts.headers,
            &opts.original_request,
            &mut md,
            &HashSet::new(),
            &elig,
        )?;
        Ok(picked.auth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weight_and_priority_parsing() {
        let mut a = Auth::new("a", "p");
        assert_eq!((auth_priority(&a), auth_weight(&a)), (0, 1));
        a.attributes.insert("priority".into(), " 5 ".into());
        a.attributes.insert("weight".into(), "3".into());
        assert_eq!((auth_priority(&a), auth_weight(&a)), (5, 3));
        a.attributes.insert("weight".into(), "bad".into());
        assert_eq!(auth_weight(&a), 0);
        a.attributes.remove("weight");
        a.metadata.insert("weight".into(), serde_json::json!(7));
        assert_eq!(auth_weight(&a), 7);
        a.attributes.insert("priority".into(), "x".into());
        assert_eq!(auth_priority(&a), 0);
    }
}
