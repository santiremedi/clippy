//! Vista previa: el lienzo con la proporción elegida y cada imagen recortada, girada y ubicada como se va a
//! exportar; abajo, la proporción. Desde el panel del clip se entra a recortar su imagen acá mismo.

use eframe::egui::text::LayoutJob;
use eframe::egui::{self, Align, Align2, Color32, CursorIcon, FontId, Layout, Pos2, Rect, Sense, Shape, Stroke, Vec2};

use crate::ui_kit::theme::*;
use super::media::{Frame, Ratio};
use super::EditorApp;

/// Proporciones del lienzo que se pueden elegir (None = la del primer clip).
pub(super) const RATIOS: [(Option<Ratio>, &str); 7] = [
    (None, "Original"),
    (Some((16, 9)), "16:9"),
    (Some((9, 16)), "9:16"),
    (Some((1, 1)), "1:1"),
    (Some((4, 5)), "4:5"),
    (Some((4, 3)), "4:3"),
    (Some((21, 9)), "21:9"),
];

/// Alto de la barra de abajo de la vista previa.
const BAR_H: f32 = 34.0;
/// Agarradera de un borde o una esquina del recorte.
const CROP_GRIP: f32 = 14.0;
/// Lo mínimo que puede quedar del recorte (px de pantalla).
const CROP_MIN: f32 = 32.0;

/// Recortando la imagen de una pieza.
pub(super) struct CropEdit {
    pub(super) id: u64,
    /// El recorte sobre la imagen girada y espejada, de 0 a 1 (izquierda, arriba, derecha, abajo).
    rect: [f32; 4],
    /// Proporción fija elegida (índice en `crop_shapes`), o libre.
    shape: usize,
    /// Mientras se arrastra: qué bordes se mueven (izquierda, arriba, derecha, abajo) y cómo estaba el recorte.
    drag: Option<([bool; 4], [f32; 4])>,
}

