// decoder.js
// Viewer-side media decode using the WebCodecs API.
//
// The Rust backend depacketizes the WebRTC stream and emits:
//   * `rtp-video` — a full H.264 access unit (Annex-B, one frame) per event
//   * `rtp-audio` — one Opus frame per event
//
// We decode H.264 with a hardware-accelerated VideoDecoder and paint each
// frame onto a <canvas>; we decode Opus with an AudioDecoder and schedule the
// PCM onto an AudioContext. This replaces the old MSE/fMP4 path: WebCodecs is
// lower latency (no container muxing, optimizeForLatency, no MSE buffering)
// and far simpler/less error-prone than hand-rolled fMP4.
//
// WebCodecs ships in modern WebView2 (Chromium 94+). If it is unavailable we
// surface a clear error rather than failing silently.

// ─── Annex-B helpers ──────────────────────────────────────────────────────────

/** Split an Annex-B buffer into NAL units (without start codes). */
function parseAnnexB(buf) {
  const nalus = []
  let start = -1
  let i = 0
  while (i + 2 < buf.length) {
    // 4-byte (00 00 00 01) or 3-byte (00 00 01) start code
    if (buf[i] === 0 && buf[i + 1] === 0 && buf[i + 2] === 1) {
      if (start !== -1) nalus.push(buf.subarray(start, i))
      start = i + 3
      i += 3
    } else if (
      i + 3 < buf.length &&
      buf[i] === 0 && buf[i + 1] === 0 && buf[i + 2] === 0 && buf[i + 3] === 1
    ) {
      if (start !== -1) nalus.push(buf.subarray(start, i))
      start = i + 4
      i += 4
    } else {
      i++
    }
  }
  if (start !== -1 && start < buf.length) nalus.push(buf.subarray(start))
  return nalus
}

const nalType = (nal) => nal[0] & 0x1f
const hex2 = (b) => b.toString(16).padStart(2, '0')

/** Build the WebCodecs codec string (e.g. "avc1.640028") from an SPS NAL. */
function codecStringFromSps(sps) {
  // sps[0] = NAL header (0x67), sps[1..4] = profile_idc, constraint flags, level_idc
  if (sps.length < 4) return 'avc1.42e01f'
  return `avc1.${hex2(sps[1])}${hex2(sps[2])}${hex2(sps[3])}`
}

// ─── Video ────────────────────────────────────────────────────────────────────

export class VideoStream {
  constructor(canvas) {
    this.canvas = canvas
    this.ctx = canvas ? canvas.getContext('2d') : null
    this.decoder = null
    this.configured = false
    this.error = null
    this.framesDecoded = 0
    this.bytesReceived = 0
    this._ts = 0
    this.supported = typeof window !== 'undefined' && 'VideoDecoder' in window
    if (!this.supported) {
      this.error = 'WebCodecs (VideoDecoder) is not available. Update the WebView2 Runtime.'
    }
  }

  _configure(codecString) {
    this.decoder = new VideoDecoder({
      output: (frame) => this._onFrame(frame),
      error: (e) => {
        this.error = e.message || String(e)
        console.error('VideoDecoder error:', e)
      },
    })
    // optimizeForLatency: emit frames ASAP (no reordering — we use no B-frames).
    this.decoder.configure({
      codec: codecString,
      optimizeForLatency: true,
      hardwareAcceleration: 'prefer-hardware',
    })
    this.configured = true
    console.info('VideoDecoder configured:', codecString)
  }

  _onFrame(frame) {
    this.framesDecoded++
    if (this.ctx) {
      const w = frame.displayWidth || frame.codedWidth
      const h = frame.displayHeight || frame.codedHeight
      if (this.canvas.width !== w || this.canvas.height !== h) {
        this.canvas.width = w
        this.canvas.height = h
      }
      try {
        this.ctx.drawImage(frame, 0, 0)
      } catch (e) {
        /* frame may be unrenderable on some edge cases — ignore one frame */
      }
    }
    frame.close()
  }

