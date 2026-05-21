// capture_win.rs
// Windows-only screen capture implementations:
//   1. Windows Graphics Capture (WGC) - primary
//   2. DXGI Desktop Duplication        - fallback
//
// Both deliver frames as BGRA VideoFrame structs with QueryPerformanceCounter PTS.
// GDI capture is not implemented (forbidden per PRD §5.1).

#![cfg(target_os = "windows")]

use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use windows::{
    core::*,
    Win32::{
        Foundation::{BOOL, HWND, LPARAM, RECT, TRUE},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
                D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
                D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
            },
            Dxgi::{
                CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIOutput1,
                IDXGISurface, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
                DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
            },
            Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            Gdi::HMONITOR,
        },
        System::{
            Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
            WinRT::{
                CreateDispatcherQueueController,
                DispatcherQueueOptions,
                DQTAT_COM_STA,
                DQTYPE_THREAD_DEDICATED,
                // CreateDirect3D11DeviceFromDXGIDevice lives here in windows 0.58
                Direct3D11::CreateDirect3D11DeviceFromDXGIDevice,
                // IGraphicsCaptureItemInterop for HWND/HMONITOR → GraphicsCaptureItem
                Graphics::Capture::IGraphicsCaptureItemInterop,
            },
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetWindowTextW, IsWindowVisible, GetWindowRect,
        },
    },
    Graphics::{
        Capture::{
            Direct3D11CaptureFramePool, GraphicsCaptureItem,
        },
        DirectX::{
            Direct3D11::{IDirect3DDevice, IDirect3DSurface},
            DirectXPixelFormat,
        },
    },
};

use crate::capture::{CaptureMethod, CaptureSession, CaptureSource, CaptureTarget, VideoFrame};

// ─── QPC helpers ─────────────────────────────────────────────────────────────

fn qpc_frequency() -> i64 {
    let mut freq = 0i64;
    unsafe { QueryPerformanceFrequency(&mut freq).ok() };
    if freq == 0 { 10_000_000 } else { freq }
}

fn qpc_now_us() -> u64 {
    let mut counter = 0i64;
    unsafe { QueryPerformanceCounter(&mut counter).ok() };
    let freq = qpc_frequency();
    (counter as u128 * 1_000_000 / freq as u128) as u64
}

// ─── D3D11 device creation ────────────────────────────────────────────────────

fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice failed")?;
    }
    Ok((device.unwrap(), context.unwrap()))
}

// ─── WGC Implementation ───────────────────────────────────────────────────────

