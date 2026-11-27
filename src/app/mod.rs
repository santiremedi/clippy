//! El proceso como app de Windows: modos del .exe, bandeja, atajos globales y actualizaciones.

pub mod autostart;
#[cfg(feature = "devtools")]
pub mod devtools;
pub mod hotkeys;
pub mod keys;
pub mod mode;
pub mod status;
pub mod tray;
pub mod update;
