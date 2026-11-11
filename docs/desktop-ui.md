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
  updates, pull requests, accounts, dark mode, reconnect, quit). It must *look* clickable (border/hover state), not like decor.
  The link status stays next to it. No lanes, no palette button, no panel buttons.
- **Left**: sessions only (live ones), newest first, current one marked. `ctrl-b` and a thin edge
  handle collapse/expand it. At the foot `+ shell` and `+ claude` (headless, home directory) start a session
  and show it. Nothing else on the left (no accounts, no lanes).
- **Centre**: for a Claude/Codex/Pi session the transcript (top) + composer (bottom). For a shell
  session the terminal pane fills it. The transcript shows what the agent streams: the user's own
  messages (with their attachments as thumbnails, hover = preview), assistant text as markdown, tool
  calls folded, questions as answerable rows. A link in the markdown, and a bare `http://` or
  `https://` address in its text, opens in the system's browser on a click (the pointer turns to a
  hand over it): the words of a link are the target, not the dim address after them, and only web
  and mail addresses open (a `file:`, `javascript:` or app-scheme link is shown, not followed).
  An `Artifact` call (Claude Code's page on claude.ai; FEMME's Claude chats run with
  `CLAUDE_CODE_ARTIFACT=1`) is one line, `artifact · <title> ↗`, whose title opens the page; a
  publish that changed an existing page adds `updated · v<N>`. Its folded body is the plain call.
- **Compaction hand-off**: a compaction shows as one row, `· Conversation compacted`. Claude Code
  restarts the model with a summary it writes itself ("This session is being continued from a
  previous conversation… Summary: …", after an `<artifact-content-authored-by-others/>` preamble
  when the conversation held artifact content): the summary is Claude's, not a message. Its
  terminal never shows it, the stream of a FEMME chat never carries it, and the mirror of a herdr
  pane drops the transcript line that holds it (`isCompactSummary` / `isVisibleInTranscriptOnly`).
  A summary stored as a `UserMessage` before the mirror dropped them is still not drawn as the
  user's: the compact note and that message fold into one muted row
  (`▸ · conversation compacted · summary`), a click shows the summary. That detection is the
  template's prefix, display only.
