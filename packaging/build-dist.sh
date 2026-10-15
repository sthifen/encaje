#!/usr/bin/env bash
# Genera los paquetes para Linux en dist/:
#   spacedesk-linux-<ver>-x86_64.tar.gz   binario + acceso directo + install.sh
#   Spacedesk_Linux-<ver>-x86_64.AppImage  todo en un archivo ejecutable
#
# El binario enlaza dinámicamente con PipeWire y GStreamer del sistema (no se
# empaquetan: deben coincidir con los del escritorio). Requisitos en destino:
# GNOME sobre Wayland, PipeWire y GStreamer ≥ 1.20 (presentes en cualquier
# GNOME moderno); para NVENC, gstreamer1-plugins-bad con el plugin nvcodec.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG="$ROOT/packaging"
DIST="$ROOT/dist"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
ARCH=x86_64
TOOL="$ROOT/target/appimagetool-$ARCH.AppImage"

echo "→ compilando v$VERSION"
cargo build --release --manifest-path "$ROOT/Cargo.toml"
rm -rf "$DIST" && mkdir -p "$DIST"
BIN="$DIST/linux-spacedesk-clone"
cp "$ROOT/target/release/linux-spacedesk-clone" "$BIN"
strip "$BIN"

echo "→ tar.gz"
NAME="spacedesk-linux-$VERSION-$ARCH"
STAGE="$DIST/$NAME"
mkdir -p "$STAGE"
cp "$BIN" "$PKG/spacedesk-linux" "$PKG/spacedesk-linux.desktop" \
   "$PKG/spacedesk-linux.svg" "$PKG/install.sh" "$STAGE/"
chmod 755 "$STAGE/linux-spacedesk-clone" "$STAGE/spacedesk-linux" "$STAGE/install.sh"
tar -C "$DIST" -czf "$DIST/$NAME.tar.gz" "$NAME"
rm -rf "$STAGE"

echo "→ AppImage"
APPDIR="$DIST/AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/scalable/apps"
install -m 755 "$BIN" "$APPDIR/usr/bin/linux-spacedesk-clone"
install -m 755 "$PKG/spacedesk-linux" "$APPDIR/AppRun"
install -m 644 "$PKG/spacedesk-linux.svg" "$APPDIR/spacedesk-linux.svg"
install -m 644 "$PKG/spacedesk-linux.svg" "$APPDIR/usr/share/icons/hicolor/scalable/apps/"
sed "s|^Exec=.*|Exec=spacedesk-linux|; \$a X-AppImage-Version=$VERSION" \
    "$PKG/spacedesk-linux.desktop" > "$APPDIR/spacedesk-linux.desktop"
cp "$APPDIR/spacedesk-linux.desktop" "$APPDIR/usr/share/applications/"
if [[ ! -x "$TOOL" ]]; then
  curl -fL --progress-bar -o "$TOOL" \
    "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage"
  chmod +x "$TOOL"
fi
APPIMAGE_EXTRACT_AND_RUN=1 ARCH=$ARCH "$TOOL" --no-appstream "$APPDIR" \
  "$DIST/Spacedesk_Linux-$VERSION-$ARCH.AppImage" >/dev/null
rm -rf "$APPDIR" "$BIN"

echo "✓ dist/:"
ls -lh "$DIST" | tail -n +2
