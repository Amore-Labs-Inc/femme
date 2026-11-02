//! The boundary between this crate's layers (the compat HTTP router, the scheduler's executor) and
//! the daemon core (sessions, holders, the store). The router and the scheduler only ever talk to
//! a [`Core`]; the real one is written by the daemon's wiring, the test one is
//! [`crate::core_fake::FakeCore`].
//!
//! Conventions
//! - Every timestamp crossing this trait is unix **milliseconds**, like `femme-proto`. The compat
//!   layer turns them into the unix *seconds* the board's JSON always carried.
//! - Compat ids are `w<handle>:p<no>` (`instinct~w<handle>:p<no>` for sessions opened through the
//!   agents API), matching `openapi.json`. [`SessionInfo`] carries what is needed to mint them.

use std::fmt;

use async_trait::async_trait;
use axum::http::StatusCode;
use femme_proto::rpc::{Answer, IssueField};
use femme_proto::state::{Agent, Attention, Issue, Mode, PaneKind, SessionStatus};
use femme_proto::Sid;
use serde_json::Value;

use crate::config::Config;

/// What a [`Core`] call can fail with; [`CoreError::status`] is the one place it becomes HTTP.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    /// No such session, pane, issue or key: 404.
    #[error("not found")]
    NotFound,
    /// The agent cannot take a message now (blocked on a question, still starting, no agent in
    /// the pane): 409, body `agent_not_ready` as the board's herdr call answered.
    #[error("agent_not_ready")]
    Busy,
    /// The state moved under the caller (same-field issue conflict, duplicate key name): 409.
    #[error("{0}")]
    Conflict(String),
    /// The input is wrong in a way only the core can tell: 400.
    #[error("{0}")]
    Bad(String),
    /// Everything else: the box could not do it (holder gone, store failed, spawn failed): 502,
    /// as the board answered when herdr failed.
    #[error("{0}")]
    Internal(String),
}

impl CoreError {
    /// The HTTP status of this error, for every route.
    pub fn status(&self) -> StatusCode {
        match self {
            CoreError::NotFound => StatusCode::NOT_FOUND,
            CoreError::Busy | CoreError::Conflict(_) => StatusCode::CONFLICT,
            CoreError::Bad(_) => StatusCode::BAD_REQUEST,
            CoreError::Internal(_) => StatusCode::BAD_GATEWAY,
        }
    }
}

/// A pane as the compat API names it: `[machine~]w<handle>:p<no>`. `machine` is `instinct` for
/// sessions opened through `/api/agents` (the board's old herdr session of that name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneRef {
    pub machine: Option<String>,
    /// The session's `handle`, without the leading `w`.
    pub handle: String,
    /// The pane number, as in `p<no>`.
    pub no: u16,
}

/// A workspace (one session) as the compat API names it: `[machine~]w<handle>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WsRef {
    pub machine: Option<String>,
    pub handle: String,
}

impl PaneRef {
    /// Parses an id from a URL. `None` for anything that is not exactly `[machine~]w<alnum>:p<digits>`.
    pub fn parse(id: &str) -> Option<PaneRef> {
        let (machine, id) = on_machine(id, true)?;
        let (ws, p) = id.split_once(':')?;
        Some(PaneRef {
            machine: machine.map(String::from),
            handle: ws[1..].to_string(),
            no: p[1..].parse().ok()?,
        })
    }

    /// The workspace this pane is in.
    pub fn ws(&self) -> WsRef {
        WsRef {
            machine: self.machine.clone(),
            handle: self.handle.clone(),
        }
    }
}

impl WsRef {
    /// Parses a workspace id from a URL (`DELETE /api/terminals/{ws}`).
    pub fn parse(id: &str) -> Option<WsRef> {
        let (machine, id) = on_machine(id, false)?;
        Some(WsRef {
            machine: machine.map(String::from),
            handle: id[1..].to_string(),
        })
    }
}

impl fmt::Display for PaneRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(m) = &self.machine {
            write!(f, "{m}~")?;
        }
        write!(f, "w{}:p{}", self.handle, self.no)
    }
}

impl fmt::Display for WsRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(m) = &self.machine {
            write!(f, "{m}~")?;
        }
        write!(f, "w{}", self.handle)
    }
}

/// Compat ids: workspaces look like `w9`, panes like `w9:p1`. Anything else never reaches the core.
pub fn valid_id(id: &str, pane: bool) -> bool {
    let word =
        |w: &str| w.len() > 1 && w.len() < 16 && w.chars().all(|c| c.is_ascii_alphanumeric());
    match id.split_once(':') {
        Some((ws, p)) => pane && ws.starts_with('w') && word(ws) && p.starts_with('p') && word(p),
        None => !pane && id.starts_with('w') && word(id),
    }
}

