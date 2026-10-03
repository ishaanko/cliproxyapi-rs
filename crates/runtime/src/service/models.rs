//! Per-auth model registration (Go: sdk/cliproxy/service_models.go, service_executors.go
//! `registerResolvedModelsForAuth`, internal/modelconfig).
//!
//! [`resolve_models_for_auth`] computes what an auth contributes to the model registry: the
//! provider's static catalog or the config `models[]` entries, then exclusions, OAuth aliases,
//! OAuth settings and prefixes, in the same order as Go. [`register_models_for_auth`] applies the
//! result to a [`ModelRegistry`]. Both are pure over `(Config, Auth)` so reloads can be tested
//! without a running service.

use std::collections::{BTreeMap, HashSet};

use cpa_auth::types::AUTH_SOURCE_CONFIG;
use cpa_auth::Auth;
use cpa_config::{
    ClaudeKey, ClaudeModel, CodexKey, CodexModel, Config, GeminiKey, GeminiModel, OAuthModelAlias,
    OAuthModelSetting, OpenAiCompatibility, OpenAiCompatibilityModel, VertexCompatKey, VertexCompatModel,
    resolve_oauth_model_setting,
};
use cpa_core::registry::{
    ModelInfo, ModelRegistry, OPENAI_IMAGE_MODEL_TYPE, ThinkingSupport, get_ai_studio_models,
    get_antigravity_models, get_claude_models, get_codex_free_models, get_codex_plus_models,
    get_codex_pro_models, get_codex_team_models, get_devin_models, get_gemini_models,
    get_gemini_vertex_models, get_kimi_models, get_meta_models, get_xai_models,
    lookup_static_model_info, lookup_static_model_info_by_channel,
};
use cpa_core::thinking::parse_suffix;
use cpa_core::util::openai_compatible_provider_key;

use super::plugins::{ServicePlugins, append_plugin_models};
use super::synth::{ATTRIBUTE_CONFIG_INDEX, oauth_model_aliases_from_attributes};

/// What an auth contributes to the model registry.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelRegistration {
    /// The auth provides no models (disabled, unknown provider, everything excluded).
    Unregister,
    Register { provider: String, models: Vec<ModelInfo> },
}

/// Computes the registry entry for `auth` under `cfg` (Go: `registerModelsForAuthWithCache`
/// without the manager-state checks and the registry write).
pub fn resolve_models_for_auth(cfg: &Config, auth: &Auth) -> ModelRegistration {
    resolve_models_for_auth_with(cfg, auth, None)
}

