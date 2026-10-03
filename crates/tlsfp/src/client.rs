//! HTTP client over the fingerprinted TLS profiles (Go: the round trippers of
//! helps/utls_client.go and auth/claude/utls_transport.go).
//!
//! * Claude inference and Claude OAuth: pooled HTTP/1.1 keep-alive connections (`ForceAttemptHTTP2:
//!   false`), request heads rewritten to the native header order, TLS session resumption.
//! * Chrome: one dedicated connection per request, protocol chosen by ALPN (`h2`, `http/1.1` or
//!   none), the connection closes with the response body.
//!
//! Known gap: HTTP/2 pseudo-headers go out in the `h2` crate's order (`:method, :scheme,
//! :authority, :path`), not Go's (`:authority, :method, :path, :scheme`); see `send_h2`.
//!
//! Requests come in as `reqwest::Request` and go out as `reqwest::Response`, so call sites keep
//! building requests with reqwest and only swap `send()` for [`FingerprintClient::execute`].
//! Like Go's transports, no proxy environment variables are consulted: the proxy comes from the
//! explicit setting only.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt};
use http::header::{ACCEPT_ENCODING, CONNECTION, CONTENT_ENCODING, CONTENT_LENGTH, HOST, RANGE, USER_AGENT};
use http::{HeaderName, HeaderValue, Method, Uri};
use http_body_util::{BodyDataStream, Full};
use hyper::body::Incoming;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::io::{ReaderStream, StreamReader};

use crate::dial::{BoxIo, Dialer};
use crate::ordered::{HeaderOrder, HeaderRewriter, OrderedConn};
use crate::profile::{Profile, TlsConnector};

/// Request or transport failure; the text carries the full cause chain.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Request(String),
    #[error("{0}")]
    Transport(String),
    #[error("request timed out")]
    Timeout,
}

/// Knobs shared by every constructor.
#[derive(Debug, Clone, Default)]
pub struct ClientConfig {
    /// Proxy setting in `proxyutil` syntax; empty or invalid dials directly.
    pub proxy: String,
    /// Total deadline per request including the body (Go: `http.Client.Timeout`).
    pub timeout: Option<Duration>,
    /// Deadline for the TLS handshake of each new connection.
    pub handshake_timeout: Option<Duration>,
    /// Extra trusted root certificates (DER), on top of the system roots.
    pub extra_roots: Vec<Vec<u8>>,
}

const CLAUDE_INFERENCE_SESSIONS: usize = 32;
const CLAUDE_OAUTH_SESSIONS: usize = 8;
const CLAUDE_OAUTH_PROXY_CACHES: usize = 64;

/// Claude Code inference header order (Go: `claudeCodeMessagesHeaderOrder` and the count_tokens
/// variant, which omits `X-Stainless-Timeout`).
pub fn claude_code_header_order(_method: &str, target: &str) -> &'static [&'static str] {
    const MESSAGES: &[&str] = &[
        "Accept",
        "Authorization",
        "Content-Type",
        "User-Agent",
        "X-Claude-Code-Session-Id",
        "X-Stainless-Arch",
        "X-Stainless-Lang",
        "X-Stainless-OS",
        "X-Stainless-Package-Version",
        "X-Stainless-Retry-Count",
        "X-Stainless-Runtime",
        "X-Stainless-Runtime-Version",
        "X-Stainless-Timeout",
        "anthropic-beta",
        "anthropic-dangerous-direct-browser-access",
        "anthropic-version",
        "x-app",
        "x-client-request-id",
        "Connection",
        "Host",
        "Accept-Encoding",
        "Content-Length",
    ];
    const COUNT_TOKENS: &[&str] = &[
        "Accept",
        "Authorization",
        "Content-Type",
        "User-Agent",
        "X-Claude-Code-Session-Id",
        "X-Stainless-Arch",
        "X-Stainless-Lang",
        "X-Stainless-OS",
        "X-Stainless-Package-Version",
        "X-Stainless-Retry-Count",
        "X-Stainless-Runtime",
        "X-Stainless-Runtime-Version",
        "anthropic-beta",
        "anthropic-dangerous-direct-browser-access",
        "anthropic-version",
        "x-app",
        "x-client-request-id",
        "Connection",
        "Host",
        "Accept-Encoding",
        "Content-Length",
    ];
    if target.starts_with("/v1/messages/count_tokens") { COUNT_TOKENS } else { MESSAGES }
}

