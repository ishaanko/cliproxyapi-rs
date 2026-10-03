//! Model name handling for the conductor (Go: conductor_models.go, oauth_model_alias.go,
//! api_key_model_capabilities.go, config_apikey.go).
//!
//! Covers prefix stripping, OAuth model aliases (per-auth and global), API-key/compat `models[]`
//! aliases and alias pools, force-mapping results, and the capability snapshot (`ModelInfo`)
//! attached to a request for the selected credential. Config-derived tables are computed on
//! demand from the shared `Config`; only the OAuth alias table is compiled up front.

use std::collections::{BTreeMap, HashMap};

use cpa_auth::types::{
    ATTRIBUTE_API_KEY, AUTH_KIND_API_KEY, AUTH_KIND_OAUTH, AUTH_SOURCE_CONFIG, Auth,
};
use cpa_config::{
    ClaudeKey, CodexKey, Config, GeminiKey, OAuthModelAlias, OpenAiCompatibility,
    ThinkingSupport as CfgThinking, VertexCompatKey,
};
use cpa_core::registry::{ModelInfo, ThinkingSupport};
use serde_json::Value;

use crate::executor::Request;

use super::util::{eq_fold, parse_suffix, rewrite_model_for_prefix};

pub const OAUTH_MODEL_ALIASES_ATTRIBUTE_KEY: &str = "model_aliases";
pub const RESOLVED_API_KEY_MODEL_INFO: &str = "cliproxy.resolved_api_key_model_info";
pub const RESOLVED_CODEX_OAUTH_MODEL_INFO: &str = "cliproxy.resolved_codex_oauth_model_info";
pub const RESOLVED_HOME_MODEL_INFO: &str = "cliproxy.resolved_home_model_info";

/// Resolved upstream model plus force-mapping metadata (Go: OAuthModelAliasResult).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AliasResult {
    /// Upstream model name (empty when no mapping was found).
    pub upstream_model: String,
    /// Rewrite the model field of responses back to `original_alias`.
    pub force_mapping: bool,
    pub original_alias: String,
}

/// One configured `models[]` entry, unified across the per-provider config types.
#[derive(Debug, Clone, Default)]
pub struct ConfiguredModel {
    pub name: String,
    pub alias: String,
    pub force_mapping: bool,
    pub thinking: Option<CfgThinking>,
    pub is_compat: bool,
    pub support_configuration_update: bool,
    pub image: bool,
}

// ---- Auth helpers ----

pub fn is_configured_model_routing_auth(auth: &Auth) -> bool {
    if auth.auth_kind() == AUTH_KIND_API_KEY {
        return true;
    }
    auth.auth_source_kind() == AUTH_SOURCE_CONFIG && !auth.attr("compat_name").is_empty()
}

pub fn is_configured_openai_compat_auth(auth: &Auth) -> bool {
    if !is_configured_model_routing_auth(auth) {
        return false;
    }
    auth.provider
        .trim()
        .eq_ignore_ascii_case("openai-compatibility")
        || !auth.attr("compat_name").is_empty()
}

pub fn rewrite_model_for_auth(model: &str, auth: &Auth) -> String {
    rewrite_model_for_prefix(model, &auth.prefix)
}

/// Executor registry key for an auth (Go: executorKeyFromAuth).
pub fn executor_key_from_auth(auth: &Auth) -> String {
    let provider_key = auth.attr("provider_key");
    let compat_name = auth.attr("compat_name");
    if !compat_name.is_empty() {
        let key = if provider_key.is_empty() {
            compat_name
        } else {
            provider_key
        };
        return cpa_core::util::openai_compatible_provider_key(&key);
    }
    if auth
        .provider
        .trim()
        .eq_ignore_ascii_case("openai-compatibility")
    {
        let key = auth.label.trim();
        let key = if key.is_empty() {
            "openai-compatibility"
        } else {
            key
        };
        return cpa_core::util::openai_compatible_provider_key(key);
    }
    canonical_scheduling_provider(&auth.provider)
}

/// Index into `eligible` (canonical lowercase executor keys) of the executor serving `auth`:
/// `canonical_scheduling_provider(executor_key_from_auth(auth))` found in the list, without
/// allocating for the common plain-provider credential.
pub(crate) fn eligible_executor_index(auth: &Auth, eligible: &[String]) -> Option<usize> {
    let provider = auth.provider.trim();
    let plain = auth.attr_ref("compat_name").is_empty()
        && provider.is_ascii()
        && !provider.eq_ignore_ascii_case("openai-compatibility")
        && !provider.eq_ignore_ascii_case("kimi.com")
        && !provider.eq_ignore_ascii_case("kimi.ai");
    if plain {
        return eligible.iter().position(|e| e.eq_ignore_ascii_case(provider));
    }
    let key = canonical_scheduling_provider(&executor_key_from_auth(auth));
    eligible.iter().position(|e| *e == key)
}

/// Lowercased provider with the kimi domain spellings folded (Go: canonicalSchedulingProvider).
pub fn canonical_scheduling_provider(key: &str) -> String {
    match key.trim().to_lowercase().as_str() {
        "kimi.com" => "kimi".into(),
        "kimi.ai" => "kimi-ai".into(),
        other => other.to_string(),
    }
}

// ---- Suffix-preserving alias helpers ----

/// Candidates tried for alias lookups: the requested name, then its base without suffix.
pub(crate) fn alias_lookup_candidates(requested: &str) -> (super::util::SuffixResult, Vec<String>) {
    let requested = requested.trim();
    if requested.is_empty() {
        return (Default::default(), Vec::new());
    }
    let result = parse_suffix(requested);
    let base = if result.model_name.is_empty() {
        requested.to_string()
    } else {
        result.model_name.clone()
    };
    let mut candidates = vec![requested.to_string()];
    if base != requested {
        candidates.push(base);
    }
    (result, candidates)
}

fn preserve_resolved_model_suffix(resolved: &str, request: &super::util::SuffixResult) -> String {
    let resolved = resolved.trim();
    if resolved.is_empty() {
        return String::new();
    }
    if parse_suffix(resolved).has_suffix {
        return resolved.to_string();
    }
    if request.has_suffix && !request.raw_suffix.is_empty() {
        return format!("{resolved}({})", request.raw_suffix);
    }
    resolved.to_string()
}

