#!/usr/bin/env bash
# Dictate into a chat's composer after the keyboard was put away, and fail when the keyboard
# comes back. Needs a build with debug assertions (it takes agentbench://speak links as heard
# speech), paired, with a composer on screen. A plain debug build crashes the SwiftShader
# emulator, so build a release with `[profile.release.package.agentbench] debug-assertions = true`.
#
#   packaging/android/keyboard-check.sh [-x X -y Y] [-n PARTIALS]
#
# X Y is where the composer is on screen (default: the middle of the bottom quarter).
set -euo pipefail
PKG=io.v0l.agentbench
ACT=$PKG/.MainActivity
PARTIALS=20
X=; Y=
while [ $# -gt 0 ]; do
    case "$1" in
        -n) PARTIALS=$2; shift 2 ;;
        -x) X=$2; shift 2 ;;
        -y) Y=$2; shift 2 ;;
        *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
done
if [ -z "$X" ] || [ -z "$Y" ]; then
    size=$(adb shell wm size | awk '{print $3}' | tr -d '\r')
    X=$(( ${size%x*} / 2 )); Y=$(( ${size#*x} * 3 / 4 ))
fi

shown() { adb shell dumpsys input_method | grep -q 'mInputShown=true'; }
state() { if shown; then echo shown; else echo hidden; fi; }
say() {
    adb shell am start -n "$ACT" -a android.intent.action.VIEW \
        -d "'agentbench://speak?$1&t=$(date +%s%N)'" >/dev/null
    sleep 1.5
}

adb shell input tap "$X" "$Y"
sleep 2
shown || { echo "FAIL: tapping the composer at $X,$Y did not show the keyboard"; exit 1; }
echo "tap: keyboard shown"
adb shell input keyevent 4
sleep 2
! shown || { echo "FAIL: the back key did not put the keyboard away"; exit 1; }
echo "back: keyboard hidden"

flips=0
words=
for n in $(seq "$PARTIALS"); do
    words="${words:+$words+}word$n"
    if [ "$n" = "$PARTIALS" ]; then say "final=$words"; else say "partial=$words"; fi
    now=$(state)
    echo "partial $n: keyboard $now"
    if [ "$now" = shown ]; then
        flips=$((flips + 1))
        adb shell input keyevent 4
        sleep 1
    fi
done
if [ $flips -gt 0 ]; then
    echo "FAIL: the keyboard came back $flips times in $PARTIALS speech updates"
    exit 1
fi
sleep 3
adb shell input tap "$X" "$Y"
sleep 2
shown || { echo "FAIL: tapping the composer after dictation did not show the keyboard"; exit 1; }
echo "ok: the keyboard stayed away through $PARTIALS speech updates and a tap brings it back"
