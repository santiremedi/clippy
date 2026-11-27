//! Qué es "el juego": el proceso de la ventana activa que ocupa todo su monitor
//! (pantalla completa o ventana sin bordes), salvo programas que no son juegos.
//! Lo usan la elección del monitor a grabar, el modo "Solo juegos" (imagen y carpeta del clip) y el canal de
//! audio "Juego". Una vez detectado queda fijo mientras siga abierto, aunque hagas Alt+Tab.

use windows::core::{w, HSTRING, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, RECT};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindow, IsZoomed,
};

/// Programas que pueden estar en pantalla completa sin ser juegos (en minúsculas, sin ".exe").
const NOT_GAMES: &[&str] = &[
    "chrome", "msedge", "firefox", "opera", "brave", "vivaldi",
    "vlc", "mpc-hc", "mpc-hc64", "mpc-be", "mpc-be64", "mpv", "potplayer", "potplayer64", "potplayermini", "potplayermini64",
    "microsoft.media.player", "video.ui", "wmplayer",
    "explorer", "applicationframehost", "powerpnt", "lockapp", "searchhost", "startmenuexperiencehost", "shellexperiencehost",
    // El recorte de pantalla de Windows tapa todo el monitor mientras elegís qué capturar.
    "snippingtool", "screenclippinghost",
    "clippy",
];

const FS_EVERY: i64 = 2_500_000; // 250 ms
const ALIVE_EVERY: i64 = 30_000_000; // 3 s
const STILL_ACTIVE: u32 = 259;

struct Proc {
    pid: u32,
    /// El ProductName del ejecutable (ej. "VALORANT") o, si no tiene, su nombre sin ".exe".
    name: String,
    handle: HANDLE,
    /// Última ventana del juego que estuvo adelante en pantalla completa (dice en qué monitor está).
    window: HWND,
}

impl Drop for Proc {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[derive(Default)]
pub struct Detector {
    game: Option<Proc>,
    /// Nada tapa al juego en su monitor: fue la última ventana que se activó ahí (ver `watch`).
    on_top: bool,
    /// Último proceso en pantalla completa que resultó no ser juego (para no consultarlo de nuevo).
    not_game: u32,
    next_fs: i64,
    next_alive: i64,
}

impl Detector {
    /// Revisa la ventana activa (cada 250 ms) y si el juego sigue abierto (cada 3 s).
    /// Devuelve true si cambió el juego.
    pub fn poll(&mut self, now: i64) -> bool {
        let mut changed = false;
        if now >= self.next_alive {
            self.next_alive = now + ALIVE_EVERY;
            if self.game.as_ref().is_some_and(|g| !alive(g)) {
                self.game = None;
                changed = true;
            }
        }
        if now >= self.next_fs {
            self.next_fs = now + FS_EVERY;
            if let Some((pid, window)) = fullscreen_window() {
                if let Some(g) = self.game.as_mut().filter(|g| g.pid == pid) {
                    g.window = window;
                } else if pid != self.not_game {
                    match open_game(pid, window) {
                        Some(p) => {
                            self.game = Some(p);
                            changed = true;
                        }
                        None => self.not_game = pid,
                    }
                }
            }
            self.watch();
        }
        changed
    }

    pub fn pid(&self) -> Option<u32> {
        self.game.as_ref().map(|g| g.pid)
    }

    /// Nombre del juego (ej. "VALORANT").
    pub fn name(&self) -> Option<String> {
        self.game.as_ref().map(|g| g.name.clone())
    }

    /// Monitor del juego, si está abierto y no minimizado.
    pub fn monitor(&self) -> Option<HMONITOR> {
        let w = self.game.as_ref()?.window;
        unsafe {
            if !IsWindow(w).as_bool() || IsIconic(w).as_bool() {
                return None;
            }
            let m = MonitorFromWindow(w, MONITOR_DEFAULTTONULL);
            (!m.is_invalid()).then_some(m)
        }
    }

    /// El juego está adelante en pantalla completa ahora mismo (ahí manda imágenes todo el tiempo).
    pub fn in_front(&self) -> bool {
        foreground().is_some_and(|h| self.is_front(h))
    }

    /// El juego se ve en el monitor `m`: está ahí (no minimizado) y nada lo tapa. Lo tapa la última ventana que se
    /// activó en su monitor si no fue él (Alt+Tab al navegador), aunque después actives algo en otro monitor o la
    /// barra de tareas; mirar Discord en otra pantalla no lo tapa. Se mira en cada cuadro: el clip pasa a negro en
    /// el mismo cuadro en que algo lo tapa.
    pub fn visible_on(&mut self, m: HMONITOR) -> bool {
        let covered = self.watch();
        !covered && self.on_top && self.monitor() == Some(m)
    }

    /// Mira la ventana activa y recuerda si tapa al juego en su monitor. Devuelve si lo tapa ahora mismo: el
    /// selector de Alt+Tab lo tapa mientras está, pero al irse deja todo como estaba.
    fn watch(&mut self) -> bool {
        let Some(m) = self.monitor() else {
            // Minimizado: al restaurarlo vuelve a estar arriba recién cuando pasa adelante.
            self.on_top = false;
            return false;
        };
        let Some(h) = foreground() else { return false };
        if self.is_front(h) {
            self.on_top = true;
            return false;
        }
        if unsafe { MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST) } != m {
            return false;
        }
        if !passing(h) {
            self.on_top = false;
        }
        true
    }

    /// `h` es la ventana del juego y ocupa todo su monitor.
    fn is_front(&self, h: HWND) -> bool {
        self.game.as_ref().is_some_and(|g| g.window == h) && covers_monitor(h)
    }
}

