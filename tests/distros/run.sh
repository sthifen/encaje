#!/usr/bin/env bash
# Prueba el binario de los paquetes (packaging/compat/build.sh) en varias
# distros, cada una en su contenedor con sólo las bibliotecas de ejecución:
#
#   1. arranca (`--version`): enlazado dinámico, glibc, símbolos;
#   2. `--diagnostico` funciona sin escritorio;
#   3. `--fuente-prueba` + tests/stress/stress.py desde fuera durante N s.
#
# Uso: tests/distros/run.sh [segundos] [distro…]
#   distros: ubuntu22 ubuntu24 debian12 fedora arch opensuse (por defecto, todas)
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ENGINE="${ENGINE:-$(command -v podman || command -v docker)}"
BIN="${BIN:-$ROOT/target/compat/release/encaje}"
SECS="${1:-20}"; shift || true
DISTROS=("${@:-ubuntu22 ubuntu24 debian12 fedora arch opensuse}")
# shellcheck disable=SC2206
DISTROS=(${DISTROS[*]})
OUT="${OUT:-$ROOT/target/distro-tests}"
mkdir -p "$OUT"
[[ -x "$BIN" ]] || { echo "falta $BIN: ejecuta packaging/compat/build.sh"; exit 1; }

image_for() {
  local deb="libgtk-4-1 libgstreamer1.0-0 libgstreamer-plugins-base1.0-0 gstreamer1.0-plugins-base libpipewire-0.3-0"
  case "$1" in
    ubuntu22) echo "docker.io/library/ubuntu:22.04|apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends $deb";;
    ubuntu24) echo "docker.io/library/ubuntu:24.04|apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ${deb/libpipewire-0.3-0/libpipewire-0.3-0t64}";;
    debian12) echo "docker.io/library/debian:12|apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends $deb";;
    fedora)   echo "registry.fedoraproject.org/fedora:latest|dnf install -y gtk4 gstreamer1 gstreamer1-plugins-base pipewire-libs";;
    arch)     echo "docker.io/library/archlinux:latest|pacman -Sy --noconfirm gtk4 gstreamer gst-plugins-base libpipewire";;
    opensuse) echo "registry.opensuse.org/opensuse/tumbleweed:latest|zypper -n install libgtk-4-1 gstreamer gstreamer-plugins-base libpipewire-0_3-0";;
    *) return 1;;
  esac
}

declare -A RESULT
port=8810
for d in "${DISTROS[@]}"; do
  spec="$(image_for "$d")" || { echo "distro desconocida: $d"; continue; }
  base="${spec%%|*}"; install="${spec#*|}"
  tag="encaje-runtime:$d"
  echo "━━ $d ($base)"
  if ! "$ENGINE" image exists "$tag" 2>/dev/null; then
    printf 'FROM %s\nRUN %s\n' "$base" "$install" | "$ENGINE" build -q -t "$tag" -f - "$ROOT/tests/distros" >"$OUT/$d-build.log" 2>&1 \
      || { echo "  ✗ no se pudo preparar la imagen (ver $OUT/$d-build.log)"; RESULT[$d]="imagen"; continue; }
  fi
  run=("$ENGINE" run --rm --network host -v "$BIN:/encaje:ro,Z" "$tag")
  if ! "${run[@]}" /encaje --version >"$OUT/$d-version.log" 2>&1; then
    echo "  ✗ no arranca:"; sed 's/^/    /' "$OUT/$d-version.log" | head -5; RESULT[$d]="no arranca"; continue
  fi
  echo "  ✓ arranca: $(cat "$OUT/$d-version.log")"
  if "${run[@]}" /encaje --diagnostico >"$OUT/$d-diagnostico.md" 2>&1; then
    echo "  ✓ --diagnostico ($(grep -m1 'Distro:' "$OUT/$d-diagnostico.md" | cut -c3-))"
  else
    echo "  ✗ --diagnostico falló (ver $OUT/$d-diagnostico.md)"; RESULT[$d]="diagnóstico"; continue
  fi
  name="encaje-stress-$d"
  "$ENGINE" rm -f "$name" >/dev/null 2>&1
  "$ENGINE" run -d --name "$name" --network host -v "$BIN:/encaje:ro,Z" "$tag" \
    /encaje --fuente-prueba --no-usb --port $port --encoder openh264 --width 1280 --height 720 >/dev/null
  for _ in $(seq 50); do (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && break; sleep 0.2; done
  pid="$("$ENGINE" inspect -f '{{.State.Pid}}' "$name")"
  if python3 "$ROOT/tests/stress/stress.py" --url "ws://127.0.0.1:$port" --pid "$pid" --seconds "$SECS" \
      --viewers 3 --slow 1 --churn 2 --input 2 --json "$OUT/$d-stress.json" >"$OUT/$d-stress.log" 2>&1; then
    echo "  ✓ estrés: $(python3 -c "import json,sys; r=json.load(open(sys.argv[1])); print(f\"{r['fps_por_visor']} fps/visor, {sum(r['conexiones'].values())} conexiones, RSS {r.get('rss_mb',{}).get('inicio')}→{r.get('rss_mb',{}).get('final')} MB\")" "$OUT/$d-stress.json")"
    RESULT[$d]="ok"
  else
    echo "  ✗ estrés (ver $OUT/$d-stress.log):"; tail -5 "$OUT/$d-stress.log" | sed 's/^/    /'
    RESULT[$d]="estrés"
  fi
  "$ENGINE" logs "$name" >"$OUT/$d-server.log" 2>&1
  "$ENGINE" rm -f "$name" >/dev/null 2>&1
  port=$((port + 1))
done

echo "━━ resumen"
fail=0
for d in "${DISTROS[@]}"; do
  printf '  %-10s %s\n' "$d" "${RESULT[$d]:-?}"
  [[ "${RESULT[$d]:-}" == ok ]] || fail=1
done
exit $fail
