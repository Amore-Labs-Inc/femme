#!/usr/bin/env bash
# Restarts the Claude in every herdr pane that runs an older binary than `claude` resolves to now,
# so the panes pick up the nightly upgrade (femme-pane-restart.timer, 01:15, after claude-update.sh's
# upgrade at 00:45). A pane is touched only when its agent has been idle for QUIET seconds with an
# empty composer: `/exit`, then `claude --resume <uuid>` in the same pane keeps the conversation.
# One pane at a time; a pane still busy when BUDGET_MIN runs out is left for the next night.
#   pane-restart.sh            restart what is outdated
#   FORCE=1 pane-restart.sh    restart every pane, current or not
#   DRY=1 pane-restart.sh      say what would be done
set -uo pipefail
export PATH="$HOME/.local/share/mise/shims:$HOME/.local/bin:$PATH"
quiet=${QUIET:-120} budget_min=${BUDGET_MIN:-240} dry=${DRY:-0} force=${FORCE:-0}
end=$(( $(date +%s) + budget_min * 60 ))
say() { printf '%s %s\n' "$(date '+%F %T')" "$*"; }
# ponytail: a running one's version = the dir under mise/installs its binary lives in
version_of() { readlink -f "/proc/$1/exe" | sed -n 's#.*/installs/claude/\([^/]*\)/.*#\1#p'; }
want=$(claude --version 2>/dev/null | awk '{print $1}')
[ -n "$want" ] || { say "no claude on PATH"; exit 1; }

# pane lines: <herdr session> <pane> <uuid> <pid>
panes() {
  for hs in $(herdr session list 2>/dev/null | awk 'NR>1 && $2=="running"{print $1}'); do
    herdr --session "$hs" agent list 2>/dev/null | python3 -c '
import sys, json
hs = sys.argv[1]
for a in json.load(sys.stdin)["result"]["agents"]:
    if a.get("agent") != "claude": continue
    print(hs, a["pane_id"], a["agent_session"]["value"])' "$hs"
  done | while read -r hs pane uuid; do
    # its pid from the session files (argv is no key: a forked session keeps its parent's --resume)
    pid=$(pid_of "$uuid")
    [ -n "$pid" ] || { say "$hs $pane $uuid: no process found, skipped"; continue; }
    echo "$hs $pane $uuid $pid"
  done
}

pid_of() { for f in $(grep -l "\"sessionId\":\"$1\"" ~/.claude/sessions/*.json 2>/dev/null); do p=$(basename "$f" .json); [ -d "/proc/$p" ] && { echo "$p"; return; }; done; }
status() { herdr --session "$1" agent get "$2" 2>/dev/null | python3 -c 'import sys,json;print(json.load(sys.stdin)["result"]["agent"]["agent_status"])' 2>/dev/null; }
empty_composer() { herdr --session "$1" pane read "$2" --source visible --lines 4 --format text 2>/dev/null | grep -qE '^❯ *$'; }

restart() {  # hs pane uuid
  local hs=$1 pane=$2 uuid=$3
  herdr --session "$hs" pane send-text "$pane" '/exit' && sleep 0.3 && herdr --session "$hs" pane send-keys "$pane" enter
  herdr --session "$hs" pane wait-output "$pane" --regex '\$ ?$' --source visible --lines 3 --timeout 30000 >/dev/null \
    || { say "$hs $pane: no shell prompt after /exit"; return 1; }
  herdr --session "$hs" pane run "$pane" "claude --resume $uuid" >/dev/null
  # herdr lists the agent again only once it has detected it: poll, `agent wait` errors until then
  for _ in $(seq 1 24); do sleep 5; case "$(status "$hs" "$pane")" in idle|done|working) return 0 ;; esac; done
  say "$hs $pane: not back after resume"; return 1
}

todo=$(panes)
[ -n "$todo" ] || { say "no panes"; exit 0; }
while [ -n "$todo" ] && [ "$(date +%s)" -lt "$end" ]; do
  rest=''
  while read -r hs pane uuid pid; do
    have=$(version_of "$pid")
    if [ "$have" = "$want" ] && [ "$force" = 0 ]; then say "$hs $pane $uuid: $have, current"; continue; fi
    s=$(status "$hs" "$pane")
    case "$s" in idle|done) ;; *) rest="$rest$hs $pane $uuid $pid"$'\n'; continue ;; esac
    # idle QUIET seconds in a row: a pane the user is still talking to is not restarted under them
    ok=1
    for _ in $(seq 1 $((quiet / 10))); do sleep 10; s=$(status "$hs" "$pane"); case "$s" in idle|done) ;; *) ok=0; break ;; esac; done
    [ "$ok" = 1 ] && empty_composer "$hs" "$pane" || { rest="$rest$hs $pane $uuid $pid"$'\n'; continue; }
    if [ "$dry" = 1 ]; then say "dry: would restart $hs $pane $uuid ($have -> $want)"; continue; fi
    if restart "$hs" "$pane" "$uuid"; then
      np=$(pid_of "$uuid")
      say "$hs $pane $uuid: $have -> $(version_of "$np") (pid $np)"
    else
      say "$hs $pane $uuid: restart failed, left as is"
    fi
  done <<<"$todo"
  todo=${rest%$'\n'}
  [ -n "$todo" ] && sleep 60
done
[ -z "$todo" ] || say "still busy at the end of the budget: $(echo "$todo" | awk '{print $1":"$2}' | paste -sd' ')"
