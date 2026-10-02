//! Sanitisers applied after decoding (port of `config_normalization.go`, `vertex_compat.go`,
//! `config_validation.go`, `claude_fingerprint_profile.go`). They trim, dedupe and drop entries
//! that cannot work, in the same order and with the same rules as the Go loader.

use std::collections::{BTreeMap, HashSet};

use crate::types::*;

pub const CLAUDE_FINGERPRINT_PROFILE_DEFAULT: &str = "";
pub const CLAUDE_FINGERPRINT_PROFILE_CLAUDE_CODE_CLI: &str = "claude-code-cli";
const CLAUDE_FINGERPRINT_PROFILE_OAUTH_CLI_ALIAS: &str = "oauth-cli";

/// Maps a raw fingerprint-profile value to its canonical form. The flag reports whether the value
/// was recognised; an unrecognised value normalises to the default profile.
pub fn normalize_claude_fingerprint_profile(raw: &str) -> (&'static str, bool) {
    match raw.trim().to_lowercase().as_str() {
        CLAUDE_FINGERPRINT_PROFILE_CLAUDE_CODE_CLI | CLAUDE_FINGERPRINT_PROFILE_OAUTH_CLI_ALIAS => {
            (CLAUDE_FINGERPRINT_PROFILE_CLAUDE_CODE_CLI, true)
        }
        CLAUDE_FINGERPRINT_PROFILE_DEFAULT => (CLAUDE_FINGERPRINT_PROFILE_DEFAULT, true),
        _ => (CLAUDE_FINGERPRINT_PROFILE_DEFAULT, false),
    }
}

/// Rejects fingerprint profiles that would be silently ignored at request time (write paths).
pub fn validate_claude_fingerprint_profile(raw: &str) -> Result<(), String> {
    match normalize_claude_fingerprint_profile(raw) {
        (_, true) => Ok(()),
        _ => Err(format!(
            "unsupported fingerprint-profile {:?} (supported: {:?} or empty)",
            raw.trim(),
            CLAUDE_FINGERPRINT_PROFILE_CLAUDE_CODE_CLI
        )),
    }
}

/// Trims whitespace and surrounding "/" and rejects prefixes that still contain "/".
pub fn normalize_model_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim().trim_matches('/');
    if trimmed.contains('/') {
        String::new()
    } else {
        trimmed.to_string()
    }
}

/// Trims header names and values and drops empty pairs.
pub fn normalize_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            let (k, v) = (k.trim(), v.trim());
            (!k.is_empty() && !v.is_empty()).then(|| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// Trims, lowercases and dedupes model exclusion patterns, keeping first-occurrence order.
pub fn normalize_excluded_models(models: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    models
        .iter()
        .map(|m| m.trim().to_lowercase())
        .filter(|m| !m.is_empty() && seen.insert(m.clone()))
        .collect()
}

/// Lowercases provider keys and normalises each exclusion list; empty results are dropped.
pub fn normalize_oauth_excluded_models(
    entries: &BTreeMap<String, Vec<String>>,
) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for (provider, models) in entries {
        let key = provider.trim().to_lowercase();
        if key.is_empty() {
            continue;
        }
        let normalized = normalize_excluded_models(models);
        if !normalized.is_empty() {
            out.insert(key, normalized);
        }
    }
    out
}

/// Trims strings and removes blank sensitive words.
pub fn normalize_cloak_config(cloak: &mut CloakConfig) {
    cloak.mode = cloak.mode.trim().to_string();
    cloak.sensitive_words = cloak
        .sensitive_words
        .iter()
        .map(|w| w.trim().to_string())
        .filter(|w| !w.is_empty())
        .collect();
}

/// Serialises headers deterministically with NUL separators (used for dedupe keys).
pub fn format_sorted_headers(headers: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (k, v) in headers {
        out.push_str(k);
        out.push('\0');
        out.push_str(v);
        out.push('\0');
    }
    out
}

impl Config {
    /// Applies default plugin configuration values (`NormalizePluginsConfig`).
    pub fn normalize_plugins_config(&mut self) {
        let plugins = &mut self.plugins;
        plugins.dir = plugins.dir.trim().to_string();
        if plugins.dir.is_empty() {
            plugins.dir = DEFAULT_PLUGINS_DIR.to_string();
        }
        plugins.store_sources = plugins
            .store_sources
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        plugins.store_auth = normalize_plugin_store_auth(&plugins.store_auth);
    }

