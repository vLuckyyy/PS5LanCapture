use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HANDLE, HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2D, ID3DBlob,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
use windows::core::{Interface, PCSTR, s};

use crate::config::{ColorGrade, Upscaling};
use crate::native;
use crate::upscale::{HdrExpander, ScalerKind, Upscaler};

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
    float peak_nits;
};

static const uint FLAG_HDR = 1;
static const uint FLAG_BILATERAL = 2;
static const uint FLAG_PQ_OUTPUT = 4;

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

float3 nits_to_pq(float3 nits) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float3 y = pow(saturate(nits / 10000.0), m1);
    return pow((c1 + c2 * y) / (1.0 + c3 * y), m2);
}

// Keeps PQ / BT.2020 and only rolls highlights off towards what the display can show.
float3 fit_to_display(float3 rgb2020_pq) {
    float3 nits = pq_to_nits(rgb2020_pq);
    float knee = peak_nits * 0.75;
    float top = max(max(nits.r, nits.g), nits.b);
    if (top > knee) {
        float range = peak_nits - knee;
        float mapped = knee + range * (1.0 - exp(-(top - knee) / range));
        nits *= mapped / top;
    }
    return nits_to_pq(nits);
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
        rgb = (flags & FLAG_PQ_OUTPUT) != 0 ? fit_to_display(rgb) : tonemap_hdr(rgb);
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
// the threshold of its surroundings, so real edges survive), clarity (local contrast on luma,
// strongest in midtones), color grading, light grain, and dithering to 8 bits.
const FINISH_SHADER: &str = r#"
Texture2D<float4> source_tex : register(t0);
SamplerState linear_sampler : register(s0);
cbuffer Params : register(b0) {
    float threshold;
    float grain;
    float2 texel;
    float clarity;
    float saturation;
    float vibrance;
    float contrast;
    float gamma;
    float warmth;
    float radius;
    float pq;
    float levels;
};

#define LUMA (pq > 0.5 ? float3(0.2627, 0.6780, 0.0593) : float3(0.2126, 0.7152, 0.0722))

float hash(float2 p) {
    float3 p3 = frac(float3(p.xyx) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return frac((p3.x + p3.y) * p3.z);
}

float3 with_luma(float3 c, float from, float to) {
    return c * (max(to, 0.0) / max(from, 1e-4));
}

float local_mean(float2 uv) {
    float sum = 0.0;
    [unroll] for (int i = 0; i < 8; i++) {
        float a = i * 0.7853982 + 0.3926991;
        float2 d = float2(cos(a), sin(a)) * texel * radius;
        sum += dot(source_tex.SampleLevel(linear_sampler, uv + d, 0).rgb, LUMA);
        sum += dot(source_tex.SampleLevel(linear_sampler, uv + d * 2.5, 0).rgb, LUMA);
    }
    return sum / 16.0;
}

float3 grade(float3 c) {
    c = saturate(c);
    float y = dot(c, LUMA);
    float curved = y + contrast * (y - 0.5) * 4.0 * y * (1.0 - y);
    c = with_luma(c, y, curved);
    y = curved;
    c = y + (c - y) * saturation;
    float hi = max(c.r, max(c.g, c.b));
    float lo = min(c.r, min(c.g, c.b));
    float chroma = hi - lo;
    float skin = 0.0;
    if (c.r >= c.g && c.g >= c.b && chroma > 1e-4) {
        float hue = 60.0 * (c.g - c.b) / chroma;
        skin = exp(-pow((hue - 25.0) / 14.0, 2.0));
    }
    float boost = vibrance * (1.0 - chroma / max(hi, 1e-4)) * (1.0 - 0.8 * skin);
    c = y + (c - y) * (1.0 + boost);
    c *= float3(1.0 + warmth * 0.05, 1.0, 1.0 - warmth * 0.05);
    return pow(saturate(c), gamma);
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
    }
    if (clarity > 0.0) {
        float y = saturate(dot(c, LUMA));
        float detail = clamp(y - local_mean(uv), -0.06, 0.06);
        c = with_luma(c, y, y + clarity * detail * 4.0 * y * (1.0 - y));
    }
    c = grade(c);
    if (threshold > 0.0) {
        c += (hash(pos.xy * 0.731 + 3.0) - 0.5) * grain;
    }
    c += (hash(pos.xy + 0.5) + hash(pos.xy * 1.31 + 9.0) - 1.0) / levels;
    return float4(saturate(c), 1.0);
}
"#;

// HDR10 output: the video (already PQ / BT.2020) goes to the back buffer untouched and the SDR
// interface, rendered by egui with premultiplied alpha, is converted to PQ and blended on top.
const COMPOSITE_SHADER: &str = r#"
Texture2D<float4> ui_tex : register(t0);
Texture2D<float4> video_tex : register(t1);
SamplerState linear_sampler : register(s0);
cbuffer Params : register(b0) {
    float4 video_rect;
    float ui_nits;
    float has_video;
};

float3 nits_to_pq(float3 nits) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float3 y = pow(saturate(nits / 10000.0), m1);
    return pow((c1 + c2 * y) / (1.0 + c3 * y), m2);
}

