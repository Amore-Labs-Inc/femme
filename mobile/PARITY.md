# FEMME desktop → phone: parity checklist

Everything a user can do or see in the GPUI desktop app (`desktop/src`), with the code that does it and
the wire it uses. This is the acceptance list for "the phone does everything desktop FEMME does".
Item body shapes (what each transcript row is made from) are in `BODIES.md`; the transport and the
planned daemon additions are in `PROTOCOL.md`.

Paths are relative to the repo root. "local only" means no request: the desktop works it out from
state it already holds (`Session` rows from `Subscribe{Global}`, `Item` rows from
`Subscribe{Session(sid)}`).

## What the desktop does NOT have (read this first)

Code-verified absences, so nobody builds a phone screen to match a desktop feature that does not exist:

| absent on desktop | evidence | protocol exists? |
|---|---|---|
| Issues board (create / edit / status / delete) | no `Req::Issue*` anywhere in `desktop/src`; `Row::Issue` is dropped: "Issue rows and streaming text are not shown here" `desktop/src/net.rs:168` | yes: `Req::IssueCreate/IssuePatch/IssueDelete` `crates/femme-proto/src/rpc.rs:147-160`, served `crates/femmed/src/front/req.rs:148-178`; statuses `IssueStatus` `crates/femme-proto/src/state.rs:208-216`. The web board (`board/`) is where issues live today. |
| Takeover (headless ↔ TUI) | no `Req::Takeover` in `desktop/src` | yes, `rpc.rs:133-136`, served `req.rs:144` |
| `Req::SetModel` | not used; the desktop switches model by sending `/model …` as a prompt (`desktop/src/composer.rs:528-547`) | yes, `rpc.rs:137-140`, served `req.rs:145` |
| Choice of agent / mode / model / label / prompt when starting a chat | every desktop create is a headless Claude on `claude-opus-5-5` with no label, name or prompt: `desktop/src/session.rs:72,82-97` | yes, `Req::SessionCreate` takes all of them `rpc.rs:97-107` |
| Unread counts, attention badges beyond the status dot, OS notifications | no unread / notification code in `desktop/src` (only `cx.notify()` redraws); attention is folded into the dot colour `desktop/src/sidebar.rs:93-109` | `Session.attention` `state.rs:40-45,65`; ntfy push is daemon-side (`PROTOCOL.md` §Push) |
| Pairing UI | the desktop is enrolled out of band: the status line prints `ssh HOST femmed enroll --name … --spki …` `desktop/src/connect.rs:180-187`, `desktop/src/net.rs:323-339` | `PairStart/PairFinish` are **new** (`mobile/PROTOCOL.md`) |
| Session search / filter, archive of closed sessions | the list is live sessions only `desktop/src/net.rs:267-272` | — |
| `femmed show` sending (agent side) | the desktop only displays it | `Req::Show` `rpc.rs:220-225`, `Req::SessionOf` `rpc.rs:262-264` |
| `Req::Upload` (big uploads) | not used | served as `Unsupported` `req.rs:182` |

