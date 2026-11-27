//! `--test-hotkeys`: cada atajo global dispara su acción una sola vez, en el escritorio y con un juego adelante
//! que toma el teclado con Raw Input y `RIDEV_NOHOTKEYS` (como Genshin Impact o NBA 2K). El juego es este mismo
//! exe en otro proceso (`--test-hotkeys juego`), con una ventana de 1×1 fuera de la pantalla.
//!
//! Aprieta F24 y Ctrl + Shift + F24, que no usa nadie, y se queda con el foco unos 2 s: de a una prueba por vez.

use std::cell::RefCell;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VIRTUAL_KEY};
use windows::Win32::UI::Input::{RegisterRawInputDevices, RAWINPUTDEVICE, RIDEV_NOHOTKEYS, RIDEV_NOLEGACY};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::hotkeys::Hotkeys;
use super::keys::{MOD_CTRL, MOD_SHIFT};
use crate::config::GlobalAction;

const F24: u16 = 0x87;
const CTRL: u16 = 0x11;
const SHIFT: u16 = 0x10;

thread_local! {
    static KEYS: RefCell<Option<Hotkeys>> = const { RefCell::new(None) };
    /// Acciones disparadas, con el camino por el que llegaron.
    static FIRED: RefCell<Vec<(GlobalAction, &'static str)>> = const { RefCell::new(Vec::new()) };
    /// `WM_HOTKEY` recibidos, se hayan usado o no (con el juego adelante tiene que dar 0).
    static HOTKEY_MSGS: RefCell<u32> = const { RefCell::new(0) };
}

pub fn test_hotkeys(args: &[String]) -> i32 {
    if args.first().is_some_and(|a| a == "juego") {
        return fake_game();
    }
    let hwnd = window(w!("ClippyPruebaAtajos"), listener, false);
    let mut hotkeys = Hotkeys::new(hwnd);
    let failed = hotkeys.register([(GlobalAction::SaveClip, F24 as u32, 0), (GlobalAction::ToggleRecording, F24 as u32, MOD_CTRL | MOD_SHIFT)]);
    if !failed.is_empty() {
        println!("ERROR atajos ocupados: {failed:?}");
        return 1;
    }
    KEYS.with(|k| *k.borrow_mut() = Some(hotkeys));
    pump(100);
    take();

    // Escritorio: por la misma pulsación llegan WM_HOTKEY y Raw Input, y la acción va una vez. Solo F24, que se
    // queda RegisterHotKey: a la app que esté adelante no le llega nada.
    let mut ok = true;
    press(&[(F24, true), (F24, false)]);
    ok &= check("escritorio, F24", &[GlobalAction::SaveClip], true);
    press(&[(F24, true), (F24, true), (F24, true), (F24, false)]);
    ok &= check("escritorio, F24 mantenida", &[GlobalAction::SaveClip], true);

    // Juego que toma el teclado: Windows no manda WM_HOTKEY y la acción llega por Raw Input.
    let exe = std::env::current_exe().unwrap();
    let mut game = Command::new(exe).args(["--test-hotkeys", "juego"]).stdout(Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    let _ = BufReader::new(game.stdout.take().unwrap()).read_line(&mut line);
    if line.trim() != "adelante" {
        println!("ERROR el juego de prueba no quedó adelante ({})", line.trim());
        let _ = game.wait();
        return 1;
    }
    pump(100);
    take();
    press(&[(F24, true), (F24, false)]);
    ok &= check("juego, F24", &[GlobalAction::SaveClip], false);
    press(&[(CTRL, true), (SHIFT, true), (F24, true), (F24, false), (SHIFT, false), (CTRL, false)]);
    ok &= check("juego, Ctrl + Shift + F24", &[GlobalAction::ToggleRecording], false);
    press(&[(F24, true), (F24, true), (F24, true), (F24, false)]);
    ok &= check("juego, F24 mantenida", &[GlobalAction::SaveClip], false);
    let _ = game.wait();

    KEYS.with(|k| k.borrow_mut().as_mut().map(Hotkeys::unregister));
    i32::from(!ok)
}

extern "system" fn listener(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let fired = KEYS.with(|k| {
        let mut k = k.borrow_mut();
        let k = k.as_mut()?;
        match m {
            WM_HOTKEY => {
                HOTKEY_MSGS.with(|n| *n.borrow_mut() += 1);
                k.on_hotkey(wp).map(|a| (a, "WM_HOTKEY"))
            }
            WM_INPUT => k.on_input(lp).map(|a| (a, "Raw Input")),
            _ => None,
        }
    });
    if let Some(f) = fired {
        FIRED.with(|v| v.borrow_mut().push(f));
    }
    unsafe { DefWindowProcW(h, m, wp, lp) }
}

/// Una ventana adelante que toma el teclado como un juego. Al terminar, devuelve el foco a la que estaba.
fn fake_game() -> i32 {
    extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(h, m, wp, lp) }
    }
    let hwnd = window(w!("ClippyPruebaJuego"), proc, true);
    let keyboard = RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x06, dwFlags: RIDEV_NOLEGACY | RIDEV_NOHOTKEYS, hwndTarget: hwnd };
    unsafe {
        RegisterRawInputDevices(&[keyboard], size_of::<RAWINPUTDEVICE>() as u32).unwrap();
        // Windows no deja robar el foco: se toma prestada la cola de entrada de la ventana de adelante.
        let prev = GetForegroundWindow();
        let (me, them) = (GetCurrentThreadId(), GetWindowThreadProcessId(prev, None));
        let _ = AttachThreadInput(me, them, true);
        let _ = SetForegroundWindow(hwnd);
        let _ = AttachThreadInput(me, them, false);
        println!("{}", if GetForegroundWindow() == hwnd { "adelante" } else { "atrás" });
        pump(1800);
        let _ = SetForegroundWindow(prev);
    }
    0
}

