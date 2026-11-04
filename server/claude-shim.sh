#!/bin/sh
# Installed as ~/.local/bin/claude by server/bootstrap.sh. A bare `claude` typed in a FEMME shell
# pane (FEMME_PANE is set by the holder, and stdin is the pane's terminal) asks the daemon for a
# headless Claude session in this directory instead: the desktop app shows it. Anything else runs
# the real claude, the first one on PATH outside this folder. If the daemon refuses, the real one
# runs here.
if [ -n "${FEMME_PANE:-}" ] && [ $# -eq 0 ] && [ -t 0 ]; then
  femmed open && exit 0
  echo "claude: no daemon session, starting claude here" >&2
fi
IFS=:
for dir in $PATH; do
  if [ "$dir" != "$HOME/.local/bin" ] && [ -x "$dir/claude" ]; then
    unset IFS
    exec "$dir/claude" "$@"
  fi
done
echo "claude: not found on PATH" >&2
exit 127