pub async fn try_wgc(
    target: CaptureTarget,
    fps_limit: u32,
    frame_tx: mpsc::Sender<VideoFrame>,
) -> Result<CaptureSession> {
    // WGC requires a DispatcherQueue on a dedicated STA thread.
    // We create the item and session on a blocking thread, then loop frames.
    // The stop channel is owned here; dropping CaptureSession signals the thread.
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(u32, u32)>>();

    std::thread::spawn(move || {
        // Create a dispatcher queue on this thread (required by WGC).
        let dq_opts = DispatcherQueueOptions {
            dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_DEDICATED,
            apartmentType: DQTAT_COM_STA,
        };
        let _controller = unsafe {
            match CreateDispatcherQueueController(dq_opts) {
                Ok(c) => c,
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow!("DispatcherQueue: {e}")));
                    return;
                }
            }
        };

        // Create D3D11 device
        let (d3d_device, d3d_ctx) = match create_d3d11_device() {
            Ok(d) => d,
            Err(e) => { let _ = ready_tx.send(Err(e)); return; }
        };

        // Wrap D3D11 device as IDirect3DDevice (WinRT interface)
        let dxgi_device: IDXGIDevice = match d3d_device.cast() {
            Ok(d) => d,
            Err(e) => { let _ = ready_tx.send(Err(anyhow!("IDXGIDevice cast: {e}"))); return; }
        };

        // In windows 0.58, CreateDirect3D11DeviceFromDXGIDevice is in
        // Win32::System::WinRT::Direct3D11 and returns IInspectable; cast to IDirect3DDevice.
        let winrt_device: IDirect3DDevice = unsafe {
            match CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) {
                Ok(inspectable) => match inspectable.cast::<IDirect3DDevice>() {
                    Ok(d) => d,
                    Err(e) => { let _ = ready_tx.send(Err(anyhow!("IDirect3DDevice cast: {e}"))); return; }
                },
                Err(e) => { let _ = ready_tx.send(Err(anyhow!("WinRT D3D device: {e}"))); return; }
            }
        };

        // Build a GraphicsCaptureItem from the target
        let item: GraphicsCaptureItem = match &target {
            CaptureTarget::Monitor { index } => {
                match capture_item_for_monitor(*index) {
                    Ok(i) => i,
                    Err(e) => { let _ = ready_tx.send(Err(e)); return; }
                }
            }
            CaptureTarget::Window { hwnd } => {
                match capture_item_for_hwnd(HWND(*hwnd as _)) {
                    Ok(i) => i,
                    Err(e) => { let _ = ready_tx.send(Err(e)); return; }
                }
            }
        };

        let size = match item.Size() {
            Ok(s) => s,
            Err(e) => { let _ = ready_tx.send(Err(anyhow!("item.Size(): {e}"))); return; }
        };
        let (w, h) = (size.Width as u32, size.Height as u32);

        // Create frame pool (2 frames, BGRA8)
        let frame_pool = match Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        ) {
            Ok(fp) => fp,
            Err(e) => { let _ = ready_tx.send(Err(anyhow!("FramePool: {e}"))); return; }
        };

        let session = match frame_pool.CreateCaptureSession(&item) {
            Ok(s) => s,
            Err(e) => { let _ = ready_tx.send(Err(anyhow!("CaptureSession: {e}"))); return; }
        };

        // Disable cursor + border (cleaner stream)
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);

        if let Err(e) = session.StartCapture() {
            let _ = ready_tx.send(Err(anyhow!("StartCapture: {e}")));
            return;
        }

        let _ = ready_tx.send(Ok((w, h)));

        // Frame loop — poll at fps_limit rate
        let frame_us = 1_000_000u64 / fps_limit.max(1) as u64;
        let mut next_frame_time = std::time::Instant::now();
        let mut last_stop_check = std::time::Instant::now();

        // Staging texture for CPU readback
        let staging = create_staging_texture(&d3d_device, w, h)
            .expect("Failed to create staging texture");

        loop {
            // Check stop signal every 16ms (break on explicit stop or sender dropped)
            if last_stop_check.elapsed().as_millis() > 16 {
                match stop_rx.try_recv() {
                    Ok(()) | Err(oneshot::error::TryRecvError::Closed) => break,
                    Err(oneshot::error::TryRecvError::Empty) => {}
                }
                last_stop_check = std::time::Instant::now();
            }

            // Rate limit
            let now = std::time::Instant::now();
            if now < next_frame_time {
                std::thread::sleep(next_frame_time - now);
            }
            next_frame_time = std::time::Instant::now()
                + std::time::Duration::from_micros(frame_us);

            // Try to grab the next frame from pool
            let frame = match frame_pool.TryGetNextFrame() {
                Ok(f) => f,
                Err(_) => continue, // no frame ready yet
            };

            let pts_us = qpc_now_us();

            // Get the surface from the frame
            let surface: IDirect3DSurface = match frame.Surface() {
                Ok(s) => s,
                Err(_) => continue,
            };

            // Get the underlying DXGI surface, then the D3D11 texture
            let dxgi_surface: IDXGISurface = match surface.cast() {
                Ok(s) => s,
                Err(_) => continue,
            };

            let src_texture: ID3D11Texture2D = match dxgi_surface.cast() {
                Ok(t) => t,
                Err(_) => continue,
            };

            // Copy GPU texture → staging texture
            unsafe {
                d3d_ctx.CopyResource(&staging, &src_texture);

                // Map staging texture to read pixels
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                if d3d_ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).is_err() {
                    continue;
                }

                let row_pitch = mapped.RowPitch as usize;
                let total_bytes = (h as usize) * row_pitch;
                let src_ptr = mapped.pData as *const u8;
                let raw = std::slice::from_raw_parts(src_ptr, total_bytes);

                // Copy row by row to handle stride differences
                let stride = (w * 4) as usize;
                let mut pixels = vec![0u8; (w * h * 4) as usize];
                for row in 0..h as usize {
                    let src_row = &raw[row * row_pitch..row * row_pitch + stride];
                    let dst_row = &mut pixels[row * stride..(row + 1) * stride];
                    dst_row.copy_from_slice(src_row);
                }

                d3d_ctx.Unmap(&staging, 0);

                let vf = VideoFrame {
                    data: Arc::new(pixels),
                    width: w,
                    height: h,
                    pts_us,
                };

                // Non-blocking send — drop frame if encoder backpressured.
                // Exit if the receiver (encoder) has been dropped.
                if frame_tx.try_send(vf).is_err() && frame_tx.is_closed() {
                    break;
                }
            }
        }

        // Cleanup
        let _ = session.Close();
        let _ = frame_pool.Close();
    });

    // Wait for the thread to confirm capture started.
    // stop_tx stays here; dropping CaptureSession drops stop_tx which signals
    // the thread's stop_rx (via TryRecvError::Closed) to exit.
    match ready_rx.await {
        Ok(Ok((w, h))) => Ok(CaptureSession {
            method: CaptureMethod::WindowsGraphicsCapture,
            width: w,
            height: h,
            _stop_tx: stop_tx,
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow!("WGC setup thread panicked")),
    }
}

