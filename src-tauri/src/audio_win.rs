// audio_win.rs
// WASAPI loopback audio capture + Opus encoding for Windows.
// Captures the system audio mix (what you hear) at 48kHz stereo,
// encodes to Opus 128kbps with RESTRICTED_LOWDELAY application mode.

#![cfg(target_os = "windows")]

use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use tokio::sync::mpsc;
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Media::Audio::{
            eConsole, eRender,
            IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
            MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
            WAVE_FORMAT_EXTENSIBLE,
        },
        System::{
            Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED},
            Threading::{CreateEventW, WaitForSingleObject, INFINITE},
        },
    },
};
use audiopus::{coder::Encoder as OpusEncoder, Application, Channels, SampleRate};

use crate::audio::AudioPacket;

const TARGET_SAMPLE_RATE: u32 = 48_000;
const TARGET_CHANNELS: u16 = 2;
const OPUS_BITRATE_KBPS: i32 = 128;
const FRAME_DURATION_MS: u32 = 20;
const SAMPLES_PER_FRAME: usize = (TARGET_SAMPLE_RATE as usize * FRAME_DURATION_MS as usize) / 1000; // 960

pub async fn start_wasapi_capture(tx: mpsc::Sender<AudioPacket>) -> Result<()> {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();

    std::thread::spawn(move || {
        if let Err(e) = wasapi_thread(tx, ready_tx) {
            log::error!("WASAPI thread error: {e}");
        }
    });

    ready_rx.await
        .map_err(|_| anyhow!("WASAPI thread panicked before signalling ready"))?
}

fn wasapi_thread(
    tx: mpsc::Sender<AudioPacket>,
    ready_tx: tokio::sync::oneshot::Sender<Result<()>>,
) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();

        // Get default render (output) device for loopback
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("CoCreateInstance MMDeviceEnumerator")?;

        let device: IMMDevice = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("GetDefaultAudioEndpoint")?;

        let audio_client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .context("IAudioClient Activate")?;

        // Get mix format (what the device is using natively)
        let mix_fmt_ptr = audio_client.GetMixFormat().context("GetMixFormat")?;
        let mix_fmt = &*mix_fmt_ptr;

        // Check if native format is 48kHz stereo float (most common)
        // If not, we request it explicitly and let WASAPI resample.
        let (wave_fmt, needs_resample) = if mix_fmt.nSamplesPerSec == TARGET_SAMPLE_RATE
            && mix_fmt.nChannels == TARGET_CHANNELS
        {
            (mix_fmt as *const _ as *const WAVEFORMATEX, false)
        } else {
            log::warn!(
                "Native format {}Hz {}ch — requesting {}Hz {}ch with WASAPI resampling",
                mix_fmt.nSamplesPerSec, mix_fmt.nChannels,
                TARGET_SAMPLE_RATE, TARGET_CHANNELS
            );
            // Build a WAVEFORMATEXTENSIBLE for 48kHz stereo float32
            // (stack-allocated; pointer valid for the duration of Initialize)
            let fmt = build_f32_wave_format();
            (&fmt as *const _ as *const WAVEFORMATEX, true)
        };

        // Buffer duration: 200ms (enough headroom; actual latency is 1 frame = 20ms)
        let buffer_duration_100ns: i64 = 2_000_000; // 200ms in 100ns units

        audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            buffer_duration_100ns,
            0,
            wave_fmt,
            None,
        ).context("AudioClient Initialize")?;

        // Event-driven mode — create event handle
        let event = CreateEventW(None, false, false, None)
            .context("CreateEventW")?;
        audio_client.SetEventHandle(event).context("SetEventHandle")?;

        let capture_client: IAudioCaptureClient = audio_client
            .GetService()
            .context("GetService IAudioCaptureClient")?;

        audio_client.Start().context("AudioClient Start")?;

        // Initialise Opus encoder
        let mut opus = OpusEncoder::new(
            SampleRate::Hz48000,
            Channels::Stereo,
            Application::LowDelay,
        ).map_err(|e| anyhow!("Opus encoder init: {e}"))?;

        opus.set_bitrate(audiopus::Bitrate::BitsPerSecond(OPUS_BITRATE_KBPS * 1000))
            .map_err(|e| anyhow!("Opus set_bitrate: {e}"))?;

        // Signal ready
        let _ = ready_tx.send(Ok(()));

        // Sample accumulator — collect until we have a full 20ms frame
        let mut sample_buf: Vec<f32> = Vec::with_capacity(SAMPLES_PER_FRAME * TARGET_CHANNELS as usize * 4);
        let mut pts_us: u64 = 0;
        let mut opus_out = vec![0u8; 4096];

        loop {
            // Wait for audio data event (timeout 500ms to allow clean shutdown)
            let wait_result = WaitForSingleObject(event, 500);
            if wait_result.0 != 0 {
                continue; // timeout or error — loop again
            }

            // Drain all available packets from the capture client
            loop {
                let mut data_ptr: *mut u8 = std::ptr::null_mut();
                let mut num_frames = 0u32;
                let mut flags = 0u32;
                let mut qpc_position = 0u64;

                match capture_client.GetBuffer(
                    &mut data_ptr,
                    &mut num_frames,
                    &mut flags,
                    Some(&mut 0u64),
                    Some(&mut qpc_position),
                ) {
                    Ok(()) if num_frames == 0 => break,
                    Ok(()) => {}
                    Err(_) => break,
                }

                let samples_count = num_frames as usize * TARGET_CHANNELS as usize;
                let byte_count = samples_count * std::mem::size_of::<f32>();
                let raw = std::slice::from_raw_parts(data_ptr as *const f32, samples_count);

                // AUDCLNT_BUFFERFLAGS_SILENT (0x2) — fill with silence
                if flags & 0x2 != 0 {
                    sample_buf.extend(std::iter::repeat(0.0f32).take(samples_count));
                } else {
                    sample_buf.extend_from_slice(raw);
                }

                capture_client.ReleaseBuffer(num_frames).ok();

                // Encode complete 20ms frames
                let frame_samples = SAMPLES_PER_FRAME * TARGET_CHANNELS as usize;
                while sample_buf.len() >= frame_samples {
                    let frame: Vec<f32> = sample_buf.drain(..frame_samples).collect();
                    match opus.encode_float(&frame, &mut opus_out) {
                        Ok(len) if len > 0 => {
                            let packet = AudioPacket {
                                data: Arc::new(opus_out[..len].to_vec()),
                                pts_us,
                                duration_us: FRAME_DURATION_MS as u64 * 1000,
                            };
                            if tx.blocking_send(packet).is_err() {
                                // Receiver dropped — exit cleanly
                                let _ = audio_client.Stop();
                                return Ok(());
                            }
                            pts_us += FRAME_DURATION_MS as u64 * 1000;
                        }
                        Ok(_) => {}
                        Err(e) => log::warn!("Opus encode error: {e}"),
                    }
                }
            }
        }
    }
}

fn build_f32_wave_format() -> WAVEFORMATEXTENSIBLE {
    // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    let subtype = windows::core::GUID::from_values(
        0x00000003, 0x0000, 0x0010,
        [0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71],
    );
    let block_align = TARGET_CHANNELS * 4; // 2ch * 4 bytes (float32)
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: TARGET_CHANNELS,
            nSamplesPerSec: TARGET_SAMPLE_RATE,
            nAvgBytesPerSec: TARGET_SAMPLE_RATE * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: 32,
            cbSize: (std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: 32,
        },
        dwChannelMask: 0x3, // SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
        SubFormat: subtype,
    }
}
