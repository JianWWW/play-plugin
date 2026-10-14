//! D3D11 rendering for overlay windows: one swapchain per stream window, one
//! shared device. Frames arrive either as CPU BGRA (software decode / test
//! source) or as D3D11 textures (hardware decode — GPU-only copy path).

use plugin_core::gpu::{GpuContext, GpuTexture};
use plugin_core::{CpuFrame, VideoFrame};
use std::collections::HashMap;
use std::sync::Arc;
use windows::core::{s, Interface, Result};
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11BlendState, ID3D11Buffer, ID3D11DeviceContext, ID3D11InputLayout, ID3D11PixelShader,
    ID3D11RasterizerState, ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11VertexShader, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_FLAG,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VERTEX_BUFFER,
    D3D11_BLEND_DESC, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_OP_ADD, D3D11_BLEND_ONE,
    D3D11_BLEND_SRC_ALPHA, D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL,
    D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_READ, D3D11_CULL_NONE, D3D11_FILL_SOLID,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_INPUT_ELEMENT_DESC, D3D11_INPUT_PER_VERTEX_DATA,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_RASTERIZER_DESC, D3D11_RENDER_TARGET_BLEND_DESC,
    D3D11_SAMPLER_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VIEWPORT, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R32G32_FLOAT,
    DXGI_FORMAT_UNKNOWN, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_PRESENT_TEST,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

const VS_SRC: &[u8] = b"
struct VSIn { float2 pos : POSITION; float2 uv : TEXCOORD0; };
struct VSOut { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };
VSOut main(VSIn i) { VSOut o; o.pos = float4(i.pos, 0, 1); o.uv = i.uv; return o; }
";

const PS_SRC: &[u8] = b"
struct VSOut { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };
cbuffer CB : register(b0) { float2 scale; float2 offset; };
Texture2D tex : register(t0);
SamplerState samp : register(s0);
float4 main(VSOut i) : SV_Target { return tex.Sample(samp, saturate((i.uv - 0.5) * scale + 0.5 + offset)); }
";

#[repr(C)]
struct QuadVertex {
    pos: [f32; 2],
    uv: [f32; 2],
}

#[repr(C)]
struct CBData {
    scale: [f32; 2],
    offset: [f32; 2],
}

/// NV12→BGRA converter state for one view, rebuilt when the frame size
/// changes. Video-processor views are driver allocations, so they are cached
/// instead of re-created per frame: d3d11va decodes into a single texture
/// array for the decoder's lifetime — only the array slice varies per frame.
struct VpState {
    size: (u32, u32),
    vp: ID3D11VideoProcessor,
    ve: ID3D11VideoProcessorEnumerator,
    /// Output view bound to the current staging texture.
    out: Option<ID3D11VideoProcessorOutputView>,
    /// Base pointer of the decoder texture array this cache is valid for;
    /// a new pointer means a new decoder pool → drop `ins`.
    src: usize,
    /// Input views keyed by texture-array slice.
    ins: HashMap<u32, ID3D11VideoProcessorInputView>,
}

pub struct StreamView {
    pub hwnd: HWND,
    swapchain: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    tex: Option<ID3D11Texture2D>,
    srv: Option<ID3D11ShaderResourceView>,
    /// Native UI layer (control bar / title bar), full-window, opaque.
    ui_tex: Option<ID3D11Texture2D>,
    ui_srv: Option<ID3D11ShaderResourceView>,
    vp: Option<VpState>,
    tex_size: (u32, u32),
    tex_size_ui: (u32, u32),
    pub client: (i32, i32),
    dirty: bool,
}

pub struct Renderer {
    pub gpu: GpuContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    layout: ID3D11InputLayout,
    vbuf: ID3D11Buffer,
    cbuf: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    rs: ID3D11RasterizerState,
    ui_blend: ID3D11BlendState,
    /// Video device/context for the NV12→BGRA path, cast once. `None` on
    /// devices without video support (e.g. WARP without VIDEO_SUPPORT) —
    /// the gpu frame path is skipped, same as before.
    vd: Option<ID3D11VideoDevice>,
    vc: Option<ID3D11VideoContext>,
}