/// Claude OAuth control plane header order (Go: `claudeOAuthRequestHeaderOrder`): authenticated
/// profile and roles GETs use the inspect order, everything else the refresh order.
pub fn claude_oauth_header_order(method: &str, target: &str) -> &'static [&'static str] {
    const REFRESH: &[&str] = &["Accept", "Content-Type", "User-Agent", "Content-Length", "Accept-Encoding", "Host", "Connection"];
    const INSPECT: &[&str] =
        &["Accept", "Content-Type", "Authorization", "Cache-Control", "User-Agent", "Accept-Encoding", "Host", "Connection"];
    const INSPECT_TARGETS: [&str; 2] = ["/api/oauth/profile", "/api/oauth/claude_cli/roles"];
    if method == "GET" && INSPECT_TARGETS.iter().any(|t| target.starts_with(t)) { INSPECT } else { REFRESH }
}

/// Global per-proxy OAuth TLS connectors (Go: `claudeOAuthSessionCaches`), keyed by proxy and
/// extra trusted roots (empty outside tests). Clients are built per operation there, so the
/// session cache must outlive them to ever resume. A cached session is only valid for the SSL
/// context that produced it, so the whole [`TlsConnector`] (context plus its private session
/// cache) is shared; each client still gets its own connection pool.
type OauthKey = (String, Vec<Vec<u8>>);
static OAUTH_TLS: Mutex<Vec<(OauthKey, TlsConnector)>> = Mutex::new(Vec::new());

/// The shared OAuth connector for `proxy`, built on first use.
fn oauth_tls(proxy: &str, extra_roots: &[Vec<u8>]) -> Result<TlsConnector, crate::profile::TlsError> {
    let hit = |caches: &mut Vec<(OauthKey, TlsConnector)>| {
        let pos = caches.iter().position(|((p, roots), _)| p == proxy && roots == extra_roots)?;
        let entry = caches.remove(pos);
        let tls = entry.1.clone();
        caches.push(entry);
        Some(tls)
    };
    if let Some(tls) = hit(&mut OAUTH_TLS.lock()) {
        return Ok(tls);
    }
    // Built outside the lock; a racing builder's entry wins so all clients share one context.
    let fresh = TlsConnector::new(Profile::ClaudeOAuth, Some(CLAUDE_OAUTH_SESSIONS), extra_roots)?;
    let mut caches = OAUTH_TLS.lock();
    if let Some(tls) = hit(&mut caches) {
        return Ok(tls);
    }
    caches.push(((proxy.to_string(), extra_roots.to_vec()), fresh.clone()));
    if caches.len() > CLAUDE_OAUTH_PROXY_CACHES {
        caches.remove(0);
    }
    Ok(fresh)
}

/// Dial + handshake for one profile; the hyper connector of the pooled clients and the
/// per-request path of the Chrome client.
#[derive(Clone)]
struct Connector {
    profile: Profile,
    dialer: Arc<Dialer>,
    tls: TlsConnector,
    order: Option<HeaderOrder>,
    handshake_timeout: Option<Duration>,
}

type Tls = rama_boring_tokio::SslStream<BoxIo>;

impl Connector {
    fn dial_error(&self, e: impl std::fmt::Display) -> Error {
        Error::Transport(match self.profile {
            Profile::Chrome => format!("utls: dial upstream: {e}"),
            Profile::ClaudeInference => format!("claude tls: dial upstream: {e}"),
            Profile::ClaudeOAuth => format!("claude oauth tls: dial upstream: {e}"),
        })
    }

    fn handshake_error(&self, e: impl std::fmt::Display) -> Error {
        Error::Transport(match self.profile {
            Profile::Chrome => format!("utls: TLS handshake: {e}"),
            Profile::ClaudeInference => format!("claude tls: handshake upstream: {e}"),
            Profile::ClaudeOAuth => format!("claude oauth tls: handshake upstream: {e}"),
        })
    }

    async fn connect(&self, host: &str, port: u16) -> Result<Tls, Error> {
        let tcp = self.dialer.dial(host, port).await.map_err(|e| self.dial_error(e))?;
        let handshake = self.tls.connect(host, tcp);
        match self.handshake_timeout {
            Some(limit) => match tokio::time::timeout(limit, handshake).await {
                Ok(r) => r.map_err(|e| self.handshake_error(e)),
                Err(_) => Err(self.handshake_error("context deadline exceeded")),
            },
            None => handshake.await.map_err(|e| self.handshake_error(e)),
        }
    }
}

