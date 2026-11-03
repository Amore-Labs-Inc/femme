# Desktop UI redesign (2026-11-03)

The user's words: "we have to redesign the ui from scratch … transcript mock part delete from
existence … lanes i dont need … left sidebar should be open and closeable and only include my current
active sessions … on the right a hoverable side bar that opens terminal and files, files if i press a
file i can see its rendering (pdf renderer image renderer etc) … in the center the terminal box with
preopened claude code whenever i kick open the app, if there was no claude before it should just be a
terminal, only my input should be visible there, all my images that i attach to the input should be
seen there and hoverable, and the output streamed where the transcript is."

Built by parallel agents, one module each, in their own git worktrees. This file is the contract.
Keep it current: when your module's public API changes, edit your section in the same commit.

## Layout

```
┌──────────────────────────────────────────────────────────────────────────────────┐
│ [femme ▾]  quic 23 ms                                   (nothing else up here)   │  top bar, 36 px
├──────────┬───────────────────────────────────────────────────────┬───────────────┤
│ sessions │  transcript (claude session)  OR  terminal (shell)    │ ▸ terminal    │
│  ● one   │  …streams from the daemon…                            │ ▸ files       │
│  ○ two   │                                                       │               │
│          ├───────────────────────────────────────────────────────┤  (hover strip │
│ [ctrl-b] │  composer: text + attachment thumbnails   [send]      │   opens panel)│
└──────────┴───────────────────────────────────────────────────────┴───────────────┘
```

- **Top bar**: the `femme` wordmark is a button: hover or click opens a dropdown (version, check for
  updates, accounts, reconnect, quit). It must *look* clickable (border/hover state), not like decor.
  The link status stays next to it. No lanes, no palette button, no panel buttons.
- **Left**: sessions only (live ones), newest first, current one marked. `ctrl-b` and a thin edge
  handle collapse/expand it. Nothing else on the left (no accounts, no lanes).
- **Centre**: for a Claude/Codex/Pi session the transcript (top) + composer (bottom). For a shell
  session the terminal pane fills it. The transcript shows what the agent streams: the user's own
  messages (with their attachments as thumbnails, hover = preview), assistant text as markdown, tool
  calls folded, questions as answerable rows.
- **Right**: a 28 px hover strip with two icons, `terminal` and `files`. Hovering or clicking opens
  the panel (320–480 px, resizable) with that tab; it closes when the pointer leaves it and nothing
  is focused inside, or on `ctrl-k`. `terminal`: the session's panes as tabs, `+` opens a shell in
  the session's directory, `close` closes the pane. `files`: the session's directory as a tree;
  a click shows the file in the panel (images, PDF, text/code, markdown).
- **Overlays**: accounts (from the menu) is a modal card over the centre, like the web app's
  accounts page (board `assets/index.html`, "Me" page): per account a row with the email, the active
  marker, the per-model line (`fable 71% · November 5 at 02:59`), 5 h and 7 d bars with the reset
  time, and **two editable caps** (5 h %, 7 d %) at which the box rotates off that account; an AWS
  line; API keys with revoke / connect GitHub / add (name + optional GitHub token); sign-out is NOT
  shown (the desktop has no cookie). `Esc` or the `‹` button closes it.
- **At start** (once connected): the newest live Claude session is shown; none → a new headless
  Claude session in the home directory is created and shown; `claude` missing on the box (the
  create fails) → a shell session. The user never sees an empty centre for longer than the connect.

Skin: keep FEMME's identity (fonts VT323 / JetBrains Mono / Schibsted Grotesk, ink borders, black
title bars, the logo) but the work surface is paper, not pink: pink is the desk margin and accents.
Light only. Tokens live in `theme.rs`, helpers in `ui.rs`: use them, never hard-code colours.

## Modules and owners

