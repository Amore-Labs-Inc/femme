#!/bin/sh
# Tests server/fable-cap.sh against a fake cswap, in a temp checkout under ${TMPDIR:-/tmp}:
# no real account, credential or state directory is touched. Needs jq. Every case runs the script
# under sh, and under dash and bash --posix when they are installed.
#   sh server/test/fable-cap.sh
set -u
src=$(cd "$(dirname "$0")/.." && pwd)/fable-cap.sh
self=$(cd "$(dirname "$0")" && pwd)/${0##*/}
T=$(mktemp -d "${TMPDIR:-/tmp}/fable-cap-test.XXXXXX") || exit 1
trap 'rm -rf "$T"' EXIT
trap 'exit 1' HUP INT TERM

# The fake checkout: the script in $T/server, a fake cswap first on PATH, HOME and FEMME_STATE in $T.
# No .env unless a case writes one, so every other case also checks that .env is optional.
mkdir -p "$T/server" "$T/bin" "$T/home"
cp "$src" "$T/server/fable-cap.sh"
HOME=$T/home FEMME_STATE=$T/state FAKE=$T/fake PATH=$T/bin:$PATH
export HOME FEMME_STATE FAKE PATH
unset FABLE_CAP FABLE_CAP_ONCE FABLE_CAP_INTERVAL
cat > "$T/bin/cswap" <<'FAKE'
#!/bin/sh
# Fake cswap. `list --json` prints $FAKE/list.json and exits with $FAKE/list.exit (default 0); on
# its Nth call it first copies $FAKE/caps-at-N.json, if there is one, to caps.json (the owner
# editing caps while the loop runs). Anything else is appended to $FAKE/calls and succeeds.
if [ "$*" = "list --json" ]; then
  n=$(($(cat "$FAKE/lists" 2>/dev/null || echo 0) + 1))
  echo "$n" > "$FAKE/lists"
  [ ! -f "$FAKE/caps-at-$n.json" ] || cp "$FAKE/caps-at-$n.json" "$FEMME_STATE/caps.json"
  cat "$FAKE/list.json"
  exit "$(cat "$FAKE/list.exit" 2>/dev/null || echo 0)"
fi
echo "$*" >> "$FAKE/calls"
FAKE
chmod +x "$T/bin/cswap"
[ "$(command -v cswap)" = "$T/bin/cswap" ] || { echo "not ok - the fake cswap is not first on PATH"; exit 1; }

# fresh: no .env, no caps.json, nothing held, no calls recorded
fresh() {
  rm -rf "$FEMME_STATE" "$FAKE" "$T/.env" "$T/out"
  mkdir -p "$FEMME_STATE" "$FAKE"
  : > "$FAKE/calls"
}

# usage 5H 7D [FABLE]: a usage object as `cswap list --json` prints it
usage() {
  printf '{"fiveHour":{"pct":%s,"resetsAt":"2026-11-03T15:00:00Z"},"sevenDay":{"pct":%s,"resetsAt":"2026-11-09T15:00:00Z"}' "$1" "$2"
  [ $# -lt 3 ] || printf ',"scoped":[{"name":"Fable","pct":%s,"resetsAt":"2026-11-09T15:00:00Z"}]' "$3"
  printf '}'
}

# account NUMBER STATE USAGE: one account of `cswap list --json`; STATE is idle, active, disabled or
# active-disabled
account() {
  case $2 in *active*) active=true ;; *) active=false ;; esac
  case $2 in *disabled*) disabled=',"disabled":true' ;; *) disabled='' ;; esac
  printf '{"number":%s,"email":"user%s@example.com","active":%s%s,"usage":%s}' "$1" "$1" "$active" "$disabled" "$3"
}

# accounts ACCOUNT...: what the fake `cswap list --json` prints from now on
accounts() {
  all=$1
  shift
  for a in "$@"; do all="$all,$a"; done
  printf '{"schemaVersion":1,"accounts":[%s]}\n' "$all" > "$FAKE/list.json"
}

caps() { printf '%s\n' "$1" > "$FEMME_STATE/caps.json"; }   # caps JSON: caps.json from now on
held() { printf '%s\n' "$@" > "$FEMME_STATE/fable-held"; }  # held N...: held by an earlier pass

# run: one pass (FABLE_CAP_ONCE=1) under the shell being tested; output in $T/out
run() { env FABLE_CAP_ONCE=1 "$sh" ${flag:+"$flag"} "$T/server/fable-cap.sh" > "$T/out" 2>&1; }

