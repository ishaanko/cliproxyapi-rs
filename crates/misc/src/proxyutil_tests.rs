use super::*;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;

#[test]
fn parse_modes() {
    for (input, want, err) in [
        ("", Mode::Inherit, false),
        ("direct", Mode::Direct, false),
        ("none", Mode::Direct, false),
        ("http://proxy.example.com:8080", Mode::Proxy, false),
        ("https://proxy.example.com:8443", Mode::Proxy, false),
        ("socks5://proxy.example.com:1080", Mode::Proxy, false),
        ("socks5h://proxy.example.com:1080", Mode::Proxy, false),
        ("bad-value", Mode::Invalid, true),
    ] {
        let (mode, is_err) = match parse(input) {
            Ok(s) => (s.mode, false),
            Err((s, _)) => (s.mode, true),
        };
        assert_eq!((mode, is_err), (want, err), "{input}");
    }
}

#[test]
fn parse_error_does_not_expose_credentials() {
    let (_, err) = parse("http://user:secret%@proxy.example.com:8080").unwrap_err();
    let text = err.to_string();
    assert!(!text.contains("user") && !text.contains("secret"), "{text}");
}

#[test]
fn parse_follows_go_url_rules() {
    assert_eq!(parse("ftp://h:1").unwrap_err().1, ParseError::UnsupportedScheme("ftp".into()));
    assert_eq!(parse("http://").unwrap_err().1, ParseError::MissingSchemeHost);
    assert_eq!(parse("localhost:8080").unwrap_err().1, ParseError::MissingSchemeHost);
    assert_eq!(parse("1.2.3.4:80").unwrap_err().1, ParseError::Parse);
    assert_eq!(parse("http://h:abc").unwrap_err().1, ParseError::Parse);
    // Go accepts an out-of-range port at parse time; ValidRequestProxy rejects it.
    let ok = parse(" HTTP://u:p%40w@Proxy.Example.com:99999/x ").unwrap();
    let url = ok.url.unwrap();
    assert_eq!((url.scheme.as_str(), url.host.as_str(), url.port()), ("http", "Proxy.Example.com:99999", "99999"));
    assert_eq!((url.username.as_deref(), url.password.as_deref()), (Some("u"), Some("p@w")));
}

#[test]
fn valid_request_proxy_checks_host_and_port() {
    assert!(valid_request_proxy("http://proxy.example.com"));
    assert!(valid_request_proxy("socks5://[::1]:1080"));
    assert!(!valid_request_proxy("http://proxy.example.com:0"));
    assert!(!valid_request_proxy("http://proxy.example.com:70000"));
    assert!(!valid_request_proxy("direct"));
    assert!(!valid_request_proxy(""));
    assert!(!valid_request_proxy("bad-value"));
}

#[test]
fn redact_proxy_url() {
    assert_eq!(redact("http://user:pass@proxy.example.com:8080/path?token=secret"), "http://redacted@proxy.example.com:8080");
    assert_eq!(redact("socks5://proxy.example.com:1080"), "socks5://proxy.example.com:1080");
    assert_eq!(redact("bad-value"), "<invalid proxy URL>");
    assert_eq!(redact("  "), "");
}

/// Plays a proxy: reads the CONNECT request head, checks it, answers 200 with an early "ok"
/// payload and expects "ping" through the tunnel.
async fn serve_connect<S: AsyncRead + AsyncWrite + Unpin>(stream: S) -> Result<(), String> {
    let mut reader = BufReader::new(stream);
    let mut head = Vec::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("eof before request head".into());
        }
        if line == "\r\n" {
            break;
        }
        head.push(line);
    }
    if head[0] != "CONNECT target.example.com:443 HTTP/1.1\r\n" || !head.contains(&"Host: target.example.com:443\r\n".to_string()) {
        return Err(format!("bad request head: {head:?}"));
    }
    let want_auth = format!("Proxy-Authorization: Basic {}\r\n", base64::engine::general_purpose::STANDARD.encode("user:pass"));
    if !head.contains(&want_auth) {
        return Err(format!("missing proxy auth: {head:?}"));
    }
    reader
        .get_mut()
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\nok")
        .await
        .map_err(|e| e.to_string())?;
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
    if &buf != b"ping" {
        return Err(format!("tunneled payload = {buf:?}"));
    }
    Ok(())
}