/// [`resolve_models_for_auth`] with the plugin model providers (Go: `appendPluginModels` calls).
pub(super) fn resolve_models_for_auth_with(
    cfg: &Config,
    auth: &Auth,
    plugins: Option<&dyn ServicePlugins>,
) -> ModelRegistration {
    if auth.disabled || auth.id.is_empty() {
        return ModelRegistration::Unregister;
    }
    let auth_kind = auth.auth_kind();
    let mut provider = auth.provider.trim().to_lowercase();
    let compat = openai_compat_info_from_auth(auth);
    if compat.is_some() {
        provider = "openai-compatibility".into();
    }
    let mut excluded: Vec<String> = oauth_excluded_models(cfg, &provider, auth_kind);
    // The synthesizer pre-merges per-account and global exclusions into `excluded_models`; when
    // present it is the complete list and overrides the global config.
    if let Some(val) = auth.attributes.get("excluded_models").filter(|v| !v.trim().is_empty()) {
        excluded = val.split(',').map(str::to_string).collect();
    }

    let apikey = auth_kind == "apikey";
    let models: Vec<ModelInfo> = match provider.as_str() {
        "gemini" | "gemini-interactions" => {
            let entries = if provider == "gemini" { &cfg.gemini_key } else { &cfg.interactions_key };
            let mut models = get_gemini_models();
            if let Some(entry) = resolve_gemini_key(auth, entries) {
                if !entry.models.is_empty() {
                    models = build_config_models(&entry.models, "google", "gemini", "gemini");
                }
                if apikey {
                    excluded = entry.excluded_models.clone();
                }
            }
            apply_excluded_models(models, &excluded)
        }
        "vertex" => {
            let mut models = get_gemini_vertex_models();
            if let Some(entry) = resolve_vertex_key(auth, &cfg.vertex_compat_api_key) {
                if !entry.models.is_empty() {
                    models = build_config_models(&entry.models, "google", "vertex", "vertex");
                }
                if apikey {
                    excluded = entry.excluded_models.clone();
                }
            }
            apply_excluded_models(models, &excluded)
        }
        "aistudio" => apply_excluded_models(get_ai_studio_models(), &excluded),
        "antigravity" => apply_excluded_models(get_antigravity_models(), &excluded),
        "claude" => {
            let mut models = get_claude_models();
            if let Some(entry) = resolve_claude_key(auth, &cfg.claude_key) {
                if !entry.models.is_empty() {
                    models = build_config_models(&entry.models, "anthropic", "claude", "claude");
                }
                if apikey {
                    excluded = entry.excluded_models.clone();
                }
            }
            apply_excluded_models(models, &excluded)
        }
        "codex" => {
            if apikey {
                let mut models = Vec::new();
                if let Some(entry) = resolve_codex_style_key(auth, &cfg.codex_key, true) {
                    models = build_codex_config_models(entry);
                    excluded = entry.excluded_models.clone();
                }
                apply_excluded_models(models, &excluded)
            } else {
                let models = match auth.attr("plan_type").to_lowercase().as_str() {
                    "plus" => get_codex_plus_models(),
                    "team" | "business" | "go" => get_codex_team_models(),
                    "free" => get_codex_free_models(),
                    // "pro" and anything unknown.
                    _ => get_codex_pro_models(),
                };
                apply_excluded_models(models, &excluded)
            }
        }
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => apply_excluded_models(get_kimi_models(), &excluded),
        "xai" => {
            let mut models = get_xai_models();
            if let Some(entry) = resolve_codex_style_key(auth, &cfg.xai_key, false) {
                if !entry.models.is_empty() {
                    models = build_config_models(&entry.models, "xai", "xai", "xai");
                }
                if apikey {
                    excluded = entry.excluded_models.clone();
                }
            }
            apply_excluded_models(models, &excluded)
        }
        "devin" => apply_excluded_models(get_devin_models(), &excluded),
        "meta" => {
            let mut models = get_meta_models();
            if let Some(entry) = resolve_codex_style_key(auth, &cfg.meta_key, false) {
                if !entry.models.is_empty() {
                    models = build_config_models(&entry.models, "meta", "meta", "meta");
                }
                if apikey {
                    excluded = entry.excluded_models.clone();
                }
            }
            apply_excluded_models(models, &excluded)
        }
        _ => {
            if let Some(reg) = resolve_compat_models(cfg, auth, &provider, compat, plugins) {
                return reg;
            }
            Vec::new()
        }
    };

    let models = apply_oauth_model_alias_for_auth(cfg, &provider, auth_kind, &auth.attributes, models);
    let key = if provider.is_empty() { auth.provider.trim().to_lowercase() } else { provider };
    let models = append_plugin_models(plugins, &key, models);
    if models.is_empty() {
        return ModelRegistration::Unregister;
    }
    let models = apply_oauth_settings_for_auth(cfg, &key, auth_kind, models);
    finalize(&key, apply_model_prefixes(models, &auth.prefix, cfg.force_model_prefix))
}

/// Go `registerResolvedModelsForAuth`: trims ids, drops empty ones, and unregisters when nothing
/// (or no provider) is left.
fn finalize(provider: &str, models: Vec<ModelInfo>) -> ModelRegistration {
    let provider = provider.trim().to_lowercase();
    if provider.is_empty() {
        return ModelRegistration::Unregister;
    }
    let models: Vec<ModelInfo> = models
        .into_iter()
        .filter_map(|mut m| {
            let id = m.id.trim().to_string();
            if id.is_empty() {
                return None;
            }
            m.id = id;
            Some(m)
        })
        .collect();
    if models.is_empty() {
        ModelRegistration::Unregister
    } else {
        ModelRegistration::Register { provider, models }
    }
}

pub(super) fn finalize_registration(provider: &str, models: Vec<ModelInfo>) -> ModelRegistration {
    finalize(provider, models)
}

/// Writes `reg` for `client_id` into the registry.
pub fn apply_registration(registry: &ModelRegistry, client_id: &str, reg: ModelRegistration) {
    match reg {
        ModelRegistration::Unregister => registry.unregister_client(client_id),
        ModelRegistration::Register { provider, models } => registry.register_client(client_id, &provider, &models),
    }
}

