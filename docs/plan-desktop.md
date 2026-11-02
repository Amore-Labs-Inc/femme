# FEMME Desktop: all-Rust desktop client + VPS daemon

## Context

FEMME today: shell scripts + a 5-dependency Rust web board (`board/src/main.rs`, `tiny_http`, one request at a time) that shells out to `herdr` for every operation and polls a pane's screen over HTTP 4x/second. Agents (Claude Code, Codex, Pi) run inside herdr-managed PTYs on a VPS ("agentbox"). The user wants a new desktop UI, written entirely in Rust, that squeezes every networking and rendering gain available, keeping FEMME's board visuals with t3code's interaction model.

**Decisions made by the user (2026-11-02):**
- herdr is dropped. The VPS daemon owns PTYs, sessions and agent processes. herdr-only laptop features (femme-link, fork-remote, drop box) are dropped at M1 and rebuilt in the desktop later.
- Pure native Rust GUI (no webview). Framework: **gpui**, git-pinned to a Zed commit.
- One daemon replaces board + worker.sh + notify.sh + herdr, and still serves the phone web board and the WhatsApp/Instinct OpenAPI routes.
- Transport: **QUIC on its own UDP port** terminating on the VPS, same framed protocol over an **SSH tunnel** as fallback.
- Agent view: **structured transcript by default**, with "terminal takeover" (stop headless, `claude --resume` in the PTY, and back).
- Permissions: **bypass everywhere** stays. The approval UI handles only agent questions (AskUserQuestion, plan approval, Codex `requestUserInput`).
- Instinct/bot sessions run **headless**; their `/screen` text is generated from the transcript.
- Platforms: Linux + macOS first, Windows later.
- Look: FEMME board's visual elements (typesafe.ai retro: `--desk #F386A1` dot grid, black `.tb` title bars, VT323 pixel font for chrome, JetBrains Mono, Schibsted Grotesk body, 1.2px ink borders, hard `3px 3px 0` shadows, light only). UX: t3code (three-column shell, queue/steer composer, inline question rows, `when`-conditioned keybindings, command palette, bottom terminal drawer).

Research base: 12 Opus agents read t3code (`d01b92c`), VS Code (`d041bd1`), Zed (`decd2399`), the Rust GUI/network/terminal/runtime crate landscape and 6 arXiv subject classes (cs.NI, cs.DC, cs.PF, cs.OS, cs.HC, cs.GR; 40+ papers), then one Plan agent designed the daemon. Clones live under the session scratchpad `research/`. arXiv IDs are listed at the end.

---

## 1. Architecture

```
laptop                                   VPS (agentbox)
┌───────────────────────┐   QUIC/UDP    ┌──────────────────────────────────────┐
│ femme-desktop (gpui)  │◄────────────►│ femmed  (tokio, systemd --user)       │
│  - UI, local echo     │  or ssh -T    │  - QUIC + axum HTTP front            │
│  - row cache per pane │  femmed proxy │  - rev-based state (SQLite WAL)      │
│  - rusqlite cache     │  → unix sock  │  - agent drivers (sans-IO)           │
└───────────────────────┘               │  - issue scheduler (ex worker.sh)    │
phone browser ──HTTPS(Caddy)───────────►│  - web board + OpenAPI compat        │
WhatsApp bot ──HTTPS───────────────────►└──────┬───────────────────────────────┘
                                               │ unix socket per session
                                        ┌──────▼──────────┐  own systemd scope:
                                        │ femme-holder ×N │  survives femmed
                                        │  PTY panes      │  restarts/deploys
                                        │  alacritty grid │
                                        │  agent pipes +  │
                                        │  spool jsonl    │
                                        └──────┬──────────┘
                                        claude / codex / pi / $SHELL
```

### Workspace (root `Cargo.toml`, inside the femme repo)
```
crates/femme-proto      wire types + codec, NO tokio. serde, postcard 1.1, bytes, ulid, bitflags; zstd behind a feature
crates/femme-transport  Conn trait {open(kind, prio), accept, datagram}; quinn 0.11.12 + rustls 0.23 (aws-lc-rs) + rcgen; SSH stdio mux. features: client / server
crates/femme-term       alacritty_terminal 0.26 wrapper: absolute rows, per-row revs, RowPatch diff, key/mouse encoder, text/ANSI screen render
crates/femme-holder     bin. tokio current_thread, pty-process 0.5.4, rustix (SO_PEERCRED, signals), sd-notify
crates/femme-store      rusqlite 0.41 bundled + rusqlite_migration, zstd. writer thread + 4-connection read pool, JSON import, retention
crates/femme-agents     sans-IO drivers: claude, codex, pi, pty-hooks; serde_json; golden-fixture tests
crates/femmed           bin. tokio 1.54 multi-thread (4 workers), axum 0.8.9, reqwest 0.12 (rustls), rmcp 3.8, clap 4, tracing + tracing-journald, hdrhistogram 7
assets/                 index.html, login.html, logo.svg, openapi.json (moved from board/src, include_str!)
desktop/                gpui app; depends on femme-proto + femme-transport[client] + femme-term (row cache/prediction only)
```
Deleted by M5: `board/`, `server/worker.sh`, `server/notify.sh`, `plugin/`, herdr steps in `server/bootstrap.sh`, `HERDR_VERSION` in `.env.example`. `laptop/sync-dir.sh`, `sync.sh`, `pc` stay. **`laptop/systemd/femme-link.service` stays**: `pc` rides on the laptop sshd it forwards (`~/.config/herdr/remote/<host>.ssh`); at M1 it loses the herdr socket forward and the path moves out of `~/.config/herdr`.

