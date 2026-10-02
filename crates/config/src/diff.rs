//! Config change description for hot reload (port of `internal/watcher/diff` and the model hash
//! helpers of `internal/modelconfig`).
//!
//! Everything here is a pure function over two [`Config`] snapshots. Secrets are never printed:
//! only structural or non-sensitive fields are surfaced. Only the OAuth excluded-model diff
//! ([`diff_oauth_excluded_model_changes`]) affects behaviour (it names the providers whose
//! registered models must be rebuilt); the rest is for logging.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;

use sha2::{Digest, Sha256};

use crate::types::*;

/// Count and content hash of a normalised list, used to detect changes without printing content.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelsSummary {
    pub hash: String,
    pub count: usize,
}

fn sha256_hex(data: &str) -> String {
    hex::encode(Sha256::digest(data.as_bytes()))
}

// ---------------------------------------------------------------------------------------------
// Hashing helpers
// ---------------------------------------------------------------------------------------------

/// JSON of a thinking capability exactly as Go's `json.Marshal(*ThinkingSupport)` writes it
/// (`null` for none; json tags use `zero_allowed` / `dynamic_allowed`).
fn thinking_hash_suffix(support: &Option<ThinkingSupport>) -> String {
    let json = match support {
        None => "null".to_string(),
        Some(t) => {
            let mut parts = Vec::new();
            if t.min != 0 {
                parts.push(format!("\"min\":{}", t.min));
            }
            if t.max != 0 {
                parts.push(format!("\"max\":{}", t.max));
            }
            if t.zero_allowed {
                parts.push("\"zero_allowed\":true".to_string());
            }
            if t.dynamic_allowed {
                parts.push("\"dynamic_allowed\":true".to_string());
            }
            if !t.levels.is_empty() {
                let levels: Vec<String> = t.levels.iter().map(|l| serde_json::Value::String(l.clone()).to_string()).collect();
                parts.push(format!("\"levels\":[{}]", levels.join(",")));
            }
            format!("{{{}}}", parts.join(","))
        }
    };
    format!("|thinking={json}")
}

fn hash_joined(keys: &[String]) -> String {
    if keys.is_empty() { String::new() } else { sha256_hex(&keys.join("\n")) }
}

/// Dedupes and sorts keys (`normalizeModelPairs`).
fn normalize_model_pairs(keys: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out: Vec<String> = keys.filter(|k| seen.insert(k.clone())).collect();
    out.sort();
    out
}

fn name_alias(name: &str, alias: &str) -> Option<(String, String)> {
    let (name, alias) = (name.trim(), alias.trim());
    (!(name.is_empty() && alias.is_empty())).then(|| (name.to_string(), alias.to_string()))
}

/// Normalised hash of an excluded-model list (trimmed, lowercased, sorted; empty -> "").
pub fn compute_excluded_models_hash(excluded: &[String]) -> String {
    let mut normalized: Vec<String> =
        excluded.iter().map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty()).collect();
    if normalized.is_empty() {
        return String::new();
    }
    normalized.sort();
    let json = serde_json::to_string(&normalized).unwrap_or_default();
    sha256_hex(&json)
}

/// Stable hash for OpenAI-compatible models (`modelconfig.ComputeOpenAICompatModelsHash`).
pub fn compute_openai_compat_models_hash(models: &[OpenAiCompatibilityModel]) -> String {
    let modalities = |raw: &[String]| -> String {
        let mut seen = HashSet::new();
        raw.iter()
            .map(|v| v.trim().to_lowercase())
            .filter(|v| !v.is_empty() && seen.insert(v.clone()))
            .collect::<Vec<_>>()
            .join(",")
    };
    let keys: Vec<String> = models
        .iter()
        .filter_map(|m| {
            let (name, alias) = name_alias(&m.name, &m.alias)?;
            Some(format!(
                "{}|{}|{}|image={}|force-mapping={}|is-compat={}|use-max-completion-tokens={}|input={}|output={}{}",
                name.to_lowercase(),
                alias.to_lowercase(),
                m.display_name.trim(),
                m.image,
                m.force_mapping,
                m.is_compat,
                m.use_max_completion_tokens,
                modalities(&m.input_modalities),
                modalities(&m.output_modalities),
                thinking_hash_suffix(&m.thinking)
            ))
        })
        .collect();
    hash_joined(&keys)
}

/// Stable hash for Vertex-compatible models.
pub fn compute_vertex_compat_models_hash(models: &[VertexCompatModel]) -> String {
    let keys: Vec<String> = models
        .iter()
        .filter_map(|m| {
            let (name, alias) = name_alias(&m.name, &m.alias)?;
            Some(format!(
                "{}|{}|{}|force-mapping={}{}",
                name.to_lowercase(),
                alias.to_lowercase(),
                m.display_name.trim(),
                m.force_mapping,
                thinking_hash_suffix(&m.thinking)
            ))
        })
        .collect();
    hash_joined(&keys)
}

/// Stable hash for Claude model aliases.
pub fn compute_claude_models_hash(models: &[ClaudeModel]) -> String {
    let keys: Vec<String> = models
        .iter()
        .filter_map(|m| {
            let (name, alias) = name_alias(&m.name, &m.alias)?;
            Some(format!(
                "{}|{}|{}|force-mapping={}|is-compat={}{}",
                name.to_lowercase(),
                alias.to_lowercase(),
                m.display_name.trim(),
                m.force_mapping,
                m.is_compat,
                thinking_hash_suffix(&m.thinking)
            ))
        })
        .collect();
    hash_joined(&keys)
}

