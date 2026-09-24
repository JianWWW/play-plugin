//! D3D11 rendering for overlay windows: one swapchain per stream window, one
//! shared device. Frames arrive either as CPU BGRA (software decode / test
//! source) or as D3D11 textures (hardware decode — GPU-only copy path).

use plugin_core::gpu::{GpuContext, GpuTexture};
use plugin_core::{CpuFrame, VideoFrame};
use std::sync::Arc;
use windows::core::{s, Result};
use windows::Win32::Foundation::{BOOL, HWND};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11DeviceContext, ID3D11InputLayout, ID3D11PixelShader,
    ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_FLAG, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VERTEX_BUFFER, D3D11_BUFFER_DESC,
    D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_READ, D3D11_FILTER_MIN_MAG_MIP_LINEAR,
    D3D11_INPUT_ELEMENT_DESC, D3D11_INPUT_PER_VERTEX_DATA, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SAMPLER_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R32G32_FLOAT,
    DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
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

pub struct StreamView {
    pub hwnd: HWND,
    swapchain: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    tex: Option<ID3D11Texture2D>,
    srv: Option<ID3D11ShaderResourceView>,
    tex_size: (u32, u32),
    client: (i32, i32),
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

            Ok(Renderer {
                gpu,
                vs,
                ps,
                layout,
                vbuf,
                cbuf,
                sampler,
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
                tex_size: (0, 0),
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
        self.ensure_texture(v, w, h);
        let Some(tex) = v.tex.as_ref() else { return };
        unsafe {
            self.gpu
                .context()
                .CopySubresourceRegion(tex, 0, 0, 0, 0, src.as_raw(), sub, None);
        }
        v.dirty = true;
    }

    /// Draws the letterboxed quad and presents. Cheap when not dirty.
    pub fn present(&self, v: &mut StreamView) {
        if !v.dirty {
            return;
        }
        v.dirty = false;
        let Some(rtv) = v.rtv.as_ref() else { return };
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
            let vp = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: ww,
                Height: wh,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[vp]));
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
            // A fully occluded window would block Present(sync=1) forever;
            // probe with TEST first and skip this tick when occluded.
            if v.swapchain.Present(0, DXGI_PRESENT_TEST).is_ok() {
                // sync=0: no vsync wait — WARP / RDP sessions can block
                // indefinitely on sync=1, and lowest latency is the goal.
                let _ = v.swapchain.Present(0, DXGI_PRESENT(0));
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
