use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_1, D3D_SRV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::config::Upscaling;
use crate::render::{Enhance, RenderTexture, VideoPipeline};
use crate::upscale::{ScalerKind, Upscaler};

const W: u32 = 1920;
const H: u32 = 1080;

fn device() -> (ID3D11Device, ID3D11DeviceContext) {
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

struct Yuv {
    luma: ID3D11ShaderResourceView,
    chroma: ID3D11ShaderResourceView,
    ten_bit: bool,
}

fn upload_yuv(device: &ID3D11Device, data: &[u8], ten_bit: bool) -> Yuv {
    let bytes = if ten_bit { 2 } else { 1 };
    let desc = D3D11_TEXTURE2D_DESC {
        Width: W,
        Height: H,
        MipLevels: 1,
        ArraySize: 1,
        Format: if ten_bit {
            DXGI_FORMAT_P010
        } else {
            DXGI_FORMAT_NV12
        },
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_DECODER.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: data.as_ptr() as *const _,
        SysMemPitch: W * bytes,
        SysMemSlicePitch: 0,
    };
    let view = |format| D3D11_SHADER_RESOURCE_VIEW_DESC {
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
        let mut texture = None;
        device
            .CreateTexture2D(&desc, Some(&init), Some(&mut texture))
            .unwrap();
        let texture = texture.unwrap();
        let (lf, cf) = if ten_bit {
            (DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM)
        } else {
            (DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM)
        };
        let mut luma = None;
        let mut chroma = None;
        device
            .CreateShaderResourceView(&texture, Some(&view(lf)), Some(&mut luma))
            .unwrap();
        device
            .CreateShaderResourceView(&texture, Some(&view(cf)), Some(&mut chroma))
            .unwrap();
        Yuv {
            luma: luma.unwrap(),
            chroma: chroma.unwrap(),
            ten_bit,
        }
    }
}

fn read_back(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
) -> (u32, u32, Vec<u8>) {
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut staging))
            .unwrap();
        let staging = staging.unwrap();
        context.CopyResource(&staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .unwrap();
        let mut rgba = Vec::with_capacity((desc.Width * desc.Height * 4) as usize);
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

/// 8 bit test frame: top half a dark blue gradient quantized into visible bands,
/// bottom half saturated red/green/blue text-like stripes on gray.
fn sdr_frame() -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut data = vec![0u8; w * h * 3 / 2];
    let (luma, chroma) = data.split_at_mut(w * h);
    for y in 0..h {
        for x in 0..w {
            luma[y * w + x] = if y < h / 2 {
                let t = x as f32 / w as f32;
                (20.0 + (t * 40.0 / 3.0).floor() * 3.0) as u8
            } else if (x / 6) % 4 == 0 && (y / 40) % 2 == 0 {
                81
            } else {
                126
            };
        }
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let (x, y) = (cx * 2, cy * 2);
            let (cb, cr) = if y < h / 2 {
                (150u8, 120u8)
            } else if (x / 6) % 4 == 0 && (y / 40) % 2 == 0 {
                (90, 240)
            } else {
                (128, 128)
            };
            chroma[cy * w + cx * 2] = cb;
            chroma[cy * w + cx * 2 + 1] = cr;
        }
    }
    data
}

fn pq_encode(nits: f32) -> f32 {
    let (m1, m2, c1, c2, c3) = (0.159_301_76, 78.843_75, 0.835_937_5, 18.851_563, 18.687_5);
    let y = (nits / 10000.0).powf(m1);
    ((c1 + c2 * y) / (1.0 + c3 * y)).powf(m2)
}

