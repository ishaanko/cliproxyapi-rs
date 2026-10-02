//! Per-request facts handlers need (the parts of gin's `Context` the Go handlers read).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequestParts, MatchedPath, OriginalUri};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};

use crate::access::Principal;
use crate::clientip;
use crate::reqlog::{ApiLog, ApiLogHandle};
use crate::state::AppState;

/// Request id generated for AI API paths (UUIDv7); stored as a request extension.
#[derive(Debug, Clone, Default)]
pub struct RequestId(pub String);

/// Principal set by the API-key middleware (gin key `userApiKey`).
#[derive(Debug, Clone)]
pub struct AuthenticatedKey(pub Principal);

#[derive(Debug, Clone)]
pub struct ReqInfo {
    pub method: Method,
    /// Route pattern (`c.FullPath()`), else the URI path.
    pub route: String,
    pub path: String,
    pub raw_query: String,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub remote: Option<SocketAddr>,
    pub client_ip: String,
    pub api_key: Option<String>,
    pub request_id: String,
    /// Shared request-log context (upstream errors, responses, websocket timeline).
    pub api_log: Arc<ApiLog>,
}

impl ReqInfo {
    /// `c.Query(name)`: first value.
    pub fn query_first(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// `GetAlt`: `alt`, else `$alt`; `sse` counts as empty.
    pub fn alt(&self) -> String {
        let alt = match self.query_first("alt") {
            Some(v) => v,
            None => self.query_first("$alt").unwrap_or(""),
        };
        if alt == "sse" { String::new() } else { alt.to_string() }
    }

    /// `strings.TrimSpace(c.GetHeader(name))`.
    pub fn header(&self, name: &str) -> String {
        crate::headers::header_trimmed(&self.headers, name)
    }

    /// `c.GetHeader(name)` without trimming.
    pub fn header_raw(&self, name: &str) -> &str {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
    }

    /// `"<METHOD> <route>"` used for usage records.
    pub fn endpoint(&self) -> String {
        format!("{} {}", self.method, self.route)
    }

    /// Remote address host only (`requestClientIP`).
    pub fn remote_host(&self) -> String {
        self.remote.map(|a| a.ip().to_string()).unwrap_or_default()
    }
}

/// Parses a raw query string like `url.ParseQuery` (values percent-decoded, `+` as space).
pub fn parse_query(raw: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(raw.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

impl FromRequestParts<AppState> for ReqInfo {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        // Nested routers rewrite the URI; handlers and logs want the path the client sent.
        let original_path = parts
            .extensions
            .get::<OriginalUri>()
            .map(|u| u.0.path().to_string())
            .unwrap_or_else(|| parts.uri.path().to_string());
        let route = parts
            .extensions
            .get::<MatchedPath>()
            .map(|m| m.as_str().to_string())
            .unwrap_or_else(|| original_path.clone());
        let remote = parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
        let cfg = state.cfg();
        let client_ip = clientip::resolve(remote.map(|a| a.ip()), &parts.headers, &cfg.trusted_proxies);
        let raw_query = parts.uri.query().unwrap_or("").to_string();
        Ok(ReqInfo {
            method: parts.method.clone(),
            route,
            path: original_path,
            query: parse_query(&raw_query),
            raw_query,
            headers: parts.headers.clone(),
            remote,
            client_ip,
            api_key: parts
                .extensions
                .get::<AuthenticatedKey>()
                .map(|k| k.0.principal.clone()),
            request_id: parts
                .extensions
                .get::<RequestId>()
                .map(|r| r.0.clone())
                .unwrap_or_default(),
            api_log: parts
                .extensions
                .get::<ApiLogHandle>()
                .map(|h| h.0.clone())
                .unwrap_or_default(),
        })
    }
}

/// IP helper re-exported for logging code.
pub fn ip_to_string(ip: Option<IpAddr>) -> String {
    ip.map(|i| i.to_string()).unwrap_or_default()
}
