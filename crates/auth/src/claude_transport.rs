//! Transport of the Claude OAuth control plane (Go: `NewAnthropicHttpClient` in
//! auth/claude/utls_transport.go).
//!
//! Every `https` request goes through the Claude Code OAuth TLS profile (compact BoringSSL
//! ClientHello without ALPN, Axios header order, per-proxy TLS session cache) from `cpa-tlsfp`;
//! plain `http` URLs (local test servers) and builds without the `tls-fingerprint` feature use
//! the reqwest client. Like Go's transport, the fingerprinted path never reads proxy environment
//! variables: an empty proxy setting dials directly.

use std::time::Duration;

/// One request channel: a reqwest client that builds requests (and serves non-TLS URLs) plus the
/// optional fingerprinted transport for `https` URLs.
#[derive(Clone)]
pub(crate) struct Channel {
    client: reqwest::Client,
    #[cfg(feature = "tls-fingerprint")]
    fp: Option<cpa_tlsfp::FingerprintClient>,
}

impl Channel {
    /// Plain reqwest channel (tests, shared transports).
    pub(crate) fn plain(client: reqwest::Client) -> Self {
        Self {
            client,
            #[cfg(feature = "tls-fingerprint")]
            fp: None,
        }
    }

    /// Fingerprinted channel for `proxy_url` (empty dials directly). `handshake_timeout` bounds
    /// each TLS handshake (Go: the refresh-only context deadline).
    #[cfg_attr(not(feature = "tls-fingerprint"), allow(unused_variables))]
    pub(crate) fn fingerprinted(
        client: reqwest::Client,
        proxy_url: &str,
        handshake_timeout: Option<Duration>,
    ) -> Self {
        #[cfg(feature = "tls-fingerprint")]
        {
            let config = cpa_tlsfp::ClientConfig {
                proxy: proxy_url.to_string(),
                handshake_timeout,
                ..cpa_tlsfp::ClientConfig::default()
            };
            let fp = match cpa_tlsfp::FingerprintClient::claude_oauth(config) {
                Ok(fp) => Some(fp),
                Err(e) => {
                    tracing::error!("claude oauth tls: {e}; using the standard transport");
                    None
                }
            };
            Self { client, fp }
        }
        #[cfg(not(feature = "tls-fingerprint"))]
        Self::plain(client)
    }

    pub(crate) fn post(&self, url: &str) -> reqwest::RequestBuilder {
        self.client.post(url)
    }

    pub(crate) fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.client.get(url)
    }

    /// Sends a request built from this channel; the error is the display text without the URL.
    pub(crate) async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
        let request = request.build().map_err(|e| e.without_url().to_string())?;
        #[cfg(feature = "tls-fingerprint")]
        if let Some(fp) = &self.fp
            && request.url().scheme() == "https"
        {
            return fp.execute(request).await.map_err(|e| e.to_string());
        }
        self.client.execute(request).await.map_err(|e| e.without_url().to_string())
    }
}
