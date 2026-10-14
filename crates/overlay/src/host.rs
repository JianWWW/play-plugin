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
        ui: UiConfig,
    },
    SetRect {
        stream_id: u64,
        rect: Rect,
        hidden: bool,
    },
    SetUiState {
        stream_id: u64,
        paused: Option<bool>,
        recording: Option<bool>,
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
    /// Newest not-yet-uploaded frame (latest-wins: a frame superseded before
    /// the UI thread got to it is dropped instead of uploaded).
    pending: Option<VideoFrame>,
    /// Last mouse-over/click on this window: drives hover-to-show controls.
    last_input: Option<std::time::Instant>,
    /// Native control bar / title bar state.
    ui: EntryUi,
}

/// UI-originated events bubbled up to the server: single-click select and
/// native control-bar button presses. Click routing is stateless in the
/// wndproc (see CLICK_TX); hit-testing happens on the UI thread.
#[derive(Clone, Copy, Debug)]
pub enum OverlayUiEvent {
    /// Single click on the overlay (outside the control bar): select.
    Select { stream_id: u64 },
    /// Control-bar button press. `value` carries the new speed step.
    Control {
        stream_id: u64,
        button: ButtonKind,
        value: Option<f64>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    PlayPause,
    Snapshot,
    Record,
    Speed,
}

impl std::fmt::Display for ButtonKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ButtonKind::PlayPause => "playPause",
            ButtonKind::Snapshot => "snapshot",
            ButtonKind::Record => "record",
            ButtonKind::Speed => "speed",
        })
    }
}

pub fn button_kind(s: &str) -> Option<ButtonKind> {
    match s {
        "playPause" => Some(ButtonKind::PlayPause),
        "snapshot" => Some(ButtonKind::Snapshot),
        "record" => Some(ButtonKind::Record),
        "speed" => Some(ButtonKind::Speed),
        _ => None,
    }
}

pub fn default_buttons() -> Vec<String> {
    vec![
        "playPause".into(),
        "snapshot".into(),
        "record".into(),
        "speed".into(),
    ]
}

/// Native UI config per stream, delivered with `stream.open`.
pub struct UiConfig {
    /// Top-bar title; empty = no title bar.
    pub title: String,
    /// Button ids ("playPause" | "snapshot" | "record" | "speed").
    pub buttons: Vec<String>,
    /// Speed steps cycled by the speed button.
    pub speeds: Vec<f64>,
}

/// Live per-stream UI state used for drawing (mirrors pipeline state).
pub struct EntryUi {
    pub cfg: UiConfig,
    pub paused: bool,
    pub recording: bool,
    pub speed_idx: usize,
    /// Mouse recently over the window → show title/control bars.
    pub shown: bool,
    pub dirty: bool,
}

/// Controls auto-hide after this much mouse inactivity.
const HOVER_HIDE_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

impl EntryUi {
    pub fn new(cfg: UiConfig) -> Self {
        let speed_idx = cfg
            .speeds
            .iter()
            .position(|s| (*s - 1.0).abs() < 1e-9)
            .unwrap_or(0);
        Self {
            cfg,
            paused: false,
            recording: false,
            speed_idx,
            shown: false,
            dirty: true,
        }
    }
}

// --- control bar layout (shared by GDI drawing and hit-testing) ----------
pub const TITLE_H: i32 = 28;
pub const BAR_H: i32 = 36;
const BTN_W: i32 = 44;
const BTN_GAP: i32 = 4;
const BTN_X0: i32 = 10;

/// Button rects (l, t, r, b) in client coordinates, laid out left→right.
pub fn button_rects(
    client_w: i32,
    client_h: i32,
    buttons: &[ButtonKind],
) -> Vec<(ButtonKind, (i32, i32, i32, i32))> {
    let _ = client_w;
    let top = client_h - BAR_H;
    buttons
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let l = BTN_X0 + i as i32 * (BTN_W + BTN_GAP);
            (*k, (l, top, l + BTN_W, client_h))
        })
        .collect()
}

