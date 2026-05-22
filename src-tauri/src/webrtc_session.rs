// webrtc_session.rs
// WebRTC peer connection management.
// Uses the `webrtc` crate (pure Rust, no libwebrtc dependency).
//
// Host side:
//   - Creates a PeerConnection with one H.264 video track + one Opus audio track.
//   - Generates an SDP offer, exchanges via signaling, starts RTP streaming.
//
// Viewer side:
//   - Creates a PeerConnection, receives the offer, generates answer.
//   - Decodes incoming RTP into raw frames for display.

use anyhow::{anyhow, Result};
use base64::Engine;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::{broadcast, mpsc, watch};
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS},
        APIBuilder,
    },
    ice_transport::ice_server::RTCIceServer,
    interceptor::registry::Registry,
    media::Sample,
    peer_connection::{
        configuration::RTCConfiguration,
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
        RTCPeerConnection,
    },
    rtp::{codecs::h264::H264Packet, packetizer::Depacketizer},
    rtp_transceiver::{
        rtp_codec::{
            RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
        },
        // In webrtc 0.11 the direction enum lives in its own sub-module.
        rtp_transceiver_direction::RTCRtpTransceiverDirection,
    },
    track::{
        track_local::{track_local_static_sample::TrackLocalStaticSample, TrackLocal},
        track_remote::TrackRemote,
    },
};

use crate::{
    encoder::EncodedPacket,
    audio::AudioPacket,
    signaling::{SignalingPeer, SignalMessage},
};

/// ICE/STUN/TURN servers (PRD §6.2).
const STUN_SERVER: &str = "stun:stun.l.google.com:19302";
// TURN server — replace with your deployment's TURN credentials.
// Leave empty string to disable TURN (P2P only mode for LAN use).
const TURN_SERVER: &str = "";
const TURN_USERNAME: &str = "";
const TURN_CREDENTIAL: &str = "";

/// Connection stats emitted periodically to the UI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct ConnectionStats {
    pub state: String,
    pub bytes_sent: u64,
    pub bytes_recv: u64,
    pub rtt_ms: Option<f64>,
    pub packets_lost: u64,
    pub packets_sent: u64,
    pub jitter_ms: f64,
    pub connection_type: String, // "P2P" | "TURN"
}

/// A live WebRTC session on the host side.
pub struct HostSession {
    pc: Arc<RTCPeerConnection>,
    video_track: Arc<TrackLocalStaticSample>,
    audio_track: Arc<TrackLocalStaticSample>,
    /// Receives ICE/DTLS connection state updates set by the callback below.
    state_rx: watch::Receiver<RTCPeerConnectionState>,
}

impl HostSession {
    /// Create a new host-side peer connection and generate an SDP offer.
    /// Returns the session and the offer SDP string.
    pub async fn new() -> Result<(Self, String)> {
        let (pc, video_track, audio_track) = create_peer_connection(true).await?;

        // Wire up ICE / DTLS connection state so we know when the peer is reachable.
        let (state_tx, state_rx) = watch::channel(RTCPeerConnectionState::New);
        let state_tx = Arc::new(state_tx);
        {
            let state_tx = Arc::clone(&state_tx);
            pc.on_peer_connection_state_change(Box::new(move |s| {
                log::info!("Host WebRTC state: {s:?}");
                let _ = state_tx.send(s);
                Box::pin(async {})
            }));
        }

        // Generate offer
        let offer = pc.create_offer(None).await
            .map_err(|e| anyhow!("create_offer: {e}"))?;
        pc.set_local_description(offer.clone()).await
            .map_err(|e| anyhow!("set_local_description: {e}"))?;

        // Wait for ICE gathering to complete (or timeout after 5s)
        let mut gather_complete = pc.gathering_complete_promise().await;
        tokio::select! {
            _ = gather_complete.recv() => {}
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                log::warn!("ICE gathering timed out — sending partial candidates");
            }
        }

        let local_desc = pc.local_description().await
            .ok_or_else(|| anyhow!("No local description after gathering"))?;

