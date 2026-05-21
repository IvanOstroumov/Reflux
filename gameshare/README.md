# GameShare

Hardware-encoded, peer-to-peer screen sharing for gamers.
Tauri v2 + Rust backend · React frontend · WebRTC transport.

## Architecture

```
capture (WGC/DXGI) → encoder (NVENC/AMF/QSV via MF) → WebRTC (H.264 + Opus) → viewer
```

| Component | Tech |
|-----------|------|
| Screen capture | Windows Graphics Capture (primary), DXGI Desktop Duplication (fallback) |
| Video encoding | Hardware H.264 via Media Foundation MFT (NVENC / AMF / QuickSync) |
| Audio capture | WASAPI loopback |
| Audio encoding | Opus 128 kbps, 48 kHz stereo, 20 ms frames |
| Transport | WebRTC (pure-Rust `webrtc` crate), P2P ICE with STUN |
| Signaling | In-process TCP (newline-delimited JSON) — no external server |
| Video decode (viewer) | WebCodecs VideoDecoder (GPU H.264 decode) → canvas |
| Audio decode (viewer) | Web Audio API (browser Opus decode) |
| App shell | Tauri v2, custom decorations, no system titlebar |

## Prerequisites

### Windows (required for full functionality)
- Windows 10 1903+ or Windows 11 (required for WGC)
- NVIDIA/AMD/Intel GPU with hardware H.264 encoder
- Visual Studio 2022 Build Tools with C++ workload
- WebView2 Runtime (ships with Windows 11; installer available for Win10)

### All platforms (development)
```
rustup (stable toolchain)   https://rustup.rs
Node.js ≥ 18                https://nodejs.org
```

## Quick Start

```bash
# 1. Install frontend dependencies
npm install

# 2. Install Tauri CLI
npm install -g @tauri-apps/cli

# 3. Run in development mode (hot-reload frontend + Rust rebuild on changes)
npx tauri dev

# 4. Build release binary
npx tauri build
```

The built installer will be at `src-tauri/target/release/bundle/`.

## Development Without Windows

On macOS/Linux the Rust backend compiles with stubs:
- Screen capture produces animated synthetic test frames (BGRA gradient)
- Audio produces silent Opus packets
- The full frontend, state machine, WebCodecs decode pipeline, and WebRTC code compile unchanged

Run the frontend in isolation (browser only, all Tauri calls are stubbed):
```bash
npm run dev
# Open http://localhost:1420
```

## Configuration

| Setting | Location | Default |
|---------|----------|---------|
| STUN server | `src-tauri/src/webrtc_session.rs` | `stun:stun.l.google.com:19302` |
| TURN server | `src-tauri/src/webrtc_session.rs` | empty (disabled) |
| Signaling port | `src-tauri/src/lib.rs` | `9001` |
| Default bitrate | `src-tauri/src/encoder.rs` | 20 Mbps |
| Max bitrate | `src-tauri/src/encoder.rs` | 35 Mbps |
| Keyframe interval | `src-tauri/src/encoder.rs` | 2 seconds |

## Session Flow

### Host
1. Click **Start Sharing** → select window or monitor
2. App generates an invite code (base64-encoded IP:port:token)
3. Share the code with the viewer (copy button)
4. Viewer joins → WebRTC P2P handshake → streaming begins

### Viewer
1. Click **Watch Stream** → paste the invite code
2. App connects to host's signaling endpoint
3. WebRTC SDP offer/answer exchange
4. H.264 access units decoded by WebCodecs VideoDecoder, painted to a `<canvas>`

## File Structure

```
gameshare/
├── src/                        React frontend
│   ├── App.jsx                 Main app, state machine, Tauri IPC
│   ├── components/
│   │   ├── TitleBar.jsx        Custom window chrome
│   │   ├── TokenDisplay.jsx    Invite code display + copy
│   │   ├── StatsOverlay.jsx    Live stream stats (Tab)
│   │   ├── StatusBar.jsx       Bottom status strip
│   │   └── ErrorBanner.jsx     Error display
│   ├── lib/
│   │   ├── tauri.js            IPC wrapper + browser stubs
│   │   └── decoder.js          WebCodecs H.264 + Opus decode pipeline
│   └── styles/
│       ├── global.css
│       └── app.css
├── src-tauri/
│   └── src/
│       ├── main.rs             Binary entry point
│       ├── lib.rs              Tauri commands, app state, pipeline orchestration
│       ├── capture.rs          Capture abstraction + non-Windows stub
│       ├── capture_win.rs      WGC + DXGI implementations (Windows only)
│       ├── encoder.rs          Hardware H.264 encoder via Media Foundation
│       ├── audio.rs            Audio abstraction
│       ├── audio_win.rs        WASAPI loopback + Opus encoder (Windows only)
│       ├── signaling.rs        In-process TCP signaling (offer/answer/ICE)
│       └── webrtc_session.rs   WebRTC peer connection, RTP tracks
├── package.json
├── vite.config.js
├── index.html
└── src-tauri/
    ├── Cargo.toml
    ├── build.rs
    └── tauri.conf.json
```

## Adding a TURN Server

Edit `src-tauri/src/webrtc_session.rs`:

```rust
const TURN_SERVER:     &str = "turn:your.server.com:3478";
const TURN_USERNAME:   &str = "user";
const TURN_CREDENTIAL: &str = "password";
```

## Known Limitations

- **Windows only** for real capture/encode. Non-Windows builds compile with stubs.
- WGC may be blocked by anti-cheat (EAC/BattleEye) in some games — DXGI fallback activates automatically.
- Viewer decode relies on WebView2's native H.264 support. WebView2 ships H.264 on all Windows 10/11 installs.
- No E2E encryption in the current signaling path — add TLS for internet use.
- HDR capture is tone-mapped to SDR by WGC automatically (no HDR passthrough yet).
