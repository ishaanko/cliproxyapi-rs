//! Auth synthesis (Go: internal/watcher/synthesizer). Turns config API-key entries and auth-dir
//! JSON files into [`cpa_auth::Auth`] records with the ids, attributes and metadata the Go app
//! derives, so management indexes, config indexes and routing hashes stay stable across ports.
//!
//! - [`synthesize_config_auths`]: one auth per key entry (`ConfigSynthesizer`).
//! - [`synthesize_auth_file`] / [`synthesize_auth_dir`]: one auth per credential file
//!   (`FileSynthesizer`; the plugin auth parser hook is not ported).
//! - [`snapshot_core_auths`]: config auths followed by file auths (`snapshotCoreAuths`).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use chrono::{DateTime, Utc};
use cpa_auth::credmeta::{
    Metadata, apply_auth_priority_metadata, apply_auth_weight_metadata, apply_custom_headers_from_metadata,
    normalize_credential_metadata, validate_metadata_weight,
};
use cpa_auth::jwt::{DEFAULT_PLAN_TYPE, parse_codex_id_token};
use cpa_auth::kimi::{normalize_kimi_domain, resolve_kimi_api_base_url, resolve_kimi_domain_from_auth};
use cpa_auth::{Auth, Status};
use cpa_config::diff::{
    compute_claude_models_hash, compute_codex_models_hash, compute_excluded_models_hash, compute_gemini_models_hash,
    compute_openai_compat_models_hash, compute_vertex_compat_models_hash,
};
use cpa_config::{Config, OAuthModelAlias, RequestScopedErrorRule, clean_path, format_sorted_headers};
use cpa_core::util::openai_compatible_provider_key;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const ATTRIBUTE_CODEX_ALPHA_SEARCH: &str = "codex_alpha_search";
pub const ATTRIBUTE_CODEX_DISABLE_CLOAKING: &str = "codex_disable_cloaking";
pub const ATTRIBUTE_CONFIG_INDEX: &str = "config_index";
/// Attribute holding the per-auth OAuth model aliases as a JSON array.
pub const ATTRIBUTE_MODEL_ALIASES: &str = "model_aliases";

#[derive(Debug, thiserror::Error)]
pub enum SynthError {
    #[error("synthesize config API key auths: {0}")]
    Config(String),
    #[error("{0}")]
    Weight(String),
}

/// Generates stable, deterministic ids for config auths (Go: `StableIDGenerator`). Identical
/// tuples get `-1`, `-2` ... suffixes. Not shared between synthesis passes: create one per pass.
#[derive(Debug, Default)]
pub struct StableIdGenerator {
    counters: HashMap<String, usize>,
}

impl StableIdGenerator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `(kind:hash, hash)` where `hash` is the first 12 hex chars of
    /// `sha256(kind 0x00 part 0x00 part ...)` over the trimmed parts.
    pub fn next(&mut self, kind: &str, parts: &[&str]) -> (String, String) {
        let mut hasher = Sha256::new();
        hasher.update(kind.as_bytes());
        for part in parts {
            hasher.update([0u8]);
            hasher.update(part.trim().as_bytes());
        }
        let digest = hex::encode(hasher.finalize());
        let mut short = digest[..12].to_string();
        let counter = self.counters.entry(format!("{kind}:{short}")).or_insert(0);
        let index = *counter;
        *counter += 1;
        if index > 0 {
            short = format!("{short}-{index}");
        }
        (format!("{kind}:{short}"), short)
    }
}

/// Fields every synthesized auth sets (the rest keeps its `Auth::default()` value; `Auth` has
/// private runtime fields so struct-update syntax is unavailable).
struct AuthParts {
    id: String,
    provider: String,
    label: String,
    prefix: String,
    status: Status,
    disabled: bool,
    proxy_url: String,
    file_name: String,
    attributes: BTreeMap<String, String>,
    metadata: Metadata,
    now: DateTime<Utc>,
}