- **A message from another Claude session**: Claude Code sessions on one machine message each
  other (`SendMessage` to a name `ListAgents` shows, 2.1.224+). The CLI delivers it as a *user*
  line with `origin.kind: "peer"`; the daemon stores it as `PeerMessage {from, text}` (the
  sender's name and the text without the `<cross-session-message>` framing), never as a
  `UserMessage`, so the outbox does not take a pending prompt off for it and it is not the
  user's words anywhere. The transcript draws it as a card on the *left*, headed `✉ from @name`
  in the accent colour, the text as markdown; a reply follows under it like any turn. It arrives
  mid-turn when the session was busy (a `queued_command` in a TUI transcript) and sits where it
  came. (`transcript/rows.rs` `peer_row`.)
- **The agent shows a picture**: `femmed show <file> [caption]`, run inside a session (the holder
  sets `FEMME_SID`), uploads the file (`BlobPut`) and sends `Show`; the daemon keeps a copy in
  `<state>/uploads/<sid>/` and appends an assistant item whose text is the caption plus an
  `Attached: <path>` line. The transcript draws an assistant row's `Attached:` images as 360×240
  pictures (hover = preview, right click = copy image / open), so the picture stays with the session even when the original file
  goes. Tell the agent in its CLAUDE.md: "to show the user a picture, run `femmed show <path>
  \"what it is\"`".
  A video (`.mp4`, `.mov`, `.webm`, `.mkv`, `.avi`, `.m4v`) goes up as it is when it fits one
  blob (8 MiB); the transcript shows a still of it with a ▶, and a click plays it in the lightbox
  (`desktop/src/video.rs`: `ffmpeg` decodes raw frames and the sound, `pw-play` or `paplay` plays
  it). A bigger one goes as a silent, looping animated GIF that ffmpeg makes on the box (640 px,
  the first minute, at most 200 frames; gpui animates a GIF held in memory, not a WebP).
- **The lightbox**: a click on a picture or video, in the transcript or the files panel, opens it
  over the whole window like the board. A video has play/pause, the time, a track to click or drag,
  and the sound; space, left/right (5 s) and `m` work too. Esc, `‹` or the backdrop close it.
- **Subagents** (`Agent` tool calls): the transcript shows one thread at a time. While the turn
  has subagents, a 24 px strip sits at the foot of the transcript box, above the composer:
  `‹ main` first, then one chip per subagent (`● description 1m 12s`, grey `done` when its call has
  answered); the thread on show is lit. A chip, or `open ›` on the Agent's tool row, shows that
  subagent's items (its prompt, tool calls and text, with its own working line); `‹ main` or `Esc`
  goes back (`Esc` only interrupts the session from the main thread). The chips go when the turn
  ends, except an agent launched in the background (its items keep coming after the turn): its
  chip stays, `●` and counting, until the agent reports it stopped, as Claude Code's own list of
  background tasks does. The Agent row stays in the main thread as the way back in. The composer
  is unchanged: a message sent while a subagent is on show goes to the session as always.
- **Goal** (Claude's `/goal <condition>`, 2.1.319+): while the session has a goal, a 24 px strip in
  the subagent strip's dress sits at the foot of the transcript box, above the composer:
  `◎ goal · <condition, one line, cut with …> · working`, `· not met yet` after the checker said no
  (Claude goes on by itself), then `✓ goal · … · achieved` (mark and verdict in `goal_met`), which
  stays until the next prompt; `/goal clear` or `/clear` take it down at once. Hovering the strip
  shows the whole condition and the checker's last reason; a click keeps them under the strip
  until the next click or verdict. Each "not yet" is also a muted row in the transcript,
  `◎ goal not met yet · <the checker's reason, whole>` (a note row shows all its lines now, not the
  first). The daemon reads the verdicts from the session's transcript on disk
  (`~/.claude/projects/*/<session>.jsonl`, `goal_status` attachment lines), the only place Claude
  writes them: a headless chat's actor follows that file while a goal is set, a session mirrored
  from a herdr pane reads it anyway. `Session::goal` (femme-proto) carries condition, state and
  reason; the strip's colours are the `goal` and `goal_met` theme tokens.
- **Background tasks** (Claude Code 2.1.319): a Bash command or an Agent the turn waits for is a
  task (`task_started`), and its tool row shows `⇣ background` at its right while it runs. A click
  moves that task to the background (`Req::Background{sid, tool_use_id}`, Claude's `background_tasks`
  control request): the call's result says it was backgrounded, the turn goes on, and the row says
  `in background` until the task's end comes as an event (`task_notification`), which starts a turn
  of its own where Claude reads the output. `ctrl-shift-b` does the same for every foreground task
  of the chat on show, as `Ctrl+B` does in Claude's own TUI (a mirrored pane gets that key).
- **A mirrored pane's prompts**: Claude in a herdr pane blocked on a prompt its TUI draws never
  writes it to its transcript, so the daemon reads it off the pane's screen (`herdr agent read`,
  `headless/herdr.rs` `screen_prompt`) and shows it as a question row with the options as buttons:
  a permission prompt ("Do you want to proceed? 1. Yes 2. No", an option wrapped over lines is one
  option), the plan dialog ("Would you like to proceed?"; herdr reads that pane as idle, so the
  transcript's ExitPlanMode call with no result yet is what marks it blocked), an AskUserQuestion
  (its options, "Type something.", "Chat about this"), the folder trust dialog (a cursor, no
  numbers). An answer is read off the screen as it is then and typed into the pane as keys: the
  option's number, plus Enter where the footer says "Enter to select" (an AskUserQuestion moves its
  cursor on a digit), up/down from the cursor and Enter for an unnumbered list, Escape for cancel. An
  answer given in the pane itself closes the row as `answered in the pane`. A message sent while it
  waits is refused with "Claude is waiting for an answer in its terminal", not `agent_not_ready`.
- **Selecting text**: all of the transcript's text is selectable, like text on a web page: a drag
  selects across paragraphs, list items and rows, in reading order (pink, the colour of the
  composer's selection), a double click selects a word, a triple click the paragraph (the text
  up to a line break: a line of a code block or of a message), a click elsewhere clears. `ctrl-c` copies
  the selection (paragraphs apart by a blank line, list items and table cells one per line, code as
  it is, no markers or bullets) unless the composer has a selection of its own, which it copies
  instead; `Esc` clears it (and only then goes on to its other work: back from a subagent, interrupt).
  The pointer is an I-beam over text (a hand over a link), and a drag never takes the keyboard from
  the composer. Text is built with `transcript/select.rs`'s `Selectable` (a `StyledText` that takes
  part in gpui-base's window-level text selection, with the links of its spans: a click that did not
  drag opens one), never with a plain `div().child("text")`: only buttons and chips are plain. The
  list builds only the rows on screen and gpui-base drops a selection whose end row is gone, so a
  selection lasts while the rows of its two ends stay in view (at least in part): a drag can scroll
  the list at its edge only while the row it began in still shows, and scrolling that row away ends
  the selection. A text a row cuts short with an ellipsis cannot be selected (its open row shows it
  whole).