fn preserve_requested_model_suffix(requested: &str, resolved: &str) -> String {
    preserve_resolved_model_suffix(resolved, &parse_suffix(requested))
}

/// Alias pool for an `models[]` list: all distinct upstream names whose alias matches, else the
/// entries whose name matches (Go: resolveModelAliasPoolFromConfigModels).
pub fn resolve_model_alias_pool(requested: &str, models: &[ConfiguredModel]) -> Vec<String> {
    let requested = requested.trim();
    if requested.is_empty() || models.is_empty() {
        return Vec::new();
    }
    let (request, candidates) = alias_lookup_candidates(requested);
    for candidate in &candidates {
        let mut out: Vec<String> = Vec::new();
        for m in models {
            let name = m.name.trim();
            let alias = m.alias.trim();
            if candidate.is_empty() || alias.is_empty() || !eq_fold(alias, candidate) {
                continue;
            }
            let resolved = if name.is_empty() {
                candidate.as_str()
            } else {
                name
            };
            let resolved = preserve_resolved_model_suffix(resolved, &request);
            let key = resolved.trim().to_lowercase();
            if key.is_empty() || out.iter().any(|o| o.trim().to_lowercase() == key) {
                continue;
            }
            out.push(resolved);
        }
        if !out.is_empty() {
            return out;
        }
    }
    for candidate in &candidates {
        for m in models {
            let name = m.name.trim();
            if candidate.is_empty() || name.is_empty() || !eq_fold(name, candidate) {
                continue;
            }
            return vec![preserve_resolved_model_suffix(name, &request)];
        }
    }
    Vec::new()
}

pub fn resolve_model_alias_from_config_models(
    requested: &str,
    models: &[ConfiguredModel],
) -> String {
    resolve_model_alias_pool(requested, models)
        .into_iter()
        .next()
        .unwrap_or_default()
}

pub fn resolve_model_alias_result_from_config_models(
    requested: &str,
    models: &[ConfiguredModel],
) -> AliasResult {
    let requested = requested.trim();
    if requested.is_empty() || models.is_empty() {
        return AliasResult::default();
    }
    let (request, candidates) = alias_lookup_candidates(requested);
    if candidates.is_empty() {
        return AliasResult::default();
    }
    let base_model = if request.model_name.is_empty() {
        requested.to_string()
    } else {
        request.model_name.clone()
    };
    for candidate in &candidates {
        let key = candidate.trim();
        if key.is_empty() {
            continue;
        }
        for m in models {
            let original = m.name.trim();
            let alias = m.alias.trim();
            if original.is_empty() || alias.is_empty() || !eq_fold(alias, key) {
                continue;
            }
            if eq_fold(original, &base_model) {
                if !m.force_mapping {
                    return AliasResult::default();
                }
                return AliasResult {
                    upstream_model: preserve_resolved_model_suffix(original, &request),
                    force_mapping: true,
                    original_alias: alias.to_string(),
                };
            }
            let original_alias = if m.force_mapping {
                alias.to_string()
            } else {
                requested.to_string()
            };
            return AliasResult {
                upstream_model: preserve_resolved_model_suffix(original, &request),
                force_mapping: m.force_mapping,
                original_alias,
            };
        }
    }
    AliasResult::default()
}

// ---- OAuth model aliases ----

#[derive(Debug, Clone)]
struct OAuthAliasEntry {
    upstream_model: String,
    config_alias: String,
    force_mapping: bool,
}

/// Compiled global OAuth alias table: channel -> alias(lower) -> entry.
#[derive(Debug, Default)]
pub struct OAuthAliasTable {
    reverse: HashMap<String, HashMap<String, OAuthAliasEntry>>,
}

pub fn compile_oauth_model_alias_table(
    aliases: &BTreeMap<String, Vec<OAuthModelAlias>>,
) -> OAuthAliasTable {
    let mut table = OAuthAliasTable::default();
    for (raw_channel, entries) in aliases {
        let channel = raw_channel.trim().to_lowercase();
        if channel.is_empty() || entries.is_empty() {
            continue;
        }
        let mut rev: HashMap<String, OAuthAliasEntry> = HashMap::new();
        for entry in entries {
            let name = entry.name.trim();
            let alias = entry.alias.trim();
            if name.is_empty() || alias.is_empty() || eq_fold(name, alias) {
                continue;
            }
            rev.entry(alias.to_lowercase())
                .or_insert_with(|| OAuthAliasEntry {
                    upstream_model: name.to_string(),
                    config_alias: alias.to_string(),
                    force_mapping: entry.force_mapping,
                });
        }
        if !rev.is_empty() {
            table.reverse.insert(channel, rev);
        }
    }
    table
}

/// OAuth model alias channel for a provider/auth kind; empty for API keys and plain Gemini.
pub fn oauth_model_alias_channel(provider: &str, auth_kind: &str) -> String {
    let provider = provider.trim().to_lowercase();
    let kind = auth_kind.trim().to_lowercase();
    if matches!(kind.as_str(), "apikey" | "api_key" | "api-key") {
        return String::new();
    }
    if provider == "gemini" {
        return String::new();
    }
    provider
}

/// Whether [`oauth_model_alias_channel`] is non-empty for this auth, without building it.
pub(crate) fn has_oauth_alias_channel(auth: &Auth) -> bool {
    let provider = auth.provider.trim();
    !provider.is_empty()
        && auth.auth_kind() != AUTH_KIND_API_KEY
        && !provider.eq_ignore_ascii_case("gemini")
}

fn model_alias_channel(auth: &Auth) -> String {
    oauth_model_alias_channel(&auth.provider, auth.auth_kind())
}

/// Per-auth OAuth aliases from the `model_aliases` attribute (JSON), sanitized.
pub fn oauth_model_aliases_from_attributes(auth: &Auth) -> Vec<OAuthModelAlias> {
    let raw = auth.attr(OAUTH_MODEL_ALIASES_ATTRIBUTE_KEY);
    if raw.is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Vec<OAuthModelAlias>>(&raw) {
        Ok(aliases) => sanitize_oauth_model_aliases(aliases),
        Err(_) => Vec::new(),
    }
}

