//! An in-memory [`Core`] for the router's tests (and, with the `test-util` feature, for anything
//! else that sits on the trait). It behaves like the real core wherever the router can tell:
//! `prompt` makes the agent `working` and refuses a blocked one, issue ids count up, key names are
//! unique, web sessions expire. What a test needs to see or arrange is public: sessions, screens,
//! one-shot errors, and what the router asked for (`prompts`, `inputs`, `created`, `closed`, `hooks`).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use femme_proto::rpc::{Answer, IssueField};
use femme_proto::state::{Agent, Attention, Issue, IssueStatus, Mode, PaneKind, SessionStatus};
use femme_proto::Sid;
use serde_json::{json, Value};

use crate::config::Config;
use crate::core::{
    ApiKey, Core, CoreError, CreateSession, GithubAccount, PaneInfo, PaneRef, Screen, SessionInfo,
};

/// See the module docs.
pub struct FakeCore {
    cfg: Config,
    st: Mutex<State>,
}

struct State {
    sessions: Vec<SessionInfo>,
    /// By compat pane id: (plain text, ANSI text).
    screens: HashMap<String, (String, String)>,
    issues: Vec<Issue>,
    web: HashMap<[u8; 32], i64>,
    keys: Vec<ApiKey>,
    meta: Value,
    errors: HashMap<&'static str, CoreError>,
    next_handle: u32,
    prompts: Vec<(String, String)>,
    inputs: Vec<(String, Vec<u8>)>,
    created: Vec<CreateSession>,
    closed: Vec<Sid>,
    hooks: Vec<(Agent, Value)>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

impl FakeCore {
    /// An empty core with `cfg`.
    pub fn new(cfg: Config) -> FakeCore {
        FakeCore {
            cfg,
            st: Mutex::new(State {
                sessions: Vec::new(),
                screens: HashMap::new(),
                issues: Vec::new(),
                web: HashMap::new(),
                keys: Vec::new(),
                meta: json!({}),
                errors: HashMap::new(),
                next_handle: 10,
                prompts: Vec::new(),
                inputs: Vec::new(),
                created: Vec::new(),
                closed: Vec::new(),
                hooks: Vec::new(),
            }),
        }
    }

    fn st(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A session to start from: an idle Claude in a TUI with one agent pane `p1`, handle as given.
    pub fn session(handle: &str) -> SessionInfo {
        SessionInfo {
            sid: Sid::new(),
            handle: handle.into(),
            machine: None,
            name: None,
            owner: "board".into(),
            agent: Agent::Claude,
            mode: Mode::Tui,
            cwd: "/".into(),
            label: None,
            status: SessionStatus::Idle,
            attention: Attention::None,
            panes: vec![PaneInfo {
                no: 1,
                kind: PaneKind::Agent,
                title: String::new(),
                cwd: None,
            }],
        }
    }

    pub fn add_session(&self, s: SessionInfo) {
        self.st().sessions.push(s);
    }

    /// Changes a session's status and attention (what a hook or the agent's own events would do).
    pub fn set_status(&self, sid: Sid, status: SessionStatus, attention: Attention) {
        if let Some(s) = self.st().sessions.iter_mut().find(|s| s.sid == sid) {
            s.status = status;
            s.attention = attention;
        }
    }

    /// What `screen` answers for the pane (`instinct~w1:p1`): plain text, and the text with colours.
    pub fn set_screen(&self, pane: &str, plain: &str, ansi: &str) {
        self.st()
            .screens
            .insert(pane.into(), (plain.into(), ansi.into()));
    }

    pub fn set_meta(&self, meta: Value) {
        self.st().meta = meta;
    }

    /// Makes the next call of `method` (the trait method's name) fail with `err`, once.
    pub fn inject_error(&self, method: &'static str, err: CoreError) {
        self.st().errors.insert(method, err);
    }

    pub fn add_issue(&self, issue: Issue) {
        self.st().issues.push(issue);
    }

    /// `(pane id, text)` of every `prompt`, oldest first.
    pub fn prompts(&self) -> Vec<(String, String)> {
        self.st().prompts.clone()
    }

    /// `(pane id, bytes)` of every `input`.
    pub fn inputs(&self) -> Vec<(String, Vec<u8>)> {
        self.st().inputs.clone()
    }

    /// Every `create_session` request.
    pub fn created(&self) -> Vec<CreateSession> {
        self.st().created.clone()
    }

    /// Every `close_session`.
    pub fn closed(&self) -> Vec<Sid> {
        self.st().closed.clone()
    }

    /// Every `hook`.
    pub fn hooks(&self) -> Vec<(Agent, Value)> {
        self.st().hooks.clone()
    }

    /// The open sessions, as `terminals` lists them.
    pub fn sessions(&self) -> Vec<SessionInfo> {
        self.st()
            .sessions
            .iter()
            .filter(|s| s.status != SessionStatus::Closed)
            .cloned()
            .collect()
    }

    fn sid_known(&self, sid: Sid) -> Result<(), CoreError> {
        match self.sessions().iter().any(|s| s.sid == sid) {
            true => Ok(()),
            false => Err(CoreError::NotFound),
        }
    }

    fn check(&self, method: &'static str) -> Result<(), CoreError> {
        match self.st().errors.remove(method) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

fn find<'a>(st: &'a mut State, pane: &PaneRef) -> Result<&'a mut SessionInfo, CoreError> {
    st.sessions
        .iter_mut()
        .find(|s| {
            s.status != SessionStatus::Closed
                && s.handle == pane.handle
                && s.machine == pane.machine
                && s.panes.iter().any(|p| p.no == pane.no)
        })
        .ok_or(CoreError::NotFound)
}

#[async_trait]
impl Core for FakeCore {
    async fn terminals(&self) -> Result<Vec<SessionInfo>, CoreError> {
        self.check("terminals")?;
        Ok(self.sessions())
    }

