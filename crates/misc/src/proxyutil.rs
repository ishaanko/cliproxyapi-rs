//! Proxy setting parsing, redaction and connection-level dialing (Go: sdk/proxyutil).
//!
//! The Go package also builds `http.Transport`s; in Rust the HTTP clients are reqwest clients
//! configured by `cpa_auth::http` and `cpa_executors::helps::proxy`, so only the pieces that are
//! not reqwest specific live here: [`parse`] (with Go's `net/url` acceptance rules),
//! [`valid_request_proxy`], [`redact`] and [`build_dialer`] (HTTP CONNECT / SOCKS5 tunnels).

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use base64::Engine;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;

/// How a proxy setting should be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No explicit proxy behavior was configured.
    Inherit,
    /// Outbound requests must bypass proxies explicitly.
    Direct,
    /// A concrete proxy URL was configured.
    Proxy,
    /// The setting is present but malformed or unsupported.
    Invalid,
}

/// The normalized interpretation of a proxy configuration value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub raw: String,
    pub mode: Mode,
    pub url: Option<ProxyUrl>,
}

/// The parts of a proxy URL the dialers need (Go `*url.URL`, unescaped like Go).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyUrl {
    pub scheme: String,
    /// `Some` when the URL has a userinfo section.
    pub username: Option<String>,
    pub password: Option<String>,
    /// `host[:port]` as written (brackets kept).
    pub host: String,
}

impl ProxyUrl {
    /// `URL.Hostname()`.
    pub fn hostname(&self) -> &str {
        let host = split_host_port(&self.host).0;
        host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
    }

    /// `URL.Port()`.
    pub fn port(&self) -> &str {
        split_host_port(&self.host).1
    }
}

/// `splitHostPort`: a trailing `:digits` is the port.
fn split_host_port(host: &str) -> (&str, &str) {
    if let Some(colon) = host.rfind(':')
        && valid_optional_port(&host[colon..])
    {
        return (&host[..colon], &host[colon + 1..]);
    }
    (host, "")
}

fn valid_optional_port(port: &str) -> bool {
    match port.strip_prefix(':') {
        None => port.is_empty(),
        Some(digits) => digits.bytes().all(|b| b.is_ascii_digit()),
    }
}

/// Errors from [`parse`]; the messages never include the proxy URL (it may hold credentials).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("parse proxy URL failed")]
    Parse,
    #[error("proxy URL missing scheme/host")]
    MissingSchemeHost,
    #[error("unsupported proxy scheme: {0}")]
    UnsupportedScheme(String),
}

/// Normalizes a proxy configuration value into inherit, direct, or proxy modes. On error the
/// setting (with `Mode::Invalid`) is returned alongside it, as Go does.
#[allow(clippy::result_large_err)] // Go returns the setting next to the error
pub fn parse(raw: &str) -> Result<Setting, (Setting, ParseError)> {
    let trimmed = raw.trim();
    let mut setting = Setting { raw: trimmed.to_string(), mode: Mode::Inherit, url: None };
    if trimmed.is_empty() {
        return Ok(setting);
    }
    if trimmed.eq_ignore_ascii_case("direct") || trimmed.eq_ignore_ascii_case("none") {
        setting.mode = Mode::Direct;
        return Ok(setting);
    }
    setting.mode = Mode::Invalid;
    let Some(parsed) = parse_go_url(trimmed) else {
        return Err((setting, ParseError::Parse));
    };
    if parsed.scheme.is_empty() || parsed.host.is_empty() {
        return Err((setting, ParseError::MissingSchemeHost));
    }
    match parsed.scheme.as_str() {
        "socks5" | "socks5h" | "http" | "https" => {
            setting.mode = Mode::Proxy;
            setting.url = Some(parsed);
            Ok(setting)
        }
        other => {
            let err = ParseError::UnsupportedScheme(other.to_string());
            Err((setting, err))
        }
    }
}

/// Reports whether `raw` is a concrete execution proxy override: a proxy URL with a host and, when
/// a port is given, one in 1-65535.
pub fn valid_request_proxy(raw: &str) -> bool {
    let Ok(setting) = parse(raw) else { return false };
    let Some(url) = setting.url.as_ref().filter(|_| setting.mode == Mode::Proxy) else {
        return false;
    };
    if url.hostname().trim().is_empty() {
        return false;
    }
    let port = url.port();
    if port.is_empty() {
        return true;
    }
    // Go's Atoi: an optional sign then digits; the port here is digits only.
    port.parse::<u32>().is_ok_and(|n| (1..=65535).contains(&n))
}

