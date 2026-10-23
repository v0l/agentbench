#!/usr/bin/env bash
# Build the Android APK: the Rust app through cargo-apk, then the Kotlin background service,
# the manifest and the signature on top. Output: target/android/foreman.apk
#
#   ANDROID_HOME   the SDK (default ~/Android/Sdk)
#   ANDROID_NDK_ROOT   the NDK (default: the newest under $ANDROID_HOME/ndk)
#   KOTLIN_HOME    a kotlinc install (default: downloaded to ~/.cache/foreman-android)
#   KEYSTORE / KEYSTORE_PASS / KEY_ALIAS   to sign a release; the debug key is used otherwise
#   RELEASE=1      a release build: not debuggable, built with --release
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$PWD
SDK=${ANDROID_HOME:-$HOME/Android/Sdk}
export ANDROID_HOME=$SDK
export ANDROID_NDK_ROOT=${ANDROID_NDK_ROOT:-$(ls -d "$SDK"/ndk/* | sort -V | tail -1)}
export ANDROID_NDK_HOME=$ANDROID_NDK_ROOT
BT=$(ls -d "$SDK"/build-tools/* | sort -V | tail -1)
JAR=$(ls -d "$SDK"/platforms/android-3[4-9]* "$SDK"/platforms/android-34 2>/dev/null | sort -V | tail -1)/android.jar
KOTLIN_VERSION=2.4.21
KOTLIN_HOME=${KOTLIN_HOME:-$HOME/.cache/foreman-android/kotlinc}
OUT=$ROOT/target/android
APP=crates/app

if [ ! -x "$KOTLIN_HOME/bin/kotlinc" ]; then
    echo "fetching kotlinc $KOTLIN_VERSION"
    mkdir -p "$(dirname "$KOTLIN_HOME")"
    curl -fsSL -o /tmp/kotlinc.zip \
        "https://github.com/JetBrains/kotlin/releases/download/v$KOTLIN_VERSION/kotlin-compiler-$KOTLIN_VERSION.zip"
    unzip -q -o /tmp/kotlinc.zip -d "$(dirname "$KOTLIN_HOME")"
fi

if [ -n "${RELEASE:-}" ]; then
    PROFILE=release; FLAG=--release; DEBUGGABLE=false
else
    PROFILE=debug; FLAG=; DEBUGGABLE=true
fi

echo "== rust"
cargo apk build --lib -p foreman $FLAG
LIB=$ROOT/target/$PROFILE/apk/lib/arm64-v8a/libforeman.so
[ -f "$LIB" ] || LIB=$(find "$ROOT/target" -path "*apk/lib/arm64-v8a/libforeman.so" | head -1)

rm -rf "$OUT"; mkdir -p "$OUT/res" "$OUT/dex"

echo "== kotlin"
"$KOTLIN_HOME/bin/kotlinc" -nowarn -jvm-target 17 -cp "$JAR" -d "$OUT/classes.jar" $APP/android/src/io/v0l/foreman/*.kt
"$BT/d8" --min-api 26 --lib "$JAR" --output "$OUT/dex" "$OUT/classes.jar" "$KOTLIN_HOME/lib/kotlin-stdlib.jar"

echo "== resources and manifest"
VERSION=$(cargo pkgid -p foreman | sed 's/.*[#@]//')
# cargo-apk derives a version code from the crate version; keep that as the base so builds from
# either path upgrade one another, and add the commit count so each build is newer than the last.
BASE=$(grep -o 'versionCode="[0-9]*"' "$(dirname "$LIB")/../../AndroidManifest.xml" | grep -o '[0-9]*')
CODE=$((BASE + $(git rev-list --count HEAD)))
sed "s/@DEBUGGABLE@/$DEBUGGABLE/" packaging/android/AndroidManifest.xml > "$OUT/AndroidManifest.xml"
"$BT/aapt2" compile --dir $APP/android/res -o "$OUT/res.zip"
"$BT/aapt2" link -o "$OUT/base.apk" -I "$JAR" --manifest "$OUT/AndroidManifest.xml" \
    --min-sdk-version 26 --target-sdk-version 34 \
    --version-code "$CODE" --version-name "$VERSION" "$OUT/res.zip"

echo "== package"
python3 - "$OUT" "$LIB" <<'PY'
import sys, zipfile
out, lib = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(f"{out}/base.apk", "a") as z:
    z.write(f"{out}/dex/classes.dex", "classes.dex", zipfile.ZIP_DEFLATED)
    # Stored, so Android maps the library straight from the APK (extractNativeLibs=false).
    z.write(lib, "lib/arm64-v8a/libforeman.so", zipfile.ZIP_STORED)
PY
"$BT/zipalign" -f -p 4 "$OUT/base.apk" "$OUT/aligned.apk"
if [ -n "${KEYSTORE:-}" ]; then
    "$BT/apksigner" sign --ks "$KEYSTORE" --ks-pass "pass:${KEYSTORE_PASS:?}" \
        ${KEY_ALIAS:+--ks-key-alias "$KEY_ALIAS"} --out "$OUT/foreman.apk" "$OUT/aligned.apk"
else
    "$BT/apksigner" sign --ks "$HOME/.android/debug.keystore" --ks-pass pass:android \
        --key-pass pass:android --out "$OUT/foreman.apk" "$OUT/aligned.apk"
fi
rm -f "$OUT/foreman.apk.idsig"
echo "built $OUT/foreman.apk"
