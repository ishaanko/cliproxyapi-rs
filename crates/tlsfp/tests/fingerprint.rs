//! Compares the ClientHello this crate emits with captures of the Go reference (uTLS), taken with
//! the same hosts the Go executors use. The Go captures live in `tests/fixtures/*.hex` (raw TLS
//! records, regenerate with a throwaway Go test calling `tls.UClient` against a local listener).
//!
//! The Claude profiles are deterministic, so every extension is compared byte for byte (except
//! the random key share). The Chrome profile randomizes GREASE and extension order per
//! connection, so the order-invariant JA4 and the per-extension contents are compared instead.

mod common;

use common::clienthello::{ClientHello, is_grease};
use cpa_tlsfp::profile::{Profile, TlsConnector};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

fn bind() -> std::net::TcpListener {
    if let Ok(l) = std::net::TcpListener::bind("127.0.0.1:0") {
        return l;
    }
    // Some sandboxes refuse ephemeral binds; fall back to a fixed range.
    (39400..39600u16)
        .find_map(|port| std::net::TcpListener::bind(("127.0.0.1", port)).ok())
        .expect("no free local port")
}

/// Handshakes `connector` against a listener that records the first TLS record and hangs up.
async fn capture(connector: &TlsConnector, host: &str) -> Vec<u8> {
    let std_listener = bind();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    let listener = TcpListener::from_std(std_listener).unwrap();
    let server = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let mut header = [0u8; 5];
        conn.read_exact(&mut header).await.unwrap();
        let len = usize::from(u16::from_be_bytes([header[3], header[4]]));
        let mut body = vec![0u8; len];
        conn.read_exact(&mut body).await.unwrap();
        [header.to_vec(), body].concat()
    });
    let stream = TcpStream::connect(addr).await.unwrap();
    let connector = connector.clone();
    let host_owned = host.to_string();
    let client = tokio::spawn(async move {
        let _ = connector.connect(&host_owned, stream).await;
    });
    let record = server.await.unwrap();
    client.abort();
    if let Ok(dir) = std::env::var("TLSFP_DUMP_DIR") {
        let hex: String = record.iter().map(|b| format!("{b:02x}")).collect();
        let _ = std::fs::write(format!("{dir}/rust_{host}.hex"), hex);
    }
    record
}

fn fixture(name: &str) -> ClientHello {
    let path = format!("{}/tests/fixtures/{name}.hex", env!("CARGO_MANIFEST_DIR"));
    let hex = std::fs::read_to_string(path).unwrap();
    let bytes: Vec<u8> = (0..hex.trim().len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex.trim()[i..i + 2], 16).unwrap())
        .collect();
    ClientHello::parse(&bytes).unwrap()
}

/// Extension data with the random key share bytes masked (group ids and lengths kept).
fn masked(hello: &ClientHello) -> Vec<(u16, Vec<u8>)> {
    hello
        .extensions
        .iter()
        .map(|(t, d)| match *t {
            51 => (*t, hello.key_share_groups().iter().flat_map(|g| g.to_be_bytes()).chain(d.len().to_be_bytes()).collect()),
            _ => (*t, d.clone()),
        })
        .collect()
}

async fn assert_deterministic_match(profile: Profile, host: &str, golden: &str) {
    let go = fixture(golden);
    let connector = TlsConnector::new(profile, Some(8), &[]).unwrap();
    let raw = capture(&connector, host).await;
    let rust = ClientHello::parse(&raw).unwrap();
    assert_eq!(rust.ciphers, go.ciphers, "ciphers");
    assert_eq!(rust.compression, go.compression);
    assert_eq!(rust.session_id_len, go.session_id_len);
    assert_eq!(rust.extension_order(), go.extension_order(), "extension order");
    assert_eq!(masked(&rust), masked(&go), "extension contents");
    assert_eq!(rust.record_len, go.record_len, "record length");
    assert_eq!(rust.ja3_string(), go.ja3_string());
    eprintln!("{profile:?} ja3={} ja4={}", rust.ja3_string(), rust.ja4());
    assert_eq!(rust.ja4(), go.ja4());
}

#[tokio::test]
async fn claude_inference_matches_go() {
    assert_deterministic_match(Profile::ClaudeInference, "api.anthropic.com", "go_claude_inference").await;
}

#[tokio::test]
async fn claude_oauth_matches_go() {
    assert_deterministic_match(Profile::ClaudeOAuth, "platform.claude.com", "go_claude_oauth").await;
}

#[tokio::test]
async fn chrome_matches_go() {
    let go = fixture("go_chrome_chatgpt");
    let connector = TlsConnector::new(Profile::Chrome, None, &[]).unwrap();
    let rust = ClientHello::parse(&capture(&connector, "chatgpt.com").await).unwrap();
    let norm = |h: &ClientHello| -> Vec<u16> { h.ciphers.iter().map(|c| if is_grease(*c) { 0x0a0a } else { *c }).collect() };
    assert_eq!(norm(&rust), norm(&go), "ciphers");
    let mut a = rust.extension_order();
    let mut b = go.extension_order();
    // ECH GREASE payload size and the trailing GREASE type are random; the set of types is not.
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b, "extension set");
    assert_eq!(rust.extensions.first().map(|e| is_grease(e.0)), Some(true), "leading GREASE extension");
    assert_eq!(rust.extensions.last().map(|e| is_grease(e.0)), Some(true), "trailing GREASE extension");
    // Groups (10) and supported versions (43) carry per-connection GREASE values; JA4 covers them.
    for ty in [0u16, 5, 11, 13, 16, 18, 23, 27, 35, 45, 65281, 17613] {
        assert_eq!(rust.extension(ty), go.extension(ty), "extension {ty}");
    }
    assert_eq!(rust.key_share_groups(), go.key_share_groups(), "key share groups");
    eprintln!("Chrome ja4={} (go {})", rust.ja4(), go.ja4());
    assert_eq!(rust.ja4(), go.ja4(), "ja4");
}
