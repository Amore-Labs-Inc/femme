//! The desktop's copy of what the daemon says: the state of the link, and the sessions and panes of
//! the global scope. [`Model`] is plain data, fed by the client's events (and tested without gpui);
//! [`Net`] is the entity that drains the client into it and runs requests for the UI.
//!
//! There are two daemons, two [`BoxId`]s (docs/plan-local.md): the remote box, and `femmed local` on
//! this machine over a unix socket. One `Model` holds the sessions of both (session ids are ULIDs,
//! unique across boxes) and remembers which box each came from, so a request about a session goes
//! to the client of its box.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use femme_client::{Client, Event, Link as Transport};
use femme_proto::rpc::{Req, Resp, Scope};
use femme_proto::state::{Agent, Entity as Kind, Pane, Row, Session, SessionStatus};
use femme_proto::{PaneId, Sid};
use gpui::{Context, EventEmitter, Task};

use crate::connect::{self, Connect};
use crate::transcript::outbox::Outbox;

/// How many characters of a tab's label are shown.
const LABEL: usize = 28;

/// Which daemon: the remote box, or `femmed local` on this machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum BoxId {
    /// The box the app was told to connect to (QUIC, else ssh).
    #[default]
    Remote,
    /// `femmed local`, over `<state>/femmed.sock`.
    Local,
}

/// Where the connection stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Link {
    /// Not connected yet.
    #[default]
    Connecting,
    /// Connected, over this transport.
    Up(Transport),
    /// The daemon is out of reach, in the transport's words.
    Down(String),
}

/// How far the daemon's list of sessions has got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Load {
    /// Nothing of the global scope has arrived yet.
    #[default]
    Waiting,
    /// A `Reset` dropped the lists: the next `Changes` is the daemon's full set again.
    Rebuilding,
    /// The list is in; what follows are changes to it.
    Done,
}

/// What the daemon last said.
#[derive(Debug, Default)]
pub struct Model {
    /// The connection.
    pub link: Link,
    /// QUIC refused this device while ssh worked: the device is not enrolled.
    pub quic_error: Option<String>,
    /// Smoothed round trip of the connection, in ms.
    pub rtt_ms: Option<u32>,
    /// Every session of the global scope (closed ones too, until the daemon forgets them).
    pub sessions: BTreeMap<Sid, Session>,
    /// The session the centre shows (docs/desktop-ui.md §sessions). It follows the session it names:
    /// when that one closes it moves to the newest live session, or to `None`; a Claude session that
    /// appears while a shell is on show (`claude` typed in it) takes its place ([`Model::apply`]).
    /// It may name a session the daemon has not listed yet: the answer to creating one comes first.
    pub current: Option<Sid>,
    /// Every pane of the global scope.
    pub panes: BTreeMap<PaneId, Pane>,
    /// Whether the daemon's list has come in. The first `Changes` of the global scope is always sent
    /// (empty if need be), so [`Load::Done`] means "this is everything", and after a `Reset` it is
    /// [`Load::Rebuilding`] until the full set follows.
    pub load: Load,
    /// Prompts sent and not yet echoed by the daemon, drawn faded at the end of their transcript.
    /// The composer adds them; the `UserMessage` rows of a session's scope take them off.
    pub outbox: Outbox,
    /// The box each session came from (or was created on). Never pruned: a few ULIDs.
    pub boxes: BTreeMap<Sid, BoxId>,
    /// The local daemon's connection; `None` when there is no local box at all.
    pub local_link: Option<Link>,
    /// [`Model::load`] of the local box.
    pub local_load: Load,
}

fn is_live(s: &Session) -> bool {
    s.closed.is_none() && s.status != SessionStatus::Closed
}

/// Whether something runs in `s`, or will again (a Claude restarting after an exit is `Starting`):
/// it is live, and its program has not crashed, failed or lost its holder. The daemon starts nothing
/// in such a session again; it stays listed (and on show, if it was) until the user closes it, but
/// it is not a session to open at start or to put on show by itself.
pub(crate) fn is_running(s: &Session) -> bool {
    is_live(s)
        && !matches!(
            s.status,
            SessionStatus::Interrupted | SessionStatus::Crashed | SessionStatus::Failed
        )
}

/// The pane a tombstone names: its id is `<sid>:<no>`.
fn pane_id(text: &str) -> Option<PaneId> {
    let (sid, no) = text.rsplit_once(':')?;
    Some(PaneId {
        sid: sid.parse().ok()?,
        no: no.parse().ok()?,
    })
}

/// Whether `row` is the tombstone of session `sid`.
fn is_tombstone_of(row: &Row, sid: Sid) -> bool {
    matches!(row, Row::Tombstone { entity: Kind::Session, id, .. }
        if id.parse::<Sid>().is_ok_and(|t| t == sid))
}

/// What a session is called: its name, else its label, else the last component of its directory.
pub fn title(s: &Session) -> String {
    let filled = |o: &Option<String>| o.clone().filter(|t| !t.is_empty());
    filled(&s.name)
        .or_else(|| filled(&s.label))
        .or_else(|| {
            let tail = s.cwd.trim_end_matches('/').rsplit('/').next()?;
            Some(tail.to_owned()).filter(|t| !t.is_empty())
        })
        .unwrap_or_else(|| s.handle.clone())
}

/// The session a request is about, if it names one.
fn req_sid(req: &Req) -> Option<Sid> {
    match req {
        Req::Subscribe {
            scope: Scope::Session(sid),
            ..
        }
        | Req::Unsubscribe {
            scope: Scope::Session(sid),
        }
        | Req::TranscriptPage { sid, .. }
        | Req::SessionClose { sid }
        | Req::Prompt { sid, .. }
        | Req::Interrupt { sid }
        | Req::Background { sid, .. }
        | Req::AnswerQuestion { sid, .. }
        | Req::Takeover { sid, .. }
        | Req::SetModel { sid, .. }
        | Req::PaneOpen { sid }
        | Req::FsList { sid, .. }
        | Req::FsRead { sid, .. }
        | Req::FsWrite { sid, .. }
        | Req::FsZip { sid, .. }
        | Req::CyclePermission { sid }
        | Req::SessionLabel { sid, .. }
        | Req::Show { sid, .. }
        | Req::Resend { sid, .. }
        | Req::TakeBack { sid } => Some(*sid),
        Req::PaneClose { pane } => Some(pane.sid),
        _ => None,
    }
}

impl Model {
    /// Takes in one event of the remote box; whether anything the UI shows changed.
    pub fn apply(&mut self, ev: &Event) -> bool {
        self.apply_from(BoxId::Remote, ev)
    }

    /// The box `sid` lives on; a session nobody has placed is the remote box's.
    pub fn box_of(&self, sid: Sid) -> BoxId {
        self.boxes.get(&sid).copied().unwrap_or_default()
    }

    /// Whether `sid` runs on this machine (`femmed local`).
    pub fn is_local(&self, sid: Sid) -> bool {
        self.box_of(sid) == BoxId::Local
    }

