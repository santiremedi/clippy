//! Panel de la derecha: lo que se ajusta de la pieza elegida (imagen, velocidad, efectos y sonido). Las acciones
//! (dividir, duplicar, congelar, borrar...) están en la barra de la línea de tiempo y en el clic derecho.

use eframe::egui::{self, RichText};

use crate::ui_kit::theme::*;
use super::library;
use super::media::{self, Frame, SPEED_MAX, SPEED_MIN};
use super::shortcuts::{self, Act};
use super::{EditorApp, HelpTab};

/// Intensidad del desenfoque de movimiento al prenderlo.
const BLUR_ON: f32 = 0.5;

/// Velocidades a un toque (en el panel y en el clic derecho).
pub(super) const SPEEDS: [f64; 5] = [0.5, 1.0, 1.5, 2.0, 3.0];

/// 0.5 -> "0.5×", 2 -> "2×", 1.25 -> "1.25×".
pub(super) fn fmt_speed(s: f64) -> String {
    let t = format!("{s:.2}");
    format!("{}×", t.trim_end_matches('0').trim_end_matches('.'))
}

/// El cambio de `r` empieza una edición, así que hay que guardar cómo estaba para deshacer: un arrastre entero (o
/// un cambio suelto, con el teclado) es un solo paso. Hay que llamarla en cada cuadro.
fn starts_edit(ui: &egui::Ui, r: &egui::Response) -> bool {
    let id = r.id.with("editing");
    let was = ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
    ui.data_mut(|d| d.insert_temp(id, r.is_pointer_button_down_on() || r.dragged()));
    r.changed() && !was
}

impl EditorApp {
    pub(super) fn props_panel(&mut self, ui: &mut egui::Ui) {
        let Some((ti, i)) = self.selection() else {
            ui.label(RichText::new("Clip").family(bold()).size(17.0).color(TEXT));
            ui.add_space(6.0);
            ui.label(RichText::new("Tocá un clip en la línea de tiempo para ajustar su imagen, su velocidad, sus efectos y su sonido.").color(MUTED));
            ui.add_space(12.0);
            let text = format!("Ayuda y atajos ({})", shortcuts::short(&self.prefs.keys, Act::Shortcuts));
            if button(ui, &text, Kind::Ghost).clicked() {
                self.open_help(HelpTab::Guide);
            }
            return;
        };
        let p = self.tracks[ti].clips[i].clone();
        let video = self.selected_video();
        ui.add(egui::Label::new(RichText::new(library::pretty_name(&p.path)).family(bold()).size(17.0).color(TEXT)).truncate())
            .on_hover_text(p.path.display().to_string());
        let mut meta = vec![if p.still() { format!("Imagen congelada · {}", fmt_time(p.len)) } else { fmt_time(p.len) }];
        if let (Some(_), Some(pr)) = (video, self.loader.probe(&p.path)) {
            meta.push(format!("{}×{} · {:.0} fps · {}", pr.width, pr.height, pr.fps(), pr.codec()));
        }
        ui.label(RichText::new(meta.join("  ·  ")).size(12.5).color(MUTED));
        ui.add_space(16.0);

        if let Some((tv, iv)) = video {
            self.image_section(ui, self.tracks[tv].clips[iv].frame, &p.path);
            ui.add_space(18.0);
        }
        if p.still() {
            caption(ui, "Duración");
            ui.label(RichText::new("Arrastrá su borde en la línea de tiempo para que dure más o menos.").size(13.0).color(MUTED));
            return;
        }
        self.speed_section(ui, p.speed);
        ui.add_space(18.0);
        if let Some((tv, iv)) = video {
            self.effects_section(ui, self.tracks[tv].clips[iv].blur);
            ui.add_space(18.0);
        }
        self.sound_section(ui, &p.path);
    }

