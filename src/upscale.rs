use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::core::{GUID, Interface, s};

use crate::config::Upscaling;
use crate::render::{blob_bytes, compile};

const FFX_A: &str = include_str!("../shaders/fsr/ffx_a.h");
const FFX_FSR1: &str = include_str!("../shaders/fsr/ffx_fsr1.h");
const NVIDIA_VENDOR_ID: u32 = 0x10de;
const MAX_OUTPUT: (u32, u32) = (3840, 2160);
const MIN_SCALE: f32 = 1.05;

// NVIDIA driver extension that turns on RTX Video Super Resolution for a D3D11 video processor
// stream (same mechanism as used by mpv and VLC).
const NVIDIA_PPE_INTERFACE: GUID = GUID::from_u128(0xd43ce1b3_1f4b_48ac_baee_c3c25375e6f7);
#[repr(C)]
struct NvidiaStreamExtension {
    version: u32,
    method: u32,
    enable: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScalerKind {
    RtxVsr,
    Fsr,
}

impl ScalerKind {
    pub fn label(self) -> &'static str {
        match self {
            ScalerKind::RtxVsr => "RTX Video Super Resolution",
            ScalerKind::Fsr => "AMD FSR 1",
        }
    }
}

/// What was produced for the current frame: the texture egui should show and how.
#[derive(Clone, Copy, Debug)]
pub struct Scaled {
    pub texture_id: egui::TextureId,
    pub size: [u32; 2],
    pub kind: ScalerKind,
}

struct Target {
    width: u32,
    height: u32,
    texture: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    srv: ID3D11ShaderResourceView,
}

impl Target {
    fn new(device: &ID3D11Device, width: u32, height: u32) -> windows::core::Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
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
}

struct RtxProcessor {
    input_size: (u32, u32),
    output_size: (u32, u32),
    source: usize,
    _enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    output_view: ID3D11VideoProcessorOutputView,
}

struct Fsr {
    vertex_shader: ID3D11VertexShader,
    easu: ID3D11PixelShader,
    rcas: ID3D11PixelShader,
    constants: ID3D11Buffer,
    sampler: ID3D11SamplerState,
}

pub struct Upscaler {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    nvidia: bool,
    rtx_failed: bool,
    rtx: Option<RtxProcessor>,
    fsr: Option<Fsr>,
    fsr_failed: bool,
    intermediate: Option<Target>,
    output: Option<(Target, egui::TextureId)>,
    last: Option<(u64, (u32, u32), ScalerKind, u32)>,
    pub active: Option<Scaled>,
}

pub fn easu_constants(input: (u32, u32), output: (u32, u32)) -> [u32; 16] {
    let (iw, ih) = (input.0 as f32, input.1 as f32);
    let (ow, oh) = (output.0 as f32, output.1 as f32);
    let f = |v: f32| v.to_bits();
    [
        f(iw / ow),
        f(ih / oh),
        f(0.5 * iw / ow - 0.5),
        f(0.5 * ih / oh - 0.5),
        f(1.0 / iw),
        f(1.0 / ih),
        f(1.0 / iw),
        f(-1.0 / ih),
        f(-1.0 / iw),
        f(2.0 / ih),
        f(1.0 / iw),
        f(2.0 / ih),
        f(0.0),
        f(4.0 / ih),
        0,
        0,
    ]
}

/// Sharpness 0..1 (1 = sharpest) to RCAS stops of sharpness reduction.
pub fn rcas_constants(sharpness: f32) -> [u32; 16] {
    let stops = (1.0 - sharpness.clamp(0.0, 1.0)) * 2.0;
    let mut con = [0u32; 16];
    con[0] = (-stops).exp2().to_bits();
    con
}

