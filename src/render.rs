use std::ffi::c_void;

use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2D, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, PCSTR, s};

use crate::native;

const SHADER: &str = r#"
Texture2D<float> luma : register(t0);
Texture2D<float2> chroma : register(t1);
SamplerState linear_sampler : register(s0);

struct VsOut {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

VsOut vs_main(uint id : SV_VertexID) {
    VsOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.uv = uv;
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

float4 ps_main(VsOut i) : SV_Target {
    float y = luma.Sample(linear_sampler, i.uv);
    float2 uv = chroma.Sample(linear_sampler, i.uv);
    y = (y - 16.0 / 255.0) * (255.0 / 219.0);
    float cb = (uv.x - 128.0 / 255.0) * (255.0 / 224.0);
    float cr = (uv.y - 128.0 / 255.0) * (255.0 / 224.0);
    float3 rgb = float3(
        y + 1.5748 * cr,
        y - 0.1873 * cb - 0.4681 * cr,
        y + 1.8556 * cb);
    return float4(saturate(rgb), 1.0);
}
"#;

pub struct VideoSurface {
    pub width: u32,
    pub height: u32,
    format: i32,
    yuv: ID3D11Texture2D,
    luma_srv: ID3D11ShaderResourceView,
    chroma_srv: ID3D11ShaderResourceView,
    _rgb: ID3D11Texture2D,
    rgb_rtv: ID3D11RenderTargetView,
    pub texture_id: egui::TextureId,
    pub sequence: u64,
    pub has_frame: bool,
}

pub struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    back_buffer: Option<ID3D11RenderTargetView>,
    egui: egui_directx11::Renderer,
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    video: Option<VideoSurface>,
    size: (u32, u32),
}

fn compile(source: &str, entry: PCSTR, target: PCSTR) -> Result<ID3DBlob, String> {
    let mut blob = None;
    let mut errors = None;
    let result = unsafe {
        D3DCompile(
            source.as_ptr() as *const c_void,
            source.len(),
            s!("pslc_video.hlsl"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut blob,
            Some(&mut errors),
        )
    };
    if let Err(e) = result {
        let detail = errors
            .map(|b| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    b.GetBufferPointer() as *const u8,
                    b.GetBufferSize(),
                ))
                .into_owned()
            })
            .unwrap_or_default();
        return Err(format!("Shader compilation failed: {e} {detail}"));
    }
    blob.ok_or_else(|| "Pusty shader".to_string())
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    }
}

