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

use crate::config::Upscaling;
use crate::native;
use crate::upscale::{ScalerKind, Upscaler};

const VERTEX_SHADER: &str = r#"
float4 vs_main(uint id : SV_VertexID) : SV_Position {
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}
"#;

// YUV to RGB. Optional luma guided chroma upsampling (joint bilateral over the 4 nearest chroma
// samples) and, for PQ / BT.2020 HDR streams, tone mapping to SDR BT.709.
const CONVERT_SHADER: &str = r#"
Texture2D<float> luma : register(t0);
Texture2D<float2> chroma : register(t1);
SamplerState linear_sampler : register(s0);
cbuffer Params : register(b0) {
    uint flags;
    uint ten_bit;
    float2 luma_size;
};

static const uint FLAG_HDR = 1;
static const uint FLAG_BILATERAL = 2;

float unit_luma(float s) {
    return ten_bit != 0 ? (s * 65535.0 / 64.0 - 64.0) / 876.0 : (s * 255.0 - 16.0) / 219.0;
}

float2 unit_chroma(float2 s) {
    return ten_bit != 0 ? (s * 65535.0 / 64.0 - 512.0) / 896.0 : (s * 255.0 - 128.0) / 224.0;
}

float2 sample_chroma(float2 uv, float y) {
    if ((flags & FLAG_BILATERAL) == 0) {
        return chroma.SampleLevel(linear_sampler, uv, 0);
    }
    float2 csize = luma_size * 0.5;
    float2 pos = uv * csize - 0.5;
    float2 base = floor(pos);
    float2 f = pos - base;
    float2 acc = 0;
    float total = 0;
    [unroll] for (int j = 0; j < 2; j++) {
        [unroll] for (int i = 0; i < 2; i++) {
            float2 cuv = (base + float2(i, j) + 0.5) / csize;
            float2 c = chroma.SampleLevel(linear_sampler, cuv, 0);
            float block_luma = luma.SampleLevel(linear_sampler, cuv, 0);
            float spatial = (i == 0 ? 1.0 - f.x : f.x) * (j == 0 ? 1.0 - f.y : f.y);
            float d = (y - block_luma) * (ten_bit != 0 ? 64.0 : 16.0);
            float w = spatial * (exp(-d * d * 0.5) + 1e-3);
            acc += c * w;
            total += w;
        }
    }
    return acc / total;
}

float3 pq_to_nits(float3 e) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float3 p = pow(saturate(e), 1.0 / m2);
    return pow(max(p - c1, 0.0) / (c2 - c3 * p), 1.0 / m1) * 10000.0;
}

float3 tonemap_hdr(float3 rgb2020_pq) {
    float3 nits = pq_to_nits(rgb2020_pq);
    float3 lin = mul(float3x3(
        1.6605, -0.5876, -0.0728,
        -0.1246, 1.1329, -0.0083,
        -0.0182, -0.1006, 1.1187), nits) / 203.0;
    lin = max(lin, 0.0);
    const float white = 1000.0 / 203.0;
    float peak = max(max(lin.r, lin.g), lin.b);
    float mapped = peak * (1.0 + peak / (white * white)) / (1.0 + peak);
    lin *= peak > 0.0 ? mapped / peak : 0.0;
    return pow(saturate(lin), 1.0 / 2.2);
}

float4 ps_main(float4 pos : SV_Position) : SV_Target {
    float2 uv = pos.xy / luma_size;
    float y_raw = luma.SampleLevel(linear_sampler, uv, 0);
    float y = unit_luma(y_raw);
    float2 c = unit_chroma(sample_chroma(uv, y_raw));
    float3 rgb;
    if ((flags & FLAG_HDR) != 0) {
        rgb = float3(
            y + 1.4746 * c.y,
            y - 0.16455 * c.x - 0.57135 * c.y,
            y + 1.8814 * c.x);
        rgb = tonemap_hdr(rgb);
    } else {
        rgb = float3(
            y + 1.5748 * c.y,
            y - 0.1873 * c.x - 0.4681 * c.y,
            y + 1.8556 * c.x);
    }
    return float4(saturate(rgb), 1.0);
}
"#;

