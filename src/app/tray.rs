//! Proceso principal: ícono en la bandeja, tecla rápida y control de la grabadora.
//! No carga nada de interfaz gráfica: los ajustes se abren en otro proceso (`--settings`).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::autostart;
use super::keys::hotkey_name;
use super::mode::{Mode, Window, TRAY_CLASS, TRAY_MUTEX, WM_HOTKEY_SUSPEND, WM_REC_EVENT, WM_RELOAD, WM_TRAY, WM_UPDATE};
use super::update;
use crate::config::{self, Config, GlobalAction};
use crate::log::log;
use crate::record::recorder::{Event, Notify, Recorder};
use crate::ui_kit::{icon, sound, theme::fmt_secs, window::open_folder};


const ID_TOGGLE: usize = 10;
const ID_SAVE: usize = 11;
const ID_SETTINGS: usize = 12;
const ID_FOLDER: usize = 13;
const ID_EXIT: usize = 14;
const ID_EDITOR: usize = 15;
const ID_MIC: usize = 16;
const NIN_BALLOONUSERCLICK: u32 = WM_USER + 5;

static EVENTS: Mutex<VecDeque<Event>> = Mutex::new(VecDeque::new());

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static TASKBAR_CREATED: RefCell<u32> = const { RefCell::new(0) };
}

struct App {
    hwnd: HWND,
    cfg: Config,
    rec: Option<Recorder>,
    icon_on: HICON,
    icon_off: HICON,
    error: Option<String>,
    /// Juego detectado por la grabadora (para el tooltip).
    game: Option<String>,
}

/// `background`: arrancar sin abrir la ventana (inicio automático con Windows).
pub fn run(background: bool) {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // Una sola instancia: si ya está corriendo, abrir sus ajustes.
        let _mutex = CreateMutexW(None, true, TRAY_MUTEX);
        if GetLastError() == ERROR_ALREADY_EXISTS {
            open_settings();
            return;
        }

        update::cleanup();
        autostart::repair();
        let first_run = !config::exists();
        let cfg = config::load();
        if first_run {
            let _ = config::save(&cfg);
        }

        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst, lpszClassName: TRAY_CLASS, ..Default::default() };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            TRAY_CLASS,
            w!("ClippyHost"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            hinst,
            None,
        ) else {
            log("no se pudo crear la ventana principal");
            return;
        };
        TASKBAR_CREATED.with(|t| *t.borrow_mut() = RegisterWindowMessageW(w!("TaskbarCreated")));

        let size = GetSystemMetrics(SM_CXSMICON).max(16) as u32;
        let app = App {
            hwnd,
            cfg: Config { enabled: false, ..cfg.clone() },
            rec: None,
            icon_on: icon::hicon(size, true),
            icon_off: icon::hicon(size, false),
            error: None,
            game: None,
        };
        app.tray(NIM_ADD);
        APP.with(|a| *a.borrow_mut() = Some(app));
        with_app(|a| a.apply(cfg.clone(), true));
        log("Clippy iniciado");
        if first_run {
            let key = hotkey_name(cfg.hotkey_vk, cfg.hotkey_mods);
            with_app(|a| {
                a.balloon(
                    "Clippy está grabando",
                    &format!("Apretá {key} para guardar los últimos {}. Clippy queda acá, en la bandeja.", fmt_secs(cfg.seconds)),
                    false,
                )
            });
        }
        if first_run || !background {
            open_settings();
        }
        // Versiones nuevas: una consulta como mucho por día, en segundo plano.
        {
            let h = hwnd.0 as isize;
            update::watch(move || {
                let _ = PostMessageW(HWND(h as *mut _), WM_UPDATE, WPARAM(0), LPARAM(0));
            });
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut b| b.as_mut().map(f)))
}

fn notifier(hwnd: HWND) -> Notify {
    let h = hwnd.0 as isize;
    Arc::new(move |ev| {
        if let Ok(mut q) = EVENTS.lock() {
            q.push_back(ev);
        }
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_REC_EVENT, WPARAM(0), LPARAM(0));
        }
    })
}

