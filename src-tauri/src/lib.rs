// lib.rs
// Tauri application library root.
// Defines all Tauri commands, application state, and the streaming pipeline.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{mpsc, Mutex};

mod audio;
mod capture;
mod encoder;
mod signaling;
mod webrtc_session;

#[cfg(target_os = "windows")]
mod audio_win;
#[cfg(target_os = "windows")]
mod capture_win;

// ─── Application state ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionRole {
    Host,
    Viewer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub role: SessionRole,
    pub status: String,
    pub invite_code: Option<String>,
    pub capture_method: Option<String>,
    pub encoder_kind: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Shared mutable state protected by a Mutex.
struct AppState {
    session: Mutex<Option<SessionState>>,
    /// Stop signal for the active streaming pipeline.
    stop_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

// ─── Tauri Commands ───────────────────────────────────────────────────────────

/// List all capturable windows and monitors.
#[tauri::command]
async fn list_capture_sources() -> Result<Vec<capture::CaptureSource>, String> {
    Ok(capture::enumerate_sources())
}

/// Start a host session: capture → encode → wait for viewer.
/// Returns the invite code the host should share.
#[tauri::command]
async fn start_host_session(
    source_id: String,
    fps: u32,
    bitrate_mbps: u32,
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<String, String> {
    let fps = fps.clamp(30, 60);
    let bitrate_bps = (bitrate_mbps.clamp(5, 35)) * 1_000_000;

    // Parse source ID into a CaptureTarget
    let target = parse_source_id(&source_id)?;

    // Emit status update
    emit_status(&app, "generating_token", None);

    // Start capture
    let (capture_session, mut frame_rx) =
        capture::start_capture(target, fps).await.map_err(|e| e.to_string())?;

    let capture_method = format!("{:?}", capture_session.method);
    let (cap_w, cap_h) = (capture_session.width, capture_session.height);

    // Initialise encoder
    let enc_config = encoder::EncoderConfig {
        width: cap_w,
        height: cap_h,
        fps,
        bitrate_bps,
        max_bitrate_bps: 35_000_000,
        keyframe_interval_secs: 2,
    };
    let mut enc = encoder::Encoder::new(enc_config).map_err(|e| e.to_string())?;
    let encoder_kind = enc.kind.to_string();

    // Start audio capture
    let mut audio_rx = audio::start_audio_capture().await.map_err(|e| e.to_string())?;

    // Start signaling server on a random port
    let signal_port: u16 = 9001;
    let (token, peer_ready_rx) =
        signaling::start_host_signaling(signal_port).await.map_err(|e| e.to_string())?;

    // Encode the invite: include local IP + port + token
    let local_ip = get_local_ip().unwrap_or_else(|| "127.0.0.1".to_string());
    let invite_code = signaling::encode_invite(&local_ip, signal_port, &token);

    // Update state
    {
        let mut sess = state.session.lock().await;
        *sess = Some(SessionState {
            role: SessionRole::Host,
            status: "waiting_for_viewer".into(),
            invite_code: Some(invite_code.clone()),
            capture_method: Some(capture_method),
            encoder_kind: Some(encoder_kind),
            width: Some(cap_w),
            height: Some(cap_h),
        });
    }

    emit_status(&app, "waiting_for_viewer", Some(&invite_code));

    // Channel for encoded video packets
    let (video_enc_tx, video_enc_rx) = mpsc::channel::<encoder::EncodedPacket>(8);

    // Stop signal
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    *state.stop_tx.lock().await = Some(stop_tx);

    let app_clone = app.clone();
    let state_clone = Arc::clone(&state);

    // Spawn pipeline task
    tokio::spawn(async move {
        // Encode loop: frame_rx → encoder → video_enc_tx
        let app2 = app_clone.clone();
        let enc_task = tokio::spawn(async move {
            let mut enc = enc;
            while let Some(frame) = frame_rx.recv().await {
                match enc.encode(&frame) {
                    Ok(packets) => {
                        for pkt in packets {
                            if video_enc_tx.send(pkt).await.is_err() { break; }
                        }
                    }
                    Err(e) => {
                        log::error!("Encoder error: {e}");
                        emit_error(&app2, &format!("Encoder error: {e}"));
                        break;
                    }
                }
            }
        });

        // Wait for viewer to connect
        let app3 = app_clone.clone();
        let peer = match peer_ready_rx.await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                emit_error(&app3, &format!("Signaling error: {e}"));
                return;
            }
            Err(_) => {
                emit_error(&app3, "Signaling channel closed unexpectedly");
                return;
            }
        };

        // Update state to connecting
        {
            let mut sess = state_clone.session.lock().await;
            if let Some(s) = sess.as_mut() {
                s.status = "connecting".into();
            }
        }
        emit_status(&app_clone, "connecting", None);

        // Status channel from WebRTC session
        let (status_tx, mut status_rx) = mpsc::channel::<String>(8);

        let app4 = app_clone.clone();
        tokio::spawn(async move {
            while let Some(status) = status_rx.recv().await {
                emit_status(&app4, &status, None);
            }
        });

        // Run the WebRTC streaming session
        if let Err(e) = webrtc_session::run_host_session(
            video_enc_rx,
            audio_rx,
            peer,
            status_tx,
        ).await {
            log::error!("Host session error: {e}");
            emit_error(&app_clone, &e.to_string());
        }

        emit_status(&app_clone, "disconnected", None);
    });

    Ok(invite_code)
}