- **Pictures, right click**: a composer chip and a thumbnail in the transcript open a small menu at
  the pointer, drawn by `ui/ctx.rs`. A chip that is a picture offers `copy image`, `open` and
  `remove`, any other chip `open` and `remove`, a thumbnail `copy image` and `open`; the left click
  is unchanged. `copy image` puts the picture on the system clipboard through `ui/clip.rs`: gpui does
  it on macOS and Windows, `wl-copy` (Wayland) or `xclip` (X11) is run for it elsewhere. A PNG goes as
  it is and any other raster is re-encoded as a PNG with its EXIF orientation applied (the programs
  that take a pasted picture read PNG); an SVG has no `copy image`. A helper that is missing or fails
  is said under the composer's input, or in the transcript's dismissible note. `open` keeps the
  hover preview on screen until Esc or a click elsewhere. `Esc` and a click outside close the menu;
  `Esc` goes no further, so it does not interrupt a working session, and while the menu is up nothing
  behind it takes a hover or a click.
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
- **Pull requests** (`femme ▾ ▸ Pull requests`, `desktop/src/prs.rs` + `prs/view.rs`): a card over
  the centre like accounts. Left, the box repo's open pull requests (`Req::Prs`): `#n title`, branch,
  `+adds −dels`, author; up/down move the selection. Right, the selected one: title, branch and
  author, its link (a click opens the browser), its description as Markdown (the transcript's
  `md_view`), `Watch demo` when `Pr::video` is set (`Req::PrVideo` → the lightbox: a GIF as a
  picture, anything else in the player), `Merge & deploy` and `Close PR`. Both act only on the
  second click, on a confirm row that says what happens (`Esc` cancels it). A merge notes
  `ServerInfo.deployed` first, sends `PrMerge`, then asks `ServerInfo` every 5 s for 10 min: the
  strip under the title says `#n merged · deploying… 1m 05s`, then `#n deployed <sha>`, or that the
  box still runs the old commit. Errors show in the red strip; a daemon without the requests is
  "daemon too old for pull requests". `refresh` in the title bar and every open read the list again.
  `Esc` or `‹` closes it; the deploy watch goes on while it is closed.
  **Conflicts**: a row whose `Pr::mergeable` is `Some(false)` carries a red `conflicts` pill (`None`:
  a faint `checking`; `Some(true)`: nothing). `Merge & deploy` stays the main button on it too, with
  a line under the buttons: `conflicts with main: Claude resolves them, then it merges and deploys
  by itself`. The daemon answers that `PrMerge` with `Ok` as for a clean merge, but queues it
  behind a conflict fix (`Pr::merging`); the card reads the list after every `Ok`: the pull request
  gone means merged (the deploy watch above), still there with `merging` means queued. A queued one
  shows an accent `merging…` pill, the detail says `Claude is resolving the conflicts; it merges and
  deploys by itself` and has no merge or fix button (`Watch demo`, `Close PR` and an `Open fix
  chat` stay). The card reads the list every 10 s until it leaves the list (open or closed, past the
  60-read cap), then watches its deploy; listed again without `merging`, it says the fix stopped.
  `Fix with Claude` sits beside `Merge & deploy` on a conflicting row for steering: `Req::PrFix` →
  `Resp::Session(sid)`, a real Claude chat labelled `fix #n` that merges main into the branch,
  resolves, tests and pushes; the card closes and the centre switches to that chat
  (`Net::set_current`, as a sidebar click). While `Pr::fixing` is set the row shows a `fixing…` pill
  and the detail `Open fix chat` in its place. While anything is merging, fixing or `checking`, the
  open card reads the list every 10 s, at most 60 times per opening. An older daemon that refuses a
  conflicting merge reads `#n conflicts with main — Fix with Claude` (other errors as the daemon
  says them). The footer's `Auto-fix conflicts after a merge` toggle is `Req::PrAutoFix`
  (`on: None` reads it at open, a click sends `Some(!cur)`; off by default; hidden when the daemon
  does not know it): with it on, each merge starts a fix chat for every `claude-update/*` pull
  request it leaves conflicting (uses Claude usage).