float4 ps_main(float4 pos : SV_Position) : SV_Target {
    float3 color = 0.0;
    if (has_video > 0.5 && all(pos.xy >= video_rect.xy) && all(pos.xy < video_rect.zw)) {
        float2 uv = (pos.xy - video_rect.xy) / (video_rect.zw - video_rect.xy);
        color = video_tex.SampleLevel(linear_sampler, uv, 0).rgb;
    }
    float4 ui = ui_tex.Load(int3(pos.xy, 0));
    if (ui.a > 0.0) {
        float3 lin = pow(saturate(ui.rgb / ui.a), 2.2);
        float3 wide = mul(float3x3(
            0.6274, 0.3293, 0.0433,
            0.0691, 0.9195, 0.0114,
            0.0164, 0.0880, 0.8956), lin);
        color = lerp(color, nits_to_pq(wide * ui_nits), ui.a);
    }
    return float4(color, 1.0);
}
"#;

const UI_WHITE_NITS: f32 = 203.0;
const DISPLAY_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// HDR state of the display the window is on.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DisplayInfo {
    pub hdr: bool,
    pub peak_nits: f32,
}

fn display_of_window(hwnd: HWND) -> Option<DisplayInfo> {
    let mut window = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut window) }.ok()?;
    let center = (
        (window.left + window.right) / 2,
        (window.top + window.bottom) / 2,
    );
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut adapter_index = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_index) } {
        adapter_index += 1;
        let mut output_index = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(output_index) } {
            output_index += 1;
            let Ok(desc) = output
                .cast::<IDXGIOutput6>()
                .and_then(|o| unsafe { o.GetDesc1() })
            else {
                continue;
            };
            let r = desc.DesktopCoordinates;
            if center.0 >= r.left && center.0 < r.right && center.1 >= r.top && center.1 < r.bottom
            {
                return Some(DisplayInfo {
                    hdr: desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
                    peak_nits: desc.MaxLuminance,
                });
            }
        }
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Enhance {
    pub better_chroma: bool,
    pub deband: f32,
    pub clarity: f32,
    pub grade: ColorGrade,
}