pub fn hit_button(
    client_w: i32,
    client_h: i32,
    buttons: &[ButtonKind],
    x: i32,
    y: i32,
) -> Option<ButtonKind> {
    button_rects(client_w, client_h, buttons)
        .into_iter()
        .find(|(_, (l, t, r, b))| x >= *l && x < *r && y >= *t && y < *b)
        .map(|(k, _)| k)
}

// Wndproc is a plain fn pointer with no state access, so click routing uses
// the statics defined below (populated on the same UI thread that runs the
// pump).
#[derive(Clone)]
pub struct OverlayHost {
    cmd_tx: std::sync::mpsc::Sender<HostCommand>,
    frame_tx: std::sync::mpsc::SyncSender<FrameMsg>,
    ui_rx: Arc<Mutex<std::sync::mpsc::Receiver<OverlayUiEvent>>>,
    gpu: GpuContext,
    drop_counters: Arc<Mutex<HashMap<u64, Arc<AtomicU64>>>>,
}

impl OverlayHost {
    pub fn start() -> anyhow::Result<OverlayHost> {
        let gpu = GpuContext::create()?;
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        // Bounded + try_send: if the UI thread ever wedges, frames are dropped
        // (latest-wins) instead of accumulating GBs of decoded video.
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<FrameMsg>(4);
        let (ui_tx, ui_rx) = std::sync::mpsc::channel();
        let (toggle_tx, toggle_rx) = std::sync::mpsc::channel();
        let (click_tx, click_rx) = std::sync::mpsc::channel();
        // Bounded: WM_MOUSEMOVE fires fast; the UI thread drains every tick.
        let (mouse_tx, mouse_rx) = std::sync::mpsc::sync_channel::<(u64, i32, i32)>(64);
        *UI_TX.lock().unwrap() = Some(ui_tx);
        *TOGGLE_TX.lock().unwrap() = Some(toggle_tx);
        *CLICK_TX.lock().unwrap() = Some(click_tx);
        *MOUSE_TX.lock().unwrap() = Some(mouse_tx);
        let ui_rx = Arc::new(Mutex::new(ui_rx));
        let gpu2 = gpu.clone();
        let counters: Arc<Mutex<HashMap<u64, Arc<AtomicU64>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let counters2 = counters.clone();
        std::thread::Builder::new()
            .name("overlay-ui".into())
            .spawn(move || {
                host_main(
                    gpu2,
                    cmd_rx,
                    frame_rx,
                    toggle_rx,
                    click_rx,
                    mouse_rx,
                    counters2,
                )
            })?;
        Ok(OverlayHost {
            cmd_tx,
            frame_tx,
            ui_rx,
            gpu,
            drop_counters: counters,
        })
    }

    /// Non-blocking poll of overlay UI events (single-click select).
    pub fn try_recv_event(&self) -> Option<OverlayUiEvent> {
        self.ui_rx.lock().unwrap().try_recv().ok()
    }

    /// Device handle for hardware decoders to attach to (same adapter/device
    /// as rendering — enables the zero-copy path).
    pub fn gpu(&self) -> GpuContext {
        self.gpu.clone()
    }

    pub fn create(&self, stream_id: u64, rect: Rect, ui: UiConfig) {
        let _ = self.cmd_tx.send(HostCommand::Create { stream_id, rect, ui });
    }