    /// The box a request goes to: that of the session it names; blobs (a transcript's pictures, a
    /// composer's attachments) are those of the session on show; anything else is the remote box's.
    pub fn route(&self, req: &Req) -> BoxId {
        let sid = req_sid(req).or(match req {
            Req::BlobGet { .. } | Req::BlobPut { .. } | Req::Upload { .. } => self.current,
            _ => None,
        });
        sid.map_or(BoxId::Remote, |s| self.box_of(s))
    }

    fn load_mut(&mut self, b: BoxId) -> &mut Load {
        match b {
            BoxId::Remote => &mut self.load,
            BoxId::Local => &mut self.local_load,
        }
    }

    /// Takes in one event of box `b`'s client; whether anything the UI shows changed.
    pub fn apply_from(&mut self, b: BoxId, ev: &Event) -> bool {
        match ev {
            Event::Connected {
                link, quic_error, ..
            } => {
                match b {
                    BoxId::Remote => {
                        self.link = Link::Up(*link);
                        self.quic_error.clone_from(quic_error);
                    }
                    BoxId::Local => self.local_link = Some(Link::Up(*link)),
                }
                true
            }
            Event::Disconnected { reason } => {
                let down = Link::Down(reason.clone());
                match b {
                    BoxId::Remote => self.link = down,
                    BoxId::Local => self.local_link = Some(down),
                }
                true
            }
            Event::Changes {
                scope: Scope::Global,
                rows,
                ..
            } => {
                let load = *self.load_mut(b);
                // Only this box's full set can say that the session on show is gone.
                let rebuilt =
                    load == Load::Rebuilding && self.current.is_none_or(|c| self.box_of(c) == b);
                // Sessions new to the list. The first list and the full set after a `Reset` are what
                // was there already, not arrivals.
                let listed = load == Load::Done;
                for r in rows {
                    if let Row::Session(s) = r {
                        self.boxes.insert(s.id, b);
                    }
                }
                let arrived: Vec<Sid> = rows
                    .iter()
                    .filter_map(|r| match r {
                        Row::Session(s) if listed && !self.sessions.contains_key(&s.id) => {
                            Some(s.id)
                        }
                        _ => None,
                    })
                    .collect();
                let changed = rows.iter().fold(false, |any, row| self.row(row) | any);
                *self.load_mut(b) = Load::Done;
                self.keep_current(rows, rebuilt) | self.follow_claude(&arrived) | changed
            }
            // The daemon cannot vouch for what we hold: its full set follows. The session on show
            // stays named, and stays if the full set lists it.
            Event::Reset {
                scope: Scope::Global,
            } => {
                let boxes = &self.boxes;
                let other = |sid: &Sid| boxes.get(sid).copied().unwrap_or_default() != b;
                self.sessions.retain(|sid, _| other(sid));
                self.panes.retain(|p, _| other(&p.sid));
                *self.load_mut(b) = Load::Rebuilding;
                true
            }
            Event::Changes {
                scope: Scope::Session(sid),
                rows,
                ..
            } => self.outbox.echo(*sid, rows),
            // Issue rows and streaming text are not shown here; the transcript keeps its items.
            _ => false,
        }
    }

    /// One row: an upsert that keeps the higher rev, or a tombstone.
    fn row(&mut self, row: &Row) -> bool {
        match row {
            Row::Session(s) if self.sessions.get(&s.id).is_none_or(|old| old.rev < s.rev) => {
                self.sessions.insert(s.id, s.clone());
                true
            }
            Row::Pane(p) if self.panes.get(&p.id).is_none_or(|old| old.rev < p.rev) => {
                self.panes.insert(p.id, p.clone());
                true
            }
            Row::Tombstone {
                entity: Kind::Session,
                id,
                ..
            } => id.parse::<Sid>().is_ok_and(|sid| {
                let (sessions, panes) = (self.sessions.len(), self.panes.len());
                self.sessions.remove(&sid);
                self.panes.retain(|p, _| p.sid != sid);
                (sessions, panes) != (self.sessions.len(), self.panes.len())
            }),
            Row::Tombstone {
                entity: Kind::Pane,
                id,
                ..
            } => pane_id(id).is_some_and(|p| self.panes.remove(&p).is_some()),
            _ => false,
        }
    }

    /// Moves [`Model::current`] off a session that is gone: its row says it is closed, a tombstone
    /// names it, or (`rebuilt`: the first `Changes` after a `Reset`) the daemon's full set does not
    /// have it. A session that has not been listed yet is not gone. Whether `current` changed.
    fn keep_current(&mut self, rows: &[Row], rebuilt: bool) -> bool {
        let Some(sid) = self.current else {
            return false;
        };
        let gone = match self.sessions.get(&sid) {
            Some(s) => !is_live(s),
            None => rebuilt || rows.iter().any(|r| is_tombstone_of(r, sid)),
        };
        if gone {
            self.current = self.live().first().map(|s| s.id);
        }
        gone
    }

    /// A Claude session that has just `arrived` takes the centre when it shows a live shell: the user
    /// typed `claude` there, and `femmed open` made it. Any other session on show, or none (the
    /// opening at start decides that), is left alone. An arrival happens once per session, so a
    /// session the user moves away from is not shown again. Whether `current` changed.
    fn follow_claude(&mut self, arrived: &[Sid]) -> bool {
        if !self
            .current_session()
            .is_some_and(|s| s.agent == Agent::Shell && is_live(s))
        {
            return false;
        }
        let claude = arrived
            .iter()
            .filter_map(|sid| self.sessions.get(sid))
            .filter(|s| s.agent == Agent::Claude && is_running(s))
            .max_by_key(|s| (s.created, s.id))
            .map(|s| s.id);
        if claude.is_some() {
            self.current = claude;
        }
        claude.is_some()
    }

    /// The session the centre shows, when the daemon has listed it.
    pub fn current_session(&self) -> Option<&Session> {
        self.sessions.get(&self.current?)
    }

    /// Whether `sid` is an agent session (Claude, Codex, Pi) as opposed to a plain shell. A session
    /// the daemon has not listed is not one.
    pub fn is_agent(&self, sid: Sid) -> bool {
        self.sessions
            .get(&sid)
            .is_some_and(|s| s.agent != Agent::Shell)
    }

    /// Makes `sid` the session the centre shows (`None`: nothing); whether that changed anything. A
    /// session the daemon lists as closed is not shown: nothing changes.
    pub fn select(&mut self, sid: Option<Sid>) -> bool {
        if sid.is_some_and(|s| self.sessions.get(&s).is_some_and(|s| !is_live(s))) {
            return false;
        }
        let changed = self.current != sid;
        self.current = sid;
        changed
    }

    /// The sessions that are not closed, newest first.
    pub fn live(&self) -> Vec<&Session> {
        let mut v: Vec<_> = self.sessions.values().filter(|s| is_live(s)).collect();
        v.sort_by_key(|s| std::cmp::Reverse((s.created, s.id)));
        v
    }