impl AuthParts {
    fn build(self) -> Auth {
        let mut auth = Auth::default();
        auth.id = self.id;
        auth.provider = self.provider;
        auth.label = self.label;
        auth.prefix = self.prefix;
        auth.status = self.status;
        auth.disabled = self.disabled;
        auth.proxy_url = self.proxy_url;
        auth.file_name = self.file_name;
        auth.attributes = self.attributes;
        auth.metadata = self.metadata;
        auth.created_at = Some(self.now);
        auth.updated_at = Some(self.now);
        auth
    }
}

/// Inputs shared by every synthesis call.
pub struct SynthesisContext<'a> {
    pub config: &'a Config,
    /// Resolved auth directory (file ids are paths relative to it).
    pub auth_dir: &'a str,
    pub now: DateTime<Utc>,
}

/// Go `ApplyAuthExcludedModelsMeta`: merges per-entry exclusions (and, for OAuth, the global
/// `oauth-excluded-models` of the provider) into `excluded_models` / `excluded_models_hash`, and
/// stamps `auth_kind`.
pub fn apply_auth_excluded_models_meta(auth: &mut Auth, cfg: &Config, per_key: &[String], auth_kind: &str) {
    let kind = auth_kind.trim().to_lowercase();
    let mut combined: Vec<String> = Vec::new();
    let mut add = |list: &[String]| {
        for entry in list {
            let trimmed = entry.trim();
            if !trimmed.is_empty() {
                let key = trimmed.to_lowercase();
                if !combined.contains(&key) {
                    combined.push(key);
                }
            }
        }
    };
    add(per_key);
    if kind != "apikey" {
        let provider = auth.provider.trim().to_lowercase();
        if let Some(global) = cfg.oauth_excluded_models.get(&provider) {
            add(global);
        }
    }
    combined.sort();
    let hash = compute_excluded_models_hash(&combined);
    if !hash.is_empty() {
        auth.attributes.insert("excluded_models_hash".into(), hash);
    }
    if !combined.is_empty() {
        auth.attributes.insert("excluded_models".into(), combined.join(","));
    }
    if !auth_kind.is_empty() {
        auth.attributes.insert("auth_kind".into(), auth_kind.to_string());
    }
}

fn add_weight_to_attrs(weight: Option<i64>, attrs: &mut BTreeMap<String, String>) {
    if let Some(w) = weight {
        attrs.insert("weight".into(), w.max(0).to_string());
    }
}

fn add_config_headers_to_attrs(headers: &BTreeMap<String, String>, attrs: &mut BTreeMap<String, String>) {
    for (k, v) in headers {
        let (key, val) = (k.trim(), v.trim());
        if key.is_empty() || val.is_empty() {
            continue;
        }
        attrs.insert(format!("header:{key}"), val.to_string());
    }
}

/// Per-credential overrides every config entry can carry, written to `metadata`.
fn config_metadata(
    disable_cooling: Option<bool>,
    request_retry: Option<i64>,
    scoped_errors: &[RequestScopedErrorRule],
) -> Metadata {
    let mut metadata = Metadata::new();
    if let Some(v) = disable_cooling {
        metadata.insert("disable_cooling".into(), Value::Bool(v));
    }
    if let Some(v) = request_retry.filter(|v| *v >= 0) {
        metadata.insert("request_retry".into(), Value::from(v));
    }
    if !scoped_errors.is_empty() {
        metadata.insert(
            "request_scoped_errors".into(),
            serde_json::to_value(scoped_errors).unwrap_or(Value::Null),
        );
    }
    metadata
}

/// Fields common to the gemini, claude, codex, xai and meta key entries.
struct KeyEntry<'a> {
    id_kind: &'a str,
    source_name: &'a str,
    label: &'a str,
    provider: &'a str,
    index: usize,
    key: &'a str,
    base: &'a str,
    prefix: &'a str,
    proxy_url: &'a str,
    headers: &'a BTreeMap<String, String>,
    priority: i64,
    weight: Option<i64>,
    models_hash: String,
    metadata: Metadata,
    excluded: &'a [String],
}

