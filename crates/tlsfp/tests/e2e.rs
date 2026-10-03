//! End to end behavior of the fingerprinted clients against a local BoringSSL server: wire header
//! order, response decoding, session resumption, HTTP/2 settings, and proxy tunnels.

use std::io::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cpa_tlsfp::clienthello::ClientHello;
use cpa_tlsfp::{ClientConfig, FingerprintClient};
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use parking_lot::Mutex;
use rama_boring::asn1::Asn1Time;
use rama_boring::bn::BigNum;
use rama_boring::ec::{EcGroup, EcKey};
use rama_boring::hash::MessageDigest;
use rama_boring::nid::Nid;
use rama_boring::pkey::PKey;
use rama_boring::ssl::{AlpnError, SslAcceptor, SslMethod};
use rama_boring::x509::extension::SubjectAlternativeName;
use rama_boring::x509::{X509, X509NameBuilder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Copy)]
enum Behavior {
    /// Reads one request, answers with a gzip body and closes (`Connection: close`).
    H1Close,
    /// Records the client's connection preface and first frames, then hangs up.
    H2Raw,
    /// A real HTTP/2 server answering `ok`.
    H2Serve,
}

#[derive(Default)]
struct Seen {
    hellos: Vec<ClientHello>,
    /// Decrypted request bytes (HTTP/1.1) or first client frames (H2Raw), one entry per connection.
    raw: Vec<Vec<u8>>,
    alpn: Vec<Option<Vec<u8>>>,
}

struct Server {
    port: u16,
    cert_der: Vec<u8>,
    seen: Arc<Mutex<Seen>>,
}

fn bind() -> std::net::TcpListener {
    if let Ok(l) = std::net::TcpListener::bind("127.0.0.1:0") {
        return l;
    }
    (39600..39800u16)
        .find_map(|port| std::net::TcpListener::bind(("127.0.0.1", port)).ok())
        .expect("no free local port")
}

fn self_signed() -> (X509, PKey<rama_boring::pkey::Private>) {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    let key = PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "localhost").unwrap();
    let name = name.build();
    let mut b = X509::builder().unwrap();
    b.set_version(2).unwrap();
    b.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap()).unwrap();
    b.set_subject_name(&name).unwrap();
    b.set_issuer_name(&name).unwrap();
    b.set_pubkey(&key).unwrap();
    b.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
    b.set_not_after(&Asn1Time::days_from_now(2).unwrap()).unwrap();
    let san = SubjectAlternativeName::new().dns("localhost").build(&b.x509v3_context(None, None)).unwrap();
    b.append_extension(&san).unwrap();
    b.sign(&key, MessageDigest::sha256()).unwrap();
    (b.build(), key)
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

/// Reads one HTTP/1.1 request (head and Content-Length body) from `conn`.
async fn read_h1_request<S: AsyncReadExt + Unpin>(conn: &mut S) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = conn.read(&mut chunk).await.unwrap();
        assert!(n > 0, "client hung up mid request");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if buf.len() >= end + 4 + len {
                return buf;
            }
        }
    }
}