    /// The panes that get a tab: those of live sessions, oldest session first (a new one lands at
    /// the right), then by number.
    pub fn tabs(&self) -> Vec<PaneId> {
        let mut v: Vec<_> = self
            .panes
            .keys()
            .filter_map(|p| Some((self.sessions.get(&p.sid).filter(|s| is_live(s))?, *p)))
            .collect();
        v.sort_by_key(|(s, p)| (s.created, s.id, p.no));
        v.into_iter().map(|(_, p)| p).collect()
    }

    /// A tab's label: the title the program gave its terminal, else the pane's own, else
    /// `<session> · <no>`; cut at [`LABEL`] characters.
    pub fn tab_label(&self, pane: PaneId, term_title: Option<String>) -> String {
        let named = |t: Option<String>| t.filter(|t| !t.is_empty());
        let given =
            named(term_title).or_else(|| named(self.panes.get(&pane).map(|p| p.title.clone())));
        let label = given.unwrap_or_else(|| {
            let session = self.sessions.get(&pane.sid).map(title).unwrap_or_default();
            format!("{session} · {}", pane.no)
        });
        match label.char_indices().nth(LABEL) {
            Some((end, _)) => format!("{}…", &label[..end]),
            None => label,
        }
    }

    /// The nav bar's word for the link.
    pub fn status(&self) -> String {
        match &self.link {
            Link::Connecting => "connecting…".into(),
            Link::Up(Transport::Quic) => self
                .rtt_ms
                .map_or_else(|| "quic".into(), |ms| format!("quic {ms} ms")),
            Link::Up(Transport::Ssh) => "ssh".into(),
            Link::Up(Transport::Local) => "local".into(),
            Link::Down(why) => format!("offline: {why}"),
        }
    }
}

/// The nav bar's line: the link, then what to do when QUIC refused this device (the hint names the
/// command that enrolls it) and the last request that failed.
pub fn status_line(model: &Model, enroll: &str, error: Option<&str>) -> String {
    status_line_with(model, enroll, None, error)
}

/// [`status_line`] with what a request in flight is doing ("starting claude…") between the hint and
/// the error.
pub fn status_line_with(
    model: &Model,
    enroll: &str,
    busy: Option<&str>,
    error: Option<&str>,
) -> String {
    let mut line = model.status();
    match &model.local_link {
        Some(Link::Up(_)) => line.push_str(" · local"),
        Some(_) => line.push_str(" · local: off"),
        None => {}
    }
    if model.quic_error.is_some() {
        line.push_str(&format!(" | quic refused this device, enroll it: {enroll}"));
    }
    if let Some(b) = busy {
        line.push_str(&format!(" | {b}"));
    }
    if let Some(e) = error {
        line.push_str(&format!(" | {e}"));
    }
    line
}

/// A running client and what the shell needs besides it.
pub struct Started {
    /// The client; it is already dialling.
    pub client: Arc<Client>,
    /// The client of `femmed local` on this machine, when there is one; it is already dialling.
    pub local: Option<Arc<Client>>,
    /// The `femmed local` this app started, stopped when it is dropped (at exit).
    pub daemon: Option<LocalDaemon>,
    /// The command that enrolls this device with the daemon, for the hint.
    pub enroll: String,
}

/// Starts the client for `c` with this device's identity and a fresh client instance (one per
/// process: terminals know it across reconnects), and the client of `femmed local` (started here
/// unless its socket is already live). Call inside the side runtime: the clients spawn their tasks
/// on it.
pub fn start(c: &Connect) -> anyhow::Result<Started> {
    let device = connect::identity(&c.identity)?;
    let spki = device
        .spki_sha256()
        .context("the device certificate has no public key")?;
    let mut cfg = femme_client::Config::new(device, ulid::Ulid::generate());
    cfg.quic = c.quic;
    cfg.ssh_host.clone_from(&c.ssh);
    let (local, daemon) = match start_local(c) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("femme-desktop: no local sessions: {e:#}");
            (None, None)
        }
    };
    Ok(Started {
        client: Arc::new(Client::start(cfg)),
        local,
        daemon,
        enroll: connect::enroll_line(c.ssh.as_deref(), &spki),
    })
}

/// Where `femmed local` keeps its store, socket and log: `$XDG_STATE_HOME/femme-local`, else
/// `~/.local/state/femme-local` (what `femmed local` takes by default).
fn local_state() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/state")))?;
    Some(base.join("femme-local"))
}

/// The `femmed` to run: the one beside this executable, else the first on `PATH`.
fn femmed_bin() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("femmed")));
    beside.filter(|p| p.is_file()).or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join("femmed"))
            .find(|p| p.is_file())
    })
}

/// `femmed local`, running as this app's child; stopped (SIGTERM, so it ends its sessions cleanly)
/// when dropped.
pub struct LocalDaemon(std::process::Child);

impl Drop for LocalDaemon {
    fn drop(&mut self) {
        let pid = self.0.id().to_string();
        let asked = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .is_ok_and(|s| s.success());
        if !asked {
            self.0.kill().ok();
        }
        self.0.wait().ok();
    }
}

/// The client of `femmed local` and the daemon itself when this call started it. A live socket is
/// used as it is (a `systemd --user` unit, another window); otherwise `femmed local` is started with
/// its output in `<state>/femmed.log`. Without a `femmed` to run there is no local box: `(None, None)`.
fn start_local(c: &Connect) -> anyhow::Result<(Option<Arc<Client>>, Option<LocalDaemon>)> {
    let state = local_state().context("no home directory for the local daemon's state")?;
    let sock = state.join("femmed.sock");
    let daemon = if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
        None
    } else {
        let Some(bin) = femmed_bin() else {
            eprintln!("femme-desktop: no femmed beside the app or on PATH: no local sessions");
            return Ok((None, None));
        };
        std::fs::create_dir_all(&state).with_context(|| format!("create {}", state.display()))?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(state.join("femmed.log"))
            .context("open femmed.log")?;
        let child = std::process::Command::new(&bin)
            .arg("local")
            .arg("--state")
            .arg(&state)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .with_context(|| format!("start {} local", bin.display()))?;
        Some(LocalDaemon(child))
    };
    // The socket may not be up yet: the client dials again with its back-off until it is.
    let mut cfg =
        femme_client::Config::new(connect::identity(&c.identity)?, ulid::Ulid::generate());
    cfg.unix = Some(sock);
    Ok((Some(Arc::new(Client::start(cfg))), daemon))
}

/// Every event of the daemon's client, re-emitted for views that keep more than [`Model`] does (the
/// transcript's items and deltas, the composer's answers). Subscribe with `cx.subscribe(&net, …)`;
/// `Model` has already applied the event when it arrives.
/// The second field is the box whose client said it.
pub struct ClientEvent(pub Event, pub BoxId);

impl EventEmitter<ClientEvent> for Net {}

