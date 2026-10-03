//! Media relay tests with local WebRTC peers (Go: media_test.go).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use cpa_config::CodexLiveMediaRelayConfig;
use rtc::media_stream::MediaStreamTrack;
use rtc::rtp;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind};
use tokio::sync::{mpsc, watch};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::Track;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceGatheringState,
    RTCSessionDescription, Registry, SettingEngineBuilder, register_default_interceptors,
};

use super::*;

pub(crate) struct TestHandler {
    gathered: watch::Sender<bool>,
    payloads: mpsc::UnboundedSender<Vec<u8>>,
    channels: mpsc::UnboundedSender<Arc<dyn DataChannel>>,
}

#[async_trait]
impl PeerConnectionEventHandler for TestHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.send(true);
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let payloads = self.payloads.clone();
        tokio::spawn(async move {
            while let Some(event) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                    let _ = payloads.send(packet.payload.to_vec());
                }
            }
        });
    }

    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        let _ = self.channels.send(channel);
    }
}

pub(crate) struct TestPeer {
    pub pc: Arc<dyn PeerConnection>,
    gathered: watch::Receiver<bool>,
    pub payloads: mpsc::UnboundedReceiver<Vec<u8>>,
    pub channels: mpsc::UnboundedReceiver<Arc<dyn DataChannel>>,
}

fn opus() -> RTCRtpCodec {
    super::opus_codec()
}

/// A peer with the relay's media settings and a wildcard UDP socket.
pub(crate) async fn test_peer() -> TestPeer {
    let mut media = MediaEngine::default();
    media.register_codec(RTCRtpCodecParameters { rtp_codec: opus(), payload_type: 111 }, RtpCodecKind::Audio).unwrap();
    let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
    let (gathered_tx, gathered) = watch::channel(false);
    let (payloads_tx, payloads) = mpsc::unbounded_channel();
    let (channels_tx, channels) = mpsc::unbounded_channel();
    let handler = Arc::new(TestHandler { gathered: gathered_tx, payloads: payloads_tx, channels: channels_tx });
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().build())
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_setting_engine(SettingEngineBuilder::new().build())
        .with_handler(handler)
        .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
        .build()
        .await
        .unwrap();
    TestPeer { pc: Arc::new(pc), gathered, payloads, channels }
}

pub(crate) fn output_track(label: &str) -> Arc<TrackLocalStaticRTP> {
    Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
        label.to_string(),
        "audio".to_string(),
        label.to_string(),
        RtpCodecKind::Audio,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(rand::random::<u32>()), ..Default::default() },
            codec: opus(),
            ..Default::default()
        }],
    )))
}

impl TestPeer {
    pub async fn complete_offer(&self) -> String {
        let offer = self.pc.create_offer(None).await.unwrap();
        self.pc.set_local_description(offer).await.unwrap();
        self.wait_gathered().await
    }

    pub async fn complete_answer(&self) -> String {
        let answer = self.pc.create_answer(None).await.unwrap();
        self.pc.set_local_description(answer).await.unwrap();
        self.wait_gathered().await
    }

    async fn wait_gathered(&self) -> String {
        let mut rx = self.gathered.clone();
        tokio::time::timeout(Duration::from_secs(5), rx.wait_for(|g| *g)).await.expect("ICE gathering").unwrap();
        self.pc.local_description().await.unwrap().sdp
    }
}

fn media_relay(config: CodexLiveMediaRelayConfig) -> PionMediaRelay {
    PionMediaRelay::new(&config, Arc::new(MediaLimiter::default())).unwrap()
}

fn offer_candidates_are_loopback(offer: &str) -> bool {
    let mut count = 0;
    for line in offer.lines() {
        let Some(rest) = line.trim().strip_prefix("a=candidate:") else { continue };
        count += 1;
        let address: Option<IpAddr> = rest.split_whitespace().nth(4).and_then(|a| a.parse().ok());
        if !address.is_some_and(|a| a.is_loopback()) {
            return false;
        }
    }
    count > 0
}

