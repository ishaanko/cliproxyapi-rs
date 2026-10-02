//! Antigravity capability probe (Go: sdk/cliproxy/antigravity_models.go).
//!
//! After an Antigravity auth registers its static catalog, the service asks
//! `fetchAvailableModels` which models can run native web search and flags them in the registry.
//! Results are cached per `(base urls, proxy)` for 5 minutes (1 minute after transient failures),
//! auth failures back off per `(auth id, token)`, and concurrent probes for the same key share one
//! request.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cpa_auth::Auth;
use cpa_auth::http::build_client;
use cpa_auth::singleflight::SingleFlight;
use cpa_config::Config;
use parking_lot::Mutex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::task::JoinSet;

use super::models::{oauth_model_alias_channel, oauth_model_aliases_for_auth};

const BASE_URL_DAILY: &str = "https://daily-cloudcode-pa.googleapis.com";
const MODELS_PATH: &str = "/v1internal:fetchAvailableModels";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const FAILURE_TTL: Duration = Duration::from_secs(60);
/// Bound on re-probes with the caller's own token after a shared probe failed with another
/// account's credentials (Go recurses without a bound).
const MAX_OWN_TOKEN_RETRIES: usize = 3;

/// Lowercased ids of models that support native web search.
pub type WebSearchHints = HashSet<String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeStatus {
    Success,
    AuthError,
    TransientError,
}

#[derive(Clone)]
struct ProbeResult {
    hints: WebSearchHints,
    status: ProbeStatus,
    /// Token the (possibly shared) probe ran with.
    token: String,
}

struct CacheEntry {
    hints: WebSearchHints,
    expires_at: Instant,
}

#[derive(Default)]
struct Caches {
    by_key: HashMap<String, CacheEntry>,
    auth_failures: HashMap<String, Instant>,
}

impl Caches {
    fn purge_expired(&mut self, now: Instant) {
        self.by_key.retain(|_, e| now < e.expires_at);
        self.auth_failures.retain(|_, exp| now < *exp);
    }
}

/// Probes `fetchAvailableModels` with caching, failure backoff and request sharing.
#[derive(Clone, Default)]
pub struct Prober {
    caches: Arc<Mutex<Caches>>,
    flights: Arc<SingleFlight<ProbeResult>>,
}

/// Go `normalizeAntigravityFetchedModelID`.
fn normalize_model_id(id: &str) -> String {
    id.trim().to_lowercase()
}

/// Go `parseAntigravityModelCapabilityHints`: the `webSearchModelIds` of a response body.
/// Mirrors `json.Unmarshal` into `struct{ WebSearchModelIDs []string }`: the body must be an
/// object (or `null`), the field (matched case-insensitively) an array of strings or `null`.
pub fn parse_hints(body: &[u8]) -> Option<WebSearchHints> {
    let parsed: Value = serde_json::from_slice(body).ok()?;
    let ids = match &parsed {
        Value::Null => None,
        Value::Object(map) => map
            .iter()
            .rev()
            .find(|(k, _)| k.eq_ignore_ascii_case("webSearchModelIds"))
            .map(|(_, v)| v),
        _ => return None,
    };
    let ids = match ids {
        Some(Value::Array(ids)) => ids.as_slice(),
        Some(Value::Null) | None => &[],
        Some(_) => return None,
    };
    let mut hints = WebSearchHints::new();
    for id in ids {
        let id = normalize_model_id(id.as_str()?);
        if !id.is_empty() {
            hints.insert(id);
        }
    }
    Some(hints)
}

fn resolve_base_url(auth: &Auth) -> String {
    let attr = auth.attr("base_url");
    if !attr.is_empty() {
        return attr.trim_end_matches('/').to_string();
    }
    let meta = auth.meta_str("base_url");
    meta.trim_end_matches('/').to_string()
}

