//! Overlay host: owns the render device and all overlay HWNDs on a dedicated
//! UI thread. The rest of the process talks to it via channels.

use crate::renderer::{Renderer, StreamView};
use crate::window;
use plugin_core::gpu::GpuContext;
use plugin_core::{CpuFrame, PipelineEvent, VideoFrame};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    pub l: i32,
    pub t: i32,
    pub w: i32,
    pub h: i32,
}

pub enum HostCommand {
    Create {
        stream_id: u64,
        rect: Rect,
    },
    SetRect {
        stream_id: u64,
        rect: Rect,
        hidden: bool,
    },
    Close {
        stream_id: u64,
    },
    Snapshot {
        stream_id: u64,
        reply: std::sync::mpsc::SyncSender<Option<CpuFrame>>,
    },
    Probe {
        reply: std::sync::mpsc::SyncSender<usize>,
    },
}

struct FrameMsg {
    stream_id: u64,
    frame: VideoFrame,
}

struct Entry {
    view: StreamView,
    hidden: bool,
    owner: Option<windows::Win32::Foundation::HWND>,
    drop_counter: Arc<AtomicU64>,
}

#[derive(Clone)]
pub struct OverlayHost {
    cmd_tx: std::sync::mpsc::Sender<HostCommand>,
    frame_tx: std::sync::mpsc::Sender<FrameMsg>,
    gpu: GpuContext,
    drop_counters: Arc<Mutex<HashMap<u64, Arc<AtomicU64>>>>,
}

impl OverlayHost {
    pub fn start() -> anyhow::Result<OverlayHost> {
        let gpu = GpuContext::create()?;
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (frame_tx, frame_rx) = std::sync::mpsc::channel();
        let gpu2 = gpu.clone();
        let counters: Arc<Mutex<HashMap<u64, Arc<AtomicU64>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let counters2 = counters.clone();
        std::thread::Builder::new()
            .name("overlay-ui".into())
            .spawn(move || host_main(gpu2, cmd_rx, frame_rx, counters2))?;
        Ok(OverlayHost {
            cmd_tx,
            frame_tx,
            gpu,
            drop_counters: counters,
        })
    }

    /// Device handle for hardware decoders to attach to (same adapter/device
    /// as rendering — enables the zero-copy path).
    pub fn gpu(&self) -> GpuContext {
        self.gpu.clone()
    }

    pub fn create(&self, stream_id: u64, rect: Rect) {
        let _ = self.cmd_tx.send(HostCommand::Create { stream_id, rect });
    }

    pub fn set_rect(&self, stream_id: u64, rect: Rect, hidden: bool) {
        let _ = self.cmd_tx.send(HostCommand::SetRect {
            stream_id,
            rect,
            hidden,
        });
    }

    pub fn close(&self, stream_id: u64) {
        let _ = self.cmd_tx.send(HostCommand::Close { stream_id });
        self.drop_counters.lock().unwrap().remove(&stream_id);
    }

    /// Latest-wins frame delivery. If the UI thread is behind, this frame
    /// replaces (not queues behind) the pending one.
    pub fn set_frame(&self, stream_id: u64, frame: VideoFrame) {
        self.drop_counters
            .lock()
            .unwrap()
            .entry(stream_id)
            .or_insert_with(|| Arc::new(AtomicU64::new(0)));
        let _ = self.frame_tx.send(FrameMsg { stream_id, frame });
    }

    pub fn dropped_frames(&self, stream_id: u64) -> u64 {
        self.drop_counters
            .lock()
            .unwrap()
            .get(&stream_id)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Number of live overlay windows (blocking, UI thread roundtrip).
    pub fn window_count(&self) -> usize {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let _ = self.cmd_tx.send(HostCommand::Probe { reply: tx });
        rx.recv_timeout(Duration::from_secs(1)).unwrap_or(0)
    }

    /// Reads back the currently displayed frame (blocking, UI thread roundtrip).
    pub fn snapshot(&self, stream_id: u64) -> Option<CpuFrame> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let _ = self.cmd_tx.send(HostCommand::Snapshot {
            stream_id,
            reply: tx,
        });
        rx.recv_timeout(Duration::from_secs(1)).ok().flatten()
    }
}