# loop_until_held N CAPS: runs the script as a loop of 1 s passes whose second `cswap list` installs
# CAPS as caps.json; stops it once N is held (at most 20 s), and says whether N got held
loop_until_held() {
  printf '%s\n' "$2" > "$FAKE/caps-at-2.json"
  env FABLE_CAP_INTERVAL=1 "$sh" ${flag:+"$flag"} "$T/server/fable-cap.sh" > "$T/out" 2>&1 &
  pid=$!
  i=0
  while [ "$i" -lt 20 ] && ! grep -qFx "$1" "$FEMME_STATE/fable-held"; do
    sleep 1
    i=$((i + 1))
  done
  kill "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
  sleep 1  # let a child of the killed loop (its sleep, the fake cswap) finish before the next case
  grep -qFx "$1" "$FEMME_STATE/fable-held"
}

calls_are() { printf '%s\n' "$@" | cmp -s - "$FAKE/calls"; }                 # exactly, in order
no_calls() { [ ! -s "$FAKE/calls" ]; }
held_are() { printf '%s\n' "$@" | cmp -s - "$FEMME_STATE/fable-held"; }      # exactly, in order
none_held() { [ ! -s "$FEMME_STATE/fable-held" ]; }
logged() { grep -qF -- "$1" "$T/out"; }

# ---- cases: each is a function that fails when the script did something else ----

parses() { "$sh" ${flag:+"$flag"} -n "$src"; }

shellcheck_clean() { fresh && shellcheck -s sh "$src" "$self"; }

five_hour_cap() {
  fresh
  accounts "$(account 1 active "$(usage 81.2 10 20)")" "$(account 2 idle "$(usage 5 5 5)")"
  caps '{"1": {"five_hour": 80, "weekly": 90}}'
  run && calls_are "disable 1" "switch --strategy best" && held_are 1 \
    && logged "account 1 held: 5-hour at 81% (cap 80%)"
}

weekly_cap() {
  fresh
  accounts "$(account 1 active "$(usage 10 10 10)")" "$(account 2 idle "$(usage 30 91.5 10)")"
  caps '{"2": {"five_hour": 80, "weekly": 90}}'
  run && calls_are "disable 2" && held_are 2 && logged "account 2 held: 7-day at 91% (cap 90%)"
}

release_below_caps() {  # 2 drops below both caps; 3 is still at its cap
  fresh
  held 3 2
  accounts "$(account 1 active "$(usage 10 10 10)")" "$(account 2 disabled "$(usage 79.9 89.9 10)")" \
    "$(account 3 disabled "$(usage 80 10 10)")"
  caps '{"2": {"five_hour": 80, "weekly": 90}, "3": {"five_hour": 80}}'
  run && calls_are "enable 2" && held_are 3 \
    && logged "account 2 back: Fable weekly at 10% (cap 96%), 5-hour at 79% (cap 80%), 7-day at 89% (cap 90%)"
}

no_entry() {
  fresh
  accounts "$(account 1 active "$(usage 99 99 10)")" "$(account 2 idle "$(usage 99.9 99.9)")"
  caps '{"3": {"five_hour": 50, "weekly": 50}}'
  run && no_calls && none_held
}

fable_hold() {  # 1 has an entry and is active, 2 has none, 3 is just under
  fresh
  accounts "$(account 1 active "$(usage 10 10 97)")" "$(account 2 idle "$(usage 10 10 96)")" \
    "$(account 3 idle "$(usage 10 10 95.9)")"
  caps '{"1": {"five_hour": 80, "weekly": 90}}'
  run && calls_are "disable 1" "disable 2" "switch --strategy best --model Fable" && held_are 1 2 \
    && logged "account 1 held: Fable weekly at 97% (cap 96%)"
}

fable_active_no_entry() {  # also: held ones come back below FABLE_CAP; no Fable window counts as 0%
  fresh
  held 3 4
  accounts "$(account 1 idle "$(usage 10 10 5)")" "$(account 2 active "$(usage 10 10 99)")" \
    "$(account 3 disabled "$(usage 10 10 12)")" "$(account 4 disabled "$(usage 10 10)")"
  run && calls_are "disable 2" "enable 3" "enable 4" "switch --strategy best --model Fable" && held_are 2
}