/// Output size that fills `target` while keeping the source aspect, capped at 4K.
/// Returns None when the result would not be meaningfully larger than the source.
pub fn plan_output(source: (u32, u32), target: [u32; 2]) -> Option<(u32, u32)> {
    let (sw, sh) = (source.0 as f32, source.1 as f32);
    let scale = (target[0] as f32 / sw)
        .min(target[1] as f32 / sh)
        .min(MAX_OUTPUT.0 as f32 / sw)
        .min(MAX_OUTPUT.1 as f32 / sh);
    if scale < MIN_SCALE {
        return None;
    }
    let even = |v: f32| ((v.round() as u32) & !1).max(2);
    Some((even(sw * scale), even(sh * scale)))
}

fn fsr_shader(pass: &str) -> String {
    let (define, callbacks, entry) = if pass == "easu" {
        (
            "#define FSR_EASU_F 1",
            r#"
cbuffer Constants : register(b0) { uint4 con0; uint4 con1; uint4 con2; uint4 con3; };
Texture2D<float4> source_tex : register(t0);
SamplerState source_sampler : register(s0);
AF4 FsrEasuRF(AF2 p) { return source_tex.GatherRed(source_sampler, p, int2(0, 0)); }
AF4 FsrEasuGF(AF2 p) { return source_tex.GatherGreen(source_sampler, p, int2(0, 0)); }
AF4 FsrEasuBF(AF2 p) { return source_tex.GatherBlue(source_sampler, p, int2(0, 0)); }
"#,
            r#"
float4 ps_main(float4 pos : SV_Position) : SV_Target {
    AF3 color;
    FsrEasuF(color, AU2(pos.xy), con0, con1, con2, con3);
    return float4(color, 1.0);
}
"#,
        )
    } else {
        (
            "#define FSR_RCAS_F 1",
            r#"
cbuffer Constants : register(b0) { uint4 con0; };
Texture2D<float4> source_tex : register(t0);
AF4 FsrRcasLoadF(ASU2 p) { return source_tex.Load(int3(p, 0)); }
void FsrRcasInputF(inout AF1 r, inout AF1 g, inout AF1 b) {}
"#,
            r#"
float4 ps_main(float4 pos : SV_Position) : SV_Target {
    AF1 r, g, b;
    FsrRcasF(r, g, b, AU2(pos.xy), con0);
    return float4(r, g, b, 1.0);
}
"#,
        )
    };
    format!(
        "#define A_GPU 1\n#define A_HLSL 1\n{FFX_A}\n{callbacks}\n{define}\n{FFX_FSR1}\n{entry}"
    )
}

const FULLSCREEN_VS: &str = r#"
float4 vs_main(uint id : SV_VertexID) : SV_Position {
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
}
"#;

impl Fsr {
    fn new(device: &ID3D11Device) -> Result<Self, String> {
        let vs = compile(FULLSCREEN_VS, s!("vs_main"), s!("vs_5_0"))?;
        let easu = compile(&fsr_shader("easu"), s!("ps_main"), s!("ps_5_0"))?;
        let rcas = compile(&fsr_shader("rcas"), s!("ps_main"), s!("ps_5_0"))?;
        let err = |e: windows::core::Error| format!("FSR: {e}");
        unsafe {
            let mut vertex_shader = None;
            let mut easu_ps = None;
            let mut rcas_ps = None;
            device
                .CreateVertexShader(blob_bytes(&vs), None, Some(&mut vertex_shader))
                .map_err(err)?;
            device
                .CreatePixelShader(blob_bytes(&easu), None, Some(&mut easu_ps))
                .map_err(err)?;
            device
                .CreatePixelShader(blob_bytes(&rcas), None, Some(&mut rcas_ps))
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
            Ok(Self {
                vertex_shader: vertex_shader.unwrap(),
                easu: easu_ps.unwrap(),
                rcas: rcas_ps.unwrap(),
                constants: constants.unwrap(),
                sampler: sampler.unwrap(),
            })
        }
    }

