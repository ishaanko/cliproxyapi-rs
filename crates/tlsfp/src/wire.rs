//! HTTP/1.1 client with a caller-chosen request header order (Go: the header-profile branch of
//! `internal/pluginhost/http_bridge.go`, `newHTTPClientForRequest`).
//!
//! The plugin host HTTP bridge lets a plugin pin the header names, casing and order of a request.
//! Go does it with a `net/http` transport whose connections are wrapped in
//! `httpwire.NewOrderedRequestConn`; this is the same: hyper writes the request head in Go's own
//! order (Host, User-Agent, Content-Length, the remaining headers sorted by exact key, then the
//! transport's extra headers), and [`OrderedConn`] rewrites the head so the profile names come
//! first with their listed casing while every other header keeps Go's order and the exact casing
//! the plugin used.
//!
//! One connection per request (`DisableKeepAlives`), HTTP/1.1 only, stock TLS, redirects followed
//! like `http.Client` with the bridge's limit of 10, proxies chosen as the Go bridge does:
//! SOCKS5 and CONNECT tunnels for HTTPS targets, absolute-form forwarding through HTTP(S)
//! proxies for plain HTTP targets, and the environment (`HTTP_PROXY` and friends) when nothing
//! is configured.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use url::Url;

use crate::client::{AbortOnDrop, Error, chain, into_reqwest_with};
use crate::dial::{BoxIo, Dialer, stock_tls};
use crate::ordered::{HeaderRewriter, OrderedConn};

/// Go's `http.Transport` dial timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// `http.Client` stops after this many requests of one redirect chain (the bridge's
/// `CheckRedirect`: `len(via) >= 10`).
const MAX_REQUESTS: usize = 10;

/// The proxy choice of the bridge, already resolved from request override, credential and config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireProxy {
    /// Nothing configured: the environment decides (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`).
    Inherit,
    /// Explicitly no proxy.
    Direct,
    /// `socks5`, `socks5h`, `http` or `https` proxy URL.
    Url(String),
}

#[derive(Debug, Clone)]
pub struct WireConfig {
    /// Header names in the order (and casing) they must appear on the wire.
    pub header_profile: Vec<String>,
    /// Do not add `Accept-Encoding: gzip` (Go: `DisableCompression`).
    pub disable_auto_compression: bool,
    /// The effective proxy.
    pub proxy: WireProxy,
    /// Raw credential and config proxy settings. For HTTPS targets Go's dial hook consults these
    /// (credential first) before the effective proxy; see [`WireClient::https_target_proxy`].
    pub auth_proxy: String,
    pub config_proxy: String,
    /// Extra trusted root certificates (DER), on top of the system roots.
    pub extra_roots: Vec<Vec<u8>>,
}

/// One request as the plugin described it. Header keys keep the exact casing the plugin used;
/// the map order is Go's write order (sorted by key).
#[derive(Debug, Clone, Default)]
pub struct WireRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, Vec<String>>,
    pub body: Bytes,
}

pub struct WireClient {
    cfg: WireConfig,
}

/// A connection ready for one request.
struct Dialed {
    io: BoxIo,
    /// The request goes to an HTTP(S) proxy in absolute form, with this `Proxy-Authorization`.
    forward: Option<Forward>,
}

struct Forward {
    authorization: Option<String>,
}

impl WireClient {
    pub fn new(cfg: WireConfig) -> Self {
        Self { cfg }
    }

    /// Sends `req` and returns the final response once its headers arrive; the body streams.
    /// Errors read like Go's `url.Error`: `Get "http://host/path": cause`.
    pub async fn execute(&self, req: WireRequest) -> Result<reqwest::Response, Error> {
        let method = if req.method.is_empty() { "GET".to_string() } else { req.method.clone() };
        let method_ok = Method::from_bytes(method.as_bytes()).map_err(|_| Error::Request(format!("net/http: invalid method {method:?}")))?;
        let mut url = Url::parse(&req.url).map_err(|e| Error::Request(format!("parse {:?}: {e}", req.url)))?;
        let mut state = Chain { method: method_ok, headers: req.headers.clone(), body: req.body.clone(), include_body: true };
        let initial = req;
        let mut via: Vec<Url> = Vec::new();
        // `url.Error.Op` follows the method of the first request of the chain.
        let op = url_error_op(&initial.method);
        loop {
            let fail = |url: &Url, cause: String| Error::Transport(format!("{op} \"{}\": {cause}", display_url(url)));
            let (resp, guard, gzip) = self.send_once(&state, &url).await.map_err(|cause| fail(&url, cause))?;
            let Some(next) = redirect_target(&resp, &url, &state) else {
                let decodable = move |encoding: &str| gzip && encoding == "gzip";
                return Ok(into_reqwest_with(resp, Some(guard), None, decodable));
            };
            drop((resp, guard));
            let next = next.map_err(|cause| fail(&url, cause))?;
            if via.len() + 1 >= MAX_REQUESTS {
                return Err(Error::Transport(format!("{op} \"{}\": stopped after 10 redirects", next.location)));
            }
            via.push(url.clone());
            state.follow(&next, &url, &initial);
            url = next.url;
        }
    }

