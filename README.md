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

## Thanks

FEMME exists because other people wrote the hard parts and gave them away. Every line below is a project we lean on, and we are grateful to the people who maintain it. If one of them makes your life better too, go star it, sponsor it, or send a fix upstream.

**Agents.** [Claude Code](https://github.com/anthropics/claude-code), [Codex](https://github.com/openai/codex) and [Pi](https://github.com/badlogic/pi-mono) are the whole point; their headless protocols are what let FEMME drive them.

**Running on the server.** [systemd](https://systemd.io), [Caddy](https://caddyserver.com), [ntfy](https://ntfy.sh), [Mutagen](https://mutagen.io), [rsync](https://rsync.samba.org), [mosh](https://mosh.org) (whose state-sync model the terminal protocol copies) and [gh](https://cli.github.com).

**The desktop app and daemon** (the all-Rust client and `femmed`, in progress) stand on:

- UI: [gpui](https://github.com/zed-industries/zed/tree/main/crates/gpui) and [gpui-component](https://github.com/longbridge/gpui-component). The [Zed](https://zed.dev) team built a GPU text engine good enough to ship an editor on and licensed it so others could too.
- Network: [quinn](https://github.com/quinn-rs/quinn), [rustls](https://github.com/rustls/rustls), [aws-lc-rs](https://github.com/aws/aws-lc-rs), [rcgen](https://github.com/rustls/rcgen), [postcard](https://github.com/jamesmunns/postcard), [zstd](https://github.com/gyscos/zstd-rs) (and [Zstandard](https://facebook.github.io/zstd/) under it), [reqwest](https://github.com/seanmonstar/reqwest), [axum](https://github.com/tokio-rs/axum).
- Runtime: [tokio](https://tokio.rs), [tracing](https://github.com/tokio-rs/tracing), [serde](https://serde.rs), [clap](https://github.com/clap-rs/clap), [rustix](https://github.com/bytecodealliance/rustix), [mimalloc](https://github.com/microsoft/mimalloc).
- Terminals: [alacritty_terminal](https://github.com/alacritty/alacritty), [pty-process](https://github.com/doy/pty-process), with [WezTerm](https://wezterm.org)'s mux and predictive echo as the reference design.
- Storage: [SQLite](https://sqlite.org) through [rusqlite](https://github.com/rusqlite/rusqlite), [redb](https://github.com/cberner/redb).
- Text: [pulldown-cmark](https://github.com/pulldown-cmark/pulldown-cmark), [syntect](https://github.com/trishume/syntect), [two-face](https://github.com/CosmicHorrorDev/two-face), [tree-sitter](https://tree-sitter.github.io), [imara-diff](https://github.com/pascalkuthe/imara-diff), [similar](https://github.com/mitsuhiko/similar).
- Agent plumbing: the [Model Context Protocol Rust SDK](https://github.com/modelcontextprotocol/rust-sdk), the [Agent Client Protocol](https://agentclientprotocol.com).
- Measuring: [HdrHistogram](https://github.com/HdrHistogram/HdrHistogram_rust), [Tracy](https://github.com/wolfpld/tracy).
- Fonts: [VT323](https://fonts.google.com/specimen/VT323), [JetBrains Mono](https://www.jetbrains.com/lp/mono/), [Schibsted Grotesk](https://fonts.google.com/specimen/Schibsted+Grotesk), [Kaushan Script](https://fonts.google.com/specimen/Kaushan+Script), all under the SIL Open Font License.

**The board today** runs on [tiny_http](https://github.com/tiny-http/tiny-http), [ureq](https://github.com/algesten/ureq), [serde](https://serde.rs) and [xterm.js](https://xtermjs.org).

**Ideas we borrowed** (and say so in the code): [t3code](https://github.com/pingdotgg/t3code) for how a session should feel, [VS Code](https://github.com/microsoft/vscode) for its reconnecting terminal and type-ahead, [Zed](https://github.com/zed-industries/zed) for remote editing over a flaky link. FEMME also owes its start to [herdr](https://herdr.dev), which ran the agents before the daemon did.

## License

Apache-2.0, see [LICENSE](LICENSE). FEMME builds on [herdr](https://herdr.dev) and other tools it installs but does not include; see [NOTICE](NOTICE). The Exposure name and logos are trademarks of Exposure and are not covered by the license.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/footer-dark.png">
  <img alt="FEMME, open source, Apache-2.0, by Exposure." src="assets/footer-light.png" width="100%">
</picture>