pub(crate) struct RenderTexture {
    pub width: u32,
    pub height: u32,
    pub format: DXGI_FORMAT,
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
                format,
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
        self.draw_to(
            context,
            shader,
            constants,
            sources,
            &target.rtv,
            target.size(),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_to(
        &self,
        context: &ID3D11DeviceContext,
        shader: &ID3D11PixelShader,
        constants: &[u32; 16],
        sources: &[Option<ID3D11ShaderResourceView>],
        target: &ID3D11RenderTargetView,
        size: (u32, u32),
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
                Width: size.0 as f32,
                Height: size.1 as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            context.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
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
    pq_peak_nits: Option<f32>,
) -> [u32; 16] {
    let mut con = [0u32; 16];
    con[0] =
        u32::from(hdr) | (u32::from(better_chroma) << 1) | (u32::from(pq_peak_nits.is_some()) << 2);
    con[4] = pq_peak_nits.unwrap_or(0.0).clamp(100.0, 10_000.0).to_bits();
    con[1] = u32::from(ten_bit);
    con[2] = (size.0 as f32).to_bits();
    con[3] = (size.1 as f32).to_bits();
    con
}

pub(crate) fn finish_constants(enhance: &Enhance, size: (u32, u32), pq: bool) -> [u32; 16] {
    let strength = enhance.deband.clamp(0.0, 1.0);
    let grade = enhance.grade.clamped();
    [
        strength * 3.0 / 255.0,
        strength * 1.5 / 255.0,
        1.0 / size.0 as f32,
        1.0 / size.1 as f32,
        enhance.clarity.clamp(0.0, 1.0),
        grade.saturation,
        grade.vibrance,
        grade.contrast,
        grade.gamma,
        grade.warmth,
        4.0 * size.1 as f32 / 1080.0,
        if pq { 1.0 } else { 0.0 },
        if pq { 1023.0 } else { 255.0 },
        0.0,
        0.0,
        0.0,
    ]
    .map(f32::to_bits)
}

/// Decoded YUV frame to the 8 bit RGB texture that everything after it works on:
/// convert (chroma upsampling, HDR tone mapping) into 16 bit float, then deband and dither.
pub(crate) struct VideoPipeline {
    pub(crate) passes: Passes,
    convert_shader: ID3D11PixelShader,
    finish_shader: ID3D11PixelShader,
    pub(crate) composite_shader: ID3D11PixelShader,
}

impl VideoPipeline {
    pub(crate) fn new(device: &ID3D11Device) -> Result<Self, String> {
        Ok(Self {
            passes: Passes::new(device)?,
            convert_shader: create_pixel_shader(device, CONVERT_SHADER)?,
            finish_shader: create_pixel_shader(device, FINISH_SHADER)?,
            composite_shader: create_pixel_shader(device, COMPOSITE_SHADER)?,
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
        pq_peak_nits: Option<f32>,
        linear: &RenderTexture,
        rgb: &RenderTexture,
    ) {
        self.passes.draw(
            context,
            &self.convert_shader,
            &convert_constants(
                hdr,
                ten_bit,
                enhance.better_chroma,
                linear.size(),
                pq_peak_nits,
            ),
            &[Some(luma.clone()), Some(chroma.clone())],
            linear,
        );
        self.passes.draw(
            context,
            &self.finish_shader,
            &finish_constants(&enhance, linear.size(), pq_peak_nits.is_some()),
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
    /// The processed frame stays PQ / BT.2020 for an HDR display instead of being tone mapped.
    pq: bool,
    yuv: ID3D11Texture2D,
    luma_srv: ID3D11ShaderResourceView,
    chroma_srv: ID3D11ShaderResourceView,
    linear: RenderTexture,
    rgb: RenderTexture,
    pub texture_id: egui::TextureId,
    pub sequence: u64,
    pub has_frame: bool,
    processed: Option<(u64, Enhance, bool, u32)>,
}

/// How long a frame takes inside the app, smoothed over recent frames.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DisplayLatency {
    /// Compressed frame received to picture decoded.
    pub decode_ms: f32,
    /// Picture decoded to handed over to the display.
    pub display_ms: f32,
}

#[derive(Default)]
pub struct LatencyMeter {
    current: Option<DisplayLatency>,
}

impl LatencyMeter {
    const SMOOTHING: f32 = 0.1;

    pub fn record(&mut self, times: native::FrameTimes, presented_us: u64) {
        let ms = |from: u64, to: u64| to.saturating_sub(from) as f32 / 1000.0;
        let sample = DisplayLatency {
            decode_ms: ms(times.received_us, times.decoded_us),
            display_ms: ms(times.decoded_us, presented_us),
        };
        if times.received_us == 0 || sample.display_ms > 1000.0 {
            return;
        }
        self.current = Some(match self.current {
            Some(c) => DisplayLatency {
                decode_ms: c.decode_ms + (sample.decode_ms - c.decode_ms) * Self::SMOOTHING,
                display_ms: c.display_ms + (sample.display_ms - c.display_ms) * Self::SMOOTHING,
            },
            None => sample,
        });
    }

    pub fn get(&self) -> Option<DisplayLatency> {
        self.current
    }

    pub fn reset(&mut self) {
        self.current = None;
    }
}

/// The texture to show for the current video frame.
#[derive(Clone, Copy, Debug)]
pub struct VideoOutput {
    /// True when the renderer composites the video itself (HDR output) and egui must not draw it.
    pub composited: bool,
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
    swap_flags: DXGI_SWAP_CHAIN_FLAG,
    tearing: bool,
    frame_slot: Option<HANDLE>,
    unpresented: Option<native::FrameTimes>,
    latency: LatencyMeter,
    hwnd: HWND,
    display: DisplayInfo,
    display_checked: Instant,
    hdr_swap: bool,
    ui_layer: Option<RenderTexture>,
    present_srv: Option<ID3D11ShaderResourceView>,
    hdr_expander: HdrExpander,
    hdr_present: bool,
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
        let mut allow_tearing = windows::core::BOOL(0);
        let tearing = factory
            .cast::<IDXGIFactory5>()
            .and_then(|f| unsafe {
                f.CheckFeatureSupport(
                    DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                    &mut allow_tearing as *mut _ as *mut c_void,
                    size_of::<windows::core::BOOL>() as u32,
                )
            })
            .is_ok()
            && allow_tearing.as_bool();
        let mut swap_flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0;
        if tearing {
            swap_flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0;
        }
        let swap_flags = DXGI_SWAP_CHAIN_FLAG(swap_flags);
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
            Flags: swap_flags.0 as u32,
        };
        let swap_chain =
            unsafe { factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None) }
                .map_err(|e| err("Cannot create the swap chain", e))?;
        unsafe {
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
        }

        let frame_slot = swap_chain
            .cast::<IDXGISwapChain2>()
            .ok()
            .and_then(|sc| unsafe {
                let _ = sc.SetMaximumFrameLatency(1);
                let handle = sc.GetFrameLatencyWaitableObject();
                (!handle.is_invalid()).then_some(handle)
            });
        log::info!(
            "Display: tearing (G-Sync/FreeSync) {}, frame latency 1 {}",
            if tearing { "available" } else { "unavailable" },
            if frame_slot.is_some() { "on" } else { "off" }
        );
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
            swap_flags,
            tearing,
            frame_slot,
            unpresented: None,
            latency: LatencyMeter::default(),
            hwnd,
            display: display_of_window(hwnd).unwrap_or_default(),
            display_checked: Instant::now(),
            hdr_swap: false,
            ui_layer: None,
            present_srv: None,
            hdr_expander: HdrExpander::default(),
            hdr_present: false,
        };
        log::info!(
            "Display: HDR {}, peak {:.0} nits",
            if renderer.display.hdr { "on" } else { "off" },
            renderer.display.peak_nits
        );
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
            self.swap_chain
                .ResizeBuffers(0, width, height, DXGI_FORMAT_UNKNOWN, self.swap_flags)
        } {
            Ok(()) => self.size = (width, height),
            Err(e) => log::error!("ResizeBuffers: {e}"),
        }
        if let Err(e) = self.create_back_buffer() {
            log::error!("Back buffer: {e}");
        }
    }