    /// Update control-bar visuals (paused / recording indicators).
    pub fn set_ui_state(&self, stream_id: u64, paused: Option<bool>, recording: Option<bool>) {
        let _ = self.cmd_tx.send(HostCommand::SetUiState {
            stream_id,
            paused,
            recording,
        });
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
        let counter = self
            .drop_counters
            .lock()
            .unwrap()
            .entry(stream_id)
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone();
        // Full channel = UI thread behind: drop this frame, count it.
        if self.frame_tx.try_send(FrameMsg { stream_id, frame }).is_err() {
            counter.fetch_add(1, Ordering::Relaxed);
        }
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
    _toggle_rx: std::sync::mpsc::Receiver<u64>,
    click_rx: std::sync::mpsc::Receiver<(u64, i32, i32, bool)>,
    mouse_rx: std::sync::mpsc::Receiver<(u64, i32, i32)>,
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
            // Ignore per-monitor DPI suggestions: the page's rect sync flow
            // owns overlay geometry, and applying the OS suggested rect would
            // distort the window crossing monitors until the next sync.
            0x02E0 /* WM_DPICHANGED */ => Some(0),
            // Mouse clicks route to the UI thread: hit-testing needs Entry
            // state (client size, configured buttons). Outside the bar:
            // single click selects, double click toggles fullscreen.
            0x0201 /* WM_LBUTTONDOWN */ | 0x0203 /* WM_LBUTTONDBLCLK */ => {
                let stream_id = HWND_STREAM.lock().unwrap().get(&(hwnd.0 as isize)).copied();
                if let Some(stream_id) = stream_id {
                    if let Some(tx) = CLICK_TX.lock().unwrap().as_ref() {
                        let _ = tx.send((
                            stream_id,
                            lparam.0 as i16 as i32,
                            (lparam.0 >> 16) as i16 as i32,
                            msg == 0x0203,
                        ));
                    }
                }
                Some(0)
            }
            // Mouse-over: wakes the hover-to-show control bars (host tick
            // auto-hides after the idle timeout).
            0x0200 /* WM_MOUSEMOVE */ => {
                let stream_id = HWND_STREAM.lock().unwrap().get(&(hwnd.0 as isize)).copied();
                if let Some(stream_id) = stream_id {
                    if let Some(tx) = MOUSE_TX.lock().unwrap().as_ref() {
                        let _ = tx.try_send((
                            stream_id,
                            lparam.0 as i16 as i32,
                            (lparam.0 >> 16) as i16 as i32,
                        ));
                    }
                }
                Some(0)
            }
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

    let trace = std::env::var("PLAY_PLUGIN_UI_TRACE").is_ok();
    // Frame received by the end-of-loop wait; drained first on the next tick.
    let mut woke_frame: Option<FrameMsg> = None;
    loop {
        let t0 = std::time::Instant::now();
        // 1. Commands.
        while let Ok(cmd) = cmd_rx.try_recv() {
            handle_command(state, cmd, &counters);
        }
        // 1b. Mouse clicks routed from the wndproc: control-bar hit-testing
        //     lives here (it needs Entry state). Also handles fullscreen toggles.
        while let Ok((stream_id, x, y, double)) = click_rx.try_recv() {
            if let Some(entry) = state.entries.get_mut(&stream_id) {
                entry.last_input = Some(std::time::Instant::now());
            }
            handle_click(state, stream_id, x, y, double);
        }
        // 1c. Mouse-over tracking: bars show while the cursor is active on a
        //     window and auto-hide HOVER_HIDE_AFTER after the last movement.
        while let Ok((stream_id, _, _)) = mouse_rx.try_recv() {
            if let Some(entry) = state.entries.get_mut(&stream_id) {
                // TEMP DEBUG (hover) — remove after verification.
                static MOUSEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let mn = MOUSEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if mn < 3 {
                    tracing::info!(stream_id, "DBG mouse move received");
                }
                entry.last_input = Some(std::time::Instant::now());
            }
        }
        for entry in state.entries.values_mut() {
            let want = entry
                .last_input
                .is_some_and(|t| t.elapsed() < HOVER_HIDE_AFTER)
                && !entry.hidden;
            if want != entry.ui.shown {
                // TEMP DEBUG (hover) — remove after verification.
                tracing::info!(want, shown = entry.ui.shown, "DBG hover flip");
                entry.ui.shown = want;
                entry.ui.dirty = true;
            }
        }
        if trace {
            eprintln!("[ui] cmds done +{:?}", t0.elapsed());
        }
        // 2. Frames — latest-wins per stream: coalesce while draining, so a
        //    backlog never turns into a burst of stale GPU uploads. Frames
        //    superseded before display (and frames for hidden windows) count
        //    as dropped in the stats counter.
        let mut present_all = false;
        while let Some(fm) = woke_frame.take() {
            coalesce_frame(state, fm, &mut present_all);
        }
        while let Ok(fm) = frame_rx.try_recv() {
            coalesce_frame(state, fm, &mut present_all);
        }
        // 2b. Native UI layer (control bar / title bar): redraw when dirty.
        let mut ui_dirty_any = false;
        for entry in state.entries.values_mut() {
            if entry.hidden || !entry.ui.dirty {
                continue;
            }
            let (cw, ch) = (
                entry.view.client.0.max(1),
                entry.view.client.1.max(1),
            );
            let px = crate::renderer::ui_pixels(&entry.ui, cw, ch);
            state
                .renderer
                .update_ui(&mut entry.view, &px, cw as u32, ch as u32);
            entry.ui.dirty = false;
            entry.view.mark_dirty();
            ui_dirty_any = true;
        }
        let present_all = present_all || ui_dirty_any;
        // 3. Upload each stream's newest frame once, then present dirty views.
        if present_all {
            let t_up = std::time::Instant::now();
            let mut up_ms = 0.0f64;
            for entry in state.entries.values_mut() {
                if entry.hidden {
                    continue;
                }
                if let Some(frame) = entry.pending.take() {
                    state.renderer.update_frame(&mut entry.view, &frame);
                    up_ms = up_ms.max(t_up.elapsed().as_secs_f64() * 1000.0);
                }
            }
            if trace {
                eprintln!("[ui] update max {up_ms:.2}ms");
            }
            let mut pr_ms = 0.0f64;
            let mut flipped = 0usize;
            for entry in state.entries.values_mut() {
                if entry.hidden {
                    continue;
                }
                let t1 = std::time::Instant::now();
                if state.renderer.present(&mut entry.view) {
                    flipped += 1;
                    pr_ms = pr_ms.max(t1.elapsed().as_secs_f64() * 1000.0);
                }
            }
            if trace {
                eprintln!("[ui] present max {pr_ms:.2}ms flipped={flipped}");
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
        // 5. Wait for the next frame or the 8ms tick. A frame arriving wakes
        //    the thread immediately instead of at the next blind tick; idle
        //    cost is unchanged.
        woke_frame = match frame_rx.recv_timeout(Duration::from_millis(8)) {
            Ok(fm) => Some(fm),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            // Sender dropped (teardown): recv would spin — fall back to sleep.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(8));
                None
            }
        };
    }
}

/// Latest-wins intake for one drained frame: hidden streams drop it, visible
/// streams keep only the newest pending frame.
fn coalesce_frame(state: &mut HostState, fm: FrameMsg, present_all: &mut bool) {
    if let Some(entry) = state.entries.get_mut(&fm.stream_id) {
        if entry.hidden {
            entry.drop_counter.fetch_add(1, Ordering::Relaxed);
        } else {
            if entry.pending.replace(fm.frame).is_some() {
                entry.drop_counter.fetch_add(1, Ordering::Relaxed);
            }
            *present_all = true;
        }
    }
}

/// Intersection test against the virtual desktop. An intersecting rect is
/// returned **unchanged**: shrinking it to the desktop (the old behavior)
/// broke the element's aspect ratio whenever the page scrolled the element
/// partially past a maximized browser's edge, and the letterboxed video no
/// longer matched the placeholder ("squashed"). Windows natively clips a
/// window at the desktop edge, so the visible part stays pixel-aligned with
/// the placeholder. A fully offscreen rect hides instead: headless browsers
/// and minimized windows can report far-offscreen coordinates, and that must
/// not pollute the desktop.
/// ponytail: w/h capped at 16384 (D3D11 texture limit) — only triggers on
/// pathological page layouts; cap distorts aspect there, upgrade path is
/// swapping the full-rect quad for a scissored sub-rect.
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
        let visible =
            rect.l < vx + vw && rect.l + rect.w > vx && rect.t < vy + vh && rect.t + rect.h > vy;
        if !visible {
            return (rect, false);
        }
        (
            Rect {
                w: rect.w.min(16384),
                h: rect.h.min(16384),
                ..rect
            },
            true,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression for this bug: a maximized browser sits at the desktop
    /// top edge, so an element scrolled partway out of the viewport has a
    /// negative top — the rect must pass through untouched (visible).
    #[test]
    fn partial_offscreen_keeps_rect_and_visible() {
        let (r, visible) = clamp_rect(Rect { l: 0, t: -100, w: 1000, h: 562 });
        assert!(visible);
        assert_eq!((r.l, r.t, r.w, r.h), (0, -100, 1000, 562));
    }

    #[test]
    fn fully_inside_unchanged() {
        let rect = Rect { l: 100, t: 100, w: 640, h: 360 };
        let (r, visible) = clamp_rect(rect);
        assert!(visible);
        assert_eq!((r.l, r.t, r.w, r.h), (100, 100, 640, 360));
    }

    #[test]
    fn fully_offscreen_hides() {
        // Entirely above the desktop.
        let (r, visible) = clamp_rect(Rect { l: 100, t: -10000, w: 640, h: 360 });
        assert!(!visible);
        assert_eq!((r.l, r.t, r.w, r.h), (100, -10000, 640, 360));
    }

    #[test]
    fn button_layout_and_hit_testing() {
        let kinds: Vec<ButtonKind> = ["playPause", "snapshot", "record", "speed"]
            .iter()
            .filter_map(|s| button_kind(s))
            .collect();
        let (cw, ch) = (640, 360);
        // First button occupies the left end of the bar strip.
        let (k, (l, t, r, b)) = button_rects(cw, ch, &kinds)[0];
        assert_eq!(k, ButtonKind::PlayPause);
        assert_eq!((l, b), (10, ch));
        assert_eq!(t, ch - BAR_H);
        assert_eq!(r - l, 44);
        // Hits: first button, speed button.
        assert_eq!(
            hit_button(cw, ch, &kinds, 20, ch - BAR_H / 2),
            Some(ButtonKind::PlayPause)
        );
        let speed_l = 10 + 3 * (44 + 4);
        assert_eq!(
            hit_button(cw, ch, &kinds, speed_l + 20, ch - 5),
            Some(ButtonKind::Speed)
        );
        // Misses: above the bar, in the 4px gap between buttons 1 and 2,
        // and past the last button.
        assert_eq!(hit_button(cw, ch, &kinds, 20, ch - BAR_H - 1), None);
        assert_eq!(hit_button(cw, ch, &kinds, 10 + 44 + 2, ch - 5), None);
        assert_eq!(hit_button(cw, ch, &kinds, 600, ch - 5), None);
    }

    #[test]
    fn fully_offscreen_each_edge_hides() {
        let vx = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                windows::Win32::UI::WindowsAndMessaging::SM_XVIRTUALSCREEN,
            )
        };
        let vy = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                windows::Win32::UI::WindowsAndMessaging::SM_YVIRTUALSCREEN,
            )
        };
        let vw = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                windows::Win32::UI::WindowsAndMessaging::SM_CXVIRTUALSCREEN,
            )
        };
        let vh = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                windows::Win32::UI::WindowsAndMessaging::SM_CYVIRTUALSCREEN,
            )
        };
        let far = 1_000_000;
        for rect in [
            Rect { l: vx, t: vy - 400, w: 640, h: 360 },          // above
            Rect { l: vx, t: vy + vh + 40, w: 640, h: 360 },      // below
            Rect { l: vx - 700, t: vy, w: 640, h: 360 },          // left
            Rect { l: vx + vw + 60, t: vy, w: 640, h: 360 },      // right
            Rect { l: far, t: far, w: 640, h: 360 },              // far offscreen
        ] {
            let (_, visible) = clamp_rect(rect);
            assert!(!visible, "rect {rect:?} should be hidden");
        }
    }
}