async fn start_server(behavior: Behavior, alpn: &'static [&'static [u8]]) -> Server {
    let (cert, key) = self_signed();
    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).unwrap();
    acceptor.set_certificate(&cert).unwrap();
    acceptor.set_private_key(&key).unwrap();
    acceptor.set_alpn_select_callback(move |_, client| {
        // Server preference order; returns the matching slice of the client's list.
        for want in alpn {
            let mut rest = client;
            while let Some((&len, tail)) = rest.split_first() {
                let (proto, next) = tail.split_at(usize::from(len).min(tail.len()));
                if proto == *want {
                    return Ok(proto);
                }
                rest = next;
            }
        }
        Err(AlpnError::NOACK)
    });
    let acceptor = acceptor.build();
    let std_listener = bind();
    std_listener.set_nonblocking(true).unwrap();
    let port = std_listener.local_addr().unwrap().port();
    let listener = TcpListener::from_std(std_listener).unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let seen_task = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else { return };
            let acceptor = acceptor.clone();
            let seen = Arc::clone(&seen_task);
            tokio::spawn(async move {
                // Peek the ClientHello record without consuming it.
                let mut head = vec![0u8; 16384];
                let mut hello = None;
                for _ in 0..200 {
                    let n = tcp.peek(&mut head).await.unwrap_or(0);
                    if n >= 5 {
                        let want = 5 + usize::from(u16::from_be_bytes([head[3], head[4]]));
                        if n >= want {
                            hello = ClientHello::parse(&head[..want]);
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                if let Some(h) = hello {
                    seen.lock().hellos.push(h);
                }
                let Ok(mut tls) = rama_boring_tokio::accept(&acceptor, tcp).await else { return };
                seen.lock().alpn.push(tls.ssl().selected_alpn_protocol().map(<[u8]>::to_vec));
                match behavior {
                    Behavior::H1Close => {
                        let req = read_h1_request(&mut tls).await;
                        seen.lock().raw.push(req);
                        let body = gzip(b"hello from server");
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        tls.write_all(head.as_bytes()).await.unwrap();
                        tls.write_all(&body).await.unwrap();
                        tls.flush().await.unwrap();
                        // Keep the stream open briefly so the client reads the response (and the
                        // TLS 1.3 session tickets) before the FIN.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Behavior::H2Raw => {
                        let mut got = Vec::new();
                        let mut chunk = [0u8; 4096];
                        // Preface (24) + SETTINGS + WINDOW_UPDATE, whatever arrives within a bit.
                        while got.len() < 24 + 9 + 24 + 13 {
                            match tokio::time::timeout(Duration::from_secs(2), tls.read(&mut chunk)).await {
                                Ok(Ok(n)) if n > 0 => got.extend_from_slice(&chunk[..n]),
                                _ => break,
                            }
                        }
                        seen.lock().raw.push(got);
                    }
                    Behavior::H2Serve => {
                        let svc = service_fn(|req: hyper::Request<hyper::body::Incoming>| async move {
                            let ua = req.headers().get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                            Ok::<_, std::convert::Infallible>(hyper::Response::new(http_body_util::Full::new(bytes::Bytes::from(
                                format!("ok h2 ua={ua}"),
                            ))))
                        });
                        let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                            .serve_connection(TokioIo::new(tls), svc)
                            .await;
                    }
                }
            });
        }
    });
    Server { port, cert_der: cert.to_der().unwrap(), seen }
}

fn config(server: &Server, proxy: &str) -> ClientConfig {
    ClientConfig {
        proxy: proxy.to_string(),
        timeout: Some(Duration::from_secs(10)),
        extra_roots: vec![server.cert_der.clone()],
        ..ClientConfig::default()
    }
}

fn post(url: &str, headers: &[(&str, &str)], body: &str) -> reqwest::Request {
    let mut b = reqwest::Client::new().post(url);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(body.to_string()).build().unwrap()
}

#[tokio::test]
async fn claude_inference_orders_headers_decodes_and_resumes() {
    let server = start_server(Behavior::H1Close, &[b"http/1.1"]).await;
    let client = FingerprintClient::claude_inference(config(&server, "")).unwrap();
    let url = format!("https://localhost:{}/v1/messages?beta=true", server.port);
    let headers = [
        ("x-app", "cli"),
        ("anthropic-version", "2023-06-01"),
        ("x-stainless-os", "MacOS"),
        ("user-agent", "claude-cli/2.1.220"),
        ("content-type", "application/json"),
        ("authorization", "Bearer t"),
        ("accept", "application/json"),
        ("accept-encoding", "gzip, deflate, br, zstd"),
        ("x-zzz-extra", "1"),
        ("a-extra", "2"),
    ];
    let resp = client.execute(post(&url, &headers, "{\"a\":1}")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("content-encoding").is_none(), "decoded bodies drop Content-Encoding");
    assert_eq!(resp.text().await.unwrap(), "hello from server");

    let raw = String::from_utf8(server.seen.lock().raw[0].clone()).unwrap();
    let lines: Vec<&str> = raw.split("\r\n").take_while(|l| !l.is_empty()).collect();
    let names: Vec<&str> = lines.iter().skip(1).map(|l| l.split(':').next().unwrap()).collect();
    assert_eq!(lines[0], "POST /v1/messages?beta=true HTTP/1.1");
    assert_eq!(
        names,
        ["Accept", "Authorization", "Content-Type", "User-Agent", "X-Stainless-OS", "anthropic-version", "x-app", "Host", "Accept-Encoding", "Content-Length", "A-Extra", "X-Zzz-Extra"]
    );
    assert!(raw.ends_with("\r\n\r\n{\"a\":1}"));

    // Second request: a new connection that resumes the first session (pre_shared_key last).
    tokio::time::sleep(Duration::from_millis(200)).await;
    let resp = client.execute(post(&url, &headers, "{}")).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello from server");
    let seen = server.seen.lock();
    assert_eq!(seen.hellos.len(), 2);
    assert!(seen.hellos[0].extension(41).is_none());
    assert_eq!(seen.hellos[1].extensions.last().map(|e| e.0), Some(41), "resumption sends pre_shared_key last");
}

