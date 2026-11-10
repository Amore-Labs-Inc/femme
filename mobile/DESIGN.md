# FEMME mobile — design

A phone app for the FEMME you already run: the agents live on your box, the phone is the place
you glance, answer and nudge from. Not a terminal in a pocket. Not the desktop shrunk. One job:
**nothing an agent needs from you waits longer than the time it takes to pull your phone out.**

Companion files: [`prototype/index.html`](prototype/index.html) (tap-through of every screen,
phone frame, light/dark), [`screens/`](screens/) (renders of it), [`PROTOCOL.md`](PROTOCOL.md)
(what the app speaks to `femmed`, and the four things `femmed` still lacks for it).

## Why a phone app at all

The web board already works on a phone, and ntfy already pushes. What they can't do:

| today | the gap |
|---|---|
| ntfy says "lovable-lesson is waiting" | you open the board, find the session, read the question, scroll to the composer. Four steps; the agent sat idle for all of them |
| the board shows a live xterm screen | a 80×24 screen on a 390 px wide phone is unreadable; the transcript is what you need, and only the desktop has one |
| the board's composer sends a message | it can't answer a Claude *question* (a tool call with options), approve a permission, or steer mid-turn |
| magic-link sign-in, 7-day cookie | you re-sign-in on the phone weekly; a native app pairs once |

So the app is built around **attention**, the one thing FEMME already models (`Session::attention`:
`None | Blocked | Done`): a session that is *blocked* on you is the top of every screen, and
answering it is one tap away from the notification.

## Principles

1. **Blocked first.** Anything waiting for you is pinned above everything else, everywhere.
   The badge on the app icon is the count of blocked sessions, nothing else.
2. **Answer, don't type.** Most phone interactions are a yes/no, an option, a short nudge, a photo.
   Questions render as tappable options; permissions as allow/deny; the composer is the last resort.
3. **The transcript, not the screen.** Same items the desktop shows (`UserMessage`, assistant
   markdown, folded tools, questions, peer cards, `Attached:` pictures), laid out for a thumb.
   A terminal exists, one tap away, read-mostly.
4. **Everything you send is saved first.** The outbox the desktop has (pending → read; "not received"
   with a resend) is the same on the phone, where links drop all the time.
5. **Same daemon, same protocol.** The app is one more `femme-client`. No new server, no new auth
   system: a paired device key, as the desktop has. `femmed` gains four small things (push
   registration, an attention digest, pairing, a WebSocket carrier) and nothing else.

## People and moments

- **Cem, 09:40, walking.** Phone buzzes: *lovable-family-twist-lesson · needs you*. Lock-screen
  notification shows the question and its first two options. Long-press → taps "yes, fold it".
  Done, phone back in pocket. The app was never opened.
- **At lunch.** Opens the app: the *Now* screen says two sessions done, one working 14 min, none
  blocked. Taps the done one, skims the last assistant message, taps 👍 (sends "ok, push it").
- **In a meeting.** A session is blocked on a permission ("Bash: `git push`"). The app shows the
  command in a card with *allow once · allow always · deny*. Taps allow.
- **On the sofa.** Starts a new chat on the Platform folder: "lesson 2 hero section, same style as
  lesson 1", attaches a photo of a sketch. Watches the first tool calls fold by, puts the phone down.
- **Something broke.** The *Box* screen: disk 71%, femmed up 4 h, deployed `df40bc9`, Claude
  2.1.319; the account rotation (cswap) at 62% of the 5-hour window. Nothing to do; closes it.

## Information architecture

Four tabs. No hamburger, no drawer.

```
┌──────────────────────────────────┐
│  Now      Chats      Box      Me │   ← bottom tab bar, 4 items
└──────────────────────────────────┘
```