- **At start** (once connected): the newest live Claude session is shown (one that crashed, failed
  or lost its holder does not count); none → a new headless Claude session in the home directory is
  created and shown; `claude` missing on the box (the create fails) → a shell session. The user
  never sees an empty centre for longer than the connect. Later, a new Claude session that appears
  while a shell is on show (`claude` typed in it) takes the centre, once.

Skin: keep FEMME's identity (fonts VT323 / JetBrains Mono / Schibsted Grotesk, ink borders, black
title bars, the logo) but the work surface is paper, not pink: pink is the desk margin and accents.
Tokens live in `theme.rs`, helpers in `ui.rs`: use them, never hard-code colours.

Dark mode: the `femme ▾` menu has a `Dark mode` switch between `Accounts` and `Reconnect`; it flips the
theme at once in every window and keeps the menu open. `theme.rs` holds two palettes with the same token
names (`LIGHT`, `DARK`) and a colour is read through its accessor (`theme::ink()`, `paper()`, ...) each
time it is drawn, so nothing is rebuilt on a flip: never keep one colour in state, a static or a cache
and draw with it later, derive it when you draw. What is costly to build (the code colours of
`viewer/highlight.rs`, the dot grid's tile, the terminal's skin) is kept once per palette and picked
with `theme::is_dark()` as it is drawn. Ink and paper swap (light ink on a near-black desk with a dim
pink dot grid), hover and neon stay pink, title bars stay dark with pink lettering (`bar`,
`bar_text`), and a label takes the colour that reads on its fill (`on_accent`, `on_accent_strong`,
`text_on`; `ink()` and `muted()` do not read on the pink fills in dark, and a child that sets its own
colour keeps it while its row is pressed), so a new fill is checked against both palettes: the tests
in `theme/tests.rs` hold every pair a screen relies on. The terminal grid (`Palette::femme`), the code
colours (`viewer/highlight.rs`) and the board follow the theme; the board's PNG export and a PDF page
stay light (a picture goes to Claude, a page is white paper). The choice is the word `dark` or `light`
in `$XDG_STATE_HOME/femme/theme`, else `~/.local/state/femme/theme` (beside `board.json`), read before
the first window opens (no file is light) and written on every flip.

## Modules and owners

