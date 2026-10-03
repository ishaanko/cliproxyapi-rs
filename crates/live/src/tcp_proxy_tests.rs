//! TCP candidate tunnel tests (Go: tcp_proxy_test.go).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
use tokio::sync::mpsc;

use super::*;

pub(crate) struct RecordedDial {
    pub address: String,
    pub connection: Option<DuplexStream>,
}

/// Records every dial and hands out one end of an in-memory pipe (`recordingProxyDialer`).
pub(crate) struct RecordingDialer {
    dials: mpsc::UnboundedSender<RecordedDial>,
    error: Option<&'static str>,
}

impl RecordingDialer {
    pub fn new() -> (Arc<RecordingDialer>, mpsc::UnboundedReceiver<RecordedDial>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(RecordingDialer { dials: tx, error: None }), rx)
    }

    fn failing(error: &'static str) -> (Arc<RecordingDialer>, mpsc::UnboundedReceiver<RecordedDial>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(RecordingDialer { dials: tx, error: Some(error) }), rx)
    }
}

#[async_trait]
impl ProxyDialer for RecordingDialer {
    async fn dial(&self, address: &str) -> std::io::Result<BoxIo> {
        if let Some(e) = self.error {
            let _ = self.dials.send(RecordedDial { address: address.to_string(), connection: None });
            return Err(std::io::Error::other(e));
        }
        let (client, server) = duplex(64 * 1024);
        let _ = self.dials.send(RecordedDial { address: address.to_string(), connection: Some(server) });
        Ok(Box::new(client))
    }
}

/// Blocks until the dial future is dropped (`blockingContextDialer`).
struct BlockingDialer {
    started: mpsc::UnboundedSender<()>,
    canceled: Arc<AtomicBool>,
}

struct CancelGuard(Arc<AtomicBool>);

impl Drop for CancelGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl ProxyDialer for BlockingDialer {
    async fn dial(&self, _address: &str) -> std::io::Result<BoxIo> {
        let _guard = CancelGuard(self.canceled.clone());
        let _ = self.started.send(());
        std::future::pending::<()>().await;
        unreachable!()
    }
}

/// An upstream that is already closed (`closedUpstreamDialer`).
struct ClosedUpstreamDialer;

#[async_trait]
impl ProxyDialer for ClosedUpstreamDialer {
    async fn dial(&self, _address: &str) -> std::io::Result<BoxIo> {
        let (client, server) = duplex(1024);
        drop(server);
        Ok(Box::new(client))
    }
}

/// An RFC 4571 framed STUN Binding request (`buildTestICEFrame`).
pub(crate) fn build_test_ice_frame(username: &str, password: &str, fingerprint: bool) -> Vec<u8> {
    let mut attrs: Vec<u8> = Vec::new();
    let push = |kind: u16, value: &[u8], attrs: &mut Vec<u8>| {
        attrs.extend_from_slice(&kind.to_be_bytes());
        attrs.extend_from_slice(&(value.len() as u16).to_be_bytes());
        attrs.extend_from_slice(value);
        attrs.resize(attrs.len().div_ceil(4) * 4, 0);
    };
    push(0x0006, username.as_bytes(), &mut attrs);
    let header = |length: usize| {
        let mut h = Vec::new();
        h.extend_from_slice(&0x0001u16.to_be_bytes());
        h.extend_from_slice(&(length as u16).to_be_bytes());
        h.extend_from_slice(&0x2112_A442u32.to_be_bytes());
        h.extend_from_slice(&[7u8; 12]);
        h
    };
    let mut covered = header(attrs.len() + 24);
    covered.extend_from_slice(&attrs);
    let mut mac = Hmac::<Sha1>::new_from_slice(password.as_bytes()).unwrap();
    mac.update(&covered);
    push(0x0008, &mac.finalize().into_bytes(), &mut attrs);
    if fingerprint {
        let mut covered = header(attrs.len() + 8);
        covered.extend_from_slice(&attrs);
        let crc = crc32fast::hash(&covered) ^ 0x5354_554e;
        push(0x8028, &crc.to_be_bytes(), &mut attrs);
    }
    let mut message = header(attrs.len());
    message.extend_from_slice(&attrs);
    let mut frame = (message.len() as u16).to_be_bytes().to_vec();
    frame.extend_from_slice(&message);
    frame
}