impl App {
    fn apply(&mut self, new: Config, startup: bool) {
        let restart = new.video_params() != self.cfg.video_params();
        let keys = |c: &Config| (GlobalAction::ALL.map(|a| c.hotkey(a)), c.enabled);
        let hotkey_changed = keys(&new) != keys(&self.cfg);
        self.cfg = new;
        if restart || (self.cfg.enabled && self.rec.is_none()) {
            self.rec = None; // detiene la grabadora anterior
            self.error = None;
            self.game = None;
            if self.cfg.enabled {
                self.rec = Some(Recorder::start(self.cfg.clone(), notifier(self.hwnd)));
            }
        } else if let Some(r) = &self.rec {
            // Duración, audio, carpeta: se aplican sin perder lo ya grabado.
            r.update(self.cfg.clone());
        }
        if hotkey_changed || startup {
            let failed = self.register_hotkeys();
            if !failed.is_empty() {
                let k = failed.join(", ");
                self.balloon("Tecla ocupada", &format!("Otro programa está usando {k}. Elegí otra en Ajustes."), true);
            }
        }
        self.tray(NIM_MODIFY);
    }

    fn unregister_all(&self) {
        unsafe {
            for a in GlobalAction::ALL {
                let _ = UnregisterHotKey(self.hwnd, a.id());
            }
        }
    }

    /// Registra los atajos globales. Devuelve los nombres de los que no se pudieron usar.
    fn register_hotkeys(&mut self) -> Vec<String> {
        self.unregister_all();
        let mut failed = Vec::new();
        for a in GlobalAction::ALL {
            let (vk, mods) = self.cfg.hotkey(a);
            // Guardar clip solo mientras graba; los demás, siempre.
            if vk == 0 || (a == GlobalAction::SaveClip && !self.cfg.enabled) {
                continue;
            }
            let ok = unsafe { RegisterHotKey(self.hwnd, a.id(), HOT_KEY_MODIFIERS(mods | MOD_NOREPEAT.0), vk).is_ok() };
            if !ok {
                failed.push(hotkey_name(vk, mods));
            }
        }
        failed
    }

    fn tip(&self) -> String {
        if let Some(e) = &self.error {
            return format!("Clippy · error: {e}");
        }
        if self.cfg.enabled {
            let mut t = format!(
                "Clippy · grabando · {} guarda {}",
                hotkey_name(self.cfg.hotkey_vk, self.cfg.hotkey_mods),
                fmt_secs(self.cfg.seconds)
            );
            if let Some(g) = &self.game {
                t.push_str(&format!(" · juego: {g}"));
            }
            t
        } else {
            "Clippy · pausado".into()
        }
    }

    fn nid(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        }
    }

    fn tray(&self, op: NOTIFY_ICON_MESSAGE) {
        let mut n = self.nid();
        n.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        n.uCallbackMessage = WM_TRAY;
        n.hIcon = if self.cfg.enabled && self.error.is_none() { self.icon_on } else { self.icon_off };
        set_wstr(&mut n.szTip, &self.tip());
        unsafe {
            let _ = Shell_NotifyIconW(op, &n);
        }
    }

    fn balloon(&self, title: &str, text: &str, warn: bool) {
        let mut n = self.nid();
        n.uFlags = NIF_INFO;
        set_wstr(&mut n.szInfoTitle, title);
        set_wstr(&mut n.szInfo, text);
        n.dwInfoFlags = if warn { NIIF_WARNING } else { NIIF_INFO };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &n);
        }
    }

    fn save_clip(&self) {
        if let Some(r) = &self.rec {
            r.save();
        }
    }

    fn toggle(&mut self) {
        let mut c = self.cfg.clone();
        c.enabled = !c.enabled;
        let _ = config::save(&c);
        self.apply(c, false);
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Started(_) => {
                self.error = None;
                self.tray(NIM_MODIFY);
            }
            Event::Game(g) => {
                self.game = g;
                self.tray(NIM_MODIFY);
            }
            Event::Saved(_) => {} // el sonido lo reproduce la grabadora al instante
            Event::SaveFailed(e) => self.balloon("No se pudo guardar el clip", &e, true),
            Event::Error(e) => {
                // La grabadora avisa en cada reintento: el globo sale solo al pasar de grabar a error.
                let new = self.error.is_none();
                self.error = Some(e.clone());
                self.tray(NIM_MODIFY);
                if new {
                    self.balloon("Clippy no puede grabar", &format!("{e}\nSe reintenta solo."), true);
                }
            }
        }
    }

    /// Cierra todo: grabadora, editor y ventana principal.
    fn quit(&mut self) {
        Window::Editor.close();
        Window::Settings.close();
        self.rec = None;
        unsafe {
            self.unregister_all();
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.nid());
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn set_wstr(dst: &mut [u16], s: &str) {
    let v: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..v.len()].copy_from_slice(&v);
    dst[v.len()] = 0;
}

fn open_settings() {
    Mode::Settings.launch();
}

fn open_editor() {
    Mode::Editor(None).launch();
}

