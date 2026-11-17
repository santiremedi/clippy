//! Captura de un monitor con DXGI Desktop Duplication. Cuál monitor lo decide `recorder`
//! (el del juego o, si no hay juego, el de la ventana activa) y se puede cambiar sin cortar el buffer.
//! La imagen nunca sale de la GPU: se copia a una textura propia y listo.
//! No se inyecta nada en el juego (compatible con anticheats como Vanguard).

use anyhow::{anyhow, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, LUID, POINT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HMONITOR, MONITOR_DEFAULTTOPRIMARY};

use crate::log::log;

/// Una duplicación que no se pudo abrir se reintenta cada 5 s (100 ns).
const RETRY: i64 = 50_000_000;

/// Qué hizo `Capture::aim`.
pub enum Aim {
    /// Sigue igual (ya era ese monitor, o no se puede cambiar sin reiniciar y no hacía falta).
    Same,
    Switched,
    /// Hay que reiniciar la captura: el monitor está en otra GPU o el video tiene que tomar su resolución.
    Restart,
}

/// Última imagen del monitor (BGRA, sin rotar), en la GPU.
pub struct Frame {
    pub tex: ID3D11Texture2D,
    pub rotation: DXGI_MODE_ROTATION,
}

/// Un monitor visto desde una GPU.
struct Output {
    monitor: HMONITOR,
    output: IDXGIOutput1,
    name: String,
    /// Tamaño en el escritorio (ya rotado), redondeado a par como lo pide H.264.
    width: u32,
    height: u32,
}

pub struct Capture {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub luid: LUID,
    /// Tamaño del video, fijo durante la sesión: otro monitor se escala adentro sin deformarse.
    pub width: u32,
    pub height: u32,
    /// Es una textura nueva cuando cambia el tamaño o la rotación de la imagen.
    pub latest: Option<Frame>,
    adapter: IDXGIAdapter1,
    gpu: String,
    out: Output,
    dup: Option<IDXGIOutputDuplication>,
    rotation: DXGI_MODE_ROTATION,
    retry_at: i64,
    /// Ya se avisó en el log que la duplicación no abre (para no repetirlo cada 5 s).
    failing: bool,
    /// Último monitor que no se pudo seguir sin reiniciar (para avisarlo una sola vez).
    skipped: Option<HMONITOR>,
}

