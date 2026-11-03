#!/bin/sh
# Holds Claude accounts out of cswap's rotation (femme-fable-cap.service). Each pass (every
# FABLE_CAP_INTERVAL seconds, default 60) holds an account, with `cswap disable` and a line in
# $STATE/fable-held, when one of its rules trips:
#   - every account: its Fable weekly is at FABLE_CAP% (default 96) or more;
#   - an account with an entry in $STATE/caps.json, which femmed writes for the desktop app and this
#     loop re-reads every pass ({"<number>": {"five_hour": 80, "weekly": 90}}, either key optional):
#     its 5-hour or its 7-day window is at that cap or more. A cap is a whole number from 50 to 99;
#     any other value, or a caps.json that is not one JSON object, is ignored with a log line.
# If the active account trips, the box moves: `cswap switch --strategy best`, plus `--model Fable`
# when its Fable rule tripped (cswap only moves to an account with more headroom; the next pass
# retries). A held account comes back (`cswap enable`) once none of its rules trips; only accounts
# held here are re-enabled. Unknown usage (cswap has no recent measurement, or a capped window has
# no pct) neither holds nor releases; a Fable window cswap does not list counts as 0%, as before.
# Limitation: femme-cswap.service (`cswap auto --threshold $CAP`) still moves the box off the active
# account at the global CAP, so a per-account cap above CAP cannot keep the box on that account any
# longer: per-account caps only take an account out of rotation earlier, or on their own.
# $STATE is $FEMME_STATE, else ~/.local/state/femme. FABLE_CAP_ONCE=1 runs one pass and exits.
set -u
cd "$(dirname "$0")/.." || exit 1  # the checkout root: .env and every path below are relative to it
# shellcheck source=/dev/null
[ -f ./.env ] && . ./.env
FABLE_CAP=${FABLE_CAP:-96}
interval=${FABLE_CAP_INTERVAL:-60}
state=${FEMME_STATE:-$HOME/.local/state/femme}
held=$state/fable-held      # account numbers this loop disabled, one per line
caps_file=$state/caps.json  # per-account caps, written by femmed
log() { printf '%s %s\n' "$(date -u +%FT%TZ)" "$*"; }
case $FABLE_CAP in ''|*[!0-9]*) log "FABLE_CAP is not a whole percent: $FABLE_CAP"; exit 1 ;; esac
case $interval in ''|*[!0-9]*) log "FABLE_CAP_INTERVAL is not a whole number of seconds: $interval"; exit 1 ;; esac
mkdir -p "$state"; touch "$held"

# jq: whether a caps.json value is a usable cap. (The $ names in the jq programs are jq's: SC2016.)
is_cap='def is_cap: type == "number" and . == floor and . >= 50 and . <= 99;'

# jq over the caps object: one line for each entry or cap it ignores (null means unset: no line).
# shellcheck disable=SC2016
caps_problems=$is_cap'
  to_entries[] | .key as $k | .value | select(. != null)
  | if type != "object" then "caps.json: account \($k) ignored: not an object"
    else to_entries[]
      | select((.key == "five_hour" or .key == "weekly") and .value != null and (.value | is_cap | not))
      | "caps.json: account \($k) \(.key) cap ignored: \(.value | tojson) is not a whole number from 50 to 99"
    end'