impl Renderer {
    pub fn new(gpu: GpuContext) -> Result<Renderer> {
        unsafe {
            let device = gpu.device();
            let (vs_blob, ps_blob) = (
                compile_shader(VS_SRC, "vs_5_0")?,
                compile_shader(PS_SRC, "ps_5_0")?,
            );
            let vs_bytes = blob_bytes(&vs_blob)?;
            let ps_bytes = blob_bytes(&ps_blob)?;
            let mut vs_opt: Option<ID3D11VertexShader> = None;
            device.CreateVertexShader(vs_bytes, None, Some(&mut vs_opt))?;
            let vs = vs_opt.unwrap();
            let mut ps_opt: Option<ID3D11PixelShader> = None;
            device.CreatePixelShader(ps_bytes, None, Some(&mut ps_opt))?;
            let ps = ps_opt.unwrap();

            let layout_desc = [
                D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: s!("POSITION"),
                    SemanticIndex: 0,
                    Format: DXGI_FORMAT_R32G32_FLOAT,
                    InputSlot: 0,
                    AlignedByteOffset: 0,
                    InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                    InstanceDataStepRate: 0,
                },
                D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: s!("TEXCOORD"),
                    SemanticIndex: 0,
                    Format: DXGI_FORMAT_R32G32_FLOAT,
                    InputSlot: 0,
                    AlignedByteOffset: 8,
                    InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                    InstanceDataStepRate: 0,
                },
            ];
            let mut layout_opt: Option<ID3D11InputLayout> = None;
            device.CreateInputLayout(&layout_desc, vs_bytes, Some(&mut layout_opt))?;
            let layout = layout_opt.unwrap();

