//! Shared HTTP plumbing: proxy resolution (`sdk/proxyutil` + `util.SetProxy`) and a default client.
//!
//! These clients use plain rustls. Claude's OAuth control plane layers the fingerprinted
//! transport of `claude_transport` on top of them.

use std::time::Duration;

use crate::error::AuthFlowError;

/// How a per-auth or global proxy string is interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxySetting {
    /// Empty: inherit (environment proxies, like Go's default transport).
    Inherit,
    /// `direct` / `none`: bypass every proxy.
    Direct,
    Proxy(String),
}

/// `proxyutil.Parse`: classifies a proxy string, rejecting unsupported schemes.
pub fn parse_proxy(raw: &str) -> Result<ProxySetting, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(ProxySetting::Inherit);
    }
    if trimmed.eq_ignore_ascii_case("direct") || trimmed.eq_ignore_ascii_case("none") {
        return Ok(ProxySetting::Direct);
    }
    let url = url::Url::parse(trimmed).map_err(|_| "parse proxy URL failed".to_string())?;
    if url.host_str().unwrap_or("").is_empty() {
        return Err("proxy URL missing scheme/host".into());
    }
    match url.scheme() {
        "socks5" | "socks5h" | "http" | "https" => Ok(ProxySetting::Proxy(trimmed.to_string())),
        other => Err(format!("unsupported proxy scheme: {other}")),
    }
}

/// Builds a rustls client honoring `proxy_url` (per-auth override first, global fallback is the
/// caller's job). An invalid proxy string is logged and ignored, like Go's `SetProxy`.
pub fn build_client(
    proxy_url: &str,
    timeout: Option<Duration>,
) -> Result<reqwest::Client, AuthFlowError> {
    build_client_ext(proxy_url, timeout, None)
}

/// [`build_client`] with an optional connect (TCP + TLS handshake) timeout.
pub fn build_client_ext(
    proxy_url: &str,
    timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
) -> Result<reqwest::Client, AuthFlowError> {
    let mut builder = reqwest::Client::builder().use_rustls_tls();
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }
    if let Some(t) = connect_timeout {
        builder = builder.connect_timeout(t);
    }
    match parse_proxy(proxy_url) {
        Ok(ProxySetting::Inherit) => {}
        Ok(ProxySetting::Direct) => builder = builder.no_proxy(),
        Ok(ProxySetting::Proxy(p)) => {
            let proxy = reqwest::Proxy::all(&p)
                .map_err(|e| AuthFlowError::Config(format!("invalid proxy: {e}")))?;
            builder = builder.proxy(proxy);
        }
        Err(e) => tracing::error!("{e}"),
    }
    builder
        .build()
        .map_err(|e| AuthFlowError::Config(format!("build http client: {e}")))
}

/// Reads a response body as text (lossy), returning `(status, body)`.
pub(crate) async fn read_text(resp: reqwest::Response) -> Result<(u16, String), reqwest::Error> {
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await?;
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_parsing_matches_go_rules() {
        assert_eq!(parse_proxy("").unwrap(), ProxySetting::Inherit);
        assert_eq!(parse_proxy(" Direct ").unwrap(), ProxySetting::Direct);
        assert_eq!(parse_proxy("none").unwrap(), ProxySetting::Direct);
        assert!(matches!(
            parse_proxy("socks5://u:p@h:1080").unwrap(),
            ProxySetting::Proxy(_)
        ));
        assert!(parse_proxy("ftp://h:1").is_err());
        assert!(parse_proxy("justahost").is_err());
    }
}