| tab | what | why it earns a tab |
|---|---|---|
| **Now** | blocked sessions as cards you can answer in place; then done; then working (with elapsed and the last line); a `+` to start a chat | the whole reason the app exists; opens here every time |
| **Chats** | every live session (as the desktop's left sidebar), grouped by folder, search; a session opens to its transcript | finding one when it isn't on Now |
| **Box** | the machine: disk, memory, load, femmed/deploy state, Claude version, accounts & caps, PRs waiting (merge/fix with Claude), issues queue | the stuff `box-health` and the PRs page cover on the desktop; on the phone it's the "is everything fine" screen |
| **Me** | pairing (QR from the desktop's accounts page), notification rules (what pushes: blocked always; done optional; per-session mute), appearance, sign out | settings, once |

Deep: Chat screen (transcript + composer + chips) → Terminal (full-screen, landscape) → Files
(the session's folder, tap to view; share-sheet into the chat) → Session info (model, effort,
context bar, permission mode, cwd, agent sid, close/end).

## Screens

### 1. Now
- Top: the wordmark, link status (`quic 41 ms` / `reconnecting…` / `offline, 3 queued`).
- **Needs you** section: one card per blocked session. The card *is* the question: the
  session label, the agent's question text, the options as full-width buttons (a `Other…`
  opens the composer), or a permission card with the tool call summary and allow/deny. Answering
  removes the card with a swipe-out; the next one slides up.
- **Done**: sessions with `Attention::Done`, the last assistant paragraph as a two-line preview,
  a 👍 quick-reply chip (sends `ok`) and `open`.
- **Working**: label · elapsed · last tool line, a small spinner; tap opens the chat.
- Pull to refresh (it is live anyway: `Subscribe{Scope::Sessions}`); the list reorders only on
  attention change, never while you read.
- Empty state: "Nothing needs you." and the `+ chat` button.

### 2. Chat (a session)
- Header: label (tap = session info), status dot, context bar (the desktop's XP bar, 4 px, under
  the header; pink → red past 90 %; a compaction pulses it once).
- Transcript: the desktop's `Item` rows re-laid for 360 px:
  - user messages right-aligned in the accent border, attachments as 96 px thumbnails (tap = viewer);
  - assistant markdown full-width, code blocks horizontally scrollable with a copy button, never
    wider than the screen;
  - tool calls folded to one line (`▸ Bash · cargo test · 2.1 s`), tap to expand, long output capped
    at 40 rows with "open in terminal";
  - questions as option buttons inline (answered ones show the pick, greyed);
  - peer messages as the `✉ from @name` card;
  - `Attached:` pictures 100 % wide, tap = lightbox; videos with ▶;
  - compaction fold row.
- A `↓ new` pill when new items land while scrolled up. Never auto-scrolls while your finger is on it.
- Composer: one-line input that grows to 5; `+` (camera / library / file / voice note transcribed on
  device); `/` opens the command sheet (`/model`, `/effort`, `/goal`, `/compact`, `/clear`, …
  from `AgentEvent::Commands`); send. While the agent works, send = **steer** (as the desktop's
  Enter); long-press send = *interrupt and send*; a swipe-down on the composer = Esc/interrupt with a
  confirm. The outbox state shows under your bubble: `queued · read · not received ↻`.
- Chips above the composer when relevant: subagents (`‹ main` + one chip each), the background
  tasks, the permission mode (tap cycles).
- Swipe from the right edge: Files. Swipe from the left edge: back to the list. Rotate: Terminal.

### 3. Terminal
- Full-screen, landscape by default, the holder's screen (`Open{Mode::Pty}` rows), 16 px mono,
  pinch to zoom. A key bar: `Esc Tab ↑ ↓ ← → ^C ⏎`, and a text field that sends on return. Read-mostly:
  it is for *looking* at Claude's own dialogs (a plan, a `/login`, an install prompt) and the rare
  key. The chat is the normal way.

### 4. Box
- Health tiles: disk (used/total, the box-health warning when it has one), memory + swap, load,
  uptime. Deploy: `deployed df40bc9 · 11 min ago`, or `deploying…`/failed with the log tail.
  Claude: version, the nightly upgrade's last result, panes on an older binary (the nightly
  restart's report).
- Accounts: the active Claude account, its 5-hour and weekly bars, the cap; *switch* goes through
  the same `POST /api/accounts/switch` the desktop uses.
- PRs: the open ones (`Req::Prs`): title, branch, checks; `merge & deploy`, `fix with Claude`;
  the demo video plays in the lightbox.
- Issues: the queue; `+` adds one (repo chip, title line, details, photos) — the board's flow.

### 5. Me
- Pairing: scan the QR the desktop's accounts page shows (a one-time code carrying the host, the
  cert fingerprint and a pairing token); the app generates its device key and registers it. Multiple
  boxes allowed; a switcher at the top of Now when there is more than one.
- Notifications: *needs you* (always on), *done* (on/off), *working > 30 min with no output*
  (on/off), per-session mute (also from the chat header), quiet hours.
- Appearance: system / light / dark; text size follows the OS.
- Diagnostics: link, round-trip, last error, "send logs".

## What the neighbours do (and what FEMME takes from them)

Looked at on 2026-11-10: the herdr app (herdr.dev/app, plus the third-party herdrup / Herdr
Mobile / Sherpr / Heeler clients), T3 Code's phone client, and the Claude app's Code tab
(Remote Control) with the companions around it (Happy Coder, Omnara, VibeTunnel).