    async fn session_by_name(&self, name: &str) -> Result<Option<SessionInfo>, CoreError> {
        self.check("session_by_name")?;
        Ok(self
            .sessions()
            .into_iter()
            .find(|s| s.name.as_deref() == Some(name)))
    }

    async fn create_session(&self, req: CreateSession) -> Result<SessionInfo, CoreError> {
        self.check("create_session")?;
        let mut st = self.st();
        st.next_handle += 1;
        let kind = if req.agent == Agent::Shell {
            PaneKind::Shell
        } else {
            PaneKind::Agent
        };
        let s = SessionInfo {
            sid: Sid::new(),
            handle: st.next_handle.to_string(),
            // Sessions of the agents API (they carry a name) live in the `instinct` machine.
            machine: req.name.is_some().then(|| "instinct".to_string()),
            name: req.name.clone(),
            owner: req.owner.clone(),
            agent: req.agent,
            mode: req.mode,
            cwd: req.cwd.clone(),
            label: req.label.clone(),
            status: SessionStatus::Idle,
            attention: Attention::None,
            panes: vec![PaneInfo {
                no: 1,
                kind,
                title: String::new(),
                cwd: Some(req.cwd.clone()),
            }],
        };
        st.created.push(req);
        st.sessions.push(s.clone());
        Ok(s)
    }

    async fn close_session(&self, sid: Sid) -> Result<(), CoreError> {
        self.check("close_session")?;
        let mut st = self.st();
        let s = st
            .sessions
            .iter_mut()
            .find(|s| s.sid == sid && s.status != SessionStatus::Closed)
            .ok_or(CoreError::NotFound)?;
        s.status = SessionStatus::Closed;
        st.closed.push(sid);
        Ok(())
    }

    async fn screen(&self, pane: &PaneRef, ansi: bool) -> Result<Screen, CoreError> {
        self.check("screen")?;
        let mut st = self.st();
        let status = find(&mut st, pane)?.agent_status().map(String::from);
        let id = pane.to_string();
        let (plain, color) = st.screens.get(&id).cloned().unwrap_or_else(|| {
            (
                format!("screen of {id}"),
                format!("\u{1b}[32mscreen of {id}\u{1b}[0m"),
            )
        });
        Ok(Screen {
            text: if ansi { color } else { plain },
            status,
        })
    }

    async fn prompt(&self, pane: &PaneRef, text: &str) -> Result<(), CoreError> {
        self.check("prompt")?;
        let mut st = self.st();
        let s = find(&mut st, pane)?;
        if s.agent == Agent::Shell
            || matches!(s.status, SessionStatus::Blocked | SessionStatus::Starting)
            || s.attention == Attention::Blocked
        {
            return Err(CoreError::Busy);
        }
        s.status = SessionStatus::Working;
        s.attention = Attention::None;
        st.prompts.push((pane.to_string(), text.to_string()));
        Ok(())
    }

    async fn input(&self, pane: &PaneRef, bytes: &[u8]) -> Result<(), CoreError> {
        self.check("input")?;
        let mut st = self.st();
        find(&mut st, pane)?;
        st.inputs.push((pane.to_string(), bytes.to_vec()));
        Ok(())
    }

    async fn interrupt(&self, sid: Sid) -> Result<(), CoreError> {
        self.check("interrupt")?;
        self.sid_known(sid)
    }

    async fn background(&self, sid: Sid, _tool_use_id: Option<&str>) -> Result<(), CoreError> {
        self.check("background")?;
        self.sid_known(sid)
    }

    async fn answer(&self, sid: Sid, _request_id: &str, _answer: Answer) -> Result<(), CoreError> {
        self.check("answer")?;
        self.sid_known(sid)
    }

    async fn set_model(&self, sid: Sid, _model: &str) -> Result<(), CoreError> {
        self.check("set_model")?;
        self.sid_known(sid)
    }