fn key_auth(ctx: &SynthesisContext<'_>, ids: &mut StableIdGenerator, e: KeyEntry<'_>) -> Auth {
    let (id, token) = ids.next(
        e.id_kind,
        &[e.key, e.base, e.proxy_url, e.prefix, &format_sorted_headers(e.headers)],
    );
    let mut attrs = BTreeMap::new();
    attrs.insert("source".to_string(), format!("config:{}[{token}]", e.source_name));
    attrs.insert(ATTRIBUTE_CONFIG_INDEX.to_string(), e.index.to_string());
    if !e.key.is_empty() {
        attrs.insert("api_key".into(), e.key.to_string());
    }
    if e.priority != 0 {
        attrs.insert("priority".into(), e.priority.to_string());
    }
    add_weight_to_attrs(e.weight, &mut attrs);
    if !e.base.is_empty() {
        attrs.insert("base_url".into(), e.base.to_string());
    }
    if !e.models_hash.is_empty() {
        attrs.insert("models_hash".into(), e.models_hash);
    }
    add_config_headers_to_attrs(e.headers, &mut attrs);
    let mut auth = AuthParts {
        id,
        provider: e.provider.to_string(),
        label: e.label.to_string(),
        prefix: e.prefix.to_string(),
        status: Status::Active,
        disabled: false,
        proxy_url: e.proxy_url.to_string(),
        file_name: String::new(),
        attributes: attrs,
        metadata: e.metadata,
        now: ctx.now,
    }
    .build();
    apply_auth_excluded_models_meta(&mut auth, ctx.config, e.excluded, "apikey");
    auth
}

/// Go `ConfigSynthesizer.Synthesize`: one auth per key entry in the order gemini, interactions,
/// claude, codex, xai, meta, openai-compatibility, vertex. Entries with neither key nor base URL
/// are skipped. Errors when a configured credential weight is invalid.
pub fn synthesize_config_auths(ctx: &SynthesisContext<'_>) -> Result<Vec<Auth>, SynthError> {
    let cfg = ctx.config;
    cfg.validate_credential_weights()
        .map_err(|e| SynthError::Config(e.to_string()))?;
    let mut ids = StableIdGenerator::new();
    let mut out = Vec::with_capacity(32);

    for (entries, id_kind, source, label, provider) in [
        (&cfg.gemini_key, "gemini:apikey", "gemini", "gemini-apikey", "gemini"),
        (
            &cfg.interactions_key,
            "gemini-interactions:apikey",
            "interactions",
            "interactions-apikey",
            "gemini-interactions",
        ),
    ] {
        for (i, e) in entries.iter().enumerate() {
            let (key, base) = (e.api_key.trim(), e.base_url.trim());
            if key.is_empty() && base.is_empty() {
                continue;
            }
            out.push(key_auth(
                ctx,
                &mut ids,
                KeyEntry {
                    id_kind,
                    source_name: source,
                    label,
                    provider,
                    index: i,
                    key,
                    base,
                    prefix: e.prefix.trim(),
                    proxy_url: e.proxy_url.trim(),
                    headers: &e.headers,
                    priority: e.priority,
                    weight: e.weight,
                    models_hash: compute_gemini_models_hash(&e.models),
                    metadata: config_metadata(e.disable_cooling, e.request_retry, &e.request_scoped_errors),
                    excluded: &e.excluded_models,
                },
            ));
        }
    }

    for (i, e) in cfg.claude_key.iter().enumerate() {
        let (key, base) = (e.api_key.trim(), e.base_url.trim());
        if key.is_empty() && base.is_empty() {
            continue;
        }
        let mut auth = key_auth(
            ctx,
            &mut ids,
            KeyEntry {
                id_kind: "claude:apikey",
                source_name: "claude",
                label: "claude-apikey",
                provider: "claude",
                index: i,
                key,
                base,
                prefix: e.prefix.trim(),
                proxy_url: e.proxy_url.trim(),
                headers: &e.headers,
                priority: e.priority,
                weight: e.weight,
                models_hash: compute_claude_models_hash(&e.models),
                metadata: config_metadata(e.disable_cooling, e.request_retry, &e.request_scoped_errors),
                excluded: &e.excluded_models,
            },
        );
        if e.rebuild_mid_system_message {
            auth.attributes.insert("rebuild_mid_system_message".into(), "true".into());
        }
        let profile = e.fingerprint_profile.trim().to_lowercase();
        if !profile.is_empty() {
            auth.attributes.insert("fingerprint_profile".into(), profile);
        }
        out.push(auth);
    }

    for (entries, provider) in [(&cfg.codex_key, "codex"), (&cfg.xai_key, "xai"), (&cfg.meta_key, "meta")] {
        for (i, e) in entries.iter().enumerate() {
            let (key, base) = (e.api_key.trim(), e.base_url.trim());
            if key.is_empty() && base.is_empty() {
                continue;
            }
            let id_kind = format!("{provider}:apikey");
            let label = format!("{provider}-apikey");
            let mut auth = key_auth(
                ctx,
                &mut ids,
                KeyEntry {
                    id_kind: &id_kind,
                    source_name: provider,
                    label: &label,
                    provider,
                    index: i,
                    key,
                    base,
                    prefix: e.prefix.trim(),
                    proxy_url: e.proxy_url.trim(),
                    headers: &e.headers,
                    priority: e.priority,
                    weight: e.weight,
                    models_hash: compute_codex_models_hash(&e.models),
                    metadata: config_metadata(e.disable_cooling, e.request_retry, &e.request_scoped_errors),
                    excluded: &e.excluded_models,
                },
            );
            if e.websockets {
                auth.attributes.insert("websockets".into(), "true".into());
            }
            if provider == "codex" && e.alpha_search {
                auth.attributes.insert(ATTRIBUTE_CODEX_ALPHA_SEARCH.into(), "true".into());
            }
            if provider == "codex"
                && let Some(v) = e.disable_codex_cloaking
            {
                auth.attributes.insert(ATTRIBUTE_CODEX_DISABLE_CLOAKING.into(), v.to_string());
            }
            out.push(auth);
        }
    }

    synthesize_openai_compat(ctx, &mut ids, &mut out);
    synthesize_vertex_compat(ctx, &mut ids, &mut out);
    Ok(out)
}