/// A log-safe proxy URL with credentials and path-like data removed.
pub fn redact(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match parse_go_url(trimmed) {
        Some(u) if !u.scheme.is_empty() && !u.host.is_empty() => {
            let user = if u.username.is_some() { "redacted@" } else { "" };
            format!("{}://{}{}", u.scheme, user, escape_host(&u.host))
        }
        _ => "<invalid proxy URL>".to_string(),
    }
}

/// `URL.String()`'s host escaping: bytes outside the host-safe set are percent-encoded.
fn escape_host(host: &str) -> String {
    let mut out = String::with_capacity(host.len());
    for c in host.chars() {
        match u8::try_from(c) {
            Ok(b) if b < 0x80 && !host_byte_ok(b) => out.push_str(&format!("%{b:02X}")),
            _ => out.push(c),
        }
    }
    out
}

fn host_byte_ok(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-_.~!$&'()*+,;=:[]<>\"".contains(&b)
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn unhex(b: u8) -> u8 {
    (b as char).to_digit(16).unwrap_or(0) as u8
}

/// `url.unescape` validation (and decoding) for the userinfo and host modes. `None` when Go
/// would return an error.
fn unescape(s: &str, host_mode: bool) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                if !is_hex(bytes[i + 1]) || !is_hex(bytes[i + 2]) {
                    return None;
                }
                let v = unhex(bytes[i + 1]) << 4 | unhex(bytes[i + 2]);
                if host_mode && unhex(bytes[i + 1]) < 8 && &bytes[i..i + 3] != b"%25" {
                    return None;
                }
                out.push(v);
                i += 3;
            }
            b if host_mode && b < 0x80 && !host_byte_ok(b) => return None,
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn parse_host(host: &str) -> Option<String> {
    if host.starts_with('[') {
        let i = host.rfind(']')?;
        if !valid_optional_port(&host[i + 1..]) {
            return None;
        }
        return unescape(host, true);
    }
    if let Some(i) = host.rfind(':')
        && !valid_optional_port(&host[i..])
    {
        return None;
    }
    unescape(host, true)
}

fn valid_userinfo(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._:~!$&'()*+,;=%@".contains(&b))
}

/// The subset of Go's `url.Parse` the proxy code relies on: scheme, authority and the validation
/// errors Go reports for them (bad escapes, ports, host characters). Path, query and fragment are
/// only checked for escapes where Go checks them.
fn parse_go_url(raw: &str) -> Option<ProxyUrl> {
    if raw.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return None;
    }
    let (main, fragment) = raw.split_once('#').unwrap_or((raw, ""));
    unescape_ok(fragment)?;
    // getScheme
    let mut scheme = String::new();
    let mut rest = main;
    for (i, c) in main.char_indices() {
        if c.is_ascii_alphabetic() {
            continue;
        }
        if c.is_ascii_digit() || matches!(c, '+' | '-' | '.') {
            if i == 0 {
                break;
            }
            continue;
        }
        if c == ':' {
            if i == 0 {
                return None;
            }
            scheme = main[..i].to_ascii_lowercase();
            rest = &main[i + 1..];
        }
        break;
    }
    let (rest, _query) = rest.split_once('?').map_or((rest, ""), |(a, b)| (a, b));
    if !rest.starts_with('/') {
        if !scheme.is_empty() {
            // Opaque URL: no host.
            return Some(ProxyUrl { scheme, username: None, password: None, host: String::new() });
        }
        let first_segment = rest.split('/').next().unwrap_or("");
        if first_segment.contains(':') {
            return None;
        }
    }
    let mut url = ProxyUrl { scheme, username: None, password: None, host: String::new() };
    let authority_path = rest.strip_prefix("//");
    let path = match authority_path {
        Some(after) if !url.scheme.is_empty() || !rest.starts_with("///") => {
            let (authority, path) = match after.find('/') {
                Some(i) => (&after[..i], &after[i..]),
                None => (after, ""),
            };
            parse_authority(authority, &mut url)?;
            path
        }
        _ => rest,
    };
    unescape_ok(path)?;
    Some(url)
}

fn unescape_ok(s: &str) -> Option<()> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            if !is_hex(bytes[i + 1]) || !is_hex(bytes[i + 2]) {
                return None;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    Some(())
}

fn parse_authority(authority: &str, url: &mut ProxyUrl) -> Option<()> {
    let (userinfo, host) = match authority.rfind('@') {
        None => (None, authority),
        Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
    };
    url.host = parse_host(host)?;
    if let Some(userinfo) = userinfo {
        if !valid_userinfo(userinfo) {
            return None;
        }
        match userinfo.split_once(':') {
            None => url.username = Some(unescape(userinfo, false)?),
            Some((user, pass)) => {
                url.username = Some(unescape(user, false)?);
                url.password = Some(unescape(pass, false)?);
            }
        }
    }
    Some(())
}

