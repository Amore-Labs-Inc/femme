#!/bin/sh
# Send a local Claude Code chat to the FEMME box and keep going there.
# Puts the chat's folder (git repo root, or cwd) on the same path on the box, copies the
# session file, then opens a fork of it as a FEMME chat (femmed open --fork). Prints the chat id.
#
# Usage: send-to-box.sh <claude-session-uuid> <host>     Run from the chat's working dir.
set -u

fail() { printf 'send-to-box: %s\n' "$1" >&2; exit 1; }

# One word for the remote shell: single quotes, each ' inside as '\'' (POSIX sh, no %q).
q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }

uuid=${1:-}
host=${2:-}
[ -n "$uuid" ] && [ -n "$host" ] || fail "usage: send-to-box.sh <claude-session-uuid> <host>"
command -v rsync >/dev/null 2>&1 || fail "rsync is required but not on PATH"

file=$(find ~/.claude/projects -name "$uuid.jsonl" 2>/dev/null | head -n 1)
[ -n "$file" ] || fail "could not find the session file for $uuid under ~/.claude/projects"

cwd=$(pwd)
root=$(git -C "$cwd" rev-parse --show-toplevel 2>/dev/null) || root=$cwd
case $root in
  "$HOME"/?*)
    if command -v mutagen >/dev/null 2>&1; then
      # Same session name and ignores as laptop/sync-dir.sh, so a FEMME checkout's script reuses it.
      name=femme-$(printf '%s' "$root" | cksum | cut -d' ' -f1)
      if ! mutagen sync list "$name" >/dev/null 2>&1; then
        ssh "$host" mkdir -p "$(q "$root")" || fail "could not reach $host over ssh"
        set --
        for p in node_modules .venv venv __pycache__ target .cache .next .git/index .git/index.lock; do
          set -- "$@" -i "$p"
        done
        mutagen sync create --name "$name" -m two-way-safe --symlink-mode posix-raw "$@" \
          "$root" "$host:$root" >/dev/null || fail "could not sync $root with $host"
      fi
      mutagen sync flush "$name" >/dev/null || fail "could not sync $root with $host"
      # a fresh copy has no git index, which git reads as every file deleted: build it from HEAD
      # shellcheck disable=SC2029
      ssh "$host" "cd $(q "$root") && if [ -d .git ] && [ ! -e .git/index ]; then git reset -q; fi"
    else
      rsync -az --mkpath --exclude=node_modules --exclude=target "$root/" "$host:$root/" \
        || fail "could not copy $root to $host"
      printf 'send-to-box: mutagen is not installed, so %s was copied to %s once, not kept in sync\n' "$root" "$host" >&2
    fi ;;
  *) ssh "$host" mkdir -p "$(q "$cwd")" || fail "could not reach $host over ssh" ;;  # never ship /tmp or all of $HOME
esac
rsync -az --mkpath "$file" "$host:$file" || fail "could not copy the session file to $host"

# femmed lives in ~/.local/bin, off a non-interactive PATH; ssh starts in the remote home.
# shellcheck disable=SC2029
out=$(ssh "$host" ".local/bin/femmed open --cwd $(q "$cwd") --resume $(q "$uuid") --fork" 2>&1) \
  || fail "femmed on $host could not open the chat: $out"
printf '%s\n' "$out"
