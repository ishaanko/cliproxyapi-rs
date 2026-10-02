//! Fingerprint and wire policy resolution (Go: claude_fingerprint_policy.go and the policy part of
//! claude_executor_cloaking.go).
//!
//! The fingerprint policy follows the credential (real OAuth token or the `claude-code-cli`
//! profile); the wire policy decides per request whether the body is cloaked.

use std::collections::HashSet;
use std::sync::LazyLock;

use cpa_auth::Auth;
use cpa_auth::types::AUTH_KIND_API_KEY;
use cpa_config::{
    CLAUDE_FINGERPRINT_PROFILE_CLAUDE_CODE_CLI, CLAUDE_FINGERPRINT_PROFILE_DEFAULT, Config,
    normalize_claude_fingerprint_profile,
};
use parking_lot::Mutex;

use super::request::is_claude_oauth_token;
use super::signing::{resolve_claude_key_cloak_config, resolve_claude_key_config};

const CLAUDE_FINGERPRINT_PROFILE_ATTR: &str = "fingerprint_profile";

/// Deduplicates the unrecognized-profile warning: resolution runs several times per request.
static PROFILE_WARNED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Single switch-driven view of the Claude fingerprint behavior for Messages requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClaudeFingerprintPolicy {
    pub auth_is_oauth_token: bool,
    pub profile_claude_code_cli: bool,
    pub use_oauth_betas: bool,
    pub apply_cli_identity: bool,
    pub synthesize_identity: bool,
    pub mcp_alias: bool,
    pub inject_diagnostics: bool,
    pub oauth_cancellation: bool,
}

fn normalize_profile(raw: &str) -> &'static str {
    let (profile, ok) = normalize_claude_fingerprint_profile(raw);
    if !ok && PROFILE_WARNED.lock().insert(raw.trim().to_string()) {
        tracing::warn!(
            "unrecognized claude fingerprint-profile {:?} (supported: {:?}); falling back to default",
            raw,
            CLAUDE_FINGERPRINT_PROFILE_CLAUDE_CODE_CLI
        );
    }
    profile
}

/// Profile named on the credential itself (attribute first, then credential JSON).
fn fingerprint_profile_from_auth(auth: &Auth) -> &'static str {
    if let Some(raw) = auth.attributes.get(CLAUDE_FINGERPRINT_PROFILE_ATTR)
        && !raw.trim().is_empty()
    {
        return normalize_profile(raw);
    }
    for key in [CLAUDE_FINGERPRINT_PROFILE_ATTR, "fingerprint-profile"] {
        let raw = auth.meta_str(key);
        if !raw.trim().is_empty() {
            return normalize_profile(&raw);
        }
    }
    CLAUDE_FINGERPRINT_PROFILE_DEFAULT
}

fn fingerprint_profile_from_config(cfg: &Config, auth: &Auth) -> &'static str {
    let profile = fingerprint_profile_from_auth(auth);
    if profile != CLAUDE_FINGERPRINT_PROFILE_DEFAULT {
        return profile;
    }
    match resolve_claude_key_config(cfg, auth) {
        Some(entry) => normalize_profile(&entry.fingerprint_profile),
        None => CLAUDE_FINGERPRINT_PROFILE_DEFAULT,
    }
}

/// Credential-scoped fingerprint behavior (Go: resolveClaudeFingerprintPolicy). Independent of the
/// upstream origin; CCH signing is decided separately by `claude_cch_signing_enabled`.
pub fn resolve_claude_fingerprint_policy(cfg: &Config, auth: &Auth, api_key: &str) -> ClaudeFingerprintPolicy {
    let auth_is_oauth = is_claude_oauth_token(api_key);
    let profile = fingerprint_profile_from_config(cfg, auth);
    let profile_cli = auth_is_oauth || profile == CLAUDE_FINGERPRINT_PROFILE_CLAUDE_CODE_CLI;
    ClaudeFingerprintPolicy {
        auth_is_oauth_token: auth_is_oauth,
        profile_claude_code_cli: profile_cli,
        use_oauth_betas: profile_cli,
        apply_cli_identity: profile_cli,
        synthesize_identity: profile_cli && !auth_is_oauth,
        mcp_alias: profile_cli,
        inject_diagnostics: profile_cli,
        oauth_cancellation: auth_is_oauth,
    }
}

