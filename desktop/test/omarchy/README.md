# Omarchy + headless Hyprland test box on EC2 (femme-desktop)

A throwaway EC2 instance running Arch Linux with Omarchy 4.0.6 (stable channel) installed on top,
Hyprland 0.56.2 running on a virtual KMS device with software rendering, and femme-desktop running
in it. Built 2026-11-02, **terminated the same day** (the user asked for nothing to keep billing).
This file says how to rebuild it from scratch.

## What existed (all deleted)

| Resource | Id | Notes |
|---|---|---|
| Instance | `i-0151e73874c80afe7` | c6a.xlarge, eu-central-1b, subnet-0680ef0d7d40cb03b (default VPC), terminated |
| Root volume | (auto, DeleteOnTermination) | 40 GB gp3, deleted with the instance |
| Security group | `sg-024fa8c080f33e4eb` (`femme-omarchy-test`) | inbound 22/tcp from agentbox's public IP only, deleted |
| Key pair | `key-05e6e25f084617afb` (`femme-omarchy-test`) | RSA, deleted; the local .pem was removed too |

All were tagged `Name=femme-omarchy-test`, `femme=omarchy-test`.

## Cost

c6a.xlarge (4 vCPU, 8 GB) on-demand in eu-central-1: **$0.1746/h**, plus 40 GB gp3 at $0.0952/GB-month
(about $0.005/h) and the public IPv4 at $0.005/h: about **$0.185/h** in all while running. Nothing bills
after termination. (t3.xlarge would be $0.192/h and is burstable; llvmpipe keeps the CPU busy, so c6a.)

## AMI

Arch Linux has no official AMIs (Arch wiki, "Arch Linux AMIs for Amazon Web Services"). The wiki points to
the community images built on the 1st and 15th of each month, owner **647457786197**, listed by
`https://arch-ami-api.drzee.net/`. Used: **`ami-02585057621942f5c`** = `arch-linux-std-hvm-2026.11.01.x86_64-ebs`
(stock Arch kernel, BIOS boot with GRUB, ext4 root, cloud-init user `arch`, RSA keys only). Old AMIs get
replaced; look up the current one with

```sh
curl -s https://arch-ami-api.drzee.net/eu-central-1/x86_64/std/latest
aws ec2 describe-images --region eu-central-1 --owners 647457786197 \
  --filters 'Name=name,Values=arch-linux-std-hvm-*' --query 'sort_by(Images,&CreationDate)[-1].[ImageId,Name]' --output text
```

## Recreate from scratch (about 10 minutes, about 6 GB of packages)

Run from agentbox (its instance role may run EC2 instances). `D` is this folder; the box scripts are in `$D/box/`.

```sh
D=$(git rev-parse --show-toplevel)/desktop/test/omarchy   # the key, ip and known_hosts land here (gitignored)
R=eu-central-1; TAGS='{Key=Name,Value=femme-omarchy-test},{Key=femme,Value=omarchy-test}'
TOKEN=$(curl -s -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
MYIP=$(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/public-ipv4)
AMI=$(curl -s https://arch-ami-api.drzee.net/$R/x86_64/std/latest | jq -r '.arch_amis[0].ami')

# key (RSA: cloud-init on these images rejects EC keys)
(umask 077; aws ec2 create-key-pair --region $R --key-name femme-omarchy-test --key-type rsa --key-format pem \
  --tag-specifications "ResourceType=key-pair,Tags=[$TAGS]" --query KeyMaterial --output text > $D/femme-omarchy.pem)

# SSH from agentbox only
SG=$(aws ec2 create-security-group --region $R --group-name femme-omarchy-test --vpc-id vpc-04cae3d9c10182218 \
  --description 'femme omarchy test box: SSH from agentbox only' \
  --tag-specifications "ResourceType=security-group,Tags=[$TAGS]" --query GroupId --output text)
aws ec2 authorize-security-group-ingress --region $R --group-id $SG \
  --ip-permissions "IpProtocol=tcp,FromPort=22,ToPort=22,IpRanges=[{CidrIp=$MYIP/32,Description=agentbox}]"

# instance: 40 GB gp3 root, same AZ as agentbox
ID=$(aws ec2 run-instances --region $R --image-id $AMI --instance-type c6a.xlarge --key-name femme-omarchy-test \
  --security-group-ids $SG --subnet-id subnet-0680ef0d7d40cb03b --associate-public-ip-address \
  --block-device-mappings 'DeviceName=/dev/sda1,Ebs={VolumeSize=40,VolumeType=gp3,DeleteOnTermination=true}' \
  --metadata-options HttpTokens=required \
  --tag-specifications "ResourceType=instance,Tags=[$TAGS]" "ResourceType=volume,Tags=[$TAGS]" \
  --query 'Instances[0].InstanceId' --output text)
aws ec2 wait instance-running --region $R --instance-ids $ID
aws ec2 describe-instances --region $R --instance-ids $ID --query 'Reservations[0].Instances[0].PublicIpAddress' --output text > $D/ip
rm -f $D/known_hosts      # new host key
```