// Debanding (several rounds of random 4 tap averages that only replace a pixel when it is within
// the threshold of its surroundings, so real edges survive), light grain, and dithering to 8 bits.
const FINISH_SHADER: &str = r#"
Texture2D<float4> source_tex : register(t0);
SamplerState linear_sampler : register(s0);
cbuffer Params : register(b0) {
    float threshold;
    float grain;
    float2 texel;
};

float hash(float2 p) {
    float3 p3 = frac(float3(p.xyx) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return frac((p3.x + p3.y) * p3.z);
}

float4 ps_main(float4 pos : SV_Position) : SV_Target {
    float2 uv = pos.xy * texel;
    float3 c = source_tex.SampleLevel(linear_sampler, uv, 0).rgb;
    if (threshold > 0.0) {
        [unroll] for (int i = 1; i <= 3; i++) {
            float dist = hash(pos.xy + i * 17.0) * 14.0 * i;
            float angle = hash(pos.xy * 1.37 + i * 5.0) * 6.2831853;
            float2 o = float2(cos(angle), sin(angle)) * dist * texel;
            float2 r = float2(-o.y, o.x);
            float3 avg = (source_tex.SampleLevel(linear_sampler, uv + o, 0).rgb
                        + source_tex.SampleLevel(linear_sampler, uv - o, 0).rgb
                        + source_tex.SampleLevel(linear_sampler, uv + r, 0).rgb
                        + source_tex.SampleLevel(linear_sampler, uv - r, 0).rgb) * 0.25;
            float3 keep = step(threshold / i, abs(c - avg));
            c = lerp(avg, c, keep);
        }
        c += (hash(pos.xy * 0.731 + 3.0) - 0.5) * grain;
    }
    c += (hash(pos.xy + 0.5) + hash(pos.xy * 1.31 + 9.0) - 1.0) / 255.0;
    return float4(saturate(c), 1.0);
}
"#;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Enhance {
    pub better_chroma: bool,
    pub deband: f32,
}

pub(crate) struct RenderTexture {
    pub width: u32,
    pub height: u32,
    pub texture: ID3D11Texture2D,
    pub rtv: ID3D11RenderTargetView,
    pub srv: ID3D11ShaderResourceView,
}

impl RenderTexture {
    pub(crate) fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        unsafe {
            let mut texture = None;
            device.CreateTexture2D(&desc, None, Some(&mut texture))?;
            let texture = texture.unwrap();
            let mut rtv = None;
            let mut srv = None;
            device.CreateRenderTargetView(&texture, None, Some(&mut rtv))?;
            device.CreateShaderResourceView(&texture, None, Some(&mut srv))?;
            Ok(Self {
                width,
                height,
                texture,
                rtv: rtv.unwrap(),
                srv: srv.unwrap(),
            })
        }
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Draws one fullscreen triangle with a pixel shader into a render target.
pub(crate) struct Passes {
    vertex_shader: ID3D11VertexShader,
    sampler: ID3D11SamplerState,
    constants: ID3D11Buffer,
}

impl Passes {
    pub(crate) fn new(device: &ID3D11Device) -> Result<Self, String> {
        let err = |e: windows::core::Error| format!("Render passes: {e}");
        let vs = compile(VERTEX_SHADER, s!("vs_main"), s!("vs_5_0"))?;
        unsafe {
            let mut vertex_shader = None;
            device
                .CreateVertexShader(blob_bytes(&vs), None, Some(&mut vertex_shader))
                .map_err(err)?;
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
            device
                .CreateSamplerState(&sampler_desc, Some(&mut sampler))
                .map_err(err)?;
            let buffer_desc = D3D11_BUFFER_DESC {
                ByteWidth: 64,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let mut constants = None;
            device
                .CreateBuffer(&buffer_desc, None, Some(&mut constants))
                .map_err(err)?;
            Ok(Self {
                vertex_shader: vertex_shader.unwrap(),
                sampler: sampler.unwrap(),
                constants: constants.unwrap(),
            })
        }
    }

    pub(crate) fn draw(
        &self,
        context: &ID3D11DeviceContext,
        shader: &ID3D11PixelShader,
        constants: &[u32; 16],
        sources: &[Option<ID3D11ShaderResourceView>],
        target: &RenderTexture,
    ) {
        unsafe {
            context.UpdateSubresource(
                &self.constants,
                0,
                None,
                constants.as_ptr() as *const _,
                0,
                0,
            );
            let viewport = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: target.width as f32,
                Height: target.height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            context.OMSetRenderTargets(Some(&[Some(target.rtv.clone())]), None);
            context.OMSetBlendState(None, None, 0xffff_ffff);
            context.RSSetState(None);
            context.RSSetViewports(Some(&[viewport]));
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex_shader, None);
            context.PSSetShader(shader, None);
            context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.PSSetShaderResources(0, Some(sources));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.Draw(3, 0);
            let empty: Vec<Option<ID3D11ShaderResourceView>> = vec![None; sources.len()];
            context.PSSetShaderResources(0, Some(&empty));
            context.OMSetRenderTargets(None, None);
        }
    }
}

pub(crate) fn create_pixel_shader(
    device: &ID3D11Device,
    source: &str,
) -> Result<ID3D11PixelShader, String> {
    let blob = compile(source, s!("ps_main"), s!("ps_5_0"))?;
    let mut shader = None;
    unsafe { device.CreatePixelShader(blob_bytes(&blob), None, Some(&mut shader)) }
        .map_err(|e| format!("Pixel shader: {e}"))?;
    Ok(shader.unwrap())
}

pub(crate) fn compile(source: &str, entry: PCSTR, target: PCSTR) -> Result<ID3DBlob, String> {
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
    blob.ok_or_else(|| "Empty shader".to_string())
}

pub(crate) fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    }
}

