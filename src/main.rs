use subtitle_renamer::core::config::UserConfig;
use subtitle_renamer::core::history::HistoryDb;
use subtitle_renamer::ui::app::App;
use subtitle_renamer::ui::fonts;

fn main() -> eframe::Result<()> {
    let config = UserConfig::default();
    let history = HistoryDb::open_default().unwrap_or_else(|e| {
        eprintln!("warning: history db unavailable ({e}); starting without persistence");
        HistoryDb::open(&std::env::temp_dir().join("subtitle-renamer-fallback.db"))
            .expect("fallback db")
    });
    let (fonts, font_notice) = fonts::build_font_definitions();
    if let Some(msg) = &font_notice {
        eprintln!("{msg}");
    }
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "subtitle-renamer",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_fonts(fonts);
            let mut app = App::new(config, history);
            if let Some(msg) = font_notice {
                app.status_message = msg;
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