SSH (`$D/ssh.sh` wraps it): `ssh -i $D/femme-omarchy.pem -o UserKnownHostsFile=$D/known_hosts arch@$(cat $D/ip)`

Install (the box has passwordless sudo for `arch`):

```sh
scp -i $D/femme-omarchy.pem -o UserKnownHostsFile=$D/known_hosts \
  $D/box/pacman-stable.conf $D/box/mirrorlist-stable arch@$(cat $D/ip):/tmp/
scp -i $D/femme-omarchy.pem -o UserKnownHostsFile=$D/known_hosts \
  $D/box/10-packages.sh $D/box/20-omarchy-apply.sh $D/box/30-headless-setup.sh arch@$(cat $D/ip):
$D/ssh.sh './10-packages.sh > 10-packages.log 2>&1'          # ~3 min; greps: "=== STEP", PACKAGES_DONE
$D/ssh.sh './20-omarchy-apply.sh > 20-omarchy-apply.log 2>&1' # ~30 s; APPLY_SYSTEM rc=0, PROVISION_USER rc=0
$D/ssh.sh './30-headless-setup.sh'                             # vkms, seatd, the Hyprland user service
scp -i $D/femme-omarchy.pem -o UserKnownHostsFile=$D/known_hosts $D/box/femme-run $D/box/femme-shot arch@$(cat $D/ip):.local/bin/
```

What the scripts do:

1. `10-packages.sh`: the AMI's `pacman_init.sh` (keyring, reflector), trusts Omarchy's signing key
   `40DFB630FF42BCFFB047046CF0134EE680CAC571` (keys.openpgp.org), installs Omarchy's **stable**
   `pacman.conf` (`[omarchy]` = `pkgs.omarchy.org/stable`) and mirrorlist (`stable-mirror.omarchy.org`,
   Omarchy's pinned Arch snapshot, reflector mirrors after it), `pacman -Syyuu`, then
   `omarchy-keyring omarchy-settings omarchy omarchy-nvim`, then every package in the installed
   `/usr/share/omarchy/install/omarchy-base.packages` plus pipewire, `mesa`, `vulkan-swrast` (lavapipe),
   `seatd` and a few tools. `pacman-stable.conf` / `mirrorlist-stable` are Omarchy's
   `default/pacman/` files at tag v4.0.6 (identical on `quattro` today).
2. `20-omarchy-apply.sh`: what the Omarchy ISO runs after pacstrap: `omarchy-apply-system --install-user arch
   --first-install` as root (config, hardware, login, post-install scripts) and
   `omarchy-provision-user --force --first-install` as the user, after seeding `$HOME` from `/etc/skel`
   (what `useradd -m` does for a new Omarchy user; this box reuses the AMI's `arch` user). EC2 fix-ups below.
3. `30-headless-setup.sh`: loads `vkms`, enables `seatd`, adds `arch` to seat/video/render/input, writes
   `~/.config/systemd/user/hyprland-headless.service` and `~/.local/bin/femme-hypr-env`, lingers the user.

Omarchy's online installer is gone: `https://omarchy.org/install` now only prints "Omarchy installs from
its ISO now" (removed June 2026), and the repo has no `boot.sh`/`install.sh` any more. Omarchy 4 ships as
packages (`omarchy`, `omarchy-settings`, `omarchy-keyring`, `omarchy-nvim`) and the ISO runs the two
finalizers above, so installing on an existing Arch system means doing the same by hand.

## Start Hyprland (if it is not running)

```sh
$D/ssh.sh 'systemctl --user start hyprland-headless; cat ~/.femme-wayland.env'
$D/ssh.sh '. ~/.femme-wayland.env; hyprctl monitors; hyprctl workspaces; grim ~/shots/desktop.png'
```

The service runs `start-hyprland` with `LIBSEAT_BACKEND=seatd`, `LIBGL_ALWAYS_SOFTWARE=1`, `NODEVICE_SELECT=1`,
on Omarchy's own `~/.config/hypr/hyprland.lua` (unchanged). `femme-hypr-env` waits for the instance,
writes `~/.femme-wayland.env` (`XDG_RUNTIME_DIR`, `WAYLAND_DISPLAY=wayland-1`,
`HYPRLAND_INSTANCE_SIGNATURE`, ...) and sets the vkms output `Virtual-1` to 1920x1080@60 scale 1 at runtime
(`FEMME_MODE`, `FEMME_SCALE` override). Any later SSH session: `. ~/.femme-wayland.env` and `hyprctl`,
`grim`, `slurp`, `wtype` work. Omarchy's autostart runs as on a laptop: Quickshell bar, wallpaper,
first-run (welcome toast), udiskie. Stop: `systemctl --user stop hyprland-headless`.

## Run femme-desktop and screenshot it

```sh
strip --strip-debug -o $D/femme-desktop $D/../../target/debug/femme-desktop  # 757 MB -> 147 MB
scp -i $D/femme-omarchy.pem -o UserKnownHostsFile=$D/known_hosts $D/femme-desktop arch@$(cat $D/ip):
$D/ssh.sh 'femme-run --mode fire,shell --panes 2'      # log: ~/femme.log
$D/ssh.sh '. ~/.femme-wayland.env; hyprctl clients -j | jq ".[] | {class, workspace}"; hyprctl activeworkspace'
$D/ssh.sh 'femme-shot ~/shots/femme.png'               # show special:femme, grim, hide it again
scp -i $D/femme-omarchy.pem -o UserKnownHostsFile=$D/known_hosts arch@$(cat $D/ip):shots/femme.png $D/
```

`femme-run` is the recipe from `desktop/README.md` ("Benchmarking where nobody is looking"): a
`hl.window_rule` for class `femme-desktop` (workspace `special:femme silent`, monitor `Virtual-1 silent`,
float, 1100x700, `render_unfocused`), then `hl.dsp.exec_cmd("[workspace special:femme silent] ...")`.
The app itself does not choose the workspace: `src/display.rs` only picks a display and says that on
Wayland a compositor rule has to pin the window. `femme-shot` toggles with
`hl.dsp.workspace.toggle_special("femme")` (Hyprland 0.56 Lua dispatch).

The binary built on Ubuntu 24.04 (needs GLIBC_2.39) runs unchanged on Arch (glibc 2.44); every library
resolves (`libxcb`, `libxkbcommon`, `libxkbcommon-x11` are present after the Omarchy install). It renders
through Vulkan on lavapipe (`libvulkan_lvp.so`).

## Result of the 2026-11-02 run

- Omarchy 4.0.6 (`/etc/os-release` says Omarchy), Hyprland 0.56.2 (commit efb50993), Quickshell 0.3.2,
  Mesa 26.2.5, kernel 7.2.11-arch1-1.
- `hyprctl monitors`: `Virtual-1` 1920x1080@60, scale 1. `hyprctl workspaces`: workspace 1, 0 windows.
- femme-desktop (`--mode fire,shell --panes 2`) landed on **`special:femme`** (id -98), floating 1100x700
  at 410,203; the active workspace stayed 1. About 230% CPU on llvmpipe/lavapipe.
- Screenshots: `01-empty-desktop.png` (in the session scratchpad, not the repo) (Omarchy desktop, empty workspace 1), `02-femme-special.png`
  (special:femme shown), `03-desktop-after.png` (special hidden again, desktop unchanged), with
  `01-monitors.txt`, `01-workspaces.txt`, `02-clients.txt`, `02-clients.json`.

## Problems met, and what this box does differently from a laptop

- **No GPU, no `/dev/dri`.** Aquamarine's headless backend alone cannot start: its GBM allocator needs a DRM
  fd from the DRM or Wayland backend ("Cannot open backend: no allocator available"), and
  `HYPRLAND_HEADLESS_ONLY` is not read by Hyprland 0.56 itself (only hyprtester sets it; Hyprland's CI runs
  hyprtester in a QEMU VM with `virtio-gpu-pci`). The box uses `vkms` instead: a virtual KMS card with one
  connector and a 60 Hz vblank timer, so Hyprland runs its normal DRM backend (seatd hands it the device,
  since an SSH login has no logind seat) and renders with llvmpipe. Aquamarine logs
  `CDRMRenderer(drm): Can't create renderer` / `Failed to update renderer state for Virtual-1` on every
  commit (its multi-GPU blitter; the screen renders fine). `hyprctl output create headless` also works for
  extra outputs.
- **Mesa crashes clients without a render node.** vkms has no `renderD*`, so the compositor's dmabuf
  feedback names no render device, EGL clients fall through to zink, and the `VK_LAYER_MESA_device_select`
  implicit layer segfaults in `vkEnumeratePhysicalDevices`. Quickshell crashed 6 times this way, and so did
  `eglinfo`, `vulkaninfo` and femme-desktop itself (rc 139) without the workaround.
  `LIBGL_ALWAYS_SOFTWARE=1` (GL -> llvmpipe) and `NODEVICE_SELECT=1` (skip that layer, Vulkan -> lavapipe)
  in the Hyprland service fix all of them. A box artifact; a real GPU has a render node.
- **Session start**: SDDM's Omarchy session is `uwsm start -e -D Hyprland hyprland.desktop`; this box runs
  `start-hyprland` from a systemd user service instead (no SDDM, no uwsm-managed units).
- **Omarchy steps skipped or neutralised**:
  - Limine bootloader, its UKI/snapper hooks and the ESP: the AMI boots BIOS + GRUB, so the Limine pacman
    hooks print `FAT32 boot partition not found` on every kernel/initramfs transaction (harmless here).
  - Snapper: ext4 root, no btrfs, so `create-config` would fail and abort `omarchy-apply-system`; an empty
    `/etc/snapper/configs/root` is pre-created so the step only writes the template.
  - `linux-omarchy` kernel, NVIDIA/Intel/Apple/ASUS/Framework hardware packages (`omarchy-other.packages`),
    plymouth splash (installed, never shown), LUKS, the archinstall disk layout, factory snapshot, SDDM
    autologin: ISO or hardware only.
  - SDDM: disabled (no display for it).
  - Bundled Node tarball: `omarchy-provision-user --first-install` expects the ISO's
    `/opt/packages/node-*.tar.gz`; run with `OMARCHY_SETUP_CONTEXT=ec2-online`, mise fetched Node 26.13.0.
  - `walker`: not part of Omarchy 4 (the Quickshell shell replaced it); `hyprctl`, `grim`, `slurp`, `wtype`
    are installed.
- **Never rebooted.** Omarchy's `network.sh` disabled systemd-networkd for the next boot and enabled
  NetworkManager; `firewall.sh` enabled ufw for the next boot (22/tcp was added to its rules). The live
  network stayed on the AMI's networkd. A reboot was not needed and was not tested.
- Seen in the screenshot, app side: at 1100x700 the left column's "sessions" panel shows only a clipped
  first row.
