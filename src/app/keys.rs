//! Nombres de teclas y lectura de la tecla presionada (para elegir el atajo).

use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, GetKeyNameTextW, MapVirtualKeyW, MAPVK_VK_TO_VSC};

use crate::i18n::{tr, trf};

pub const MOD_ALT: u32 = 1;
pub const MOD_CTRL: u32 = 2;
pub const MOD_SHIFT: u32 = 4;
const MOD_WIN: u32 = 8;

pub fn hotkey_name(vk: u32, mods: u32) -> String {
    let mut s = String::new();
    if mods & MOD_CTRL != 0 {
        s += "Ctrl + ";
    }
    if mods & MOD_SHIFT != 0 {
        s += "Shift + ";
    }
    if mods & MOD_ALT != 0 {
        s += "Alt + ";
    }
    s + &vk_name(vk)
}

pub fn vk_name(vk: u32) -> String {
    let fixed = match vk {
        0x08 => tr!("Retroceso", "Backspace", "Backspace"),
        0x09 => "Tab",
        0x0D => "Enter",
        0x13 => tr!("Pausa", "Pause", "Pause"),
        0x20 => tr!("Espacio", "Space", "Espaço"),
        0x21 => tr!("RePág", "Page Up", "Page Up"),
        0x22 => tr!("AvPág", "Page Down", "Page Down"),
        0x23 => tr!("Fin", "End", "End"),
        0x24 => tr!("Inicio", "Home", "Home"),
        0x25 => tr!("Flecha izq.", "Left arrow", "Seta esq."),
        0x26 => tr!("Flecha arriba", "Up arrow", "Seta para cima"),
        0x27 => tr!("Flecha der.", "Right arrow", "Seta dir."),
        0x28 => tr!("Flecha abajo", "Down arrow", "Seta para baixo"),
        0x2C => tr!("Impr Pant", "Print Screen", "Print Screen"),
        0x2D => "Insert",
        0x2E => tr!("Supr", "Delete", "Delete"),
        0x6A => "Num *",
        0x6B => "Num +",
        0x6D => "Num -",
        0x6E => "Num .",
        0x6F => "Num /",
        0x91 => tr!("Bloq Despl", "Scroll Lock", "Scroll Lock"),
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.into();
    }
    match vk {
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x30..=0x39 | 0x41..=0x5A => (vk as u8 as char).to_string(),
        0x60..=0x69 => format!("Num {}", vk - 0x60),
        _ => unsafe {
            let scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
            let mut buf = [0u16; 64];
            let n = GetKeyNameTextW((scan << 16) as i32, &mut buf);
            if n > 0 {
                String::from_utf16_lossy(&buf[..n as usize])
            } else {
                trf!("Tecla {vk:#04X}", "Key {vk:#04X}", "Tecla {vk:#04X}")
            }
        },
    }
}

/// Teclas modificadoras y su `MOD_*`: las genéricas, las de cada lado y las dos de Windows.
const MODIFIERS: [(u32, u32); 11] = [
    (0x10, MOD_SHIFT),
    (0xA0, MOD_SHIFT),
    (0xA1, MOD_SHIFT),
    (0x11, MOD_CTRL),
    (0xA2, MOD_CTRL),
    (0xA3, MOD_CTRL),
    (0x12, MOD_ALT),
    (0xA4, MOD_ALT),
    (0xA5, MOD_ALT),
    (0x5B, MOD_WIN),
    (0x5C, MOD_WIN),
];

/// El `MOD_*` de una tecla modificadora; 0 si no es una.
pub fn modifier(vk: u32) -> u32 {
    MODIFIERS.iter().find(|m| m.0 == vk).map_or(0, |m| m.1)
}

/// ¿Está apretada ahora?
pub fn down(vk: u32) -> bool {
    unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

/// Devuelve la primera tecla (no modificadora) que esté apretada, con sus modificadores.
pub fn pressed_key() -> Option<(u32, u32)> {
    // Bloq Mayús, menú y Bloq Num se ignoran (los botones del mouse van antes de 0x08).
    let vk = (0x08..=0xFE).find(|&vk| modifier(vk) == 0 && !matches!(vk, 0x14 | 0x5D | 0x90) && down(vk))?;
    // Los atajos no usan la tecla de Windows.
    let mods = MODIFIERS.iter().filter(|m| m.1 != MOD_WIN && down(m.0)).fold(0, |acc, m| acc | m.1);
    Some((vk, mods))
}
