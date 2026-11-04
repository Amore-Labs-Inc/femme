//! Desktop ↔ daemon control protocol (`ctrl` stream, priority 5; `events` stream, priority 3).
//! Resume = a new connection + `Hello` with cursors. There is no ack/replay layer: state is
//! rev-based, terminals are frame-based, requests are deduped by `OpId`.

use serde::{Deserialize, Serialize};

use crate::state::{Agent, IssueStatus, Item, Mode, Row};
use crate::{OpId, PaneId, Sid};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Cursors {
    /// Daemon database epoch; a mismatch means "forget everything".
    pub epoch: u64,
    /// Highest rev held for the global scope (sessions, panes, issues).
    pub global_rev: Option<u64>,
    /// Highest item rev held per session the client has open.
    pub sessions: Vec<(Sid, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    /// Sessions, panes and issues.
    Global,
    /// Transcript items of one session.
    Session(Sid),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum C2S {
    Hello {
        proto: u16,
        client_instance: u128,
        cursors: Cursors,
    },
    Req {
        op: OpId,
        req: Req,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptHow {
    /// Run after the current turn.
    Queue,
    /// Inject into the running turn.
    Steer,
    InterruptThenSend,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    /// Uploaded first via `Req::Upload`.
    pub blob: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Answer {
    Option(u16),
    Options(Vec<u16>),
    Text(String),
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueField {
    Repo(String),
    Title(String),
    Body(String),
    Status(IssueStatus),
    Note(Option<String>),
    PrUrl(Option<String>),
    /// The session working the issue: the daemon's own link (`None` unlinks).
    Session(Option<Sid>),
    /// When a paused issue may resume, unix ms (`None` clears).
    ResumeAt(Option<i64>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Req {
    /// Rows with rev > `after` (or everything when `None`), then live changes.
    Subscribe { scope: Scope, after: Option<u64> },
    Unsubscribe { scope: Scope },
    TranscriptPage { sid: Sid, before_seq: u64, limit: u16 },
    BlobGet { hash: [u8; 32] },
    SessionCreate {
        cwd: String,
        agent: Agent,
        mode: Mode,
        label: Option<String>,
        name: Option<String>,
        prompt: Option<String>,
        /// The agent's model (`None`: its default). A Claude model that names its maker
        /// (`deepseek/deepseek-v4.1-flash-ultrafast`) runs through OpenGateway.
        model: Option<String>,
    },
    SessionClose { sid: Sid },
    Prompt {
        sid: Sid,
        text: String,
        attachments: Vec<Attachment>,
        how: PromptHow,
    },
    Interrupt { sid: Sid },
    AnswerQuestion { sid: Sid, request_id: String, answer: Answer },
    Takeover { sid: Sid, to: Mode },
    SetModel { sid: Sid, model: String },
    PaneOpen { sid: Sid },
    PaneClose { pane: PaneId },
    IssueCreate { repo: String, title: String, body: String },
    /// Per-field merge: 409 only when a changed field moved past `base_rev`.
    IssuePatch { id: u64, base_rev: u64, fields: Vec<IssueField> },
    IssueDelete { id: u64 },
    /// Opens a `bulk` stream for the bytes; `Resp::UploadStream` names it.
    Upload { name: String, len: u64 },
    FsList { sid: Sid, path: String },
    FsRead { sid: Sid, path: String },
    /// The folders inside `path`, for picking a project's folder before any session runs there:
    /// `~` and a relative path mean the home folder, and the folder must lie under it. With
    /// `make` a missing folder is created first. Answered with `Resp::Folders`.
    Folders { path: String, make: bool },
    /// One call of the web app's JSON API (`/api/accounts`, `/api/keys`, …) made by the daemon
    /// on the desktop's behalf, as the owner: the accounts overlay shows what the board shows.
    /// `method` is GET, POST or DELETE; `body` is the JSON to send (empty for none).
    BoardApi {
        method: String,
        path: String,
        body: String,
    },
    /// Stores `data` as a blob and answers its hash: what a `Prompt` attachment names. At most
    /// [`MAX_BLOB_PUT`] bytes.
    BlobPut { name: String, data: Vec<u8> },
    /// The per-account rotation caps (percent of the 5-hour and the 7-day window at which the
    /// box moves off an account); accounts without one use the box's `CAP` / `FABLE_CAP`.
    Caps,
    /// Sets (or with both `None`, clears) an account's caps. `account` is cswap's number.
    SetCap {
        account: u32,
        five_hour: Option<u8>,
        weekly: Option<u8>,
    },
    /// What the daemon is: its version and the commit the box was deployed from.
    ServerInfo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsEntry {
    pub name: String,
    pub dir: bool,
    pub len: u64,
}

/// The most a `BlobPut` carries: an attachment, not a transfer (M5 brings `Upload`).
pub const MAX_BLOB_PUT: usize = 8 << 20;

/// One account's rotation caps, in percent of the window; `None` means the box's default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountCap {
    /// cswap's account number.
    pub account: u32,
    pub five_hour: Option<u8>,
    pub weekly: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resp {
    Ok,
    Session(Sid),
    Pane(PaneId),
    Page { items: Vec<Item>, next_before: Option<u64> },
    Blob(Vec<u8>),
    Issue(u64),
    UploadStream(u64),
    FsEntries(Vec<FsEntry>),
    FsData(Vec<u8>),
    /// The answer to `Folders`: the folder as an absolute path, and the names of the folders in it.
    Folders { path: String, names: Vec<String> },
    /// The web app's answer to `BoardApi`: its HTTP status and body (JSON, or an error text).
    BoardApi { status: u16, body: String },
    /// The hash `BlobPut` stored under.
    BlobHash([u8; 32]),
    Caps(Vec<AccountCap>),
    ServerInfo {
        version: String,
        /// `<state>/deployed`: the commit the box runs, when the file is there.
        deployed: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrCode {
    Bad,
    NotFound,
    Conflict,
    Busy,
    Unauthorized,
    Unsupported,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Err {
    pub code: ErrCode,
    pub msg: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum S2C {
    Welcome {
        proto: u16,
        server_version: String,
        epoch: u64,
    },
    Resp {
        op: OpId,
        result: Result<Resp, Err>,
    },
    /// Rows newer than the subscriber's cursor; `rev` is the highest rev included. `reset`: the
    /// cursor could not be honoured (older than the tombstone horizon, or ahead of the database),
    /// so `rows` are the scope's full set and the subscriber drops its copy of the scope before
    /// applying them; a scope the daemon no longer has (a purged session) comes as an empty reset.
    /// Only the first message of a backlog carries it.
    Changes {
        scope: Scope,
        rev: u64,
        rows: Vec<Row>,
        reset: bool,
    },
    /// Streaming text of an in-progress item (never persisted as such).
    Delta {
        sid: Sid,
        item_id: String,
        offset: u32,
        text: String,
    },
    /// The backlog was too big to inline: a `bulk` stream carries it in 256-row chunks.
    SnapshotOnBulk { scope: Scope, rev: u64, stream: u64 },
}

/// What a transport stream carries. The side that opens a QUIC or SSH-mux stream sends this as the
/// first frame (postcard, same framing as every other frame) so the acceptor can route the stream.
/// Stream priorities hang off the kind (plan §3 rule 1): `Term` 10, `Ctrl` 5, `Events` 3, `Bulk` −10.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StreamKind {
    /// Request/response control channel: `C2S` up, `S2C` down.
    Ctrl,
    /// Server push of replicated state: `S2C::Changes` / `S2C::Delta`.
    Events,
    /// One attached terminal pane: `TermUp` up, `TermDown` down.
    Term(PaneId),
    /// A bulk transfer (upload, snapshot, blob); the id comes from `Resp::UploadStream` or
    /// `S2C::SnapshotOnBulk`.
    Bulk(u64),
}