/// Offer or answer skeleton with bundled ICE credentials (`testProxySDP`).
pub(crate) fn test_proxy_sdp(ufrag: &str, password: &str, candidates: &[String]) -> String {
    let mut out = String::from("v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1\r\n");
    for (line, mid) in [("m=audio 9 UDP/TLS/RTP/SAVPF 111", "0"), ("m=application 9 UDP/DTLS/SCTP webrtc-datachannel", "1")] {
        out.push_str(&format!("{line}\\r\\nc=IN IP4 0.0.0.0\\r\\na=mid:{mid}\\r\\na=ice-ufrag:{ufrag}\\r\\na=ice-pwd:{password}\\r\\n").replace("\\r\\n", "\r\n"));
        if mid == "0" {
            for c in candidates {
                out.push_str(&format!("a=candidate:{c}\r\n"));
            }
        }
    }
    out
}

fn candidates_of(sdp: &str) -> Vec<String> {
    let description = parse_sdp(sdp).unwrap();
    description
        .media_descriptions
        .iter()
        .flat_map(|m| m.attributes.iter())
        .filter(|a| a.is_ice_candidate())
        .map(|a| a.value.clone().unwrap_or_default())
        .collect()
}

#[tokio::test]
async fn prepare_restricts_and_rewrites_candidates() {
    let (dialer, _dials) = RecordingDialer::new();
    let answer = test_proxy_sdp(
        "remote-ufrag",
        "remote-password",
        &["1 1 udp 2130706431 20.42.0.10 3478 typ host".into(), "2 1 tcp 1671430143 20.42.0.20 443 typ host tcptype passive".into()],
    );
    let local_offer = test_proxy_sdp("local-ufrag", "local-password", &[]);
    let (rewritten, tunnels) = prepare_proxied_upstream_answer(&answer, &local_offer, dialer).await.unwrap();
    assert_eq!(tunnels.len(), 1);
    assert_eq!(tunnels[0].target().to_string(), "20.42.0.20:443");
    assert_eq!(tunnels[0].expected_user(), "remote-ufrag:local-ufrag");
    let candidates = candidates_of(&rewritten);
    assert_eq!(candidates.len(), 1, "{candidates:?}");
    let fields: Vec<&str> = candidates[0].split_whitespace().collect();
    assert!(fields.len() >= 8 && fields[2] == "tcp" && fields[4] == "127.0.0.1" && fields[5] != "443", "{}", candidates[0]);
    assert!(candidates[0].contains("tcptype passive"));
    close_candidate_tunnels(&tunnels);
}

#[tokio::test]
async fn prepare_rejects_unsafe_targets() {
    for (name, candidate) in [
        ("private target", "1 1 tcp 1671430143 10.0.0.1 443 typ host tcptype passive"),
        ("zero network target", "1 1 tcp 1671430143 0.0.0.1 443 typ host tcptype passive"),
        ("carrier NAT target", "1 1 tcp 1671430143 100.64.0.1 443 typ host tcptype passive"),
        ("reserved target", "1 1 tcp 1671430143 203.0.113.10 443 typ host tcptype passive"),
        ("site-local IPv6 target", "1 1 tcp 1671430143 fec0::1 443 typ host tcptype passive"),
        ("wrong port", "1 1 tcp 1671430143 20.42.0.10 8443 typ host tcptype passive"),
        ("relay target", "1 1 tcp 1671430143 20.42.0.10 443 typ relay raddr 192.0.2.1 rport 5000 tcptype passive"),
        ("active target", "1 1 tcp 1671430143 20.42.0.10 443 typ host tcptype active"),
    ] {
        let (dialer, _dials) = RecordingDialer::new();
        let result = prepare_proxied_upstream_answer(
            &test_proxy_sdp("remote", "remote-password", &[candidate.to_string()]),
            &test_proxy_sdp("local", "local-password", &[]),
            dialer,
        )
        .await;
        if let Ok((_, tunnels)) = result {
            close_candidate_tunnels(&tunnels);
            panic!("{name}: expected unsafe candidate to be rejected");
        }
    }
}

