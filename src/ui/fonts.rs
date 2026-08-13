//! Locate and register a CJK-capable font with egui so that filenames
//! containing non-ASCII characters render correctly instead of as boxes.
//!
//! On startup, we probe a list of well-known system locations for a CJK
//! font (Noto Sans CJK, Source Han, `WenQuanYi`, …). If found, the font is
//! registered as a `proportional` family fallback so it is used for any
//! characters the default egui fonts don't cover. If not found, we print
//! a one-shot warning and let egui render missing glyphs as boxes (the
//! app still works, just with ugly text).

use std::path::PathBuf;

const PROBE_PATHS: &[&str] = &[
    // Noto CJK (Debian/Ubuntu: opentype/noto; Arch: noto-fonts-cjk).
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    // Source Han Sans.
    "/usr/share/fonts/opentype/source-han-sans/SourceHanSansSC-Regular.otf",
    "/usr/share/fonts/source-han-sans/SourceHanSansSC-Regular.otf",
    // WenQuanYi.
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/wqy-microhei/wqy-microhei.ttc",
    // macOS.
    "/System/Library/Fonts/PingFang.ttc",
    "/Library/Fonts/Songti.ttc",
    // Windows.
    "C:/Windows/Fonts/msyh.ttc",
    "C:/Windows/Fonts/simhei.ttf",
    // User-installed.
    "/usr/local/share/fonts/NotoSansCJK-Regular.ttc",
];

/// Look for a CJK-capable font on disk. Returns the first existing path.
pub fn locate_cjk_font() -> Option<PathBuf> {
    for path in PROBE_PATHS {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Build the egui font definitions with a CJK font registered (if one
/// was found on disk). The CJK font is appended to the
/// `proportional` family so egui uses it as a fallback for characters
/// not covered by the default fonts.
pub fn build_font_definitions() -> (egui::FontDefinitions, Option<String>) {
    let mut fonts = egui::FontDefinitions::default();
    let notice = if let Some(path) = locate_cjk_font() {
        match std::fs::read(&path) {
            Ok(bytes) => {
                let family = "cjk-fallback".to_string();
                fonts.font_data.insert(family.clone(), egui::FontData::from_owned(bytes).into());
                fonts
                    .families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .push(family.clone());
                Some(format!("loaded CJK font from {}", path.display()))
            }
            Err(_) => Some(format!("found CJK font at {} but failed to read it", path.display())),
        }
    } else {
        Some("no CJK font found; non-ASCII filenames may render as boxes".into())
    };
    (fonts, notice)
}
