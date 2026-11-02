#!/bin/bash
# Run as "arch" after 10-packages.sh: Omarchy's root-side and user-side finalizers, the way the
# ISO runs them (omarchy-apply-system --install-user USER --first-install, then
# omarchy-provision-user --force --first-install as the user), plus the EC2 fix-ups.
set -uo pipefail
step() { echo "=== STEP $* ($(date +%T))"; }
U=$(id -un)

step "1/5 pre-empt snapper (ext4 root: no btrfs, so create-config would fail the whole run)"
sudo install -d /etc/snapper/configs
[[ -f /etc/snapper/configs/root ]] || sudo touch /etc/snapper/configs/root

step "2/5 omarchy-apply-system --install-user $U --first-install"
sudo env OMARCHY_MIRROR=stable OMARCHY_LOG_TO_STDOUT=1 /usr/bin/omarchy-apply-system --install-user "$U" --first-install
echo "APPLY_SYSTEM rc=$?"

step "3/5 EC2 fix-ups: keep SSH reachable, no display manager on a GPU-less box"
sudo ufw allow 22/tcp comment 'ssh (EC2 security group restricts the source)' || true
sudo systemctl disable sddm.service || true
# Live networking stays on the AMI's systemd-networkd until a reboot; Omarchy's network.sh
# switched the next boot to NetworkManager (untested here: the box is throwaway).

step "4/5 seed \$HOME from /etc/skel (what useradd -m does for a new Omarchy user)"
cp -af /etc/skel/. "$HOME"/

step "5/5 omarchy-provision-user --force --first-install"
# Not "runtime" (it would turn into iso-chroot and look for the ISO-bundled Node tarball in
# /opt/packages); any other value makes the theme step run headless and mise fetch Node online.
OMARCHY_SETUP_CONTEXT=ec2-online OMARCHY_PATH=/usr/share/omarchy /usr/bin/omarchy-provision-user --force --first-install
echo "PROVISION_USER rc=$?"
sudo loginctl enable-linger "$U"
echo "APPLY_DONE"