        Ok((HostSession { pc, video_track, audio_track, state_rx }, local_desc.sdp))
    }

    /// Complete handshake: set the viewer's SDP answer.
    pub async fn set_answer(&self, sdp: String) -> Result<()> {
        let answer = RTCSessionDescription::answer(sdp)
            .map_err(|e| anyhow!("answer SDP parse: {e}"))?;
        self.pc.set_remote_description(answer).await
            .map_err(|e| anyhow!("set_remote_description: {e}"))
    }

    /// Add a remote ICE candidate.
    pub async fn add_ice_candidate(&self, candidate: &str) -> Result<()> {
        use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
        let init = RTCIceCandidateInit {
            candidate: candidate.to_string(),
            ..Default::default()
        };
        self.pc.add_ice_candidate(init).await
            .map_err(|e| anyhow!("add_ice_candidate: {e}"))
    }

    /// Send an encoded video packet over the video track.
    pub async fn send_video(&self, packet: &EncodedPacket) -> Result<()> {
        let duration = std::time::Duration::from_micros(
            // 1/60s per frame at 60fps
            16_667,
        );
        let sample = Sample {
            data: bytes::Bytes::copy_from_slice(&packet.data),
            duration,
            timestamp: std::time::SystemTime::now(),
            packet_timestamp: (packet.pts_us / 90) as u32, // 90kHz RTP clock
            ..Default::default()
        };
        self.video_track.write_sample(&sample).await
            .map_err(|e| anyhow!("video write_sample: {e}"))
    }

    /// Send an encoded audio packet over the audio track.
    pub async fn send_audio(&self, packet: &AudioPacket) -> Result<()> {
        let duration = std::time::Duration::from_micros(packet.duration_us);
        let sample = Sample {
            data: bytes::Bytes::copy_from_slice(&packet.data),
            duration,
            timestamp: std::time::SystemTime::now(),
            packet_timestamp: (packet.pts_us / 1000 * 48) as u32, // 48kHz RTP clock
            ..Default::default()
        };
        self.audio_track.write_sample(&sample).await
            .map_err(|e| anyhow!("audio write_sample: {e}"))
    }

    /// Block until the WebRTC ICE+DTLS handshake reaches `Connected`,
    /// or return an error on `Failed` / timeout.
    pub async fn wait_for_connection(&self) -> Result<()> {
        wait_for_ice_connected(self.state_rx.clone(), 30).await
    }

    /// Close the connection.
    pub async fn close(&self) -> Result<()> {
        self.pc.close().await.map_err(|e| anyhow!("pc.close: {e}"))
    }
}

/// A live WebRTC session on the viewer side.
///
/// We do not decode pixels in Rust. Incoming media is depacketized on
/// background tasks and emitted to the WebView as `rtp-video` (Annex-B H.264
/// access units) and `rtp-audio` (Opus frames), both base64-encoded. The
/// WebView decodes with WebCodecs (GPU-accelerated) and renders to a canvas.
pub struct ViewerSession {
    pc: Arc<RTCPeerConnection>,
    state_rx: watch::Receiver<RTCPeerConnectionState>,
}

impl ViewerSession {
    /// Create a viewer-side peer connection, set the host's offer, and return
    /// an SDP answer. `app` is used by the decoder tasks to emit media events.
    pub async fn from_offer(sdp: String, app: AppHandle) -> Result<(Self, String)> {
        let (pc, _v, _a) = create_peer_connection(false).await?;

        // Wire ICE state so we can wait for actual connectivity.
        let (state_tx, state_rx) = watch::channel(RTCPeerConnectionState::New);
        let state_tx = Arc::new(state_tx);
        {
            let state_tx = Arc::clone(&state_tx);
            pc.on_peer_connection_state_change(Box::new(move |s| {
                log::info!("Viewer WebRTC state: {s:?}");
                let _ = state_tx.send(s);
                Box::pin(async {})
            }));
        }

        // Register track handler — called when remote tracks arrive.
        // In webrtc 0.11, on_track is a sync fn; codec() is also sync now.
        pc.on_track(Box::new(move |track, _receiver, _transceiver| {
            let app = app.clone();
            Box::pin(async move {
                let codec = track.codec(); // sync in 0.11 — no .await
                let mime = codec.capability.mime_type.to_lowercase();
                log::info!("Remote track: {mime}");

                if mime.contains("h264") {
                    tokio::spawn(async move { decode_h264_track(track, app).await; });
                } else if mime.contains("opus") {
                    tokio::spawn(async move { decode_opus_track(track, app).await; });
                }
            })
        })); // sync — no .await

        // Set remote offer
        let offer = RTCSessionDescription::offer(sdp)
            .map_err(|e| anyhow!("offer parse: {e}"))?;
        pc.set_remote_description(offer).await
            .map_err(|e| anyhow!("set_remote_description: {e}"))?;

        // Generate answer
        let answer = pc.create_answer(None).await
            .map_err(|e| anyhow!("create_answer: {e}"))?;
        pc.set_local_description(answer.clone()).await
            .map_err(|e| anyhow!("set_local_description: {e}"))?;

        // Wait for ICE
        let mut gather_complete = pc.gathering_complete_promise().await;
        tokio::select! {
            _ = gather_complete.recv() => {}
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
        }

        let local_desc = pc.local_description().await
            .ok_or_else(|| anyhow!("No local description"))?;

        Ok((ViewerSession { pc, state_rx }, local_desc.sdp))
    }