/// Go `antigravityModelBaseURLs`: `base_urls` attribute, else `base_url`, else the daily host.
pub fn model_base_urls(auth: &Auth) -> Vec<String> {
    let raw = auth.attr("base_urls");
    if !raw.is_empty() {
        let urls: Vec<String> = raw
            .split(',')
            .map(|p| p.trim().trim_end_matches('/').to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if !urls.is_empty() {
            return urls;
        }
    }
    let base = resolve_base_url(auth);
    if base.is_empty() { vec![BASE_URL_DAILY.to_string()] } else { vec![base] }
}

/// Go `buildAntigravityReverseAliasMap`: lowercased alias -> upstream name for the auth.
pub fn reverse_alias_map(cfg: &Config, auth: &Auth) -> HashMap<String, String> {
    let channel = oauth_model_alias_channel(&auth.provider, auth.auth_kind());
    oauth_model_aliases_for_auth(cfg, &channel, &auth.attributes)
        .iter()
        .filter_map(|a| {
            let alias = a.alias.trim().to_lowercase();
            let upstream = a.name.trim().to_lowercase();
            (!alias.is_empty() && !upstream.is_empty()).then_some((alias, upstream))
        })
        .collect()
}

/// Go `resolveAntigravityUpstreamModelID`: strips the auth prefix and maps an alias back to the
/// upstream model id (lowercased).
pub fn resolve_upstream_model_id(model_id: &str, prefix: &str, alias_map: &HashMap<String, String>) -> String {
    let model_id = model_id.trim().to_lowercase();
    let prefix = prefix.trim().trim_matches('/').to_lowercase();
    let unprefixed = if !prefix.is_empty() {
        model_id.strip_prefix(&format!("{prefix}/")).unwrap_or(&model_id).to_string()
    } else {
        model_id
    };
    match alias_map.get(&unprefixed) {
        Some(upstream) if !upstream.is_empty() => upstream.clone(),
        _ => unprefixed,
    }
}

async fn fetch_from_url(client: &reqwest::Client, base_url: &str, token: &str) -> (WebSearchHints, ProbeStatus) {
    let url = format!("{}{MODELS_PATH}", base_url.trim_end_matches('/'));
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", cpa_core::misc::antigravity_user_agent())
        .body("{}")
        .send()
        .await;
    let Ok(response) = response else {
        return (WebSearchHints::new(), ProbeStatus::TransientError);
    };
    let status = response.status();
    if status == 401 || status == 403 {
        return (WebSearchHints::new(), ProbeStatus::AuthError);
    }
    let Ok(body) = response.bytes().await else {
        return (WebSearchHints::new(), ProbeStatus::TransientError);
    };
    if !status.is_success() {
        return (WebSearchHints::new(), ProbeStatus::TransientError);
    }
    match parse_hints(&body) {
        Some(hints) => (hints, ProbeStatus::Success),
        None => (WebSearchHints::new(), ProbeStatus::TransientError),
    }
}

/// Go `probeAntigravityModelCapabilityHints`: one base URL probes directly, several race and the
/// first success with web-search hints wins.
async fn probe(base_urls: Vec<String>, proxy_url: String, token: String) -> (WebSearchHints, ProbeStatus) {
    let work = async {
        let Ok(client) = build_client(&proxy_url, Some(PROBE_TIMEOUT)) else {
            return (WebSearchHints::new(), ProbeStatus::TransientError);
        };
        if base_urls.len() == 1 {
            return fetch_from_url(&client, &base_urls[0], &token).await;
        }
        let mut set = JoinSet::new();
        for url in base_urls {
            let (client, token) = (client.clone(), token.clone());
            set.spawn(async move { fetch_from_url(&client, &url, &token).await });
        }
        let mut first_success: Option<WebSearchHints> = None;
        let mut overall = ProbeStatus::TransientError;
        while let Some(joined) = set.join_next().await {
            let Ok((hints, status)) = joined else { continue };
            match status {
                ProbeStatus::Success if !hints.is_empty() => return (hints, ProbeStatus::Success),
                ProbeStatus::Success => {
                    first_success.get_or_insert(hints);
                }
                ProbeStatus::AuthError => overall = ProbeStatus::AuthError,
                ProbeStatus::TransientError => {}
            }
        }
        match first_success {
            Some(hints) => (hints, ProbeStatus::Success),
            None => (WebSearchHints::new(), overall),
        }
    };
    tokio::time::timeout(PROBE_TIMEOUT, work)
        .await
        .unwrap_or((WebSearchHints::new(), ProbeStatus::TransientError))
}

impl Prober {
    /// Go `fetchAntigravityModelCapabilityHintsForAuth`. `global_proxy` is the config `proxy-url`
    /// used when the auth has none. Empty hints on any failure.
    pub async fn fetch_hints(&self, auth: &Auth, global_proxy: &str) -> WebSearchHints {
        let token = auth.meta_str("access_token");
        if token.is_empty() {
            return WebSearchHints::new();
        }
        let base_urls = model_base_urls(auth);
        let proxy_url = match auth.proxy_url.trim() {
            "" => global_proxy.trim().to_string(),
            own => own.to_string(),
        };
        let cache_key = format!("{}#{proxy_url}", base_urls.join("|"));
        let token_hash = hex::encode(Sha256::digest(token.as_bytes()));
        let fail_key = format!("{}#{token_hash}", auth.id);

        for _ in 0..=MAX_OWN_TOKEN_RETRIES {
            let now = Instant::now();
            {
                let caches = self.caches.lock();
                if caches.auth_failures.get(&fail_key).is_some_and(|exp| now < *exp) {
                    return WebSearchHints::new();
                }
                if let Some(entry) = caches.by_key.get(&cache_key).filter(|e| now < e.expires_at) {
                    return entry.hints.clone();
                }
            }

            let result = {
                let (caches, key) = (self.caches.clone(), cache_key.clone());
                let (urls, proxy, own_token) = (base_urls.clone(), proxy_url.clone(), token.clone());
                self.flights
                    .run(&cache_key, move || async move {
                        let now = Instant::now();
                        if let Some(entry) = caches.lock().by_key.get(&key).filter(|e| now < e.expires_at) {
                            return Ok(ProbeResult {
                                hints: entry.hints.clone(),
                                status: ProbeStatus::Success,
                                token: own_token,
                            });
                        }
                        let (hints, status) = probe(urls, proxy, own_token.clone()).await;
                        if status != ProbeStatus::AuthError {
                            let ttl = if status == ProbeStatus::Success { CACHE_TTL } else { FAILURE_TTL };
                            let mut caches = caches.lock();
                            caches.purge_expired(now);
                            caches.by_key.insert(key, CacheEntry { hints: hints.clone(), expires_at: Instant::now() + ttl });
                        }
                        Ok(ProbeResult { hints, status, token: own_token })
                    })
                    .await
            };
            let Ok(result) = result else {
                return WebSearchHints::new();
            };
            if result.status == ProbeStatus::AuthError {
                if result.token != token {
                    // The shared probe ran with another account's token; retry with ours.
                    continue;
                }
                let mut caches = self.caches.lock();
                caches.purge_expired(now);
                caches.auth_failures.insert(fail_key, Instant::now() + FAILURE_TTL);
                return WebSearchHints::new();
            }
            return result.hints;
        }
        WebSearchHints::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_are_lowercased_and_malformed_bodies_rejected() {
        let hints = parse_hints(br#"{"webSearchModelIds":[" Gemini-3-Pro ","","x"]}"#).unwrap();
        assert_eq!(hints, ["gemini-3-pro", "x"].iter().map(|s| s.to_string()).collect());
        assert!(parse_hints(b"{}").unwrap().is_empty());
        assert!(parse_hints(b"not json").is_none());
        assert!(parse_hints(br#"{"webSearchModelIds":"x"}"#).is_none());
        assert!(parse_hints(b"[1]").is_none());
        assert!(parse_hints(br#"{"webSearchModelIds":[1]}"#).is_none());
        assert!(parse_hints(b"null").unwrap().is_empty());
    }

    #[test]
    fn upstream_id_strips_prefix_then_maps_alias() {
        let aliases: HashMap<String, String> = [("fast".to_string(), "gemini-3-flash".to_string())].into();
        assert_eq!(resolve_upstream_model_id("Team/Fast", "team", &aliases), "gemini-3-flash");
        assert_eq!(resolve_upstream_model_id("fast", "", &aliases), "gemini-3-flash");
        assert_eq!(resolve_upstream_model_id("other", "team", &aliases), "other");
    }

    #[test]
    fn base_urls_prefer_attribute_list_then_single_then_daily() {
        let mut auth = Auth::new("a.json", "antigravity");
        assert_eq!(model_base_urls(&auth), [BASE_URL_DAILY]);
        auth.attributes.insert("base_url".into(), "https://x.example/".into());
        assert_eq!(model_base_urls(&auth), ["https://x.example"]);
        auth.attributes.insert("base_urls".into(), "https://a/, https://b".into());
        assert_eq!(model_base_urls(&auth), ["https://a", "https://b"]);
    }
}