// ---------------------------------------------------------------------------------------------
// Dialer

/// Any bidirectional async byte stream.
pub trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> AsyncStream for T {}

/// A boxed tunnel to the target.
pub type BoxedStream = Box<dyn AsyncStream>;

/// A connection-level proxy dialer (Go `proxy.Dialer`).
#[derive(Clone)]
pub enum Dialer {
    /// `direct` / `none`: dial the target itself.
    Direct,
    /// `http` / `https` proxies via `CONNECT`.
    HttpConnect { proxy: ProxyUrl, tls: Option<Arc<rustls::ClientConfig>> },
    /// `socks5` / `socks5h` proxies (the target host name is always resolved by the proxy).
    Socks5 { proxy: ProxyUrl },
}

/// Builds the dialer for a proxy setting. `None` means inherit (no explicit proxy behavior).
pub fn build_dialer(raw: &str) -> Result<(Option<Dialer>, Mode), (ParseError, Mode)> {
    let setting = match parse(raw) {
        Ok(s) => s,
        Err((s, e)) => return Err((e, s.mode)),
    };
    let dialer = match (setting.mode, setting.url) {
        (Mode::Direct, _) => Some(Dialer::Direct),
        (Mode::Proxy, Some(url)) => Some(match url.scheme.as_str() {
            "http" | "https" => Dialer::HttpConnect { proxy: url, tls: None },
            _ => Dialer::Socks5 { proxy: url },
        }),
        _ => None,
    };
    Ok((dialer, setting.mode))
}

impl Dialer {
    /// Overrides the TLS settings used to reach an `https` proxy (Go: `httpConnectDialer.tlsConfig`).
    pub fn with_tls_config(mut self, config: Arc<rustls::ClientConfig>) -> Self {
        if let Dialer::HttpConnect { tls, .. } = &mut self {
            *tls = Some(config);
        }
        self
    }

    /// Opens a tunnel to `addr` (`host:port`). Dropping the future cancels the dial and closes
    /// any connection already opened.
    pub async fn dial(&self, addr: &str) -> io::Result<BoxedStream> {
        match self {
            Dialer::Direct => Ok(Box::new(TcpStream::connect(addr).await?)),
            Dialer::HttpConnect { proxy, tls } => http_connect(proxy, tls.as_ref(), addr).await,
            Dialer::Socks5 { proxy } => socks5_connect(proxy, addr).await,
        }
    }
}

fn proxy_dial_addr(proxy: &ProxyUrl) -> String {
    let port = match proxy.port() {
        "" if proxy.scheme == "https" => "443",
        "" => "80",
        p => p,
    };
    let host = proxy.hostname();
    if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") }
}

fn default_tls_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    // ring provider with default protocol versions cannot fail to build.
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map(|b| b.with_root_certificates(roots).with_no_client_auth());
    match builder {
        Ok(cfg) => Arc::new(cfg),
        Err(_) => Arc::new(rustls::ClientConfig::builder().with_root_certificates(rustls::RootCertStore::empty()).with_no_client_auth()),
    }
}

async fn http_connect(proxy: &ProxyUrl, tls: Option<&Arc<rustls::ClientConfig>>, addr: &str) -> io::Result<BoxedStream> {
    let tcp = TcpStream::connect(proxy_dial_addr(proxy))
        .await
        .map_err(|e| io::Error::new(e.kind(), format!("dial HTTP proxy failed: {e}")))?;
    let mut conn: BoxedStream = if proxy.scheme == "https" {
        let mut config = (**tls.unwrap_or(&default_tls_config())).clone();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let name = rustls::pki_types::ServerName::try_from(proxy.hostname().to_string())
            .map_err(|e| io::Error::other(format!("HTTPS proxy TLS handshake failed: {e}")))?;
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let stream = connector
            .connect(name, tcp)
            .await
            .map_err(|e| io::Error::new(e.kind(), format!("HTTPS proxy TLS handshake failed: {e}")))?;
        Box::new(stream)
    } else {
        Box::new(tcp)
    };

    // Same bytes as Go's Request.Write for a CONNECT request.
    let mut request = format!("CONNECT {addr} HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: Go-http-client/1.1\r\n");
    if let Some(user) = &proxy.username {
        let creds = format!("{}:{}", user, proxy.password.as_deref().unwrap_or(""));
        let encoded = base64::engine::general_purpose::STANDARD.encode(creds);
        request.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
    }
    request.push_str("\r\n");
    conn.write_all(request.as_bytes())
        .await
        .map_err(|e| io::Error::new(e.kind(), format!("write CONNECT request failed: {e}")))?;
    conn.flush().await.ok();

    let (status, leftover) = read_connect_response(&mut conn)
        .await
        .map_err(|e| io::Error::new(e.kind(), format!("read CONNECT response failed: {e}")))?;
    if !status.starts_with("200") {
        return Err(io::Error::other(format!("proxy CONNECT returned status {status}")));
    }
    if leftover.is_empty() {
        return Ok(conn);
    }
    Ok(Box::new(BufferedConn { prefix: leftover, pos: 0, inner: conn }))
}