/// Per-request wire decisions (Go: claudeWirePolicy).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClaudeWirePolicy {
    /// Real OAuth token runtime identity.
    pub oauth: bool,
    /// Request fingerprint looks like the Claude Code CLI.
    pub profile_claude_code_cli: bool,
    pub confirmed_claude_code: bool,
    pub cloak: bool,
}

/// Cloak knobs resolved from credential attributes, credential JSON and config
/// (Go: claudeCloakSettings).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeCloakSettings {
    pub strict_mode: bool,
    pub sensitive_words: Vec<String>,
    pub cache_user_id: bool,
}

/// Cloak configuration read from the auth attributes, falling back to its stored metadata
/// (Go: getCloakConfigFromAuth). Returns `(cloak_mode, strict_mode, sensitive_words,
/// cache_user_id)`; an empty mode means the credential did not configure one.
pub fn get_cloak_config_from_auth(auth: &Auth) -> (String, bool, Vec<String>, bool) {
    let lookup = |key: &str| -> String {
        if let Some(value) = auth.attributes.get(key) {
            let value = value.trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
        auth.meta_str(key).trim().to_string()
    };
    let cloak_mode = lookup("cloak_mode");
    let strict_mode = lookup("cloak_strict_mode").eq_ignore_ascii_case("true");
    let words = lookup("cloak_sensitive_words");
    let sensitive_words = if words.is_empty() {
        Vec::new()
    } else {
        words.split(',').map(|w| w.trim().to_string()).collect()
    };
    let cache_user_id = lookup("cloak_cache_user_id").eq_ignore_ascii_case("true");
    (cloak_mode, strict_mode, sensitive_words, cache_user_id)
}

/// Decides whether a request is cloaked and with which settings (Go: resolveClaudeWirePolicy).
pub fn resolve_claude_wire_policy(
    cfg: &Config,
    auth: &Auth,
    api_key: &str,
    confirmed_claude_code: bool,
) -> (ClaudeWirePolicy, ClaudeCloakSettings) {
    let scoped;
    let cfg = if auth.auth_kind() == AUTH_KIND_API_KEY {
        scoped = cfg.for_api_key();
        &*scoped
    } else {
        cfg
    };
    let cloak_cfg = resolve_claude_key_cloak_config(cfg, auth);
    let (attr_mode, attr_strict, attr_words, attr_cache) = get_cloak_config_from_auth(auth);

    let mut cloak_mode = if cfg.disable_claude_cloak_mode { "never".to_string() } else { "auto".to_string() };
    let mut settings = ClaudeCloakSettings {
        strict_mode: attr_strict,
        sensitive_words: attr_words.clone(),
        cache_user_id: attr_cache,
    };
    if !attr_mode.is_empty() {
        cloak_mode = attr_mode.clone();
    }
    if let Some(cloak) = cloak_cfg {
        let mode = cloak.mode.trim();
        if !mode.is_empty() {
            cloak_mode = mode.to_string();
        }
        if cloak.strict_mode {
            settings.strict_mode = true;
        }
        if !cloak.sensitive_words.is_empty() {
            settings.sensitive_words = cloak.sensitive_words.clone();
        }
        if let Some(cache) = cloak.cache_user_id {
            settings.cache_user_id = cache;
        }
    }

    let fp = resolve_claude_fingerprint_policy(cfg, auth, api_key);
    let cloak_configured = cloak_cfg.is_some()
        || !attr_mode.is_empty()
        || attr_strict
        || !attr_words.is_empty()
        || attr_cache;
    let mut policy = ClaudeWirePolicy {
        oauth: fp.auth_is_oauth_token,
        profile_claude_code_cli: fp.profile_claude_code_cli,
        confirmed_claude_code,
        cloak: (fp.profile_claude_code_cli || cloak_configured) && !confirmed_claude_code,
    };
    if confirmed_claude_code {
        // Native Claude Code is always a passthrough client, even under mode "always".
        policy.cloak = false;
        return (policy, settings);
    }
    match cloak_mode.trim().to_lowercase().as_str() {
        "always" => policy.cloak = true,
        "never" => policy.cloak = false,
        // Auto keeps the default: cloak only OAuth, profile opt-ins and explicit cloak settings.
        _ => {}
    }
    (policy, settings)
}