impl Capture {
    /// Captura `monitor` (o el principal si no hay o ya no existe). Prueba cada GPU que lo ve:
    /// en notebooks con dos GPUs la que lo enumera primero no siempre puede duplicarlo.
    pub fn new(monitor: Option<HMONITOR>) -> Result<Self> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let mut all = Vec::new();
        let mut i = 0;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
            i += 1;
            all.extend(outputs(&adapter).into_iter().map(|o| (adapter.clone(), o)));
        }
        let primary = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
        let want = monitor.filter(|m| all.iter().any(|(_, o)| o.monitor == *m)).unwrap_or(primary);
        let mut last = anyhow!("no se encontró ningún monitor para grabar");
        for (adapter, out) in all.into_iter().filter(|(_, o)| o.monitor == want) {
            let name = out.name.clone();
            match Self::open_on(adapter, out) {
                Ok(c) => return Ok(c),
                Err(e) => {
                    log(&format!("no se pudo capturar {name}: {e}"));
                    last = e;
                }
            }
        }
        Err(last)
    }

    fn open_on(adapter: IDXGIAdapter1, out: Output) -> Result<Self> {
        unsafe {
            let adesc = adapter.GetDesc1()?;
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.ok_or_else(|| anyhow!("sin dispositivo D3D11"))?;
            let context = context.ok_or_else(|| anyhow!("sin contexto D3D11"))?;
            // El encoder de Media Foundation usa el mismo dispositivo desde sus propios hilos.
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            let mut cap = Capture {
                device,
                context,
                luid: adesc.AdapterLuid,
                width: out.width,
                height: out.height,
                latest: None,
                adapter,
                gpu: wide(&adesc.Description),
                out,
                dup: None,
                rotation: DXGI_MODE_ROTATION::default(),
                retry_at: 0,
                failing: false,
                skipped: None,
            };
            cap.open()?;
            Ok(cap)
        }
    }

    /// "\\.\DISPLAY2 (NVIDIA GeForce RTX 2060)", para el log.
    pub fn describe(&self) -> String {
        format!("{} ({})", self.out.name, self.gpu)
    }

    fn open(&mut self) -> Result<()> {
        self.dup = None;
        let dup = unsafe { self.out.output.DuplicateOutput(&self.device)? };
        self.rotation = unsafe { dup.GetDesc() }.Rotation;
        self.dup = Some(dup);
        Ok(())
    }

    /// Pasa a grabar `monitor`. Con `native` el video tiene que quedar a la resolución de ese
    /// monitor (el del juego): si no coincide, pide reiniciar. Sin `native`, un monitor de otra
    /// resolución se escala adentro del video y uno de otra GPU se deja pasar (reiniciar perdería el buffer).
    pub fn aim(&mut self, monitor: HMONITOR, native: bool) -> Aim {
        let same_size = |o: &Output| (o.width, o.height) == (self.width, self.height);
        if monitor == self.out.monitor && (!native || same_size(&self.out)) {
            return Aim::Same;
        }
        let Some(out) = outputs(&self.adapter).into_iter().find(|o| o.monitor == monitor) else {
            return self.skip(monitor, native, "está en otra GPU");
        };
        if native && !same_size(&out) {
            log(&format!("{} tiene otra resolución ({}x{}): reiniciando la captura", out.name, out.width, out.height));
            return Aim::Restart;
        }
        let prev = std::mem::replace(&mut self.out, out);
        match self.open() {
            Ok(()) => {
                log(&format!("grabando {}", self.describe()));
                self.failing = false;
                self.skipped = None;
                Aim::Switched
            }
            Err(e) => {
                // Se vuelve al monitor de antes; `poll` reabre su duplicación.
                self.out = prev;
                self.dup = None;
                self.retry_at = 0;
                self.skip(monitor, native, &format!("no se pudo capturar con esta GPU ({e})"))
            }
        }
    }

    fn skip(&mut self, monitor: HMONITOR, native: bool, why: &str) -> Aim {
        if native {
            log(&format!("el monitor del juego {why}: reiniciando la captura"));
            return Aim::Restart;
        }
        if self.skipped != Some(monitor) {
            self.skipped = Some(monitor);
            log(&format!("la ventana activa está en un monitor que {why}: sigo grabando {}", self.out.name));
        }
        Aim::Same
    }

    /// Toma la imagen más reciente si hubo cambios (no bloquea).
    pub fn poll(&mut self, now: i64) -> Result<()> {
        if self.dup.is_none() {
            if now < self.retry_at {
                return Ok(());
            }
            // Escritorio seguro (UAC, bloqueo), cambio de modo u otra app que ocupa la duplicación.
            if let Err(e) = self.open() {
                self.retry_at = now + RETRY;
                if !self.failing {
                    self.failing = true;
                    log(&format!("no se puede capturar {} ({e}): el video queda congelado, reintento cada 5 s", self.out.name));
                }
                return Ok(());
            }
            if std::mem::take(&mut self.failing) {
                log(&format!("captura de {} recuperada", self.out.name));
            }
        }
        let dup = self.dup.clone().unwrap();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(0, &mut info, &mut res) } {
            Ok(()) => {
                let r = if info.LastPresentTime != 0 {
                    match res {
                        Some(res) => res.cast::<ID3D11Texture2D>().map_err(Into::into).and_then(|t| self.copy(&t)),
                        None => Ok(()),
                    }
                } else {
                    Ok(())
                };
                unsafe {
                    let _ = dup.ReleaseFrame();
                }
                r?;
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
            Err(e) => {
                if e.code() != DXGI_ERROR_ACCESS_LOST {
                    log(&format!("duplicación de {}: {e}", self.out.name));
                }
                self.dup = None;
                self.retry_at = now;
            }
        }
        Ok(())
    }

    fn copy(&mut self, frame: &ID3D11Texture2D) -> Result<()> {
        unsafe {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            frame.GetDesc(&mut d);
            let fits = self.latest.as_ref().is_some_and(|f| {
                let mut l = D3D11_TEXTURE2D_DESC::default();
                f.tex.GetDesc(&mut l);
                f.rotation == self.rotation && (l.Width, l.Height, l.Format) == (d.Width, d.Height, d.Format)
            });
            if !fits {
                d.MipLevels = 1;
                d.ArraySize = 1;
                d.Usage = D3D11_USAGE_DEFAULT;
                d.BindFlags = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;
                d.CPUAccessFlags = 0;
                d.MiscFlags = 0;
                let mut t = None;
                self.device.CreateTexture2D(&d, None, Some(&mut t))?;
                let tex = t.ok_or_else(|| anyhow!("sin textura de captura"))?;
                self.latest = Some(Frame { tex, rotation: self.rotation });
            }
            if let Some(f) = &self.latest {
                self.context.CopyResource(&f.tex, frame);
            }
        }
        Ok(())
    }
}

/// Monitores conectados al escritorio que ve esta GPU.
fn outputs(adapter: &IDXGIAdapter1) -> Vec<Output> {
    let mut v = Vec::new();
    let mut j = 0;
    while let Ok(output) = unsafe { adapter.EnumOutputs(j) } {
        j += 1;
        let (Ok(d), Ok(output)) = (unsafe { output.GetDesc() }, output.cast::<IDXGIOutput1>()) else { continue };
        if !d.AttachedToDesktop.as_bool() {
            continue;
        }
        let r = d.DesktopCoordinates;
        v.push(Output {
            monitor: d.Monitor,
            output,
            name: wide(&d.DeviceName),
            width: (r.right - r.left) as u32 & !1,
            height: (r.bottom - r.top) as u32 & !1,
        });
    }
    v
}

fn wide(s: &[u16]) -> String {
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..n])
}
