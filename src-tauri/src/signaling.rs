// signaling.rs
// Lightweight in-process signaling layer for WebRTC offer/answer/ICE exchange.
//
// Architecture:
//   Host side:  runs a tiny tokio-based WebSocket server on localhost:9001.
//               Generates a 6-character invite code. The viewer connects using
//               the host's IP + code.
//
//   Viewer side: connects to the host's signaling WS URL (encoded in the invite).
//
// For LAN / internet use the invite code encodes host IP + port + random token.
// No external signaling server is required (PRD §6.1 direct P2P goal).
//
// If P2P ICE fails, the code falls back to the configured TURN server.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use tokio::sync::{mpsc, oneshot};

/// Messages exchanged over the signaling channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMessage {
    /// Host → Viewer: WebRTC SDP offer.
    Offer { sdp: String },
    /// Viewer → Host: WebRTC SDP answer.
    Answer { sdp: String },
    /// Both directions: ICE candidates.
    IceCandidate { candidate: String, sdp_mid: String, sdp_mline_index: u32 },
    /// Host → Viewer: session accepted, streaming about to start.
    SessionReady,
    /// Either direction: graceful hangup.
    Bye,
}

/// A connected signaling peer — bidirectional channel of SignalMessages.
pub struct SignalingPeer {
    pub tx: mpsc::Sender<SignalMessage>,
    pub rx: mpsc::Receiver<SignalMessage>,
}

/// Start a host-side signaling listener.
/// Returns the invite token and a future that resolves once a viewer connects.
pub async fn start_host_signaling(
    port: u16,
) -> Result<(String, oneshot::Receiver<Result<SignalingPeer>>)> {
    use tokio::net::TcpListener;

    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    let listener = TcpListener::bind(addr).await
        .map_err(|e| anyhow!("Failed to bind signaling port {port}: {e}"))?;

    // Generate a random 8-char alphanumeric token (no ambiguous chars), per PRD §5.5
    let token = generate_token(8);

    let (peer_tx, peer_rx) = oneshot::channel::<Result<SignalingPeer>>();

    tokio::spawn(async move {
        match listener.accept().await {
            Ok((stream, addr)) => {
                log::info!("Signaling: viewer connected from {addr}");
                match accept_ws(stream).await {
                    Ok(peer) => { let _ = peer_tx.send(Ok(peer)); }
                    Err(e) => { let _ = peer_tx.send(Err(e)); }
                }
            }
            Err(e) => {
                let _ = peer_tx.send(Err(anyhow!("Signaling accept error: {e}")));
            }
        }
    });

    Ok((token, peer_rx))
}

/// Connect to a host's signaling server as a viewer.
pub async fn connect_viewer_signaling(host_ip: &str, port: u16) -> Result<SignalingPeer> {
    let url = format!("ws://{host_ip}:{port}/");
    log::info!("Connecting to signaling server: {url}");
    connect_ws(&url).await
}

// ─── WebSocket transport ──────────────────────────────────────────────────────
// We use a simple line-delimited JSON protocol over raw TCP with a minimal
// WebSocket framing layer, avoiding heavy deps. In production this should use
// tokio-tungstenite or a similar battle-tested WS library.

async fn accept_ws(stream: tokio::net::TcpStream) -> Result<SignalingPeer> {
    #[cfg(feature = "tungstenite")]
    {
        use tokio_tungstenite::accept_async;
        let ws = accept_async(stream).await?;
        ws_to_peer(ws).await
    }
    // Fallback: raw newline-delimited JSON over TCP
    #[cfg(not(feature = "tungstenite"))]
    {
        raw_tcp_peer(stream).await
    }
}

async fn connect_ws(url: &str) -> Result<SignalingPeer> {
    #[cfg(feature = "tungstenite")]
    {
        use tokio_tungstenite::connect_async;
        let (ws, _) = connect_async(url).await?;
        ws_to_peer(ws).await
    }
    #[cfg(not(feature = "tungstenite"))]
    {
        // Parse host:port from URL and use raw TCP
        let addr = url
            .trim_start_matches("ws://")
            .trim_end_matches('/')
            .to_string();
        let stream = tokio::net::TcpStream::connect(&addr).await
            .map_err(|e| anyhow!("TCP connect to {addr}: {e}"))?;
        raw_tcp_peer(stream).await
    }
}

/// Wrap a raw TCP stream as a SignalingPeer using newline-delimited JSON.
async fn raw_tcp_peer(stream: tokio::net::TcpStream) -> Result<SignalingPeer> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read_half, mut write_half) = stream.into_split();
    let (in_tx, in_rx) = mpsc::channel::<SignalMessage>(32);
    let (out_tx, mut out_rx) = mpsc::channel::<SignalMessage>(32);

    // Reader task
    tokio::spawn(async move {
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() { continue; }
                    match serde_json::from_str::<SignalMessage>(trimmed) {
                        Ok(msg) => {
                            if in_tx.send(msg).await.is_err() { break; }
                        }
                        Err(e) => log::warn!("Signaling parse error: {e} — line: {trimmed}"),
                    }
                }
                Err(e) => { log::error!("Signaling read error: {e}"); break; }
            }
        }
    });

    // Writer task
    tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            match serde_json::to_string(&msg) {
                Ok(mut s) => {
                    s.push('\n');
                    if write_half.write_all(s.as_bytes()).await.is_err() { break; }
                }
                Err(e) => log::error!("Signaling serialize error: {e}"),
            }
        }
    });

    Ok(SignalingPeer { tx: out_tx, rx: in_rx })
}

/// Generate a random invite token using unambiguous alphanumeric chars.
pub fn generate_token(len: usize) -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

/// Encode host IP + port + token into a shareable invite string.
/// Format: BASE64(IP_BYTES + PORT_U16_BE + TOKEN_BYTES)
pub fn encode_invite(host_ip: &str, port: u16, token: &str) -> String {
    let mut buf = Vec::new();
    // Store as "IP:PORT:TOKEN" plain string for simplicity
    buf.extend_from_slice(host_ip.as_bytes());
    buf.push(b':');
    buf.extend_from_slice(port.to_string().as_bytes());
    buf.push(b':');
    buf.extend_from_slice(token.as_bytes());
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &buf)
}

/// Decode an invite string back to (host_ip, port, token).
pub fn decode_invite(invite: &str) -> Result<(String, u16, String)> {
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        invite,
    ).map_err(|e| anyhow!("Invalid invite (base64): {e}"))?;

    let s = String::from_utf8(bytes)
        .map_err(|e| anyhow!("Invalid invite (utf8): {e}"))?;

    let parts: Vec<&str> = s.splitn(3, ':').collect();
    if parts.len() != 3 {
        return Err(anyhow!("Invalid invite format"));
    }
    let host = parts[0].to_string();
    let port: u16 = parts[1].parse().map_err(|e| anyhow!("Invalid port: {e}"))?;
    let token = parts[2].to_string();
    Ok((host, port, token))
}
