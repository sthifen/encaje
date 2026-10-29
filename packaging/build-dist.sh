#!/usr/bin/env bash
# Genera los paquetes para Linux en dist/:
#   encaje-<ver>-x86_64.tar.gz            binario + acceso directo + install.sh
#   Encaje-x86_64.AppImage        todo en un archivo ejecutable (nombre fijo, para
#                                 enlazar releases/latest/download/ desde el README)
#
# El binario enlaza dinámicamente con PipeWire y GStreamer del sistema (no se
# empaquetan: deben coincidir con los del escritorio). Se compila en un
# contenedor de Ubuntu 22.04 (packaging/compat/) para que arranque en
# cualquier distro con glibc ≥ 2.34: Zorin OS 17, Ubuntu 22.04+, Mint 21+,
# Debian 12, Fedora, Arch, openSUSE… (tests/distros/run.sh lo comprueba).
# Requisitos en destino: GNOME o KDE Plasma 6 sobre Wayland, PipeWire y
# GStreamer ≥ 1.20; para NVENC, gstreamer1-plugins-bad con el plugin nvcodec.
#
# NATIVE=1 compila con el sistema actual (más rápido, pero el binario sólo
# arranca en distros con una glibc tan nueva como la de este equipo).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG="$ROOT/packaging"
DIST="$ROOT/dist"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
ARCH=x86_64
TOOL="$ROOT/target/appimagetool-$ARCH.AppImage"

echo "→ compilando v$VERSION"
if [[ "${NATIVE:-}" == 1 ]]; then
  cargo build --release --manifest-path "$ROOT/Cargo.toml"
  BUILT="$ROOT/target/release/encaje"
else
  "$PKG/compat/build.sh"
  BUILT="$ROOT/target/compat/release/encaje"
fi
rm -rf "$DIST" && mkdir -p "$DIST"
BIN="$DIST/encaje"
cp "$BUILT" "$BIN"
strip "$BIN"

echo "→ tar.gz"
NAME="encaje-$VERSION-$ARCH"
STAGE="$DIST/$NAME"
mkdir -p "$STAGE"
cp "$BIN" "$PKG/encaje-launch" "$PKG/encaje.desktop" \
   "$PKG/encaje.svg" "$PKG/install.sh" "$STAGE/"
chmod 755 "$STAGE/encaje" "$STAGE/encaje-launch" "$STAGE/install.sh"
tar -C "$DIST" -czf "$DIST/$NAME.tar.gz" "$NAME"
rm -rf "$STAGE"

echo "→ AppImage"
APPDIR="$DIST/AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
         "$APPDIR/usr/share/icons/hicolor/scalable/apps"
install -m 755 "$BIN" "$APPDIR/usr/bin/encaje"
install -m 755 "$PKG/encaje-launch" "$APPDIR/AppRun"
install -m 644 "$PKG/encaje.svg" "$APPDIR/encaje.svg"
install -m 644 "$PKG/encaje.svg" "$APPDIR/usr/share/icons/hicolor/scalable/apps/"
sed "s|^Exec=.*|Exec=encaje-launch|; \$a X-AppImage-Version=$VERSION" \
    "$PKG/encaje.desktop" > "$APPDIR/encaje.desktop"
cp "$APPDIR/encaje.desktop" "$APPDIR/usr/share/applications/"
if [[ ! -x "$TOOL" ]]; then
  curl -fL --progress-bar -o "$TOOL" \
    "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$ARCH.AppImage"
  chmod +x "$TOOL"
fi
APPIMAGE_EXTRACT_AND_RUN=1 ARCH=$ARCH "$TOOL" --no-appstream "$APPDIR" \
  "$DIST/Encaje-$ARCH.AppImage" >/dev/null
rm -rf "$APPDIR" "$BIN"

echo "✓ dist/:"
ls -lh "$DIST" | tail -n +2