    async fn takeover(&self, sid: Sid, to: Mode) -> Result<(), CoreError> {
        self.check("takeover")?;
        let mut st = self.st();
        let s = st
            .sessions
            .iter_mut()
            .find(|s| s.sid == sid && s.status != SessionStatus::Closed)
            .ok_or(CoreError::NotFound)?;
        s.mode = to;
        Ok(())
    }

    async fn issues(&self) -> Result<Vec<Issue>, CoreError> {
        self.check("issues")?;
        Ok(self.st().issues.clone())
    }

    async fn issue_create(&self, repo: &str, title: &str, body: &str) -> Result<Issue, CoreError> {
        self.check("issue_create")?;
        let mut st = self.st();
        let id = st.issues.iter().map(|i| i.id).max().unwrap_or(0) + 1;
        let now = now_ms();
        let issue = Issue {
            id,
            rev: 1,
            repo: repo.into(),
            title: title.into(),
            body: body.into(),
            status: IssueStatus::Open,
            pr_url: None,
            note: None,
            session: None,
            resume_at: None,
            created: now,
            updated: now,
        };
        st.issues.push(issue.clone());
        Ok(issue)
    }

    async fn issue_patch(&self, id: u64, fields: Vec<IssueField>) -> Result<Issue, CoreError> {
        self.check("issue_patch")?;
        let mut st = self.st();
        let issue = st
            .issues
            .iter_mut()
            .find(|i| i.id == id)
            .ok_or(CoreError::NotFound)?;
        for f in fields {
            match f {
                IssueField::Repo(s) => issue.repo = s,
                IssueField::Title(s) => issue.title = s,
                IssueField::Body(s) => issue.body = s,
                IssueField::Status(s) => issue.status = s,
                IssueField::Note(s) => issue.note = s,
                IssueField::PrUrl(s) => issue.pr_url = s,
                IssueField::Session(s) => issue.session = s,
                IssueField::ResumeAt(s) => issue.resume_at = s,
            }
        }
        issue.rev += 1;
        issue.updated = now_ms();
        Ok(issue.clone())
    }

    async fn issue_delete(&self, id: u64) -> Result<(), CoreError> {
        self.check("issue_delete")?;
        let mut st = self.st();
        let before = st.issues.len();
        st.issues.retain(|i| i.id != id);
        if st.issues.len() == before {
            Err(CoreError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn web_session_valid(&self, token_sha256: [u8; 32]) -> bool {
        self.st()
            .web
            .get(&token_sha256)
            .is_some_and(|exp| *exp > now_ms())
    }

    async fn web_session_create(
        &self,
        token_sha256: [u8; 32],
        expires: i64,
    ) -> Result<(), CoreError> {
        self.check("web_session_create")?;
        self.st().web.insert(token_sha256, expires);
        Ok(())
    }

    async fn web_session_delete(&self, token_sha256: [u8; 32]) -> Result<(), CoreError> {
        self.st().web.remove(&token_sha256);
        Ok(())
    }

    async fn api_keys(&self) -> Result<Vec<ApiKey>, CoreError> {
        self.check("api_keys")?;
        Ok(self.st().keys.clone())
    }

    async fn api_key_create(&self, key: ApiKey) -> Result<(), CoreError> {
        self.check("api_key_create")?;
        let mut st = self.st();
        if st.keys.iter().any(|k| k.name == key.name) {
            return Err(CoreError::Conflict("a key with that name exists".into()));
        }
        st.keys.push(key);
        Ok(())
    }

    async fn api_key_delete(&self, name: &str) -> Result<(), CoreError> {
        self.check("api_key_delete")?;
        let mut st = self.st();
        let before = st.keys.len();
        st.keys.retain(|k| k.name != name);
        if st.keys.len() == before {
            Err(CoreError::NotFound)
        } else {
            Ok(())
        }
    }

    async fn api_key_set_github(
        &self,
        name: &str,
        github: Option<GithubAccount>,
    ) -> Result<(), CoreError> {
        self.check("api_key_set_github")?;
        let mut st = self.st();
        let key = st
            .keys
            .iter_mut()
            .find(|k| k.name == name)
            .ok_or(CoreError::NotFound)?;
        let g = github.unwrap_or(GithubAccount {
            token: String::new(),
            login: String::new(),
            name: String::new(),
            email: String::new(),
        });
        let some = |s: String| (!s.is_empty()).then_some(s);
        key.github_token = some(g.token);
        key.github_login = some(g.login);
        key.git_name = some(g.name);
        key.git_email = some(g.email);
        Ok(())
    }

    async fn meta(&self) -> Result<Value, CoreError> {
        self.check("meta")?;
        Ok(self.st().meta.clone())
    }

    async fn hook(&self, agent: Agent, payload: Value) -> Result<(), CoreError> {
        self.check("hook")?;
        self.st().hooks.push((agent, payload));
        Ok(())
    }

    fn config(&self) -> &Config {
        &self.cfg
    }
}