    /// One request on a fresh connection. Returns the response, the connection driver guard and
    /// whether this client asked for gzip itself (only then is a gzip body decoded, like Go).
    async fn send_once(&self, state: &Chain, url: &Url) -> Result<(http::Response<Incoming>, AbortOnDrop, bool), String> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(format!("unsupported protocol scheme {:?}", url.scheme()));
        }
        let host = url.host_str().filter(|h| !h.is_empty()).ok_or_else(|| "no Host in request URL".to_string())?;
        validate_headers(&state.headers)?;
        let dial_host = host.trim_matches(['[', ']']).to_string();
        let port = url.port_or_known_default().unwrap_or(80);
        let dialed = tokio::time::timeout(CONNECT_TIMEOUT, self.dial(url, &dial_host, port))
            .await
            .map_err(|_| format!("dial tcp {dial_host}:{port}: i/o timeout"))??;

        let head = self.build_head(state, url, dialed.forward.as_ref())?;
        let mut names = self.cfg.header_profile.clone();
        names.extend(head.names);
        let io = OrderedConn::with_rewriter(dialed.io, HeaderRewriter::with_names(names));
        let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
            .title_case_headers(true)
            .handshake::<_, Full<Bytes>>(TokioIo::new(io))
            .await
            .map_err(|e| chain(&e))?;
        let guard = AbortOnDrop(tokio::spawn(async move {
            let _ = conn.await;
        }));

        let target = if dialed.forward.is_some() {
            format!("{}://{}{}", url.scheme(), authority(url), request_uri(url))
        } else {
            request_uri(url)
        };
        let mut request = http::Request::builder().method(state.method.clone()).uri(target);
        *request.headers_mut().ok_or("invalid request")? = head.headers;
        let request = request.body(Full::new(state.body.clone())).map_err(|e| e.to_string())?;
        let resp = sender.send_request(request).await.map_err(|e| chain(&e))?;
        Ok((resp, guard, head.requested_gzip))
    }

    // ------------------------------------------------------------------ request head

    /// The request head in `net/http`'s write order (`Request.write` plus the transport's extra
    /// headers) and the exact-cased names in that order, for the rewriter to keep.
    fn build_head(&self, state: &Chain, url: &Url, forward: Option<&Forward>) -> Result<Head, String> {
        let mut names: Vec<String> = Vec::new();
        let mut headers = HeaderMap::new();
        let mut put = |name: &str, value: &str| -> Result<(), String> {
            let n = HeaderName::from_bytes(name.as_bytes()).map_err(|_| format!("net/http: invalid header field name {name:?}"))?;
            let v = HeaderValue::from_bytes(value.as_bytes()).map_err(|_| format!("net/http: invalid header field value for {name:?}"))?;
            headers.append(n, v);
            names.push(name.to_string());
            Ok(())
        };

        put("Host", &authority(url))?;
        // `Header.has("User-Agent")` is an exact-key lookup; a blank value drops the header.
        let user_agent = state.headers.get("User-Agent").map_or("Go-http-client/1.1", |v| v.first().map_or("", String::as_str));
        let user_agent = user_agent.replace(['\n', '\r'], " ");
        if !user_agent.trim().is_empty() {
            put("User-Agent", user_agent.trim())?;
        }
        let expects_body = matches!(state.method, Method::POST | Method::PUT | Method::PATCH);
        if !state.body.is_empty() || expects_body {
            put("Content-Length", &state.body.len().to_string())?;
        }

        let mut rest = state.headers.clone();
        if !url.username().is_empty() || url.password().is_some() {
            let missing = rest.get("Authorization").is_none_or(|v| v.first().is_none_or(|s| s.is_empty()));
            if missing {
                let creds = format!("{}:{}", percent_decode(url.username()), percent_decode(url.password().unwrap_or_default()));
                let token = base64::engine::general_purpose::STANDARD.encode(creds);
                rest.insert("Authorization".to_string(), vec![format!("Basic {token}")]);
            }
        }
        for (key, values) in &rest {
            if matches!(key.as_str(), "Host" | "User-Agent" | "Content-Length" | "Transfer-Encoding" | "Trailer") {
                continue;
            }
            for value in values {
                put(key, value.trim())?;
            }
        }

        // The transport's own headers: sorted, after the caller's.
        let get = |key: &str| rest.get(key).and_then(|v| v.first()).map_or("", String::as_str);
        let requested_gzip = !self.cfg.disable_auto_compression && get("Accept-Encoding").is_empty() && get("Range").is_empty() && state.method != Method::HEAD;
        if requested_gzip {
            put("Accept-Encoding", "gzip")?;
        }
        let wants_close = has_token(get("Connection"), "close");
        let protocol_switch = !get("Upgrade").is_empty() && rest.get("Connection").is_some_and(|v| v.iter().any(|s| has_token(s, "upgrade")));
        if !wants_close && !protocol_switch {
            put("Connection", "close")?;
        }
        if let Some(auth) = forward.and_then(|f| f.authorization.as_deref()) {
            put("Proxy-Authorization", auth)?;
        }
        Ok(Head { headers, names, requested_gzip })
    }

    // ------------------------------------------------------------------ connections

    async fn dial(&self, url: &Url, host: &str, port: u16) -> Result<Dialed, String> {
        if url.scheme() == "https" {
            let tunnel = match &self.cfg.proxy {
                WireProxy::Url(u) if is_socks(u) => Some(u.clone()),
                _ => self.https_target_proxy(url)?,
            };
            let tcp = match tunnel {
                Some(proxy) => Dialer::parse(&proxy).map_err(|e| format!("parse proxy: {e}"))?.dial(host, port).await,
                None => Dialer::Direct.dial(host, port).await,
            }
            .map_err(|e| dial_error(host, port, &e))?;
            let tls = stock_tls(host, tcp, &self.cfg.extra_roots).await.map_err(|e| format!("pluginhost TLS handshake: {e}"))?;
            return Ok(Dialed { io: Box::new(tls), forward: None });
        }

        let proxy = match &self.cfg.proxy {
            WireProxy::Url(u) => Some(u.clone()),
            WireProxy::Direct => None,
            WireProxy::Inherit => env_proxy(url),
        };
        let Some(proxy) = proxy else {
            let tcp = Dialer::Direct.dial(host, port).await.map_err(|e| dial_error(host, port, &e))?;
            return Ok(Dialed { io: tcp, forward: None });
        };
        if is_socks(&proxy) {
            let tcp = Dialer::parse(&proxy).map_err(|e| format!("parse proxy: {e}"))?.dial(host, port).await.map_err(|e| dial_error(host, port, &e))?;
            return Ok(Dialed { io: tcp, forward: None });
        }
        // Plain HTTP through an HTTP(S) proxy: the request is forwarded in absolute form.
        let parsed = Url::parse(&proxy).map_err(|e| format!("parse proxy: {e}"))?;
        let proxy_host = parsed.host_str().unwrap_or_default().trim_matches(['[', ']']).to_string();
        let proxy_port = parsed.port_or_known_default().unwrap_or(80);
        let tcp = Dialer::Direct.dial(&proxy_host, proxy_port).await.map_err(|e| dial_error(&proxy_host, proxy_port, &e))?;
        let io: BoxIo = if parsed.scheme() == "https" {
            let tls = stock_tls(&proxy_host, tcp, &self.cfg.extra_roots).await.map_err(|e| format!("HTTPS proxy TLS handshake: {e}"))?;
            Box::new(tls)
        } else {
            tcp
        };
        let authorization = (!parsed.username().is_empty() || parsed.password().is_some()).then(|| {
            let creds = format!("{}:{}", percent_decode(parsed.username()), percent_decode(parsed.password().unwrap_or_default()));
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(creds))
        });
        Ok(Dialed { io, forward: Some(Forward { authorization }) })
    }

    /// Go's `resolveProxyForRequest` for the TLS dial hook: the credential's proxy, then the
    /// config's, then what the transport would pick (the effective proxy or the environment).
    fn https_target_proxy(&self, url: &Url) -> Result<Option<String>, String> {
        for (raw, what) in [(&self.cfg.auth_proxy, "auth"), (&self.cfg.config_proxy, "config")] {
            match parse_setting(raw).map_err(|e| format!("pluginhost: parse {what} proxy: {e}"))? {
                WireProxy::Direct => return Ok(None),
                WireProxy::Url(u) => return Ok(Some(u)),
                WireProxy::Inherit => {}
            }
        }
        Ok(match &self.cfg.proxy {
            WireProxy::Url(u) => Some(u.clone()),
            WireProxy::Direct => None,
            WireProxy::Inherit => env_proxy(url),
        })
    }
}