            let verts = [
                QuadVertex {
                    pos: [-1.0, -1.0],
                    uv: [0.0, 1.0],
                },
                QuadVertex {
                    pos: [1.0, -1.0],
                    uv: [1.0, 1.0],
                },
                QuadVertex {
                    pos: [-1.0, 1.0],
                    uv: [0.0, 0.0],
                },
                QuadVertex {
                    pos: [1.0, 1.0],
                    uv: [1.0, 0.0],
                },
            ];
            let mut vbuf_opt: Option<ID3D11Buffer> = None;
            device.CreateBuffer(
                &buffer_desc(
                    std::mem::size_of_val(&verts) as u32,
                    D3D11_BIND_VERTEX_BUFFER,
                ),
                Some(&D3D11_SUBRESOURCE_DATA {
                    pSysMem: verts.as_ptr() as _,
                    SysMemPitch: 0,
                    SysMemSlicePitch: 0,
                }),
                Some(&mut vbuf_opt),
            )?;
            let vbuf = vbuf_opt.unwrap();
            let mut cbuf_opt: Option<ID3D11Buffer> = None;
            device.CreateBuffer(
                &buffer_desc(16, D3D11_BIND_CONSTANT_BUFFER),
                None,
                Some(&mut cbuf_opt),
            )?;
            let cbuf = cbuf_opt.unwrap();
            let mut sampler_opt: Option<ID3D11SamplerState> = None;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    MipLODBias: 0.0,
                    MaxAnisotropy: 1,
                    ComparisonFunc: D3D11_COMPARISON_NEVER,
                    BorderColor: [0.0; 4],
                    MinLOD: 0.0,
                    MaxLOD: f32::MAX,
                },
                Some(&mut sampler_opt),
            )?;
            let sampler = sampler_opt.unwrap();
            // Fullscreen quad: cull nothing — the triangle-strip winding is
            // back-facing under D3D's Y-down front-face rule, and the default
            // CULL_BACK rasterizer state silently dropped every quad (black
            // window with a perfectly healthy texture behind it).
            let mut rs_opt: Option<ID3D11RasterizerState> = None;
            device.CreateRasterizerState(
                &D3D11_RASTERIZER_DESC {
                    FillMode: D3D11_FILL_SOLID,
                    CullMode: D3D11_CULL_NONE,
                    FrontCounterClockwise: BOOL(0),
                    DepthBias: 0,
                    DepthBiasClamp: 0.0,
                    SlopeScaledDepthBias: 0.0,
                    DepthClipEnable: BOOL(0),
                    ScissorEnable: BOOL(0),
                    MultisampleEnable: BOOL(0),
                    AntialiasedLineEnable: BOOL(0),
                    ..Default::default()
                },
                Some(&mut rs_opt),
            )?;
            let rs = rs_opt.unwrap();
            // UI layer alpha blending: the UI texture is opaque only in the
            // bar/title strips, transparent elsewhere (video shows through).
            let mut blend_opt: Option<ID3D11BlendState> = None;
            device.CreateBlendState(
                &D3D11_BLEND_DESC {
                    AlphaToCoverageEnable: BOOL(0),
                    IndependentBlendEnable: BOOL(0),
                    RenderTarget: [D3D11_RENDER_TARGET_BLEND_DESC {
                        BlendEnable: BOOL(1),
                        SrcBlend: D3D11_BLEND_SRC_ALPHA,
                        DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                        BlendOp: D3D11_BLEND_OP_ADD,
                        SrcBlendAlpha: D3D11_BLEND_ONE,
                        DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                        BlendOpAlpha: D3D11_BLEND_OP_ADD,
                        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
                    },
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    Default::default()],
                },
                Some(&mut blend_opt),
            )?;
            let ui_blend = blend_opt.unwrap();
            let vd: Option<ID3D11VideoDevice> = device.cast().ok();
            let vc: Option<ID3D11VideoContext> = gpu.context().cast().ok();

            Ok(Renderer {
                gpu,
                vs,
                ps,
                layout,
                vbuf,
                cbuf,
                sampler,
                rs,
                ui_blend,
                vd,
                vc,
            })
        }
    }

    pub fn create_view(&self, hwnd: HWND) -> Result<StreamView> {
        unsafe {
            let mut rect = windows::Win32::Foundation::RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect);
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: (rect.right - rect.left).max(1) as u32,
                Height: (rect.bottom - rect.top).max(1) as u32,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: 0,
                Stereo: BOOL::default(),
            };
            let swapchain = {
                let factory: IDXGIFactory2 = CreateDXGIFactory1()?;
                factory.CreateSwapChainForHwnd(self.gpu.device(), hwnd, &desc, None, None)?
            };
            let mut view = StreamView {
                hwnd,
                swapchain,
                rtv: None,
                tex: None,
                srv: None,
                ui_tex: None,
                ui_srv: None,
                vp: None,
                tex_size: (0, 0),
                tex_size_ui: (0, 0),
                client: (rect.right - rect.left, rect.bottom - rect.top),
                dirty: true,
            };
            self.recreate_rtv(&mut view)?;
            Ok(view)
        }
    }

    fn recreate_rtv(&self, v: &mut StreamView) -> Result<()> {
        unsafe {
            v.rtv = None;
            let back: ID3D11Texture2D = v.swapchain.GetBuffer(0)?;
            let mut rtv_opt: Option<ID3D11RenderTargetView> = None;
            self.gpu
                .device()
                .CreateRenderTargetView(&back, None, Some(&mut rtv_opt))?;
            v.rtv = Some(rtv_opt.unwrap());
        }
        Ok(())
    }

    pub fn resize(&self, v: &mut StreamView, w: i32, h: i32) {
        let (w, h) = (w.max(1), h.max(1));
        if v.client == (w, h) {
            return;
        }
        v.client = (w, h);
        v.dirty = true;
        unsafe {
            v.rtv = None;
            if let Err(e) = v.swapchain.ResizeBuffers(
                0,
                w as u32,
                h as u32,
                DXGI_FORMAT_UNKNOWN,
                windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_FLAG(0),
            ) {
                tracing::warn!(error = %e, "ResizeBuffers failed");
                return;
            }
            let _ = self.recreate_rtv(v);
        }
    }

    pub fn update_frame(&self, v: &mut StreamView, frame: &VideoFrame) {
        match frame {
            VideoFrame::Cpu(f) => self.update_frame_cpu(v, f),
            VideoFrame::Gpu(tex, sub, w, h) => self.update_frame_gpu(v, tex, *sub, *w, *h),
        }
    }

    fn ensure_texture(&self, v: &mut StreamView, w: u32, h: u32) {
        if v.tex_size == (w, h) && v.tex.is_some() && v.srv.is_some() {
            return;
        }
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex_opt: Option<ID3D11Texture2D> = None;
            match self
                .gpu
                .device()
                .CreateTexture2D(&desc, None, Some(&mut tex_opt))
            {
                Ok(()) => {
                    let tex = tex_opt.unwrap();
                    let mut srv_opt: Option<ID3D11ShaderResourceView> = None;
                    match self
                        .gpu
                        .device()
                        .CreateShaderResourceView(&tex, None, Some(&mut srv_opt))
                    {
                        Ok(()) => {
                            v.tex = Some(tex);
                            v.srv = Some(srv_opt.unwrap());
                            v.tex_size = (w, h);
                        }
                        Err(e) => tracing::warn!(error = %e, "CreateShaderResourceView failed"),
                    }
                }
                Err(e) => tracing::warn!(error = %e, "CreateTexture2D failed"),
            }
        }
    }

    fn update_frame_cpu(&self, v: &mut StreamView, f: &CpuFrame) {
        self.ensure_texture(v, f.width, f.height);
        let Some(tex) = v.tex.as_ref() else { return };
        unsafe {
            self.gpu.context().UpdateSubresource(
                tex,
                0,
                None,
                f.data.as_ptr() as _,
                f.stride as u32,
                0,
            );
        }
        v.dirty = true;
    }

    fn update_frame_gpu(&self, v: &mut StreamView, src: &GpuTexture, sub: u32, w: u32, h: u32) {
        {
            use std::sync::atomic::{AtomicBool, Ordering};
            static SEEN: AtomicBool = AtomicBool::new(false);
            if !SEEN.swap(true, Ordering::Relaxed) {
                tracing::info!(w, h, sub, "gpu frame arrived at renderer");
            }
        }
        let (Some(vd), Some(vc)) = (self.vd.as_ref(), self.vc.as_ref()) else {
            return;
        };
        self.ensure_texture(v, w, h);
        let Some(tex) = v.tex.as_ref() else { return };
        // Decoded frames are NV12 (Y + interleaved UV planes) on a texture
        // array; the render pipeline wants BGRA. CopySubresourceRegion
        // cannot convert formats — use the hardware video processor.
        if v.vp.as_ref().map(|p| p.size != (w, h)).unwrap_or(true) {
            match Self::make_video_processor(vd, vc, w, h) {
                Ok(p) => v.vp = Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "video processor unavailable");
                    return;
                }
            }
        }
        let Some(p) = v.vp.as_mut() else { return };
        if p.out.is_none() {
            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut out_view = None;
            if let Err(e) = unsafe {
                vd.CreateVideoProcessorOutputView(tex, &p.ve, &out_desc, Some(&mut out_view))
            } {
                tracing::warn!(error = %e, "CreateVideoProcessorOutputView failed");
                return;
            }
            p.out = out_view;
        }
        // New base pointer = new decoder texture array → cache is stale.
        let src_ptr = Interface::as_raw(src.as_raw()) as usize;
        if p.src != src_ptr {
            p.ins.clear();
            p.src = src_ptr;
        }
        let in_view = match p.ins.get(&sub) {
            Some(view) => view.clone(),
            None => {
                let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV {
                            MipSlice: 0,
                            ArraySlice: sub,
                        },
                    },
                };
                let mut in_view = None;
                match unsafe {
                    vd.CreateVideoProcessorInputView(
                        src.as_raw(),
                        &p.ve,
                        &in_desc,
                        Some(&mut in_view),
                    )
                } {
                    Ok(()) => {
                        let view = in_view.unwrap();
                        p.ins.insert(sub, view.clone());
                        view
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "CreateVideoProcessorInputView failed");
                        return;
                    }
                }
            }
        };
        let Some(out_view) = p.out.clone() else {
            return;
        };
        unsafe {
            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: BOOL(1),
                pInputSurface: std::mem::ManuallyDrop::new(Some(in_view)),
                ..Default::default()
            }];
            let blt = vc.VideoProcessorBlt(&p.vp, &out_view, 0, &streams);
            // pInputSurface is ManuallyDrop: release the per-frame view ref.
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
            if let Err(e) = blt {
                tracing::warn!(error = %e, "VideoProcessorBlt failed");
            }
        }
        v.dirty = true;
    }

    fn make_video_processor(
        vd: &ID3D11VideoDevice,
        vc: &ID3D11VideoContext,
        w: u32,
        h: u32,
    ) -> windows::core::Result<VpState> {
        let rate = DXGI_RATIONAL {
            Numerator: 25,
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: w,
            InputHeight: h,
            OutputWidth: w,
            OutputHeight: h,
            OutputFrameRate: rate,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let ve = unsafe { vd.CreateVideoProcessorEnumerator(&content)? };
        let vp = unsafe { vd.CreateVideoProcessor(&ve, 0)? };
        // Stream state persists on the processor object — set once instead
        // of per frame.
        unsafe {
            let _ =
                vc.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            // BT.709 studio range (bit 5 = matrix, bits 6-7 = nominal range).
            // ponytail: calibration knob — use (2 << 6) full-range or clear bit
            // 5 for BT.601 if colors look washed on a given camera.
            let _ = vc.VideoProcessorSetStreamColorSpace(
                &vp,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: (1 << 5) | (1 << 6),
                },
            );
        }
        Ok(VpState {
            size: (w, h),
            vp,
            ve,
            out: None,
            src: 0,
            ins: HashMap::new(),
        })
    }

    /// Uploads the native UI layer (full-window opaque BGRA). Recreates the
    /// texture on size change, otherwise updates in place.
    pub fn update_ui(&self, v: &mut StreamView, pixels: &[u8], w: u32, h: u32) {
        unsafe {
            let device = self.gpu.device();
            let ctx: &ID3D11DeviceContext = self.gpu.context();
            let need_new = v.ui_tex.is_none() || v.tex_size_ui != (w, h);
            if need_new {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut tex_opt: Option<ID3D11Texture2D> = None;
                if device
                    .CreateTexture2D(&desc, None, Some(&mut tex_opt))
                    .is_err()
                {
                    return;
                }
                let tex = tex_opt.unwrap();
                let mut srv_opt: Option<ID3D11ShaderResourceView> = None;
                if device
                    .CreateShaderResourceView(&tex, None, Some(&mut srv_opt))
                    .is_err()
                {
                    return;
                }
                v.ui_tex = Some(tex);
                v.ui_srv = srv_opt;
                v.tex_size_ui = (w, h);
            }
            if let Some(tex) = v.ui_tex.as_ref() {
                ctx.UpdateSubresource(tex, 0, None, pixels.as_ptr() as _, (w * 4) as _, 0);
            }
        }
    }

    /// Draws the letterboxed quad and presents. Cheap when not dirty.
    /// Returns true if a frame was submitted. `dirty` is only cleared on a
    /// real flip — a skipped/deferred window stays dirty for the next tick.
    pub fn present(&self, v: &mut StreamView) -> bool {
        if !v.dirty {
            return false;
        }
        let Some(rtv) = v.rtv.as_ref() else {
            return false;
        };
        let (ww, wh) = (v.client.0.max(1) as f32, v.client.1.max(1) as f32);
        let (fw, fh) = (v.tex_size.0.max(1) as f32, v.tex_size.1.max(1) as f32);
        let aspect_win = ww / wh;
        let aspect_frame = fw / fh;
        let cb = CBData {
            scale: [
                (aspect_frame / aspect_win).min(1.0),
                (aspect_win / aspect_frame).min(1.0),
            ],
            offset: [0.0, 0.0],
        };

        unsafe {
            let ctx: &ID3D11DeviceContext = self.gpu.context();
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            // Video pass: opaque (reset whatever the last UI pass left).
            ctx.OMSetBlendState(None, None, 0xffffffff);
            let vp = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: ww,
                Height: wh,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[vp]));
            ctx.RSSetState(&self.rs);
            ctx.ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 1.0]);
            ctx.IASetInputLayout(&self.layout);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            let stride = std::mem::size_of::<QuadVertex>() as u32;
            let vbuf_slot: Option<ID3D11Buffer> = Some(self.vbuf.clone());
            ctx.IASetVertexBuffers(0, 1, Some(&vbuf_slot), Some(&stride), Some(&0u32));
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.UpdateSubresource(&self.cbuf, 0, None, &cb as *const _ as _, 0, 0);
            let cbuf_slot: Option<ID3D11Buffer> = Some(self.cbuf.clone());
            ctx.PSSetConstantBuffers(0, Some(&[cbuf_slot]));
            if let Some(srv) = v.srv.as_ref() {
                let srv_slot: Option<ID3D11ShaderResourceView> = Some(srv.clone());
                ctx.PSSetShaderResources(0, Some(&[srv_slot]));
            }
            let sampler_slot: Option<ID3D11SamplerState> = Some(self.sampler.clone());
            ctx.PSSetSamplers(0, Some(&[sampler_slot]));
            ctx.Draw(4, 0);
            // Native UI layer on top: alpha-blended so only the bar/title
            // strips (opaque pixels) cover the video.
            if let Some(ui_srv) = v.ui_srv.as_ref() {
                ctx.OMSetBlendState(Some(&self.ui_blend), None, 0xffffffff);
                let cb_ui = CBData {
                    scale: [1.0, 1.0],
                    offset: [0.0, 0.0],
                };
                let srv_slot: Option<ID3D11ShaderResourceView> = Some(ui_srv.clone());
                ctx.PSSetShaderResources(0, Some(&[srv_slot]));
                ctx.UpdateSubresource(&self.cbuf, 0, None, &cb_ui as *const _ as _, 0, 0);
                ctx.Draw(4, 0);
            }
            // A fully occluded window would block Present(sync=1) forever;
            // probe with TEST first and skip this tick when occluded.
            if v.swapchain.Present(0, DXGI_PRESENT_TEST).is_ok() {
                // sync=0: no vsync wait — WARP / RDP sessions can block
                // indefinitely on sync=1. Serial sync=1 presents would also
                // cap the UI thread at 60/N fps (measured).
                let _ = v.swapchain.Present(0, DXGI_PRESENT(0));
                v.dirty = false; // flipped: window is up to date
                true
            } else {
                // Occluded: nothing to show — treat as presented so the tick
                // budget accounts for it.
                v.dirty = false;
                true
            }
        }
    }
}