#[tokio::test]
async fn claude_oauth_uses_inspect_order_for_profile_gets() {
    let server = start_server(Behavior::H1Close, &[]).await;
    let client = FingerprintClient::claude_oauth(config(&server, "")).unwrap();
    let url = format!("https://localhost:{}/api/oauth/profile", server.port);
    let req = reqwest::Client::new()
        .get(&url)
        .header("user-agent", "axios/1.8.4")
        .header("authorization", "Bearer t")
        .header("accept", "application/json, text/plain, */*")
        .header("content-type", "application/json")
        .header("accept-encoding", "gzip, compress, deflate, br")
        .build()
        .unwrap();
    let resp = client.execute(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let raw = String::from_utf8(server.seen.lock().raw[0].clone()).unwrap();
    let names: Vec<&str> = raw.split("\r\n").skip(1).take_while(|l| !l.is_empty()).map(|l| l.split(':').next().unwrap()).collect();
    assert_eq!(names, ["Accept", "Content-Type", "Authorization", "User-Agent", "Accept-Encoding", "Host"]);
    assert!(server.seen.lock().alpn[0].is_none(), "the OAuth profile sends no ALPN");
}

#[tokio::test]
async fn chrome_speaks_h2_with_go_transport_settings() {
    let server = start_server(Behavior::H2Serve, &[b"h2", b"http/1.1"]).await;
    let client = FingerprintClient::chrome(config(&server, "")).unwrap();
    let url = format!("https://localhost:{}/backend-api/codex/responses", server.port);
    let resp = client.execute(post(&url, &[("content-type", "application/json")], "{}")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok h2 ua=Go-http-client/2.0");
    assert_eq!(server.seen.lock().alpn[0].as_deref(), Some(&b"h2"[..]));
}

#[tokio::test]
async fn chrome_h2_connection_preface_matches_go_http2_transport() {
    let server = start_server(Behavior::H2Raw, &[b"h2"]).await;
    let client = FingerprintClient::chrome(config(&server, "")).unwrap();
    let url = format!("https://localhost:{}/x", server.port);
    let _ = client.execute(post(&url, &[], "{}")).await;
    let raw = server.seen.lock().raw[0].clone();
    assert_eq!(&raw[..24], b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    let len = (usize::from(raw[24]) << 16) | (usize::from(raw[25]) << 8) | usize::from(raw[26]);
    assert_eq!(raw[27], 4, "SETTINGS frame");
    let settings: Vec<(u16, u32)> = raw[33..33 + len]
        .chunks_exact(6)
        .map(|c| (u16::from_be_bytes([c[0], c[1]]), u32::from_be_bytes([c[2], c[3], c[4], c[5]])))
        .collect();
    // Go: ENABLE_PUSH=0, INITIAL_WINDOW_SIZE=4MiB, MAX_FRAME_SIZE=1MiB, MAX_HEADER_LIST_SIZE=10MiB.
    assert_eq!(settings, [(2, 0), (4, 4 << 20), (5, 1 << 20), (6, 10 << 20)]);
    // Go then raises the connection window by 1 GiB.
    let wu = &raw[33 + len..];
    assert_eq!(&wu[..9], [0, 0, 4, 8, 0, 0, 0, 0, 0], "connection WINDOW_UPDATE frame");
    assert_eq!(u32::from_be_bytes([wu[9], wu[10], wu[11], wu[12]]), 1 << 30);
}

/// A CONNECT proxy that tunnels to `target` and records the request line.
async fn start_connect_proxy(target: SocketAddr) -> (u16, Arc<Mutex<Vec<String>>>) {
    let std_listener = bind();
    std_listener.set_nonblocking(true).unwrap();
    let port = std_listener.local_addr().unwrap().port();
    let listener = TcpListener::from_std(std_listener).unwrap();
    let lines = Arc::new(Mutex::new(Vec::new()));
    let lines_task = Arc::clone(&lines);
    tokio::spawn(async move {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else { return };
            let lines = Arc::clone(&lines_task);
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if conn.read(&mut b).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(b[0]);
                }
                lines.lock().push(String::from_utf8_lossy(&head).into_owned());
                let Ok(mut upstream) = TcpStream::connect(target).await else { return };
                conn.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut conn, &mut upstream).await;
            });
        }
    });
    (port, lines)
}