/// OpenAI-compatibility providers: one auth per `api-key-entries[]` item, or one keyless auth.
/// These never carry `auth_kind` or excluded-model attributes (as in Go).
fn synthesize_openai_compat(ctx: &SynthesisContext<'_>, ids: &mut StableIdGenerator, out: &mut Vec<Auth>) {
    for (i, compat) in ctx.config.openai_compatibility.iter().enumerate() {
        if compat.disabled {
            continue;
        }
        let prefix = compat.prefix.trim();
        let mut provider_name = compat.name.trim().to_lowercase();
        if provider_name.is_empty() {
            provider_name = "openai-compatibility".into();
        }
        let provider_key = openai_compatible_provider_key(&provider_name);
        let base = compat.base_url.trim();
        let id_kind = format!("openai-compatibility:{provider_name}");

        let build = |ids: &mut StableIdGenerator, key: Option<&str>, proxy_url: &str, weight: Option<i64>| {
            let (id, token) = match key {
                Some(key) => ids.next(&id_kind, &[key, base, proxy_url]),
                None => ids.next(&id_kind, &[base]),
            };
            let mut attrs = BTreeMap::new();
            attrs.insert("source".to_string(), format!("config:{provider_name}[{token}]"));
            attrs.insert("base_url".into(), base.to_string());
            attrs.insert("compat_name".into(), compat.name.clone());
            attrs.insert("provider_key".into(), provider_key.clone());
            attrs.insert(ATTRIBUTE_CONFIG_INDEX.into(), i.to_string());
            if compat.priority != 0 {
                attrs.insert("priority".into(), compat.priority.to_string());
            }
            add_weight_to_attrs(weight, &mut attrs);
            if let Some(key) = key.filter(|k| !k.is_empty()) {
                attrs.insert("api_key".into(), key.to_string());
            }
            let hash = compute_openai_compat_models_hash(&compat.models);
            if !hash.is_empty() {
                attrs.insert("models_hash".into(), hash);
            }
            add_config_headers_to_attrs(&compat.headers, &mut attrs);
            AuthParts {
                id,
                provider: provider_key.clone(),
                label: compat.name.clone(),
                prefix: prefix.to_string(),
                status: Status::Active,
                disabled: false,
                proxy_url: proxy_url.to_string(),
                file_name: String::new(),
                attributes: attrs,
                metadata: config_metadata(compat.disable_cooling, compat.request_retry, &compat.request_scoped_errors),
                now: ctx.now,
            }
            .build()
        };

        for entry in &compat.api_key_entries {
            out.push(build(ids, Some(entry.api_key.trim()), entry.proxy_url.trim(), entry.weight));
        }
        if compat.api_key_entries.is_empty() {
            out.push(build(ids, None, "", None));
        }
    }
}