#[tokio::test]
async fn prepare_limits_candidate_count() {
    let candidates: Vec<String> = (0..=MAX_UPSTREAM_ICE_CANDIDATES).map(|i| format!("{} 1 udp 2130706431 20.42.0.10 3478 typ host", i + 1)).collect();
    let (dialer, _dials) = RecordingDialer::new();
    let err = prepare_proxied_upstream_answer(
        &test_proxy_sdp("remote", "remote-password", &candidates),
        &test_proxy_sdp("local", "local-password", &[]),
        dialer,
    )
    .await
    .err()
    .expect("error");
    assert!(err.contains("candidate limit"), "{err}");
}

/// Feeds the data in chunks of at most 3 bytes (`fragmentedReader`).
struct Fragmented {
    data: Vec<u8>,
}

impl AsyncRead for Fragmented {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        let n = self.data.len().min(3).min(buf.remaining());
        let chunk: Vec<u8> = self.data.drain(..n).collect();
        buf.put_slice(&chunk);
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn read_validated_ice_binding_frame_cases() {
    let valid = build_test_ice_frame("remote:local", "remote-password", true);
    let cases: Vec<(&str, Vec<u8>, &str, &str, bool)> = vec![
        ("valid", valid.clone(), "remote:local", "remote-password", false),
        ("wrong username", valid.clone(), "local:remote", "remote-password", true),
        ("wrong password", valid.clone(), "remote:local", "local-password", true),
        ("missing fingerprint", build_test_ice_frame("remote:local", "remote-password", false), "remote:local", "remote-password", true),
        ("undersized", vec![0, 1, 0], "", "", true),
    ];
    for (name, frame, user, password, want_error) in cases {
        let result = read_validated_ice_binding_frame(&mut Fragmented { data: frame.clone() }, user, password).await;
        match (result, want_error) {
            (Ok(got), false) => assert_eq!(got, frame, "{name}"),
            (Err(_), true) => {}
            (other, _) => panic!("{name}: unexpected {other:?}"),
        }
    }
}

async fn wait_none<T>(rx: &mut mpsc::UnboundedReceiver<T>, millis: u64) -> bool {
    tokio::time::timeout(Duration::from_millis(millis), rx.recv()).await.is_err()
}

fn started_channel() -> (Arc<dyn Fn() + Send + Sync>, mpsc::UnboundedReceiver<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (Arc::new(move || {
        let _ = tx.send(());
    }), rx)
}

async fn tunnel_with(dialer: Arc<dyn ProxyDialer>) -> TcpCandidateTunnel {
    TcpCandidateTunnel::new("20.42.0.20:443".parse().unwrap(), dialer, "remote:local", "remote-password").await.unwrap()
}

#[tokio::test]
async fn tunnel_authenticates_before_fixed_target_dial() {
    let (dialer, mut dials) = RecordingDialer::new();
    let tunnel = tunnel_with(dialer).await;
    let (handler, mut started) = started_channel();
    tunnel.set_forwarding_started_handler(handler);
    let mut client = TcpStream::connect(tunnel.listener_addr()).await.unwrap();
    let frame = build_test_ice_frame("remote:local", "remote-password", true);
    client.write_all(&frame).await.unwrap();
    let dial = tokio::time::timeout(Duration::from_secs(1), dials.recv()).await.expect("proxy dial after authentication").unwrap();
    assert_eq!(dial.address, "20.42.0.20:443");
    let mut upstream = dial.connection.unwrap();
    let mut forwarded = vec![0u8; frame.len()];
    upstream.read_exact(&mut forwarded).await.unwrap();
    assert_eq!(forwarded, frame);
    tokio::time::timeout(Duration::from_secs(1), started.recv()).await.expect("forwarding start handler").unwrap();
    upstream.write_all(b"reply").await.unwrap();
    let mut reply = [0u8; 5];
    client.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"reply");
    tunnel.close();
}

