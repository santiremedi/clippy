//! Modos de prueba sin ventana (solo con `--features devtools`; no van en el exe publicado).
//!
//! - `--test <segundos> <carpeta> [game|desktop|mic|juegos]`: graba, guarda un clip y sale.
//! - `--test-mic`: cuánto audio entrega cada micrófono en 3 s.
//! - `--test-download`: descarga la versión publicada como el botón "Actualizar".
//! - `--test-export`, `--test-frame`, `--test-strip`, `--test-covers`: ver `editor::devtools`.

use std::sync::Arc;
use std::time::Duration;

use crate::record::{audio, clock};

/// Devuelve el código de salida si `args` pide un modo de prueba.
pub fn run(args: &[String]) -> Option<i32> {
    let at = |flag: &str| args.iter().position(|a| a == flag);
    if at("--test-download").is_some() {
        return Some(test_download());
    }
    if at("--test-mic").is_some() {
        return Some(test_mic());
    }
    if let Some(i) = args.iter().position(|a| a == "--test-export" || a == "--test-frame" || a == "--test-strip" || a == "--test-covers") {
        return Some(crate::editor::devtools::test_cli(&args[i..]));
    }
    at("--test").map(|i| test_mode(&args[i + 1..]))
}

fn test_download() -> i32 {
    // Prueba: descargar la versión publicada como lo hace el botón "Actualizar".
    let url = crate::app::update::load().url.unwrap_or_default();
    match crate::app::update::download(&url) {
        Ok(p) => println!("OK {} ({} bytes)", p.display(), std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0)),
        Err(e) => println!("ERROR {e}"),
    }
    0
}

fn test_mic() -> i32 {
    // Prueba: cuánto audio entrega cada micrófono en 3 s.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED);
    }
    let mut devs = vec![(String::new(), "Predeterminado".to_string())];
    devs.extend(audio::capture_devices());
    for (id, name) in devs {
        match audio::Audio::open(audio::Source::Mic(id)) {
            Ok(mut a) => {
                let mut buf = audio::AudioBuf::default();
                let t0 = clock::now_100ns();
                for _ in 0..60 {
                    std::thread::sleep(Duration::from_millis(50));
                    let _ = a.poll(&mut buf, t0);
                }
                let peak = buf.data.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
                println!("{name}: {} muestras, pico {peak}", buf.data.len() / 2);
            }
            Err(e) => println!("{name}: error {e}"),
        }
    }
    0
}

/// `clippy --test <segundos_a_esperar> <carpeta> [game|desktop|mic|juegos]`: graba, guarda un clip y sale (para pruebas).
fn test_mode(args: &[String]) -> i32 {
    let wait: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(8);
    let dir = args.get(1).map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let mut cfg = crate::config::load();
    cfg.output_dir = dir;
    cfg.sound_on_save = false;
    match args.get(2).map(|s| s.as_str()) {
        Some("game") => (cfg.game_audio, cfg.desktop_audio) = (true, false),
        Some("desktop") => (cfg.game_audio, cfg.desktop_audio) = (false, true),
        Some("mic") => (cfg.game_audio, cfg.desktop_audio, cfg.mic_audio) = (false, false, true),
        Some("juegos") => cfg.capture_mode = crate::config::CaptureMode::GamesOnly,
        _ => {}
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let notify: crate::record::recorder::Notify = Arc::new(move |e| {
        let _ = tx.send(e);
    });
    let rec = crate::record::recorder::Recorder::start(cfg, notify);
    std::thread::sleep(Duration::from_secs(wait));
    rec.save();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(crate::record::recorder::Event::Started(n)) => println!("encoder: {n}"),
            Ok(crate::record::recorder::Event::Game(g)) => println!("juego: {}", g.as_deref().unwrap_or("ninguno")),
            Ok(crate::record::recorder::Event::Saved(p)) => {
                println!("OK {}", p.display());
                return 0;
            }
            Ok(crate::record::recorder::Event::SaveFailed(e)) | Ok(crate::record::recorder::Event::Error(e)) => {
                println!("ERROR {e}");
                return 1;
            }
            Err(_) => {}
        }
    }
    println!("ERROR timeout");
    2
}