# jq over `cswap list --json`: one line per account, "number active disabled fable_tripped tripped
# all_known what", where what names the tripped rules (else all of them) with their pct and cap.
# A rule's pct is null when unknown: it then neither trips nor counts as known.
# shellcheck disable=SC2016
rows_jq=$is_cap'
  def pct: if type == "object" and (.pct | type) == "number" then .pct | floor else null end;
  ($fable_cap | tonumber) as $fcap
  | .accounts[] | select((.number | type) == "number")
  | (.number | tostring) as $n
  | (.usage | if type == "object" then . else null end) as $u
  | ($caps[$n] | if type == "object" then . else {} end) as $c
  | [ {name: "Fable weekly", fable: true, cap: $fcap,
       pct: (if $u == null then null
             else first($u.scoped[]? | select(.name? == "Fable")) // {pct: 0} | pct end)},
      ($c.five_hour | select(is_cap) | {name: "5-hour", cap: ., pct: ($u.fiveHour | pct)}),
      ($c.weekly | select(is_cap) | {name: "7-day", cap: ., pct: ($u.sevenDay | pct)})
    ] as $rules
  | [$rules[] | select(.pct != null and .pct >= .cap)] as $trips
  | [$n, .active == true, .disabled == true, any($trips[]; .fable == true), $trips != [],
     all($rules[]; .pct != null),
     (if $trips == [] then $rules else $trips end | map("\(.name) at \(.pct)% (cap \(.cap)%)") | join(", "))]
  | map(tostring) | join(" ")'

# Sets caps to caps.json for this pass: {} when the file is missing or empty, and also {} (with a
# log line) when it cannot be read or is not one JSON object.
load_caps() {
  caps='{}'
  [ -s "$caps_file" ] || return 0
  if ! caps=$(jq -c -s 'if length == 0 then {}
      elif length == 1 and (.[0] | type) == "object" then .[0] else error("not one object") end' \
      "$caps_file" 2>/dev/null); then
    caps='{}'
    log "caps.json ignored this pass: unreadable or not a JSON object"
    return 0
  fi
  printf '%s\n' "$caps" | jq -r "$caps_problems" | while IFS= read -r line; do log "$line"; done
}

is_held() { grep -qFx -- "$1" "$held" 2>/dev/null; }
unhold() { grep -vFx -- "$1" "$held" > "$held.new" 2>/dev/null; mv -f "$held.new" "$held"; }

hold() {  # account, what tripped
  if cswap disable "$1" </dev/null >/dev/null; then
    is_held "$1" || echo "$1" >> "$held"
    log "account $1 held: $2"
  else
    log "account $1: cswap disable failed ($2)"
  fi
}

release() {  # account, its rules now
  if cswap enable "$1" </dev/null >/dev/null; then
    unhold "$1"
    log "account $1 back: $2"
  else
    log "account $1: cswap enable failed"
  fi
}

pass() {
  if ! list=$(cswap list --json </dev/null 2>/dev/null); then
    log "cswap list --json failed: nothing changed this pass"
    return 0
  fi
  load_caps
  if ! rows=$(printf '%s\n' "$list" \
      | jq -r --argjson caps "$caps" --arg fable_cap "$FABLE_CAP" "$rows_jq" 2>/dev/null); then
    log "cswap list --json gave no account list: nothing changed this pass"
    return 0
  fi
  move='' fable_move=''
  while read -r n active disabled fable trip known what; do
    [ -n "$n" ] || continue
    if [ "$trip" = true ]; then
      if [ "$active" = true ]; then
        move=$n
        [ "$fable" = true ] && fable_move=1
      fi
      [ "$disabled" = true ] || hold "$n" "$what"
    elif [ "$disabled" = true ]; then
      # Back only when every rule is known and clear, and only if this loop held it.
      if [ "$known" = true ] && is_held "$n"; then release "$n" "$what"; fi
    elif is_held "$n"; then
      # Enabled again by hand: no longer ours, so a later hand disable is never undone here.
      unhold "$n"
      log "account $n was enabled by hand: no longer held here"
    fi
  done <<ROWS
$rows
ROWS
  [ -n "$move" ] || return 0
  if [ -n "$fable_move" ]; then
    log "moving off account $move: $(cswap switch --strategy best --model Fable </dev/null 2>&1 | tail -n 1)"
  else
    log "moving off account $move: $(cswap switch --strategy best </dev/null 2>&1 | tail -n 1)"
  fi
}

while :; do
  pass
  [ "${FABLE_CAP_ONCE:-}" = 1 ] && exit 0
  sleep "$interval"
done
