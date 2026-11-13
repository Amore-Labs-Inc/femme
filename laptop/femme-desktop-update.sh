#!/usr/bin/env bash
# PC side, from femme-desktop-update.timer (pc-setup.sh installs it): keeps ~/.local/bin/femme-desktop
# at the commit the box runs (~/.local/state/femme/deployed there, written by sync.sh and deploy.sh),
# so the app and femmed never drift apart. Built from `git archive` of that commit, not the working
# tree: the archive is synced by content into one source folder that stays put, so only the files the
# commit touched get a new mtime and cargo's incremental cache (profile `quick`: optimised, no LTO) does
# the rest, in this checkout's desktop/target. A small change installs in well under a minute; the
# menu's "check for updates" starts this at once instead of waiting for the timer. Safe to rerun; a
# failed build leaves the old binary. build.rs reads FEMME_COMMIT (the archive has no .git) so the
# menu knows which commit it runs.
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
new=$(mktemp -d "${TMPDIR:-/tmp}/femme-desktop-src.XXXXXX")
git archive "$want" | tar -x -C "$new"
# By checksum, not time (`git archive` stamps every file with the commit's time): an unchanged file
# keeps its mtime and cargo leaves its crate alone.
mkdir -p "$src" && rsync -rc --delete "$new/" "$src/" && rm -rf "$new"
# mold links the app in a second or two where the default linker takes ten (laptop-setup.sh installs it).
command -v mold >/dev/null && export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-fuse-ld=mold"
FEMME_COMMIT=$want ~/.cargo/bin/cargo build -q --profile quick --manifest-path "$src/desktop/Cargo.toml" --target-dir desktop/target
# The daemon and holder of the same commit, beside the app: the desktop starts `femmed local` from
# there for sessions that run on this laptop (docs/plan-local.md). Their own target dir: the root
# workspace and the desktop's are two workspaces, and cargo must not mix them in one.
~/.cargo/bin/cargo build -q --release --manifest-path "$src/Cargo.toml" --target-dir desktop/target-femmed -p femmed -p femme-holder
# Copy then rename: the running app keeps its old binary open until it is restarted. Stripped here
# rather than by the profile, which would make every build strip again.
install -Dsm755 desktop/target/quick/femme-desktop ~/.local/bin/femme-desktop.new && mv ~/.local/bin/femme-desktop.new ~/.local/bin/femme-desktop
for b in femmed femme-holder; do
  install -Dsm755 "desktop/target-femmed/release/$b" ~/.local/bin/$b.new && mv ~/.local/bin/$b.new ~/.local/bin/$b
done
mkdir -p "$state" && echo "$want" > "$state/desktop-commit"
# The launcher entry (app menus); rewritten each time so a moved checkout cannot leave it stale.
mkdir -p ~/.local/share/applications ~/.local/share/icons/hicolor/scalable/apps
cp assets/logo.svg ~/.local/share/icons/hicolor/scalable/apps/femme.svg
printf '[Desktop Entry]\nType=Application\nName=FEMME\nComment=Agent sessions on the box\nExec=%s/.local/bin/femme-desktop\nIcon=femme\nTerminal=false\nCategories=Development;\n' "$HOME" > ~/.local/share/applications/femme-desktop.desktop
what=$(git log -1 --format='%h %s' "$want")
echo "femme-desktop-update: installed $what"
notify-send "FEMME desktop updated" "$what. Restart the app to use it." 2>/dev/null || true
