//! Biblioteca de videos: lista los clips de la carpeta + los importados, y en segundo plano
//! calcula duración, miniatura de portada, tira de miniaturas y forma de onda para la línea de tiempo.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::SystemTime;

use eframe::egui;

use super::media::{self, Probe};
use crate::mf::Mf;
use super::player::Video;
use crate::log::app_dir;

use crate::clipcache::Wave;
pub const VIDEO_EXTS: [&str; 6] = ["mp4", "mov", "m4v", "mkv", "wmv", "avi"];

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
    /// Miniaturas de la tira, ordenadas por tiempo (van llegando salteadas: primero las que se ven).
    pub thumbs: Vec<(i64, egui::TextureHandle)>,
    /// Tramos del archivo que la línea de tiempo muestra sin miniatura (uno por cuadro, de izquierda a
    /// derecha): de cada uno se hace primero la que cueste menos.
    pub wanted: Vec<(i64, i64)>,
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
    let Ok(mut v) = Video::open(p, w, h, true) else { return };
    let t = (pr.duration / 2).min(10_000_000);
    if let Ok(Some(img)) = v.frame_at(t) {
        let tex = ctx.load_texture(format!("cover:{}", p.display()), img, egui::TextureOptions::LINEAR);
        info.lock().unwrap().entry(p.to_path_buf()).or_default().cover = Some(tex);
    }
}

fn strip(p: &Path, info: &Arc<Mutex<HashMap<PathBuf, Info>>>, ctx: &egui::Context) {
    let Some(pr) = ensure_probe(p, info) else { return };
    // Fotogramas clave y forma de onda, de la caché si ya se calcularon. Si no, los claves salen enseguida
    // del índice del archivo; la forma de onda decodifica todo el audio (segundos), así que va en otro
    // hilo y las miniaturas no la esperan.
    let (wave, kf) = match crate::clipcache::load(p) {
        Some((w, k)) => (Some(w), k),
        None => (None, media::keyframes(p).unwrap_or_default()),
    };
    {
        let mut i = info.lock().unwrap();
        let e = i.entry(p.to_path_buf()).or_default();
        e.keyframes = Some(kf.clone());
        if wave.is_some() {
            e.wave = wave;
        } else {
            let (p, info, ctx, kf) = (p.to_path_buf(), info.clone(), ctx.clone(), kf.clone());
            std::thread::spawn(move || {
                let _mf = Mf::init();
                let wave = media::waveform(&p).unwrap_or_default();
                crate::clipcache::save(&p, &wave, &kf);
                info.lock().unwrap().entry(p).or_default().wave = Some(wave);
                ctx.request_repaint();
            });
        }
    }
    ctx.request_repaint();
    // Las miniaturas van apareciendo de a una: primero las que pide la pantalla.
    let (w, h) = fit(&pr, 160);
    if let Ok(mut v) = Video::open(p, w, h, true) {
        let wanted = || info.lock().unwrap().get(p).map(|e| e.wanted.clone()).unwrap_or_default();
        strip_frames(&mut v, pr.duration, &kf, wanted, |t, img| {
            let tex = ctx.load_texture(format!("strip:{}:{t}", p.display()), img, egui::TextureOptions::LINEAR);
            let mut i = info.lock().unwrap();
            let thumbs = &mut i.entry(p.to_path_buf()).or_default().thumbs;
            thumbs.insert(thumbs.partition_point(|(tt, _)| *tt < t), (t, tex));
            ctx.request_repaint();
        });
    }
}

/// Distancia entre miniaturas de la tira de un clip de esa duración: una por segundo, 150 como mucho.
pub fn strip_step(duration: i64) -> i64 {
    (duration / 150).max(10_000_000)
}

/// Cuánto después de un fotograma clave se pide el cuadro: el índice del archivo da su tiempo redondeado
/// y sin el corrimiento de los cuadros B, y pedirlo justo encima podría hacer buscar desde el clave anterior.
const KEY_MARGIN: i64 = 500_000; // 50 ms

/// Miniaturas de la tira: una cada `strip_step`. Si hay un fotograma clave a menos de medio paso se usa ese
/// (se decodifica un solo cuadro). Antes de cada una se mira `wanted` (los tramos que la pantalla muestra sin
/// miniatura) y se hace primero, del primero de esos tramos, la que menos cuadros hay que decodificar para
/// sacar. Si no falta ninguno, se sigue en orden desde la anterior sin volver a buscar.
pub(super) fn strip_frames(v: &mut Video, duration: i64, keys: &[i64], wanted: impl Fn() -> Vec<(i64, i64)>, mut each: impl FnMut(i64, egui::ColorImage)) {
    let step = strip_step(duration);
    // Plan: el tiempo de cada miniatura y el fotograma clave anterior a él.
    let mut plan: Vec<(i64, i64)> = Vec::new();
    let mut t = 0;
    while t < duration {
        let prev = plan.last().map_or(i64::MIN, |p| p.0);
        let from = keys.partition_point(|&k| k <= t - step / 2);
        let near = keys[from..].iter().copied().take_while(|&k| k < t + step / 2).filter(|&k| k > prev).min_by_key(|&k| (k - t).abs());
        let at = near.map_or(t, |k| (k + KEY_MARGIN).min(duration - 1));
        // Sin índice de claves no se sabe hasta dónde conviene seguir leyendo: se busca cada vez.
        let key = keys[..keys.partition_point(|&k| k <= at)].last().copied().unwrap_or(at);
        plan.push((at, key));
        t += step;
    }
    let mut done = vec![false; plan.len()];
    // La que conviene hacer para un tramo: la más barata de las que caen adentro (la que está sobre un
    // fotograma clave sale de un cuadro); si el tramo es más angosto que el paso, la más cercana.
    let pick = |(lo, hi): (i64, i64), done: &[bool]| {
        let (a, b) = (plan.partition_point(|p| p.0 < lo), plan.partition_point(|p| p.0 <= hi));
        if a < b {
            return (a..b).filter(|&i| !done[i]).min_by_key(|&i| plan[i].0 - plan[i].1);
        }
        let c = (lo + hi) / 2;
        let i = if a > 0 && (a == plan.len() || c - plan[a - 1].0 < plan[a].0 - c) { a - 1 } else { a };
        (i < plan.len() && !done[i]).then_some(i)
    };
    let mut next = 0;
    loop {
        let urgent = wanted().into_iter().find_map(|r| pick(r, &done));
        while next < plan.len() && done[next] {
            next += 1;
        }
        let Some(i) = urgent.or((next < plan.len()).then_some(next)) else { break };
        done[i] = true;
        let (at, key) = plan[i];
        if let Ok(Some(img)) = v.frame_ahead(at, key) {
            each(at, img);
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

/// Clips de la carpeta (más nuevos primero) + importados.
pub fn scan(dir: &Path, imported: &[PathBuf]) -> Vec<Asset> {
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
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("mp4")) && !p.to_string_lossy().contains("clippy-tmp") {
                add(p, false);
            }
        }
    }
    for p in imported {
        add(p.clone(), true);
    }
    v.sort_by_key(|a| std::cmp::Reverse(a.modified));
    v
}

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
