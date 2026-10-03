//! Connection-level proxy dialing (Go: `proxyutil.BuildDialer` + `proxy.Direct`).
//!
//! The uTLS transports never read proxy environment variables: an empty or invalid proxy setting
//! dials directly, `direct`/`none` dials directly, and `socks5`/`socks5h`/`http`/`https` URLs
//! tunnel (SOCKS5 with remote name resolution, HTTP CONNECT, CONNECT over TLS for `https`).

use std::io;

use base64::Engine as _;
use rama_boring::ssl::{SslConnector, SslMethod, SslVerifyMode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_socks::tcp::Socks5Stream;

const KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(15);

/// A type-erased duplex connection.
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type BoxIo = Box<dyn Io>;

/// How outbound connections are established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialer {
    Direct,
    Socks5 { addr: (String, u16), auth: Option<(String, String)> },
    HttpConnect { addr: (String, u16), tls: bool, authorization: Option<String> },
}

/// Redacts credentials for logs (Go: `proxyutil.Redact`).
pub fn redact(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    match url::Url::parse(raw) {
        Ok(u) if u.host_str().is_some() => {
            let user = if u.username().is_empty() && u.password().is_none() { "" } else { "redacted@" };
            let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
            format!("{}://{user}{}{port}", u.scheme(), u.host_str().unwrap_or_default())
        }
        _ => "<invalid proxy URL>".to_string(),
    }
}

/// Percent-decodes URL userinfo (Go's `url.Userinfo` accessors return decoded values).
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Dialer {
    /// Dialer for a proxy setting. `label` prefixes the error log for an invalid setting, which
    /// falls back to a direct dial like Go.
    pub fn from_setting(raw: &str, label: &str) -> Dialer {
        match Self::parse(raw) {
            Ok(d) => d,
            Err(err) => {
                tracing::error!("{label}: failed to configure proxy dialer for {:?}: {err}", redact(raw));
                Dialer::Direct
            }
        }
    }

    /// Go: `proxyutil.Parse` + `BuildDialer`; inherit and direct both dial directly here.
    pub fn parse(raw: &str) -> Result<Dialer, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("direct") || trimmed.eq_ignore_ascii_case("none") {
            return Ok(Dialer::Direct);
        }
        let url = url::Url::parse(trimmed).map_err(|_| "parse proxy URL failed".to_string())?;
        let host = url.host_str().unwrap_or_default().trim_matches(['[', ']']).to_string();
        if host.is_empty() {
            return Err("proxy URL missing scheme/host".into());
        }
        let userinfo = (!url.username().is_empty() || url.password().is_some())
            .then(|| (decode(url.username()), decode(url.password().unwrap_or_default())));
        match url.scheme() {
            "socks5" | "socks5h" => Ok(Dialer::Socks5 { addr: (host, url.port().unwrap_or(1080)), auth: userinfo }),
            scheme @ ("http" | "https") => {
                let tls = scheme == "https";
                let port = url.port().unwrap_or(if tls { 443 } else { 80 });
                let authorization = userinfo.map(|(u, p)| {
                    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}")))
                });
                Ok(Dialer::HttpConnect { addr: (host, port), tls, authorization })
            }
            other => Err(format!("unsupported proxy scheme: {other}")),
        }
    }

    /// Opens a TCP tunnel to `host:port`.
    pub async fn dial(&self, host: &str, port: u16) -> io::Result<BoxIo> {
        match self {
            Dialer::Direct => Ok(Box::new(tcp(host, port).await?)),
            Dialer::Socks5 { addr, auth } => {
                let proxy = tcp(&addr.0, addr.1).await.map_err(|e| io::Error::new(e.kind(), format!("dial proxy: {e}")))?;
                let target = (host, port);
                let stream = match auth {
                    Some((user, pass)) => Socks5Stream::connect_with_password_and_socket(proxy, target, user, pass).await,
                    None => Socks5Stream::connect_with_socket(proxy, target).await,
                }
                .map_err(|e| io::Error::other(format!("socks connect {host}:{port}: {e}")))?;
                Ok(Box::new(stream.into_inner()))
            }
            Dialer::HttpConnect { addr, tls, authorization } => {
                let proxy = tcp(&addr.0, addr.1)
                    .await
                    .map_err(|e| io::Error::new(e.kind(), format!("dial HTTP proxy failed: {e}")))?;
                let mut conn: BoxIo = if *tls { Box::new(proxy_tls(&addr.0, proxy).await?) } else { Box::new(proxy) };
                http_connect(&mut conn, host, port, authorization.as_deref()).await?;
                Ok(conn)
            }
        }
    }
}

