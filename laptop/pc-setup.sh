#!/usr/bin/env bash
# Laptop side, once (Linux: Arch, Debian/Ubuntu or Fedora): lets `pc` on the VPS run commands here, in a synced folder
# (docs/laptop.md: Work on PC files from the VPS). sshd listens on 127.0.0.1 only; the VPS reaches it
# through the link this machine opens (femme-link.service), so nothing is exposed to the
# network. Safe to rerun.
set -euo pipefail
cd "$(dirname "$0")/.."  # the checkout root: .env and every path below are relative to it
. ./.env

command -v mutagen >/dev/null && systemctl --user is-enabled -q mutagen \
  || { echo "pc-setup: install mutagen and enable mutagen.service first (./laptop/laptop-setup.sh)" >&2; exit 1; }

[ "$(uname -s)" = Linux ] || { echo "pc-setup: pc needs a Linux laptop for now" >&2; exit 1; }
if command -v pacman >/dev/null; then sudo pacman -S --needed --noconfirm openssh
elif command -v apt-get >/dev/null; then sudo apt-get install -y -qq openssh-server
elif command -v dnf >/dev/null; then sudo dnf install -y -q openssh-server
else echo "pc-setup: install the OpenSSH server yourself, then rerun" >&2; command -v sshd >/dev/null || exit 1; fi
grep -q '^Include /etc/ssh/sshd_config.d/\*\.conf' /etc/ssh/sshd_config \
  || sudo sed -i '1i Include /etc/ssh/sshd_config.d/*.conf' /etc/ssh/sshd_config
printf 'ListenAddress 127.0.0.1\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nAllowUsers %s\n' "$(id -un)" \
  | sudo tee /etc/ssh/sshd_config.d/femme.conf >/dev/null
sudo sshd -t
svc=sshd; systemctl list-unit-files ssh.service >/dev/null 2>&1 && svc=ssh
sudo systemctl enable --now $svc
sudo systemctl reload $svc

# The VPS's own key (bootstrap.sh makes it) may log in here.
key=$(ssh "$HOST" cat .ssh/id_ed25519.pub) || { echo "pc-setup: the VPS has no key yet: ./laptop/sync.sh first" >&2; exit 1; }
mkdir -p ~/.ssh && chmod 700 ~/.ssh && touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys
grep -qxF "$key" ~/.ssh/authorized_keys || echo "$key" >> ~/.ssh/authorized_keys

# The link now also forwards this sshd; the VPS names "Host pc" after the linked laptop.
mkdir -p ~/.config/systemd/user
cp laptop/systemd/femme-link.service ~/.config/systemd/user/
# The desktop app (desktop/README.md), rebuilt here whenever the box moves to a new commit (FEMMED=1 or 2 there).
if [ "${FEMMED:-0}" != 0 ]; then
  sed "s|@SRC@|$PWD|" laptop/systemd/femme-desktop-update.service > ~/.config/systemd/user/femme-desktop-update.service
  cp laptop/systemd/femme-desktop-update.timer ~/.config/systemd/user/
fi
systemctl --user daemon-reload
systemctl --user enable --now femme-link
systemctl --user restart femme-link
[ "${FEMMED:-0}" = 0 ] || systemctl --user enable --now femme-desktop-update.timer
ssh "$HOST" 'grep -q "^Host pc$" ~/.ssh/config 2>/dev/null' \
  || ssh "$HOST" "cd femme && HERDR_VERSION=$(herdr --version | cut -d' ' -f2) ./server/bootstrap.sh" >/dev/null

echo "pc-setup: checking from the VPS..."
for _ in 1 2 3 4 5 6; do
  ssh "$HOST" ssh -o BatchMode=yes pc true 2>/dev/null && { echo "pc-setup: ok, pc works on the VPS"; exit 0; }
  sleep 5
done
echo "pc-setup: the VPS could not reach this machine; there, try: ssh -v pc true" >&2
exit 1
