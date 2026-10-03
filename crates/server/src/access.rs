//! Client API-key authentication (Go: sdk/access + internal/access/config_access).

use std::collections::BTreeMap;

use axum::http::HeaderMap;
use cpa_plugin::CallCtx;
use cpa_plugin::{AccessAdapter, AccessFailure};

/// Provider id of the inline `api-keys` provider.
pub const DEFAULT_ACCESS_PROVIDER_NAME: &str = "config-inline";

/// A successfully authenticated caller (`access.Result`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub provider: String,
    /// The matching API key (or the principal a plugin provider reported).
    pub principal: String,
    /// Provider metadata; the inline provider reports where the key was found under `source`:
    /// `authorization`, `x-goog-api-key`, `x-api-key`, `query-key`, `query-auth-token`.
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthFailure {
    /// No credential was presented.
    Missing,
    /// A credential was presented but matched no key.
    Invalid,
    /// A provider failed with an internal error (status 500).
    Internal(String),
}

impl AuthFailure {
    pub fn message(&self) -> &str {
        match self {
            AuthFailure::Missing => "Missing API key",
            AuthFailure::Invalid => "Invalid API key",
            AuthFailure::Internal(m) => m,
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            AuthFailure::Internal(_) => 500,
            _ => 401,
        }
    }
}

/// `normalizeKeys` as an iterator: trimmed, non-empty keys (duplicates do not matter for lookups).
fn usable_keys(keys: &[String]) -> impl Iterator<Item = &str> {
    keys.iter().map(|k| k.trim()).filter(|k| !k.is_empty())
}

/// `extractBearerToken`: `Bearer <key>` (scheme case-insensitive), otherwise the whole value.
pub fn extract_bearer_token(header: &str) -> String {
    if header.is_empty() {
        return String::new();
    }
    match header.split_once(' ') {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => rest.trim().to_string(),
        _ => header.to_string(),
    }
}

