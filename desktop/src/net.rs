//! The desktop's copy of what the daemon says: the state of the link, and the sessions and panes of
//! the global scope. [`Model`] is plain data, fed by the client's events (and tested without gpui);
//! [`Net`] is the entity that drains the client into it and runs requests for the UI.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use femme_client::{Client, Event, Link as Transport};
use femme_proto::rpc::{Req, Resp, Scope};
use femme_proto::state::{Agent, Entity as Kind, Pane, Row, Session, SessionStatus};
use femme_proto::{PaneId, Sid};
use gpui::{Context, EventEmitter, Task};

use crate::connect::{self, Connect};

/// How many characters of a tab's label are shown.
const LABEL: usize = 28;

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
    /// when that one closes it moves to the newest live session, or to `None` ([`Model::apply`]).
    /// It may name a session the daemon has not listed yet: the answer to creating one comes first.
    pub current: Option<Sid>,
    /// Every pane of the global scope.
    pub panes: BTreeMap<PaneId, Pane>,
    /// Whether the daemon's list has come in. The first `Changes` of the global scope is always sent
    /// (empty if need be), so [`Load::Done`] means "this is everything", and after a `Reset` it is
    /// [`Load::Rebuilding`] until the full set follows.
    pub load: Load,
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

impl Model {
    /// Takes in one event; whether anything the UI shows changed.
    pub fn apply(&mut self, ev: &Event) -> bool {
        match ev {
            Event::Connected {
                link, quic_error, ..
            } => {
                self.link = Link::Up(*link);
                self.quic_error.clone_from(quic_error);
                true
            }
            Event::Disconnected { reason } => {
                self.link = Link::Down(reason.clone());
                true
            }
            Event::Changes {
                scope: Scope::Global,
                rows,
                ..
            } => {
                let rebuilt = self.load == Load::Rebuilding;
                let changed = rows.iter().fold(false, |any, row| self.row(row) | any);
                self.load = Load::Done;
                self.keep_current(rows, rebuilt) | changed
            }
            // The daemon cannot vouch for what we hold: its full set follows. The session on show
            // stays named, and stays if the full set lists it.
            Event::Reset {
                scope: Scope::Global,
            } => {
                self.sessions.clear();
                self.panes.clear();
                self.load = Load::Rebuilding;
                true
            }
            // Item and issue rows, transcript scopes and streaming text are not shown yet.
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
    /// The command that enrolls this device with the daemon, for the hint.
    pub enroll: String,
}

/// Starts the client for `c` with this device's identity and a fresh client instance (one per
/// process: terminals know it across reconnects). Call inside the side runtime: the client spawns
/// its tasks on it.
pub fn start(c: &Connect) -> anyhow::Result<Started> {
    let device = connect::identity(&c.identity)?;
    let spki = device
        .spki_sha256()
        .context("the device certificate has no public key")?;
    let mut cfg = femme_client::Config::new(device, ulid::Ulid::generate());
    cfg.quic = c.quic;
    cfg.ssh_host.clone_from(&c.ssh);
    Ok(Started {
        client: Arc::new(Client::start(cfg)),
        enroll: connect::enroll_line(c.ssh.as_deref(), &spki),
    })
}

/// Every event of the daemon's client, re-emitted for views that keep more than [`Model`] does (the
/// transcript's items and deltas, the composer's answers). Subscribe with `cx.subscribe(&net, …)`;
/// `Model` has already applied the event when it arrives.
pub struct ClientEvent(pub Event);

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
    client: Arc<Client>,
    rt: tokio::runtime::Handle,
    _tasks: [Task<()>; 2],
}

impl Net {
    /// Drains `client`'s events into the model and subscribes to the global scope.
    pub fn new(
        client: Arc<Client>,
        rt: tokio::runtime::Handle,
        enroll: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut events = client.events();
        let drain = cx.spawn(async move |this, cx| {
            while let Some(ev) = events.recv().await {
                let alive = this
                    .update(cx, |n, cx| {
                        if n.model.apply(&ev) {
                            cx.notify();
                        }
                        cx.emit(ClientEvent(ev));
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        let poll = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            if this.update(cx, |n, cx| n.poll(cx)).is_err() {
                break;
            }
        });
        // The client resubscribes by itself after a reconnect, but a subscribe that gets no answer
        // in 30 s is forgotten: ask again until the daemon has said yes.
        let c = client.clone();
        rt.spawn(async move {
            while c.subscribe(Scope::Global).await.is_err() {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        Self {
            model: Model::default(),
            error: None,
            enroll,
            starting: None,
            opening: Opening::default(),
            client,
            rt,
            _tasks: [drain, poll],
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

    /// The client, for panes that attach to a terminal.
    pub fn client(&self) -> Arc<Client> {
        self.client.clone()
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

    /// Runs `req` on the side runtime. The answer is `None` when the request failed, and the
    /// reason waits in [`Net::error`] for the nav bar.
    pub fn run(&mut self, req: Req, cx: &mut Context<Self>) -> Task<Option<Resp>> {
        let sent = self.try_run(req, cx);
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
        let c = self.client.clone();
        let sent = self.rt.spawn(async move { c.request(req).await });
        cx.spawn(async move |_, _| match sent.await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(e)) => Err(e.to_string()),
            Err(e) => Err(e.to_string()),
        })
    }
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