/// Stores sanitized per-auth OAuth aliases on an auth (Go: SetOAuthModelAliasesAttribute).
pub fn set_oauth_model_aliases_attribute(auth: &mut Auth, aliases: Vec<OAuthModelAlias>) {
    let aliases = sanitize_oauth_model_aliases(aliases);
    if aliases.is_empty() {
        return;
    }
    if let Ok(data) = serde_json::to_string(&aliases) {
        auth.attributes
            .insert(OAUTH_MODEL_ALIASES_ATTRIBUTE_KEY.into(), data);
    }
}

fn sanitize_oauth_model_aliases(aliases: Vec<OAuthModelAlias>) -> Vec<OAuthModelAlias> {
    if aliases.is_empty() {
        return Vec::new();
    }
    let mut cfg = Config::default();
    cfg.oauth_model_alias.insert("auth".into(), aliases);
    cfg.sanitize_oauth_model_alias();
    cfg.oauth_model_alias.remove("auth").unwrap_or_default()
}

fn resolve_upstream_model_from_aliases(
    aliases: &[OAuthModelAlias],
    requested: &str,
) -> AliasResult {
    if aliases.is_empty() {
        return AliasResult::default();
    }
    let (request, candidates) = alias_lookup_candidates(requested);
    if candidates.is_empty() {
        return AliasResult::default();
    }
    let base_model = if request.model_name.is_empty() {
        requested.trim().to_string()
    } else {
        request.model_name.clone()
    };
    for candidate in &candidates {
        let key = candidate.trim();
        if key.is_empty() {
            continue;
        }
        for entry in aliases {
            let original = entry.name.trim();
            let alias = entry.alias.trim();
            if original.is_empty() || alias.is_empty() || !eq_fold(alias, key) {
                continue;
            }
            if eq_fold(original, &base_model) {
                if !entry.force_mapping {
                    return AliasResult::default();
                }
                return AliasResult {
                    upstream_model: preserve_resolved_model_suffix(original, &request),
                    force_mapping: true,
                    original_alias: alias.to_string(),
                };
            }
            let original_alias = if entry.force_mapping {
                alias.to_string()
            } else {
                requested.to_string()
            };
            return AliasResult {
                upstream_model: preserve_resolved_model_suffix(original, &request),
                force_mapping: entry.force_mapping,
                original_alias,
            };
        }
    }
    AliasResult::default()
}

fn resolve_upstream_model_from_alias_table(
    table: &OAuthAliasTable,
    requested: &str,
    channel: &str,
) -> AliasResult {
    if channel.is_empty() {
        return AliasResult::default();
    }
    let (request, candidates) = alias_lookup_candidates(requested);
    let Some(rev) = table.reverse.get(channel) else {
        return AliasResult::default();
    };
    let base_model = &request.model_name;
    for candidate in &candidates {
        let key = candidate.trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        let Some(entry) = rev.get(&key) else { continue };
        let target = &entry.upstream_model;
        if target.is_empty() {
            continue;
        }
        if eq_fold(target, base_model) {
            if !entry.force_mapping {
                return AliasResult::default();
            }
            return AliasResult {
                upstream_model: preserve_resolved_model_suffix(target, &request),
                force_mapping: true,
                original_alias: entry.config_alias.trim().to_string(),
            };
        }
        let upstream = if parse_suffix(target).has_suffix {
            target.clone()
        } else if request.has_suffix && !request.raw_suffix.is_empty() {
            format!("{target}({})", request.raw_suffix)
        } else {
            target.clone()
        };
        let original_alias = if entry.force_mapping {
            entry.config_alias.trim().to_string()
        } else {
            requested.to_string()
        };
        return AliasResult {
            upstream_model: upstream,
            force_mapping: entry.force_mapping,
            original_alias,
        };
    }
    AliasResult::default()
}

/// Per-auth aliases first, then the global table (Go: resolveOAuthModelAliasWithResult).
pub fn resolve_oauth_model_alias_with_result(
    table: &OAuthAliasTable,
    auth: &Auth,
    requested: &str,
) -> AliasResult {
    let channel = model_alias_channel(auth);
    if channel.is_empty() {
        return AliasResult::default();
    }
    let per_auth =
        resolve_upstream_model_from_aliases(&oauth_model_aliases_from_attributes(auth), requested);
    if !per_auth.upstream_model.is_empty() {
        return per_auth;
    }
    resolve_upstream_model_from_alias_table(table, requested, &channel)
}

/// Upstream name for an OAuth auth, the request unchanged when no alias applies.
pub fn apply_oauth_model_alias(table: &OAuthAliasTable, auth: &Auth, requested: &str) -> String {
    let r = resolve_oauth_model_alias_with_result(table, auth, requested);
    if r.upstream_model.is_empty() {
        requested.to_string()
    } else {
        r.upstream_model
    }
}

// ---- API-key config lookup ----

/// Credentials/routing identity of a config key entry.
pub trait KeyEntry {
    fn api_key(&self) -> &str;
    fn base_url(&self) -> &str;
    fn prefix(&self) -> &str;
    fn proxy_url(&self) -> &str;
}

macro_rules! impl_key_entry {
    ($($t:ty),*) => {$(
        impl KeyEntry for $t {
            fn api_key(&self) -> &str { &self.api_key }
            fn base_url(&self) -> &str { &self.base_url }
            fn prefix(&self) -> &str { &self.prefix }
            fn proxy_url(&self) -> &str { &self.proxy_url }
        }
    )*};
}
impl_key_entry!(ClaudeKey, CodexKey, GeminiKey, VertexCompatKey);

/// Locates the config entry backing an API-key auth: by `config_index` when it still matches,
/// else key+base+prefix+proxy, else key+base, else any entry with the same key.
pub fn resolve_api_key_config<'a, T: KeyEntry>(entries: &'a [T], auth: &Auth) -> Option<&'a T> {
    if entries.is_empty() {
        return None;
    }
    let attr_key = auth.attr(ATTRIBUTE_API_KEY);
    let attr_base = auth.attr("base_url");
    let matches_credentials = |e: &T| -> bool {
        let cfg_key = e.api_key().trim();
        let cfg_base = e.base_url().trim();
        if !attr_key.is_empty() && !attr_base.is_empty() {
            return eq_fold(cfg_key, &attr_key) && eq_fold(cfg_base, &attr_base);
        }
        if !attr_key.is_empty() {
            return eq_fold(cfg_key, &attr_key)
                && (cfg_base.is_empty() || eq_fold(cfg_base, &attr_base));
        }
        !attr_base.is_empty() && eq_fold(cfg_base, &attr_base)
    };
    if auth.auth_source_kind() == AUTH_SOURCE_CONFIG
        && let Ok(index) = auth.attr("config_index").parse::<usize>()
        && let Some(e) = entries.get(index)
        && matches_credentials(e)
    {
        return Some(e);
    }
    for e in entries {
        if matches_credentials(e)
            && eq_fold(e.prefix().trim(), auth.prefix.trim())
            && eq_fold(e.proxy_url().trim(), auth.proxy_url.trim())
        {
            return Some(e);
        }
    }
    if let Some(e) = entries.iter().find(|e| matches_credentials(e)) {
        return Some(e);
    }
    if !attr_key.is_empty() {
        return entries
            .iter()
            .find(|e| eq_fold(e.api_key().trim(), &attr_key));
    }
    None
}

