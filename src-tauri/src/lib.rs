// lib.rs
// Tauri application library root.
// Defines all Tauri commands, application state, and the streaming pipeline.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{broadcast, mpsc, Mutex, Notify};

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
    /// Fired when the user explicitly stops the session (Stop / Cancel button).
    stop_notify: Arc<Notify>,
}

// ─── Tauri Commands ───────────────────────────────────────────────────────────

/// List all capturable windows and monitors.
#[tauri::command]
async fn list_capture_sources() -> Result<Vec<capture::CaptureSource>, String> {
    Ok(capture::enumerate_sources())
}

/// Start a host session: capture → encode → wait for viewer.
///
/// The encoder and capture run for the lifetime of the host session (the user
/// doesn't have to restart them between viewers). After each viewer disconnects
/// or a session fails, the session manager automatically restarts the signaling
/// server and shows a fresh invite code — the host never has to go back to the
/// home screen between viewer connections.
///
/// Returns the first invite code so the frontend can show it immediately.
#[tauri::command]
async fn start_host_session(
    source_id: String,
    fps: u32,
    bitrate_mbps: u32,
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> Result<String, String> {
    let fps = fps.clamp(30, 60);
    let bitrate_bps = bitrate_mbps.clamp(5, 35) * 1_000_000;

    let target = parse_source_id(&source_id)?;

    emit_status(&app, "generating_token", None);

    // ── Start capture ─────────────────────────────────────────────────────────
    let (capture_session, mut frame_rx) =
        capture::start_capture(target, fps).await.map_err(|e| e.to_string())?;

    let capture_method = format!("{:?}", capture_session.method);
    let (cap_w, cap_h) = (capture_session.width, capture_session.height);

    // ── Initialise encoder ────────────────────────────────────────────────────
    let enc_config = encoder::EncoderConfig {
        width: cap_w,
        height: cap_h,
        fps,
        bitrate_bps,
        max_bitrate_bps: 35_000_000,
        keyframe_interval_secs: 2,
    };
    let enc = encoder::Encoder::new(enc_config).map_err(|e| e.to_string())?;
    let encoder_kind = enc.kind.to_string();

    // ── Start audio capture ───────────────────────────────────────────────────
    let audio_mpsc_rx = audio::start_audio_capture().await.map_err(|e| e.to_string())?;

    // ── Broadcast channels ────────────────────────────────────────────────────
    // Both channels survive across multiple viewer attempts so the encoder and
    // audio capture threads never need to restart.
    //
    // Video: capacity 8 frames (~133 ms at 60 fps) — old frames are dropped
    //        if no viewer is subscribed, which is fine.
    // Audio: capacity 64 packets (20 ms each = 1.28 s) — generous buffer
    //        so audio starts cleanly after a new viewer connects.
    let (video_tx, _) = broadcast::channel::<encoder::EncodedPacket>(8);
    let (audio_tx, _) = broadcast::channel::<audio::AudioPacket>(64);

    // ── Encoder thread (MFT is !Send, must run on a plain OS thread) ─────────
    let video_bcast = video_tx.clone();
    let app_enc = app.clone();
    std::thread::spawn(move || {
        let mut enc = enc;
        while let Some(frame) = frame_rx.blocking_recv() {
            match enc.encode(&frame) {
                Ok(packets) => {
                    for pkt in packets {
                        // Ignore if no active subscriber — they'll get the next keyframe.
                        let _ = video_bcast.send(pkt);
                    }
                }
                Err(e) => {
                    log::error!("Encoder error: {e}");
                    emit_error(&app_enc, &format!("Encoder error: {e}"));
                    return;
                }
            }
        }
    });

    // ── Audio bridge: mpsc → broadcast ───────────────────────────────────────
    let audio_bcast = audio_tx.clone();
    tokio::spawn(async move {
        let mut audio_mpsc_rx = audio_mpsc_rx;
        while let Some(pkt) = audio_mpsc_rx.recv().await {
            let _ = audio_bcast.send(pkt);
        }
    });

    // ── Initialise session state ──────────────────────────────────────────────
    {
        let mut sess = state.session.lock().await;
        *sess = Some(SessionState {
            role: SessionRole::Host,
            status: "waiting_for_viewer".into(),
            invite_code: None,
            capture_method: Some(capture_method),
            encoder_kind: Some(encoder_kind),
            width: Some(cap_w),
            height: Some(cap_h),
        });
    }

    let local_ip = get_local_ip().unwrap_or_else(|| "127.0.0.1".to_string());

    // ── Start the first signaling server before spawning the manager task ─────
    // We need the initial invite code to return it from this command (so the
    // frontend can show it without waiting for an event).
    let (token, port, first_peer_rx) =
        signaling::start_host_signaling().await.map_err(|e| e.to_string())?;
    let initial_invite = signaling::encode_invite(&local_ip, port, &token);

    {
        let mut sess = state.session.lock().await;
        if let Some(s) = sess.as_mut() {
            s.invite_code = Some(initial_invite.clone());
        }
    }
    emit_status(&app, "waiting_for_viewer", Some(&initial_invite));

    let app_clone = app.clone();
    let state_clone = Arc::clone(&state);
    let stop_notify = Arc::clone(&state.stop_notify);

    // ── Session manager task ──────────────────────────────────────────────────
    // Loops indefinitely:
    //   1. Wait for a viewer to connect (or stop signal)
    //   2. Run the WebRTC session
    //   3. On disconnect / failure → restart signaling → new invite code → loop
    //   4. On stop_notify → exit cleanly
    tokio::spawn(async move {
        // First iteration reuses the pre-created signaling server.
        let mut pending_peer_rx = Some(first_peer_rx);
        let mut first = true;

        loop {
            // ── Get signaling future (pre-created or fresh) ───────────────────
            let peer_rx = if first {
                first = false;
                pending_peer_rx.take().unwrap()
            } else {
                // Restart signaling on a fresh OS-assigned port.
                let (tok, p, prx) = match signaling::start_host_signaling().await {
                    Ok(v) => v,
                    Err(e) => {
                        emit_error(&app_clone, &format!("Cannot restart signaling: {e}"));
                        break;
                    }
                };
                let invite = signaling::encode_invite(&local_ip, p, &tok);
                {
                    let mut sess = state_clone.session.lock().await;
                    if let Some(s) = sess.as_mut() {
                        s.invite_code = Some(invite.clone());
                        s.status = "waiting_for_viewer".into();
                    }
                }
                emit_status(&app_clone, "waiting_for_viewer", Some(&invite));
                prx
            };

            // ── Wait for viewer or stop ───────────────────────────────────────
            let peer = tokio::select! {
                result = peer_rx => {
                    match result {
                        Ok(Ok(p))  => p,
                        Ok(Err(e)) => {
                            emit_error(&app_clone, &format!("Signaling error: {e}"));
                            continue; // restart signaling
                        }
                        Err(_) => {
                            emit_error(&app_clone, "Signaling task panicked");
                            break;
                        }
                    }
                }
                _ = stop_notify.notified() => {
                    log::info!("Host session stopped by user");
                    break;
                }
            };

            // Viewer connected — start WebRTC handshake.
            {
                let mut sess = state_clone.session.lock().await;
                if let Some(s) = sess.as_mut() { s.status = "connecting".into(); }
            }
            emit_status(&app_clone, "connecting", None);

            // Status relay channel for WebRTC session.
            let (status_tx, mut status_rx) = mpsc::channel::<String>(8);
            let app_status = app_clone.clone();
            tokio::spawn(async move {
                while let Some(s) = status_rx.recv().await {
                    emit_status(&app_status, &s, None);
                }
            });

            // Fresh broadcast subscribers for this viewer attempt.
            let video_rx = video_tx.subscribe();
            let audio_rx = audio_tx.subscribe();

            match webrtc_session::run_host_session(video_rx, audio_rx, peer, status_tx).await {
                Ok(()) => {
                    log::info!("Viewer disconnected normally — restarting signaling");
                    // Loop: show new invite code for the next viewer.
                }
                Err(e) => {
                    log::warn!("Session failed ({e}) — restarting signaling");
                    emit_error(&app_clone, &e.to_string());
                    // Loop: show new invite code so the user can retry.
                }
            }
        }

        // Stopped — clean up.
        *state_clone.session.lock().await = None;
        emit_status(&app_clone, "disconnected", None);
    });

    Ok(initial_invite)
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

    let peer = signaling::connect_viewer_signaling(&host_ip, port)
        .await
        .map_err(|e| format!("Cannot reach host: {e}"))?;

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
    // Signal the session manager loop to exit.
    state.stop_notify.notify_one();
    // Clear frontend-visible session state immediately.
    *state.session.lock().await = None;
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
        stop_notify: Arc::new(Notify::new()),
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
            Some(SignalMessage::IceCandidate { .. }) => {}
            None => return Err(anyhow::anyhow!("Signaling closed before offer")),
            _ => {}
        }
    };

    let (viewer_session, answer_sdp) =
        webrtc_session::ViewerSession::from_offer(offer_sdp, app.clone()).await?;

    peer.tx.send(SignalMessage::Answer { sdp: answer_sdp }).await
        .map_err(|e| anyhow::anyhow!("send answer: {e}"))?;

    log::info!("Viewer waiting for WebRTC ICE connection…");
    viewer_session.wait_for_connection().await?;

    emit_status(&app, "streaming", None);

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
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    Some(addr.ip().to_string())
}

// Re-export SignalMessage for use in run_viewer_session above.
use signaling::SignalMessage;