    fn image_section(&mut self, ui: &mut egui::Ui, frame: Frame, path: &std::path::Path) {
        caption(ui, "Imagen");
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            if icon_button(ui, Glyph::Rotate, "Girar", Kind::Secondary, egui::Vec2::ZERO).on_hover_text("Girar un cuarto de vuelta a la derecha").clicked() {
                self.edit_frame(|f| f.turns = (f.turns + 1) % 4);
            }
            if icon_button(ui, Glyph::Flip, "Espejo", Kind::Secondary, egui::Vec2::ZERO).on_hover_text("Dar vuelta de izquierda a derecha").clicked() {
                self.edit_frame(|f| f.flip = !f.flip);
            }
            if icon_button(ui, Glyph::Crop, "Recortar", Kind::Secondary, egui::Vec2::ZERO).on_hover_text("Elegir qué parte de la imagen se ve").clicked() {
                self.start_crop();
            }
        });
        // Entera o llenando solo cambia algo si la imagen no tiene la forma del lienzo.
        let canvas = self.canvas_shape();
        let shape = self.loader.probe(path).map(|p| frame.view_size((p.width as f32, p.height as f32)));
        if shape.is_some_and(|(w, h)| (w / h - canvas.0 / canvas.1).abs() > 0.01) {
            ui.add_space(8.0);
            let mut fill = frame.fill;
            if seg(ui, &mut fill, &[(false, "Entera"), (true, "Llenar")]) {
                self.edit_frame(|f| f.fill = fill);
            }
        }
        if frame != Frame::default() {
            ui.add_space(4.0);
            if button(ui, "Restablecer imagen", Kind::Ghost).clicked() {
                self.edit_frame(|f| *f = Frame::default());
            }
        }
    }

    fn speed_section(&mut self, ui: &mut egui::Ui, speed: f64) {
        caption(ui, "Velocidad");
        let mut pick = (speed * 100.0).round() as u32;
        let labels: Vec<(u32, String)> = SPEEDS.iter().map(|&s| ((s * 100.0) as u32, fmt_speed(s))).collect();
        let opts: Vec<(u32, &str)> = labels.iter().map(|(v, l)| (*v, l.as_str())).collect();
        if seg(ui, &mut pick, &opts) {
            self.push_undo();
            self.set_speed(pick as f64 / 100.0);
        }
        ui.add_space(4.0);
        let mut s = speed;
        ui.spacing_mut().slider_width = ui.available_width() - 70.0;
        let r = ui.add(egui::Slider::new(&mut s, SPEED_MIN..=SPEED_MAX).logarithmic(true).custom_formatter(|v, _| fmt_speed(v)).custom_parser(|t| t.trim_end_matches('×').trim().replace(',', ".").parse().ok()));
        if starts_edit(ui, &r) {
            self.push_undo();
        }
        if r.changed() {
            self.set_speed(s);
        }
        if speed != 1.0 && self.selected_audio().is_some() {
            ui.label(RichText::new("El sonido sube o baja de tono con la velocidad.").size(12.5).color(MUTED));
        }
    }

    fn effects_section(&mut self, ui: &mut egui::Ui, blur: f32) {
        caption(ui, "Efectos");
        let mut on = blur > 0.0;
        let switched = ui
            .horizontal(|ui| {
                let r = toggle(ui, &mut on).on_hover_text("Mezcla cada cuadro con los que siguen: el movimiento deja una estela suave");
                ui.label(RichText::new("Desenfoque de movimiento").color(if on { TEXT } else { MUTED }));
                r.changed()
            })
            .inner;
        if switched {
            self.push_undo();
            self.set_blur(if on { BLUR_ON } else { 0.0 });
            return;
        }
        if !on {
            return;
        }
        let mut pct = (blur * 100.0).round() as u32;
        ui.spacing_mut().slider_width = ui.available_width() - 70.0;
        let r = ui.add(egui::Slider::new(&mut pct, 10..=100).suffix(" %"));
        if starts_edit(ui, &r) {
            self.push_undo();
        }
        if r.changed() {
            self.set_blur(pct as f32 / 100.0);
        }
        ui.label(RichText::new("Se nota más con el clip acelerado o exportando a menos cuadros por segundo.").size(12.5).color(MUTED));
    }

    fn sound_section(&mut self, ui: &mut egui::Ui, path: &std::path::Path) {
        caption(ui, "Sonido");
        let Some((ta, ia)) = self.selected_audio() else {
            let why = if self.has_audio(path) { "Está separado: lo encontrás en una pista de audio." } else { "Este clip no tiene sonido." };
            ui.label(RichText::new(why).size(13.0).color(MUTED));
            return;
        };
        let p = &self.tracks[ta].clips[ia];
        let (mut on, mut vol) = (!p.muted, (p.gain * 100.0).round() as u32);
        let mut changed = false;
        ui.horizontal(|ui| {
            let key = shortcuts::short(&self.prefs.keys, Act::MuteClip);
            changed |= toggle(ui, &mut on).on_hover_text(format!("{} ({key})", if on { "Silenciar" } else { "Activar el sonido" })).changed();
            ui.label(RichText::new(if on { "Con sonido" } else { "Silenciado" }).color(if on { TEXT } else { MUTED }));
        });
        let mut start = changed;
        ui.add_enabled_ui(on, |ui| {
            ui.spacing_mut().slider_width = ui.available_width() - 70.0;
            let r = ui.add(egui::Slider::new(&mut vol, 0..=200).suffix(" %"));
            changed |= r.changed();
            start |= starts_edit(ui, &r);
        });
        if changed {
            if start {
                self.push_undo();
            }
            let c = &mut self.tracks[ta].clips[ia];
            c.muted = !on;
            c.gain = vol as f32 / 100.0;
        }
    }

    /// Forma del lienzo (ancho, alto): la proporción elegida o la del primer clip.
    pub(super) fn canvas_shape(&self) -> (f32, f32) {
        let first = self.edit().first_video().and_then(|c| self.loader.probe(&c.path)).map_or((1920, 1080), |p| (p.width, p.height));
        let (w, h) = media::canvas_size(first, self.ratio, 0);
        (w as f32, h as f32)
    }
}
