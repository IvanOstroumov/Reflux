// capture.rs
// Screen capture pipeline using Windows Graphics Capture (WGC) as primary,
// DXGI Desktop Duplication as fallback.
// All frames are delivered as raw BGRA pixel buffers with a timestamp.
// GDI / BitBlt is explicitly forbidden per PRD §5.1.

use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::sync::mpsc;

/// A single captured video frame.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Raw BGRA pixels, row-major, stride = width * 4.
    pub data: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
    /// Capture timestamp in microseconds (QueryPerformanceCounter on Windows).
    pub pts_us: u64,
}

/// Describes what to capture.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind")]
pub enum CaptureTarget {
    /// Capture a specific monitor by index (0 = primary).
    Monitor { index: u32 },
    /// Capture a specific window by its native HWND value.
    Window { hwnd: isize },
}

/// Which API was actually used for capture.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CaptureMethod {
    /// Windows Graphics Capture — zero-copy shared texture, HDR→SDR auto.
    WindowsGraphicsCapture,
    /// DXGI Desktop Duplication — requires DXGI 1.2, slightly higher latency.
    DxgiDesktopDuplication,
    // GDI is intentionally absent — forbidden per PRD §5.1.
}

/// A running capture session.  Drop to stop capture.
pub struct CaptureSession {
    pub method: CaptureMethod,
    pub width: u32,
    pub height: u32,
    // internal stop signal — pub(crate) so capture_win.rs can construct the struct
    pub(crate) _stop_tx: tokio::sync::oneshot::Sender<()>,
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        // _stop_tx is consumed on drop, which closes the channel and signals
        // the capture thread to exit.
    }
}

/// Start capturing `target`.  Returns the active session and a frame receiver.
/// The receiver produces up to 60 frames/s.  The session must be kept alive;
/// dropping it stops capture and closes the channel.
pub async fn start_capture(
    target: CaptureTarget,
    fps_limit: u32,
) -> Result<(CaptureSession, mpsc::Receiver<VideoFrame>)> {
    // Channel with a small backpressure buffer.  If the encoder falls behind
    // we drop frames rather than grow memory unboundedly.
    let (frame_tx, frame_rx) = mpsc::channel::<VideoFrame>(4);

    #[cfg(target_os = "windows")]
    {
        use crate::capture_win::{try_wgc, try_dxgi};

        // Try WGC first (§5.1 priority 1).
        // Each try_* function creates its own stop channel and stores the real
        // stop_tx inside the returned CaptureSession.
        match try_wgc(target.clone(), fps_limit, frame_tx.clone()).await {
            Ok(session) => return Ok((session, frame_rx)),
            Err(wgc_err) => {
                log::warn!("WGC failed ({wgc_err}), falling back to DXGI");
                match try_dxgi(target, fps_limit, frame_tx).await {
                    Ok(session) => return Ok((session, frame_rx)),
                    Err(dxgi_err) => {
                        return Err(anyhow!(
                            "All capture methods failed. WGC: {wgc_err}. DXGI: {dxgi_err}"
                        ));
                    }
                }
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        // On non-Windows (dev machine / CI) run a software fallback that
        // generates synthetic frames so the pipeline can be tested end-to-end.
        log::warn!("Non-Windows platform: using synthetic test frames");
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            use std::time::{Duration, Instant};
            let frame_us = 1_000_000u64 / fps_limit.max(1) as u64;
            let start = Instant::now();

            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    _ = tokio::time::sleep(Duration::from_micros(frame_us)) => {}
                }

                let w: u32 = 1920;
                let h: u32 = 1080;
                let pts_us = start.elapsed().as_micros() as u64;

                // Generate a simple animated test pattern (grey gradient + timestamp bar)
                let mut pixels = vec![0u8; (w * h * 4) as usize];
                let t = (pts_us / 16_667) as u8; // frame counter modulo 256
                for y in 0..h {
                    for x in 0..w {
                        let idx = ((y * w + x) * 4) as usize;
                        let r = ((x * 255 / w) as u8).wrapping_add(t);
                        let g = ((y * 255 / h) as u8).wrapping_add(t / 2);
                        let b = t;
                        pixels[idx]     = b; // B
                        pixels[idx + 1] = g; // G
                        pixels[idx + 2] = r; // R
                        pixels[idx + 3] = 255; // A
                    }
                }

                let frame = VideoFrame {
                    data: Arc::new(pixels),
                    width: w,
                    height: h,
                    pts_us,
                };

                if frame_tx.send(frame).await.is_err() {
                    break; // receiver dropped
                }
            }
        });

        let session = CaptureSession {
            method: CaptureMethod::WindowsGraphicsCapture, // reported as WGC even for stub
            width: 1920,
            height: 1080,
            _stop_tx: stop_tx,
        };
        return Ok((session, frame_rx));
    }
}

/// Enumerate capturable windows/monitors for the UI picker.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CaptureSource {
    pub id: String,
    pub name: String,
    pub kind: String, // "monitor" | "window"
    pub hwnd: Option<isize>,
    pub monitor_index: Option<u32>,
    pub width: u32,
    pub height: u32,
    pub thumbnail_b64: Option<String>, // base64 PNG thumbnail, optional
}

pub fn enumerate_sources() -> Vec<CaptureSource> {
    #[cfg(target_os = "windows")]
    {
        crate::capture_win::enumerate_sources_win()
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Stub for development on non-Windows
        vec![
            CaptureSource {
                id: "monitor:0".into(),
                name: "Primary Monitor (1920×1080)".into(),
                kind: "monitor".into(),
                hwnd: None,
                monitor_index: Some(0),
                width: 1920,
                height: 1080,
                thumbnail_b64: None,
            },
            CaptureSource {
                id: "window:fake1".into(),
                name: "Test Window — Synthetic".into(),
                kind: "window".into(),
                hwnd: None,
                monitor_index: None,
                width: 1280,
                height: 720,
                thumbnail_b64: None,
            },
        ]
    }
}
