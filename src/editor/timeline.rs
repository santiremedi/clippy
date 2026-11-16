//! Línea de tiempo: pistas de video y de audio con sus piezas, las ediciones y deshacer.
//!
//! Un clip entra como dos piezas unidas, la imagen en una pista de video y el sonido en una de audio:
//! se eligen, mueven, recortan, dividen y borran juntas hasta que se las separa.
//!
//! Con el imán prendido, las piezas de cada pista quedan una detrás de la otra desde el inicio
//! (si dos ya se fundían, siguen igual). Apagado, cada pieza queda donde se la suelta; si se
//! superpone con otra de su pista, lo que se superponen es un fundido cruzado.

use std::path::{Path, PathBuf};

use super::media::{self, Clip, Curve, Edit};
use super::playback::UNKNOWN_FRAME;
use super::EditorApp;

pub(super) const MIN_LEN: i64 = 2_000_000; // 0,2 s
/// Pasos de deshacer que se guardan.
const UNDO_MAX: usize = 100;
/// Pistas de cada tipo como máximo: más no entran en la línea de tiempo.
pub(super) const MAX_TRACKS: usize = 4;

fn push_capped(stack: &mut Vec<Vec<Track>>, v: Vec<Track>) {
    stack.push(v);
    if stack.len() > UNDO_MAX {
        stack.remove(0);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TrackKind {
    Video,
    Audio,
}

#[derive(Clone, PartialEq)]
pub(super) struct Piece {
    pub(super) id: u64,
    pub(super) path: PathBuf,
    /// Dónde empieza en la línea.
    pub(super) start: i64,
    pub(super) src_in: i64,
    pub(super) src_out: i64,
    pub(super) gain: f32,
    pub(super) muted: bool,
    /// Las piezas con el mismo número (distinto de 0) están unidas: la imagen y el sonido de un clip.
    pub(super) link: u64,
    /// Cuánto tarda en aparecer desde negro (o silencio) y en irse.
    pub(super) fade_in: i64,
    pub(super) fade_out: i64,
    /// Forma de los fundidos del sonido.
    pub(super) curve: Curve,
}

impl Piece {
    pub(super) fn len(&self) -> i64 {
        self.src_out - self.src_in
    }
    pub(super) fn end(&self) -> i64 {
        self.start + self.len()
    }
    pub(super) fn covers(&self, t: i64) -> bool {
        self.start <= t && t < self.end()
    }
    pub(super) fn eff_gain(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            self.gain
        }
    }
    /// Que los fundidos a negro entren en la pieza (después de recortarla).
    fn fit_fades(&mut self) {
        self.fade_in = self.fade_in.min(self.len());
        self.fade_out = self.fade_out.min(self.len() - self.fade_in);
    }
}

#[derive(Clone, PartialEq)]
pub(super) struct Track {
    pub(super) id: u64,
    pub(super) kind: TrackKind,
    /// Pista de video oculta: no se ve.
    pub(super) hidden: bool,
    /// Pista de audio muda: no suena.
    pub(super) muted: bool,
    /// Ordenadas por inicio.
    pub(super) clips: Vec<Piece>,
}

impl Track {
    pub(super) fn new(id: u64, kind: TrackKind) -> Self {
        Track { id, kind, hidden: false, muted: false, clips: Vec::new() }
    }

    pub(super) fn end(&self) -> i64 {
        self.clips.iter().map(Piece::end).max().unwrap_or(0)
    }

    /// Cuánto se superpone cada pieza con la anterior y con la siguiente: sus fundidos cruzados.
    pub(super) fn overlaps(&self) -> Vec<(i64, i64)> {
        media::overlaps(&self.clips.iter().map(|p| (p.start, p.end())).collect::<Vec<_>>())
    }

    /// Lo que hay en esta pista se oye.
    pub(super) fn sounds(&self) -> bool {
        self.kind == TrackKind::Audio && !self.muted
    }

    fn sort(&mut self) {
        self.clips.sort_by_key(|p| p.start);
    }

    /// Con el imán: aplica `f` a las piezas y a cuánto se superpone cada una con la anterior
    /// (0 = pegadas) y las vuelve a poner una detrás de la otra desde el inicio.
    fn reflow(&mut self, f: impl FnOnce(&mut Vec<Piece>, &mut Vec<i64>)) {
        let mut ov: Vec<i64> = self.overlaps().iter().map(|f| f.0).collect();
        f(&mut self.clips, &mut ov);
        self.relayout(&ov);
    }

    /// Ubica las piezas una detrás de la otra desde el inicio, cada una superpuesta `ov` con la anterior
    /// (nunca más que la mitad de cualquiera de las dos).
    fn relayout(&mut self, ov: &[i64]) {
        let mut end = 0;
        let mut prev = 0;
        for (i, p) in self.clips.iter_mut().enumerate() {
            let o = if i == 0 { 0 } else { ov.get(i).copied().unwrap_or(0).clamp(0, prev.min(p.len()) / 2) };
            p.start = end - o;
            end = p.end();
            prev = p.len();
        }
    }

    /// Saca la pieza `i`; con el imán las siguientes se corren a ocupar su lugar.
    fn take(&mut self, i: usize, magnet: bool) -> Piece {
        if !magnet {
            return self.clips.remove(i);
        }
        let mut p = None;
        self.reflow(|c, ov| {
            p = Some(c.remove(i));
            ov.remove(i);
            if let Some(o) = ov.get_mut(i) {
                *o = 0;
            }
        });
        p.unwrap()
    }

    /// Pone `p`: con el imán, en el lugar que le toca a su centro y pegada a sus vecinas; sin él, donde dice su inicio.
    fn put(&mut self, p: Piece, magnet: bool) {
        if !magnet {
            self.clips.push(p);
            self.sort();
            return;
        }
        let center = p.start + p.len() / 2;
        let k = self.clips.iter().filter(|c| c.start + c.len() / 2 < center).count();
        self.reflow(|c, ov| {
            c.insert(k, p);
            ov.insert(k, 0);
            if let Some(o) = ov.get_mut(k + 1) {
                *o = 0;
            }
        });
    }
}

/// Una pieza a la que se le cambia un fundido arrastrando una esquina de arriba.
#[derive(Clone)]
pub(super) struct Faded {
    pub(super) at: (usize, usize),
    orig: Piece,
    ov: Vec<i64>,
    /// Largo de la vecina de ese lado, si la toca: la esquina cambia cuánto se funden (alargando la pieza por
    /// debajo de la otra con el material que sobra del archivo) y, pasando el borde, cuánto tarda en ir a
    /// negro. Sin vecina, solo lo segundo.
    neighbor: Option<i64>,
    max: i64,
}

/// Una pieza que se está recortando: dónde está y cómo estaban ella y su pista al empezar.
#[derive(Clone)]
pub(super) struct Trimmed {
    pub(super) at: (usize, usize),
    pub(super) orig: Piece,
    /// Cómo se unía cada pieza de su pista con la anterior (con el imán, eso no cambia al recortar).
    ov: Vec<i64>,
    /// Hasta dónde llega el archivo.
    max: i64,
}

impl EditorApp {
    pub(super) fn total(&self) -> i64 {
        self.tracks.iter().map(Track::end).max().unwrap_or(0)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.tracks.iter().all(|t| t.clips.is_empty())
    }

    /// Pista y lugar de la pieza `id`.
    pub(super) fn find(&self, id: u64) -> Option<(usize, usize)> {
        self.tracks.iter().enumerate().find_map(|(ti, t)| t.clips.iter().position(|p| p.id == id).map(|i| (ti, i)))
    }

    pub(super) fn selection(&self) -> Option<(usize, usize)> {
        self.selected.and_then(|id| self.find(id))
    }

    /// La pieza `id` y, si está unida, su pareja de la otra pista: lo que se edita junto.
    pub(super) fn group(&self, id: u64) -> Vec<(usize, usize)> {
        let Some((ti, i)) = self.find(id) else { return Vec::new() };
        let link = self.tracks[ti].clips[i].link;
        if link == 0 {
            return vec![(ti, i)];
        }
        let mut g = Vec::new();
        for (tj, t) in self.tracks.iter().enumerate() {
            g.extend(t.clips.iter().enumerate().filter(|(_, p)| p.link == link).map(|(j, _)| (tj, j)));
        }
        g
    }

    /// La pieza está elegida, o unida a la elegida.
    pub(super) fn is_selected(&self, p: &Piece) -> bool {
        let link = self.selection().map(|(ti, i)| self.tracks[ti].clips[i].link);
        self.selected == Some(p.id) || (p.link != 0 && link == Some(p.link))
    }

    /// La pieza que se ve (o se oye) en `t`: la de más arriba en la línea de tiempo.
    pub(super) fn piece_at(&self, t: i64) -> Option<(usize, usize)> {
        self.display_order().into_iter().find_map(|ti| self.tracks[ti].clips.iter().rposition(|p| p.covers(t)).map(|i| (ti, i)))
    }

    /// Pistas en el orden en que se ven, de arriba hacia abajo: las de video de la última a la primera
    /// (la de más arriba tapa a las de abajo) y después las de audio.
    pub(super) fn display_order(&self) -> Vec<usize> {
        let of = |k: TrackKind| (0..self.tracks.len()).filter(move |&i| self.tracks[i].kind == k);
        of(TrackKind::Video).rev().chain(of(TrackKind::Audio)).collect()
    }

    pub(super) fn count(&self, kind: TrackKind) -> usize {
        self.tracks.iter().filter(|t| t.kind == kind).count()
    }

    /// "Video 2", "Audio 1": el número entre las pistas de su tipo.
    pub(super) fn track_name(&self, ti: usize) -> String {
        let k = self.tracks[ti].kind;
        let n = self.tracks[..=ti].iter().filter(|t| t.kind == k).count();
        format!("{} {n}", if k == TrackKind::Video { "Video" } else { "Audio" })
    }

    pub(super) fn has_audio(&self, path: &Path) -> bool {
        self.loader.probe(path).is_some_and(|p| p.audio.is_some())
    }

    fn new_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// La primera pista de ese tipo; si no hay, una nueva.
    fn first_track(&mut self, kind: TrackKind) -> usize {
        if let Some(ti) = self.tracks.iter().position(|t| t.kind == kind) {
            return ti;
        }
        let id = self.new_id();
        self.tracks.push(Track::new(id, kind));
        self.tracks.len() - 1
    }

    /// Guarda cómo estaba la línea (`before`) para poder deshacer el cambio que se hizo o se va a hacer.
    pub(super) fn record_undo(&mut self, before: Vec<Track>) {
        self.dirty = true;
        push_capped(&mut self.undo, before);
        self.redo.clear();
    }

    fn push_undo(&mut self) {
        self.record_undo(self.tracks.clone());
    }

    pub(super) fn undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            let pos = self.pos();
            self.dirty = true;
            self.redo.push(std::mem::replace(&mut self.tracks, prev));
            self.after_edit(pos);
        }
    }

    pub(super) fn redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            let pos = self.pos();
            self.dirty = true;
            let now = std::mem::replace(&mut self.tracks, next);
            push_capped(&mut self.undo, now);
            self.after_edit(pos);
        }
    }

    /// Agrega el clip al final de la primera pista de video, con su sonido unido en la primera de audio.
    pub(super) fn add_to_timeline(&mut self, path: &Path) {
        let Some(pr) = self.loader.probe(path) else { return };
        if pr.duration <= MIN_LEN {
            return;
        }
        self.push_undo();
        let vt = self.first_track(TrackKind::Video);
        let start = self.tracks[vt].end();
        let id = self.new_id();
        let mut p = Piece { id, path: path.to_path_buf(), start, src_in: 0, src_out: pr.duration, gain: 1.0, muted: false, link: 0, fade_in: 0, fade_out: 0, curve: Curve::default() };
        if pr.audio.is_some() {
            p.link = self.new_id();
            let mut a = p.clone();
            a.id = self.new_id();
            let at = self.first_track(TrackKind::Audio);
            self.tracks[at].put(a, false);
        }
        self.tracks[vt].clips.push(p);
        self.selected = Some(id);
        self.loader.want_strip(path);
        self.after_edit(start);
        self.message = None;
    }

    /// Divide en el cursor la pieza elegida (con su pareja) si está ahí; si no, la de más arriba.
    pub(super) fn split(&mut self) {
        let t = self.pos();
        let chosen = self.selection().filter(|&(ti, i)| self.tracks[ti].clips[i].covers(t));
        let Some((ti, i)) = chosen.or_else(|| self.piece_at(t)) else { return };
        let parts: Vec<(usize, usize)> = self.group(self.tracks[ti].clips[i].id).into_iter().filter(|&(tj, j)| self.tracks[tj].clips[j].covers(t)).collect();
        let fits = |p: &Piece| t - p.start >= MIN_LEN && p.end() - t >= MIN_LEN;
        if !parts.iter().all(|&(tj, j)| fits(&self.tracks[tj].clips[j])) {
            return;
        }
        self.push_undo();
        let link = if parts.len() > 1 { self.new_id() } else { 0 };
        let clicked = self.tracks[ti].clips[i].id;
        for (tj, j) in parts {
            let mut b = self.tracks[tj].clips[j].clone();
            let st = b.src_in + t - b.start;
            b.id = self.new_id();
            b.src_in = st;
            b.start = t;
            b.link = link;
            b.fade_in = 0;
            if self.tracks[tj].clips[j].id == clicked {
                self.selected = Some(b.id);
            }
            let track = &mut self.tracks[tj];
            track.clips[j].src_out = st;
            track.clips[j].fade_out = 0;
            track.clips[j].fit_fades();
            b.fit_fades();
            track.clips.insert(j + 1, b);
            track.sort();
        }
        self.after_edit(t);
    }

    pub(super) fn delete_selected(&mut self) {
        let Some((ti, i)) = self.selection() else { return };
        self.push_undo();
        let start = self.tracks[ti].clips[i].start;
        let magnet = self.prefs.magnet;
        // Cada pieza del grupo está en otra pista: sacar una no corre a las demás.
        for (tj, j) in self.group(self.tracks[ti].clips[i].id) {
            self.tracks[tj].take(j, magnet);
        }
        let track = &self.tracks[ti];
        self.selected = track.clips.get(i).or(track.clips.last()).map(|p| p.id);
        self.after_edit(start);
    }

    /// Copia la pieza elegida (con su pareja) justo después de ella. Sin el imán, si ahí ya hay algo,
    /// va después de lo último de esas pistas.
    pub(super) fn duplicate_selected(&mut self) {
        let Some((ti, i)) = self.selection() else { return };
        self.push_undo();
        let magnet = self.prefs.magnet;
        let group = self.group(self.tracks[ti].clips[i].id);
        let len = self.tracks[ti].clips[i].len();
        let free = group.iter().all(|&(tj, j)| {
            let at = self.tracks[tj].clips[j].end();
            self.tracks[tj].clips.iter().all(|c| c.end() <= at || c.start >= at + len)
        });
        let shift = if free || magnet {
            len
        } else {
            group.iter().map(|&(tj, j)| self.tracks[tj].end() - self.tracks[tj].clips[j].start).max().unwrap_or(len)
        };
        let link = if group.len() > 1 { self.new_id() } else { 0 };
        let copies: Vec<(usize, Piece)> = group.iter().map(|&(tj, j)| (tj, self.tracks[tj].clips[j].clone())).collect();
        let first = self.tracks[ti].clips[i].id;
        for (tj, mut p) in copies {
            let was = p.id;
            p.id = self.new_id();
            p.start += shift;
            p.link = link;
            if was == first {
                self.selected = Some(p.id);
            }
            self.tracks[tj].put(p, magnet);
        }
        let start = self.selection().map_or(0, |(ti, i)| self.tracks[ti].clips[i].start);
        self.after_edit(start);
    }

    /// La pieza de audio de lo elegido (ella misma o su pareja), si tiene.
    pub(super) fn selected_audio(&self) -> Option<(usize, usize)> {
        let id = self.selected?;
        self.group(id).into_iter().find(|&(ti, _)| self.tracks[ti].kind == TrackKind::Audio)
    }

    pub(super) fn toggle_mute_selected(&mut self) {
        let Some((ti, i)) = self.selected_audio() else { return };
        self.push_undo();
        let p = &mut self.tracks[ti].clips[i];
        p.muted = !p.muted;
    }

    /// Con qué se uniría la pieza elegida si no está unida: la pieza suelta de la otra clase de pista que
    /// salió del mismo archivo (sus tramos se tocan), la más cercana.
    fn link_partner(&self) -> Option<(usize, usize)> {
        let (ti, i) = self.selection()?;
        let p = &self.tracks[ti].clips[i];
        if p.link != 0 {
            return None;
        }
        let mut best: Option<((usize, usize), i64)> = None;
        for (tj, t) in self.tracks.iter().enumerate().filter(|(_, t)| t.kind != self.tracks[ti].kind) {
            for (j, c) in t.clips.iter().enumerate() {
                if c.link == 0 && c.path == p.path && c.src_in < p.src_out && p.src_in < c.src_out {
                    let gap = ((c.start - c.src_in) - (p.start - p.src_in)).abs();
                    if best.is_none_or(|b| gap < b.1) {
                        best = Some(((tj, j), gap));
                    }
                }
            }
        }
        best.map(|b| b.0)
    }

    /// La pieza elegida está unida a otra (se puede separar) o tiene con cuál volver a unirse.
    pub(super) fn can_link(&self) -> bool {
        self.group(self.selected.unwrap_or(0)).len() > 1 || self.link_partner().is_some()
    }

    /// Qué hace ahora el botón de separar o unir.
    pub(super) fn link_label(&self) -> &'static str {
        if self.group(self.selected.unwrap_or(0)).len() > 1 {
            "Separar audio y video"
        } else {
            "Volver a unir audio y video"
        }
    }

    /// Separa la imagen y el sonido de lo elegido o, si están separados, los vuelve a unir: el sonido
    /// vuelve a quedar a la par de la imagen.
    pub(super) fn toggle_link(&mut self) {
        let Some((ti, i)) = self.selection() else { return };
        let group = self.group(self.tracks[ti].clips[i].id);
        if group.len() > 1 {
            self.push_undo();
            for (tj, j) in group {
                self.tracks[tj].clips[j].link = 0;
            }
            return;
        }
        let Some((tj, j)) = self.link_partner() else { return };
        self.push_undo();
        let link = self.new_id();
        let p = self.tracks[ti].clips[i].clone();
        let c = &mut self.tracks[tj].clips[j];
        c.start = p.start + c.src_in - p.src_in;
        c.link = link;
        self.tracks[ti].clips[i].link = link;
        self.tracks[tj].sort();
        let pos = self.pos();
        self.after_edit(pos);
    }

    pub(super) fn add_track(&mut self, kind: TrackKind) {
        if self.count(kind) >= MAX_TRACKS {
            return;
        }
        self.push_undo();
        let id = self.new_id();
        self.tracks.push(Track::new(id, kind));
    }

    /// Quita la pista y lo que tiene (lo que estaba unido a eso queda suelto). Siempre queda una de video.
    pub(super) fn remove_track(&mut self, ti: usize) {
        if self.tracks[ti].kind == TrackKind::Video && self.count(TrackKind::Video) == 1 {
            return;
        }
        self.push_undo();
        let pos = self.pos();
        let gone = self.tracks.remove(ti);
        for t in &mut self.tracks {
            for p in &mut t.clips {
                if p.link != 0 && gone.clips.iter().any(|g| g.link == p.link) {
                    p.link = 0;
                }
            }
        }
        self.after_edit(pos);
    }

    /// Oculta o muestra una pista de video; silencia o activa una de audio.
    pub(super) fn toggle_track(&mut self, ti: usize) {
        self.push_undo();
        let t = &mut self.tracks[ti];
        match t.kind {
            TrackKind::Video => t.hidden = !t.hidden,
            TrackKind::Audio => t.muted = !t.muted,
        }
    }

    /// Suelta la pieza `id` (y su pareja, corrida lo mismo) empezando en `start`, en la pista `to` si es de su
    /// tipo. Con el imán cada una va al lugar que le toca a su centro, pegada a sus vecinas. Devuelve si cambió algo.
    pub(super) fn drop_piece(&mut self, id: u64, to: usize, start: i64) -> bool {
        let Some((ti, i)) = self.find(id) else { return false };
        let magnet = self.prefs.magnet;
        let shift = start.max(0) - self.tracks[ti].clips[i].start;
        let group = self.group(id);
        // Que ninguna quede antes del inicio de la línea.
        let shift = group.iter().map(|&(tj, j)| -self.tracks[tj].clips[j].start).fold(shift, i64::max);
        let before = self.tracks.clone();
        let mut moved: Vec<(usize, Piece)> = Vec::new();
        // De la última a la primera de cada pista, así los lugares que quedan siguen valiendo.
        let mut group = group;
        group.sort_by(|a, b| b.cmp(a));
        for (tj, j) in group {
            let mut p = self.tracks[tj].take(j, magnet);
            p.start += shift;
            moved.push((if p.id == id { to } else { tj }, p));
        }
        for (tj, p) in moved {
            self.tracks[tj].put(p, magnet);
        }
        self.tracks != before
    }

    /// Empieza a recortar la pieza `(ti, i)` y su pareja (primero la tocada).
    pub(super) fn start_trim(&self, ti: usize, i: usize) -> Vec<Trimmed> {
        let mut group = self.group(self.tracks[ti].clips[i].id);
        group.sort_by_key(|&at| at != (ti, i));
        group
            .into_iter()
            .map(|(tj, j)| {
                let orig = self.tracks[tj].clips[j].clone();
                let max = self.loader.probe(&orig.path).map(|p| p.duration).unwrap_or(orig.src_out);
                Trimmed { at: (tj, j), ov: self.tracks[tj].overlaps().iter().map(|f| f.0).collect(), max, orig }
            })
            .collect()
    }

    /// Mueve el borde izquierdo (`left`) o derecho de las piezas que se recortan `dt` desde donde estaban,
    /// hasta donde dejen todas. Con el imán, las que siguen en cada pista se corren y cada unión queda como
    /// antes; sin él, nada más se mueve.
    pub(super) fn trim(&mut self, parts: &[Trimmed], left: bool, dt: i64) {
        let magnet = self.prefs.magnet;
        let (mut lo, mut hi) = (i64::MIN, i64::MAX);
        for t in parts {
            let o = &t.orig;
            if left {
                // Sin el imán el borde no puede quedar antes del inicio de la línea.
                let first = if magnet { 0 } else { (o.src_in - o.start).max(0) };
                lo = lo.max(first - o.src_in);
                hi = hi.min(o.src_out - MIN_LEN - o.src_in);
            } else {
                lo = lo.max(o.src_in + MIN_LEN - o.src_out);
                hi = hi.min(t.max - o.src_out);
            }
        }
        let dt = dt.clamp(lo, hi.max(lo));
        for t in parts {
            let track = &mut self.tracks[t.at.0];
            let p = &mut track.clips[t.at.1];
            if left {
                p.src_in = t.orig.src_in + dt;
                if !magnet {
                    p.start = t.orig.start + dt;
                }
            } else {
                p.src_out = t.orig.src_out + dt;
            }
            p.fit_fades();
            if magnet {
                track.relayout(&t.ov);
            }
        }
    }

    /// Dónde está la esquina de arriba de cada lado de la pieza: en el borde si se funde con la vecina o no
    /// tiene fundido; si no, donde termina su fundido a negro.
    pub(super) fn fade_handles(&self, ti: usize, i: usize) -> (i64, i64) {
        let t = &self.tracks[ti];
        let (p, ov) = (&t.clips[i], t.overlaps()[i]);
        (p.start + if ov.0 > 0 { 0 } else { p.fade_in }, p.end() - if ov.1 > 0 { 0 } else { p.fade_out })
    }

    /// Lo que mide el fundido que se está arrastrando, para mostrarlo: cruzado (true) o a negro.
    pub(super) fn fade_len(&self, f: &Faded, out: bool) -> (bool, i64) {
        let (ti, i) = f.at;
        let ov = self.tracks[ti].overlaps()[i];
        let p = &self.tracks[ti].clips[i];
        match (out, if out { ov.1 } else { ov.0 }) {
            (_, o) if o > 0 => (true, o),
            (true, _) => (false, p.fade_out),
            (false, _) => (false, p.fade_in),
        }
    }

    /// Empieza a arrastrar la esquina de arriba (`out`: la derecha) de la pieza `(ti, i)`. Solo de esa: aunque
    /// esté unida, la imagen y el sonido tienen cada uno sus fundidos (y siguen a la par: alargar el fundido
    /// cruzado no cambia dónde cae cada momento del archivo).
    pub(super) fn start_fade(&self, ti: usize, i: usize, out: bool) -> Faded {
        let t = &self.tracks[ti];
        let p = &t.clips[i];
        // Toca a la vecina si la pisa o queda a menos de un cuadro.
        let near = if out { t.clips.get(i + 1).filter(|n| n.start <= p.end() + UNKNOWN_FRAME / 2) } else { i.checked_sub(1).map(|k| &t.clips[k]).filter(|n| n.end() >= p.start - UNKNOWN_FRAME / 2) };
        let max = self.loader.probe(&p.path).map(|q| q.duration).unwrap_or(p.src_out);
        Faded { at: (ti, i), orig: p.clone(), ov: t.overlaps().iter().map(|f| f.0).collect(), neighbor: near.map(Piece::len), max }
    }

    /// Mueve la esquina de arriba (`out`: la derecha) de la pieza `dt` desde donde estaba.
    pub(super) fn fade(&mut self, f: &Faded, out: bool, dt: i64) {
        let magnet = self.prefs.magnet;
        let o = &f.orig;
        // Hacia afuera de la pieza es positivo.
        let d = if out { dt } else { -dt };
        let (fade0, other) = if out { (o.fade_out, o.fade_in) } else { (o.fade_in, o.fade_out) };
        let fade_max = (o.len() - other).max(0);
        let track = &mut self.tracks[f.at.0];
        let p = &mut track.clips[f.at.1];
        let Some(nb_len) = f.neighbor else {
            let v = (fade0 - d).clamp(0, fade_max);
            if out {
                p.fade_out = v;
            } else {
                p.fade_in = v;
            }
            return;
        };
        // Índice de la unión con la vecina en `ov`.
        let k = if out { f.at.1 + 1 } else { f.at.1 };
        let ov0 = f.ov.get(k).copied().unwrap_or(0);
        let room = if out { f.max - o.src_out } else { o.src_in };
        // Positivo: cuánto se funde con la vecina; negativo: cuánto tarda en ir a negro.
        let s = (ov0 - fade0 + d).clamp(-fade_max, (ov0 + room).min(nb_len / 2).max(ov0));
        let grow = s.max(0) - ov0;
        if out {
            p.src_out = o.src_out + grow;
            p.fade_out = (-s).max(0);
        } else {
            p.src_in = o.src_in - grow;
            p.fade_in = (-s).max(0);
            if !magnet {
                p.start = o.start - grow;
            }
        }
        if magnet {
            let mut ov = f.ov.clone();
            if let Some(x) = ov.get_mut(k) {
                *x = s.max(0);
            }
            track.relayout(&ov);
        }
    }

    /// Tras cualquier cambio: pausar, soltar reproductores que ya no se usan y ubicar el cursor.
    /// Las piezas quedan en su lugar de una (deshacer y el teclado no se animan).
    fn after_edit(&mut self, pos: i64) {
        self.settle = None;
        self.pause();
        let used: Vec<PathBuf> = self.tracks.iter().flat_map(|t| t.clips.iter().map(|p| p.path.clone())).collect();
        self.idle.retain(|v| used.contains(&v.path));
        for p in &used {
            self.loader.want_strip(p);
        }
        self.seek(pos);
    }

    pub(super) fn commit(&mut self, before: Option<Vec<Track>>, pos: i64) {
        for t in &mut self.tracks {
            t.sort();
        }
        if let Some(b) = before {
            if b != self.tracks {
                self.record_undo(b);
            }
        }
        self.after_edit(pos);
    }

    /// Lo que hay que exportar: las pistas de video visibles y las de audio, con sus fundidos.
    pub(super) fn edit(&self) -> Edit {
        let mut e = Edit::default();
        for t in &self.tracks {
            let clips: Vec<Clip> = t
                .clips
                .iter()
                .zip(t.overlaps())
                .map(|(p, xfade)| {
                    let gain = if t.sounds() { p.eff_gain() } else { 0.0 };
                    let fade = (p.fade_in, p.fade_out);
                    Clip { id: p.id, path: p.path.clone(), start: p.start, src_in: p.src_in, src_out: p.src_out, fade, xfade, gain, curve: p.curve }
                })
                .collect();
            match t.kind {
                TrackKind::Video if !t.hidden => e.video.push(clips),
                TrackKind::Video => {}
                TrackKind::Audio => e.audio.push(clips),
            }
        }
        e
    }
}