/// Stable hash for Codex (and xAI/Meta) model aliases.
pub fn compute_codex_models_hash(models: &[CodexModel]) -> String {
    let keys: Vec<String> = models
        .iter()
        .filter_map(|m| {
            let (name, alias) = name_alias(&m.name, &m.alias)?;
            Some(format!(
                "{}|{}|{}|force-mapping={}|is-compat={}{}",
                name.to_lowercase(),
                alias.to_lowercase(),
                m.display_name.trim(),
                m.force_mapping,
                m.is_compat,
                thinking_hash_suffix(&m.thinking)
            ))
        })
        .collect();
    hash_joined(&keys)
}

/// Stable hash for Gemini (and Interactions) model aliases.
pub fn compute_gemini_models_hash(models: &[GeminiModel]) -> String {
    let keys: Vec<String> = models
        .iter()
        .filter_map(|m| {
            let (name, alias) = name_alias(&m.name, &m.alias)?;
            Some(format!(
                "{}|{}|{}|force-mapping={}|is-compat={}{}",
                name.to_lowercase(),
                alias.to_lowercase(),
                m.display_name.trim(),
                m.force_mapping,
                m.is_compat,
                thinking_hash_suffix(&m.thinking)
            ))
        })
        .collect();
    hash_joined(&keys)
}

// ---------------------------------------------------------------------------------------------
// Summaries (change detection only)
// ---------------------------------------------------------------------------------------------

fn summarize_keys(keys: Vec<String>) -> ModelsSummary {
    ModelsSummary { hash: hash_joined(&keys), count: keys.len() }
}

/// Hashes Gemini model aliases for change detection.
pub fn summarize_gemini_models(models: &[GeminiModel]) -> ModelsSummary {
    summarize_keys(normalize_model_pairs(models.iter().filter_map(|m| {
        let (name, alias) = name_alias(&m.name, &m.alias)?;
        Some(format!(
            "{}|{}|{}|is-compat={}{}",
            name.to_lowercase(),
            alias.to_lowercase(),
            m.display_name.trim(),
            m.is_compat,
            thinking_hash_suffix(&m.thinking)
        ))
    })))
}

/// Hashes Claude model aliases for change detection.
pub fn summarize_claude_models(models: &[ClaudeModel]) -> ModelsSummary {
    summarize_keys(normalize_model_pairs(models.iter().filter_map(|m| {
        let (name, alias) = name_alias(&m.name, &m.alias)?;
        Some(format!(
            "{}|{}|{}|is-compat={}{}",
            name.to_lowercase(),
            alias.to_lowercase(),
            m.display_name.trim(),
            m.is_compat,
            thinking_hash_suffix(&m.thinking)
        ))
    })))
}

/// Hashes Codex (xAI, Meta) model aliases for change detection.
pub fn summarize_codex_models(models: &[CodexModel]) -> ModelsSummary {
    summarize_keys(normalize_model_pairs(models.iter().filter_map(|m| {
        let (name, alias) = name_alias(&m.name, &m.alias)?;
        Some(format!(
            "{}|{}|{}|force-mapping={}|is-compat={}{}",
            name.to_lowercase(),
            alias.to_lowercase(),
            m.display_name.trim(),
            m.force_mapping,
            m.is_compat,
            thinking_hash_suffix(&m.thinking)
        ))
    })))
}

/// Hashes Vertex-compatible model aliases for change detection.
pub fn summarize_vertex_models(models: &[VertexCompatModel]) -> ModelsSummary {
    let mut names: Vec<String> = models
        .iter()
        .filter_map(|m| {
            let (name, alias) = name_alias(&m.name, &m.alias)?;
            let name = if alias.is_empty() { name } else { alias };
            Some(format!("{name}|{}{}", m.display_name.trim(), thinking_hash_suffix(&m.thinking)))
        })
        .collect();
    if names.is_empty() {
        return ModelsSummary::default();
    }
    names.sort();
    ModelsSummary { hash: sha256_hex(&names.join("|")), count: names.len() }
}

/// Normalises and hashes an excluded-model list.
pub fn summarize_excluded_models(list: &[String]) -> ModelsSummary {
    if list.is_empty() {
        return ModelsSummary::default();
    }
    let mut seen = HashSet::new();
    let mut normalized: Vec<String> = list
        .iter()
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty() && seen.insert(e.clone()))
        .collect();
    normalized.sort();
    ModelsSummary { hash: compute_excluded_models_hash(&normalized), count: normalized.len() }
}

/// Summarises a per-channel map (channel keys trimmed and lowercased, empty ones dropped).
fn summarize_channels<V>(
    entries: &BTreeMap<String, Vec<V>>,
    summarize: impl Fn(&[V]) -> ModelsSummary,
) -> BTreeMap<String, ModelsSummary> {
    let mut out = BTreeMap::new();
    for (key, list) in entries {
        let key = key.trim().to_lowercase();
        if !key.is_empty() {
            out.insert(key, summarize(list));
        }
    }
    out
}

/// Compares two per-channel summaries; returns (sorted change lines, sorted affected channels).
fn diff_channels(
    label: &str,
    old: &BTreeMap<String, ModelsSummary>,
    new: &BTreeMap<String, ModelsSummary>,
) -> (Vec<String>, Vec<String>) {
    let keys: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    let mut changes = Vec::new();
    let mut affected = Vec::new();
    for key in keys {
        match (old.get(key), new.get(key)) {
            (Some(_), None) => {
                changes.push(format!("{label}[{key}]: removed"));
                affected.push(key.clone());
            }
            (None, Some(n)) => {
                changes.push(format!("{label}[{key}]: added ({} entries)", n.count));
                affected.push(key.clone());
            }
            (Some(o), Some(n)) if o.hash != n.hash => {
                changes.push(format!("{label}[{key}]: updated ({} -> {} entries)", o.count, n.count));
                affected.push(key.clone());
            }
            _ => {}
        }
    }
    changes.sort();
    affected.sort();
    (changes, affected)
}

