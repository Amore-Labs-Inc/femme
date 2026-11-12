# FEMME transcript bodies: what a phone has to read

Everything here is read off the code on branch `mobile-app`; every claim cites `file:line`
(repo-relative). Where the brief's assumptions do not match the code, it says so under
**Surprises** at the end. Wire framing, subscriptions and the screens that use these items are in
`mobile/PROTOCOL.md`; this file is only about the `Item` rows and their JSON `body`.

---

## 1. The envelope: `Item`

`crates/femme-proto/src/state.rs:166-185`

| field | type | meaning |
|---|---|---|
| `sid` | ULID (u128 on the wire) | session |
| `item_id` | string | unique per session; agent id when it has one, else a ULID or `o<offset>` (ids table in §1.3) |
| `seq` | u64 | order within the session (the list is sorted by it; `TranscriptPage` pages by it) |
| `rev` | u64 | daemon-wide monotonic; a row replaces the held one only if its `rev` is higher (`desktop/src/transcript/items.rs:263-268`) |
| `kind` | `ItemKind` | `UserMessage, PeerMessage, AssistantMessage, Thinking, ToolCall, ToolResult, Question, Plan, Event, TurnStart, TurnEnd` (`state.rs:133-153`); no others exist |
| `status` | `ItemStatus` | `InProgress, Completed, Failed, Pending, Answered` (`state.rs:156-163`) |
| `preview` | string ≤ 512 B | list label (§1.2) |
| `body` | `Option<String>` | JSON text, inline when ≤ 4 KiB, else `None` (§1.1) |
| `blob_hash` | `Option<[u8;32]>` | SHA-256 of the body stored as a blob when `body` is `None` |
| `ts` | i64 unix ms | creation time; a re-put of a final item keeps the first `ts` (`crates/femmed/src/headless.rs:2005`) |
| `parent` | `Option<String>` | set on a subagent's items: the id of the `Agent` tool call that spawned it (`state.rs:182-184`) |

Deletion is `Row::Tombstone{entity: Item, id: "<sid>/<item_id>", rev}` (`state.rs:250-255`;
the desktop splits on the first `/`, `desktop/src/transcript/items.rs:304-311`).

**JSON key order.** Bodies are built with `serde_json::to_value` (`crates/femme-agents/src/items.rs:14-16`)
without the `preserve_order` feature (`Cargo.toml:23`), so object keys come out **sorted
alphabetically**. The desktop relies on that to salvage truncated bodies
(`desktop/src/transcript/body.rs:170-171`, `:190-191`, `:396`). Daemon-made bodies built with
`json!` are sorted the same way.

### 1.1 `body` vs `blob_hash` (the 4 KiB rule)

Enforced by the store at write time, `crates/femme-store/src/items.rs:420-458` (constants `:48-60`, table `:8-12`):

| body size | `items.body` column | client sees |
|---|---|---|
| ≤ 512 B | raw UTF-8 | `body: Some(json)` |
| ≤ 4096 B | zstd (or raw if not smaller) | `body: Some(json)` |
| > 4096 B | NULL, text in `blobs` | `body: None, blob_hash: Some(sha256)` |

- Blobs are capped at 1 MiB (`BLOB_MAX`, `items.rs:53`); a longer body is cut on a char boundary and
  ends with `…[truncated]` (`TRUNCATED`, `items.rs:59`), which is **not valid JSON** (`items.rs:17-21`).
  The desktop then extracts single string fields by hand (`string_field`/`unescape`,
  `desktop/src/transcript/body.rs:234-296`); `ToolResult.link` survives a cut because `link` sorts
  before `output` (`body.rs:190-196`); a `ToolCall` cut inside `input` loses `name` (sorted after
  `input`) and the desktop falls back to `preview` (`body.rs:170-176`, `turns.rs:216-229`).
- Fetching: `Req::BlobGet{hash}` → `Resp::Blob(bytes)` (`crates/femme-proto/src/rpc.rs:94-96`,
  `crates/femmed/src/front/req.rs:104-112`); `NotFound` when retention evicted it
  (`crates/femme-store/src/items.rs:33-36`: "show the preview"). Retention evictions do not bump `rev`.
- Desktop policy: for every kind except `ToolCall`/`ToolResult` the blob is fetched as soon as the
  row is drawn (`desktop/src/transcript/rows.rs:288-295`); tool input/output blobs only when the tool
  row is opened (`rows.rs:733-741`). Fetch once, remember failures (`live.rs:553-576`).
  `Items::json` returns inline body or the fetched blob (`items.rs:371-376`); while a blob is missing
  the row shows the streamed text it had, else `preview` (`items.rs:386-403`).
- An `ItemUpdated` (in-progress body change) rewrites the body and clears `blob_hash`
  (`crates/femmed/src/headless.rs:2020-2031`).

### 1.2 `preview`

- Daemon formula `preview_of` (`crates/femmed/src/headless.rs:2036-2064`):
  text kinds (`UserMessage, PeerMessage, AssistantMessage, Thinking, Plan, Event`) → body `text`;
  `ToolCall` → first line of `"<name> <hint>"` where hint = the first string among
  `input.command, file_path, path, pattern, url, query, description`; `ToolResult` → first line of
  `output`; `Question` → `prompt`; `TurnStart`/`TurnEnd` → `""`.
- While an item is open, `preview` is what the driver gave at `ItemStarted`
  (`crates/femmed/src/headless.rs:1839-1849`): the tool name for Claude/Pi tool calls
  (`crates/femme-agents/src/claude.rs:767-773`), Codex's command/paths/query
  (`crates/femme-agents/src/codex.rs:828-835`), `""` for streamed text.
- Store caps it to 512 B; if a caller gives `""`, the store uses the first 512 B of the **raw body
  JSON** (`crates/femme-store/src/items.rs:421-428`).
- Exceptions: a saved prompt's preview is its text (`crates/femmed/src/sent.rs:77`); a `femmed show`
  item's preview is the caption's first line or `shows <file>` (`crates/femmed/src/uploads.rs:99-105`).

### 1.3 Item ids (what links to what)

