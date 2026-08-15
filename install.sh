#!/bin/sh

set -e
set -u

cd "$(dirname "$0")"

prefix="${PREFIX:-${HOME}/.local}"
bindir="${prefix}/bin"
appsdir="${prefix}/share/applications"
iconbasedir="${prefix}/share/icons/hicolor"

if [ ! -f "target/release/subtitle-renamer" ]; then
    printf 'Build first: cargo build --release\n' >&2
    exit 1
fi

install -Dm755 "target/release/subtitle-renamer" "${bindir}/subtitle-renamer"
install -Dm644 "assets/subtitle-renamer.svg" "${iconbasedir}/scalable/apps/subtitle-renamer.svg"

mkdir -p "${appsdir}"
sed "s|@BINDIR@|${bindir}|" "assets/subtitle-renamer.desktop" >"${appsdir}/subtitle-renamer.desktop"
chmod 644 "${appsdir}/subtitle-renamer.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "${prefix}/share/applications" 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "${prefix}/share/icons/hicolor" 2>/dev/null || true
fi

printf 'Installed to %s\n' "${prefix}"
