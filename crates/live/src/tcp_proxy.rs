//! TCP candidate tunnels for proxied upstream media (Go: tcp_proxy.go).
//!
//! When the credential uses a proxy, the upstream leg may only talk ICE-TCP to a fixed public
//! `:443` passive candidate. The answer is rewritten so that candidate points at a loopback
//! listener; the listener forwards a connection only after its first STUN frame authenticates
//! (username and message integrity) and then dials the fixed target through the proxy.

use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use rtc::ice::candidate::unmarshal_candidate;
use rtc::ice::network_type::NetworkType;
use rtc::ice::tcp_type::TcpType;
use rtc::sdp::description::session::SessionDescription;
use sha1::Sha1;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};

use crate::ws_client::BoxIo;

pub const MAX_UPSTREAM_ICE_CANDIDATES: usize = 64;
pub const MAX_PROXIED_TCP_CANDIDATES: usize = 16;
const MAX_UNAUTHENTICATED_TCP_CONNS: usize = 4;
const MAX_INITIAL_STUN_FRAME_SIZE: usize = 4096;
const STUN_MESSAGE_HEADER_SIZE: usize = 20;

/// Dials the fixed upstream target through the credential's proxy (Go: `proxy.ContextDialer`).
/// Dropping the returned future cancels the dial.
#[async_trait]
pub trait ProxyDialer: Send + Sync {
    async fn dial(&self, address: &str) -> std::io::Result<BoxIo>;
}

/// Real proxy: HTTP `CONNECT` or SOCKS5 through the configured URL.
pub struct ConfiguredProxyDialer {
    url: url::Url,
}

impl ConfiguredProxyDialer {
    pub fn new(url: url::Url) -> Self {
        ConfiguredProxyDialer { url }
    }
}

#[async_trait]
impl ProxyDialer for ConfiguredProxyDialer {
    async fn dial(&self, address: &str) -> std::io::Result<BoxIo> {
        let addr: SocketAddr = address.parse().map_err(|e| std::io::Error::other(format!("invalid target {address}: {e}")))?;
        let host = addr.ip().to_string();
        match self.url.scheme() {
            "socks5" | "socks5h" => crate::ws_client::socks5_connect(&self.url, &host, addr.port()).await,
            _ => crate::ws_client::http_connect(&self.url, &host, addr.port()).await,
        }
    }
}

/// Non-routable ranges refused as proxy targets (`nonRoutableProxyTargetPrefixes`).
const NON_ROUTABLE: &[(&str, u8)] = &[
    ("0.0.0.0", 8),
    ("10.0.0.0", 8),
    ("100.64.0.0", 10),
    ("127.0.0.0", 8),
    ("169.254.0.0", 16),
    ("172.16.0.0", 12),
    ("192.0.0.0", 24),
    ("192.0.2.0", 24),
    ("192.88.99.0", 24),
    ("192.168.0.0", 16),
    ("198.18.0.0", 15),
    ("198.51.100.0", 24),
    ("203.0.113.0", 24),
    ("224.0.0.0", 4),
    ("240.0.0.0", 4),
    ("::", 96),
    ("::ffff:0:0:0", 96),
    ("64:ff9b::", 96),
    ("64:ff9b:1::", 48),
    ("100::", 64),
    ("2001::", 23),
    ("2001:db8::", 32),
    ("2002::", 16),
    ("3fff::", 20),
    ("5f00::", 16),
    ("fc00::", 7),
    ("fe80::", 10),
    ("fec0::", 10),
    ("ff00::", 8),
];

fn prefix_contains(prefix: &str, bits: u8, ip: &IpAddr) -> bool {
    let Ok(base) = prefix.parse::<IpAddr>() else { return false };
    match (base, ip) {
        (IpAddr::V4(b), IpAddr::V4(a)) => mask_eq(&b.octets(), &a.octets(), bits),
        (IpAddr::V6(b), IpAddr::V6(a)) => mask_eq(&b.octets(), &a.octets(), bits),
        _ => false,
    }
}