/// `Transport.roundTrip`'s header check (`httpguts.ValidHeaderFieldName/Value`), done before any
/// connection is made.
fn validate_headers(headers: &BTreeMap<String, Vec<String>>) -> Result<(), String> {
    let token = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    for (name, values) in headers {
        if name.is_empty() || !name.bytes().all(token) {
            return Err(format!("net/http: invalid header field name {name:?}"));
        }
        if values.iter().any(|v| v.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f)) {
            return Err(format!("net/http: invalid header field value for {name:?}"));
        }
    }
    Ok(())
}

/// [`WireProxy`] of a raw proxy setting (Go: `proxyutil.Parse`).
fn parse_setting(raw: &str) -> Result<WireProxy, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(WireProxy::Inherit);
    }
    match Dialer::parse(trimmed)? {
        Dialer::Direct => Ok(WireProxy::Direct),
        _ => Ok(WireProxy::Url(trimmed.to_string())),
    }
}

fn is_socks(proxy: &str) -> bool {
    let scheme = proxy.trim().split("://").next().unwrap_or_default();
    scheme.eq_ignore_ascii_case("socks5") || scheme.eq_ignore_ascii_case("socks5h")
}

/// `dial tcp host:port: cause`, the shape of Go's `net.OpError`.
fn dial_error(host: &str, port: u16, e: &std::io::Error) -> String {
    let cause = match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "connect: connection refused".to_string(),
        _ => e.to_string(),
    };
    format!("dial tcp {host}:{port}: {cause}")
}

