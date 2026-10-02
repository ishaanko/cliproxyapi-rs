//! HTTP transport for Claude upstreams (Go: helps.NewUtlsHTTPClient without the TLS fingerprint).
//!
//! Anthropic is reached over HTTP/1.1 like the native Node client. Unlike the shared executor
//! client this one decodes every `Content-Encoding` the Claude wire profile advertises
//! (`gzip, deflate, br, zstd`). TLS fingerprinting is not reproduced (reqwest + rustls).

use std::sync::LazyLock;
use std::time::Duration;

use cpa_auth::Auth;
use cpa_auth::http::{ProxySetting, parse_proxy};
use cpa_config::Config;
use cpa_runtime::executor::ExecError;
use http::HeaderMap;

use crate::helps::proxy::{BoundedLru, DEFAULT_TRANSPORT_CACHE_CAPACITY, effective_proxy_url};

static CLIENTS: LazyLock<BoundedLru<String, reqwest::Client>> =
    LazyLock::new(|| BoundedLru::new(DEFAULT_TRANSPORT_CACHE_CAPACITY));

fn build_client(setting: &ProxySetting) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .http1_only()
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

/// Cached client for the execution's effective proxy (request override, credential, global).
/// No total timeout: streams run as long as the upstream keeps sending.
pub fn claude_http_client(request_proxy: &str, cfg: &Config, auth: &Auth) -> reqwest::Client {
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
/// (Go: doClaudeUpstreamRequest; wire casing and header order are not reproduced).
pub async fn send_messages(
    client: &reqwest::Client,
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
        .map_err(|e| ExecError::new(0, describe_send_error(&e)))
}

/// Full cause chain of a failed send (`error sending request: client error (Connect): tcp connect
/// error: Connection refused (os error 111)`), so the conductor can recognize transient dial and
/// reset failures by text. The URL is dropped to keep it out of client-visible errors.
fn describe_send_error(err: &reqwest::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        parts.push(cause.to_string());
        source = cause.source();
    }
    let top = parts.remove(0);
    let top = top.split(" for url").next().unwrap_or(&top).to_string();
    std::iter::once(top).chain(parts).collect::<Vec<_>>().join(": ")
}

/// Message for a response-body read failure. Go surfaces a body cut short as `unexpected EOF`
/// and the conductor retries on that text, so the incomplete-message errors of hyper map to it;
/// anything else keeps the innermost cause (for example `connection reset by peer`).
pub fn describe_body_error(err: &reqwest::Error) -> String {
    let mut chain = vec![err.to_string()];
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        chain.push(cause.to_string());
        source = cause.source();
    }
    let lower = chain.join(": ").to_lowercase();
    const INCOMPLETE: [&str; 5] = [
        "unexpected eof",
        "unexpected end of file",
        "connection closed before message completed",
        "end of file before message length reached",
        "incomplete message",
    ];
    if INCOMPLETE.iter().any(|needle| lower.contains(needle)) {
        return "unexpected EOF".to_string();
    }
    chain.pop().unwrap_or_default()
}
