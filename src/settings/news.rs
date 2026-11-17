//! Novedades: el texto del release de GitHub, en un modal. Aparece una vez después de actualizar
//! ("Novedades de la …") y cuando se quiere ver qué trae la versión nueva antes de instalarla.

use eframe::egui::{self, Align, Layout, RichText};

use super::SettingsApp;
use crate::app::update;
use crate::ui_kit::theme::*;

const WIDTH: f32 = 520.0;
/// Alto máximo del texto: si el release es largo, se desplaza dentro del modal.
const MAX_TEXT_H: f32 = 360.0;
const BUTTON: egui::Vec2 = egui::vec2(120.0, 38.0);

impl SettingsApp {
    pub(super) fn news_modals(&mut self, ctx: &egui::Context) {
        if let Some((v, text)) = self.news.clone() {
            let mut done = false;
            let x = notes_modal(ctx, "news", &format!("Novedades de la {v}"), "Clippy se actualizó. Esto es lo que cambió.", &text, |ui| {
                done = button_sized(ui, "Listo", Kind::Primary, BUTTON).clicked();
            });
            if x || done {
                self.news = None;
                update::news_seen();
            }
        } else if let Some((v, text)) = self.preview.clone() {
            let (mut install, mut later) = (false, false);
            let x = notes_modal(ctx, "preview", &format!("Qué trae la {v}"), &format!("Tenés la {}.", update::current()), &text, |ui| {
                install = button_sized(ui, "Actualizar", Kind::Primary, BUTTON).clicked();
                later = button_sized(ui, "Ahora no", Kind::Secondary, BUTTON).clicked();
            });
            if install {
                self.start_update();
            }
            if x || install || later {
                self.preview = None;
            }
        }
    }
}

/// Modal con el texto de un release. `buttons` se agregan de derecha a izquierda. Devuelve true si se
/// tocó la X o Esc.
fn notes_modal(ctx: &egui::Context, id: &str, title: &str, subtitle: &str, text: &str, buttons: impl FnOnce(&mut egui::Ui)) -> bool {
    modal(ctx, id, title, subtitle, WIDTH, |ui| {
        modal_body(ui, |ui| {
            egui::ScrollArea::vertical().max_height(MAX_TEXT_H).auto_shrink([false, true]).show(ui, |ui| release_notes(ui, text));
        });
        modal_footer(ui, |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), buttons);
        });
    })
}

/// Markdown del release con lo justo: títulos (`#`), viñetas (`-`, `*`) y párrafos, sin los adornos
/// (`**`, `` ` ``). La línea "Full Changelog" que agrega GitHub no le sirve a quien usa la app.
fn release_notes(ui: &mut egui::Ui, text: &str) {
    ui.spacing_mut().item_spacing.y = 6.0;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("**Full Changelog**") {
            continue;
        }
        if let Some(h) = line.strip_prefix('#') {
            ui.add_space(4.0);
            ui.label(RichText::new(plain(h.trim_start_matches('#').trim())).family(bold()).size(15.0).color(TEXT));
        } else if let Some(item) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            ui.horizontal_top(|ui| {
                ui.label(RichText::new("•").color(MUTED));
                ui.add(egui::Label::new(RichText::new(plain(item)).color(TEXT)).wrap());
            });
        } else {
            ui.add(egui::Label::new(RichText::new(plain(line)).color(MUTED)).wrap());
        }
    }
}

fn plain(s: &str) -> String {
    s.replace("**", "").replace('`', "")
}
