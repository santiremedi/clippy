//! Biblioteca de videos: lista los clips de la carpeta (y de sus subcarpetas) + los importados, los mueve y los
//! manda a la Papelera, y en segundo plano calcula duración, miniatura de portada, tira de miniaturas y forma de
//! onda para la línea de tiempo.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::os::windows::ffi::OsStrExt;
use std::time::SystemTime;

use eframe::egui;

use super::media::{self, Probe};
use crate::mf::Mf;
use super::player::Video;
use crate::log::app_dir;

use crate::clipcache::Wave;
pub const VIDEO_EXTS: [&str; 6] = ["mp4", "mov", "m4v", "mkv", "wmv", "avi"];

/// Una carpeta dentro de la de los clips.
#[derive(Clone)]
pub struct Folder {
    pub path: PathBuf,
    pub name: String,
    /// Videos que tiene.
    pub count: usize,
}

pub struct Asset {
    pub path: PathBuf,
    pub title: String,
    pub size: u64,
    pub modified: SystemTime,
    pub imported: bool,
}

#[derive(Default)]
pub struct Info {
    pub probe: Option<Probe>,
    pub error: Option<String>,
    pub cover: Option<egui::TextureHandle>,
    pub thumbs: Vec<(i64, egui::TextureHandle)>,
    pub wave: Option<Wave>,
    pub keyframes: Option<Vec<i64>>,
    /// Tamaño y fecha del archivo cuando se leyó: si cambian, se vuelve a leer.
    stamp: Option<(u64, SystemTime)>,
    cover_requested: bool,
    strip_requested: bool,
}

enum Job {
    Cover(PathBuf),
    Strip(PathBuf),
}

type Queue = Arc<(Mutex<VecDeque<Job>>, Condvar)>;

pub struct Loader {
    pub info: Arc<Mutex<HashMap<PathBuf, Info>>>,
    queue: Queue,
}

impl Loader {
    pub fn new(ctx: egui::Context) -> Self {
        let info: Arc<Mutex<HashMap<PathBuf, Info>>> = Arc::default();
        let queue: Queue = Arc::default();
        {
            let (info, queue) = (info.clone(), queue.clone());
            std::thread::spawn(move || worker(info, queue, ctx));
        }
        Self { info, queue }
    }

    fn push(&self, job: Job, front: bool) {
        let (q, cv) = &*self.queue;
        let mut q = q.lock().unwrap();
        if front {
            q.push_front(job);
        } else {
            q.push_back(job);
        }
        cv.notify_one();
    }

    /// Pide la portada (para una tarjeta que se está viendo: va primero en la fila).
    /// `stamp` es el tamaño y la fecha del archivo: si cambiaron desde la última vez (por ejemplo,
    /// un clip que la grabadora estaba terminando de escribir), se vuelve a leer todo.
    pub fn want_cover(&self, path: &Path, stamp: (u64, SystemTime)) {
        let mut info = self.info.lock().unwrap();
        let e = info.entry(path.to_path_buf()).or_default();
        let mut strip = false;
        if e.stamp != Some(stamp) {
            strip = e.strip_requested;
            *e = Info { stamp: Some(stamp), ..Info::default() };
        }
        let cover = !e.cover_requested;
        e.cover_requested = true;
        e.strip_requested |= strip;
        drop(info);
        if strip {
            self.push(Job::Strip(path.to_path_buf()), true);
        }
        if cover {
            self.push(Job::Cover(path.to_path_buf()), true);
        }
    }

    /// Pide la tira de miniaturas y la forma de onda (prioridad: está en la línea de tiempo).
    pub fn want_strip(&self, path: &Path) {
        let mut info = self.info.lock().unwrap();
        let e = info.entry(path.to_path_buf()).or_default();
        if !e.strip_requested {
            e.strip_requested = true;
            drop(info);
            self.push(Job::Strip(path.to_path_buf()), true);
        }
    }

    pub fn probe(&self, path: &Path) -> Option<Probe> {
        if let Some(p) = self.info.lock().unwrap().get(path).and_then(|i| i.probe.clone()) {
            return Some(p);
        }
        let p = media::probe(path).ok()?;
        self.info.lock().unwrap().entry(path.to_path_buf()).or_default().probe = Some(p.clone());
        Some(p)
    }
}

