//! Replicated state: sessions, panes, transcript items, issues. Every row has a monotonic `rev`
//! (one counter per daemon). Clients subscribe with the highest rev they hold and receive rows
//! with a higher rev; deletes are tombstones.

use serde::{Deserialize, Serialize};

use crate::{PaneId, Sid};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Agent {
    Shell,
    Claude,
    Codex,
    Pi,
}

/// How the agent runs: headless (stream-json pipes, structured transcript) or in a PTY (its own TUI).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mode {
    Headless,
    Tui,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionStatus {
    Starting,
    Idle,
    Working,
    /// Waiting on a question for the user.
    Blocked,
    Paused,
    /// Daemon found the session's holder gone (e.g. after a reboot).
    Interrupted,
    Crashed,
    Failed,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Attention {
    #[default]
    None,
    Blocked,
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: Sid,
    /// Short unique handle used in compat ids (`w<handle>:p<no>`) and URLs.
    pub handle: String,
    pub rev: u64,
    /// Name an API key or the board gave it (`/api/agents/{name}`).
    pub name: Option<String>,
    /// `board`, `issue:<n>`, `key:<name>`.
    pub owner: String,
    pub agent: Agent,
    pub mode: Mode,
    pub cwd: String,
    pub label: Option<String>,
    /// The agent's own session/thread id (`claude --resume`, Codex thread id).
    pub agent_sid: Option<String>,
    pub model: Option<String>,
    pub status: SessionStatus,
    pub attention: Attention,
    /// Unix ms.
    pub created: i64,
    pub closed: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PaneKind {
    /// The agent's TUI (TUI mode only).
    Agent,
    Shell,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    pub id: PaneId,
    pub rev: u64,
    pub kind: PaneKind,
    pub title: String,
}

/// Transcript item kinds. Bodies are JSON (see [`Item::body`]), shaped per kind by `femme-agents`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ItemKind {
    UserMessage,
    AssistantMessage,
    Thinking,
    ToolCall,
    ToolResult,
    Question,
    Plan,
    /// Status/system events shown inline (model change, interrupt, rate limit, …).
    Event,
    TurnStart,
    TurnEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
    /// Question awaiting an answer.
    Pending,
    Answered,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub sid: Sid,
    /// Agent-assigned id when it has one, else a ULID.
    pub item_id: String,
    /// Order within the session.
    pub seq: u64,
    pub rev: u64,
    pub kind: ItemKind,
    pub status: ItemStatus,
    /// First ~512 bytes of the text, for list rows and the phone board.
    pub preview: String,
    /// JSON body, inline when ≤ 4 KiB; else `blob_hash` is set and the client fetches on expand.
    pub body: Option<String>,
    pub blob_hash: Option<[u8; 32]>,
    /// Unix ms.
    pub ts: i64,
    /// Set on a subagent's items: the id of the tool call (Agent) that spawned it. The main
    /// transcript shows items without one; a subagent view shows the items with its id.
    pub parent: Option<String>,
}

/// A question an agent asks the user (AskUserQuestion, plan approval, Codex `requestUserInput`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub request_id: String,
    pub prompt: String,
    pub options: Vec<String>,
    pub multi: bool,
    /// Free text allowed (an "Other" answer).
    pub free_text: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// Issue statuses as the board stores them today (`STATUSES` in board/src/main.rs), same order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IssueStatus {
    Open,
    Working,
    Waiting,
    Paused,
    Pr,
    Done,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub id: u64,
    pub rev: u64,
    pub repo: String,
    pub title: String,
    pub body: String,
    pub status: IssueStatus,
    pub pr_url: Option<String>,
    pub note: Option<String>,
    pub session: Option<Sid>,
    /// Unix ms; set when paused for a rate limit.
    pub resume_at: Option<i64>,
    pub created: i64,
    pub updated: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Entity {
    Session,
    Pane,
    Item,
    Issue,
}

/// One replicated row, as carried by `S2C::Changes`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Row {
    Session(Session),
    Pane(Pane),
    Item(Item),
    Issue(Issue),
    /// `id` is the entity's id as text (session ULID, `sid:no`, `sid/item_id`, issue number).
    Tombstone { entity: Entity, id: String, rev: u64 },
}

impl Row {
    pub fn rev(&self) -> u64 {
        match self {
            Row::Session(s) => s.rev,
            Row::Pane(p) => p.rev,
            Row::Item(i) => i.rev,
            Row::Issue(i) => i.rev,
            Row::Tombstone { rev, .. } => *rev,
        }
    }
}
