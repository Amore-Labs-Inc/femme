# FEMME mobile — what the app speaks

The app is one more client of `femmed`, on the **WebSocket carrier** (`crates/femme-transport/src/ws.rs`,
served at `/femme/ws` by `crates/femmed/src/front/ws.rs`): the same streams, requests and events as
the desktop's QUIC, carried as JSON text messages over one `wss://` socket behind the board's HTTPS
(Caddy), so it works on every network a browser does and needs no UDP and no client certificate.
The types are those of `crates/femme-proto` (`rpc.rs`, `state.rs`, `term.rs`) as serde_json writes
them. Everything below exists today unless marked **new**.

## The carrier

- URL `wss://<board host>/femme/ws` (`ws://127.0.0.1:8787/femme/ws` on the box itself), header
  `Authorization: Bearer <token>`, or `?token=` when a client cannot set headers.
- One text message = one JSON object, multiplexing streams:
  `{"s":N,"open":KIND}` opens stream `N` (phone ids odd, daemon ids even);
  `{"s":N,"m":MSG}` carries one message of stream `N`; `{"s":N,"fin":true}` finishes the sender's
  direction; `{"s":N,"rst":true}` resets it.
  `KIND` = `"Ctrl"` | `"Events"` | `{"Term":{"sid":"<ULID>","no":0}}` | `{"Bulk":7}`.
- JSON shapes: serde's externally tagged enums (`"Idle"`, `{"Session":"01J…"}`,
  `{"Subscribe":{"scope":"Global","after":null}}`), `Result` as `{"Ok":…}` / `{"Err":{"code":"NotFound","msg":"…"}}`,
  ids (`Sid`, `OpId`, `DeviceId`) as 26-char ULID text, `[u8;32]` and `Vec<u8>` as arrays of
  numbers, bit flags (`Mods`, `Attrs`, `Modes`) as `"SHIFT | CTRL"`, `char` as a one-char string.
  `Item.body` is a JSON string (decode it again). `client_instance` is any number below 2^53.
- Flow: open `Ctrl` as stream 1 → `{"Hello":{"proto":1,"client_instance":N,"cursors":{…}}}` →
  `{"Welcome":…}` back on stream 1 → the daemon opens `Events` (stream 2) and pushes `Changes` /
  `Delta` there → requests go as `{"Req":{"op":"<ULID>","req":…}}` on stream 1, answered with
  `{"Resp":{"op":…,"result":…}}` on stream 1. A terminal is a stream the phone opens with
  `{"Term":pane}` and starts with `TermUp::Open`.
- One socket, one FIFO: a blob in flight delays a keystroke behind it (QUIC's per-stream
  priorities do not exist here). Fine for a phone; the terminal is read-mostly there.

## Pairing

`femmed pair --name <phone>` on the box enrolls the phone under a fresh 256-bit token (the device
row's key is the token's SHA-256, so `femmed devices` lists it and `femmed devices revoke <name>`
cuts it off like a desktop) and prints a QR code of

    femme://pair?host=<board host>&tls=1&name=<box name>&token=<token>

The app scans it (or takes the link pasted) and keeps the token in the Keychain / Keystore.

## Per screen

| screen | requests | stream |
|---|---|---|
| Now | `Subscribe{Global}` for the session rows (status, attention, label, context); the blocked ones' pending question comes from `Subscribe{Session(sid)}` of each blocked session (**new** `Scope::Attention` makes this one stream) | Events |
| answer a question | `AnswerQuestion{sid, request_id, answer: Option(n) \| Options \| Text \| Cancel}` | Ctrl |
| permission | `CyclePermission{sid}` for the mode; allow/deny of one call is an `AnswerQuestion` on the permission item (the holder's `permission-prompt-tool=stdio` already turns it into one) | Ctrl |
| Chat | `Subscribe{Session(sid)}` → `Changes{rows: Item…}` + `Delta{sid, item_id, offset, text}`; older history `TranscriptPage{sid, before_seq, limit}`; pictures `BlobGet{hash}` / `FsRead` for `Attached:` paths | Events, Bulk |
| send | `BlobPut{name, data ≤ 8 MiB}` per attachment → `Prompt{sid, text, attachments, how: Queue \| Steer \| InterruptThenSend}`; the bubble's state is the `user_message` item's status (`Pending` = queued, `Completed`+`read_at` = read, `Failed` = not received → `Resend{sid, item_id}`) | Ctrl |
| interrupt | `Interrupt{sid}`; `Background{sid, tool_use_id}` | Ctrl |
| `/` sheet | `Commands` → names; `SetModel`, effort/goal as typed commands in `Prompt` | Ctrl |
| Terminal | `PaneOpen{sid}` → `Pane(id)`, then a `Term(pane)` stream for rows and keys; `PaneClose` on leave | Term |
| Files | `FsList{sid, path}`, `FsRead{sid, path}` (ask above 8 MiB) | Bulk |
| new chat | `Folders{path}` for the picker, `SessionCreate{cwd, agent, mode: Headless, label, prompt, model}` | Ctrl |
| Box | `ServerInfo` (version, deployed), `Caps` / `SetCap`, `BoardApi{GET /api/accounts, /api/meta}`, `Prs`, `PrMerge`, `PrFix` (→ a session), `PrVideo` (→ lightbox), issues via `IssueCreate/Patch/Delete` + `Row::Issue` | Ctrl, Bulk |
| session info | `SessionLabel`, `SessionClose`, `Takeover{sid, to}` (not offered on the phone, read-only) | Ctrl |
| Me | pairing is the QR above (no request); `PushRegister` (**new**) | Ctrl |

Reconnect: `Hello{proto, client_instance, cursors{epoch, global_rev, sessions}}` resumes every
subscription from the last rev; requests carry an `OpId` and are deduped, so the app resends
anything unanswered. Nothing in the app depends on the connection being up to *save* a
message: the draft and the outbox are on disk first, the `Pending` row on the box second.

## Additions to `femmed`

### 1. Push registration

```
Req::PushRegister { platform: Apns | Fcm | Ntfy, token: String, rules: PushRules }
Req::PushUnregister { token }
PushRules { blocked: true, done: bool, stalled_after: Option<Duration>, muted: Vec<Sid>, quiet: Option<(u8,u8)> }
```

`notify.rs` already posts to ntfy on an attention change; it gains a list of registrations and
posts to each (ntfy direct; APNs/FCM through a relay the box owner runs or Amore hosts). The
payload carries `sid`, `item_id` of the pending question, the question text and up to three
options, so the OS notification shows them as actions and the notification extension can send
`AnswerQuestion` without the app in the foreground. Rate: one push per session per 10 s, as now.

### 2. `Scope::Attention` (not built)

Now subscribes to `Global` plus `Session(sid)` for each blocked session, which is one small
stream per open question. A merged scope saves nothing a phone can feel at that size; build it
when a box has hundreds of blocked sessions at once.

### 3. Pairing, 4. WebSocket carrier

Done: see *The carrier* and *Pairing* above.

## Not needed

- No new HTTP API: the board's compat routes stay for bots; the app speaks the desktop's protocol.
- No server-side rendering of the transcript: the fold (`crates/femme-agents` driver → `Item`
  rows) already happens on the box; the app draws rows.
- No upload service: `BlobPut` + `Prompt` is the upload (videos over 8 MiB wait for the chunked
  `Upload{name, len}` → `UploadStream` path, which exists but the desktop does not use yet).