/// Resolves and applies the model registration of one auth.
pub fn register_models_for_auth(registry: &ModelRegistry, cfg: &Config, auth: &Auth) {
    if auth.id.is_empty() {
        return;
    }
    apply_registration(registry, &auth.id, resolve_models_for_auth(cfg, auth));
}

/// Go `openAICompatInfoFromAuth`: `(provider key, display name)` when the auth belongs to an
/// OpenAI-compatibility provider.
pub fn openai_compat_info_from_auth(auth: &Auth) -> Option<(String, String)> {
    let compat_name = auth.attr("compat_name");
    if !compat_name.is_empty() {
        let provider_key = auth.attr("provider_key");
        let key = if provider_key.is_empty() { compat_name.clone() } else { provider_key };
        return Some((openai_compatible_provider_key(&key), compat_name));
    }
    if auth.provider.trim().eq_ignore_ascii_case("openai-compatibility") {
        let name = auth.label.trim().to_string();
        let key = if name.is_empty() { "openai-compatibility".to_string() } else { name.clone() };
        return Some((openai_compatible_provider_key(&key), name));
    }
    None
}

/// The default branch of Go's provider switch: models of an OpenAI-compatibility entry.
/// `Some` when the auth is a compat auth (the registration is final), `None` to fall through to
/// the generic tail with an empty list.
fn resolve_compat_models(
    cfg: &Config,
    auth: &Auth,
    provider: &str,
    compat: Option<(String, String)>,
    plugins: Option<&dyn ServicePlugins>,
) -> Option<ModelRegistration> {
    let mut provider_key = provider.to_string();
    let mut compat_name = auth.provider.trim().to_string();
    let mut is_compat = false;
    if let Some((key, display)) = compat {
        if !key.is_empty() {
            provider_key = key;
        }
        if !display.is_empty() {
            compat_name = display;
        }
        is_compat = true;
    }
    if provider_key.eq_ignore_ascii_case("openai-compatibility") {
        is_compat = true;
        let name = auth.attr("compat_name");
        if !name.is_empty() {
            compat_name = name;
        }
        let key = auth.attr("provider_key");
        if !key.is_empty() {
            provider_key = key.to_lowercase();
        }
        if provider_key == "openai-compatibility" && !compat_name.is_empty() {
            provider_key = compat_name.to_lowercase();
        }
    } else {
        let name = auth.attr("compat_name");
        if !name.is_empty() {
            compat_name = name;
            is_compat = true;
        }
        let key = auth.attr("provider_key");
        if !key.is_empty() {
            provider_key = key.to_lowercase();
            is_compat = true;
        }
    }

    let register = |entry: &OpenAiCompatibility, provider_key: &str| {
        let provider_key = if provider_key.is_empty() { "openai-compatibility" } else { provider_key };
        let models = append_plugin_models(plugins, provider_key, build_openai_compat_config_models(entry));
        finalize(provider_key, apply_model_prefixes(models, &auth.prefix, cfg.force_model_prefix))
    };
    if let Some(entry) = config_entry_for_auth_index(auth, &cfg.openai_compatibility).filter(|e| !e.disabled) {
        return Some(register(entry, &provider_key));
    }
    if let Some(entry) = cfg
        .openai_compatibility
        .iter()
        .find(|e| !e.disabled && e.name.to_lowercase() == compat_name.to_lowercase())
    {
        return Some(register(entry, &provider_key));
    }
    // A compat auth whose entry is gone (or disabled) keeps only plugin models, if any.
    if is_compat {
        let key = if provider_key.is_empty() { "openai-compatibility" } else { provider_key.as_str() };
        let models = append_plugin_models(plugins, key, Vec::new());
        return Some(finalize(key, apply_model_prefixes(models, &auth.prefix, cfg.force_model_prefix)));
    }
    None
}

// ---- Config entry resolution ----

/// Go `configEntryForAuthIndex`: the entry a config-sourced auth was synthesized from.
fn config_entry_for_auth_index<'a, T>(auth: &Auth, entries: &'a [T]) -> Option<&'a T> {
    if auth.auth_source_kind() != AUTH_SOURCE_CONFIG {
        return None;
    }
    let index: usize = auth.attr(ATTRIBUTE_CONFIG_INDEX).parse().ok()?;
    entries.get(index)
}