fn worker(info: Arc<Mutex<HashMap<PathBuf, Info>>>, queue: Queue, ctx: egui::Context) {
    let _mf = Mf::init();
    crate::clipcache::prune();
    loop {
        let job = {
            let (q, cv) = &*queue;
            let mut q = q.lock().unwrap();
            while q.is_empty() {
                q = cv.wait(q).unwrap();
            }
            q.pop_front().unwrap()
        };
        match job {
            Job::Cover(p) => cover(&p, &info, &ctx),
            Job::Strip(p) => strip(&p, &info, &ctx),
        }
        ctx.request_repaint();
    }
}

fn ensure_probe(p: &Path, info: &Mutex<HashMap<PathBuf, Info>>) -> Option<Probe> {
    if let Some(pr) = info.lock().unwrap().get(p).and_then(|i| i.probe.clone()) {
        return Some(pr);
    }
    match media::probe(p) {
        Ok(pr) => {
            info.lock().unwrap().entry(p.to_path_buf()).or_default().probe = Some(pr.clone());
            Some(pr)
        }
        Err(e) => {
            info.lock().unwrap().entry(p.to_path_buf()).or_default().error = Some(e.to_string());
            None
        }
    }
}

fn cover(p: &Path, info: &Mutex<HashMap<PathBuf, Info>>, ctx: &egui::Context) {
    let Some(pr) = ensure_probe(p, info) else { return };
    let (w, h) = fit(&pr, 256);
    let Ok(mut v) = Video::open(p, w, h) else { return };
    let t = (pr.duration / 2).min(10_000_000);
    if let Ok(Some(img)) = v.frame_at(t) {
        let tex = ctx.load_texture(format!("cover:{}", p.display()), img, egui::TextureOptions::LINEAR);
        info.lock().unwrap().entry(p.to_path_buf()).or_default().cover = Some(tex);
    }
}

fn strip(p: &Path, info: &Mutex<HashMap<PathBuf, Info>>, ctx: &egui::Context) {
    let Some(pr) = ensure_probe(p, info) else { return };
    // Primero lo rápido: forma de onda y fotogramas clave (de la caché si ya se calcularon).
    let (wave, kf) = match crate::clipcache::load(p) {
        Some(c) => c,
        None => {
            let kf = media::keyframes(p).unwrap_or_default();
            let wave = media::waveform(p).unwrap_or_default();
            crate::clipcache::save(p, &wave, &kf);
            (wave, kf)
        }
    };
    {
        let mut i = info.lock().unwrap();
        let e = i.entry(p.to_path_buf()).or_default();
        e.wave = Some(wave);
        e.keyframes = Some(kf);
    }
    ctx.request_repaint();
    // Después las miniaturas, que van apareciendo de a una.
    let (w, h) = fit(&pr, 160);
    if let Ok(mut v) = Video::open(p, w, h) {
        let step = (pr.duration / 150).max(10_000_000);
        let mut t = 0;
        while t < pr.duration {
            if let Ok(Some(img)) = v.frame_at(t) {
                let tex = ctx.load_texture(format!("strip:{}:{t}", p.display()), img, egui::TextureOptions::LINEAR);
                info.lock().unwrap().entry(p.to_path_buf()).or_default().thumbs.push((t, tex));
                ctx.request_repaint();
            }
            t += step;
        }
    }
}

fn fit(p: &Probe, w: u32) -> (u32, u32) {
    let w = w.min(p.width.max(2)) & !1;
    let h = ((w as u64 * p.height as u64 / p.width.max(1) as u64) as u32).max(2) & !1;
    (w, h)
}

// --- Lista de archivos ---------------------------------------------------------------

fn library_file() -> PathBuf {
    app_dir().join("library.json")
}