/// A connection handed to hyper's pool: the TLS stream, header-ordered when the profile asks.
struct PooledConn(TokioIo<OrderedOrPlain>);

enum OrderedOrPlain {
    Ordered(OrderedConn<Tls>),
    Plain(Tls),
}

impl tokio::io::AsyncRead for OrderedOrPlain {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Ordered(c) => Pin::new(c).poll_read(cx, buf),
            Self::Plain(c) => Pin::new(c).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for OrderedOrPlain {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Ordered(c) => Pin::new(c).poll_write(cx, buf),
            Self::Plain(c) => Pin::new(c).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Ordered(c) => Pin::new(c).poll_flush(cx),
            Self::Plain(c) => Pin::new(c).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Ordered(c) => Pin::new(c).poll_shutdown(cx),
            Self::Plain(c) => Pin::new(c).poll_shutdown(cx),
        }
    }
}

impl hyper::rt::Read for PooledConn {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: hyper::rt::ReadBufCursor<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for PooledConn {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl Connection for PooledConn {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl tower_service::Service<Uri> for Connector {
    type Response = PooledConn;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<PooledConn>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let host = uri.host().ok_or_else(|| io::Error::other("missing host"))?.trim_matches(['[', ']']).to_string();
            let port = uri.port_u16().unwrap_or(443);
            let tls = this.connect(&host, port).await.map_err(|e| io::Error::other(e.to_string()))?;
            let io = match this.order {
                Some(order) => OrderedOrPlain::Ordered(OrderedConn::new(tls, order)),
                None => OrderedOrPlain::Plain(tls),
            };
            Ok(PooledConn(TokioIo::new(io)))
        })
    }
}

type PooledClient = hyper_util::client::legacy::Client<Connector, Full<Bytes>>;

enum Mode {
    Pooled(Box<PooledClient>),
    Dedicated(Connector),
}

/// TLS fingerprinted client; cheap to clone.
#[derive(Clone)]
pub struct FingerprintClient {
    mode: Arc<Mode>,
    timeout: Option<Duration>,
}

impl FingerprintClient {
    /// Claude Code inference profile for api.anthropic.com (Go: `newClaudeCodeRoundTripper`).
    pub fn claude_inference(cfg: ClientConfig) -> Result<Self, crate::profile::TlsError> {
        let tls = TlsConnector::new(Profile::ClaudeInference, Some(CLAUDE_INFERENCE_SESSIONS), &cfg.extra_roots)?;
        Self::pooled(Profile::ClaudeInference, Some(claude_code_header_order), tls, cfg)
    }

    /// Claude OAuth control plane profile (Go: `newUtlsRoundTripper` in auth/claude). Clients of
    /// one proxy share the TLS context and session cache but not the connection pool.
    pub fn claude_oauth(cfg: ClientConfig) -> Result<Self, crate::profile::TlsError> {
        let tls = oauth_tls(cfg.proxy.trim(), &cfg.extra_roots)?;
        Self::pooled(Profile::ClaudeOAuth, Some(claude_oauth_header_order), tls, cfg)
    }

    /// Chrome 133 profile, one connection per request (Go: `utlsRoundTripper`).
    pub fn chrome(cfg: ClientConfig) -> Result<Self, crate::profile::TlsError> {
        let tls = TlsConnector::new(Profile::Chrome, None, &cfg.extra_roots)?;
        let connector = Self::connector(Profile::Chrome, None, tls, &cfg);
        Ok(Self { mode: Arc::new(Mode::Dedicated(connector)), timeout: cfg.timeout })
    }

    fn connector(profile: Profile, order: Option<HeaderOrder>, tls: TlsConnector, cfg: &ClientConfig) -> Connector {
        let label = match profile {
            Profile::Chrome => "utls",
            Profile::ClaudeInference => "claude tls",
            Profile::ClaudeOAuth => "claude oauth tls",
        };
        Connector {
            profile,
            dialer: Arc::new(Dialer::from_setting(&cfg.proxy, label)),
            tls,
            order,
            handshake_timeout: cfg.handshake_timeout,
        }
    }

