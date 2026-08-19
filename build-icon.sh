#!/bin/bash
# sudo apt install imagemagick librsvg2-bin

set -o errexit -o nounset

build_icon() {
    icon_svg="$1"
    icon_base="${icon_svg%.svg}"

    rsvg-convert -b none "${icon_svg}" >"${icon_base}.png"

    for size in 256 128 64 48 32 16; do
        rsvg-convert -b none -w "${size}" -h "${size}" "${icon_svg}" >"${icon_base}-${size}.png"
    done

    # 使用 ImageMagick 打包为 ICO
    magick "${icon_base}"-{256,128,64,48,32,16}.png \
        -colorspace sRGB \
        -type truecoloralpha \
        -define icon:auto-resize=256,128,64,48,32,16 \
        -strip "${icon_base}.ico"

    # 清理临时 PNG
    rm "${icon_base}"-{256,128,64,48,32,16}.png
}

build_icon "$@"