fn first_header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn first_query(query: &[(String, String)], name: &str) -> String {
    query
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

/// Authenticates a request against the configured keys. `Ok(None)` means no keys are configured
/// and every request is allowed. Candidates are tried in Go's order: `Authorization`,
/// `X-Goog-Api-Key`, `X-Api-Key`, query `key`, query `auth_token`; the first that is an exact key
/// wins.
pub fn authenticate(
    headers: &HeaderMap,
    query: &[(String, String)],
    api_keys: &[String],
) -> Result<Option<Principal>, AuthFailure> {
    if usable_keys(api_keys).next().is_none() {
        return Ok(None);
    }
    let auth_header = first_header(headers, "authorization");
    let google = first_header(headers, "x-goog-api-key");
    let anthropic = first_header(headers, "x-api-key");
    let query_key = first_query(query, "key");
    let query_token = first_query(query, "auth_token");
    if [&auth_header, &google, &anthropic, &query_key, &query_token].iter().all(|v| v.is_empty()) {
        return Err(AuthFailure::Missing);
    }
    let bearer = extract_bearer_token(&auth_header);
    let candidates: [(&str, &'static str); 5] = [
        (bearer.as_str(), "authorization"),
        (google.as_str(), "x-goog-api-key"),
        (anthropic.as_str(), "x-api-key"),
        (query_key.as_str(), "query-key"),
        (query_token.as_str(), "query-auth-token"),
    ];
    for (value, source) in candidates {
        if value.is_empty() {
            continue;
        }
        if usable_keys(api_keys).any(|k| k == value) {
            return Ok(Some(Principal {
                provider: DEFAULT_ACCESS_PROVIDER_NAME.to_string(),
                principal: value.to_string(),
                metadata: BTreeMap::from([("source".to_string(), source.to_string())]),
            }));
        }
    }
    Err(AuthFailure::Invalid)
}

/// The request parts a plugin frontend auth provider sees.
pub struct AccessRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub headers: &'a HeaderMap,
    pub query: &'a [(String, String)],
}

/// Go `Manager.Authenticate` over the registered providers: the inline api-key provider (when
/// keys exist) followed by plugin frontend providers; an exclusive plugin provider replaces the
/// whole chain. `body` is only awaited when a plugin provider is consulted. `Ok(None)` means no
/// provider is registered and every request is allowed.
pub async fn authenticate_chain<B, F>(
    req: &AccessRequest<'_>,
    api_keys: &[String],
    plugins: &[std::sync::Arc<AccessAdapter>],
    plugin_exclusive: bool,
    body: B,
) -> Result<Option<Principal>, AuthFailure>
where
    B: FnOnce() -> F,
    F: std::future::Future<Output = Result<bytes::Bytes, String>>,
{
    let inline = !plugin_exclusive && usable_keys(api_keys).next().is_some();
    if !inline && plugins.is_empty() {
        return Ok(None);
    }
    let mut invalid = false;
    if inline {
        match authenticate(req.headers, req.query, api_keys) {
            Ok(Some(p)) => return Ok(Some(p)),
            Ok(None) => {}
            Err(AuthFailure::Missing) => {}
            Err(AuthFailure::Invalid) => invalid = true,
            Err(e) => return Err(e),
        }
    }
    if !plugins.is_empty() {
        let Ok(body) = body().await else {
            return Err(AuthFailure::Internal("failed to read plugin auth request body".into()));
        };
        let ctx = CallCtx::background();
        for provider in plugins {
            match provider.authenticate(&ctx, req.method, req.path, req.headers, req.query, &body).await {
                Ok(r) => {
                    return Ok(Some(Principal { provider: r.provider, principal: r.principal, metadata: r.metadata }));
                }
                Err(AccessFailure::NotHandled | AccessFailure::NoCredentials) => {}
                Err(AccessFailure::InvalidCredential) => invalid = true,
                Err(AccessFailure::Internal { message }) => return Err(AuthFailure::Internal(message)),
            }
        }
    }
    Err(if invalid { AuthFailure::Invalid } else { AuthFailure::Missing })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn q(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn keys() -> Vec<String> {
        vec!["k1".into(), " k2 ".into(), "".into(), "k1".into()]
    }

    #[test]
    fn open_when_no_keys_configured() {
        assert_eq!(authenticate(&HeaderMap::new(), &[], &[]), Ok(None));
        assert_eq!(authenticate(&HeaderMap::new(), &[], &["  ".into()]), Ok(None));
    }

    #[test]
    fn missing_vs_invalid() {
        assert_eq!(authenticate(&HeaderMap::new(), &[], &keys()), Err(AuthFailure::Missing));
        assert_eq!(
            authenticate(&headers(&[("x-api-key", "nope")]), &[], &keys()),
            Err(AuthFailure::Invalid)
        );
    }

    #[test]
    fn lookup_order_and_sources() {
        let p = authenticate(&headers(&[("authorization", "Bearer k1")]), &[], &keys()).unwrap().unwrap();
        assert_eq!((p.principal.as_str(), p.metadata["source"].as_str()), ("k1", "authorization"));
        // raw key without scheme and other schemes use the whole header value
        let p = authenticate(&headers(&[("authorization", "k2")]), &[], &keys()).unwrap().unwrap();
        assert_eq!(p.principal, "k2");
        assert_eq!(
            authenticate(&headers(&[("authorization", "Basic k1")]), &[], &keys()),
            Err(AuthFailure::Invalid)
        );
        // an invalid Authorization does not stop later candidates from matching
        let p = authenticate(
            &headers(&[("authorization", "Bearer bad"), ("x-goog-api-key", "k2")]),
            &[],
            &keys(),
        )
        .unwrap()
        .unwrap();
        assert_eq!((p.principal.as_str(), p.metadata["source"].as_str()), ("k2", "x-goog-api-key"));
        // header candidates win over query candidates
        let p = authenticate(&headers(&[("x-api-key", "k1")]), &q(&[("key", "k2")]), &keys()).unwrap().unwrap();
        assert_eq!((p.principal.as_str(), p.metadata["source"].as_str()), ("k1", "x-api-key"));
        let p = authenticate(&HeaderMap::new(), &q(&[("key", "bad"), ("auth_token", "k2")]), &keys())
            .unwrap()
            .unwrap();
        assert_eq!(p.metadata["source"], "query-auth-token");
    }

    #[test]
    fn bearer_scheme_is_case_insensitive_and_trimmed() {
        assert_eq!(extract_bearer_token("bEaReR   abc "), "abc");
        assert_eq!(extract_bearer_token("abc"), "abc");
        assert_eq!(extract_bearer_token(""), "");
    }
}