/// API key and base URL of a config key entry.
trait KeyFields {
    fn key(&self) -> &str;
    fn base(&self) -> &str;
}

macro_rules! key_fields {
    ($($ty:ty),*) => {$(
        impl KeyFields for $ty {
            fn key(&self) -> &str { &self.api_key }
            fn base(&self) -> &str { &self.base_url }
        }
    )*};
}
key_fields!(ClaudeKey, GeminiKey, VertexCompatKey, CodexKey);

fn eq_fold(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

fn resolve_claude_key<'a>(auth: &Auth, entries: &'a [ClaudeKey]) -> Option<&'a ClaudeKey> {
    if let Some(entry) = config_entry_for_auth_index(auth, entries) {
        return Some(entry);
    }
    let (attr_key, attr_base) = (auth.attr("api_key"), auth.attr("base_url"));
    for entry in entries {
        let (cfg_key, cfg_base) = (entry.key().trim(), entry.base().trim());
        if !attr_key.is_empty() && !attr_base.is_empty() {
            if eq_fold(cfg_key, &attr_key) && eq_fold(cfg_base, &attr_base) {
                return Some(entry);
            }
            continue;
        }
        if !attr_key.is_empty() && eq_fold(cfg_key, &attr_key) && (cfg_base.is_empty() || eq_fold(cfg_base, &attr_base)) {
            return Some(entry);
        }
        if attr_key.is_empty() && !attr_base.is_empty() && eq_fold(cfg_base, &attr_base) {
            return Some(entry);
        }
    }
    key_only_match(&attr_key, entries)
}

/// Last-resort lookup by API key alone.
fn key_only_match<'a, E: KeyFields>(attr_key: &str, entries: &'a [E]) -> Option<&'a E> {
    if attr_key.is_empty() {
        return None;
    }
    entries.iter().find(|e| eq_fold(e.key().trim(), attr_key))
}

/// Shared by gemini and vertex: key (+ optional base) or base-only match.
fn match_key_or_base<'a, E: KeyFields>(auth: &Auth, entries: &'a [E]) -> Option<&'a E> {
    let (attr_key, attr_base) = (auth.attr("api_key"), auth.attr("base_url"));
    for entry in entries {
        let (cfg_key, cfg_base) = (entry.key().trim(), entry.base().trim());
        if !attr_key.is_empty() && eq_fold(cfg_key, &attr_key) {
            if cfg_base.is_empty() || eq_fold(cfg_base, &attr_base) {
                return Some(entry);
            }
            continue;
        }
        if attr_key.is_empty() && !attr_base.is_empty() && eq_fold(cfg_base, &attr_base) {
            return Some(entry);
        }
    }
    None
}

fn resolve_gemini_key<'a>(auth: &Auth, entries: &'a [GeminiKey]) -> Option<&'a GeminiKey> {
    config_entry_for_auth_index(auth, entries).or_else(|| match_key_or_base(auth, entries))
}

fn resolve_vertex_key<'a>(auth: &Auth, entries: &'a [VertexCompatKey]) -> Option<&'a VertexCompatKey> {
    config_entry_for_auth_index(auth, entries)
        .or_else(|| match_key_or_base(auth, entries))
        .or_else(|| key_only_match(&auth.attr("api_key"), entries))
}

/// Codex, xAI and Meta entries. Codex validates that the entry at `config_index` still carries
/// the auth's credentials (`validate_index_credentials`).
fn resolve_codex_style_key<'a>(
    auth: &Auth,
    entries: &'a [CodexKey],
    validate_index_credentials: bool,
) -> Option<&'a CodexKey> {
    let (attr_key, attr_base) = (auth.attr("api_key"), auth.attr("base_url"));
    let matches_credentials = |entry: &CodexKey| {
        let (cfg_key, cfg_base) = (entry.api_key.trim(), entry.base_url.trim());
        if !attr_key.is_empty() {
            return eq_fold(cfg_key, &attr_key) && (cfg_base.is_empty() || eq_fold(cfg_base, &attr_base));
        }
        !attr_base.is_empty() && eq_fold(cfg_base, &attr_base)
    };
    if let Some(entry) = config_entry_for_auth_index(auth, entries)
        && (!validate_index_credentials || matches_credentials(entry))
    {
        return Some(entry);
    }
    entries.iter().find(|e| matches_credentials(e))
}

// ---- Exclusions, aliases, settings, prefixes ----

