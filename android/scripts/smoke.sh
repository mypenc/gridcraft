#!/usr/bin/env bash
# Run inside an emulator: install the debug APK, start the app, keep its log and a screenshot.
APK=$(ls android/app/build/outputs/apk/debug/*.apk | head -1)
adb install -r "$APK" || exit 1
adb logcat -c
adb shell am start -n ai.storyteller.gridcraft/.MainActivity
sleep 45
adb logcat -d -t 4000 > smoke.log || true
adb shell pidof ai.storyteller.gridcraft > pid.txt || echo DIED > pid.txt
adb exec-out screencap -p > smoke.png || true
exit 0
