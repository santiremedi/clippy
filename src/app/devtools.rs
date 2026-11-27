//! `--test-hotkeys`: cada atajo global dispara su acción una sola vez, en el escritorio y con un juego adelante
//! que toma el teclado con Raw Input y `RIDEV_NOHOTKEYS` (como Genshin Impact o NBA 2K). El juego es este mismo
//! exe en otro proceso (`--test-hotkeys juego`), con una ventana de 1×1 fuera de la pantalla.
//!
//! Registra F24, Ctrl + Shift + F24 e Inicio, aprieta teclas y se queda con el foco unos 4 s: de a una prueba
//! por vez.

use std::cell::RefCell;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::{RIDEV_NOHOTKEYS, RIDEV_NOLEGACY};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::hotkeys::{listen_keyboard, Hotkeys};
use super::keys::{MOD_CTRL, MOD_SHIFT};
use crate::config::GlobalAction;

pub const TEST_HOTKEYS: &str = "--test-hotkeys";
const GAME: &str = "juego";

const KEYS: [(GlobalAction, u32, u32); 3] = [
    (GlobalAction::SaveClip, VK_F24.0 as u32, 0),
    (GlobalAction::ToggleRecording, VK_F24.0 as u32, MOD_CTRL | MOD_SHIFT),
    (GlobalAction::OpenEditor, VK_HOME.0 as u32, 0),
];
/// Código físico de Inicio, que comparte con el 7 del teclado numérico (Inicio lleva además E0).
const HOME_SCAN: u16 = 0x47;

