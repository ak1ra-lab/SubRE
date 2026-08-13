//! Drop-handling is implemented inline in `app.rs` (the `eframe::App::ui`
//! method consumes `egui::InputState::raw.dropped_files`). This module
//! stays as a placeholder so future cross-platform drop quirks can land
//! here without re-plumbing the lib's module tree.

#![allow(dead_code)]