fn mask_eq(a: &[u8], b: &[u8], bits: u8) -> bool {
    let full = (bits / 8) as usize;
    if a[..full] != b[..full] {
        return false;
    }
    let rem = bits % 8;
    rem == 0 || (a[full] ^ b[full]) >> (8 - rem) == 0
}

/// `isPublicProxyTarget`.
pub fn is_public_proxy_target(address: &IpAddr) -> bool {
    let unspecified = address.is_unspecified();
    let loopback = address.is_loopback();
    let multicast = address.is_multicast();
    let (private, link_local_unicast, broadcast) = match address {
        IpAddr::V4(a) => (a.is_private(), a.is_link_local(), a.is_broadcast()),
        IpAddr::V6(a) => ((a.segments()[0] & 0xfe00) == 0xfc00, (a.segments()[0] & 0xffc0) == 0xfe80, false),
    };
    if unspecified || loopback || multicast || private || link_local_unicast || broadcast {
        return false;
    }
    !NON_ROUTABLE.iter().any(|(prefix, bits)| prefix_contains(prefix, *bits, address))
}

#[derive(Clone, PartialEq, Eq)]
pub struct IceCredentials {
    pub ufrag: String,
    pub password: String,
}

/// `bundledICECredentials`: the single ICE credential pair shared by all media sections.
pub fn bundled_ice_credentials(description: &SessionDescription) -> Result<IceCredentials, String> {
    let session_ufrag = description.attribute("ice-ufrag").cloned().unwrap_or_default();
    let session_password = description.attribute("ice-pwd").cloned().unwrap_or_default();
    let mut selected: Option<IceCredentials> = None;
    for media in &description.media_descriptions {
        let ufrag = media.attribute("ice-ufrag").map(|v| v.unwrap_or("").to_string()).unwrap_or_else(|| session_ufrag.clone());
        let password = media.attribute("ice-pwd").map(|v| v.unwrap_or("").to_string()).unwrap_or_else(|| session_password.clone());
        let (ufrag, password) = (ufrag.trim().to_string(), password.trim().to_string());
        if ufrag.is_empty() && password.is_empty() {
            continue;
        }
        if ufrag.is_empty() || password.is_empty() {
            return Err("SDP contains incomplete ICE credentials".into());
        }
        let current = IceCredentials { ufrag, password };
        match &selected {
            None => selected = Some(current),
            Some(s) if *s != current => return Err("SDP contains inconsistent bundled ICE credentials".into()),
            Some(_) => {}
        }
    }
    let selected = selected.unwrap_or(IceCredentials { ufrag: session_ufrag.trim().to_string(), password: session_password.trim().to_string() });
    if selected.ufrag.is_empty() || selected.password.is_empty() {
        return Err("SDP is missing ICE credentials".into());
    }
    Ok(selected)
}

/// Parses SDP text.
pub fn parse_sdp(text: &str) -> Result<SessionDescription, String> {
    SessionDescription::unmarshal(&mut Cursor::new(text.as_bytes())).map_err(|e| e.to_string())
}

struct CandidatePlan {
    media_index: usize,
    attribute_index: usize,
    fields: Vec<String>,
    target: SocketAddr,
}

/// `proxiedTCPCandidatePlan`: `Ok(None)` drops the candidate, an error rejects the answer.
fn proxied_tcp_candidate_plan(raw: &str) -> Result<Option<CandidatePlan>, String> {
    let trimmed = raw.trim();
    let candidate = unmarshal_candidate(trimmed).map_err(|e| format!("parse upstream WebRTC candidate: {e}"))?;
    if !matches!(candidate.network_type(), NetworkType::Tcp4 | NetworkType::Tcp6) {
        return Ok(None);
    }
    if candidate.tcp_type() != TcpType::Passive {
        return Ok(None);
    }
    if candidate.component() != 1 || candidate.candidate_type() != rtc::ice::candidate::CandidateType::Host {
        return Ok(None);
    }
    if candidate.port() != 443 {
        return Err(format!("upstream WebRTC TCP proxy candidate uses disallowed port {}", candidate.port()));
    }
    let Ok(mut address) = candidate.address().parse::<IpAddr>() else {
        return Err("upstream WebRTC TCP proxy candidate address must be an IP".into());
    };
    if let IpAddr::V6(v6) = address
        && let Some(v4) = v6.to_ipv4_mapped()
    {
        address = IpAddr::V4(v4);
    }
    if !is_public_proxy_target(&address) {
        return Err("upstream WebRTC TCP proxy candidate address must be globally routable".into());
    }
    let fields: Vec<String> = trimmed.split_whitespace().map(str::to_string).collect();
    if fields.len() < 8 {
        return Err("upstream WebRTC TCP proxy candidate is malformed".into());
    }
    Ok(Some(CandidatePlan { media_index: 0, attribute_index: 0, fields, target: SocketAddr::new(address, 443) }))
}

