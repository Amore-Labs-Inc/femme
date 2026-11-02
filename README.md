<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/banner-dark.png">
  <img alt="FEMME, open source by Exposure: Claude Code, Codex and Pi on a server you own. They keep working after you close the laptop: three agents still running over ssh while the laptop lid is closed." src="assets/banner-light.png" width="100%">
</picture>

<p align="center">
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/license-Apache--2.0-0339A6?style=flat-square"></a>
  <img alt="Server: Ubuntu 24.04, x86_64 or arm64" src="https://img.shields.io/badge/server-Ubuntu%2024.04-0339A6?style=flat-square">
  <img alt="Laptop: Linux or macOS" src="https://img.shields.io/badge/laptop-Linux%20%7C%20macOS-0339A6?style=flat-square">
  <img alt="Agents: Claude Code, Codex, Pi" src="https://img.shields.io/badge/agents-Claude%20Code%20%7C%20Codex%20%7C%20Pi-7C3AED?style=flat-square">
</p>

FEMME keeps your coding agents running on a server you own. Start Claude Code, Codex or Pi, close the laptop, and they keep working. Check in from the browser, from your phone, or by texting them on WhatsApp.

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="docs/api.md">Use it from WhatsApp</a> ·
  <a href="docs/install.md">Install guide</a> ·
  <a href="#docs">Docs</a>
</p>

## What it does

- **Agents that don't stop.** Claude Code, Codex and Pi run under [herdr](https://herdr.dev) on your server. They survive a closed lid, a dropped SSH connection and a reboot, and pick up their conversations where they left off.
- **Talk to them from WhatsApp.** Connect [Instinct](https://instinct.com/), or any bot, with an API key. Text "fix the signup form in our repo", get a short report back in plain words, and nothing is pushed until you answer "yes". See [API](docs/api.md).
- **Agents that talk to each other.** Through the same API one agent can read another's screen or hand it a task.
- **Issues in, pull requests out.** Write an issue on the board; an agent clones the repo, does the work and opens a PR.
- **A board in the browser.** Live terminals, sessions and the issue queue, made to work on a phone too.
- **All your agents in one terminal.** `ssh <server> femme-tui`, or one key in herdr: every agent on every linked machine on the left, the selected one's screen in the middle, a prompt box underneath. Works over mosh on a phone. See [TUI](docs/tui.md).
- **Move an agent to the server.** One key in herdr forks a local agent, with its conversation and folder, onto the server. The folder stays in two-way sync.
- **A push when you are needed.** A notification on your phone when an agent is waiting for you or done.
- **Yours, and closed by default.** Shell scripts, systemd user services and one small Rust server. Nothing is open to the internet unless you turn it on.

## Quick start

On your laptop (Linux or macOS):

```sh
git clone https://github.com/Amore-Labs-Inc/femme && cd femme
./install.sh
```

It asks where FEMME should run (an Ubuntu 24.04 server you have, a new AWS instance, or this machine) and which optional parts you want. Then it prepares the server, installs everything and checks it. **You** bring the server and sign in on it once (`claude` → `/login`, `gh auth login`). The [install guide](docs/install.md) lists exactly what is automatic and what is not.

Afterwards: `./doctor.sh` checks everything, `./install.sh update` updates, `./uninstall.sh` takes it off.

## Docs

| | |
|---|---|
| [Install](docs/install.md) | where it can run, what `install.sh` does, the board private or public, by hand |
| [API & Instinct](docs/api.md) | use FEMME from a chat bot or another agent; a [minimal client](examples/api-client.sh) |
| [Laptop](docs/laptop.md) | forking agents, folder sync, drop box, building on the laptop with `pc` |
| [Board](docs/board.md) | issues, terminals and sessions in the browser |
| [Notifications](docs/notifications.md) | a push to your phone when an agent needs you or is done |
| [Claude accounts](docs/accounts.md) | the accounts window, optional rotation between accounts |
| [AWS](docs/aws.md) | the optional instance role, a spend brake |
| [Troubleshooting](docs/troubleshooting.md) | common errors and their fixes |
| [Costs](docs/costs.md) | what you pay for, what is optional |
| [Contributing](CONTRIBUTING.md) | how the pieces fit, how to change them |

## Requirements

- **Laptop:** Linux or macOS with `ssh`, `jq`, `git`, `curl` and rsync 3.2.3 or newer (on macOS: `brew install rsync`; the built-in openrsync is too old). herdr and [Mutagen](https://mutagen.io) only for forking agents and folder sync.
- **Server:** Ubuntu 24.04 on x86_64 or arm64, reachable over SSH, with a user that can `sudo`; or an AWS account, and `provision.sh` makes one.
- **Accounts:** Claude (for Claude Code) and GitHub. Optional: Codex or Pi, a domain and Resend for a public board.

## Security

Agents on the server run without permission prompts, and anything running as your user there can use what the server can reach. The parts that widen that (public board, AWS role, auto-deploy, account rotation) are off until you turn them on. Read [SECURITY.md](SECURITY.md) before you do.

## License

Apache-2.0, see [LICENSE](LICENSE). FEMME builds on [herdr](https://herdr.dev) and other tools it installs but does not include; see [NOTICE](NOTICE). The Exposure name and logos are trademarks of Exposure and are not covered by the license.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/footer-dark.png">
  <img alt="FEMME, open source, Apache-2.0, by Exposure." src="assets/footer-light.png" width="100%">
</picture>
