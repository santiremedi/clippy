//! Editor de clips estilo CapCut (proceso aparte: `clippy.exe --editor`).
//! Biblioteca a la izquierda, vista previa al centro, ajustes del clip a la derecha y
//! línea de tiempo abajo con pistas de video (miniaturas) y de audio (forma de onda).
//! Al cerrar la ventana el proceso termina.

mod browser;
mod library;
mod inspector;
mod media;
mod player;
mod prefs;
mod preview;
mod shortcuts;
mod export;
mod panels;
mod playback;
mod timeline;
#[cfg(feature = "devtools")]
pub mod devtools;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Align2, FontId, Layout, Margin, RichText, Stroke};

use crate::app::mode::Window;
use crate::config::{self, Config};
use crate::mf::Mf;
use crate::ui_kit::theme::*;
use crate::ui_kit::watch::Watcher;
use crate::ui_kit::window::{maximize_once, single_instance, window_options};
use shortcuts::Act;
use library::{Asset, Loader};
use prefs::Prefs;
use player::Clock;
use playback::{Layer, Voice};
use timeline::{Snapshot, Track, TrackKind};
use export::{ExportDialog, ExportJob};
use panels::{Drag, Menu};
use browser::Browser;
use preview::CropEdit;

/// Tamaño mínimo de la ventana: entran los paneles en su tamaño mínimo y la vista previa.
const MIN_SIZE: [f32; 2] = [960.0, 600.0];
/// Los paneles se agrandan o achican arrastrando su borde, sin pasar de estos límites (y de un 45 % del
/// ancho o un 60 % del alto de la ventana), así la vista previa nunca desaparece.
const SIDE_MIN: f32 = 220.0;
const TIMELINE_MIN: f32 = 200.0;
const PREVIEW_MIN_W: f32 = 320.0;

pub fn run(file: Option<PathBuf>) {
    let win = Window::Editor;
    let Some(_mutex) = single_instance(win.mutex(), win.title()) else { return };
    let _ = eframe::run_native(
        win.title(),
        window_options(win.title(), MIN_SIZE),
        Box::new(move |cc| {
            setup_style(&cc.egui_ctx);
            Ok(Box::new(EditorApp::new(cc.egui_ctx.clone(), file)))
        }),
    );
}

/// Pestañas de la ventana de la tuerca.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum HelpTab {
    Keys,
    Guide,
}

struct EditorApp {
    ctx: egui::Context,
    _mf: Mf,
    /// Solo se lee (carpeta de clips); la maneja la ventana de ajustes.
    cfg: Config,
    prefs: Prefs,
    loader: Loader,
    /// Los videos de la carpeta que se ve en la biblioteca.
    assets: Vec<Asset>,
    imported: Vec<PathBuf>,
    browser: Browser,
    /// Avisa cuando cambia la carpeta de clips o la config (por ejemplo, un clip recién guardado).
    watcher: Watcher,

    tracks: Vec<Track>,
    /// Marcadores de la línea de tiempo, ordenados.
    markers: Vec<i64>,
    /// Proporción del lienzo (None = la del primer clip).
    ratio: Option<media::Ratio>,
    next_id: u64,
    selected: Option<u64>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,

    /// Dónde está el cursor de reproducción y si avanza.
    clock: Clock,
    playing: bool,
    voices: Vec<Voice>,
    idle: Vec<Voice>,
    /// Piezas que muestra la vista previa (de abajo hacia arriba) y cuánto se ve cada una.
    shown: Vec<(u64, f32)>,
    layers: Vec<Layer>,
    /// Recortando la imagen de una pieza en la vista previa.
    crop: Option<CropEdit>,

    zoom: f32,
    /// Zoom pedido (botones, deslizador, teclado o Ctrl + rueda); se aplica al dibujar la línea de tiempo,
    /// dejando quieta la x de pantalla que trae (o, si no trae, el cursor de reproducción).
    zoom_to: Option<(f32, Option<f32>)>,
    scroll_x: f32,
    set_scroll: Option<f32>,
    drag: Drag,
    drag_before: Option<Vec<Track>>,
    last_seek: Instant,
    /// Mientras se arrastra el cursor de reproducción: el tiempo bajo el mouse.
    scrub_t: Option<i64>,
    /// Mientras se arrastra un borde o una esquina con el imán: el punto donde quedó enganchado.
    snap: Option<i64>,
    /// Lo que se tocó con el clic derecho en la línea de tiempo.
    menu: Menu,
    /// Recién soltado un clip en otro lugar: cuándo y cuántos px le faltan a cada pieza para llegar.
    settle: Option<(f64, Vec<(u64, f32)>)>,

