import { useState, useEffect, useRef, useCallback } from 'react'
import { invoke, listen, stubListen } from './lib/tauri'
import { VideoStream, AudioStream } from './lib/decoder'
import TitleBar from './components/TitleBar'
import SourcePicker from './components/SourcePicker'
import TokenDisplay from './components/TokenDisplay'
import StatsOverlay from './components/StatsOverlay'
import StatusBar from './components/StatusBar'
import ErrorBanner from './components/ErrorBanner'
import './styles/app.css'

// ─── Status machine ───────────────────────────────────────────────────────────
// idle → waiting_for_viewer → connecting → streaming → disconnected  (host)
// idle → connecting → streaming → disconnected                       (viewer)

export default function App() {
  const [mode, setMode] = useState(null)            // null | 'host' | 'viewer'
  const [status, setStatus] = useState('idle')
  const [inviteCode, setInviteCode] = useState('')
  const [viewerInput, setViewerInput] = useState('')
  const [sources, setSources] = useState([])
  const [selectedSource, setSelectedSource] = useState(null)
  const [showSourcePicker, setShowSourcePicker] = useState(false)
  const [error, setError] = useState(null)
  const [showStats, setShowStats] = useState(false)
  const [fps, setFps] = useState(60)
  const [bitrateMbps, setBitrateMbps] = useState(20)
  const [stats, setStats] = useState({
    fps: 0, bitrate: 0, rtt: null, loss: null,
    encoder: '—', captureMethod: '—', resolution: '—', connType: '—',
    framesDecoded: 0, mb: 0,
  })

  const canvasRef = useRef(null)
  const videoStreamRef = useRef(null)
  const audioStreamRef = useRef(null)
  const statsTimerRef = useRef(null)
  const lastFramesRef = useRef(0)
  const lastBytesRef = useRef(0)

  // ─── Source list ──────────────────────────────────────────────────────────
  // Fetched once on mount, then re-polled every 3 s while the picker is open
  // so newly launched windows appear without the user having to reopen it.
  useEffect(() => {
    const refresh = () =>
      invoke('list_capture_sources')
        .then(src => { if (src?.length) setSources(src) })
        .catch(e => console.warn('list_capture_sources:', e))

    refresh()
    if (!showSourcePicker) return
    const id = setInterval(refresh, 3000)
    return () => clearInterval(id)
  }, [showSourcePicker])

  // ─── Keyboard shortcuts ───────────────────────────────────────────────────
  useEffect(() => {
    const handler = (e) => {
      if (e.key === 'Tab' && status === 'streaming') {
        e.preventDefault()
        setShowStats(s => !s)
      }
    }
    window.addEventListener('keydown', handler)
    return () => window.removeEventListener('keydown', handler)
  }, [status])

  // ─── Tauri event listeners ────────────────────────────────────────────────
  // Registered once at mount. React StrictMode (dev) runs the effect twice
  // (mount → cleanup → remount). We use a `cancelled` flag so that if
  // cleanup fires while setup() is still awaiting, we immediately unlisten
  // the just-registered handlers rather than leaking them.
  useEffect(() => {
    let cancelled = false
    const registered = []

    const setup = async () => {
      // session-status: { status, invite_code? }
      const u1 = await listen('session-status', ({ payload }) => {
        setStatus(payload.status)
        if (payload.invite_code) setInviteCode(payload.invite_code)
        if (payload.status === 'disconnected') teardownDecoder()
      })

      // session-error: { message }
      const u2 = await listen('session-error', ({ payload }) => {
        setError(payload.message)
      })

      // rtp-video: base64 H.264 Annex-B access unit (one frame, viewer only)
      const u3 = await listen('rtp-video', ({ payload }) => {
        const vs = videoStreamRef.current
        if (!vs) return
        vs.feed(base64ToUint8Array(payload))
        if (vs.error) setError(vs.error)
      })

      // rtp-audio: base64 Opus frame (viewer only)
      const u4 = await listen('rtp-audio', ({ payload }) => {
        audioStreamRef.current?.feed(base64ToUint8Array(payload))
      })

      if (cancelled) {
        // Cleanup already ran (StrictMode double-mount) — immediately unlisten
        u1(); u2(); u3(); u4()
      } else {
        registered.push(u1, u2, u3, u4)
      }
    }

    setup()
    return () => {
      cancelled = true
      registered.forEach(fn => typeof fn === 'function' && fn())
    }
  }, [])

  // Also wire stub listeners for browser dev mode
  useEffect(() => {
    const removers = [
      stubListen('session-status', ({ payload }) => {
        setStatus(payload.status)
        if (payload.invite_code) setInviteCode(payload.invite_code)
      }),
      stubListen('session-error', ({ payload }) => {
        setError(payload.message)
      }),
    ]
    return () => removers.forEach(r => r())
  }, [])

  // ─── Stats polling ────────────────────────────────────────────────────────
  // Static facts (encoder, capture method, resolution, transport) come once
  // from the backend session state. Live fps/bitrate are derived from real
  // decoder counters on the viewer, and from the configured settings on the
  // host (the host doesn't decode, so it has no frame counter to read).
  useEffect(() => {
    if (status !== 'streaming') {
      clearInterval(statsTimerRef.current)
      return
    }

    // One-shot: pull the static session facts from the backend.
    invoke('get_session_state')
      .then(st => {
        if (!st) return
        setStats(prev => ({
          ...prev,
          encoder: st.encoder_kind || prev.encoder,
          captureMethod: st.capture_method || prev.captureMethod,
          resolution: st.width && st.height ? `${st.width}×${st.height}` : prev.resolution,
          connType: 'P2P',
        }))
      })
      .catch(() => { /* stubbed in browser dev — keep placeholders */ })

    lastFramesRef.current = 0
    lastBytesRef.current = 0

    statsTimerRef.current = setInterval(() => {
      if (mode === 'viewer') {
        const vs = videoStreamRef.current
        if (!vs) return
        const dFrames = vs.framesDecoded - lastFramesRef.current
        const dBytes = vs.bytesReceived - lastBytesRef.current
        lastFramesRef.current = vs.framesDecoded
        lastBytesRef.current = vs.bytesReceived
        setStats(prev => ({
          ...prev,
          fps: dFrames,
          bitrate: +((dBytes * 8) / 1_000_000).toFixed(1), // Mbps over last 1s
          framesDecoded: vs.framesDecoded,
          mb: +(vs.bytesReceived / (1024 * 1024)).toFixed(1),
        }))
      } else {
        // Host: report the settings it is encoding with.
        setStats(prev => ({ ...prev, fps, bitrate: bitrateMbps }))
      }
    }, 1000)

    return () => clearInterval(statsTimerRef.current)
  }, [status, mode, fps, bitrateMbps])

  // ─── Decoder lifecycle ────────────────────────────────────────────────────
  // Created only once we are actually streaming as a viewer, so the <canvas>
  // is mounted and canvasRef is non-null. (The previous MSE code initialised
  // from the status event before the element existed, and silently bailed.)
  useEffect(() => {
    if (status === 'streaming' && mode === 'viewer') {
      initDecoder()
    }
    return () => teardownDecoder()
  }, [status, mode])

  // ─── Decoder pipeline lifecycle ───────────────────────────────────────────
  function initDecoder() {
    if (!canvasRef.current) return
    teardownDecoder()
    const vs = new VideoStream(canvasRef.current)
    if (vs.error) setError(vs.error)
    videoStreamRef.current = vs
    audioStreamRef.current = new AudioStream()
  }

  function teardownDecoder() {
    if (videoStreamRef.current) {
      videoStreamRef.current.destroy()
      videoStreamRef.current = null
    }
    if (audioStreamRef.current) {
      audioStreamRef.current.destroy()
      audioStreamRef.current = null
    }
  }

  // ─── Host flow ────────────────────────────────────────────────────────────
  function handleSourceSelected(src) {
    setSelectedSource(src)
    setShowSourcePicker(false)
    // Update stats display
    setStats(prev => ({
      ...prev,
      resolution: `${src.width}×${src.height}`,
    }))
    // Auto-start after picking source
    setTimeout(() => handleStartWithSource(src), 0)
  }

  async function handleStartWithSource(src) {
    setError(null)
    setMode('host')
    setStatus('generating_token') // show spinner immediately, don't wait for event
    try {
      const code = await invoke('start_host_session', {
        sourceId: src.id,
        fps,
        bitrateMbps,
      })
      // Backend returned the invite code synchronously — drive state from the
      // return value so the waiting screen shows even if the Tauri event was
      // delivered before our listener was ready (StrictMode timing race).
      if (code) {
        setInviteCode(code)
        setStatus('waiting_for_viewer')
      }
    } catch (e) {
      setError(String(e))
      setStatus('idle')
      setMode(null)
    }
  }

  // ─── Viewer flow ──────────────────────────────────────────────────────────
  async function handleJoinViewer() {
    const code = viewerInput.trim()
    if (!code) { setError('Enter the invite code from the host.'); return }
    setError(null)
    setMode('viewer')
    setStatus('connecting') // show spinner immediately, prevents double-click
    try {
      await invoke('join_viewer_session', { inviteCode: code })
    } catch (e) {
      setError(String(e))
      setStatus('idle')
      setMode(null)
    }
  }

  // ─── Stop session ─────────────────────────────────────────────────────────
  async function handleStop() {
    await invoke('stop_session')
    teardownDecoder()
    setStatus('idle')
    setMode(null)
    setInviteCode('')
    setViewerInput('')
    setSelectedSource(null)
    setShowStats(false)
    setError(null)
  }

  // ─── Render ───────────────────────────────────────────────────────────────

  const isStreaming = status === 'streaming'
  const isBusy = !['idle', 'disconnected'].includes(status)

  return (
    <div className="app-root">
      <TitleBar
        title={mode ? `GAMESHARE — ${mode.toUpperCase()}` : 'GAMESHARE'}
        onBack={!isBusy && mode ? handleStop : null}
      />

      {error && (
        <ErrorBanner message={error} onDismiss={() => setError(null)} />
      )}

      <div className="app-body">

        {/* ── Landing ── */}
        {!mode && (
          <div className="landing">
            <div className="logo-block">
              <svg className="logo-icon" viewBox="0 0 40 40" fill="none">
                <rect x="3" y="5" width="34" height="22" rx="3"
                  stroke="var(--accent)" strokeWidth="1.5" fill="var(--accent-dim)" />
                <polygon points="16,12 28,18 16,24" fill="var(--accent)" />
                <line x1="13" y1="33" x2="27" y2="33" stroke="var(--accent)" strokeWidth="1.5" />
                <line x1="20" y1="27" x2="20" y2="33" stroke="var(--accent)" strokeWidth="1.5" />
              </svg>
              <span className="logo-text">GAMESHARE</span>
            </div>
            <p className="tagline">Hardware-encoded. Peer-to-peer. Under 80ms.</p>

            <div className="mode-row">
              <button className="mode-btn mode-btn--primary" onClick={() => setShowSourcePicker(true)}>
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5">
                  <rect x="2" y="3" width="20" height="14" rx="2" />
                  <line x1="8" y1="21" x2="16" y2="21" />
                  <line x1="12" y1="17" x2="12" y2="21" />
                </svg>
                <span>Start Sharing</span>
                <small>Host a stream</small>
              </button>

              <button className="mode-btn mode-btn--secondary" onClick={() => setMode('viewer')}>
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5">
                  <path d="M2 12s3-7 10-7 10 7 10 7-3 7-10 7-10-7-10-7z" />
                  <circle cx="12" cy="12" r="3" />
                </svg>
                <span>Watch Stream</span>
                <small>Join with invite code</small>
              </button>
            </div>

            <div className="settings-row">
              <label className="setting-item">
                <span>FPS</span>
                <select value={fps} onChange={e => setFps(Number(e.target.value))}>
                  <option value={30}>30</option>
                  <option value={60}>60</option>
                </select>
              </label>
              <label className="setting-item">
                <span>Bitrate</span>
                <select value={bitrateMbps} onChange={e => setBitrateMbps(Number(e.target.value))}>
                  <option value={10}>10 Mbps</option>
                  <option value={15}>15 Mbps</option>
                  <option value={20}>20 Mbps</option>
                  <option value={25}>25 Mbps</option>
                  <option value={30}>30 Mbps</option>
                </select>
              </label>
            </div>

            <div className="spec-row">
              {[['1080p60','Resolution'],['H.264','Codec'],['Opus','Audio'],['WebRTC P2P','Transport']].map(([v,l]) => (
                <div key={l} className="spec-item">
                  <div className="spec-value">{v}</div>
                  <div className="spec-label">{l}</div>
                </div>
              ))}
            </div>
          </div>
        )}

        {/* ── Viewer join screen ── */}
        {mode === 'viewer' && status === 'idle' && (
          <div className="join-screen">
            <div className="join-icon">
              <svg viewBox="0 0 24 24" fill="none" stroke="var(--accent)" strokeWidth="1.2">
                <path d="M2 12s3-7 10-7 10 7 10 7-3 7-10 7-10-7-10-7z"/>
                <circle cx="12" cy="12" r="3"/>
              </svg>
            </div>
            <h2>Enter Invite Code</h2>
            <p>Paste the code the host shared with you</p>
            <input
              className="code-input"
              value={viewerInput}
              onChange={e => setViewerInput(e.target.value.trim())}
              onKeyDown={e => e.key === 'Enter' && handleJoinViewer()}
              placeholder="Paste invite code from host"
              autoFocus
              spellCheck={false}
            />
            <div className="join-actions">
              <button className="btn-primary" onClick={handleJoinViewer}>Join Stream</button>
              <button className="btn-ghost" onClick={() => { setMode(null); setError(null) }}>Back</button>
            </div>
          </div>
        )}

        {/* ── Waiting / Connecting spinner ── */}
        {isBusy && !isStreaming && (
          <div className="busy-screen">
            <Spinner />
            <div className="busy-title">
              {statusLabel(status)}
            </div>
            {inviteCode && status === 'waiting_for_viewer' && (
              <TokenDisplay code={inviteCode} />
            )}
            {(status === 'waiting_for_viewer' || status === 'connecting') && (
              <button className="btn-ghost btn-ghost--sm" onClick={handleStop}>Cancel</button>
            )}
          </div>
        )}

        {/* ── Streaming view (host + viewer share this) ── */}
        {isStreaming && (
          <div className="stream-view">
            <div className="stream-toolbar">
              <span className="live-chip">● LIVE</span>
              {selectedSource && (
                <span className="stream-source-name">{selectedSource.name}</span>
              )}
              <div style={{ flex: 1 }} />
              <button
                className="toolbar-btn"
                onClick={() => setShowStats(s => !s)}
                title="Toggle stats [Tab]"
              >
                {showStats ? 'Hide Stats' : 'Stats [Tab]'}
              </button>
              {mode === 'viewer' && inviteCode && (
                <TokenDisplay code={inviteCode} compact />
              )}
              <button className="toolbar-btn toolbar-btn--danger" onClick={handleStop}>
                ■ {mode === 'host' ? 'Stop' : 'Leave'}
              </button>
            </div>

            <div className="stream-canvas-wrap">
              {/* Host: show a "preview" label since the host sees the actual game.
                  Viewer: <canvas> painted by the WebCodecs VideoDecoder. */}
              {mode === 'host' ? (
                <HostPreview source={selectedSource} />
              ) : (
                <canvas
                  ref={canvasRef}
                  className="stream-video"
                />
              )}

              {showStats && (
                <StatsOverlay
                  stats={stats}
                  role={mode}
                  onClose={() => setShowStats(false)}
                />
              )}
            </div>

            <div className="stream-hint">
              Press <kbd>Tab</kbd> for stats
              {mode === 'viewer' && <> · <kbd>F11</kbd> fullscreen</>}
            </div>
          </div>
        )}

        {/* ── Disconnected screen ── */}
        {status === 'disconnected' && (
          <div className="busy-screen">
            <div className="disconnect-icon">
              <svg viewBox="0 0 24 24" fill="none" stroke="var(--red)" strokeWidth="1.5">
                <circle cx="12" cy="12" r="10"/>
                <line x1="15" y1="9" x2="9" y2="15"/>
                <line x1="9" y1="9" x2="15" y2="15"/>
              </svg>
            </div>
            <div className="busy-title">Session Ended</div>
            <button className="btn-primary" onClick={handleStop}>Back to Home</button>
          </div>
        )}
      </div>

      <StatusBar status={status} stats={stats} mode={mode} />

      {/* Source picker modal */}
      {showSourcePicker && (
        <SourcePicker
          sources={sources}
          onSelect={handleSourceSelected}
          onCancel={() => setShowSourcePicker(false)}
        />
      )}
    </div>
  )
}

