// Embed the multi-resolution `assets/SubRE.ico` and version info into the
// Windows PE resource section so the `.exe` displays the app icon in Explorer /
// taskbar / shortcuts (matching the runtime window icon set in `src/main.rs`).
// No-op on non-Windows targets so Linux/macOS builds are unaffected.
//
// `embed-resource` (https://github.com/nabijaczleweli/rust-embed-resource)
// handles both MSVC and GNU toolchains internally: on MSVC it drives `rc.exe`
// and `link.exe` (with the appropriate /WHOLEARCHIVE equivalent), on GNU it
// drives `windres` + `ld`. We no longer hand-roll tool detection or
// architecture-prefixed tool names.

use std::path::{Path, PathBuf};

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let ico = Path::new(&manifest_dir).join("assets").join("SubRE.ico");
    let rc = PathBuf::from(&out_dir).join("resource.rc");

    std::fs::copy(&ico, Path::new(&out_dir).join("SubRE.ico")).expect("copy SubRE.ico");

    let pkg_name = std::env::var("CARGO_PKG_NAME").expect("CARGO_PKG_NAME");
    let pkg_version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let pkg_description =
        std::env::var("CARGO_PKG_DESCRIPTION").unwrap_or_else(|_| pkg_name.clone());

    let ver_tuple: Vec<u16> = pkg_version
        .split('.')
        .map(|p| p.parse::<u16>().unwrap_or(0))
        .chain(std::iter::repeat(0))
        .take(4)
        .collect();
    let (vmaj, vmin, vpat, vbuild) = (ver_tuple[0], ver_tuple[1], ver_tuple[2], ver_tuple[3]);

    let rc_body = format!(
        r#"#pragma code_page(65001)

1 VERSIONINFO
FILEVERSION {vmaj}, {vmin}, {vpat}, {vbuild}
PRODUCTVERSION {vmaj}, {vmin}, {vpat}, {vbuild}
FILEOS 0x40004
FILETYPE 0x1
{{
BLOCK "StringFileInfo"
{{
BLOCK "000004b0"
{{
VALUE "ProductName", "{pkg_name}"
VALUE "ProductVersion", "{pkg_version}"
VALUE "FileVersion", "{pkg_version}"
VALUE "FileDescription", "{pkg_description}"
}}
}}
BLOCK "VarFileInfo"
{{
VALUE "Translation", 0x0, 0x04b0
}}
}}

1 ICON "SubRE.ico"
"#
    );

    std::fs::write(&rc, rc_body).expect("write resource.rc");

    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_optional()
        .expect("failed to embed Windows resource");
}