pub fn resolve_openai_compat_config<'a>(
    cfg: &'a Config,
    provider_key: &str,
    compat_name: &str,
    auth_provider: &str,
) -> Option<&'a OpenAiCompatibility> {
    let candidates: Vec<&str> = [compat_name, provider_key, auth_provider]
        .into_iter()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .collect();
    cfg.openai_compatibility
        .iter()
        .filter(|c| !c.disabled)
        .find(|c| candidates.iter().any(|cand| eq_fold(cand, &c.name)))
}

pub fn resolve_openai_compat_config_for_auth<'a>(
    cfg: &'a Config,
    auth: &Auth,
    provider_key: &str,
    compat_name: &str,
) -> Option<&'a OpenAiCompatibility> {
    if auth.auth_source_kind() == AUTH_SOURCE_CONFIG
        && let Ok(index) = auth.attr("config_index").parse::<usize>()
        && let Some(entry) = cfg.openai_compatibility.get(index)
        && !entry.disabled
    {
        return Some(entry);
    }
    resolve_openai_compat_config(cfg, provider_key, compat_name, &auth.provider)
}

fn compat_entry<'a>(cfg: &'a Config, auth: &Auth) -> Option<&'a OpenAiCompatibility> {
    let provider_key = auth.attr("provider_key");
    let compat_name = auth.attr("compat_name");
    resolve_openai_compat_config_for_auth(cfg, auth, &provider_key, &compat_name)
}

macro_rules! models_of {
    ($models:expr, |$m:ident| $conv:expr) => {
        $models
            .iter()
            .map(|$m| $conv)
            .collect::<Vec<ConfiguredModel>>()
    };
}

/// `models[]` of the config entry backing `auth` (Go: configuredModelAliasEntries).
pub fn configured_models(cfg: &Config, auth: &Auth) -> Vec<ConfiguredModel> {
    configured_models_for(cfg, auth, true)
}

/// `require_compat`: alias lookups only consult a compat entry for compat auths; the capability
/// lookup (Go: compileAPIKeyModelCapabilitiesForAuth) tries it for any unknown provider.
fn configured_models_for(cfg: &Config, auth: &Auth, require_compat: bool) -> Vec<ConfiguredModel> {
    match auth.provider.trim().to_lowercase().as_str() {
        "gemini" => resolve_api_key_config(&cfg.gemini_key, auth)
            .map(|e| {
                models_of!(e.models, |m| ConfiguredModel {
                    name: m.name.clone(),
                    alias: m.alias.clone(),
                    force_mapping: m.force_mapping,
                    thinking: m.thinking.clone(),
                    is_compat: m.is_compat,
                    ..Default::default()
                })
            })
            .unwrap_or_default(),
        "gemini-interactions" => resolve_api_key_config(&cfg.interactions_key, auth)
            .map(|e| {
                models_of!(e.models, |m| ConfiguredModel {
                    name: m.name.clone(),
                    alias: m.alias.clone(),
                    force_mapping: m.force_mapping,
                    thinking: m.thinking.clone(),
                    is_compat: m.is_compat,
                    ..Default::default()
                })
            })
            .unwrap_or_default(),
        "claude" => resolve_api_key_config(&cfg.claude_key, auth)
            .map(|e| {
                models_of!(e.models, |m| ConfiguredModel {
                    name: m.name.clone(),
                    alias: m.alias.clone(),
                    force_mapping: m.force_mapping,
                    thinking: m.thinking.clone(),
                    is_compat: m.is_compat,
                    ..Default::default()
                })
            })
            .unwrap_or_default(),
        "codex" => codex_like_models(resolve_api_key_config(&cfg.codex_key, auth)),
        "xai" => codex_like_models(resolve_api_key_config(&cfg.xai_key, auth)),
        "meta" => codex_like_models(resolve_api_key_config(&cfg.meta_key, auth)),
        "vertex" => resolve_api_key_config(&cfg.vertex_compat_api_key, auth)
            .map(|e| {
                models_of!(e.models, |m| ConfiguredModel {
                    name: m.name.clone(),
                    alias: m.alias.clone(),
                    force_mapping: m.force_mapping,
                    thinking: m.thinking.clone(),
                    ..Default::default()
                })
            })
            .unwrap_or_default(),
        _ => {
            let compat_name = auth.attr("compat_name");
            if require_compat
                && compat_name.is_empty()
                && !auth
                    .provider
                    .trim()
                    .eq_ignore_ascii_case("openai-compatibility")
            {
                return Vec::new();
            }
            compat_entry(cfg, auth)
                .map(|e| compat_models(e))
                .unwrap_or_default()
        }
    }
}

fn codex_like_models(entry: Option<&CodexKey>) -> Vec<ConfiguredModel> {
    entry
        .map(|e| {
            models_of!(e.models, |m| ConfiguredModel {
                name: m.name.clone(),
                alias: m.alias.clone(),
                force_mapping: m.force_mapping,
                thinking: m.thinking.clone(),
                is_compat: m.is_compat,
                support_configuration_update: m.support_configuration_update,
                ..Default::default()
            })
        })
        .unwrap_or_default()
}

fn compat_models(entry: &OpenAiCompatibility) -> Vec<ConfiguredModel> {
    models_of!(entry.models, |m| ConfiguredModel {
        name: m.name.clone(),
        alias: m.alias.clone(),
        force_mapping: m.force_mapping,
        thinking: m.thinking.clone(),
        is_compat: m.is_compat,
        image: m.image,
        ..Default::default()
    })
}

