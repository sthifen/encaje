#!/usr/bin/env bash
# Arranca `encaje --fuente-prueba` (sin escritorio) y le aplica stress.py.
#   tests/stress/run-local.sh [binario] [segundos] [opciones de encaje…]
# p. ej.: tests/stress/run-local.sh target/release/encaje 60 --encoder openh264 --fps 90
# Variables: PORT (8799), OUT (carpeta de resultados), STRESS_ARGS (cargas).
set -euo pipefail
BIN="${1:-target/release/encaje}"; SECS="${2:-30}"; shift 2 || true
PORT="${PORT:-8799}"
OUT="${OUT:-$(mktemp -d)}"
mkdir -p "$OUT"
"$BIN" --fuente-prueba --no-usb --port "$PORT" "$@" >"$OUT/server.log" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null || true' EXIT
for _ in $(seq 50); do (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && break; sleep 0.1; done
set +e
# shellcheck disable=SC2086
python3 "$(dirname "$0")/stress.py" --url "ws://127.0.0.1:$PORT" --pid "$PID" --seconds "$SECS" \
  ${STRESS_ARGS:---viewers 6 --slow 2 --churn 4 --input 4} --json "$OUT/result.json"
rc=$?
kill -0 $PID 2>/dev/null || { echo "✗ el servidor ya no está vivo"; rc=1; }
echo "── servidor ($OUT/server.log) ──"
grep -E "codificador:|captura .* fps|WARN|ERROR|pánico" "$OUT/server.log" | tail -6
exit $rc
