//! In-process WebRTC media relay (Go: media.go): the client leg is terminated locally and a
//! second leg to the upstream carries the Opus audio track and the `oai-events` data channel.
//!
//! Built on webrtc-rs. Differences from the Go/pion stack, all inherent to the library:
//! UDP sockets are bound explicitly (the port range picks one free port per session for all
//! interfaces, IPv4 only) and the remote IP filter is applied to the SDP candidates before they
//! are handed to the peer connection.
//!
//! Known upstream leak: webrtc 0.21.0 keeps a reference cycle per peer connection
//! (`PeerConnectionRef.rtp_transceivers` -> `RtpTransceiverImpl` -> `Arc<PeerConnectionRef>`,
//! peer_connection/mod.rs) that survives `close()`, so a small fixed amount of memory per closed
//! peer connection is never reclaimed. Everything this module owns (session, forward targets,
//! close handler, limiter slot) is released on close and does not add to it.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use bytes::BytesMut;
use cpa_auth::http::{ProxySetting, parse_proxy};
use cpa_config::CodexLiveMediaRelayConfig;
use parking_lot::Mutex;
use rtc::ice::network_type::NetworkType;
use rtc::media_stream::MediaStreamTrack;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use tokio::sync::{mpsc, watch};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::Track;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::rtp_transceiver::RtpSender;
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceCandidateType,
    RTCIceGatheringState, RTCIceServer, RTCPeerConnectionState, RTCSessionDescription, Registry, SettingEngineBuilder,
    register_default_interceptors,
};

use crate::media::{MediaError, MediaLimiter, MediaRelayFactory, MediaRelaySession, MediaRoute};
use crate::tcp_proxy::{
    ConfiguredProxyDialer, ProxyDialer, TcpCandidateTunnel, close_candidate_tunnels, prepare_proxied_upstream_answer, proxy_scheme,
};

pub const REALTIME_DATA_CHANNEL_LABEL: &str = "oai-events";
const MEDIA_DATA_QUEUE_SIZE: usize = 64;
const MEDIA_DATA_MESSAGE_MAX_SIZE: usize = 256 << 10;
const MEDIA_DATA_BUFFERED_MAX_SIZE: usize = 1 << 20;
const OPUS_PAYLOAD_TYPE: u8 = 111;
const RTCP_REBIND_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const MIME_TYPE_OPUS: &str = "audio/opus";

fn opus_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