#[tokio::test]
async fn tunnel_close_cancels_proxy_dial() {
    let (started_tx, mut started) = mpsc::unbounded_channel();
    let canceled = Arc::new(AtomicBool::new(false));
    let tunnel = tunnel_with(Arc::new(BlockingDialer { started: started_tx, canceled: canceled.clone() })).await;
    let mut client = TcpStream::connect(tunnel.listener_addr()).await.unwrap();
    client.write_all(&build_test_ice_frame("remote:local", "remote-password", true)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), started.recv()).await.expect("proxy dial started").unwrap();
    let (handler, mut forwarding) = started_channel();
    tunnel.set_forwarding_started_handler(handler);
    tunnel.close();
    for _ in 0..50 {
        if canceled.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(canceled.load(Ordering::SeqCst), "tunnel close did not cancel proxy dial");
    assert!(wait_none(&mut forwarding, 100).await);
}

#[tokio::test]
async fn tunnel_proxy_failure_does_not_fall_back() {
    let (dialer, mut dials) = RecordingDialer::failing("proxy blocked");
    let tunnel = tunnel_with(dialer).await;
    let (handler, mut forwarding) = started_channel();
    tunnel.set_forwarding_started_handler(handler);
    let addr = tunnel.listener_addr();
    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(&build_test_ice_frame("remote:local", "remote-password", true)).await.unwrap();
    let dial = tokio::time::timeout(Duration::from_secs(1), dials.recv()).await.expect("proxy dial attempted").unwrap();
    assert_eq!(dial.address, "20.42.0.20:443");
    assert!(dial.connection.is_none());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(TcpStream::connect(addr).await.is_err(), "candidate listener remained available after proxy failure");
    assert!(wait_none(&mut forwarding, 100).await);
    tunnel.close();
}

#[tokio::test]
async fn tunnel_write_failure_does_not_log_forwarding_start() {
    let tunnel = tunnel_with(Arc::new(ClosedUpstreamDialer)).await;
    let (handler, mut forwarding) = started_channel();
    tunnel.set_forwarding_started_handler(handler);
    let mut client = TcpStream::connect(tunnel.listener_addr()).await.unwrap();
    client.write_all(&build_test_ice_frame("remote:local", "remote-password", true)).await.unwrap();
    drop(client);
    assert!(wait_none(&mut forwarding, 100).await);
    tunnel.close();
}

#[tokio::test]
async fn tunnel_rejects_unauthenticated_connection_without_dial() {
    let (dialer, mut dials) = RecordingDialer::new();
    let tunnel = tunnel_with(dialer).await;
    let (handler, mut forwarding) = started_channel();
    tunnel.set_forwarding_started_handler(handler);
    let mut client = TcpStream::connect(tunnel.listener_addr()).await.unwrap();
    client.write_all(&build_test_ice_frame("attacker:local", "remote-password", true)).await.unwrap();
    drop(client);
    assert!(wait_none(&mut dials, 100).await, "unauthenticated connection triggered a proxy dial");
    assert!(wait_none(&mut forwarding, 100).await);
    tunnel.close();
}

#[test]
fn bundled_credentials_reject_mixed() {
    let mixed = test_proxy_sdp("first", "first-password", &[]).replacen(
        "a=mid:1\r\na=ice-ufrag:first\r\na=ice-pwd:first-password",
        "a=mid:1\r\na=ice-ufrag:second\r\na=ice-pwd:second-password",
        1,
    );
    assert!(bundled_ice_credentials(&parse_sdp(&mixed).unwrap()).is_err());
}

#[test]
fn proxy_target_ranges() {
    for (ip, want) in [("20.42.0.20", true), ("8.8.8.8", true), ("2606:4700::1", true), ("10.1.1.1", false), ("100.64.0.1", false), ("fec0::1", false)] {
        assert_eq!(is_public_proxy_target(&ip.parse().unwrap()), want, "{ip}");
    }
}
