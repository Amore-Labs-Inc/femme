#!/usr/bin/env bash
# Runs the release APK on a physical Google Pixel in AWS Device Farm (us-west-2), paired with a
# live box through its femme:// link, and downloads the screenshots, logcat and video:
#   mobile/ci/devicefarm.sh <app-release.apk> '<femme://pair?...>' [out dir]
# Needs the aws CLI with Device Farm rights (the project `femme-mobile` and the one-device pool
# `pixel8` are created on first use). The pairing link goes into the test package, which only
# this AWS account can read; revoke the device (`femmed devices revoke <name>`) afterwards.
# Cost: a few device-minutes per run. Pixel 8 is 1080x2400, the same as the CI emulator, so the
# tab taps at 94 % of the height land in the tab bar with either navigation mode.
set -euo pipefail
apk=$1; link=$2; out=${3:-devicefarm-out}
region=us-west-2
aws() { command aws --region "$region" --output json "$@"; }
j() { python3 -c "import sys,json; print(eval('json.load(sys.stdin)$1'))"; }

project=$(aws devicefarm list-projects | j "['projects']" | python3 -c "
import sys,ast; ps=ast.literal_eval(sys.stdin.read()); print(next((p['arn'] for p in ps if p['name']=='femme-mobile'),''))")
[ -n "$project" ] || project=$(aws devicefarm create-project --name femme-mobile | j "['project']['arn']")
pool=$(aws devicefarm list-device-pools --arn "$project" --type PRIVATE | j "['devicePools']" | python3 -c "
import sys,ast; ps=ast.literal_eval(sys.stdin.read()); print(next((p['arn'] for p in ps if p['name']=='pixel8'),''))")
[ -n "$pool" ] || pool=$(aws devicefarm create-device-pool --project-arn "$project" --name pixel8 \
  --rules '[{"attribute":"ARN","operator":"IN","value":"[\"arn:aws:devicefarm:us-west-2::device:AC2E189FD1154D05BFCC187783715555\"]"}]' | j "['devicePool']['arn']")

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
mkdir -p "$work/pkg/tests"
cat > "$work/pkg/tests/run.sh" <<EOF
#!/bin/bash
set -x
S="\$DEVICEFARM_SCREENSHOT_PATH"; mkdir -p "\$S"; L="\$DEVICEFARM_LOG_DIR"
pkg=org.exposureai.femme_mobile
shot() { adb exec-out screencap -p > "\$S/\$1.png"; }
adb shell monkey -p \$pkg -c android.intent.category.LAUNCHER 1; sleep 10; shot 01-first-run
adb shell am start -a android.intent.action.VIEW -d "'$link'"; sleep 15; shot 02-now
read W H < <(adb shell wm size | sed 's/.*: //; s/x/ /')
tab() { adb shell input tap \$((W*\$1/8)) \$((H*94/100)); sleep 5; }
tab 3; shot 03-chats
tab 5; sleep 3; shot 04-box
tab 7; shot 05-me
tab 1; adb shell input tap \$((W/2)) \$((H*36/100)); sleep 8; shot 06-chat
adb logcat -d > "\$L/logcat.txt"
adb shell getprop ro.product.model > "\$L/device.txt"; adb shell getprop ro.build.version.release >> "\$L/device.txt"
exit 0
EOF
printf 'def test_noop():\n    assert True\n' > "$work/pkg/tests/test_noop.py"
printf 'pytest\n' > "$work/pkg/requirements.txt"
python3 - "$work" <<'PY'
import sys, os, zipfile
w=sys.argv[1]
with zipfile.ZipFile(f'{w}/tests.zip','w',zipfile.ZIP_DEFLATED) as z:
    z.write(f'{w}/pkg/requirements.txt','requirements.txt')
    for f in os.listdir(f'{w}/pkg/tests'): z.write(f'{w}/pkg/tests/{f}', 'tests/'+f)
PY
cat > "$work/spec.yml" <<'EOF'
version: 0.1
phases:
  install:
    commands:
      - echo install
  pre_test:
    commands:
      - adb devices
  test:
    commands:
      - bash $DEVICEFARM_TEST_PACKAGE_PATH/tests/run.sh
  post_test:
    commands:
      - echo done
artifacts:
  - $DEVICEFARM_LOG_DIR
EOF

upload() { # <file> <type> -> arn, once processed
  local u; u=$(aws devicefarm create-upload --project-arn "$project" --name "$(basename "$1")" --type "$2")
  local url arn; url=$(echo "$u" | j "['upload']['url']"); arn=$(echo "$u" | j "['upload']['arn']")
  curl -sf -o /dev/null -T "$1" "$url"
  for _ in $(seq 1 30); do
    case "$(aws devicefarm get-upload --arn "$arn" | j "['upload']['status']")" in
      SUCCEEDED) echo "$arn"; return 0;; FAILED) echo "upload failed: $1" >&2; return 1;;
    esac; sleep 5
  done
  echo "upload timed out: $1" >&2; return 1
}
app=$(upload "$apk" ANDROID_APP)
pkg=$(upload "$work/tests.zip" APPIUM_PYTHON_TEST_PACKAGE)
spec=$(upload "$work/spec.yml" APPIUM_PYTHON_TEST_SPEC)
run=$(aws devicefarm schedule-run --project-arn "$project" --app-arn "$app" --device-pool-arn "$pool" \
  --name "femme pixel8 $(date +%F-%H%M)" --test "{\"type\":\"APPIUM_PYTHON\",\"testPackageArn\":\"$pkg\",\"testSpecArn\":\"$spec\"}" \
  --execution-configuration '{"jobTimeoutMinutes":15,"videoCapture":true}' | j "['run']['arn']")
echo "run $run"
for _ in $(seq 1 90); do
  st=$(aws devicefarm get-run --arn "$run" | j "['run']['status']"); echo "  $st"
  [ "$st" = COMPLETED ] && break; sleep 10
done
echo "result: $(aws devicefarm get-run --arn "$run" | j "['run']['result']")"
mkdir -p "$out"
for t in SCREENSHOT FILE LOG; do
  aws devicefarm list-artifacts --arn "$run" --type $t | python3 -c "
import sys,json,subprocess
for a in json.load(sys.stdin)['artifacts']:
    fn='$out/'+a['type']+'-'+a['name'].replace(' ','_')+'.'+a['extension']
    subprocess.run(['curl','-s','-o',fn,a['url']]); print(fn)"
done