/// OAuth excluded-model changes. The second value lists the providers whose model lists must be
/// rebuilt.
pub fn diff_oauth_excluded_model_changes(
    old: &BTreeMap<String, Vec<String>>,
    new: &BTreeMap<String, Vec<String>>,
) -> (Vec<String>, Vec<String>) {
    let summarize = |m: &BTreeMap<String, Vec<String>>| summarize_channels(m, summarize_excluded_models);
    diff_channels("oauth-excluded-models", &summarize(old), &summarize(new))
}

fn summarize_oauth_model_alias_list(list: &[OAuthModelAlias]) -> ModelsSummary {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for alias in list {
        let name = alias.name.trim().to_lowercase();
        let alias_val = alias.alias.trim().to_lowercase();
        if name.is_empty() || alias_val.is_empty() {
            continue;
        }
        let mut key = format!("{name}->{alias_val}");
        if alias.fork {
            key.push_str("|fork");
        }
        let display = alias.display_name.trim();
        if !display.is_empty() {
            key.push_str("|display-name=");
            key.push_str(display);
        }
        if alias.force_mapping {
            key.push_str("|force-mapping");
        }
        if seen.insert(key.clone()) {
            normalized.push(key);
        }
    }
    if normalized.is_empty() {
        return ModelsSummary::default();
    }
    normalized.sort();
    ModelsSummary { hash: sha256_hex(&normalized.join("|")), count: normalized.len() }
}

/// OAuth model alias changes per channel.
pub fn diff_oauth_model_alias_changes(
    old: &BTreeMap<String, Vec<OAuthModelAlias>>,
    new: &BTreeMap<String, Vec<OAuthModelAlias>>,
) -> (Vec<String>, Vec<String>) {
    diff_channels(
        "oauth-model-alias",
        &summarize_channels(old, summarize_oauth_model_alias_list),
        &summarize_channels(new, summarize_oauth_model_alias_list),
    )
}

fn summarize_request_scoped_errors_list(list: &[RequestScopedErrorRule]) -> ModelsSummary {
    let mut text = String::new();
    let mut valid = 0;
    for entry in list {
        if entry.status <= 0 || (entry.r#match.is_empty() && entry.match_regexr.is_empty()) || entry.action.is_empty() {
            continue;
        }
        valid += 1;
        let _ = writeln!(
            text,
            "{}|{}|{}|{}",
            entry.status,
            entry.r#match.join(","),
            entry.match_regexr.join(","),
            entry.action
        );
    }
    if valid == 0 {
        return ModelsSummary::default();
    }
    ModelsSummary { hash: sha256_hex(&text), count: valid }
}

/// OAuth request-scoped error rule changes per channel.
pub fn diff_oauth_request_scoped_errors_changes(
    old: &BTreeMap<String, Vec<RequestScopedErrorRule>>,
    new: &BTreeMap<String, Vec<RequestScopedErrorRule>>,
) -> (Vec<String>, Vec<String>) {
    diff_channels(
        "oauth-request-scoped-errors",
        &summarize_channels(old, summarize_request_scoped_errors_list),
        &summarize_channels(new, summarize_request_scoped_errors_list),
    )
}

fn summarize_oauth_settings_list(list: &[OAuthModelSetting]) -> ModelsSummary {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for setting in list {
        let name = setting.name.trim().to_lowercase();
        if name.is_empty() {
            continue;
        }
        let mut key = format!("{name}->{}", setting.alias.trim().to_lowercase());
        if setting.max_context_length > 0 {
            let _ = write!(key, "|max-context-length={}", setting.max_context_length);
        }
        if seen.insert(key.clone()) {
            normalized.push(key);
        }
    }
    if normalized.is_empty() {
        return ModelsSummary::default();
    }
    ModelsSummary { hash: sha256_hex(&normalized.join("|")), count: normalized.len() }
}

/// OAuth model settings changes per channel.
pub fn diff_oauth_settings_changes(
    old: &BTreeMap<String, Vec<OAuthModelSetting>>,
    new: &BTreeMap<String, Vec<OAuthModelSetting>>,
) -> (Vec<String>, Vec<String>) {
    diff_channels(
        "oauth-settings",
        &summarize_channels(old, summarize_oauth_settings_list),
        &summarize_channels(new, summarize_oauth_settings_list),
    )
}

// ---------------------------------------------------------------------------------------------
// OpenAI compatibility providers
// ---------------------------------------------------------------------------------------------

fn count_api_keys(entry: &OpenAiCompatibility) -> usize {
    entry.api_key_entries.iter().filter(|k| !k.api_key.trim().is_empty()).count()
}

fn count_openai_models(models: &[OpenAiCompatibilityModel]) -> usize {
    models.iter().filter(|m| name_alias(&m.name, &m.alias).is_some()).count()
}

fn openai_compat_signature(entry: &OpenAiCompatibility) -> String {
    let mut parts = Vec::new();
    let name = entry.name.trim();
    if !name.is_empty() {
        parts.push(format!("name={}", name.to_lowercase()));
    }
    let base = entry.base_url.trim();
    if !base.is_empty() {
        parts.push(format!("base={base}"));
    }
    let mut models: Vec<String> = entry
        .models
        .iter()
        .filter_map(|m| {
            let (name, alias) = name_alias(&m.name, &m.alias)?;
            Some(format!("{}|{}|{}|image={}", name.to_lowercase(), alias.to_lowercase(), m.display_name.trim(), m.image))
        })
        .collect();
    if !models.is_empty() {
        models.sort();
        parts.push(format!("models={}", models.join(",")));
    }
    let mut headers: Vec<String> =
        entry.headers.keys().map(|k| k.trim().to_lowercase()).filter(|k| !k.is_empty()).collect();
    if !headers.is_empty() {
        headers.sort();
        parts.push(format!("headers={}", headers.join(",")));
    }
    // API key material is intentionally excluded; only non-empty entries are counted.
    let keys = count_api_keys(entry);
    if keys > 0 {
        parts.push(format!("api_keys={keys}"));
    }
    if parts.is_empty() { String::new() } else { sha256_hex(&parts.join("|")) }
}

