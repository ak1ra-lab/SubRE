#!/bin/sh

set -e
set -u

cd "$(dirname "$0")"

prefix="${PREFIX:-${HOME}/.local}"
bindir="${prefix}/bin"
appsdir="${prefix}/share/applications"
iconbasedir="${prefix}/share/icons/hicolor"

if [ -f "./SubRE" ]; then
    bin="./SubRE"
elif [ -f "target/release/SubRE" ]; then
    bin="target/release/SubRE"
else
    printf 'SubRE binary not found (expected ./SubRE or target/release/SubRE)\n' >&2
    exit 1
fi

install -Dm755 "$bin" "${bindir}/SubRE"
install -Dm644 "assets/SubRE.svg" "${iconbasedir}/scalable/apps/SubRE.svg"

mkdir -p "${appsdir}"
sed "s|@BINDIR@|${bindir}|" "assets/SubRE.desktop" >"${appsdir}/SubRE.desktop"
chmod 644 "${appsdir}/SubRE.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "${prefix}/share/applications" 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "${prefix}/share/icons/hicolor" 2>/dev/null || true
fi

printf 'Installed to %s\n' "${prefix}"
