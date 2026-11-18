//! Captura de audio con WASAPI:
//! - "Juego": solo el proceso del juego (process loopback, incluye su árbol de procesos).
//! - "Escritorio": todo lo demás (process loopback excluyendo el juego), o todo el sistema si no hay juego detectado.
//! - "Micrófono": el dispositivo elegido o el predeterminado de Windows.
//!
//! Se captura en el formato del dispositivo (float, con sus canales) y Clippy lo baja a estéreo:
//! la conversión de Windows de 5.1/7.1 a estéreo atenúa el sonido 10 dB o más.
//! Se guarda en RAM como PCM 48 kHz estéreo y se mezcla/comprime a AAC recién al guardar.

use std::collections::VecDeque;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use windows::core::{implement, Interface, GUID, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Threading::CreateEventW;

use crate::i18n::tr;
use crate::mf::{frames_at, RATE};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Source {
    /// Todo lo que suena en el dispositivo de salida por defecto.
    System,
    /// Solo este proceso (y sus hijos).
    Only(u32),
    /// Todo menos este proceso (y sus hijos).
    AllBut(u32),
    /// Micrófono (id del dispositivo; vacío = el predeterminado de Windows).
    Mic(String),
}

pub struct Audio {
    client: IAudioClient,
    cap: IAudioCaptureClient,
    event: HANDLE,
    pub source: Source,
    pub device_id: String,
    /// Cuánto de cada canal capturado va a la izquierda y a la derecha.
    downmix: Vec<(f32, f32)>,
    /// Búfer reutilizado para el estéreo de cada paquete.
    stereo: Vec<i16>,
}

const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// Canales y máscara de altavoces del formato de mezcla de un cliente (estéreo si no se sabe).
unsafe fn mix_layout(client: &IAudioClient) -> (u16, u32) {
    let Ok(f) = client.GetMixFormat() else { return (2, 0x3) };
    let ch = (*f).nChannels.max(1);
    let mask = if (*f).wFormatTag == WAVE_FORMAT_EXTENSIBLE { (*(f as *const WAVEFORMATEXTENSIBLE)).dwChannelMask } else { 0 };
    CoTaskMemFree(Some(f as *const _));
    (ch, mask)
}

/// Cuánto de cada canal va a cada lado del estéreo. Adelante izquierda y derecha pasan enteros
/// (así un juego en estéreo se graba al mismo volumen que se escucha). Centro y envolventes suman
/// menos: muchas salidas 5.1/7.1 copian el estéreo a esos canales y sumarlos enteros lo subiría.
/// El subwoofer no se suma (solo repite graves).
fn downmix(channels: u16, mask: u32) -> Vec<(f32, f32)> {
    const H: f32 = 0.35;
    if channels == 1 {
        return vec![(1.0, 1.0)];
    }
    // Sin máscara: el orden estándar (adelante izq., der., centro, sub, atrás izq., der., ...).
    let mask = if mask == 0 { (1u32 << channels) - 1 } else { mask };
    (0..32)
        .filter(|b| mask & (1 << b) != 0)
        .take(channels as usize)
        .map(|b| match 1u32 << b {
            0x1 => (1.0, 0.0),
            0x2 => (0.0, 1.0),
            0x4 => (0.5, 0.5),
            0x10 | 0x40 | 0x200 => (H, 0.0),
            0x20 | 0x80 | 0x400 => (0.0, H),
            0x100 => (H, H),
            _ => (0.0, 0.0),
        })
        .chain(std::iter::repeat((0.0, 0.0)))
        .take(channels as usize)
        .collect()
}

impl Audio {
    pub fn open(source: Source) -> Result<Self> {
        unsafe {
            let mic = matches!(source, Source::Mic(_));
            let (client, device_id) = match &source {
                Source::System => {
                    let dev = default_device()?;
                    let id = device_id(&dev);
                    (dev.Activate::<IAudioClient>(CLSCTX_ALL, None)?, id)
                }
                Source::Only(pid) => (activate_process(*pid, true)?, String::new()),
                Source::AllBut(pid) => (activate_process(*pid, false)?, String::new()),
                Source::Mic(id) => {
                    let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                    let dev = if id.is_empty() {
                        en.GetDefaultAudioEndpoint(eCapture, eConsole)?
                    } else {
                        en.GetDevice(&windows::core::HSTRING::from(id.as_str()))?
                    };
                    (dev.Activate::<IAudioClient>(CLSCTX_ALL, None)?, device_id(&dev))
                }
            };
            let loopback = if mic { 0 } else { AUDCLNT_STREAMFLAGS_LOOPBACK };
            let event = CreateEventW(None, false, false, PCWSTR::null())?;
            // Los canales del dispositivo (la captura por proceso no tiene formato propio: usa el de la
            // salida por defecto). Windows solo cambia la frecuencia a 48 kHz; los canales los baja Clippy.
            let (channels, mask) = match &source {
                Source::Only(_) | Source::AllBut(_) => mix_layout(&default_device()?.Activate::<IAudioClient>(CLSCTX_ALL, None)?),
                _ => mix_layout(&client),
            };
            let format = WAVEFORMATEXTENSIBLE {
                Format: WAVEFORMATEX {
                    wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                    nChannels: channels,
                    nSamplesPerSec: RATE as u32,
                    nAvgBytesPerSec: RATE as u32 * channels as u32 * 4,
                    nBlockAlign: channels * 4,
                    wBitsPerSample: 32,
                    cbSize: 22,
                },
                Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
                dwChannelMask: if mask == 0 { (1u32 << channels) - 1 } else { mask },
                SubFormat: SUBTYPE_IEEE_FLOAT,
            };
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                loopback
                    | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                2_000_000,
                0,
                &format.Format,
                None,
            )?;
            client.SetEventHandle(event)?;
            let cap: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Self { client, cap, event, source, device_id, downmix: downmix(channels, mask), stereo: Vec::new() })
        }
    }

    /// Vacía los paquetes disponibles y los ubica en la línea de tiempo según su marca QPC.
    pub fn poll(&mut self, buf: &mut AudioBuf, t0: i64) -> Result<()> {
        unsafe {
            loop {
                if self.cap.GetNextPacketSize()? == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64;
                self.cap.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let pos = if qpc != 0 { frames_at(qpc as i64 - t0) } else { buf.end() };
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    buf.push_at(pos, None, frames as usize);
                } else {
                    let ch = self.downmix.len();
                    let src = std::slice::from_raw_parts(data as *const f32, frames as usize * ch);
                    self.stereo.clear();
                    for f in src.chunks_exact(ch) {
                        let (mut l, mut r) = (0.0f32, 0.0f32);
                        for (x, (gl, gr)) in f.iter().zip(&self.downmix) {
                            l += x * gl;
                            r += x * gr;
                        }
                        self.stereo.push((l.clamp(-1.0, 1.0) * 32767.0) as i16);
                        self.stereo.push((r.clamp(-1.0, 1.0) * 32767.0) as i16);
                    }
                    buf.push_at(pos, Some(&self.stereo), frames as usize);
                }
                self.cap.ReleaseBuffer(frames)?;
            }
        }
        Ok(())
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(mpsc::SyncSender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: Option<&IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let _ = self.0.try_send(());
        Ok(())
    }
}

