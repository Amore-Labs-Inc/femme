#!/usr/bin/env bash
# Runs inside the Android emulator job (.github/workflows/mobile.yml): installs the release APK,
# seeds the pairing (FEMME_CI_TOKEN = the `ci-emulator` device's token), launches the app and
# screenshots every tab into ./screens. One script because android-emulator-runner runs each line
# of its `script:` input as its own shell.
set -euxo pipefail
pkg=org.exposureai.femme_mobile
adb root && sleep 3 && adb wait-for-device
adb install -r app-release.apk
adb shell monkey -p "$pkg" -c android.intent.category.LAUNCHER 1 && sleep 8
adb shell am force-stop "$pkg"
cat > prefs.xml <<EOF
<?xml version='1.0' encoding='utf-8' standalone='yes' ?>
<map>
  <string name="flutter.host">femme.exposureai.org</string>
  <string name="flutter.token">${FEMME_CI_TOKEN:?}</string>
  <long name="flutter.client_instance" value="111111111111" />
</map>
EOF
adb push prefs.xml /sdcard/prefs.xml
adb shell "mkdir -p /data/data/$pkg/shared_prefs && cp /sdcard/prefs.xml /data/data/$pkg/shared_prefs/FlutterSharedPreferences.xml && uid=\$(stat -c %u /data/data/$pkg) && chown -R \$uid:\$uid /data/data/$pkg/shared_prefs && chmod 660 /data/data/$pkg/shared_prefs/FlutterSharedPreferences.xml && restorecon -R /data/data/$pkg"
rm prefs.xml
mkdir -p screens
adb shell monkey -p "$pkg" -c android.intent.category.LAUNCHER 1 && sleep 20
adb exec-out screencap -p > screens/01-now.png
# pixel_6 is 1080x2400 with a 3-button nav bar at the bottom (y > 2280): the app's tab bar sits
# at y≈2208, tabs at x 135 / 405 / 675 / 945; the first Done card on Now is around y≈930.
adb shell input tap 405 2208 && sleep 4 && adb exec-out screencap -p > screens/02-chats.png
adb shell input tap 675 2208 && sleep 6 && adb exec-out screencap -p > screens/03-box.png
adb shell input tap 945 2208 && sleep 3 && adb exec-out screencap -p > screens/04-me.png
adb shell input tap 135 2208 && sleep 2
adb shell input tap 540 930 && sleep 8 && adb exec-out screencap -p > screens/05-chat.png
adb logcat -d | grep -iE 'flutter|AndroidRuntime' | tail -80 > screens/logcat.txt || true
