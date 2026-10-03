//! Manager-level model routing helpers: alias-aware selection/state model keys and the upstream
//! model candidates for one credential (Go: conductor_models.go methods on Manager).

use cpa_auth::Auth;
use cpa_config::Config;

use super::Manager;
use super::home_selection::{
    HOME_FORCE_MAPPING_ATTRIBUTE, HOME_ORIGINAL_ALIAS_ATTRIBUTE, HOME_UPSTREAM_MODEL_ATTRIBUTE,
};
use super::cooldown::{CoolingPolicy, is_auth_blocked_for_model};
use super::models::{
    AliasResult, apply_api_key_model_alias, apply_oauth_model_alias, execution_alias_pool_model,
    has_oauth_alias_channel,
    execution_result_model, is_configured_model_routing_auth, openai_compat_model_pool_key,
    resolve_api_key_model_alias_with_result, resolve_oauth_model_alias_with_result,
    resolve_openai_compat_upstream_model_pool, rewrite_model_for_auth, rotate_strings,
};
use super::util::{canonical_model_key, canonical_model_key_ref, rewrite_model_for_prefix_ref};

/// Go: `homeForceMappingAliasResult`: Home told us to rewrite responses back to the alias.
pub(super) fn home_force_mapping_alias_result(auth: &Auth, requested: &str) -> AliasResult {
    if !auth.attr(HOME_FORCE_MAPPING_ATTRIBUTE).eq_ignore_ascii_case("true") {
        return AliasResult::default();
    }
    let original_alias = auth.attr(HOME_ORIGINAL_ALIAS_ATTRIBUTE);
    let canonical_original = super::home_concurrency::canonical_concurrency_model_key(&original_alias);
    let canonical_requested = super::home_concurrency::canonical_concurrency_model_key(requested);
    if canonical_original.is_empty() || canonical_original != canonical_requested {
        return AliasResult::default();
    }
    let mut upstream = auth.attr(HOME_UPSTREAM_MODEL_ATTRIBUTE);
    if upstream.is_empty() {
        upstream = requested.trim().to_string();
    }
    AliasResult { upstream_model: upstream, force_mapping: true, original_alias }
}

impl Manager {
    /// Model name used for availability checks and state keys of this credential: prefix
    /// stripped, OAuth alias resolved (Go: selectionModelForAuth).
    pub(crate) fn selection_model_for_auth(&self, auth: &Auth, route_model: &str) -> String {
        if !has_oauth_alias_channel(auth) {
            return self.selection_model_ref(auth, route_model).to_string();
        }
        let mut requested = rewrite_model_for_auth(route_model, auth);
        if requested.trim().is_empty() {
            requested = route_model.trim().to_string();
        }
        let table = self.oauth_alias.read().clone();
        let resolved = apply_oauth_model_alias(&table, auth, &requested);
        if resolved.trim().is_empty() {
            requested
        } else {
            resolved
        }
    }