/// `isPublicRemoteIP`.
pub fn is_public_remote_ip(ip: &IpAddr) -> bool {
    // Unwrap IPv4-mapped IPv6 (::ffff:127.0.0.1) so it is judged as the IPv4 address.
    let ip = &ip.to_canonical();
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    match ip {
        IpAddr::V4(a) => !(a.is_private() || a.is_link_local()),
        IpAddr::V6(a) => {
            let first = a.segments()[0];
            !((first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80)
        }
    }
}

/// Removes `a=candidate:` lines that fail the remote IP filter (hostnames such as mDNS names
/// cannot be vetted and are dropped too).
fn filter_remote_candidates(sdp: &str) -> String {
    let mut out = String::with_capacity(sdp.len());
    for line in sdp.split_inclusive('\n') {
        if let Some(rest) = line.trim_end().strip_prefix("a=candidate:") {
            let allowed = rest.split_whitespace().nth(4).and_then(|a| a.parse::<IpAddr>().ok()).is_some_and(|ip| is_public_remote_ip(&ip));
            if !allowed {
                continue;
            }
        }
        out.push_str(line);
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PeerKind {
    /// Client leg: optional private remote IP filtering.
    Downstream { filter_private: bool },
    /// Direct upstream leg.
    Upstream,
    /// Upstream leg through a proxy: loopback candidates only, ICE-TCP.
    ProxyUpstream,
}

/// Picks one free UDP port in `[min, max]` (0 when no range is configured, i.e. any port).
/// Fails when the whole configured range is in use.
fn pick_udp_port(cfg: &CodexLiveMediaRelayConfig) -> Result<u16, String> {
    if cfg.udp_port_min == 0 {
        return Ok(0);
    }
    let (min, max) = (cfg.udp_port_min as u32, cfg.udp_port_max as u32);
    let span = max - min + 1;
    let start = rand::random::<u32>() % span;
    for i in 0..span {
        let port = (min + (start + i) % span) as u16;
        if std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok() {
            return Ok(port);
        }
    }
    Err(format!("no free UDP port in range {min}-{max}"))
}

pub struct PionMediaRelay {
    config: CodexLiveMediaRelayConfig,
    ice_servers: Vec<RTCIceServer>,
    limiter: Arc<MediaLimiter>,
}

impl PionMediaRelay {
    /// `newPionMediaRelayWithLimiter`: validates the config and shares the session limiter.
    pub fn new(config: &CodexLiveMediaRelayConfig, limiter: Arc<MediaLimiter>) -> Result<Self, String> {
        config.validate().map_err(|e| e.to_string())?;
        let ice_servers = config
            .ice_servers
            .iter()
            .map(|s| RTCIceServer {
                urls: s.urls.iter().map(|u| u.trim().to_string()).collect(),
                username: s.username.clone(),
                credential: s.credential.clone(),
            })
            .collect();
        limiter.set_limit(config.effective_max_sessions());
        Ok(PionMediaRelay { config: config.clone(), ice_servers, limiter })
    }

    /// Builds one peer connection of the given kind.
    async fn build_peer(
        &self,
        kind: PeerKind,
        ice_servers: Vec<RTCIceServer>,
        handler: Arc<dyn PeerConnectionEventHandler>,
    ) -> Result<Arc<dyn PeerConnection>, String> {
        let mut media = MediaEngine::default();
        media
            .register_codec(RTCRtpCodecParameters { rtp_codec: opus_codec(), payload_type: OPUS_PAYLOAD_TYPE }, RtpCodecKind::Audio)
            .map_err(|e| format!("register Opus codec: {e}"))?;
        let registry = register_default_interceptors(Registry::new(), &mut media).map_err(|e| format!("register WebRTC interceptors: {e}"))?;
        let mut settings = SettingEngineBuilder::new();
        let mut builder = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().with_ice_servers(ice_servers).build())
            .with_data_channel_send_buffer_limit(MEDIA_DATA_BUFFERED_MAX_SIZE)
            .with_handler(handler);
        if kind == PeerKind::ProxyUpstream {
            settings = settings
                .with_network_types(vec![NetworkType::Udp4, NetworkType::Tcp4])
                .with_include_loopback_candidate(true);
            builder = builder.with_udp_addrs(vec!["127.0.0.1:0".to_string()]).with_tcp_addrs(vec!["127.0.0.1:0".to_string()]);
        } else {
            let public_ip = self.config.public_ip.trim();
            if !public_ip.is_empty() {
                settings = settings.with_nat_1to1_ips(vec![public_ip.to_string()], RTCIceCandidateType::Host);
            }
            let port = pick_udp_port(&self.config)?;
            builder = builder.with_udp_addrs(vec![format!("0.0.0.0:{port}")]);
        }
        let pc = builder
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(settings.build())
            .build()
            .await
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(pc))
    }
}

/// Which side a log line or handler belongs to (`local` = client leg, `remote` = upstream leg).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Peer {
    Local,
    Remote,
}

impl Peer {
    fn name(self) -> &'static str {
        match self {
            Peer::Local => "local",
            Peer::Remote => "remote",
        }
    }

    fn reason_prefix(self) -> &'static str {
        match self {
            Peer::Local => "downstream",
            Peer::Remote => "upstream",
        }
    }
}

/// Output track of a leg plus its sender (for the negotiated payload type).
#[derive(Clone)]
struct ForwardTarget {
    track: Arc<TrackLocalStaticRTP>,
    sender: Arc<dyn RtpSender>,
}

/// Event handler of one peer connection; the session is attached after it is built.
struct PeerHandler {
    peer: Peer,
    session: OnceLock<Weak<SessionInner>>,
    gathered: watch::Sender<bool>,
    /// Track the packets of this peer's remote audio are written to.
    forward_to: watch::Receiver<Option<ForwardTarget>>,
}

impl PeerHandler {
    fn session(&self) -> Option<Arc<SessionInner>> {
        self.session.get().and_then(Weak::upgrade)
    }
}

#[async_trait]
impl PeerConnectionEventHandler for PeerHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.send(true);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let Some(session) = self.session() else { return };
        session.on_state(self.peer, state);
    }

    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        let Some(session) = self.session() else { return };
        if self.peer != Peer::Local {
            return;
        }
        if channel.label().await.unwrap_or_default() != REALTIME_DATA_CHANNEL_LABEL {
            let _ = channel.close().await;
            return;
        }
        session.bridge.attach_downstream(channel);
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let Some(session) = self.session() else { return };
        let ssrc = track.ssrcs().await.first().copied();
        let is_opus = match ssrc {
            Some(ssrc) => track.codec(ssrc).await.is_some_and(|c| c.mime_type.eq_ignore_ascii_case(MIME_TYPE_OPUS)),
            None => false,
        };
        if !is_opus {
            return;
        }
        let name = match self.peer {
            Peer::Local => "downstream-to-upstream",
            Peer::Remote => "upstream-to-downstream",
        };
        let mut forward_to = self.forward_to.clone();
        let done = session.done.subscribe();
        tokio::spawn(async move {
            relay_rtp(name, track, &mut forward_to, done).await;
        });
    }
}