hand_disabled() {  # 2 and 3 were disabled by hand: never enabled, never recorded, even over a cap
  fresh
  accounts "$(account 1 active "$(usage 10 10 10)")" "$(account 2 disabled "$(usage 10 10 10)")" \
    "$(account 3 disabled "$(usage 99 99 99)")"
  caps '{"2": {"five_hour": 80}, "3": {"five_hour": 80}}'
  run && no_calls && none_held \
    && accounts "$(account 1 active "$(usage 10 10 10)")" "$(account 3 disabled "$(usage 10 10 10)")" \
    && run && no_calls && none_held
}

hand_disabled_active() {  # the box still moves off it, but it is not recorded as held
  fresh
  accounts "$(account 1 active-disabled "$(usage 85 10 10)")" "$(account 2 idle "$(usage 10 10 10)")"
  caps '{"1": {"five_hour": 80}}'
  run && calls_are "switch --strategy best" && none_held
}

enabled_by_hand() {  # held here, enabled by hand: forgotten, so a later hand disable stays
  fresh
  held 2
  accounts "$(account 1 active "$(usage 10 10 10)")" "$(account 2 idle "$(usage 10 10 10)")"
  run && no_calls && none_held && logged "account 2 was enabled by hand" \
    && accounts "$(account 1 active "$(usage 10 10 10)")" "$(account 2 disabled "$(usage 10 10 10)")" \
    && run && no_calls && none_held
}

unknown_usage() {  # no usage, a capped window without pct, a Fable window without pct
  fresh
  held 2 3 4
  accounts "$(account 1 active null)" "$(account 2 disabled null)" \
    "$(account 3 disabled '{"sevenDay":{"pct":10},"scoped":[{"name":"Fable","pct":10}]}')" \
    "$(account 4 disabled '{"fiveHour":{"pct":10},"sevenDay":{"pct":10},"scoped":[{"name":"Fable"}]}')" \
    "$(account 5 idle '{"fiveHour":{"resetsAt":"2026-11-03T15:00:00Z"},"sevenDay":{"pct":10}}')"
  caps '{"1": {"five_hour": 50, "weekly": 50}, "3": {"five_hour": 80}, "5": {"five_hour": 50}}'
  run && no_calls && held_are 2 3 4
}

caps_not_json() {  # cut off, not an object, two objects: ignored with a line; empty: silently none
  fresh
  accounts "$(account 1 active "$(usage 99 99 10)")"
  caps '{"1": {"five_hour": 80'
  run && no_calls && logged "caps.json ignored this pass" \
    && caps '[{"1": {"five_hour": 80}}]' && run && no_calls && logged "caps.json ignored this pass" \
    && caps '{"1": {"five_hour": 80}} {}' && run && no_calls && logged "caps.json ignored this pass" \
    && : > "$FEMME_STATE/caps.json" && run && no_calls && ! logged caps.json
}

bad_cap_values() {  # only 6's weekly cap is usable; null means unset and is not reported
  fresh
  accounts "$(account 1 active "$(usage 99 99 10)")" "$(account 2 idle "$(usage 99 99 10)")" \
    "$(account 3 idle "$(usage 99 99 10)")" "$(account 4 idle "$(usage 99 99 10)")" \
    "$(account 5 idle "$(usage 99 99 10)")" "$(account 6 idle "$(usage 99 99 10)")" \
    "$(account 7 idle "$(usage 99 99 10)")"
  caps '{"1": {"five_hour": 49}, "2": {"weekly": 100}, "3": {"five_hour": 80.5},
         "4": {"five_hour": "80"}, "5": [80], "6": {"five_hour": true, "weekly": 90},
         "7": {"five_hour": null, "weekly": null}}'
  run && calls_are "disable 6" && held_are 6 && logged "account 1 five_hour cap ignored: 49" \
    && logged "account 2 weekly cap ignored: 100" && logged "account 3 five_hour cap ignored: 80.5" \
    && logged 'account 4 five_hour cap ignored: "80"' && logged "account 5 ignored: not an object" \
    && logged "account 6 five_hour cap ignored: true" && logged "account 6 held: 7-day at 99% (cap 90%)" \
    && ! logged "account 7"
}

caps_reread() {  # one pass under the old cap holds nothing; the loop holds 2 once the cap is lowered
  fresh
  accounts "$(account 1 active "$(usage 10 10 10)")" "$(account 2 idle "$(usage 85 10 10)")"
  caps '{"2": {"five_hour": 90}}'
  run && no_calls && rm "$FAKE/lists" && loop_until_held 2 '{"2": {"five_hour": 80}}' \
    && grep -qx 'disable 2' "$FAKE/calls" && ! grep -qvx 'disable 2' "$FAKE/calls" && held_are 2
}