/// (identity key, display label) of a provider entry.
fn openai_compat_key(entry: &OpenAiCompatibility, index: usize) -> (String, String) {
    let name = entry.name.trim();
    if !name.is_empty() {
        return (format!("name:{name}"), name.to_string());
    }
    let base = entry.base_url.trim();
    if !base.is_empty() {
        return (format!("base:{base}"), format_url(base));
    }
    for model in &entry.models {
        let alias = model.alias.trim();
        let alias = if alias.is_empty() { model.name.trim() } else { alias };
        if !alias.is_empty() {
            return (format!("alias:{alias}"), alias.to_string());
        }
    }
    let sig = openai_compat_signature(entry);
    if sig.is_empty() {
        return (format!("index:{index}"), format!("entry-{}", index + 1));
    }
    let short = &sig[..sig.len().min(8)];
    (format!("sig:{sig}"), format!("compat-{short}"))
}

fn unique_openai_compat_key(
    existing: &BTreeMap<String, &OpenAiCompatibility>,
    entry: &OpenAiCompatibility,
    index: usize,
) -> (String, String) {
    let (mut key, label) = openai_compat_key(entry, index);
    let base_key = key.clone();
    let mut duplicate = 1;
    while existing.contains_key(&key) {
        key = format!("duplicate:{base_key}:{duplicate}");
        duplicate += 1;
    }
    (key, label)
}

fn describe_openai_compat_update(old: &OpenAiCompatibility, new: &OpenAiCompatibility) -> String {
    let mut details = Vec::new();
    if old.disabled != new.disabled {
        details.push(format!("disabled {} -> {}", old.disabled, new.disabled));
    }
    if old.support_prompt_cache_key != new.support_prompt_cache_key {
        details.push(format!(
            "support-prompt-cache-key {} -> {}",
            old.support_prompt_cache_key, new.support_prompt_cache_key
        ));
    }
    if old.disable_cooling != new.disable_cooling {
        details.push(format!(
            "disable-cooling {} -> {}",
            format_optional_bool(old.disable_cooling),
            format_optional_bool(new.disable_cooling)
        ));
    }
    if old.request_retry != new.request_retry {
        details.push(format!(
            "request-retry {} -> {}",
            format_optional_int(old.request_retry),
            format_optional_int(new.request_retry)
        ));
    }
    let (old_keys, new_keys) = (count_api_keys(old), count_api_keys(new));
    if old_keys != new_keys {
        details.push(format!("api-keys {old_keys} -> {new_keys}"));
    }
    let (old_models, new_models) = (count_openai_models(&old.models), count_openai_models(&new.models));
    if old_models != new_models {
        details.push(format!("models {old_models} -> {new_models}"));
    }
    if old.headers != new.headers {
        details.push("headers updated".to_string());
    }
    if details.is_empty() { String::new() } else { format!("({})", details.join(", ")) }
}

/// Human-readable changes between two `openai-compatibility` lists, ordered by provider key.
pub fn diff_openai_compatibility(old: &[OpenAiCompatibility], new: &[OpenAiCompatibility]) -> Vec<String> {
    fn index(list: &[OpenAiCompatibility]) -> (BTreeMap<String, &OpenAiCompatibility>, BTreeMap<String, String>) {
        let mut map = BTreeMap::new();
        let mut labels = BTreeMap::new();
        for (i, entry) in list.iter().enumerate() {
            let (key, label) = unique_openai_compat_key(&map, entry, i);
            map.insert(key.clone(), entry);
            labels.insert(key, label);
        }
        (map, labels)
    }
    let (old_map, old_labels) = index(old);
    let (new_map, new_labels) = index(new);
    let keys: BTreeSet<&String> = old_map.keys().chain(new_map.keys()).collect();
    let mut changes = Vec::new();
    for key in keys {
        let label = old_labels
            .get(key)
            .filter(|l| !l.is_empty())
            .or_else(|| new_labels.get(key))
            .cloned()
            .unwrap_or_default();
        match (old_map.get(key), new_map.get(key)) {
            (None, Some(n)) => changes.push(format!(
                "provider added: {label} (api-keys={}, models={})",
                count_api_keys(n),
                count_openai_models(&n.models)
            )),
            (Some(o), None) => changes.push(format!(
                "provider removed: {label} (api-keys={}, models={})",
                count_api_keys(o),
                count_openai_models(&o.models)
            )),
            (Some(o), Some(n)) => {
                let detail = describe_openai_compat_update(o, n);
                if !detail.is_empty() {
                    changes.push(format!("provider updated: {label} {detail}"));
                }
            }
            (None, None) => {}
        }
    }
    changes
}

// ---------------------------------------------------------------------------------------------
// Config change details
// ---------------------------------------------------------------------------------------------

fn format_optional_bool(value: Option<bool>) -> String {
    value.map_or_else(|| "inherit".to_string(), |v| v.to_string())
}

fn format_optional_int(value: Option<i64>) -> String {
    value.map_or_else(|| "<unset>".to_string(), |v| v.to_string())
}

fn display_optional_value(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() { "<none>".to_string() } else { t.to_string() }
}

/// Redacted `scheme://host[:port]` form of a URL (credentials, path and query are dropped);
/// accepts bare `host:port`.
pub fn format_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "<none>".to_string();
    }
    if trimmed.chars().any(|c| c.is_control() || c == ' ') {
        return "<redacted>".to_string();
    }
    // The host (with port) of `scheme://[userinfo@]host[:port][/path...]`; empty when malformed.
    let authority = |s: &str| -> String {
        let end = s.find(['/', '?', '#']).unwrap_or(s.len());
        let auth = &s[..end];
        let host = auth.rsplit_once('@').map_or(auth, |(_, host)| host);
        // An IPv6 literal must be bracketed on both sides.
        if host.contains('[') != host.contains(']') { String::new() } else { host.to_string() }
    };
    let redacted = || "<redacted>".to_string();
    if let Some((scheme, rest)) = trimmed.split_once("://") {
        let valid = scheme.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if valid {
            let host = authority(rest);
            return if host.is_empty() { redacted() } else { format!("{}://{host}", scheme.to_lowercase()) };
        }
    }
    let host = authority(trimmed);
    if host.is_empty() { redacted() } else { host }
}