fn handle_command(
    state: &mut HostState,
    cmd: HostCommand,
    counters: &Arc<Mutex<HashMap<u64, Arc<AtomicU64>>>>,
) {
    match cmd {
        HostCommand::Create { stream_id, rect, ui } => {
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
                                pending: None,
                                last_input: None,
                                ui: EntryUi::new(ui),
                            },
                        );
                        HWND_STREAM
                            .lock()
                            .unwrap()
                            .insert(state.entries[&stream_id].view.hwnd.0 as isize, stream_id);
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
                // Auto-create with an empty UI (rect arrived before open —
                // should not happen; open always sends Create first).
                let ui = UiConfig {
                    title: String::new(),
                    buttons: Vec::new(),
                    speeds: vec![1.0],
                };
                handle_command(
                    state,
                    HostCommand::Create { stream_id, rect, ui },
                    counters,
                );
                return;
            }
            apply_rect(state, stream_id, rect, hidden);
            // Client size changed → redraw the control bar.
            if let Some(entry) = state.entries.get_mut(&stream_id) {
                entry.ui.dirty = true;
            }
        }
        HostCommand::SetUiState {
            stream_id,
            paused,
            recording,
        } => {
            if let Some(entry) = state.entries.get_mut(&stream_id) {
                if let Some(p) = paused {
                    entry.ui.paused = p;
                    entry.ui.dirty = true;
                }
                if let Some(r) = recording {
                    entry.ui.recording = r;
                    entry.ui.dirty = true;
                }
            }
        }
        HostCommand::Close { stream_id } => {
            if let Some(entry) = state.entries.remove(&stream_id) {
                HWND_STREAM
                    .lock()
                    .unwrap()
                    .remove(&(entry.view.hwnd.0 as isize));
                FS_SAVED
                    .lock()
                    .unwrap()
                    .remove(&(entry.view.hwnd.0 as isize));
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

static HWND_STREAM: std::sync::LazyLock<Mutex<HashMap<isize, u64>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
static UI_TX: std::sync::LazyLock<Mutex<Option<std::sync::mpsc::Sender<OverlayUiEvent>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));
/// Mouse clicks from the wndproc: (stream, x, y, double). Consumed only by
/// the host loop, which owns the hit-testing state.
static CLICK_TX:
    std::sync::LazyLock<
        Mutex<Option<std::sync::mpsc::Sender<(u64, i32, i32, bool)>>,
    >,
