#!/usr/bin/env bash
# Close the app and open it again on a device or emulator over adb, and fail when the second
# launch does not get the app drawing. Needs the APK installed; pair it first so the background
# watch service runs, which is what kept the old process alive.
#
#   packaging/android/relaunch-check.sh [back|recents|link|force-stop|kill ...] [-n ROUNDS] [-p URL TOKEN]
#
# With no mode it runs back, recents and link. `link` swipes the app away and relaunches through
# an agentbench://open link, as a notification tap does. `kill` sends the app home and kills it
# with `am kill`, as Android does to a background app. With -p it clears the app, pairs it with
# URL and TOKEN first, and fails any relaunch that shows the connect screen.
set -euo pipefail
PKG=io.v0l.agentbench
ACT=$PKG/.MainActivity
MARKER="agentbench frame loop started"
ROUNDS=1
MODES=()
PAIR_URL=; PAIR_TOKEN=
while [ $# -gt 0 ]; do
    case "$1" in
        -n) ROUNDS=$2; shift 2 ;;
        -p) PAIR_URL=$2; PAIR_TOKEN=$3; shift 3 ;;
        back|recents|link|force-stop|kill) MODES+=("$1"); shift ;;
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
        kill)
            adb shell input keyevent 3
            sleep 2
            adb shell am kill "$PKG"
            ;;
    esac
}

pair() {
    adb shell pm clear "$PKG" >/dev/null
    adb shell pm grant "$PKG" android.permission.POST_NOTIFICATIONS 2>/dev/null || true
    adb logcat -c
    adb shell am start -W -a android.intent.action.VIEW \
        -d "'agentbench://pair?url=$PAIR_URL&token=$PAIR_TOKEN'" >/dev/null
    wait_for marker_seen 20 || { echo "FAIL: the pairing launch drew nothing"; exit 1; }
    sleep 3
    echo "paired with $PAIR_URL"
}

paired_ok() {
    [ -z "$PAIR_URL" ] || [ "$1" = "$2" ] && return 0
    local log='adb logcat -d'
    wait_for "$log | grep -q 'agentbench connect\(ing to the saved server\| screen shown\)'" 10
    eval "$log" | grep -q "agentbench connecting to the saved server"
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
    local mode=$1 before after
    destroyed=no
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
    paired_ok "$before" "$after" || { echo "FAIL $mode: the relaunch asked to connect again"; return 1; }
    echo "ok $mode: closed=$destroyed ui pid $before -> $after, processes: $(adb shell ps -A -o NAME | grep -c "^$PKG")${PAIR_URL:+, still paired}"
}

[ -z "$PAIR_URL" ] || pair

failed=0
for round in $(seq "$ROUNDS"); do
    for mode in "${MODES[@]}"; do
        check "$mode" || failed=1
    done
done
exit $failed
