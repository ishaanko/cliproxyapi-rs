//! Proxy-aware websocket client for the upstream sideband and direct realtime sockets (Go:
//! `newProxyAwareSidebandDialer` over gorilla's `Dialer`).

use std::io;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine;
use cpa_auth::http::{ProxySetting, parse_proxy};
use http::{HeaderMap, HeaderName, HeaderValue};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use url::Url;

const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Rejected handshake bodies are cut at 1024 bytes, like gorilla's.
const ERROR_BODY_LIMIT: usize = 1024;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type BoxIo = Box<dyn Io>;
pub type UpstreamStream = WebSocketStream<BoxIo>;

/// A completed upstream handshake.
pub struct Dialed {
    pub stream: UpstreamStream,
    pub subprotocol: Option<String>,
    pub response_headers: HeaderMap,
    pub status: u16,
}

/// A failed dial: a rejected upgrade carries the HTTP response, a transport failure only text.
#[derive(Debug, Default)]
pub struct DialFailure {
    pub status: Option<u16>,
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
    static CONFIG: OnceLock<Arc<tokio_rustls::rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = tokio_rustls::rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            Arc::new(tokio_rustls::rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth())
        })
        .clone()
}

fn env_value(names: &[&str]) -> String {
    names.iter().find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty())).unwrap_or_default()
}

/// `http.ProxyFromEnvironment` bypass rules: loopback and `NO_PROXY` entries go direct.
fn bypasses_env_proxy(host: &str, port: u16) -> bool {
    let lower = host.to_lowercase();
    if lower == "localhost" || lower.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
        return true;
    }
    for entry in env_value(&["NO_PROXY", "no_proxy"]).split(',').map(str::trim).filter(|e| !e.is_empty()) {
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

fn route_for(proxy_url: &str, tls: bool, host: &str, port: u16) -> Route {
    // An unparsable proxy URL is logged and ignored: the default dialer applies.
    let setting = match parse_proxy(proxy_url) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("codex live sideband: {e}");
            ProxySetting::Inherit
        }
    };
    let raw = match setting {
        ProxySetting::Direct => return Route::Direct,
        ProxySetting::Proxy(raw) => raw,
        ProxySetting::Inherit => {
            if bypasses_env_proxy(host, port) {
                return Route::Direct;
            }
            let raw = if tls { env_value(&["HTTPS_PROXY", "https_proxy"]) } else { env_value(&["HTTP_PROXY", "http_proxy"]) };
            if raw.is_empty() {
                return Route::Direct;
            }
            if raw.contains("://") { raw } else { format!("http://{raw}") }
        }
    };
    match Url::parse(&raw) {
        Ok(u) if matches!(u.scheme(), "socks5" | "socks5h") => Route::Socks5(u),
        Ok(u) if matches!(u.scheme(), "http" | "https") => Route::Http(u),
        _ => Route::Direct,
    }
}

async fn tcp_connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let stream = tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dial tcp: i/o timeout"))??;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

fn authority(url: &Url, default_port: u16) -> io::Result<(String, u16)> {
    let host = url.host_str().ok_or_else(|| io::Error::other("proxy url has no host"))?;
    Ok((host.trim_matches(['[', ']']).to_string(), url.port().unwrap_or(default_port)))
}

async fn tls_wrap(stream: BoxIo, host: &str) -> io::Result<BoxIo> {
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| io::Error::other(format!("invalid server name: {e}")))?;
    let tls = TlsConnector::from(tls_config()).connect(name, stream).await?;
    Ok(Box::new(tls))
}

async fn read_head<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> io::Result<String> {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected EOF"));
        }
        let end = line == "\r\n" || line == "\n";
        head.push_str(&line);
        if end {
            return Ok(head);
        }
    }
}