fn capture_item_for_monitor(index: u32) -> Result<GraphicsCaptureItem> {
    // Enumerate monitors via DXGI factory, pick by index.
    // Then use IGraphicsCaptureItemInterop::CreateForMonitor (classic Win32 interop path).
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                .context("IGraphicsCaptureItemInterop factory")?;

        let mut adapter_idx = 0u32;
        let mut monitor_count = 0u32;
        loop {
            let adapter: IDXGIAdapter1 = match factory.EnumAdapters1(adapter_idx) {
                Ok(a) => a,
                Err(_) => break,
            };
            let mut output_idx = 0u32;
            loop {
                let output = match adapter.EnumOutputs(output_idx) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                if monitor_count == index {
                    // GetDesc returns Result<DXGI_OUTPUT_DESC> in windows 0.58
                    let desc: DXGI_OUTPUT_DESC = output.GetDesc()
                        .context("IDXGIOutput::GetDesc")?;
                    let hmon: HMONITOR = desc.Monitor;
                    return interop.CreateForMonitor(hmon)
                        .context("IGraphicsCaptureItemInterop::CreateForMonitor");
                }
                monitor_count += 1;
                output_idx += 1;
            }
            adapter_idx += 1;
        }
        Err(anyhow!("Monitor index {index} not found"))
    }
}

fn capture_item_for_hwnd(hwnd: HWND) -> Result<GraphicsCaptureItem> {
    // Use the classic Win32 interop COM interface — no WindowId required.
    unsafe {
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                .context("IGraphicsCaptureItemInterop factory")?;
        interop.CreateForWindow(hwnd)
            .context("IGraphicsCaptureItemInterop::CreateForWindow")
    }
}

fn create_staging_texture(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        // In windows 0.58, BindFlags/CPUAccessFlags/MiscFlags are plain u32.
        BindFlags: 0u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0u32,
    };
    let mut tex = None;
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut tex))
              .context("CreateTexture2D staging")?;
    }
    tex.ok_or_else(|| anyhow!("CreateTexture2D returned None"))
}

// ─── DXGI Desktop Duplication ─────────────────────────────────────────────────

pub async fn try_dxgi(
    target: CaptureTarget,
    fps_limit: u32,
    frame_tx: mpsc::Sender<VideoFrame>,
) -> Result<CaptureSession> {
    let monitor_index = match &target {
        CaptureTarget::Monitor { index } => *index,
        CaptureTarget::Window { .. } => 0, // DXGI always captures the full output
    };

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(u32, u32)>>();

    std::thread::spawn(move || {
        let result = dxgi_capture_loop(
            monitor_index, fps_limit, frame_tx, stop_rx, ready_tx
        );
        if let Err(e) = result {
            log::error!("DXGI capture loop error: {e}");
        }
    });

    match ready_rx.await {
        Ok(Ok((w, h))) => Ok(CaptureSession {
            method: CaptureMethod::DxgiDesktopDuplication,
            width: w,
            height: h,
            _stop_tx: stop_tx,
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow!("DXGI setup thread panicked")),
    }
}

fn dxgi_capture_loop(
    monitor_index: u32,
    fps_limit: u32,
    frame_tx: mpsc::Sender<VideoFrame>,
    mut stop_rx: oneshot::Receiver<()>,
    ready_tx: oneshot::Sender<Result<(u32, u32)>>,
) -> Result<()> {
    unsafe {
        let (d3d_device, d3d_ctx) = create_d3d11_device()?;

        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let adapter: IDXGIAdapter1 = factory.EnumAdapters1(0)?;

        // Find the requested output (monitor)
        let output = adapter.EnumOutputs(monitor_index)
            .map_err(|_| anyhow!("Monitor {monitor_index} not found for DXGI"))?;
        let output1: IDXGIOutput1 = output.cast()?;

        // Create the duplication interface
        let duplication = output1.DuplicateOutput(&d3d_device)
            .map_err(|e| anyhow!("DuplicateOutput failed: {e}"))?;

        // IDXGIOutputDuplication::GetDesc takes 0 args and returns DXGI_OUTDUPL_DESC in windows 0.58.
        let desc = duplication.GetDesc();
        let w = desc.ModeDesc.Width;
        let h = desc.ModeDesc.Height;

        let staging = create_staging_texture(&d3d_device, w, h)?;

        let _ = ready_tx.send(Ok((w, h)));

        let _frame_us = 1_000_000u64 / fps_limit.max(1) as u64;
        let timeout_ms = (1000 / fps_limit.max(1) + 5).min(50);

        loop {
            match stop_rx.try_recv() {
                Ok(()) | Err(oneshot::error::TryRecvError::Closed) => break,
                Err(oneshot::error::TryRecvError::Empty) => {}
            }

            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;

            match duplication.AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    log::warn!("DXGI access lost — stopping duplication");
                    break;
                }
                Err(e) => {
                    log::error!("AcquireNextFrame: {e}");
                    break;
                }
            }

            if frame_info.LastPresentTime == 0 {
                // No new frame — just a cursor update
                let _ = duplication.ReleaseFrame();
                continue;
            }

            let pts_us = qpc_now_us();

            let texture: ID3D11Texture2D = match resource.as_ref() {
                Some(r) => match r.cast::<ID3D11Texture2D>() {
                    Ok(t) => t,
                    Err(_) => { let _ = duplication.ReleaseFrame(); continue; }
                },
                None => { let _ = duplication.ReleaseFrame(); continue; }
            };

            d3d_ctx.CopyResource(&staging, &texture);
            let _ = duplication.ReleaseFrame();

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if d3d_ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).is_err() {
                continue;
            }

            let row_pitch = mapped.RowPitch as usize;
            let stride = (w * 4) as usize;
            let src_ptr = mapped.pData as *const u8;
            let raw = std::slice::from_raw_parts(src_ptr, h as usize * row_pitch);

            let mut pixels = vec![0u8; (w * h * 4) as usize];
            for row in 0..h as usize {
                pixels[row * stride..(row + 1) * stride]
                    .copy_from_slice(&raw[row * row_pitch..row * row_pitch + stride]);
            }

            d3d_ctx.Unmap(&staging, 0);

            let vf = VideoFrame {
                data: Arc::new(pixels),
                width: w,
                height: h,
                pts_us,
            };
            if frame_tx.try_send(vf).is_err() && frame_tx.is_closed() {
                break;
            }
        }
    }
    Ok(())
}

