#!/bin/sh
# Instala Spacedesk Linux para el usuario actual (sin sudo):
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
    rm -f "$BIN_DIR/linux-spacedesk-clone" "$BIN_DIR/spacedesk-linux" \
          "$APP_DIR/spacedesk-linux.desktop" "$ICON_DIR/spacedesk-linux.svg"
    echo "Spacedesk Linux desinstalado."
    exit 0
fi

mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR"
install -m 755 "$HERE/linux-spacedesk-clone" "$BIN_DIR/linux-spacedesk-clone"
install -m 755 "$HERE/spacedesk-linux" "$BIN_DIR/spacedesk-linux"
install -m 644 "$HERE/spacedesk-linux.svg" "$ICON_DIR/spacedesk-linux.svg"
# Ruta absoluta: ~/.local/bin puede no estar en el PATH del menú.
sed "s|^Exec=.*|Exec=$BIN_DIR/spacedesk-linux|" \
    "$HERE/spacedesk-linux.desktop" > "$APP_DIR/spacedesk-linux.desktop"
chmod 644 "$APP_DIR/spacedesk-linux.desktop"
command -v update-desktop-database >/dev/null && update-desktop-database "$APP_DIR" 2>/dev/null || true

echo "✓ Instalado. Busca «Spacedesk Linux» en el menú de aplicaciones"
echo "  o ejecuta: spacedesk-linux"
