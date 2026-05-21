// audio.rs
// Desktop audio capture (WASAPI loopback) + Opus encoding.
// PRD §5.3: 48kHz/16-bit stereo, Opus 128kbps, 20ms frames,
//           OPUS_APPLICATION_RESTRICTED_LOWDELAY.

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Encoded Opus audio packet.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub data: Arc<Vec<u8>>,
    pub pts_us: u64,
    /// Duration of this packet in microseconds (normally 20_000).
    pub duration_us: u64,
}

/// Start WASAPI loopback capture + Opus encoding.
/// Returns a receiver of encoded packets.
pub async fn start_audio_capture() -> Result<mpsc::Receiver<AudioPacket>> {
    let (tx, rx) = mpsc::channel::<AudioPacket>(16);

    #[cfg(target_os = "windows")]
    {
        crate::audio_win::start_wasapi_capture(tx).await?;
    }

    #[cfg(not(target_os = "windows"))]
    {
        log::warn!("Non-Windows: using silent stub audio");
        tokio::spawn(async move {
            use std::time::Duration;
            let mut pts = 0u64;
            loop {
                tokio::time::sleep(Duration::from_millis(20)).await;
                // Emit silent Opus packets so the pipeline stays live
                let silence = vec![0xF8, 0xFF, 0xFE]; // Opus comfort noise
                if tx.send(AudioPacket {
                    data: Arc::new(silence),
                    pts_us: pts,
                    duration_us: 20_000,
                }).await.is_err() { break; }
                pts += 20_000;
            }
        });
    }

    Ok(rx)
}