// ─── Window enumeration ───────────────────────────────────────────────────────

pub fn enumerate_sources_win() -> Vec<CaptureSource> {
    let mut sources: Vec<CaptureSource> = Vec::new();

    // Add monitors first
    unsafe {
        let factory: IDXGIFactory1 = match CreateDXGIFactory1() {
            Ok(f) => f,
            Err(_) => return sources,
        };
        let mut monitor_idx = 0u32;
        let mut adapter_idx = 0u32;
        loop {
            let adapter: IDXGIAdapter1 = match factory.EnumAdapters1(adapter_idx) {
                Ok(a) => a,
                Err(_) => break,
            };
            let mut output_idx = 0u32;
            loop {
                let output = match adapter.EnumOutputs(output_idx) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                // GetDesc returns Result<DXGI_OUTPUT_DESC> in windows 0.58
                let desc: DXGI_OUTPUT_DESC = match output.GetDesc() {
                    Ok(d) => d,
                    Err(_) => { output_idx += 1; continue; }
                };
                let rect = desc.DesktopCoordinates;
                let w = (rect.right - rect.left) as u32;
                let h = (rect.bottom - rect.top) as u32;
                sources.push(CaptureSource {
                    id: format!("monitor:{monitor_idx}"),
                    name: format!("Display {} ({}×{})", monitor_idx + 1, w, h),
                    kind: "monitor".into(),
                    hwnd: None,
                    monitor_index: Some(monitor_idx),
                    width: w,
                    height: h,
                    thumbnail_b64: None,
                });
                monitor_idx += 1;
                output_idx += 1;
            }
            adapter_idx += 1;
        }
    }

    // Add visible windows
    struct EnumState(Vec<CaptureSource>);
    extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return TRUE;
            }
            let mut title = [0u16; 256];
            let len = GetWindowTextW(hwnd, &mut title);
            if len == 0 { return TRUE; }
            let name = String::from_utf16_lossy(&title[..len as usize]);
            if name.trim().is_empty() { return TRUE; }

            let mut rect = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rect);
            let w = (rect.right - rect.left).max(0) as u32;
            let h = (rect.bottom - rect.top).max(0) as u32;
            if w < 100 || h < 100 { return TRUE; } // skip tiny utility windows

            let state = &mut *(lparam.0 as *mut EnumState);
            state.0.push(CaptureSource {
                id: format!("window:{}", hwnd.0 as isize),
                name,
                kind: "window".into(),
                hwnd: Some(hwnd.0 as isize),
                monitor_index: None,
                width: w,
                height: h,
                thumbnail_b64: None,
            });
            TRUE
        }
    }

    let mut state = EnumState(Vec::new());
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut state as *mut _ as isize));
    }
    sources.extend(state.0);
    sources
}