/// 10 bit PQ frame: neutral gray ramp 0..1000 nits on top, colored patches at 200 nits below.
fn hdr_frame() -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut data = vec![0u16; w * h * 3 / 2];
    let code = |v: f32| ((64.0 + v * 876.0).round() as u16) << 6;
    let ccode = |v: f32| ((512.0 + v * 896.0).round() as u16) << 6;
    for y in 0..h {
        for x in 0..w {
            let e = if y < h / 2 {
                pq_encode(x as f32 / w as f32 * 1000.0)
            } else {
                pq_encode(200.0) * 0.9
            };
            data[y * w + x] = code(e);
        }
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let (cb, cr) = if cy * 2 < h / 2 {
                (0.0, 0.0)
            } else {
                match (cx * 2 * 3) / w {
                    0 => (-0.1, 0.4),
                    1 => (-0.3, -0.3),
                    _ => (0.4, -0.05),
                }
            };
            data[w * h + cy * w + cx * 2] = ccode(cb);
            data[w * h + cy * w + cx * 2 + 1] = ccode(cr);
        }
    }
    data.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn out_dir() -> std::path::PathBuf {
    let dir = std::env::var_os("PSLC_TEST_OUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("pslc-gpu-test"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn save(name: &str, (w, h, pixels): &(u32, u32, Vec<u8>)) {
    image::save_buffer(
        out_dir().join(name),
        pixels,
        *w,
        *h,
        image::ColorType::Rgba8,
    )
    .unwrap();
}

/// Largest jump between neighbouring 16 px averages of the blue channel: a hard band edge shows up
/// as a big jump, a smoothed gradient as small ones. Averaging hides grain and dither.
fn max_band_jump(row: &[u8]) -> f32 {
    let blue: Vec<f32> = row.as_chunks::<4>().0.iter().map(|p| p[2] as f32).collect();
    let averages: Vec<f32> = blue
        .windows(16)
        .step_by(4)
        .map(|w| w.iter().sum::<f32>() / 16.0)
        .collect();
    averages
        .windows(2)
        .map(|p| (p[1] - p[0]).abs())
        .fold(0.0, f32::max)
}

fn run(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    pipeline: &VideoPipeline,
    yuv: &Yuv,
    hdr: bool,
    enhance: Enhance,
) -> (RenderTexture, (u32, u32, Vec<u8>)) {
    let linear = RenderTexture::new(device, W, H, DXGI_FORMAT_R16G16B16A16_FLOAT).unwrap();
    let rgb = RenderTexture::new(device, W, H, DXGI_FORMAT_B8G8R8A8_UNORM).unwrap();
    pipeline.process(
        context,
        &yuv.luma,
        &yuv.chroma,
        yuv.ten_bit,
        hdr,
        enhance,
        &linear,
        &rgb,
    );
    let pixels = read_back(device, context, &rgb.texture);
    (rgb, pixels)
}

#[test]
#[ignore = "needs a Direct3D 11 GPU; writes PNGs to PSLC_TEST_OUT"]
fn enhance_pipeline_on_gpu() {
    let (device, context) = device();
    let pipeline = VideoPipeline::new(&device).unwrap();
    let plain = Enhance {
        better_chroma: false,
        deband: 0.0,
    };
    let full = Enhance {
        better_chroma: true,
        deband: 1.0,
    };

    let sdr = upload_yuv(&device, &sdr_frame(), false);
    let (_, before) = run(&device, &context, &pipeline, &sdr, false, plain);
    let (rgb, after) = run(&device, &context, &pipeline, &sdr, false, full);
    save("sdr-plain.png", &before);
    save("sdr-enhanced.png", &after);
    let row = |img: &(u32, u32, Vec<u8>), y: u32| {
        let start = (y * W * 4) as usize;
        img.2[start..start + (W * 4) as usize].to_vec()
    };
    let jump_before = max_band_jump(&row(&before, 200));
    let jump_after = max_band_jump(&row(&after, 200));
    println!("largest band jump: plain {jump_before:.2}, debanded {jump_after:.2}");
    assert!(
        jump_after < jump_before * 0.7,
        "deband did not smooth the gradient"
    );

    let hdr = upload_yuv(&device, &hdr_frame(), true);
    let (_, tonemapped) = run(&device, &context, &pipeline, &hdr, true, full);
    save("hdr-tonemapped.png", &tonemapped);
    let gray = |x: u32| tonemapped.2[((200 * W + x) * 4) as usize];
    println!(
        "hdr ramp: 0 nits {}, ~200 nits {}, ~500 nits {}, ~1000 nits {}",
        gray(2),
        gray(W / 5),
        gray(W / 2),
        gray(W - 2)
    );
    assert!(gray(2) < 40 && gray(W / 5) > 120 && gray(W - 2) > gray(W / 2));
    assert!(gray(W - 2) < 255, "highlights clip instead of rolling off");

    let mut egui = egui_directx11::Renderer::new(&device).unwrap();
    for (mode, sharpness, target, name) in [
        (Upscaling::Off, 0.6, [W, H], "sharpen-1080p.png"),
        (Upscaling::Fsr, 0.6, [3840, 2160], "fsr-4k.png"),
        (Upscaling::Auto, 0.0, [3840, 2160], "auto-4k.png"),
    ] {
        let mut upscaler = Upscaler::new(&device, &context);
        let scaled = upscaler
            .process(&mut egui, mode, sharpness, 1, &rgb, Some(target))
            .expect("output");
        println!("{name}: {:?} {:?}", scaled.kind, scaled.size);
        assert_eq!(scaled.size, target);
        if mode == Upscaling::Off {
            assert_eq!(scaled.kind, ScalerKind::Sharpen);
        }
        let image = read_back(&device, &context, &upscaler.output_texture().unwrap());
        save(name, &image);
    }

    let out = RenderTexture::new(&device, 3840, 2160, DXGI_FORMAT_B8G8R8A8_UNORM).unwrap();
    let mut results = Vec::new();
    for vsr in [false, true] {
        match crate::upscale::run_video_processor_once(&device, &context, &rgb, &out, vsr) {
            Ok(()) => results.push(read_back(&device, &context, &out.texture)),
            Err(e) => {
                println!("video processor (vsr={vsr}) failed: {e}");
                return;
            }
        }
    }
    save("vp-plain-4k.png", &results[0]);
    save("vp-vsr-4k.png", &results[1]);
    let differing = results[0]
        .2
        .iter()
        .zip(&results[1].2)
        .filter(|(a, b)| a.abs_diff(**b) > 3)
        .count();
    println!(
        "RTX VSR on RGB input changes {:.1}% of channel values",
        differing as f64 * 100.0 / results[0].2.len() as f64
    );
}
