#!/usr/bin/env bash
# Compila el binario de los paquetes en el contenedor de Ubuntu 22.04 (ver
# Containerfile) con podman o docker. Deja target/compat/release/encaje.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ENGINE="${ENGINE:-$(command -v podman || command -v docker)}"
IMAGE=encaje-compat:22.04
"$ENGINE" build -q -t "$IMAGE" "$ROOT/packaging/compat" >/dev/null
mkdir -p "$ROOT/target/compat-registry"
"$ENGINE" run --rm -v "$ROOT":/src:Z -v "$ROOT/target/compat-registry":/root/.cargo/registry:Z \
  -w /src -e CARGO_TARGET_DIR=/src/target/compat "$IMAGE" \
  sh -c 'cargo build --release && packaging/compat/check-symbols.sh target/compat/release/encaje'