fn push_change<T: std::fmt::Display + PartialEq>(changes: &mut Vec<String>, field: &str, old: T, new: T) {
    if old != new {
        changes.push(format!("{field}: {old} -> {new}"));
    }
}

fn push_trimmed_change(changes: &mut Vec<String>, field: &str, old: &str, new: &str) {
    push_change(changes, field, old.trim(), new.trim());
}

fn push_optional_bool_change(changes: &mut Vec<String>, field: &str, old: Option<bool>, new: Option<bool>) {
    if old != new {
        changes.push(format!("{field}: {} -> {}", format_optional_bool(old), format_optional_bool(new)));
    }
}

fn push_optional_int_change(changes: &mut Vec<String>, field: &str, old: Option<i64>, new: Option<i64>) {
    if old != new {
        changes.push(format!("{field}: {} -> {}", format_optional_int(old), format_optional_int(new)));
    }
}

fn push_url_change(changes: &mut Vec<String>, field: &str, old: &str, new: &str) {
    if old.trim() != new.trim() {
        changes.push(format!("{field}: {} -> {}", format_url(old), format_url(new)));
    }
}

fn push_models_change(changes: &mut Vec<String>, field: &str, old: &ModelsSummary, new: &ModelsSummary) {
    if old.hash != new.hash {
        changes.push(format!("{field}: updated ({} -> {} entries)", old.count, new.count));
    }
}

/// base-url, proxy-url and prefix lines shared by every credential family.
fn push_endpoint_changes(
    changes: &mut Vec<String>,
    label: &str,
    base_url: (&str, &str),
    proxy_url: (&str, &str),
    prefix: (&str, &str),
) {
    push_url_change(changes, &format!("{label}.base-url"), base_url.0, base_url.1);
    push_url_change(changes, &format!("{label}.proxy-url"), proxy_url.0, proxy_url.1);
    push_trimmed_change(changes, &format!("{label}.prefix"), prefix.0, prefix.1);
}

/// api-key, headers, models and excluded-models lines (never prints key material).
fn push_credential_tail_changes(
    changes: &mut Vec<String>,
    label: &str,
    api_key: (&str, &str),
    headers: (&BTreeMap<String, String>, &BTreeMap<String, String>),
    models: (&ModelsSummary, &ModelsSummary),
    excluded: (&[String], &[String]),
) {
    if api_key.0.trim() != api_key.1.trim() {
        changes.push(format!("{label}.api-key: updated"));
    }
    if headers.0 != headers.1 {
        changes.push(format!("{label}.headers: updated"));
    }
    push_models_change(changes, &format!("{label}.models"), models.0, models.1);
    push_models_change(
        changes,
        &format!("{label}.excluded-models"),
        &summarize_excluded_models(excluded.0),
        &summarize_excluded_models(excluded.1),
    );
}