/// Proporciones del recorte: libre, la del lienzo y algunas fijas.
fn crop_shapes(canvas: (f32, f32)) -> [(&'static str, Option<f32>); 5] {
    [("Libre", None), ("Lienzo", Some(canvas.0 / canvas.1)), ("16:9", Some(16.0 / 9.0)), ("9:16", Some(9.0 / 16.0)), ("1:1", Some(1.0))]
}

/// `r` en el rectángulo de pantalla `area` (de 0 a 1 -> px).
fn to_screen(area: Rect, r: [f32; 4]) -> Rect {
    Rect::from_min_max(area.lerp_inside(Vec2::new(r[0], r[1])), area.lerp_inside(Vec2::new(r[2], r[3])))
}

/// La textura en `dst` (pantalla) con el punto del archivo que va en cada esquina (`uv`, como en `Placed`).
fn image_mesh(tex: egui::TextureId, dst: Rect, uv: [[f32; 2]; 4], alpha: f32) -> Shape {
    let mut m = egui::Mesh::with_texture(tex);
    let col = Color32::WHITE.gamma_multiply(alpha);
    for (pos, uv) in [dst.left_top(), dst.right_top(), dst.right_bottom(), dst.left_bottom()].into_iter().zip(uv) {
        m.vertices.push(egui::epaint::Vertex { pos, uv: Pos2::new(uv[0], uv[1]), color: col });
    }
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    Shape::mesh(m)
}

/// El recorte más grande con forma `aspect` (ancho / alto, en px) dentro de una imagen de `size` px, centrado.
fn biggest(aspect: f32, size: (f32, f32)) -> [f32; 4] {
    let (w, h) = if size.0 / size.1 > aspect { (aspect * size.1 / size.0, 1.0) } else { (1.0, size.0 / aspect / size.1) };
    [(1.0 - w) / 2.0, (1.0 - h) / 2.0, (1.0 + w) / 2.0, (1.0 + h) / 2.0]
}

impl EditorApp {
    pub(super) fn preview(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap();
        let area = Rect::from_min_max(full.min, Pos2::new(full.right(), full.bottom() - BAR_H - 8.0));
        let bar = Rect::from_min_max(Pos2::new(full.left(), area.bottom() + 8.0), full.max);
        if self.crop.as_ref().is_some_and(|c| self.find(c.id).is_none()) {
            self.crop = None;
        }
        if self.crop.is_some() {
            self.crop_view(ui, area, bar);
        } else {
            self.canvas_view(ui, area);
            self.ratio_bar(ui, bar);
        }
    }

    fn canvas_view(&mut self, ui: &mut egui::Ui, area: Rect) {
        let resp = ui.interact(area, ui.id().with("canvas"), Sense::click());
        let p = ui.painter_at(area);
        let (cw, ch) = self.canvas_shape();
        let scale = (area.width() / cw).min(area.height() / ch);
        let canvas = Rect::from_center_size(area.center(), Vec2::new(cw, ch) * scale);
        p.rect_filled(canvas, 6.0, Color32::BLACK);
        if self.is_empty() {
            p.text(area.center() - egui::vec2(0.0, 12.0), Align2::CENTER_CENTER, "Tu video aparece acá", FontId::proportional(20.0), TEXT);
            p.text(area.center() + egui::vec2(0.0, 16.0), Align2::CENTER_CENTER, "Elegí un clip de la biblioteca para empezar", FontId::proportional(14.0), MUTED);
            return;
        }
        if let Some(e) = self.preview_error() {
            let mut job = LayoutJob::simple(e, FontId::proportional(14.0), MUTED, (canvas.width() - 48.0).clamp(120.0, 480.0));
            job.halign = Align::Center;
            let g = p.layout_job(job);
            p.galley(Pos2::new(canvas.center().x, canvas.center().y - g.size().y / 2.0), g, MUTED);
        } else {
            // Cada imagen como se va a exportar; donde dos piezas se funden, la de arriba encima con su transparencia.
            let clip = p.with_clip_rect(canvas.intersect(p.clip_rect()));
            for l in &self.layers {
                let Some(frame) = self.find(l.piece()).map(|(ti, i)| self.tracks[ti].clips[i].frame) else { continue };
                let s = l.tex.size_vec2();
                if let Some(pl) = frame.place((s.x, s.y), (cw, ch)) {
                    let dst = Rect::from_min_max(canvas.min + Vec2::new(pl.dst[0], pl.dst[1]) * scale, canvas.min + Vec2::new(pl.dst[2], pl.dst[3]) * scale);
                    clip.add(image_mesh(l.tex.id(), dst, pl.uv, l.alpha));
                }
            }
        }
        if resp.clicked() {
            self.toggle_play();
        }
    }

    /// Barra de abajo: la proporción del lienzo.
    fn ratio_bar(&mut self, ui: &mut egui::Ui, bar: Rect) {
        let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(bar).layout(Layout::right_to_left(Align::Center)));
        let current = RATIOS.iter().find(|r| r.0 == self.ratio).map_or("Original", |r| r.1);
        let r = button(&mut ui, current, Kind::Secondary).on_hover_text("Forma del video: horizontal, vertical, cuadrado…");
        let menu = ui.id().with("ratio_menu");
        if r.clicked() {
            ui.memory_mut(|m| m.toggle_popup(menu));
        }
        let mut pick = None;
        let first = self.edit().first_video().and_then(|c| self.loader.probe(&c.path)).map(|p| (p.width, p.height));
        egui::popup_above_or_below_widget(&ui, menu, &r, egui::AboveOrBelow::Above, egui::PopupCloseBehavior::CloseOnClick, |ui| {
            ui.set_min_width(170.0);
            for (ratio, label) in RATIOS {
                let tip = match (ratio, first) {
                    (None, Some((w, h))) => format!("Como el primer clip ({w}×{h})"),
                    (None, None) => "Como el primer clip".to_string(),
                    (Some((w, h)), _) => format!("{w} de ancho por {h} de alto"),
                };
                if ui.add(egui::Button::new(label).selected(ratio == self.ratio)).on_hover_text(tip).clicked() {
                    pick = Some(ratio);
                }
            }
        });
        if let Some(r) = pick {
            self.set_ratio(r);
        }
        ui.label(egui::RichText::new("Proporción").size(13.0).color(MUTED));
    }

    /// Empieza a recortar la imagen de lo elegido, con el cursor sobre esa pieza.
    pub(super) fn start_crop(&mut self) {
        let Some((ti, i)) = self.selected_video() else { return };
        let p = &self.tracks[ti].clips[i];
        let [l, t, r, b] = p.frame.crop;
        let (a, z) = (p.frame.view(l, t), p.frame.view(r, b));
        let rect = [a[0].min(z[0]), a[1].min(z[1]), a[0].max(z[0]), a[1].max(z[1])];
        let (id, start, end) = (p.id, p.start, p.end());
        if !(start..end).contains(&self.pos()) {
            self.seek(start);
        }
        self.pause();
        self.crop = Some(CropEdit { id, rect, shape: 0, drag: None });
    }

    fn crop_view(&mut self, ui: &mut egui::Ui, area: Rect, bar: Rect) {
        let Some(mut c) = self.crop.take() else { return };
        let Some((ti, i)) = self.find(c.id) else { return };
        let frame = self.tracks[ti].clips[i].frame;
        // La imagen entera (sin recortar), girada y espejada como se va a ver.
        let whole = Frame { crop: [0.0, 0.0, 1.0, 1.0], fill: false, ..frame };
        let p = ui.painter_at(area);
        let layer = self.layers.iter().find(|l| l.piece() == c.id);
        let Some(l) = layer else {
            p.text(area.center(), Align2::CENTER_CENTER, "Llevá el cursor a un lugar donde se vea este clip", FontId::proportional(14.0), MUTED);
            self.crop_bar(ui, bar, c, (16.0, 9.0));
            return;
        };
        let s = l.tex.size_vec2();
        let (vw, vh) = whole.view_size((s.x, s.y));
        let k = (area.width() / vw).min(area.height() / vh);
        let img = Rect::from_center_size(area.center(), Vec2::new(vw, vh) * k);
        p.add(image_mesh(l.tex.id(), img, [whole.source(0.0, 0.0), whole.source(1.0, 0.0), whole.source(1.0, 1.0), whole.source(0.0, 1.0)], 1.0));

        // Arrastrar un borde, una esquina o todo el recuadro.
        let resp = ui.interact(img.expand(CROP_GRIP), ui.id().with("crop"), Sense::drag());
        let shown = to_screen(img, c.rect);
        let edges_at = |h: Pos2| -> Option<[bool; 4]> {
            let near = |a: f32, b: f32| (a - b).abs() <= CROP_GRIP;
            let inside_y = (shown.top() - CROP_GRIP..=shown.bottom() + CROP_GRIP).contains(&h.y);
            let inside_x = (shown.left() - CROP_GRIP..=shown.right() + CROP_GRIP).contains(&h.x);
            let e = [near(h.x, shown.left()) && inside_y, near(h.y, shown.top()) && inside_x, near(h.x, shown.right()) && inside_y, near(h.y, shown.bottom()) && inside_x];
            if e.iter().any(|&x| x) {
                Some(e)
            } else if shown.contains(h) {
                Some([true; 4])
            } else {
                None
            }
        };
        if resp.drag_started() {
            c.drag = resp.interact_pointer_pos().and_then(edges_at).map(|e| (e, c.rect));
        }
        if let (Some((e, from)), true) = (c.drag, resp.dragged()) {
            let d = resp.interact_pointer_pos().unwrap_or_default() - ui.input(|i| i.pointer.press_origin()).unwrap_or_default();
            let (dx, dy) = (d.x / img.width(), d.y / img.height());
            let aspect = crop_shapes(self.canvas_shape())[c.shape].1;
            c.rect = drag_crop(from, e, (dx, dy), (CROP_MIN / img.width(), CROP_MIN / img.height()), aspect.map(|a| a * vh / vw));
        }
        if resp.drag_stopped() {
            c.drag = None;
        }
        let hover = resp.hover_pos().or(resp.interact_pointer_pos()).and_then(|h| c.drag.map(|d| d.0).or_else(|| edges_at(h)));
        if let Some(e) = hover {
            ui.ctx().set_cursor_icon(match e {
                [true, true, true, true] => CursorIcon::Move,
                [true, true, false, false] | [false, false, true, true] => CursorIcon::ResizeNwSe,
                [true, false, false, true] | [false, true, true, false] => CursorIcon::ResizeNeSw,
                [true, false, false, false] | [false, false, true, false] => CursorIcon::ResizeHorizontal,
                _ => CursorIcon::ResizeVertical,
            });
        }

        // Lo que queda afuera, oscuro; el recuadro con sus tercios y sus esquinas.
        let r = to_screen(img, c.rect);
        for out in [
            Rect::from_min_max(img.min, Pos2::new(img.right(), r.top())),
            Rect::from_min_max(Pos2::new(img.left(), r.bottom()), img.max),
            Rect::from_min_max(Pos2::new(img.left(), r.top()), Pos2::new(r.left(), r.bottom())),
            Rect::from_min_max(Pos2::new(r.right(), r.top()), Pos2::new(img.right(), r.bottom())),
        ] {
            p.rect_filled(out, 0.0, SCRIM);
        }
        let thin = Stroke::new(1.0_f32, WHITE.gamma_multiply(0.35));
        for k in [1.0, 2.0] {
            p.vline(r.left() + r.width() * k / 3.0, r.y_range(), thin);
            p.hline(r.x_range(), r.top() + r.height() * k / 3.0, thin);
        }
        p.rect_stroke(r, 0.0, Stroke::new(1.5_f32, WHITE));
        let arm = 14.0_f32.min(r.width() / 3.0).min(r.height() / 3.0);
        for (corner, dx, dy) in [(r.left_top(), 1.0, 1.0), (r.right_top(), -1.0, 1.0), (r.right_bottom(), -1.0, -1.0), (r.left_bottom(), 1.0, -1.0)] {
            p.add(Shape::line(vec![corner + egui::vec2(arm * dx, 0.0), corner, corner + egui::vec2(0.0, arm * dy)], Stroke::new(3.5_f32, WHITE)));
        }
        self.crop_bar(ui, bar, c, (vw, vh));
    }

    /// Barra de abajo mientras se recorta: la forma del recorte y terminar. Enter aplica y Esc cancela.
    fn crop_bar(&mut self, ui: &mut egui::Ui, bar: Rect, mut c: CropEdit, size: (f32, f32)) {
        let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(bar).layout(Layout::left_to_right(Align::Center)));
        ui.spacing_mut().item_spacing.x = 4.0;
        let shapes = crop_shapes(self.canvas_shape());
        for (k, (label, aspect)) in shapes.iter().enumerate() {
            let kind = if c.shape == k { Kind::Selected } else { Kind::Secondary };
            if button(&mut ui, label, kind).clicked() && c.shape != k {
                c.shape = k;
                if let Some(a) = aspect {
                    c.rect = biggest(*a, size);
                }
            }
        }
        let (mut done, mut cancel) = ui.input(|i| (i.key_pressed(egui::Key::Enter), i.key_pressed(egui::Key::Escape)));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            done |= button(ui, "Listo", Kind::Secondary).on_hover_text("Enter").clicked();
            cancel |= button(ui, "Cancelar", Kind::Ghost).on_hover_text("Esc").clicked();
            if button(ui, "Ver todo", Kind::Ghost).on_hover_text("Volver a la imagen entera").clicked() {
                c.rect = [0.0, 0.0, 1.0, 1.0];
                c.shape = 0;
            }
        });
        if done {
            let id = c.id;
            if let Some((ti, i)) = self.find(id) {
                let f = self.tracks[ti].clips[i].frame;
                let whole = Frame { crop: [0.0, 0.0, 1.0, 1.0], ..f };
                let (a, z) = (whole.source(c.rect[0], c.rect[1]), whole.source(c.rect[2], c.rect[3]));
                let crop = [a[0].min(z[0]), a[1].min(z[1]), a[0].max(z[0]), a[1].max(z[1])];
                self.selected = Some(id);
                self.edit_frame(|f| f.crop = crop);
            }
        } else if !cancel {
            self.crop = Some(c);
        }
    }
}

