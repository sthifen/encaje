#!/bin/sh
# Instala Encaje para el usuario actual (sin sudo):
#   ~/.local/bin/                      binario y lanzador
#   ~/.local/share/applications/       acceso directo del menú
#   ~/.local/share/icons/hicolor/...   icono
# Desinstalar:  ./install.sh --uninstall
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"
ICON_DIR="$HOME/.local/share/icons/hicolor/scalable/apps"

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$BIN_DIR/encaje" "$BIN_DIR/encaje-launch" \
          "$APP_DIR/encaje.desktop" "$ICON_DIR/encaje.svg"
    echo "Encaje desinstalado."
    exit 0
fi

mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR"
install -m 755 "$HERE/encaje" "$BIN_DIR/encaje"
install -m 755 "$HERE/encaje-launch" "$BIN_DIR/encaje-launch"
install -m 644 "$HERE/encaje.svg" "$ICON_DIR/encaje.svg"
# Ruta absoluta: ~/.local/bin puede no estar en el PATH del menú.
sed "s|^Exec=.*|Exec=$BIN_DIR/encaje-launch|" \
    "$HERE/encaje.desktop" > "$APP_DIR/encaje.desktop"
chmod 644 "$APP_DIR/encaje.desktop"
command -v update-desktop-database >/dev/null && update-desktop-database "$APP_DIR" 2>/dev/null || true

echo "✓ Instalado. Busca «Encaje» en el menú de aplicaciones"
echo "  o ejecuta: encaje-launch"