    /// [`Self::selection_model_for_auth`] for credentials without an OAuth alias channel (API
    /// keys, plain Gemini), where only the prefix is stripped; borrowed from `route_model`.
    pub(crate) fn selection_model_ref<'a>(&self, auth: &Auth, route_model: &'a str) -> &'a str {
        let requested = rewrite_model_for_prefix_ref(route_model, &auth.prefix);
        if requested.trim().is_empty() { route_model.trim() } else { requested }
    }

    pub(crate) fn selection_model_key_for_auth(&self, auth: &Auth, route_model: &str) -> String {
        if !has_oauth_alias_channel(auth) {
            return canonical_model_key_ref(self.selection_model_ref(auth, route_model)).to_string();
        }
        canonical_model_key(&self.selection_model_for_auth(auth, route_model))
    }

    /// Per-model state key for an attempt (Go: stateModelForExecution).
    pub(crate) fn state_model_for_execution(
        &self,
        auth: &Auth,
        route_model: &str,
        upstream_model: &str,
        pooled: bool,
    ) -> String {
        let home_model = auth.attr(HOME_UPSTREAM_MODEL_ATTRIBUTE);
        if !home_model.is_empty() {
            let resolved = upstream_model.trim();
            return if resolved.is_empty() { home_model } else { resolved.to_string() };
        }
        let state_model = execution_result_model(route_model, upstream_model, pooled);
        let selection = self.selection_model_for_auth(auth, route_model);
        if canonical_model_key(&selection) == canonical_model_key(upstream_model)
            && !selection.trim().is_empty()
        {
            return upstream_model.trim().to_string();
        }
        state_model
    }

    fn next_model_pool_offset(&self, key: &str, size: usize) -> usize {
        let key = key.trim();
        if size <= 1 || key.is_empty() {
            return 0;
        }
        let mut offsets = self.pool_offsets.lock();
        let slot = offsets.entry(key.to_string()).or_insert(0);
        if *slot >= 2_147_483_640 {
            *slot = 0;
        }
        let offset = *slot;
        *slot += 1;
        offset % size
    }

    pub(crate) fn alias_result_for_requested(
        &self,
        cfg: &Config,
        auth: &Auth,
        requested: &str,
    ) -> AliasResult {
        let home = home_force_mapping_alias_result(auth, requested);
        if home.force_mapping {
            return home;
        }
        if is_configured_model_routing_auth(auth) {
            return resolve_api_key_model_alias_with_result(cfg, auth, requested);
        }
        let table = self.oauth_alias.read().clone();
        let r = resolve_oauth_model_alias_with_result(&table, auth, requested);
        if r.upstream_model.is_empty() {
            AliasResult {
                upstream_model: requested.to_string(),
                ..Default::default()
            }
        } else {
            r
        }
    }

    /// Upstream model candidates for the credential: normally one; an OpenAI-compat alias pool
    /// yields several, rotated per credential (Go: executionModelCandidatesWithAlias).
    pub(crate) fn execution_model_candidates_with_alias(
        &self,
        auth: &Auth,
        route_model: &str,
    ) -> (Vec<String>, bool, AliasResult) {
        let cfg = self.cfg();
        let requested = rewrite_model_for_auth(route_model, auth);
        let mut alias = self.alias_result_for_requested(&cfg, auth, &requested);
        if alias.force_mapping && auth.attr(HOME_FORCE_MAPPING_ATTRIBUTE).eq_ignore_ascii_case("true") {
            alias.original_alias = route_model.trim().to_string();
        }
        let upstream_model = execution_alias_pool_model(auth, &requested, &alias);
        let home_model = auth.attr(HOME_UPSTREAM_MODEL_ATTRIBUTE);
        let pool = if home_model.is_empty() {
            resolve_openai_compat_upstream_model_pool(&cfg, auth, &upstream_model)
        } else {
            Vec::new()
        };
        let candidates = if !home_model.is_empty() {
            vec![home_model]
        } else if pool.len() == 1 {
            pool
        } else if pool.len() > 1 {
            let offset = self.next_model_pool_offset(
                &openai_compat_model_pool_key(auth, &upstream_model),
                pool.len(),
            );
            rotate_strings(&pool, offset)
        } else {
            let resolved = apply_api_key_model_alias(&cfg, auth, &upstream_model);
            vec![if resolved.trim().is_empty() {
                upstream_model
            } else {
                resolved
            }]
        };
        let pooled = candidates.len() > 1;
        (candidates, pooled, alias)
    }

    /// Candidates minus models currently blocked for this credential (Go:
    /// preparedExecutionModelsWithAlias).
    pub(crate) fn prepared_execution_models_with_alias(
        &self,
        auth: &Auth,
        route_model: &str,
    ) -> (Vec<String>, bool, AliasResult) {
        let (candidates, pooled, alias) =
            self.execution_model_candidates_with_alias(auth, route_model);
        let now = self.now();
        let models = candidates
            .into_iter()
            .filter(|m| {
                let state_model = self.state_model_for_execution(auth, route_model, m, pooled);
                !is_auth_blocked_for_model(auth, &state_model, now).blocked
            })
            .collect();
        (models, pooled, alias)
    }

    /// Whether cooling is disabled for the credential (Go: quotaCooldownDisabledForAuthWithConfig):
    /// per-auth override, compat provider setting, config, then the global switch.
    pub(crate) fn cooldown_disabled_for_auth_cfg(&self, auth: &Auth, cfg: &Config) -> bool {
        // Home owns cooldown state, so downstream instances must not schedule local cooldowns.
        if cfg.home.enabled {
            return true;
        }
        if let Some(b) = auth.disable_cooling_override() {
            return b;
        }
        if let Some(b) = provider_cooling_override(auth, cfg) {
            return b;
        }
        if cfg.disable_cooling {
            return true;
        }
        self.cooldown_disabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn cooldown_disabled_for_auth(&self, auth: &Auth) -> bool {
        self.cooldown_disabled_for_auth_cfg(auth, &self.cfg())
    }

    pub(crate) fn cooling_policy_for(&self, auth: &Auth) -> CoolingPolicy {
        let cfg = self.cfg();
        CoolingPolicy {
            disable_cooling: self.cooldown_disabled_for_auth_cfg(auth, &cfg),
            transient_seconds: cfg.transient_error_cooldown_seconds,
        }
    }
}

fn provider_cooling_override(auth: &Auth, cfg: &Config) -> Option<bool> {
    let provider = auth.provider.trim().to_lowercase();
    if provider.is_empty() {
        return None;
    }
    let provider_key = auth.attr("provider_key");
    let compat_name = auth.attr("compat_name");
    if provider_key.is_empty() && compat_name.is_empty() && provider != "openai-compatibility" {
        return None;
    }
    let provider_key = if provider_key.is_empty() {
        provider.clone()
    } else {
        provider_key
    };
    super::models::resolve_openai_compat_config(cfg, &provider_key, &compat_name, &provider)?
        .disable_cooling
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_config::OpenAiCompatibility;

    #[test]
    fn compat_provider_cooling_override_applies_only_to_compat_auths() {
        let mut cfg = Config::default();
        cfg.openai_compatibility.push(OpenAiCompatibility {
            name: "p".into(),
            disable_cooling: Some(true),
            ..Default::default()
        });
        let m = Manager::new();
        let mut compat = Auth::new("c", "openai-compatibility");
        compat.attributes.insert("compat_name".into(), "p".into());
        assert!(m.cooldown_disabled_for_auth_cfg(&compat, &cfg));
        let plain = Auth::new("x", "claude");
        assert!(!m.cooldown_disabled_for_auth_cfg(&plain, &cfg));
        let mut over = compat.clone();
        over.metadata
            .insert("disable_cooling".into(), serde_json::json!(false));
        assert!(!m.cooldown_disabled_for_auth_cfg(&over, &cfg));
    }
}
