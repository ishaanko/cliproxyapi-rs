//! Meta credential resolution (Go: meta_executor.go `metaCreds`, `enrichAuth`).

use cpa_auth::Auth;
use cpa_auth::meta::DEFAULT_API_BASE_URL;
use cpa_auth::storage::TokenStorage;
use serde_json::Value;

use super::USER_AGENT;

fn meta_str(auth: &Auth, key: &str) -> String {
    auth.metadata.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// A usable (non-DCA) token candidate.
fn usable(token: &str) -> bool {
    !token.is_empty() && !token.starts_with("dca:")
}

/// Upstream base URL and API key of an auth. Attributes win over metadata, metadata over the
/// token storage; DCA tokens (`dca:` prefix) never count as an API key (Go: metaCreds).
pub fn meta_creds(auth: Option<&Auth>) -> (String, String) {
    let mut base_url = DEFAULT_API_BASE_URL.to_string();
    let mut token = String::new();
    let Some(a) = auth else {
        return (base_url, token);
    };

    let attr_base = a.attr("base_url");
    if !attr_base.is_empty() {
        base_url = attr_base;
    }
    let attr_key = a.attr("api_key");
    let attr_access = a.attr("access_token");
    if usable(&attr_key) {
        token = attr_key;
    } else if usable(&attr_access) {
        token = attr_access;
    }

    if base_url == DEFAULT_API_BASE_URL {
        let b = meta_str(a, "base_url");
        if !b.is_empty() {
            base_url = b;
        } else {
            let b = meta_str(a, "api_base_url");
            if !b.is_empty() {
                base_url = b;
            }
        }
    }
    if token.is_empty() {
        let key = meta_str(a, "api_key");
        let access = meta_str(a, "access_token");
        if usable(&key) {
            token = key;
        } else if usable(&access) {
            token = access;
        }
    }
    if token.is_empty()
        && let Some(TokenStorage::Meta(s)) = &a.storage
    {
        if !s.api_key.is_empty() {
            token = s.api_key.clone();
        } else if !s.access_token.is_empty() && !s.access_token.starts_with("dca:") {
            token = s.access_token.clone();
        }
        if !s.base_url.is_empty() && base_url == DEFAULT_API_BASE_URL {
            base_url = s.base_url.clone();
        }
    }
    (base_url, token)
}

/// Clone of `auth` whose attributes carry the resolved base URL, API key and the default
/// `User-Agent` custom header, so request building only reads attributes (Go: enrichAuth).
pub fn enrich_auth(auth: &Auth) -> Auth {
    let (base_url, token) = meta_creds(Some(auth));
    let mut cloned = auth.clone();
    if cloned.attr("base_url").is_empty() {
        cloned.attributes.insert("base_url".into(), base_url);
    }
    if cloned.attr("api_key").is_empty() {
        cloned.attributes.insert("api_key".into(), token);
    }
    cloned.attributes.entry("header:User-Agent".into()).or_insert_with(|| USER_AGENT.to_string());
    cloned
}