/// Vertex-compatible API keys (`vertex-api-key`): no prefix in the id, no request-scoped rules.
fn synthesize_vertex_compat(ctx: &SynthesisContext<'_>, ids: &mut StableIdGenerator, out: &mut Vec<Auth>) {
    for (i, e) in ctx.config.vertex_compat_api_key.iter().enumerate() {
        let (key, base) = (e.api_key.trim(), e.base_url.trim());
        let (id, token) = ids.next("vertex:apikey", &[key, base, e.proxy_url.trim()]);
        let mut attrs = BTreeMap::new();
        attrs.insert("source".to_string(), format!("config:vertex-apikey[{token}]"));
        attrs.insert("base_url".into(), base.to_string());
        attrs.insert("provider_key".into(), "vertex".into());
        attrs.insert(ATTRIBUTE_CONFIG_INDEX.into(), i.to_string());
        if e.priority != 0 {
            attrs.insert("priority".into(), e.priority.to_string());
        }
        add_weight_to_attrs(e.weight, &mut attrs);
        if !key.is_empty() {
            attrs.insert("api_key".into(), key.to_string());
        }
        let hash = compute_vertex_compat_models_hash(&e.models);
        if !hash.is_empty() {
            attrs.insert("models_hash".into(), hash);
        }
        add_config_headers_to_attrs(&e.headers, &mut attrs);
        let mut auth = AuthParts {
            id,
            provider: "vertex".into(),
            label: "vertex-apikey".into(),
            prefix: e.prefix.trim().to_string(),
            status: Status::Active,
            disabled: false,
            proxy_url: e.proxy_url.trim().to_string(),
            file_name: String::new(),
            attributes: attrs,
            metadata: config_metadata(e.disable_cooling, e.request_retry, &[]),
            now: ctx.now,
        }
        .build();
        apply_auth_excluded_models_meta(&mut auth, ctx.config, &e.excluded_models, "apikey");
        out.push(auth);
    }
}

// ---- Auth files ----