/// El recorte `from` con los bordes `e` (izquierda, arriba, derecha, abajo) corridos (`dx`, `dy`), sin salirse de la
/// imagen ni quedar más chico que `min`. Con `aspect` (ancho / alto del recorte en fracciones de la imagen) la forma
/// se mantiene.
fn drag_crop(from: [f32; 4], e: [bool; 4], (dx, dy): (f32, f32), min: (f32, f32), aspect: Option<f32>) -> [f32; 4] {
    let [l, t, r, b] = from;
    if e == [true; 4] {
        // Todo el recuadro, sin salirse.
        let dx = dx.clamp(-l, 1.0 - r);
        let dy = dy.clamp(-t, 1.0 - b);
        return [l + dx, t + dy, r + dx, b + dy];
    }
    let mut n = from;
    if e[0] {
        n[0] = (l + dx).clamp(0.0, r - min.0);
    }
    if e[2] {
        n[2] = (r + dx).clamp(l + min.0, 1.0);
    }
    if e[1] {
        n[1] = (t + dy).clamp(0.0, b - min.1);
    }
    if e[3] {
        n[3] = (b + dy).clamp(t + min.1, 1.0);
    }
    let Some(a) = aspect else { return n };
    // Con la forma fija manda el ancho (o el alto, si solo se mueve un borde de arriba o de abajo); el otro lado
    // crece desde el borde quieto (o desde el centro) y, si no entra, se achican los dos.
    let horizontal = e[0] || e[2];
    let (mut w, mut h) = (n[2] - n[0], n[3] - n[1]);
    if horizontal {
        h = w / a;
    } else {
        w = h * a;
    }
    let k = (1.0f32).min(1.0 / w).min(1.0 / h);
    let (w, h) = (w * k, h * k);
    let x0 = match (e[0], e[2]) {
        (true, false) => n[2] - w,
        (false, true) => n[0],
        _ => (n[0] + n[2] - w) / 2.0,
    };
    let y0 = match (e[1], e[3]) {
        (true, false) => n[3] - h,
        (false, true) => n[1],
        _ => (n[1] + n[3] - h) / 2.0,
    };
    let x0 = x0.clamp(0.0, 1.0 - w);
    let y0 = y0.clamp(0.0, 1.0 - h);
    [x0, y0, x0 + w, y0 + h]
}