| Agent | Branch | Owns (only these files) | Public API (stubs are in place, compile-clean) |
|---|---|---|---|
| shell | `ui/shell` | `desktop/src/app.rs`, `main.rs`, `lib.rs` (mod list), deletes `panels.rs` and `bench.rs`'s transcript parts | `FemmeApp` layout, keymap, mounts every module below, focus cycle, integration owner |
| menu | `ui/menu` | `desktop/src/menu.rs` | `FemmeMenu::new(net, cx)`, emits `MenuEvent::{Accounts, CheckUpdates, Reconnect, Quit}`; renders the wordmark button + dropdown; "check for updates" compares `env!("FEMME_COMMIT")` (add `desktop/build.rs` to stamp it) with `Req::ServerInfo.deployed` and says what the timer will do |
| accounts | `ui/accounts` | `desktop/src/accounts.rs` | `AccountsOverlay::new(net, cx)`, `open/close/is_open`; data from `Req::BoardApi{GET /api/accounts, GET /api/keys}`, `Req::Caps`; actions `POST /api/accounts/switch`, `POST/DELETE /api/keys…`, `Req::SetCap` |
| sidebar | `ui/sidebar` | `desktop/src/sidebar.rs` | `Sidebar::new(net, cx)`, `set_selected(sid, cx)`, emits `OpenSession(Sid)`; lists `Net.model.sessions` that are live, newest first, status dot, title as `net::title` |
| sessions | `ui/sessions` | `desktop/src/net.rs`, `desktop/src/session.rs` | `Net::current()/set_current()`, `session::open_at_start(net, cx)`; status/attention handling; `Model` fields other agents read stay (`sessions`, `panes`, `link`, `rtt_ms`, `current`) |
| transcript | `ui/transcript` | `desktop/src/transcript/*` (rewrites `live.rs`, deletes `model.rs`, `pacer.rs`, `view.rs`, `rng.rs`; `bench.rs` loses its transcript bench) | `Transcript::new(net, cx)`, `set_session(sid, cx)`; subscribes `Scope::Session(sid)`, renders `Item` rows + `Delta`, markdown via `pulldown-cmark`, folded tool calls, question rows answer with `Req::AnswerQuestion`, user rows show attachments through `viewer::FileView` (`Req::BlobGet`) |
| composer | `ui/composer` | `desktop/src/composer.rs` | `Composer::new(net, cx)`, `set_session(sid, cx)`, `Focusable`; a multi-line input (gpui-component `input`), Enter sends, Shift-Enter newline, Esc = `Req::Interrupt`; paste image / drop file → `Req::BlobPut` → thumbnail chip (hover: `viewer::FileView` popover; ×) → `Req::Prompt{attachments, how: Queue}` (`Steer` with ctrl-Enter) |
| terminal | `ui/terminal` | `desktop/src/terminal/panel.rs`, `terminal/panel/tabs.rs`, `terminal/pane.rs`, `terminal/bridge.rs`, `terminal/input.rs` | `TerminalPanel::new(net, cx)`, `set_session(sid, cx)`, `pane(id)`, `main_pane(sid)` (the session's first terminal: a shell's, or an agent's TUI; the centre draws it), `main_id(sid)`, `set_centre(pane, cx)`, `shown_pane(cx)`, `watch_window(window, cx)`, `Focusable` (focusing the panel hands the keyboard to the tab on show); the panel owns every attached `TerminalPane` (first 8 terminals at once, the rest when their session is current; headless agents' pane 1 is the transcript's, never attached) and draws the right panel's terminal tab: tab per pane except the one the centre draws (a pane is drawn in one place only: `set_centre` says which, the default is the first terminal of a shell or TUI session), `+` (`PaneOpen`, or `SessionCreate` shell when there is no session), `close` (`PaneClose`; `SessionClose` for a shell session's first pane; never an agent's own pane), ctrl-tab / ctrl-shift-tab |
| files | `ui/files` | `desktop/src/files.rs` | `FilesPanel::new(net, cx)`, `set_session(sid, cwd, cx)`; tree from `Req::FsList{sid, path}` (lazy per directory), click → `Req::FsRead` → `viewer::FileView` in the panel, back button, size guard (ask before > 8 MiB) |
| viewer | `ui/viewer` | `desktop/src/viewer.rs`, `desktop/Cargo.toml` (deps it adds) | `FileView::new(name, data, cx)`: png/jpg/gif/webp/bmp via gpui `img`, SVG, PDF pages (pure-Rust renderer, no system libs), text/code with syntect, markdown; unknown → hex/size notice |
| theme | `ui/theme` | `desktop/src/theme.rs`, `desktop/src/ui.rs`, `desktop/assets/*` | tokens and helpers; **add or change values, never rename or remove** existing items (every other agent compiles against them); paper work surface, pink desk margin, hover/active states for buttons |
| daemon | `ui/daemon` | `crates/femmed/**`, `crates/femme-proto/**` (only to extend), `server/fable-cap.sh`, `server/systemd/femme-cswap.service` | serves the new requests below |

Not touched by anyone: `desktop/src/connect.rs`, `display.rs`, `terminal/element.rs`, `terminal/runs.rs`, `terminal/synth.rs`, `crates/femme-client`, `crates/femme-term`.

## Daemon contract (femme-proto `Req`/`Resp`, already in the enum)

| Request | Answer | Semantics (daemon agent) |
|---|---|---|
| `BoardApi{method, path, body}` | `BoardApi{status, body}` | The daemon calls the web app on the box: `http://127.0.0.1:8787` + `path` when it runs beside the board (`FEMMED=1`), its own `http_bind` when it is the board (`FEMMED=2`), with the owner bearer (`<state>/worker.token`). Only `path` starting `/api/accounts`, `/api/keys`, `/api/meta`; 10 s timeout; body ≤ 1 MiB each way. |
| `BlobPut{name, data}` | `BlobHash(h)` | `data` ≤ `MAX_BLOB_PUT` into the `blobs` table (hash = sha256). A `Prompt` whose `attachments` name blobs: each is written to `<state>/uploads/<sid>/<name>` (name sanitised) and the prompt text gets `\n\nAttached: <path>` lines, as the compat `/prompt` does. `BlobGet` returns it to the transcript. |
| `Caps` / `SetCap{account, five_hour, weekly}` | `Caps(Vec<AccountCap>)` / `Ok` | Stored in `<state>/caps.json` (`{"<number>": {"five_hour": 80, "weekly": 90}}`), written atomically. `server/fable-cap.sh` reads it: an account past *its* cap (either window) is `cswap disable`d and, if active, the box `cswap switch --strategy best`; below it again → `cswap enable`. `femme-cswap.service` keeps the global `CAP`. |
| `ServerInfo` | `ServerInfo{version, deployed}` | `env!("CARGO_PKG_VERSION")` and `<state>/deployed` trimmed. |
| `FsList{sid, path}` / `FsRead{sid, path}` | `FsEntries` / `FsData` | Implement: `path` relative to the session's `cwd` (absolute allowed under `$HOME`), no `..` escapes, no symlink out of home, hidden files listed, `FsRead` ≤ 16 MiB (else `Bad`), directories sorted dirs-first. |

Usage JSON: `GET /api/accounts` → `{"cswap": <cswap list --json>, "aws": {...}}`; each account has
`number`, `email`, `active`, `usage` (5 h / 7 d windows with `utilization` and `resets_at`, per-model
entries such as Fable), `usageFetchedAt`. See `crates/femmed/src/compat/accounts.rs` and the board's
renderer in `assets/index.html` (search `accts`). Keys: `GET /api/keys` → the list the board shows.

## Working rules

- Your worktree is `~/Documents/Web_Apps/femme-ui-<agent>` on branch `ui/<agent>`, from the
  skeleton commit. Edit only your files (table above). Need something from another module? Code
  against its stub API; if the API must change, say so in your final report, do not edit it.
- Build with the shared target dir so nothing compiles twice:
  `cd <worktree>/desktop && CARGO_TARGET_DIR=$HOME/Documents/Web_Apps/femme/desktop/target nice -n 15 cargo check -j 1`
  (femmed/proto: `cd <worktree> && CARGO_TARGET_DIR=$HOME/Documents/Web_Apps/femme/target nice -n 10 cargo test -p femmed -j 2`).
  Cargo serialises builds across agents with a lock: "Blocking waiting for file lock" is normal, wait.
  `export PATH=$HOME/.cargo/bin:$PATH` in every background shell.
- Clippy `-D warnings` and `cargo fmt` clean before every commit. `#![warn(missing_docs)]` is on:
  document your pub items and drop the stub's `#![allow(missing_docs)]`.
- Commit each unit as it goes green (one module, one test file, one Cargo change), ≤ 1 000 lines
  per commit, message says what and why, trailer `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`.
  Never push. Never `git add -A`.
- Headful checks on this VPS: Xvfb + lavapipe, your own display and ports (table below):
  `Xvfb :<D> -screen 0 1400x900x24 +extension GLX +render &` then
  `DISPLAY=:<D> VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json <target>/debug/femme-desktop --size 1400x900 …`.
  Screenshot: `xwd -root -display :<D> -silent | convert xwd:- png:shot.png`. Input: XTEST via
  `python3 <scratch>/wire/xt.py :<D> click X Y | type TEXT | key NAME` (the scratch path is in your brief).
- A scratch daemon for live data: build `femmed` + `femme-holder` once (`cargo build -p femmed -p femme-holder`
  in the root workspace), then
  `FEMME_STATE=<dir>/state XDG_RUNTIME_DIR=<dir>/run HTTP_BIND=127.0.0.1:<H> QUIC_PORT=<Q> FEMME_HOLDER_DIRECT=1 FEMME_HOLDER_BIN=<target>/debug/femme-holder HOME=$HOME femmed serve`,
  enrol a scratch identity (`femme-desktop --show-spki --identity <dir>/dev` → `FEMME_STATE=… femmed enroll --name x --spki …`)
  and run the desktop with `--daemon 127.0.0.1:<Q> --fingerprint … --identity <dir>/dev`.
  Agents in tests are **fakes only**: put a `claude` symlink to `crates/femme-agents/fixtures/claude/fake-claude.py`
  first on `PATH` for the scratch daemon. Never run the real `claude`, `codex` or `pi`.
- Displays and ports: shell :110 11787/11788 · menu :111 11887/11888 · accounts :112 11987/11988 ·
  sidebar :113 12087/12088 · sessions :114 12187/12188 · transcript :115 12287/12288 ·
  composer :116 12387/12388 · terminal :117 12487/12488 · files :118 12587/12588 ·
  viewer :119 12687/12688 · theme :120 12787/12788 · daemon 12887/12888.
- The live box (`femmed.service`, `~/femme`, ports 7788/8787/8788) is not a test bed: never touch it.
- Report at the end: what landed (commits), what you could not do, API changes you need from
  others, and the screenshot paths that prove it.

## Integration (orchestrator)

Branches merge into `main` in this order: daemon, theme, viewer, sessions, terminal, sidebar, menu,
accounts, files, composer, transcript, shell. Disjoint files, so conflicts are limited to `Cargo.toml`
/ `Cargo.lock` (viewer, transcript) and `lib.rs` (shell). Then one headful run against a scratch
daemon with the fake claude, then the box.
