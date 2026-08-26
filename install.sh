#!/bin/sh

set -e
set -u

cd "$(dirname "$0")"

prefix="${PREFIX:-${HOME}/.local}"
bindir="${prefix}/bin"
appsdir="${prefix}/share/applications"
iconbasedir="${prefix}/share/icons/hicolor"

host_target=""
if command -v rustc >/dev/null 2>&1; then
    host_target="$(rustc -vV 2>/dev/null | awk '/^host:[[:space:]]/{print $2; exit}')"
fi

if [ -f "./SubRE" ]; then
    bin="./SubRE"
elif [ -n "${host_target}" ] && [ -f "target/${host_target}/release/SubRE" ]; then
    bin="target/${host_target}/release/SubRE"
elif [ -f "target/release/SubRE" ]; then
    bin="target/release/SubRE"
else
    printf 'SubRE binary not found.\n' >&2
    printf 'Expected one of:\n' >&2
    printf '  - ./SubRE (release tarball)\n' >&2
    if [ -n "${host_target}" ]; then
        printf '  - target/%s/release/SubRE (cargo build --release --target %s)\n' "${host_target}" "${host_target}" >&2
    else
        printf '  - target/<host-triple>/release/SubRE (cargo build --release --target <host-triple>)\n' >&2
    fi
    printf '  - target/release/SubRE (cargo build --release, legacy fallback)\n' >&2
    printf 'Run "cargo build --release --target %s" or extract the release tarball first.\n' "${host_target:-<host-triple>}" >&2
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