    job: Option<ExportJob>,
    message: Option<(bool, String, Option<PathBuf>)>,
    maximized: bool,
    /// Ventana de la tuerca abierta, y en qué pestaña.
    help: Option<HelpTab>,
    export_dlg: Option<ExportDialog>,
    /// Hay cambios sin exportar.
    dirty: bool,
    confirm_close: bool,
    allow_close: bool,
    keys_editor: shortcuts::Editor,
    cfg_mtime: Option<std::time::SystemTime>,
}

impl eframe::App for EditorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        maximize_once(ctx, &mut self.maximized, Window::Editor.title());
        if self.watcher.take_changed() {
            self.reload_config();
            self.scan();
        }
        self.poll_export();
        self.shortcuts(ctx);
        self.tick();
        self.update_texture();
        if self.playing {
            // El hilo de video ya pide cuadro con cada imagen nueva; esto mueve el cursor entre una y otra.
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        let screen = ctx.screen_rect();
        let mut resized = false;

        egui::TopBottomPanel::top("top")
            .exact_height(60.0)
            .frame(egui::Frame::none().fill(BG).inner_margin(Margin::symmetric(18.0, 12.0)))
            .show(ctx, |ui| self.top_bar(ui));
        forget_panel_size(ctx, "timeline");
        let shown = egui::TopBottomPanel::bottom("timeline")
            .resizable(true)
            .default_height(self.prefs.timeline_h)
            .height_range(TIMELINE_MIN..=(screen.height() * 0.6).max(TIMELINE_MIN))
            .frame(egui::Frame::none().fill(CARD).inner_margin(Margin::symmetric(14.0, 10.0)))
            .show(ctx, |ui| self.timeline_panel(ui))
            .response
            .rect;
        resized |= follow_panel_size(ctx, "timeline", &mut self.prefs.timeline_h, shown.height());
        // El de la izquierda deja lugar para el de la derecha en su mínimo; el de la derecha, para lo que quedó.
        let side_max = |other: f32| (screen.width() * 0.45).min(screen.width() - other - PREVIEW_MIN_W).max(SIDE_MIN);
        forget_panel_size(ctx, "library");
        let shown = egui::SidePanel::left("library")
            .resizable(true)
            .default_width(self.prefs.library_w)
            .width_range(SIDE_MIN..=side_max(SIDE_MIN))
            .frame(egui::Frame::none().fill(CARD).inner_margin(Margin::symmetric(16.0, 14.0)))
            .show(ctx, |ui| self.library_panel(ui))
            .response
            .rect;
        resized |= follow_panel_size(ctx, "library", &mut self.prefs.library_w, shown.width());
        forget_panel_size(ctx, "props");
        let shown = egui::SidePanel::right("props")
            .resizable(true)
            .default_width(self.prefs.props_w)
            .width_range(SIDE_MIN..=side_max(shown.width()))
            .frame(egui::Frame::none().fill(CARD).inner_margin(Margin::symmetric(18.0, 14.0)))
            // Con la ventana baja no entra todo: se desplaza.
            .show(ctx, |ui| egui::ScrollArea::vertical().id_salt("props").auto_shrink([false, false]).show(ui, |ui| self.props_panel(ui)))
            .response
            .rect;
        resized |= follow_panel_size(ctx, "props", &mut self.prefs.props_w, shown.width());
        if resized {
            self.prefs.save();
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(BG).inner_margin(Margin::same(18.0)))
            .show(ctx, |ui| self.preview(ui));

        self.help_window(ctx);
        self.export_dialog(ctx);
        self.library_dialogs(ctx);
        self.close_guard(ctx);
        self.drop_files(ctx);
    }
}