    fn pooled(
        profile: Profile,
        order: Option<HeaderOrder>,
        tls: TlsConnector,
        cfg: ClientConfig,
    ) -> Result<Self, crate::profile::TlsError> {
        let connector = Self::connector(profile, order, tls, &cfg);
        let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new())
            .http1_title_case_headers(true)
            // Caps hyper's per-connection read buffer (default grows to ~400 KB on a busy stream).
            .http1_max_buf_size(32 * 1024)
            .pool_max_idle_per_host(2)
            .pool_idle_timeout(None)
            .build(connector);
        Ok(Self { mode: Arc::new(Mode::Pooled(Box::new(client))), timeout: cfg.timeout })
    }

    /// Sends `req` and returns the response once its headers arrive. The body streams and is
    /// decoded (`gzip`, `deflate`, `br`, `zstd`) like reqwest does.
    pub async fn execute(&self, req: reqwest::Request) -> Result<reqwest::Response, Error> {
        let timeout = req.timeout().copied().or(self.timeout);
        let deadline = timeout.map(|t| Instant::now() + t);
        let prepared = Prepared::new(&req, matches!(&*self.mode, Mode::Dedicated(_)))?;
        let send = self.send(prepared);
        let (response, guard) = match deadline {
            Some(d) => tokio::time::timeout_at(d, send).await.map_err(|_| Error::Timeout)??,
            None => send.await?,
        };
        Ok(into_reqwest(response, guard, deadline))
    }

    async fn send(&self, p: Prepared) -> Result<(http::Response<Incoming>, Option<AbortOnDrop>), Error> {
        match &*self.mode {
            Mode::Pooled(client) => {
                let resp = client.request(p.into_h1_request(true)?).await.map_err(|e| Error::Transport(chain(&e)))?;
                Ok((resp, None))
            }
            Mode::Dedicated(connector) => {
                let host = p.host.clone();
                let tls = connector.connect(&host, p.port).await?;
                let alpn = tls.ssl().selected_alpn_protocol().map(<[u8]>::to_vec);
                match alpn.as_deref() {
                    Some(b"h2") => self.send_h2(tls, p).await,
                    None | Some(b"") | Some(b"http/1.1") => self.send_h1(tls, p).await,
                    Some(other) => {
                        Err(Error::Transport(format!("utls: unsupported negotiated protocol {:?}", String::from_utf8_lossy(other))))
                    }
                }
            }
        }
    }

    /// HTTP/1.1 on a dedicated connection. hyper writes the headers in map order; the transport's
    /// `Connection: close` is injected after the last one, like Go's extra headers.
    async fn send_h1(&self, tls: Tls, p: Prepared) -> Result<(http::Response<Incoming>, Option<AbortOnDrop>), Error> {
        let rewriter = HeaderRewriter::new(|_, _| &[]);
        let io = if p.close_after {
            OrderedOrPlain::Ordered(OrderedConn::with_rewriter(tls, rewriter.with_trailing_header("Connection: close")))
        } else {
            OrderedOrPlain::Plain(tls)
        };
        let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
            .title_case_headers(true)
            .handshake(TokioIo::new(io))
            .await
            .map_err(|e| Error::Transport(chain(&e)))?;
        let guard = AbortOnDrop(tokio::spawn(async move {
            let _ = conn.await;
        }));
        let resp = sender.send_request(p.into_h1_request(false)?).await.map_err(|e| Error::Transport(chain(&e)))?;
        Ok((resp, Some(guard)))
    }

    /// HTTP/2 with the settings of Go's `http2.Transport` (ENABLE_PUSH 0, 4 MiB stream window,
    /// 1 MiB max frame, 10 MiB header list, 1 GiB connection window).
    ///
    /// Known gap: the `h2` crate always emits the pseudo-headers as `:method, :scheme,
    /// :authority, :path`, while Go's transport sends `:authority, :method, :path, :scheme`. The
    /// order is not configurable without forking `h2`, so the Chrome HTTP/2 frame differs from
    /// Go in that one respect (settings, window updates and regular header order do match).
    async fn send_h2(&self, tls: Tls, p: Prepared) -> Result<(http::Response<Incoming>, Option<AbortOnDrop>), Error> {
        let (mut sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .initial_stream_window_size(4 << 20)
            .initial_connection_window_size((1 << 30) + 65535)
            .max_frame_size(1 << 20)
            .max_header_list_size(10 << 20)
            .handshake(TokioIo::new(tls))
            .await
            .map_err(|e| Error::Transport(chain(&e)))?;
        let guard = AbortOnDrop(tokio::spawn(async move {
            let _ = conn.await;
        }));
        let resp = sender.send_request(p.into_h2_request()?).await.map_err(|e| Error::Transport(chain(&e)))?;
        Ok((resp, Some(guard)))
    }
}