/// `relayRTP`: forwards packets of a remote track to the other leg's output track.
async fn relay_rtp(
    name: &str,
    source: Arc<dyn TrackRemote>,
    destination: &mut watch::Receiver<Option<ForwardTarget>>,
    done: watch::Receiver<bool>,
) {
    let Ok(dest) = destination.wait_for(|d| d.is_some()).await.map(|d| d.clone()) else { return };
    let Some(target) = dest else { return };
    let dest = target.track.clone();
    let sender = target.sender.clone();
    // The target holds the other leg's sender; do not keep it alive for the whole relay.
    drop(target);
    let dest_ssrc = dest.ssrcs().await.first().copied();
    // The payload type the leg negotiated for Opus (the peer's offer may number it differently).
    let dest_pt = match sender.get_parameters().await {
        Ok(p) => p
            .rtp_parameters
            .codecs
            .iter()
            .find(|c| c.rtp_codec.mime_type.eq_ignore_ascii_case(MIME_TYPE_OPUS))
            .map(|c| c.payload_type)
            .unwrap_or(OPUS_PAYLOAD_TYPE),
        Err(_) => OPUS_PAYLOAD_TYPE,
    };
    drop(sender);
    loop {
        let event = source.poll().await;
        if *done.borrow() {
            return;
        }
        match event {
            Some(TrackRemoteEvent::OnRtpPacket(mut packet)) => {
                // `normalizeRTPPacket`: header extensions are not carried across legs.
                packet.header.extension = false;
                packet.header.extension_profile = 0;
                packet.header.extensions.clear();
                packet.header.extensions_padding = 0;
                if let Some(ssrc) = dest_ssrc {
                    packet.header.ssrc = ssrc;
                }
                packet.header.payload_type = dest_pt;
                if let Err(e) = dest.write_rtp(packet).await {
                    if !*done.borrow() {
                        tracing::debug!("codex live media: {name} RTP write stopped: {e}");
                    }
                    return;
                }
            }
            Some(TrackRemoteEvent::OnEnded) | None => return,
            Some(_) => {}
        }
    }
}

/// `drainRTCP`: consumes the RTCP feedback delivered for an output track so its event queue never
/// backs up. `poll` yields `None` until the track is bound by the negotiation (and again once the
/// peer connection is gone), so it is retried until the session is done.
async fn drain_rtcp(name: &'static str, track: Arc<TrackLocalStaticRTP>, mut done: watch::Receiver<bool>) {
    loop {
        let event = tokio::select! {
            e = track.poll() => e,
            _ = done.wait_for(|d| *d) => return,
        };
        if event.is_some() {
            continue;
        }
        if *done.borrow() {
            return;
        }
        tracing::trace!("codex live media: {name} RTCP not available yet");
        tokio::select! {
            _ = tokio::time::sleep(RTCP_REBIND_INTERVAL) => {}
            _ = done.wait_for(|d| *d) => return,
        }
    }
}

struct DataMessage {
    data: BytesMut,
    is_string: bool,
}

/// One direction of the data channel bridge: a bounded queue drained into the destination.
struct Pipe {
    name: &'static str,
    queue: mpsc::Sender<DataMessage>,
    destination: watch::Sender<Option<Arc<dyn DataChannel>>>,
    ready: watch::Sender<bool>,
    on_error: Arc<dyn Fn(String) + Send + Sync>,
}

impl Pipe {
    fn new(name: &'static str, done: watch::Receiver<bool>, on_error: Arc<dyn Fn(String) + Send + Sync>) -> Arc<Pipe> {
        let (queue, mut rx) = mpsc::channel::<DataMessage>(MEDIA_DATA_QUEUE_SIZE);
        let (destination, dest_rx) = watch::channel(None);
        let (ready, mut ready_rx) = watch::channel(false);
        let pipe = Arc::new(Pipe { name, queue, destination, ready, on_error });
        let runner = pipe.clone();
        tokio::spawn(async move {
            let mut done_rx = done.clone();
            tokio::select! {
                _ = ready_rx.wait_for(|r| *r) => {}
                _ = done_rx.wait_for(|d| *d) => return,
            }
            loop {
                let message = tokio::select! {
                    m = rx.recv() => match m { Some(m) => m, None => return },
                    _ = done_rx.wait_for(|d| *d) => return,
                };
                let Some(channel) = dest_rx.borrow().clone() else {
                    runner.report(format!("{} DataChannel destination unavailable", runner.name));
                    return;
                };
                let sent = tokio::select! {
                    r = async {
                        if message.is_string {
                            channel.send_text(&String::from_utf8_lossy(&message.data)).await
                        } else {
                            channel.send(message.data).await
                        }
                    } => r,
                    _ = done_rx.wait_for(|d| *d) => return,
                };
                if let Err(e) = sent {
                    runner.report(format!("send {} DataChannel message: {e}", runner.name));
                    return;
                }
            }
        });
        pipe
    }