async fn assert_tunnel(dialer: Dialer) {
    let mut conn = tokio::time::timeout(Duration::from_secs(5), dialer.dial("target.example.com:443"))
        .await
        .unwrap()
        .unwrap();
    let mut buf = [0u8; 2];
    conn.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ok", "buffered tunnel payload");
    conn.write_all(b"ping").await.unwrap();
    conn.flush().await.unwrap();
}

#[tokio::test]
async fn http_proxy_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
        serve_connect(stream).await
    });
    let (dialer, mode) = build_dialer(&format!("http://user:pass@{addr}")).unwrap();
    assert_eq!(mode, Mode::Proxy);
    assert_tunnel(dialer.unwrap()).await;
    assert_eq!(server.await.unwrap(), Ok(()));
}

#[tokio::test]
async fn https_proxy_connect() {
    let key = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
    let cert_der = key.cert.der().clone();
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(key.signing_key.serialize_der());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server_config = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der.clone()], key_der.into())
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert_der).unwrap();
    let client_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let tls = acceptor.accept(stream).await.map_err(|e| e.to_string())?;
        if tls.get_ref().1.alpn_protocol().is_some_and(|p| p != b"http/1.1") {
            return Err("negotiated protocol is not http/1.1".to_string());
        }
        serve_connect(tls).await
    });
    let (dialer, _) = build_dialer(&format!("https://user:pass@{addr}")).unwrap();
    assert_tunnel(dialer.unwrap().with_tls_config(Arc::new(client_config))).await;
    assert_eq!(server.await.unwrap(), Ok(()));
}

#[tokio::test]
async fn http_proxy_connect_cancellation_closes_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (read_tx, read_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap() > 0 && line != "\r\n" {
            line.clear();
        }
        let _ = read_tx.send(());
        let mut byte = [0u8; 1];
        reader.read(&mut byte).await
    });
    let (dialer, _) = build_dialer(&format!("http://{addr}")).unwrap();
    let dial = tokio::spawn(async move { dialer.unwrap().dial("20.42.0.20:443").await.map(|_| ()) });
    tokio::time::timeout(Duration::from_secs(1), read_rx).await.unwrap().unwrap();
    dial.abort();
    assert!(dial.await.unwrap_err().is_cancelled());
    // The proxy side sees EOF once the aborted dial drops its connection.
    let read = tokio::time::timeout(Duration::from_secs(1), server).await.unwrap().unwrap();
    assert_eq!(read.unwrap(), 0);
}

#[tokio::test]
async fn non_200_connect_status_is_an_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 512];
        let _ = stream.read(&mut buf).await;
        let _ = stream.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n").await;
    });
    let (dialer, _) = build_dialer(&format!("http://{addr}")).unwrap();
    let err = dialer.unwrap().dial("t.example.com:443").await.err().unwrap();
    assert_eq!(err.to_string(), "proxy CONNECT returned status 407 Proxy Authentication Required");
}

#[tokio::test]
async fn socks5_connect_with_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.map_err(|e| e.to_string())?;
        let mut greet = [0u8; 4];
        s.read_exact(&mut greet).await.map_err(|e| e.to_string())?;
        if greet != [5, 2, 0, 2] {
            return Err(format!("greeting {greet:?}"));
        }
        s.write_all(&[5, 2]).await.map_err(|e| e.to_string())?;
        let mut auth = [0u8; 11];
        s.read_exact(&mut auth).await.map_err(|e| e.to_string())?;
        if &auth != b"\x01\x04user\x04pass" {
            return Err(format!("auth {auth:?}"));
        }
        s.write_all(&[1, 0]).await.map_err(|e| e.to_string())?;
        let mut req = vec![0u8; 5 + "target.example.com".len() + 2];
        s.read_exact(&mut req).await.map_err(|e| e.to_string())?;
        if req[..5] != [5, 1, 0, 3, 18] || req[5..23] != *b"target.example.com" || req[23..] != [1, 187] {
            return Err(format!("request {req:?}"));
        }
        s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.map_err(|e| e.to_string())?;
        s.write_all(b"ok").await.map_err(|e| e.to_string())?;
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
        if &buf != b"ping" {
            return Err(format!("payload {buf:?}"));
        }
        Ok(())
    });
    let (dialer, _) = build_dialer(&format!("socks5h://user:pass@{addr}")).unwrap();
    assert_tunnel(dialer.unwrap()).await;
    assert_eq!(server.await.unwrap(), Ok(()));
}