/// Splits the machine tag off an id (`instinct~w9:p1` is machine `instinct`, id `w9:p1`) and checks
/// the id. The tag is a plain name (letters, digits, `-`, `_`); which machines exist is the core's call.
pub fn on_machine(id: &str, pane: bool) -> Option<(Option<&str>, &str)> {
    let (machine, id) = match id.split_once('~') {
        None => (None, id),
        Some((m, id)) => {
            let ok = m.len() < 64
                && m.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !ok || m.is_empty() {
                return None;
            }
            (Some(m), id)
        }
    };
    valid_id(id, pane).then_some((machine, id))
}

/// One terminal pane of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneInfo {
    /// The compat pane number (`p<no>`). A headless session still has one pane, the agent's.
    pub no: u16,
    pub kind: PaneKind,
    pub title: String,
    pub cwd: Option<String>,
}

/// A session as the compat API and the scheduler see it: one board "workspace".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub sid: Sid,
    /// Short unique handle: `w<handle>` in compat ids.
    pub handle: String,
    /// `Some("instinct")` for every session opened through `/api/agents` (it carries a `name`),
    /// whoever opened it: the board kept them all on herdr's `instinct` machine.
    pub machine: Option<String>,
    /// The agent name `/api/agents/{name}` knows it by.
    pub name: Option<String>,
    /// `board`, `issue:<n>`, `key:<name>`.
    pub owner: String,
    pub agent: Agent,
    pub mode: Mode,
    pub cwd: String,
    pub label: Option<String>,
    pub status: SessionStatus,
    pub attention: Attention,
    pub panes: Vec<PaneInfo>,
}

impl SessionInfo {
    /// `[machine~]w<handle>`.
    pub fn workspace_id(&self) -> String {
        WsRef {
            machine: self.machine.clone(),
            handle: self.handle.clone(),
        }
        .to_string()
    }

    /// `[machine~]w<handle>:p<no>`.
    pub fn pane_id(&self, no: u16) -> String {
        PaneRef {
            machine: self.machine.clone(),
            handle: self.handle.clone(),
            no,
        }
        .to_string()
    }

    /// The pane a client talks to: the agent's, else the first.
    pub fn main_pane(&self) -> Option<&PaneInfo> {
        self.panes
            .iter()
            .find(|p| p.kind == PaneKind::Agent)
            .or(self.panes.first())
    }

    /// The compat id of [`SessionInfo::main_pane`].
    pub fn main_pane_id(&self) -> Option<String> {
        self.main_pane().map(|p| self.pane_id(p.no))
    }

    /// The agent status in the board's vocabulary, `None` for a plain shell.
    pub fn agent_status(&self) -> Option<&'static str> {
        (self.agent != Agent::Shell).then(|| agent_status(self.status, self.attention))
    }
}

/// herdr's `agent_status` for a session: `working`, `idle`, `done` (finished, not yet looked at),
/// `blocked` (asking something) or `unknown` (the session is gone or broken). The board, `openapi.json`
/// and `examples/api-client.sh` all speak this.
pub fn agent_status(status: SessionStatus, attention: Attention) -> &'static str {
    match (status, attention) {
        (SessionStatus::Blocked, _) | (_, Attention::Blocked) => "blocked",
        (SessionStatus::Working | SessionStatus::Starting, _) => "working",
        (SessionStatus::Idle | SessionStatus::Paused, Attention::Done) => "done",
        (SessionStatus::Idle | SessionStatus::Paused, _) => "idle",
        _ => "unknown",
    }
}

/// `claude`, `codex`, `pi`; `None` for a shell (the board listed those with `agent: null`).
pub fn agent_name(agent: Agent) -> Option<&'static str> {
    match agent {
        Agent::Shell => None,
        Agent::Claude => Some("claude"),
        Agent::Codex => Some("codex"),
        Agent::Pi => Some("pi"),
    }
}

/// The inverse of [`agent_name`], plus `shell`.
pub fn parse_agent(s: &str) -> Option<Agent> {
    match s {
        "shell" => Some(Agent::Shell),
        "claude" => Some(Agent::Claude),
        "codex" => Some(Agent::Codex),
        "pi" => Some(Agent::Pi),
        _ => None,
    }
}

/// What `POST /api/terminals` and `/api/agents/{name}` ask the core to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateSession {
    /// An existing directory under home (already checked).
    pub cwd: String,
    pub agent: Agent,
    /// A request: the board's own terminals ask for `Tui`, agents-API and issue sessions for
    /// `Headless`. A shell is always `Tui`, whatever is asked.
    pub mode: Mode,
    pub label: Option<String>,
    /// Set for agents-API sessions.
    pub name: Option<String>,
    /// `board`, or `key:<name>` for agents-API sessions.
    pub owner: String,
}

