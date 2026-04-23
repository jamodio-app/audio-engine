#!/usr/bin/env bash
# ═══════════════════════════════════════════════════════════════
#  Régénère les icônes Jamodio Audio Engine depuis les SVG sources
# ═══════════════════════════════════════════════════════════════
#
# Prérequis (une fois) :
#   brew install librsvg imagemagick
#
# Usage (depuis la racine du repo agent) :
#   ./jamodio-agent/icons/src/regenerate.sh
#
# Produit les fichiers attendus par tauri.conf.json :
#   icons/32x32.png        — Linux / petite taille
#   icons/128x128.png      — Linux / medium
#   icons/128x128@2x.png   — Linux high-DPI (256x256)
#   icons/icon.png         — fallback générique
#   icons/icon.icns        — macOS app icon (multi-tailles bundle)
#   icons/icon.ico         — Windows app icon
#   icons/tray.png         — tray macOS (monochrome template)

set -euo pipefail

# Résoudre les chemins quelle que soit la localisation de l'appel
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC_DIR="$SCRIPT_DIR"
OUT_DIR="$SCRIPT_DIR/.."

APP_SVG="$SRC_DIR/jamodio-app-icon.svg"
TRAY_SVG="$SRC_DIR/jamodio-tray.svg"

command -v rsvg-convert >/dev/null || { echo "❌ rsvg-convert introuvable. brew install librsvg"; exit 1; }
command -v magick >/dev/null || command -v convert >/dev/null || { echo "❌ ImageMagick introuvable. brew install imagemagick"; exit 1; }

# Helper : magick ou convert selon version installée
if command -v magick >/dev/null; then MAGICK="magick"; else MAGICK="convert"; fi

echo "▸ PNG multi-tailles depuis $APP_SVG"
rsvg-convert -w 32   -h 32   "$APP_SVG" -o "$OUT_DIR/32x32.png"
rsvg-convert -w 128  -h 128  "$APP_SVG" -o "$OUT_DIR/128x128.png"
rsvg-convert -w 256  -h 256  "$APP_SVG" -o "$OUT_DIR/128x128@2x.png"
rsvg-convert -w 512  -h 512  "$APP_SVG" -o "$OUT_DIR/icon.png"

echo "▸ Bundle .icns (macOS) via iconutil"
ICONSET="$OUT_DIR/icon.iconset"
rm -rf "$ICONSET"; mkdir "$ICONSET"
for size in 16 32 64 128 256 512 1024; do
  rsvg-convert -w $size -h $size "$APP_SVG" -o "$ICONSET/icon_${size}x${size}.png"
done
# Apple veut aussi des @2x pour chaque taille (sauf 1024)
for size in 16 32 128 256 512; do
  dbl=$((size * 2))
  rsvg-convert -w $dbl -h $dbl "$APP_SVG" -o "$ICONSET/icon_${size}x${size}@2x.png"
done
iconutil -c icns -o "$OUT_DIR/icon.icns" "$ICONSET"
rm -rf "$ICONSET"

echo "▸ Bundle .ico (Windows) via ImageMagick"
TMP_WIN=$(mktemp -d)
for size in 16 32 48 64 128 256; do
  rsvg-convert -w $size -h $size "$APP_SVG" -o "$TMP_WIN/icon_${size}.png"
done
$MAGICK "$TMP_WIN"/icon_16.png "$TMP_WIN"/icon_32.png "$TMP_WIN"/icon_48.png \
        "$TMP_WIN"/icon_64.png "$TMP_WIN"/icon_128.png "$TMP_WIN"/icon_256.png \
        "$OUT_DIR/icon.ico"
rm -rf "$TMP_WIN"

echo "▸ Tray icon (monochrome template, 32x32 + @2x)"
rsvg-convert -w 32 -h 32 "$TRAY_SVG" -o "$OUT_DIR/tray.png"
rsvg-convert -w 64 -h 64 "$TRAY_SVG" -o "$OUT_DIR/tray@2x.png" 2>/dev/null || true

echo "✓ Icônes régénérées dans $OUT_DIR/"
echo "  Commit + push dans le repo audio-engine + nouveau tag (v0.1.1) pour publier."