impl Renderer {
    pub fn new(hwnd: HWND, width: u32, height: u32) -> Result<Self, String> {
        let err = |what: &str, e: windows::core::Error| format!("{what}: {e}");
        let mut device = None;
        let mut context = None;
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                flags,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(|e| err("Cannot create a Direct3D 11 device", e))?;
        let device: ID3D11Device = device.unwrap();
        let context: ID3D11DeviceContext = context.unwrap();

        if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = multithread.SetMultithreadProtected(true);
            }
        }

        let dxgi_device: IDXGIDevice = device.cast().map_err(|e| err("DXGI", e))?;
        let adapter = unsafe { dxgi_device.GetAdapter() }.map_err(|e| err("DXGI adapter", e))?;
        if let Ok(desc) = unsafe { adapter.GetDesc() } {
            let name = String::from_utf16_lossy(&desc.Description);
            log::info!("GPU: {}", name.trim_end_matches('\0'));
        }
        let factory: IDXGIFactory2 =
            unsafe { adapter.GetParent() }.map_err(|e| err("DXGI factory", e))?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width.max(1),
            Height: height.max(1),
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
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
        };
        let swap_chain =
            unsafe { factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None) }
                .map_err(|e| err("Cannot create the swap chain", e))?;
        unsafe {
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
        }

        let vs_blob = compile(SHADER, s!("vs_main"), s!("vs_5_0"))?;
        let ps_blob = compile(SHADER, s!("ps_main"), s!("ps_5_0"))?;
        let mut vertex_shader = None;
        let mut pixel_shader = None;
        unsafe {
            device
                .CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vertex_shader))
                .map_err(|e| err("Vertex shader", e))?;
            device
                .CreatePixelShader(blob_bytes(&ps_blob), None, Some(&mut pixel_shader))
                .map_err(|e| err("Pixel shader", e))?;
        }
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler = None;
        unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler)) }
            .map_err(|e| err("Sampler", e))?;

        let egui = egui_directx11::Renderer::new(&device).map_err(|e| err("Renderer egui", e))?;
        let mut renderer = Self {
            device,
            context,
            swap_chain,
            back_buffer: None,
            egui,
            vertex_shader: vertex_shader.unwrap(),
            pixel_shader: pixel_shader.unwrap(),
            sampler: sampler.unwrap(),
            video: None,
            size: (width.max(1), height.max(1)),
        };
        renderer.create_back_buffer()?;
        Ok(renderer)
    }

    pub fn device_ptr(&self) -> *mut c_void {
        self.device.as_raw()
    }

    fn create_back_buffer(&mut self) -> Result<(), String> {
        let texture: ID3D11Texture2D =
            unsafe { self.swap_chain.GetBuffer(0) }.map_err(|e| e.to_string())?;
        let mut rtv = None;
        unsafe {
            self.device
                .CreateRenderTargetView(&texture, None, Some(&mut rtv))
        }
        .map_err(|e| e.to_string())?;
        self.back_buffer = rtv;
        Ok(())
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) == self.size {
            return;
        }
        self.back_buffer = None;
        unsafe {
            self.context.OMSetRenderTargets(None, None);
            self.context.Flush();
        }
        match unsafe {
            self.swap_chain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )
        } {
            Ok(()) => self.size = (width, height),
            Err(e) => log::error!("ResizeBuffers: {e}"),
        }
        if let Err(e) = self.create_back_buffer() {
            log::error!("Back buffer: {e}");
        }
    }

    pub fn video(&self) -> Option<&VideoSurface> {
        self.video.as_ref()
    }

    pub fn drop_video(&mut self) {
        if let Some(video) = self.video.take() {
            self.egui.unregister_user_texture(video.texture_id);
        }
    }

    fn ensure_video(&mut self, width: u32, height: u32, format: i32) -> Result<(), String> {
        if let Some(v) = &self.video
            && v.width == width
            && v.height == height
            && v.format == format
        {
            return Ok(());
        }
        self.drop_video();
        let (yuv_format, luma_format, chroma_format) = if format == native::FORMAT_P010 {
            (
                DXGI_FORMAT_P010,
                DXGI_FORMAT_R16_UNORM,
                DXGI_FORMAT_R16G16_UNORM,
            )
        } else {
            (
                DXGI_FORMAT_NV12,
                DXGI_FORMAT_R8_UNORM,
                DXGI_FORMAT_R8G8_UNORM,
            )
        };
        let yuv_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: yuv_format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let rgb_desc = D3D11_TEXTURE2D_DESC {
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            ..yuv_desc
        };
        let srv_desc = |format: DXGI_FORMAT| D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        unsafe {
            let mut yuv = None;
            self.device
                .CreateTexture2D(&yuv_desc, None, Some(&mut yuv))
                .map_err(|e| format!("Tekstura YUV: {e}"))?;
            let yuv = yuv.unwrap();
            let mut luma_srv = None;
            let mut chroma_srv = None;
            self.device
                .CreateShaderResourceView(&yuv, Some(&srv_desc(luma_format)), Some(&mut luma_srv))
                .map_err(|e| format!("SRV luma: {e}"))?;
            self.device
                .CreateShaderResourceView(
                    &yuv,
                    Some(&srv_desc(chroma_format)),
                    Some(&mut chroma_srv),
                )
                .map_err(|e| format!("SRV chroma: {e}"))?;
            let mut rgb = None;
            self.device
                .CreateTexture2D(&rgb_desc, None, Some(&mut rgb))
                .map_err(|e| format!("Tekstura RGB: {e}"))?;
            let rgb = rgb.unwrap();
            let mut rgb_rtv = None;
            let mut rgb_srv = None;
            self.device
                .CreateRenderTargetView(&rgb, None, Some(&mut rgb_rtv))
                .map_err(|e| format!("RTV: {e}"))?;
            self.device
                .CreateShaderResourceView(&rgb, None, Some(&mut rgb_srv))
                .map_err(|e| format!("SRV RGB: {e}"))?;
            let rgb_rtv = rgb_rtv.unwrap();
            self.context
                .ClearRenderTargetView(&rgb_rtv, &[0.0, 0.0, 0.0, 1.0]);
            let texture_id = self.egui.register_user_texture(rgb_srv.unwrap());
            log::info!(
                "Video surface {width}x{height} ({})",
                if format == native::FORMAT_P010 {
                    "P010"
                } else {
                    "NV12"
                }
            );
            self.video = Some(VideoSurface {
                width,
                height,
                format,
                yuv,
                luma_srv: luma_srv.unwrap(),
                chroma_srv: chroma_srv.unwrap(),
                _rgb: rgb,
                rgb_rtv,
                texture_id,
                sequence: 0,
                has_frame: false,
            });
        }
        Ok(())
    }

    /// Pulls the newest decoded frame (GPU-to-GPU copy) and converts it to RGB.
    /// Returns true when a new frame was presented to the video surface.
    pub fn update_video(&mut self, session: &native::Session) -> bool {
        let Some(info) = session.frame_info() else {
            return false;
        };
        if info.width == 0 || info.height == 0 {
            return false;
        }
        if let Err(e) = self.ensure_video(info.width, info.height, info.format) {
            log::error!("{e}");
            return false;
        }
        let video = self.video.as_mut().unwrap();
        if info.sequence == video.sequence {
            return false;
        }
        let mut sequence = video.sequence;
        let copied = unsafe { session.copy_frame(video.yuv.as_raw(), &mut sequence) };
        if !copied {
            return false;
        }
        video.sequence = sequence;
        video.has_frame = true;
        unsafe {
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: video.width as f32,
                Height: video.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            self.context
                .OMSetRenderTargets(Some(&[Some(video.rgb_rtv.clone())]), None);
            self.context.OMSetBlendState(None, None, 0xffff_ffff);
            self.context.RSSetState(None);
            self.context.RSSetViewports(Some(&[viewport]));
            self.context.IASetInputLayout(None);
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vertex_shader, None);
            self.context.PSSetShader(&self.pixel_shader, None);
            self.context.PSSetShaderResources(
                0,
                Some(&[Some(video.luma_srv.clone()), Some(video.chroma_srv.clone())]),
            );
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context.Draw(3, 0);
            self.context.PSSetShaderResources(0, Some(&[None, None]));
            self.context.OMSetRenderTargets(None, None);
        }
        true
    }

    pub fn render(
        &mut self,
        ctx: &egui::Context,
        output: egui_directx11::RendererOutput,
    ) -> Result<(), String> {
        let Some(back_buffer) = self.back_buffer.clone() else {
            return Ok(());
        };
        unsafe {
            self.context
                .OMSetRenderTargets(Some(&[Some(back_buffer.clone())]), None);
            self.context
                .ClearRenderTargetView(&back_buffer, &[0.04, 0.04, 0.05, 1.0]);
        }
        self.egui
            .render(&self.context, &back_buffer, ctx, output)
            .map_err(|e| e.to_string())?;
        let hr = unsafe { self.swap_chain.Present(1, DXGI_PRESENT(0)) };
        if hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET {
            return Err("Graphics device lost (the GPU driver was reset)".into());
        }
        Ok(())
    }
}