| item | id | source |
|---|---|---|
| Claude assistant text / thinking block | `<api message id>:<block index>` (e.g. `msg_011Cgdv…:0`), `o<offset>` if no id | `claude.rs:738-741`, `:950-952` |
| Claude tool call | `tool_use.id` (`toolu_…`) | `claude.rs:742-748` |
| tool result | `<call id>:result` (all drivers) | `claude.rs:1095`, `codex.rs:898`, `pi.rs:820`; desktop `turns.rs:157-161` |
| Claude user echo | the line's `uuid` (= the uuid FEMME wrote the prompt under) | `claude.rs:1026`, `:2187-2191`, `:444-449` |
| saved prompt (client-sent) | a fresh ULID | `crates/femmed/src/sent.rs:73` |
| AskUserQuestion question | the tool_use id; `<tool_use_id>#<i>` when it asks several | `claude.rs:2355-2359` |
| permission / plan approval | `<tool_use_id>:ask` | `claude.rs:1522-1524` |
| MCP elicitation | `<control request_id>:elicit` | `claude.rs:1404` |
| TUI-pane screen question | `herdr:<pane>:<unix ms>` | `crates/femmed/src/headless/herdr.rs:55`, `:377` |
| Plan (Claude) | the ExitPlanMode tool_use id | `claude.rs:872-880` |
| Codex items | Codex's ids; plan checklist `<turnId>:plan` | `codex.rs:31-36`, `:1077-1086` |
| subagent offset ids | `<parent>/o<offset>` | `claude.rs:2045-2055` |
| daemon events | `x<ULID>` (`headless.rs:1198`), `<base>-<n>` incl. every `TurnEnd` (`headless.rs:1796-1830`), `unknown:<kind>` (`herdr.rs:766-771`), `btw-<ULID>` / `btw-<ULID>-a` (`headless/btw.rs:37-86`) | |

---

## 2. Streaming: `S2C::Delta`

`S2C::Delta{sid, item_id, offset: u32, text}` (`crates/femme-proto/src/rpc.rs:393-399`).

- **Only text appends.** Drivers emit `ItemDelta` for Claude `text_delta` and `thinking_delta`
  (`claude.rs:794-823`), Pi `text_delta`/`thinking_delta` (`pi.rs:502`), Codex
  `item/agentMessage/delta`, `item/reasoning/summaryTextDelta`, `item/plan/delta`
  (`codex.rs:1024`). Tool input JSON never streams (`claude.rs:825-827` only buffers it); a tool
  call's input arrives whole in an `ItemUpdated` row.
- **`offset` is a UTF-8 byte offset** into the item's accumulated text: the daemon keeps the text per
  open item and sends `acc.len()` before appending (`crates/femmed/src/headless.rs:1851-1856`).
- The hub batches deltas 16 ms per item and sends the **offset of the first piece with the
  concatenated text** (`crates/femmed/src/hub.rs:368-389`); clients whose subscription is lagged get
  **no** deltas (`hub.rs:400-402`), the final row carries everything.
- After a daemon restart the whole open text is re-sent once at offset 0
  (`crates/femmed/src/headless.rs:924-931`). A client that subscribes mid-item only sees later
  offsets.
- Desktop merge (`desktop/src/transcript/items.rs:31-128`, applied at `:324-358`):
  - bytes already held win (a repeat is harmless); text at an offset past the held head is kept as a
    tail segment and the gap is drawn as `…` (`:28`, `:106-120`);
  - an offset inside a UTF-8 char is ignored (`:51-53`);
  - deltas for an item whose row is final (`Completed/Failed/Answered`) are ignored (`:326-330`);
  - deltas may precede their row; up to 64 such orphan streams are kept (`:25`, `:330-339`);
  - per item at most 8 MiB is kept (`MAX_STREAM`, `:21`);
  - when the final row arrives the stream is dropped; if the final row has no inline body (blob),
    the streamed text is kept for display until the blob is fetched (`:274-287`).
- Display: the text of a row is body `text` if parsed, else stream, else kept text, else `preview`
  (`items.rs:386-403`); `InProgress` rows get a streaming marker (`rows.rs:317-325`, `:532`).

Typical sequence (fixture `crates/femme-agents/fixtures/claude/tool.events.json`, as driver events):

```json
{"ItemStarted": {"id": "msg_011CgdvEAggEcb7NCwh5GvLe:0", "kind": "AssistantMessage", "preview": ""}}
{"ItemDelta":   {"id": "msg_011CgdvEAggEcb7NCwh5GvLe:0", "text": "done"}}
{"ItemCompleted": {"id": "msg_011CgdvEAggEcb7NCwh5GvLe:0", "kind": "AssistantMessage", "status": "Completed", "body": {"text": "done"}}}
```

On the wire that is: a `Row::Item` (`InProgress`, `body: None`), `Delta{offset:0,text:"done"}`, then a
`Row::Item` (`Completed`, `body: "{\"text\":\"done\"}"`). `ItemStarted` creates the row with no
body (`headless.rs:1839-1849`); `ItemUpdated` sets the whole body without finishing
(`headless.rs:2020-2031`, skipped once the item is final); `ItemCompleted` writes the final row
(`headless.rs:1857-1870`).

---

## 3. Bodies per `ItemKind`

Structs: `crates/femme-agents/src/items.rs`. Readers on the desktop: `desktop/src/transcript/body.rs:130-147`.
Every text kind is read the same way: `text` field, or a bare JSON string as its own text
(`body.rs:154-161`).

### 3.1 `UserMessage`

| field | type | when present | source |
|---|---|---|---|
| `text` | string | always | `items.rs:135-138` |
| `op` | string (OpId as ULID text) | on a row the daemon saved from a client `Req::Prompt` | `crates/femmed/src/sent.rs:60-64`, `req.rs:123-133` |
| `echo` | string (uuid) | saved row of a FEMME-owned headless **Claude** chat: the uuid the CLI will echo it under | `sent.rs:44-49`, `:65-67` |
| `read_at` | i64 unix ms | added when the agent's echo was paired with the saved row | `sent.rs:248`, `:309` |

