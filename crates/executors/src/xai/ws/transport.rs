//! Websocket client transport: proxy-aware TCP dial, TLS, a hand-written upgrade handshake (so
//! header casing matches Go's gorilla dialer) and the framed stream (Go: dialXAIWebsocket and
//! newProxyAwareWebsocketDialer; copied from the Codex port, which keeps its module private).

use std::io;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use cpa_auth::http::ProxySetting;
use http::{HeaderMap, HeaderName, HeaderValue};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use url::Url;

use super::codec::{Deflate, WsReader, WsWriter, split};
use super::WireHeaders;

/// Handshake timeout and TCP dial timeout (Go: 30 s each).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Upstream handshake error bodies are cut at 1024 bytes, like gorilla's.
const ERROR_BODY_LIMIT: usize = 1024;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type BoxIo = Box<dyn Io>;

/// A completed handshake.
pub struct Dialed {
    pub reader: WsReader,
    pub writer: WsWriter,
    pub response_headers: HeaderMap,
}

/// A failed dial: a rejected upgrade carries the HTTP response, a transport failure only text.
#[derive(Debug, Default)]
pub struct DialFailure {
    pub status: Option<u16>,
    /// Response headers of a rejected upgrade.
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub error: String,
}

impl DialFailure {
    fn transport(err: impl std::fmt::Display) -> Self {
        DialFailure { error: err.to_string(), ..Default::default() }
    }
}

enum Route {
    Direct,
    Http(Url),
    Socks5(Url),
}

fn tls_config() -> Arc<tokio_rustls::rustls::ClientConfig> {
    use std::sync::OnceLock;
    static CONFIG: OnceLock<Arc<tokio_rustls::rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = tokio_rustls::rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            let config = tokio_rustls::rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

fn env_value(names: &[&str]) -> String {
    names.iter().find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty())).unwrap_or_default()
}

/// Whether `host:port` bypasses the environment proxy (Go: httpproxy NO_PROXY rules, loopback
/// always bypasses).
fn bypasses_env_proxy(host: &str, port: u16) -> bool {
    let lower = host.to_lowercase();
    if lower == "localhost" || lower.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
        return true;
    }
    let no_proxy = env_value(&["NO_PROXY", "no_proxy"]);
    for entry in no_proxy.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        if entry == "*" {
            return true;
        }
        let (name, entry_port) = match entry.rsplit_once(':') {
            Some((n, p)) if p.parse::<u16>().is_ok() => (n, p.parse::<u16>().ok()),
            _ => (entry, None),
        };
        if entry_port.is_some_and(|p| p != port) {
            continue;
        }
        let name = name.trim_start_matches('.').to_lowercase();
        if lower == name || lower.ends_with(&format!(".{name}")) {
            return true;
        }
    }
    false
}

fn route_for(setting: &ProxySetting, tls: bool, host: &str, port: u16) -> Result<Route, String> {
    let url = match setting {
        ProxySetting::Direct => return Ok(Route::Direct),
        ProxySetting::Proxy(raw) => raw.clone(),
        ProxySetting::Inherit => {
            if bypasses_env_proxy(host, port) {
                return Ok(Route::Direct);
            }
            let raw = if tls { env_value(&["HTTPS_PROXY", "https_proxy"]) } else { env_value(&["HTTP_PROXY", "http_proxy"]) };
            if raw.is_empty() {
                return Ok(Route::Direct);
            }
            if raw.contains("://") { raw } else { format!("http://{raw}") }
        }
    };
    let parsed = Url::parse(&url).map_err(|e| format!("invalid proxy url: {e}"))?;
    match parsed.scheme() {
        "socks5" | "socks5h" => Ok(Route::Socks5(parsed)),
        "http" | "https" => Ok(Route::Http(parsed)),
        other => Err(format!("unsupported proxy scheme: {other}")),
    }
}

async fn tcp_connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dial tcp: i/o timeout"))??;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

fn proxy_authority(url: &Url) -> io::Result<(String, u16)> {
    let host = url.host_str().ok_or_else(|| io::Error::other("proxy url has no host"))?;
    let port = url.port_or_known_default().unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
    Ok((host.to_string(), port))
}

async fn read_head<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> io::Result<String> {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected EOF reading response head"));
        }
        let end = line == "\r\n" || line == "\n";
        head.push_str(&line);
        if end {
            return Ok(head);
        }
        if head.len() > 64 * 1024 {
            return Err(io::Error::other("response head too large"));
        }
    }
}

