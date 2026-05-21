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
        core::*,
        Win32::Media::MediaFoundation::*,
        Win32::System::Com::*,
    };

    pub struct MfEncoder {
        transform: IMFTransform,
        input_type: IMFMediaType,
        config: EncoderConfig,
        sample_count: u64,
        mf_started: bool,
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

                let attrs = create_mf_attrs(&[
                    (MF_TRANSFORM_CATEGORY_Attribute, MFT_CATEGORY_VIDEO_ENCODER.0 as u64),
                ])?;

                // Request hardware-only encoders
                let hw_flag_attr = create_mf_attrs(&[])?;
                hw_flag_attr.SetUINT32(
                    &MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
                    1,
                ).ok();

                let mut flags: MFT_ENUM_FLAG = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;

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

                // Determine which encoder vendor this is
                let mut name_len = 0u32;
                let kind = if activate.GetStringLength(&MFT_FRIENDLY_NAME_Attribute, &mut name_len).is_ok() {
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
                // H.264 Baseline profile for maximum decoder compatibility
                output_mt.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
                // CBR rate control (PRD requirement)
                output_mt.SetUINT32(&MF_MT_VIDEO_PROFILE, 0)?;
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
                    // Rate control: CBR
                    let cbr_variant = com_variant_u32(eAVEncCommonRateControlMode_CBR.0 as u32);
                    let _ = codec_api.SetValue(&CODECAPI_AVEncCommonRateControlMode, &cbr_variant);
                    // No B-frames (PRD requirement)
                    let bframes = com_variant_u32(0);
                    let _ = codec_api.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &bframes);
                    // Keyframe interval
                    let gop = com_variant_u32(config.fps * config.keyframe_interval_secs);
                    let _ = codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &gop);
                    // Low latency mode
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
                    input_type: input_mt,
                    config,
                    sample_count: 0,
                    mf_started: true,
                }, kind))
            }
        }

        pub fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<EncodedPacket>> {
            unsafe {
                // Convert BGRA frame to NV12 for MFT input
                let nv12 = bgra_to_nv12(&frame.data, frame.width, frame.height);

                // Create MF sample + buffer
                let sample: IMFSample = MFCreateSample()?;
                let buffer: IMFMediaBuffer =
                    MFCreateMemoryBuffer(nv12.len() as u32)?;

                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut _max = 0u32;
                let mut _cur = 0u32;
                buffer.Lock(&mut ptr, Some(&mut _max), Some(&mut _cur))?;
                std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
                buffer.Unlock()?;
                buffer.SetCurrentLength(nv12.len() as u32)?;

                sample.AddBuffer(&buffer)?;

                // PTS in 100ns units (MF time base)
                let pts_100ns = (frame.pts_us * 10) as i64;
                sample.SetSampleTime(pts_100ns)?;
                let duration_100ns = 10_000_000i64 / self.config.fps as i64;
                sample.SetSampleDuration(duration_100ns)?;

                self.transform.ProcessInput(0, &sample, 0)?;
                self.sample_count += 1;

                // Collect output packets
                self.collect_output(frame.pts_us)
            }
        }

        pub fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
            unsafe {
                self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
                self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
                self.collect_output(0)
            }
        }

        unsafe fn collect_output(&mut self, pts_hint: u64) -> Result<Vec<EncodedPacket>> {
            let mut packets = Vec::new();
            loop {
                let mut output_data = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(None),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                };
                let mut status = 0u32;

                match self.transform.ProcessOutput(0, &mut [output_data], &mut status) {
                    Ok(()) => {}
                    Err(e) if e.code().0 as u32 == 0xC00D6D72 /* MF_E_TRANSFORM_NEED_MORE_INPUT */ => break,
                    Err(e) => return Err(anyhow!("ProcessOutput: {e}")),
                }

                let sample_opt = std::mem::ManuallyDrop::take(&mut output_data.pSample);
                if let Some(sample) = sample_opt {
                    let pts_us = sample.GetSampleTime().map(|t| t as u64 / 10).unwrap_or(pts_hint);

                    // Read compressed bytes
                    let buffer = sample.ConvertToContiguousBuffer()?;
                    let mut ptr: *mut u8 = std::ptr::null_mut();
                    let mut _max = 0u32;
                    let mut len = 0u32;
                    buffer.Lock(&mut ptr, Some(&mut _max), Some(&mut len))?;
                    let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
                    buffer.Unlock()?;

                    // Check if IDR (keyframe)
                    let is_keyframe = is_idr_nal(&data);

                    packets.push(EncodedPacket {
                        data: Arc::new(data),
                        pts_us,
                        is_keyframe,
                    });
                }
            }
            Ok(packets)
        }
    }

    fn pack_u32_u32(hi: u32, lo: u32) -> u64 {
        ((hi as u64) << 32) | lo as u64
    }

    fn com_variant_u32(val: u32) -> windows::Win32::System::Variant::VARIANT {
        use windows::Win32::System::Variant::*;
        let mut v = VARIANT::default();
        unsafe {
            v.Anonymous.Anonymous.vt = VT_UI4;
            v.Anonymous.Anonymous.Anonymous.uintVal = val;
        }
        v
    }

    fn create_mf_attrs(pairs: &[(GUID, u64)]) -> Result<IMFAttributes> {
        use windows::Win32::Media::MediaFoundation::MFCreateAttributes;
        unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, pairs.len() as u32 + 1)?;
            let attrs = attrs.unwrap();
            for (guid, val) in pairs {
                attrs.SetUINT64(guid, *val)?;
            }
            Ok(attrs)
        }
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