// ─── Sub-components ───────────────────────────────────────────────────────────

function Spinner() {
  return (
    <svg className="spinner" viewBox="0 0 40 40" fill="none">
      <circle cx="20" cy="20" r="16" stroke="var(--border2)" strokeWidth="3"/>
      <path d="M20 4 A16 16 0 0 1 36 20" stroke="var(--accent)" strokeWidth="3" strokeLinecap="round"/>
    </svg>
  )
}

function HostPreview({ source }) {
  return (
    <div className="host-preview">
      <div className="host-preview-label">
        <svg viewBox="0 0 24 24" fill="none" stroke="var(--accent)" strokeWidth="1.5">
          <rect x="2" y="3" width="20" height="14" rx="2"/>
          <line x1="8" y1="21" x2="16" y2="21"/>
          <line x1="12" y1="17" x2="12" y2="21"/>
        </svg>
        {source ? source.name : 'Capturing…'}
      </div>
      <p className="host-preview-sub">
        Your screen is being captured and streamed to the viewer.<br />
        The viewer sees your display in real time.
      </p>
    </div>
  )
}

function statusLabel(status) {
  const map = {
    generating_token: 'Preparing session…',
    waiting_for_viewer: 'Waiting for viewer…',
    connecting: 'Establishing P2P connection…',
    reconnecting: 'Reconnecting…',
  }
  return map[status] || status
}

function base64ToUint8Array(b64) {
  const binary = atob(b64)
  const arr = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i++) arr[i] = binary.charCodeAt(i)
  return arr
}