/// Closes the connection driver when the response (and its body) is dropped.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `err` and its sources joined with `": "`, skipping sources already contained in the text.
fn chain(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let part = cause.to_string();
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        source = cause.source();
    }
    text
}

/// A request reduced to what the wire needs, with headers in Go's write order.
struct Prepared {
    method: Method,
    uri: Uri,
    host: String,
    port: u16,
    /// Value of the Host header (authority as written in the URL).
    authority: String,
    headers: Vec<(HeaderName, HeaderValue)>,
    body: Bytes,
    /// No User-Agent was given and the Go default was substituted.
    ua_defaulted: bool,
    /// HTTP/1.1 only: the transport's own `Connection: close` goes after the last header (Go:
    /// the `DisableKeepAlives` extra header). A caller's `Connection` header stays in the sorted
    /// block.
    close_after: bool,
}

/// Whether any value of the `Connection` header contains `token` (Go: `HeaderValuesContainsToken`).
fn connection_has(headers: &http::HeaderMap, token: &str) -> bool {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
}

impl Prepared {
    /// `dedicated` requests close the connection after the response (Go: `DisableKeepAlives`).
    fn new(req: &reqwest::Request, dedicated: bool) -> Result<Self, Error> {
        let url = req.url();
        let bad = |what: &str| Error::Request(format!("{what}: {url}"));
        if url.scheme() != "https" {
            return Err(bad("fingerprinted transport needs an https URL"));
        }
        let host = url.host_str().ok_or_else(|| bad("missing host"))?.trim_matches(['[', ']']).to_string();
        let port = url.port().unwrap_or(443);
        let authority = match url.port() {
            Some(p) => format!("{}:{p}", url.host_str().unwrap_or_default()),
            None => url.host_str().unwrap_or_default().to_string(),
        };
        let body = match req.body() {
            None => Bytes::new(),
            Some(b) => Bytes::copy_from_slice(b.as_bytes().ok_or_else(|| Error::Request("streaming request bodies are not supported".into()))?),
        };
        let uri: Uri = url.as_str().parse().map_err(|_| bad("invalid URI"))?;
        let method = req.method().clone();
        let src = req.headers();
        let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::with_capacity(src.len() + 4);
        // Go writes Host, User-Agent, Content-Length, then the rest sorted by name, then the
        // transport's own extra headers sorted (Accept-Encoding, Connection: close).
        let ua_defaulted = !src.contains_key(USER_AGENT);
        headers.push((USER_AGENT, src.get(USER_AGENT).cloned().unwrap_or_else(|| HeaderValue::from_static("Go-http-client/1.1"))));
        // Go: no extra `Connection: close` when the request already wants close or switches protocol.
        let protocol_switch = src.contains_key("upgrade") && connection_has(src, "upgrade");
        let close_after = dedicated && !connection_has(src, "close") && !protocol_switch;
        let expects_body = matches!(method, Method::POST | Method::PUT | Method::PATCH);
        if !body.is_empty() || expects_body {
            headers.push((CONTENT_LENGTH, HeaderValue::from(body.len())));
        }
        let mut rest: Vec<(&HeaderName, &HeaderValue)> = src
            .iter()
            .filter(|(n, _)| {
                ![USER_AGENT, CONTENT_LENGTH, HOST].contains(*n) && n.as_str() != "transfer-encoding"
            })
            .collect();
        // Stable sort keeps the values of a repeated header in their original order.
        rest.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        headers.extend(rest.into_iter().map(|(n, v)| (n.clone(), v.clone())));
        if !src.contains_key(ACCEPT_ENCODING) && !src.contains_key(RANGE) && method != Method::HEAD {
            headers.push((ACCEPT_ENCODING, HeaderValue::from_static("gzip")));
        }
        Ok(Self { method, uri, host, port, authority, headers, body, ua_defaulted, close_after })
    }