/// PROPVARIANT con VT_BLOB (diseño de memoria x64) apuntando a los parámetros de activación.
#[repr(C)]
struct BlobVariant {
    vt: u16,
    r1: u16,
    r2: u16,
    r3: u16,
    size: u32,
    _pad: u32,
    data: *const u8,
}

unsafe fn activate_process(pid: u32, include: bool) -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: if include {
                    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
                } else {
                    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
                },
            },
        },
    };
    let blob = BlobVariant {
        vt: 65, // VT_BLOB
        r1: 0,
        r2: 0,
        r3: 0,
        size: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        _pad: 0,
        data: &params as *const _ as *const u8,
    };
    let (tx, rx) = mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(tx).into();
    let op = ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &IAudioClient::IID,
        Some(&blob as *const BlobVariant as *const windows::core::PROPVARIANT),
        &handler,
    )?;
    // Esto corre en el hilo de video: si Windows no contesta enseguida, se sigue sin este audio
    // (se reintenta en unos segundos) en vez de congelar la grabación.
    rx.recv_timeout(Duration::from_secs(1)).map_err(|_| anyhow!("la captura de audio por proceso no respondió"))?;
    let mut hr = windows::core::HRESULT(0);
    let mut unk = None;
    op.GetActivateResult(&mut hr, &mut unk)?;
    hr.ok()?;
    Ok(unk.ok_or_else(|| anyhow!("sin cliente de audio"))?.cast()?)
}