struct HostState {
    renderer: Renderer,
    entries: HashMap<u64, Entry>,
}

fn host_main(
    gpu: GpuContext,
    cmd_rx: std::sync::mpsc::Receiver<HostCommand>,
    frame_rx: std::sync::mpsc::Receiver<FrameMsg>,
    counters: Arc<Mutex<HashMap<u64, Arc<AtomicU64>>>>,
) {
    let renderer = match Renderer::new(gpu) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "D3D11 renderer init failed, overlay disabled");
            // Drain forever so the rest of the process keeps working.
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
    };
    let state = &mut HostState {
        renderer,
        entries: HashMap::new(),
    };

    // Route window messages into our pump.
    fn wndproc_hook(
        hwnd: windows::Win32::Foundation::HWND,
        msg: u32,
        wparam: windows::Win32::Foundation::WPARAM,
        lparam: windows::Win32::Foundation::LPARAM,
    ) -> Option<isize> {
        match msg {
            // We draw on our own tick; validate the update region, otherwise
            // WM_PAINT regenerates forever and starves the message pump.
            0x000F /* WM_PAINT */ => {
                unsafe {
                    let _ = windows::Win32::Graphics::Gdi::ValidateRect(hwnd, None);
                }
                Some(0)
            }
            0x0014 /* WM_ERASEBKGND */ => Some(1),
            0x0002 /* WM_DESTROY */ => Some(0),
            _ => {
                let _ = (hwnd, wparam, lparam);
                None
            }
        }
    }
    *window::WNDPROC_TARGET.lock().unwrap() = Some(wndproc_hook);

    if let Err(e) = window::register_overlay_class() {
        tracing::error!(error = %e, "RegisterClass failed, overlay disabled");
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }

    loop {
        let trace = std::env::var("PLAY_PLUGIN_UI_TRACE").is_ok();
        let t0 = std::time::Instant::now();
        // 1. Commands.
        while let Ok(cmd) = cmd_rx.try_recv() {
            handle_command(state, cmd, &counters);
        }
        if trace {
            eprintln!("[ui] cmds done +{:?}", t0.elapsed());
        }
        // 2. Frames (latest-wins: we drain fully each tick and draw the last
        //    state; hidden windows count their frames as dropped).
        let mut present_all = false;
        while let Ok(fm) = frame_rx.try_recv() {
            if let Some(entry) = state.entries.get_mut(&fm.stream_id) {
                if entry.hidden {
                    entry.drop_counter.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                state.renderer.update_frame(&mut entry.view, &fm.frame);
                present_all = true;
            }
        }
        // 3. Present dirty views.
        if present_all {
            for entry in state.entries.values_mut() {
                if !entry.hidden {
                    state.renderer.present(&mut entry.view);
                }
            }
        }
        if trace {
            eprintln!("[ui] frames+present done +{:?}", t0.elapsed());
        }
        static HEARTBEAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = HEARTBEAT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n.is_multiple_of(125) {
            tracing::info!(loop_n = n, entries = state.entries.len(), tag = "alive");
        }
        // 4. Message pump (non-blocking) so windows stay responsive.
        if trace {
            eprintln!("[ui] pre-pump +{:?}", t0.elapsed());
        }
        unsafe {
            let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
            while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                &mut msg,
                None,
                0,
                0,
                windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
            )
            .as_bool()
            {
                let _ = windows::Win32::UI::WindowsAndMessaging::TranslateMessage(&msg);
                windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(8));
    }
}

/// Clamps a page-supplied rect to the virtual desktop. Headless browsers and
/// minimized windows can report far-offscreen coordinates; a fully offscreen
/// rect collapses to a hidden window instead of polluting the desktop.
fn clamp_rect(rect: Rect) -> (Rect, bool) {
    unsafe {
        let vx = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_XVIRTUALSCREEN,
        );
        let vy = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_YVIRTUALSCREEN,
        );
        let vw = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CXVIRTUALSCREEN,
        );
        let vh = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CYVIRTUALSCREEN,
        );
        let (l, t0, r, b) = (rect.l, rect.t, rect.l + rect.w, rect.t + rect.h);
        let cl = l.max(vx);
        let ct = t0.max(vy);
        let cr = r.min(vx + vw);
        let cb = b.min(vy + vh);
        let visible = cr > cl && cb > ct;
        (
            Rect {
                l: cl,
                t: ct,
                w: (cr - cl).max(1),
                h: (cb - ct).max(1),
            },
            visible,
        )
    }
}