/// HTTP `CONNECT` through `proxy` to `host:port`.
pub(crate) async fn http_connect(proxy: &Url, host: &str, port: u16) -> io::Result<BoxIo> {
    let default_port = if proxy.scheme() == "https" { 443 } else { 80 };
    let (proxy_host, proxy_port) = authority(proxy, default_port)?;
    let mut stream: BoxIo = Box::new(tcp_connect(&proxy_host, proxy_port).await?);
    if proxy.scheme() == "https" {
        stream = tls_wrap(stream, &proxy_host).await?;
    }
    let target = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if !proxy.username().is_empty() {
        let creds = format!("{}:{}", proxy.username(), proxy.password().unwrap_or(""));
        request.push_str(&format!("Proxy-Authorization: Basic {}\r\n", base64::engine::general_purpose::STANDARD.encode(creds)));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut reader = BufReader::new(stream);
    let head = read_head(&mut reader).await?;
    let status_line = head.lines().next().unwrap_or("");
    let ok = status_line.split_whitespace().nth(1).is_some_and(|c| c == "200");
    if !ok {
        return Err(io::Error::other(format!("proxyconnect tcp: {}", status_line.trim())));
    }
    // CONNECT replies are headers only, so nothing of the tunnel can be buffered yet.
    Ok(Box::new(reader.into_inner()))
}

pub(crate) async fn socks5_connect(proxy: &Url, host: &str, port: u16) -> io::Result<BoxIo> {
    let (proxy_host, proxy_port) = authority(proxy, 1080)?;
    let stream = tcp_connect(&proxy_host, proxy_port).await?;
    let target = (host.to_string(), port);
    let result = if proxy.username().is_empty() {
        tokio_socks::tcp::Socks5Stream::connect_with_socket(stream, target).await
    } else {
        tokio_socks::tcp::Socks5Stream::connect_with_password_and_socket(stream, target, proxy.username(), proxy.password().unwrap_or("")).await
    };
    let stream = result.map_err(|e| io::Error::other(format!("socks connect: {e}")))?;
    Ok(Box::new(stream))
}

/// Opens the byte stream to `url`'s host through the configured route.
async fn connect_stream(url: &Url, proxy_url: &str) -> io::Result<BoxIo> {
    let tls = matches!(url.scheme(), "wss" | "https");
    let host = url.host_str().ok_or_else(|| io::Error::other("missing host"))?.trim_matches(['[', ']']).to_string();
    let port = url.port_or_known_default().unwrap_or(if tls { 443 } else { 80 });
    let stream: BoxIo = match route_for(proxy_url, tls, &host, port) {
        Route::Direct => Box::new(tcp_connect(&host, port).await?),
        Route::Http(proxy) => http_connect(&proxy, &host, port).await?,
        Route::Socks5(proxy) => socks5_connect(&proxy, &host, port).await?,
    };
    if tls { tls_wrap(stream, &host).await } else { Ok(stream) }
}

/// Dials `url` (`ws`/`wss`) with `headers` and the client's requested subprotocols.
pub async fn dial(url: &str, headers: &HeaderMap, subprotocols: &[String], proxy_url: &str) -> Result<Dialed, DialFailure> {
    let parsed = Url::parse(url).map_err(DialFailure::transport)?;
    if !matches!(parsed.scheme(), "ws" | "wss") {
        return Err(DialFailure::transport("malformed ws or wss URL"));
    }
    let mut request = url.into_client_request().map_err(DialFailure::transport)?;
    for (name, value) in headers {
        request.headers_mut().append(name.clone(), value.clone());
    }
    if !subprotocols.is_empty()
        && let Ok(v) = HeaderValue::from_str(&subprotocols.join(", "))
    {
        request.headers_mut().insert(HeaderName::from_static("sec-websocket-protocol"), v);
    }
    let stream = connect_stream(&parsed, proxy_url).await.map_err(DialFailure::transport)?;
    let config = WebSocketConfig::default().max_message_size(None).max_frame_size(None);
    match tokio_tungstenite::client_async_with_config(request, stream, Some(config)).await {
        Ok((stream, response)) => {
            let subprotocol = response.headers().get("sec-websocket-protocol").and_then(|v| v.to_str().ok()).map(str::to_string);
            Ok(Dialed { stream, subprotocol, response_headers: response.headers().clone(), status: response.status().as_u16() })
        }
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            let mut body = response.body().clone().unwrap_or_default();
            body.truncate(ERROR_BODY_LIMIT);
            Err(DialFailure {
                status: Some(response.status().as_u16()),
                headers: response.headers().clone(),
                body,
                error: "websocket: bad handshake".into(),
            })
        }
        Err(e) => Err(DialFailure::transport(e)),
    }
}

