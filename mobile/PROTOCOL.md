# FEMME mobile — what the app speaks

The app is one more `femme-client`: QUIC (quinn, TLS 1.3, ALPN `femme/1`, UDP 7788), mutual
TLS with an enrolled device cert, `PROTO_VERSION` 1, the frames of `crates/femme-proto`
(`varint len | flags | postcard`, zstd outside Term streams). Everything below exists today
unless marked **new**.

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
| Me | `PairStart`/`PairFinish` (**new**), `PushRegister` (**new**) | Ctrl |

Reconnect: `Hello{proto, client_instance, cursors{epoch, global_rev, sessions}}` resumes every
subscription from the last rev; requests carry an `OpId` and are deduped, so the app resends
anything unanswered. Nothing in the app depends on the connection being up to *save* a
message: the draft and the outbox are on disk first, the `Pending` row on the box second.

## The four additions to `femmed`

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

### 2. `Scope::Attention`

A subscription whose rows are only sessions whose attention changed, each with the pending
`Item::Question` (or permission item) inlined. One stream for the Now screen instead of
`Subscribe{Session}` per blocked session; the desktop could use it for its badge too.

### 3. Pairing

```
Req::PairStart            → Resp::Pair { code, expires }      (an enrolled device asks; the desktop shows it as a QR)
   QR = femme://pair?host=<h>&port=7788&fp=<server spki sha256>&code=<one-time>
Req::PairFinish { code, name, spki }  → Resp::Ok              (sent by the phone over a connection that is
                                                              not yet trusted: TLS up, client cert presented but unknown;
                                                              the server allows exactly this request on such a connection)
```

`PairFinish` writes the same device row `femmed enroll --name <name> --spki <sha256>` writes, then
the phone reconnects as a trusted device. Codes are one-time and live 2 minutes. `femmed devices`
lists and revokes phones like any device.

### 4. WebSocket carrier

Caddy already fronts `femmed`'s HTTP. A `/femme/ws` route carries the same frames (one binary
message per frame, stream kind as the first frame per socket, one socket per stream) for
networks that drop UDP. Auth on this path: the device cert as a TLS client cert is not available
through Caddy, so the first frame is a signed challenge (the device signs the server nonce with
its cert key). Same `Hello` after that.

## Not needed

- No new HTTP API: the board's compat routes stay for bots; the app speaks the desktop's protocol.
- No server-side rendering of the transcript: the fold (`crates/femme-agents` driver → `Item`
  rows) already happens on the box; the app draws rows.
- No upload service: `BlobPut` + `Prompt` is the upload (videos over 8 MiB wait for the chunked
  `Upload{name, len}` → `UploadStream` path, which exists but the desktop does not use yet).