no_duplicates() {  # still over and enabled on pass 2 (by hand), then disabled on pass 3
  fresh
  accounts "$(account 1 active "$(usage 10 10 10)")" "$(account 2 idle "$(usage 10 95 10)")"
  caps '{"2": {"weekly": 90}}'
  run && run \
    && accounts "$(account 1 active "$(usage 10 10 10)")" "$(account 2 disabled "$(usage 10 95 10)")" \
    && run && calls_are "disable 2" "disable 2" && held_are 2
}

list_fails() {  # cswap fails, or answers without an account list: nothing changes
  fresh
  held 2
  printf '%s\n' '{"schemaVersion":1,"error":{"type":"ConfigError","message":"busy"}}' > "$FAKE/list.json"
  echo 1 > "$FAKE/list.exit"
  run && no_calls && held_are 2 && logged "cswap list --json failed" \
    && echo 0 > "$FAKE/list.exit" && run && no_calls && held_are 2 && logged "gave no account list"
}

env_file() {  # .env is still read: FABLE_CAP from it
  fresh
  printf 'FABLE_CAP=90\n' > "$T/.env"
  accounts "$(account 1 active "$(usage 10 10 91)")" "$(account 2 idle "$(usage 10 10 10)")"
  run && calls_are "disable 1" "switch --strategy best --model Fable" \
    && logged "account 1 held: Fable weekly at 91% (cap 90%)"
}

bad_fable_cap() {  # a FABLE_CAP that is not a whole percent stops the script before any cswap call
  fresh
  printf 'FABLE_CAP=ninety\n' > "$T/.env"
  accounts "$(account 1 active "$(usage 10 10 99)")"
  ! run && no_calls && logged "FABLE_CAP is not a whole percent"
}

# ---- run every case under each shell ----

total=0 failed=0
# check NAME CASE: runs the function CASE, prints ok/not ok (with $where, the shell) and, on a
# failure, what the script did
check() {
  total=$((total + 1))
  if "$2"; then
    echo "ok - $1${where:+ [$where]}"
  else
    failed=$((failed + 1))
    echo "not ok - $1${where:+ [$where]}"
    for f in "$T/out" "$FAKE/calls" "$FEMME_STATE/fable-held"; do
      [ ! -f "$f" ] || sed "s|^|#   ${f##*/}: |" "$f"
    done
  fi
}

for sh in sh dash bash; do
  if ! command -v "$sh" >/dev/null 2>&1; then echo "# $sh is not installed: skipped"; continue; fi
  flag=''
  [ "$sh" != bash ] || flag=--posix
  where="$sh${flag:+ $flag}"
  check "k: the script parses (-n)" parses
  check "a: five-hour cap holds the active account, the box moves without --model" five_hour_cap
  check "b: weekly cap holds an account under its five-hour cap" weekly_cap
  check "c: below both caps again: enabled, out of the held file; at the cap: still held" release_below_caps
  check "d: no caps entry: 99% five-hour and weekly do nothing" no_entry
  check "e: Fable rule holds at FABLE_CAP with and without an entry, active one: --model Fable" fable_hold
  check "e: Fable rule for an active account without entry; held ones come back below it" fable_active_no_entry
  check "f: an account disabled by hand is never enabled or recorded" hand_disabled
  check "f: a hand-disabled active account over a cap: the box moves, nothing recorded" hand_disabled_active
  check "f: held here, then enabled by hand: forgotten, a later hand disable is kept" enabled_by_hand
  check "g: unknown usage or a missing pct neither holds nor releases" unknown_usage
  check "h: caps.json that is not one JSON object is ignored, without a crash" caps_not_json
  check "h: out-of-range, fractional, string, boolean caps are ignored and logged" bad_cap_values
  check "i: caps.json is re-read every pass of the loop" caps_reread
  check "j: no duplicate lines in the held file over three passes" no_duplicates
  check "cswap list failing or without accounts changes nothing" list_fails
  check ".env is optional and still read when present" env_file
  check "a bad FABLE_CAP stops the script before any change" bad_fable_cap
done

where=''
if command -v shellcheck >/dev/null 2>&1; then
  check "shellcheck -s sh on the script and this harness" shellcheck_clean
else
  echo "ok - shellcheck -s sh # SKIP shellcheck is not installed"
fi
echo "# $((total - failed)) of $total passed"
[ "$failed" -eq 0 ]
