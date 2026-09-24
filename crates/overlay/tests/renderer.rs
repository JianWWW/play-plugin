//! Reproduces renderer init outside the app (run: cargo test -p plugin-overlay -- --nocapture).

use plugin_core::gpu::GpuContext;

#[test]
fn renderer_init() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let gpu = GpuContext::create().expect("d3d device");
    let r = plugin_overlay::renderer::Renderer::new(gpu);
    assert!(r.is_ok(), "renderer init failed: {:?}", r.err());
}