pub fn load_imported() -> Vec<PathBuf> {
    std::fs::read_to_string(library_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_imported(v: &[PathBuf]) {
    let _ = std::fs::create_dir_all(app_dir());
    let _ = std::fs::write(library_file(), serde_json::to_string_pretty(v).unwrap_or_default());
}

pub fn is_video(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Videos de `dir` que se muestran (el archivo temporal de una exportación en curso, no).
fn videos_in(dir: &Path) -> impl Iterator<Item = PathBuf> {
    std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| is_video(p) && !p.to_string_lossy().contains("clippy-tmp"))
}

/// Las carpetas de adentro de la de los clips (`root`) y los videos que se ven: en la de los clips (`dir` None), los
/// suyos y los importados; dentro de una carpeta, los de ella. Los más nuevos primero.
pub fn scan(root: &Path, dir: Option<&Path>, imported: &[PathBuf]) -> (Vec<Folder>, Vec<Asset>) {
    let mut v: Vec<Asset> = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |p: PathBuf, imported: bool| {
        if !seen.insert(p.clone()) {
            return;
        }
        if let Ok(m) = std::fs::metadata(&p) {
            v.push(Asset {
                title: pretty_name(&p),
                size: m.len(),
                modified: m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                path: p,
                imported,
            });
        }
    };
    for p in videos_in(dir.unwrap_or(root)) {
        add(p, false);
    }
    if dir.is_none() {
        for p in imported {
            add(p.clone(), true);
        }
    }
    let rd = std::fs::read_dir(root).into_iter().flatten().flatten();
    let mut folders: Vec<Folder> = rd
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| Folder { name: e.file_name().to_string_lossy().into_owned(), count: videos_in(&e.path()).count(), path: e.path() })
        .collect();
    folders.sort_by_key(|f| f.name.to_lowercase());
    v.sort_by_key(|a| std::cmp::Reverse(a.modified));
    (folders, v)
}

/// Repite `op` un rato si falla: el editor (o el que lee las miniaturas) puede tener el archivo abierto un
/// instante más.
fn retry<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut left = 20;
    loop {
        match op() {
            Err(_) if left > 0 => {
                left -= 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            r => return r,
        }
    }
}

/// Manda un archivo o una carpeta a la Papelera de reciclaje. Si ese disco no tiene Papelera, Windows avisa
/// antes de borrar para siempre.
pub fn recycle(p: &Path) -> std::io::Result<()> {
    use windows::Win32::UI::Shell::*;
    let from: Vec<u16> = p.as_os_str().encode_wide().chain([0, 0]).collect();
    retry(|| {
        let mut op = SHFILEOPSTRUCTW {
            wFunc: FO_DELETE,
            pFrom: windows::core::PCWSTR(from.as_ptr()),
            fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING).0 as u16,
            ..Default::default()
        };
        let r = unsafe { SHFileOperationW(&mut op) };
        if r == 0 && !op.fAnyOperationsAborted.as_bool() && !p.exists() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!("no se pudo mandar a la Papelera ({r})")))
        }
    })
}

/// Mueve un video a la carpeta `dir` (si ahí hay uno que se llama igual, le agrega un número). Su caché se va con
/// él. Devuelve dónde quedó.
pub fn move_into(p: &Path, dir: &Path) -> std::io::Result<PathBuf> {
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut to = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while to.exists() {
        to = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    let cache = crate::clipcache::cache_file(p);
    retry(|| std::fs::rename(p, &to))?;
    let _ = std::fs::rename(cache, crate::clipcache::cache_file(&to));
    Ok(to)
}

/// Le cambia el nombre a una carpeta (`from` -> `to`); las cachés de sus videos se van con ellos. Devuelve dónde
/// quedó cada video.
pub fn rename_folder(from: &Path, to: &Path) -> std::io::Result<Vec<(PathBuf, PathBuf)>> {
    let videos: Vec<(PathBuf, PathBuf)> = videos_in(from).map(|p| {
        let c = crate::clipcache::cache_file(&p);
        (p, c)
    }).collect();
    retry(|| std::fs::rename(from, to))?;
    Ok(videos
        .into_iter()
        .map(|(p, cache)| {
            let new = to.join(p.file_name().unwrap_or_default());
            let _ = std::fs::rename(cache, crate::clipcache::cache_file(&new));
            (p, new)
        })
        .collect())
}

/// Un nombre de carpeta que Windows acepta: sin caracteres prohibidos ni puntos o espacios al final.
pub fn folder_name_ok(name: &str) -> bool {
    let n = name.trim();
    !n.is_empty() && !n.ends_with('.') && !n.chars().any(|c| BAD_NAME.contains(c) || c.is_control())
}

/// Caracteres que Windows no deja usar en un nombre de archivo o carpeta.
pub const BAD_NAME: &str = "<>:\"/\\|?*";

/// "Clippy_2026-11-15_23-38-30" -> "15/11 · 23:38:30"
pub fn pretty_name(p: &Path) -> String {
    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    if let Some(rest) = stem.strip_prefix("Clippy_") {
        let parts: Vec<&str> = rest.split(['_', '-']).collect();
        if parts.len() >= 6 && parts[..6].iter().all(|x| x.chars().all(|c| c.is_ascii_digit())) {
            let extra = if parts.len() > 6 { format!(" ({})", parts[6..].join(" ")) } else { String::new() };
            return format!("{}/{} · {}:{}:{}{}", parts[2], parts[1], parts[3], parts[4], parts[5], extra);
        }
    }
    stem
}
