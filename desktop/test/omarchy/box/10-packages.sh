#!/bin/bash
# Run as the AMI's "arch" user (passwordless sudo). Needs /tmp/pacman-stable.conf and
# /tmp/mirrorlist-stable from omarchy's default/pacman/ next to it. Idempotent.
set -euo pipefail
step() { echo "=== STEP $* ($(date +%T))"; }
OMARCHY_KEY=40DFB630FF42BCFFB047046CF0134EE680CAC571

step "1/5 pacman keyring + mirrors"
if [[ ! -f /etc/pacman.d/.femme-init ]]; then
  sudo bash /etc/pacman.d/pacman_init.sh
  sudo pacman-key --recv-keys $OMARCHY_KEY --keyserver keys.openpgp.org
  sudo pacman-key --lsign-key $OMARCHY_KEY
  sudo touch /etc/pacman.d/.femme-init
fi

step "2/5 Omarchy stable pacman.conf + mirrorlist"
[[ -f /etc/pacman.conf.ami ]] || sudo cp /etc/pacman.conf /etc/pacman.conf.ami
[[ -f /etc/pacman.d/mirrorlist.reflector ]] || sudo cp /etc/pacman.d/mirrorlist /etc/pacman.d/mirrorlist.reflector
sudo install -m644 /tmp/pacman-stable.conf /etc/pacman.conf
{ cat /tmp/mirrorlist-stable; echo; echo "# reflector fallback"; grep ^Server /etc/pacman.d/mirrorlist.reflector | head -5; } | sudo tee /etc/pacman.d/mirrorlist >/dev/null

step "3/5 full upgrade onto the stable channel snapshot"
sudo pacman -Syyuu --noconfirm

step "4/5 omarchy packages"
sudo pacman -S --noconfirm --needed omarchy-keyring omarchy-settings omarchy omarchy-nvim

step "5/5 omarchy base package list + headless extras"
mapfile -t base < <(sed 's/#.*//' /usr/share/omarchy/install/omarchy-base.packages | tr -s ' \t' '\n' | grep -v '^$')
extras=(pipewire pipewire-alsa pipewire-jack pipewire-pulse gst-plugin-pipewire libpulse gtk4-layer-shell webp-pixbuf-loader
        mesa vulkan-swrast vulkan-tools mesa-utils wayland-utils seatd)
echo "installing ${#base[@]} base + ${#extras[@]} extra packages"
sudo pacman -S --noconfirm --needed "${base[@]}" "${extras[@]}"
echo "PACKAGES_DONE"
df -h /