/// `prepareProxiedUpstreamAnswer`: keeps only the allowed passive TCP candidates, each pointing
/// at a fresh loopback tunnel. Returns the rewritten SDP and the tunnels.
pub async fn prepare_proxied_upstream_answer(
    answer: &str,
    local_offer: &str,
    dialer: Arc<dyn ProxyDialer>,
) -> Result<(String, Vec<TcpCandidateTunnel>), String> {
    let mut remote = parse_sdp(answer).map_err(|e| format!("parse upstream WebRTC answer for TCP proxy: {e}"))?;
    let local = parse_sdp(local_offer).map_err(|e| format!("parse upstream WebRTC offer for TCP proxy: {e}"))?;
    let remote_credentials = bundled_ice_credentials(&remote).map_err(|e| format!("read upstream WebRTC answer ICE credentials: {e}"))?;
    let local_credentials = bundled_ice_credentials(&local).map_err(|e| format!("read upstream WebRTC offer ICE credentials: {e}"))?;

    let mut plans: Vec<CandidatePlan> = Vec::new();
    let mut candidate_count = 0usize;
    for (media_index, media) in remote.media_descriptions.iter_mut().enumerate() {
        let mut filtered = Vec::with_capacity(media.attributes.len());
        for attribute in std::mem::take(&mut media.attributes) {
            if !attribute.is_ice_candidate() {
                filtered.push(attribute);
                continue;
            }
            candidate_count += 1;
            if candidate_count > MAX_UPSTREAM_ICE_CANDIDATES {
                return Err(format!("upstream WebRTC answer exceeds the {MAX_UPSTREAM_ICE_CANDIDATES} candidate limit"));
            }
            let Some(mut plan) = proxied_tcp_candidate_plan(attribute.value.as_deref().unwrap_or(""))? else {
                continue;
            };
            if plans.len() >= MAX_PROXIED_TCP_CANDIDATES {
                return Err(format!("upstream WebRTC answer exceeds the {MAX_PROXIED_TCP_CANDIDATES} TCP candidate proxy limit"));
            }
            plan.media_index = media_index;
            plan.attribute_index = filtered.len();
            filtered.push(attribute);
            plans.push(plan);
        }
        media.attributes = filtered;
    }
    if plans.is_empty() {
        return Err("upstream WebRTC answer has no supported public TCP passive candidate on port 443".into());
    }

    let expected_user = format!("{}:{}", remote_credentials.ufrag, local_credentials.ufrag);
    let mut tunnels: Vec<TcpCandidateTunnel> = Vec::with_capacity(plans.len());
    for plan in &plans {
        let tunnel = match TcpCandidateTunnel::new(plan.target, dialer.clone(), &expected_user, &remote_credentials.password).await {
            Ok(t) => t,
            Err(e) => {
                tunnels.iter().for_each(|t| t.close());
                return Err(e);
            }
        };
        let listener = tunnel.listener_addr();
        let mut fields = plan.fields.clone();
        fields[4] = listener.ip().to_string();
        fields[5] = listener.port().to_string();
        remote.media_descriptions[plan.media_index].attributes[plan.attribute_index].value = Some(fields.join(" "));
        tunnels.push(tunnel);
    }
    Ok((remote.marshal(), tunnels))
}

/// Closes every tunnel.
pub fn close_candidate_tunnels(tunnels: &[TcpCandidateTunnel]) {
    tunnels.iter().for_each(|t| t.close());
}