  /** Feed one H.264 access unit (Annex-B Uint8Array). */
  feed(annexB) {
    if (!this.supported || this.error) return
    this.bytesReceived += annexB.byteLength

    const nalus = parseAnnexB(annexB)
    if (nalus.length === 0) return
    const isKey = nalus.some((n) => nalType(n) === 5) // IDR present

    if (!this.configured) {
      // We must start on a keyframe that carries SPS, so the decoder can
      // initialise. Drop everything until that arrives.
      const sps = nalus.find((n) => nalType(n) === 7)
      if (!sps || !isKey) return
      try {
        this._configure(codecStringFromSps(sps))
      } catch (e) {
        this.error = e.message || String(e)
        console.error('VideoDecoder configure failed:', e)
        return
      }
    }

    const ts = this._ts
    this._ts += Math.round(1_000_000 / 60) // synthetic monotonic PTS (µs)
    try {
      this.decoder.decode(
        new EncodedVideoChunk({
          type: isKey ? 'key' : 'delta',
          timestamp: ts,
          data: annexB,
        })
      )
    } catch (e) {
      // Most commonly: a delta arrived before the first key was accepted.
      console.warn('video decode skipped:', e?.message || e)
    }
  }

  destroy() {
    try {
      if (this.decoder && this.decoder.state !== 'closed') this.decoder.close()
    } catch (_) {}
    this.decoder = null
    this.configured = false
  }
}

// ─── Audio ──────────────────────────────────────────────────────────────────--

export class AudioStream {
  constructor() {
    this.supported = typeof window !== 'undefined' && 'AudioDecoder' in window
    this.ctx = null
    this.decoder = null
    this.gain = null
    this.nextTime = 0
    this._ts = 0
    this.error = null
    if (!this.supported) {
      this.error = 'WebCodecs (AudioDecoder) is not available — audio disabled.'
    }
  }

  _init() {
    try {
      this.ctx = new (window.AudioContext || window.webkitAudioContext)({
        sampleRate: 48000,
        latencyHint: 'interactive',
      })
    } catch (_) {
      // Some devices reject a forced 48 kHz context — fall back to default.
      this.ctx = new (window.AudioContext || window.webkitAudioContext)()
    }
    this.gain = this.ctx.createGain()
    this.gain.connect(this.ctx.destination)

    this.decoder = new AudioDecoder({
      output: (data) => this._onData(data),
      error: (e) => {
        this.error = e.message || String(e)
        console.error('AudioDecoder error:', e)
      },
    })
    this.decoder.configure({ codec: 'opus', sampleRate: 48000, numberOfChannels: 2 })
    // Start ~60 ms ahead — a small jitter buffer (PRD §5.3 target 20–60 ms).
    this.nextTime = this.ctx.currentTime + 0.06
  }

  _onData(audioData) {
    try {
      const ch = audioData.numberOfChannels
      const frames = audioData.numberOfFrames
      const rate = audioData.sampleRate
      const buf = this.ctx.createBuffer(ch, frames, rate)
      const tmp = new Float32Array(frames)
      for (let c = 0; c < ch; c++) {
        audioData.copyTo(tmp, { planeIndex: c, format: 'f32-planar' })
        buf.copyToChannel(tmp, c)
      }
      const src = this.ctx.createBufferSource()
      src.buffer = buf
      src.connect(this.gain)
      const now = this.ctx.currentTime
      if (this.nextTime < now) this.nextTime = now + 0.02 // resync if we fell behind
      src.start(this.nextTime)
      this.nextTime += buf.duration
    } catch (e) {
      console.warn('audio render skipped:', e?.message || e)
    } finally {
      audioData.close()
    }
  }

  /** Feed one Opus frame (Uint8Array). */
  feed(opus) {
    if (!this.supported) return
    if (!this.decoder) this._init()
    if (this.ctx.state === 'suspended') this.ctx.resume()
    const ts = this._ts
    this._ts += 20_000 // 20 ms Opus frames (µs)
    try {
      this.decoder.decode(new EncodedAudioChunk({ type: 'key', timestamp: ts, data: opus }))
    } catch (e) {
      console.warn('audio decode skipped:', e?.message || e)
    }
  }

  destroy() {
    try {
      if (this.decoder && this.decoder.state !== 'closed') this.decoder.close()
    } catch (_) {}
    try {
      this.ctx && this.ctx.close()
    } catch (_) {}
    this.decoder = null
    this.ctx = null
  }
}
