//! Conversión BGRA -> NV12 con el procesador de video de la GPU (sin shaders ni CPU).
//! También endereza monitores verticales, estira la resolución estirada y encaja un monitor de otra forma sin deformarlo.

use std::mem::ManuallyDrop;

use anyhow::Result;
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::capture::Frame;

pub struct Converter {
    /// La imagen que convierte (si `Capture` cambia de textura, hace falta otro `Converter`).
    src: ID3D11Texture2D,
    vctx: ID3D11VideoContext,
    vp: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    pool: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    black: ID3D11Texture2D,
}

impl Converter {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        frame: &Frame,
        w: u32,
        h: u32,
        fps: u32,
        pool_size: usize,
    ) -> Result<Self> {
        unsafe {
            let vdev: ID3D11VideoDevice = device.cast()?;
            let vctx: ID3D11VideoContext = context.cast()?;
            let src = &frame.tex;
            let mut sd = D3D11_TEXTURE2D_DESC::default();
            src.GetDesc(&mut sd);

            let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
            let cd = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: sd.Width,
                InputHeight: sd.Height,
                OutputFrameRate: rate,
                OutputWidth: w,
                OutputHeight: h,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let en = vdev.CreateVideoProcessorEnumerator(&cd)?;
            let vp = vdev.CreateVideoProcessor(&en, 0)?;

            let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut input = None;
            vdev.CreateVideoProcessorInputView(src, &en, &ivd, Some(&mut input))?;

            let ovd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut pool = Vec::with_capacity(pool_size);
            for _ in 0..pool_size {
                let mut td = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut t = None;
                if device.CreateTexture2D(&td, None, Some(&mut t)).is_err() {
                    td.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
                    device.CreateTexture2D(&td, None, Some(&mut t))?;
                }
                let t = t.unwrap();
                let mut ov = None;
                vdev.CreateVideoProcessorOutputView(&t, &en, &ovd, Some(&mut ov))?;
                pool.push((t, ov.unwrap()));
            }

            // Colores correctos: RGB completo -> YUV BT.709 rango limitado.
            if let Ok(v1) = vctx.cast::<ID3D11VideoContext1>() {
                v1.VideoProcessorSetStreamColorSpace1(&vp, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                v1.VideoProcessorSetOutputColorSpace1(&vp, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            }
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);

            // Monitor vertical: la imagen llega acostada (DXGI_MODE_ROTATION va de 1 a 4, la del
            // procesador de 0 a 3, en el mismo orden).
            let quarter = frame.rotation.0 - 1;
            if quarter > 0 {
                vctx.VideoProcessorSetStreamRotation(&vp, 0, true, D3D11_VIDEO_PROCESSOR_ROTATION(quarter));
            }
            // Encajar con la forma con la que se ve en el monitor: estirada si Windows la estira (resolución
            // estirada); un monitor con otra forma queda con bandas negras.
            let (sw, sh) = frame.shape;
            let scale = (w as f64 / sw as f64).min(h as f64 / sh as f64);
            let dw = (sw as f64 * scale).round() as i32 & !1;
            let dh = (sh as f64 * scale).round() as i32 & !1;
            let (x, y) = (((w as i32 - dw) / 2) & !1, ((h as i32 - dh) / 2) & !1);
            let dest = RECT { left: x, top: y, right: x + dw, bottom: y + dh };
            vctx.VideoProcessorSetStreamDestRect(&vp, 0, true, Some(&dest));
            let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
            vctx.VideoProcessorSetOutputBackgroundColor(&vp, false, &black);

            // Cuadro negro (Y=16, UV=128) para el modo "solo juegos" cuando no hay juego en pantalla.
            let (bw, bh) = (w as usize, h as usize);
            let mut px = vec![16u8; bw * bh];
            px.resize(bw * bh * 3 / 2, 128);
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: px.as_ptr() as *const _, SysMemPitch: w, SysMemSlicePitch: 0 };
            let bd = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut black = None;
            device.CreateTexture2D(&bd, Some(&init), Some(&mut black))?;

            Ok(Self { src: src.clone(), vctx, vp, input: input.unwrap(), pool, next: 0, black: black.unwrap() })
        }
    }

    pub fn is_for(&self, frame: &Frame) -> bool {
        self.src == frame.tex
    }

    pub fn black(&self) -> ID3D11Texture2D {
        self.black.clone()
    }

    /// Convierte la imagen actual a una textura NV12 del pool y la devuelve.
    pub fn convert(&mut self) -> Result<ID3D11Texture2D> {
        let (tex, view) = &self.pool[self.next];
        self.next = (self.next + 1) % self.pool.len();
        unsafe {
            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
                ..Default::default()
            }];
            let r = self.vctx.VideoProcessorBlt(&self.vp, view, 0, &streams);
            ManuallyDrop::drop(&mut streams[0].pInputSurface);
            r?;
        }
        Ok(tex.clone())
    }
}
