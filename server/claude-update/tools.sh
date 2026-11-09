#!/usr/bin/env bash
# Installs the demo recorders record.sh uses into ~/.local/bin, from GitHub release binaries.
# Idempotent: a tool already at its pinned version is left alone. vhs drives a terminal in a
# headless Chromium (ttyd serves the terminal); asciinema + agg are the fallback for plain scripts.
set -euo pipefail
bin=$HOME/.local/bin
mkdir -p "$bin"
arch=$(uname -m)  # x86_64 or aarch64
VHS=0.12.1 TTYD=1.7.7 ASCIINEMA=3.2.1 AGG=1.9.0

has() { "$bin/$1" --version 2>/dev/null | grep -qF "$2"; }
fetch() {  # url dest: download beside the target, then rename (a running binary can't be written over)
  curl -fsSL -o "$2.new" "$1" && chmod +x "$2.new" && mv "$2.new" "$2"
}

has vhs "$VHS" || {
  t=$(mktemp -d)
  a=$arch; [ "$a" = aarch64 ] && a=arm64
  curl -fsSL "https://github.com/charmbracelet/vhs/releases/download/v$VHS/vhs_${VHS}_Linux_$a.tar.gz" | tar -xz -C "$t"
  install -m755 "$(find "$t" -type f -name vhs | head -n1)" "$bin/vhs"
  rm -rf "$t"
}
has ttyd "$TTYD" || fetch "https://github.com/tsl0922/ttyd/releases/download/$TTYD/ttyd.$arch" "$bin/ttyd"
has asciinema "$ASCIINEMA" || fetch "https://github.com/asciinema/asciinema/releases/download/v$ASCIINEMA/asciinema-$arch-unknown-linux-musl" "$bin/asciinema"
has agg "$AGG" || fetch "https://github.com/asciinema/agg/releases/download/v$AGG/agg-$arch-unknown-linux-musl" "$bin/agg"
# vhs needs a Chromium: the box has Playwright's headless shell behind ~/.local/bin/chromium
# (it adds --no-sandbox). Without one, vhs downloads its own on first run.
command -v ffmpeg >/dev/null || { sudo apt-get update -qq && sudo apt-get install -y -qq ffmpeg; }