/// Go `oauthExcludedModels`: the global per-provider exclusion list; never applies to API keys.
pub(super) fn oauth_excluded_models(cfg: &Config, provider: &str, auth_kind: &str) -> Vec<String> {
    if auth_kind.trim().eq_ignore_ascii_case("apikey") {
        return Vec::new();
    }
    cfg.oauth_excluded_models
        .get(&provider.trim().to_lowercase())
        .cloned()
        .unwrap_or_default()
}

/// Case-insensitive wildcard match where `*` matches any substring (Go `matchWildcard`).
/// `pattern` and `value` must already be lowercased.
pub fn match_wildcard(pattern: &str, value: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if !pattern.contains('*') {
        return pattern == value;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut value = value;
    let prefix = parts[0];
    if !prefix.is_empty() {
        match value.strip_prefix(prefix) {
            Some(rest) => value = rest,
            None => return false,
        }
    }
    let suffix = parts[parts.len() - 1];
    if !suffix.is_empty() {
        match value.strip_suffix(suffix) {
            Some(rest) => value = rest,
            None => return false,
        }
    }
    for segment in &parts[1..parts.len() - 1] {
        if segment.is_empty() {
            continue;
        }
        match value.find(segment) {
            Some(idx) => value = &value[idx + segment.len()..],
            None => return false,
        }
    }
    true
}

/// Go `applyExcludedModels`: drops models whose lowercased id matches any exclusion pattern.
pub fn apply_excluded_models(models: Vec<ModelInfo>, excluded: &[String]) -> Vec<ModelInfo> {
    if models.is_empty() || excluded.is_empty() {
        return models;
    }
    let patterns: Vec<String> = excluded
        .iter()
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    if patterns.is_empty() {
        return models;
    }
    models
        .into_iter()
        .filter(|m| {
            let id = m.id.trim().to_lowercase();
            !patterns.iter().any(|p| match_wildcard(p, &id))
        })
        .collect()
}

/// Go `applyModelPrefixes`: adds `prefix/<id>` clones (and keeps the bare id unless
/// `force_model_prefix`), deduplicating by id.
pub fn apply_model_prefixes(models: Vec<ModelInfo>, prefix: &str, force_model_prefix: bool) -> Vec<ModelInfo> {
    let prefix = prefix.trim();
    if prefix.is_empty() || models.is_empty() {
        return models;
    }
    let mut out = Vec::with_capacity(models.len() * 2);
    let mut seen = HashSet::new();
    let mut add = |model: ModelInfo| {
        let id = model.id.trim().to_string();
        if id.is_empty() || !seen.insert(id) {
            return;
        }
        out.push(model);
    };
    for model in models {
        let base_id = model.id.trim().to_string();
        if base_id.is_empty() {
            continue;
        }
        let mut clone = model.clone();
        clone.id = format!("{prefix}/{base_id}");
        if clone.metadata_model_id.is_empty() {
            clone.metadata_model_id = base_id.clone();
        }
        if !force_model_prefix || prefix == base_id {
            add(model);
        }
        add(clone);
    }
    out
}

/// Go `OAuthModelAliasChannel`: the alias/settings channel for an OAuth auth. API-key auths and
/// `gemini` (which has no OAuth) have none; every other provider is its own channel.
pub fn oauth_model_alias_channel(provider: &str, auth_kind: &str) -> String {
    let provider = provider.trim().to_lowercase();
    let kind = match auth_kind.trim().to_lowercase().as_str() {
        "api_key" | "api-key" => "apikey".to_string(),
        other => other.to_string(),
    };
    if kind == "apikey" || provider == "gemini" {
        return String::new();
    }
    provider
}

/// Go `oauthModelAliasesForAuth`: per-auth aliases first, then the channel's global aliases
/// (deduplicated by alias, per-auth wins).
pub(super) fn oauth_model_aliases_for_auth(
    cfg: &Config,
    channel: &str,
    attributes: &BTreeMap<String, String>,
) -> Vec<OAuthModelAlias> {
    let per_auth = oauth_model_aliases_from_attributes(attributes);
    if cfg.oauth_model_alias.is_empty() {
        return per_auth;
    }
    let global = cfg.oauth_model_alias.get(channel).cloned().unwrap_or_default();
    if per_auth.is_empty() {
        return global;
    }
    if global.is_empty() {
        return per_auth;
    }
    let mut out = Vec::with_capacity(per_auth.len() + global.len());
    let mut seen = HashSet::new();
    for entry in per_auth.into_iter().chain(global) {
        let alias = entry.alias.trim().to_lowercase();
        if alias.is_empty() || !seen.insert(alias) {
            continue;
        }
        out.push(entry);
    }
    out
}

/// Go `applyOAuthModelAliasForAuth`.
pub fn apply_oauth_model_alias_for_auth(
    cfg: &Config,
    provider: &str,
    auth_kind: &str,
    attributes: &BTreeMap<String, String>,
    models: Vec<ModelInfo>,
) -> Vec<ModelInfo> {
    if models.is_empty() {
        return models;
    }
    let channel = oauth_model_alias_channel(provider, auth_kind);
    if channel.is_empty() {
        return models;
    }
    let aliases = oauth_model_aliases_for_auth(cfg, &channel, attributes);
    if aliases.is_empty() {
        return models;
    }
    apply_oauth_model_alias_entries(&aliases, models)
}

/// Go `rewriteModelInfoName`: rewrites a Gemini-style `models/<id>` name after an alias rename.
fn rewrite_model_info_name(name: &str, old_id: &str, new_id: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return name.to_string();
    }
    let (old_id, new_id) = (old_id.trim(), new_id.trim());
    if old_id.is_empty() || new_id.is_empty() || eq_fold(old_id, new_id) {
        return name.to_string();
    }
    if eq_fold(trimmed, old_id) {
        return new_id.to_string();
    }
    if let Some(prefix) = trimmed.strip_suffix(&format!("/{old_id}")) {
        return format!("{prefix}/{new_id}");
    }
    name.to_string()
}