    fn report(&self, error: String) {
        (self.on_error)(error);
    }
}

/// `dataChannelBridge`: the two channels and their pipes.
struct Bridge {
    done: watch::Receiver<bool>,
    down_to_up: Arc<Pipe>,
    up_to_down: Arc<Pipe>,
    downstream: Mutex<Option<Arc<dyn DataChannel>>>,
    upstream: Mutex<Option<Arc<dyn DataChannel>>>,
}

impl Bridge {
    fn new(done: watch::Receiver<bool>, on_error: Arc<dyn Fn(String) + Send + Sync>) -> Self {
        Bridge {
            down_to_up: Pipe::new("downstream-to-upstream", done.clone(), on_error.clone()),
            up_to_down: Pipe::new("upstream-to-downstream", done.clone(), on_error),
            done,
            downstream: Mutex::new(None),
            upstream: Mutex::new(None),
        }
    }

    fn attach_downstream(&self, channel: Arc<dyn DataChannel>) {
        {
            let mut slot = self.downstream.lock();
            if slot.is_some() {
                drop(slot);
                tokio::spawn(async move {
                    let _ = channel.close().await;
                });
                return;
            }
            *slot = Some(channel.clone());
        }
        self.bind(channel, self.down_to_up.clone(), self.up_to_down.clone());
    }

    fn attach_upstream(&self, channel: Arc<dyn DataChannel>) {
        {
            let mut slot = self.upstream.lock();
            if slot.is_some() {
                drop(slot);
                tokio::spawn(async move {
                    let _ = channel.close().await;
                });
                return;
            }
            *slot = Some(channel.clone());
        }
        self.bind(channel, self.up_to_down.clone(), self.down_to_up.clone());
    }

    /// Polls `channel`: its messages feed `incoming`'s queue, and it is `outgoing`'s destination.
    fn bind(&self, channel: Arc<dyn DataChannel>, incoming: Arc<Pipe>, outgoing: Arc<Pipe>) {
        let _ = outgoing.destination.send(Some(channel.clone()));
        let done = self.done.clone();
        tokio::spawn(async move {
            // Messages from this channel are queued for the other side's destination.
            let to_queue = incoming;
            loop {
                let Some(event) = channel.poll().await else {
                    if !*done.borrow() {
                        to_queue.report(format!("{} DataChannel closed", to_queue.name));
                    }
                    return;
                };
                match event {
                    DataChannelEvent::OnOpen => {
                        let _ = outgoing.ready.send(true);
                    }
                    DataChannelEvent::OnMessage(message) => {
                        if message.data.len() > MEDIA_DATA_MESSAGE_MAX_SIZE {
                            to_queue.report(format!("{} DataChannel message exceeds {MEDIA_DATA_MESSAGE_MAX_SIZE} bytes", to_queue.name));
                            continue;
                        }
                        let mut done_wait = done.clone();
                        tokio::select! {
                            _ = to_queue.queue.send(DataMessage { data: message.data, is_string: message.is_string }) => {}
                            _ = done_wait.wait_for(|d| *d) => return,
                        }
                    }
                    DataChannelEvent::OnError => to_queue.report(format!("{} DataChannel error", to_queue.name)),
                    DataChannelEvent::OnClose => {
                        if !*done.borrow() {
                            to_queue.report(format!("{} DataChannel closed", to_queue.name));
                        }
                        return;
                    }
                    _ => {}
                }
            }
        });
    }

    fn close(&self) {
        let channels: Vec<Arc<dyn DataChannel>> =
            [self.downstream.lock().clone(), self.upstream.lock().clone()].into_iter().flatten().collect();
        tokio::spawn(async move {
            for channel in channels {
                let _ = channel.close().await;
            }
        });
    }
}

type CloseHandler = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
struct SessionState {
    call_id: String,
    on_close: Option<CloseHandler>,
    failure_reason: String,
    handler_called: bool,
    tunnels: Vec<TcpCandidateTunnel>,
    local_offer: String,
}

/// A slot of the shared media session limiter; released at most once, on close or on drop (so a
/// `new_session` future cancelled mid-setup does not leak the slot).
struct SlotPermit {
    limiter: Arc<MediaLimiter>,
    held: AtomicBool,
}

impl SlotPermit {
    /// Takes a slot, or `None` when the limiter is at capacity.
    fn acquire(limiter: &Arc<MediaLimiter>) -> Option<SlotPermit> {
        limiter.acquire().then(|| SlotPermit { limiter: limiter.clone(), held: AtomicBool::new(true) })
    }

    fn release(&self) {
        if self.held.swap(false, Ordering::SeqCst) {
            self.limiter.release();
        }
    }
}

impl Drop for SlotPermit {
    fn drop(&mut self) {
        self.release();
    }
}