/// Pool of upstream models for an OpenAI-compatible alias (Go: resolveOpenAICompatUpstreamModelPool).
pub fn resolve_openai_compat_upstream_model_pool(
    cfg: &Config,
    auth: &Auth,
    requested: &str,
) -> Vec<String> {
    if !is_configured_openai_compat_auth(auth) {
        return Vec::new();
    }
    let requested = requested.trim();
    if requested.is_empty() {
        return Vec::new();
    }
    match compat_entry(cfg, auth) {
        Some(entry) => resolve_model_alias_pool(requested, &compat_models(entry)),
        None => Vec::new(),
    }
}

/// Alias result for API-key / compat auths (Go: resolveAPIKeyModelAliasWithResult).
pub fn resolve_api_key_model_alias_with_result(
    cfg: &Config,
    auth: &Auth,
    requested: &str,
) -> AliasResult {
    let requested = requested.trim();
    if requested.is_empty() {
        return AliasResult::default();
    }
    let models = configured_models(cfg, auth);
    if models.is_empty() {
        return AliasResult {
            upstream_model: requested.to_string(),
            ..Default::default()
        };
    }
    let result = resolve_model_alias_result_from_config_models(requested, &models);
    if result.upstream_model.trim().is_empty() {
        return AliasResult {
            upstream_model: requested.to_string(),
            ..Default::default()
        };
    }
    result
}

/// First-wins alias lookup replicating the compiled `alias -> name` table of the Go snapshot:
/// per model, in order: alias, base(alias), name, base(name) all map to `name`.
pub fn lookup_api_key_upstream_model(cfg: &Config, auth: &Auth, requested: &str) -> String {
    if auth.id.trim().is_empty() {
        return String::new();
    }
    let requested = requested.trim();
    if requested.is_empty() {
        return String::new();
    }
    let models = configured_models(cfg, auth);
    if models.is_empty() {
        return String::new();
    }
    let mut keys = vec![requested.to_lowercase()];
    let base_key = parse_suffix(requested).model_name.trim().to_lowercase();
    if !base_key.is_empty() && base_key != keys[0] {
        keys.push(base_key);
    }
    for key in &keys {
        for m in &models {
            let alias = m.alias.trim();
            let name = m.name.trim();
            if alias.is_empty() || name.is_empty() {
                continue;
            }
            let additions = [
                alias.to_lowercase(),
                parse_suffix(alias).model_name.trim().to_lowercase(),
                name.to_lowercase(),
                parse_suffix(name).model_name.trim().to_lowercase(),
            ];
            if additions.iter().any(|a| !a.is_empty() && a == key) {
                return preserve_requested_model_suffix(requested, name);
            }
        }
    }
    String::new()
}

/// Upstream model for an API-key auth: alias table, then config scan, else the request.
pub fn apply_api_key_model_alias(cfg: &Config, auth: &Auth, requested: &str) -> String {
    if auth.auth_kind() != AUTH_KIND_API_KEY {
        return requested.to_string();
    }
    let requested = requested.trim();
    if requested.is_empty() {
        return String::new();
    }
    let fast = lookup_api_key_upstream_model(cfg, auth, requested);
    if !fast.is_empty() {
        return fast;
    }
    let models = configured_models(cfg, auth);
    let resolved = resolve_model_alias_from_config_models(requested, &models);
    if resolved.is_empty() {
        requested.to_string()
    } else {
        resolved
    }
}

fn resolve_model_alias_result_for_upstream(
    cfg: &Config,
    auth: &Auth,
    requested: &str,
    upstream: &str,
) -> AliasResult {
    let requested = requested.trim();
    let upstream = upstream.trim();
    if requested.is_empty() || upstream.is_empty() {
        return AliasResult::default();
    }
    let request = parse_suffix(requested);
    let filtered: Vec<ConfiguredModel> = configured_models(cfg, auth)
        .into_iter()
        .filter(|m| {
            let name = m.name.trim();
            !name.is_empty() && eq_fold(&preserve_resolved_model_suffix(name, &request), upstream)
        })
        .collect();
    if filtered.is_empty() {
        return AliasResult::default();
    }
    resolve_model_alias_result_from_config_models(requested, &filtered)
}

/// Alias result of the model actually attempted (matters for pooled compat aliases).
pub fn resolve_attempt_alias_result(
    cfg: &Config,
    auth: &Auth,
    route_model: &str,
    upstream_model: &str,
    fallback: &AliasResult,
) -> AliasResult {
    if !is_configured_model_routing_auth(auth) {
        return fallback.clone();
    }
    let requested = rewrite_model_for_auth(route_model, auth);
    let mut result = resolve_model_alias_result_for_upstream(cfg, auth, &requested, upstream_model);
    if result.upstream_model.trim().is_empty() {
        return fallback.clone();
    }
    if result.force_mapping && fallback.force_mapping && !fallback.original_alias.trim().is_empty()
    {
        result.original_alias = fallback.original_alias.clone();
    }
    result
}

/// Pool key for rotating compat alias pools.
pub fn openai_compat_model_pool_key(auth: &Auth, requested: &str) -> String {
    let mut base = parse_suffix(requested).model_name.trim().to_string();
    if base.is_empty() {
        base = requested.trim().to_string();
    }
    let provider_key = {
        let pk = auth.attr("provider_key");
        let cn = auth.attr("compat_name");
        if !pk.is_empty() {
            cpa_core::util::openai_compatible_provider_key(&pk)
        } else if !cn.is_empty() {
            cpa_core::util::openai_compatible_provider_key(&cn)
        } else {
            cpa_core::util::openai_compatible_provider_key(&auth.provider)
        }
    };
    format!(
        "{}|{provider_key}|{}",
        auth.id.trim().to_lowercase(),
        base.to_lowercase()
    )
}

pub fn rotate_strings(values: &[String], offset: usize) -> Vec<String> {
    if values.len() <= 1 {
        return values.to_vec();
    }
    let offset = offset % values.len();
    let mut out = Vec::with_capacity(values.len());
    out.extend_from_slice(&values[offset..]);
    out.extend_from_slice(&values[..offset]);
    out
}

/// Pool model used as the base for pool/alias resolution (Go: executionAliasPoolModel).
pub fn execution_alias_pool_model(auth: &Auth, requested: &str, alias: &AliasResult) -> String {
    if is_configured_model_routing_auth(auth) && !requested.trim().is_empty() {
        return requested.to_string();
    }
    if !alias.upstream_model.trim().is_empty() {
        return alias.upstream_model.clone();
    }
    requested.to_string()
}