/// Go `applyOAuthModelAliasEntries`: with `fork` the alias is listed next to the original model,
/// otherwise it replaces it.
pub fn apply_oauth_model_alias_entries(aliases: &[OAuthModelAlias], models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    struct Entry {
        alias: String,
        display_name: String,
        fork: bool,
    }
    let mut forward: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for a in aliases {
        let (name, alias) = (a.name.trim(), a.alias.trim());
        if name.is_empty() || alias.is_empty() || eq_fold(name, alias) {
            continue;
        }
        forward.entry(name.to_lowercase()).or_default().push(Entry {
            alias: alias.to_string(),
            display_name: a.display_name.trim().to_string(),
            fork: a.fork,
        });
    }
    if forward.is_empty() {
        return models;
    }

    let mut out = Vec::with_capacity(models.len());
    let mut seen = HashSet::new();
    for model in models {
        let id = model.id.trim().to_string();
        if id.is_empty() {
            continue;
        }
        let key = id.to_lowercase();
        let Some(entries) = forward.get(&key) else {
            if seen.insert(key) {
                out.push(model);
            }
            continue;
        };
        let keep_original = entries.iter().any(|e| e.fork);
        if keep_original && seen.insert(key.clone()) {
            out.push(model.clone());
        }
        let mut added_alias = false;
        for entry in entries {
            let mapped = entry.alias.trim();
            if mapped.is_empty() || eq_fold(mapped, &id) {
                continue;
            }
            if !seen.insert(mapped.to_lowercase()) {
                continue;
            }
            let mut clone = model.clone();
            clone.id = mapped.to_string();
            clone.metadata_model_id = if model.metadata_model_id.is_empty() {
                id.clone()
            } else {
                model.metadata_model_id.clone()
            };
            if !entry.display_name.is_empty() {
                clone.display_name = entry.display_name.clone();
            }
            if !clone.name.is_empty() {
                clone.name = rewrite_model_info_name(&clone.name, &id, mapped);
            }
            out.push(clone);
            added_alias = true;
        }
        if !keep_original && !added_alias && seen.insert(key) {
            out.push(model);
        }
    }
    out
}

/// Go `applyOAuthSettingsForAuth`: per-channel `max-context-length` overrides.
pub fn apply_oauth_settings_for_auth(
    cfg: &Config,
    provider: &str,
    auth_kind: &str,
    models: Vec<ModelInfo>,
) -> Vec<ModelInfo> {
    if models.is_empty() {
        return models;
    }
    let channel = oauth_model_alias_channel(provider, auth_kind);
    if channel.is_empty() {
        return models;
    }
    let Some(settings) = cfg.oauth_settings.get(&channel).filter(|s| !s.is_empty()) else {
        return models;
    };
    apply_oauth_setting_entries(settings, models)
}