fn handle_command(
    state: &mut HostState,
    cmd: HostCommand,
    counters: &Arc<Mutex<HashMap<u64, Arc<AtomicU64>>>>,
) {
    match cmd {
        HostCommand::Create { stream_id, rect } => {
            if state.entries.contains_key(&stream_id) {
                apply_rect(state, stream_id, rect, false);
                return;
            }
            let (rect, on_screen) = clamp_rect(rect);
            let cx = rect.l + rect.w / 2;
            let cy = rect.t + rect.h / 2;
            let owner = window::find_browser_owner(cx, cy);
            match window::create_overlay_window(
                windows::core::w!("PlayPlugin"),
                owner,
                rect.l,
                rect.t,
                rect.w,
                rect.h,
            ) {
                Ok(hwnd) => match state.renderer.create_view(hwnd) {
                    Ok(view) => {
                        window::show_noactivate(hwnd, on_screen);
                        let counter = counters
                            .lock()
                            .unwrap()
                            .entry(stream_id)
                            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
                            .clone();
                        state.entries.insert(
                            stream_id,
                            Entry {
                                view,
                                hidden: !on_screen,
                                owner,
                                drop_counter: counter,
                            },
                        );
                        tracing::info!(
                            stream_id,
                            owner = owner.map(|h| h.0 as usize).unwrap_or(0),
                            "overlay window created"
                        );
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "create_view failed");
                        unsafe {
                            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
                        }
                    }
                },
                Err(e) => tracing::error!(error = %e, "CreateWindow failed"),
            }
        }
        HostCommand::SetRect {
            stream_id,
            rect,
            hidden,
        } => {
            if !state.entries.contains_key(&stream_id) && rect.w > 0 && rect.h > 0 {
                handle_command(state, HostCommand::Create { stream_id, rect }, counters);
                return;
            }
            apply_rect(state, stream_id, rect, hidden);
        }
        HostCommand::Close { stream_id } => {
            if let Some(entry) = state.entries.remove(&stream_id) {
                unsafe {
                    let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(entry.view.hwnd);
                }
                counters.lock().unwrap().remove(&stream_id);
                tracing::info!(stream_id, "overlay window closed");
            }
        }
        HostCommand::Probe { reply } => {
            let _ = reply.send(state.entries.len());
        }
        HostCommand::Snapshot { stream_id, reply } => {
            let frame = state
                .entries
                .get(&stream_id)
                .and_then(|e| crate::renderer::readback(&state.renderer.gpu, &e.view));
            let _ = reply.send(frame);
        }
    }
}

fn apply_rect(state: &mut HostState, stream_id: u64, rect: Rect, hidden: bool) {
    let Some(entry) = state.entries.get_mut(&stream_id) else {
        return;
    };
    let (rect, on_screen) = clamp_rect(rect);
    let visible = !hidden && on_screen;
    // Re-bind owner if the browser window died (e.g. tab moved to a new window).
    let owner_alive = entry
        .owner
        .map(|h| unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(h) }.as_bool())
        .unwrap_or(false);
    if !owner_alive && entry.owner.is_some() {
        let cx = rect.l + rect.w / 2;
        let cy = rect.t + rect.h / 2;
        entry.owner = window::find_browser_owner(cx, cy);
        if let Some(o) = entry.owner {
            window::set_owner(entry.view.hwnd, o);
        }
    }
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
            entry.view.hwnd,
            None,
            rect.l,
            rect.t,
            rect.w.max(1),
            rect.h.max(1),
            windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE
                | windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
        );
    }
    window::show_noactivate(entry.view.hwnd, visible);
    entry.hidden = !visible;
    if visible {
        state.renderer.resize(&mut entry.view, rect.w, rect.h);
    }
}

/// Convenience used by the server: route video frames into the overlay.
pub fn route_pipeline_event(host: &OverlayHost, stream_id: u64, ev: &PipelineEvent) {
    if let PipelineEvent::Video(frame) = ev {
        host.set_frame(stream_id, frame.clone());
    }
}
