//! Reproduces renderer init outside the app (run: cargo test -p plugin-overlay -- --nocapture).

use plugin_core::gpu::GpuContext;
use plugin_core::{CpuFrame, VideoFrame};
use std::sync::Arc;

fn test_frame(w: u32, h: u32) -> CpuFrame {
    CpuFrame {
        width: w,
        height: h,
        stride: (w * 4) as usize,
        data: Arc::new(vec![0x40u8; (w * h * 4) as usize]),
    }
}

#[test]
fn renderer_init() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let gpu = GpuContext::create().expect("d3d device");
    let r = plugin_overlay::renderer::Renderer::new(gpu);
    assert!(r.is_ok(), "renderer init failed: {:?}", r.err());
}

/// Regression: double-click fullscreen resizes the swapchain. After a
/// present() the immediate context still binds the back-buffer RTV, so
/// ResizeBuffers failed and left `rtv = None` forever — the view stopped
/// drawing (frozen video). Resize must keep the view renderable.
#[test]
fn resize_after_present_still_presents() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let gpu = GpuContext::create().expect("d3d device");
    let r = plugin_overlay::renderer::Renderer::new(gpu).unwrap();
    plugin_overlay::window::register_overlay_class().unwrap();
    let hwnd = plugin_overlay::window::create_overlay_window(
        windows::core::w!("resize-test"),
        None,
        10,
        10,
        320,
        240,
    )
    .unwrap();
    let mut v = r.create_view(hwnd).unwrap();

    r.update_frame(&mut v, &VideoFrame::Cpu(test_frame(320, 240)));
    assert!(r.present(&mut v), "present before resize");

    // Simulates the double-click fullscreen toggle (also exercises the
    // restore path: resize back).
    r.resize(&mut v, 1920, 1080);
    r.update_frame(&mut v, &VideoFrame::Cpu(test_frame(320, 240)));
    assert!(r.present(&mut v), "present after resize (fullscreen)");

    r.resize(&mut v, 320, 240);
    r.update_frame(&mut v, &VideoFrame::Cpu(test_frame(320, 240)));
    assert!(r.present(&mut v), "present after resize (restore)");
}