> = std::sync::LazyLock::new(|| Mutex::new(None));
static TOGGLE_TX: std::sync::LazyLock<Mutex<Option<std::sync::mpsc::Sender<u64>>>> =
    std::sync::LazyLock::new(|| Mutex::new(None));
/// Mouse-over from the wndproc: (stream, x, y). Drives hover-to-show bars.
static MOUSE_TX: std::sync::LazyLock<
    Mutex<Option<std::sync::mpsc::SyncSender<(u64, i32, i32)>>>,
> = std::sync::LazyLock::new(|| Mutex::new(None));
/// Saved window rect per hwnd while that window is user-toggled fullscreen.
static FS_SAVED: std::sync::LazyLock<Mutex<HashMap<isize, Rect>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Double-click fullscreen toggle: covers the whole virtual screen, restores
/// the previous rect on the second double click.
fn toggle_fullscreen(state: &mut HostState, stream_id: u64) {
    let Some(entry) = state.entries.get_mut(&stream_id) else {
        return;
    };
    let hwnd = entry.view.hwnd;
    let key = hwnd.0 as isize;
    // Bind BEFORE branching: an `if let` scrutinee guard survives into the
    // else block (edition 2021), so re-locking FS_SAVED in the else deadlocked
    // the overlay UI thread on the first double click (frozen windows).
    let saved = FS_SAVED.lock().unwrap().remove(&key);
    unsafe {
        if let Some(saved) = saved {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                hwnd,
                None,
                saved.l,
                saved.t,
                saved.w.max(1),
                saved.h.max(1),
                windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
            );
            state.renderer.resize(&mut entry.view, saved.w, saved.h);
        } else {
            let mut rc = windows::Win32::Foundation::RECT::default();
            if windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rc).is_err() {
                return;
            }
            // Fullscreen covers the monitor the window is on, not the whole
            // virtual desktop (which spans every monitor in multi-display
            // setups).
            let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO {
                cbSize: std::mem::size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
                ..Default::default()
            };
            let monitor = windows::Win32::Graphics::Gdi::MonitorFromWindow(
                hwnd,
                windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
            );
            if !windows::Win32::Graphics::Gdi::GetMonitorInfoW(monitor, &mut mi).as_bool() {
                return;
            }
            let (vx, vy, vw, vh) = (
                mi.rcMonitor.left,
                mi.rcMonitor.top,
                mi.rcMonitor.right - mi.rcMonitor.left,
                mi.rcMonitor.bottom - mi.rcMonitor.top,
            );
            FS_SAVED.lock().unwrap().insert(
                key,
                Rect {
                    l: rc.left,
                    t: rc.top,
                    w: rc.right - rc.left,
                    h: rc.bottom - rc.top,
                },
            );
            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::HWND_TOPMOST,
                vx,
                vy,
                vw,
                vh,
                windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
            );
            state.renderer.resize(&mut entry.view, vw, vh);
        }
    }
    tracing::info!(
        stream_id,
        fullscreen = FS_SAVED.lock().unwrap().contains_key(&key),
        "overlay fullscreen toggled"
    );
}