unsafe fn default_device() -> Result<IMMDevice> {
    let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    Ok(en.GetDefaultAudioEndpoint(eRender, eConsole)?)
}

unsafe fn device_id(dev: &IMMDevice) -> String {
    match dev.GetId() {
        Ok(p) => {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            s
        }
        Err(_) => String::new(),
    }
}

/// Micrófonos conectados: (id, nombre).
pub fn capture_devices() -> Vec<(String, String)> {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::System::Com::STGM_READ;
    let mut v = Vec::new();
    unsafe {
        let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return v };
        let Ok(col) = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else { return v };
        for i in 0..col.GetCount().unwrap_or(0) {
            let Ok(dev) = col.Item(i) else { continue };
            let id = device_id(&dev);
            let name = dev
                .OpenPropertyStore(STGM_READ)
                .and_then(|ps| ps.GetValue(&PKEY_Device_FriendlyName))
                .map(|p| p.to_string())
                .unwrap_or_else(|_| tr!("Micrófono", "Microphone", "Microfone").into());
            v.push((id, name));
        }
    }
    v
}

/// Id del dispositivo de salida por defecto (para detectar si el usuario cambió de auriculares).
pub fn current_default_id() -> String {
    unsafe { default_device().map(|d| device_id(&d)).unwrap_or_default() }
}

/// Buffer circular de PCM estéreo intercalado. `start` = índice de frame (desde t0) de data[0].
#[derive(Default)]
pub struct AudioBuf {
    pub start: i64,
    pub data: VecDeque<i16>,
}

impl AudioBuf {
    pub fn end(&self) -> i64 {
        self.start + (self.data.len() / 2) as i64
    }

    pub fn push_at(&mut self, pos: i64, samples: Option<&[i16]>, frames: usize) {
        if self.data.is_empty() {
            self.start = pos;
        }
        let end = self.end();
        let mut skip = 0usize;
        if pos > end + RATE / 100 {
            // Hueco (no sonaba nada): rellenar con silencio para mantener la sincronía.
            let gap = (pos - end) as usize;
            self.data.extend(std::iter::repeat_n(0, gap * 2));
        } else if pos < end - RATE / 50 {
            // Vamos adelantados: descartar la parte superpuesta.
            skip = ((end - pos) as usize).min(frames);
        }
        match samples {
            Some(s) => self.data.extend(s[skip * 2..].iter().copied()),
            None => self.data.extend(std::iter::repeat_n(0, (frames - skip) * 2)),
        }
    }

    pub fn trim_before(&mut self, frame: i64) {
        if frame > self.start {
            let n = ((frame - self.start) as usize).min(self.data.len() / 2);
            self.data.drain(..n * 2);
            self.start += n as i64;
        }
    }

    /// Suma en `out` (estéreo intercalado, que empieza en el frame `from`) lo grabado en ese tramo,
    /// multiplicado por `gain`. Recorre las dos mitades del buffer circular de corrido.
    #[inline(never)] // se llama por tramo: copiarlo en cada llamada agranda el exe 3 KB sin ganar nada
    pub fn mix_into(&self, out: &mut [f32], from: i64, gain: f32) {
        let skip = (((self.start - from).max(0) * 2) as usize).min(out.len());
        let mut out = &mut out[skip..];
        let mut first = ((from - self.start).max(0) * 2) as usize;
        let (a, b) = self.data.as_slices();
        for part in [a, b] {
            let src = part.get(first..).unwrap_or(&[]);
            first = first.saturating_sub(part.len());
            let n = src.len().min(out.len());
            for (o, &s) in out[..n].iter_mut().zip(src) {
                *o += s as f32 * gain;
            }
            out = &mut std::mem::take(&mut out)[n..];
        }
    }
}