impl EditorApp {
    fn new(ctx: egui::Context, file: Option<PathBuf>) -> Self {
        let cfg = config::load();
        let imported = library::load_imported();
        let watcher = Watcher::new(ctx.clone(), vec![crate::log::app_dir(), cfg.output_dir.clone()], false);
        let mut app = Self {
            loader: Loader::new(ctx.clone()),
            ctx,
            _mf: Mf::init(),
            cfg,
            prefs: Prefs::load(),
            assets: Vec::new(),
            imported,
            browser: Browser::default(),
            watcher,
            tracks: vec![Track::new(1, TrackKind::Video), Track::new(2, TrackKind::Audio)],
            markers: Vec::new(),
            ratio: None,
            next_id: 2,
            selected: None,
            undo: Vec::new(),
            redo: Vec::new(),
            clock: Clock::new(),
            playing: false,
            voices: Vec::new(),
            idle: Vec::new(),
            shown: Vec::new(),
            layers: Vec::new(),
            crop: None,
            zoom: 1.0,
            zoom_to: None,
            scroll_x: 0.0,
            set_scroll: None,
            drag: Drag::None,
            drag_before: None,
            last_seek: Instant::now(),
            scrub_t: None,
            snap: None,
            menu: Menu::None,
            settle: None,
            job: None,
            message: None,
            maximized: false,
            help: None,
            export_dlg: None,
            dirty: false,
            confirm_close: false,
            allow_close: false,
            keys_editor: shortcuts::Editor::default(),
            cfg_mtime: config::config_mtime(),
        };
        app.scan();
        #[cfg(feature = "devtools")]
        app.load_test_timeline();
        if let Some(f) = file {
            app.import(vec![f], true);
        }
        app
    }