struct Head {
    headers: HeaderMap,
    /// Exact-cased header names in write order.
    names: Vec<String>,
    requested_gzip: bool,
}

// ---------------------------------------------------------------------- URL helpers

/// `url.Error.Op` of a method: first letter upper-case, the rest lower-case.
fn url_error_op(method: &str) -> String {
    let mut chars = method.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase(),
        None => "Get".to_string(),
    }
}

/// The URL as Go prints it in a `url.Error`: no credentials.
fn display_url(url: &Url) -> String {
    let mut u = url.clone();
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u.to_string()
}

/// `Host` header value: the authority as written (port only when the URL has one).
fn authority(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

/// `URL.RequestURI()`: path (never empty) plus query.
fn request_uri(url: &Url) -> String {
    let path = if url.path().is_empty() { "/" } else { url.path() };
    match url.query() {
        Some(q) => format!("{path}?{q}"),
        None => path.to_string(),
    }
}

fn percent_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("k={}", s.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

fn has_token(value: &str, token: &str) -> bool {
    value.split(',').any(|t| t.trim().eq_ignore_ascii_case(token))
}

// ---------------------------------------------------------------------- redirects

/// The request of the current hop of a redirect chain.
struct Chain {
    method: Method,
    headers: BTreeMap<String, Vec<String>>,
    body: Bytes,
    include_body: bool,
}

struct Redirect {
    url: Url,
    /// The `Location` value as received, for error messages.
    location: String,
    method: Method,
    include_body: bool,
}

/// `Client.do`'s redirect decision for `resp`: `None` when it is final, else the next hop (or
/// why it cannot be followed).
fn redirect_target(resp: &http::Response<Incoming>, current: &Url, state: &Chain) -> Option<Result<Redirect, String>> {
    let status = resp.status();
    let (method, include_body) = match status.as_u16() {
        301..=303 => (if matches!(state.method, Method::GET | Method::HEAD) { state.method.clone() } else { Method::GET }, false),
        307 | 308 => (state.method.clone(), true),
        _ => return None,
    };
    let location = resp.headers().get(http::header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    if location.is_empty() {
        return None;
    }
    match current.join(&location) {
        Ok(url) => Some(Ok(Redirect { url, location, method, include_body })),
        Err(e) => Some(Err(format!("failed to parse Location header {location:?}: {e}"))),
    }
}

impl Chain {
    /// Prepares the request for the redirect `next` of a response to `last` (Go: the headers
    /// copier, `Referer`, and dropping the body unless the status keeps it).
    fn follow(&mut self, next: &Redirect, last: &Url, initial: &WireRequest) {
        self.method = next.method.clone();
        self.include_body = next.include_body;
        if !next.include_body {
            self.body = Bytes::new();
        }
        // Headers come from the initial request, minus the credentials sent to another domain.
        let initial_host = Url::parse(&initial.url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
        let new_host = next.url.host_str().unwrap_or_default();
        let same_domain = is_domain_or_subdomain(new_host, &initial_host);
        self.headers = initial
            .headers
            .iter()
            .filter(|(k, _)| same_domain || !is_sensitive(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if let Some(referer) = referer_for(last, &next.url, initial.headers.get("Referer").and_then(|v| v.first()).map_or("", String::as_str)) {
            self.headers.insert("Referer".to_string(), vec![referer]);
        }
    }
}

fn is_sensitive(key: &str) -> bool {
    ["Authorization", "Www-Authenticate", "Cookie", "Cookie2"].iter().any(|s| s.eq_ignore_ascii_case(key))
}

/// `isDomainOrSubdomain`: `sub` equals `parent` or ends with `.parent` (IP literals only match
/// exactly).
fn is_domain_or_subdomain(sub: &str, parent: &str) -> bool {
    let (sub, parent) = (sub.to_ascii_lowercase(), parent.to_ascii_lowercase());
    if sub == parent {
        return true;
    }
    if sub.parse::<IpAddr>().is_ok() || sub.trim_matches(['[', ']']).parse::<IpAddr>().is_ok() {
        return false;
    }
    sub.ends_with(&format!(".{parent}"))
}

/// `refererForURL`: none when going from https to http, the initial `Referer` when set, else the
/// previous URL without credentials.
fn referer_for(last: &Url, next: &Url, explicit: &str) -> Option<String> {
    if last.scheme() == "https" && next.scheme() == "http" {
        return None;
    }
    if !explicit.is_empty() {
        return Some(explicit.to_string());
    }
    Some(display_url(last))
}

// ---------------------------------------------------------------------- environment proxy

/// `http.ProxyFromEnvironment` for `url`: `HTTP_PROXY` for http, `HTTPS_PROXY` for https,
/// minus `NO_PROXY` matches and loopback hosts. A value without a scheme means `http://`.
fn env_proxy(url: &Url) -> Option<String> {
    let any = |names: [&str; 2]| names.iter().find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()));
    let raw = match url.scheme() {
        "https" => any(["HTTPS_PROXY", "https_proxy"])?,
        "http" => {
            // Go refuses HTTP_PROXY in a CGI environment.
            if std::env::var_os("REQUEST_METHOD").is_some() {
                return None;
            }
            any(["HTTP_PROXY", "http_proxy"])?
        }
        _ => return None,
    };
    let no_proxy = any(["NO_PROXY", "no_proxy"]).unwrap_or_default();
    let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']);
    let port = url.port_or_known_default().map(|p| p.to_string()).unwrap_or_default();
    if !use_proxy(host, &port, &no_proxy) {
        return None;
    }
    let raw = raw.trim();
    match Url::parse(raw) {
        Ok(u) if u.host_str().is_some() && !u.scheme().is_empty() && raw.contains("://") => Some(raw.to_string()),
        _ => Some(format!("http://{raw}")),
    }
}

/// `httpproxy.Config.useProxy`: loopback and `NO_PROXY` hosts bypass the proxy.
fn use_proxy(host: &str, port: &str, no_proxy: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return false;
    }
    let ip = host.parse::<IpAddr>().ok();
    if ip.is_some_and(|ip| ip.is_loopback()) {
        return false;
    }
    let host = host.trim().to_ascii_lowercase();
    for phrase in no_proxy.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if phrase == "*" {
            return false;
        }
        let (name, phrase_port) = split_host_port(phrase);
        let port_ok = phrase_port.is_none_or(|p| p == port);
        if let Ok(phrase_ip) = name.trim_matches(['[', ']']).parse::<IpAddr>() {
            if ip == Some(phrase_ip) && port_ok {
                return false;
            }
            continue;
        }
        if let Some((net, bits)) = phrase.split_once('/')
            && let (Ok(net), Ok(bits), Some(ip)) = (net.parse::<IpAddr>(), bits.parse::<u32>(), ip)
        {
            if cidr_contains(net, bits, ip) {
                return false;
            }
            continue;
        }
        if ip.is_some() {
            continue;
        }
        let name = name.to_ascii_lowercase();
        let name = name.strip_prefix('*').unwrap_or(&name).to_string();
        let (suffix, match_host) = if name.starts_with('.') { (name.clone(), false) } else { (format!(".{name}"), true) };
        if (host.ends_with(&suffix) || (match_host && host == suffix[1..])) && port_ok {
            return false;
        }
    }
    true
}

fn split_host_port(s: &str) -> (&str, Option<&str>) {
    if let Some(rest) = s.strip_prefix('[')
        && let Some((host, tail)) = rest.split_once(']')
    {
        return (host, tail.strip_prefix(':').filter(|p| !p.is_empty()));
    }
    match s.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && !port.is_empty() => (host, Some(port)),
        _ => (s, None),
    }
}

fn cidr_contains(net: IpAddr, bits: u32, ip: IpAddr) -> bool {
    match (net, ip) {
        (IpAddr::V4(n), IpAddr::V4(i)) if bits <= 32 => {
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            u32::from(n) & mask == u32::from(i) & mask
        }
        (IpAddr::V6(n), IpAddr::V6(i)) if bits <= 128 => {
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
            u128::from(n) & mask == u128::from(i) & mask
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// One scripted answer per connection; every request head (and body) is recorded raw.
    type Answer = Box<dyn Fn(&str) -> Vec<u8> + Send + Sync>;

    async fn serve(answer: Answer) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let answer = Arc::new(answer);
        tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = listener.accept().await else { return };
                let (log, answer) = (log.clone(), answer.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                        match conn.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                    let want = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                        .unwrap_or(0);
                    while buf.len() < head_end + want {
                        match conn.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&buf).into_owned();
                    log.lock().expect("log").push(request.clone());
                    let _ = conn.write_all(&answer(&request)).await;
                });
            }
        });
        (port, seen)
    }

    fn ok(body: &str) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
    }

    fn found(location: &str) -> Vec<u8> {
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
    }

    fn config(profile: &[&str]) -> WireConfig {
        WireConfig {
            header_profile: profile.iter().map(|s| (*s).to_string()).collect(),
            disable_auto_compression: false,
            proxy: WireProxy::Direct,
            auth_proxy: String::new(),
            config_proxy: String::new(),
            extra_roots: Vec::new(),
        }
    }

    fn request(url: &str, headers: &[(&str, &str)]) -> WireRequest {
        let mut r = WireRequest { method: "GET".into(), url: url.into(), ..Default::default() };
        for (k, v) in headers {
            r.headers.entry((*k).to_string()).or_default().push((*v).to_string());
        }
        r
    }

    fn head_lines(raw: &str) -> Vec<&str> {
        raw.split("\r\n\r\n").next().unwrap_or_default().split("\r\n").collect()
    }

    // Go: TestHostHTTPClientAppliesWireProfile. Profile names lead in their listed order and
    // casing; everything else keeps Go's write order (sorted by exact key) and the plugin's
    // casing, with the transport's `Connection: close` last.
    #[tokio::test]
    async fn profile_names_lead_and_the_rest_keeps_go_order() {
        let (port, seen) = serve(Box::new(|_| ok("ok"))).await;
        let mut cfg = config(&["x-custom-b", "X-Custom-A", "User-Agent", "Host"]);
        cfg.disable_auto_compression = true;
        let headers = [("X-Custom-A", "value-a"), ("x-custom-b", "value-b"), ("User-Agent", "test-agent"), ("x-trace", "t"), ("Z-Last", "z")];
        let resp = WireClient::new(cfg).execute(request(&format!("http://127.0.0.1:{port}/test"), &headers)).await.expect("request");
        assert_eq!(resp.text().await.expect("body"), "ok");
        let seen = seen.lock().expect("log");
        assert_eq!(
            head_lines(&seen[0]),
            ["GET /test HTTP/1.1", "x-custom-b: value-b", "X-Custom-A: value-a", "User-Agent: test-agent", &format!("Host: 127.0.0.1:{port}"), "Z-Last: z", "x-trace: t", "Connection: close"]
        );
    }

    // Without a profile entry for it, Go's own order holds: Host, User-Agent, Content-Length, the
    // caller's headers sorted, then Accept-Encoding and Connection.
    #[tokio::test]
    async fn default_order_post_body_and_auto_gzip() {
        let (port, seen) = serve(Box::new(|_| ok("done"))).await;
        let mut req = request(&format!("http://127.0.0.1:{port}/p?q=1"), &[("X-B", "2"), ("X-A", "1")]);
        req.method = "POST".into();
        req.body = Bytes::from_static(b"payload");
        WireClient::new(config(&["Nothing-Listed"])).execute(req).await.expect("request");
        let seen = seen.lock().expect("log");
        assert_eq!(
            head_lines(&seen[0]),
            [
                "POST /p?q=1 HTTP/1.1",
                &format!("Host: 127.0.0.1:{port}"),
                "User-Agent: Go-http-client/1.1",
                "Content-Length: 7",
                "X-A: 1",
                "X-B: 2",
                "Accept-Encoding: gzip",
                "Connection: close"
            ]
        );
        assert!(seen[0].ends_with("\r\n\r\npayload"));
    }

    // Only a gzip body the client asked for itself is decoded, and the representation headers go.
    #[tokio::test]
    async fn gzip_is_decoded_only_when_the_client_asked_for_it() {
        use std::io::Write as _;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"hello gzip").expect("gzip");
        let gz = enc.finish().expect("gzip");
        let answer = move |_: &str| {
            let mut out = format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", gz.len()).into_bytes();
            out.extend_from_slice(&gz);
            out
        };
        let (port, _) = serve(Box::new(answer)).await;
        let url = format!("http://127.0.0.1:{port}/");
        let decoded = WireClient::new(config(&[])).execute(request(&url, &[])).await.expect("request");
        assert!(decoded.headers().get("content-encoding").is_none());
        assert_eq!(decoded.text().await.expect("body"), "hello gzip");
        let own_header = WireClient::new(config(&[])).execute(request(&url, &[("Accept-Encoding", "gzip")])).await.expect("request");
        assert_eq!(own_header.headers()["content-encoding"], "gzip");
    }

    // Go: TestHostHTTPClientWireProfile_PlainHTTPProxyUsesStandardProxy. Plain HTTP goes to the
    // proxy in absolute form, with the proxy's credentials as `Proxy-Authorization`.
    #[tokio::test]
    async fn plain_http_is_forwarded_through_an_http_proxy_in_absolute_form() {
        let (port, seen) = serve(Box::new(|_| ok("proxied"))).await;
        let mut cfg = config(&["Host", "User-Agent"]);
        cfg.proxy = WireProxy::Url(format!("http://user:pa%20ss@127.0.0.1:{port}"));
        let resp = WireClient::new(cfg).execute(request("http://example.invalid/resource?x=1", &[])).await.expect("request");
        assert_eq!(resp.text().await.expect("body"), "proxied");
        let seen = seen.lock().expect("log");
        let lines = head_lines(&seen[0]);
        assert_eq!(lines[0], "GET http://example.invalid/resource?x=1 HTTP/1.1");
        assert_eq!(&lines[1..3], ["Host: example.invalid", "User-Agent: Go-http-client/1.1"]);
        assert!(lines.contains(&"Proxy-Authorization: Basic dXNlcjpwYSBzcw=="), "{lines:?}");
    }

    // `http.Client` redirects: 302 turns POST into GET without the body, the Referer is set, the
    // chain stops after ten requests.
    #[tokio::test]
    async fn redirects_follow_http_client_rules() {
        let (port, seen) = serve(Box::new(|req| {
            if req.starts_with("POST /start") {
                found("/final")
            } else if req.contains("/loop") {
                found("/loop")
            } else {
                ok("landed")
            }
        }))
        .await;
        let base = format!("http://127.0.0.1:{port}");
        let mut req = request(&format!("{base}/start"), &[("Content-Type", "application/json"), ("Authorization", "Bearer t")]);
        req.method = "POST".into();
        req.body = Bytes::from_static(b"{}");
        let resp = WireClient::new(config(&[])).execute(req).await.expect("request");
        assert_eq!(resp.text().await.expect("body"), "landed");
        {
            let seen = seen.lock().expect("log");
            assert_eq!(seen.len(), 2);
            let second = head_lines(&seen[1]);
            assert_eq!(second[0], "GET /final HTTP/1.1");
            assert!(second.contains(&format!("Referer: {base}/start").as_str()), "{second:?}");
            assert!(second.contains(&"Authorization: Bearer t"), "same host keeps credentials: {second:?}");
            assert!(!second.iter().any(|l| l.starts_with("Content-Length")), "body dropped: {second:?}");
        }
        let err = WireClient::new(config(&[])).execute(request(&format!("{base}/loop"), &[])).await.unwrap_err();
        assert_eq!(err.to_string(), "Get \"/loop\": stopped after 10 redirects");
        assert_eq!(seen.lock().expect("log").iter().filter(|r| r.contains("/loop")).count(), 10);
    }

    #[tokio::test]
    async fn errors_read_like_go_url_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let err = WireClient::new(config(&[])).execute(request(&format!("http://127.0.0.1:{port}/x"), &[])).await.unwrap_err();
        assert_eq!(err.to_string(), format!("Get \"http://127.0.0.1:{port}/x\": dial tcp 127.0.0.1:{port}: connect: connection refused"));
        let err = WireClient::new(config(&[])).execute(request("ftp://example.com/x", &[])).await.unwrap_err();
        assert_eq!(err.to_string(), "Get \"ftp://example.com/x\": unsupported protocol scheme \"ftp\"");
        let err = WireClient::new(config(&[])).execute(request("http://127.0.0.1:1/", &[("Bad Name", "v")])).await.unwrap_err();
        assert_eq!(err.to_string(), "Get \"http://127.0.0.1:1/\": net/http: invalid header field name \"Bad Name\"");
    }

    // Go: httpproxy.Config semantics.
    #[test]
    fn no_proxy_matches_like_httpproxy() {
        assert!(!use_proxy("localhost", "80", ""));
        assert!(!use_proxy("127.0.0.1", "80", ""));
        assert!(use_proxy("example.com", "80", ""));
        assert!(!use_proxy("example.com", "80", "*"));
        // A bare domain matches itself and its subdomains, a dotted one only subdomains.
        assert!(!use_proxy("api.example.com", "443", "example.com"));
        assert!(!use_proxy("example.com", "443", "example.com"));
        assert!(use_proxy("example.com", "443", ".example.com"));
        assert!(!use_proxy("api.example.com", "443", "*.example.com"));
        assert!(use_proxy("example.com", "443", "example.com:8080"));
        assert!(!use_proxy("example.com", "8080", "example.com:8080"));
        assert!(!use_proxy("10.1.2.3", "80", "10.0.0.0/8"));
        assert!(use_proxy("11.1.2.3", "80", "10.0.0.0/8"));
        assert!(!use_proxy("1.2.3.4", "80", "1.2.3.4"));
    }

    // Go's TLS dial hook (`resolveProxyForRequest`) asks the credential's proxy first, then the
    // config's, and only then the transport's own choice.
    #[test]
    fn https_targets_consult_credential_then_config_then_effective_proxy() {
        let url = Url::parse("https://example.com/").unwrap();
        let client = |auth: &str, cfg: &str, proxy: WireProxy| {
            WireClient::new(WireConfig { auth_proxy: auth.into(), config_proxy: cfg.into(), proxy, ..config(&[]) })
        };
        let effective = || WireProxy::Url("http://effective:1".into());
        assert_eq!(client("http://auth:1", "http://cfg:1", effective()).https_target_proxy(&url), Ok(Some("http://auth:1".into())));
        assert_eq!(client("direct", "http://cfg:1", effective()).https_target_proxy(&url), Ok(None));
        assert_eq!(client("", "http://cfg:1", effective()).https_target_proxy(&url), Ok(Some("http://cfg:1".into())));
        assert_eq!(client("", "none", effective()).https_target_proxy(&url), Ok(None));
        assert_eq!(client("  ", "", effective()).https_target_proxy(&url), Ok(Some("http://effective:1".into())));
        assert_eq!(client("", "", WireProxy::Direct).https_target_proxy(&url), Ok(None));
        assert!(client("ftp://bad", "", WireProxy::Direct).https_target_proxy(&url).unwrap_err().starts_with("pluginhost: parse auth proxy"));
    }

    #[test]
    fn credentials_do_not_follow_a_redirect_to_another_domain() {
        assert!(is_domain_or_subdomain("api.example.com", "example.com"));
        assert!(!is_domain_or_subdomain("example.org", "example.com"));
        assert!(!is_domain_or_subdomain("badexample.com", "example.com"));
        assert!(!is_domain_or_subdomain("10.0.0.2", "0.0.2"));
        assert_eq!(referer_for(&Url::parse("https://a.test/x").unwrap(), &Url::parse("http://b.test/").unwrap(), ""), None);
        assert_eq!(referer_for(&Url::parse("http://u:p@a.test/x").unwrap(), &Url::parse("http://b.test/").unwrap(), "").as_deref(), Some("http://a.test/x"));
    }
}