fn apply_oauth_setting_entries(settings: &[OAuthModelSetting], models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    models
        .into_iter()
        .map(|mut model| {
            if let Some(setting) =
                resolve_oauth_model_setting(settings, &model.id, &model.metadata_model_id, &model.name)
                && setting.max_context_length > 0
            {
                model.context_length = setting.max_context_length;
                model.max_context_length = setting.max_context_length;
            }
            model
        })
        .collect()
}

// ---- Config models[] -> ModelInfo ----

/// Common view over the per-provider `models[]` entry types (Go: `modelEntry` and friends).
trait ConfigModel {
    fn name(&self) -> &str;
    fn alias(&self) -> &str;
    fn display_name(&self) -> &str;
    fn thinking(&self) -> Option<ThinkingSupport>;
    fn max_context_length(&self) -> i64 {
        0
    }
    fn is_compat(&self) -> bool {
        false
    }
}

fn to_registry_thinking(t: &cpa_config::ThinkingSupport) -> ThinkingSupport {
    ThinkingSupport {
        min: t.min,
        max: t.max,
        zero_allowed: t.zero_allowed,
        dynamic_allowed: t.dynamic_allowed,
        levels: t.levels.clone(),
    }
}

macro_rules! config_model {
    ($ty:ty) => {
        impl ConfigModel for $ty {
            fn name(&self) -> &str { &self.name }
            fn alias(&self) -> &str { &self.alias }
            fn display_name(&self) -> &str { &self.display_name }
            fn thinking(&self) -> Option<ThinkingSupport> { self.thinking.as_ref().map(to_registry_thinking) }
            fn max_context_length(&self) -> i64 { self.max_context_length }
            fn is_compat(&self) -> bool { self.is_compat }
        }
    };
}
config_model!(ClaudeModel);
config_model!(CodexModel);
config_model!(GeminiModel);
config_model!(OpenAiCompatibilityModel);

impl ConfigModel for VertexCompatModel {
    fn name(&self) -> &str {
        &self.name
    }
    fn alias(&self) -> &str {
        &self.alias
    }
    fn display_name(&self) -> &str {
        &self.display_name
    }
    fn thinking(&self) -> Option<ThinkingSupport> {
        self.thinking.as_ref().map(to_registry_thinking)
    }
}