async fn http_connect<S: AsyncRead + AsyncWrite + Unpin>(stream: S, proxy: &Url, host: &str, port: u16) -> io::Result<BufReader<S>> {
    let mut reader = BufReader::new(stream);
    let mut request = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if !proxy.username().is_empty() {
        let credentials = format!("{}:{}", proxy.username(), proxy.password().unwrap_or_default());
        request.push_str(&format!("Proxy-Authorization: Basic {}\r\n", base64::engine::general_purpose::STANDARD.encode(credentials)));
    }
    request.push_str("\r\n");
    reader.write_all(request.as_bytes()).await?;
    let head = read_head(&mut reader).await?;
    let status = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
    if status != 200 {
        return Err(io::Error::other(format!("proxy CONNECT failed: {}", head.lines().next().unwrap_or(""))));
    }
    Ok(reader)
}

async fn socks5_connect(mut stream: TcpStream, proxy: &Url, host: &str, port: u16) -> io::Result<TcpStream> {
    let has_auth = !proxy.username().is_empty();
    let methods: &[u8] = if has_auth { &[0x00, 0x02] } else { &[0x00] };
    let mut greeting = vec![0x05, methods.len() as u8];
    greeting.extend_from_slice(methods);
    stream.write_all(&greeting).await?;
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await?;
    if choice[0] != 0x05 {
        return Err(io::Error::other("socks5: bad protocol version"));
    }
    match choice[1] {
        0x00 => {}
        0x02 if has_auth => {
            let (user, pass) = (proxy.username().as_bytes(), proxy.password().unwrap_or_default().as_bytes());
            if user.len() > 255 || pass.len() > 255 {
                return Err(io::Error::other("socks5: credentials too long"));
            }
            let mut auth = vec![0x01, user.len() as u8];
            auth.extend_from_slice(user);
            auth.push(pass.len() as u8);
            auth.extend_from_slice(pass);
            stream.write_all(&auth).await?;
            let mut reply = [0u8; 2];
            stream.read_exact(&mut reply).await?;
            if reply[1] != 0x00 {
                return Err(io::Error::other("socks5: authentication failed"));
            }
        }
        _ => return Err(io::Error::other("socks5: no acceptable authentication method")),
    }
    if host.len() > 255 {
        return Err(io::Error::other("socks5: host name too long"));
    }
    let mut request = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await?;
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[1] != 0x00 {
        return Err(io::Error::other(format!("socks5: connect failed (code {})", head[1])));
    }
    let addr_len = match head[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            len[0] as usize
        }
        _ => return Err(io::Error::other("socks5: bad address type")),
    };
    let mut rest = vec![0u8; addr_len + 2];
    stream.read_exact(&mut rest).await?;
    Ok(stream)
}

async fn tls_wrap(io: BoxIo, host: &str) -> io::Result<BoxIo> {
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| io::Error::other(e.to_string()))?;
    let tls = TlsConnector::from(tls_config()).connect(name, io).await?;
    Ok(Box::new(tls))
}

/// Opens the byte stream to `host:port` through the selected route, with TLS when requested.
async fn connect_io(setting: &ProxySetting, tls: bool, host: &str, port: u16) -> io::Result<BoxIo> {
    let route = route_for(setting, tls, host, port).map_err(io::Error::other)?;
    let io: BoxIo = match route {
        Route::Direct => Box::new(tcp_connect(host, port).await?),
        Route::Socks5(url) => {
            let (proxy_host, proxy_port) = proxy_authority(&url)?;
            let stream = tcp_connect(&proxy_host, proxy_port).await?;
            Box::new(socks5_connect(stream, &url, host, port).await?)
        }
        Route::Http(url) => {
            let (proxy_host, proxy_port) = proxy_authority(&url)?;
            let stream = tcp_connect(&proxy_host, proxy_port).await?;
            if url.scheme() == "https" {
                let tls_stream = tls_wrap(Box::new(stream), &proxy_host).await?;
                Box::new(http_connect(tls_stream, &url, host, port).await?)
            } else {
                Box::new(http_connect(stream, &url, host, port).await?)
            }
        }
    };
    if tls { tls_wrap(io, host).await } else { Ok(io) }
}

fn random_key() -> String {
    base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 16]>())
}

fn accept_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

fn parse_head(head: &str) -> (u16, HeaderMap) {
    let mut lines = head.lines();
    let status = lines.next().and_then(|l| l.split_whitespace().nth(1)).and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
    let mut headers = HeaderMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.trim().as_bytes()), HeaderValue::from_str(value.trim())) {
            headers.append(name, value);
        }
    }
    (status, headers)
}