    fn pass(
        &self,
        context: &ID3D11DeviceContext,
        shader: &ID3D11PixelShader,
        constants: &[u32; 16],
        source: &ID3D11ShaderResourceView,
        target: &Target,
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
            context.PSSetShaderResources(0, Some(&[Some(source.clone())]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.Draw(3, 0);
            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetRenderTargets(None, None);
        }
    }
}

fn create_rtx(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    source: &ID3D11Texture2D,
    input_size: (u32, u32),
    output: &Target,
) -> Result<RtxProcessor, String> {
    let video_device: ID3D11VideoDevice =
        device.cast().map_err(|e| format!("video device: {e}"))?;
    let video_context: ID3D11VideoContext1 =
        context.cast().map_err(|e| format!("video context: {e}"))?;
    let rate = DXGI_RATIONAL {
        Numerator: 60,
        Denominator: 1,
    };
    let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: rate,
        InputWidth: input_size.0,
        InputHeight: input_size.1,
        OutputFrameRate: rate,
        OutputWidth: output.width,
        OutputHeight: output.height,
        Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
    };
    unsafe {
        let enumerator = video_device
            .CreateVideoProcessorEnumerator(&content)
            .map_err(|e| format!("enumerator: {e}"))?;
        let processor = video_device
            .CreateVideoProcessor(&enumerator, 0)
            .map_err(|e| format!("processor: {e}"))?;
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut input_view = None;
        video_device
            .CreateVideoProcessorInputView(source, &enumerator, &input_desc, Some(&mut input_view))
            .map_err(|e| format!("input view: {e}"))?;
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_view = None;
        video_device
            .CreateVideoProcessorOutputView(
                &output.texture,
                &enumerator,
                &output_desc,
                Some(&mut output_view),
            )
            .map_err(|e| format!("output view: {e}"))?;

        let source_rect = RECT {
            left: 0,
            top: 0,
            right: input_size.0 as i32,
            bottom: input_size.1 as i32,
        };
        let dest_rect = RECT {
            left: 0,
            top: 0,
            right: output.width as i32,
            bottom: output.height as i32,
        };
        video_context.VideoProcessorSetStreamFrameFormat(
            &processor,
            0,
            D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        );
        video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source_rect));
        video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dest_rect));
        video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dest_rect));
        video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
        video_context.VideoProcessorSetStreamColorSpace1(
            &processor,
            0,
            DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
        );
        video_context.VideoProcessorSetOutputColorSpace1(
            &processor,
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
        );

        let extension = NvidiaStreamExtension {
            version: 0x1,
            method: 0x2,
            enable: 1,
        };
        let hr = video_context.VideoProcessorSetStreamExtension(
            &processor,
            0,
            &NVIDIA_PPE_INTERFACE,
            std::mem::size_of::<NvidiaStreamExtension>() as u32,
            &extension as *const _ as *const _,
        );
        if hr != 0 {
            return Err(format!(
                "NVIDIA extension: {}",
                windows::core::Error::from_hresult(windows::core::HRESULT(hr))
            ));
        }
        Ok(RtxProcessor {
            input_size,
            output_size: (output.width, output.height),
            source: source.as_raw() as usize,
            _enumerator: enumerator,
            processor,
            input_view: input_view.unwrap(),
            output_view: output_view.unwrap(),
        })
    }
}

impl Upscaler {
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Self {
        let nvidia = device
            .cast::<IDXGIDevice>()
            .and_then(|d| unsafe { d.GetAdapter() })
            .and_then(|a| unsafe { a.GetDesc() })
            .map(|desc| desc.VendorId == NVIDIA_VENDOR_ID)
            .unwrap_or(false);
        Self {
            device: device.clone(),
            context: context.clone(),
            nvidia,
            rtx_failed: false,
            rtx: None,
            fsr: None,
            fsr_failed: false,
            intermediate: None,
            output: None,
            last: None,
            active: None,
        }
    }

    pub fn rtx_possible(&self) -> bool {
        self.nvidia && !self.rtx_failed
    }