/// Join a session as a viewer using the invite code.
#[tauri::command]
async fn join_viewer_session(
    invite_code: String,
    app: AppHandle,
    _state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    let (host_ip, port, _token) =
        signaling::decode_invite(&invite_code).map_err(|e| e.to_string())?;

    emit_status(&app, "connecting", None);

    // Connect to host signaling.
    let peer = signaling::connect_viewer_signaling(&host_ip, port)
        .await
        .map_err(|e| format!("Cannot reach host: {e}"))?;

    // Run the viewer handshake + streaming. The decoder tasks inside the
    // WebRTC session emit `rtp-video` / `rtp-audio` events directly to the
    // frontend (no global channels or sync/async bridge needed).
    let app2 = app.clone();
    tokio::spawn(async move {
        if let Err(e) = run_viewer_session(peer, app2.clone()).await {
            log::error!("Viewer session error: {e}");
            emit_error(&app2, &e.to_string());
            emit_status(&app2, "disconnected", None);
        }
    });

    Ok(())
}

/// Stop the active session (either role).
#[tauri::command]
async fn stop_session(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let mut stop = state.stop_tx.lock().await;
    if let Some(tx) = stop.take() {
        let _ = tx.send(());
    }
    let mut sess = state.session.lock().await;
    *sess = None;
    Ok(())
}

/// Get current session state.
#[tauri::command]
async fn get_session_state(
    state: State<'_, Arc<AppState>>,
) -> Result<Option<SessionState>, String> {
    Ok(state.session.lock().await.clone())
}

/// Minimise the window.
#[tauri::command]
async fn window_minimize(app: AppHandle) -> Result<(), String> {
    app.get_webview_window("main")
        .ok_or("no main window")?
        .minimize()
        .map_err(|e| e.to_string())
}

/// Toggle maximise.
#[tauri::command]
async fn window_maximize(app: AppHandle) -> Result<(), String> {
    let w = app.get_webview_window("main").ok_or("no main window")?;
    if w.is_maximized().map_err(|e| e.to_string())? {
        w.unmaximize().map_err(|e| e.to_string())
    } else {
        w.maximize().map_err(|e| e.to_string())
    }
}

/// Close the window / exit app.
#[tauri::command]
async fn window_close(app: AppHandle) -> Result<(), String> {
    app.get_webview_window("main")
        .ok_or("no main window")?
        .close()
        .map_err(|e| e.to_string())
}

// ─── Tauri app entry point ────────────────────────────────────────────────────

pub fn run() {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,gameshare=debug"),
    )
    .init();

    let app_state = Arc::new(AppState {
        session: Mutex::new(None),
        stop_tx: Mutex::new(None),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            list_capture_sources,
            start_host_session,
            join_viewer_session,
            stop_session,
            get_session_state,
            window_minimize,
            window_maximize,
            window_close,
        ])
        .run(tauri::generate_context!())
        .expect("error while running gameshare application");
}

// ─── Viewer session orchestration ─────────────────────────────────────────────

async fn run_viewer_session(mut peer: signaling::SignalingPeer, app: AppHandle) -> Result<()> {
    // Wait for offer from host
    let offer_sdp = loop {
        match peer.rx.recv().await {
            Some(SignalMessage::Offer { sdp }) => break sdp,
            Some(SignalMessage::IceCandidate { .. }) => {} // buffer; handled after answer
            None => return Err(anyhow::anyhow!("Signaling closed before offer")),
            _ => {}
        }
    };

    let (viewer_session, answer_sdp) =
        webrtc_session::ViewerSession::from_offer(offer_sdp, app.clone()).await?;

    // Send answer
    peer.tx.send(SignalMessage::Answer { sdp: answer_sdp }).await
        .map_err(|e| anyhow::anyhow!("send answer: {e}"))?;

    emit_status(&app, "streaming", None);

    // Forward remaining ICE candidates
    while let Some(msg) = peer.rx.recv().await {
        match msg {
            SignalMessage::IceCandidate { candidate, .. } => {
                let _ = viewer_session.add_ice_candidate(&candidate).await;
            }
            SignalMessage::Bye => break,
            _ => {}
        }
    }

    viewer_session.close().await?;
    emit_status(&app, "disconnected", None);
    Ok(())
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn emit_status(app: &AppHandle, status: &str, invite: Option<&str>) {
    #[derive(Clone, Serialize)]
    struct StatusEvent {
        status: String,
        invite_code: Option<String>,
    }
    let _ = app.emit("session-status", StatusEvent {
        status: status.to_string(),
        invite_code: invite.map(|s| s.to_string()),
    });
}

fn emit_error(app: &AppHandle, message: &str) {
    #[derive(Clone, Serialize)]
    struct ErrorEvent { message: String }
    let _ = app.emit("session-error", ErrorEvent { message: message.to_string() });
}

fn parse_source_id(id: &str) -> Result<capture::CaptureTarget, String> {
    if let Some(idx) = id.strip_prefix("monitor:") {
        let index: u32 = idx.parse().map_err(|_| format!("Invalid monitor index: {id}"))?;
        Ok(capture::CaptureTarget::Monitor { index })
    } else if let Some(hwnd_str) = id.strip_prefix("window:") {
        let hwnd: isize = hwnd_str.parse().map_err(|_| format!("Invalid hwnd: {id}"))?;
        Ok(capture::CaptureTarget::Window { hwnd })
    } else {
        Err(format!("Unknown source id format: {id}"))
    }
}

fn get_local_ip() -> Option<String> {
    // Find the local IP by connecting a UDP socket to a public address
    // (no data is sent — just determines the outbound interface).
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    Some(addr.ip().to_string())
}

// Re-export SignalMessage for webrtc_session.rs
use signaling::SignalMessage;