/// Go `buildConfiguredModelInfo`.
fn build_configured_model_info<M: ConfigModel>(
    model: &M,
    owned_by: &str,
    model_type: &str,
    created: i64,
    fallback_display_name: &str,
    user_defined: bool,
) -> Option<ModelInfo> {
    let name = model.name().trim();
    let mut alias = model.alias().trim();
    if alias.is_empty() {
        alias = name;
    }
    if alias.is_empty() {
        return None;
    }
    let mut display_name = model.display_name().trim();
    if display_name.is_empty() {
        display_name = fallback_display_name;
    }
    if display_name.is_empty() {
        display_name = alias;
    }
    let max_context = model.max_context_length();
    Some(ModelInfo {
        id: alias.to_string(),
        metadata_model_id: if name.is_empty() { alias } else { name }.to_string(),
        object: "model".into(),
        created,
        owned_by: owned_by.to_string(),
        r#type: model_type.to_string(),
        display_name: display_name.to_string(),
        user_defined,
        context_length: max_context.max(0),
        max_context_length: max_context.max(0),
        is_compat: model.is_compat(),
        ..Default::default()
    })
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Go `modelconfig.NormalizeThinkingSupport`: lowercases and dedupes levels; `none` implies
/// `zero_allowed` and `auto` implies `dynamic_allowed`.
pub fn normalize_thinking_support(raw: Option<ThinkingSupport>) -> Option<ThinkingSupport> {
    let raw = raw?;
    let mut normalized = ThinkingSupport { levels: Vec::new(), ..raw.clone() };
    let mut seen = HashSet::new();
    for value in &raw.levels {
        let level = value.trim().to_lowercase();
        if level.is_empty() {
            continue;
        }
        match level.as_str() {
            "none" => normalized.zero_allowed = true,
            "auto" => normalized.dynamic_allowed = true,
            _ => {}
        }
        if seen.insert(level.clone()) {
            normalized.levels.push(level);
        }
    }
    Some(normalized)
}

/// Go `modelconfig.ResolveModelInfo(...).Thinking`: explicit configuration wins, otherwise the
/// static capability of the suffix-free upstream name.
fn resolve_model_thinking(name: &str, explicit: Option<ThinkingSupport>) -> Option<ThinkingSupport> {
    if let Some(explicit) = explicit {
        return normalize_thinking_support(Some(explicit));
    }
    lookup_static_model_info(parse_suffix(name.trim()).model_name.trim()).and_then(|info| info.thinking)
}

/// Go `buildConfigModels`: key-entry models, deduplicated by lowercase alias.
fn build_config_models<M: ConfigModel>(
    models: &[M],
    owned_by: &str,
    model_type: &str,
    metadata_channel: &str,
) -> Vec<ModelInfo> {
    let now = now_unix();
    let mut out = Vec::with_capacity(models.len());
    let mut seen = HashSet::new();
    for model in models {
        let name = model.name().trim();
        let Some(mut info) = build_configured_model_info(model, owned_by, model_type, now, name, true) else {
            continue;
        };
        if !seen.insert(info.id.to_lowercase()) {
            continue;
        }
        let explicit = model.thinking();
        info.explicit_thinking = explicit.is_some();
        if let Some(thinking) = resolve_model_thinking(name, explicit) {
            info.thinking = Some(thinking);
        }
        if let Some(caps) = lookup_static_model_info_by_channel(name, metadata_channel).and_then(|s| s.native_capabilities)
        {
            info.native_capabilities = Some(caps);
        }
        out.push(info);
    }
    out
}

/// Go `buildCodexConfigModels`: no configured models means the codex-pro catalog without
/// configuration updates; otherwise display names and `support-configuration-update` come from
/// the config.
fn build_codex_config_models(entry: &CodexKey) -> Vec<ModelInfo> {
    if entry.models.is_empty() {
        let mut models = get_codex_pro_models();
        for model in &mut models {
            model.support_configuration_update = false;
        }
        return models;
    }
    let mut models = build_config_models(&entry.models, "openai", "openai", "codex");
    let mut display_names = BTreeMap::new();
    let mut config_updates = BTreeMap::new();
    for model in &entry.models {
        let mut alias = model.alias.trim();
        if alias.is_empty() {
            alias = model.name.trim();
        }
        if alias.is_empty() {
            continue;
        }
        let key = alias.to_lowercase();
        if config_updates.contains_key(&key) {
            continue;
        }
        config_updates.insert(key.clone(), model.support_configuration_update);
        let display = model.display_name.trim();
        if !display.is_empty() {
            display_names.insert(key, display.to_string());
        }
    }
    for model in &mut models {
        let key = model.id.to_lowercase();
        if let Some(display) = display_names.get(&key) {
            model.display_name = display.clone();
        }
        model.support_configuration_update = config_updates.get(&key).copied().unwrap_or(false);
    }
    models
}

fn normalize_compat_modalities(raw: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    raw.iter()
        .map(|m| m.trim().to_lowercase())
        .filter(|m| !m.is_empty() && seen.insert(m.clone()))
        .collect()
}

/// Go `buildOpenAICompatibilityConfigModels`: every configured model (no dedupe: pools register
/// repeated aliases), thinking levels default to low/medium/high for non-image models.
pub fn build_openai_compat_config_models(compat: &OpenAiCompatibility) -> Vec<ModelInfo> {
    let now = now_unix();
    let mut models = Vec::with_capacity(compat.models.len());
    for model in &compat.models {
        let model_type = if model.image { OPENAI_IMAGE_MODEL_TYPE } else { "openai-compatibility" };
        let Some(mut info) =
            build_configured_model_info(model, &compat.name, model_type, now, model.alias.trim(), false)
        else {
            continue;
        };
        let mut thinking = model.thinking();
        if thinking.is_none() && !model.image {
            thinking = Some(ThinkingSupport {
                levels: vec!["low".into(), "medium".into(), "high".into()],
                ..Default::default()
            });
        }
        info.explicit_thinking = model.thinking.is_some();
        info.explicit_input_modalities = !model.input_modalities.is_empty();
        info.thinking = normalize_thinking_support(thinking);
        info.supported_input_modalities = normalize_compat_modalities(&model.input_modalities);
        info.supported_output_modalities = normalize_compat_modalities(&model.output_modalities);
        models.push(info);
    }
    models
}