    /// HTTP/1.1 request: origin-form target with an explicit Host header first.
    fn into_h1_request(self, absolute_uri: bool) -> Result<http::Request<Full<Bytes>>, Error> {
        let uri = if absolute_uri {
            self.uri
        } else {
            self.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/").parse::<Uri>().map_err(|e| Error::Request(e.to_string()))?
        };
        let mut b = http::Request::builder().method(self.method).uri(uri);
        let map = b.headers_mut().ok_or_else(|| Error::Request("invalid request".into()))?;
        map.insert(HOST, HeaderValue::from_str(&self.authority).map_err(|e| Error::Request(e.to_string()))?);
        for (n, v) in self.headers {
            map.append(n, v);
        }
        b.body(Full::new(self.body)).map_err(|e| Error::Request(e.to_string()))
    }

    /// HTTP/2 request: absolute URI (becomes `:scheme`/`:authority`/`:path`), no Host header and
    /// no connection-specific headers.
    fn into_h2_request(self) -> Result<http::Request<Full<Bytes>>, Error> {
        let mut b = http::Request::builder().method(self.method).uri(self.uri);
        let map = b.headers_mut().ok_or_else(|| Error::Request("invalid request".into()))?;
        // Go's http2 transport writes the caller's headers first, then content-length,
        // accept-encoding and user-agent (`Go-http-client/2.0` when none was set).
        let trailing = [CONTENT_LENGTH, ACCEPT_ENCODING, USER_AGENT];
        for (n, v) in self.headers.iter().filter(|(n, _)| *n != CONNECTION && !trailing.contains(n)) {
            map.append(n.clone(), v.clone());
        }
        for name in trailing {
            for (_, v) in self.headers.iter().filter(|(n, _)| *n == name) {
                let v = if name == USER_AGENT && self.ua_defaulted { HeaderValue::from_static("Go-http-client/2.0") } else { v.clone() };
                map.append(name.clone(), v);
            }
        }
        b.body(Full::new(self.body)).map_err(|e| Error::Request(e.to_string()))
    }
}

type ByteStream = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;

/// Keeps `guard` (the connection driver) alive for as long as the body is being read.
struct Guarded {
    inner: ByteStream,
    _guard: Option<AbortOnDrop>,
}

impl Stream for Guarded {
    type Item = io::Result<Bytes>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

/// Fails the body stream once the request deadline passes.
struct Deadlined {
    inner: ByteStream,
    sleep: Pin<Box<tokio::time::Sleep>>,
}

impl Stream for Deadlined {
    type Item = io::Result<Bytes>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.sleep.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Some(Err(io::Error::new(io::ErrorKind::TimedOut, "request timed out"))));
        }
        self.inner.poll_next_unpin(cx)
    }
}

/// Wraps `raw` in the decoder for a supported `Content-Encoding` (checked by the caller).
fn decoded(content_encoding: &str, raw: ByteStream) -> ByteStream {
    use async_compression::tokio::bufread::{BrotliDecoder, GzipDecoder, ZlibDecoder, ZstdDecoder};
    let reader = StreamReader::new(raw);
    match content_encoding {
        "deflate" => Box::pin(ReaderStream::new(ZlibDecoder::new(reader))),
        "br" => Box::pin(ReaderStream::new(BrotliDecoder::new(reader))),
        "zstd" => Box::pin(ReaderStream::new(ZstdDecoder::new(reader))),
        _ => Box::pin(ReaderStream::new(GzipDecoder::new(reader))),
    }
}

fn into_reqwest(resp: http::Response<Incoming>, guard: Option<AbortOnDrop>, deadline: Option<Instant>) -> reqwest::Response {
    let (mut parts, body) = resp.into_parts();
    let mut stream: ByteStream = Box::pin(BodyDataStream::new(body).map_err(io::Error::other));
    let encoding = parts
        .headers
        .get(CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if matches!(encoding.as_str(), "gzip" | "x-gzip" | "deflate" | "br" | "zstd") {
        stream = decoded(&encoding, stream);
        // Like reqwest, the decoded body no longer matches these representation headers.
        parts.headers.remove(CONTENT_ENCODING);
        parts.headers.remove(CONTENT_LENGTH);
    }
    if let Some(d) = deadline {
        stream = Box::pin(Deadlined { inner: stream, sleep: Box::pin(tokio::time::sleep_until(d)) });
    }
    let stream = Guarded { inner: stream, _guard: guard };
    reqwest::Response::from(http::Response::from_parts(parts, reqwest::Body::wrap_stream(stream)))
}
