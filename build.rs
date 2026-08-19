// Embed the multi-resolution `assets/subtitle-renamer.ico` into the Windows
// PE resource section so the `.exe` displays the app icon in Explorer /
// taskbar / shortcuts (matching the runtime window icon set in
// `src/main.rs`). No-op on non-Windows targets so Linux/macOS builds are
// unaffected.
//
// Implementation notes:
//   * Build scripts run on the HOST. `#[cfg(windows)]` would evaluate
//     against the host (false when cross-compiling from Linux to Windows),
//     so we inspect `CARGO_CFG_TARGET_OS` to detect the actual target.
//   * Build scripts also run from the `target/` build dir, so all asset
//     paths must be resolved against `CARGO_MANIFEST_DIR`.
//   * The windres-compiled `libresource.a` contains only a `.rsrc`
//     section with no referenced symbols, so the linker would GC it
//     without `+whole-archive`. We bypass `winres::WindowsResource::compile`
//     (which emits its own `cargo:rustc-link-lib` without the modifier)
//     and emit the directive ourselves.
//
// Version info is emitted into the PE resource alongside the icon so the
// `.exe`'s Properties dialog shows real metadata instead of an empty
// version block.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let ico = PathBuf::from(&manifest_dir).join("assets/subtitle-renamer.ico");
    let rc = PathBuf::from(&out_dir).join("resource.rc");
    let obj = PathBuf::from(&out_dir).join("resource.o");
    let lib = PathBuf::from(&out_dir).join("libresource.a");

    // Pick toolchain. On a Windows host the bare `windres` / `ar` resolve
    // fine; on a Linux host cross-compiling to Windows we need the
    // mingw-prefixed names.
    let (windres, ar) = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => ("x86_64-w64-mingw32-windres", "x86_64-w64-mingw32-ar"),
        Ok("i686") => ("i686-w64-mingw32-windres", "i686-w64-mingw32-ar"),
        Ok("aarch64") => ("aarch64-w64-mingw32-windres", "aarch64-w64-mingw32-ar"),
        _ => ("windres", "ar"),
    };

    let pkg_name = std::env::var("CARGO_PKG_NAME").expect("CARGO_PKG_NAME");
    let pkg_version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let pkg_description =
        std::env::var("CARGO_PKG_DESCRIPTION").unwrap_or_else(|_| pkg_name.clone());
    let ver_tuple = pkg_version
        .split('.')
        .map(|p| p.parse::<u16>().unwrap_or(0))
        .chain(std::iter::repeat(0))
        .take(4)
        .collect::<Vec<_>>();

    let rc_body = format!(
        "#pragma code_page(65001)\n\
         1 VERSIONINFO\n\
         FILESUBTYPE 0x0\n\
         FILEOS 0x40004\n\
         PRODUCTVERSION {0}, {1}, {2}, {3}\n\
         FILETYPE 0x1\n\
         FILEFLAGSMASK 0x3f\n\
         FILEFLAGS 0x0\n\
         FILEVERSION {0}, {1}, {2}, {3}\n\
         {{\n\
         BLOCK \"StringFileInfo\"\n\
         {{\n\
         BLOCK \"000004b0\"\n\
         {{\n\
         VALUE \"ProductName\", \"{name}\"\n\
         VALUE \"ProductVersion\", \"{ver}\"\n\
         VALUE \"FileVersion\", \"{ver}\"\n\
         VALUE \"FileDescription\", \"{desc}\"\n\
         }}\n\
         }}\n\
         BLOCK \"VarFileInfo\" {{\n\
         VALUE \"Translation\", 0x0, 0x04b0\n\
         }}\n\
         }}\n\
         1 ICON \"{ico}\"\n",
        ver_tuple[0],
        ver_tuple[1],
        ver_tuple[2],
        ver_tuple[3],
        name = pkg_name,
        ver = pkg_version,
        desc = pkg_description,
        ico = ico.display(),
    );
    std::fs::write(&rc, rc_body).expect("write resource.rc");

    let windres_status = Command::new(windres)
        .arg(format!("-I{manifest_dir}"))
        .arg(format!("{}", rc.display()))
        .arg(format!("{}", obj.display()))
        .status()
        .expect("spawn windres");
    assert!(windres_status.success(), "windres failed");

    let ar_status = Command::new(ar)
        .arg("rsc")
        .arg(format!("{}", lib.display()))
        .arg(format!("{}", obj.display()))
        .status()
        .expect("spawn ar");
    assert!(ar_status.success(), "ar failed");

    println!("cargo:rustc-link-search=native={out_dir}");
    println!("cargo:rustc-link-lib=static:+whole-archive=resource");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/subtitle-renamer.ico");
    println!("cargo:rerun-if-changed=assets/subtitle-renamer.svg");
    println!("cargo:rerun-if-changed=Cargo.toml");
}
