#!/usr/bin/env bash
# Records a FEMME desktop demo: record-desktop.sh <scene.sh> <video.mp4>, run from a worktree root
# after `cargo build -p femmed -p femme-holder` there and `cargo build` in desktop/. It starts a
# throwaway femmed (own state, socket and ports; the live box is untouched), the desktop from this
# worktree in an Xvfb display (1600x1000, lavapipe), records the screen and sources <scene.sh>,
# which drives it with:
#   env_desktop VAR=VALUE...   environment for the desktop (fixtures); before `start`
#   start                      launches the desktop and waits until it has drawn
#   click X Y | dclick X Y     a left click / double click at 1600x1000 coordinates
#   key KEYS...                xdotool key names: ctrl+k, Escape, Return, Down
#   type TEXT                  types like a person
#   pause SECONDS
#   shot NAME                  $SHOTS/NAME.png (default /tmp/femme-shots/<video name>/), to find coordinates and check the effect
# The daemon and the desktop get their own empty HOME: the box's mirrored chats (herdr panes,
# ~/.claude/sessions) and the user's files never reach the video. Claude sessions there run the real
# `claude` with the box's sign-in and nothing else of ~/.claude; FAKE_CLAUDE=1 runs the test fake instead. The video is H.264, at most 8 MiB (re-encoded smaller when over).
set -euo pipefail
[ $# -eq 2 ] || { echo "usage: record-desktop.sh <scene.sh> <video.mp4>" >&2; exit 2; }
scene=$(realpath "$1") out=$(realpath -m "$2")
root=$PWD
femmed=$root/target/debug/femmed holder=$root/target/debug/femme-holder
desktop=$root/desktop/target/debug/femme-desktop
for b in "$femmed" "$holder" "$desktop"; do [ -x "$b" ] || { echo "record-desktop: build $b first" >&2; exit 1; }; done
W=1600 H=1000 max=$((8 << 20))

tmp=$(mktemp -d /tmp/femme-rec.XXXXXX)
shots=${SHOTS:-/tmp/femme-shots/$(basename "${out%.*}")}
mkdir -p "$tmp/state" "$tmp/run" "$tmp/home" "$tmp/bin" "$shots" "$(dirname "$out")"
chmod 700 "$tmp/run"
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  # The daemon's holders and chats outlive it: everything that runs with this HOME goes too.
  local e
  for e in /proc/[0-9]*/environ; do
    { tr '\0' '\n' <"$e"; } 2>/dev/null | grep -qxF "HOME=$tmp/home" && kill "${e//[!0-9]/}" 2>/dev/null
  done
  sleep 1  # let them exit before their files go
  wait 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT

# A free display and two free ports.
d=150; while [ -e "/tmp/.X$d-lock" ] || [ -e "/tmp/.X11-unix/X$d" ]; do d=$((d + 1)); done
read -r qport hport < <(python3 -c 'import socket
s=[socket.socket(type=t) for t in (socket.SOCK_DGRAM,socket.SOCK_STREAM)]
[x.bind(("127.0.0.1",0)) for x in s]; print(*[x.getsockname()[1] for x in s])')
export DISPLAY=:$d

Xvfb "$DISPLAY" -screen 0 "${W}x${H}x24" +extension GLX +render -nolisten tcp >"$tmp/xvfb.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.1; done

daemon_env=(HOME="$tmp/home" FEMME_STATE="$tmp/state" XDG_RUNTIME_DIR="$tmp/run" HTTP_BIND="127.0.0.1:$hport" QUIC_PORT="$qport"
  FEMME_HOLDER_DIRECT=1 FEMME_HOLDER_BIN="$holder" FEMME_ENV=/dev/null)
# mise's shims read $HOME's config, which the daemon's HOME has none of: link the real binaries.
if [ "${FAKE_CLAUDE:-0}" = 1 ]; then
  ln -s "$root/crates/femme-agents/fixtures/claude/fake-claude.py" "$tmp/bin/claude"
else
  # Chats use the empty HOME's ~/.claude with only the box's sign-in linked in (the daemon passes
  # no CLAUDE_CONFIG_DIR on): the box's live sessions can't see them (ListAgents, SendMessage) and
  # the user's CLAUDE.md, hooks and plugins stay out.
  mkdir -m 700 "$tmp/home/.claude"
  ln -s "$HOME/.claude/.credentials.json" "$tmp/home/.claude/.credentials.json"
  ln -s "$(mise which claude)" "$tmp/bin/claude"
fi
env "${daemon_env[@]}" PATH="$tmp/bin:$PATH" "$femmed" serve >"$tmp/femmed.log" 2>&1 &
pids+=($!)
for _ in $(seq 100); do [ -S "$tmp/run/femme/femmed.sock" ] && break; sleep 0.2; done

# Enrol a scratch device and read what it pins.
spki=$("$desktop" --show-spki --identity "$tmp/dev" 2>/dev/null | grep -oE '[0-9a-f]{64}' | head -1)
pins=$(env "${daemon_env[@]}" "$femmed" enroll --name rec --spki "$spki")
fp=$(grep -oE 'fingerprint=[0-9a-f]{64}' <<<"$pins" | cut -d= -f2)
[ -n "$fp" ] || { echo "record-desktop: enroll gave no fingerprint: $pins" >&2; tail -20 "$tmp/femmed.log" >&2; exit 1; }

desktop_env=(HOME="$tmp/home" VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json)
env_desktop() { desktop_env+=("$@"); }

ffmpeg -loglevel error -f x11grab -video_size "${W}x${H}" -framerate 30 -draw_mouse 1 -i "$DISPLAY" \
  -c:v libx264 -preset veryfast -crf 23 -pix_fmt yuv420p -y "$tmp/raw.mp4" </dev/null &
rec=$!

start() {
  env "${desktop_env[@]}" "$desktop" --size "${W}x${H}" --daemon "127.0.0.1:$qport" --fingerprint "$fp" \
    --identity "$tmp/dev" >"$tmp/desktop.log" 2>&1 &
  pids+=($!)
  local win=''
  for _ in $(seq 100); do win=$(xdotool search --onlyvisible --pid "${pids[-1]}" 2>/dev/null | head -1) && [ -n "$win" ] && break; sleep 0.2; done
  [ -n "$win" ] || { echo "record-desktop: the desktop showed no window" >&2; tail -20 "$tmp/desktop.log" >&2; exit 1; }
  # Xvfb has no window manager: place it and give it focus ourselves.
  xdotool windowmove "$win" 0 0 windowsize "$win" "$W" "$H" windowfocus --sync "$win" 2>/dev/null || true
  sleep 3  # the first frames under lavapipe, and the daemon link
}
click() { xdotool mousemove --sync "$1" "$2"; sleep 0.25; xdotool click 1; sleep 0.4; }
dclick() { xdotool mousemove --sync "$1" "$2"; sleep 0.25; xdotool click --repeat 2 --delay 80 1; sleep 0.4; }
key() { xdotool key --delay 120 "$@"; sleep 0.4; }
type() { xdotool type --delay 55 -- "$*"; sleep 0.3; }
pause() { sleep "$1"; }
shot() { ffmpeg -loglevel error -f x11grab -video_size "${W}x${H}" -i "$DISPLAY" -frames:v 1 -y "$shots/$1.png" </dev/null; }

# shellcheck source=/dev/null
. "$scene"
pause 1

kill -INT "$rec"; wait "$rec" 2>/dev/null || true
cp "$tmp/raw.mp4" "$out"
# Over the cap: smaller and slower until it fits.
for scale in 1280 960 720; do
  [ "$(stat -c %s "$out")" -le "$max" ] && break
  ffmpeg -loglevel error -i "$tmp/raw.mp4" -vf "scale=$scale:-2,fps=20" -c:v libx264 -preset slow -crf 30 \
    -pix_fmt yuv420p -movflags +faststart -y "$out" </dev/null
done
echo "record-desktop: $out $(stat -c %s "$out") bytes, shots in $shots"
