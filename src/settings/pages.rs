//! Páginas de la ventana principal: navegación, Inicio (grabación y audio), Atajos y General.

use std::time::SystemTime;

use eframe::egui::{self, Align, Layout, Margin, RichText};

use crate::app::autostart;
use crate::app::keys::hotkey_name;
use crate::app::mode::Mode;
use crate::app::update;
use crate::config::{CaptureMode, GlobalAction, MAX_MBPS, MAX_SECONDS, MIN_MBPS};

/// Calidades de video de un toque (Mbps): baja, media y alta.
const QUALITY: [u32; 3] = [15, 30, 60];
use crate::log::app_dir;
use crate::record::audio;
use crate::ui_kit::sound;
use crate::ui_kit::theme::*;
use crate::ui_kit::window::open_folder;

use super::{Page, SettingsApp, CAPTURE_HINT, LABEL_W};

pub(super) fn page_title(ui: &mut egui::Ui, title: &str, sub: &str) {
    ui.label(RichText::new(title).family(bold()).size(26.0).color(TEXT));
    ui.label(RichText::new(sub).color(MUTED));
    ui.add_space(14.0);
}

impl SettingsApp {
    /// Barra lateral: logo, estado de la grabación, secciones y acceso al editor.
    pub(super) fn nav(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            app_mark(ui, 28.0);
            ui.label(RichText::new("Clippy").family(bold()).size(20.0).color(TEXT));
        });
        ui.add_space(16.0);

        egui::Frame::none().fill(BG).stroke(egui::Stroke::new(1.0_f32, LINE)).rounding(8.0).inner_margin(Margin::same(12.0)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if self.main_running {
                ui.horizontal(|ui| {
                    let (txt, col) = if self.cfg.enabled { ("Grabando", TEXT) } else { ("En pausa", MUTED) };
                    let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.0, col);
                    ui.label(RichText::new(txt).family(bold()).color(col));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        toggle(ui, &mut self.cfg.enabled).on_hover_text(if self.cfg.enabled { "Pausar" } else { "Reanudar" });
                    });
                });
                let sub = if self.cfg.enabled {
                    format!("{} guarda los últimos {}", hotkey_name(self.cfg.hotkey_vk, self.cfg.hotkey_mods), fmt_secs(self.cfg.seconds))
                } else {
                    "No se está guardando nada".into()
                };
                ui.label(RichText::new(sub).size(12.5).color(MUTED));
            } else {
                ui.label(RichText::new("La grabadora está cerrada").color(TEXT));
                ui.add_space(4.0);
                if wide_button(ui, "Iniciar grabadora", Kind::Primary, 34.0).clicked() {
                    Mode::Tray { background: false }.launch();
                }
            }
        });
        ui.add_space(16.0);

        for (page, label) in [
            (Page::Home, "Inicio"),
            (Page::Keys, "Atajos"),
            (Page::General, "General"),
        ] {
            if nav_item(ui, label, self.page == page).clicked() {
                self.page = page;
                self.end_capture();
            }
        }

        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            if wide_button(ui, "Salir de Clippy", Kind::Danger, 36.0).on_hover_text("Deja de grabar y cierra todo").clicked() {
                self.confirm_quit = true;
            }
            ui.add_space(14.0);
            divider(ui);
            ui.add_space(14.0);
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                let w = ui.available_width();
                if icon_button(ui, Glyph::Folder, "Carpeta de clips", Kind::Secondary, egui::vec2(w, 36.0)).clicked() {
                    open_folder(&self.cfg.output_dir);
                }
                // Un solo botón principal: con la grabadora cerrada, lo principal es abrirla.
                let editor = if self.main_running { Kind::Primary } else { Kind::Secondary };
                if icon_button(ui, Glyph::Film, "Abrir editor", editor, egui::vec2(w, 42.0)).clicked() {
                    Mode::Editor(None).launch();
                }
                self.update_button(ui);
            });
        });
    }

    pub(super) fn home_page(&mut self, ui: &mut egui::Ui) {
        page_title(ui, "Inicio", "Cómo se graban tus clips.");
        // Lo primero que se busca: con qué tecla se guarda un clip. Va arriba y se cambia ahí mismo.
        self.clip_key_hero(ui);
        ui.add_space(12.0);
        self.editor_hero(ui);
        ui.add_space(18.0);
        self.recording_section(ui);
        self.audio_section(ui);
    }

    /// Versión nueva: un botón más al pie de la barra lateral, del tamaño de los otros (como en
    /// Discord o Medal), no un cartel. La bandeja ya avisa con una notificación.
    fn update_button(&mut self, ui: &mut egui::Ui) {
        let Some((v, _)) = self.update.clone() else { return };
        let busy = self.update_busy();
        // El pie se arma de abajo hacia arriba: lo primero que se agrega queda abajo del botón.
        if let Some(Err(e)) = self.update_status.lock().unwrap().clone() {
            ui.add(egui::Label::new(RichText::new(format!("No se pudo actualizar: {e}")).size(12.0).color(DANGER)).wrap());
        }
        let label = if busy { format!("Bajando la {v}…") } else { format!("Actualizar a {v}") };
        let tip = format!("Tenés la {}.", update::current());
        let size = egui::vec2(ui.available_width(), 36.0);
        let r = ui.add_enabled_ui(!busy, |ui| icon_button(ui, Glyph::Download, &label, Kind::Secondary, size)).inner;
        // Primero se confirma, viendo qué trae la versión nueva.
        if r.on_hover_text(&tip).on_disabled_hover_text(&tip).clicked() {
            self.confirm_update = Some((v.clone(), update::notes(&v)));
        }
    }

    /// Tarjeta principal: la tecla para guardar clips, dibujada como una tecla que se toca para cambiar.
    fn clip_key_hero(&mut self, ui: &mut egui::Ui) {
        let a = GlobalAction::SaveClip;
        let (vk, mods) = self.cfg.hotkey(a);
        let waiting = self.capturing == Some(a);
        hero_card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 22.0;
                    let text = if waiting { "Apretá…".to_string() } else { hotkey_name(vk, mods) };
                    if keycap(ui, &text, waiting).on_hover_text("Tocá para cambiar la tecla").clicked() {
                        self.start_capture(a);
                    }
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        ui.add_space(4.0);
                        ui.label(RichText::new("Tu tecla para guardar clips").family(bold()).size(20.0).color(TEXT));
                        let sub = if waiting {
                            format!("Apretá la tecla nueva. {CAPTURE_HINT}")
                        } else {
                            format!("Apretá {} en cualquier momento, incluso en el juego, y se guardan los últimos {}.", hotkey_name(vk, mods), fmt_secs(self.cfg.seconds))
                        };
                        ui.add(egui::Label::new(RichText::new(sub).color(MUTED)).wrap());
                        let note = match &self.global_note {
                            Some(n) if !waiting => n.clone(),
                            _ => "Tocá la tecla para cambiarla.".to_string(),
                        };
                        ui.label(RichText::new(note).size(12.5).color(MUTED));
                    });
                });
            });
    }

    /// Tarjeta destacada para abrir el editor.
    fn editor_hero(&mut self, ui: &mut egui::Ui) {
        hero_card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Editor de clips").family(bold()).size(17.0).color(TEXT));
                        ui.label(RichText::new("Recortá, uní y exportá tus jugadas en un solo video.").color(MUTED));
                        let info = match self.clips {
                            (0, _) => "Todavía no hay clips guardados.".to_string(),
                            (n, last) => format!(
                                "{} {} · el último {}",
                                fmt_count(n),
                                if n == 1 { "clip guardado" } else { "clips guardados" },
                                last.map(ago).unwrap_or_default()
                            ),
                        };
                        ui.label(RichText::new(info).size(12.5).color(MUTED));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon_button(ui, Glyph::Film, "Abrir editor", Kind::Secondary, egui::vec2(150.0, 40.0)).clicked() {
                            Mode::Editor(None).launch();
                        }
                    });
                });
            });
    }

    pub(super) fn about_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "Acerca de", |ui| {
            row(ui, "Versión", LABEL_W, |ui| {
                ui.label(RichText::new(format!("Clippy {}", env!("CARGO_PKG_VERSION"))).color(TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| match self.update.clone() {
                    Some((v, _)) => {
                        let busy = self.update_busy();
                        let label = if busy { "Actualizando…".to_string() } else { format!("Actualizar a {v}") };
                        if ui.add_enabled_ui(!busy, |ui| button(ui, &label, Kind::Secondary)).inner.clicked() {
                            self.start_update();
                        }
                    }
                    None => {
                        let label = if self.checking { "Buscando…" } else { "Buscar actualizaciones" };
                        // Ancho fijo: al pasar a "Buscando…" no se achica ni se corre.
                        let size = egui::vec2(190.0, 0.0);
                        if ui.add_enabled_ui(!self.checking, |ui| button_sized(ui, label, Kind::Secondary, size)).inner.clicked() {
                            self.start_check();
                        }
                    }
                });
            });
            self.finish_check(ui.ctx());
            if let Some(note) = self.check_note.clone() {
                hint(ui, LABEL_W + 8.0, note);
            }
            divider(ui);
            row(ui, "Datos y registro", LABEL_W, |ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icon_button(ui, Glyph::Folder, "Abrir carpeta", Kind::Secondary, egui::Vec2::ZERO).clicked() {
                        open_folder(&app_dir());
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(app_dir().display().to_string()).color(MUTED)).truncate());
                    });
                });
            });
        });
    }

    fn recording_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "Grabación", |ui| {
            row(ui, "Duración del clip", LABEL_W, |ui| {
                ui.spacing_mut().slider_width = (ui.available_width() - 104.0).max(80.0);
                ui.add(
                    egui::Slider::new(&mut self.cfg.seconds, 5..=MAX_SECONDS)
                        .step_by(5.0)
                        .custom_formatter(|v, _| fmt_secs(v as u32)),
                );
            });
            hint(ui, LABEL_W + 8.0, format!("Guarda los últimos {} antes de apretar la tecla.", fmt_secs(self.cfg.seconds)));
            divider(ui);
            row(ui, "Calidad", LABEL_W, |ui| {
                // 0 = "Personalizada": se elige el bitrate a mano.
                let mut q = if self.custom_quality || !QUALITY.contains(&self.cfg.bitrate_mbps) { 0 } else { self.cfg.bitrate_mbps };
                if seg(ui, &mut q, &[(QUALITY[0], "Baja"), (QUALITY[1], "Media"), (QUALITY[2], "Alta"), (0, "Personalizada")]) {
                    self.custom_quality = q == 0;
                    if q != 0 {
                        self.cfg.bitrate_mbps = q;
                    }
                }
            });
            if self.custom_quality || !QUALITY.contains(&self.cfg.bitrate_mbps) {
                row(ui, "", LABEL_W, |ui| {
                    ui.spacing_mut().slider_width = (ui.available_width() - 104.0).max(80.0);
                    ui.add(egui::Slider::new(&mut self.cfg.bitrate_mbps, MIN_MBPS..=MAX_MBPS).suffix(" Mbps"));
                });
                hint(ui, LABEL_W + 8.0, "Más Mbps, mejor imagen y clips más pesados. A 1080p y 60 fps, 40 a 60 ya se ve muy bien; a 1440p o 4K, conviene 80 o más.");
            }
            row(ui, "Cuadros por segundo", LABEL_W, |ui| {
                seg(ui, &mut self.cfg.fps, &[(30, "30"), (60, "60"), (120, "120")]);
            });
            hint(ui, LABEL_W + 8.0, "Video MP4 (H.264): se reproduce en cualquier PC, celular, Discord o WhatsApp.");
            divider(ui);
            row(ui, "Qué se graba", LABEL_W, |ui| {
                seg(ui, &mut self.cfg.capture_mode, &[(CaptureMode::Screen, "Toda la pantalla"), (CaptureMode::GamesOnly, "Solo juegos")]);
            });
            hint(
                ui,
                LABEL_W + 8.0,
                if self.cfg.capture_mode == CaptureMode::GamesOnly {
                    "Graba solo el juego en pantalla completa, en el monitor donde esté. Si lo minimizás o lo tapás con otra ventana, esa parte del clip queda en negro y no se ve tu escritorio."
                } else {
                    "Graba todo lo que se ve en el monitor del juego (sin juego, el de la ventana activa): juegos, escritorio y otras ventanas."
                },
            );
            divider(ui);
            row(ui, "Memoria", LABEL_W, |ui| {
                ui.label(RichText::new(format!("Unos {} MB de RAM mientras graba", self.cfg.typical_ram_mb())).family(bold()).color(TEXT));
            });
            hint(ui, LABEL_W + 8.0, "Aproximado, como lo muestra el Administrador de tareas. Sube un poco con más segundos, más calidad o mucho movimiento en pantalla.");
            divider(ui);
            row(ui, "Carpeta", LABEL_W, |ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if button(ui, "Cambiar", Kind::Secondary).clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .set_title("Carpeta para los clips")
                            .set_directory(&self.cfg.output_dir)
                            .pick_folder()
                        {
                            self.cfg.output_dir = p;
                        }
                    }
                    let path = self.cfg.output_dir.display().to_string();
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(&path).color(MUTED)).truncate()).on_hover_text(path);
                    });
                });
            });
        });
    }

    pub(super) fn global_keys_section(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            for a in GlobalAction::ALL {
                // Guardar clip siempre tiene tecla; los demás se pueden quitar.
                let optional = a != GlobalAction::SaveClip;
                if optional {
                    divider(ui);
                }
                row(ui, a.label(), LABEL_W + 40.0, |ui| {
                    let (vk, mods) = self.cfg.hotkey(a);
                    let capturing = self.capturing == Some(a);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if optional && vk != 0 && !capturing && button(ui, "Quitar", Kind::Ghost).clicked() {
                            let (v, m) = self.cfg.hotkey_mut(a);
                            *v = 0;
                            *m = 0;
                        }
                        let text = if capturing {
                            "Apretá una tecla…".to_string()
                        } else if vk == 0 {
                            "Sin asignar".to_string()
                        } else {
                            hotkey_name(vk, mods)
                        };
                        let kind = if capturing { Kind::Selected } else { Kind::Secondary };
                        let w = ui.available_width();
                        if button_sized(ui, &text, kind, egui::vec2(w, 34.0)).clicked() {
                            self.start_capture(a);
                        }
                    });
                });
            }
            let note = if self.capturing.is_some() {
                CAPTURE_HINT.to_string()
            } else if let Some(n) = &self.global_note {
                n.clone()
            } else {
                "Los atajos del editor se cambian desde el editor, en «Ajustes del editor» (la tuerca).".to_string()
            };
            ui.label(RichText::new(note).size(12.5).color(MUTED));
        });
    }

    fn audio_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "Audio", |ui| {
            volume_row(ui, "Juego", &mut self.cfg.game_audio, &mut self.cfg.game_volume);
            hint(
                ui,
                LABEL_W + 8.0,
                if self.cfg.game_audio {
                    "Se detecta solo: el juego que tengas en pantalla completa. Mientras no haya uno, todo va a Escritorio."
                } else {
                    "Silenciado: el juego no se va a escuchar en los clips."
                },
            );
            divider(ui);
            volume_row(ui, "Escritorio", &mut self.cfg.desktop_audio, &mut self.cfg.desktop_volume);
            hint(ui, LABEL_W + 8.0, "Todo lo demás: Discord, música, navegador.");
            divider(ui);
            row(ui, "Micrófono", LABEL_W, |ui| {
                toggle(ui, &mut self.cfg.mic_audio).on_hover_text(if self.cfg.mic_audio { "Apagar micrófono" } else { "Grabar tu voz" });
                ui.add_space(8.0);
                ui.add_enabled_ui(self.cfg.mic_audio, |ui| {
                    ui.spacing_mut().slider_width = (ui.available_width() - 86.0).max(60.0);
                    ui.add(egui::Slider::new(&mut self.cfg.mic_volume, 0..=200).suffix(" %"));
                });
            });
            if self.cfg.mic_audio {
                row(ui, "", LABEL_W, |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if button(ui, "Actualizar", Kind::Secondary).on_hover_text("Buscar micrófonos conectados").clicked() {
                            self.mics = audio::capture_devices();
                        }
                        let current = if self.cfg.mic_device.is_empty() {
                            "Predeterminado de Windows".to_string()
                        } else {
                            self.mics.iter().find(|(id, _)| *id == self.cfg.mic_device).map(|(_, n)| n.clone()).unwrap_or_else(|| "Micrófono desconectado".into())
                        };
                        let w = ui.available_width();
                        egui::ComboBox::from_id_salt("mic")
                            .width(w - 8.0)
                            .truncate()
                            .selected_text(current.clone())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.cfg.mic_device, String::new(), "Predeterminado de Windows");
                                for (id, name) in self.mics.clone() {
                                    ui.selectable_value(&mut self.cfg.mic_device, id, name);
                                }
                            })
                            .response
                            .on_hover_text(current);
                    });
                });
            }
            hint(
                ui,
                LABEL_W + 8.0,
                if self.cfg.mic_audio { "Tu voz se mezcla en el clip. También lo podés prender o apagar desde el ícono de la bandeja." } else { "Apagado: tu voz no queda en los clips." },
            );

        });
    }

    pub(super) fn general_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "Preferencias", |ui| {
            row(ui, "Sonido al guardar", LABEL_W, |ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    toggle(ui, &mut self.cfg.sound_on_save);
                    if button(ui, "Probar sonido", Kind::Secondary).clicked() {
                        sound::play_saved();
                    }
                });
            });
            divider(ui);
            row(ui, "Iniciar con Windows", LABEL_W, |ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let mut a = self.autostart;
                    if toggle(ui, &mut a).changed() {
                        autostart::set(a);
                        self.autostart = autostart::enabled();
                    }
                });
            });
        });
    }
}

