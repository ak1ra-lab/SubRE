#!/usr/bin/env bash
# Package the built binary into a platform-specific archive.
# Called by .github/workflows/release.yaml from each build matrix instance.
#
# Reads from env vars (set by the workflow's env: block on the calling step):
#   PACKAGE_OS           - ubuntu-latest | windows-latest | macos-latest
#   PACKAGE_TARGET       - cargo target triple (e.g. x86_64-unknown-linux-gnu)
#   PACKAGE_TARGET_DIR   - cargo target output dir (e.g. target/x86_64-unknown-linux-gnu/release)
#   PACKAGE_BINARY_NAME  - binary name as built (e.g. SubRE, SubRE.exe)
#   PACKAGE_VERSION      - package version (required)

set -euo pipefail

: "${PACKAGE_OS:?PACKAGE_OS not set}"
: "${PACKAGE_TARGET:?PACKAGE_TARGET not set}"
: "${PACKAGE_TARGET_DIR:?PACKAGE_TARGET_DIR not set}"
: "${PACKAGE_BINARY_NAME:?PACKAGE_BINARY_NAME not set}"
: "${PACKAGE_VERSION:?PACKAGE_VERSION not set}"

ARCHIVE_BASE="SubRE-${PACKAGE_VERSION}-${PACKAGE_TARGET}"

mkdir -p "dist/${ARCHIVE_BASE}"

case "${PACKAGE_OS}" in
    macos-latest)
        # Construct SubRE.app bundle layout.
        APP_DIR="dist/${ARCHIVE_BASE}/SubRE.app"
        mkdir -p "${APP_DIR}/Contents/MacOS" "${APP_DIR}/Contents/Resources"
        cp "${PACKAGE_TARGET_DIR}/${PACKAGE_BINARY_NAME}" "${APP_DIR}/Contents/MacOS/SubRE"
        cp "assets/SubRE.icns" "${APP_DIR}/Contents/Resources/SubRE.icns"
        cat >"${APP_DIR}/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>SubRE</string>
    <key>CFBundleIdentifier</key>
    <string>lab.ak1ra.subre</string>
    <key>CFBundleName</key>
    <string>SubRE</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${PACKAGE_VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${PACKAGE_VERSION}</string>
    <key>CFBundleIconFile</key>
    <string>SubRE</string>
    <key>LSMinimumSystemVersion</key>
    <string>10.13.0</string>
</dict>
</plist>
EOF
        cp LICENSE "${APP_DIR}/Contents/Resources/LICENSE"
        codesign --force --sign - "${APP_DIR}"
        (cd dist/ && 7z a -tzip "../${ARCHIVE_BASE}.zip" "${ARCHIVE_BASE}")
        ;;
    windows-latest)
        mkdir -p "dist/${ARCHIVE_BASE}"
        cp "${PACKAGE_TARGET_DIR}/${PACKAGE_BINARY_NAME}" "dist/${ARCHIVE_BASE}/SubRE.exe"
        cp LICENSE "dist/${ARCHIVE_BASE}/"
        (cd dist/ && 7z a -tzip "../${ARCHIVE_BASE}.zip" "${ARCHIVE_BASE}")
        ;;
    *)
        # Linux (or other unix): tar.gz with binary + install.sh + assets/ + LICENSE
        # inside a versioned directory.
        mkdir -p "dist/${ARCHIVE_BASE}"
        cp "${PACKAGE_TARGET_DIR}/${PACKAGE_BINARY_NAME}" "dist/${ARCHIVE_BASE}/SubRE"
        cp install.sh "dist/${ARCHIVE_BASE}/"
        cp -r assets "dist/${ARCHIVE_BASE}/"
        cp LICENSE "dist/${ARCHIVE_BASE}/"
        tar -czf "${ARCHIVE_BASE}.tar.gz" -C dist "${ARCHIVE_BASE}"
        ;;
esac
