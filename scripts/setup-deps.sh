#!/usr/bin/env bash
# Prepara las cabeceras de PipeWire que necesita el crate `pipewire` (bindgen)
# sin usar sudo: descarga el RPM pipewire-devel y lo extrae en .deps/.
# .cargo/config.toml apunta PKG_CONFIG_PATH ahí.
#
# Alternativa con permisos de administrador (y entonces puedes borrar el
# bloque [env] de .cargo/config.toml):
#   sudo dnf install pipewire-devel clang-devel gstreamer1-devel gstreamer1-plugins-base-devel
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEPS="$ROOT/.deps"

rm -rf "$DEPS" && mkdir -p "$DEPS/rpms"
cd "$DEPS/rpms"
dnf download pipewire-devel
rm -f ./*.i686.rpm  # sólo la arquitectura nativa
cd "$DEPS"
for r in rpms/*.rpm; do rpm2cpio "$r" | cpio -idm --quiet; done

# El .so de desarrollo es un enlace relativo: apuntarlo a la librería del sistema.
ln -sf /usr/lib64/libpipewire-0.3.so.0 usr/lib64/libpipewire-0.3.so
sed -i "s|^prefix=.*|prefix=$DEPS/usr|" usr/lib64/pkgconfig/*.pc

PKG_CONFIG_PATH="$DEPS/usr/lib64/pkgconfig" pkg-config --modversion libpipewire-0.3 \
  | sed 's/^/libpipewire-0.3 /'
echo "✓ listo: cargo build --release"
