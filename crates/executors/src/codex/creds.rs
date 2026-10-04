//! Codex credential facts: token and base URL, config-entry matching, compat and cloaking flags
//! (Go: codex_executor_auth.go, isCodexCloakingDisabled, codexWebsocketsEnabled).

use cpa_auth::Auth;
use cpa_auth::types::AUTH_KIND_API_KEY;
use cpa_config::{CodexKey, Config};
use cpa_runtime::conductor::{codex_api_key_model_is_compat, resolved_model_info};
use cpa_runtime::executor::Request;
use cpa_runtime::service::synth::{ATTRIBUTE_CODEX_DISABLE_CLOAKING, ATTRIBUTE_CONFIG_INDEX};
use serde_json::Value;

/// Default upstream base URL.
pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// `(api_key, base_url)`: the `api_key` attribute, else the OAuth access token (Go: codexCreds).
pub fn codex_creds(auth: &Auth) -> (String, String) {
    let api_key = auth.attributes.get("api_key").cloned().unwrap_or_default();
    let base_url = auth.attributes.get("base_url").cloned().unwrap_or_default();
    if api_key.is_empty()
        && let Some(Value::String(token)) = auth.metadata.get("access_token")
    {
        return (token.clone(), base_url);
    }
    (api_key, base_url)
}

/// Base URL without a trailing slash, defaulting to the ChatGPT backend.
pub fn base_url(configured: &str) -> String {
    let url = if configured.is_empty() { DEFAULT_BASE_URL } else { configured };
    url.strip_suffix('/').unwrap_or(url).to_string()
}

/// API-key credentials: `auth_kind` apikey or a non-blank `api_key` attribute (Go: codexAuthUsesAPIKey).
pub fn auth_uses_api_key(auth: &Auth) -> bool {
    auth.auth_kind() == AUTH_KIND_API_KEY || auth.attributes.get("api_key").is_some_and(|k| !k.trim().is_empty())
}

/// Free-plan OAuth credentials (no image generation tool).
pub fn is_free_plan_auth(auth: &Auth) -> bool {
    auth.provider.trim().eq_ignore_ascii_case("codex")
        && auth.attributes.get("plan_type").is_some_and(|p| p.trim().eq_ignore_ascii_case("free"))
}

/// The `codex-api-key` entry that produced `auth`: config-index attribute validated against key
/// and base URL, then case-insensitive key + base URL matching (Go: resolveCodexKeyConfig).
pub fn resolve_codex_key_config<'a>(cfg: &'a Config, auth: &Auth) -> Option<&'a CodexKey> {
    let attr_key = auth.attributes.get("api_key").map(|v| v.trim()).unwrap_or_default();
    let attr_base = auth.attributes.get("base_url").map(|v| v.trim()).unwrap_or_default();
    if let Some(index) = auth.attributes.get(ATTRIBUTE_CONFIG_INDEX).and_then(|v| v.trim().parse::<usize>().ok())
        && let Some(entry) = cfg.codex_key.get(index)
    {
        let (cfg_key, cfg_base) = (entry.api_key.trim(), entry.base_url.trim());
        if (attr_key.is_empty() || cfg_key.eq_ignore_ascii_case(attr_key)) && (attr_base.is_empty() || cfg_base.eq_ignore_ascii_case(attr_base)) {
            return Some(entry);
        }
    }
    for entry in &cfg.codex_key {
        let (cfg_key, cfg_base) = (entry.api_key.trim(), entry.base_url.trim());
        if !attr_key.is_empty() && !attr_base.is_empty() {
            if cfg_key.eq_ignore_ascii_case(attr_key) && cfg_base.eq_ignore_ascii_case(attr_base) {
                return Some(entry);
            }
            continue;
        }
        if !attr_key.is_empty() && cfg_key.eq_ignore_ascii_case(attr_key) && (cfg_base.is_empty() || cfg_base.eq_ignore_ascii_case(attr_base)) {
            return Some(entry);
        }
        if attr_key.is_empty() && !attr_base.is_empty() && cfg_base.eq_ignore_ascii_case(attr_base) {
            return Some(entry);
        }
    }
    if !attr_key.is_empty() {
        return cfg.codex_key.iter().find(|e| e.api_key.trim().eq_ignore_ascii_case(attr_key));
    }
    None
}

/// Whether the model runs in compat mode: the conductor-resolved model info, else the matching
/// configured model of the key entry, else the API-key compat lookup (Go: resolveCodexModelIsCompat).
pub fn resolve_model_is_compat(cfg: &Config, auth: &Auth, req: &Request, base_model: &str) -> bool {
    if let Some(resolved) = resolved_model_info(req) {
        return resolved.is_compat;
    }
    if let Some(entry) = resolve_codex_key_config(cfg, auth)
        && !entry.models.is_empty()
    {
        let requested = req.model.trim();
        let target = base_model.trim();
        for model in &entry.models {
            let (name, alias) = (model.name.trim(), model.alias.trim());
            let matches_target = !target.is_empty() && (name.eq_ignore_ascii_case(target) || alias.eq_ignore_ascii_case(target));
            let matches_requested = !requested.is_empty() && (name.eq_ignore_ascii_case(requested) || alias.eq_ignore_ascii_case(requested));
            if matches_target || matches_requested {
                return model.is_compat;
            }
        }
        return false;
    }
    codex_api_key_model_is_compat(cfg, auth, base_model) || codex_api_key_model_is_compat(cfg, auth, &req.model)
}