/// Closes a peer connection in the background when dropped, until disarmed (covers a cancelled
/// `new_session` before the session owns its peer connections).
struct CloseOnDrop(Option<Arc<dyn PeerConnection>>);

impl CloseOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        if let (Some(pc), Ok(handle)) = (self.0.take(), tokio::runtime::Handle::try_current()) {
            handle.spawn(async move {
                let _ = pc.close().await;
            });
        }
    }
}

struct SessionInner {
    id: String,
    downstream: Arc<dyn PeerConnection>,
    upstream: Arc<dyn PeerConnection>,
    bridge: Bridge,
    closed: AtomicBool,
    done: watch::Sender<bool>,
    failure_once: std::sync::Once,
    forwarding_once: std::sync::Once,
    state: Mutex<SessionState>,
    proxy: Option<(Arc<dyn ProxyDialer>, String)>,
    credential: String,
    auth_index: String,
    permit: SlotPermit,
    /// Senders of the forward targets, cleared on close to break the cross-leg sender cycle.
    forward_targets: [watch::Sender<Option<ForwardTarget>>; 2],
    downstream_gathered: watch::Receiver<bool>,
    upstream_gathered: watch::Receiver<bool>,
    upstream_state: Mutex<RTCPeerConnectionState>,
}

impl SessionInner {
    fn call_id(&self) -> String {
        self.state.lock().call_id.clone()
    }

    /// The `call_id` log field: absent until the call id is known (Go: `logFields`).
    fn call_id_field(&self) -> Option<String> {
        Some(self.call_id()).filter(|c| !c.is_empty())
    }

    /// Fields shared by every log line of the session.
    fn log_peer(&self, peer: Peer, state: &str, message: &str) {
        tracing::info!(media_session_id = self.id.as_str(), peer = peer.name(), call_id = self.call_id_field().as_deref(), state, "{message}");
    }

    fn on_state(&self, peer: Peer, state: RTCPeerConnectionState) {
        if peer == Peer::Remote {
            *self.upstream_state.lock() = state;
        }
        let name = format!("{state}");
        match state {
            RTCPeerConnectionState::Connecting => self.log_peer(peer, &name, "codex live WebRTC peer connecting"),
            RTCPeerConnectionState::Connected => {
                self.log_peer(peer, &name, "codex live WebRTC peer connected");
                if peer == Peer::Remote {
                    self.log_forwarding_started();
                }
            }
            RTCPeerConnectionState::Disconnected => {
                tracing::warn!(media_session_id = self.id.as_str(), peer = peer.name(), call_id = self.call_id_field().as_deref(), state = name.as_str(), "codex live WebRTC peer disconnected");
            }
            RTCPeerConnectionState::Failed => {
                tracing::warn!(media_session_id = self.id.as_str(), peer = peer.name(), call_id = self.call_id_field().as_deref(), state = name.as_str(), "codex live WebRTC peer failed");
                self.fail(&format!("{}_failed", peer.reason_prefix()), &format!("{} PeerConnection failed", peer.reason_prefix()));
            }
            RTCPeerConnectionState::Closed if !*self.done.borrow() => {
                self.log_peer(peer, &name, "codex live WebRTC peer closed by remote");
                self.fail(&format!("{}_closed", peer.reason_prefix()), &format!("{} PeerConnection closed", peer.reason_prefix()));
            }
            _ => {}
        }
    }

    fn log_forwarding_started(&self) {
        self.forwarding_once.call_once(|| {
            // Go adds call_id, auth_index and credential only when non-empty; proxy_scheme comes
            // from `logFields` for proxied remote peers.
            let (connection, transport, proxy_scheme) = match &self.proxy {
                Some((_, scheme)) => (format!("via {scheme} proxy"), "tcp", Some(scheme.as_str())),
                None => ("direct".to_string(), "ice", None),
            };
            let state = self.upstream_state.lock().to_string();
            tracing::info!(
                media_session_id = self.id.as_str(),
                peer = "remote",
                call_id = self.call_id_field().as_deref(),
                auth_index = Some(self.auth_index.as_str()).filter(|v| !v.is_empty()),
                credential = Some(self.credential.as_str()).filter(|v| !v.is_empty()),
                connection = connection.as_str(),
                remote_transport = transport,
                proxy_scheme,
                state = state.as_str(),
                "codex live remote media forwarding started"
            );
        });
    }

