#!/bin/sh
# Fork the focused local agent onto the VPS and keep the local one running.
# Puts the agent's folder (git repo root, or cwd) in live two-way sync with the
# same path on the VPS (laptop/sync-dir.sh), copies the session file, then starts the
# copy there: a Claude fork opens as a FEMME chat (femmed open --fork), codex and pi
# start in a new herdr workspace in their own fork mode, like forkr does locally.
#
# Usage: fork-remote.sh [pane-id]     Host: $FEMME_HOST (default agentbox)
set -u

herdr=${HERDR_BIN_PATH:-herdr}
host=${FEMME_HOST:-agentbox}
remote() { "$herdr" --machine "$host" "$@"; }

notify() {
  "$herdr" notification show "femme" --body "$1" --sound "${2:-none}" >/dev/null 2>&1
}

fail() {
  notify "$1" request
  printf 'fork-remote: %s\n' "$1" >&2
  exit 1
}

# One word for the remote shell: single quotes, each ' inside as '\'' (POSIX sh, no %q).
q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }

command -v jq >/dev/null 2>&1 || fail "jq is required but not on PATH"
command -v mutagen >/dev/null 2>&1 || fail "mutagen is required: https://mutagen.io"

target=${1:-}
[ -n "$target" ] || target=$(printf '%s' "${HERDR_PLUGIN_CONTEXT_JSON:-}" | jq -r '.focused_pane_id // empty' 2>/dev/null)
[ -n "$target" ] || target=${HERDR_PANE_ID:-}
[ -n "$target" ] || fail "no focused pane"

info=$("$herdr" agent get "$target" 2>/dev/null) || fail "no agent running in $target"
field() { printf '%s' "$info" | jq -r ".result.agent.$1 // empty"; }
kind=$(field agent)
cwd=$(field foreground_cwd)
[ -n "$cwd" ] || cwd=$(field cwd)
ref=$(field agent_session.value)
[ -n "$ref" ] || fail "herdr has no session for the $kind in $target. Install its integration: herdr integration install $kind"

# Session file to copy, then the agent's own fork argv (same recipes as forkr; claude's is femmed's).
case $kind in
  claude) file=$(find ~/.claude/projects -name "$ref.jsonl" 2>/dev/null | head -n 1) ;;
  codex)  file=$(find ~/.codex/sessions -name "*$ref*.jsonl" 2>/dev/null | head -n 1); set -- fork "$ref" ;;
  pi)     case $ref in /*) file=$ref ;; *) file=$(find ~/.pi -name "*$ref*.jsonl" 2>/dev/null | head -n 1) ;; esac
          set -- --fork "$ref" ;;
  *) fail "no remote fork recipe for $kind" ;;  # ponytail: opencode keeps sessions in many files
esac
[ -n "$file" ] || fail "could not find the $kind session file for $ref"

root=$(git -C "$cwd" rev-parse --show-toplevel 2>/dev/null) || root=$cwd
case $root in
  "$HOME"/?*)
    notify "Syncing $root with $host"
    sh "${HERDR_PLUGIN_ROOT:-$(dirname "$0")}/../laptop/sync-dir.sh" "$root" >/dev/null 2>&1 \
      || fail "could not sync $root with $host" ;;
  *) ssh "$host" mkdir -p "$(q "$cwd")" ;;  # never ship /tmp or all of $HOME
esac
rsync -az --mkpath "$file" "$host:$file" || fail "could not copy the session file to $host"

# Claude: the copy is a FEMME chat. femmed lives in ~/.local/bin, off a non-interactive PATH;
# ssh starts in the remote home, so the path is relative. cwd and ref are quoted here on purpose.
if [ "$kind" = claude ]; then
  # shellcheck disable=SC2029
  out=$(ssh "$host" ".local/bin/femmed open --cwd $(q "$cwd") --resume $(q "$ref") --fork" 2>&1) \
    || fail "femmed on $host could not fork the claude session: $out"
  notify "Forked claude to $host: $target -> FEMME chat $out"
  printf '%s\n' "$out"
  exit 0
fi

out=$(remote workspace create --cwd "$cwd" --focus 2>&1) || fail "could not create a workspace on $host: $out"
new_pane=$(printf '%s' "$out" | jq -r '.result.root_pane.pane_id // empty')
[ -n "$new_pane" ] || fail "herdr on $host returned no pane id: $out"

# Wait (up to ~15s) for the new pane's shell, as forkr does locally.
i=0
while [ $i -lt 30 ]; do
  ready=$(remote pane process-info --pane "$new_pane" 2>/dev/null | jq -r '
    .result.process_info
    | if (.foreground_processes | length) == 1
         and (.foreground_processes[0].pid == .shell_pid
              or (.foreground_processes[0].name | test("^(zsh|bash|fish|sh|dash|ksh|nu)$")))
      then "yes" else "no" end' 2>/dev/null)
  [ "$ready" = yes ] && break
  i=$((i + 1))
  sleep 0.5
done

err=$(remote agent start "femme-$$" --kind "$kind" --pane "$new_pane" --timeout 60000 -- "$@" 2>&1 >/dev/null)
status=$?
remote agent rename "$new_pane" --clear >/dev/null 2>&1
if [ $status -ne 0 ] && ! printf '%s' "$err" | grep -q agent_not_ready; then
  fail "$kind did not start on $host: $err"
fi

notify "Forked $kind to $host: $target -> $new_pane"
printf '%s\n' "$new_pane"
