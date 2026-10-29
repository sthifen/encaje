#!/usr/bin/env bash
# Comprueba que todos los símbolos que pide el binario existen en las
# bibliotecas de este sistema (dentro del contenedor: las de Ubuntu 22.04) y
# muestra la glibc mínima que exige.
set -euo pipefail
BIN="$1"
echo "glibc mínima: $(objdump -T "$BIN" | grep -oE 'GLIBC_[0-9.]+' | sort -Vu | tail -1)"
need="$(nm -D --undefined-only "$BIN" | awk '$1 == "U" {print $2}' | sed 's/@.*//' | sort -u)"
have="$(ldd "$BIN" | awk '/=> \// {print $3} $1 ~ /^\// {print $1}' | xargs -r nm -D --defined-only 2>/dev/null \
        | awk '{print $3}' | sed 's/@.*//' | sort -u)"
lost="$(comm -23 <(echo "$need") <(echo "$have"))"
if [[ -n "$lost" ]]; then
  echo "✗ símbolos que no existen en este sistema:"; echo "$lost" | sed 's/^/    /'
  exit 1
fi
echo "✓ los $(echo "$need" | wc -l) símbolos importados existen en las bibliotecas de este sistema"