/// A pane's screen and the status of the agent in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screen {
    pub text: String,
    /// The agent status (see [`agent_status`]), `None` for a plain shell.
    pub status: Option<String>,
}

/// A GitHub account on an API key: its clones push as it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubAccount {
    pub token: String,
    pub login: String,
    pub name: String,
    pub email: String,
}

/// Another agent's key (Instinct's), made on the board.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApiKey {
    pub name: String,
    /// 64 hex characters, shown once.
    pub key: String,
    pub github_token: Option<String>,
    pub github_login: Option<String>,
    pub git_name: Option<String>,
    pub git_email: Option<String>,
    /// Unix ms.
    pub created: i64,
}

/// The daemon core as the compat layer and the scheduler's executor need it.
///
/// Errors: [`CoreError::NotFound`] when the thing does not exist, [`CoreError::Busy`] when an
/// agent cannot take a message, [`CoreError::Conflict`] for state clashes, [`CoreError::Bad`] for
/// input only the core can judge, [`CoreError::Internal`] for the rest.
#[async_trait]
pub trait Core: Send + Sync + 'static {
    // ---- sessions and terminals --------------------------------------------------------------

    /// Every open session as a board workspace, in a stable order: board-made, issue-made and
    /// key-made (`machine = Some("instinct")`). What `GET /api/terminals` lists.
    async fn terminals(&self) -> Result<Vec<SessionInfo>, CoreError>;

    /// The open session an agent name belongs to (`/api/agents/{name}`), if any.
    async fn session_by_name(&self, name: &str) -> Result<Option<SessionInfo>, CoreError>;

    /// Opens a session in `req.cwd` and starts the agent in it; returns once it is up (it may still
    /// be `Starting`). Failure is `Internal` with a message a client can read ("Claude did not start: ...").
    async fn create_session(&self, req: CreateSession) -> Result<SessionInfo, CoreError>;

    /// Ends a session: the agent and its panes stop.
    async fn close_session(&self, sid: Sid) -> Result<(), CoreError>;

    /// The pane's screen. `ansi`: with colours, for the phone's terminal; else plain text.
    /// A headless session has no grid: its text is the last 50 transcript lines, then its pending
    /// question with numbered options.
    async fn screen(&self, pane: &PaneRef, ansi: bool) -> Result<Screen, CoreError>;

    /// Hands the agent in the pane a whole message (PTY: bracketed paste, then Enter; headless: a
    /// `Prompt` request). `Busy` when it is blocked on a question, still starting, busy with no
    /// way to queue, or the pane holds no agent.
    async fn prompt(&self, pane: &PaneRef, text: &str) -> Result<(), CoreError>;

    /// Types raw bytes into the pane (the chat panel's keys, pasted paths). For a headless
    /// session blocked on a question, digits, `y`, `n` and Escape answer it; otherwise `Busy`.
    async fn input(&self, pane: &PaneRef, bytes: &[u8]) -> Result<(), CoreError>;

    /// Stops the running turn (headless: the driver's interrupt; PTY: Escape). Held prompts stay.
    /// `Busy` when the agent is not up.
    async fn interrupt(&self, sid: Sid) -> Result<(), CoreError>;

    /// Answers a pending question of a headless session (`request_id` is the question item's).
    /// The first answer wins: a second one is `Conflict`.
    async fn answer(&self, sid: Sid, request_id: &str, answer: Answer) -> Result<(), CoreError>;

    /// Switches the model for the turns that follow (headless only; `Bad` for a PTY session).
    async fn set_model(&self, sid: Sid, model: &str) -> Result<(), CoreError>;

    /// Moves a session between headless and the agent's own TUI, returning once the other side is
    /// up (`Busy` while a switch or a replay runs). Never two agent processes at once.
    async fn takeover(&self, sid: Sid, to: Mode) -> Result<(), CoreError>;

    // ---- issues (the store's per-field patch semantics) --------------------------------------

    /// Every issue, any order.
    async fn issues(&self) -> Result<Vec<Issue>, CoreError>;
    /// A new `open` issue; the core allocates the id.
    async fn issue_create(&self, repo: &str, title: &str, body: &str) -> Result<Issue, CoreError>;
    /// Applies the fields (each last-writer-wins) and returns the issue. `NotFound`, or `Conflict`.
    async fn issue_patch(&self, id: u64, fields: Vec<IssueField>) -> Result<Issue, CoreError>;
    async fn issue_delete(&self, id: u64) -> Result<(), CoreError>;

    // ---- sign-in sessions, keys ---------------------------------------------------------------

    /// Whether a cookie's token (SHA-256 of the cookie value) is a live web session. Errors mean no.
    async fn web_session_valid(&self, token_sha256: [u8; 32]) -> bool;
    /// Stores a web session until `expires` (unix ms).
    async fn web_session_create(
        &self,
        token_sha256: [u8; 32],
        expires: i64,
    ) -> Result<(), CoreError>;
    async fn web_session_delete(&self, token_sha256: [u8; 32]) -> Result<(), CoreError>;

    /// Every API key, with its secret: the router compares a bearer to each in constant time.
    async fn api_keys(&self) -> Result<Vec<ApiKey>, CoreError>;
    /// Stores a key; `Conflict` when the name is taken.
    async fn api_key_create(&self, key: ApiKey) -> Result<(), CoreError>;
    /// `NotFound` when there is no such key.
    async fn api_key_delete(&self, name: &str) -> Result<(), CoreError>;
    /// Puts a GitHub account on a key, or takes it off (`None`). `NotFound` when there is no such key.
    async fn api_key_set_github(
        &self,
        name: &str,
        github: Option<GithubAccount>,
    ) -> Result<(), CoreError>;

    // ---- meta, hooks, config ------------------------------------------------------------------

    /// What `GET /api/meta` shows besides `deploy`: `{state, usage, cap, repos, updated}`, the
    /// scheduler's last word (what `worker.sh` wrote to `meta.json`).
    async fn meta(&self) -> Result<Value, CoreError>;

    /// An agent's hook, delivered by `femmed hook` (PTY-mode status: Claude hooks, Codex notify,
    /// the pi extension). `payload` is the object the CLI posted: the agent's own JSON with
    /// `agent`, `event` and `sid` (the `FEMME_SID` of the pane) set.
    ///
    /// Trust: the route is loopback-only, so the caller is some process of this user, which under
    /// bypass permissions can do anything else on the box anyway; `sid` is taken at its word. The
    /// rest of the payload is the agent's own JSON: `transcript_path` and the like are its claims,
    /// never paths to open without checking they are the session's own.
    async fn hook(&self, agent: Agent, payload: Value) -> Result<(), CoreError>;

    /// The parsed `.env`.
    fn config(&self) -> &Config;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_ids_parse_and_print() {
        let p = PaneRef::parse("instinct~w9:p1").unwrap();
        assert_eq!(
            p,
            PaneRef {
                machine: Some("instinct".into()),
                handle: "9".into(),
                no: 1
            }
        );
        assert_eq!(p.to_string(), "instinct~w9:p1");
        assert_eq!(p.ws().to_string(), "instinct~w9");
        assert_eq!(PaneRef::parse("wC:p12").unwrap().to_string(), "wC:p12");
        for bad in [
            "",
            "w9",
            "w9:",
            "w9:pA",
            "w9:p1;rm",
            "x9:p1",
            "~w9:p1",
            "a~b~w9:p1",
            "w9:p70000",
        ] {
            assert_eq!(PaneRef::parse(bad), None, "{bad}");
        }
        assert_eq!(
            WsRef::parse("w9"),
            Some(WsRef {
                machine: None,
                handle: "9".into()
            })
        );
        assert_eq!(
            WsRef::parse("instinct~w9").unwrap().to_string(),
            "instinct~w9"
        );
        assert_eq!(WsRef::parse("w9:p1"), None);
    }

    #[test]
    fn statuses_speak_the_boards_vocabulary() {
        use SessionStatus::*;
        assert_eq!(agent_status(Working, Attention::None), "working");
        assert_eq!(agent_status(Starting, Attention::None), "working");
        assert_eq!(agent_status(Idle, Attention::None), "idle");
        assert_eq!(agent_status(Idle, Attention::Done), "done");
        assert_eq!(agent_status(Blocked, Attention::Blocked), "blocked");
        assert_eq!(agent_status(Working, Attention::Blocked), "blocked");
        assert_eq!(agent_status(Paused, Attention::None), "idle");
        for gone in [Interrupted, Crashed, Failed, Closed] {
            assert_eq!(agent_status(gone, Attention::None), "unknown");
        }
    }

    #[test]
    fn errors_map_to_one_status_each() {
        assert_eq!(CoreError::NotFound.status(), StatusCode::NOT_FOUND);
        assert_eq!(CoreError::Busy.status(), StatusCode::CONFLICT);
        assert_eq!(
            CoreError::Conflict("x".into()).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(CoreError::Bad("x".into()).status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            CoreError::Internal("x".into()).status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(CoreError::Busy.to_string(), "agent_not_ready");
    }
}
