#!/usr/bin/env bash
# Regénère les BMPs WiX (banner + dialog) à partir des SVG sources.
#
# Cibles WiX MSI Tauri (cf. tauri.conf.json → windows.wix) :
#   - banner.bmp : 493×58  px, top banner sur les écrans intermédiaires
#   - dialog.bmp : 493×312 px, sidebar du Welcome + Exit
#
# Format BMP : Windows 3.x 24 bits (BMP3) sans canal alpha — exigence WiX.
#
# Dépendances : `rsvg-convert` (librsvg) + ImageMagick (`magick`).
# Sur macOS : `brew install librsvg imagemagick`.

set -euo pipefail
cd "$(dirname "$0")"

rsvg-convert -w 493 -h 312 -o dialog.png dialog.svg
rsvg-convert -w 493 -h 58  -o banner.png banner.svg

magick dialog.png -define bmp:format=bmp3 dialog.bmp
magick banner.png -define bmp:format=bmp3 banner.bmp

rm -f dialog.png banner.png

echo "✓ dialog.bmp et banner.bmp régénérés"
file dialog.bmp banner.bmp