    /// Agrega archivos a la biblioteca (y opcionalmente a la línea de tiempo).
    fn import(&mut self, files: Vec<PathBuf>, to_timeline: bool) {
        let mut bad = Vec::new();
        let mut added = false;
        for f in files {
            if !library::is_video(&f) || self.loader.probe(&f).is_none() {
                bad.push(f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
                continue;
            }
            // Lo que ya está en la carpeta de clips (o en una de sus carpetas) no se importa: ya se ve.
            if !f.starts_with(&self.cfg.output_dir) && !self.imported.contains(&f) {
                self.imported.push(f.clone());
                added = true;
            }
            if to_timeline {
                self.add_to_timeline(&f);
            }
        }
        library::save_imported(&self.imported);
        // Los importados se ven en la biblioteca, no dentro de una carpeta.
        if added && self.browser.dir.is_some() {
            self.open_dir(None);
        } else {
            self.scan();
        }
        if !bad.is_empty() {
            self.message = Some((false, format!("No se pudo abrir: {}", bad.join(", ")), None));
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let modal = self.export_dlg.is_some() || self.crop.is_some() || self.confirm_close || self.library_dialog_open();
        if ctx.wants_keyboard_input() || self.keys_editor.capturing.is_some() || modal {
            return;
        }
        for act in shortcuts::pressed(ctx, &self.prefs.keys) {
            if self.help.is_some() && act != Act::Shortcuts {
                continue;
            }
            // Mientras el mouse arrastra un clip o un borde, el arrastre apunta a una pieza:
            // editar con el teclado en ese momento la movería o borraría debajo del mouse.
            if act.edits_timeline() && !matches!(self.drag, Drag::None) {
                continue;
            }
            // Supr con lo último tocado en la biblioteca borra lo elegido ahí (con confirmación).
            if act == Act::Delete && self.delete_library_selection() {
                continue;
            }
            self.run_action(act);
        }
    }

    fn run_action(&mut self, act: Act) {
        match act {
            Act::PlayPause => self.toggle_play(),
            Act::PrevFrame => self.step(-self.frame_at_cursor()),
            Act::NextFrame => self.step(self.frame_at_cursor()),
            Act::Back => self.step(-10_000_000),
            Act::Forward => self.step(10_000_000),
            Act::ToStart => self.seek(0),
            Act::ToEnd => {
                let t = self.total();
                self.seek(t);
            }
            Act::Split => self.split(),
            Act::Duplicate => self.duplicate_selected(),
            Act::Delete => self.delete_selected(),
            Act::Freeze => self.freeze(),
            Act::Marker => self.toggle_marker(),
            Act::MuteClip => self.toggle_mute_selected(),
            Act::Link => self.toggle_link(),
            Act::Magnet => {
                self.prefs.magnet = !self.prefs.magnet;
                self.prefs.save();
            }
            Act::Undo => self.undo(),
            Act::Redo => self.redo(),
            Act::ZoomIn => self.zoom_by(1.5),
            Act::ZoomOut => self.zoom_by(1.0 / 1.5),
            Act::ZoomFit => {
                self.zoom = 1.0;
                self.zoom_to = None;
                self.set_scroll = Some(0.0);
            }
            Act::Import => self.import_dialog(),
            Act::Export => self.export(),
            Act::Shortcuts => {
                if self.help.is_some() {
                    self.help = None;
                } else {
                    self.open_help(HelpTab::Keys);
                }
                self.keys_editor.capturing = None;
            }
        }
    }

    fn import_dialog(&mut self) {
        if let Some(files) = rfd::FileDialog::new().set_title("Importar videos").add_filter("Videos", &library::VIDEO_EXTS).pick_files() {
            self.import(files, false);
        }
    }

    /// Relee la config si cambió (por ejemplo, otra carpeta de clips elegida en Ajustes).
    fn reload_config(&mut self) {
        let m = config::config_mtime();
        if m != self.cfg_mtime {
            self.cfg_mtime = m;
            let dir = self.cfg.output_dir.clone();
            self.cfg = config::load();
            if self.cfg.output_dir != dir {
                self.browser.dir = None;
            }
            self.watcher.set_dirs(self.watched_dirs());
        }
    }

    pub(super) fn open_help(&mut self, tab: HelpTab) {
        self.help = Some(tab);
        self.keys_editor.capturing = None;
    }

    /// La ventana de la tuerca: los atajos (se cambian tocándolos) y cómo se usa el editor.
    fn help_window(&mut self, ctx: &egui::Context) {
        let Some(mut tab) = self.help else { return };
        if self.keys_editor.poll(ctx, &mut self.prefs.keys) {
            self.prefs.save();
        }
        let capturing = self.keys_editor.capturing.is_some();
        let max_h = (ctx.screen_rect().height() - 340.0).clamp(220.0, 560.0);
        let mut done = false;
        let subtitle = match tab {
            HelpTab::Keys => "Tocá un atajo para cambiarlo. Los que funcionan mientras jugás están en la ventana de Clippy.",
            HelpTab::Guide => "Lo que conviene saber para editar rápido.",
        };
        let x = modal(ctx, "keys", "Ajustes del editor", subtitle, 820.0, |ui| {
            modal_body(ui, |ui| {
                ui.allocate_ui(egui::vec2(320.0, 32.0), |ui| {
                    if seg(ui, &mut tab, &[(HelpTab::Keys, "Atajos"), (HelpTab::Guide, "Cómo se usa")]) {
                        self.keys_editor.capturing = None;
                    }
                });
                ui.add_space(6.0);
                egui::ScrollArea::vertical().max_height(max_h).auto_shrink([false, true]).show(ui, |ui| match tab {
                    HelpTab::Keys => self.keys_editor.ui(ui, &self.prefs.keys),
                    HelpTab::Guide => self.guide(ui),
                });
            });
            modal_footer(ui, |ui| {
                if tab == HelpTab::Keys && button_sized(ui, "Restablecer atajos", Kind::Secondary, egui::vec2(0.0, 38.0)).clicked() {
                    self.keys_editor.reset(&mut self.prefs.keys);
                    self.prefs.save();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if button_sized(ui, "Listo", Kind::Primary, egui::vec2(120.0, 38.0)).clicked() {
                        done = true;
                    }
                });
            });
        });
        self.help = Some(tab);
        // Esc mientras se elige una tecla cancela esa elección, no cierra la ventana.
        if done || (x && !capturing) {
            self.help = None;
            self.keys_editor.capturing = None;
        }
    }

    /// "Cómo se usa": lo que no se descubre solo (arrastres, Alt, clic derecho), con los atajos de ahora.
    fn guide(&self, ui: &mut egui::Ui) {
        let k = |a: Act| shortcuts::short(&self.prefs.keys, a);
        let topics = [
            (
                "Biblioteca",
                "Un clic elige un video (con Ctrl se suman, con Shift se elige un tramo) y arrastrando elegís varios con un recuadro. Doble clic o el + lo suma a la línea de tiempo; también podés arrastrar videos del Explorador a la ventana. Con el clic derecho los movés a una carpeta o los mandás a la Papelera de reciclaje.".to_string(),
            ),
            (
                "Recortar y mover",
                "Arrastrá los bordes de un clip para recortarlo y del medio para cambiarlo de lugar o de pista. Con Alt apretado, el borde o la esquina cambia solo la imagen o solo el sonido.".to_string(),
            ),
            (
                "Fundidos",
                "Arrastrá una esquina de arriba de un clip para que aparezca desde negro o se vaya a negro. Si toca a otro clip, se funden uno en el otro. Con el clic derecho sobre el fundido elegís su curva.".to_string(),
            ),
            (
                "Imán",
                format!("Prendido ({}), los clips de cada pista quedan pegados uno detrás del otro. Apagado, quedan donde los soltás y, si se superponen, se funden.", k(Act::Magnet)),
            ),
            (
                "Clic derecho en un clip",
                "Dividir, duplicar, congelar el cuadro, cambiar la velocidad, girar, espejar o recortar la imagen, silenciar, separar el audio y eliminar.".to_string(),
            ),
            (
                "Velocidad, imagen y proporción",
                "En el panel de la derecha cambiás la velocidad del clip y su imagen. Debajo de la vista previa elegís la proporción del video: horizontal, vertical para el celular o cuadrado. Si un clip no tiene esa forma, elegís si entra entero o llena el cuadro.".to_string(),
            ),
            (
                "Congelar y marcadores",
                format!(
                    "{} deja quieto el cuadro del cursor 3 segundos (arrastrá su borde para que dure más o menos). {} pone un marcador: tocalo para ir ahí, arrastralo para moverlo o sacalo con el clic derecho. Con el imán, los bordes se enganchan en los marcadores.",
                    k(Act::Freeze),
                    k(Act::Marker)
                ),
            ),
        ];
        ui.columns(2, |cols| {
            for (n, (title, text)) in topics.iter().enumerate() {
                let ui = &mut cols[n % 2];
                ui.label(RichText::new(*title).family(bold()).size(13.5).color(TEXT));
                ui.add(egui::Label::new(RichText::new(text).size(13.0).color(MUTED)).wrap());
                ui.add_space(12.0);
            }
        });
    }

    /// Si hay cambios sin exportar (o se está exportando), preguntar antes de cerrar.
    fn close_guard(&mut self, ctx: &egui::Context) {
        let risky = (self.dirty && !self.is_empty()) || self.job.is_some();
        if ctx.input(|i| i.viewport().close_requested()) && risky && !self.allow_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
        if !self.confirm_close {
            return;
        }
        let exporting = self.job.is_some();
        let (title, body) = if exporting {
            ("Se está exportando el video", "Si cerrás ahora, se cancela la exportación.")
        } else {
            ("¿Cerrar sin exportar?", "Tenés cambios en la línea de tiempo que todavía no exportaste. Si cerrás, se pierden.")
        };
        let mut action = 0;
        let x = modal(ctx, "close", title, "", 480.0, |ui| {
            modal_body(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(body).color(MUTED)).wrap());
            });
            modal_footer(ui, |ui| {
                if button_sized(ui, "Seguir editando", Kind::Secondary, egui::vec2(0.0, 38.0)).clicked() {
                    action = 1;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if !exporting && button_sized(ui, "Exportar", Kind::Primary, egui::vec2(120.0, 38.0)).clicked() {
                        action = 3;
                    }
                    if button_sized(ui, "Cerrar sin guardar", Kind::Danger, egui::vec2(0.0, 38.0)).clicked() {
                        action = 2;
                    }
                });
            });
        });
        if x && action == 0 {
            action = 1;
        }
        match action {
            1 => self.confirm_close = false,
            2 => {
                if let Some(j) = &self.job {
                    j.progress.cancel.store(true, Ordering::Relaxed);
                }
                self.allow_close = true;
                self.confirm_close = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            3 => {
                self.confirm_close = false;
                self.export();
            }
            _ => {}
        }
    }

    fn drop_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() {
            self.import(dropped, true);
        }
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        // Aparece con un fundido corto y se va un tercio más rápido.
        let time = if hovering { T_FADE } else { T_FADE * 2.0 / 3.0 };
        let a = ctx.animate_bool_with_time_and_easing(egui::Id::new("drop"), hovering, time, ease_out);
        if a > 0.0 {
            let screen = ctx.screen_rect();
            let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop")));
            p.rect_filled(screen, 0.0, SCRIM.gamma_multiply(a));
            p.rect_stroke(screen.shrink(24.0), 16.0, Stroke::new(2.0_f32, WHITE.gamma_multiply(a)));
            p.text(screen.center(), Align2::CENTER_CENTER, "Soltá los videos para agregarlos", FontId::proportional(24.0), WHITE.gamma_multiply(a));
        }
    }
}

/// egui recuerda solo el tamaño de cada panel; acá se olvida en cada cuadro para que mande el de las
/// preferencias (recortado a la ventana de ahora, así una pantalla más chica no rompe nada).
fn forget_panel_size(ctx: &egui::Context, id: &str) {
    ctx.data_mut(|d| d.remove::<egui::containers::panel::PanelState>(egui::Id::new(id)));
}

/// Mientras se arrastra el borde del panel `id`, su tamaño en pantalla (`shown`) pasa a `size`.
/// Devuelve true al soltarlo (hay que guardar).
fn follow_panel_size(ctx: &egui::Context, id: &str, size: &mut f32, shown: f32) -> bool {
    // "__resize" es el id que egui le da a la agarradera del borde.
    let Some(r) = ctx.read_response(egui::Id::new(id).with("__resize")) else { return false };
    if r.dragged() || r.drag_stopped() {
        *size = shown;
    }
    r.drag_stopped()
}
