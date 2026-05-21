// audio_win.rs
// WASAPI loopback audio capture + Opus encoding for Windows.
// Captures the system audio mix (what you hear) at 48kHz stereo,
// encodes to Opus 128kbps with RESTRICTED_LOWDELAY application mode.
//
// Uses `unsafe-libopus` (a pure-Rust c2rust transpilation of libopus) so no
// CMake / C build toolchain is required on the developer's machine.

#![cfg(target_os = "windows")]

use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use tokio::sync::mpsc;
use unsafe_libopus::{
    opus_encode_float, opus_encoder_create, opus_encoder_ctl_impl, opus_encoder_destroy,
    varargs, OPUS_APPLICATION_RESTRICTED_LOWDELAY, OPUS_OK, OPUS_SET_BITRATE_REQUEST,
    OpusEncoder,
};
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Media::{
            Audio::{
                eConsole, eRender,
                IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
                MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
            },
            // WAVE_FORMAT_EXTENSIBLE lives in KernelStreaming in windows 0.58
            KernelStreaming::WAVE_FORMAT_EXTENSIBLE,
        },
        System::{
            Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED},
            Threading::{CreateEventW, WaitForSingleObject, INFINITE},
        },
    },
};

use crate::audio::AudioPacket;

const TARGET_SAMPLE_RATE: u32 = 48_000;
const TARGET_CHANNELS: u16 = 2;
const OPUS_BITRATE_BPS: i32 = 128_000; // 128 kbps
const FRAME_DURATION_MS: u32 = 20;
const SAMPLES_PER_FRAME: usize =
    (TARGET_SAMPLE_RATE as usize * FRAME_DURATION_MS as usize) / 1000; // 960

/// RAII wrapper around a raw `*mut OpusEncoder` so it is always freed even on
/// early return from the capture thread.
struct RawOpusEncoder(*mut OpusEncoder);

impl RawOpusEncoder {
    fn new(sample_rate: i32, channels: i32, application: i32) -> Result<Self> {
        let mut error: i32 = 0;
        let enc = unsafe { opus_encoder_create(sample_rate, channels, application, &mut error) };
        if error != OPUS_OK || enc.is_null() {
            return Err(anyhow!("opus_encoder_create failed with code {error}"));
        }
        Ok(Self(enc))
    }

    fn set_bitrate(&self, bps: i32) -> Result<()> {
        let ret = unsafe {
            opus_encoder_ctl_impl(self.0, OPUS_SET_BITRATE_REQUEST, varargs!(bps))
        };
        if ret != OPUS_OK {
            return Err(anyhow!("OPUS_SET_BITRATE failed with code {ret}"));
        }
        Ok(())
    }

    /// Encode one frame of interleaved float32 PCM (range -1.0 … 1.0).
    /// Returns the number of bytes written to `out`, or 0 if the frame was
    /// encoded but contained no data (DTX silence).
    fn encode_float(&self, pcm: &[f32], out: &mut [u8]) -> Result<usize> {
        let frame_size = SAMPLES_PER_FRAME as i32; // 960 samples @ 48kHz
        let ret = unsafe {
            opus_encode_float(
                self.0,
                pcm.as_ptr(),
                frame_size,
                out.as_mut_ptr(),
                out.len() as i32,
            )
        };
        if ret < 0 {
            Err(anyhow!("opus_encode_float error {ret}"))
        } else {
            Ok(ret as usize)
        }
    }
}

impl Drop for RawOpusEncoder {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { opus_encoder_destroy(self.0) };
        }
    }
}

// SAFETY: the encoder pointer is used only inside a single thread.
unsafe impl Send for RawOpusEncoder {}

pub async fn start_wasapi_capture(tx: mpsc::Sender<AudioPacket>) -> Result<()> {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<()>>();

    std::thread::spawn(move || {
        if let Err(e) = wasapi_thread(tx, ready_tx) {
            log::error!("WASAPI thread error: {e}");
        }
    });

    ready_rx
        .await
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

        // Check if native format is 48kHz stereo float (most common).
        // If not, request it explicitly and let WASAPI resample.
        let (wave_fmt, _needs_resample) = if mix_fmt.nSamplesPerSec == TARGET_SAMPLE_RATE
            && mix_fmt.nChannels == TARGET_CHANNELS
        {
            (mix_fmt as *const _ as *const WAVEFORMATEX, false)
        } else {
            // Copy packed-struct fields to locals before passing to the log macro
            // to avoid unaligned reference UB (WAVEFORMATEX is 1-byte aligned).
            let native_rate = mix_fmt.nSamplesPerSec;
            let native_ch = mix_fmt.nChannels;
            log::warn!(
                "Native format {}Hz {}ch — requesting {}Hz {}ch with WASAPI resampling",
                native_rate,
                native_ch,
                TARGET_SAMPLE_RATE,
                TARGET_CHANNELS
            );
            let fmt = build_f32_wave_format();
            (&fmt as *const _ as *const WAVEFORMATEX, true)
        };

        // Buffer duration: 200ms (actual latency is 1 frame = 20ms)
        let buffer_duration_100ns: i64 = 2_000_000;

        audio_client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                buffer_duration_100ns,
                0,
                wave_fmt,
                None,
            )
            .context("AudioClient Initialize")?;

        let event = CreateEventW(None, false, false, None).context("CreateEventW")?;
        audio_client
            .SetEventHandle(event)
            .context("SetEventHandle")?;

        let capture_client: IAudioCaptureClient =
            audio_client.GetService().context("GetService IAudioCaptureClient")?;

        audio_client.Start().context("AudioClient Start")?;

        // Initialise Opus encoder (RAII — destroyed automatically on return)
        let opus = RawOpusEncoder::new(
            TARGET_SAMPLE_RATE as i32,
            TARGET_CHANNELS as i32,
            OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        )?;
        opus.set_bitrate(OPUS_BITRATE_BPS)?;

        // Signal ready
        let _ = ready_tx.send(Ok(()));

        // Sample accumulator — collect until we have a full 20ms frame
        let mut sample_buf: Vec<f32> =
            Vec::with_capacity(SAMPLES_PER_FRAME * TARGET_CHANNELS as usize * 4);
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

                match capture_client.GetBuffer(
                    &mut data_ptr,
                    &mut num_frames,
                    &mut flags,
                    Some(&mut 0u64),
                    Some(&mut 0u64),
                ) {
                    Ok(()) if num_frames == 0 => break,
                    Ok(()) => {}
                    Err(_) => break,
                }

                let samples_count = num_frames as usize * TARGET_CHANNELS as usize;
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
        0x00000003,
        0x0000,
        0x0010,
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
            cbSize: (std::mem::size_of::<WAVEFORMATEXTENSIBLE>()
                - std::mem::size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: 32,
        },
        dwChannelMask: 0x3, // SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
        SubFormat: subtype,
    }
}