type ForwardingHandler = Arc<dyn Fn() + Send + Sync>;

struct TunnelShared {
    target: SocketAddr,
    dialer: Arc<dyn ProxyDialer>,
    expected_user: String,
    remote_password: String,
    state: Mutex<TunnelState>,
    /// Closes the tunnel: listener, connections and a pending proxy dial.
    cancel: watch::Sender<bool>,
    /// Stops accepting (set once a connection has been claimed).
    stop_accept: watch::Sender<bool>,
    validation_slots: Semaphore,
}

#[derive(Default)]
struct TunnelState {
    closed: bool,
    claimed: bool,
    on_forwarding_started: Option<ForwardingHandler>,
}

/// One loopback listener forwarding an authenticated ICE-TCP connection to the fixed target.
#[derive(Clone)]
pub struct TcpCandidateTunnel {
    shared: Arc<TunnelShared>,
    listener_addr: SocketAddr,
}

impl TcpCandidateTunnel {
    /// `newTCPCandidateTunnel`.
    pub async fn new(target: SocketAddr, dialer: Arc<dyn ProxyDialer>, expected_user: &str, remote_password: &str) -> Result<Self, String> {
        if !is_public_proxy_target(&target.ip()) || target.port() != 443 {
            return Err("Codex live TCP proxy target is not allowed".into());
        }
        if expected_user.trim().is_empty() || remote_password.trim().is_empty() {
            return Err("Codex live TCP proxy tunnel configuration is incomplete".into());
        }
        let bind: SocketAddr = if target.is_ipv6() { SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 0) } else { SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0) };
        let listener = TcpListener::bind(bind).await.map_err(|e| format!("listen for Codex live TCP proxy candidate: {e}"))?;
        let listener_addr = listener.local_addr().map_err(|e| format!("Codex live TCP proxy listener returned an invalid address: {e}"))?;
        let (cancel, _) = watch::channel(false);
        let (stop_accept, _) = watch::channel(false);
        let shared = Arc::new(TunnelShared {
            target,
            dialer,
            expected_user: expected_user.to_string(),
            remote_password: remote_password.to_string(),
            state: Mutex::new(TunnelState::default()),
            cancel,
            stop_accept,
            validation_slots: Semaphore::new(MAX_UNAUTHENTICATED_TCP_CONNS),
        });
        tokio::spawn(accept_loop(shared.clone(), listener));
        Ok(TcpCandidateTunnel { shared, listener_addr })
    }

    pub fn listener_addr(&self) -> SocketAddr {
        self.listener_addr
    }

    pub fn target(&self) -> SocketAddr {
        self.shared.target
    }

    pub fn expected_user(&self) -> &str {
        &self.shared.expected_user
    }

    /// `setForwardingStartedHandler`.
    pub fn set_forwarding_started_handler(&self, handler: ForwardingHandler) {
        self.shared.state.lock().on_forwarding_started = Some(handler);
    }

    /// `Close`: stops the listener, drops connections and cancels a pending proxy dial.
    pub fn close(&self) {
        {
            let mut state = self.shared.state.lock();
            if state.closed {
                return;
            }
            state.closed = true;
        }
        let _ = self.shared.cancel.send(true);
        let _ = self.shared.stop_accept.send(true);
    }
}

impl TunnelShared {
    fn claim(&self) -> bool {
        let mut state = self.state.lock();
        if state.closed || state.claimed {
            return false;
        }
        state.claimed = true;
        true
    }

    fn notify_forwarding_started(&self) {
        let handler = self.state.lock().on_forwarding_started.clone();
        if let Some(handler) = handler {
            handler();
        }
    }
}

async fn wait_flag(mut rx: watch::Receiver<bool>) {
    while !*rx.borrow() {
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn accept_loop(shared: Arc<TunnelShared>, listener: TcpListener) {
    let stop = shared.stop_accept.subscribe();
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("codex live TCP proxy: accept candidate connection failed: {e}");
                    return;
                }
            },
            _ = wait_flag(stop.clone()) => return,
        };
        if shared.state.lock().closed {
            return;
        }
        match shared.validation_slots.try_acquire() {
            Ok(permit) => {
                // The slot is held while the first frame is validated (unauthenticated phase).
                permit.forget();
                let shared = shared.clone();
                tokio::spawn(async move {
                    handle_connection(shared.clone(), stream).await;
                });
            }
            Err(_) => {
                drop(stream);
                tracing::warn!("codex live TCP proxy: rejected excess unauthenticated candidate connection");
            }
        }
    }
}