/// A no-auth SOCKS5 proxy tunneling every request to `target`; records the requested host.
async fn start_socks_proxy(target: SocketAddr) -> (u16, Arc<Mutex<Vec<String>>>) {
    let std_listener = bind();
    std_listener.set_nonblocking(true).unwrap();
    let port = std_listener.local_addr().unwrap().port();
    let listener = TcpListener::from_std(std_listener).unwrap();
    let hosts = Arc::new(Mutex::new(Vec::new()));
    let hosts_task = Arc::clone(&hosts);
    tokio::spawn(async move {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else { return };
            let hosts = Arc::clone(&hosts_task);
            tokio::spawn(async move {
                let mut greeting = [0u8; 2];
                conn.read_exact(&mut greeting).await.unwrap();
                let mut methods = vec![0u8; usize::from(greeting[1])];
                conn.read_exact(&mut methods).await.unwrap();
                conn.write_all(&[5, 0]).await.unwrap();
                let mut req = [0u8; 4];
                conn.read_exact(&mut req).await.unwrap();
                assert_eq!(req[3], 3, "domain names are resolved by the proxy");
                let mut len = [0u8; 1];
                conn.read_exact(&mut len).await.unwrap();
                let mut host = vec![0u8; usize::from(len[0]) + 2];
                conn.read_exact(&mut host).await.unwrap();
                hosts.lock().push(String::from_utf8_lossy(&host[..usize::from(len[0])]).into_owned());
                let Ok(mut upstream) = TcpStream::connect(target).await else { return };
                conn.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut conn, &mut upstream).await;
            });
        }
    });
    (port, hosts)
}

#[tokio::test]
async fn tunnels_through_http_connect_and_socks5_proxies() {
    let server = start_server(Behavior::H1Close, &[b"http/1.1"]).await;
    let target: SocketAddr = ([127, 0, 0, 1], server.port).into();
    // The URL host is never resolved locally: it only reaches the proxy as the tunnel target.
    let url = format!("https://localhost:{}/v1/messages", server.port);

    let (http_port, connects) = start_connect_proxy(target).await;
    let client = FingerprintClient::claude_inference(config(&server, &format!("http://u:p@127.0.0.1:{http_port}"))).unwrap();
    let resp = client.execute(post(&url, &[], "{}")).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello from server");
    let head = connects.lock()[0].clone();
    assert!(head.starts_with(&format!("CONNECT localhost:{} HTTP/1.1\r\n", server.port)), "{head}");
    assert!(head.contains("Proxy-Authorization: Basic dTpw\r\n"), "{head}");

    let (socks_port, hosts) = start_socks_proxy(target).await;
    let client = FingerprintClient::chrome(config(&server, &format!("socks5://127.0.0.1:{socks_port}"))).unwrap();
    let resp = client.execute(post(&url, &[], "{}")).await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "hello from server");
    assert_eq!(hosts.lock().as_slice(), ["localhost"]);
}
