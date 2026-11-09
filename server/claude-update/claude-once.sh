#!/usr/bin/env bash
# claude-once.sh <limit> <out> <claude args...>: one headless `claude -p` (stream-json into <out>)
# that ends when its turn does. claude -p stays alive while shells it left in the background run
# (a build or a poll loop it meant to come back to), so once the result line is in <out> the whole
# process group goes. <limit>: timeout(1)'s duration, the hard stop. Exit: claude's, or 0 after a result.
set -uo pipefail
limit=$1 out=$2; shift 2
setsid timeout "$limit" claude "$@" </dev/null >"$out" 2>&1 &
pid=$!
while kill -0 "$pid" 2>/dev/null; do
  if grep -q '"type":"result"' "$out" 2>/dev/null; then
    sleep 10  # its own clean exit, when nothing is left behind
    kill -- -"$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    exit 0
  fi
  sleep 5
done
wait "$pid"
