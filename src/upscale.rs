use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::core::{GUID, Interface};

use crate::config::Upscaling;
use crate::render::{Passes, RenderTexture, create_pixel_shader};

const FFX_A: &str = include_str!("../shaders/fsr/ffx_a.h");
const FFX_FSR1: &str = include_str!("../shaders/fsr/ffx_fsr1.h");
const NVIDIA_VENDOR_ID: u32 = 0x10de;
const MAX_OUTPUT: (u32, u32) = (3840, 2160);
const MIN_SCALE: f32 = 1.05;
const MIN_SHARPNESS: f32 = 0.01;

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
    Sharpen,
}

impl ScalerKind {
    pub fn label(self) -> &'static str {
        match self {
            ScalerKind::RtxVsr => "RTX Video Super Resolution",
            ScalerKind::Fsr => "AMD FSR 1",
            ScalerKind::Sharpen => "sharpening only",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            ScalerKind::RtxVsr => "RTX VSR",
            ScalerKind::Fsr => "FSR 1",
            ScalerKind::Sharpen => "Sharpen",
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

fn fsr_shader(easu: bool) -> String {
    let (define, callbacks, entry) = if easu {
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

struct Fsr {
    passes: Passes,
    easu: ID3D11PixelShader,
    rcas: ID3D11PixelShader,
}

impl Fsr {
    fn new(device: &ID3D11Device) -> Result<Self, String> {
        Ok(Self {
            passes: Passes::new(device)?,
            easu: create_pixel_shader(device, &fsr_shader(true))?,
            rcas: create_pixel_shader(device, &fsr_shader(false))?,
        })
    }
}

struct RtxProcessor {
    key: (usize, usize),
    _enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    output_view: ID3D11VideoProcessorOutputView,
}

fn create_rtx(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    source: &RenderTexture,
    output: &RenderTexture,
    enable_vsr: bool,
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
        InputWidth: source.width,
        InputHeight: source.height,
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
            .CreateVideoProcessorInputView(
                &source.texture,
                &enumerator,
                &input_desc,
                Some(&mut input_view),
            )
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
            right: source.width as i32,
            bottom: source.height as i32,
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
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
        );
        video_context.VideoProcessorSetOutputColorSpace1(
            &processor,
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
        );
        if enable_vsr {
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
        }
        Ok(RtxProcessor {
            key: (
                source.texture.as_raw() as usize,
                output.texture.as_raw() as usize,
            ),
            _enumerator: enumerator,
            processor,
            input_view: input_view.unwrap(),
            output_view: output_view.unwrap(),
        })
    }
}

fn run_video_processor(
    context: &ID3D11DeviceContext,
    processor: &RtxProcessor,
) -> Result<(), String> {
    let video_context: ID3D11VideoContext = context.cast().map_err(|e| e.to_string())?;
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

#[cfg(test)]
pub fn run_video_processor_once(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    source: &RenderTexture,
    output: &RenderTexture,
    vsr: bool,
) -> Result<(), String> {
    let processor = create_rtx(device, context, source, output, vsr)?;
    run_video_processor(context, &processor)
}

pub struct Upscaler {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    nvidia: bool,
    rtx_failed: bool,
    rtx: Option<RtxProcessor>,
    fsr: Option<Fsr>,
    fsr_failed: bool,
    intermediate: Option<RenderTexture>,
    output: Option<(RenderTexture, egui::TextureId)>,
    last: Option<(u64, (u32, u32), ScalerKind, u32)>,
    pub active: Option<Scaled>,
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

    #[cfg(test)]
    pub fn output_texture(&self) -> Option<ID3D11Texture2D> {
        self.output.as_ref().map(|(t, _)| t.texture.clone())
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
            && target.size() == size
        {
            return Some(*id);
        }
        if let Some((_, id)) = self.output.take() {
            egui.unregister_user_texture(id);
        }
        self.rtx = None;
        let target = RenderTexture::new(&self.device, size.0, size.1, DXGI_FORMAT_B8G8R8A8_UNORM)
            .map_err(|e| log::warn!("Upscaler output {}x{}: {e}", size.0, size.1))
            .ok()?;
        let id = egui.register_user_texture(target.srv.clone());
        self.output = Some((target, id));
        Some(id)
    }

    fn ensure_intermediate(&mut self, size: (u32, u32)) -> bool {
        if self.intermediate.as_ref().is_some_and(|t| t.size() == size) {
            return true;
        }
        self.rtx = None;
        match RenderTexture::new(&self.device, size.0, size.1, DXGI_FORMAT_B8G8R8A8_UNORM) {
            Ok(t) => {
                self.intermediate = Some(t);
                true
            }
            Err(e) => {
                log::warn!("Upscaler intermediate: {e}");
                false
            }
        }
    }

    fn ensure_fsr(&mut self) -> bool {
        if self.fsr.is_some() {
            return true;
        }
        if self.fsr_failed {
            return false;
        }
        match Fsr::new(&self.device) {
            Ok(fsr) => {
                self.fsr = Some(fsr);
                true
            }
            Err(e) => {
                log::error!("{e}");
                self.fsr_failed = true;
                false
            }
        }
    }

    /// Upscales and/or sharpens the processed frame to fit `target` pixels.
    /// Returns None when neither applies, so the frame should be shown as is.
    pub fn process(
        &mut self,
        egui: &mut egui_directx11::Renderer,
        mode: Upscaling,
        sharpness: f32,
        generation: u64,
        source: &RenderTexture,
        target: Option<[u32; 2]>,
    ) -> Option<Scaled> {
        let scale = match (mode, target) {
            (Upscaling::Off, _) | (_, None) => None,
            (_, Some(target)) => plan_output(source.size(), target),
        };
        let sharpen = sharpness >= MIN_SHARPNESS;
        if scale.is_none() && !sharpen {
            if self.active.is_some() {
                self.reset(egui);
            }
            return None;
        }
        let out_size = scale.unwrap_or(source.size());
        let mut kind = match scale {
            None => ScalerKind::Sharpen,
            Some(_) if mode == Upscaling::Auto && self.rtx_possible() => ScalerKind::RtxVsr,
            Some(_) => ScalerKind::Fsr,
        };
        let key = (generation, out_size, kind, sharpness.to_bits());
        if self.last == Some(key)
            && let Some(active) = self.active
        {
            return Some(active);
        }
        let texture_id = self.ensure_output(egui, out_size)?;
        let needs_intermediate = scale.is_some() && sharpen;
        if needs_intermediate && !self.ensure_intermediate(out_size) {
            return None;
        }

        if kind == ScalerKind::RtxVsr
            && let Err(e) = self.run_rtx(source, needs_intermediate)
        {
            log::warn!("RTX Video Super Resolution unavailable ({e}), using FSR");
            self.rtx_failed = true;
            self.rtx = None;
            kind = ScalerKind::Fsr;
        }
        if !self.ensure_fsr() {
            self.reset(egui);
            return None;
        }
        let fsr = self.fsr.as_ref().unwrap();
        let output = &self.output.as_ref().unwrap().0;
        let scaled_target = if needs_intermediate {
            self.intermediate.as_ref().unwrap()
        } else {
            output
        };
        if kind == ScalerKind::Fsr {
            fsr.passes.draw(
                &self.context,
                &fsr.easu,
                &easu_constants(source.size(), out_size),
                &[Some(source.srv.clone())],
                scaled_target,
            );
        }
        if sharpen {
            let sharpen_source = if kind == ScalerKind::Sharpen {
                source
            } else {
                self.intermediate.as_ref().unwrap()
            };
            fsr.passes.draw(
                &self.context,
                &fsr.rcas,
                &rcas_constants(sharpness),
                &[Some(sharpen_source.srv.clone())],
                output,
            );
        }

        if self.active.map(|a| (a.kind, a.size)) != Some((kind, [out_size.0, out_size.1])) {
            log::info!(
                "Video output {}x{} -> {}x{}: {}{}",
                source.width,
                source.height,
                out_size.0,
                out_size.1,
                kind.label(),
                if sharpen && kind != ScalerKind::Sharpen {
                    " + sharpening"
                } else {
                    ""
                }
            );
        }
        let scaled = Scaled {
            texture_id,
            size: [out_size.0, out_size.1],
            kind,
        };
        self.last = Some((generation, out_size, kind, sharpness.to_bits()));
        self.active = Some(scaled);
        Some(scaled)
    }

    fn run_rtx(&mut self, source: &RenderTexture, to_intermediate: bool) -> Result<(), String> {
        let destination = if to_intermediate {
            self.intermediate.as_ref().unwrap()
        } else {
            &self.output.as_ref().unwrap().0
        };
        let key = (
            source.texture.as_raw() as usize,
            destination.texture.as_raw() as usize,
        );
        if self.rtx.as_ref().is_none_or(|p| p.key != key) {
            self.rtx = Some(create_rtx(
                &self.device,
                &self.context,
                source,
                destination,
                true,
            )?);
        }
        run_video_processor(&self.context, self.rtx.as_ref().unwrap())
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
}