    pub fn sanitize_codex_header_defaults(&mut self) {
        let d = &mut self.codex_header_defaults;
        d.user_agent = d.user_agent.trim().to_string();
        d.beta_features = d.beta_features.trim().to_string();
    }

    pub fn sanitize_claude_header_defaults(&mut self) {
        let d = &mut self.claude_header_defaults;
        for field in [
            &mut d.user_agent,
            &mut d.package_version,
            &mut d.runtime_version,
            &mut d.os,
            &mut d.arch,
            &mut d.timeout,
            &mut d.timezone,
        ] {
            *field = field.trim().to_string();
        }
    }

    /// Normalises and dedupes global OAuth model aliases: lowercases channels, drops empty or
    /// self-referential entries, and keeps aliases unique per channel.
    pub fn sanitize_oauth_model_alias(&mut self) {
        if self.oauth_model_alias.is_empty() {
            return;
        }
        let mut out = BTreeMap::new();
        for (raw_channel, aliases) in &self.oauth_model_alias {
            let channel = raw_channel.trim().to_lowercase();
            if channel.is_empty() || aliases.is_empty() {
                continue;
            }
            let mut seen = HashSet::new();
            let mut clean = Vec::new();
            for entry in aliases {
                let (name, alias) = (entry.name.trim(), entry.alias.trim());
                if name.is_empty() || alias.is_empty() || name.eq_ignore_ascii_case(alias) {
                    continue;
                }
                if !seen.insert(alias.to_lowercase()) {
                    continue;
                }
                clean.push(OAuthModelAlias {
                    name: name.to_string(),
                    alias: alias.to_string(),
                    fork: entry.fork,
                    display_name: entry.display_name.trim().to_string(),
                    force_mapping: entry.force_mapping,
                });
            }
            if !clean.is_empty() {
                out.insert(channel, clean);
            }
        }
        self.oauth_model_alias = out;
    }

    /// Normalises global OAuth model settings; later duplicates win (entries are deduped from the
    /// end, then restored to their original order).
    pub fn sanitize_oauth_settings(&mut self) {
        if self.oauth_settings.is_empty() {
            return;
        }
        let mut out = BTreeMap::new();
        for (raw_channel, settings) in &self.oauth_settings {
            let channel = raw_channel.trim().to_lowercase();
            if channel.is_empty() || settings.is_empty() {
                continue;
            }
            let mut seen = HashSet::new();
            let mut reversed = Vec::new();
            for entry in settings.iter().rev() {
                let name = entry.name.trim();
                if name.is_empty() {
                    continue;
                }
                let alias = entry.alias.trim();
                if !seen.insert(format!("{}->{}", name.to_lowercase(), alias.to_lowercase())) {
                    continue;
                }
                reversed.push(OAuthModelSetting {
                    name: name.to_string(),
                    alias: alias.to_string(),
                    max_context_length: entry.max_context_length,
                });
            }
            if !reversed.is_empty() {
                reversed.reverse();
                out.insert(channel, reversed);
            }
        }
        self.oauth_settings = out;
    }

