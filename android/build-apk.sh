#!/usr/bin/env bash
# Compila la app Android sin Gradle ni Android Studio:
#   aapt2 (recursos) → javac (vía el módulo jdk.compiler del JRE) → d8 (dex)
#   → zipalign → apksigner (clave de depuración).
#
# Si faltan, descarga build-tools y la plataforma en $ANDROID_HOME
# (por defecto ~/Android/Sdk). Su uso está sujeto a la licencia del Android SDK:
# https://developer.android.com/studio/terms
#
# También compila android/input-server.jar (modo --solo-entrada), que el
# binario de Linux lleva incrustado.
#
# Uso:  android/build-apk.sh [--install]
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SDK="${ANDROID_HOME:-$HOME/Android/Sdk}"
BT_VER=36.1.0
BT_ZIP=build-tools_r36.1_linux.zip
PLATFORM=android-36
PLATFORM_ZIP=platform-36_r02.zip
REPO=https://dl.google.com/android/repository
MIN_SDK=24

# La versión de la app es la del programa (Cargo.toml): 0.8.0 → código 800.
VERSION="$(grep -m1 '^version' "$HERE/../Cargo.toml" | cut -d'"' -f2)"
IFS=. read -r V_MAJOR V_MINOR V_PATCH <<<"$VERSION"
VERSION_CODE=$((V_MAJOR * 10000 + V_MINOR * 100 + V_PATCH))

BT="$SDK/build-tools/$BT_VER"
ANDROID_JAR="$SDK/platforms/$PLATFORM/android.jar"
OUT="$HERE/build"
APK="$HERE/encaje.apk"

fetch() { # fetch <zip> <destino>
  local tmp; tmp="$(mktemp -d)"
  echo "→ descargando $1"
  curl -fL --progress-bar "$REPO/$1" -o "$tmp/pkg.zip"
  unzip -q "$tmp/pkg.zip" -d "$tmp/x"
  mkdir -p "$(dirname "$2")"
  mv "$tmp/x/"* "$2"
  rm -rf "$tmp"
}
[[ -x "$BT/aapt2" ]] || fetch "$BT_ZIP" "$BT"
[[ -f "$ANDROID_JAR" ]] || fetch "$PLATFORM_ZIP" "$SDK/platforms/$PLATFORM"

# javac: usa el del sistema si existe; si no, el módulo jdk.compiler del JRE.
if command -v javac >/dev/null; then JAVAC=(javac); else JAVAC=(java -m jdk.compiler/com.sun.tools.javac.Main); fi

rm -rf "$OUT" && mkdir -p "$OUT"/{gen,classes,dex}

echo "→ recursos"
"$BT/aapt2" compile --dir "$HERE/res" -o "$OUT/res.zip"
"$BT/aapt2" link -o "$OUT/base.apk" -I "$ANDROID_JAR" \
  --manifest "$HERE/AndroidManifest.xml" -A "$HERE/assets" \
  --java "$OUT/gen" --min-sdk-version $MIN_SDK --target-sdk-version 36 \
  --version-code "$VERSION_CODE" --version-name "$VERSION" "$OUT/res.zip"

echo "→ java"
"${JAVAC[@]}" -source 17 -target 17 -nowarn -Xlint:-options -encoding UTF-8 \
  -classpath "$ANDROID_JAR" -d "$OUT/classes" \
  $(find "$HERE/src" "$OUT/gen" -name '*.java')
"${JAVAC[@]}" -source 17 -target 17 -nowarn -Xlint:-options -encoding UTF-8 \
  -classpath "$ANDROID_JAR" -d "$OUT/server-classes" \
  $(find "$HERE/server" -name '*.java')

echo "→ dex"
"$BT/d8" --release --min-api $MIN_SDK --lib "$ANDROID_JAR" --output "$OUT/dex" \
  $(find "$OUT/classes" -name '*.class')
(cd "$OUT/dex" && zip -q "$OUT/base.apk" classes.dex)
mkdir -p "$OUT/server-dex"
"$BT/d8" --release --min-api $MIN_SDK --lib "$ANDROID_JAR" --output "$OUT/server-dex" \
  $(find "$OUT/server-classes" -name '*.class')
rm -f "$HERE/input-server.jar"
(cd "$OUT/server-dex" && zip -q -X "$HERE/input-server.jar" classes.dex)

echo "→ firma"
KEYSTORE="$HERE/debug.keystore"
if [[ ! -f "$KEYSTORE" ]]; then
  keytool -genkeypair -keystore "$KEYSTORE" -storepass android -keypass android \
    -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 \
    -dname "CN=Encaje Debug" >/dev/null 2>&1
fi
"$BT/zipalign" -P 16 -f 4 "$OUT/base.apk" "$OUT/aligned.apk"
"$BT/apksigner" sign --ks "$KEYSTORE" --ks-pass pass:android --key-pass pass:android \
  --out "$APK" "$OUT/aligned.apk"
"$BT/apksigner" verify "$APK"
echo "✓ $APK ($(du -h "$APK" | cut -f1))"
echo "✓ $HERE/input-server.jar ($(du -h "$HERE/input-server.jar" | cut -f1))"

if [[ "${1:-}" == "--install" ]]; then
  adb install -r "$APK"
  adb shell am start -n dev.encaje/.MainActivity
fi
