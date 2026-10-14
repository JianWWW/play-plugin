//! Regression: double-clicking an overlay window (fullscreen toggle) must not
//! wedge the overlay UI thread. Drives the real path: posted mouse messages →
//! wndproc → ToggleFullscreen → SetWindowPos + swapchain resize, then asserts
//! the UI thread still answers probes and keeps presenting frames.
//!
//! Run: cargo test -p plugin-overlay --test fullscreen_toggle -- --nocapture

use plugin_core::{CpuFrame, PipelineEvent, VideoFrame};
use plugin_overlay::host::{OverlayHost, Rect};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn test_frame(w: u32, h: u32) -> CpuFrame {
    CpuFrame {
        width: w,
        height: h,
        stride: (w * 4) as usize,
        data: Arc::new(vec![0x40u8; (w * h * 4) as usize]),
    }
}

/// Finds this process's overlay window by class name.
fn find_overlay_hwnd() -> Option<isize> {
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowThreadProcessId};
    unsafe {
        let hwnd = FindWindowW(windows::core::w!("PlayPluginOverlay"), None).unwrap_or_default();
        if hwnd.is_invalid() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        (pid == std::process::id()).then_some(hwnd.0 as isize)
    }
}

fn double_click(hwnd: isize) {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        PostMessageW, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    };
    let h = HWND(hwnd as *mut _);
    let pt = |x: i32, y: i32| LPARAM((y << 16) as isize | x as isize);
    unsafe {
        // Down/up + dblclk/up: the CS_DBLCLKS press sequence.
        let _ = PostMessageW(h, WM_LBUTTONDOWN, WPARAM(1), pt(50, 50)); // MK_LBUTTON
        let _ = PostMessageW(h, WM_LBUTTONUP, WPARAM(0), pt(50, 50));
        let _ = PostMessageW(h, WM_LBUTTONDBLCLK, WPARAM(1), pt(50, 50));
        let _ = PostMessageW(h, WM_LBUTTONUP, WPARAM(0), pt(50, 50));
    }
}

fn single_click(hwnd: isize) {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_LBUTTONDOWN, WM_LBUTTONUP};
    let h = HWND(hwnd as *mut _);
    let pt = |x: i32, y: i32| LPARAM((y << 16) as isize | x as isize);
    unsafe {
        let _ = PostMessageW(h, WM_LBUTTONDOWN, WPARAM(1), pt(50, 50)); // MK_LBUTTON
        let _ = PostMessageW(h, WM_LBUTTONUP, WPARAM(0), pt(50, 50));
    }
}

#[test]
fn double_click_fullscreen_stays_responsive() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    std::env::set_var("PLAY_PLUGIN_UI_TRACE", "1");

    let host = OverlayHost::start().unwrap();
    // Feed one animated stream (test source) like smoke does.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = plugin_core::spawn_stream(
        plugin_core::StreamOptions {
            url: "test://pattern?w=320&h=240&fps=30".into(),
            muted: true,
            title: String::new(),
        },
        Some(host.gpu()),
        true,
        tx,
    );
    let h = host.clone();
    std::thread::spawn(move || {
        while let Some(ev) = rx.blocking_recv() {
            if let PipelineEvent::Video(VideoFrame::Cpu(_)) = &ev {
                // Route as-is (Gpu variant also fine — same API).
            }
            if let PipelineEvent::Video(f) = ev {
                h.set_frame(1, f);
            }
        }
    });
    handle
        .close
        .store(false, std::sync::atomic::Ordering::Relaxed);

    host.create(
        1,
        Rect {
            l: 10,
            t: 10,
            w: 320,
            h: 240,
        },
        plugin_overlay::host::UiConfig {
            title: String::new(),
            buttons: Vec::new(),
            speeds: vec![1.0],
        },
    );
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(host.window_count(), 1, "overlay window missing");

    let hwnd = find_overlay_hwnd().expect("overlay hwnd not found");
    let s1 = host.snapshot(1).expect("snapshot before toggle");

    // Routing: single click must reach the server via try_recv_event (the
    // host loop used to drain and eat it).
    single_click(hwnd);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        matches!(
            host.try_recv_event(),
            Some(plugin_overlay::host::OverlayUiEvent::Select { stream_id: 1 })
        ),
        "single click never reached try_recv_event"
    );
    assert!(host.try_recv_event().is_none(), "stale select event queued");

    // --- the bug trigger: double click = fullscreen toggle ---
    eprintln!("[test] sending double click");
    double_click(hwnd);
    std::thread::sleep(Duration::from_millis(700));

    // Distinguish "wedged UI thread" (probe timeout) from "window destroyed".
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsWindow};
    let alive = unsafe { IsWindow(HWND(hwnd as *mut _)).as_bool() };
    let mut rc = windows::Win32::Foundation::RECT::default();
    let rect = if alive {
        unsafe {
            let _ = GetWindowRect(HWND(hwnd as *mut _), &mut rc);
            Some((rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top))
        }
    } else {
        None
    };
    eprintln!("[test] hwnd alive={alive} rect={rect:?}");

    // The UI thread must still answer blocking roundtrips quickly.
    let t0 = Instant::now();
    let n = host.window_count();
    let probe_ms = t0.elapsed().as_millis();
    eprintln!("[test] window_count={n} probe={probe_ms}ms");
    assert_eq!(
        n, 1,
        "window lost after fullscreen toggle (alive={alive}, rect={rect:?})"
    );
    assert!(
        probe_ms < 500,
        "UI thread wedged after toggle (probe {probe_ms}ms)"
    );

    // ...and the window actually went fullscreen (toggle must not be lost).
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };
    let (vx, vy) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    };
    let (vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    assert_eq!(
        rect,
        Some((vx, vy, vw, vh)),
        "double click did not enter fullscreen (toggle event lost?)"
    );

    // Toggle handled internally: the double click's first press sends a Select
    // (page keeps selection), but the toggle itself must never leak here.
    let _ = host.try_recv_event(); // drain the Select from the first press
    assert!(
        host.try_recv_event().is_none(),
        "fullscreen toggle leaked into the select event channel"
    );

    // ...and keep presenting frames (animation advances).
    let s2 = host.snapshot(1).expect("snapshot after toggle");
    std::thread::sleep(Duration::from_millis(300));
    let s3 = host.snapshot(1).expect("snapshot after toggle (2)");
    assert_ne!(s2.data, s3.data, "video frozen after fullscreen toggle");

    // Restore (second double click) and re-check.
    double_click(hwnd);
    std::thread::sleep(Duration::from_millis(700));
    let n = host.window_count();
    assert_eq!(n, 1, "window lost after restore");
    let s4 = host.snapshot(1).expect("snapshot after restore");
    assert_ne!(s3.data, s4.data, "video frozen after restore");

    let _ = s1; // content equality not asserted; liveness is the signal
    handle
        .close
        .store(true, std::sync::atomic::Ordering::Relaxed);
}
