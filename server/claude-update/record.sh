#!/usr/bin/env bash
# record.sh <demo> <branch>: renders a terminal demo to the pull request's video, which the desktop
# app shows beside the PR: <state>/pr-videos/<branch, every '/' as '-'>.mp4, at most 8 MiB.
#   <demo> is one of
#     *.tape            a vhs tape (Type/Enter/Sleep...; its Output lines are ignored)
#     *.cast            an asciinema recording
#     *.mp4|.webm|.gif  a video made some other way (only converted and shrunk)
#     anything else     a command run in a 100x30 terminal by asciinema (an executable script)
# Prints the ffprobe line of the result; fails when the video is missing, empty or too big.
set -euo pipefail
[ $# -eq 2 ] || { echo "usage: record.sh <demo.tape|demo.cast|video|script> <branch>" >&2; exit 2; }
demo=$1 branch=$2
here=$(cd "$(dirname "$0")" && pwd)
"$here/tools.sh" >/dev/null
export PATH="$HOME/.local/bin:$PATH"
dir=${FEMME_STATE:-$HOME/.local/state/femme}/pr-videos
mkdir -p "$dir"
out=$dir/${branch//\//-}.mp4
max=$((8 * 1024 * 1024))
t=$(mktemp -d); trap 'rm -rf "$t"' EXIT

case $demo in
  *.tape)
    # Our Output first; the tape's own Output lines would write elsewhere.
    { printf 'Output "%s"\n' "$t/raw.mp4"; grep -v '^[[:space:]]*Output[[:space:]]' "$demo"; } > "$t/demo.tape"
    VHS_NO_SANDBOX=1 vhs "$t/demo.tape" >&2
    raw=$t/raw.mp4 ;;
  *.mp4|*.webm|*.gif|*.mov|*.mkv) raw=$demo ;;
  *)
    if [ "${demo##*.}" = cast ]; then cast=$demo; else
      cast=$t/demo.cast
      asciinema rec -q --headless --window-size 100x30 -i 2 -c "$demo" "$cast" >&2
    fi
    agg --font-size 18 --idle-time-limit 2 "$cast" "$t/raw.gif" >&2
    raw=$t/raw.gif ;;
esac

# H.264 + yuv420p plays everywhere; even sizes are an H.264 requirement. Smaller and blurrier until it fits.
for try in "23 1280" "28 1280" "32 960" "36 720"; do
  set -- $try
  ffmpeg -v error -y -i "$raw" -an -c:v libx264 -preset veryfast -crf "$1" -pix_fmt yuv420p -movflags +faststart \
    -vf "scale='min($2,iw)':-2:flags=lanczos,pad=ceil(iw/2)*2:ceil(ih/2)*2" "$t/out.mp4"
  [ "$(stat -c %s "$t/out.mp4")" -le $max ] && break
done
size=$(stat -c %s "$t/out.mp4")
[ "$size" -le $max ] || { echo "record.sh: still $size bytes after the smallest setting; shorten the demo" >&2; exit 1; }
probe=$(ffprobe -v error -select_streams v:0 -show_entries stream=codec_name,width,height:format=duration -of compact=p=0 "$t/out.mp4" | tr "\n" " ")
case $probe in *duration=*) ;; *) echo "record.sh: ffprobe can't read the result" >&2; exit 1 ;; esac
mv "$t/out.mp4" "$out"
echo "$out $size bytes $probe"