| | herdr app | T3 Code | Claude Code (Code tab) | Happy Coder |
|---|---|---|---|---|
| home | inbox: needs you → done → working → idle | unified inbox across environments | sessions list | sessions list |
| pairing | SSH / Tailscale profiles, no account | `npx t3 pair` → one-time token + QR; or a managed relay | `claude remote-control` prints a QR | QR → shared secret, E2E relay |
| answering | in the real terminal (type, paste a screenshot, speak) | chat + diff review (Approve / Request changes) | permission prompt in the chat; Bypass never offered | Allow / Deny with details, "remember for this session" |
| push | needs input, stopped, finished | lock-screen summary of what needs attention | done or needs a decision; two toggles | status, done, permission, question; deep links |
| terminal | the agent's own, with a keys row | Esc + Ctrl keys | — | — |
| complaints | — | — | prompts not rendering on iOS, no pushes, no Approve/Deny on the push itself | — |

Taken into FEMME mobile:

- **Now = inbox in that order**: needs you, done, working, idle (herdr, T3). A question's options
  sit on the card; one tap answers (nobody else does this on the home screen).
- **Push only for what changes what you do next** (needs input, stopped, finished), Time Sensitive,
  deep-linked, with **Allow / Deny as notification actions**, the thing Claude users ask for and
  nobody ships (M2, `PushRegister` in PROTOCOL.md).
- **Permission prompt as a sheet**: the command in monospace, the cwd, the agent's real options.
- **One-time QR pairing from a CLI command** (`femmed pair`), an environment screen with the box's
  state, a revoke on the box (T3, Claude, Happy).
- **Permission mode in the chat header**, switchable per chat; `bypassPermissions` is shown but
  never *offered* from the phone (Claude).
- **Terminal with a keys row** (Esc, Tab, Ctrl, arrows) above the keyboard (herdr, T3).
- **New chat = three fields** first (folder, agent, task), the rest folded (herdrup).
- **Diff review** stays out of v1: FEMME's unit of review is the PR card with Merge & deploy,
  not per-file approval (T3's model is for a different workflow).

## Visual language

The desktop's, one-to-one (`desktop/src/theme.rs`; the tokens are copied into
`prototype/index.html` `:root`):

- **Paper and ink**: paper `#FEFEFE` / ink `#1E1E1E` (dark: `#1E1E1E` / `#ECECEC`), panel
  `#E5E5E5`, muted `#5C5C5C`. The desk pink `#F386A1` only as the gutter behind cards.
- **Pink is "you"**: accent `#D45BB6` fills the selected row and your own bubbles' border; neon
  `#FF4FB4` is pressed/focus and the working spinner; never a resting fill.
- **Status is a dot, not a glyph**: an 8 px ink-bordered square filled with the lane colour,
  working `#03AA5C`, blocked `#F2B33D`, idle `#DEDEDE`, failed `#D93036`, closed `#858585`; PR
  teal `#09AEA1`. The transcript's working line spins `· ✢ ✳ ✶ ✻ ✽`.
- **Flat and hard**: 1.2 px ink borders, 0 radius, shadows with zero blur (`2px 2px 0 ink` under
  buttons and cards, `3px 3px 0` dim under the composer and dialogs); a pressed button loses its
  shadow and moves 2 px.
- **Type**: VT323 for titles (22 px) and buttons (20 px), Schibsted Grotesk 14 px for body,
  JetBrains Mono 13 px for code and 12 px for meta, Kaushan Script for the wordmark. On the phone
  body goes to 16 px and follows the OS text size; mono stays 13.
- **Motion**: one curve, 180 ms `cubic-bezier(.32,.72,0,1)`, nothing over 200 ms, nothing
  bounces: a card leaving Now, the context bar pulsing once on a compaction, the `↓ new` pill.
- Touch targets 44 px. Red only for *not received* and past-90 % context.

## Architecture

```
phone app ──QUIC (same femme-proto as the desktop)──► femmed on the box
   │                                                      │
   └──APNs / FCM (via ntfy for the self-hosted path)◄─────┘  push: blocked / done
```

- **Stack: Flutter** (one codebase, iOS + Android, Rust FFI for the protocol). `femme-client`
  (Rust) compiles to a static lib with `flutter_rust_bridge`; the Dart side holds the UI only. The
  Rust core is the *same crate the desktop uses* (framing, reconnection, outbox, the item fold), so
  a protocol change lands in one place. Rejected: React Native (JS bridge for a binary QUIC stream
  is pain), native ×2 (two UIs to keep in step with a moving protocol), a PWA (no push on iOS
  worth the name, no QUIC, no background).