    /// Normalises global OAuth request-scoped error rules and drops invalid ones.
    pub fn sanitize_oauth_request_scoped_errors(&mut self) {
        if self.oauth_request_scoped_errors.is_empty() {
            return;
        }
        let trimmed = |items: &[String]| -> Vec<String> {
            items
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let mut out = BTreeMap::new();
        for (raw_channel, rules) in &self.oauth_request_scoped_errors {
            let channel = raw_channel.trim().to_lowercase();
            if channel.is_empty() || rules.is_empty() {
                continue;
            }
            let mut clean = Vec::new();
            for rule in rules {
                let action = rule.action.trim().to_lowercase();
                let (r#match, match_regexr) = (trimmed(&rule.r#match), trimmed(&rule.match_regexr));
                if rule.status <= 0
                    || (r#match.is_empty() && match_regexr.is_empty())
                    || action.is_empty()
                {
                    continue;
                }
                clean.push(RequestScopedErrorRule {
                    status: rule.status,
                    r#match,
                    match_regexr,
                    action,
                });
            }
            if !clean.is_empty() {
                out.insert(channel, clean);
            }
        }
        self.oauth_request_scoped_errors = out;
    }

    /// Drops OpenAI-compatibility providers without a base URL and trims the rest.
    pub fn sanitize_openai_compatibility(&mut self) {
        let entries = std::mem::take(&mut self.openai_compatibility);
        self.openai_compatibility = entries
            .into_iter()
            .filter_map(|mut e| {
                e.name = e.name.trim().to_string();
                e.prefix = normalize_model_prefix(&e.prefix);
                e.base_url = e.base_url.trim().to_string();
                e.headers = normalize_headers(&e.headers);
                (!e.base_url.is_empty()).then_some(e)
            })
            .collect();
    }

    pub fn sanitize_codex_keys(&mut self) {
        self.codex_key = sanitize_codex_key_entries(std::mem::take(&mut self.codex_key));
    }

    /// Same rules as codex-api-key; alpha search is Codex-only.
    pub fn sanitize_xai_keys(&mut self) {
        self.xai_key = sanitize_codex_key_entries(std::mem::take(&mut self.xai_key));
        for key in &mut self.xai_key {
            key.alpha_search = false;
        }
    }

    /// Drops Meta entries without a plain API key ("dca:" tokens need OAuth storage) and defaults
    /// the base URL to https://api.meta.ai/v1.
    pub fn sanitize_meta_keys(&mut self) {
        let entries = std::mem::take(&mut self.meta_key);
        self.meta_key = entries
            .into_iter()
            .filter_map(|mut e| {
                e.api_key = e.api_key.trim().to_string();
                if e.api_key.is_empty() || e.api_key.starts_with("dca:") {
                    return None;
                }
                e.prefix = normalize_model_prefix(&e.prefix);
                e.base_url = e.base_url.trim().to_string();
                if e.base_url.is_empty() {
                    e.base_url = "https://api.meta.ai/v1".to_string();
                }
                e.headers = normalize_headers(&e.headers);
                e.excluded_models = normalize_excluded_models(&e.excluded_models);
                e.alpha_search = false;
                Some(e)
            })
            .collect();
    }

    /// Normalises Claude credentials. An unrecognised fingerprint profile is preserved as written
    /// (trimmed) so sanitising never destroys operator input.
    pub fn sanitize_claude_keys(&mut self) {
        for entry in &mut self.claude_key {
            entry.prefix = normalize_model_prefix(&entry.prefix);
            entry.headers = normalize_headers(&entry.headers);
            entry.excluded_models = normalize_excluded_models(&entry.excluded_models);
            if let Some(cloak) = &mut entry.cloak {
                normalize_cloak_config(cloak);
            }
            entry.fingerprint_profile =
                match normalize_claude_fingerprint_profile(&entry.fingerprint_profile) {
                    (normalized, true) => normalized.to_string(),
                    _ => entry.fingerprint_profile.trim().to_string(),
                };
        }
    }

    /// Deduplicates Gemini credentials by key, base URL, proxy URL, prefix and headers.
    pub fn sanitize_gemini_keys(&mut self) {
        self.gemini_key = sanitize_gemini_key_entries(std::mem::take(&mut self.gemini_key));
    }

    pub fn sanitize_interactions_keys(&mut self) {
        self.interactions_key =
            sanitize_gemini_key_entries(std::mem::take(&mut self.interactions_key));
    }

    /// Dedupes Vertex-compatible keys by key + base URL and drops models without name or alias.
    pub fn sanitize_vertex_compat_keys(&mut self) {
        let entries = std::mem::take(&mut self.vertex_compat_api_key);
        let mut seen = HashSet::new();
        for mut entry in entries {
            entry.api_key = entry.api_key.trim().to_string();
            if entry.api_key.is_empty() {
                continue;
            }
            entry.prefix = normalize_model_prefix(&entry.prefix);
            entry.base_url = entry.base_url.trim().to_string();
            entry.proxy_url = entry.proxy_url.trim().to_string();
            entry.headers = normalize_headers(&entry.headers);
            entry.excluded_models = normalize_excluded_models(&entry.excluded_models);
            entry.models = std::mem::take(&mut entry.models)
                .into_iter()
                .filter_map(|mut m| {
                    m.alias = m.alias.trim().to_string();
                    m.name = m.name.trim().to_string();
                    (!m.alias.is_empty() && !m.name.is_empty()).then_some(m)
                })
                .collect();
            if seen.insert(format!("{}|{}", entry.api_key, entry.base_url)) {
                self.vertex_compat_api_key.push(entry);
            }
        }
    }

    /// Validates raw JSON payload rule params and drops rules with invalid values.
    pub fn sanitize_payload_rules(&mut self) {
        self.payload.default_raw = sanitize_payload_raw_rules(
            std::mem::take(&mut self.payload.default_raw),
            "default-raw",
        );
        self.payload.override_raw = sanitize_payload_raw_rules(
            std::mem::take(&mut self.payload.override_raw),
            "override-raw",
        );
    }
}

fn sanitize_codex_key_entries(entries: Vec<CodexKey>) -> Vec<CodexKey> {
    entries
        .into_iter()
        .filter_map(|mut e| {
            e.prefix = normalize_model_prefix(&e.prefix);
            e.base_url = e.base_url.trim().to_string();
            e.headers = normalize_headers(&e.headers);
            e.excluded_models = normalize_excluded_models(&e.excluded_models);
            (!e.base_url.is_empty()).then_some(e)
        })
        .collect()
}

fn sanitize_gemini_key_entries(entries: Vec<GeminiKey>) -> Vec<GeminiKey> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(entries.len());
    for mut entry in entries {
        entry.api_key = entry.api_key.trim().to_string();
        entry.base_url = entry.base_url.trim().to_string();
        if entry.api_key.is_empty() && entry.base_url.is_empty() {
            continue;
        }
        entry.prefix = normalize_model_prefix(&entry.prefix);
        entry.proxy_url = entry.proxy_url.trim().to_string();
        entry.headers = normalize_headers(&entry.headers);
        entry.excluded_models = normalize_excluded_models(&entry.excluded_models);
        let id = format!(
            "{}\0{}\0{}\0{}\0{}",
            entry.api_key,
            entry.base_url,
            entry.proxy_url,
            entry.prefix,
            format_sorted_headers(&entry.headers)
        );
        if seen.insert(id) {
            out.push(entry);
        }
    }
    out
}

fn sanitize_payload_raw_rules(rules: Vec<PayloadRule>, section: &str) -> Vec<PayloadRule> {
    rules
        .into_iter()
        .enumerate()
        .filter(|(i, rule)| {
            if rule.params.is_empty() {
                return false;
            }
            for (path, value) in &rule.params {
                // Only string values are raw JSON fragments.
                let serde_yaml_ng::Value::String(raw) = value else { continue };
                let trimmed = raw.trim();
                if trimmed.is_empty() || serde_json::from_str::<serde::de::IgnoredAny>(trimmed).is_err() {
                    tracing::warn!(section, rule_index = i + 1, param = %path, "payload rule dropped: invalid raw JSON");
                    return false;
                }
            }
            true
        })
        .map(|(_, rule)| rule)
        .collect()
}

/// Normalises plugin store auth rules: trims fields, defaults the type to "none", drops rules
/// without a match, and dedupes/lowercases `apply-to`.
pub fn normalize_plugin_store_auth(auth: &[PluginStoreAuth]) -> Vec<PluginStoreAuth> {
    auth.iter()
        .filter_map(|item| {
            let mut item = item.clone();
            item.r#match = item.r#match.trim().to_string();
            item.kind = item.kind.trim().to_lowercase();
            for field in [
                &mut item.token_env,
                &mut item.username_env,
                &mut item.password_env,
                &mut item.header_name,
                &mut item.header_value_env,
            ] {
                *field = field.trim().to_string();
            }
            if item.kind.is_empty() {
                item.kind = "none".to_string();
            }
            if item.r#match.is_empty() {
                return None;
            }
            if !item.apply_to.is_empty() {
                let mut seen = HashSet::new();
                item.apply_to = item
                    .apply_to
                    .iter()
                    .map(|v| v.trim().to_lowercase())
                    .filter(|v| !v.is_empty() && seen.insert(v.clone()))
                    .collect();
            }
            Some(item)
        })
        .collect()
}
