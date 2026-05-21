# GameShare — How to Launch

GameShare is a **Tauri v2** desktop app: a **Rust** backend (screen capture,
hardware H.264 encode, WebRTC) plus a **React** UI rendered in WebView2. The
real streaming features are **Windows-only** (they use Windows Graphics
Capture, Media Foundation hardware encoders, and WASAPI audio). You can still
run the UI on macOS/Linux in a stubbed "dev" mode to click through the screens.

---

## 1. Prerequisites

Install these once.

### All platforms
- **Rust** (stable) — https://rustup.rs
- **Node.js ≥ 18** — https://nodejs.org
- **Tauri CLI**: `npm install -g @tauri-apps/cli` (or use `npx tauri …`)

### Windows (required for real streaming)
- **Windows 10 1903+ or Windows 11**
- **WebView2 Runtime** — preinstalled on Win 11; on Win 10 get the
  "Evergreen Standalone Installer" from Microsoft.
- **Visual Studio 2022 Build Tools** with the **"Desktop development with
  C++"** workload (gives you the MSVC linker the Rust/Windows crates need).
- A GPU with a hardware H.264 encoder (NVIDIA NVENC, AMD AMF, or Intel
  QuickSync) — virtually all GPUs from the last decade qualify.

---

## 2. Install dependencies

From the project root (the folder containing `package.json`):

```bash
npm install
```

This pulls the small frontend toolchain (React + Vite + the Tauri JS API).
Rust crates are fetched automatically the first time you build.

---

## 3. Run it

### A) Full app — real streaming (Windows)

```bash
npx tauri dev
```

The first run compiles the Rust backend, so expect a few minutes. After that
the GameShare window opens. Rust changes hot-recompile; React changes
hot-reload instantly.

To produce a distributable build (an `.exe` + installer):

```bash
npx tauri build
```

Output lands in `src-tauri/target/release/` (and `…/bundle/` for the
installer).

### B) UI only — browser dev mode (macOS / Linux / quick UI work)

```bash
npm run dev
```

Open the printed URL (default **http://localhost:1420**). Here every Tauri
call is **stubbed** in `src/lib/tauri.js`: you can navigate all screens and the
host flow shows a simulated invite code, but there is **no real capture,
encode, or streaming**, and no `rtp-video` / `rtp-audio` events fire — so the
viewer canvas stays black. This mode is purely for working on the interface.

---

## 4. Testing a real end-to-end stream

Real streaming is **peer-to-peer over the LAN** and needs **two separate
Windows machines on the same network**:

1. **Host PC** → launch GameShare → **Start Sharing** → pick a monitor/window.
   It shows an 8-character invite code.
2. **Viewer PC** → launch GameShare → **Watch Stream** → enter that code.

The signaling server binds `0.0.0.0:9001` on the host, so the two machines
must reach each other on the LAN (allow the app through Windows Firewall when
prompted). You **cannot** fully test host+viewer as two windows on one PC,
because the second instance can't bind the same port.

> **Note on decode:** the encoder produces **H.264 High profile**. The viewer
> reads the exact profile/level from the stream's SPS and configures the
> WebCodecs decoder accordingly, so it adapts automatically — you don't set a
> codec string by hand.

---

## 5. Common issues

| Symptom | Fix |
|---|---|
| `link.exe`/MSVC errors on Windows | Install the **VS 2022 "Desktop development with C++"** workload. |
| `tauri: command not found` | `npm install -g @tauri-apps/cli`, or prefix with `npx`. |
| Window opens but viewer canvas is black in `npm run dev` | Expected — browser dev mode has no real stream. Use `npx tauri dev` on two Windows PCs. |
| "WebCodecs (VideoDecoder) is not available" | Update the **WebView2 Runtime** (needs a recent Chromium). |
| Viewer can't reach host | Same LAN? Port **9001** open in Windows Firewall on the host? |
| First `tauri dev` is very slow | Normal — it's compiling the Rust + WebRTC crates once. Later builds are fast. |

---

## 6. What works today vs. what's next

**Working:** capture → hardware encode → WebRTC P2P transport → WebCodecs
decode → canvas render, custom UI, invite-code signaling on the LAN, live
viewer stats (fps / bitrate / frames decoded) from real decoder counters.

**Documented as next steps (not yet implemented):** real RTT/packet-loss from
WebRTC RTCP stats, automatic reconnection, adaptive bitrate, an external
signaling server + TURN for streaming across the internet (today it's
LAN-only), and code-signing for distribution.