/// Redacted, human-readable list of config changes. Secrets are never printed; only structural
/// or non-sensitive fields are surfaced.
pub fn build_config_change_details(old: &Config, new: &Config) -> Vec<String> {
    let mut c: Vec<String> = Vec::with_capacity(16);

    // Simple scalars.
    push_change(&mut c, "client.codex.enable-apply-patch", old.client.codex.enable_apply_patch, new.client.codex.enable_apply_patch);
    push_change(&mut c, "port", old.port, new.port);
    push_change(&mut c, "auth-dir", old.auth_dir.as_str(), new.auth_dir.as_str());
    push_change(&mut c, "debug", old.debug, new.debug);
    push_change(&mut c, "pprof.enable", old.pprof.enable, new.pprof.enable);
    push_trimmed_change(&mut c, "pprof.addr", &old.pprof.addr, &new.pprof.addr);
    push_change(&mut c, "logging-to-file", old.logging_to_file, new.logging_to_file);
    push_change(&mut c, "usage-statistics-enabled", old.usage_statistics_enabled, new.usage_statistics_enabled);
    push_change(
        &mut c,
        "redis-usage-queue-retention-seconds",
        old.redis_usage_queue_retention_seconds,
        new.redis_usage_queue_retention_seconds,
    );
    push_change(&mut c, "disable-cooling", old.disable_cooling, new.disable_cooling);
    push_change(&mut c, "save-cooldown-status", old.save_cooldown_status, new.save_cooldown_status);
    push_change(
        &mut c,
        "transient-error-cooldown-seconds",
        old.transient_error_cooldown_seconds,
        new.transient_error_cooldown_seconds,
    );
    push_change(&mut c, "disable-claude-cloak-mode", old.disable_claude_cloak_mode, new.disable_claude_cloak_mode);
    push_change(
        &mut c,
        "claude-code.disable-cloaking-model-list",
        old.claude_code.disable_cloaking_model_list,
        new.claude_code.disable_cloaking_model_list,
    );
    push_change(&mut c, "disable-image-generation", old.disable_image_generation, new.disable_image_generation);
    push_trimmed_change(&mut c, "gpt-image-2-base-model", &old.gpt_image_2_base_model, &new.gpt_image_2_base_model);
    push_change(&mut c, "request-log", old.request_log, new.request_log);
    push_change(&mut c, "logs-max-total-size-mb", old.logs_max_total_size_mb, new.logs_max_total_size_mb);
    push_change(&mut c, "error-logs-max-files", old.error_logs_max_files, new.error_logs_max_files);
    push_change(&mut c, "request-retry", old.request_retry, new.request_retry);
    push_change(&mut c, "max-retry-credentials", old.max_retry_credentials, new.max_retry_credentials);
    push_change(&mut c, "max-retry-interval", old.max_retry_interval, new.max_retry_interval);
    if old.proxy_url != new.proxy_url {
        c.push(format!("proxy-url: {} -> {}", format_url(&old.proxy_url), format_url(&new.proxy_url)));
    }
    push_change(&mut c, "ws-auth", old.websocket_auth, new.websocket_auth);
    push_change(&mut c, "force-model-prefix", old.force_model_prefix, new.force_model_prefix);
    push_change(
        &mut c,
        "nonstream-keepalive-interval",
        old.nonstream_keepalive_interval,
        new.nonstream_keepalive_interval,
    );

    // Quota-exceeded behaviour.
    push_change(&mut c, "quota-exceeded.switch-project", old.quota_exceeded.switch_project, new.quota_exceeded.switch_project);
    push_change(
        &mut c,
        "quota-exceeded.switch-preview-model",
        old.quota_exceeded.switch_preview_model,
        new.quota_exceeded.switch_preview_model,
    );
    push_change(
        &mut c,
        "quota-exceeded.antigravity-credits",
        old.quota_exceeded.antigravity_credits,
        new.quota_exceeded.antigravity_credits,
    );
    if old.antigravity.sensitive_words != new.antigravity.sensitive_words {
        c.push(format!(
            "antigravity.sensitive-words: {} -> {}",
            old.antigravity.sensitive_words.len(),
            new.antigravity.sensitive_words.len()
        ));
    }
    if old.devin.sensitive_words != new.devin.sensitive_words {
        c.push(format!(
            "devin.sensitive-words: {} -> {}",
            old.devin.sensitive_words.len(),
            new.devin.sensitive_words.len()
        ));
    }
    let (op, np) = (&old.antigravity.connection_pool, &new.antigravity.connection_pool);
    let nil_or = |v: Option<String>| v.unwrap_or_else(|| "<nil>".to_string());
    push_change(
        &mut c,
        "antigravity.connection-pool.enabled",
        nil_or(op.enabled.map(|v| v.to_string())),
        nil_or(np.enabled.map(|v| v.to_string())),
    );
    if op.idle_conn_timeout != np.idle_conn_timeout {
        c.push(format!(
            "antigravity.connection-pool.idle-conn-timeout: {:?} -> {:?}",
            op.idle_conn_timeout, np.idle_conn_timeout
        ));
    }
    push_change(
        &mut c,
        "antigravity.connection-pool.max-idle-conns-per-host",
        nil_or(op.max_idle_conns_per_host.map(|v| v.to_string())),
        nil_or(np.max_idle_conns_per_host.map(|v| v.to_string())),
    );

    push_change(&mut c, "codex.disable-codex-cloaking", old.codex.disable_codex_cloaking, new.codex.disable_codex_cloaking);
    push_change(
        &mut c,
        "codex.stream-bootstrap-buffering",
        old.codex.stream_bootstrap_buffering,
        new.codex.stream_bootstrap_buffering,
    );
    push_trimmed_change(
        &mut c,
        "codex.stream-bootstrap-timeout",
        &old.codex.stream_bootstrap_timeout,
        &new.codex.stream_bootstrap_timeout,
    );
    push_change(
        &mut c,
        "client.codex.optimize-multi-agent-v2",
        old.client.codex.optimize_multi_agent_v2,
        new.client.codex.optimize_multi_agent_v2,
    );
    push_change(
        &mut c,
        "codex.orphan-delegation-compatibility",
        old.codex.orphan_delegation_compatibility,
        new.codex.orphan_delegation_compatibility,
    );
    push_change(&mut c, "xai.inject-x-search", old.xai.inject_x_search, new.xai.inject_x_search);
    let (or, nr) = (&old.codex.live_media_relay, &new.codex.live_media_relay);
    push_change(&mut c, "codex.live-media-relay.enabled", or.enabled, nr.enabled);
    push_change(&mut c, "codex.live-media-relay.max-sessions", or.max_sessions, nr.max_sessions);
    push_change(
        &mut c,
        "codex.live-media-relay.disable-private-remote-ips",
        or.disable_private_remote_ips,
        nr.disable_private_remote_ips,
    );
    if or.public_ip.trim() != nr.public_ip.trim() {
        c.push(format!(
            "codex.live-media-relay.public-ip: {} -> {}",
            display_optional_value(&or.public_ip),
            display_optional_value(&nr.public_ip)
        ));
    }
    push_change(&mut c, "codex.live-media-relay.udp-port-min", or.udp_port_min, nr.udp_port_min);
    push_change(&mut c, "codex.live-media-relay.udp-port-max", or.udp_port_max, nr.udp_port_max);
    if or.ice_servers != nr.ice_servers {
        c.push(format!(
            "codex.live-media-relay.ice-servers: updated ({} -> {} entries, credentials redacted)",
            or.ice_servers.len(),
            nr.ice_servers.len()
        ));
    }

    push_change(&mut c, "routing.strategy", old.routing.strategy.as_str(), new.routing.strategy.as_str());
    if old.payload != new.payload {
        push_payload_changes(&mut c, &old.payload, &new.payload);
    }

    // API keys (redacted) and counts.
    if old.api_keys.len() != new.api_keys.len() {
        c.push(format!("api-keys count: {} -> {}", old.api_keys.len(), new.api_keys.len()));
    } else if old.api_keys.iter().map(|k| k.trim()).ne(new.api_keys.iter().map(|k| k.trim())) {
        c.push("api-keys: values updated (count unchanged, redacted)".to_string());
    }

    for (name, short, old_keys, new_keys) in [
        ("gemini-api-key", "gemini", &old.gemini_key, &new.gemini_key),
        ("interactions-api-key", "interactions", &old.interactions_key, &new.interactions_key),
    ] {
        if old_keys.len() != new_keys.len() {
            c.push(format!("{name} count: {} -> {}", old_keys.len(), new_keys.len()));
            continue;
        }
        for (i, (o, n)) in old_keys.iter().zip(new_keys).enumerate() {
            let label = format!("{short}[{i}]");
            push_endpoint_changes(
                &mut c,
                &label,
                (&o.base_url, &n.base_url),
                (&o.proxy_url, &n.proxy_url),
                (&o.prefix, &n.prefix),
            );
            push_optional_bool_change(&mut c, &format!("{label}.disable-cooling"), o.disable_cooling, n.disable_cooling);
            push_credential_tail_changes(
                &mut c,
                &label,
                (&o.api_key, &n.api_key),
                (&o.headers, &n.headers),
                (&summarize_gemini_models(&o.models), &summarize_gemini_models(&n.models)),
                (&o.excluded_models, &n.excluded_models),
            );
            push_optional_int_change(&mut c, &format!("{label}.request-retry"), o.request_retry, n.request_retry);
        }
    }

    // Claude keys.
    if old.claude_key.len() != new.claude_key.len() {
        c.push(format!("claude-api-key count: {} -> {}", old.claude_key.len(), new.claude_key.len()));
    } else {
        for (i, (o, n)) in old.claude_key.iter().zip(&new.claude_key).enumerate() {
            let label = format!("claude[{i}]");
            push_endpoint_changes(
                &mut c,
                &label,
                (&o.base_url, &n.base_url),
                (&o.proxy_url, &n.proxy_url),
                (&o.prefix, &n.prefix),
            );
            push_optional_bool_change(&mut c, &format!("{label}.disable-cooling"), o.disable_cooling, n.disable_cooling);
            push_credential_tail_changes(
                &mut c,
                &label,
                (&o.api_key, &n.api_key),
                (&o.headers, &n.headers),
                (&summarize_claude_models(&o.models), &summarize_claude_models(&n.models)),
                (&o.excluded_models, &n.excluded_models),
            );
            if o.rebuild_mid_system_message != n.rebuild_mid_system_message {
                c.push(format!(
                    "{label}.rebuild-mid-system-message: {} -> {}",
                    o.rebuild_mid_system_message, n.rebuild_mid_system_message
                ));
            }
            push_trimmed_change(&mut c, &format!("{label}.fingerprint-profile"), &o.fingerprint_profile, &n.fingerprint_profile);
            push_optional_int_change(&mut c, &format!("{label}.request-retry"), o.request_retry, n.request_retry);
            if let (Some(oc), Some(nc)) = (&o.cloak, &n.cloak) {
                push_trimmed_change(&mut c, &format!("{label}.cloak.mode"), &oc.mode, &nc.mode);
                push_change(&mut c, &format!("{label}.cloak.strict-mode"), oc.strict_mode, nc.strict_mode);
                push_change(&mut c, &format!("{label}.cloak.sensitive-words"), oc.sensitive_words.len(), nc.sensitive_words.len());
            }
        }
    }

    // Codex keys.
    if old.codex_key.len() != new.codex_key.len() {
        c.push(format!("codex-api-key count: {} -> {}", old.codex_key.len(), new.codex_key.len()));
    } else {
        for (i, (o, n)) in old.codex_key.iter().zip(&new.codex_key).enumerate() {
            let label = format!("codex[{i}]");
            push_endpoint_changes(
                &mut c,
                &label,
                (&o.base_url, &n.base_url),
                (&o.proxy_url, &n.proxy_url),
                (&o.prefix, &n.prefix),
            );
            push_change(&mut c, &format!("{label}.websockets"), o.websockets, n.websockets);
            push_change(&mut c, &format!("{label}.alpha-search"), o.alpha_search, n.alpha_search);
            push_optional_bool_change(&mut c, &format!("{label}.disable-cooling"), o.disable_cooling, n.disable_cooling);
            push_optional_bool_change(
                &mut c,
                &format!("{label}.disable-codex-cloaking"),
                o.disable_codex_cloaking,
                n.disable_codex_cloaking,
            );
            push_credential_tail_changes(
                &mut c,
                &label,
                (&o.api_key, &n.api_key),
                (&o.headers, &n.headers),
                (&summarize_codex_models(&o.models), &summarize_codex_models(&n.models)),
                (&o.excluded_models, &n.excluded_models),
            );
            push_optional_int_change(&mut c, &format!("{label}.request-retry"), o.request_retry, n.request_retry);
        }
    }

    // xAI and Meta keys share the Codex structure.
    for (name, short, old_keys, new_keys) in [
        ("xai-api-key", "xai", &old.xai_key, &new.xai_key),
        ("meta-api-key", "meta", &old.meta_key, &new.meta_key),
    ] {
        if old_keys.len() != new_keys.len() {
            c.push(format!("{name} count: {} -> {}", old_keys.len(), new_keys.len()));
            continue;
        }
        for (i, (o, n)) in old_keys.iter().zip(new_keys).enumerate() {
            let label = format!("{short}[{i}]");
            push_endpoint_changes(
                &mut c,
                &label,
                (&o.base_url, &n.base_url),
                (&o.proxy_url, &n.proxy_url),
                (&o.prefix, &n.prefix),
            );
            push_change(&mut c, &format!("{label}.priority"), o.priority, n.priority);
            if short == "xai" {
                push_change(&mut c, &format!("{label}.websockets"), o.websockets, n.websockets);
            }
            push_optional_bool_change(&mut c, &format!("{label}.disable-cooling"), o.disable_cooling, n.disable_cooling);
            push_optional_int_change(&mut c, &format!("{label}.request-retry"), o.request_retry, n.request_retry);
            push_credential_tail_changes(
                &mut c,
                &label,
                (&o.api_key, &n.api_key),
                (&o.headers, &n.headers),
                (&summarize_codex_models(&o.models), &summarize_codex_models(&n.models)),
                (&o.excluded_models, &n.excluded_models),
            );
        }
    }

    c.extend(diff_oauth_excluded_model_changes(&old.oauth_excluded_models, &new.oauth_excluded_models).0);
    c.extend(diff_oauth_model_alias_changes(&old.oauth_model_alias, &new.oauth_model_alias).0);
    c.extend(diff_oauth_request_scoped_errors_changes(&old.oauth_request_scoped_errors, &new.oauth_request_scoped_errors).0);
    c.extend(diff_oauth_settings_changes(&old.oauth_settings, &new.oauth_settings).0);

    // Remote management (never print the key).
    let (om, nm) = (&old.remote_management, &new.remote_management);
    push_change(&mut c, "remote-management.allow-remote", om.allow_remote, nm.allow_remote);
    push_change(&mut c, "remote-management.disable-control-panel", om.disable_control_panel, nm.disable_control_panel);
    push_change(
        &mut c,
        "remote-management.disable-auto-update-panel",
        om.disable_auto_update_panel,
        nm.disable_auto_update_panel,
    );
    push_url_change(&mut c, "remote-management.panel-github-repository", &om.panel_github_repository, &nm.panel_github_repository);
    push_url_change(&mut c, "remote-management.base-url", &om.base_url, &nm.base_url);
    if om.secret_key != nm.secret_key {
        c.push(
            match (om.secret_key.is_empty(), nm.secret_key.is_empty()) {
                (true, false) => "remote-management.secret-key: created",
                (false, true) => "remote-management.secret-key: deleted",
                _ => "remote-management.secret-key: updated",
            }
            .to_string(),
        );
    }

    // OpenAI compatibility providers (summarised).
    let compat = diff_openai_compatibility(&old.openai_compatibility, &new.openai_compatibility);
    if !compat.is_empty() {
        c.push("openai-compatibility:".to_string());
        c.extend(compat.into_iter().map(|line| format!("  {line}")));
    }

    // Vertex-compatible API keys.
    if old.vertex_compat_api_key.len() != new.vertex_compat_api_key.len() {
        c.push(format!(
            "vertex-api-key count: {} -> {}",
            old.vertex_compat_api_key.len(),
            new.vertex_compat_api_key.len()
        ));
    } else {
        for (i, (o, n)) in old.vertex_compat_api_key.iter().zip(&new.vertex_compat_api_key).enumerate() {
            let label = format!("vertex[{i}]");
            push_url_change(&mut c, &format!("{label}.base-url"), &o.base_url, &n.base_url);
            push_url_change(&mut c, &format!("{label}.proxy-url"), &o.proxy_url, &n.proxy_url);
            push_trimmed_change(&mut c, &format!("{label}.prefix"), &o.prefix, &n.prefix);
            push_optional_bool_change(&mut c, &format!("{label}.disable-cooling"), o.disable_cooling, n.disable_cooling);
            if o.api_key.trim() != n.api_key.trim() {
                c.push(format!("{label}.api-key: updated"));
            }
            push_models_change(
                &mut c,
                &format!("{label}.models"),
                &summarize_vertex_models(&o.models),
                &summarize_vertex_models(&n.models),
            );
            push_models_change(
                &mut c,
                &format!("{label}.excluded-models"),
                &summarize_excluded_models(&o.excluded_models),
                &summarize_excluded_models(&n.excluded_models),
            );
            if o.headers != n.headers {
                c.push(format!("{label}.headers: updated"));
            }
            push_optional_int_change(&mut c, &format!("{label}.request-retry"), o.request_retry, n.request_retry);
        }
    }

    c
}