pub fn execution_result_model(route_model: &str, upstream_model: &str, pooled: bool) -> String {
    if pooled && !upstream_model.trim().is_empty() {
        return upstream_model.trim().to_string();
    }
    if !route_model.trim().is_empty() {
        return route_model.trim().to_string();
    }
    upstream_model.trim().to_string()
}

// ---- Response model rewrite for force-mapped aliases ----

/// Rewrites the model field(s) of a JSON response to `target_model`.
pub fn rewrite_model_in_response(data: &[u8], target_model: &str) -> Vec<u8> {
    super::rewriter::rewrite_model(data, target_model)
}

// ---- Capability snapshot attached to requests ----

fn normalize_thinking(raw: &CfgThinking) -> ThinkingSupport {
    let mut out = ThinkingSupport {
        min: raw.min,
        max: raw.max,
        zero_allowed: raw.zero_allowed,
        dynamic_allowed: raw.dynamic_allowed,
        levels: Vec::new(),
    };
    for value in &raw.levels {
        let level = value.trim().to_lowercase();
        if level.is_empty() {
            continue;
        }
        match level.as_str() {
            "none" => out.zero_allowed = true,
            "auto" => out.dynamic_allowed = true,
            _ => {}
        }
        if !out.levels.contains(&level) {
            out.levels.push(level);
        }
    }
    out
}

/// Private capability snapshot for a configured model (Go: modelconfig.ResolveModelInfo): static
/// catalog metadata of the suffix-free name, with explicit thinking config taking precedence.
pub fn resolve_model_info(
    name: &str,
    model_type: &str,
    support: Option<&CfgThinking>,
) -> ModelInfo {
    let trimmed = name.trim();
    let base = parse_suffix(trimmed).model_name;
    let mut info = cpa_core::registry::lookup_static_model_info(base.trim()).unwrap_or_default();
    info.id = trimmed.to_string();
    info.r#type = model_type.trim().to_string();
    if let Some(s) = support {
        info.thinking = Some(normalize_thinking(s));
    }
    info.user_defined = false;
    info
}

#[derive(Debug, Clone)]
struct CapabilityRoute {
    upstream_model: String,
    model: ConfiguredModel,
    model_type: &'static str,
}

fn configured_upstream_fallback_matches(configured: &str, selected: &str) -> bool {
    let c = parse_suffix(configured.trim());
    if c.has_suffix {
        return false;
    }
    let s = parse_suffix(selected.trim());
    eq_fold(c.model_name.trim(), s.model_name.trim())
}

fn capability_model_type(auth: &Auth) -> &'static str {
    match auth.provider.trim().to_lowercase().as_str() {
        "gemini" | "vertex" => "gemini",
        "gemini-interactions" => "interactions",
        "claude" => "claude",
        "codex" => "codex",
        "xai" => "xai",
        "meta" => "meta",
        _ => "openai-compatibility",
    }
}

/// Capability routes of an auth keyed by lookup candidates (Go: compileAPIKeyModelCapabilitiesForAuth,
/// evaluated lazily for the candidates of one request).
fn capability_routes(cfg: &Config, auth: &Auth, candidates: &[String]) -> Vec<CapabilityRoute> {
    let models = configured_models_for(cfg, auth, false);
    let model_type = capability_model_type(auth);
    let wanted: Vec<String> = candidates.iter().map(|c| c.trim().to_lowercase()).collect();
    let mut by_key: HashMap<String, Vec<CapabilityRoute>> = HashMap::new();
    for m in &models {
        let mut name = m.name.trim().to_string();
        let mut alias = m.alias.trim().to_string();
        if name.is_empty() {
            name = alias.clone();
        }
        if alias.is_empty() {
            alias = name.clone();
        }
        if name.is_empty() {
            continue;
        }
        let mut model = m.clone();
        if model_type == "openai-compatibility" && model.thinking.is_none() && !model.image {
            model.thinking = Some(CfgThinking {
                levels: vec!["low".into(), "medium".into(), "high".into()],
                ..Default::default()
            });
        }
        let route = CapabilityRoute {
            upstream_model: name,
            model,
            model_type,
        };
        let mut seen_keys: Vec<String> = Vec::new();
        for route_model in [&alias, &route.upstream_model] {
            let (_, cands) = alias_lookup_candidates(route_model);
            for c in cands {
                let key = c.trim().to_lowercase();
                if key.is_empty() || seen_keys.contains(&key) {
                    continue;
                }
                seen_keys.push(key.clone());
                if !wanted.contains(&key) {
                    continue;
                }
                let list = by_key.entry(key).or_default();
                if !list
                    .iter()
                    .any(|e| eq_fold(&e.upstream_model, &route.upstream_model))
                {
                    list.push(route.clone());
                }
            }
        }
    }
    let mut out = Vec::new();
    for w in &wanted {
        if let Some(list) = by_key.get(w) {
            out.extend(list.iter().cloned());
        }
    }
    out
}

fn route_model_info(route: &CapabilityRoute) -> ModelInfo {
    let mut info = resolve_model_info(
        &route.upstream_model,
        route.model_type,
        route.model.thinking.as_ref(),
    );
    info.is_compat = route.model.is_compat;
    // Only codex-api-key entries carry `support-configuration-update`.
    if route.model_type == "codex" {
        info.support_configuration_update = route.model.support_configuration_update;
    }
    info
}

fn lookup_api_key_model_capability(
    cfg: &Config,
    auth: &Auth,
    route_model: &str,
    upstream_model: &str,
) -> Option<ModelInfo> {
    if !is_configured_model_routing_auth(auth) {
        return None;
    }
    let requested = rewrite_model_for_auth(route_model.trim(), auth);
    let (_, candidates) = alias_lookup_candidates(&requested);
    let routes = capability_routes(cfg, auth, &candidates);
    if routes.is_empty() {
        return None;
    }
    let selected = upstream_model.trim();
    if let Some(r) = routes
        .iter()
        .find(|r| eq_fold(r.upstream_model.trim(), selected))
    {
        return Some(route_model_info(r));
    }
    routes
        .iter()
        .find(|r| configured_upstream_fallback_matches(&r.upstream_model, selected))
        .map(route_model_info)
}