    /// HDR state of the display under the window, refreshed every couple of seconds.
    pub fn display(&mut self) -> DisplayInfo {
        if self.display_checked.elapsed() > DISPLAY_CHECK_INTERVAL {
            self.display_checked = Instant::now();
            if let Some(display) = display_of_window(self.hwnd)
                && display != self.display
            {
                log::info!(
                    "Display changed: HDR {}, peak {:.0} nits",
                    if display.hdr { "on" } else { "off" },
                    display.peak_nits
                );
                self.display = display;
            }
        }
        self.display
    }

    /// Switches the swap chain between SDR (8 bit BT.709) and HDR10 (10 bit PQ BT.2020).
    fn set_hdr_swap(&mut self, hdr: bool) {
        if self.hdr_swap == hdr {
            return;
        }
        let (format, color_space) = if hdr {
            (
                DXGI_FORMAT_R10G10B10A2_UNORM,
                DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
            )
        } else {
            (
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            )
        };
        let name = if hdr { "HDR10" } else { "SDR" };
        self.back_buffer = None;
        unsafe {
            self.context.OMSetRenderTargets(None, None);
            self.context.Flush();
        }
        let resized = unsafe {
            self.swap_chain
                .ResizeBuffers(0, self.size.0, self.size.1, format, self.swap_flags)
        };
        let colored = resized.and_then(|()| {
            self.swap_chain
                .cast::<IDXGISwapChain3>()
                .and_then(|sc| unsafe { sc.SetColorSpace1(color_space) })
        });
        match colored {
            Ok(()) => {
                self.hdr_swap = hdr;
                log::info!("Output: {name}");
            }
            Err(e) => log::error!("Switching output to {name}: {e}"),
        }
        if let Err(e) = self.create_back_buffer() {
            log::error!("Back buffer: {e}");
        }
    }

