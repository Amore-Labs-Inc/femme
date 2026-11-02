//! The desktop's copy of what the daemon says: the state of the link, and the sessions and panes of
//! the global scope. [`Model`] is plain data, fed by the client's events (and tested without gpui);
//! [`Net`] is the entity that drains the client into it and runs requests for the UI.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use femme_client::{Client, Event, Link as Transport};
use femme_proto::rpc::{Req, Resp, Scope};
use femme_proto::state::{Entity as Kind, Pane, Row, Session, SessionStatus};
use femme_proto::{PaneId, Sid};
use gpui::{Context, Task};

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
    /// Every pane of the global scope.
    pub panes: BTreeMap<PaneId, Pane>,
}

fn is_live(s: &Session) -> bool {
    s.closed.is_none() && s.status != SessionStatus::Closed
}

/// The pane a tombstone names: its id is `<sid>:<no>`.
fn pane_id(text: &str) -> Option<PaneId> {
    let (sid, no) = text.rsplit_once(':')?;
    Some(PaneId {
        sid: sid.parse().ok()?,
        no: no.parse().ok()?,
    })
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
            } => rows.iter().fold(false, |any, row| self.row(row) | any),
            // The daemon cannot vouch for what we hold: its full set follows.
            Event::Reset {
                scope: Scope::Global,
            } => {
                self.sessions.clear();
                self.panes.clear();
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
    let mut line = model.status();
    if model.quic_error.is_some() {
        line.push_str(&format!(" | quic refused this device, enroll it: {enroll}"));
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

/// The client of the daemon, as an entity the views observe.
pub struct Net {
    /// What the daemon said.
    pub model: Model,
    /// The last request that failed, until one succeeds.
    pub error: Option<String>,
    /// The line that enrolls this device, for the hint when QUIC refuses it.
    pub enroll: String,
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

    /// The side runtime the client lives on.
    pub fn rt(&self) -> tokio::runtime::Handle {
        self.rt.clone()
    }

    /// The nav bar's line.
    pub fn status_line(&self) -> String {
        status_line(&self.model, &self.enroll, self.error.as_deref())
    }

    /// Runs `req` on the side runtime. The answer is `None` when the request failed, and the
    /// reason waits in [`Net::error`] for the nav bar.
    pub fn run(&mut self, req: Req, cx: &mut Context<Self>) -> Task<Option<Resp>> {
        let c = self.client.clone();
        let sent = self.rt.spawn(async move { c.request(req).await });
        cx.spawn(async move |this, cx| {
            let answer = match sent.await {
                Ok(Ok(resp)) => Ok(resp),
                Ok(Err(e)) => Err(e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let error = answer.as_ref().err().cloned();
            this.update(cx, |n, cx| {
                n.error = error;
                cx.notify();
            })
            .ok()?;
            answer.ok()
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
}