/// Where the choice of a first session ([`crate::session::open_at_start`]) stands: it is made once,
/// however many times the shell asks (every reconnect says `Connected` again).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Opening {
    /// Not started, or given up on before anything was decided or made: the next call may start.
    #[default]
    Idle,
    /// A call is waiting for the daemon's list or for the daemon's answer.
    Running,
    /// Decided; a session is on show or was made. Later calls do nothing.
    Done,
}

impl Opening {
    /// Claims the opening: true for the first caller only, until it [`end`](Opening::end)s without
    /// having decided.
    pub(crate) fn begin(&mut self) -> bool {
        let first = *self == Opening::Idle;
        if first {
            *self = Opening::Running;
        }
        first
    }

    /// The caller is through: `done` when it decided, else (it gave up, or nothing could be made)
    /// the next call may try again.
    pub(crate) fn end(&mut self, done: bool) {
        *self = if done { Opening::Done } else { Opening::Idle };
    }
}

/// The client of the daemon, as an entity the views observe.
pub struct Net {
    /// What the daemon said.
    pub model: Model,
    /// The last request that failed, until one succeeds.
    pub error: Option<String>,
    /// The line that enrolls this device, for the hint when QUIC refuses it.
    pub enroll: String,
    /// What the app is waiting for the daemon to do for it ("starting claude…"): in the status line.
    starting: Option<String>,
    /// How far the choice of a first session has got.
    pub(crate) opening: Opening,
    /// The remote box's client.
    client: Arc<Client>,
    /// The client of `femmed local`, when there is a local box.
    local: Option<Arc<Client>>,
    rt: tokio::runtime::Handle,
    _tasks: Vec<Task<()>>,
}