#[tokio::test]
async fn relay_selects_remote_proxy_mode() {
    let client = test_peer().await;
    client.pc.create_data_channel(REALTIME_DATA_CHANNEL_LABEL, None).await.unwrap();
    let client_offer = client.complete_offer().await;
    let relay = media_relay(CodexLiveMediaRelayConfig { enabled: true, public_ip: "198.51.100.1".into(), ..Default::default() });
    for (name, proxy_url, proxied) in [
        ("inherit", "", false),
        ("direct", "direct", false),
        ("HTTP", "http://proxy.example:8080", true),
        ("HTTPS", "https://proxy.example:8443", true),
        ("SOCKS5", "socks5://proxy.example:1080", true),
        ("SOCKS5H", "socks5h://proxy.example:1080", true),
    ] {
        let route = MediaRoute { proxy_url: proxy_url.into(), ..Default::default() };
        let (session, offer) = relay.new_session(&client_offer, route).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        let _ = session;
        if proxied {
            assert!(offer_candidates_are_loopback(&offer), "{name}: proxied upstream offer exposed a non-loopback candidate\n{offer}");
        }
        // The concrete type is hidden behind the trait; proxying shows in the candidates above.
        let _ = proxied;
        session.close_with_reason("test_complete");
    }
    let route = MediaRoute { proxy_url: "invalid-proxy".into(), ..Default::default() };
    assert!(relay.new_session(&client_offer, route).await.is_err());
}

async fn recv<T>(rx: &mut mpsc::UnboundedReceiver<T>, what: &str) -> T {
    tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap_or_else(|_| panic!("timed out waiting for {what}")).expect(what)
}

/// Polls a data channel in the background: messages as `(is_string, data)`, opening via `open`.
fn watch_channel(channel: Arc<dyn DataChannel>) -> (mpsc::UnboundedReceiver<(bool, Vec<u8>)>, watch::Receiver<bool>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (open_tx, open_rx) = watch::channel(false);
    tokio::spawn(async move {
        while let Some(event) = channel.poll().await {
            match event {
                DataChannelEvent::OnOpen => {
                    let _ = open_tx.send(true);
                }
                DataChannelEvent::OnMessage(m) => {
                    let _ = tx.send((m.is_string, m.data.to_vec()));
                }
                DataChannelEvent::OnClose => return,
                _ => {}
            }
        }
    });
    (rx, open_rx)
}

async fn wait_open(mut open: watch::Receiver<bool>) {
    tokio::time::timeout(Duration::from_secs(10), open.wait_for(|o| *o)).await.expect("DataChannel did not open").unwrap();
}

async fn send_test_rtp(track: &TrackLocalStaticRTP, payload: &[u8], received: &mut mpsc::UnboundedReceiver<Vec<u8>>) {
    let ssrc = track.ssrcs().await[0];
    for sequence in 1u16..=100 {
        let mut packet = rtp::Packet::default();
        packet.header.version = 2;
        packet.header.payload_type = 111;
        packet.header.sequence_number = sequence;
        packet.header.timestamp = u32::from(sequence) * 960;
        packet.header.ssrc = ssrc;
        packet.payload = Bytes::copy_from_slice(payload);
        track.write_rtp(packet).await.unwrap();
        if let Ok(Some(got)) = tokio::time::timeout(Duration::from_millis(50), received.recv()).await {
            assert_eq!(got, payload);
            return;
        }
    }
    panic!("RTP packet was not relayed");
}