    /// `CloseWithReason`.
    fn close(&self, reason: &str) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.teardown(reason);
    }

    /// Releases everything the session owns. Runs once (guarded by `closed`), from `close` or `Drop`.
    /// The limiter slot is released here, synchronously, not after the peer connections finished
    /// closing.
    fn teardown(&self, reason: &str) {
        tracing::info!(media_session_id = self.id.as_str(), peer = "session", call_id = self.call_id_field().as_deref(), reason, "codex live WebRTC media session closing");
        let _ = self.done.send(true);
        self.bridge.close();
        let (tunnels, on_close) = {
            let mut state = self.state.lock();
            (std::mem::take(&mut state.tunnels), state.on_close.take())
        };
        close_candidate_tunnels(&tunnels);
        // The close handler captures the session store; dropping it ends the store <-> session cycle.
        drop(on_close);
        for target in &self.forward_targets {
            target.send_replace(None);
        }
        self.permit.release();
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let (id, call_id, reason) = (self.id.clone(), self.call_id_field(), reason.to_string());
        let peers = [(Peer::Local, self.downstream.clone()), (Peer::Remote, self.upstream.clone())];
        handle.spawn(async move {
            for (peer, pc) in peers {
                match pc.close().await {
                    Ok(()) => tracing::info!(media_session_id = id.as_str(), peer = peer.name(), call_id = call_id.as_deref(), state = "closed", "codex live WebRTC peer closed"),
                    Err(e) => tracing::warn!(media_session_id = id.as_str(), peer = peer.name(), "codex live WebRTC peer close failed: {e}"),
                }
            }
            tracing::info!(media_session_id = id.as_str(), peer = "session", call_id = call_id.as_deref(), reason = reason.as_str(), "codex live WebRTC media session closed");
        });
    }

    /// `fail`: closes the session once and reports the reason to the close handler.
    fn fail(&self, reason: &str, error: &str) {
        self.failure_once.call_once(|| {
            tracing::warn!(media_session_id = self.id.as_str(), peer = "session", reason, "codex live WebRTC media session failed: {error}");
            // The handler is taken before closing: closing drops any handler still registered.
            let handler = {
                let mut state = self.state.lock();
                state.failure_reason = reason.to_string();
                if state.handler_called {
                    None
                } else {
                    let handler = state.on_close.take();
                    state.handler_called = handler.is_some();
                    handler
                }
            };
            self.close(reason);
            if let Some(handler) = handler {
                handler(reason);
            }
        });
    }
}

impl Drop for SessionInner {
    /// A session dropped without an explicit close (cancelled setup) still releases its resources.
    fn drop(&mut self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.teardown("dropped");
        }
    }
}

/// A live relayed call.
pub struct PionMediaSession {
    inner: Arc<SessionInner>,
}

fn media_err(message: String) -> MediaError {
    MediaError::new(message)
}

async fn wait_gathered(rx: &watch::Receiver<bool>) {
    let mut rx = rx.clone();
    let _ = rx.wait_for(|g| *g).await;
}

#[async_trait]
impl MediaRelayFactory for PionMediaRelay {
    async fn new_session(&self, client_offer: &str, route: MediaRoute) -> Result<(Arc<dyn MediaRelaySession>, String), MediaError> {
        let setting = parse_proxy(&route.proxy_url).map_err(|e| media_err(format!("configure Codex live remote TCP proxy: {e}")))?;
        let proxy: Option<(Arc<dyn ProxyDialer>, String)> = match setting {
            ProxySetting::Proxy(raw) => {
                let url = url::Url::parse(&raw).map_err(|e| media_err(format!("configure Codex live remote TCP proxy: {e}")))?;
                Some((Arc::new(ConfiguredProxyDialer::new(url)), proxy_scheme(&route.proxy_url)))
            }
            _ => None,
        };
        // Held across every await below; moved into the session, or released when a cancelled
        // or failed setup drops it.
        let Some(permit) = SlotPermit::acquire(&self.limiter) else {
            return Err(media_err("Codex live media relay capacity exhausted".into()));
        };

        let (down_gather_tx, down_gather_rx) = watch::channel(false);
        let (up_gather_tx, up_gather_rx) = watch::channel(false);
        let (to_upstream_tx, to_upstream_rx) = watch::channel::<Option<ForwardTarget>>(None);
        let (to_downstream_tx, to_downstream_rx) = watch::channel::<Option<ForwardTarget>>(None);
        let down_handler = Arc::new(PeerHandler { peer: Peer::Local, session: OnceLock::new(), gathered: down_gather_tx, forward_to: to_upstream_rx });
        let up_handler = Arc::new(PeerHandler { peer: Peer::Remote, session: OnceLock::new(), gathered: up_gather_tx, forward_to: to_downstream_rx });

        let filter_private = self.config.disable_private_remote_ips;
        let downstream = match self
            .build_peer(PeerKind::Downstream { filter_private }, self.ice_servers.clone(), down_handler.clone())
            .await
        {
            Ok(pc) => pc,
            Err(e) => return Err(media_err(format!("create downstream PeerConnection: {e}"))),
        };
        let mut downstream_guard = CloseOnDrop(Some(downstream.clone()));
        let (up_kind, up_ice) = if proxy.is_some() { (PeerKind::ProxyUpstream, Vec::new()) } else { (PeerKind::Upstream, self.ice_servers.clone()) };
        let upstream = match self.build_peer(up_kind, up_ice, up_handler.clone()).await {
            Ok(pc) => pc,
            Err(e) => return Err(media_err(format!("create upstream PeerConnection: {e}"))),
        };
        let mut upstream_guard = CloseOnDrop(Some(upstream.clone()));

        let (done_tx, done_rx) = watch::channel(false);
        let inner = Arc::new_cyclic(|weak: &Weak<SessionInner>| {
            let weak = weak.clone();
            let on_error: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |error: String| {
                if let Some(session) = weak.upgrade() {
                    session.fail("data_channel_failed", &error);
                }
            });
            SessionInner {
                id: uuid::Uuid::new_v4().to_string(),
                downstream: downstream.clone(),
                upstream: upstream.clone(),
                bridge: Bridge::new(done_rx, on_error),
                closed: AtomicBool::new(false),
                done: done_tx,
                failure_once: std::sync::Once::new(),
                forwarding_once: std::sync::Once::new(),
                state: Mutex::new(SessionState::default()),
                proxy,
                credential: route.credential.trim().to_string(),
                auth_index: route.auth_index.trim().to_string(),
                permit,
                forward_targets: [to_downstream_tx.clone(), to_upstream_tx.clone()],
                downstream_gathered: down_gather_rx,
                upstream_gathered: up_gather_rx,
                upstream_state: Mutex::new(RTCPeerConnectionState::New),
            }
        });
        // From here the session closes the peer connections itself (also when dropped).
        downstream_guard.disarm();
        upstream_guard.disarm();
        let _ = down_handler.session.set(Arc::downgrade(&inner));
        let _ = up_handler.session.set(Arc::downgrade(&inner));
        tracing::info!(media_session_id = inner.id.as_str(), peer = "session", "codex live WebRTC media session created");

