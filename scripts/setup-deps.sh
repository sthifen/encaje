#!/usr/bin/env bash
# Prepara las cabeceras de PipeWire que necesita el crate `pipewire` (bindgen)
# sin usar sudo: descarga el RPM pipewire-devel y lo extrae en .deps/.
# .cargo/config.toml apunta PKG_CONFIG_PATH ahí.
#
# Alternativa con permisos de administrador (y entonces puedes borrar el
# bloque [env] de .cargo/config.toml):
#   Fedora: sudo dnf install pipewire-devel clang-devel gstreamer1-devel gstreamer1-plugins-base-devel gtk4-devel
#   Debian/Ubuntu: sudo apt install libpipewire-0.3-dev libclang-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgtk-4-dev
set -euo pipefail

if pkg-config --exists libpipewire-0.3; then
  pkg-config --modversion libpipewire-0.3 | sed 's/^/libpipewire-0.3 (sistema) /'
  echo "✓ listo: cargo build --release"
  exit 0
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEPS="$ROOT/.deps"

if command -v dnf >/dev/null 2>&1; then
  rm -rf "$DEPS" && mkdir -p "$DEPS/rpms"
  cd "$DEPS/rpms"
  dnf download pipewire-devel
  rm -f ./*.i686.rpm  # sólo la arquitectura nativa
  cd "$DEPS"
  for r in rpms/*.rpm; do rpm2cpio "$r" | cpio -idm --quiet; done

  # El .so de desarrollo es un enlace relativo: apuntarlo a la librería del sistema.
  ln -sf /usr/lib64/libpipewire-0.3.so.0 usr/lib64/libpipewire-0.3.so
  sed -i "s|^prefix=.*|prefix=$DEPS/usr|" usr/lib64/pkgconfig/*.pc
elif command -v apt-get >/dev/null 2>&1; then
  rm -rf "$DEPS" && mkdir -p "$DEPS/debs" "$DEPS/usr/lib64/pkgconfig"
  cd "$DEPS/debs"
  apt-get download libpipewire-0.3-dev libspa-0.2-dev
  cd "$DEPS"
  for d in debs/*.deb; do dpkg-deb -x "$d" "$DEPS"; done
  ARCH="$(dpkg-architecture -qDEB_HOST_MULTIARCH)"
  cp -f "usr/lib/$ARCH/pkgconfig/"*.pc usr/lib64/pkgconfig/
  sed -i "s|^prefix=.*|prefix=$DEPS/usr|" usr/lib64/pkgconfig/*.pc
else
  echo "Instala las cabeceras de libpipewire-0.3 y gstreamer-1.0 con el gestor de paquetes de tu distribución." >&2
  exit 1
fi

PKG_CONFIG_PATH="$DEPS/usr/lib64/pkgconfig" pkg-config --modversion libpipewire-0.3 \
  | sed 's/^/libpipewire-0.3 /'
echo "✓ listo: cargo build --release"
