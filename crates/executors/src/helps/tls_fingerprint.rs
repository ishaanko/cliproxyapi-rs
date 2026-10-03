//! Provider-specific TLS fingerprints for protected hosts (Go: `helps.NewUtlsHTTPClient`).
//!
//! * `https://api.anthropic.com[:443]` uses the Claude Code ClientHello with pooled HTTP/1.1 and
//!   the native header order.
//! * `https://chatgpt.com` uses the Chrome ClientHello, one connection per request, HTTP/1.1 or
//!   HTTP/2 by ALPN.
//! * Everything else goes through the caller's standard reqwest client (the Go fallback
//!   transport), which is also the only path when the `tls-fingerprint` feature is off.
//!
//! Like Go, the fingerprinted transports never read proxy environment variables: an empty proxy
//! setting dials directly. Building them requires BoringSSL (cmake, a C++ compiler and libclang);
//! see `cpa-tlsfp`.

use cpa_auth::Auth;
use cpa_config::Config;
use cpa_runtime::executor::ExecError;
use http::HeaderMap;

use crate::helps::proxy::effective_proxy_url;
use crate::helps::status::error_chain_text;

/// Failure of a request sent through [`UtlsClient`].
#[derive(Debug)]
pub enum UtlsError {
    /// The standard transport failed (or the request could not be built).
    Reqwest(reqwest::Error),
    /// A fingerprinted transport failed; the text already carries the full cause chain.
    Fingerprint(String),
}

impl UtlsError {
    /// Status-less executor error for this failure, with the cause text the conductor classifies
    /// transient transport failures by.
    pub fn exec_error(&self) -> ExecError {
        match self {
            UtlsError::Reqwest(e) => crate::helps::status::transport_error(e),
            UtlsError::Fingerprint(text) => ExecError::new(0, text.clone()),
        }
    }
}

impl std::fmt::Display for UtlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UtlsError::Reqwest(e) => f.write_str(&error_chain_text(e)),
            UtlsError::Fingerprint(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for UtlsError {}

/// Which transport serves a URL (Go: `fallbackRoundTripper`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Anthropic,
    Chrome,
    Standard,
}

/// `helps.IsAnthropicUpstreamURL`: https, no userinfo, host api.anthropic.com, port 443.
pub fn is_anthropic_upstream_url(url: &url::Url) -> bool {
    url.username().is_empty()
        && url.password().is_none()
        && url.scheme().eq_ignore_ascii_case("https")
        && url.host_str().is_some_and(|h| h.eq_ignore_ascii_case("api.anthropic.com"))
        && url.port().is_none_or(|p| p == 443)
}

fn route(url: &url::Url) -> Route {
    if is_anthropic_upstream_url(url) {
        Route::Anthropic
    } else if url.scheme() == "https" && url.host_str().is_some_and(|h| h.eq_ignore_ascii_case("chatgpt.com")) {
        Route::Chrome
    } else {
        Route::Standard
    }
}

#[cfg(feature = "tls-fingerprint")]
mod fingerprinted {
    use std::sync::LazyLock;

    use cpa_tlsfp::{ClientConfig, FingerprintClient};

    use crate::helps::proxy::{BoundedLru, DEFAULT_TRANSPORT_CACHE_CAPACITY};

    /// Claude Code round trippers cached per proxy URL (Go: `claudeCodeRoundTripperCache`, 64).
    static ANTHROPIC: LazyLock<BoundedLru<String, FingerprintClient>> =
        LazyLock::new(|| BoundedLru::new(DEFAULT_TRANSPORT_CACHE_CAPACITY));
    static CHROME: LazyLock<BoundedLru<String, FingerprintClient>> =
        LazyLock::new(|| BoundedLru::new(DEFAULT_TRANSPORT_CACHE_CAPACITY));

    pub fn anthropic(proxy: &str) -> Result<FingerprintClient, String> {
        ANTHROPIC.get_or_build(proxy.to_string(), || {
            FingerprintClient::claude_inference(ClientConfig { proxy: proxy.to_string(), ..ClientConfig::default() })
                .map_err(|e| e.to_string())
        })
    }

