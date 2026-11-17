//! Codificador por hardware (NVENC / AMF / QuickSync) vía Media Foundation.
//! Recibe texturas NV12 de la GPU directo (sin copias a la CPU) y entrega paquetes H.264.

use std::mem::ManuallyDrop;
use std::sync::Arc;

use anyhow::{bail, Result};
use windows::core::{Interface, GUID, VARIANT};
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;

use crate::log::log;

#[derive(Clone)]
pub struct Packet {
    pub data: Arc<[u8]>,
    pub pts: i64,
    pub key: bool,
}

/// Cada cuánto se pide un fotograma clave (100 ns): permite recortar el buffer al segundo exacto.
const KEY_EVERY: i64 = 10_000_000;

/// Si pasa esto sin fotogramas clave, el buffer no se puede recortar ni el clip empezar bien (100 ns).
const KEY_LATE: i64 = 5 * KEY_EVERY;

pub struct Encoder {
    mft: IMFTransform,
    api: Option<ICodecAPI>,
    /// Antes de esto no se vuelve a pedir un fotograma clave a mano.
    next_key: i64,
    last_key: i64,
    /// Ya se avisó en el log que el encoder no genera fotogramas clave.
    warned_keys: bool,
    events: IMFMediaEventGenerator,
    _mgr: IMFDXGIDeviceManager,
    pub pending_input: u32,
    /// SPS y PPS del último fotograma clave: los que de verdad usa el video.
    seq_header: Option<Vec<u8>>,
    provides_samples: bool,
    out_size: u32,
    pub name: String,
}

impl Encoder {
    pub fn new(device: &ID3D11Device, luid: LUID, w: u32, h: u32, fps: u32, bitrate: u32) -> Result<Self> {
        unsafe {
            let (mft, name) = find_encoder(luid)?;

            let attrs = mft.GetAttributes()?;
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                bail!("el encoder {name} no es asíncrono");
            }
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            let mut token = 0u32;
            let mut mgr = None;
            MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
            let mgr = mgr.unwrap();
            mgr.ResetDevice(device, token)?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize)?;

            let api = mft.cast::<ICodecAPI>().ok();
            if let Some(api) = &api {
                let set = |g: &GUID, v: VARIANT| {
                    let _ = api.SetValue(g, &v);
                };
                set(&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_UnconstrainedVBR.0 as u32));
                set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate));
                // Un fotograma clave por segundo (si no aparecen, `feed` los pide a mano: hay encoders que ignoran esto).
                set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps));
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
                set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true));
            }

            let ot = MFCreateMediaType()?;
            ot.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            ot.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            ot.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            set_video_common(&ot, w, h, fps)?;
            ot.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            mft.SetOutputType(0, &ot, 0)?;

            let it = MFCreateMediaType()?;
            it.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            it.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            set_video_common(&it, w, h, fps)?;
            mft.SetInputType(0, &it, 0)?;

            let info = mft.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                != 0;

            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events: IMFMediaEventGenerator = mft.cast()?;

            Ok(Self {
                mft,
                api,
                next_key: 0,
                last_key: 0,
                warned_keys: false,
                events,
                _mgr: mgr,
                pending_input: 0,
                seq_header: None,
                provides_samples: provides,
                out_size: info.cbSize.max(4 * 1024 * 1024),
                name,
            })
        }
    }

    /// Procesa los eventos pendientes del encoder sin bloquear.
    pub fn poll(&mut self, out: &mut Vec<Packet>) -> Result<()> {
        loop {
            let ev = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(e) => return Err(e.into()),
            };
            let t = unsafe { ev.GetType()? };
            if t == METransformNeedInput.0 as u32 {
                self.pending_input += 1;
            } else if t == METransformHaveOutput.0 as u32 {
                self.pull(out)?;
            }
        }
        Ok(())
    }

    fn pull(&mut self, out: &mut Vec<Packet>) -> Result<()> {
        unsafe {
            let own = if self.provides_samples {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                Some(s)
            };
            let mut buf = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = self.mft.ProcessOutput(0, &mut buf, &mut status);
            let sample = ManuallyDrop::take(&mut buf[0].pSample);
            drop(ManuallyDrop::take(&mut buf[0].pEvents));
            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                    return Ok(());
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) => return Err(e.into()),
            }
            let Some(sample) = sample else { return Ok(()) };

            let pts = sample.GetSampleTime().unwrap_or(0);
            let clean = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let mb = sample.ConvertToContiguousBuffer()?;
            let mut p = std::ptr::null_mut();
            let mut len = 0u32;
            mb.Lock(&mut p, None, Some(&mut len))?;
            let data: Arc<[u8]> = std::slice::from_raw_parts(p, len as usize).into();
            mb.Unlock()?;

            // Un clip tiene que empezar en una imagen IDR: con otra imagen "limpia" (I sin IDR, que algunos
            // encoders marcan igual) los reproductores muestran negro o verde hasta el próximo IDR.
            // Si la salida no es Annex B no se puede mirar adentro y vale la marca del encoder.
            let key = if is_annex_b(&data) { is_keyframe(&data) } else { clean };
            if key {
                self.last_key = pts;
            } else if pts - self.last_key > KEY_LATE && !self.warned_keys {
                self.warned_keys = true;
                log(&format!("el encoder {} no genera fotogramas clave: los clips pueden salir negros o verdes", self.name));
            }
            if key {
                let ps = Some(param_sets(&data)).filter(|ps| !ps.is_empty());
                if self.seq_header.is_none() {
                    let published = self.published_header();
                    let hex = |h: &Option<Vec<u8>>| h.as_ref().map_or("no".into(), |h| h.iter().map(|b| format!("{b:02x}")).collect::<String>());
                    let same = if ps.is_some() && ps == published { " (iguales)" } else { "" };
                    log(&format!("encabezado H.264{same}: en el video {}, publicado por el encoder {}", hex(&ps), hex(&published)));
                }
                if ps.is_some() {
                    self.seq_header = ps;
                }
            }
            out.push(Packet { data, pts, key });
        }
        Ok(())
    }

    pub fn feed(&mut self, tex: &ID3D11Texture2D, pts: i64, dur: i64) -> Result<()> {
        unsafe {
            // Solo si el encoder no generó uno solo (con margen: los paquetes salen unos cuadros después).
            if pts >= self.next_key && pts - self.last_key > KEY_EVERY * 3 / 2 {
                self.next_key = pts + KEY_EVERY;
                if let Some(api) = &self.api {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                }
            }
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
                if let Ok(len) = b2.GetContiguousLength() {
                    let _ = buf.SetCurrentLength(len);
                }
            }
            let s = MFCreateSample()?;
            s.AddBuffer(&buf)?;
            s.SetSampleTime(pts)?;
            s.SetSampleDuration(dur)?;
            self.mft.ProcessInput(0, &s, 0)?;
        }
        self.pending_input -= 1;
        Ok(())
    }

    /// SPS y PPS que el encoder publica en su tipo de salida. El de AMD no coincide con lo que
    /// codifica (dice POC tipo 2 y los cuadros usan tipo 0): con ese encabezado el MP4 sale negro o verde.
    fn published_header(&self) -> Option<Vec<u8>> {
        unsafe {
            let t = self.mft.GetOutputCurrentType(0).ok()?;
            let mut v = vec![0u8; t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()? as usize];
            t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut v, None).ok()?;
            Some(v)
        }
    }

    /// Tipo de salida serializado (para reconstruirlo en el hilo que escribe el MP4), con el encabezado
    /// que viene en el video si lo hay, y si no el que publica el encoder ahora.
    pub fn type_blob(&self) -> Option<Vec<u8>> {
        unsafe {
            let src = self.mft.GetOutputCurrentType(0).ok()?;
            let t = MFCreateMediaType().ok()?;
            src.CopyAllItems(&t).ok()?;
            if let Some(sh) = &self.seq_header {
                t.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, sh).ok()?;
            }
            let size = MFGetAttributesAsBlobSize(&t).ok()?;
            let mut v = vec![0u8; size as usize];
            MFGetAttributesAsBlob(&t, &mut v).ok()?;
            Some(v)
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(sd) = self.mft.cast::<IMFShutdown>() {
                let _ = sd.Shutdown();
            }
        }
    }
}

