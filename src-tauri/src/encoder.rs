// encoder.rs
// Hardware H.264 encoder abstraction.
// Priority: NVENC → AMF → Intel QuickSync → error (software encoding forbidden).
//
// On Windows we use Media Foundation's hardware-accelerated H.264 encoder
// (MFT), which transparently dispatches to NVENC, AMF, or QuickSync
// depending on what the driver exposes.
//
// On non-Windows a stub encoder produces synthetic NAL units for pipeline testing.

use anyhow::{anyhow, Result};
use std::sync::Arc;
use crate::capture::VideoFrame;

/// Encoded video packet (H.264 NAL unit stream).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EncodedPacket {
    /// H.264 Annex-B byte stream (starts with 0x00 0x00 0x00 0x01).
    pub data: Arc<Vec<u8>>,
    /// Presentation timestamp in microseconds (from capture PTS).
    pub pts_us: u64,
    /// True if this packet contains an IDR (keyframe).
    pub is_keyframe: bool,
}

/// Which hardware encoder is in use.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum EncoderKind {
    Nvenc,
    Amf,
    QuickSync,
    /// Stub used on non-Windows platforms during development.
    Stub,
}

impl std::fmt::Display for EncoderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            EncoderKind::Nvenc => write!(f, "NVENC H264"),
            EncoderKind::Amf => write!(f, "AMF H264"),
            EncoderKind::QuickSync => write!(f, "QuickSync H264"),
            EncoderKind::Stub => write!(f, "Stub (test)"),
        }
    }
}

/// Encoder configuration (matches PRD §5.2 required settings).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Target bitrate in bits/second.  PRD: 15–30 Mbps.
    pub bitrate_bps: u32,
    /// Max bitrate.  PRD: 35 Mbps.
    pub max_bitrate_bps: u32,
    /// Keyframe interval in seconds.  PRD: 2s.
    pub keyframe_interval_secs: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 20_000_000,       // 20 Mbps default
            max_bitrate_bps: 35_000_000,   // 35 Mbps hard cap
            keyframe_interval_secs: 2,
        }
    }
}

/// Active encoder instance.
pub struct Encoder {
    pub kind: EncoderKind,
    inner: EncoderInner,
}

// SAFETY: Windows hardware MFTs (NVENC, AMF, QuickSync) are free-threaded COM
// objects — they do not have STA apartment affinity and can be safely moved
// between threads.  The windows-rs crate conservatively omits the Send impl
// for all COM interfaces, so we opt back in here.
#[cfg(target_os = "windows")]
unsafe impl Send for Encoder {}

#[allow(dead_code)]
enum EncoderInner {
    #[cfg(target_os = "windows")]
    MediaFoundation(mf_encoder::MfEncoder),
    Stub(stub_encoder::StubEncoder),
}

impl Encoder {
    /// Detect and initialise the best available hardware encoder.
    /// Returns an error if no hardware encoder is available (software encoding
    /// is explicitly forbidden per PRD §5.2).
    pub fn new(config: EncoderConfig) -> Result<Self> {
        #[cfg(target_os = "windows")]
        {
            match mf_encoder::MfEncoder::new(config) {
                Ok((enc, kind)) => return Ok(Self { kind, inner: EncoderInner::MediaFoundation(enc) }),
                Err(e) => return Err(anyhow!(
                    "No compatible hardware encoder found. NVENC/AMF/QuickSync required.\nDetail: {e}"
                )),
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            log::warn!("Non-Windows: using stub encoder (no hardware available)");
            Ok(Self {
                kind: EncoderKind::Stub,
                inner: EncoderInner::Stub(stub_encoder::StubEncoder::new(config)),
            })
        }
    }

    /// Encode a single video frame.  Returns zero or more packets
    /// (the encoder may buffer internally).
    pub fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<EncodedPacket>> {
        match &mut self.inner {
            #[cfg(target_os = "windows")]
            EncoderInner::MediaFoundation(enc) => enc.encode(frame),
            EncoderInner::Stub(enc) => enc.encode(frame),
        }
    }

    /// Flush remaining frames out of the encoder (call on session end).
    #[allow(dead_code)]
    pub fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        match &mut self.inner {
            #[cfg(target_os = "windows")]
            EncoderInner::MediaFoundation(enc) => enc.flush(),
            EncoderInner::Stub(enc) => enc.flush(),
        }
    }
}