/// Monitor de la ventana activa (el escritorio y la barra de tareas no cuentan: el escritorio abarca todos).
pub fn active_monitor() -> Option<HMONITOR> {
    let m = unsafe { MonitorFromWindow(foreground()?, MONITOR_DEFAULTTONULL) };
    (!m.is_invalid()).then_some(m)
}

/// Ventana activa, salvo el escritorio y las barras de tareas.
fn foreground() -> Option<HWND> {
    let h = unsafe { GetForegroundWindow() };
    (!h.is_invalid() && !matches!(class(h).as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")).then_some(h)
}

/// Ventanas de Windows que pasan adelante solo mientras cambiás de ventana: el selector de Alt+Tab (Windows 11 y
/// 10) y la que activa Windows en el medio del cambio.
fn passing(h: HWND) -> bool {
    matches!(class(h).as_str(), "XamlExplorerHostIslandWindow" | "MultitaskingViewFrame" | "ForegroundStaging")
}

fn class(h: HWND) -> String {
    let mut cls = [0u16; 64];
    let n = unsafe { GetClassNameW(h, &mut cls) }.max(0) as usize;
    String::from_utf16_lossy(&cls[..n])
}

fn is_game(exe: &str) -> bool {
    !exe.is_empty() && !NOT_GAMES.iter().any(|n| exe.eq_ignore_ascii_case(n))
}

/// El proceso `pid`, si es un juego.
fn open_game(pid: u32, window: HWND) -> Option<Proc> {
    if pid == 0 || pid == std::process::id() {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).unwrap_or_default();
        // Si no es un juego, al soltarlo se cierra el proceso abierto.
        let mut p = Proc { pid, name: String::new(), handle, window };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let path = if !handle.is_invalid() && QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            // Algunos anticheats no dejan abrir el proceso: el nombre sale de la lista de procesos.
            snapshot().into_iter().find(|(p, _)| *p == pid).map(|(_, n)| n)?
        };
        let exe = exe_stem(&path);
        if !is_game(&exe) {
            return None;
        }
        p.name = product_name(&path).unwrap_or(exe);
        Some(p)
    }
}

/// "C:\\...\\VALORANT-Win64-Shipping.exe" -> "VALORANT-Win64-Shipping".
fn exe_stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or("");
    match file.len().checked_sub(4) {
        Some(n) if file.is_char_boundary(n) && file[n..].eq_ignore_ascii_case(".exe") => file[..n].to_string(),
        _ => file.to_string(),
    }
}

/// ProductName del ejecutable: el nombre del juego tal como lo publica, sin una lista para cada uno.
fn product_name(path: &str) -> Option<String> {
    if !std::path::Path::new(path).is_absolute() {
        return None;
    }
    unsafe {
        let path = HSTRING::from(path);
        let size = GetFileVersionInfoSizeW(&path, None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u64; (size as usize).div_ceil(8)];
        GetFileVersionInfoW(&path, 0, size, data.as_mut_ptr().cast()).ok()?;
        let block = data.as_ptr().cast();
        let mut value = std::ptr::null_mut();
        let mut len = 0;
        if !VerQueryValueW(block, w!("\\VarFileInfo\\Translation"), &mut value, &mut len).as_bool() || value.is_null() {
            return None;
        }
        let translations = std::slice::from_raw_parts(value.cast::<u16>(), len as usize / 2);
        for t in translations.as_chunks::<2>().0 {
            let key = HSTRING::from(format!("\\StringFileInfo\\{:04x}{:04x}\\ProductName", t[0], t[1]));
            if VerQueryValueW(block, &key, &mut value, &mut len).as_bool() && !value.is_null() && len > 1 {
                let name = String::from_utf16_lossy(std::slice::from_raw_parts(value.cast::<u16>(), len as usize));
                let name = name.trim_matches('\0').trim();
                if !name.is_empty() {
                    return Some(name.into());
                }
            }
        }
        None
    }
}

fn alive(p: &Proc) -> bool {
    if p.handle.is_invalid() {
        return snapshot().iter().any(|(pid, _)| *pid == p.pid);
    }
    let mut code = 0u32;
    unsafe { GetExitCodeProcess(p.handle, &mut code).is_ok() && code == STILL_ACTIVE }
}

/// (pid, nombre.exe) de todos los procesos.
fn snapshot() -> Vec<(u32, String)> {
    let mut v = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return v };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                v.push((e.th32ProcessID, String::from_utf16_lossy(&e.szExeFile[..n])));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    v
}

/// (PID, ventana) de la ventana activa si ocupa todo su monitor (el escritorio no cuenta).
fn fullscreen_window() -> Option<(u32, HWND)> {
    let h = foreground().filter(|&h| covers_monitor(h))?;
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    (pid != 0).then_some((pid, h))
}

/// La ventana `h` ocupa todo su monitor. Las ventanas normales y las maximizadas no cuentan: en un monitor sin barra
/// de tareas una ventana maximizada también lo tapa entero, y apps sin barra de título estándar (Discord, Spotify)
/// parecerían juegos. Un juego sin bordes no se maximiza: se agranda al tamaño del monitor.
fn covers_monitor(h: HWND) -> bool {
    unsafe {
        if IsZoomed(h).as_bool() {
            return false;
        }
        let mut r = RECT::default();
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if GetWindowRect(h, &mut r).is_err() || !GetMonitorInfoW(MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool() {
            return false;
        }
        let m = mi.rcMonitor;
        r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
    }
}
