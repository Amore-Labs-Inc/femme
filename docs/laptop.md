# Laptop: fork agents, sync folders, drop box, pc

Optional. The server works on its own; this is for moving agents between your laptop and the server.

```sh
./laptop/laptop-setup.sh
```

On Linux (systemd) or macOS (launchd): adds the server as a herdr machine, links `plugin/` as the `femme` plugin, adds its three keys to `~/.config/herdr/config.toml`, and starts Mutagen and `femme-link` (this laptop's terminals on the board). Needs herdr; Mutagen only for forking and sync. macOS support is new: report what does not work. Run `./laptop/sync.sh` afterwards so the server gets the keys too.

The keys it adds are below. A `[[keys.command]]` binding belongs to the machine selected in herdr, not to the client: the fork key is used on Local panes, so the laptop's own copy serves it (`herdr server reload-config` is enough), but the drop box key is pressed on agentbox panes, so it must be in the VPS's `config.toml` too, which `sync.sh` copies over and reloads (herdr issue #5183). `prefix+?` lists the selected machine's keys.

```toml
[[keys.command]]
key = "prefix+shift+v"
type = "plugin_action"
command = "femme.fork-remote"
description = "fork agent to VPS"

[[keys.command]]
key = "prefix+u"
type = "plugin_action"
command = "femme.files"
description = "drop box: files into the agent's folder"

[[keys.command]]
key = "prefix+f"
type = "plugin_action"
command = "femme.tui"
description = "FEMME: all agents"
```

`prefix+f` opens [the agent view](tui.md) as an overlay: every agent on the selected machine and the ones linked to it, the selected one's screen, a prompt box. On a VPS pane it runs the VPS's copy (built by `sync.sh`); on a Local pane it needs a local build, which `laptop-setup.sh` makes when cargo is installed.

Local and VPS workspaces show side by side. Local ones die when the laptop sleeps; `prefix+shift+v` on a local agent forks it to the VPS and leaves the local one running:

1. The agent's folder (its git root, or its cwd; git not required) is put in two-way sync with the same path on the VPS. The first time, rsync seeds it (never overwriting newer VPS files); after that Mutagen streams changes both ways.
2. The session file is copied, and the agent starts there in its own fork mode. Claude Code, Codex and Pi are supported.

Sync rules:
- `node_modules .venv venv __pycache__ target .cache .next` stay on their own side; reinstall deps on the VPS.
- Mode is two-way-safe. If both sides change the same file, that file pauses with a conflict and neither copy is overwritten. See them with `mutagen sync list`.
- Deletions propagate both ways, like any other change.
- Only folders inside `$HOME` are synced; an agent in `/tmp` gets an empty folder on the VPS.
- `sh laptop/sync-dir.sh <dir>` syncs a folder without forking anything. `mutagen sync terminate <name>` stops it, and both copies stay.

`prefix+u` (the prefix is `ctrl+b` unless `keys.prefix` says otherwise) on a VPS agent opens **Files · drop box** beside it: the agent's folder, newest first. Drag a file from the laptop's file manager onto the box and it is uploaded into that folder: a spinner while it runs, ✓ once it is listed. The copy goes through the laptop's linked herdr (an `upload: …` workspace flashes there), so `femme-link` must be running; the box says `laptop not linked` otherwise. A drop lands in the focused pane, so the box must be it (herdr's blue border): unfocused, it dims and shows a yellow *click here first* bar. A dropped image takes herdr's own image bridge, and a file from a synced folder is copied on the VPS side. `q` closes the box.

## Work on PC files from the VPS

Agents run on the VPS (its RAM) while the heavy files and the toolchain stay on the laptop: the folder's text syncs as above, and the agent builds on the laptop with `pc <command>`.

```sh
./laptop/pc-setup.sh                               # once, on the laptop: sshd on 127.0.0.1 only, the VPS's key, the link forwards it
```

`pc` needs a Linux laptop (Arch, Debian/Ubuntu or Fedora) for now.

Then per folder, on the laptop: put what must not leave it in `<dir>/.femme-ignore`, one Mutagen pattern per line, and sync it (`sh laptop/sync-dir.sh <dir>`, or `prefix+shift+v` on an agent there):

```
# big inputs and the build stay on the laptop
*.iso
/build
/assets/raw
```

Open a terminal on the board in that folder (Claude or Codex; both get the rule from `rules/pc.md`). The agent edits on the VPS and runs `pc make` (or `pc 'make && ./check.sh'`, or just `pc` for a shell): `pc` flushes the sync, runs the line on the laptop in the same folder through its login shell, and flushes again, so the build sees the edits and the results come back. Asleep or away laptop: edits go on, only `pc` says `laptop not linked`. Ignores are fixed when the session is made: `mutagen sync terminate <name>`, edit `.femme-ignore`, sync again. `PC_HOST` in `.env` picks the laptop when several are linked.

Trust: the VPS's key logs in to your laptop as you, so anything running as your user on the VPS can run commands there (as it can already type into the laptop's terminals while femme-link runs, see [SECURITY.md](../SECURITY.md)). The laptop's sshd is reachable only through the link the laptop itself opens.
