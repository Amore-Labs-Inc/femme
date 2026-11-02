#!/bin/sh
# FEMME's agent view (tui/) as a herdr plugin pane. `tui.sh open` is the action a key runs: it asks
# herdr to open the pane entry as an overlay; without arguments it is that pane's command and runs
# the binary, which bootstrap.sh (server) or laptop-setup.sh (laptop, with cargo) put in ~/.local/bin.
set -u

herdr=${HERDR_BIN_PATH:-herdr}
notify() { "$herdr" notification show "femme" --body "$1" --sound "${2:-none}" >/dev/null 2>&1; }

if [ "${1:-}" = open ]; then
  out=$("$herdr" plugin pane open --plugin femme --entrypoint tui --focus 2>&1) \
    || { notify "could not open the agent view: $out" request; printf 'tui: %s\n' "$out" >&2; exit 1; }
  exit 0
fi

bin=${FEMME_TUI:-$HOME/.local/bin/femme-tui}
[ -x "$bin" ] || bin=$(command -v femme-tui 2>/dev/null || true)
if [ -z "$bin" ]; then
  notify "femme-tui is not installed here: run ./laptop/sync.sh (server) or ./laptop/laptop-setup.sh with cargo (laptop)" request
  printf 'femme-tui is not installed here.\nServer: ./laptop/sync.sh builds it. Laptop: cargo, then ./laptop/laptop-setup.sh.\n' >&2
  sleep 5
  exit 1
fi
exec "$bin"
