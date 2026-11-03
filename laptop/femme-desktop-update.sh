#!/usr/bin/env bash
# PC side, from femme-desktop-update.timer (pc-setup.sh installs it): keeps ~/.local/bin/femme-desktop
# at the commit the box runs (~/.local/state/femme/deployed there, written by sync.sh and deploy.sh),
# so the app and femmed never drift apart. Built from `git archive` of that commit, not the working
# tree, in this checkout's desktop/target (warm). Safe to rerun; a failed build leaves the old binary. build.rs reads
# FEMME_COMMIT (the archive has no .git) so the menu's "check for updates" knows which commit it runs.
set -euo pipefail
cd "$(dirname "$0")/.."  # the checkout root: .env and every path below are relative to it
. ./.env
state=$HOME/.local/state/femme
want=$(ssh -o BatchMode=yes -o ConnectTimeout=10 "$HOST" cat .local/state/femme/deployed 2>/dev/null) || exit 0  # box away: next tick
[ -n "$want" ] || exit 0
[ "$want" != "$(cat "$state/desktop-commit" 2>/dev/null || :)" ] || exit 0
# The commit arrives through the synced .git or GitHub.
git cat-file -e "$want^{commit}" 2>/dev/null || git fetch -q origin 2>/dev/null || :
git cat-file -e "$want^{commit}" 2>/dev/null || { echo "femme-desktop-update: $want is not in this checkout yet" >&2; exit 0; }
src=${XDG_CACHE_HOME:-$HOME/.cache}/femme-desktop-src
rm -rf "$src" && mkdir -p "$src" && git archive "$want" | tar -x -C "$src"
FEMME_COMMIT=$want ~/.cargo/bin/cargo build -q --release --manifest-path "$src/desktop/Cargo.toml" --target-dir desktop/target
# Copy then rename: the running app keeps its old binary open until it is restarted.
install -Dm755 desktop/target/release/femme-desktop ~/.local/bin/femme-desktop.new && mv ~/.local/bin/femme-desktop.new ~/.local/bin/femme-desktop
mkdir -p "$state" && echo "$want" > "$state/desktop-commit"
# The launcher entry (app menus); rewritten each time so a moved checkout cannot leave it stale.
mkdir -p ~/.local/share/applications ~/.local/share/icons/hicolor/scalable/apps
cp assets/logo.svg ~/.local/share/icons/hicolor/scalable/apps/femme.svg
printf '[Desktop Entry]\nType=Application\nName=FEMME\nComment=Agent sessions on the box\nExec=%s/.local/bin/femme-desktop\nIcon=femme\nTerminal=false\nCategories=Development;\n' "$HOME" > ~/.local/share/applications/femme-desktop.desktop
what=$(git log -1 --format='%h %s' "$want")
echo "femme-desktop-update: installed $what"
notify-send "FEMME desktop updated" "$what. Restart the app to use it." 2>/dev/null || true
