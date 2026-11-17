# Clippy (app) — guía para agentes

Grabador de repeticiones instantáneas para Windows en Rust (eframe/egui 0.29 glow, windows 0.58).
Ver `README.md` para el mapa del código y los modos del exe.

## Reglas

- **Liviano ante todo.** El exe no puede crecer sin una razón fuerte: medí `target/release/clippy.exe`
  antes y después de cada cambio. No sumes dependencias si Windows ya trae la API.
- **Que se comporte como una app de verdad** (Medal, NVIDIA Instant Replay): nada hardcodeado
  para un juego, nada que el usuario tenga que configurar si se puede detectar.
- **Textos de la interfaz en español rioplatense** ("vos", "tocá", "apretá"). Comentarios en español.
- **Commits y PRs sin atribución de IA**: nada de `Co-Authored-By`, "Generated with" ni links de sesión.
- No publicar releases sin que el usuario lo pida.

## Dónde va cada cosa

- Modos del exe, argumentos, candados, títulos de ventana y mensajes a la bandeja: solo en
  `src/app/mode.rs` (`Mode`, `Window`). No escribas `"--settings"` ni `"Local\\Clippy..."` en otro lado.
- Atajos globales: `config::GlobalAction` (ids, etiqueta, tecla). Atajos del editor: `editor/shortcuts.rs`.
- Qué cuenta como juego: solo `src/record/game.rs` (lo usan el modo "Solo juegos" y el canal de audio).
- `config.json` lo escriben la ventana de ajustes y la bandeja; `editor.json` solo el editor.
- Código que solo sirve para probar va detrás de `#[cfg(feature = "devtools")]`.

## Interfaz y movimiento

Criterios de las skills de Emil Kowalski (`emil-design-eng` y las de animación, instaladas aparte:
`break-ui`, `gestures`, `animation-performance`; no van en el repo). Están escritas para la web: traducilas a egui.
Antes de tocar la interfaz, leé `emil-design-eng`; para revisar, `review-animations`.

- **Colores, duraciones, curvas y widgets solo en `ui_kit/theme.rs`.** Usá `T_PRESS`, `T_TOGGLE`,
  `T_MODAL`, `T_FADE`, `T_SETTLE`, `ease_out`, `ease_in_out`. No escribas otra duración ni otra curva.
- **Nada que se dispare con el teclado se anima** (atajos, deshacer, Esc, F1, Ctrl+E): se usa decenas de
  veces por día y tiene que sentirse inmediato. Lo que cambia desde afuera (bandeja, otro clip) tampoco.
- **Todo lo que se toca con el mouse responde al bajar el botón**: `press` + `paint_pressed`
  (`PRESS_DEPTH` 0.97, `PRESS_DEPTH_ICON` 0.95). Los botones de `theme.rs` ya lo hacen.
- **Sin fundidos de hover** ni animaciones decorativas en lo que se usa seguido. El cabezal y los datos
  de la línea de tiempo nunca se suavizan: siguen al mouse 1 a 1.
- **Respetá `reduced_motion()`** (Windows con "Efectos de animación" apagado): sin movimiento ni escala;
  los fundidos de opacidad quedan.
- **Ninguna animación ni ventana repinta en reposo.** Pedí repintado solo mientras algo se mueve
  (`animate_bool_*` ya lo hace) y nunca `request_repaint()` en cada cuadro "por las dudas".
- **Datos de verdad, no de ejemplo**: nombres y rutas largas se cortan con "…" y muestran el texto completo
  al pasar el mouse; tiempos de más de una hora (`fmt_time`, `fmt_clock`), tamaños (`fmt_bytes`), cantidades
  (`fmt_count`) y plurales bien escritos ("1 clip", "hace 1 semana").
- **Jerarquía**: lo principal de cada pantalla arriba y en `hero_card`; un solo botón `Kind::Primary` visible
  por pantalla.

## Estructura del código

Seguí la skill `untangle-codebase`: lo que cambia junto va junto (una sola definición de cada
valor, texto o regla), nada de casos especiales para un solo valor ni envoltorios que no agregan nada,
y nada de código muerto.

## Probar

```bash
cargo clippy --features devtools -- -D warnings   # chequeo rápido, el mismo que corre el CI
cargo build --profile fast --features devtools    # para probar: unos segundos por cambio
target/fast/clippy.exe --test 8 <carpeta> game
cargo build --release                             # una sola vez, al final, para medir el tamaño
```

Compilá siempre en `target/` con estos perfiles: otra `--target-dir` recompila todo de cero.

No hay tests automáticos. Después de cambiar la grabación, probá con `--test` y mirá
`%APPDATA%\Clippy\clippy.log`. Después de cambiar ventanas, abrí `--settings` y `--editor`.