pub(crate) fn convert_constants(
    hdr: bool,
    ten_bit: bool,
    better_chroma: bool,
    size: (u32, u32),
) -> [u32; 16] {
    let mut con = [0u32; 16];
    con[0] = u32::from(hdr) | (u32::from(better_chroma) << 1);
    con[1] = u32::from(ten_bit);
    con[2] = (size.0 as f32).to_bits();
    con[3] = (size.1 as f32).to_bits();
    con
}

pub(crate) fn finish_constants(deband: f32, size: (u32, u32)) -> [u32; 16] {
    let strength = deband.clamp(0.0, 1.0);
    let mut con = [0u32; 16];
    con[0] = (strength * 3.0 / 255.0).to_bits();
    con[1] = (strength * 1.5 / 255.0).to_bits();
    con[2] = (1.0 / size.0 as f32).to_bits();
    con[3] = (1.0 / size.1 as f32).to_bits();
    con
}

/// Decoded YUV frame to the 8 bit RGB texture that everything after it works on:
/// convert (chroma upsampling, HDR tone mapping) into 16 bit float, then deband and dither.
pub(crate) struct VideoPipeline {
    passes: Passes,
    convert_shader: ID3D11PixelShader,
    finish_shader: ID3D11PixelShader,
}

impl VideoPipeline {
    pub(crate) fn new(device: &ID3D11Device) -> Result<Self, String> {
        Ok(Self {
            passes: Passes::new(device)?,
            convert_shader: create_pixel_shader(device, CONVERT_SHADER)?,
            finish_shader: create_pixel_shader(device, FINISH_SHADER)?,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn process(
        &self,
        context: &ID3D11DeviceContext,
        luma: &ID3D11ShaderResourceView,
        chroma: &ID3D11ShaderResourceView,
        ten_bit: bool,
        hdr: bool,
        enhance: Enhance,
        linear: &RenderTexture,
        rgb: &RenderTexture,
    ) {
        self.passes.draw(
            context,
            &self.convert_shader,
            &convert_constants(hdr, ten_bit, enhance.better_chroma, linear.size()),
            &[Some(luma.clone()), Some(chroma.clone())],
            linear,
        );
        self.passes.draw(
            context,
            &self.finish_shader,
            &finish_constants(enhance.deband, linear.size()),
            &[Some(linear.srv.clone())],
            rgb,
        );
    }
}

pub struct VideoSurface {
    pub width: u32,
    pub height: u32,
    format: i32,
    pub hdr: bool,
    yuv: ID3D11Texture2D,
    luma_srv: ID3D11ShaderResourceView,
    chroma_srv: ID3D11ShaderResourceView,
    linear: RenderTexture,
    rgb: RenderTexture,
    pub texture_id: egui::TextureId,
    pub sequence: u64,
    pub has_frame: bool,
    processed: Option<(u64, Enhance, bool)>,
}

/// The texture to show for the current video frame.
#[derive(Clone, Copy, Debug)]
pub struct VideoOutput {
    pub texture_id: egui::TextureId,
    pub size: [u32; 2],
    pub source: [u32; 2],
    pub scaler: Option<ScalerKind>,
    pub hdr: bool,
}

pub struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    back_buffer: Option<ID3D11RenderTargetView>,
    egui: egui_directx11::Renderer,
    pipeline: VideoPipeline,
    video: Option<VideoSurface>,
    upscaler: Upscaler,
    size: (u32, u32),
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

        let pipeline = VideoPipeline::new(&device)?;
        let egui = egui_directx11::Renderer::new(&device).map_err(|e| err("egui renderer", e))?;
        let upscaler = Upscaler::new(&device, &context);
        let mut renderer = Self {
            device,
            context,
            swap_chain,
            back_buffer: None,
            egui,
            pipeline,
            video: None,
            upscaler,
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

    pub fn drop_video(&mut self) {
        self.upscaler.reset(&mut self.egui);
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
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_DECODER.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
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
                .map_err(|e| format!("YUV texture: {e}"))?;
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
            let linear =
                RenderTexture::new(&self.device, width, height, DXGI_FORMAT_R16G16B16A16_FLOAT)
                    .map_err(|e| format!("Linear texture: {e}"))?;
            let rgb = RenderTexture::new(&self.device, width, height, DXGI_FORMAT_B8G8R8A8_UNORM)
                .map_err(|e| format!("RGB texture: {e}"))?;
            self.context
                .ClearRenderTargetView(&rgb.rtv, &[0.0, 0.0, 0.0, 1.0]);
            let texture_id = self.egui.register_user_texture(rgb.srv.clone());
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
                hdr: false,
                yuv,
                luma_srv: luma_srv.unwrap(),
                chroma_srv: chroma_srv.unwrap(),
                linear,
                rgb,
                texture_id,
                sequence: 0,
                has_frame: false,
                processed: None,
            });
        }
        Ok(())
    }