    pub async fn add_ice_candidate(&self, candidate: &str) -> Result<()> {
        use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
        let init = RTCIceCandidateInit { candidate: candidate.to_string(), ..Default::default() };
        self.pc.add_ice_candidate(init).await.map_err(|e| anyhow!("{e}"))
    }

    /// Block until ICE+DTLS reaches `Connected`, or error on failure/timeout.
    pub async fn wait_for_connection(&self) -> Result<()> {
        wait_for_ice_connected(self.state_rx.clone(), 30).await
    }

    pub async fn close(&self) -> Result<()> {
        self.pc.close().await.map_err(|e| anyhow!("{e}"))
    }
}

// ─── ICE connection waiter ────────────────────────────────────────────────────

/// Wait until the peer connection reaches `Connected`, or return an error on
/// `Failed`/`Closed` or if `timeout_secs` elapses without connecting.
///
/// This ensures we only declare "streaming" when the ICE + DTLS handshake has
/// actually completed and RTP can flow — not merely when SDP was exchanged.
async fn wait_for_ice_connected(
    mut rx: watch::Receiver<RTCPeerConnectionState>,
    timeout_secs: u64,
) -> Result<()> {
    let timeout = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
    tokio::pin!(timeout);
    loop {
        let state = *rx.borrow();
        match state {
            RTCPeerConnectionState::Connected => {
                log::info!("WebRTC ICE connected ✓");
                return Ok(());
            }
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                return Err(anyhow!(
                    "WebRTC ICE failed (state={state:?}). \
                     If testing locally, allow the app through Windows Firewall \
                     (UDP traffic between the two instances must not be blocked)."
                ));
            }
            _ => {}
        }
        tokio::select! {
            res = rx.changed() => {
                if res.is_err() { return Err(anyhow!("ICE state channel closed")); }
            }
            _ = &mut timeout => {
                return Err(anyhow!(
                    "WebRTC ICE timed out after {timeout_secs}s — peers could not reach \
                     each other. Check Windows Firewall or try a TURN relay."
                ));
            }
        }
    }
}

// ─── Shared peer connection factory ──────────────────────────────────────────