/// Per-account `excluded_models` (canonical) or `excluded-models` (legacy) of a credential file.
fn extract_excluded_models(metadata: &Metadata) -> Vec<String> {
    let Some(Value::Array(items)) = metadata.get("excluded_models").or_else(|| metadata.get("excluded-models"))
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Go `sanitizeOAuthModelAliases`: trim, drop empty/self-referential entries, unique per alias.
pub fn sanitize_oauth_model_aliases(aliases: Vec<OAuthModelAlias>) -> Vec<OAuthModelAlias> {
    if aliases.is_empty() {
        return aliases;
    }
    let mut cfg = Config::default();
    cfg.oauth_model_alias.insert("auth".into(), aliases);
    cfg.sanitize_oauth_model_alias();
    cfg.oauth_model_alias.remove("auth").unwrap_or_default()
}

/// Per-account `model_aliases` (canonical) or `model-aliases` of a credential file, sanitized.
fn extract_oauth_model_aliases(metadata: &Metadata) -> Vec<OAuthModelAlias> {
    let Some(raw) = metadata.get("model_aliases").or_else(|| metadata.get("model-aliases")) else {
        return Vec::new();
    };
    match serde_json::from_value::<Vec<OAuthModelAlias>>(raw.clone()) {
        Ok(aliases) => sanitize_oauth_model_aliases(aliases),
        Err(_) => Vec::new(),
    }
}

/// Go `SetOAuthModelAliasesAttribute`: stores sanitized per-auth aliases as JSON in
/// `attributes["model_aliases"]` (nothing when empty).
pub fn set_oauth_model_aliases_attribute(auth: &mut Auth, aliases: Vec<OAuthModelAlias>) {
    let aliases = sanitize_oauth_model_aliases(aliases);
    if aliases.is_empty() {
        return;
    }
    if let Ok(data) = serde_json::to_string(&aliases) {
        auth.attributes.insert(ATTRIBUTE_MODEL_ALIASES.into(), data);
    }
}

/// Go `OAuthModelAliasesFromAttributes`: the sanitized per-auth aliases stored on an auth.
pub fn oauth_model_aliases_from_attributes(attributes: &BTreeMap<String, String>) -> Vec<OAuthModelAlias> {
    let Some(raw) = attributes.get(ATTRIBUTE_MODEL_ALIASES).map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    match serde_json::from_str::<Vec<OAuthModelAlias>>(raw) {
        Ok(aliases) => sanitize_oauth_model_aliases(aliases),
        Err(_) => Vec::new(),
    }
}

fn fingerprint_profile_from_metadata(metadata: &Metadata) -> String {
    for key in ["fingerprint_profile", "fingerprint-profile"] {
        if let Some(raw) = metadata.get(key).and_then(Value::as_str) {
            let profile = raw.trim().to_lowercase();
            if !profile.is_empty() {
                return profile;
            }
        }
    }
    String::new()
}

/// Id of a credential file: its path relative to `auth_dir`, or the full path when it is outside.
pub fn file_auth_id(auth_dir: &str, full_path: &str) -> String {
    if auth_dir.trim().is_empty() {
        return full_path.to_string();
    }
    let base = clean_path(auth_dir);
    let full = clean_path(full_path);
    match Path::new(&full).strip_prefix(&base) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
        _ => full_path.to_string(),
    }
}

