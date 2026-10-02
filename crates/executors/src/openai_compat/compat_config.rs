//! Resolution of the `openai-compatibility` config entry that serves a credential (Go:
//! `OpenAICompatExecutor.resolveCompatConfig`).

use std::borrow::Cow;

use cpa_auth::Auth;
use cpa_auth::types::AUTH_SOURCE_CONFIG;
use cpa_config::{Config, OpenAiCompatibility, OpenAiCompatibilityModel};
use cpa_runtime::executor::Request;
use serde::Deserialize;

/// Home mode keeps provider credentials out of the global config; the non-secret options travel
/// with the credential instead (metadata `credential_options`).
#[derive(Default, Deserialize)]
#[serde(default)]
struct HomeOptions {
    #[serde(rename = "support-prompt-cache-key")]
    support_prompt_cache_key: bool,
    models: Vec<OpenAiCompatibilityModel>,
}

/// Go `strconv.ParseBool`.
fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// The config entry for `auth`: Home credential options, else the entry at `config_index`
/// (config-sourced auths), else the first enabled entry whose name matches `compat_name`,
/// `provider_key` or the auth provider (case-insensitive).
pub fn resolve_compat_config<'a>(
    cfg: &'a Config,
    auth: &Auth,
    _req: &Request,
) -> Option<Cow<'a, OpenAiCompatibility>> {
    if cfg.home.enabled {
        let mut options = HomeOptions::default();
        let mut present = false;
        if let Some(raw) = auth.metadata.get("credential_options") {
            match serde_json::from_value::<HomeOptions>(raw.clone()) {
                Ok(parsed) => {
                    options = parsed;
                    present = true;
                }
                Err(_) => present = false,
            }
        }
        if let Some(raw) = auth.attributes.get("support_prompt_cache_key")
            && let Some(value) = parse_bool(raw)
        {
            options.support_prompt_cache_key = value;
            present = true;
        }
        if present {
            return Some(Cow::Owned(OpenAiCompatibility {
                name: auth.attributes.get("compat_name").cloned().unwrap_or_default(),
                support_prompt_cache_key: options.support_prompt_cache_key,
                models: options.models,
                ..Default::default()
            }));
        }
    }
    if auth.auth_source_kind() == AUTH_SOURCE_CONFIG {
        let raw = auth.attr("config_index");
        if let Ok(index) = raw.parse::<usize>()
            && let Some(compat) = cfg.openai_compatibility.get(index)
            && !compat.disabled
        {
            return Some(Cow::Borrowed(compat));
        }
    }
    let mut candidates = Vec::with_capacity(3);
    for key in ["compat_name", "provider_key"] {
        let v = auth.attr(key);
        if !v.is_empty() {
            candidates.push(v);
        }
    }
    let provider = auth.provider.trim();
    if !provider.is_empty() {
        candidates.push(provider.to_string());
    }
    cfg.openai_compatibility
        .iter()
        .filter(|compat| !compat.disabled)
        .find(|compat| candidates.iter().any(|c| c.trim().eq_ignore_ascii_case(&compat.name)))
        .map(Cow::Borrowed)
}