/// Mouse click dispatch: control bar first (buttons swallow the click),
/// then legacy behavior (single = select, double = fullscreen toggle).
fn handle_click(state: &mut HostState, stream_id: u64, x: i32, y: i32, double: bool) {
    let Some(entry) = state.entries.get_mut(&stream_id) else {
        return;
    };
    let (cw, ch) = (entry.view.client.0.max(1), entry.view.client.1.max(1));
    if y >= ch - BAR_H {
        // Inside the control bar strip: buttons swallow; empty space too
        // (no select / no fullscreen from the bar).
        let kinds: Vec<ButtonKind> = entry
            .ui
            .cfg
            .buttons
            .iter()
            .filter_map(|s| button_kind(s))
            .collect();
        if let Some(k) = hit_button(cw, ch, &kinds, x, y) {
            let value = if k == ButtonKind::Speed {
                let n = entry.ui.cfg.speeds.len().max(1);
                entry.ui.speed_idx = (entry.ui.speed_idx + 1) % n;
                entry.ui.dirty = true;
                entry.ui.cfg.speeds.get(entry.ui.speed_idx).copied()
            } else {
                None
            };
            if let Some(tx) = UI_TX.lock().unwrap().as_ref() {
                let _ = tx.send(OverlayUiEvent::Control {
                    stream_id,
                    button: k,
                    value,
                });
            }
        }
        return;
    }
    if double {
        toggle_fullscreen(state, stream_id);
    } else if let Some(tx) = UI_TX.lock().unwrap().as_ref() {
        let _ = tx.send(OverlayUiEvent::Select { stream_id });
    }
}

fn apply_rect(state: &mut HostState, stream_id: u64, rect: Rect, hidden: bool) {
    let Some(entry) = state.entries.get_mut(&stream_id) else {
        return;
    };
    // While user-toggled fullscreen, keep the fullscreen geometry; the page's
    // scroll/zoom rect syncs must not shrink the window back.
    let hwnd_key = entry.view.hwnd.0 as isize;
    if FS_SAVED.lock().unwrap().contains_key(&hwnd_key) {
        return;
    }
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