## 1. Connection, device, server info

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| Where the daemon is | `--daemon ADDR:PORT --fingerprint HEX64`, `--ssh HOST`, or `FEMME_DAEMON / FEMME_FINGERPRINT / FEMME_SSH` env, or `~/.config/femme/desktop.env`; flags beat env beat file | `desktop/src/connect.rs:79-106,124-141` | local only | Phone: QR pairing (`femme://pair?...`, PROTOCOL.md) replaces the env file. No ssh fallback on a phone. |
| Device identity | key made on first run in `~/.config/femme/device` (dir 0700, key 0600); `--show-spki` prints the SPKI hash and the enroll line | `desktop/src/connect.rs:167-178,189-218` | mTLS client cert | Keep the key in the Keychain / Keystore. |
| Not enrolled hint | status line says `quic refused this device, enroll it: ssh HOST femmed enroll --name <host> --spki <hex>` | `desktop/src/net.rs:323-339`, `connect.rs:180-187` | QUIC handshake refused | Phone shows the pairing screen instead. |
| Link status in the top bar | `connecting…`, `quic 23 ms` (smoothed RTT, polled once a second), `ssh`, `offline: <why>`, then ` \| <what is starting> \| <last failed request>` | `desktop/src/net.rs:302-313,472-484,520-528`; top bar `desktop/src/app.rs:303-312` | client `Event::Connected/Disconnected`, RTT from the transport | A one-line status chip; tap for the full text (the desktop's tooltip has it whole, `app.rs:303-305`). |
| Centre cards | `no daemon configured`, `connecting…`, `offline` + reason, `no session` (`nothing is running on the daemon` / `pick one in the list on the left` / what is being started), `opening the session…`, `session closed` | `desktop/src/frame.rs:25-75` | local only | Same states as empty-states of the chat screen. |
| Reconnect | automatic: back-off 250 ms → 1 s → 2 s → 5 s; on reconnect the client resubscribes by itself (rev cursors) | `crates/femme-client/src/conn.rs:30-36,81`; global resubscribe `desktop/src/net.rs:452-458` | `C2S::Hello{cursors}` `rpc.rs:29-39`; `S2C::Changes{reset}` `rpc.rs:387-392` | On `reset: true` drop the scope's copy first (`net.rs:118-170`, test `reset_clears_the_global_scope_only` `net.rs:702`). |
| Manual reconnect | femme ▸ Reconnect: QUIC migrates to a fresh UDP socket, ssh redials | `desktop/src/net.rs:504-513`, `desktop/src/app.rs:627-631` | client-side | Phone: also on network change (Wi-Fi ↔ cellular) automatically. |
| Request timeout | 30 s per request; a failed request's reason sits in `Net::error` and shows in the status line until the next success | `crates/femme-client/src/lib.rs:55`, `desktop/src/net.rs:543-575` | `S2C::Resp{result: Err(Err{code,msg})}` `rpc.rs:354-369` | `ErrCode::Unsupported` means "daemon too old": cards say so plainly (`desktop/src/prs.rs:261-268`, `desktop/src/accounts.rs:9-10`). |
| Server info / check for updates | femme ▸ Check for updates: asks which commit the box runs, compares with the build's `COMMIT`; verdict row for 10 s (`up to date`, `box runs <sha>: building it now …`, `no marker`, `daemon too old`); also run on every (re)connection and it starts the PC updater unit | `desktop/src/menu.rs:16-19,88-200,474-540`, `app.rs:492-501` | `Req::ServerInfo` → `Resp::ServerInfo{version, deployed}` `rpc.rs:205,340-344` | Phone: show daemon version + deployed sha in a "Box" screen; no self-updater (app stores do that). |
| Which session opens at start | newest live, running agent session (Claude > Codex > Pi; crashed/failed/interrupted don't count); none → create headless Claude in `~`; that fails → newest live shell or a new shell. Waits for the first `Changes` (≤ 2 s after link up) | `desktop/src/session.rs:1-60,128-...` (`open_at_start` at :138) | `Subscribe{Global}`, maybe `SessionCreate` | Phone should NOT auto-create a session at start; open the "Now" list instead. |
| Current session follows closes | when the shown session closes, the centre moves to the newest live one (or none); a Claude session that appears while a shell is shown (user typed `claude`) takes the centre | `desktop/src/net.rs:56-61,203-242` | rows of `Subscribe{Global}` | Optional on phone. |

## 2. Sidebar / sessions list

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| Which sessions are listed | live only (`closed == None` and status ≠ `Closed`) | `desktop/src/net.rs:73-75,267-272` | `Row::Session` via `Subscribe{Global}` | Tombstones remove (`net.rs:173-201`). |
| Sort | newest first by `(created, id)` | `desktop/src/net.rs:268-271` | local only | |
| Grouping | grouped by folder (`cwd` without trailing `/`), project of the newest session first; header = folder's last part (`~` for home), tooltip = short dir | `desktop/src/sidebar.rs:165-197` | local only | Collapsible sections. |
| Fold a project | click header → folds; folded shows the hidden count | `desktop/src/sidebar.rs:683-738` | local only | Remember folds per device. |
| `+` per project | header's `+` (on hover) starts a headless Claude in that folder and shows it | `desktop/src/sidebar.rs:725-736,537-559` | `Req::SessionCreate{cwd, Claude, Headless, model: claude-opus-5-5}` `desktop/src/session.rs:80-97` → `Resp::Session(sid)` | Then `set_current(sid)`; a second click while one is out is ignored (`sidebar.rs:542-544`). |
| Row content | status dot, title, second line = agent name (`claude`/`codex`/`pi`/`shell`) or the provider of a gateway model (`deepseek` from `deepseek/…`) | `desktop/src/sidebar.rs:199-227,624-681` | local only | |
| Title rule | `name`, else `label`, else last path component of `cwd`, else `handle` | `desktop/src/net.rs:104-114` | local only | |
| Home dir shortening | `/home/<u>`, `/Users/<u>`, `/root` → `~` | `desktop/src/sidebar.rs:138-163` | local only | |
| Status dot (attention) | Working → `work` colour; `Blocked` status OR `attention == Blocked` → `pause` (amber); Idle/Starting → `queue`; Interrupted/Paused/Crashed/Failed → `bad` (red); Closed → `done`. `Attention::Done` has no colour of its own | `desktop/src/sidebar.rs:81-126` | `Session.status`, `Session.attention` `state.rs:24-45` | This is the only "badge" the desktop has. Phone: same mapping, plus a Blocked section on "Now". |
| Collapsed strip | mod+b collapses to a 28 px strip of dots; a dot click opens and expands | `desktop/src/sidebar.rs:50-76,740-762,821-838`, key `app.rs:109` | local only | Phone: n/a (list is a screen). |
| Keyboard | Up/Down/Home/End move a cursor, Enter opens, Esc leaves | `desktop/src/sidebar.rs:513-535` | local only | |
| Open a session | click / Enter → `OpenSession(sid)` → `Net::set_current` | `desktop/src/sidebar.rs:476-481`, `app.rs:610-615` | then `Subscribe{Session(sid)}` (`desktop/src/transcript/live.rs:738-770`) | Unsubscribe the previous scope (`live.rs:745-747`); retry a refused subscribe every 3 s (`live.rs:54`). |
| Rename | right-click → `rename` → inline box prefilled with the title; Enter sends; empty clears the label | `desktop/src/sidebar/ctx.rs:39-78` | `Req::SessionLabel{sid, label: Option}` `rpc.rs:213-217` | Long-press menu. |
| Close | right-click → `close` (red): ends the session for real (holder and agent stop) — works on crashed ones too. No confirm | `desktop/src/sidebar/ctx.rs:1-5,98-106`, `sidebar.rs:561-567` | `Req::SessionClose{sid}` `rpc.rs:108-110` | Phone: swipe + confirm (closing kills the agent). No "hide" exists. |
| Empty state | `+ claude` button (headless Claude in daemon's home, `cwd: ""`); only when connected | `desktop/src/sidebar.rs:19-21,250-263,613-622` | `SessionCreate{cwd: ""}` (empty = daemon home; `~` is refused) `session.rs:74-77` | |

## 3. New chat (add project)

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| Open the picker | title bar's add-project button (`AddProject`) | `desktop/src/sidebar.rs:78-79`, `desktop/src/projects.rs:93-160` | — | |
| Folder browsing | path field starting at `~/`; lists the folders of the typed dir, filtered by what follows the last `/` (case-insensitive prefix; hidden ones only when the filter starts with `.`); `..` row away from home | `desktop/src/projects.rs:27-81,175-228` | `Req::Folders{path, make: false}` → `Resp::Folders{path (absolute), names}` `rpc.rs:174-180,328-331`; must lie under home | Phone: tappable folder list + breadcrumb. |
| Keys | Up/Down pick, Enter goes into the picked folder (or adds the typed path when none picked), mod+Enter adds the picked folder, Tab completes the common prefix, Esc/Backspace-on-empty closes | `desktop/src/projects.rs:1-12,240-280` | local only | |
| Create & Add | a missing path is created ("Create & Add") | `desktop/src/projects.rs:229-238,282-320` | `Req::Folders{path, make: true}` then `Req::SessionCreate{cwd, Claude, Headless, model: claude-opus-5-5}` | |
| Agent / mode / model / label / first prompt | **not offered on desktop** (always Claude, headless, opus 5.5; femmed adds `--effort=medium`) | `desktop/src/session.rs:71-97` | `SessionCreate` supports `agent, mode, label, name, prompt, model` `rpc.rs:97-107` | The phone brief asks for these; they are a superset of desktop. A model naming a maker (`deepseek/…`) runs through OpenGateway (`rpc.rs:104-106`). |

## 4. Transcript

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| Live items | rows upsert by `item_id` keeping the higher rev; tombstones remove | `desktop/src/transcript/items.rs:1-12` | `S2C::Changes{scope: Session(sid), rows: Row::Item…}` | |
| Streaming text | `Delta{item_id, offset, text}` merged by byte offset; may be out of order, repeated after reconnect, start mid-item, or precede the item's row | `desktop/src/transcript/items.rs:1-12` | `S2C::Delta` `rpc.rs:393-399` | Rules in `BODIES.md` §Delta. |
| Older history | scrolling to the top fetches 100 older items; a header row "older messages" (retry on failure); scroll position kept | `desktop/src/transcript/live.rs:50-51,483-523`, `rows.rs:684-708` | `Req::TranscriptPage{sid, before_seq, limit}` → `Resp::Page{items, next_before}` `rpc.rs:89-93,318-321` | |
| Long bodies | body > 4 KiB comes as `blob_hash`; message rows fetch at once when on screen, tool rows only when opened | `desktop/src/transcript/rows.rs:286-292`, `live.rs:552-575` | `Req::BlobGet{hash}` → `Resp::Blob` `rpc.rs:94-96` | |
| Follow the end / ↓ pill | list pins to the end while text streams; scrolling up >150 ms shows a "↓" pill; click glides back (200 ms) | `desktop/src/transcript/live.rs:56-61,345-405,694-736` | local only | |
| Keys | PageUp/PageDown, Home, End (follow again), Esc (clear selection, or back from a subagent view) | `desktop/src/transcript/live.rs:641-669` | local only | |
| Rows by kind | user card (right), peer card (left, headed by sender), assistant markdown, plan, thinking (folded line), tool row (call+result in one row), question (dialog or answered summary), event note, `read` mark, notable turn end; `ToolCall`/`TurnStart` draw nothing themselves | `desktop/src/transcript/rows.rs:272-314` | — | Body fields per kind: `BODIES.md`. |
| Markdown | paragraphs, headings, code blocks with language, lists, quotes, tables, links (bare `http(s)://` too); unclosed fence/`**` while streaming | `desktop/src/transcript/markdown.rs:1-8`, `md_view.rs:1-6` | local only | |
| Select / copy | mouse selection over all transcript text, ctrl-c copies | `desktop/src/transcript/select.rs:1-20` | local only | Long-press select / copy message. |
| Folding finished turns | a finished turn's thinking + tool calls collapse into one line: `worked for 3m · 12 tools`, `thought for 5s`; click opens; a run of calls in a running turn says `ran 3 commands, read 2 files` | `desktop/src/transcript/turns.rs:36-133,271-...` | local only | |
| Tool row | one line: name + hint (+ `→ first line of result`, red if failed); click opens input/output blocks capped at 400 lines / 24 000 bytes; an `Artifact` call's hint is a link | `desktop/src/transcript/rows.rs:40-44,733-876`, `turns.rs:186-270` | output blob via `BlobGet` | Summary rules: `BODIES.md`. |
| Move a task to the background | a running Bash/Agent call the turn waits for shows `⇣ background`; click moves it. mod+shift+b moves every foreground task. Afterwards `in background` until its end event | `desktop/src/transcript/rows.rs:824-844`, `live.rs:420-447`, `app.rs:110,976-978` | `Req::Background{sid, tool_use_id: Some(id) \| None}` `rpc.rs:120-127` (`Busy` when nothing runs) | |
| Subagents | strip under the list: main thread + one chip per `Agent`/`Task` call of the turn (and background ones still running), with elapsed time; a chip, or `open ›` on the tool row, opens the subagent view (items whose `parent` is the call id); Esc goes back | `desktop/src/transcript/agents.rs:1-120`, `live.rs:311-336,813-889`, `rows.rs:746-756,807-822` | `Item.parent` `state.rs:182-184` | |
| Working line | `✻ Thinking… (5m 42s · ↓ 15.0k tokens)`: verb from the last item (Thinking / Writing / Running a tool), time since the last user message, bytes produced; redrawn every second | `desktop/src/transcript/working.rs:1-80`, `live.rs:778-811` | local only (session `Working`) | |
| Goal strip | above the composer while a `/goal` is set: `◎ goal · <condition> · working \| not met yet`, `✓ … achieved`; hover/click shows full condition and checker's reason; hidden when `Cleared` | `desktop/src/transcript/live.rs:891-968,1095-1112` | `Session.goal` `state.rs:77-102` | |
| Compaction hand-off | Claude's post-compaction summary (arrives as a user message) is a folded muted row; the compact event's own row is hidden | `desktop/src/transcript/rows.rs:52-57,612-650,283-285` | local only | |
| Peer messages | another Claude session's SendMessage: left card headed by the sender | `desktop/src/transcript/rows.rs:465-501` | `ItemKind::PeerMessage` | |
| Prompts on their way (outbox) | a sent prompt shows at once as a faded "you" bubble (with file names) until the stored `UserMessage` with the same op id arrives; dropped on error, after 10 min, or for `/commands` | `desktop/src/transcript/outbox.rs:1-15`, `rows.rs:158-172`, `composer.rs:674-683,720-731` | `Req::Prompt` op id ↔ `UserMessage.body.op` | Persist the outbox on the phone (PROTOCOL.md). |
| Delivery state of a message | under a user message: `queued` (Pending), `read 13:53` (Completed + `read_at`), red `Claude didn't get this · resend` (Failed) | `desktop/src/transcript/rows.rs:358-404` | `ItemStatus` + body `read_at` | |
| Resend | click the red line | `desktop/src/transcript/rows.rs:406-418` | `Req::Resend{sid, item_id}` `rpc.rs:253-259` | |
| Read mark | where the agent read a queued message: thin line that scrolls to it (hidden if directly below) | `desktop/src/transcript/rows.rs:174-178,420-463` | `Event` whose body has `read` | |
| Fade-in | rows < 1 s old fade in and rise 4 px | `desktop/src/transcript/rows.rs:46-50,232-270` | local only | |
| Empty hint | words over an empty transcript | `desktop/src/transcript/live.rs:671-692` | — | |
| Request errors | a failed fetch/answer/resend shows a dismissible note | `desktop/src/transcript/live.rs:106-107` | — | |

## 5. Attachments and media in the transcript (`Attached:` lines, `femmed show`)

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| Attached files of a user message | the daemon appends `Attached: <path>` per uploaded file to the prompt text; these lines are removed from the text and shown as chips / thumbnails | `desktop/src/transcript/attach.rs:1-34`, `rows.rs:327-356` | text convention | |
| Which files are pictures | by extension: png jpg jpeg gif webp bmp svg; videos mp4 mov webm mkv avi m4v (shown as a still) | `desktop/src/transcript/attach.rs:36-57`, `desktop/src/video.rs:90-97` | — | |
| Thumbnail bytes | read from the box by path | `desktop/src/transcript/live.rs:582-613` | `Req::FsRead{sid, path}` → `Resp::FsData` `rpc.rs:170-173` | Not `BlobGet`: the path is what the text carries. |
| Video still | ffmpeg poster frame off the UI thread | `desktop/src/transcript/live.rs:615-639` | local | Phone: AVFoundation / MediaMetadataRetriever. |
| `femmed show` media | `Attached:` lines inside an AssistantMessage = files the agent showed; bigger boxes | `desktop/src/transcript/rows.rs:503-534`, `thumb.rs:1-10` | appended by `Req::Show` `rpc.rs:218-225` | |
| Hover preview, lightbox | hover = larger preview; click = full-window lightbox (picture fitted; video player) | `desktop/src/transcript/thumb.rs:1-10,190-200` | local | |
| Thumbnail menu | right-click: `copy image`, `open` | `desktop/src/transcript/thumb.rs:220-230,244-...` | local | Share sheet / save to Photos. |

## 6. Composer

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| Which sessions take prompts | agents only; placeholders `not connected`, `no session open`, `this is a shell: type in the terminal`, `message codex`, `message pi`, `message claude` | `desktop/src/composer/draft.rs:134-188` | local only | |
| Model / effort display | placeholder `message opus 5.5 · high` from `Session.model` / `Session.effort` (`claude-opus-5-5` → `opus 5.5`, `[1m]` dropped, gateway provider dropped) | `desktop/src/composer.rs:257-269`, `draft.rs:163-205` | `Session.model`, `Session.effort` `state.rs:63,75-76` | Show as a chip in the chat header. |
| Send | Enter (or `send` button); Shift+Enter newline; input grows 2→8 lines | `desktop/src/composer.rs:47-50,443-452,1080-1093` | `Req::Prompt{sid, text (trailing ws trimmed), attachments, how}` `rpc.rs:111-116` | |
| Steer / queue / interrupt | idle → `Queue`; working: Enter → `Steer`, ctrl-Enter → `InterruptThenSend`, a `/command` → `Queue` | `desktop/src/composer/slash.rs:117-131` | `PromptHow` `rpc.rs:41-48` | Phone: a send button with a long-press "stop and send". |
| Interrupt | Esc in the composer while the session is Working | `desktop/src/composer.rs:595-628` | `Req::Interrupt{sid}` `rpc.rs:117-119` | Stop button while working. |
| Send gate | blocked while: not an agent; a prompt is in flight; uploads still running (`still uploading: wait`); uploads failed (`failed: retry or remove`); empty text and no files | `desktop/src/composer/draft.rs:206-247,411-446` | — | |
| After send | text and sent chips cleared only on success; text typed meanwhile stays; error stays under the input; an expired upload (`its upload is gone`) re-uploads | `desktop/src/composer.rs:712-770`, `draft.rs:221-223` | — | |
| Drafts | per session; survive restarts in `$XDG_STATE_HOME/femme/drafts/<sid>.txt` (text only) | `desktop/src/composer.rs:201-218`, `composer/disk.rs:1-6` | local only | Persist per sid. |
| Prompt history | Up on the first row recalls earlier prompts of the session (from replicated `UserMessage`s + unechoed local sends), Down goes forward, Esc ends | `desktop/src/composer/history.rs:1-120`, `composer.rs:330-376` | local only | |
| Attach files | `+` (file picker, multiple), drag-and-drop, paste images or copied files | `desktop/src/composer.rs:794-889,1076-1078,1140-1164` | `Req::BlobPut{name, data}` → `Resp::BlobHash` `rpc.rs:189-194,338`, then `Attachment{name, blob}` in `Prompt` | Camera, Photos, Files. |
| Attachment limits | ≤ 8 MiB per file (`MAX_BLOB_PUT`), ≤ 16 files per prompt, empty files and directories refused; duplicate names become `name (2).ext`; pasted pictures `paste-<n>.png` | `crates/femme-proto/src/rpc.rs:301-302`, `desktop/src/composer/draft.rs:47-104,279-322` | — | |
| Chip states | uploading (spinner) / ready (size, pixel size) / failed (reason on hover, retry, remove); hover preview | `desktop/src/composer/chips.rs:1-6,80-100,255-300`, `draft.rs:19-45,333-391` | `BlobPut` again on retry | |
| Chip menu | right-click: `copy image` (raster only), `open`, `remove` | `desktop/src/composer/chips.rs:140-150,420-432` | local | |
| Permission mode | 10 px colour square above `+`: manual/default (green), acceptEdits (yellow), plan (purple), auto (neon), bypassPermissions (red), dontAsk; tooltip = name; Claude sessions only | `desktop/src/composer/permission.rs:1-44`, `composer.rs:1097-1112` | `Session.permission` `state.rs:69-71` | |
| Cycle permission | Shift+Tab in the input or click the square | `desktop/src/composer.rs:275-325` | `Req::CyclePermission{sid}` `rpc.rs:209-212`; new mode arrives on the session row | Tap the mode chip. |
| Context XP bar | bar above `send`: fill = `used / window` (0 while window unknown), red ≥ 90 %, 5 notches; level = compactions + 1, ring pulse on level-up; tooltip `current context 123,456 / 1,000,000`, `level progress`, `room left`, `level` | `desktop/src/composer/xp.rs:1-120`, `composer.rs:1120-1131` | `Session.context{used, window, compactions}` `state.rs:104-114` | **Where the numbers come from:** `used` = input + cache_read + cache_creation tokens of the latest call (`crates/femme-agents/src/claude.rs:2282-2288`); `window` = the `result` line's `modelUsage[model].contextWindow` (`claude.rs:702-711,1204-1208`), or a built-in 1,000,000 for opus-5-5 / fable-5-1 / fable-5-2 before the first result (`claude.rs:2295-2304`); merged into the row and compactions summed by `with_context` (`crates/femmed/src/headless.rs:1492-1512`). |
| Notices | one line under the input: refusals, send errors, `permission mode: …`, `interrupt failed: …` | `desktop/src/composer.rs:784-789` | — | Toast / inline. |
| Board diagram in | the drawing board hands a PNG to `add_attachment` | `desktop/src/composer.rs:220-237`, `app.rs:670-676` | `BlobPut` | Phone: n/a (or a sketch pad later). |

## 7. Slash commands

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| Command list | `/` menu of the daemon's commands, asked once per connection, again at most every 30 s while empty | `desktop/src/composer.rs:52-54,475-499` | `Req::Commands` → `Resp::Commands{names}` (from the last `claude` that started, plus FEMME's `btw`) `rpc.rs:206-208,345-348` | |
| Matching | while the text is `/word` with no space: prefix matches alphabetically, then substring matches, max 8 | `desktop/src/composer/slash.rs:55-101` | local only | |
| Menu keys | Up/Down pick, Tab/Enter complete (`/name `), Enter on an exact name sends it, Esc closes | `desktop/src/composer.rs:560-588`, `slash.rs:103-114` | local only | |
| `/model` picker (client-side) | `/model ` lists `opus, sonnet, haiku, fable, deepseek` (labels `model opus 5.5`); a Claude model then lists efforts `low medium high xhigh max`; picking sends `/model <name>` then `/effort <effort>` as two queued prompts; `deepseek` switches at once | `desktop/src/composer/slash.rs:9-53,60-82`, `composer.rs:528-547` | two `Req::Prompt{how: Queue}` | The only client-side expansion. Ids: opus→`claude-opus-5-5`, sonnet→`claude-sonnet-5-5`, haiku→`claude-haiku-5-5`, fable→`claude-fable-5-2`, deepseek→`deepseek/deepseek-v4.1-flash-ultrafast` (`slash.rs:20-27`). |
| Everything else | sent as the prompt text (`/goal …`, `/effort …`, `/compact`, `/btw …`); the daemon stores no message for a `/command`, so its outbox bubble is dropped on the answer | `desktop/src/composer.rs:720-731` | `Req::Prompt` | |

## 8. Questions and permission prompts

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| Open question dialog | `question` dialog in the transcript: prompt, numbered options; single-choice click answers at once; multi-choice ticks then `send`; free-text field (Enter sends) when allowed; `cancel` | `desktop/src/transcript/rows.rs:939-1060`, `ask.rs:1-91` | `Req::AnswerQuestion{sid, request_id, answer: Option(i) \| Options(v) \| Text(s) \| Cancel}` `rpc.rs:57-63,128-132` | Bottom sheet; also from a push action. |
| Sending state | `sending…` until the daemon turns the item into `Answered`; on error the buttons come back with a note | `desktop/src/transcript/ask.rs:92-131` | item status `Pending → Answered` `state.rs:156-163` | |
| Answered / ended | prompt + what was answered, or why it ended unanswered | `desktop/src/transcript/rows.rs:1073-1104`, `question.rs:1-5` | body `answered`, `reason` | |
| Session blocked | sidebar dot amber while `status == Blocked` or `attention == Blocked` | `desktop/src/sidebar.rs:93-97` | `Session` row | |
| Permission prompts / plan approval | rendered through the same question row | see `BODIES.md` §Question / §Plan | `AnswerQuestion` | Mapping of answers to Allow / Deny / Always: `BODIES.md`. |

## 9. Terminal

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| Shell / TUI session in the centre | a shell, or an agent in TUI mode, fills the centre with its first terminal; ended → `session closed` card | `desktop/src/frame.rs:176-186` | `Term(pane)` stream | |
| Terminal tab (right panel) | one tab per pane of live sessions (oldest first), label = program's title, else pane title, else `<session> · <no>` (28 chars) | `desktop/src/net.rs:274-300`, `terminal/panel.rs:1-20` | `Row::Pane` (global scope) | Headless agent's pane 1 is its transcript, never shown (`panel.rs:11-13`). |
| `+` shell | new pane in the current session's folder; no session → new shell session in `~` | `desktop/src/terminal/panel.rs:366-385` | `Req::PaneOpen{sid}` → `Resp::Pane` or `SessionCreate{Shell, Tui}` | |
| close | first pane of a shell session → closes the session; other panes → `PaneClose`; agent's own pane cannot be closed | `desktop/src/terminal/panel/tabs.rs:107-123` | `Req::SessionClose` / `Req::PaneClose{pane}` `rpc.rs:141-146` | |
| Tabs keys | ctrl-tab / ctrl-shift-tab | `desktop/src/terminal/panel.rs:76-77` | local | |
| Rendering | row cache from `femme-term`, frames applied and acked on draw (a hidden pane stops acking); first 8 (`EAGER`, `panel.rs:46`) panes attached eagerly, others when shown | `desktop/src/terminal/pane.rs:1-15`, `panel.rs:7-13` | `TermUp::Open/Ack/GetRows`, `TermDown::Frame/Rows/Exited/Bell` `crates/femme-proto/src/term.rs:206-258` | Phone: attach only the pane on screen. |
| Input | keys as `Key{code, mods}` (holder encodes), paste (ctrl-shift-v / cmd-v), mouse, resize, focus | `desktop/src/terminal/input.rs:1-30,73-80` | `TermUp::Key/Paste/Mouse/Resize/Focus` | Phone: soft keyboard + an extra-keys row (Esc, Tab, Ctrl, arrows). |
| Open/close panel | mod+j toggles terminal tab, mod+k closes the panel; hover strip on the right edge | `desktop/src/app.rs:111-112,979-982`, `app/right.rs:1-8` | local | |

## 10. Files

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| Tree of the session's folder | `▸/▾`, sizes, dot-files muted; dirs listed when opened; double-click/Enter makes a dir the root; `..` never above the session dir | `desktop/src/files.rs:1-15`, `files/tree.rs:1-10` | `Req::FsList{sid, path}` (relative to session cwd) → `Resp::FsEntries(Vec<FsEntry{name, dir, len}>)` `rpc.rs:166-169,294-299` | Paths built only by a `join` that refuses `..`, `.`, `/` (`tree.rs:5-10`). |
| Keys | Up/Down/PageUp/PageDown/Home/End, Right opens, Left closes/climbs, Enter, Backspace up, F5 refresh | `desktop/src/files.rs:6-10,428-475` | local | |
| Open a file | page with `‹ back`; viewer: images (raster, SVG), PDF pages, text/code with highlighting, markdown, hex dump; picture/video → lightbox | `desktop/src/files.rs:1-5,320-330`, `desktop/src/viewer.rs:1-12` | `Req::FsRead{sid, path}` → `Resp::FsData` | |
| Size rules | > 8 MiB asks first; > `MAX_FRAME − 4096` (just under 16 MiB) not offered | `desktop/src/files/plan.rs:6-35`, `files/source.rs:16` | — | Phone: lower the ask threshold on cellular. |
| Cache | last files kept (48 MiB budget) | `desktop/src/files/cache.rs:10` | local | |

## 11. Lightbox and video

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| Picture | full-window dimmed card `‹ name`, fitted; Esc / `‹` / backdrop click closes | `desktop/src/lightbox.rs:1-10,185-200` | — | Pinch-zoom on phone. |
| Video | play/pause, time, scrub track, sound; space, ←/→ 5 s, `m` mute; ffmpeg decode + pw-play | `desktop/src/lightbox.rs:5-7,189-200`, `desktop/src/video.rs:1-15` | bytes from `FsRead` (transcript/files) or `PrVideo` | Native player. |

## 12. Pull requests (femme ▸ Pull requests)

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| List | number, title, branch, +adds/−dels, author; badges `merging…`, `fixing…`, conflicts, faint "not worked out yet" | `desktop/src/prs.rs:1-5,151-176`, `prs/view.rs` | `Req::Prs` → `Resp::Prs(Vec<Pr>)` `rpc.rs:226-227,267-292` | |
| Detail | title, link, Markdown description | `desktop/src/prs.rs:2-4` | `Pr.body` | |
| Watch demo | when the branch has a demo video | `desktop/src/prs.rs:670-710` | `Req::PrVideo{name}` → `Resp::Blob` `rpc.rs:228-232` | |
| Merge & deploy | always the main button (asked twice); on a conflicting PR the daemon queues the merge behind a Claude fix (`merging`) and merges itself when green; after a merge polls `ServerInfo` every 5 s for ≤ 600 s: `deploying…` → `deployed <sha>` | `desktop/src/prs.rs:5-19,48-52,113-143,184-260,747-760` | `Req::PrMerge{number}` → `Ok` `rpc.rs:233-237`; `ServerInfo` | Confirm sheet (it ships to prod). |
| Close PR | asked twice; closes and deletes the branch | `desktop/src/prs.rs:785-795` | `Req::PrClose{number}` `rpc.rs:238-241` | |
| Fix with Claude / Open fix chat | conflicting PR: starts a `fix #n` chat and the card closes onto it; while it runs, "Open fix chat" | `desktop/src/prs.rs:8-11,122-130,595-610` | `Req::PrFix{number}` → `Resp::Session(sid)` `rpc.rs:242-247`; `Pr.fixing` | |
| Auto-fix toggle | footer "Auto-fix conflicts after a merge"; hidden when the daemon doesn't know it | `desktop/src/prs.rs:14,301-305,545-580` | `Req::PrAutoFix{on: None \| Some(bool)}` → `Resp::PrAutoFix(bool)` `rpc.rs:248-252,351` | |
| Refresh | re-reads every 10 s while unsettled, ≤ 60 times per opening; queued merges watched until they land | `desktop/src/prs.rs:11-13,54-59,177-183` | `Prs` | |
| Keys | Up/Down select, Esc cancels confirm / closes | `desktop/src/prs/view.rs:118-126` | local | |

## 13. Accounts & caps (femme ▸ Accounts)

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| Read | on open and every 10 s: accounts, keys, caps, meta, one at a time; closed card re-reads accounts every 10 min for the menu dot | `desktop/src/accounts/flow.rs:11-35,102-145` | `Req::BoardApi{GET /api/accounts}`, `{GET /api/keys}`, `{GET /api/meta}`, `Req::Caps` | `BoardApi` → `Resp::BoardApi{status, body}` `rpc.rs:181-188,332-336`. |
| Accounts | each cswap account: name (alias/email), in use, disabled, 5 h and 7 d bars `71% · 2d 5h` + per-model weekly (Fable) tag; stale note > 15 min; sign-in line amber < 10 days, red < 2 days / ended | `desktop/src/accounts/model.rs:14-24,39-130,208-267`, `accounts/clock.rs:1-6` | body of `/api/accounts` | |
| Menu red dot | "Accounts" gets a red dot when any sign-in is red | `desktop/src/accounts.rs:236-246`, `menu.rs:367-373,640`, `app.rs:640-642` | — | Phone: badge on the Box tab. |
| Switch | box moves to account N | `desktop/src/accounts/flow.rs:240-246`, `api.rs:48-55` | `BoardApi POST /api/accounts/switch {"account": "<digits>"}` | |
| Add Claude account | `+ add` → sign-in link opened → paste code → "Add account"; Cancel | `desktop/src/accounts/flow.rs:247-295`, `api.rs:57-74` | `POST /api/accounts/claude/login {}`, `POST /api/accounts/claude/code`, `DELETE /api/accounts/claude/login` | |
| Rotation caps | two boxes per account (5 h, 7 d), 50–99 %, empty = box default (greyed); sent 500 ms after the last keystroke, flushed on close | `desktop/src/accounts/model.rs:14-17,485-515`, `flow.rs:17-24,371-440` | `Req::Caps` → `Resp::Caps(Vec<AccountCap>)`; `Req::SetCap{account, five_hour, weekly}` `rpc.rs:195-203,304-311` | |
| AWS identity | account · role, green bar when the CLI has one | `desktop/src/accounts/model.rs:280-290` | `/api/accounts` body | |
| API keys | list `name · …a1b2 · pushes as … · 3d ago`; `+ key` (name `[a-z0-9-]{1,32}`, optional GitHub token) shows the secret once with `copy`; `revoke` asked twice; `connect GitHub` device code (polls keys every 3 s) / cancel | `desktop/src/accounts/model.rs:365-460`, `api.rs:76-120`, `flow.rs:296-370` | `GET/POST /api/keys`, `DELETE /api/keys/{name}`, `POST/DELETE /api/keys/{name}/github` | |
| Scheduler line | "when the box moves on" from `/api/meta` | `desktop/src/accounts/model.rs:464-482`, `accounts/view.rs:1-4` | `GET /api/meta` | |
| Too-old daemon | `Unsupported` → "daemon too old for accounts" | `desktop/src/accounts.rs:9-10`, `model.rs:546-...` | — | |

## 14. Settings, menu, theme

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| femme menu | Check for updates, Pull requests, Accounts, Dark mode, Reconnect, Quit; hover 120 ms opens, click pins; Up/Down/Enter/Esc | `desktop/src/menu.rs:1-20,209-228,542-570` | — | Phone: Box / Me tabs. |
| Dark mode | switch, persisted | `desktop/src/menu.rs:13-15`, `desktop/src/theme/state.rs` | local | Follow the OS by default. |
| Version | build version + commit in the menu | `desktop/src/menu.rs:42-45` | — | |

## 15. Drawing board (desktop only)

| feature | what the user does / sees | desktop code | protocol | notes for the phone |
|---|---|---|---|---|
| Excalidraw-style canvas | rect, ellipse, line, arrow, freehand, text; move/restyle; undo/redo; zoom/pan; saved to `$XDG_STATE_HOME/femme/board.json` | `desktop/src/board.rs:1-40` | local | Out of scope for v1; a sketch → PNG attachment is the parity path. |
| Send to Claude | ctrl-enter: PNG attached to the composer of the shown transcript | `desktop/src/board.rs:5,86`, `app.rs:670-676`, `frame.rs:169-174` | `BlobPut` | |
| Toggle | mod+shift+d, or the strip's button | `desktop/src/app.rs:115,654-...` | local | |

## 16. Keyboard shortcuts that matter (→ phone gestures)

| keys | does | code |
|---|---|---|
| mod+b | collapse/expand sidebar | `desktop/src/app.rs:109` |
| mod+shift+b | move every foreground task of the shown chat to the background | `app.rs:110,976-978` |
| mod+j / mod+k | terminal tab / close right panel | `app.rs:111-112` |
| F6 / shift+F6 | focus cycle | `app.rs:113-114` |
| mod+shift+d | drawing board | `app.rs:115` |
| Enter / Shift+Enter / ctrl-Enter | send (queue or steer) / newline / interrupt then send | `composer/slash.rs:117-131` |
| Esc (composer) | interrupt a working session | `composer.rs:595-628` |
| Shift+Tab (composer) | cycle permission mode | `composer.rs:318-325` |
| Up/Down (composer) | prompt history, or `/` menu navigation | `composer.rs:330-376,560-588` |
| Esc (transcript) | back from subagent view | `transcript/live.rs:659-663` |
| ctrl-tab / ctrl-shift-tab | terminal tabs | `terminal/panel.rs:76-77` |

## 17. Protocol coverage summary

Used by the desktop: `Subscribe/Unsubscribe` (via client), `TranscriptPage`, `BlobGet`, `BlobPut`,
`SessionCreate`, `SessionClose`, `SessionLabel`, `Prompt`, `Interrupt`, `Background`, `AnswerQuestion`,
`Resend`, `CyclePermission`, `Commands`, `PaneOpen`, `PaneClose`, `FsList`, `FsRead`, `Folders`,
`BoardApi`, `Caps`, `SetCap`, `ServerInfo`, `Prs`, `PrVideo`, `PrMerge`, `PrClose`, `PrFix`,
`PrAutoFix` (grep of `Req::` in `desktop/src`).

Served by femmed but not used by the desktop: `Takeover`, `SetModel`, `IssueCreate`, `IssuePatch`,
`IssueDelete`, `Show`, `SessionOf`, `Upload` (Unsupported) — `crates/femmed/src/front/req.rs:144-196`.