fn blob_bytes(blob: &windows::Win32::Graphics::Direct3D::ID3DBlob) -> Result<&'static [u8]> {
    unsafe {
        Ok(std::slice::from_raw_parts(
            blob.GetBufferPointer() as *const u8,
            blob.GetBufferSize(),
        ))
    }
}

/// Reads back the current texture (GPU→CPU, used for snapshots).
pub fn readback(gpu: &GpuContext, v: &StreamView) -> Option<plugin_core::CpuFrame> {
    let tex = v.tex.as_ref()?;
    let (w, h) = v.tex_size;
    if w == 0 || h == 0 {
        return None;
    }
    unsafe {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging_opt: Option<ID3D11Texture2D> = None;
        gpu.device()
            .CreateTexture2D(&desc, None, Some(&mut staging_opt))
            .ok()?;
        let staging = staging_opt?;
        gpu.context().CopyResource(&staging, tex);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.context()
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .ok()?;
        let src_stride = mapped.RowPitch as usize;
        let row_bytes = (w as usize) * 4;
        let mut data = Vec::with_capacity(row_bytes * h as usize);
        let src = mapped.pData as *const u8;
        for y in 0..h as usize {
            data.extend_from_slice(std::slice::from_raw_parts(
                src.add(y * src_stride),
                row_bytes,
            ));
        }
        gpu.context().Unmap(&staging, 0);
        Some(plugin_core::CpuFrame {
            width: w,
            height: h,
            stride: row_bytes,
            data: Arc::new(data),
        })
    }
}

