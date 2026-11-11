#!/usr/bin/env bash
# Runs the unsigned ipa on a physical Apple iPhone 16 in AWS Device Farm (us-west-2), paired
# with a live box, and downloads screenshots, syslog and video:
#   mobile/ci/devicefarm-ios.sh <femme-ios-unsigned.ipa> '<femme://pair?...>' [out dir]
# No Apple developer account needed: Device Farm re-signs the ipa with its own profile (so push
# and the keychain are off there). The pairing link goes into the test package, which only this
# AWS account can read; revoke the device afterwards (`femmed devices revoke <name>`). A run
# costs about 10 device-minutes, most of it WebDriverAgent and pip. The test is
# mobile/ci/devicefarm/test_femme_ios.py; the test spec is Device Farm's own
# "Default iOS Test Spec for Appium 3 Python".
set -euo pipefail
ipa=$1; link=$2; out=${3:-devicefarm-ios-out}
region=us-west-2
here=$(cd "$(dirname "$0")" && pwd)
aws() { command aws --region "$region" --output json "$@"; }
j() { python3 -c "import sys,json; print(eval('json.load(sys.stdin)$1'))"; }
pick() { python3 -c "
import sys,json; xs=json.load(sys.stdin)['$1']; print(next((x['arn'] for x in xs if x['name']$2),''))"; }

project=$(aws devicefarm list-projects | pick projects "=='femme-mobile'")
[ -n "$project" ] || project=$(aws devicefarm create-project --name femme-mobile | j "['project']['arn']")
pool=$(aws devicefarm list-device-pools --arn "$project" --type PRIVATE | pick devicePools "=='iphone16'")
[ -n "$pool" ] || pool=$(aws devicefarm create-device-pool --project-arn "$project" --name iphone16 \
  --rules '[{"attribute":"ARN","operator":"IN","value":"[\"arn:aws:devicefarm:us-west-2::device:C3481B68E9EA4202BBF6F9D215E9AE5F\"]"}]' | j "['devicePool']['arn']")
spec=$(aws devicefarm list-uploads --arn "$project" --type APPIUM_PYTHON_TEST_SPEC | python3 -c "
import sys,json
xs=[u for u in json.load(sys.stdin)['uploads'] if u['name'].startswith('Default iOS Test Spec for Appium 3 Python')]
print(sorted(xs, key=lambda u: u['name'])[-1]['arn'])")

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
mkdir -p "$work/pkg/tests"
cp "$here/devicefarm/test_femme_ios.py" "$work/pkg/tests/test_femme.py"
printf '%s\n' "$link" > "$work/pkg/tests/pair.txt"
printf 'Appium-Python-Client\npytest\n' > "$work/pkg/requirements.txt"
python3 - "$work" <<'PY'
import sys, os, zipfile
w=sys.argv[1]
with zipfile.ZipFile(f'{w}/tests.zip','w',zipfile.ZIP_DEFLATED) as z:
    z.write(f'{w}/pkg/requirements.txt','requirements.txt')
    for f in os.listdir(f'{w}/pkg/tests'): z.write(f'{w}/pkg/tests/{f}', 'tests/'+f)
PY

upload() { # <file> <type> -> arn, once processed
  local u url arn; u=$(aws devicefarm create-upload --project-arn "$project" --name "$(basename "$1")" --type "$2")
  url=$(echo "$u" | j "['upload']['url']"); arn=$(echo "$u" | j "['upload']['arn']")
  curl -sf -o /dev/null -T "$1" "$url"
  for _ in $(seq 1 40); do
    case "$(aws devicefarm get-upload --arn "$arn" | j "['upload']['status']")" in
      SUCCEEDED) echo "$arn"; return 0;; FAILED) echo "upload failed: $1" >&2; return 1;;
    esac; sleep 5
  done
  echo "upload timed out: $1" >&2; return 1
}
app=$(upload "$ipa" IOS_APP)
pkg=$(upload "$work/tests.zip" APPIUM_PYTHON_TEST_PACKAGE)
run=$(aws devicefarm schedule-run --project-arn "$project" --app-arn "$app" --device-pool-arn "$pool" \
  --name "femme iphone16 $(date +%F-%H%M)" --test "{\"type\":\"APPIUM_PYTHON\",\"testPackageArn\":\"$pkg\",\"testSpecArn\":\"$spec\"}" \
  --execution-configuration '{"jobTimeoutMinutes":20,"videoCapture":true}' | j "['run']['arn']")
echo "run $run"
for _ in $(seq 1 120); do
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