    pub fn chrome(proxy: &str) -> Result<FingerprintClient, String> {
        CHROME.get_or_build(proxy.to_string(), || {
            FingerprintClient::chrome(ClientConfig { proxy: proxy.to_string(), ..ClientConfig::default() })
                .map_err(|e| e.to_string())
        })
    }
}

/// HTTP client that picks a TLS fingerprint by destination (Go: the client returned by
/// `NewUtlsHTTPClient`). Cheap to build; the fingerprinted transports are cached per proxy.
#[derive(Clone)]
pub struct UtlsClient {
    #[cfg_attr(not(feature = "tls-fingerprint"), allow(dead_code))]
    proxy: String,
    fallback: reqwest::Client,
}

/// Go: `NewUtlsHTTPClient`. `fallback` is the standard transport for hosts without a
/// fingerprint; the proxy priority (request, credential, global) is resolved here.
pub fn new_utls_http_client(
    request_proxy: &str,
    cfg: Option<&Config>,
    auth: Option<&Auth>,
    fallback: reqwest::Client,
) -> UtlsClient {
    UtlsClient { proxy: effective_proxy_url(request_proxy, auth, cfg), fallback }
}

impl UtlsClient {
    pub fn post(&self, url: &str) -> UtlsRequestBuilder {
        UtlsRequestBuilder { client: self.clone(), inner: self.fallback.post(url) }
    }

    /// Sends `req` over the transport its URL selects.
    pub async fn execute(&self, req: reqwest::Request) -> Result<reqwest::Response, UtlsError> {
        match route(req.url()) {
            #[cfg(feature = "tls-fingerprint")]
            Route::Anthropic => {
                let client = fingerprinted::anthropic(&self.proxy).map_err(UtlsError::Fingerprint)?;
                client.execute(req).await.map_err(|e| UtlsError::Fingerprint(e.to_string()))
            }
            #[cfg(feature = "tls-fingerprint")]
            Route::Chrome => {
                let client = fingerprinted::chrome(&self.proxy).map_err(UtlsError::Fingerprint)?;
                client.execute(req).await.map_err(|e| UtlsError::Fingerprint(e.to_string()))
            }
            _ => self.fallback.execute(req).await.map_err(UtlsError::Reqwest),
        }
    }
}

/// The slice of `reqwest::RequestBuilder` the executors use, sent through [`UtlsClient`].
pub struct UtlsRequestBuilder {
    client: UtlsClient,
    inner: reqwest::RequestBuilder,
}

impl UtlsRequestBuilder {
    pub fn headers(mut self, headers: HeaderMap) -> Self {
        self.inner = self.inner.headers(headers);
        self
    }

    pub fn body(mut self, body: impl Into<reqwest::Body>) -> Self {
        self.inner = self.inner.body(body);
        self
    }

    pub async fn send(self) -> Result<reqwest::Response, UtlsError> {
        let req = self.inner.build().map_err(UtlsError::Reqwest)?;
        self.client.execute(req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route_of(url: &str) -> Route {
        route(&url::Url::parse(url).unwrap())
    }

    #[test]
    fn routes_like_the_go_fallback_round_tripper() {
        assert_eq!(route_of("https://api.anthropic.com/v1/messages"), Route::Anthropic);
        assert_eq!(route_of("https://API.Anthropic.com:443/v1/messages"), Route::Anthropic);
        assert_eq!(route_of("https://api.anthropic.com:8443/v1/messages"), Route::Standard);
        assert_eq!(route_of("http://api.anthropic.com/v1/messages"), Route::Standard);
        assert_eq!(route_of("https://u:p@api.anthropic.com/v1/messages"), Route::Standard);
        assert_eq!(route_of("https://chatgpt.com/backend-api/codex/responses"), Route::Chrome);
        assert_eq!(route_of("https://chatgpt.com:444/x"), Route::Chrome);
        assert_eq!(route_of("http://chatgpt.com/x"), Route::Standard);
        assert_eq!(route_of("https://api.openai.com/v1"), Route::Standard);
    }
}