Staging: `FEMMED=1` in `.env` switches `bootstrap.sh`/`provision.sh` to the daemon (units `femmed` + `femme-sessions.slice`, UDP `QUIC_PORT`); default 0 keeps the herdr setup until M1 passes its exit criteria.

---

## 2. Libraries (fastest proven choice per concern; versions checked 2026-11-02)

### Desktop
| Concern | Crate | Why |
|---|---|---|
| GUI | **gpui** (git, pinned Zed commit; Apache-2.0) + **gpui-component 0.8.1** | Only Rust framework with all three of our workloads in production: Zed terminal, Zed streaming agent list, tty7 (888 fps DOOM-fire vs 485 Alacritty). Native Metal/DirectX, wgpu on Linux, ~10 MB idle, subpixel text, `list()` variable-height virtualization on SumTree, cached views replay last frame, AccessKit since 2026-05. |
| Terminal grid | no emulator on the desktop: `RowPatch` frames into a row cache, drawn as batched same-style glyph runs + merged background rects (Zed `terminal_element.rs` idea) | Emulator runs on the VPS (mosh / wezterm-mux model). |
| Markdown | **pulldown-cmark 0.13.6** + **mdstream 0.3** | Fastest pull parser; reparse only the open block while streaming. |
| Highlighting | **syntect 5.3 + two-face 0.5.2** (line-resumable, streaming code blocks); **tree-sitter-highlight 0.27** for diff/file views later | |
| Diff | **imara-diff 0.2** + **similar 3.2** (intra-line) | |
| Async | **tokio 1.54** side runtime, 2 workers (Zed's `gpui_tokio` pattern); never `block_on` the UI thread | |
| Cache | **rusqlite 0.41** + **redb 4.4** (blobs) | |
| Profile | **mimalloc 0.1.52**; `lto="fat"`, `codegen-units=1`, `panic="abort"`, `strip=true`; PGO (cargo-pgo) last | perf-book: LTO 10–20 %, PGO ≥10 %. |
| Observability | **tracing + tracing-tracy 0.12**, **hdrhistogram 7.6** | Percentiles, never means (2601.10572). |

### Network (both sides)
| Concern | Choice | Why |
|---|---|---|
| Transport | **quinn 0.11.12** + rustls/aws-lc-rs. Verified in source: `set_priority`, `send_datagram`, `rebind`, `into_0rtt`. No `network_change` API: call `rebind` on wake/network change. | 0-RTT, migration, independent streams, datagrams. rustls+aws-lc-rs wins 14/16 vs BoringSSL/OpenSSL. **iroh 1.4** noted as alternative (relay-over-443 + roaming built in) if P2 shows UDP blocked too often. |
| Fallback | `ssh -T agentbox femmed proxy` → stdio ↔ `$XDG_RUNTIME_DIR/femme/femmed.sock`; same frames over an SSH mux | SSH login is the auth. |
| Encoding | **postcard 1.1.3** everywhere (append-only enums, `PROTO_VERSION` in Hello); snapshots = zstd'd postcard | One serializer. bitcode dropped (version-locked format); bincode is dead (3.0 tombstone). |
| Compression | **zstd 0.14**, one long-lived level-1 context per stream, payloads > 256 B only | Stream context gives dictionary-like gains; small-message compression leaks (CRIME) and wastes CPU. |
| Session layer | none generic: rev cursors for state, `rev`/`seq` for terminals, ULID op-ids deduped in `ops` | Server keeps no per-connection state: no grace period, no reconnect token (simpler than VS Code/Zed). |

### VPS daemon + holder
| Concern | Choice | Why |
|---|---|---|
| Runtime | **tokio 1.54** multi-thread, `worker_threads = 4` (vCPU count) | io_uring shows no gain for tens of PTYs / few clients (2512.04859; monoio bench). |
| HTTP | **axum 0.8.9** (web board, OpenAPI, WebSocket/SSE for the phone) | |
| PTY | **pty-process 0.5.4** (tokio, setsid/TIOCSCTTY handled) | |
| Emulator | **alacritty_terminal 0.26** behind a trait; fallback **wezterm-term** (git; has `StableRowIndex` + per-line seqno) if P1 fails; **libghostty-vt 0.2.2** later for images (needs Zig) | |
| Isolation | plain `systemd-run --user --scope/--unit` (zbus only if spawn > 50 ms); cgroup delegation confirmed on agentbox | t3code's `agentScope.ts` pattern. |
| State | **rusqlite 0.41** WAL, `synchronous=NORMAL`, `auto_vacuum=INCREMENTAL`, grouped commits (≤5 ms / 64 ops), FTS5 | avoids t3code's single-reader + fat-log failure. |
| Agents | Claude `claude -p --input-format stream-json --output-format stream-json --verbose --include-partial-messages`; Codex `codex app-server` (one per session, stdio, under the holder); Pi `pi --mode rpc`; generic **agent-client-protocol 3.6.0** later | First-party protocols, no Node, no extra hop. |
| MCP | **rmcp 3.8** streamable-HTTP on localhost: FEMME tools (sessions, issues, notify, ask-user) | |
| Notifications | ntfy via reqwest on status transitions (blocked prio 5, done prio 3, 10 s debounce) | replaces the 5 s poll. |

---

## 3. Design rules (research-backed)

Networking
1. **Never share an ordered byte stream between keystrokes and bulk.** Streams: `ctrl` (prio 5), `events` (3), `term:<pane>` (10, one per attached pane), `bulk` (−10). Datagrams: RTT pings, optional duplicate keystrokes. CATS 2606.16924 (p99.9 27 vs 307 ms), 1906.07415.
2. **Migrate, don't reconnect.** QUIC terminates on `femmed` (no proxy/LB: 2410.06066). `rebind` + probe at once on wake/network change (saves 2–3 RTT: 2408.01791).
3. **Bulk is paced** to 50 % of estimated bandwidth whenever a terminal had input in the last 2 s; duplicate sub-100 B keystroke datagrams when loss is detected (2305.03684, 2607.14482).
4. **Terminals are state, not logs.** Reconnect = frames since `have_rev`, never a byte replay (mosh, VS Code pty host, wezterm mux).
5. **Coalesce at the source.** PTY read following input in the last 50 ms: send at once; otherwise ≥ 8 ms (≤ 120 Hz) after draining. State changes: first immediately, then 50 ms latest-per-entity (t3code). Deltas ≤ 16 ms.
6. **Flow control ack'd after apply.** ≤ 8 frames / 128 KiB unacked per view; while blocked the holder keeps parsing and sends one merged frame (Ctrl-C during `cat` of 1 GB shows within ~1 RTT).
7. **Keepalive 1 s foreground / 15 s background; dead after max(3× smoothed RTT, 1.5 s)**; QUIC idle 30 s; race QUIC vs SSH on connect (QUIC wins within 300 ms), re-probe QUIC every 60 s while on SSH.
8. **Reconnect sync is digest/rev-based**, never echo a change back to its origin (1803.02750, 2505.01144).

Rendering / HCI
9. Budgets: key→predicted glyph ≤ 50 ms (aim 20); click→state ≤ 100 ms; agent activity cue ≤ 100 ms; p99 frame within 2 ms of refresh (2408.02525, 2606.25681, 2604.06183, 2306.01691).
10. **Predictive local echo** (wezterm-style, underlined until `echo_seq` confirms), on when RTT > 20–30 ms, off in alt-screen apps. Mosh: median response < 5 ms vs 503 ms.
11. **Token reveal paced 12–20 words/s per frame**, decoupled from arrival (2504.17999, 2404.16283).
12. **No busy-polling, no periodic timers on the laptop** (2112.07010); cursor blink from the frame scheduler.
13. **Heavy bodies out of the state tables**: tool output > 4 KiB → blobs, 512 B preview inline, fetched on expand (t3code).
14. Percentiles from hours-long soaks; 1 % of frames carry a `Timing` record; photodiode key→photon rig mid-screen for release gating (2601.10572, 2202.06429).
15. Protocol version negotiated in Hello (current and previous accepted); never require identical builds.
16. Zed's `terminal`, `markdown`, `rope`, `remote` crates are **GPL**: ideas only. gpui + sum_tree are Apache.

---

## 4. Desktop app (`desktop/`)

### Layout (t3code shell, FEMME skin)
```
┌──────────────┬───────────────────────────────────────┬──────────────────────────┐
│ femme   ⌘K   │ ▌repo › femme/issue-12  [working]     │ [diff|files|term|issue]  │  ← .tb black bars, VT323 lowercase
│──────────────│───────────────────────────────────────│──────────────────────────│
│ ▾ needs you  │  ┌ user ───────────────────────────┐  │  turn diff (imara)       │
│  ● #12 fix…  │  └─────────────────────────────────┘  │  file tree               │
│ ▾ working    │  ▸ worked for 3m (12 tools)           │  or issue card           │
│  ◐ api-auth  │  ▸ read 4 files, ran 2 cmds           │──────────────────────────│
│ ▾ queue (5)  │  assistant markdown (streaming, paced)│ usage 5h ▓▓▓░ 7d ▓░░░    │
│ ▾ PR ready   │  ┌ question: which auth lib? ───────┐ │ account · PR · ports     │
│ ▸ done       │  │ (1) jose  (2) jsonwebtoken  [⋯]  │ │                          │
│──────────────│  └──────────────────────────────────┘ │                          │
│ sessions     │  ┊ queued: "then add tests" ✎ ⇪     ┊ │                          │
│  vps·claude  │ ┌───────────────────────────────────┐ │                          │
│  vps·codex   │ │ prompt…   [repo▾][agent▾][queue⇧] │ │                          │
│ accounts     │ └───────────────────────────────────┘ │                          │
├──────────────┴───────────────────────────────────────┴──────────────────────────┤
│ terminal drawer ⌘J: [#12 claude | shell | +]  split ⌘D   (live pane, local echo)│
└─────────────────────────────────────────────────────────────────────────────────┘
```
- Skin tokens from `board/src/index.html` lines 18–25: `--desk #F386A1` dot grid, `--paper #FEFEFE`, `--panel #E5E5E5`, `--ink #1E1E1E`, `--hover #D45BB6`, `--neon #FF4FB4`, lanes `--work #03AA5C / --pause #F2B33D / --queue #DEDEDE / --pr #09AEA1 / --bad #D93036 / --done #858585`; `.win` 1.2px ink border, `.tb` black bar VT323 22px lowercase, `.dialog` hard shadow, square corners, buttons VT323. Fonts bundled: VT323, JetBrains Mono, Schibsted Grotesk, Kaushan Script (logo). Light only.
- UX from t3code: sidebar sections = FEMME lanes (needs you / working / queue / PR ready / done) + sessions + accounts; `mod+k` palette; `mod+b`/`mod+alt+b` panels; `mod+j` drawer; `mod+d` split; `mod+p` file picker; F6 focus cycle; `mod+shift+enter` steer; `mod+s` stash prompt; `alt+up` edit queued; `{key, command, when}` rules stored on the daemon (`keybindings` table).
- Transcript rows (port t3code `MessagesTimeline` kinds): message, assistant-meta, work, work-live, work-toggle (collapsed tool groups + sentence summary), thinking, turn-fold ("worked for 3m"), proposed-plan, question row, event. gpui `list()`; each message its own entity so streaming reparses only the live one. Bodies on expand.
- Questions (AskUserQuestion / plan approval / Codex `requestUserInput`): inline row + composer takeover, keyboard-answerable. No tool-permission prompts (bypass everywhere).
- Proactive diff panel when a turn ends; diff sources: turn, branch, uncommitted, commit.
- Notifications: sound + desktop notification + dock badge on `attention: blocked|done`.

### Modules
`app/` (gpui app, windows, keymap, palette) · `theme/` · `net/` (femme-transport client, QUIC/SSH race, rebind on wake) · `state/` (rev subscriptions, issue/session stores, offline op queue with ULIDs) · `terminal/` (row cache keyed by absolute row, prediction overlay, glyph-run renderer, lazy `GetRows`) · `transcript/` (row model, markdown/highlight caches, token pacer) · `diff/` · `issues/` · `sessions/` · `accounts/`.

---

## 5. VPS: `femmed` and `femme-holder`

### Holder (one process per session; chosen over the systemd fd store)
Why: auto-deploy restarts `femmed` on every merge; the holder keeps grid, scrollback and the agent's pipes intact; an emulator panic kills one session, not all; its transient unit is the session's cgroup; cost ≈ 10 MB/session + a versioned holder protocol.

- Spawn: `systemd-run --user --unit=femme-s-<sid> --slice=femme-sessions.slice --service-type=notify --collect -p KillMode=control-group -p OOMPolicy=continue -p CPUWeight=100 -p MemoryHigh=5G -p MemoryMax=7G -p TasksMax=4096 femme-holder --sid <sid>`. Slice `MemoryMax=12G`. `femmed.service`: `Type=notify WatchdogSec=10 Restart=always RestartSec=1 CPUWeight=500`.
- Owns: PTY panes (each a femme-term emulator, 10 000 lines scrollback) and at most one **headless agent in pipe mode**: stdin kept open, stdout appended to `$XDG_STATE_HOME/femme/spool/<sid>.jsonl` with absolute offsets, last 64 KiB of stderr. A dead daemon never EOFs the agent; the agent never blocks on a full pipe.
- Socket `$XDG_RUNTIME_DIR/femme/s/<sid>.sock` (dir 0700, SO_PEERCRED uid check). Protocol: `u32 len` + postcard; `Hello{v}` accepts current and previous version.
  - control: `Hello → Info{epoch, panes, agent{pid, alive, spool_lo, spool_hi, exit}}`, `SpawnPty`, `SpawnAgent`, `AgentWrite`, `SpoolCommit{offset}`, `Signal`, `Screen{pane, Text|Ansi}`, `Shutdown`; events `ChildExited`, `Bell`, `Title`, `Osc9/777`.
  - view (per client per pane): `Attach{pane, epoch, have_rev}` → `Frame*`; in: `Input/Resize/Ack/GetRows`.
  - spool: `Read{from}` → tail -f byte stream.
- Lifecycle: femmed start = scan sockets, reconcile with `sessions`, resume spool from `spool_committed`; DB-open-but-no-socket → `interrupted`. Holder crash → unit stops, session `crashed` with Resume action; emulator feed under `catch_unwind` (reset one pane + SIGWINCH). Unrequested agent exit → restart once with `--resume` after 2 s, second exit in 30 s → `failed` + stderr tail. Upgrade = install by rename, running holders keep the old binary. Reboot → `<sid>.snap` (screen + 200 lines, written ≤ every 30 s) shown as last screen.
- M5 measure-first: on focus `systemctl --user set-property --runtime femme-s-<sid> CPUWeight=400`; if keystroke p99 still rises > 5 ms under `stress-ng`, `Delegate=yes` + holder in its own sub-cgroup.

### Agent drivers (`femme-agents`, sans-IO; session actor does the IO)
```rust
trait Driver {
  fn spawn(&self, l:&Launch) -> Spawn;  fn tui(&self, l:&Launch) -> Option<Spawn>;
  fn on_start(&mut self, out:&mut Out);
  fn on_line(&mut self, offset:u64, line:&[u8], out:&mut Out) -> Vec<AgentEvent>;
  fn prompt/steer/interrupt/answer_question/set_model(&mut self, .., out:&mut Out) -> Result<()>;
  fn resume_offset(&self) -> u64;           // min start offset of in-progress items → spool_committed
  fn agent_session_id(&self) -> Option<&str>;
}
enum AgentEvent { Started{agent_sid, model}, TurnStarted, TurnEnded{stop, usage}, ItemStarted{id, kind},
  ItemDelta{id, text}, ItemUpdated{id, patch}, ItemCompleted{id, body},
  QuestionAsked{request_id, prompt, options}, RateLimit{pct, resets_at}, Status(AgentStatus) }   // ACP session/update shapes + _femme
```
- **Claude:** long-lived `claude -p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --permission-mode bypassPermissions --session-id|--resume <id> --settings <femme-hooks.json>`. Control requests `initialize`, `interrupt`, `set_model` (and `can_use_tool` only for AskUserQuestion/ExitPlanMode under bypass; confirmed in P3). Undocumented message shapes live in one module backed by fixtures from the pinned CLI version.
- **Codex:** one `codex app-server` per session under the holder: `initialize`, `thread/start|resume`, `turn/start|steer|interrupt`, `item/*`; `item/tool/requestUserInput` → `QuestionAsked`. Hand-written subset with `#[serde(other)]`; CI checks against `codex app-server generate-json-schema` of the pinned version. `codex --remote` not relied on.
- **Pi:** `pi --mode rpc` (prompt, steer, follow_up, abort); no permissions.
- **PTY mode (M1 only, and takeover):** TUI in pane 1; status via `femmed hook <event>` wired through Claude `--settings` hooks, Codex `-c notify=[...]`, a pi extension gated on `FEMME_SID` (replaces herdr's `herdr-agent-state.*` integrations). Prompt = bracketed paste + Enter 50 ms later.
- **Takeover state machine** (holder refuses a second agent child):
  `Headless → Takeover (turn active? ask wait|interrupt) → Stopping (close stdin, SIGTERM 2 s, SIGKILL) → SpawnPty(pane1, claude --resume <id> | codex resume <id> | pi --session <f>) → Tui`
  `Tui → Return/TUI exit → wait Stop hook → "/exit\r", 5 s, SIGTERM → backfill from agent transcript → SpawnAgent(--resume) → Headless`
- **Questions:** pending item, session `attention=blocked`, ntfy prio 5; any client answers, first wins (CAS on the item, later 409); daemon restart re-reads spool from `resume_offset` and recreates pending questions deduped by `request_id`.
- **Transcripts:** completed items + `spool_committed` in one transaction; token deltas never hit SQLite. Codex backfill via `thread/resume`; Claude via `~/.claude/projects/<mangled>/<id>.jsonl` tolerant parser (takeover gaps and imported sessions only).

### Scheduler (replaces `worker.sh`)
`plan(now, issues, sessions, usage, cfg) -> Vec<Action>` pure, table-tested against every `worker.sh` transition. Actions: Start, MarkPr, MarkWaiting, MarkWorking, Pause, Resume, Requeue, Fail, Close. Runs on session status changes plus a 60 s tick, never in reaction to its own issue patches (a fresh Start/Resume gets 60 s before "stopped without a PR" can fire; a failed start's slot frees on the next tick, as worker.sh's cadence did) (usage via `claude -p /usage` until `RateLimit` events prove sufficient; `gh pr list --head femme/issue-<id>`). Same prompts, branches, `~/femme-work/<id>` clones, `PARALLEL`, `CAP`. Pause = `interrupt` headless / Esc in PTY. cswap/fable-cap services stay outside femmed.

---

## 6. Wire protocol (`femme-proto` / `femme-transport`)

- **Auth, SSH as root of trust:** first run `ssh agentbox femmed enroll --name <dev> --spki <sha256>` registers the device and prints the server fingerprint + UDP port. QUIC ALPN `femme/1`, self-signed server cert (rcgen, 0600) pinned by fingerprint; client presents a self-signed device cert whose SPKI must be in `devices` and whose TLS 1.3 signature is verified (`rustls::crypto::verify_tls13_signature`, never stubbed). No tokens. `femmed devices revoke`. SSH fallback: `ssh -T agentbox femmed proxy` ↔ `femmed.sock` (uid check).
- **Connect:** race QUIC (UDP 7788, `QUIC_PORT` in `.env`, opened by `provision.sh` like mosh's range) vs SSH; see rule 7.
- **Framing (every stream):** `varint len | u8 flags (bit0 zstd) | postcard`. SSH mux: `varint stream_id | u8 flags (OPEN/FIN/RST/DGRAM/WINDOW) | varint len | data`, 16 KiB chunks, strict priority writer, 64 KiB credit per stream; same `Conn` trait as QUIC. Keystrokes get their own TCP connection over SSH to dodge head-of-line blocking.
- **Resume = new connection + Hello with cursors.** No ack/replay layer, no grace period, no reconnect token.
```rust
enum C2S { Hello{proto:u16, client_instance:Ulid, cursors:Cursors}, Req{op:Ulid, req:Req} }
struct Cursors { epoch:u64, global_rev:Option<u64>, sessions:Vec<(Sid,u64)> }
enum Req { Subscribe{scope, after:Option<u64>}, Unsubscribe{scope}, TranscriptPage{sid, before_seq, limit}, BlobGet{hash},
  SessionCreate{cwd, agent, mode, label, name, prompt}, SessionClose{sid},
  Prompt{sid, text, attachments, how:Queue|Steer|InterruptThenSend}, Interrupt{sid},
  AnswerQuestion{sid, request_id, answer}, Takeover{sid, to:Mode}, SetModel{sid, model},
  PaneOpen{sid}, PaneClose{pane}, IssueCreate{..}, IssuePatch{id, base_rev, fields}, IssueDelete{id},
  Upload{name, len}, FsList{path}, FsRead{path} }
enum S2C { Welcome{proto, server_version, epoch}, Resp{op, Result<Resp, Err{code,msg}>},
  Changes{scope, rev, rows:Vec<Row /*Session|Issue|Pane|Item|Tombstone*/>},
  Delta{sid, item_id, offset:u32, text}, SnapshotOnBulk{scope, rev, stream} }
enum TermUp { Open{pane, epoch, have_rev, client_instance}, Key{seq, key}, Paste{seq, text}, Bytes{seq, data},
  Mouse{seq, ev}, Resize{cols, rows}, Focus(bool), Ack{rev}, GetRows{from_abs, to_abs} }
enum TermDown { Frame(Frame), Rows(Vec<RowPatch>), Exited{status}, Bell }
struct Frame { epoch, rev:u64, base_rev:u64, applied_seq:u32, echo_seq:u32, screen:Primary|Alt, size:(u16,u16),
  top_abs:u64, history_lo_abs:u64, cursor, modes, title:Option<String>, new_styles:Vec<(u16,Style)>,
  rows:Vec<RowPatch>, timing:Option<Timing> /* 1 % sampled */ }
struct RowPatch { abs:u64, wrapped:bool, runs:Vec<(u16 /*style id*/, String)>, wide_cols:Vec<u16> }
```
- **Terminal rules:** holder sends rows whose rev is newer than what that view last got; a scroll = one new row + `top_abs`; scrollback rows are immutable (resize reflow bumps a layout generation, client refetches lazily); alt screen sent whole on switch; keys encoded in the holder from live modes (no mode races); `applied_seq` dedups resent input after reconnect; `echo_seq` drives prediction confirmation; most recently focused desktop client owns the size, the phone never resizes.
- **State sync (rev-based, not an event log):** every row in sessions/panes/issues/items carries a monotonic `rev`; deletes leave tombstones (14 days). `Subscribe{after}` → `SELECT … WHERE rev > after` (≤ 1 MiB inline, else bulk chunks of 256 rows); invalid cursor (past tombstone horizon or new epoch) → full reset. A session opened without a cursor gets its last 200 items + paging cursor + in-progress items from memory. Live: first change immediately, then 50 ms latest-per-entity; on broadcast lag the subscriber re-queries from its rev; delta backlog > 256 KiB → full item row.

---

## 7. Persistence (`femme-store`, SQLite)

```
meta(key PK, value)                     -- schema_version, epoch, rev counters, json_imported
sessions(id PK, handle UNIQUE, rev, name, owner /*board|issue:N|key:<name>*/, agent, mode, cwd, label,
         agent_sid, status, attention, spool_committed, created, closed)
panes(session_id, no, rev, kind, title, PK(session_id,no))
items(session_id, item_id, seq, rev, kind, status, preview, body BLOB /*zstd ≤64 KiB*/, blob_hash, ts,
      PK(session_id,item_id)); INDEX(session_id,rev); INDEX(session_id,seq)
blobs(hash PK, len, data, ts)
issues(id PK, rev, repo, title, body, status, pr_url, note, session_id, resume_at, field_revs, created, updated)
tombstones(entity, id, rev, ts)   ops(device, op_id, resp, ts, PK(device,op_id))
devices(id PK, name, spki_sha256 UNIQUE, created, last_seen, revoked)
web_sessions(token_sha256 PK, expires)   api_keys(name PK, key, github_token, github_login, git_name, git_email, created)
keybindings(device, json)   items_fts FTS5 (M4)
```
Retention (daily job + alarm at 1 GiB): terminal bytes never stored; tool output > 4 KiB → blob (cap 1 MiB each, 512 MiB total, oldest out); items/blobs of closed sessions deleted after 30 days; tombstones 14 days; ops 24 h; spool truncated below committed offset, deleted with the session; incremental vacuum nightly; no git checkpoint refs. Issues: per-field merge on `base_rev`, 409 only on same-field conflict (`field_revs`). No CRDT (Loro 1.17 only if offline multi-device text editing ships).
Migration: first start imports `issues.json` (ids kept), `sessions.json` (tokens hashed), `api-keys.json`, repos from `meta.json`, moves originals to `backups/`; idempotent. `femmed export-json` allows rollback to femme-board.

---

## 8. Compatibility layer (axum on `127.0.0.1:8787`, Caddy unchanged)

Port `board/src/main.rs` route by route; keep the JSON content-type CSRF rule, 1 MiB / 20 MB body limits, magic link + per-IP mail throttle + 7-day cookie, `api_route` allowlist with its exact 403 text, constant-time token compare, cswap/gh `Login` via `spawn_blocking`, and move every existing unit test verbatim (`api_route`, `valid_repo`, `outcome`, …).

| Route | Served by |
|---|---|
| `/api/terminals` | sessions + panes; ids minted `w<handle>:p<no>` (matches `openapi.json` regex); Instinct sessions `instinct~w..`, machine `instinct` |
| `/screen` (ANSI), `GET /terminals/{pane}` (text) | PTY pane → holder `Screen`; headless session → last 50 transcript lines rendered as text, pending question with numbered options |
| `/prompt` | `Prompt` request (headless) or paste + Enter (PTY); 409 when blocked, as today |
| `/input` | raw `Bytes` for PTY panes; for a headless session blocked on a question, digits / y / n / Esc map to `AnswerQuestion` |
| `/api/agents/{name}`, `/push`, `/discard` | `sessions.name`; brief, report path and `outcome()` git logic unchanged; sessions headless |
| `/api/meta`, keys, accounts, login, deploy | scheduler state + `deploy.json`; admin routes as today |
| `/api/terminals/upload` | `files.upload` into `~/.local/state/femme/uploads` (7-day prune) |
`worker.token` accepted as bearer for one release. Day-one gate: `examples/api-client.sh` and the phone board work against femmed before herdr is removed. Phone board gets SSE/WebSocket push in M5 (no more 250 ms polling).

---

## 9. Build

- `[profile.release]`: `lto="fat"`, `codegen-units=1`, `strip=true`; desktop `panic="abort"`; daemon/holder `panic="unwind"` (tokio task isolation, `catch_unwind` in the holder). `[profile.profiling]` inherits release + `debug="line-tables-only"`. Hot crates opt-level 3 in dev (gpui, alacritty_terminal, taffy).
- gpui via `[patch]` git rev, re-pinned monthly; gpui-component restyled with FEMME tokens.
- `femmed`/`femme-holder`: static musl (Zed's remote_server approach); `bootstrap.sh` symlinks `femmed` into `/usr/local/bin`. Desktop: Wayland/X11 + Vulkan/GLES + fontconfig on Linux, Metal on macOS.
- CI: `cargo test --workspace`, clippy, netem latency harness with hdrhistogram thresholds, golden agent fixtures, `generate-json-schema` drift check.

---

## 10. Milestones

**M0, week 1: prototype the riskiest assumptions**
| | Assumption | Exit criterion | If it fails |
|---|---|---|---|
| P1 | alacritty_terminal 0.26 gives stable absolute rows + per-row revs without a fork (count scroll-ups in a wrapping vte Handler) | 10 recorded sessions (Claude TUI, vim, htop, cargo, resize storms): rebuilt client screen == holder screen at every frame; diff < 0.3 ms at 200×60; parse ≥ 100 MB/s; memory per pane measured | wezterm-term from git (`StableRowIndex`, per-line seqno) |
| P2 | quinn priorities hold keystroke latency under contention | netem 80 ms ±10 ms, 1 % loss: keystroke→frame p99 < RTT+15 ms idle, < RTT+30 ms during a 50 MB download; p99.9 at 2 % loss with/without duplicate datagrams; UDP reachability on the user's real networks | lean on SSH fallback; evaluate iroh |
| P3 | Claude headless control protocol + takeover work on the installed CLI | initialize/interrupt/set_model work; which prompts arrive under bypass (AskUserQuestion, ExitPlanMode) known; `kill -9` mid-turn then `--resume` recovers; 20 headless↔TUI cycles lose no turns, never two processes, each < 3 s; fixtures captured | stay in PTY mode longer |
| P4 | gpui git-pinned app renders 4 `RowPatch`-fed panes + 50k-row streaming `list()` | p99 frame ≤ 10 ms at 120 Hz with 4 DOOM-fire panes | iced 0.14 with a custom wgpu shader widget |

**Main milestones**
| | Scope | Exit criteria |
|---|---|---|
| M1 | Parity without herdr: femmed + holder + store + compat layer; agents in PTY mode; scheduler; ntfy; herdr, board, worker, notify, plugin removed from bootstrap; femme-link keeps only the sshd forward; `FEMMED` default flips to 1 | ported tests pass; `api-client.sh` flow passes; issue lifecycle passes against a fake `claude`; 50 femmed restarts mid-turn → 0 agent restarts, 0 lost output on the phone; `/screen` p99 server time < 20 ms; ntfy within 1 s |
| M2 | Desktop terminal over QUIC with SSH fallback, FEMME skin, drawer/splits, local echo | netem 80 ms: keystroke→frame-sent p99 < 3 ms in-server, end-to-end p99 < RTT+10 ms (RTT+25 with bulk); usable 1.5 s after a 30 s loss; disconnect fuzzing leaves PTY input byte-exact; UDP blocked → SSH within 2 s; Ctrl-C after `cat` 1 GB visible < 300 ms; femmed < 3 % CPU/pane at 60 fps; predicted glyph p99 ≤ 20 ms |
| M3 | Claude/Codex/Pi drivers, headless default, takeover, questions, transcript UI, queue/steer, issues board in desktop; Instinct sessions headless | golden fixtures pass; first stream event → client cue < 50 ms server-side; restart mid-stream → transcript identical to uninterrupted run; first answer wins |
| M4 | Tombstones, blobs, retention, FTS5, JSONL backfill, per-field issue merge, `keybindings`, `doctor.sh` without herdr | 30-day synthetic load → DB < 400 MB; resubscribe after 1 h offline < 200 ms; search p95 < 50 ms on 1M items; `doctor.sh` green on fresh Ubuntu 24.04 |
| M5 | Files/uploads, bulk pacing, focus CPU boost, 0-RTT for idempotent Hello/Subscribe/Open, phone board on push, `board/` deleted, soak + PGO; later: fork-to-VPS and drop box as desktop features | 100 MB upload adds < 20 ms to keystroke p99; 24 h soak: RSS flat, p99.9 within budget |

Throughout: hdrhistogram per hop (client → daemon → holder write → echo → frame), 1 % full timelines, `femmed stats` prints p50/p99/p99.9/max.

---

## 11. Verification

- Unit: proto round-trips; Frame diff/apply property tests on random grids; `plan()` scheduler tables; driver golden fixtures; moved board tests.
- Integration: `cargo test -p femmed` spins a holder with `$SHELL`, attaches two clients, `kill -9` femmed, restarts, asserts both re-attach with identical screens and `spool_committed` resumes; netem harness asserting percentiles; `generate-json-schema` drift check for Codex.
- Manual: desktop against agentbox: 4 terminals + a Claude session; close lid; Wi-Fi → hotspot; no reconnect dialog, correct screens; takeover round trip; `examples/api-client.sh`; phone board on the same daemon.
- Perf rig: tracing-tracy per frame; `/debug/latency`; photodiode key→photon at mid-screen for release gating.

## 12. Open points (not blocking)
- Claude `control_request` shapes are undocumented outside the SDK source: pin CLI version in `bootstrap.sh`, keep fixtures.
- alacritty absolute-row tracking (P1), `--permission-prompt-tool` behaviour under bypass and existence of rate-limit events (P3) unverified.
- gpui accessibility is new; best-effort in v1.
- Auto-deploy causes a 1–2 s client reconnect blip (agents unaffected).

## Research sources (arXiv)
cs.NI: 1906.07415, 2310.09423, 2410.06066, 2408.01791, 2606.16924, 2411.10952, 2607.14482, 2305.03684 · cs.DC: 1803.02750, 2505.01144, 2409.14252, 2409.06066, 2212.13567 · cs.PF/OS: 2512.04859, 2502.09281, 2112.07010, 2502.14987, 2610.25546, 2407.11582, 2609.14123, 1902.04738, 2401.03523, 2604.09591, 2501.06716, 2610.04615, 2601.10572 · cs.HC/GR: 2408.02525, 2105.10498, 2606.25681, 2507.18085, 2604.06183, 2504.17999, 2202.06429, 2306.01691, 2506.19084, 2405.00127, 2610.02468, 2007.00308, 2208.11774, 2004.05609, 2212.14073 · plus mosh (USENIX ATC 2012), t3code `docs/internals/*`, VS Code `ipc.net.ts`/`ptyService.ts`/`terminalTypeAheadAddon.ts`, Zed `remote_client.rs`/`terminal_element.rs`/`gpui_tokio`.
