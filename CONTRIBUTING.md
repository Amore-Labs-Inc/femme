# Contributing

Thanks for helping. One rule shapes everything here: **FEMME stays small.** It is a thin layer that keeps herdr and coding agents running on your own server. Fixes, clearer docs, easier installs and support for more setups are welcome. New features are a hard sell: open an issue first and say what problem they solve.

## Before you open a PR

- **Board (Rust):** `cargo test --manifest-path board/Cargo.toml`. The board has five dependencies; adding one needs a good reason.
- **TUI (Rust):** `cargo test --manifest-path tui/Cargo.toml` (the layouts render headless in the tests), `cargo run --manifest-path tui/Cargo.toml -- --demo` to look at it without herdr. Five dependencies too.
- **Shell scripts:** `bash -n <file>` and `shellcheck <file>`. CI fails on shellcheck errors.
- **Laptop-side scripts** (`install.sh`, `doctor.sh`, `uninstall.sh` and everything in `laptop/`) must run under **bash 3.2** (what macOS ships) and with **BSD tools**: no `sed -i` without a suffix, no `readarray`, no `${var,,}`. Scripts in `server/` can assume Ubuntu 24.04. Every script `cd`s to the checkout root first, so paths inside read the same from anywhere.
- **Anything that changes the install:** run it on a fresh Ubuntu 24.04 server (`./install.sh`, then `./doctor.sh` all green, then `./uninstall.sh server`), and say so in the PR.
- **No secrets, no personal data:** no keys, tokens, IPs, email addresses, hostnames or private repo names, in code, tests, docs or examples. Use `example-org/example-repo`, `203.0.113.7`, `hello@example.com`. CI runs gitleaks.
- **Safe defaults:** anything that widens what the server or its agents can do (network exposure, cloud permissions, running code from elsewhere) is off by default and explained in `.env.example` and [SECURITY.md](SECURITY.md).

## Visuals

The README images are HTML pages in `assets/src/`, in the Exposure brand: Geist, Academy Cream, Exposure Blue as the one accent, and the parent brand's blue-violet-ember glow on dark. Each renders in a light and a dark version; GitHub shows the one that matches the reader's theme. Every card says something true about FEMME. To change one, edit its page and run `assets/src/render.sh` (needs Google Chrome). Keep text in images short, and put the same meaning in the image's `alt`.

## How the pieces fit

Three commands sit at the top: `install.sh`, `doctor.sh`, `uninstall.sh`. The rest is grouped by where it runs: `laptop/`, `server/` (copied to `~/femme` on the server), `plugin/` (the herdr plugin), `board/` (the Rust server).

| File | Runs on | Does |
|---|---|---|
| `doctor.sh` | laptop | Checks the installation; each problem comes with the fix. |
| `install.sh` | laptop | The one entry point: questions, `.env`, server, check. `update` pulls and updates. |
| `uninstall.sh` | laptop | Takes FEMME off the server or the laptop; lists AWS resources without deleting them. |
| `laptop/cloud-init.yaml` | VPS, first boot | Your user (same name as on the laptop, so paths match), key-only SSH, 8 GB swap, linger. |
| `laptop/host-init.sh` | laptop | Prepares an existing Ubuntu 24.04 server over ssh, as `cloud-init.yaml` does on AWS. |
| `laptop/laptop-setup.sh` | laptop | herdr machine, plugin, keys and the background services (Linux or macOS). |
| `laptop/lib.sh` | laptop | Helpers the laptop scripts share. |
| `laptop/pc-setup.sh` | laptop | Lets `pc` on the VPS run commands here. |
| `laptop/provision.sh` | laptop | Creates the instance. Safe to rerun; reuses everything tagged `femme`. |
| `laptop/sync-dir.sh` | laptop | Puts one folder in live two-way sync with the VPS, minus its `.femme-ignore`. |
| `laptop/sync.sh` | laptop | Copies config: `~/.claude` minus credentials and history, herdr, mise. Merges onboarding, trust and portable MCP servers into the VPS `~/.claude.json`. |
| `laptop/systemd/femme-link.service` | laptop | Puts the laptop's herdr terminals on the board and its sshd within reach of `pc`. |
| `laptop/systemd/mutagen.service` | laptop | Keeps sync sessions running across reboots. |
| `server/bootstrap.sh` | VPS | Installs mise toolchains, agents, herdr and the systemd units; builds the board. |
| `server/deploy.sh` | VPS | Optional (`AUTODEPLOY=1`), on a timer: deploys `main` after a merge. |
| `server/fable-cap.sh`, `server/systemd/femme-fable-cap.service` | VPS | Optional: auto-rotates Claude accounts at `FABLE_CAP` of the Fable weekly limit. |
| `server/notify.sh` | VPS | Polls `herdr api snapshot`, pushes to ntfy on `blocked` and `done`. |
| `server/pc.sh` | VPS | `pc <command>`: runs it on the laptop in the same folder, sync flushed around it. |
| `server/rules/pc.md` | VPS | Tells Claude and Codex when to use `pc`. |
| `server/worker.sh` | VPS | Runs board issues as herdr Claude agents within the budget. |
| `server/systemd/femme-cswap.service` | VPS | Optional: auto-rotates Claude accounts at `CAP`. |
| `server/systemd/herdr-instinct.service` | VPS | The herdr session the API's standing sessions run in. |
| `plugin/files.sh`, `plugin/files-pane.sh` | VPS | The plugin's drop box: action and pane. Files dragged from the laptop land in the agent's folder. |
| `plugin/fork-remote.sh` | laptop | The herdr plugin action: fork to the VPS. |
| `plugin/herdr-plugin.toml` | laptop and VPS | The herdr plugin: fork to the server, the drop box. `laptop-setup.sh` and `bootstrap.sh` link `plugin/`. |
| `board/` | VPS | The issues board server (Rust). |
| `tui/` | VPS, laptop | `femme-tui`: every agent, one screen, a prompt box, over herdr's socket API (Rust). |

## Security issues

Not in a public issue: see [SECURITY.md](SECURITY.md).

## License

By contributing you agree that your contribution is licensed under the [Apache License 2.0](LICENSE).