- **Transport**: `femme-client` over QUIC (quinn, ALPN `femme/1`, UDP 7788), mutual TLS: the
  phone's own cert is enrolled on the box, the box's fingerprint pinned on the phone, exactly as a
  desktop device. The desktop's second path, an SSH mux, has no place on a phone; when QUIC is
  blocked (hotel wifi) the app needs a WebSocket carrier of the same frames behind Caddy's HTTPS,
  which `femmed` does not have yet (it is the 4th addition below, and the plan already names it).
- **State**: the app holds the session list and one open session's items in memory, persisted to
  SQLite so it opens instantly and offline shows the last state. Items come as `Scope::Session`
  deltas from the daemon's rev; reconnection resends from the last rev (the protocol already does
  this for the desktop).
- **Outbox**: `Req::Prompt` saved locally *and* in the daemon's `Pending` row before delivery (the
  save-first path `femmed` got in Nov 2026); the bubble shows the daemon's status, and the app
  retries on reconnect until the daemon acknowledges the row.
- **Push**: `femmed` gains `Req::PushRegister{token, platform}` and sends, through ntfy (self-hosted
  path, works today) or a tiny relay for APNs/FCM, one notification per attention change to
  `Blocked`/`Done`, carrying the session id and the question + up to 3 options as notification
  actions. Tapping an action sends `Req::AnswerQuestion` from the notification extension without
  opening the app.
- **Uploads**: photos up to 8 MiB go as one `BlobPut`; `femmed` gains a chunked variant for videos.
- **Security**: a device cert per phone, enrolled through the pairing QR (today enrolment is
  `femmed enroll --name <dev> --spki <sha256>` over SSH, which a phone can't do), listed and
  revocable with `femmed devices` and on the desktop's accounts page; biometric lock optional
  (Me); nothing cached in plaintext beyond the SQLite file in the app sandbox.

## What `femmed` needs (all small; see PROTOCOL.md)

1. `Req::PushRegister` / `PushUnregister` and a notifier backend that posts to the registered
   tokens (ntfy topic per device for the self-hosted path).
2. A `Scope::Attention` subscription: only the sessions whose attention changed, with the pending
   question (`Item::Question`) inlined, so Now needs one stream and no per-session subscriptions.
3. ~~A pairing QR via `PairStart/PairFinish`~~ **Shipped simpler** (PR #50): `femmed pair --name phone`
   on the box prints a `femme://pair?host=…&tls=1&name=…&token=…` QR; the token's SHA-256 is the
   device row's key, so `femmed devices` lists and revokes the phone like any device.
4. ~~A WebSocket carrier~~ **Shipped** (PRs #50, #51): `wss://<box>/femme/ws`, JSON frames
   multiplexing the same Ctrl / Events / Term streams (`PROTOCOL.md` §Carrier).

## Out of scope (v1)

- Editing files, a code viewer with syntax highlighting (the viewer shows text; no editing).
- Forking local agents to the box (there is no local agent on a phone).
- The full terminal as a working surface (it is read-mostly).
- Multi-user: one owner, many devices.

## Milestones

| | ships | proof |
|---|---|---|
| M0 | this design + the prototype; the four `femmed` additions as PRs | tap-through on a phone; `PairStart/Finish` round trip; a push lands on a test device |
| M1 | Now + Chat (read + send + answer questions), pairing, push | answer a real question from the lock screen in < 5 s |
| M2 | Files, Terminal, Box, Me | PR merge from the phone; a photo of a sketch becomes a prompt |
| M3 | offline-first polish, voice notes, quiet hours, multi-box | 24 h on a phone with flaky signal: no lost message |

### Status (2026-11-10)

M1 and M2 are built in `mobile/app` (Flutter, pure Cupertino; `PARITY.md` is the checklist):
Now (needs-you cards with single and multi-select answers), Chats, Chat (markdown, tools,
questions, permission prompts, attachments with video posters, slash commands, history), Terminal
(read, keys, mouse reporting), Files (folder pages with iOS swipe-back, viewer, hex, media), Box
(link round-trip, PRs with merge and auto-fix, accounts with add-account and the scheduler line,
API keys with the GitHub device flow, issues, a red dot on the tab when a sign-in ends soon), Me
(pairing, appearance). The outbox survives app restarts.

Verified natively: the Linux desktop build against the live box; CI installs the release APK on
an Android emulator and a simulator build on an iPhone, pairs both with the box and screenshots
the tabs (`.github/workflows/mobile.yml`, artifacts `femme-android-screens`, `femme-ios-screens`);
the device ipa is built unsigned on the same macOS runner (Xcode 26).

Not yet: push to the phone itself (`PushRegister` + an APNs/FCM notifier needs the user's Apple
developer account and a Firebase project; until then the box's ntfy topic is the push),
`Scope::Attention` (Now subscribes per session today), terminal fling momentum and sticky
modifiers on mouse events.
