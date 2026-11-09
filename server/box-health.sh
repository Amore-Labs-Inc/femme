#!/usr/bin/env bash
# The box's hourly check-up (femme-box-health.timer). Housekeeping first: old cargo build output,
# stale Claude scratch dirs in /tmp, Docker build cache and dangling images, the journal. Then the
# checks: disk, load, swap, and orphaned femme-desktop/Xvfb processes (a recording whose parent died),
# which it kills. Every problem goes to the journal as a warning, to ~/.local/state/femme/box-health.json
# ({ok, at, problems}) and, once per kind while it is open, to a GitHub issue "box: <problem>" labelled
# box-health on this checkout's repo. Those issues are safe: the femmed scheduler only works the
# board's own issues (femme.db), never GitHub's. Exits 1 when there was a problem, so the unit shows failed.
# One day the box hit load 28, 4 GB swap and 95 % disk, femmed was watchdog-killed 7 times, and a
# leaked desktop recording ran 76 min before anyone looked.
set -eu
cd "$(dirname "$0")/.."  # the checkout: gh finds the repo from its remote
export PATH="$HOME/.cargo/bin:$HOME/.local/share/mise/shims:$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin"
state=${FEMME_STATE:-$HOME/.local/state/femme}
mkdir -p "$state"
days=14
problems=''
problem() { problems="$problems$1"$'\n'; echo "PROBLEM: $1"; logger -p user.warning -t femme-box-health -- "$1" 2>/dev/null || true; }
used() { df --output=used,pcent -h / | tail -n1 | tr -s ' '; }

echo "disk before:$(used)"
# Housekeeping. Each step may fail (a build holding a lock, no Docker) without stopping the rest.
if command -v cargo-sweep >/dev/null; then
  for d in "$HOME/femme-work" "$HOME/Documents/Web_Apps/femme" "$HOME/femme"; do
    [ -d "$d" ] || continue
    # quiet: it lists every project it walks; its last line is the total
    cargo sweep --time "$days" -r "$d" 2>&1 | tail -n1 | sed "s|^|sweep $d: |" || echo "sweep $d failed"
  done
fi
# A Claude session's scratch dir with nothing written for two weeks belongs to no live session.
for d in /tmp/claude-"$(id -u)"/*/; do
  [ -d "$d" ] || continue
  [ -n "$(find "$d" -newermt "$days days ago" -print -quit 2>/dev/null)" ] && continue
  rm -rf "$d" 2>/dev/null && echo "removed $d" || echo "could not remove all of $d"
done
if command -v docker >/dev/null && docker info >/dev/null 2>&1; then
  docker builder prune -af --filter until=168h 2>&1 | tail -n1 | sed 's/^/docker builder: /' || true
  docker image prune -f 2>&1 | tail -n1 | sed 's/^/docker images: /' || true
fi
# The system journal is root's: sudo when it needs no password, else only the user journal.
if sudo -n true 2>/dev/null; then sudo -n journalctl --vacuum-size=200M 2>&1 | tail -n1; else journalctl --user --vacuum-size=200M 2>&1 | tail -n1; fi || true
echo "disk after:$(used)"

# Checks.
pct=$(df --output=pcent / | tail -n1 | tr -dc 0-9)
[ "$pct" -le 85 ] || problem "disk / at $pct % (over 85 %)"
cores=$(nproc)
load=$(cut -d' ' -f1 /proc/loadavg)
awk -v l="$load" -v c="$cores" 'BEGIN { exit !(l > 2 * c) }' && problem "load $load on $cores cores (over $((2 * cores)))"
swap_kb=$(awk '/^SwapTotal:/ { t = $2 } /^SwapFree:/ { f = $2 } END { print t - f }' /proc/meminfo)
[ "$swap_kb" -le $((2 << 20)) ] || problem "swap at $(awk -v k="$swap_kb" 'BEGIN { printf "%.1f", k / 1048576 }') GB used (over 2 GB)"
# Orphans: reparented to pid 1 or to the user's systemd (a subreaper) and older than 30 min. Xvfb :1
# and anything drawing on it belong to someone's headful session: never touched.
while read -r pid ppid age comm; do
  case $comm in femme-desktop | Xvfb) ;; *) continue ;; esac
  [ "$age" -gt 1800 ] || continue
  [ "$ppid" = 1 ] || [ "$(cat "/proc/$ppid/comm" 2>/dev/null)" = systemd ] || continue
  if [ "$comm" = Xvfb ]; then
    disp=$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null | cut -d' ' -f2)
  else
    disp=$(tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null | sed -n 's/^DISPLAY=//p')
  fi
  [ "$disp" != :1 ] || continue
  kill "$pid" 2>/dev/null || continue
  sleep 3; kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null
  problem "leaked $comm (pid $pid, display ${disp:-?}, $((age / 60)) min, orphaned) killed"
done < <(ps -u "$(id -u)" -o pid=,ppid=,etimes=,comm=)

# Report.
printf '%s' "$problems" | jq -Rs --arg at "$(date -Is)" 'split("\n") | map(select(. != "")) | {ok: (length == 0), at: $at, problems: .}' \
  >"$state/box-health.json.new" && mv "$state/box-health.json.new" "$state/box-health.json"
[ -n "$problems" ] || { echo "ok"; exit 0; }
if command -v gh >/dev/null && gh auth status >/dev/null 2>&1; then
  gh label create box-health --color d93f0b --description "Raised by server/box-health.sh" >/dev/null 2>&1 || true
  open=$(gh issue list --state open --label box-health --limit 100 --json title -q '.[].title' 2>/dev/null) || open=''
  # Process names only, never command lines: those can carry tokens.
  top=$(ps -eo pid,pcpu,pmem,etime,comm --sort=-pcpu | head -n 10)
  printf '%s' "$problems" | while read -r p; do
    # The title names the kind of problem, without its numbers, so an hourly repeat is not a new issue.
    case $p in
      disk*) t='box: disk / over 85 %' ;;
      load*) t='box: load over 2x cores' ;;
      swap*) t='box: swap over 2 GB' ;;
      leaked*) t='box: leaked femme-desktop/Xvfb' ;;
      *) t="box: $p" ;;
    esac
    grep -qxF "$t" <<<"$open" && { echo "issue already open: $t"; continue; }
    # shellcheck disable=SC2016 # the backticks are the issue's code fence
    url=$(gh issue create --title "$t" --label box-health --body "$(printf '%s on %s at %s.\n\nTop processes:\n```\n%s\n```\n' "$p" "$(hostname)" "$(date -Is)" "$top")" 2>&1) \
      && echo "issue opened: $url" || echo "gh issue create failed: $url"
    open="$open"$'\n'"$t"
  done
else
  echo "no signed-in gh: no GitHub issue"
fi
exit 1