        let fail = |inner: &Arc<SessionInner>, message: String| {
            inner.close("closed");
            media_err(message)
        };

        let offer_text = if filter_private { filter_remote_candidates(client_offer) } else { client_offer.to_string() };
        let remote = match RTCSessionDescription::offer(offer_text) {
            Ok(d) => d,
            Err(e) => return Err(fail(&inner, format!("set downstream WebRTC offer: {e}"))),
        };
        if let Err(e) = downstream.set_remote_description(remote).await {
            return Err(fail(&inner, format!("set downstream WebRTC offer: {e}")));
        }

        let make_track = |label: &str| {
            Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
                "codex-live".to_string(),
                "audio".to_string(),
                label.to_string(),
                RtpCodecKind::Audio,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(rand::random::<u32>()), ..Default::default() },
                    codec: opus_codec(),
                    ..Default::default()
                }],
            )))
        };
        let to_desktop = make_track("codex-live-downstream");
        let desktop_sender = match downstream.add_track(to_desktop.clone() as Arc<dyn TrackLocal>).await {
            Ok(sender) => sender,
            Err(e) => return Err(fail(&inner, format!("add downstream audio track: {e}"))),
        };
        let to_openai = make_track("codex-live-upstream");
        let openai_sender = match upstream.add_track(to_openai.clone() as Arc<dyn TrackLocal>).await {
            Ok(sender) => sender,
            Err(e) => return Err(fail(&inner, format!("add upstream audio track: {e}"))),
        };
        let session_done = inner.done.subscribe();
        tokio::spawn(drain_rtcp("downstream", to_desktop.clone(), session_done.clone()));
        tokio::spawn(drain_rtcp("upstream", to_openai.clone(), session_done));
        to_downstream_tx.send_replace(Some(ForwardTarget { track: to_desktop, sender: desktop_sender }));
        to_upstream_tx.send_replace(Some(ForwardTarget { track: to_openai, sender: openai_sender }));

        match upstream.create_data_channel(REALTIME_DATA_CHANNEL_LABEL, None).await {
            Ok(channel) => inner.bridge.attach_upstream(channel),
            Err(e) => return Err(fail(&inner, format!("create upstream DataChannel: {e}"))),
        }

        let offer = match upstream.create_offer(None).await {
            Ok(o) => o,
            Err(e) => return Err(fail(&inner, format!("create upstream WebRTC offer: {e}"))),
        };
        if let Err(e) = upstream.set_local_description(offer).await {
            return Err(fail(&inner, format!("set upstream WebRTC offer: {e}")));
        }
        wait_gathered(&inner.upstream_gathered).await;
        let local = upstream.local_description().await;
        let Some(local) = local.filter(|d| !d.sdp.trim().is_empty()) else {
            return Err(fail(&inner, "upstream WebRTC offer is empty".into()));
        };
        inner.state.lock().local_offer = local.sdp.clone();
        let session: Arc<dyn MediaRelaySession> = Arc::new(PionMediaSession { inner });
        Ok((session, local.sdp))
    }
}

