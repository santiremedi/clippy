//! Atajos globales de la bandeja, por dos caminos para la misma tecla:
//!
//! - `RegisterHotKey`: reserva la tecla (no le llega a la app de adelante) y avisa si otro programa ya la usa.
//! - Raw Input con `RIDEV_INPUTSINK`: respaldo para los juegos que toman el teclado con `RIDEV_NOHOTKEYS`, con
//!   los que Windows deja de mandar `WM_HOTKEY` sin dar ningún error. Llega un mensaje por tecla apretada, sin
//!   sondeo: en reposo no despierta a nadie.
//!
//! Si llegan los dos por la misma pulsación, la acción se dispara una sola vez.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTDEVICE_FLAGS, RAWINPUTHEADER, RIDEV_INPUTSINK,
    RID_INPUT, RIM_TYPEKEYBOARD,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageTime, RI_KEY_BREAK, RI_KEY_E0, RI_KEY_E1, WM_HOTKEY, WM_INPUT};

use super::keys::{down, hotkey_name, modifier};
use crate::config::GlobalAction;
use crate::log::log;

/// Los dos avisos de una misma pulsación salen juntos de Windows, uno detrás del otro y con la misma hora
/// (±1 tic de 16 ms); nadie aprieta dos veces el mismo atajo en menos que esto.
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
    /// Teclas apretadas, en el orden en que llegaron; todas, porque la que se acaba de elegir como atajo puede
    /// seguir apretada. Los modificadores salen de acá y no de `GetAsyncKeyState`: un Stream Deck o un macro
    /// del mouse mandan el atajo entero de golpe y, para cuando se lee el mensaje, Ctrl ya se soltó.
    held: Vec<Held>,
    /// Última acción disparada y la hora de la tecla que la disparó (`GetMessageTime`).
    last: Option<(GlobalAction, i32)>,
}

/// Recibe el teclado por Raw Input en `hwnd`, con estas opciones.
pub fn listen_keyboard(hwnd: HWND, flags: RAWINPUTDEVICE_FLAGS) -> windows::core::Result<()> {
    let keyboard = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x06, dwFlags: flags, hwndTarget: hwnd };
    unsafe { RegisterRawInputDevices(&[keyboard], size_of::<RAWINPUTDEVICE>() as u32) }
}

impl Hotkeys {
    /// Empieza a escuchar el teclado por Raw Input en `hwnd`, aunque no tenga el foco.
    pub fn new(hwnd: HWND) -> Self {
        if let Err(e) = listen_keyboard(hwnd, RIDEV_INPUTSINK) {
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

    /// `WM_HOTKEY` o `WM_INPUT`: la acción del atajo, si es el primer aviso de esa pulsación. Se compara la hora
    /// en que Windows puso cada mensaje en la cola y no la de ahora: si la bandeja estuvo ocupada (pausar
    /// detiene la grabadora), dos pulsaciones seguidas se leen juntas pero siguen siendo dos.
    pub fn on_message(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<GlobalAction> {
        let t = unsafe { GetMessageTime() };
        let a = match msg {
            WM_HOTKEY => GlobalAction::from_id(wp.0 as i32)?,
            WM_INPUT => self.pressed(lp, t)?,
            _ => return None,
        };
        if self.last.is_some_and(|(prev, pt)| prev == a && within(t, pt, SAME_PRESS_MS)) {
            return None;
        }
        if msg == WM_INPUT {
            log(&format!("atajo {a:?} por Raw Input"));
        }
        self.last = Some((a, t));
        Some(a)
    }

    /// El atajo que se acaba de apretar según Raw Input (las repeticiones al mantenerlo no cuentan).
    fn pressed(&mut self, lp: LPARAM, t: i32) -> Option<GlobalAction> {
        let mut raw = RAWINPUT::default();
        let mut size = size_of::<RAWINPUT>() as u32;
        let header = size_of::<RAWINPUTHEADER>() as u32;
        let read = unsafe { GetRawInputData(HRAWINPUT(lp.0 as _), RID_INPUT, Some(&mut raw as *mut _ as _), &mut size, header) };
        if read == u32::MAX || raw.header.dwType != RIM_TYPEKEYBOARD.0 {
            return None;
        }
        let key = unsafe { raw.data.keyboard };
        let flags = key.Flags as u32;
        let vk = numpad(key.VKey as u32, key.MakeCode, flags);
        let scan = key.MakeCode as u32 | (flags & (RI_KEY_E0 | RI_KEY_E1)) << 16;
        if flags & RI_KEY_BREAK != 0 {
            self.held.retain(|h| (h.vk, h.scan) != (vk, scan));
            return None;
        }
        match self.held.iter_mut().find(|h| (h.vk, h.scan) == (vk, scan)) {
            Some(h) => {
                let repeat = within(t, h.time, REPEAT_MS);
                h.time = t;
                if repeat {
                    return None;
                }
            }
            None => self.held.push(Held { vk, scan, time: t }),
        }
        // Mismos modificadores exactos que pide RegisterHotKey (Ctrl + F9 no es F9).
        self.active.iter().find(|&&(_, k, m)| k == vk && m == self.mods(t)).map(|&(a, ..)| a)
    }

    /// Modificadores apretados cuando se apretó la tecla de la hora `t`. Si Windows no avisó que se soltó uno
    /// (Ctrl + Alt + Supr pasa a otra pantalla), deja de contar cuando hace rato que no avisa y no está apretado.
    fn mods(&self, t: i32) -> u32 {
        self.held.iter().filter(|h| within(t, h.time, REPEAT_MS) || down(h.vk)).fold(0, |m, h| m | modifier(h.vk))
    }
}

/// ¿Pasó menos de `ms` entre dos horas de `GetMessageTime` (que vuelve a cero cada 49 días)?
fn within(a: i32, b: i32, ms: u32) -> bool {
    a.wrapping_sub(b).unsigned_abs() < ms
}

/// Raw Input da las teclas del teclado numérico como con Bloq Num apagado (Num 7 llega como Inicio, sin E0),
/// mientras que `RegisterHotKey` las ve como números: se pasan a número para que un atajo en Inicio no salte
/// con el 7 del teclado numérico.
fn numpad(vk: u32, make: u16, flags: u32) -> u32 {
    if flags & RI_KEY_E0 != 0 {
        return vk;
    }
    let num = match (make, VIRTUAL_KEY(vk as u16)) {
        (0x52, VK_INSERT) => VK_NUMPAD0,
        (0x4F, VK_END) => VK_NUMPAD1,
        (0x50, VK_DOWN) => VK_NUMPAD2,
        (0x51, VK_NEXT) => VK_NUMPAD3,
        (0x4B, VK_LEFT) => VK_NUMPAD4,
        (0x4C, VK_CLEAR) => VK_NUMPAD5,
        (0x4D, VK_RIGHT) => VK_NUMPAD6,
        (0x47, VK_HOME) => VK_NUMPAD7,
        (0x48, VK_UP) => VK_NUMPAD8,
        (0x49, VK_PRIOR) => VK_NUMPAD9,
        (0x53, VK_DELETE) => VK_DECIMAL,
        _ => return vk,
    };
    num.0 as u32
}
