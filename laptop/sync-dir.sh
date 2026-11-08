#!/bin/sh
# Keep <dir> in live two-way sync with the same path on the VPS (Mutagen, two-way-safe:
# a conflict pauses that file instead of overwriting either side). The first call seeds
# the VPS with rsync, which is faster for bulk, and never overwrites newer VPS files.
#
# Usage: sync-dir.sh <dir>     Host: $FEMME_HOST (default agentbox)
set -eu
host=${FEMME_HOST:-agentbox}
dir=$(cd "$1" && pwd)
case $dir in "$HOME"/?*) ;; *) echo "sync-dir: only folders inside $HOME" >&2; exit 1 ;; esac
name=femme-$(printf '%s' "$dir" | cksum | cut -d' ' -f1)
# Rebuildable, platform-specific dirs stay on their own side, plus <dir>/.femme-ignore: one
# Mutagen pattern per line (`/build`, `*.iso`, `!keep.iso`; `#` comments). Ignores are fixed when
# the session is made: to change them, `mutagen sync terminate <name>` and run this again.
patterns() {
  printf '%s\n' node_modules .venv venv __pycache__ target .cache .next
  [ ! -f "$dir/.femme-ignore" ] || tr -d '\r' < "$dir/.femme-ignore" | grep -Ev '^[[:space:]]*(#|$)' || :
}

if ! mutagen sync list "$name" >/dev/null 2>&1; then
  set --
  while read -r p; do case $p in !*) ;; *) set -- "$@" --exclude="$p" ;; esac; done <<EOF
$(patterns)
EOF
  # git's index is a per-machine stat cache and git rewrites it in place: a seed copy taken mid-write
  # left a laptop with a 90-entry index (570 files shown deleted). Each side builds its own.
  rsync -az --mkpath --update --partial --exclude=.git/index --exclude=.git/index.lock "$@" "$dir/" "$host:$dir/"  # --partial: big files resume
  # a fresh copy has no index, which git reads as every file deleted: build it from HEAD
  ssh "$host" "cd '$dir' && if [ -d .git ] && [ ! -e .git/index ]; then git reset -q; fi"
  set --
  while read -r p; do set -- "$@" -i "$p"; done <<EOF
$(patterns)
EOF
  set -- "$@" -i .git/index -i .git/index.lock
  mutagen sync create --name "$name" -m two-way-safe --symlink-mode posix-raw "$@" "$dir" "$host:$dir" >/dev/null
fi
mutagen sync flush "$name"
