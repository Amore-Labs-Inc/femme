#!/bin/bash
# Run once as "arch": a GPU-less EC2 box gets a virtual KMS device (vkms) and seatd, and Hyprland
# runs on it from a systemd user service (the user lingers, so no login is needed).
set -euo pipefail
U=$(id -un)

# vkms: a virtual DRM/KMS card with one connector (Virtual-1) and a 60 Hz vblank timer. Aquamarine
# needs a DRM fd for its GBM allocator even for headless outputs, so "headless only" is not enough
# on a box with no /dev/dri at all. Mesa renders through kms_swrast/llvmpipe on it.
echo vkms | sudo tee /etc/modules-load.d/vkms.conf >/dev/null
sudo modprobe vkms

# seatd hands the DRM device to a process that is not in a logind seat session (we come in over SSH).
sudo systemctl enable --now seatd.service
sudo usermod -aG seat,video,render,input "$U"

mkdir -p ~/.config/systemd/user ~/.local/bin
cat > ~/.local/bin/femme-hypr-env <<'EOS'
#!/bin/bash
# Waits for the Hyprland instance started by hyprland-headless.service and writes ~/.femme-wayland.env.
for _ in $(seq 1 120); do
  sig=$(ls -t "$XDG_RUNTIME_DIR/hypr" 2>/dev/null | head -n1)
  if [[ -n $sig && -S $XDG_RUNTIME_DIR/hypr/$sig/.socket.sock ]]; then
    sock=$(ls -t "$XDG_RUNTIME_DIR" | grep -E '^wayland-[0-9]+$' | head -n1)
    if [[ -n $sock ]]; then
      cat > "$HOME/.femme-wayland.env" <<EOV
export XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR
export WAYLAND_DISPLAY=$sock
export HYPRLAND_INSTANCE_SIGNATURE=$sig
export XDG_SESSION_TYPE=wayland
export XDG_CURRENT_DESKTOP=Hyprland
EOV
      # vkms prefers 1024x768, which Omarchy's "auto" scale turns into scale 2. Give it a laptop-like
      # 1920x1080 at scale 1 (FEMME_MODE / FEMME_SCALE override). Runtime only, the config is untouched.
      HYPRLAND_INSTANCE_SIGNATURE=$sig hyprctl eval "hl.monitor({ output = 'Virtual-1', mode = '${FEMME_MODE:-1920x1080@60}', position = '0x0', scale = ${FEMME_SCALE:-1} })" >/dev/null
      exit 0
    fi
  fi
  sleep 0.5
done
echo "Hyprland did not come up" >&2
exit 1
EOS
chmod +x ~/.local/bin/femme-hypr-env

cat > ~/.config/systemd/user/hyprland-headless.service <<'EOS'
[Unit]
Description=Hyprland on vkms (headless EC2 test box)

[Service]
Type=simple
Environment=LIBSEAT_BACKEND=seatd
# vkms has no render node, so the compositor's dmabuf feedback names no render device and Mesa
# falls back to zink -> Vulkan, where the MESA device_select layer segfaults (Quickshell, eglinfo,
# vulkaninfo all crash). Clients get llvmpipe for GL and skip that layer for Vulkan (lavapipe).
Environment=LIBGL_ALWAYS_SOFTWARE=1
Environment=NODEVICE_SELECT=1
Environment=XDG_SESSION_TYPE=wayland
Environment=XDG_CURRENT_DESKTOP=Hyprland
Environment=OMARCHY_PATH=/usr/share/omarchy
Environment=PATH=/usr/share/omarchy/bin:/usr/local/bin:/usr/bin
ExecStartPre=-/bin/rm -f %h/.femme-wayland.env
ExecStart=/usr/bin/start-hyprland
ExecStartPost=%h/.local/bin/femme-hypr-env
Restart=no

[Install]
WantedBy=default.target
EOS

# The user manager must pick up the new groups before it can start Hyprland.
sudo systemctl restart "user@$(id -u).service"
echo "HEADLESS_SETUP_DONE"