async fn handle_connection(shared: Arc<TunnelShared>, mut client: TcpStream) {
    let cancel = shared.cancel.subscribe();
    let validated = tokio::select! {
        v = read_validated_ice_binding_frame(&mut client, &shared.expected_user, &shared.remote_password) => v,
        _ = wait_flag(cancel.clone()) => {
            shared.validation_slots.add_permits(1);
            return;
        }
    };
    shared.validation_slots.add_permits(1);
    let first_frame = match validated {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("codex live TCP proxy: rejected unauthenticated candidate connection: {e}");
            return;
        }
    };
    if !shared.claim() {
        return;
    }
    // The first authenticated connection wins; the listener is closed right away.
    let _ = shared.stop_accept.send(true);
    let target = shared.target.to_string();
    let mut upstream = tokio::select! {
        r = shared.dialer.dial(&target) => match r {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("codex live TCP proxy: connect fixed upstream candidate failed: {e}");
                return;
            }
        },
        _ = wait_flag(cancel.clone()) => return,
    };
    if shared.state.lock().closed {
        return;
    }
    if let Err(e) = upstream.write_all(&first_frame).await {
        tracing::warn!("codex live TCP proxy: forward authenticated ICE frame failed: {e}");
        return;
    }
    if upstream.flush().await.is_err() {
        return;
    }
    shared.notify_forwarding_started();
    tokio::select! {
        _ = tokio::io::copy_bidirectional(&mut client, &mut upstream) => {}
        _ = wait_flag(cancel) => {}
    }
}

/// `readValidatedICEBindingFrame`: the first RFC 4571 framed STUN message must be a Binding
/// request with the expected username, valid message integrity and a valid fingerprint.
pub async fn read_validated_ice_binding_frame<R: AsyncRead + Unpin>(
    connection: &mut R,
    expected_user: &str,
    remote_password: &str,
) -> Result<Vec<u8>, String> {
    let mut header = [0u8; 2];
    connection.read_exact(&mut header).await.map_err(|e| format!("read ICE-TCP frame header: {e}"))?;
    let frame_size = u16::from_be_bytes(header) as usize;
    if !(STUN_MESSAGE_HEADER_SIZE..=MAX_INITIAL_STUN_FRAME_SIZE).contains(&frame_size) {
        return Err(format!("invalid initial ICE-TCP STUN frame size {frame_size}"));
    }
    let mut payload = vec![0u8; frame_size];
    connection.read_exact(&mut payload).await.map_err(|e| format!("read ICE-TCP STUN frame: {e}"))?;
    validate_stun_binding_request(&payload, expected_user, remote_password)?;
    let mut frame = Vec::with_capacity(2 + payload.len());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&payload);
    Ok(frame)
}

const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_FINGERPRINT: u16 = 0x8028;
const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;

