#![cfg_attr(windows, windows_subsystem = "windows")]

use subtitle_renamer::core::config::{ConfigStore, UserConfig};
use subtitle_renamer::core::history::HistoryDb;
use subtitle_renamer::ui::app::App;
use subtitle_renamer::ui::fonts;

/// Global body/button font size (px). Bumped from egui's 14 default for
/// fullscreen readability. Not yet a config item.
const BODY_FONT_SIZE: f32 = 16.0;

fn main() -> eframe::Result<()> {
    let config = ConfigStore::load_default().unwrap_or_else(|e| {
        eprintln!("warning: config load failed ({e}); starting with defaults");
        UserConfig::default()
    });
    let history = HistoryDb::open_default().unwrap_or_else(|e| {
        eprintln!("warning: history db unavailable ({e}); starting without persistence");
        HistoryDb::open(&std::env::temp_dir().join("subtitle-renamer-fallback.db"))
            .expect("fallback db")
    });
    let (fonts, font_notice) = fonts::build_font_definitions();
    if let Some(msg) = &font_notice {
        eprintln!("{msg}");
    }
    let mut viewport = egui::ViewportBuilder::default();
    if config.always_on_top {
        viewport = viewport.with_always_on_top();
    }
    if let Ok(icon) =
        eframe::icon_data::from_png_bytes(include_bytes!("../assets/subtitle-renamer.png"))
    {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "subtitle-renamer",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_fonts(fonts);
            cc.egui_ctx.all_styles_mut(|style| {
                style
                    .text_styles
                    .insert(egui::TextStyle::Body, egui::FontId::proportional(BODY_FONT_SIZE));
                style
                    .text_styles
                    .insert(egui::TextStyle::Button, egui::FontId::proportional(BODY_FONT_SIZE));
            });
            let mut app = App::new(config, history);
            if let Some(msg) = font_notice {
                app.push_status(msg);
            }
            // Non-recursively scan the working directory and preload any
            // recognized video/subtitle files found there.
            if let Ok(cwd) = std::env::current_dir() {
                app.ingest_dir_auto(&cwd);
            }
            Ok(Box::new(app))
        }),
    )
}