fn lookup_unlisted_codex_api_key_model_capability(
    cfg: &Config,
    auth: &Auth,
    upstream_model: &str,
) -> Option<ModelInfo> {
    if auth.auth_kind() != AUTH_KIND_API_KEY
        || !auth.provider.trim().eq_ignore_ascii_case("codex")
        || upstream_model.trim().is_empty()
    {
        return None;
    }
    let entry = resolve_api_key_config(&cfg.codex_key, auth)?;
    let key = auth.attr(ATTRIBUTE_API_KEY);
    let base_url = auth.attr("base_url");
    if (key.is_empty() && base_url.is_empty())
        || !eq_fold(&key, entry.api_key.trim())
        || (!entry.base_url.trim().is_empty() && !eq_fold(&base_url, entry.base_url.trim()))
    {
        return None;
    }
    for configured in &entry.models {
        if eq_fold(configured.name.trim(), upstream_model.trim())
            || configured_upstream_fallback_matches(&configured.name, upstream_model)
        {
            let mut info =
                resolve_model_info(upstream_model, "codex", configured.thinking.as_ref());
            info.support_configuration_update = configured.support_configuration_update;
            info.is_compat = configured.is_compat;
            return Some(info);
        }
    }
    let mut info = resolve_model_info(upstream_model, "codex", None);
    info.support_configuration_update = false;
    Some(info)
}

fn lookup_codex_oauth_model_capability(auth: &Auth, upstream_model: &str) -> Option<ModelInfo> {
    if auth.auth_kind() != AUTH_KIND_OAUTH || !auth.provider.trim().eq_ignore_ascii_case("codex") {
        return None;
    }
    let models = match auth.attr("plan_type").to_lowercase().as_str() {
        "plus" => cpa_core::registry::get_codex_plus_models(),
        "team" | "business" | "go" => cpa_core::registry::get_codex_team_models(),
        "free" => cpa_core::registry::get_codex_free_models(),
        _ => cpa_core::registry::get_codex_pro_models(),
    };
    let selected = parse_suffix(upstream_model.trim()).model_name;
    let selected = selected.trim();
    models.into_iter().find(|m| eq_fold(&m.id, selected))
}

/// A `ModelInfo` plus the internal flags `ModelInfo` does not serialize.
#[derive(Debug, Clone)]
pub struct ResolvedModelInfo {
    pub info: ModelInfo,
    pub is_compat: bool,
    pub support_configuration_update: bool,
}

pub(crate) fn model_info_value(info: &ModelInfo) -> Value {
    let mut v = serde_json::to_value(info).unwrap_or(Value::Null);
    if let Value::Object(m) = &mut v {
        m.insert("is_compat".into(), Value::Bool(info.is_compat));
        m.insert(
            "support_configuration_update".into(),
            Value::Bool(info.support_configuration_update),
        );
    }
    v
}

/// Capability snapshot bound to this execution attempt, if any (Go: ResolvedModelInfo).
pub fn resolved_model_info(req: &Request) -> Option<ResolvedModelInfo> {
    for key in [RESOLVED_HOME_MODEL_INFO, RESOLVED_CODEX_OAUTH_MODEL_INFO, RESOLVED_API_KEY_MODEL_INFO] {
        let Some(v) = req.metadata.get(key) else {
            continue;
        };
        let Ok(mut info) = serde_json::from_value::<ModelInfo>(v.clone()) else {
            continue;
        };
        let is_compat = v.get("is_compat").and_then(Value::as_bool).unwrap_or(false);
        let support = v
            .get("support_configuration_update")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        info.is_compat = is_compat;
        info.support_configuration_update = support;
        return Some(ResolvedModelInfo {
            info,
            is_compat,
            support_configuration_update: support,
        });
    }
    None
}

/// Attaches the configured model's capabilities to the request metadata (Go:
/// attachResolvedExecutionModelInfo).
pub fn attach_resolved_execution_model_info(
    cfg: &Config,
    req: &mut Request,
    auth: &Auth,
    route_model: &str,
    upstream_model: &str,
    restore_execution_model: bool,
) {
    let (route_model, upstream_model) = if restore_execution_model {
        if !auth.provider.trim().eq_ignore_ascii_case("codex") {
            return;
        }
        (req.model.clone(), req.model.clone())
    } else {
        (route_model.to_string(), upstream_model.to_string())
    };
    let mut key = RESOLVED_API_KEY_MODEL_INFO;
    let mut info = lookup_api_key_model_capability(cfg, auth, &route_model, &upstream_model)
        .or_else(|| lookup_unlisted_codex_api_key_model_capability(cfg, auth, &upstream_model));
    if info.is_none() {
        info = lookup_codex_oauth_model_capability(auth, &upstream_model);
        key = RESOLVED_CODEX_OAUTH_MODEL_INFO;
    }
    if info.is_none()
        && !req.metadata.contains_key(RESOLVED_API_KEY_MODEL_INFO)
        && !req.metadata.contains_key(RESOLVED_CODEX_OAUTH_MODEL_INFO)
    {
        return;
    }
    req.metadata.remove(RESOLVED_API_KEY_MODEL_INFO);
    req.metadata.remove(RESOLVED_CODEX_OAUTH_MODEL_INFO);
    if let Some(info) = info {
        req.metadata
            .insert(key.to_string(), model_info_value(&info));
    }
}