| Agent | Branch | Owns (only these files) | Public API (stubs are in place, compile-clean) |
|---|---|---|---|
| shell | `ui/shell` | `desktop/src/app.rs`, `main.rs`, `lib.rs` (mod list), deletes `panels.rs` and `bench.rs`'s transcript parts | `FemmeApp` layout, keymap, mounts every module below, focus cycle, integration owner |
| menu | `ui/menu` | `desktop/src/menu.rs` | `FemmeMenu::new(net, cx)`, emits `MenuEvent::{Accounts, CheckUpdates, Reconnect, Quit}`; renders the wordmark button + dropdown (with a `Dark mode` switch that calls `theme::set_dark` itself and emits nothing); "check for updates" compares `env!("FEMME_COMMIT")` (add `desktop/build.rs` to stamp it) with `Req::ServerInfo.deployed` and says what the timer will do |
| accounts | `ui/accounts` | `desktop/src/accounts.rs` | `AccountsOverlay::new(net, cx)`, `open/close/is_open`; data from `Req::BoardApi{GET /api/accounts, GET /api/keys}`, `Req::Caps`; actions `POST /api/accounts/switch`, `POST/DELETE /api/keys…`, `Req::SetCap` |
| sidebar | `ui/sidebar` | `desktop/src/sidebar.rs` | `Sidebar::new(net, cx)`, `set_selected(sid, cx)`, emits `OpenSession(Sid)`; lists `Net.model.sessions` that are live, newest first, status dot, title as `net::title` |
| sessions | `ui/sessions` | `desktop/src/net.rs`, `desktop/src/session.rs` | `Net::current()/set_current()`, `session::open_at_start(net, cx)`; status/attention handling; `Model` fields other agents read stay (`sessions`, `panes`, `link`, `rtt_ms`, `current`) |
| transcript | `ui/transcript` | `desktop/src/transcript/*` (rewrites `live.rs`, deletes `model.rs`, `pacer.rs`, `view.rs`, `rng.rs`; `bench.rs` loses its transcript bench) | `Transcript::new(net, cx)`, `set_session(sid, cx)`; subscribes `Scope::Session(sid)`, renders `Item` rows + `Delta`, markdown via `pulldown-cmark` (`Inline::links` keeps each link's target; `md_view` makes a text with links an `InteractiveText` whose click is `cx.open_url`), folded tool calls, question rows answer with `Req::AnswerQuestion`, user rows show attachments through `viewer::FileView` (`Req::BlobGet`) |
| composer | `ui/composer` | `desktop/src/composer.rs` | `Composer::new(net, cx)`, `set_session(sid, cx)`, `Focusable`; a multi-line input (gpui-component `input`), Enter sends, Shift-Enter newline, Esc = `Req::Interrupt`; paste image / drop file → `Req::BlobPut` → thumbnail chip (hover: `viewer::FileView` popover; ×) → `Req::Prompt{attachments, how}`; `how` follows the terminal's keys (`composer/slash.rs` `how_for`): idle session = `Queue` on either key; working session: Enter and the `send` button = `Steer` (Claude takes the message when its current tool calls finish, inside the running turn; the outbox's faded bubble stays until the CLI's `isReplay` echo lands it between the tool rows), ctrl-Enter = `InterruptThenSend` (stop the turn, send now), a `/command` line = `Queue` (held until the turn ends, as the terminal holds commands); the text typed for a session is kept in `$XDG_STATE_HOME/femme/drafts/<sid>.txt` on every edit (`composer/disk.rs`) and is back in that chat on the next run, until it is sent; above `send`, a Claude session's context XP bar (`composer/xp.rs`, 10 px like the permission square above `+`): `Session::context` as a notched bar that fills, pink then red past 90%, its tooltip the numbers (current / window, progress, room left, level = compactions + 1); a compaction is a level up: a neon ring pulses once around the bar. A session mirrored from a herdr pane has it too, read from its transcript on disk: what each message read, its compactions, and the window from the driver's table by model (the transcript never says it); Up on the input's first row recalls the session's earlier prompts (its `UserMessage` items plus sends not echoed yet, `composer/history.rs`), Down on the last row goes forward and back to the draft, Esc or a send ends it (the `/` menu keeps Up/Down while open) |
| terminal | `ui/terminal` | `desktop/src/terminal/panel.rs`, `terminal/panel/tabs.rs`, `terminal/pane.rs`, `terminal/bridge.rs`, `terminal/input.rs` | `TerminalPanel::new(net, cx)`, `set_session(sid, cx)`, `pane(id)`, `main_pane(sid)` (the session's first terminal: a shell's, or an agent's TUI; the centre draws it), `main_id(sid)`, `set_centre(pane, cx)`, `shown_pane(cx)`, `watch_window(window, cx)`, `Focusable` (focusing the panel hands the keyboard to the tab on show); the panel owns every attached `TerminalPane` (first 8 terminals at once, the rest when their session is current; headless agents' pane 1 is the transcript's, never attached) and draws the right panel's terminal tab: tab per pane except the one the centre draws (a pane is drawn in one place only: `set_centre` says which, the default is the first terminal of a shell or TUI session), `+` (`PaneOpen`, or `SessionCreate` shell when there is no session), `close` (`PaneClose`; `SessionClose` for a shell session's first pane; never an agent's own pane), ctrl-tab / ctrl-shift-tab |
| files | `ui/files` | `desktop/src/files.rs` | `FilesPanel::new(net, cx)`, `set_session(sid, cwd, cx)`; tree from `Req::FsList{sid, path}` (lazy per directory), click → `Req::FsRead` → `viewer::FileView` in the panel, back button, size guard (ask before > 8 MiB) |
| viewer | `ui/viewer` | `desktop/src/viewer.rs`, `desktop/Cargo.toml` (deps it adds) | `FileView::new(name, data, cx)`: png/jpg/gif/webp/bmp via gpui `img`, SVG, PDF pages (pure-Rust renderer, no system libs), text/code with syntect, markdown; unknown → hex/size notice |
| theme | `ui/theme` | `desktop/src/theme.rs`, `desktop/src/ui.rs`, `desktop/assets/*` | tokens and helpers; **add or change values, never rename or remove** existing items (every other agent compiles against them); paper work surface, pink desk margin, hover/active states for buttons. The one rename, made with dark mode: a colour token is a function (`ink()`, `paper()`, ...) reading the live palette, and the old `INK`/`PAPER`/... constants are gone |
| daemon | `ui/daemon` | `crates/femmed/**`, `crates/femme-proto/**` (only to extend), `server/fable-cap.sh`, `server/systemd/femme-cswap.service` | serves the new requests below |

Not touched by anyone: `desktop/src/connect.rs`, `display.rs`, `terminal/element.rs`, `terminal/runs.rs`, `terminal/synth.rs`, `crates/femme-client`, `crates/femme-term`.

## Daemon contract (femme-proto `Req`/`Resp`, already in the enum)

| Request | Answer | Semantics (daemon agent) |
|---|---|---|
| `BoardApi{method, path, body}` | `BoardApi{status, body}` | The daemon calls the web app on the box: `http://127.0.0.1:8787` + `path` when it runs beside the board (`FEMMED=1`), its own `http_bind` when it is the board (`FEMMED=2`), or the base URL in `FEMME_BOARD_API` when that is set; with the owner bearer (`<state>/worker.token`, read on every call; a missing one is `Internal`). Only GET, POST, DELETE and a `path` under `/api/accounts`, `/api/keys`, `/api/meta` (no `..`, encoded dots or slashes, spaces), else `Bad`; 10 s timeout, no redirects; body ≤ 1 MiB each way. The board's status and body come back as they are (`BoardApi{status: 404, ..}` is an answer, not an error); a board that is down, slow, or answers over 1 MiB is `Internal`. A GET only runs; a POST or DELETE runs once per op id, so a retry gets the first answer (that is how a lost reply to `POST /api/keys`, which shows the new key once, is not a 409). |
| `Show{sid, name, blob, caption}` | `Ok` | `femmed show`: the blob is saved as `<state>/uploads/<sid>/<name>` like an attachment and an `AssistantMessage` item (`{"text": "<caption>\n\nAttached: <path>"}`) is appended to the session. `Bad` for an unknown session or a blob that is gone. |
| `BlobPut{name, data}` | `BlobHash(h)` | `data` ≤ `MAX_BLOB_PUT` (8 MiB; this request may weigh that much, every other one 1 MiB) into the `blobs` table (hash = sha256); `name` is not kept. A `Prompt` whose `attachments` name blobs: each is written to `<state>/uploads/<sid>/<name>` (last path component, control characters become `_`, leading dots dropped, `-2` before the extension for a taken name; at most 16; a blob that is gone is `Bad`) and the prompt text gets a blank line and one `Attached: <absolute path>` line per file. **`BlobGet` serves an upload only until retention sweeps it**: a blob no transcript item refers to goes at the next daemon start or daily sweep. So `BlobPut` and `Prompt` close together, and a transcript row that shows an attachment reads the file at its `Attached:` path with `FsRead`. |
| `Caps` / `SetCap{account, five_hour, weekly}` | `Caps(Vec<AccountCap>)` / `Ok` | Stored in `<state>/caps.json` (`{"<number>": {"five_hour": 80, "weekly": 90}}`), written atomically. `server/fable-cap.sh` reads it: an account past *its* cap (either window) is `cswap disable`d and, if active, the box `cswap switch --strategy best`; below it again → `cswap enable`. `femme-cswap.service` keeps the global `CAP`, which still moves the box off the active account, so a per-account cap above it cannot keep an account in use longer. A cap is 50 to 99 (else `Bad`) and `account` is cswap's number, from 1; both `None` clears. The scheduler's pause at `CAP` stays global (`/usage` carries no account). |
| `ServerInfo` | `ServerInfo{version, deployed}` | `env!("CARGO_PKG_VERSION")` and `<state>/deployed` trimmed. |
| `Background{sid, tool_use_id}` | `Ok` | Headless: the driver's `background_tasks` control request for the call (`None`: every foreground task); a TUI or herdr pane gets `Ctrl+B`. `Busy` when nothing runs in the foreground. |
| `FsList{sid, path}` / `FsRead{sid, path}` | `FsEntries` / `FsData` | Implement: `path` relative to the session's `cwd` (absolute allowed under `$HOME`), no `..` escapes, no symlink out of home, hidden files listed, directories sorted dirs-first. `FsRead` is capped just under a frame, 16 MiB − 4 KiB (else `Bad`): a reply has to fit one 16 MiB frame. The session's own `<state>/uploads/<sid>/` is readable too, wherever the state folder is. |

Usage JSON: `GET /api/accounts` → `{"cswap": <cswap list --json>, "aws": {...}}`; each account has
`number`, `email`, `active`, `usage` (5 h / 7 d windows with `utilization` and `resets_at`, per-model
entries such as Fable), `usageFetchedAt`. See `crates/femmed/src/compat/accounts.rs` and the board's
renderer in `assets/index.html` (search `accts`). Keys: `GET /api/keys` → the list the board shows.

## Motion

One curve for everything that appears or goes: 180 ms ease-out (`cubic-bezier(.32,.72,0,1)`),
a fade with a short move, through `desktop/src/motion.rs` (`appear` for presence, `slide` for a
length) over gpui-base's `Presence`/`transition`. The rules, from the Apple HIG and T3 Code / Zed /
VS Code (`docs/smooth.md` has the sources):

- A panel slides in 16 px from its edge; a menu drops 4 px; a card rises 6 px with its scrim; the
  sidebar column slides over 160 ms. Each leaves the way it came, drawn until it is gone, and can be
  reversed mid-way. Nothing is longer than 200 ms and nothing bounces.
- Motion runs at the display's own rate: never cap a transition's frame rate, and let it dirty only
  what it moves (the panels are `.cached` leaves). A leaving element keeps no hover handler.
- Hover, selection, tab switches and typing are instant. Motion never blocks input.
- Streamed text appends; nothing is animated, re-laid out or scrolled by hand per token. The list
  follows the tail; any upward scroll by the user releases it; the "↓" pill shows 150 ms later and
  goes at once when they are back at the end. New rows of the current turn fade in once, whole.
- The working line is the 1 Hz star, nothing faster or brighter; it sits centred under the last row,
  not pinned to the bottom edge.
- Under the OS's "reduce motion", every one of these is an instant change (`Presence` sees
  `cx.reduce_motion()`).

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