async fn tcp(host: &str, port: u16) -> io::Result<TcpStream> {
    let stream = TcpStream::connect((host, port)).await?;
    // Go enables TCP_NODELAY and 15 s keep-alives (`net.Dialer` defaults) on every connection.
    stream.set_nodelay(true)?;
    let keepalive = socket2::TcpKeepalive::new().with_time(KEEPALIVE).with_interval(KEEPALIVE);
    socket2::SockRef::from(&stream).set_tcp_keepalive(&keepalive)?;
    Ok(stream)
}

/// TLS to an `https://` proxy with a stock client hello (Go uses crypto/tls with ALPN http/1.1).
async fn proxy_tls(host: &str, stream: TcpStream) -> io::Result<rama_boring_tokio::SslStream<TcpStream>> {
    let other = |e: String| io::Error::other(format!("HTTPS proxy TLS handshake failed: {e}"));
    let mut b = SslConnector::no_default_verify_builder(SslMethod::tls_client()).map_err(|e| other(e.to_string()))?;
    crate::profile::install_roots(&mut b, &[]).map_err(|e| other(e.to_string()))?;
    b.set_verify(SslVerifyMode::PEER);
    b.set_alpn_protos(b"\x08http/1.1").map_err(|e| other(e.to_string()))?;
    let cfg = b.build().configure().map_err(|e| other(e.to_string()))?;
    rama_boring_tokio::connect(cfg, Some(host), stream).await.map_err(|e| other(e.to_string()))
}

/// Writes `CONNECT host:port` and consumes the response head. Reads byte-wise so no tunnel bytes
/// are swallowed (Go keeps the leftovers in a buffered conn).
async fn http_connect(conn: &mut BoxIo, host: &str, port: u16, authorization: Option<&str>) -> io::Result<()> {
    let target = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nUser-Agent: Go-http-client/1.1\r\n");
    if let Some(auth) = authorization {
        req.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    }
    req.push_str("\r\n");
    conn.write_all(req.as_bytes())
        .await
        .map_err(|e| io::Error::new(e.kind(), format!("write CONNECT request failed: {e}")))?;
    conn.flush().await?;
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 64 * 1024 {
            return Err(io::Error::other("read CONNECT response failed: header too large"));
        }
        let n = conn
            .read(&mut byte)
            .await
            .map_err(|e| io::Error::new(e.kind(), format!("read CONNECT response failed: {e}")))?;
        if n == 0 {
            return Err(io::Error::other("read CONNECT response failed: unexpected EOF"));
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    let status_line = text.lines().next().unwrap_or_default();
    let status = status_line.split_whitespace().nth(1).unwrap_or_default();
    if status != "200" {
        let reason = status_line.split_once(' ').map_or(status_line, |x| x.1);
        return Err(io::Error::other(format!("proxy CONNECT returned status {reason}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_like_proxyutil() {
        assert_eq!(Dialer::parse("").unwrap(), Dialer::Direct);
        assert_eq!(Dialer::parse(" Direct ").unwrap(), Dialer::Direct);
        assert_eq!(
            Dialer::parse("socks5://u:p%40@h:1081").unwrap(),
            Dialer::Socks5 { addr: ("h".into(), 1081), auth: Some(("u".into(), "p@".into())) }
        );
        assert!(matches!(Dialer::parse("http://h").unwrap(), Dialer::HttpConnect { addr, tls: false, .. } if addr.1 == 80));
        assert!(matches!(Dialer::parse("https://h").unwrap(), Dialer::HttpConnect { addr, tls: true, .. } if addr.1 == 443));
        assert!(Dialer::parse("ftp://h:1").is_err());
        assert!(Dialer::parse("nohost").is_err());
        assert_eq!(redact("http://user:pw@proxy:8080/x"), "http://redacted@proxy:8080");
    }
}
