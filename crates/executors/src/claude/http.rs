//! HTTP transport for Claude upstreams (Go: helps.NewUtlsHTTPClient).
//!
//! First-party `https://api.anthropic.com` requests use the Claude Code TLS fingerprint and wire
//! header order (`helps::tls_fingerprint`); any other URL (custom base URLs, test servers) goes
//! through a reqwest client over HTTP/1.1 like the native Node client. Unlike the shared executor
//! client the standard one decodes every `Content-Encoding` the Claude wire profile advertises
//! (`gzip, deflate, br, zstd`).

use std::sync::LazyLock;
use std::time::Duration;

use cpa_auth::Auth;
use cpa_auth::http::{ProxySetting, parse_proxy};
use cpa_config::Config;
use cpa_runtime::executor::ExecError;
use http::HeaderMap;

use crate::helps::proxy::{BoundedLru, DEFAULT_TRANSPORT_CACHE_CAPACITY, effective_proxy_url};
use crate::helps::tls_fingerprint::{UtlsClient, new_utls_http_client};

static CLIENTS: LazyLock<BoundedLru<String, reqwest::Client>> =
    LazyLock::new(|| BoundedLru::new(DEFAULT_TRANSPORT_CACHE_CAPACITY));

fn build_client(setting: &ProxySetting) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .http1_only()
        .http1_max_buf_size(crate::helps::proxy::UPSTREAM_HTTP1_MAX_BUF)
        .connect_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90));
    match setting {
        ProxySetting::Inherit => {}
        ProxySetting::Direct => builder = builder.no_proxy(),
        ProxySetting::Proxy(url) => {
            let url = match url.strip_prefix("socks5://") {
                Some(rest) => format!("socks5h://{rest}"),
                None => url.clone(),
            };
            builder = builder.proxy(reqwest::Proxy::all(url)?);
        }
    }
    builder.build()
}

/// Client for the execution's effective proxy (request override, credential, global): the TLS
/// fingerprinted transports for first-party hosts, the cached standard client for the rest.
/// No total timeout: streams run as long as the upstream keeps sending.
pub fn claude_http_client(request_proxy: &str, cfg: &Config, auth: &Auth) -> UtlsClient {
    let fallback = standard_client(request_proxy, cfg, auth);
    new_utls_http_client(request_proxy, Some(cfg), Some(auth), fallback)
}

fn standard_client(request_proxy: &str, cfg: &Config, auth: &Auth) -> reqwest::Client {
    let proxy_url = effective_proxy_url(request_proxy, Some(auth), Some(cfg));
    let (setting, key) = match parse_proxy(&proxy_url) {
        Ok(ProxySetting::Inherit) => (ProxySetting::Inherit, String::new()),
        Ok(ProxySetting::Direct) => (ProxySetting::Direct, "direct".to_string()),
        Ok(ProxySetting::Proxy(p)) => (ProxySetting::Proxy(p.clone()), p),
        Err(err) => {
            tracing::debug!("failed to setup proxy for claude upstream, falling back to default transport ({err})");
            (ProxySetting::Inherit, String::new())
        }
    };
    match CLIENTS.get_or_build(key, || build_client(&setting)) {
        Ok(client) => client,
        Err(err) => {
            tracing::error!("failed to build claude http client: {err}");
            reqwest::Client::new()
        }
    }
}

/// POSTs a Messages / count_tokens body with the assembled headers
/// (Go: doClaudeUpstreamRequest; first-party requests get the native wire casing and order).
pub async fn send_messages(
    client: &UtlsClient,
    url: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<reqwest::Response, ExecError> {
    client
        .post(url)
        .headers(headers.clone())
        .body(body.to_vec())
        .send()
        .await
        .map_err(|e| e.exec_error())
}
