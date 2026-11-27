//! Atajos globales de la bandeja, por dos caminos para la misma tecla:
//!
//! - `RegisterHotKey`: reserva la tecla (no le llega a la app de adelante) y avisa si otro programa ya la usa.
//! - Raw Input con `RIDEV_INPUTSINK`: respaldo para los juegos que toman el teclado con `RIDEV_NOHOTKEYS`
//!   (o DirectInput exclusivo), con los que Windows deja de mandar `WM_HOTKEY` sin dar ningún error.
//!   Llega un mensaje por tecla apretada, sin sondeo: en reposo no despierta a nadie.
//!
//! Si llegan los dos por la misma pulsación, la acción se dispara una sola vez.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEKEYBOARD,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageTime, RI_KEY_BREAK, RI_KEY_E0, RI_KEY_E1};

use super::keys::{down, hotkey_name, modifier};
use crate::config::GlobalAction;
use crate::log::log;

/// Los dos avisos de una misma pulsación salen juntos de Windows (traen la misma hora, ±1 tic de 16 ms);
/// nadie aprieta dos veces el mismo atajo en menos que esto.
const SAME_PRESS_MS: u32 = 150;
/// Una tecla que se mantiene apretada se repite antes de esto (la demora más larga de Windows es 1 s).
const REPEAT_MS: u32 = 1500;

/// Una tecla apretada según Raw Input.
struct Held {
    vk: u32,
    /// Código físico: separa la de la izquierda de la de la derecha.
    scan: u32,
    /// Hora del último aviso (al mantenerla, Windows la repite).
    time: i32,
}

pub struct Hotkeys {
    hwnd: HWND,
    /// Los atajos que se pudieron registrar: acción, tecla y modificadores.
    active: Vec<(GlobalAction, u32, u32)>,
    /// Modificadores y teclas de atajo apretados, en el orden en que llegaron. Los modificadores no se leen
    /// con `GetAsyncKeyState`: un Stream Deck o un macro del mouse mandan el atajo entero de golpe y, para
    /// cuando se lee el mensaje, Ctrl ya se soltó.
    held: Vec<Held>,
    /// Última acción disparada y la hora de la tecla que la disparó (`GetMessageTime`).
    last: Option<(GlobalAction, i32)>,
}

impl Hotkeys {
    /// Empieza a escuchar el teclado por Raw Input en `hwnd`, aunque no tenga el foco.
    pub fn new(hwnd: HWND) -> Self {
        let keyboard = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x06, dwFlags: RIDEV_INPUTSINK, hwndTarget: hwnd };
        if let Err(e) = unsafe { RegisterRawInputDevices(&[keyboard], size_of::<RAWINPUTDEVICE>() as u32) } {
            log(&format!("Raw Input no disponible, los atajos dependen solo de RegisterHotKey: {e}"));
        }
        Self { hwnd, active: Vec::new(), held: Vec::new(), last: None }
    }

    /// Registra estos atajos en lugar de los anteriores. Devuelve los nombres de los que usa otro programa.
    pub fn register(&mut self, keys: impl IntoIterator<Item = (GlobalAction, u32, u32)>) -> Vec<String> {
        self.unregister();
        let mut failed = Vec::new();
        for (a, vk, mods) in keys {
            if unsafe { RegisterHotKey(self.hwnd, a.id(), HOT_KEY_MODIFIERS(mods | MOD_NOREPEAT.0), vk) }.is_ok() {
                self.active.push((a, vk, mods));
            } else {
                failed.push(hotkey_name(vk, mods));
            }
        }
        failed
    }

    /// Suelta todos los atajos (mientras se elige uno nuevo, o al salir).
    pub fn unregister(&mut self) {
        for (a, ..) in self.active.drain(..) {
            unsafe {
                let _ = UnregisterHotKey(self.hwnd, a.id());
            }
        }
    }

    /// `WM_HOTKEY`: la acción de ese atajo, si no se disparó ya por Raw Input.
    pub fn on_hotkey(&mut self, wp: WPARAM) -> Option<GlobalAction> {
        let t = unsafe { GetMessageTime() };
        GlobalAction::from_id(wp.0 as i32).filter(|&a| self.first_signal(a, t))
    }

    /// `WM_INPUT`: la acción del atajo que se acaba de apretar, si no llegó ya por `WM_HOTKEY`.
    pub fn on_input(&mut self, lp: LPARAM) -> Option<GlobalAction> {
        let mut raw = RAWINPUT::default();
        let mut size = size_of::<RAWINPUT>() as u32;
        let header = size_of::<RAWINPUTHEADER>() as u32;
        let read = unsafe { GetRawInputData(HRAWINPUT(lp.0 as _), RID_INPUT, Some(&mut raw as *mut _ as _), &mut size, header) };
        if read == u32::MAX || raw.header.dwType != RIM_TYPEKEYBOARD.0 {
            return None;
        }
        let key = unsafe { raw.data.keyboard };
        let (vk, flags) = (key.VKey as u32, key.Flags as u32);
        let scan = key.MakeCode as u32 | (flags & (RI_KEY_E0 | RI_KEY_E1)) << 16;
        if flags & RI_KEY_BREAK != 0 {
            self.held.retain(|h| (h.vk, h.scan) != (vk, scan));
            return None;
        }
        if modifier(vk) == 0 && !self.active.iter().any(|&(_, k, _)| k == vk) {
            return None;
        }
        let t = unsafe { GetMessageTime() };
        match self.held.iter_mut().find(|h| (h.vk, h.scan) == (vk, scan)) {
            Some(h) => {
                let repeat = t.wrapping_sub(h.time).unsigned_abs() < REPEAT_MS;
                h.time = t;
                if repeat {
                    return None;
                }
            }
            None => self.held.push(Held { vk, scan, time: t }),
        }
        // Mismos modificadores exactos que pide RegisterHotKey (Ctrl + F9 no es F9).
        let mods = self.mods(t);
        let a = self.active.iter().find(|&&(_, k, m)| k == vk && m == mods)?.0;
        self.first_signal(a, t).then_some(a)
    }

    /// Modificadores apretados cuando se apretó la tecla de la hora `t`. Si Windows no avisó que se soltó uno
    /// (Ctrl + Alt + Supr pasa a otra pantalla), deja de contar cuando hace rato que no avisa y no está apretado.
    fn mods(&self, t: i32) -> u32 {
        let fresh = |h: &&Held| t.wrapping_sub(h.time).unsigned_abs() < REPEAT_MS || down(h.vk);
        self.held.iter().filter(fresh).fold(0, |m, h| m | modifier(h.vk))
    }

    /// ¿Es el primer aviso de esta pulsación? Se compara la hora en que Windows puso cada mensaje en la cola
    /// y no la de ahora: si la acción anterior tardó (pausar detiene la grabadora), el segundo aviso se
    /// procesa tarde pero sigue trayendo la hora de la tecla.
    fn first_signal(&mut self, a: GlobalAction, t: i32) -> bool {
        if self.last.is_some_and(|(prev, pt)| prev == a && t.wrapping_sub(pt).unsigned_abs() < SAME_PRESS_MS) {
            return false;
        }
        self.last = Some((a, t));
        true
    }
}
