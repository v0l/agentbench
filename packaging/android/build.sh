#!/usr/bin/env bash
# The one way to build the Android APK: the Rust library through cargo-ndk, then the Kotlin
# activity and background service, the manifest and the signature on top. Do not use cargo-apk,
# it cannot ship the Kotlin. Output: target/android/agentbench.apk
#
#   ABI=arm64-v8a  the device ABI (default), or x86_64 for the emulator
#   ANDROID_HOME   the SDK (default ~/Android/Sdk)
#   ANDROID_NDK_ROOT   the NDK (default: the newest under $ANDROID_HOME/ndk)
#   KOTLIN_HOME    a kotlinc install (default: downloaded to ~/.cache/agentbench-android)
#   KEYSTORE / KEYSTORE_PASS / KEY_PASS / KEY_ALIAS   to sign a release; the debug key is used
#                  otherwise. KEY_PASS defaults to KEYSTORE_PASS.
#   RELEASE=1      a release build: not debuggable, built with --release
#
#   packaging/android/build.sh                  debug, arm64, debug key
#   RELEASE=1 KEYSTORE=... packaging/android/build.sh   signed release for a phone
#   ABI=x86_64 packaging/android/build.sh       debug build for the emulator
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
KOTLIN_HOME=${KOTLIN_HOME:-$HOME/.cache/agentbench-android/kotlinc}
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

ABI=${ABI:-arm64-v8a}
case "$ABI" in
    arm64-v8a|x86_64) ;;
    *) echo "ABI must be arm64-v8a or x86_64, not $ABI" >&2; exit 1 ;;
esac

echo "== rust"
JNI=$ROOT/target/android-jni
rm -rf "$JNI"
cargo ndk --platform 26 -t "$ABI" -o "$JNI" build --lib -p agentbench --no-default-features --features local-speech $FLAG
LIB=$JNI/$ABI/libagentbench.so
[ -f "$LIB" ] || { echo "cargo ndk did not produce $LIB" >&2; exit 1; }

rm -rf "$OUT"; mkdir -p "$OUT/res" "$OUT/dex"

echo "== kotlin"
"$KOTLIN_HOME/bin/kotlinc" -nowarn -jvm-target 17 -cp "$JAR" -d "$OUT/classes.jar" $APP/android/src/io/v0l/agentbench/*.kt
"$BT/d8" --min-api 26 --lib "$JAR" --output "$OUT/dex" "$OUT/classes.jar" "$KOTLIN_HOME/lib/kotlin-stdlib.jar"

echo "== resources and manifest"
VERSION=$(cargo pkgid -p agentbench | sed 's/.*[#@]//')
# Releases up to 0.8.0 had a code of 2^24 + minor * 256 + patch from the crate version, plus the
# commit count; keep that base so every build upgrades the one before.
IFS=. read -r _ MINOR PATCH <<<"${VERSION%%[-+]*}"
BASE=$(( (1 << 24) + MINOR * 256 + PATCH ))
CODE=$((BASE + $(git rev-list --count HEAD)))
sed "s/@DEBUGGABLE@/$DEBUGGABLE/" packaging/android/AndroidManifest.xml > "$OUT/AndroidManifest.xml"
"$BT/aapt2" compile --dir $APP/android/res -o "$OUT/res.zip"
"$BT/aapt2" link -o "$OUT/base.apk" -I "$JAR" --manifest "$OUT/AndroidManifest.xml" \
    --min-sdk-version 26 --target-sdk-version 34 \
    --version-code "$CODE" --version-name "$VERSION" "$OUT/res.zip"

echo "== package"
python3 - "$OUT" "$LIB" "$ABI" <<'PY'
import sys, zipfile
out, lib, abi = sys.argv[1], sys.argv[2], sys.argv[3]
with zipfile.ZipFile(f"{out}/base.apk", "a") as z:
    z.write(f"{out}/dex/classes.dex", "classes.dex", zipfile.ZIP_DEFLATED)
    # Stored, so Android maps the library straight from the APK (extractNativeLibs=false).
    z.write(lib, f"lib/{abi}/libagentbench.so", zipfile.ZIP_STORED)
PY
"$BT/zipalign" -f -p 4 "$OUT/base.apk" "$OUT/aligned.apk"
if [ -n "${KEYSTORE:-}" ]; then
    "$BT/apksigner" sign --ks "$KEYSTORE" --ks-pass "pass:${KEYSTORE_PASS:?}" \
        --key-pass "pass:${KEY_PASS:-$KEYSTORE_PASS}" \
        ${KEY_ALIAS:+--ks-key-alias "$KEY_ALIAS"} --out "$OUT/agentbench.apk" "$OUT/aligned.apk"
else
    "$BT/apksigner" sign --ks "$HOME/.android/debug.keystore" --ks-pass pass:android \
        --key-pass pass:android --out "$OUT/agentbench.apk" "$OUT/aligned.apk"
fi
rm -f "$OUT/agentbench.apk.idsig"

echo "== check"
LISTING=$(unzip -l "$OUT/agentbench.apk")
grep -q "lib/$ABI/libagentbench.so" <<<"$LISTING" || { echo "APK has no lib/$ABI/libagentbench.so" >&2; exit 1; }
[ "$(grep -c ' lib/' <<<"$LISTING")" = 1 ] || { echo "APK has native libraries for other ABIs" >&2; exit 1; }
unzip -p "$OUT/agentbench.apk" classes.dex > "$OUT/check.dex"
for cls in ImeView MainActivity WatchService; do
    grep -aq "Lio/v0l/agentbench/$cls;" "$OUT/check.dex" || { echo "APK has no $cls in classes.dex" >&2; exit 1; }
done
rm -f "$OUT/check.dex"
BADGING=$("$BT/aapt2" dump badging "$OUT/agentbench.apk")
if [ -n "${RELEASE:-}" ]; then
    ! grep -q 'application-debuggable' <<<"$BADGING" || { echo "release APK is debuggable" >&2; exit 1; }
fi
echo "built $OUT/agentbench.apk"