#[tokio::test]
async fn relay_bridges_audio_and_data_channel() {
    let mut client = test_peer().await;
    let client_audio = output_track("client-audio");
    client.pc.add_track(client_audio.clone() as Arc<dyn TrackLocal>).await.unwrap();
    let client_data = client.pc.create_data_channel(REALTIME_DATA_CHANNEL_LABEL, None).await.unwrap();
    let (mut client_messages, client_open) = watch_channel(client_data.clone());
    let client_offer = client.complete_offer().await;

    let limiter = Arc::new(MediaLimiter::default());
    let config = CodexLiveMediaRelayConfig { enabled: true, max_sessions: 1, ..Default::default() };
    let relay = PionMediaRelay::new(&config, limiter.clone()).unwrap();
    let (session, relay_offer) = relay
        .new_session(&client_offer, MediaRoute { credential: "Voice credential".into(), auth_index: "auth-index".into(), ..Default::default() })
        .await
        .unwrap();
    session.set_call_id("call-log-test");
    // A reloaded relay shares the session capacity.
    let reloaded = PionMediaRelay::new(&config, limiter.clone()).unwrap();
    assert!(reloaded.new_session(&client_offer, MediaRoute::default()).await.is_err(), "reloaded relay bypassed the shared capacity");

    let mut upstream = test_peer().await;
    upstream.pc.set_remote_description(RTCSessionDescription::offer(relay_offer).unwrap()).await.unwrap();
    let upstream_audio = output_track("upstream-audio");
    upstream.pc.add_track(upstream_audio.clone() as Arc<dyn TrackLocal>).await.unwrap();
    let upstream_answer = upstream.complete_answer().await;
    let downstream_answer = session.accept_upstream_answer(&upstream_answer).await.unwrap();
    client.pc.set_remote_description(RTCSessionDescription::answer(downstream_answer).unwrap()).await.unwrap();

    let upstream_channel = recv(&mut upstream.channels, "upstream DataChannel").await;
    assert_eq!(upstream_channel.label().await.unwrap(), REALTIME_DATA_CHANNEL_LABEL);
    let (mut upstream_messages, upstream_open) = watch_channel(upstream_channel.clone());
    wait_open(client_open).await;
    wait_open(upstream_open).await;

    client_data.send_text("from-client").await.unwrap();
    assert_eq!(recv(&mut upstream_messages, "upstream message").await, (true, b"from-client".to_vec()));
    upstream_channel.send_text("from-upstream").await.unwrap();
    assert_eq!(recv(&mut client_messages, "client message").await, (true, b"from-upstream".to_vec()));
    client_data.send(BytesMut::from(&[1u8, 2, 3][..])).await.unwrap();
    assert_eq!(recv(&mut upstream_messages, "binary message").await, (false, vec![1, 2, 3]));

    send_test_rtp(&client_audio, &[0xf8, 0xff, 0xfe], &mut upstream.payloads).await;
    send_test_rtp(&upstream_audio, &[0xf8, 0xfe, 0xfd], &mut client.payloads).await;

    session.close_with_reason("closed");
    // The slot is released once the peers are closed.
    let mut released = false;
    for _ in 0..100 {
        if limiter.acquire() {
            released = true;
            limiter.release();
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(released, "shared capacity was not released");
}

#[test]
fn public_remote_ip_filter() {
    for (raw, want) in [
        ("8.8.8.8", true),
        ("2001:4860::1", true),
        ("127.0.0.1", false),
        ("10.0.0.1", false),
        ("169.254.1.1", false),
        ("224.0.0.1", false),
        ("::1", false),
        ("fc00::1", false),
        ("fe80::1", false),
        ("ff02::1", false),
        ("0.0.0.0", false),
    ] {
        assert_eq!(is_public_remote_ip(&raw.parse().unwrap()), want, "{raw}");
    }
}

#[test]
fn private_candidates_are_stripped_from_the_offer() {
    let sdp = "v=0\r\na=candidate:1 1 udp 1 10.0.0.1 5000 typ host\r\na=candidate:2 1 udp 1 8.8.8.8 5000 typ host\r\na=candidate:3 1 udp 1 abc.local 5000 typ host\r\na=mid:0\r\n";
    assert_eq!(filter_remote_candidates(sdp), "v=0\r\na=candidate:2 1 udp 1 8.8.8.8 5000 typ host\r\na=mid:0\r\n");
}