/// `visible`: una ventana de 1×1 fuera de la pantalla (para poder estar adelante); si no, oculta.
fn window(class: PCWSTR, proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT, visible: bool) -> HWND {
    unsafe {
        let inst = GetModuleHandleW(None).unwrap();
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(proc), hInstance: inst.into(), lpszClassName: class, ..Default::default() });
        let style = if visible { WS_POPUP | WS_VISIBLE } else { WS_OVERLAPPED };
        CreateWindowExW(WS_EX_TOOLWINDOW, class, class, style, -10, -10, 1, 1, None, None, inst, None).unwrap()
    }
}

fn pump(ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        let mut msg = MSG::default();
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Aprieta (`true`) o suelta cada tecla en orden y deja que lleguen los mensajes.
fn press(seq: &[(u16, bool)]) {
    let inputs: Vec<INPUT> = seq
        .iter()
        .map(|&(vk, down)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), dwFlags: if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP }, ..Default::default() },
            },
        })
        .collect();
    unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    pump(300);
}

/// Lo que pasó desde la última vez; lo deja en cero.
fn take() -> (Vec<(GlobalAction, &'static str)>, u32) {
    (FIRED.with(|v| std::mem::take(&mut *v.borrow_mut())), HOTKEY_MSGS.with(|n| std::mem::take(&mut *n.borrow_mut())))
}

/// `hotkey_msgs`: si puede llegar `WM_HOTKEY` (en el escritorio sí; con el juego adelante, no).
fn check(what: &str, want: &[GlobalAction], hotkey_msgs: bool) -> bool {
    let (fired, msgs) = take();
    let ok = fired.iter().map(|f| f.0).eq(want.iter().copied()) && (hotkey_msgs || msgs == 0);
    println!("{} {what}: {fired:?}, {msgs} WM_HOTKEY", if ok { "OK   " } else { "ERROR" });
    ok
}
