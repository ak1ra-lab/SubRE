#!/bin/bash
# Build Windows `.ico` and macOS `.icns` from a single `assets/SubRE.svg`.
#
# Requires `svgy` (https://github.com/ystorian/svgy) on $PATH.
# One-time install: `cargo install svgy`
#
# Re-run only when the SVG changes; the generated .ico and .icns are
# committed to the repo and the release workflow just `cp`s them.

set -o errexit -o nounset

svgy assets/SubRE.svg \
    --ico=assets/SubRE.ico \
    --icns=assets/SubRE.icns