fn show_menu(hwnd: HWND) {
    let Some((enabled, key)) = with_app(|a| (a.cfg.enabled, hotkey_name(a.cfg.hotkey_vk, a.cfg.hotkey_mods))) else {
        return;
    };
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        // El editor va primero y en negrita: es lo que más se usa después de grabar.
        let _ = AppendMenuW(menu, MF_STRING, ID_EDITOR, w!("Abrir editor de clips"));
        let _ = SetMenuDefaultItem(menu, ID_EDITOR as u32, 0);
        let save_label = HSTRING::from(format!("Guardar clip ahora\t{key}"));
        let save_flags = if enabled { MF_STRING } else { MF_STRING | MF_GRAYED };
        let _ = AppendMenuW(menu, save_flags, ID_SAVE, &save_label);
        let _ = AppendMenuW(menu, MF_STRING, ID_FOLDER, w!("Abrir carpeta de clips"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let check = if enabled { MF_CHECKED } else { MF_UNCHECKED };
        let _ = AppendMenuW(menu, MF_STRING | check, ID_TOGGLE, w!("Grabando"));
        let mic = with_app(|a| a.cfg.mic_audio).unwrap_or(false);
        let _ = AppendMenuW(menu, MF_STRING | if mic { MF_CHECKED } else { MF_UNCHECKED }, ID_MIC, w!("Micrófono"));
        let _ = AppendMenuW(menu, MF_STRING, ID_SETTINGS, w!("Abrir Clippy"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("Salir"));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        run_command(cmd.0 as usize);
    }
}

fn run_command(id: usize) {
    match id {
        ID_TOGGLE => {
            with_app(|a| a.toggle());
        }
        ID_SAVE => {
            with_app(|a| a.save_clip());
        }
        ID_SETTINGS => open_settings(),
        ID_EDITOR => open_editor(),
        ID_MIC => {
            with_app(|a| {
                let mut c = a.cfg.clone();
                c.mic_audio = !c.mic_audio;
                let _ = config::save(&c);
                a.apply(c, false);
            });
        }
        ID_FOLDER => open_clips_folder(),
        ID_EXIT => {
            with_app(|a| a.quit());
        }
        _ => {}
    }
}

fn open_clips_folder() {
    if let Some(d) = with_app(|a| a.cfg.output_dir.clone()) {
        open_folder(&d);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match (lp.0 & 0xFFFF) as u32 {
                WM_LBUTTONUP | NIN_BALLOONUSERCLICK => open_settings(),
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_HOTKEY => {
            match GlobalAction::from_id(wp.0 as i32) {
                Some(GlobalAction::SaveClip) => {
                    with_app(|a| a.save_clip());
                }
                Some(GlobalAction::ToggleRecording) => {
                    // Desde el juego no se ve la bandeja: avisar con un sonido.
                    with_app(|a| {
                        a.toggle();
                        if a.cfg.enabled {
                            sound::play_resumed();
                        } else {
                            sound::play_paused();
                        }
                    });
                }
                Some(GlobalAction::OpenEditor) => open_editor(),
                Some(GlobalAction::OpenFolder) => open_clips_folder(),
                None => {}
            }
            LRESULT(0)
        }
        WM_UPDATE => {
            if let Some((v, _)) = update::available() {
                with_app(|a| a.balloon("Hay una versión nueva de Clippy", &format!("La versión {v} está lista. Tocá acá para actualizar."), false));
            }
            LRESULT(0)
        }
        WM_RELOAD => {
            let cfg = config::load();
            with_app(|a| a.apply(cfg, false));
            LRESULT(0)
        }
        WM_HOTKEY_SUSPEND => {
            with_app(|a| {
                if wp.0 == 1 {
                    a.unregister_all();
                } else {
                    a.register_hotkeys();
                }
            });
            LRESULT(0)
        }
        WM_REC_EVENT => {
            let evs: Vec<Event> = EVENTS.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default();
            for ev in evs {
                with_app(|a| a.handle_event(ev));
            }
            LRESULT(0)
        }
        // wparam 0: se canceló el apagado de Windows, así que se sigue grabando.
        WM_ENDSESSION if wp.0 == 0 => LRESULT(0),
        WM_CLOSE | WM_ENDSESSION => {
            // `taskkill /IM Clippy.exe` o apagado de Windows: salir prolijo.
            with_app(|a| a.quit());
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            let tc = TASKBAR_CREATED.with(|t| *t.borrow());
            if tc != 0 && msg == tc {
                // Explorer se reinició: volver a poner el ícono.
                with_app(|a| a.tray(NIM_ADD));
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}