    /// Pulls the newest decoded frame (GPU-to-GPU copy) and runs the enhance passes on it.
    /// Returns true when a new frame was produced.
    pub fn update_video(&mut self, session: &native::Session, enhance: Enhance) -> bool {
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
        if info.sequence != video.sequence {
            let mut sequence = video.sequence;
            let copied = unsafe { session.copy_frame(video.yuv.as_raw(), &mut sequence) };
            if copied {
                video.sequence = sequence;
                video.has_frame = true;
                video.hdr = info.hdr != 0;
            }
        }
        let key = (video.sequence, enhance, video.hdr);
        if !video.has_frame || video.processed == Some(key) {
            return false;
        }
        self.pipeline.process(
            &self.context,
            &video.luma_srv,
            &video.chroma_srv,
            video.format == native::FORMAT_P010,
            video.hdr,
            enhance,
            &video.linear,
            &video.rgb,
        );
        video.processed = Some(key);
        true
    }

    /// Picks the texture to show, upscaling and sharpening the frame when that is enabled.
    pub fn video_output(
        &mut self,
        mode: Upscaling,
        sharpness: f32,
        target: Option<[u32; 2]>,
    ) -> Option<VideoOutput> {
        let video = self.video.as_ref().filter(|v| v.has_frame)?;
        let source = [video.width, video.height];
        let generation = video.processed.map(|p| p.0).unwrap_or(video.sequence);
        let scaled = self.upscaler.process(
            &mut self.egui,
            mode,
            sharpness,
            generation,
            &video.rgb,
            target,
        );
        Some(match scaled {
            Some(s) => VideoOutput {
                texture_id: s.texture_id,
                size: s.size,
                source,
                scaler: Some(s.kind),
                hdr: video.hdr,
            },
            None => VideoOutput {
                texture_id: video.texture_id,
                size: source,
                source,
                scaler: None,
                hdr: video.hdr,
            },
        })
    }

    pub fn rtx_possible(&self) -> bool {
        self.upscaler.rtx_possible()
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