Statuses (the Item's `status`, not body; `state.rs:134-140`):

- `Pending`: saved before the agent read it (`sent.rs:75`). Desktop shows `queued` (`rows.rs:364`).
- `Completed` + `read_at`: the agent read it; desktop shows `read HH:MM` (`rows.rs:365-369`). The echo
  itself becomes an `Event` (§3.9 `read`).
- `Failed`: the agent did not get it; desktop shows `<Agent> didn't get this · resend`
  → `Req::Resend{sid, item_id}` (`rows.rs:370-388`, `:407-419`, `rpc.rs:253-259`). A late echo still
  pairs with a `Failed` row and heals it (`sent.rs:18-23`, `:171-173`).
- `Completed` with no `op`/`read_at`: an echo the daemon did not save first (typed in the agent's
  own terminal, a subagent prompt, other agents' history) (`state.rs:138-139`).

Saved only for headless sessions, non-empty text not starting with `/` (`sent.rs:51-56`). A `/`
command is not saved; its output comes back as an `Event` (§3.9).

Desktop rendering: `Attached:` lines are removed from the text and shown as chips/thumbnails (§6,
`rows.rs:327-357`). A user message starting with `This session is being continued from a previous
conversation` is Claude's compaction hand-off: folded as `· conversation compacted · summary`
(`rows.rs:53-56`, `:614-650`), and the `Event` just before it is hidden (`rows.rs:299`).
`/btw <q>` creates its own `UserMessage` `btw-<ULID>` with text `/btw <q>` (`headless/btw.rs:37-46`).

The composer shows a faded local bubble until a row with the request's `op` arrives
(`desktop/src/transcript/outbox.rs:1-6`).

```json
{"echo":"5f0c…-uuid","op":"01JC9W6K7Q2R3S4T5V6W7X8Y9Z","read_at":1793617201234,"text":"fix the build\n\nAttached: /home/u/.local/state/femme/uploads/01JC…/err.png"}
```

### 3.2 `PeerMessage`

Another Claude Code session's `SendMessage` (`state.rs:141-142`).

| field | type | when | source |
|---|---|---|---|
| `from` | string | always (`""` if unknown): sender's session name | `items.rs:143-148`, `claude.rs:2208-2224` |
| `text` | string | always, without the `<cross-session-message>` framing | same |

Produced when a `user` line has `origin.kind == "peer"` (`claude.rs:1047-1055`); TUI mirror too
(`crates/femmed/src/headless/herdr.rs:1979`). Desktop: left card headed `✉ from @<from>`
(`rows.rs:467-501`, `body.rs:149-152`).

```json
{"from":"pr-fix-43","text":"merged, you can rebase now"}
```

### 3.3 `AssistantMessage`

`{"text": string}` (`items.rs:19-23`). Markdown. Streams (§2). Built at block stop
(`claude.rs:838-847`) or from a non-streamed `assistant` line (`claude.rs:975-983`). `Failed` when the
stream was cut (`claude.rs:910-931`).

Also produced by the daemon for `femmed show` (`Req::Show`): text = caption, blank line,
`Attached: <abs path>` (`crates/femmed/src/uploads.rs:65-121`, `with_lines` `:203-214`), and for a
`/btw` answer (`headless/btw.rs:78-86`). Desktop renders markdown plus `Attached:` files as large
pictures (`rows.rs:503-534`, `thumb.rs:155-163`).

```json
{"text":"You chose **B**."}
```
(`crates/femme-agents/fixtures/claude/question.events.json`)

### 3.4 `Thinking`

`{"text": string}` (`items.rs:26-30`). Claude: only when the CLI sends readable thinking text, which
it normally does not (`claude.rs:41-42`, `:801-822`). Codex: reasoning summary (or content), parts
joined by blank lines (`codex.rs:1442-1452`, `:927-932`). Desktop: collapsed `▸ thinking <first line>`,
opens on click (`rows.rs:555-612`); counts into a turn's fold line (`turns.rs:332`).

### 3.5 `ToolCall`

| field | type | when | source |
|---|---|---|---|
| `call_id` | string | always (= the item id) | `items.rs:40-41` |
| `input` | JSON | `null` until the streamed input is complete or when the stream died; `{}` for no args | `items.rs:38-39`, `claude.rs:858`, `:922-928` |
| `name` | string | always | `items.rs:36-37` |
| `push` | string | only on a Claude `PushNotification` call femmed sent to the phone itself: `"sent"`, or the reason it did not arrive | `femmed/src/headless.rs` `stamp_pushes` |
| `task` | `"foreground"` \| `"background"` | omitted unless the agent runs the call as a task (Claude Bash / subagent) | `items.rs:42-57` |

Lifecycle (Claude): `ItemStarted` (preview = name, no body) → `ItemUpdated` with full body once the
input is final (`claude.rs:869-901`) → possibly `ItemUpdated` again when `task` changes
(`claude.rs:330-360`) → `ItemCompleted` `Completed`/`Failed` with the result (`claude.rs:1101-1132`),
or `Failed` when the turn ended or the process exited first (`claude.rs:1260-1284`).
Status `Failed` mirrors the result's `is_error` (`claude.rs:1085-1089`).

Real example (`fixtures/claude/tool.events.json`):
```json
{"call_id":"toolu_01JBjzExyVfbT78Xq8F96Eoa","input":{"command":"echo hi","description":"Print hi"},"name":"Bash"}
```
With a task (`claude.rs:3100-3138`):
```json
{"call_id":"toolu_1","input":{"command":"sleep 45"},"name":"Bash","task":"foreground"}
```

`Req::Background{sid, tool_use_id}` moves a foreground task to the background (`rpc.rs:120-127`);
the desktop offers it while the call is running and `task == "foreground"` (`turns.rs:250-251`). The
task's end is an `Event` with `task.call` (§3.9).

### 3.6 `ToolResult`

| field | type | when | source |
|---|---|---|---|
| `call_id` | string | always: the call's id; the item id is `<call_id>:result` | `items.rs:59-63` |
| `is_error` | bool | always | `items.rs:66-67` |
| `link` | `{url, title, version?, updated}` | only for a Claude `Artifact` call whose result names a claude.ai page | `items.rs:68-86`, `claude.rs:2060-2097` |
| `output` | string | always | `items.rs:64-65` |

- **Linking:** by id only: result id = `<call item id>:result` (`turns.rs:156-161`; layout merges the
  result into the call's row, `turns.rs:278-293`). There is no `parent` linking for results; `parent`
  is only the subagent marker. A result whose call is not held is a row of its own
  (`turns.rs:278-293`, `:457-458`; drawn by `rows.rs:879-912`).
- **Images:** never carried. Claude content blocks are flattened: text joined by `\n`, an image block
  becomes `[image]`, any other block `[<type>]` (`claude.rs:2248-2266`). Codex: `[<type>: <name|path>]`
  or `[<type>]` (`codex.rs:1592-1611`), command exit code appended as `[exit code N]` (`codex.rs:1495-1503`).
- `ItemStatus` of the result: `Failed` iff `is_error` (`claude.rs:1085-1089`).
- `link.version` is the publish `seq`, `updated` true for a re-publish (`items.rs:74-86`).

```json
{"call_id":"toolu_01JBjzExyVfbT78Xq8F96Eoa","is_error":false,"output":"hi"}
{"call_id":"t1","is_error":false,"link":{"title":"FEMME artifact test","updated":false,"url":"https://claude.ai/artifact/AbC123","version":1},"output":"Published /tmp/x/page.html at https://claude.ai/artifact/AbC123 (Version 1, version id 1-1)"}
```
(`fixtures/claude/tool.events.json`; `claude.rs:2724-2748`)

### 3.7 `Question`

Body = `femme_proto::state::Question` flattened + answer (`items.rs:88-103`, `state.rs:187-196`):

| field | type | when | source |
|---|---|---|---|
| `request_id` | string | always: the id to send in `Req::AnswerQuestion` | `state.rs:190` |
| `prompt` | string | always | `state.rs:191` |
| `options` | string[] | always (may be empty) | `state.rs:192` |
| `multi` | bool | always | `state.rs:193` |
| `free_text` | bool | always: a typed answer is allowed | `state.rs:194-195` |
| `answered` | `Answer` \| `null` | always present; `null` while open or when it failed | `items.rs:98-99` |
| `reason` | string | only on a question that ended `Failed` without an answer | `items.rs:100-102` |

`Answer` in JSON (serde external tagging, `rpc.rs:57-63`): `{"Option":1}`, `{"Options":[0,1]}`,
`{"Text":"..."}`, `"Cancel"`. Example from `items.rs:180-183`:
```json
{"answered":{"Option":1},"free_text":true,"multi":false,"options":["A","B"],"prompt":"A or B?","request_id":"r1"}
```

Statuses: `Pending` (open, daemon-created from `QuestionAsked`, `headless.rs:1872-1889`) →
`Answered` (with `answered`, also for a deliberate Cancel/Deny/"Keep planning") or `Failed` (with
`reason`: `the turn ended before it was answered`, `claude exited before it was answered`,
`withdrawn by Claude`, `answered in the pane`, or the CLI's own error text) (`claude.rs:59-63`,
`:1167-1199`, `:1224`, `:2015`, `:1686`; `herdr.rs:345`). A question item is never tagged with a
subagent `parent` (`claude.rs:381-383`, `:403`).

**Answering:** `Req::AnswerQuestion{sid, request_id, answer}` (`rpc.rs:128-132`; desktop
`desktop/src/transcript/ask.rs:94-131`). Desktop UI (`rows.rs:939-1072`): open iff
`status == Pending && answered == null && reason == null` (`rows.rs:949-965`, `question.rs:57-59`);
options are numbered buttons; single choice sends `Option(i)` on click; `multi` toggles and a `send`
button sends `Options([...])` in option order (`question.rs:63-72`); `free_text` adds a text field
that sends `Text(s)`; `cancel` always sends `Cancel`. The row shows `sending…` until the row changes.
Closed: `→ <answer>` (labels joined by `, `, or the text, or `cancelled`) or
`not answered: <reason>` (`rows.rs:1074-1080`, `question.rs:39-53`).

The answer validity rules (driver rejects with `Bad`): Claude `check_answer` `claude.rs:2397-2421`
(index in range; `Options` non-empty and only for `multi` unless one pick; elicitations take no text);
Codex `codex.rs:1395-1411`; Pi `pi.rs:1135-1150`.

#### Where questions come from, per driver

| source | item id / `request_id` | `prompt` | `options` | `multi` | `free_text` |
|---|---|---|---|---|---|
| Claude `AskUserQuestion` (main thread only) | item = tool_use id (`#i` when several); request = CLI control `request_id` (`#i`) | `questions[i].question` | `"<label> — <description>"` per option (em dash, description omitted if empty) | `questions[i].multiSelect` | `true` | 
| Claude `ExitPlanMode` approval (plan mode) | item `<tool_use_id>:ask` | `Approve this plan and start coding?` | `["Approve","Keep planning"]` | false | `true` |
| Claude permission (`can_use_tool` for any other tool) | item `<tool_use_id>:ask`, request = control id | `Allow <title\|description\|display_name\|tool>?` (kept as is if it already ends in `?`) | `["Allow","Deny"]` | false | `true` |
| Claude MCP elicitation | `<request_id>:elicit` / control id | `<server> asks: <message>` + URL line or `Accept leaves the form's fields empty: a, b` | `["Accept","Decline"]` | false | `false` |
| TUI pane (herdr mirror) dialog read from the screen | `herdr:<pane>:<ms>` | screen text | screen options | false | `false` |
| Codex `requestUserInput` | `<itemId>` (`#i`) | `question` or `header` | option labels | false | `isOther` or no options |
| Codex approval (normally off) | `<itemId>:<approvalId>` | ``Allow Codex to run `cmd`?`` (+ reason line) | `["Accept","Decline"]` | false | `false` |
| Pi extension `select` / `confirm` / `input` / `editor` | pi request id | title (`confirm`/`editor`: title + message/prefill) | select options / `["Yes","No"]` / none | false | `input`, `editor` only |

Sources: `claude.rs:2348-2395` (ask_parts), `:1482-1540` (ask), `:2099-2106` (allow_prompt),
`:1365-1373` (can_use_tool title), `:1379-1427` (elicitation); `herdr.rs:370-399`;
`codex.rs:664-731`; `pi.rs:844-905`.

#### Permission prompts: Answer → what the agent gets

Permission requests are `Question` items (no separate kind). `response_body` (`claude.rs:2423-2468`):

| question | `Option(0)` | `Option(1)` | `Text(t)` | `Cancel` |
|---|---|---|---|---|
| Allow / Deny (`PendKind::Allow`) | allow (input unchanged) | deny "The user said no." | **deny with message `t`** (feedback to Claude) | deny "The user said no." |
| Approve / Keep planning | approve | keep planning (deny) | deny with `t` | deny |
| AskUserQuestion | answers map `question → label` (`Options` → labels joined `, `, `Text` → the text) then allow | | | deny "The user declined to answer." (cancels the whole request, even with several parts) |
| Elicitation | `{"action":"accept"}` (form: schema defaults only) | `{"action":"decline"}` | rejected | `{"action":"cancel"}` |
| Codex approval | `accept` | `decline` | rejected | `cancel` (`codex.rs:1299-1305`) |

There is **no "Always allow"** option anywhere: the brief's Allow/Deny/Always does not exist. A
permanent change is a permission *mode* (`Req::CyclePermission`, `rpc.rs:209-212`; mode shown in
`Session.permission`, `state.rs:69-71`). Under `bypassPermissions` the CLI only asks for
AskUserQuestion and ExitPlanMode (`claude.rs:13-14`), so Allow/Deny items appear only after a plan
approval returned the session to `default` (`claude.rs:27-29`).

A multi-question AskUserQuestion is answered part by part; the daemon marks each part `Answered` as it
comes because the driver writes nothing until all parts are in (`crates/femmed/src/headless.rs:711-727`,
`:2209-2224`).

### 3.8 `Plan`

`{"text": string}` markdown (`items.rs:105-110`).

- Claude: one `Plan` item per main-thread `ExitPlanMode` call, id = tool_use id, text = `input.plan`,
  `Completed` at once (`claude.rs:872-880`). No `ToolCall`/`ToolResult` rows exist for it (hidden,
  `claude.rs:2108-2110`, `:1079-1083`). In plan mode the approval is a separate `Question`
  `<tool_use_id>:ask` (§3.7). Real sequence (`fixtures/claude/plan.events.json`):
  ```json
  {"ItemCompleted":{"id":"toolu_016h8PFN6VQ4cbAyRzfLGaYg","kind":"Plan","status":"Completed","body":{"text":"create hello.txt\n"}}}
  {"QuestionAsked":{"item_id":"toolu_016h8PFN6VQ4cbAyRzfLGaYg:ask","q":{"request_id":"deab25a3-3557-44dc-91fe-d31d30be358c","prompt":"Approve this plan and start coding?","options":["Approve","Keep planning"],"multi":false,"free_text":true}}}
  ```
  Then `Option(0)` → `{"answered":{"Option":0},…}` `Answered`, and the session's `permission` changes.
  `EnterPlanMode` is an ordinary `ToolCall`.
- Codex: its `plan` item (streams via `item/plan/delta`), and `turn/plan/updated` as a markdown
  checklist `- [x] done`, `- [ ] **in progress**`, `- [ ] todo` under `<turnId>:plan`
  (`codex.rs:1614-1630`, `:1077-1086`). No approval.
- Desktop: a ruled box titled `plan` with the markdown (`rows.rs:536-553`). Approval is the
  question row, not the plan row.

### 3.9 `Event`

| field | type | when | source |
|---|---|---|---|
| `text` | string | always | `items.rs:114-120` |
| `task` | `{call, status}` | a background task stopped (Claude `task_notification`) | `items.rs:117-131`, `claude.rs:2169-2185` |
| `read` | string (item_id) | the echo of a saved prompt: names the `UserMessage` row it read | `crates/femmed/src/sent.rs:253-266`, `:315-319` |
| `warning` | bool (absent = false) | the line is a warning: Claude answers with a fallback model, or a refusal had none | `items.rs` `Event`, `claude.rs` `fallback` |
| `detail` | string | the agent's own longer wording of a warning (Claude's banner, with its link) | same |

Status is `Completed`, except Codex `error` notices that will not retry: `Failed` (`codex.rs:474-487`,
`:1089-1099`).

Event texts that exist (exhaustive for what the code builds):

| text | from | source |
|---|---|---|
| `Conversation compacted` | Claude `compact_boundary` (also bumps `Session.context.compactions`) | `claude.rs:595-603`; `headless.rs:1946-1949` |
| `Context compacted` / `Review started: …` / `Review finished: …` / hook prompt text / `Sub-agent <path> <kind>` | Codex items | `codex.rs:1454-1469` |
| `Context compacted (<reason>), N tokens before`, `Compaction aborted`, `Compaction failed: …`, `Retries gave up: …`, pi `notify` (`<type>: <msg>`) | Pi | `pi.rs:1040-1060`, `:885-891` |
| `<summary>: <status>` + `task` | Claude background task ended | `claude.rs:2169-2185`, `:589-594` |
| `◷ <description> · <event>` (a longer event's other lines follow the first) | Claude Monitor event: a `<task-notification>` user line without `<status>` (one with a status is no item, the row above is its row) | `claude.rs` `user()`, `monitor_event`; one that starts a turn of its own is not in the stream and is read from the transcript on disk, `headless.rs` `tail_monitor` |
| `[Request interrupted by user]` and other non-replayed user-line text | Claude markers | `claude.rs:1064-1067` |
| output of a `/` command (`<local-command-stdout>`, ANSI stripped), e.g. ``Set model to `x` ``, `Goal set: …` | Claude | `claude.rs:1019-1045` |
| `◎ goal not met yet · <reason>` | Claude `/goal` Stop-hook verdict | `claude.rs:1044-1046` |
| `<original> → <fallback> · <reason>` + `warning`, `detail` (`fable 5.2 → opus 4.8 · safeguards flagged this message (cyber)`; reason also `usage-credit gate` or the trigger, `overloaded`); the first line of Claude's text when a refusal has no fallback | Claude `model_refusal_fallback`, `model_consent_fallback`, `model_fallback`, `model_refusal_no_fallback` (2.1.321). The items of the refused reply are deleted (item tombstones); a session-scoped switch sets `Session.model` | `claude.rs` `fallback` |
| `claude failed before it ran a turn: <why>` | Claude | `claude.rs:1234-1247` |
| `Claude asked something FEMME cannot answer: <subtype>` (once per subtype) | Claude unknown control request | `claude.rs:1623-1646` |
| `claude rejected a control request: <why>` | Claude | `claude.rs:1704-1711` |
| `A message was not run (<why>): <text ≤300>` | any driver `PromptDropped` | `headless.rs:1892-1915` |
| `Claude read your message` / `Codex …` / `pi …` + `read` | daemon pairing | `sent.rs:252-266` |
| `Model: <model>` | daemon, provider switch | `headless.rs:1306-1312` |
| `<Agent> exited (<status>); starting it again in 2 s.` / `… again right after a restart: not started again.` / `<Agent> did not start again: …` | daemon | `headless.rs:1316-1330`, `:1349-1356` |
| `Claude Code wrote (N) entr(y/ies) FEMME does not read yet: <kind>` | TUI mirror | `herdr.rs:755-772` |
| `/btw failed: …`, `/btw gave up after 5 minutes` | daemon | `headless/btw.rs:68-90` |

Not events (state lives on the `Session` row instead): model changes (`Session.model`), permission
mode (`Session.permission`), context size (`Session.context`), goal state incl. `Met` and `Cleared`
(`Session.goal`), rate limits (only the 5-hour reading is kept, as a usage snapshot)
(`headless.rs:1916-1945`, `state.rs:63-79`).

Desktop rendering: an `Event` with `read` → a thin `↳ <first line>` that scrolls to the message,
hidden when the message is the row right above (`rows.rs:422-465`); otherwise a muted mono line
prefixed `· ` unless it starts with `◎` or `◷` (`rows.rs` `note_row`, `note_text`); a `◷` note of
several lines shows its first line with ` …` and unfolds on a click. `task.call` ends an agent
chip and clears a tool row's `in_background` (`agents.rs:43-46`, `:108-118`; `turns.rs:241-252`).
A `warning` → `⚠ <text>` in `warn_text` (amber), which unfolds to `detail` as markdown
(`rows.rs` `warning_row`); on the phone the same amber line, `detail` in what a long press copies
(unfolding it there is a follow-up).

```json
{"read":"01JC9W6K7Q2R3S4T5V6W7X8Y9Z","text":"Claude read your message"}
{"task":{"call":"toolu_1","status":"completed"},"text":"Sleep: completed"}
```

### 3.10 `TurnStart`

Exists in the enum but **no producer emits it** (no `ItemKind::TurnStart` construction in
`crates/femme-agents` or `crates/femmed`; `AgentEvent::TurnStarted` only sets the session status,
`headless.rs:1813-1817`). Readers treat it as empty (`body.rs:145`, `headless.rs:2063`); the
desktop layout would use it as a turn boundary if present (`turns.rs:309`).

### 3.11 `TurnEnd`

Made by the daemon from `AgentEvent::TurnEnded`, id `<base>-<n>`, `Completed`
(`headless.rs:1818-1837`; struct `items.rs:150-159`).

| field | type | when | source |
|---|---|---|---|
| `duration_ms` | u64 \| null | agent's own measure | `items.rs:157-158` |
| `stop` | `"EndTurn"` \| `"MaxTokens"` \| `"Interrupted"` \| `{"Error":msg}` \| `{"Other":msg}` | always | `crates/femme-agents/src/lib.rs:219-231` |
| `usage` | `{input, output, cache_read, cache_write}` \| null | null when unknown (all-zero counts) | `state.rs:198-204`, `claude.rs:2269-2280` |

```json
{"duration_ms":3255,"stop":"EndTurn","usage":{"cache_read":61032,"cache_write":20729,"input":4,"output":201}}
```
Desktop (`turns.rs:163-184`, `rows.rs:664-683`): `EndTurn` has no row; the turn's thinking/tool work
before it folds into one line `worked for 3m 12s · 12 tools · 1 failed` / `thought for 5s`
(`turns.rs:51-76`, durations `:78-93`; duration from `duration_ms`, else `ts` difference `:340-346`).
Other stops show `· interrupted`, `· stopped: output limit reached`, `· turn failed: <m>` (red),
`· turn ended: <m>`. Unknown JSON reads as `EndTurn` (`body.rs:210-230`).

---

## 4. Tool calls: names, inputs, one-line summary

### 4.1 The desktop's one-line summary (`turns.rs:216-269`, `body.rs:11-62`)

A tool row reads: **`<name>`** (bold, red if failed) `<hint>` (muted, truncated) `→ <result>`.

- `hint` = first line of the first **string** among `input.command, input.file_path, input.path,
  input.pattern, input.url, input.query, input.description`, in that order (`body.rs:12-20`, `:55-62`).
  Then its first non-empty trimmed line (`turns.rs:208-214`, `:220`).
- If the body is not at hand (in progress without body, blob not fetched, cut), name and hint come
  from `preview` split at the first whitespace (`turns.rs:221-228`).
- `result` = first non-empty line of the result's `output`; `None` until the result exists
  (`turns.rs:230-237`).
- `failed` = result `is_error` or the call or result row `Failed` (`turns.rs:238-240`).
- `running` = no result and call `InProgress` (`turns.rs:241`); `backgroundable` = running and
  `task == "foreground"`; `in_background` = `task == "background"` and no `Event` with `task.call`
  equal to this call yet (`turns.rs:241-253`).
- `Artifact` result with a `link` (and not failed): name `artifact`, hint `· <title> ↗` (a link to
  `url`), result `updated · v<version>` only for an update with a version (`turns.rs:256-266`).
- `PushNotification` call (Claude's push to the phone): name `🔔 push`, hint `· <first line of
  input.message>`. femmed sends the message through ntfy itself (Claude Code only delivers through
  Remote Control) and writes the outcome on the call's body as `push`: `"sent"` → result
  `sent to your phone` (and that is the opened row's output too), any other string is the reason →
  result `not sent: <reason>`; no `push` (femmed left the call alone) → Claude Code's own answer.
- Opened row: input shown as the bare command if the input is exactly `{command}`, else pretty JSON
  (empty object → nothing) (`body.rs:64-77`); output shown; both capped at 400 lines / 24 000 bytes
  (`rows.rs:40-44`, `:93-111`).
- In the turn still running, each run of consecutive tool calls (except the last one) folds into a
  sentence (`turns.rs:99-132`, `:359-418`): `Bash|shell|exec_command` → `ran N command(s)`, `Read` →
  `read N file(s)`, `Grep|Glob|WebSearch` → `searched N time(s)`,
  `Edit|Write|MultiEdit|NotebookEdit|apply_patch` → `edited N file(s)`, `WebFetch` →
  `fetched N page(s)`, anything else → `N <name> call(s)`; joined by `, `, first letter upper-cased.
  A finished turn folds all its work into the `worked for …` line (§3.11).

### 4.2 Per tool (Claude Code names)

The repo does not define Claude Code's tool input schemas; the drivers pass `input` through as the CLI
sent it (`claude.rs:944-972`). Fields marked *(fixture)* are seen in `crates/femme-agents/fixtures/claude/*.events.json`
or tests; fields marked *(code)* are read by FEMME code; the rest are Claude Code's own schema, not
verified in this repo. The "desktop line" column applies §4.1.

| tool | input | desktop line | notes |
|---|---|---|---|
| `Bash` | `command`, `description` *(fixture)*, `timeout?`, `run_in_background?` | `Bash echo hi → hi` | `task` set when it runs as a task (`claude.rs:3100-3138`) |
| `Read` | `file_path` *(test `body.rs:353`)*, `offset?`, `limit?` | `Read /etc/hostname → <first line>` | |
| `Write` | `file_path`, `content` *(fixture)* | `Write /path → File created successfully at: …` | |
| `Edit` | `file_path`, `old_string`, `new_string`, `replace_all?` | `Edit /path → …` | no diff view on desktop; opened row shows the JSON |
| `MultiEdit` | `file_path`, `edits[{old_string,new_string,replace_all?}]` | `MultiEdit /path` | |
| `NotebookEdit` | `notebook_path`, … | `NotebookEdit` (no hint: `notebook_path` is not a hint key) | |
| `Grep` | `pattern`, `path?`, `glob?`, `output_mode?`, … | `Grep <path>` if `path` given, else `Grep <pattern>` (`path` precedes `pattern` in the key list) | |
| `Glob` | `pattern`, `path?` | same rule as Grep | |
| `WebFetch` | `url`, `prompt` | `WebFetch <url>` | |
| `WebSearch` | `query`, … | `WebSearch <query>` | |
| `ToolSearch` | `query`, `max_results` *(fixture)* | `ToolSearch select:ExitPlanMode → [tool_reference]` | |
| `Agent` (`Task` before 2.0) | `description`, `prompt`, `subagent_type` *(code `agents.rs:72-78`)* | `Agent <description>`; row gets an "open" control when items with this `parent` exist (`rows.rs:744-750`) | see §5 |
| `AskUserQuestion` | `questions[{question, header?, options[{label, description}], multiSelect}]` *(code `claude.rs:2348-2395`)* | main thread: no tool row, only `Question` items; subagent: a plain tool row with no hint | |
| `ExitPlanMode` | `plan` *(code `claude.rs:874`)* | main thread: `Plan` item (+ approval question); subagent: plain tool row | |
| `EnterPlanMode` | `{}` *(fixture)* | `EnterPlanMode → Entered plan mode. …` | |
| `TodoWrite` | `todos[{content, status, activeForm}]` | `TodoWrite` (no hint) | no checklist rendering on desktop |
| `TaskCreate` / `TaskUpdate` / other task tools | Claude Code's | name only, unless an input has a hint key (e.g. `description`) | no special handling anywhere in the repo |
| `Artifact` | `file_path` or `url` or `action` *(code `claude.rs:2060-2097`)* | `artifact · <title> ↗` + `updated · vN` | via `ToolResult.link` |
| MCP tools `mcp__<server>__<tool>` | the tool's args *(fixture)* | `mcp__elicit__ask_form` + hint if any | |

### 4.3 Codex and Pi tool names

- Codex (`codex.rs:1486-1590`): `commandExecution` `{command, cwd}`; `fileChange` `{changes}` (the
  open row's preview is `update <path>, …`, the parsed body has no hint key); `<server>.<tool>` for
  MCP with the call's `arguments`; `dynamicToolCall` named after its tool; `webSearch`
  `{query, action}`; `collabAgentToolCall` `{prompt, receiverThreadIds, model}`; unknown types:
  name = type, input = the item's other fields. Note the desktop's run summary knows `shell`,
  `exec_command`, `apply_patch` but not `commandExecution`/`fileChange`, so those read
  `3 commandExecution calls` (`turns.rs:104-112`).
- Pi: name = pi's `toolName`, input = its arguments (`pi.rs:520-530`, `:700-713`); no `task`.

---

## 5. Subagents (`parent`)

- Claude lines with `parent_tool_use_id` run through the same handlers with their own stream state;
  every item they emit except `Question` gets `parent = <Agent call id>` (`claude.rs:36-41`,
  `:381-414`, `on_line` `:1818-1830`). The subagent's first item is its prompt as a `UserMessage`
  (`claude.rs:1056-1063`). Subagents never open/close turns or change status, and their
  AskUserQuestion/ExitPlanMode calls are plain tool calls (`claude.rs:425-428`).
- A subagent still open when its `Agent` call's turn ends fails its open items with `the turn ended
  first`; a background one keeps going (`claude.rs:1211-1222`).
- Main transcript = items with `parent == None`; a subagent view = items whose `parent` is the call id
  (`state.rs:182-184`, desktop `items.rs:200-206`, `live.rs` `set_view`).
- Desktop chips (`desktop/src/transcript/agents.rs:48-121`): one chip per `Agent`/`Task` call since the
  last `TurnEnd`, label = `input.description` else `subagent_type` else hint else `agent`; running
  while there is no result (and not failed); a result whose output starts with `Async agent launched`
  marks it background and running; later items with that `parent` keep it running; an `Event` with
  `task.call == id` stops it (removed if from an earlier turn). Elapsed shown as `1m 12s`
  (`agents.rs:123-131`).

---

## 6. Pictures and videos: the `Attached:` convention

- **Sending:** the client uploads each file with `Req::BlobPut{name, data}` (≤ `MAX_BLOB_PUT` = 8 MiB,
  `rpc.rs:189-194`, `:301-302`; desktop refuses bigger, `desktop/src/composer/draft.rs:288`) → hash,
  then `Req::Prompt{sid, text, attachments: [{name, blob}], how}` (`rpc.rs:111-116`, `:50-55`).
  At most 16 attachments (`crates/femmed/src/uploads.rs:24`). The daemon writes each blob to
  `<state>/uploads/<sid>/<cleaned name>` (mode 600, `-2` suffix on a clash) and appends to the text a
  blank line plus one `Attached: <absolute path>` line per file (`uploads.rs:30-63`, `:203-214`). Just
  the lines when the text is empty. Uploaded blobs are orphans swept at the next retention pass, so
  `Prompt` must follow soon (`crates/femme-store/src/items.rs:26-29`).
- **`femmed show`** (`Req::Show{sid, name, blob, caption}`, `rpc.rs:218-225`): same save, then a
  daemon-made `AssistantMessage` with text `<caption>\n\nAttached: <path>` (`uploads.rs:65-121`). The
  CLI converts a video over 8 MiB into an animated GIF first (`crates/femmed/src/cli.rs:9-11`, `:411-435`).
- **Parsing (desktop `desktop/src/transcript/attach.rs:21-43`):** every line that starts with exactly
  `Attached: ` and has a non-empty path is removed from the text wherever it stands; the rest is
  trimmed at the end. File name = last `/` component. "Image" = extension png/jpg/jpeg/gif/webp/bmp/svg
  (`attach.rs:45-57`) or video mp4/mov/webm/mkv/avi/m4v (`desktop/src/video.rs:89-97`). Applies to
  `UserMessage` (88 px thumbnails, `rows.rs:327-357`, `thumb.rs:31`) and `AssistantMessage`
  (360×240 boxes, `rows.rs:503-534`, `thumb.rs:33-34`). Non-images are name chips.
- **Resolving:** by **path with `Req::FsRead{sid, path}`**, never by blob hash (`live.rs:583-612`).
  `FsRead` allows paths under `$HOME` or the session's uploads folder, no `..`, at most
  `MAX_FRAME - 4096` bytes (16 MiB frame) (`crates/femmed/src/front/fs.rs:1-8`, `:23-24`, `:169-175`).
  The picture format is sniffed from the bytes (`attach.rs:59-82`); videos get an ffmpeg poster frame
  and play in the lightbox (`live.rs:614-637`).
- Purging a session deletes its uploads (`uploads.rs:123-127`), so old `Attached:` paths can fail.
- The daemon's message pairing ignores `Attached:` lines, a bare `Attached:`, `[Image #N]` and
  `<pasted_content …>` wrappers (`crates/femmed/src/sent.rs:83-114`); a client may want the same when
  matching its outbox.

---

## 7. Driver differences in one place

| | Claude (headless) | Claude TUI mirror (herdr) | Codex | Pi |
|---|---|---|---|---|
| user message id | uuid FEMME wrote | transcript uuid | Codex UUIDv7 | `o<offset>` |
| saved-row pairing | by `echo` uuid | by text | by text | by text (`sent.rs:8-17`) |
| streaming | text, thinking | none (reads the file) | agent text, reasoning summary, plan | text, thinking |
| thinking | rare | | reasoning summary | yes |
| `task` on tool calls | yes | | no | no |
| `ToolResult.link` | Artifact only | | no | no |
| questions | AskUserQuestion, plan approval, Allow/Deny, elicitation | AskUserQuestion on the call itself; screen dialogs `herdr:…` | `requestUserInput`, approvals | extension dialogs |
| question completes to `Answered`/`Failed` | yes, by the driver | screen ones by the daemon | **see Surprises** | **see Surprises** |
| `Plan` | ExitPlanMode | ExitPlanMode (approval read from screen) | plan item + checklist | none |
| `PeerMessage` | yes | yes | no | no |
| `parent` (subagents) | yes | yes | no (collab tool call instead) | no |

---

## Surprises / where the brief's assumptions do not hold

1. **`free_text` does exist** (`state.rs:194-195`), and it is `true` for every Claude
   AskUserQuestion, plan approval and permission question (`claude.rs:1530`, `:2384`). On a permission
   or plan approval, a typed answer is sent to Claude as a **deny with that message**
   (`claude.rs:2454-2456`), not as an allow.
2. **No "Always" answer.** Permission questions are only `Allow`/`Deny` (plus text = deny with
   feedback, `Cancel` = deny). Persistent changes go through the session's permission mode.
3. **`TurnStart` items are never produced**; turn boundaries are `TurnEnd` rows only.
4. **`op`, `echo`, `read_at` live in the `UserMessage` body**, not on `Item`; Pending/Completed/Failed
   is `Item.status`. There is a fourth body field the brief did not list: `echo`.
5. **The read receipt is a separate `Event` row** (`read: <item_id>`), placed where the agent read the
   message; the original row never moves (`sent.rs:1-6`).
6. **Codex and Pi questions appear to stay `Pending` after the last answer.** Neither driver emits
   `ItemCompleted` for a question (`pi.rs:59-66`; Codex only `QuestionResolved`, `codex.rs:786-797`),
   the daemon ignores `QuestionResolved` (`headless.rs:1890-1891`), and it marks a part `Answered`
   only when the driver wrote nothing to the agent (`headless.rs:716-722`), which is never the case for
   the final (or only) answer. A client should therefore also look at the session's status (no
   longer `Blocked`) before offering an answer, and expect `AnswerQuestion` to fail with
   `UnknownRequest` for one already answered.
7. **Tool results carry no images**: an image block becomes the literal `[image]`. Pictures reach
   the transcript only through `Attached:` lines, and the desktop fetches them by **path**
   (`FsRead`), not by `BlobGet`.
8. **Rate limits, model changes, permission-mode changes and goal `Met` are not transcript items**;
   they update the `Session` row. Only a goal `NotMet` verdict and `/` command output appear as events.
9. **A truncated blob body is not valid JSON**; clients must tolerate it and can rely on sorted keys
   to recover `text`, `output`, `is_error`, `link`, `name`.
10. **Grep/Glob show the `path`, not the pattern, when a path is given**, because `path` precedes
    `pattern` in the hint keys (`body.rs:12-20`).