fn push_payload_changes(changes: &mut Vec<String>, old: &PayloadConfig, new: &PayloadConfig) {
    fn section<T: PartialEq>(changes: &mut Vec<String>, name: &str, old: &[T], new: &[T]) {
        if old != new {
            changes.push(format!("payload.{name}: updated ({} -> {} rules)", old.len(), new.len()));
        }
    }
    section(changes, "default", &old.default, &new.default);
    section(changes, "default-raw", &old.default_raw, &new.default_raw);
    section(changes, "override", &old.r#override, &new.r#override);
    section(changes, "override-raw", &old.override_raw, &new.override_raw);
    section(changes, "filter", &old.filter, &new.filter);
}

// ---------------------------------------------------------------------------------------------
// Reload plan
// ---------------------------------------------------------------------------------------------

/// What a server must do after a config reload (the decisions `reloadConfig` makes from the old
/// and new config).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReloadPlan {
    /// The auth directory changed (or there was no previous config): rescan auth files.
    pub auth_dir_changed: bool,
    /// Providers whose `oauth-excluded-models` changed; their registered models must be rebuilt.
    pub affected_oauth_providers: Vec<String>,
    /// Prefix, alias, settings or retry config changed: re-register every auth and rebuild model
    /// lists even if the auths themselves are unchanged.
    pub force_auth_refresh: bool,
}

impl ReloadPlan {
    /// Computes the plan for a reload from `old` (`None` on first load) to `new`.
    pub fn between(old: Option<&Config>, new: &Config) -> Self {
        let Some(old) = old else {
            return Self { auth_dir_changed: true, ..Self::default() };
        };
        let retry_changed = old.request_retry != new.request_retry
            || old.max_retry_interval != new.max_retry_interval
            || old.max_retry_credentials != new.max_retry_credentials;
        Self {
            auth_dir_changed: old.auth_dir != new.auth_dir,
            affected_oauth_providers: diff_oauth_excluded_model_changes(
                &old.oauth_excluded_models,
                &new.oauth_excluded_models,
            )
            .1,
            force_auth_refresh: old.force_model_prefix != new.force_model_prefix
                || old.oauth_model_alias != new.oauth_model_alias
                || old.oauth_settings != new.oauth_settings
                || retry_changed,
        }
    }
}
