//! Grabación: captura de pantalla y audio, codificación en la GPU, buffer en RAM y guardado de clips.

pub mod audio;
mod capture;
pub mod clock;
mod convert;
mod cursor;
mod encoder;
mod game;
mod mux;
mod screen;
pub mod recorder;

pub use encoder::{Device, CPU_MAX_FPS, CPU_MAX_SHORT};
/// El juego de mentira de `devtools` compila su carga para la placa con lo mismo que el cursor.
#[cfg(feature = "devtools")]
pub use cursor::compile;