thread_local! {
    static HOTKEYS: RefCell<Option<Hotkeys>> = const { RefCell::new(None) };
    /// Acciones disparadas, con el camino por el que llegaron.
    static FIRED: RefCell<Vec<(GlobalAction, &'static str)>> = const { RefCell::new(Vec::new()) };
    /// `WM_HOTKEY` recibidos, se hayan usado o no (con el juego adelante tiene que dar 0).
    static HOTKEY_MSGS: RefCell<u32> = const { RefCell::new(0) };
}

pub fn test_hotkeys(args: &[String]) -> i32 {
    if args.first().is_some_and(|a| a == GAME) {
        return fake_game();
    }
    let hwnd = window(w!("ClippyPruebaAtajos"), listener, false);
    HOTKEYS.with(|h| *h.borrow_mut() = Some(Hotkeys::new(hwnd)));
    let failed = with_hotkeys(|h| h.register(KEYS));
    if !failed.is_empty() {
        println!("ERROR atajos ocupados: {failed:?}");
        return 1;
    }
    pump(100);
    take_results();

    // Escritorio: por la misma pulsación llegan WM_HOTKEY y Raw Input, y la acción va una vez. Solo F24, que se
    // queda RegisterHotKey: a la app que esté adelante no le llega nada.
    let mut ok = true;
    press(&[key(VK_F24, true), key(VK_F24, false)]);
    ok &= check("escritorio, F24", &[GlobalAction::SaveClip], true);
    press(&[key(VK_F24, true), key(VK_F24, true), key(VK_F24, true), key(VK_F24, false)]);
    ok &= check("escritorio, F24 mantenida", &[GlobalAction::SaveClip], true);
    // Con la bandeja ocupada, dos pulsaciones seguidas se leen juntas: cuenta la hora de cada tecla.
    send(&[key(VK_F24, true), key(VK_F24, false)]);
    std::thread::sleep(Duration::from_millis(250));
    press(&[key(VK_F24, true), key(VK_F24, false)]);
    ok &= check("escritorio, dos F24 con la bandeja ocupada", &[GlobalAction::SaveClip, GlobalAction::SaveClip], true);

    // Juego que toma el teclado: Windows no manda WM_HOTKEY y la acción llega por Raw Input.
    let exe = std::env::current_exe().unwrap();
    let mut game = Command::new(exe).args([TEST_HOTKEYS, GAME]).stdout(Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    let _ = BufReader::new(game.stdout.take().unwrap()).read_line(&mut line);
    if line.trim() != "adelante" {
        println!("ERROR el juego de prueba no quedó adelante ({})", line.trim());
        let _ = game.wait();
        return 1;
    }
    pump(100);
    take_results();
    press(&[key(VK_F24, true), key(VK_F24, false)]);
    ok &= check("juego, F24", &[GlobalAction::SaveClip], false);
    press(&[key(VK_CONTROL, true), key(VK_SHIFT, true), key(VK_F24, true), key(VK_F24, false), key(VK_SHIFT, false), key(VK_CONTROL, false)]);
    ok &= check("juego, Ctrl + Shift + F24 de golpe", &[GlobalAction::ToggleRecording], false);
    press(&[key(VK_F24, true), key(VK_F24, true), key(VK_F24, true), key(VK_F24, false)]);
    ok &= check("juego, F24 mantenida", &[GlobalAction::SaveClip], false);
    press(&[key_scan(VK_HOME, HOME_SCAN, false, true), key_scan(VK_HOME, HOME_SCAN, false, false)]);
    ok &= check("juego, 7 del teclado numérico", &[], false);
    press(&[key_scan(VK_HOME, HOME_SCAN, true, true), key_scan(VK_HOME, HOME_SCAN, true, false)]);
    ok &= check("juego, Inicio", &[GlobalAction::OpenEditor], false);
    // Se elige F24 de nuevo en Ajustes y se la sigue apretando hasta que se repite: no es otra pulsación.
    with_hotkeys(Hotkeys::unregister);
    press(&[key(VK_F24, true)]);
    with_hotkeys(|h| h.register(KEYS));
    press(&[key(VK_F24, true), key(VK_F24, false)]);
    ok &= check("juego, atajo recién elegido y todavía apretado", &[], false);
    let _ = game.wait();

    with_hotkeys(Hotkeys::unregister);
    i32::from(!ok)
}

fn with_hotkeys<R>(f: impl FnOnce(&mut Hotkeys) -> R) -> R {
    HOTKEYS.with(|h| f(h.borrow_mut().as_mut().unwrap()))
}

extern "system" fn listener(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if m == WM_HOTKEY {
        HOTKEY_MSGS.with(|n| *n.borrow_mut() += 1);
    }
    if let Some(a) = HOTKEYS.with(|k| k.borrow_mut().as_mut()?.on_message(m, wp, lp)) {
        FIRED.with(|v| v.borrow_mut().push((a, if m == WM_HOTKEY { "WM_HOTKEY" } else { "Raw Input" })));
    }
    unsafe { DefWindowProcW(h, m, wp, lp) }
}

/// Una ventana adelante que toma el teclado como un juego. Al terminar, devuelve el foco a la que estaba.
fn fake_game() -> i32 {
    extern "system" fn proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(h, m, wp, lp) }
    }
    let hwnd = window(w!("ClippyPruebaJuego"), proc, true);
    listen_keyboard(hwnd, RIDEV_NOLEGACY | RIDEV_NOHOTKEYS).unwrap();
    unsafe {
        // Windows no deja robar el foco: se toma prestada la cola de entrada de la ventana de adelante.
        let prev = GetForegroundWindow();
        let (me, them) = (GetCurrentThreadId(), GetWindowThreadProcessId(prev, None));
        let _ = AttachThreadInput(me, them, true);
        let _ = SetForegroundWindow(hwnd);
        let _ = AttachThreadInput(me, them, false);
        println!("{}", if GetForegroundWindow() == hwnd { "adelante" } else { "atrás" });
        pump(4000);
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

fn key(vk: VIRTUAL_KEY, down: bool) -> INPUT {
    key_scan(vk, 0, false, down)
}

/// Con código físico; `extended`: con E0, como Inicio (sin E0 es el 7 del teclado numérico).
fn key_scan(vk: VIRTUAL_KEY, scan: u16, extended: bool, down: bool) -> INPUT {
    let mut flags = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, ..Default::default() } } }
}

fn send(inputs: &[INPUT]) {
    unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
}

/// Manda las teclas de golpe y deja que lleguen los mensajes.
fn press(inputs: &[INPUT]) {
    send(inputs);
    pump(300);
}

/// Las acciones disparadas y los `WM_HOTKEY` recibidos desde la última vez; los deja en cero.
fn take_results() -> (Vec<(GlobalAction, &'static str)>, u32) {
    (FIRED.with(|v| std::mem::take(&mut *v.borrow_mut())), HOTKEY_MSGS.with(|n| std::mem::take(&mut *n.borrow_mut())))
}

/// `hotkey_msgs`: si puede llegar `WM_HOTKEY` (en el escritorio sí; con el juego adelante, no).
fn check(what: &str, want: &[GlobalAction], hotkey_msgs: bool) -> bool {
    let (fired, msgs) = take_results();
    let ok = fired.iter().map(|f| f.0).eq(want.iter().copied()) && (hotkey_msgs || msgs == 0);
    println!("{} {what}: {fired:?}, {msgs} WM_HOTKEY", if ok { "OK   " } else { "ERROR" });
    ok
}
