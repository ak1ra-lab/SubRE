//! GUI layer. The core is GUI-free; this module owns all `eframe`/`egui`
//! interactions and delegates business logic to `crate::core`.

pub mod app;
pub mod dnd_drop;
pub mod fonts;
