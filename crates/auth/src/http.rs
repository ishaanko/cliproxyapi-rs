//! Shared HTTP plumbing: proxy resolution (`sdk/proxyutil` + `util.SetProxy`) and a default client.
//!
//! Gap vs Go: Claude's OAuth/inference plane uses a uTLS (Firefox ClientHello) transport and an
//! Axios-ordered header set. reqwest + rustls cannot fingerprint the TLS ClientHello, so Claude
//! OAuth calls here use plain rustls. Header values are kept; header order and `compress`
//! content-encoding are not reproduced.

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

/// `proxyutil.Parse`: classifies a proxy string, rejecting unsupported schemes (the Go
/// `net/url` acceptance rules live in `cpa_misc::proxyutil`).
pub fn parse_proxy(raw: &str) -> Result<ProxySetting, String> {
    match cpa_misc::proxyutil::parse(raw) {
        Ok(setting) => Ok(match setting.mode {
            cpa_misc::proxyutil::Mode::Direct => ProxySetting::Direct,
            cpa_misc::proxyutil::Mode::Proxy => ProxySetting::Proxy(setting.raw),
            _ => ProxySetting::Inherit,
        }),
        Err((_, err)) => Err(err.to_string()),
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
        // Go's url.Parse rejects a malformed escape in the userinfo.
        assert_eq!(parse_proxy("http://user:secret%@h:1").unwrap_err(), "parse proxy URL failed");
    }
}