    /// Presents one frame through the HDR10 path without a stream.
    #[cfg(test)]
    pub fn present_hdr_for_test(
        &mut self,
        ctx: &egui::Context,
        output: egui_directx11::RendererOutput,
    ) -> Result<(), String> {
        self.set_hdr_swap(true);
        if !self.hdr_swap {
            return Err("the swap chain did not switch to HDR10".into());
        }
        let back_buffer = self.back_buffer.clone().ok_or("no back buffer")?;
        self.render_hdr(ctx, output, &back_buffer, None)?;
        drop(back_buffer);
        unsafe { self.swap_chain.Present(1, DXGI_PRESENT(0)) }
            .ok()
            .map_err(|e| e.to_string())?;
        self.set_hdr_swap(false);
        if self.hdr_swap {
            return Err("the swap chain did not switch back to SDR".into());
        }
        Ok(())
    }

    pub fn tearing_supported(&self) -> bool {
        self.tearing
    }

    pub fn latency(&self) -> Option<DisplayLatency> {
        self.latency.get()
    }

    /// Blocks until the swap chain can take a frame (at most one is ever queued), so the frame
    /// is picked as late as possible before the display shows it.
    pub fn wait_for_frame_slot(&self) {
        if let Some(handle) = self.frame_slot {
            unsafe {
                WaitForSingleObjectEx(handle, 100, true);
            }
        }
    }

    pub fn drop_video(&mut self) {
        self.present_srv = None;
        self.hdr_present = false;
        self.hdr_expander.reset();
        self.unpresented = None;
        self.latency.reset();
        self.upscaler.reset(&mut self.egui);
        if let Some(video) = self.video.take() {
            self.egui.unregister_user_texture(video.texture_id);
        }
    }

    fn ensure_video(
        &mut self,
        width: u32,
        height: u32,
        format: i32,
        pq: bool,
    ) -> Result<(), String> {
        if let Some(v) = &self.video
            && v.width == width
            && v.height == height
            && v.format == format
            && v.pq == pq
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
            let rgb_format = if pq {
                DXGI_FORMAT_R10G10B10A2_UNORM
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            };
            let rgb = RenderTexture::new(&self.device, width, height, rgb_format)
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
                pq,
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
    ///
    /// `hdr_output` asks for HDR streams to be shown as HDR10 when the display supports it.
    pub fn update_video(
        &mut self,
        session: &native::Session,
        enhance: Enhance,
        hdr_output: bool,
    ) -> bool {
        let Some(info) = session.frame_info() else {
            return false;
        };
        if info.width == 0 || info.height == 0 {
            return false;
        }
        let display = self.display();
        let pq = hdr_output && display.hdr && info.hdr != 0;
        let peak = pq.then_some(display.peak_nits);
        if let Err(e) = self.ensure_video(info.width, info.height, info.format, pq) {
            log::error!("{e}");
            return false;
        }
        let video = self.video.as_mut().unwrap();
        if info.sequence != video.sequence {
            let mut sequence = video.sequence;
            let copied = unsafe { session.copy_frame(video.yuv.as_raw(), &mut sequence) };
            if let Some(times) = copied {
                self.unpresented = Some(times);
                video.sequence = sequence;
                video.has_frame = true;
                video.hdr = info.hdr != 0;
            }
        }
        let key = (
            video.sequence,
            enhance,
            video.hdr,
            peak.unwrap_or(0.0).to_bits(),
        );
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
            peak,
            &video.linear,
            &video.rgb,
        );
        video.processed = Some(key);
        true
    }

