#!/usr/bin/env bash
# Checks an installation from the laptop: each line is ok, or what is wrong and the one thing to do about it.
# Exit status: the number of problems (0 = all good). Safe to run any time; it changes nothing.
set -uo pipefail
cd "$(dirname "$0")"
. ./laptop/lib.sh
[ -f .env ] || die "no .env here: run ./install.sh first"
. ./.env
bad=0
ok() { printf '  \033[32mok\033[0m   %s\n' "$1"; }
no() { printf '  \033[31mfix\033[0m  %s\n       -> %s\n' "$1" "$2"; bad=$((bad + 1)); }
opt() { printf '  \033[90m--\033[0m   %s\n' "$1"; }
on() { [ "${1:-0}" = 1 ]; }

say "laptop"
for t in ssh rsync jq git; do
  command -v $t >/dev/null && ok "$t" || no "$t is missing" "install $t with your package manager"
done
rsync_ok && ok "rsync $(rsync --version | sed -n '1s/^rsync  *version v\{0,1\}\([0-9.]*\).*/\1/p')" || no "rsync is too old (macOS ships openrsync)" "$(rsync_fix)"
[ "$(stat -c %a .env 2>/dev/null || stat -f %Lp .env)" = 600 ] && ok ".env readable only by you" || no ".env is readable by others" "chmod 600 .env"
command -v herdr >/dev/null && ok "herdr $(herdr --version 2>/dev/null | cut -d' ' -f2)" || opt "herdr not installed here (only needed to fork agents to the server)"
command -v mutagen >/dev/null && ok "mutagen" || opt "mutagen not installed here (only needed to fork agents and sync folders)"

say "server ($HOST)"
if ! ssh -o BatchMode=yes -o ConnectTimeout=10 "$HOST" true 2>/dev/null; then
  no "cannot ssh to $HOST" "check ~/.ssh/config Host $HOST and that the server is running: ssh -v $HOST"
  exit $bad
fi
ok "ssh $HOST"
units="herdr-server herdr-instinct herdr-notify"
[ -z "${BOARD_URL:-}" ] || units="$units femme-board femme-worker"
on "${ACCOUNT_ROTATION:-}" && units="$units femme-cswap femme-fable-cap"
on "${AUTODEPLOY:-}" && units="$units femme-deploy.timer"
# One ssh for every remote fact: tab-separated name, value.
facts=$(ssh "$HOST" "PATH=\$HOME/.local/bin:\$HOME/.local/share/mise/shims:\$PATH; units='$units'"'
  for u in $units; do printf "unit\t%s\t%s\n" "$u" "$(systemctl --user is-active "$u" 2>/dev/null)"; done
  printf "herdr\t%s\n" "$(herdr --version 2>/dev/null | cut -d" " -f2)"
  printf "tui\t%s\n" "$(femme-tui --version 2>/dev/null | cut -d" " -f2)"
  printf "claude\t%s\n" "$(command -v claude >/dev/null && { [ -s ~/.claude/.credentials.json ] && echo in || echo out; } || echo missing)"
  printf "gh\t%s\n" "$(gh auth status >/dev/null 2>&1 && echo in || echo out)"
  printf "board\t%s\n" "$(curl -s -o /dev/null -m 5 -w "%{http_code}" http://127.0.0.1:8787/api/ping)"
  printf "swap\t%s\n" "$(swapon --show --noheadings | wc -l)"
  printf "linger\t%s\n" "$(loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null)"' 2>/dev/null)
fact() { printf '%s\n' "$facts" | awk -F'\t' -v k="$1" '$1==k {print $2; exit}'; }

while IFS=$'\t' read -r kind u state; do
  [ "$kind" = unit ] || continue
  [ "$state" = active ] && ok "$u running" || no "$u is ${state:-unknown}" "ssh $HOST journalctl --user -u $u -n 30   (then ./laptop/sync.sh)"
done <<<"$facts"

v=$(fact herdr); lv=$(herdr --version 2>/dev/null | cut -d' ' -f2)
if [ -z "$v" ]; then no "herdr is not installed on the server" "./laptop/sync.sh"
elif [ -n "$lv" ] && [ "$v" != "$lv" ]; then no "herdr $v on the server, $lv here" "./laptop/sync.sh (the server follows the laptop's version)"
else ok "herdr $v"; fi
v=$(fact tui); [ -n "$v" ] && ok "femme-tui $v (ssh $HOST femme-tui, or prefix+f in herdr)" || no "femme-tui is not built on the server" "./laptop/sync.sh"
case $(fact claude) in
  in) ok "Claude Code signed in" ;;
  out) no "Claude Code is not signed in on the server" "ssh $HOST, run claude, then /login" ;;
  *) no "Claude Code is not installed on the server" "./laptop/sync.sh (it installs Claude Code)" ;;
esac
[ "$(fact gh)" = in ] && ok "GitHub CLI signed in" || no "GitHub CLI is not signed in on the server" "ssh $HOST 'gh auth login && gh auth setup-git'"
[ "$(fact linger)" = yes ] && ok "services start at boot (linger)" || no "services stop when you log out" "ssh $HOST sudo loginctl enable-linger \$USER"
[ "$(fact swap)" -gt 0 ] 2>/dev/null && ok "swap" || no "no swap: agents can run the server out of memory" "./laptop/host-init.sh <admin>@<server> (or add a swapfile)"

if [ -n "${BOARD_URL:-}" ]; then
  say "board"
  # /api/ping answers 401 to a stranger: that still means the board is up.
  case $(fact board) in
    200|401) ok "board answers on the server (127.0.0.1:8787)" ;;
    *) no "board does not answer" "ssh $HOST journalctl --user -u femme-board -n 30" ;;
  esac
  if on "${BOARD_PUBLIC:-}"; then
    code=$(curl -s -o /dev/null -m 10 -w '%{http_code}' "$BOARD_URL/login")
    [ "$code" = 200 ] && ok "$BOARD_URL reachable over HTTPS" || no "$BOARD_URL answers ${code:-nothing}" "check the DNS A record points at ${IP:-the server} and ports 80/443 are open"
  else
    ok "board is private: ssh -N -L 8787:127.0.0.1:8787 $HOST, then $BOARD_URL"
  fi
  [ -n "${RESEND_API_KEY:-}" ] || opt "no RESEND_API_KEY: sign-in links appear in: ssh $HOST journalctl --user -u femme-board -n 20"
fi
[ -n "${NTFY_TOPIC:-}" ] && ok "phone notifications: subscribe to the NTFY_TOPIC from .env in the ntfy app" || no "no NTFY_TOPIC" "./install.sh (or set one in .env), then ./sync.sh"

echo
[ $bad = 0 ] && say "all good" || warn "$bad thing(s) to fix, see above"
exit $bad