    /// Drops all textures that depend on the egui renderer.
    pub fn reset(&mut self, egui: &mut egui_directx11::Renderer) {
        if let Some((_, id)) = self.output.take() {
            egui.unregister_user_texture(id);
        }
        self.rtx = None;
        self.intermediate = None;
        self.last = None;
        self.active = None;
    }

    fn ensure_output(
        &mut self,
        egui: &mut egui_directx11::Renderer,
        size: (u32, u32),
    ) -> Option<egui::TextureId> {
        if let Some((target, id)) = &self.output
            && (target.width, target.height) == size
        {
            return Some(*id);
        }
        if let Some((_, id)) = self.output.take() {
            egui.unregister_user_texture(id);
        }
        self.rtx = None;
        let target = match Target::new(&self.device, size.0, size.1) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("Upscaler output {}x{}: {e}", size.0, size.1);
                return None;
            }
        };
        let id = egui.register_user_texture(target.srv.clone());
        self.output = Some((target, id));
        Some(id)
    }

    /// Upscales the newest frame to `target` pixels. `yuv` is the decoded NV12 frame (used by
    /// RTX VSR), `rgb` the converted frame (used by FSR). Returns None when no upscaling applies.
    #[allow(clippy::too_many_arguments)]
    pub fn process(
        &mut self,
        egui: &mut egui_directx11::Renderer,
        mode: Upscaling,
        sharpness: f32,
        sequence: u64,
        source_size: (u32, u32),
        yuv: &ID3D11Texture2D,
        rgb: &ID3D11ShaderResourceView,
        target: Option<[u32; 2]>,
    ) -> Option<Scaled> {
        let plan = match (mode, target) {
            (Upscaling::Off, _) | (_, None) => None,
            (_, Some(target)) => plan_output(source_size, target),
        };
        let Some(out_size) = plan else {
            if self.active.is_some() {
                self.reset(egui);
            }
            return None;
        };
        let want_rtx = mode == Upscaling::Auto && self.rtx_possible();
        let kind = if want_rtx {
            ScalerKind::RtxVsr
        } else {
            ScalerKind::Fsr
        };
        let key = (sequence, out_size, kind, sharpness.to_bits());
        if self.last == Some(key)
            && let Some(active) = self.active
        {
            return Some(active);
        }
        let texture_id = self.ensure_output(egui, out_size)?;

        let done = if want_rtx {
            match self.run_rtx(yuv, source_size) {
                Ok(()) => true,
                Err(e) => {
                    log::warn!("RTX Video Super Resolution unavailable ({e}), using FSR");
                    self.rtx_failed = true;
                    self.rtx = None;
                    false
                }
            }
        } else {
            false
        };
        let kind = if done {
            ScalerKind::RtxVsr
        } else if self.run_fsr(rgb, source_size, sharpness) {
            ScalerKind::Fsr
        } else {
            self.reset(egui);
            return None;
        };
        if self.active.map(|a| (a.kind, a.size)) != Some((kind, [out_size.0, out_size.1])) {
            log::info!(
                "Upscaling {}x{} -> {}x{} with {}",
                source_size.0,
                source_size.1,
                out_size.0,
                out_size.1,
                kind.label()
            );
        }
        let scaled = Scaled {
            texture_id,
            size: [out_size.0, out_size.1],
            kind,
        };
        self.last = Some((sequence, out_size, kind, sharpness.to_bits()));
        self.active = Some(scaled);
        Some(scaled)
    }

    fn run_rtx(&mut self, yuv: &ID3D11Texture2D, source_size: (u32, u32)) -> Result<(), String> {
        let (output, _) = self.output.as_ref().expect("output target");
        let out_size = (output.width, output.height);
        let stale = self.rtx.as_ref().is_none_or(|p| {
            p.input_size != source_size
                || p.output_size != out_size
                || p.source != yuv.as_raw() as usize
        });
        if stale {
            self.rtx = Some(create_rtx(
                &self.device,
                &self.context,
                yuv,
                source_size,
                output,
            )?);
        }
        let processor = self.rtx.as_ref().unwrap();
        let video_context: ID3D11VideoContext = self.context.cast().map_err(|e| e.to_string())?;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(processor.input_view.clone())),
            ..Default::default()
        };
        let result = unsafe {
            video_context.VideoProcessorBlt(
                &processor.processor,
                &processor.output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.map_err(|e| format!("blt: {e}"))
    }

    fn run_fsr(
        &mut self,
        rgb: &ID3D11ShaderResourceView,
        source_size: (u32, u32),
        sharpness: f32,
    ) -> bool {
        if self.fsr_failed {
            return false;
        }
        if self.fsr.is_none() {
            match Fsr::new(&self.device) {
                Ok(fsr) => self.fsr = Some(fsr),
                Err(e) => {
                    log::error!("{e}");
                    self.fsr_failed = true;
                    return false;
                }
            }
        }
        let (output, _) = self.output.as_ref().expect("output target");
        let out_size = (output.width, output.height);
        if self
            .intermediate
            .as_ref()
            .is_none_or(|t| (t.width, t.height) != out_size)
        {
            match Target::new(&self.device, out_size.0, out_size.1) {
                Ok(t) => self.intermediate = Some(t),
                Err(e) => {
                    log::warn!("FSR intermediate: {e}");
                    return false;
                }
            }
        }
        let fsr = self.fsr.as_ref().unwrap();
        let intermediate = self.intermediate.as_ref().unwrap();
        fsr.pass(
            &self.context,
            &fsr.easu,
            &easu_constants(source_size, out_size),
            rgb,
            intermediate,
        );
        fsr.pass(
            &self.context,
            &fsr.rcas,
            &rcas_constants(sharpness),
            &intermediate.srv,
            output,
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_output_only_when_bigger() {
        assert_eq!(plan_output((1920, 1080), [1920, 1080]), None);
        assert_eq!(plan_output((1920, 1080), [1280, 720]), None);
        assert_eq!(plan_output((1920, 1080), [2560, 1440]), Some((2560, 1440)));
        assert_eq!(plan_output((1920, 1080), [2560, 1600]), Some((2560, 1440)));
        assert_eq!(plan_output((1920, 1080), [7680, 4320]), Some((3840, 2160)));
        assert_eq!(plan_output((1280, 720), [1920, 1080]), Some((1920, 1080)));
    }

    #[test]
    fn easu_constants_match_reference_layout() {
        let con = easu_constants((1920, 1080), (3840, 2160));
        assert_eq!(f32::from_bits(con[0]), 0.5);
        assert_eq!(f32::from_bits(con[2]), -0.25);
        assert_eq!(f32::from_bits(con[7]), -1.0 / 1080.0);
        assert_eq!(f32::from_bits(con[13]), 4.0 / 1080.0);
    }

    #[test]
    fn rcas_sharpness_maps_to_stops() {
        assert_eq!(f32::from_bits(rcas_constants(1.0)[0]), 1.0);
        assert_eq!(f32::from_bits(rcas_constants(0.5)[0]), 0.5);
        assert_eq!(f32::from_bits(rcas_constants(0.0)[0]), 0.25);
    }

    fn device() -> (ID3D11Device, ID3D11DeviceContext) {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::{
            D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_1,
        };
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .unwrap();
        }
        (device.unwrap(), context.unwrap())
    }

    fn texture(
        device: &ID3D11Device,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
        data: &[u8],
        pitch: u32,
    ) -> ID3D11Texture2D {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: if format == DXGI_FORMAT_NV12 {
                (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_DECODER.0) as u32
            } else {
                D3D11_BIND_SHADER_RESOURCE.0 as u32
            },
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: data.as_ptr() as *const _,
            SysMemPitch: pitch,
            SysMemSlicePitch: 0,
        };
        let mut tex = None;
        unsafe {
            device
                .CreateTexture2D(&desc, Some(&init), Some(&mut tex))
                .unwrap()
        };
        tex.unwrap()
    }

    fn read_back(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        tex: &ID3D11Texture2D,
    ) -> (u32, u32, Vec<u8>) {
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap();
            let staging = staging.unwrap();
            context.CopyResource(&staging, tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let mut rgba = Vec::new();
            for y in 0..desc.Height as usize {
                let row = std::slice::from_raw_parts(
                    (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                    desc.Width as usize * 4,
                );
                for px in row.as_chunks::<4>().0 {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                }
            }
            context.Unmap(&staging, 0);
            (desc.Width, desc.Height, rgba)
        }
    }

    #[test]
    #[ignore = "needs a GPU and PSLC_TEST_FRAME (raw 1920x1080 nv12) + PSLC_TEST_OUT"]
    fn gpu_upscale_to_4k() {
        let frame = std::fs::read(std::env::var("PSLC_TEST_FRAME").unwrap()).unwrap();
        let out_dir = std::path::PathBuf::from(std::env::var("PSLC_TEST_OUT").unwrap());
        let (w, h) = (1920u32, 1080u32);
        assert_eq!(frame.len(), (w * h * 3 / 2) as usize);
        let (device, context) = device();
        let yuv = texture(&device, w, h, DXGI_FORMAT_NV12, &frame, w);
        let mut bgra = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as usize {
            for x in 0..w as usize {
                let luma = frame[y * w as usize + x] as f32;
                let uv = (w * h) as usize + (y / 2) * w as usize + (x & !1);
                let (cb, cr) = (frame[uv] as f32 - 128.0, frame[uv + 1] as f32 - 128.0);
                let yy = (luma - 16.0) * 255.0 / 219.0;
                let k = 255.0 / 224.0;
                let r = yy + 1.5748 * cr * k;
                let g = yy - 0.1873 * cb * k - 0.4681 * cr * k;
                let b = yy + 1.8556 * cb * k;
                bgra.extend_from_slice(&[
                    b.clamp(0.0, 255.0) as u8,
                    g.clamp(0.0, 255.0) as u8,
                    r.clamp(0.0, 255.0) as u8,
                    255,
                ]);
            }
        }
        let rgb_tex = texture(&device, w, h, DXGI_FORMAT_B8G8R8A8_UNORM, &bgra, w * 4);
        let mut rgb_srv = None;
        unsafe {
            device
                .CreateShaderResourceView(&rgb_tex, None, Some(&mut rgb_srv))
                .unwrap()
        };
        let rgb_srv = rgb_srv.unwrap();
        image::save_buffer(
            out_dir.join("source-1080p.png"),
            &read_back(&device, &context, &rgb_tex).2,
            w,
            h,
            image::ColorType::Rgba8,
        )
        .unwrap();

        let mut egui = egui_directx11::Renderer::new(&device).unwrap();
        for (mode, name) in [(Upscaling::Auto, "auto"), (Upscaling::Fsr, "fsr")] {
            let mut upscaler = Upscaler::new(&device, &context);
            let scaled = upscaler
                .process(
                    &mut egui,
                    mode,
                    0.8,
                    1,
                    (w, h),
                    &yuv,
                    &rgb_srv,
                    Some([3840, 2160]),
                )
                .expect("upscaled");
            println!(
                "{name}: {:?} {:?} nvidia={}",
                scaled.kind, scaled.size, upscaler.nvidia
            );
            let (ow, oh, pixels) = read_back(
                &device,
                &context,
                &upscaler.output.as_ref().unwrap().0.texture,
            );
            assert_eq!((ow, oh), (3840, 2160));
            let lit = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[0] as u32 + p[1] as u32 + p[2] as u32 > 30)
                .count();
            assert!(lit > pixels.len() / 4 / 2, "{name}: output mostly black");
            image::save_buffer(
                out_dir.join(format!("{name}-{:?}-4k.png", scaled.kind)),
                &pixels,
                ow,
                oh,
                image::ColorType::Rgba8,
            )
            .unwrap();
        }
    }
}
