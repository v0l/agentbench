#!/usr/bin/env bash
# Close the app and open it again on a device or emulator over adb, and fail when the second
# launch does not get the app drawing. Needs the APK installed; pair it first so the background
# watch service runs, which is what kept the old process alive.
#
#   packaging/android/relaunch-check.sh [back|recents|link|force-stop ...] [-n ROUNDS]
#
# With no mode it runs back, recents and link. `link` swipes the app away and relaunches through
# an agentbench://open link, as a notification tap does.
set -euo pipefail
PKG=io.v0l.agentbench
ACT=$PKG/.MainActivity
MARKER="agentbench frame loop started"
ROUNDS=1
MODES=()
while [ $# -gt 0 ]; do
    case "$1" in
        -n) ROUNDS=$2; shift 2 ;;
        back|recents|link|force-stop) MODES+=("$1"); shift ;;
        *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
done
[ ${#MODES[@]} -gt 0 ] || MODES=(back recents link)

wait_for() {
    local what=$1 tries=$2
    for _ in $(seq "$tries"); do
        if eval "$what"; then return 0; fi
        sleep 1
    done
    return 1
}

marker_seen() { adb logcat -d -s agentbench:I | grep -q "$MARKER"; }
resumed() { adb shell dumpsys activity activities | grep -q "topResumedActivity=.*$ACT"; }
activity_gone() {
    local acts
    acts=$(adb shell dumpsys activity activities)
    ! grep -q "topResumedActivity=.*$ACT" <<<"$acts" &&
        { ! grep -q "ActivityRecord{.*$ACT" <<<"$acts" || grep -q "finishing=true" <<<"$acts"; }
}

launch() {
    adb shell am start -W -n "$ACT" >/dev/null
    wait_for marker_seen 20 || { echo "FAIL: first launch drew nothing"; return 1; }
    wait_for resumed 10
}

close_by() {
    case "$1" in
        back)
            for _ in 1 2 3; do
                adb shell input keyevent 4
                sleep 1
                resumed || break
            done
            ;;
        recents|link)
            local size w h
            size=$(adb shell wm size | awk '{print $3}' | tr -d '\r')
            w=${size%x*}; h=${size#*x}
            adb shell input keyevent 187
            sleep 2
            adb shell input swipe $((w / 2)) $((h / 2)) $((w / 2)) $((h / 20)) 200
            sleep 1
            adb shell input keyevent 3
            ;;
        force-stop) adb shell am force-stop "$PKG" ;;
    esac
}

relaunch_by() {
    if [ "$1" = link ]; then
        adb shell am start -W -n "$ACT" -f 0x30000000 \
            -d "'agentbench://open?session=relaunch-check&t=$(date +%s%N)'" >/dev/null
    else
        adb shell am start -W -n "$ACT" >/dev/null
    fi
}

check() {
    local mode=$1 destroyed before after
    adb shell am force-stop "$PKG"
    adb logcat -c
    launch
    before=$(adb shell pidof "$PKG" | tr -d '\r')
    close_by "$mode"
    if wait_for activity_gone 10; then destroyed=yes; else destroyed=no; fi
    sleep 2
    adb logcat -c
    relaunch_by "$mode"
    if [ $destroyed = yes ]; then
        wait_for marker_seen 20 || { echo "FAIL $mode: closed, then the relaunch never drew a frame"; return 1; }
    fi
    wait_for resumed 10 || { echo "FAIL $mode: the app is not in front after the relaunch"; return 1; }
    if adb logcat -d | grep -q "destroy timeout.*$ACT"; then
        echo "FAIL $mode: the old activity hung on destroy"
        return 1
    fi
    after=$(adb shell pidof "$PKG" | tr -d '\r')
    echo "ok $mode: closed=$destroyed ui pid $before -> $after, processes: $(adb shell ps -A -o NAME | grep -c "^$PKG")"
}

failed=0
for round in $(seq "$ROUNDS"); do
    for mode in "${MODES[@]}"; do
        check "$mode" || failed=1
    done
done
exit $failed