#[async_trait]
impl MediaRelaySession for PionMediaSession {
    async fn accept_upstream_answer(&self, upstream_answer: &str) -> Result<String, MediaError> {
        let inner = &self.inner;
        let mut answer_to_apply = upstream_answer.to_string();
        if let Some((dialer, _)) = &inner.proxy {
            let local_offer = inner.state.lock().local_offer.clone();
            let (rewritten, tunnels) = prepare_proxied_upstream_answer(upstream_answer, &local_offer, dialer.clone()).await.map_err(media_err)?;
            for tunnel in &tunnels {
                let weak = Arc::downgrade(inner);
                tunnel.set_forwarding_started_handler(Arc::new(move || {
                    if let Some(session) = weak.upgrade() {
                        session.log_forwarding_started();
                    }
                }));
            }
            {
                let mut state = inner.state.lock();
                if inner.closed.load(Ordering::SeqCst) {
                    drop(state);
                    close_candidate_tunnels(&tunnels);
                    return Err(media_err("Codex live media session closed while configuring TCP proxy".into()));
                }
                state.tunnels = tunnels;
            }
            answer_to_apply = rewritten;
        }
        let proxied_candidates = if inner.proxy.is_some() { Some(answer_to_apply.clone()) } else { None };
        let close_tunnels = || {
            let tunnels = std::mem::take(&mut inner.state.lock().tunnels);
            close_candidate_tunnels(&tunnels);
        };
        let remote = RTCSessionDescription::answer(answer_to_apply).map_err(|e| {
            close_tunnels();
            media_err(format!("set upstream WebRTC answer: {e}"))
        })?;
        if let Err(e) = inner.upstream.set_remote_description(remote).await {
            close_tunnels();
            return Err(media_err(format!("set upstream WebRTC answer: {e}")));
        }
        if let Some(sdp) = proxied_candidates
            && let Err(e) = add_remote_tcp_candidates(inner.upstream.as_ref(), &sdp).await
        {
            close_tunnels();
            return Err(media_err(format!("set upstream WebRTC answer: {e}")));
        }
        let answer = inner.downstream.create_answer(None).await.map_err(|e| media_err(format!("create downstream WebRTC answer: {e}")))?;
        inner
            .downstream
            .set_local_description(answer)
            .await
            .map_err(|e| media_err(format!("set downstream WebRTC answer: {e}")))?;
        wait_gathered(&inner.downstream_gathered).await;
        match inner.downstream.local_description().await {
            Some(d) if !d.sdp.trim().is_empty() => Ok(d.sdp),
            _ => Err(media_err("downstream WebRTC answer is empty".into())),
        }
    }

    fn set_call_id(&self, call_id: &str) {
        self.inner.state.lock().call_id = call_id.trim().to_string();
    }

    fn set_close_handler(&self, handler: Box<dyn Fn(&str) + Send + Sync>) {
        let handler: CloseHandler = Arc::from(handler);
        let pending = {
            let mut state = self.inner.state.lock();
            // A closed session never calls the handler again, so it is not kept (it would pin the
            // session store for as long as the session lives).
            if !self.inner.closed.load(Ordering::SeqCst) {
                state.on_close = Some(handler.clone());
            }
            if !state.failure_reason.is_empty() && !state.handler_called {
                state.handler_called = true;
                Some(state.failure_reason.clone())
            } else {
                None
            }
        };
        if let Some(reason) = pending {
            handler(&reason);
        }
    }

    fn close_with_reason(&self, reason: &str) {
        self.inner.close(reason);
    }
}

/// webrtc-rs only dials a remote ICE-TCP passive candidate when it is added explicitly, not
/// when it arrives inside a remote description; the rewritten proxied answer's candidates
/// (loopback tunnels) are therefore added one by one.
pub(crate) async fn add_remote_tcp_candidates(pc: &dyn PeerConnection, sdp: &str) -> Result<(), String> {
    let description = crate::tcp_proxy::parse_sdp(sdp)?;
    for (index, media) in description.media_descriptions.iter().enumerate() {
        let mid = media.attribute("mid").flatten().unwrap_or("").to_string();
        for attribute in media.attributes.iter().filter(|a| a.is_ice_candidate()) {
            let Some(value) = attribute.value.clone() else { continue };
            let init = RTCIceCandidateInit {
                candidate: format!("candidate:{value}"),
                sdp_mid: Some(mid.clone()),
                sdp_mline_index: Some(index as u16),
                ..Default::default()
            };
            // Duplicates of candidates already in the description are harmless.
            if let Err(e) = pc.add_ice_candidate(init).await {
                tracing::debug!("codex live media: add remote TCP candidate: {e}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "relay_tests.rs"]
mod tests;