impl Net {
    /// Drains the events of `client` (the remote box) and of `local` (`femmed local`, if any) into
    /// the model and subscribes both to the global scope.
    pub fn new(
        client: Arc<Client>,
        local: Option<Arc<Client>>,
        rt: tokio::runtime::Handle,
        enroll: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut tasks = vec![drain(&client, BoxId::Remote, &rt, cx)];
        tasks.extend(local.iter().map(|l| drain(l, BoxId::Local, &rt, cx)));
        tasks.push(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            if this.update(cx, |n, cx| n.poll(cx)).is_err() {
                break;
            }
        }));
        let model = Model {
            local_link: local.as_ref().map(|_| Link::Connecting),
            ..Model::default()
        };
        Self {
            model,
            error: None,
            enroll,
            starting: None,
            opening: Opening::default(),
            client,
            local,
            rt,
            _tasks: tasks,
        }
    }

    /// The round trip is not an event: look at it once a second, and redraw only when it moved.
    fn poll(&mut self, cx: &mut Context<Self>) {
        let rtt = self
            .client
            .rtt()
            .map(|d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX));
        if rtt != self.model.rtt_ms {
            self.model.rtt_ms = rtt;
            if self.model.link == Link::Up(Transport::Quic) {
                cx.notify();
            }
        }
    }

    /// The remote box's client, for what only it has (accounts, pull requests, GitHub).
    pub fn client(&self) -> Arc<Client> {
        self.client.clone()
    }

    /// The client of box `b`; `None` for a local box this app has not got.
    pub fn client_of(&self, b: BoxId) -> Option<Arc<Client>> {
        match b {
            BoxId::Remote => Some(self.client.clone()),
            BoxId::Local => self.local.clone(),
        }
    }

    /// The client of the box session `sid` lives on.
    pub fn client_for(&self, sid: Sid) -> Arc<Client> {
        self.client_of(self.model.box_of(sid))
            .unwrap_or_else(|| self.client.clone())
    }

    /// The client `req` goes to ([`Model::route`]).
    pub fn client_for_req(&self, req: &Req) -> Arc<Client> {
        self.client_of(self.model.route(req))
            .unwrap_or_else(|| self.client.clone())
    }

    /// Every client with its box: the remote one first.
    pub fn clients(&self) -> Vec<(BoxId, Arc<Client>)> {
        let mut v = vec![(BoxId::Remote, self.client.clone())];
        v.extend(self.local.clone().map(|l| (BoxId::Local, l)));
        v
    }

    /// Tells every daemon whether the window is out of sight (they ping less often then).
    pub fn set_background(&self, background: bool) {
        for (_, c) in self.clients() {
            c.set_background(background);
        }
    }

    /// The session the centre shows.
    pub fn current(&self) -> Option<Sid> {
        self.model.current
    }

    /// Makes `sid` the session the centre shows (`None`: nothing); observers of `Net` are told. A
    /// session the daemon lists as closed is not shown ([`Model::select`]).
    pub fn set_current(&mut self, sid: Option<Sid>, cx: &mut Context<Self>) {
        if self.model.select(sid) {
            cx.notify();
        }
    }

    /// Connects again, now (the menu's "reconnect"): QUIC moves to a fresh UDP socket and the
    /// connection, its streams and the terminals on them carry on; on ssh the connection is dropped
    /// and dialled at once; while down it stops waiting out the back-off. The client keeps its
    /// subscriptions and resubscribes by itself, so nothing here is forgotten. A hard drop of a
    /// healthy QUIC connection is not something the client offers.
    pub fn reconnect(&self) {
        // The new socket is registered with the side runtime's reactor, which the UI thread is not in.
        let _in_rt = self.rt.enter();
        self.client.rebind();
    }

    /// The side runtime the client lives on.
    pub fn rt(&self) -> tokio::runtime::Handle {
        self.rt.clone()
    }

    /// The nav bar's line: the link, what the app is starting, the last request that failed.
    pub fn status_line(&self) -> String {
        status_line_with(
            &self.model,
            &self.enroll,
            self.starting.as_deref(),
            self.error.as_deref(),
        )
    }

    /// What the app has asked the daemon to start and waits for ("starting claude…"), if anything.
    pub fn starting(&self) -> Option<&str> {
        self.starting.as_deref()
    }

    /// Says what the app waits for (`None`: nothing), in the status line.
    pub fn set_starting(&mut self, what: Option<String>, cx: &mut Context<Self>) {
        if self.starting != what {
            self.starting = what;
            cx.notify();
        }
    }

    /// Runs `req` on the side runtime, on the box it is about ([`Model::route`]). The answer is
    /// `None` when the request failed, and the reason waits in [`Net::error`] for the nav bar.
    pub fn run(&mut self, req: Req, cx: &mut Context<Self>) -> Task<Option<Resp>> {
        let b = self.model.route(&req);
        self.run_on(b, req, cx)
    }

    /// [`Net::run`] on box `b` (a new session is made where the user chose).
    pub fn run_on(&mut self, b: BoxId, req: Req, cx: &mut Context<Self>) -> Task<Option<Resp>> {
        let sent = self.try_run_on(b, req, cx);
        cx.spawn(async move |this, cx| {
            let answer = sent.await;
            let error = answer.as_ref().err().cloned();
            this.update(cx, |n, cx| {
                n.error = error;
                cx.notify();
            })
            .ok()?;
            answer.ok()
        })
    }

    /// Runs `req` on the side runtime and gives the answer or the reason it failed, leaving
    /// [`Net::error`] alone: for a caller that has another plan when this one fails.
    pub fn try_run(&mut self, req: Req, cx: &mut Context<Self>) -> Task<Result<Resp, String>> {
        let b = self.model.route(&req);
        self.try_run_on(b, req, cx)
    }

    /// [`Net::try_run`] on box `b`. A session it makes is placed on `b` before the answer is given,
    /// so whoever shows it reaches the right daemon at once.
    pub fn try_run_on(
        &mut self,
        b: BoxId,
        req: Req,
        cx: &mut Context<Self>,
    ) -> Task<Result<Resp, String>> {
        let Some(c) = self.client_of(b) else {
            return Task::ready(Err("this laptop has no local daemon".to_owned()));
        };
        let sent = self.rt.spawn(async move { c.request(req).await });
        cx.spawn(async move |this, cx| {
            let answer = match sent.await {
                Ok(Ok(resp)) => Ok(resp),
                Ok(Err(e)) => Err(e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            if let Ok(Resp::Session(sid)) = &answer {
                let sid = *sid;
                this.update(cx, |n, _| n.model.boxes.insert(sid, b)).ok();
            }
            answer
        })
    }
}

/// Drains `client`'s events into the model as box `b`'s, re-emits them, and keeps asking for the
/// global scope until the daemon says yes (the client resubscribes by itself after a reconnect, but
/// a subscribe that gets no answer in 30 s is forgotten).
fn drain(
    client: &Arc<Client>,
    b: BoxId,
    rt: &tokio::runtime::Handle,
    cx: &mut Context<Net>,
) -> Task<()> {
    let c = client.clone();
    rt.spawn(async move {
        while c.subscribe(Scope::Global).await.is_err() {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    let mut events = client.events();
    cx.spawn(async move |this, cx| {
        while let Some(ev) = events.recv().await {
            let alive = this
                .update(cx, |n, cx| {
                    if n.model.apply_from(b, &ev) {
                        cx.notify();
                    }
                    cx.emit(ClientEvent(ev, b));
                })
                .is_ok();
            if !alive {
                break;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::state::{Agent, Attention, Mode, PaneKind};
    use femme_proto::Id;

    fn session(n: u128, rev: u64, created: i64) -> Session {
        Session {
            id: Id(n),
            handle: format!("h{n}"),
            rev,
            name: None,
            owner: "board".into(),
            agent: Agent::Shell,
            mode: Mode::Tui,
            cwd: "/home/u/work".into(),
            label: None,
            agent_sid: None,
            model: None,
            status: SessionStatus::Idle,
            attention: Attention::None,
            created,
            closed: None,
            permission: None,
            context: None,
            effort: None,
            goal: None,
        }
    }

    fn pane(sid: u128, no: u16, rev: u64) -> Pane {
        Pane {
            id: PaneId { sid: Id(sid), no },
            rev,
            kind: PaneKind::Shell,
            title: String::new(),
        }
    }

    fn changes(rows: Vec<Row>) -> Event {
        Event::Changes {
            scope: Scope::Global,
            rev: 0,
            rows,
        }
    }

    fn tombstone(entity: Kind, id: String) -> Row {
        Row::Tombstone {
            entity,
            id,
            rev: 99,
        }
    }

    /// A live session of `agent`, created at `created`.
    fn agent(n: u128, agent: Agent, created: i64) -> Session {
        Session {
            agent,
            mode: Mode::Headless,
            ..session(n, 1, created)
        }
    }

    /// `closed` as the daemon says it: a newer row of the session with `closed` set.
    fn closed(mut s: Session) -> Row {
        s.rev += 1;
        s.closed = Some(s.created + 1);
        Row::Session(s)
    }

    /// A model that holds `sessions` and shows `current`.
    fn showing(current: u128, sessions: Vec<Session>) -> Model {
        let mut m = Model::default();
        m.apply(&changes(sessions.into_iter().map(Row::Session).collect()));
        m.current = Some(Id(current));
        m
    }

    #[test]
    fn rows_upsert_and_keep_the_higher_rev() {
        let mut m = Model::default();
        let mut newer = session(1, 5, 10);
        newer.label = Some("new".into());
        assert!(m.apply(&changes(vec![
            Row::Session(newer),
            Row::Pane(pane(1, 1, 6))
        ])));
        // A row with an older rev (a replayed backlog) changes nothing.
        let mut stale = session(1, 4, 10);
        stale.label = Some("stale".into());
        assert!(!m.apply(&changes(vec![
            Row::Session(stale),
            Row::Pane(pane(1, 1, 6))
        ])));
        assert_eq!(m.sessions[&Id(1)].label.as_deref(), Some("new"));
        let mut newest = pane(1, 1, 7);
        newest.title = "vim".into();
        assert!(m.apply(&changes(vec![Row::Pane(newest)])));
        assert_eq!(m.panes[&PaneId { sid: Id(1), no: 1 }].title, "vim");
    }

    #[test]
    fn tombstones_remove_and_garbage_is_ignored() {
        let mut m = Model::default();
        m.apply(&changes(vec![
            Row::Session(session(1, 1, 1)),
            Row::Session(session(2, 1, 2)),
            Row::Pane(pane(1, 1, 1)),
            Row::Pane(pane(1, 2, 1)),
            Row::Pane(pane(2, 1, 1)),
        ]));
        let p = PaneId { sid: Id(1), no: 2 };
        assert!(m.apply(&changes(vec![tombstone(
            Kind::Pane,
            format!("{}:{}", p.sid, p.no)
        )])));
        assert_eq!(m.panes.len(), 2);
        assert!(!m.apply(&changes(vec![
            tombstone(Kind::Pane, "nonsense".into()),
            tombstone(Kind::Session, "nonsense".into()),
            tombstone(Kind::Issue, "7".into()),
        ])));
        // A session takes its panes with it.
        assert!(m.apply(&changes(vec![tombstone(Kind::Session, Id(1).to_string())])));
        assert_eq!(m.sessions.keys().collect::<Vec<_>>(), [&Id(2)]);
        assert_eq!(m.panes.keys().map(|p| p.sid).collect::<Vec<_>>(), [Id(2)]);
    }

    #[test]
    fn reset_clears_the_global_scope_only() {
        let mut m = Model::default();
        m.apply(&changes(vec![
            Row::Session(session(1, 1, 1)),
            Row::Pane(pane(1, 1, 1)),
        ]));
        assert!(!m.apply(&Event::Reset {
            scope: Scope::Session(Id(1))
        }));
        assert_eq!(m.sessions.len(), 1);
        assert!(m.apply(&Event::Reset {
            scope: Scope::Global
        }));
        assert!((m.sessions.is_empty(), m.panes.is_empty()) == (true, true));
        // What follows a reset is the full set.
        assert!(m.apply(&changes(vec![Row::Session(session(3, 1, 1))])));
        assert_eq!(m.sessions.len(), 1);
    }

    #[test]
    fn items_issues_deltas_and_other_scopes_are_not_shown() {
        let mut m = Model::default();
        let other = Event::Changes {
            scope: Scope::Session(Id(1)),
            rev: 1,
            rows: vec![Row::Session(session(1, 1, 1))],
        };
        assert!(!m.apply(&other));
        assert!(!m.apply(&Event::Delta {
            sid: Id(1),
            item_id: "i".into(),
            offset: 0,
            text: "x".into()
        }));
        assert!(m.sessions.is_empty());
    }

    #[test]
    fn live_sessions_newest_first_and_tabs_oldest_first() {
        let mut m = Model::default();
        let mut closed = session(3, 1, 30);
        closed.closed = Some(31);
        let mut ended = session(4, 1, 40);
        ended.status = SessionStatus::Closed;
        m.apply(&changes(vec![
            Row::Session(session(1, 1, 10)),
            Row::Session(session(2, 1, 20)),
            Row::Session(closed),
            Row::Session(ended),
            Row::Pane(pane(2, 1, 1)),
            Row::Pane(pane(1, 2, 1)),
            Row::Pane(pane(1, 1, 1)),
            Row::Pane(pane(3, 1, 1)),
            Row::Pane(pane(4, 1, 1)),
            // Its session has not arrived (yet).
            Row::Pane(pane(9, 1, 1)),
        ]));
        assert_eq!(
            m.live().iter().map(|s| s.id).collect::<Vec<_>>(),
            [Id(2), Id(1)]
        );
        let id = |s: u128, no: u16| PaneId { sid: Id(s), no };
        assert_eq!(m.tabs(), [id(1, 1), id(1, 2), id(2, 1)]);
    }

    #[test]
    fn a_session_is_called_by_name_label_or_directory() {
        let mut s = session(1, 1, 1);
        assert_eq!(title(&s), "work");
        s.cwd = "/".into();
        assert_eq!(title(&s), "h1", "no directory name: the handle");
        s.cwd = "/srv/app/".into();
        assert_eq!(title(&s), "app");
        s.label = Some("fix the build".into());
        assert_eq!(title(&s), "fix the build");
        s.name = Some("nightly".into());
        assert_eq!(title(&s), "nightly");
        s.name = Some(String::new());
        assert_eq!(title(&s), "fix the build", "an empty name is no name");
    }

    #[test]
    fn tabs_are_named_by_the_program_the_pane_or_the_session() {
        let mut m = Model::default();
        let mut s = session(1, 1, 1);
        s.name = Some("build".into());
        let mut titled = pane(1, 2, 1);
        titled.title = "vim".into();
        m.apply(&changes(vec![
            Row::Session(s),
            Row::Pane(pane(1, 1, 1)),
            Row::Pane(titled),
        ]));
        let id = |no| PaneId { sid: Id(1), no };
        assert_eq!(m.tab_label(id(1), None), "build · 1");
        assert_eq!(m.tab_label(id(2), None), "vim");
        assert_eq!(m.tab_label(id(2), Some("htop".into())), "htop");
        assert_eq!(m.tab_label(id(2), Some(String::new())), "vim");
        assert_eq!(m.tab_label(id(9), None), "build · 9");
        let long = "é".repeat(40);
        assert_eq!(
            m.tab_label(id(1), Some(long)),
            format!("{}…", "é".repeat(LABEL))
        );
    }

    #[test]
    fn the_status_line_tells_a_refused_device_how_to_enroll() {
        let mut m = Model::default();
        let up = |quic_error| Event::Connected {
            link: Transport::Ssh,
            server_version: "1".into(),
            epoch: 1,
            quic_error,
        };
        m.apply(&up(None));
        assert_eq!(status_line(&m, "ssh h femmed enroll", None), "ssh");
        m.apply(&up(Some("unknown device".into())));
        assert_eq!(
            status_line(&m, "ssh h femmed enroll", Some("boom")),
            "ssh | quic refused this device, enroll it: ssh h femmed enroll | boom"
        );
    }

    #[test]
    fn the_link_in_words() {
        let mut m = Model::default();
        assert_eq!(m.status(), "connecting…");
        let up = |link, quic_error| Event::Connected {
            link,
            server_version: "1".into(),
            epoch: 1,
            quic_error,
        };
        m.apply(&up(Transport::Quic, None));
        assert_eq!(m.status(), "quic");
        m.rtt_ms = Some(23);
        assert_eq!(m.status(), "quic 23 ms");
        m.apply(&up(Transport::Ssh, Some("not enrolled".into())));
        assert_eq!(m.status(), "ssh");
        assert_eq!(m.quic_error.as_deref(), Some("not enrolled"));
        m.apply(&Event::Disconnected {
            reason: "closed".into(),
        });
        assert_eq!(m.status(), "offline: closed");
        m.apply(&up(Transport::Quic, None));
        assert_eq!(
            m.quic_error, None,
            "a connection without a QUIC refusal clears it"
        );
    }

    #[test]
    fn the_session_on_show_and_whether_it_is_an_agent() {
        let mut m = Model::default();
        assert!(m.current_session().is_none());
        m.apply(&changes(vec![
            Row::Session(agent(1, Agent::Claude, 10)),
            Row::Session(agent(2, Agent::Codex, 20)),
            Row::Session(agent(3, Agent::Pi, 30)),
            Row::Session(session(4, 1, 40)),
        ]));
        assert!(m.is_agent(Id(1)) && m.is_agent(Id(2)) && m.is_agent(Id(3)));
        assert!(!m.is_agent(Id(4)), "a shell is not an agent");
        assert!(!m.is_agent(Id(9)), "nor is a session nobody has listed");
        assert!(m.select(Some(Id(1))));
        assert_eq!(m.current_session().map(|s| s.id), Some(Id(1)));
        assert!(!m.select(Some(Id(1))), "nothing changed");
        assert!(m.select(Some(Id(9))), "its row may be on its way");
        assert!(m.current_session().is_none());
        assert!(m.select(None));
        assert_eq!(m.current, None);
    }

    #[test]
    fn a_closed_session_is_not_shown() {
        let mut m = Model::default();
        m.apply(&changes(vec![
            closed(session(1, 1, 10)),
            Row::Session(session(2, 1, 20)),
        ]));
        assert!(!m.select(Some(Id(1))));
        assert_eq!(m.current, None);
        assert!(m.select(Some(Id(2))));
    }

    #[test]
    fn current_moves_to_the_newest_live_session_when_its_own_closes() {
        let sessions = || {
            vec![
                session(1, 1, 10),
                session(2, 1, 20),
                session(3, 1, 30),
                session(4, 1, 40),
            ]
        };
        // The daemon marks it closed.
        let mut m = showing(3, sessions());
        assert!(m.apply(&changes(vec![closed(session(3, 1, 30))])));
        assert_eq!(m.current, Some(Id(4)));
        // Its status says so.
        let mut m = showing(3, sessions());
        let mut ended = session(3, 2, 30);
        ended.status = SessionStatus::Closed;
        assert!(m.apply(&changes(vec![Row::Session(ended)])));
        assert_eq!(m.current, Some(Id(4)));
        // A tombstone takes it away.
        let mut m = showing(4, sessions());
        assert!(m.apply(&changes(vec![tombstone(Kind::Session, Id(4).to_string())])));
        assert_eq!(m.current, Some(Id(3)));
        // Another session going does not move it.
        let mut m = showing(3, sessions());
        assert!(m.apply(&changes(vec![closed(session(1, 1, 10))])));
        assert_eq!(m.current, Some(Id(3)));
        assert!(m.apply(&changes(vec![tombstone(Kind::Session, Id(2).to_string())])));
        assert_eq!(m.current, Some(Id(3)));
    }

    #[test]
    fn current_is_none_when_the_last_live_session_closes() {
        let mut m = showing(1, vec![session(1, 1, 10), session(2, 1, 20)]);
        assert!(m.apply(&changes(vec![
            closed(session(1, 1, 10)),
            closed(session(2, 1, 20))
        ])));
        assert_eq!(m.current, None);
        assert!(m.current_session().is_none());
        // Nothing is on show: a session that turns up later is not put on show by the model (the
        // opening at start, and the user, choose).
        assert!(m.apply(&changes(vec![Row::Session(session(5, 1, 50))])));
        assert_eq!(m.current, None);
    }

    #[test]
    fn a_session_nobody_has_listed_yet_stays_current() {
        // `SessionCreate` is answered before its row arrives: other changes must not undo the choice.
        let mut m = showing(7, vec![session(1, 1, 10)]);
        assert!(m.apply(&changes(vec![Row::Session(session(2, 1, 20))])));
        assert_eq!(m.current, Some(Id(7)));
        assert!(m.apply(&changes(vec![tombstone(Kind::Session, Id(1).to_string())])));
        assert_eq!(m.current, Some(Id(7)));
        assert!(m.current_session().is_none(), "not listed: not shown");
        assert!(m.apply(&changes(vec![Row::Session(session(7, 1, 70))])));
        assert_eq!(m.current_session().map(|s| s.id), Some(Id(7)));
        // A tombstone for it, once it is known, does move on.
        assert!(m.apply(&changes(vec![tombstone(Kind::Session, Id(7).to_string())])));
        assert_eq!(m.current, Some(Id(2)));
    }

    #[test]
    fn a_claude_session_that_appears_while_a_shell_is_on_show_takes_the_centre() {
        // `claude` typed in shell 1: `femmed open` makes Claude session 5 and the daemon lists it.
        let mut m = showing(1, vec![session(1, 1, 10), agent(2, Agent::Claude, 20)]);
        assert!(m.apply(&changes(vec![Row::Session(agent(5, Agent::Claude, 50))])));
        assert_eq!(m.current, Some(Id(5)));
        // Once only: back on the shell, a newer row of the same session does not take it again.
        m.select(Some(Id(1)));
        let mut again = agent(5, Agent::Claude, 50);
        again.rev = 2;
        assert!(m.apply(&changes(vec![Row::Session(again)])));
        assert_eq!(m.current, Some(Id(1)));
        // Nor does one already known (2), nor a new one that crashed, nor a new Codex.
        let mut dead = agent(6, Agent::Claude, 60);
        dead.status = SessionStatus::Crashed;
        assert!(m.apply(&changes(vec![
            Row::Session(dead),
            Row::Session(agent(7, Agent::Codex, 70)),
        ])));
        assert_eq!(m.current, Some(Id(1)));
    }

    #[test]
    fn a_new_claude_session_leaves_an_agent_or_an_empty_centre_alone() {
        let mut m = showing(2, vec![session(1, 1, 10), agent(2, Agent::Codex, 20)]);
        assert!(m.apply(&changes(vec![Row::Session(agent(5, Agent::Claude, 50))])));
        assert_eq!(m.current, Some(Id(2)), "an agent session on show stays");
        let mut m = showing(3, vec![agent(3, Agent::Claude, 30)]);
        assert!(m.apply(&changes(vec![Row::Session(agent(5, Agent::Claude, 50))])));
        assert_eq!(m.current, Some(Id(3)));
        // Nothing on show: the opening at start chooses, not this rule.
        let mut m = Model::default();
        m.apply(&changes(vec![Row::Session(session(1, 1, 10))]));
        assert!(m.apply(&changes(vec![Row::Session(agent(5, Agent::Claude, 50))])));
        assert_eq!(m.current, None);
    }

    #[test]
    fn the_first_list_and_the_set_after_a_reset_are_not_arrivals() {
        // A shell made before the list came is on show: the old Claude sessions in it stay put.
        let mut m = Model::default();
        m.select(Some(Id(1)));
        m.apply(&changes(vec![
            Row::Session(session(1, 1, 10)),
            Row::Session(agent(2, Agent::Claude, 20)),
        ]));
        assert_eq!(m.current, Some(Id(1)));
        // After a reset the full set lists them again: still not new.
        m.apply(&Event::Reset {
            scope: Scope::Global,
        });
        m.apply(&changes(vec![
            Row::Session(session(1, 1, 10)),
            Row::Session(agent(2, Agent::Claude, 20)),
        ]));
        assert_eq!(m.current, Some(Id(1)));
    }

    #[test]
    fn current_survives_a_reset_when_the_full_set_lists_it() {
        let mut m = showing(2, vec![session(1, 1, 10), session(2, 1, 20)]);
        m.apply(&Event::Reset {
            scope: Scope::Global,
        });
        assert_eq!(m.load, Load::Rebuilding);
        assert_eq!(m.current, Some(Id(2)), "named while the lists are empty");
        assert!(m.current_session().is_none());
        assert!(m.apply(&changes(vec![
            Row::Session(session(1, 1, 10)),
            Row::Session(session(2, 3, 20)),
        ])));
        assert_eq!(m.load, Load::Done);
        assert_eq!(m.current, Some(Id(2)));
        assert_eq!(m.current_session().map(|s| s.rev), Some(3));
    }

    #[test]
    fn current_moves_on_when_the_full_set_after_a_reset_lacks_it() {
        let mut m = showing(2, vec![session(1, 1, 10), session(2, 1, 20)]);
        m.apply(&Event::Reset {
            scope: Scope::Global,
        });
        // It was deleted while the app was away: no tombstone comes, the set just does not have it.
        assert!(m.apply(&changes(vec![
            Row::Session(session(1, 1, 10)),
            Row::Session(session(3, 1, 30)),
        ])));
        assert_eq!(m.current, Some(Id(3)));
        // And with nothing live in it at all, nothing is on show.
        let mut m = showing(2, vec![session(2, 1, 20)]);
        m.apply(&Event::Reset {
            scope: Scope::Global,
        });
        assert!(m.apply(&changes(vec![])));
        assert_eq!(m.current, None);
        // A reset of another scope touches nothing.
        let mut m = showing(2, vec![session(2, 1, 20)]);
        m.apply(&Event::Reset {
            scope: Scope::Session(Id(2)),
        });
        assert_eq!((m.load, m.current), (Load::Done, Some(Id(2))));
    }

    #[test]
    fn the_first_list_never_takes_a_session_just_made_off_the_screen() {
        // The daemon is slow to send its list: the session made meanwhile is on show, and the list,
        // computed before it existed, does not have it. Only a `Reset` makes absence mean "gone".
        let mut m = Model::default();
        assert_eq!(m.load, Load::Waiting);
        m.select(Some(Id(5)));
        assert!(m.apply(&changes(vec![Row::Session(session(1, 1, 10))])));
        assert_eq!(m.load, Load::Done);
        assert_eq!(m.current, Some(Id(5)));
        assert!(m.apply(&changes(vec![Row::Session(session(5, 1, 50))])));
        assert_eq!(m.current_session().map(|s| s.id), Some(Id(5)));
    }

    #[test]
    fn an_empty_first_list_is_still_a_list() {
        let mut m = Model::default();
        assert_eq!(m.load, Load::Waiting);
        assert!(!m.apply(&changes(vec![])), "nothing to redraw");
        assert_eq!(m.load, Load::Done);
        // Changes of other scopes are not the list.
        let mut m = Model::default();
        m.apply(&Event::Changes {
            scope: Scope::Session(Id(1)),
            rev: 1,
            rows: vec![],
        });
        assert_eq!(m.load, Load::Waiting);
    }

    #[test]
    fn the_status_line_says_what_is_being_started() {
        let mut m = Model::default();
        m.apply(&Event::Connected {
            link: Transport::Ssh,
            server_version: "1".into(),
            epoch: 1,
            quic_error: None,
        });
        assert_eq!(
            status_line_with(&m, "", Some("starting claude…"), None),
            "ssh | starting claude…"
        );
        assert_eq!(
            status_line_with(&m, "", Some("starting a shell…"), Some("boom")),
            "ssh | starting a shell… | boom"
        );
        assert_eq!(
            status_line_with(&m, "", None, None),
            status_line(&m, "", None)
        );
    }

    fn changes_of(m: &mut Model, b: BoxId, rows: Vec<Row>) -> bool {
        m.apply_from(b, &changes(rows))
    }

    #[test]
    fn a_session_is_routed_to_the_box_that_listed_it() {
        let mut m = Model::default();
        changes_of(&mut m, BoxId::Remote, vec![Row::Session(session(1, 1, 10))]);
        changes_of(&mut m, BoxId::Local, vec![Row::Session(session(2, 1, 20))]);
        assert_eq!(m.live().len(), 2, "one list for both boxes");
        assert_eq!(
            (m.box_of(Id(1)), m.box_of(Id(2))),
            (BoxId::Remote, BoxId::Local)
        );
        assert!(m.is_local(Id(2)) && !m.is_local(Id(1)));
        assert_eq!(m.box_of(Id(9)), BoxId::Remote, "unplaced: the remote box");
        let prompt = |sid| Req::Prompt {
            sid: Id(sid),
            text: "hi".into(),
            attachments: vec![],
            how: femme_proto::rpc::PromptHow::Queue,
        };
        assert_eq!(m.route(&prompt(2)), BoxId::Local);
        assert_eq!(m.route(&prompt(1)), BoxId::Remote);
        assert_eq!(
            m.route(&Req::PaneClose {
                pane: PaneId { sid: Id(2), no: 1 }
            }),
            BoxId::Local
        );
        // Global requests stay on the remote box; blobs go with the session on show.
        m.current = Some(Id(2));
        assert_eq!(m.route(&Req::Prs), BoxId::Remote);
        assert_eq!(m.route(&Req::BlobGet { hash: [0; 32] }), BoxId::Local);
    }

    #[test]
    fn a_reset_of_one_box_keeps_the_other_boxs_sessions() {
        let mut m = Model::default();
        changes_of(&mut m, BoxId::Remote, vec![Row::Session(session(1, 1, 10))]);
        changes_of(
            &mut m,
            BoxId::Local,
            vec![Row::Session(session(2, 1, 20)), Row::Pane(pane(2, 1, 1))],
        );
        m.current = Some(Id(1));
        m.apply_from(
            BoxId::Local,
            &Event::Reset {
                scope: Scope::Global,
            },
        );
        assert_eq!(m.sessions.keys().collect::<Vec<_>>(), [&Id(1)]);
        assert!(m.panes.is_empty());
        assert_eq!((m.load, m.local_load), (Load::Done, Load::Rebuilding));
        // The local full set does not list the remote session on show: it stays.
        changes_of(&mut m, BoxId::Local, vec![]);
        assert_eq!(m.current, Some(Id(1)));
        // The local link is its own; the remote one is not touched.
        m.apply_from(
            BoxId::Local,
            &Event::Disconnected {
                reason: "gone".into(),
            },
        );
        assert_eq!(m.link, Link::Connecting);
        assert_eq!(m.local_link, Some(Link::Down("gone".into())));
    }

    #[test]
    fn the_status_line_names_the_local_link() {
        let mut m = Model::default();
        m.apply(&Event::Connected {
            link: Transport::Quic,
            server_version: "1".into(),
            epoch: 1,
            quic_error: None,
        });
        m.rtt_ms = Some(23);
        assert_eq!(
            status_line(&m, "", None),
            "quic 23 ms",
            "no local box: not named"
        );
        m.local_link = Some(Link::Connecting);
        assert_eq!(status_line(&m, "", None), "quic 23 ms · local: off");
        m.apply_from(
            BoxId::Local,
            &Event::Connected {
                link: Transport::Local,
                server_version: "1".into(),
                epoch: 1,
                quic_error: None,
            },
        );
        assert_eq!(m.status(), "quic 23 ms", "the remote link is untouched");
        assert_eq!(
            status_line(&m, "", Some("boom")),
            "quic 23 ms · local | boom"
        );
        m.apply_from(
            BoxId::Local,
            &Event::Disconnected {
                reason: "refused".into(),
            },
        );
        assert_eq!(status_line(&m, "", None), "quic 23 ms · local: off");
    }

    #[test]
    fn the_opening_runs_once_until_it_gives_up() {
        let mut o = Opening::default();
        assert!(o.begin());
        assert!(
            !o.begin(),
            "a second call while the first runs does nothing"
        );
        o.end(false);
        assert!(
            o.begin(),
            "it gave up before deciding: a reconnect may try again"
        );
        o.end(true);
        assert!(!o.begin(), "decided: never again");
        assert_eq!(o, Opening::Done);
    }
}