const MAX_RESPONSE_HEAD: usize = 1 << 20;

/// Reads the response head; returns the status text after the HTTP version (`"200 OK"`) and any
/// bytes read past the blank line.
async fn read_connect_response(conn: &mut BoxedStream) -> io::Result<(String, Vec<u8>)> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_RESPONSE_HEAD {
            return Err(io::Error::other("response header too large"));
        }
        let n = conn.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let line = head.lines().next().unwrap_or("");
    let status = match line.split_once(' ') {
        Some((version, status)) if version.starts_with("HTTP/") => status.trim().to_string(),
        _ => return Err(io::Error::other(format!("malformed HTTP response {line:?}"))),
    };
    Ok((status, buf[end..].to_vec()))
}

/// A stream that serves already-buffered bytes before reading the inner stream.
struct BufferedConn<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for BufferedConn<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if self.pos < self.prefix.len() {
            let n = (self.prefix.len() - self.pos).min(buf.remaining());
            let start = self.pos;
            buf.put_slice(&self.prefix[start..start + n]);
            self.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for BufferedConn<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// RFC 1928 CONNECT with optional RFC 1929 username/password (like `x/net/proxy`'s SOCKS5): the
/// target is always sent as a domain name or IP literal for the proxy to resolve.
async fn socks5_connect(proxy: &ProxyUrl, addr: &str) -> io::Result<BoxedStream> {
    let (host, port) = addr
        .rsplit_once(':')
        .ok_or_else(|| io::Error::other("proxy: no port in address"))?;
    let port: u16 = port.parse().map_err(|_| io::Error::other("proxy: failed to parse port number"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.len() > 255 {
        return Err(io::Error::other("proxy: destination host name too long"));
    }
    let mut stream = TcpStream::connect(proxy_dial_addr(proxy)).await?;

    let auth = proxy.username.is_some();
    let greeting: &[u8] = if auth { &[5, 2, 0, 2] } else { &[5, 1, 0] };
    stream.write_all(greeting).await?;
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await?;
    if reply[0] != 5 {
        return Err(io::Error::other("proxy: SOCKS5 proxy at the address gave a bad version"));
    }
    match reply[1] {
        0 => {}
        2 if auth => {
            let user = proxy.username.as_deref().unwrap_or("");
            let pass = proxy.password.as_deref().unwrap_or("");
            if user.len() > 255 || pass.len() > 255 {
                return Err(io::Error::other("proxy: username/password too long"));
            }
            let mut msg = vec![1, user.len() as u8];
            msg.extend_from_slice(user.as_bytes());
            msg.push(pass.len() as u8);
            msg.extend_from_slice(pass.as_bytes());
            stream.write_all(&msg).await?;
            let mut r = [0u8; 2];
            stream.read_exact(&mut r).await?;
            if r[0] != 1 || r[1] != 0 {
                return Err(io::Error::other("proxy: SOCKS5 authentication failed"));
            }
        }
        _ => return Err(io::Error::other("proxy: SOCKS5 proxy requires an unsupported authentication method")),
    }

    let mut req = vec![5, 1, 0];
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            req.push(1);
            req.extend_from_slice(&ip.octets());
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            req.push(4);
            req.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            req.push(3);
            req.push(host.len() as u8);
            req.extend_from_slice(host.as_bytes());
        }
    }
    req.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&req).await?;

    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[1] != 0 {
        return Err(io::Error::other(format!("proxy: SOCKS5 proxy failed to connect (reply code {})", head[1])));
    }
    let skip = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            len[0] as usize
        }
        _ => return Err(io::Error::other("proxy: unknown address type")),
    };
    let mut rest = vec![0u8; skip + 2];
    stream.read_exact(&mut rest).await?;
    Ok(Box::new(stream))
}

#[cfg(test)]
#[path = "proxyutil_tests.rs"]
mod tests;