// ─── Windows Media Foundation Encoder ────────────────────────────────────────

#[cfg(target_os = "windows")]
mod mf_encoder {
    use super::*;
    use anyhow::Context;
    use windows::{
        // Avoid glob-importing windows_core::Result, which would clash with anyhow::Result
        // brought in by `use super::*`.  Import specific items instead.
        core::Interface,
        Win32::Media::MediaFoundation::*,
        Win32::System::Com::*,
    };

    // GetEvent flag: blocking (0) vs. non-blocking (1).
    // The parameter type in windows 0.58 is MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS.
    const GET_EVENT_BLOCK:    MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS =
        MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0);
    const GET_EVENT_NO_WAIT:  MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS =
        MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(1); // MF_EVENT_FLAG_NO_WAIT
    // HRESULT returned when the event queue is empty (no-wait mode).
    // The correct value per mferror.h is 0xC00D3E80 — the old value
    // (0xC00D36B1) is actually MF_E_BUFFERTOOSMALL and never matched, so the
    // benign "no more events" condition was propagated as a real encoder error.
    const MF_E_NO_EVENTS_AVAILABLE: u32 = 0xC00D_3E80;
    // GetType() returns u32 in windows 0.58; MF_EVENT_TYPE.0 is i32 — cast to match.
    const EVT_NEED_INPUT:     u32 = METransformNeedInput.0    as u32; // 600
    const EVT_HAVE_OUTPUT:    u32 = METransformHaveOutput.0   as u32; // 601
    const EVT_DRAIN_COMPLETE: u32 = METransformDrainComplete.0 as u32; // 602

    pub struct MfEncoder {
        transform: IMFTransform,
        /// Async event generator — hardware MFTs are always asynchronous.
        event_gen: IMFMediaEventGenerator,
        config: EncoderConfig,
        sample_count: u64,
        mf_started: bool,
        /// Counts buffered METransformNeedInput events not yet consumed by encode().
        /// Avoids re-blocking when the MFT fires NeedInput early (pipeline filling).
        pending_need_input: u32,
    }

    impl Drop for MfEncoder {
        fn drop(&mut self) {
            if self.mf_started {
                unsafe { let _ = MFShutdown(); }
            }
        }
    }

    impl MfEncoder {
        pub fn new(config: EncoderConfig) -> Result<(Self, EncoderKind)> {
            unsafe {
                // Initialise Media Foundation
                MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
                    .context("MFStartup")?;

                // Enumerate hardware H.264 encoders
                let mut count = 0u32;
                let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();

                let flags: MFT_ENUM_FLAG = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;

                let input_type_info = MFT_REGISTER_TYPE_INFO {
                    guidMajorType: MFMediaType_Video,
                    guidSubtype: MFVideoFormat_NV12,
                };
                let output_type_info = MFT_REGISTER_TYPE_INFO {
                    guidMajorType: MFMediaType_Video,
                    guidSubtype: MFVideoFormat_H264,
                };

                let hr = MFTEnumEx(
                    MFT_CATEGORY_VIDEO_ENCODER,
                    flags,
                    Some(&input_type_info),
                    Some(&output_type_info),
                    &mut activates,
                    &mut count,
                );

                if hr.is_err() || count == 0 {
                    let _ = MFShutdown();
                    return Err(anyhow!("No hardware H.264 MFT encoders found (count={})", count));
                }

                let activate_slice = std::slice::from_raw_parts(activates, count as usize);
                let activate = activate_slice[0]
                    .as_ref()
                    .ok_or_else(|| anyhow!("Null IMFActivate"))?
                    .clone();

                // Determine which encoder vendor this is.
                // GetStringLength in windows 0.58 takes only the GUID and returns Result<u32>.
                let kind = if let Ok(name_len) = activate.GetStringLength(&MFT_FRIENDLY_NAME_Attribute) {
                    let mut name_len = name_len; // make mutable for the GetString call below
                    let mut name_buf = vec![0u16; name_len as usize + 1];
                    let _ = activate.GetString(
                        &MFT_FRIENDLY_NAME_Attribute,
                        &mut name_buf,
                        Some(&mut name_len),
                    );
                    let name = String::from_utf16_lossy(&name_buf[..name_len as usize]).to_lowercase();
                    if name.contains("nvidia") || name.contains("nvenc") {
                        EncoderKind::Nvenc
                    } else if name.contains("amd") || name.contains("amf") || name.contains("advanced micro") {
                        EncoderKind::Amf
                    } else if name.contains("intel") || name.contains("quick") {
                        EncoderKind::QuickSync
                    } else {
                        EncoderKind::QuickSync // unknown — treat as QuickSync
                    }
                } else {
                    EncoderKind::QuickSync
                };

                let transform: IMFTransform = activate.ActivateObject()?;

                // Free the activate array
                CoTaskMemFree(Some(activates as *mut _));

                // ── Async unlock ────────────────────────────────────────────────
                // Hardware MFTs are *asynchronous* COM objects.  Before calling
                // SetOutputType / SetInputType we must set MF_TRANSFORM_ASYNC_UNLOCK
                // on the transform's attribute store, otherwise every subsequent
                // call returns MF_E_TRANSFORM_ASYNC_LOCKED (0xC00D6D77).
                let mft_attrs: IMFAttributes = transform.GetAttributes()
                    .context("IMFTransform::GetAttributes")?;
                mft_attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .context("MF_TRANSFORM_ASYNC_UNLOCK")?;

                // Get the event generator used to drive the async encode loop.
                let event_gen: IMFMediaEventGenerator = transform.cast()
                    .context("IMFMediaEventGenerator — MFT does not expose async events")?;

                // Configure output type (H.264)
                let output_mt: IMFMediaType = MFCreateMediaType()?;
                output_mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                output_mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
                output_mt.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_bps)?;
                output_mt.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    pack_u32_u32(config.width, config.height),
                )?;
                output_mt.SetUINT64(
                    &MF_MT_FRAME_RATE,
                    pack_u32_u32(config.fps, 1),
                )?;
                output_mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                output_mt.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
                transform.SetOutputType(0, &output_mt, 0)?;

                // Configure input type (NV12 — native GPU format)
                let input_mt: IMFMediaType = MFCreateMediaType()?;
                input_mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                input_mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
                input_mt.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    pack_u32_u32(config.width, config.height),
                )?;
                input_mt.SetUINT64(
                    &MF_MT_FRAME_RATE,
                    pack_u32_u32(config.fps, 1),
                )?;
                input_mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                transform.SetInputType(0, &input_mt, 0)?;

                // Set CBR via codec API
                if let Ok(codec_api) = transform.cast::<ICodecAPI>() {
                    let cbr_variant = com_variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32);
                    let _ = codec_api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &cbr_variant);
                    let bframes = com_variant_u32(0); // no B-frames (PRD requirement)
                    let _ = codec_api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &bframes);
                    let gop = com_variant_u32(config.fps * config.keyframe_interval_secs);
                    let _ = codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &gop);
                    let low_latency = com_variant_u32(1);
                    let _ = codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &low_latency);
                }

                transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
                transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

                log::info!("MF encoder initialised: {:?} {}×{}@{}fps {}Mbps",
                    kind, config.width, config.height, config.fps,
                    config.bitrate_bps / 1_000_000);

                Ok((MfEncoder {
                    transform,
                    event_gen,
                    config,
                    sample_count: 0,
                    mf_started: true,
                    pending_need_input: 0,
                }, kind))
            }
        }

        pub fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<EncodedPacket>> {
            unsafe {
                let sample = self.build_sample(frame)?;
                let mut packets = Vec::new();

                // ── Step 1: obtain a METransformNeedInput slot ──────────────────
                // Hardware async MFTs fire METransformNeedInput to signal they can
                // accept input.  We may already have a buffered one from the
                // previous frame's output-drain phase.
                if self.pending_need_input > 0 {
                    self.pending_need_input -= 1;
                } else {
                    // Block until NeedInput arrives; collect any stray output first.
                    loop {
                        let ev = self.event_gen.GetEvent(GET_EVENT_BLOCK)
                            .context("GetEvent (await NeedInput)")?;
                        match ev.GetType()? {
                            EVT_NEED_INPUT => break,
                            EVT_HAVE_OUTPUT => {
                                if let Some(pkt) = self.try_process_output(0)? {
                                    packets.push(pkt);
                                }
                            }
                            _ => {}
                        }
                    }
                }

                // ── Step 2: submit the frame ────────────────────────────────────
                self.transform.ProcessInput(0, &sample, 0)?;
                self.sample_count += 1;

                // ── Step 3: non-blocking drain ──────────────────────────────────
                // With low-latency + no-B-frames settings, HaveOutput typically
                // follows immediately.  NeedInput events for the *next* frame are
                // buffered so encode() can skip the blocking wait next call.
                loop {
                    match self.event_gen.GetEvent(GET_EVENT_NO_WAIT) {
                        Err(e) if e.code().0 as u32 == MF_E_NO_EVENTS_AVAILABLE => break,
                        Err(e) => return Err(anyhow!("GetEvent (drain): {e}")),
                        Ok(ev) => match ev.GetType()? {
                            EVT_HAVE_OUTPUT => {
                                if let Some(pkt) = self.try_process_output(frame.pts_us)? {
                                    packets.push(pkt);
                                }
                            }
                            EVT_NEED_INPUT => {
                                self.pending_need_input += 1;
                            }
                            _ => {}
                        }
                    }
                }

                Ok(packets)
            }
        }

        #[allow(dead_code)]
        pub fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
            unsafe {
                self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
                self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;

                let mut packets = Vec::new();
                loop {
                    let ev = self.event_gen.GetEvent(GET_EVENT_BLOCK)
                        .context("GetEvent (flush)")?;
                    match ev.GetType()? {
                        EVT_HAVE_OUTPUT => {
                            if let Some(pkt) = self.try_process_output(0)? {
                                packets.push(pkt);
                            }
                        }
                        EVT_DRAIN_COMPLETE => break,
                        _ => {}
                    }
                }
                Ok(packets)
            }
        }

        /// Build an IMFSample from a captured VideoFrame (BGRA → NV12).
        unsafe fn build_sample(&self, frame: &VideoFrame) -> Result<IMFSample> {
            let nv12 = bgra_to_nv12(&frame.data, frame.width, frame.height);

            let sample: IMFSample = MFCreateSample()?;
            let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(nv12.len() as u32)?;

            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut _max = 0u32;
            let mut _cur = 0u32;
            buffer.Lock(&mut ptr, Some(&mut _max), Some(&mut _cur))?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(nv12.len() as u32)?;
            sample.AddBuffer(&buffer)?;

            let pts_100ns = (frame.pts_us * 10) as i64;
            sample.SetSampleTime(pts_100ns)?;
            let duration_100ns = 10_000_000i64 / self.config.fps as i64;
            sample.SetSampleDuration(duration_100ns)?;

            Ok(sample)
        }

        /// Call ProcessOutput once and return the resulting packet (if any).
        /// Called after a METransformHaveOutput event is received.
        unsafe fn try_process_output(&mut self, pts_hint: u64) -> Result<Option<EncodedPacket>> {
            let output_data = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut buffers = [output_data];
            let mut status = 0u32;

            match self.transform.ProcessOutput(0, &mut buffers, &mut status) {
                Ok(()) => {}
                Err(e) if e.code().0 as u32 == 0xC00D_6D72 /* MF_E_TRANSFORM_NEED_MORE_INPUT */ => {
                    return Ok(None);
                }
                Err(e) => return Err(anyhow!("ProcessOutput: {e}")),
            }

            let sample_opt = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
            if let Some(sample) = sample_opt {
                let pts_us = sample.GetSampleTime()
                    .map(|t| t as u64 / 10)
                    .unwrap_or(pts_hint);

                let buffer = sample.ConvertToContiguousBuffer()?;
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut _max = 0u32;
                let mut len = 0u32;
                buffer.Lock(&mut ptr, Some(&mut _max), Some(&mut len))?;
                let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
                buffer.Unlock()?;

                Ok(Some(EncodedPacket {
                    is_keyframe: is_idr_nal(&data),
                    data: Arc::new(data),
                    pts_us,
                }))
            } else {
                Ok(None)
            }
        }
    }

    fn pack_u32_u32(hi: u32, lo: u32) -> u64 {
        ((hi as u64) << 32) | lo as u64
    }

    fn com_variant_u32(val: u32) -> windows::core::VARIANT {
        // windows::core::VARIANT has From<u32> that sets VT_UI4 correctly.
        windows::core::VARIANT::from(val)
    }

    fn is_idr_nal(data: &[u8]) -> bool {
        let mut i = 0;
        while i + 4 < data.len() {
            if data[i..i+4] == [0, 0, 0, 1] {
                let nal_type = data[i + 4] & 0x1F;
                if nal_type == 5 { return true; } // IDR slice
                i += 4;
            } else {
                i += 1;
            }
        }
        false
    }

    /// BGRA → NV12 color space conversion (CPU — for staging path).
    /// In the zero-copy path the GPU handles this via shader; this is the
    /// fallback for the staging-readback path.
    fn bgra_to_nv12(bgra: &[u8], width: u32, height: u32) -> Vec<u8> {
        let w = width as usize;
        let h = height as usize;
        let mut nv12 = vec![0u8; w * h * 3 / 2];

        // Y plane
        for y in 0..h {
            for x in 0..w {
                let src = (y * w + x) * 4;
                let b = bgra[src] as i32;
                let g = bgra[src + 1] as i32;
                let r = bgra[src + 2] as i32;
                let yv = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                nv12[y * w + x] = yv.clamp(16, 235) as u8;
            }
        }

        // UV plane (interleaved, half-resolution)
        let uv_offset = w * h;
        for y in (0..h).step_by(2) {
            for x in (0..w).step_by(2) {
                let src = (y * w + x) * 4;
                let b = bgra[src] as i32;
                let g = bgra[src + 1] as i32;
                let r = bgra[src + 2] as i32;
                let uv = uv_offset + (y / 2) * w + x;
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                nv12[uv]     = u.clamp(16, 240) as u8;
                nv12[uv + 1] = v.clamp(16, 240) as u8;
            }
        }

        nv12
    }
}

// ─── Stub Encoder (non-Windows / testing) ────────────────────────────────────

#[allow(dead_code)]
mod stub_encoder {
    use super::*;

    pub struct StubEncoder {
        config: EncoderConfig,
        frame_count: u64,
    }

    impl StubEncoder {
        pub fn new(config: EncoderConfig) -> Self {
            Self { config, frame_count: 0 }
        }

        pub fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<EncodedPacket>> {
            self.frame_count += 1;
            // Emit a fake IDR every keyframe_interval_secs seconds
            let kf_interval = self.config.fps as u64 * self.config.keyframe_interval_secs as u64;
            let is_keyframe = self.frame_count % kf_interval == 1;

            // Fake Annex-B NAL header so downstream can parse it
            let nal_header = vec![0x00, 0x00, 0x00, 0x01, if is_keyframe { 0x65 } else { 0x41 }];
            Ok(vec![EncodedPacket {
                data: Arc::new(nal_header),
                pts_us: frame.pts_us,
                is_keyframe,
            }])
        }

        pub fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
            Ok(vec![])
        }
    }
}