/// Cloaking opt-out: auth attribute, then key entry, then provider-wide (Go: isCodexCloakingDisabled).
pub fn is_cloaking_disabled(cfg: &Config, auth: &Auth) -> bool {
    let scoped;
    let cfg = if auth.auth_kind() == AUTH_KIND_API_KEY {
        scoped = cfg.for_api_key();
        &*scoped
    } else {
        cfg
    };
    if let Some(raw) = auth.attributes.get(ATTRIBUTE_CODEX_DISABLE_CLOAKING)
        && let Some(parsed) = parse_bool(raw.trim())
    {
        return parsed;
    }
    if let Some(entry) = resolve_codex_key_config(cfg, auth)
        && let Some(disabled) = entry.disable_codex_cloaking
    {
        return disabled;
    }
    cfg.codex.disable_codex_cloaking
}

/// Go `strconv.ParseBool`.
pub fn parse_bool(raw: &str) -> Option<bool> {
    match raw {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// Whether the credential enables the upstream websocket transport: the `websockets` attribute
/// wins when it parses, else the metadata value (bool or string) (Go: codexWebsocketsEnabled).
pub fn websockets_enabled(auth: &Auth) -> bool {
    if let Some(raw) = auth.attributes.get("websockets")
        && !raw.trim().is_empty()
        && let Some(parsed) = parse_bool(raw.trim())
    {
        return parsed;
    }
    match auth.metadata.get("websockets") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => parse_bool(s.trim()).unwrap_or(false),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_key_auth(key: &str, base: &str, index: Option<usize>) -> Auth {
        let mut auth = Auth::new("a", "codex");
        auth.attributes.insert("api_key".into(), key.into());
        if !base.is_empty() {
            auth.attributes.insert("base_url".into(), base.into());
        }
        if let Some(i) = index {
            auth.attributes.insert(ATTRIBUTE_CONFIG_INDEX.into(), i.to_string());
        }
        auth
    }

    #[test]
    fn key_config_matches_by_index_then_key_and_base() {
        let cfg = Config {
            codex_key: vec![
                CodexKey { api_key: "sk-1".into(), base_url: "https://a".into(), ..Default::default() },
                CodexKey { api_key: "SK-2".into(), base_url: "https://b".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        assert_eq!(resolve_codex_key_config(&cfg, &api_key_auth("sk-2", "https://b", Some(1))).unwrap().api_key, "SK-2");
        // A stale index falls back to key + base matching.
        assert_eq!(resolve_codex_key_config(&cfg, &api_key_auth("sk-1", "https://a", Some(1))).unwrap().api_key, "sk-1");
        assert!(resolve_codex_key_config(&cfg, &api_key_auth("sk-3", "", None)).is_none());
    }

    #[test]
    fn websockets_flag_prefers_attribute() {
        let mut auth = Auth::new("a", "codex");
        assert!(!websockets_enabled(&auth));
        auth.metadata.insert("websockets".into(), Value::String("true".into()));
        assert!(websockets_enabled(&auth));
        auth.attributes.insert("websockets".into(), "false".into());
        assert!(!websockets_enabled(&auth));
    }

    /// Go `TestCodexV8HistoricalCloakingAliasAffectsBothAuthKinds`.
    #[test]
    fn historical_cloaking_alias_affects_both_auth_kinds() {
        for (name, settings, key_option, want_api) in [
            ("legacy global", "codex: {disable-codex-cloaking: true}\n", "", true),
            ("historical alias", "oauth: {providers: {codex: {disable-codex-cloaking: true}}}\n", "", true),
            ("explicit key override", "oauth: {providers: {codex: {disable-codex-cloaking: true}}}\n", ", disable-codex-cloaking: false", false),
        ] {
            let raw = format!(
                "{settings}api-keys: {{codex: [{{name: independent, base-url: 'https://example.invalid/v1', keys: [{{api-key: test-key{key_option}}}]}}]}}\n"
            );
            let cfg = cpa_config::parse_config_bytes(raw.as_bytes()).expect("config");
            let mut oauth = Auth::new("oauth", "codex");
            oauth.metadata.insert("access_token".into(), Value::String("test-oauth".into()));
            assert!(is_cloaking_disabled(&cfg, &oauth), "{name}: OAuth setting was not applied");
            let key = api_key_auth("test-key", "https://example.invalid/v1", None);
            assert_eq!(is_cloaking_disabled(&cfg, &key), want_api, "{name}: wrong API-key cloaking policy");
            assert!(cfg.codex.disable_codex_cloaking, "{name}: shared configuration was mutated");
        }
    }

    #[test]
    fn cloaking_precedence_attr_then_entry_then_global() {
        let mut cfg = Config::default();
        cfg.codex.disable_codex_cloaking = true;
        let mut auth = api_key_auth("sk-1", "", None);
        // API-key credentials ignore OAuth-only scoped settings, not the plain global one.
        assert!(is_cloaking_disabled(&cfg, &auth));
        auth.attributes.insert(ATTRIBUTE_CODEX_DISABLE_CLOAKING.into(), "false".into());
        assert!(!is_cloaking_disabled(&cfg, &auth));
    }
}