/// First bytes of a rejected upgrade's body (content-length, chunked or until EOF).
async fn read_error_body<R: AsyncBufReadExt + Unpin>(reader: &mut R, headers: &HeaderMap) -> Vec<u8> {
    let mut body = Vec::new();
    let chunked = headers.get("transfer-encoding").and_then(|v| v.to_str().ok()).is_some_and(|v| v.to_lowercase().contains("chunked"));
    if chunked {
        while body.len() < ERROR_BODY_LIMIT {
            let mut size_line = String::new();
            if reader.read_line(&mut size_line).await.unwrap_or(0) == 0 {
                break;
            }
            let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
            if size == 0 {
                break;
            }
            let take = size.min(ERROR_BODY_LIMIT - body.len());
            let mut chunk = vec![0u8; take];
            if reader.read_exact(&mut chunk).await.is_err() {
                break;
            }
            body.extend_from_slice(&chunk);
            if take < size {
                break;
            }
            let mut crlf = [0u8; 2];
            let _ = reader.read_exact(&mut crlf).await;
        }
        return body;
    }
    let length = headers.get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<usize>().ok());
    let want = length.map_or(ERROR_BODY_LIMIT, |l| l.min(ERROR_BODY_LIMIT));
    let mut limited = reader.take(want as u64);
    let _ = limited.read_to_end(&mut body).await;
    body
}

/// Dials and upgrades `url`; `headers` are written as given (gorilla adds its own handshake
/// headers). A rejected upgrade is returned as [`DialFailure`] with the response.
pub async fn dial(url: &str, headers: &WireHeaders, proxy: &ProxySetting) -> Result<Dialed, DialFailure> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, dial_inner(url, headers, proxy))
        .await
        .map_err(|_| DialFailure::transport("websocket: handshake timeout"))?
}

async fn dial_inner(url: &str, headers: &WireHeaders, proxy: &ProxySetting) -> Result<Dialed, DialFailure> {
    let parsed = Url::parse(url).map_err(DialFailure::transport)?;
    let tls = match parsed.scheme() {
        "wss" => true,
        "ws" => false,
        _ => return Err(DialFailure::transport("malformed ws or wss URL")),
    };
    let host = parsed.host_str().ok_or_else(|| DialFailure::transport("malformed ws or wss URL"))?;
    let port = parsed.port_or_known_default().unwrap_or(if tls { 443 } else { 80 });
    let io = connect_io(proxy, tls, host, port).await.map_err(DialFailure::transport)?;
    let mut reader = BufReader::new(io);

    let key = random_key();
    let host_header = match parsed.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    };
    let mut target = parsed.path().to_string();
    if target.is_empty() {
        target.push('/');
    }
    if let Some(query) = parsed.query() {
        target.push('?');
        target.push_str(query);
    }
    let mut request = format!(
        "GET {target} HTTP/1.1\r\nHost: {host_header}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Extensions: permessage-deflate; server_no_context_takeover; client_no_context_takeover\r\n"
    );
    let mut has_user_agent = false;
    for (name, value) in &headers.0 {
        has_user_agent |= name.eq_ignore_ascii_case("user-agent");
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if !has_user_agent {
        request.push_str("User-Agent: Go-http-client/1.1\r\n");
    }
    request.push_str("\r\n");
    reader.write_all(request.as_bytes()).await.map_err(DialFailure::transport)?;
    reader.flush().await.map_err(DialFailure::transport)?;

    let head = read_head(&mut reader).await.map_err(DialFailure::transport)?;
    let (status, response_headers) = parse_head(&head);
    let valid = status == 101
        && response_headers.get("upgrade").and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        && response_headers.get("connection").and_then(|v| v.to_str().ok()).is_some_and(|v| v.to_lowercase().contains("upgrade"))
        && response_headers.get("sec-websocket-accept").and_then(|v| v.to_str().ok()) == Some(accept_key(&key).as_str());
    if !valid {
        let body = read_error_body(&mut reader, &response_headers).await;
        return Err(DialFailure { status: Some(status), headers: response_headers, body, error: "websocket: bad handshake".to_string() });
    }
    let extensions = response_headers.get("sec-websocket-extensions").and_then(|v| v.to_str().ok()).unwrap_or_default();
    let deflate = Deflate::from_response_header(extensions).map_err(DialFailure::transport)?;
    let leftover = reader.buffer().to_vec();
    let (reader, writer) = split(reader.into_inner(), &leftover, deflate);
    Ok(Dialed { reader, writer, response_headers })
}