fn buffer_desc(byte_width: u32, bind: D3D11_BIND_FLAG) -> D3D11_BUFFER_DESC {
    D3D11_BUFFER_DESC {
        ByteWidth: byte_width,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    }
}

fn compile_shader(
    src: &[u8],
    target: &str,
) -> Result<windows::Win32::Graphics::Direct3D::ID3DBlob> {
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        let target_c = std::ffi::CString::new(target).unwrap();
        D3DCompile(
            src.as_ptr() as _,
            src.len(),
            None,
            None,
            None,
            s!("main"),
            windows::core::PCSTR(target_c.as_ptr() as *const u8),
            0,
            0,
            &mut blob,
            Some(&mut errors),
        )
        .inspect_err(|_| {
            if let Some(e) = errors.as_ref() {
                let text = String::from_utf8_lossy(std::slice::from_raw_parts(
                    e.GetBufferPointer() as *const u8,
                    e.GetBufferSize(),
                ));
                eprintln!("shader compile failed ({target}): {text}");
            }
        })?;
        blob.ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(-1)))
    }
}

impl StreamView {
    /// Forces the next present (used after UI-layer updates).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }
}

// --- native UI layer (title bar + control bar), GDI-rendered ---------------

/// Renders the UI layer into a full-window **opaque** BGRA buffer via GDI.
/// ponytail: no alpha compositing — bars draw their own dark background;
/// switch to D2D if translucency is ever needed.
pub fn ui_pixels(ui: &crate::host::EntryUi, cw: i32, ch: i32) -> Vec<u8> {
    use windows::Win32::Graphics::Gdi as g;
    const BG: u32 = 0x00_10_10_10; // 0x00BBGGRR near-black
    let (w, h) = (cw.max(1) as usize, ch.max(1) as usize);
    let mut out = vec![0u8; w * h * 4];
    // Hover-to-show: no recent mouse → fully transparent, video untouched.
    if !ui.shown {
        return out;
    }
    unsafe {
        let hdc = g::CreateCompatibleDC(None);
        let bmi = g::BITMAPINFO {
            bmiHeader: g::BITMAPINFOHEADER {
                biSize: std::mem::size_of::<g::BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: g::BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let Ok(hbm) = g::CreateDIBSection(hdc, &bmi, g::DIB_RGB_COLORS, &mut bits, None, 0)
        else {
            let _ = g::DeleteDC(hdc);
            return out;
        };
        let old_bm = g::SelectObject(hdc, windows::Win32::Graphics::Gdi::HGDIOBJ::from(hbm));
        let brush = g::CreateSolidBrush(windows::Win32::Foundation::COLORREF(BG));
        g::SetBkMode(hdc, g::TRANSPARENT);

        // Symbol font for button glyphs; YaHei for text (Chinese titles).
        let sym_font = g::CreateFontW(
            18,
            0,
            0,
            0,
            g::FW_NORMAL.0 as i32,
            0,
            0,
            0,
            g::DEFAULT_CHARSET.0 as u32,
            0,
            0,
            g::CLEARTYPE_QUALITY.0 as u32,
            0,
            windows::core::w!("Segoe UI Symbol"),
        );
        let text_font = g::CreateFontW(
            15,
            0,
            0,
            0,
            g::FW_NORMAL.0 as i32,
            0,
            0,
            0,
            g::DEFAULT_CHARSET.0 as u32,
            0,
            0,
            g::CLEARTYPE_QUALITY.0 as u32,
            0,
            windows::core::w!("Microsoft YaHei UI"),
        );

        let dt = |text: &mut [u16],
                  mut rect: windows::Win32::Foundation::RECT,
                  color: u32,
                  font: windows::Win32::Graphics::Gdi::HFONT| {
            g::SelectObject(hdc, windows::Win32::Graphics::Gdi::HGDIOBJ::from(font));
            g::SetTextColor(hdc, windows::Win32::Foundation::COLORREF(color));
            g::DrawTextW(
                hdc,
                text,
                &mut rect,
                g::DT_CENTER | g::DT_VCENTER | g::DT_SINGLELINE,
            );
        };

        // Control bar strip.
        g::FillRect(
            hdc,
            &windows::Win32::Foundation::RECT {
                left: 0,
                top: ch - crate::host::BAR_H,
                right: cw,
                bottom: ch,
            },
            brush,
        );
        // Title bar (only when a title was configured).
        if !ui.cfg.title.is_empty() {
            g::FillRect(
                hdc,
                &windows::Win32::Foundation::RECT {
                    left: 0,
                    top: 0,
                    right: cw,
                    bottom: crate::host::TITLE_H,
                },
                brush,
            );
            let mut title: Vec<u16> = ui.cfg.title.encode_utf16().collect();
            title.push(0);
            g::SelectObject(hdc, windows::Win32::Graphics::Gdi::HGDIOBJ::from(text_font));
            g::SetTextColor(hdc, windows::Win32::Foundation::COLORREF(0x00_E8_E8_E8));
            g::DrawTextW(
                hdc,
                &mut title,
                &mut windows::Win32::Foundation::RECT {
                    left: 10,
                    top: 0,
                    right: cw - 10,
                    bottom: crate::host::TITLE_H,
                },
                g::DT_LEFT | g::DT_VCENTER | g::DT_SINGLELINE | g::DT_END_ELLIPSIS,
            );
        }

        // Buttons.
        let kinds: Vec<crate::host::ButtonKind> = ui
            .cfg
            .buttons
            .iter()
            .filter_map(|s| crate::host::button_kind(s))
            .collect();
        for (k, (l, t, r, b)) in crate::host::button_rects(cw, ch, &kinds) {
            let mut rect = windows::Win32::Foundation::RECT {
                left: l,
                top: t,
                right: r,
                bottom: b,
            };
            match k {
                crate::host::ButtonKind::PlayPause => {
                    let mut glyph: Vec<u16> = if ui.paused {
                        "\u{25B6}".encode_utf16().collect()
                    } else {
                        "\u{23F8}".encode_utf16().collect()
                    };
                    dt(&mut glyph, rect, 0x00_E8_E8_E8, sym_font);
                }
                crate::host::ButtonKind::Snapshot => {
                    let mut glyph: Vec<u16> = "\u{25A3}".encode_utf16().collect();
                    dt(&mut glyph, rect, 0x00_E8_E8_E8, sym_font);
                }
                crate::host::ButtonKind::Record => {
                    let mut glyph: Vec<u16> = "\u{23FA}".encode_utf16().collect();
                    dt(
                        &mut glyph,
                        rect,
                        if ui.recording { 0x00_50_50_FF } else { 0x00_E8_E8_E8 },
                        sym_font,
                    );
                }
                crate::host::ButtonKind::Speed => {
                    let v = ui.cfg.speeds.get(ui.speed_idx).copied().unwrap_or(1.0);
                    let label = if (v - v.round()).abs() < 1e-9 {
                        format!("{}x", v.round() as i64)
                    } else {
                        format!("{v}x")
                    };
                    let mut t16: Vec<u16> = label.encode_utf16().collect();
                    t16.push(0);
                    g::SelectObject(hdc, windows::Win32::Graphics::Gdi::HGDIOBJ::from(text_font));
                    g::SetTextColor(
                        hdc,
                        windows::Win32::Foundation::COLORREF(0x00_E8_E8_E8),
                    );
                    g::DrawTextW(
                        hdc,
                        &mut t16,
                        &mut rect,
                        g::DT_CENTER | g::DT_VCENTER | g::DT_SINGLELINE,
                    );
                }
            }
        }

        // Copy DIB → out. Only the bar/title strips are opaque; the rest of
        // the window stays alpha 0 so the video quad shows through (the UI
        // quad is drawn with alpha blending in present()).
        std::ptr::copy_nonoverlapping(bits as *const u8, out.as_mut_ptr(), out.len());
        let title_h = if ui.cfg.title.is_empty() {
            0
        } else {
            crate::host::TITLE_H as usize
        };
        let bar_top = (h - crate::host::BAR_H as usize).max(title_h);
        for (y, row) in out.chunks_exact_mut(w * 4).enumerate() {
            let a = if y < title_h || y >= bar_top { 255u8 } else { 0u8 };
            for px in row.chunks_exact_mut(4) {
                px[3] = a;
            }
        }

        g::SelectObject(hdc, old_bm);
        let _ = g::DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ::from(hbm));
        let _ = g::DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ::from(brush));
        let _ = g::DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ::from(sym_font));
        let _ = g::DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ::from(text_font));
        let _ = g::DeleteDC(hdc);
    }
    out
}