unsafe fn set_video_common(t: &IMFMediaType, w: u32, h: u32, fps: u32) -> Result<()> {
    t.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
    t.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
    t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    let _ = t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
    let _ = t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
    let _ = t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
    let _ = t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
    Ok(())
}

unsafe fn find_encoder(luid: LUID) -> Result<(IMFTransform, String)> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut attrs = None;
    MFCreateAttributes(&mut attrs, 1)?;
    let attrs = attrs.unwrap();
    // Solo encoders de la misma GPU que captura (importa en notebooks con dos GPUs).
    let luid_bytes = std::slice::from_raw_parts(&luid as *const LUID as *const u8, std::mem::size_of::<LUID>());
    attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, luid_bytes)?;

    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnum2(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        Some(&input),
        Some(&output),
        &attrs,
        &mut ptr,
        &mut count,
    )?;
    let acts: Vec<IMFActivate> = if ptr.is_null() {
        Vec::new()
    } else {
        let s = std::slice::from_raw_parts_mut(ptr, count as usize);
        let v = s.iter_mut().filter_map(|a| a.take()).collect();
        CoTaskMemFree(Some(ptr as *const _));
        v
    };
    for act in acts {
        let name = activate_name(&act);
        match act.ActivateObject::<IMFTransform>() {
            Ok(t) => return Ok((t, name)),
            Err(e) => log(&format!("no se pudo activar {name}: {e}")),
        }
    }
    bail!("tu GPU no tiene encoder por hardware para H.264")
}

unsafe fn activate_name(act: &IMFActivate) -> String {
    let mut p = windows::core::PWSTR::null();
    let mut len = 0u32;
    if act.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_ok() {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    } else {
        "encoder".into()
    }
}

// --- Utilidades Annex-B -------------------------------------------------------

fn nals(d: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut e = if k + 1 < starts.len() { starts[k + 1] - 3 } else { d.len() };
        while e > s && d[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&d[s..e]);
        }
    }
    out
}

fn nal_type(n: &[u8]) -> u8 {
    n[0] & 0x1f
}

/// ¿Empieza con un código de inicio (00 00 01 o 00 00 00 01)?
fn is_annex_b(d: &[u8]) -> bool {
    d.starts_with(&[0, 0, 1]) || d.starts_with(&[0, 0, 0, 1])
}

/// ¿Hay una imagen IDR (NAL 5)?
fn is_keyframe(d: &[u8]) -> bool {
    nals(d).iter().any(|n| nal_type(n) == 5)
}

/// SPS (7) y PPS (8), para el encabezado del MP4.
fn param_sets(d: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    for n in nals(d) {
        if matches!(nal_type(n), 7 | 8) {
            v.extend_from_slice(&[0, 0, 0, 1]);
            v.extend_from_slice(n);
        }
    }
    v
}