    /// Picks the texture to show, upscaling and sharpening the frame when that is enabled.
    ///
    /// `sdr_to_hdr` asks for SDR streams to be expanded with RTX Video HDR on an HDR display.
    pub fn video_output(
        &mut self,
        mode: Upscaling,
        sharpness: f32,
        target: Option<[u32; 2]>,
        sdr_to_hdr: bool,
    ) -> Option<VideoOutput> {
        let expand = sdr_to_hdr && self.display().hdr && self.upscaler.rtx_possible();
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
        let shown = scaled
            .and_then(|_| self.upscaler.output_target())
            .unwrap_or(&video.rgb);
        let expanded = if expand && !video.pq {
            self.hdr_expander
                .process(&self.device, &self.context, shown, generation)
        } else {
            None
        };
        self.hdr_present = video.pq || expanded.is_some();
        self.present_srv = Some(expanded.unwrap_or_else(|| shown.srv.clone()));
        Some(match scaled {
            Some(s) => VideoOutput {
                composited: self.hdr_present,
                texture_id: s.texture_id,
                size: s.size,
                source,
                scaler: Some(s.kind),
                hdr: video.hdr,
            },
            None => VideoOutput {
                composited: self.hdr_present,
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

    fn render_hdr(
        &mut self,
        ctx: &egui::Context,
        output: egui_directx11::RendererOutput,
        back_buffer: &ID3D11RenderTargetView,
        video_rect: Option<[f32; 4]>,
    ) -> Result<(), String> {
        if self.ui_layer.as_ref().is_none_or(|l| l.size() != self.size) {
            self.ui_layer = Some(
                RenderTexture::new(
                    &self.device,
                    self.size.0,
                    self.size.1,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                )
                .map_err(|e| format!("Interface layer: {e}"))?,
            );
        }
        let layer = self.ui_layer.as_ref().unwrap();
        unsafe {
            self.context
                .OMSetRenderTargets(Some(&[Some(layer.rtv.clone())]), None);
            self.context
                .ClearRenderTargetView(&layer.rtv, &[0.0, 0.0, 0.0, 0.0]);
        }
        self.egui
            .render(&self.context, &layer.rtv, ctx, output)
            .map_err(|e| e.to_string())?;
        let video = self.present_srv.clone().zip(video_rect);
        let rect = video.as_ref().map(|(_, r)| *r).unwrap_or_default();
        let mut constants = [0.0f32; 16];
        constants[..4].copy_from_slice(&rect);
        constants[4] = UI_WHITE_NITS;
        constants[5] = if video.is_some() { 1.0 } else { 0.0 };
        self.pipeline.passes.draw_to(
            &self.context,
            &self.pipeline.composite_shader,
            &constants.map(f32::to_bits),
            &[Some(layer.srv.clone()), video.map(|(srv, _)| srv)],
            back_buffer,
            self.size,
        );
        Ok(())
    }

    /// `video_rect` is where the interface left room for a composited (HDR) video, in pixels.
    pub fn render(
        &mut self,
        ctx: &egui::Context,
        output: egui_directx11::RendererOutput,
        immediate: bool,
        video_rect: Option<[f32; 4]>,
    ) -> Result<(), String> {
        let composite = self.hdr_present && self.video.as_ref().is_some_and(|v| v.has_frame);
        self.set_hdr_swap(composite);
        let Some(back_buffer) = self.back_buffer.clone() else {
            return Ok(());
        };
        if self.hdr_swap {
            self.render_hdr(ctx, output, &back_buffer, video_rect)?;
        } else {
            unsafe {
                self.context
                    .OMSetRenderTargets(Some(&[Some(back_buffer.clone())]), None);
                self.context
                    .ClearRenderTargetView(&back_buffer, &[0.04, 0.04, 0.05, 1.0]);
            }
            self.egui
                .render(&self.context, &back_buffer, ctx, output)
                .map_err(|e| e.to_string())?;
        }
        let hr = unsafe {
            if immediate && self.tearing {
                self.swap_chain.Present(0, DXGI_PRESENT_ALLOW_TEARING)
            } else {
                self.swap_chain.Present(1, DXGI_PRESENT(0))
            }
        };
        if let Some(times) = self.unpresented.take() {
            self.latency.record(times, native::now_us());
        }
        if hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET {
            return Err("Graphics device lost (the GPU driver was reset)".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_meter_splits_and_smooths() {
        let mut meter = LatencyMeter::default();
        assert_eq!(meter.get(), None);
        let times = native::FrameTimes {
            received_us: 1_000_000,
            decoded_us: 1_002_000,
        };
        meter.record(times, 1_008_000);
        assert_eq!(
            meter.get(),
            Some(DisplayLatency {
                decode_ms: 2.0,
                display_ms: 6.0
            })
        );
        meter.record(times, 1_018_000);
        let smoothed = meter.get().unwrap();
        assert!(smoothed.display_ms > 6.0 && smoothed.display_ms < 8.0);
        meter.record(native::FrameTimes::default(), 5_000_000);
        assert_eq!(meter.get(), Some(smoothed));
    }
}