/// `is-compat` of the configured codex-api-key model (Go: CodexAPIKeyModelIsCompat).
pub fn codex_api_key_model_is_compat(cfg: &Config, auth: &Auth, model: &str) -> bool {
    if !auth.provider.trim().eq_ignore_ascii_case("codex") {
        return false;
    }
    if cfg.home.enabled
        && let Some(options) = super::home_model_info::home_api_key_model_options(auth, model, model)
    {
        return options.is_compat;
    }
    let Some(entry) = resolve_api_key_config(&cfg.codex_key, auth) else {
        return false;
    };
    if entry.models.is_empty() {
        return false;
    }
    let requested = model.trim();
    if requested.is_empty() {
        return false;
    }
    let mut base = parse_suffix(requested).model_name.trim().to_string();
    if base.is_empty() {
        base = requested.to_string();
    }
    for m in &entry.models {
        let mut name = m.name.trim().to_string();
        let mut alias = m.alias.trim().to_string();
        if name.is_empty() {
            name = alias.clone();
        }
        if alias.is_empty() {
            alias = name.clone();
        }
        if name.is_empty() {
            continue;
        }
        if eq_fold(&name, requested)
            || eq_fold(&name, &base)
            || eq_fold(&alias, requested)
            || eq_fold(&alias, &base)
        {
            return m.is_compat;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_config::{ClaudeModel, OpenAiCompatibilityModel};

    fn cm(name: &str, alias: &str, force: bool) -> ConfiguredModel {
        ConfiguredModel {
            name: name.into(),
            alias: alias.into(),
            force_mapping: force,
            ..Default::default()
        }
    }

    #[test]
    fn alias_result_preserves_suffix_and_force_mapping() {
        let models = vec![
            cm("claude-sonnet-4", "sonnet", false),
            cm("gpt-5", "gpt-5", true),
        ];
        let r = resolve_model_alias_result_from_config_models("sonnet(8192)", &models);
        assert_eq!(r.upstream_model, "claude-sonnet-4(8192)");
        assert!(!r.force_mapping);
        assert_eq!(r.original_alias, "sonnet(8192)");
        // alias == name without force: no mapping; with force: rewrite back to the alias.
        let r = resolve_model_alias_result_from_config_models("gpt-5", &models);
        assert!(r.force_mapping && r.upstream_model == "gpt-5" && r.original_alias == "gpt-5");
        assert_eq!(
            resolve_model_alias_result_from_config_models("nope", &models),
            AliasResult::default()
        );
    }

    #[test]
    fn pool_collects_distinct_names_in_config_order() {
        let models = vec![
            cm("up-a", "pool", false),
            cm("up-b", "pool", false),
            cm("UP-A", "pool", false),
        ];
        assert_eq!(
            resolve_model_alias_pool("pool", &models),
            vec!["up-a".to_string(), "up-b".to_string()]
        );
        // Name fallback when no alias matches.
        let models = vec![cm("real", "other", false)];
        assert_eq!(
            resolve_model_alias_pool("real", &models),
            vec!["real".to_string()]
        );
        assert_eq!(
            rotate_strings(&["a".into(), "b".into(), "c".into()], 1),
            vec!["b", "c", "a"]
        );
    }

    #[test]
    fn oauth_alias_table_first_wins_and_force_mapping() {
        let mut aliases = BTreeMap::new();
        aliases.insert(
            "Claude".to_string(),
            vec![
                OAuthModelAlias {
                    name: "claude-sonnet-4-5".into(),
                    alias: "sonnet".into(),
                    ..Default::default()
                },
                OAuthModelAlias {
                    name: "other".into(),
                    alias: "Sonnet".into(),
                    ..Default::default()
                },
                OAuthModelAlias {
                    name: "claude-opus".into(),
                    alias: "opus".into(),
                    force_mapping: true,
                    ..Default::default()
                },
            ],
        );
        let table = compile_oauth_model_alias_table(&aliases);
        let mut auth = Auth::new("a.json", "claude");
        auth.metadata.insert("access_token".into(), "t".into());
        assert_eq!(
            apply_oauth_model_alias(&table, &auth, "SONNET(high)"),
            "claude-sonnet-4-5(high)"
        );
        let r = resolve_oauth_model_alias_with_result(&table, &auth, "opus");
        assert!(r.force_mapping);
        assert_eq!(r.original_alias, "opus");
        // API-key auths never use OAuth aliases.
        let mut key = Auth::new("k", "claude");
        key.attributes.insert("api_key".into(), "sk".into());
        assert_eq!(apply_oauth_model_alias(&table, &key, "sonnet"), "sonnet");
    }

    #[test]
    fn api_key_alias_uses_first_wins_table() {
        let mut cfg = Config::default();
        cfg.claude_key.push(ClaudeKey {
            api_key: "sk".into(),
            models: vec![
                ClaudeModel {
                    name: "A".into(),
                    alias: "X".into(),
                    ..Default::default()
                },
                ClaudeModel {
                    name: "B".into(),
                    alias: "A".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        let mut auth = Auth::new("k", "claude");
        auth.attributes.insert("api_key".into(), "sk".into());
        auth.attributes.insert("auth_kind".into(), "apikey".into());
        assert_eq!(apply_api_key_model_alias(&cfg, &auth, "X"), "A");
        // name->name of entry 1 wins over alias A->B of entry 2 in the compiled table.
        assert_eq!(apply_api_key_model_alias(&cfg, &auth, "A"), "A");
        assert_eq!(apply_api_key_model_alias(&cfg, &auth, "zzz"), "zzz");
    }

    #[test]
    fn compat_pool_and_capability_defaults() {
        let mut cfg = Config::default();
        cfg.openai_compatibility.push(OpenAiCompatibility {
            name: "Pool".into(),
            models: vec![
                OpenAiCompatibilityModel {
                    name: "m1".into(),
                    alias: "gpt".into(),
                    ..Default::default()
                },
                OpenAiCompatibilityModel {
                    name: "m2".into(),
                    alias: "gpt".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        let mut auth = Auth::new("c1", "openai-compatibility");
        auth.attributes.insert("compat_name".into(), "pool".into());
        auth.attributes.insert("api_key".into(), "k".into());
        auth.attributes
            .insert("source".into(), "config:openai-compat[abc]".into());
        assert_eq!(
            resolve_openai_compat_upstream_model_pool(&cfg, &auth, "gpt"),
            vec!["m1".to_string(), "m2".to_string()]
        );
        let mut req = Request {
            model: "m1".into(),
            payload: Default::default(),
            format: cpa_translator::Format::OpenAI,
            metadata: Default::default(),
        };
        attach_resolved_execution_model_info(&cfg, &mut req, &auth, "gpt", "m1", false);
        let resolved = resolved_model_info(&req).unwrap();
        assert_eq!(resolved.info.id, "m1");
        assert_eq!(
            resolved.info.thinking.unwrap().levels,
            vec!["low", "medium", "high"]
        );
    }

    #[test]
    fn response_rewrite_sets_known_model_paths() {
        let out =
            rewrite_model_in_response(br#"{"model":"up","message":{"model":"up"},"x":1}"#, "alias");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"model":"alias","message":{"model":"alias"},"x":1}"#
        );
        let same = rewrite_model_in_response(b"not json", "alias");
        assert_eq!(same, b"not json");
    }
}