async fn create_peer_connection(
    is_host: bool,
) -> Result<(Arc<RTCPeerConnection>, Arc<TrackLocalStaticSample>, Arc<TrackLocalStaticSample>)> {
    // Media engine with H.264 + Opus codecs
    let mut media_engine = MediaEngine::default();

    // H.264 — Baseline/High profile, PT 96
    media_engine.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_string(),
                clock_rate: 90_000,
                channels: 0,
                // Annex-B stream, no packetization-mode constraint
                sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_string(),
                rtcp_feedback: vec![],
            },
            payload_type: 96,
            ..Default::default()
        },
        RTPCodecType::Video,
    ).map_err(|e| anyhow!("register H264 codec: {e}"))?;

    // Opus stereo 48kHz, PT 111
    media_engine.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_string(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1".to_string(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        },
        RTPCodecType::Audio,
    ).map_err(|e| anyhow!("register Opus codec: {e}"))?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media_engine)
        .map_err(|e| anyhow!("register interceptors: {e}"))?;

    let api = APIBuilder::new()
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .build();

    // ICE servers
    let mut ice_servers = vec![
        RTCIceServer {
            urls: vec![STUN_SERVER.to_string()],
            ..Default::default()
        },
    ];
    if !TURN_SERVER.is_empty() {
        ice_servers.push(RTCIceServer {
            urls: vec![TURN_SERVER.to_string()],
            username: TURN_USERNAME.to_string(),
            credential: TURN_CREDENTIAL.to_string(),
            ..Default::default()
        });
    }

    let config = RTCConfiguration { ice_servers, ..Default::default() };
    let pc = Arc::new(
        api.new_peer_connection(config).await
           .map_err(|e| anyhow!("new_peer_connection: {e}"))?,
    );

    // Create video track
    let video_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_string(),
            clock_rate: 90_000,
            ..Default::default()
        },
        "video".to_string(),
        "gameshare-video".to_string(),
    ));

    // Create audio track
    let audio_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_OPUS.to_string(),
            clock_rate: 48_000,
            channels: 2,
            ..Default::default()
        },
        "audio".to_string(),
        "gameshare-audio".to_string(),
    ));

    if is_host {
        // Host sends video + audio; viewer only receives
        pc.add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
          .await.map_err(|e| anyhow!("add video track: {e}"))?;
        pc.add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
          .await.map_err(|e| anyhow!("add audio track: {e}"))?;
    } else {
        // Viewer needs recv-only transceivers so SDP negotiation works
        pc.add_transceiver_from_kind(
            RTPCodecType::Video,
            Some(webrtc::rtp_transceiver::RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        ).await.map_err(|e| anyhow!("add video transceiver: {e}"))?;
        pc.add_transceiver_from_kind(
            RTPCodecType::Audio,
            Some(webrtc::rtp_transceiver::RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        ).await.map_err(|e| anyhow!("add audio transceiver: {e}"))?;
    }

    Ok((pc, video_track, audio_track))
}

// ─── Viewer-side depacketizers ───────────────────────────────────────────────
//
// We do NOT decode pixels in Rust. Instead we reassemble the RTP stream back
// into a clean elementary stream and hand it to the WebView, which decodes
// with WebCodecs (GPU-accelerated) and renders to a canvas.
//
//   * Video: RTP H.264 (RFC 6184) is depacketized — FU-A fragments are
//     reassembled and STAP-A aggregates are split — into Annex-B NAL units.
//     We accumulate NALs until the RTP marker bit signals the end of an
//     access unit (one frame), then emit that whole frame as one event.
//   * Audio: each Opus RTP payload already is a complete Opus frame, so we
//     forward payloads directly.

/// Standard base64 engine, reused across packets.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

async fn decode_h264_track(track: Arc<TrackRemote>, app: AppHandle) {
    log::info!("H264 depacketizer started → emitting `rtp-video`");
    // is_avc = false (default) ⇒ depacketize() outputs Annex-B (start-code) NALs.
    let mut depacketizer = H264Packet::default();
    let mut access_unit: Vec<u8> = Vec::with_capacity(256 * 1024);

    loop {
        match track.read_rtp().await {
            Ok((rtp_packet, _)) => {
                if !rtp_packet.payload.is_empty() {
                    match depacketizer.depacketize(&rtp_packet.payload) {
                        Ok(nal) if !nal.is_empty() => access_unit.extend_from_slice(&nal),
                        Ok(_) => {} // mid-fragment of an FU-A: nothing complete yet
                        Err(e) => log::trace!("H264 depacketize: {e}"),
                    }
                }
                // Marker bit set ⇒ last packet of this frame ⇒ flush the access unit.
                if rtp_packet.header.marker && !access_unit.is_empty() {
                    let b64 = B64.encode(&access_unit);
                    if app.emit("rtp-video", b64).is_err() {
                        break; // frontend gone
                    }
                    access_unit.clear();
                }
            }
            Err(e) => {
                log::warn!("H264 track read ended: {e}");
                break;
            }
        }
    }
}

async fn decode_opus_track(track: Arc<TrackRemote>, app: AppHandle) {
    log::info!("Opus forwarder started → emitting `rtp-audio`");
    loop {
        match track.read_rtp().await {
            Ok((rtp_packet, _)) => {
                if rtp_packet.payload.is_empty() {
                    continue;
                }
                let b64 = B64.encode(&rtp_packet.payload);
                if app.emit("rtp-audio", b64).is_err() {
                    break;
                }
            }
            Err(e) => {
                log::warn!("Opus track read ended: {e}");
                break;
            }
        }
    }
}

// ─── Full signaling + streaming orchestration ─────────────────────────────────

/// Run a complete host streaming pipeline:
/// capture → encode → WebRTC → viewer.
/// This is called from the Tauri command and runs until the session ends.
pub async fn run_host_session(
    mut video_rx: broadcast::Receiver<EncodedPacket>,
    mut audio_rx: broadcast::Receiver<AudioPacket>,
    mut peer: SignalingPeer,
    state_tx: mpsc::Sender<String>,
) -> Result<()> {
    let _ = state_tx.send("connecting".to_string()).await;

    // Wrap session creation in a timeout — webrtc-rs can stall inside its own
    // async locks (ICE setup, DTLS init) before our own ICE timer even starts,
    // which would leave the session manager permanently stuck on this viewer.
    let (session, offer_sdp) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        HostSession::new(),
    )
    .await
    .unwrap_or_else(|_| Err(anyhow!("Host session init timed out after 10 s")))?;

    // Send offer to viewer
    peer.tx.send(SignalMessage::Offer { sdp: offer_sdp }).await
        .map_err(|e| anyhow!("send offer: {e}"))?;

    // Exchange ICE candidates and wait for answer — bounded by 15 s.
    // Without a deadline, a viewer that TCP-connects then crashes before
    // sending an answer would leave the host stuck here forever, preventing
    // the session manager from restarting with a new invite code.
    let answer_deadline = tokio::time::sleep(std::time::Duration::from_secs(15));
    tokio::pin!(answer_deadline);
    let answer_sdp = loop {
        tokio::select! {
            msg = peer.rx.recv() => {
                match msg {
                    Some(SignalMessage::Answer { sdp }) => break sdp,
                    Some(SignalMessage::IceCandidate { candidate, .. }) => {
                        let _ = session.add_ice_candidate(&candidate).await;
                    }
                    Some(SignalMessage::Bye) => return Err(anyhow!("Viewer sent Bye before answer")),
                    None => return Err(anyhow!("Signaling channel closed before answer")),
                    _ => {}
                }
            }
            _ = &mut answer_deadline => {
                return Err(anyhow!(
                    "Viewer did not send SDP answer within 15 s — \
                     restarting signaling for a new invite code"
                ));
            }
        }
    };

    session.set_answer(answer_sdp).await?;

    // Block until ICE+DTLS actually connects before claiming we're streaming.
    // On failure or timeout this returns Err which the caller will surface as
    // a session-error event so the user sees a real message instead of silence.
    log::info!("Waiting for WebRTC ICE connection…");
    session.wait_for_connection().await?;

    let _ = state_tx.send("streaming".to_string()).await;

    let pc_for_ice = Arc::new(session);

    // Main streaming loop — also drains late ICE candidates from the viewer.
    let mut video_bytes_sent: u64 = 0;
    let mut audio_bytes_sent: u64 = 0;
    let mut video_pkts_sent: u64 = 0;

    loop {
        tokio::select! {
            result = video_rx.recv() => {
                match result {
                    Ok(pkt) => {
                        video_bytes_sent += pkt.data.len() as u64;
                        video_pkts_sent += 1;
                        if video_pkts_sent == 1 {
                            log::info!(
                                "Host: sending first video packet ({} bytes, keyframe={})",
                                pkt.data.len(), pkt.is_keyframe
                            );
                        } else if video_pkts_sent % 120 == 0 {
                            log::info!("Host: {video_pkts_sent} video packets sent ({} KB total)",
                                video_bytes_sent / 1024);
                        }
                        if let Err(e) = pc_for_ice.send_video(&pkt).await {
                            log::warn!("Video send error: {e}");
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Consumer is too slow; encoder dropped n frames.
                        // The next keyframe (every 2s) will recover cleanly.
                        log::warn!("Video broadcast lagged by {n} frames — skipping");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            result = audio_rx.recv() => {
                match result {
                    Ok(pkt) => {
                        audio_bytes_sent += pkt.data.len() as u64;
                        if let Err(e) = pc_for_ice.send_audio(&pkt).await {
                            log::warn!("Audio send error: {e}");
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        log::warn!("Audio broadcast lagged by {n} packets — skipping");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = peer.rx.recv() => {
                match msg {
                    Some(SignalMessage::IceCandidate { candidate, .. }) => {
                        let _ = pc_for_ice.add_ice_candidate(&candidate).await;
                    }
                    // Viewer sent Bye or signaling channel closed — end session.
                    Some(SignalMessage::Bye) | None => break,
                    _ => {}
                }
            }
        }
    }

    log::info!(
        "Session ended. Video sent: {} MB, Audio sent: {} KB",
        video_bytes_sent / 1_000_000,
        audio_bytes_sent / 1_000,
    );

    pc_for_ice.close().await?;
    Ok(())
}