/// Strict decode plus the checks of the Go validator.
fn validate_stun_binding_request(payload: &[u8], expected_user: &str, password: &str) -> Result<(), String> {
    if payload.len() < STUN_MESSAGE_HEADER_SIZE {
        return Err("decode initial ICE-TCP STUN message: unexpected EOF".into());
    }
    let message_type = u16::from_be_bytes([payload[0], payload[1]]);
    let length = u16::from_be_bytes([payload[2], payload[3]]) as usize;
    let cookie = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
    if cookie != MAGIC_COOKIE {
        return Err("decode initial ICE-TCP STUN message: incorrect magic cookie".into());
    }
    if length % 4 != 0 || payload.len() < STUN_MESSAGE_HEADER_SIZE + length {
        return Err("decode initial ICE-TCP STUN message: bad message length".into());
    }
    if payload.len() != STUN_MESSAGE_HEADER_SIZE + length {
        return Err("initial ICE-TCP STUN message contains trailing data".into());
    }
    // Attributes: (type, value offset, value length, attribute start offset).
    let mut attributes: Vec<(u16, usize, usize, usize)> = Vec::new();
    let mut offset = STUN_MESSAGE_HEADER_SIZE;
    while offset < payload.len() {
        if payload.len() - offset < 4 {
            return Err("decode initial ICE-TCP STUN message: attribute header truncated".into());
        }
        let attr_type = u16::from_be_bytes([payload[offset], payload[offset + 1]]);
        let attr_len = u16::from_be_bytes([payload[offset + 2], payload[offset + 3]]) as usize;
        let padded = attr_len.div_ceil(4) * 4;
        if offset + 4 + padded > payload.len() {
            return Err("decode initial ICE-TCP STUN message: attribute value truncated".into());
        }
        attributes.push((attr_type, offset + 4, attr_len, offset));
        offset += 4 + padded;
    }
    if message_type != BINDING_REQUEST {
        return Err(format!("initial ICE-TCP STUN message has unexpected type {message_type:#06x}"));
    }
    let find = |t: u16| attributes.iter().find(|a| a.0 == t).copied();
    let Some((_, value_offset, value_len, _)) = find(ATTR_USERNAME) else {
        return Err("read initial ICE-TCP STUN username: attribute not found".into());
    };
    if &payload[value_offset..value_offset + value_len] != expected_user.as_bytes() {
        return Err("initial ICE-TCP STUN username does not match the media session".into());
    }
    let Some((_, integrity_value, integrity_len, integrity_start)) = find(ATTR_MESSAGE_INTEGRITY) else {
        return Err("verify initial ICE-TCP STUN integrity: attribute not found".into());
    };
    if integrity_len != 20 {
        return Err("verify initial ICE-TCP STUN integrity: bad attribute size".into());
    }
    // The integrity covers the message up to the attribute, with the length field rewritten to
    // end right after the integrity attribute.
    let mut covered = payload[..integrity_start].to_vec();
    let adjusted = (integrity_start + 4 + 20 - STUN_MESSAGE_HEADER_SIZE) as u16;
    covered[2..4].copy_from_slice(&adjusted.to_be_bytes());
    let mut mac = Hmac::<Sha1>::new_from_slice(password.as_bytes()).map_err(|e| format!("verify initial ICE-TCP STUN integrity: {e}"))?;
    mac.update(&covered);
    if mac.verify_slice(&payload[integrity_value..integrity_value + 20]).is_err() {
        return Err("verify initial ICE-TCP STUN integrity: integrity check failed".into());
    }
    let Some((_, fingerprint_value, fingerprint_len, fingerprint_start)) = find(ATTR_FINGERPRINT) else {
        return Err("verify initial ICE-TCP STUN fingerprint: attribute not found".into());
    };
    if fingerprint_len != 4 {
        return Err("verify initial ICE-TCP STUN fingerprint: bad attribute size".into());
    }
    let mut covered = payload[..fingerprint_start].to_vec();
    let adjusted = (fingerprint_start + 8 - STUN_MESSAGE_HEADER_SIZE) as u16;
    covered[2..4].copy_from_slice(&adjusted.to_be_bytes());
    let expected = crc32fast::hash(&covered) ^ 0x5354_554e;
    let actual = u32::from_be_bytes([
        payload[fingerprint_value],
        payload[fingerprint_value + 1],
        payload[fingerprint_value + 2],
        payload[fingerprint_value + 3],
    ]);
    if expected != actual {
        return Err("verify initial ICE-TCP STUN fingerprint: fingerprint check failed".into());
    }
    Ok(())
}

/// `proxyScheme`: lowercase scheme of a proxy URL, or `proxy`.
pub fn proxy_scheme(raw: &str) -> String {
    let trimmed = raw.trim();
    match trimmed.find("://") {
        Some(i) if i > 0 => trimmed[..i].to_lowercase(),
        _ => "proxy".into(),
    }
}

#[allow(dead_code)]
async fn _write_all<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> std::io::Result<()> {
    w.write_all(data).await
}
