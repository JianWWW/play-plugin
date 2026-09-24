//! GPU (D3D11) handle wrappers safe for cross-thread transfer.
//!
//! D3D11 COM interfaces are !Send in windows-rs, but the objects themselves are
//! internally thread-safe once `ID3D11Multithread::SetMultithreadProtected(true)`
//! is set on the device context (done in `create_device`). These wrappers make
//! moving device/texture handles across threads explicit.

use std::sync::Arc;
use windows::core::{Interface, Result};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
};

pub struct SendDevice(ID3D11Device);
unsafe impl Send for SendDevice {}
unsafe impl Sync for SendDevice {}

pub struct SendContext(ID3D11DeviceContext);
unsafe impl Send for SendContext {}
unsafe impl Sync for SendContext {}

pub struct SendTexture(ID3D11Texture2D);
unsafe impl Send for SendTexture {}
unsafe impl Sync for SendTexture {}

/// D3D11 device + immediate context shared between the render/UI thread and the
/// per-stream decode threads (hardware decode attaches to this device).
#[derive(Clone)]
pub struct GpuContext {
    pub device: Arc<SendDevice>,
    pub context: Arc<SendContext>,
}

impl GpuContext {
    pub fn device(&self) -> &ID3D11Device {
        &self.device.0
    }
    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context.0
    }

    /// Creates a hardware D3D11 device (falls back to WARP for headless/CI smoke).
    pub fn create() -> Result<GpuContext> {
        let mut device = None;
        let mut context = None;
        let mut hr = unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                7, // D3D11_SDK_VERSION
                Some(&mut device),
                None::<*mut D3D_FEATURE_LEVEL>,
                Some(&mut context),
            )
        };
        if hr.is_err() {
            // No GPU present (CI VM): fall back to WARP software rasterizer.
            hr = unsafe {
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_WARP,
                    windows::Win32::Foundation::HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_0]),
                    7,
                    Some(&mut device),
                    None::<*mut D3D_FEATURE_LEVEL>,
                    Some(&mut context),
                )
            };
        }
        hr?;
        let device = device.unwrap();
        let context = context.unwrap();
        // The immediate context is used from multiple threads (decode + render);
        // make D3D11 serialize access internally.
        let mt: ID3D11Multithread = device.cast()?;
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
        Ok(GpuContext {
            device: Arc::new(SendDevice(device)),
            context: Arc::new(SendContext(context)),
        })
    }
}

impl GpuContext {
    pub fn wrap_texture(&self, tex: ID3D11Texture2D) -> GpuTexture {
        GpuTexture(Arc::new(SendTexture(tex)))
    }
}

#[derive(Clone)]
pub struct GpuTexture(pub Arc<SendTexture>);

impl GpuTexture {
    pub fn as_raw(&self) -> &ID3D11Texture2D {
        &self.0 .0
    }
}