/// Fila de audio: interruptor + volumen + porcentaje.
fn volume_row(ui: &mut egui::Ui, label: &str, on: &mut bool, vol: &mut u32) {
    row(ui, label, LABEL_W, |ui| {
        toggle(ui, on).on_hover_text(if *on { "Silenciar" } else { "Activar" });
        ui.add_space(8.0);
        ui.add_enabled_ui(*on, |ui| {
            ui.spacing_mut().slider_width = (ui.available_width() - 86.0).max(60.0);
            ui.add(egui::Slider::new(vol, 0..=100).suffix(" %"));
        });
    });
}

fn ago(t: SystemTime) -> String {
    let s = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    let hace = |n: u64, one: &str, many: &str| format!("hace {n} {}", if n == 1 { one } else { many });
    match s {
        0..=59 => "hace un momento".into(),
        60..=3599 => format!("hace {} min", s / 60),
        3600..=86_399 => format!("hace {} h", s / 3600),
        86_400..=172_799 => "ayer".into(),
        172_800..=604_799 => hace(s / 86_400, "día", "días"),
        604_800..=2_591_999 => hace(s / 604_800, "semana", "semanas"),
        2_592_000..=31_535_999 => hace(s / 2_592_000, "mes", "meses"),
        _ => hace(s / 31_536_000, "año", "años"),
    }
}
