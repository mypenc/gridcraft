#!/usr/bin/env bash
# Run inside an emulator: install the debug APK, start the app, keep its log and state.
APK=$(ls android/app/build/outputs/apk/debug/*.apk | head -1)
adb install -r "$APK" || exit 1
adb logcat -c
adb shell am start -n ai.storyteller.gridcraft/.MainActivity
sleep 40
PID=$(adb shell pidof ai.storyteller.gridcraft | tr -d '\r')
echo "pid=$PID" > pid.txt
adb logcat -d -v time > smoke.log || true
adb logcat -d -v time --pid="$PID" > app.log 2>/dev/null || true
adb shell dumpsys window | grep -E "mCurrentFocus|mFocusedApp" >> pid.txt
adb shell run-as ai.storyteller.gridcraft sh -c 'ls -R files/.config 2>&1 | head -20; tail -n 60 files/.config/GridCraft/logs/* 2>&1' > files.txt
adb exec-out screencap -p > smoke.png || true
python3 - <<'PY' >> pid.txt 2>&1
try:
    from PIL import Image
    im = Image.open("smoke.png").convert("RGB"); w, h = im.size
    cols = im.resize((8, 8)).getdata()
    print("size", w, h, "sample", list(cols)[:64:9])
except Exception as e:
    print("png:", e)
PY
exit 0