/// Go `SynthesizeAuthFile`: the auth for one credential file payload, `Ok(None)` for files that
/// yield nothing (empty, not a JSON object, no `type`, legacy `gemini`/`gemini-cli` files).
/// Errors for an invalid `weight`, in which case the file must be skipped.
pub fn synthesize_auth_file(
    ctx: &SynthesisContext<'_>,
    full_path: &str,
    data: &[u8],
) -> Result<Option<Auth>, SynthError> {
    if data.is_empty() {
        return Ok(None);
    }
    let Ok(Value::Object(mut metadata)) = serde_json::from_slice::<Value>(data) else {
        return Ok(None);
    };
    let base_name = Path::new(full_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    normalize_credential_metadata(&mut metadata);
    validate_metadata_weight(&metadata)
        .map_err(|e| SynthError::Weight(format!("invalid weight in {base_name}: {e}")))?;
    let provider = metadata
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if provider.is_empty() || provider == "gemini" || provider == "gemini-cli" {
        return Ok(None);
    }
    let label = match metadata.get("email").and_then(Value::as_str) {
        Some(email) if !email.is_empty() => email.to_string(),
        _ => provider.clone(),
    };
    let proxy_url = metadata
        .get("proxy_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let prefix = metadata
        .get("prefix")
        .and_then(Value::as_str)
        .map(|p| p.trim().trim_matches('/'))
        .filter(|p| !p.is_empty() && !p.contains('/'))
        .unwrap_or("")
        .to_string();
    let disabled = metadata.get("disabled").and_then(Value::as_bool).unwrap_or(false);
    let per_account_excluded = extract_excluded_models(&metadata);
    let per_account_aliases = extract_oauth_model_aliases(&metadata);

    let mut attributes = BTreeMap::new();
    attributes.insert("source".to_string(), full_path.to_string());
    attributes.insert("path".to_string(), full_path.to_string());
    attributes.insert("source_backend".to_string(), "file".to_string());
    let mut auth = AuthParts {
        id: file_auth_id(ctx.auth_dir, full_path),
        file_name: base_name.clone(),
        provider: provider.clone(),
        label,
        prefix,
        status: if disabled { Status::Disabled } else { Status::Active },
        disabled,
        attributes,
        proxy_url,
        metadata: metadata.clone(),
        now: ctx.now,
    }
    .build();

    apply_auth_priority_metadata(&mut auth, &metadata);
    apply_auth_weight_metadata(&mut auth, &metadata)
        .map_err(|e| SynthError::Weight(format!("invalid auth weight in {base_name}: {e}")))?;
    if let Some(note) = metadata
        .get("note")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        auth.attributes.insert("note".into(), note.to_string());
    }
    apply_custom_headers_from_metadata(&mut auth);
    set_oauth_model_aliases_attribute(&mut auth, per_account_aliases);
    apply_auth_excluded_models_meta(&mut auth, ctx.config, &per_account_excluded, "oauth");
    let profile = fingerprint_profile_from_metadata(&metadata);
    if !profile.is_empty() {
        auth.attributes.insert("fingerprint_profile".into(), profile);
    }

    if matches!(provider.as_str(), "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com") {
        for key in ["base_url", "domain"] {
            if let Some(v) = metadata
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
            {
                auth.attributes.insert(key.into(), v.to_string());
            }
        }
        let resolved_domain = resolve_kimi_domain_from_auth(&auth);
        let configured = auth.attributes.get("domain").cloned().unwrap_or_default();
        let domain = if configured.is_empty() {
            resolved_domain
        } else {
            normalize_kimi_domain(&configured)
        };
        auth.attributes.insert("domain".into(), domain.to_string());
        if auth.attributes.get("base_url").is_none_or(String::is_empty) {
            auth.attributes
                .insert("base_url".into(), resolve_kimi_api_base_url(resolved_domain).to_string());
        }
    }

    if provider == "codex" {
        let non_empty = |key: &str| {
            metadata
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        if let Some(plan) = non_empty("plan_type") {
            auth.attributes.insert("plan_type".into(), plan.to_string());
        } else if let Some(token) = non_empty("id_token") {
            let plan = match parse_codex_id_token(token) {
                Ok(claims) => claims.plan_type(),
                Err(_) => DEFAULT_PLAN_TYPE.to_string(),
            };
            auth.attributes.insert("plan_type".into(), plan);
        }
    }
    Ok(Some(auth))
}

/// Go `FileSynthesizer.Synthesize`: every direct `*.json` file of `ctx.auth_dir` in name order
/// (as `os.ReadDir`). Unreadable, empty or invalid files are skipped; a missing directory yields
/// nothing.
pub fn synthesize_auth_dir(ctx: &SynthesisContext<'_>) -> Vec<Auth> {
    if ctx.auth_dir.is_empty() {
        return Vec::new();
    }
    let Ok(read_dir) = fs::read_dir(ctx.auth_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read_dir
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.to_lowercase().ends_with(".json"))
        .collect();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let full = Path::new(ctx.auth_dir).join(&name);
        let Ok(data) = fs::read(&full) else { continue };
        if data.is_empty() {
            continue;
        }
        match synthesize_auth_file(ctx, &full.to_string_lossy(), &data) {
            Ok(Some(auth)) => out.push(auth),
            Ok(None) => {}
            Err(err) => tracing::warn!("skipping auth file {name}: {err}"),
        }
    }
    out
}

/// Go `snapshotCoreAuths`: config-synthesized auths followed by file auths. A config synthesis
/// error (invalid weights) drops only the config half, like Go.
pub fn snapshot_core_auths(ctx: &SynthesisContext<'_>) -> Vec<Auth> {
    let mut out = synthesize_config_auths(ctx).unwrap_or_else(|err| {
        tracing::warn!("{err}");
        Vec::new()
    });
    out.extend(synthesize_auth_dir(ctx));
    out
}
