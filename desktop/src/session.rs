//! Which session the centre shows when the app opens, and the rules for choosing it.
//!
//! The user wants Claude in the centre as soon as the app is up: the newest live agent session when
//! there is one (Claude before Codex before Pi; one that crashed or failed does not count), else a
//! new headless Claude session in the box's home directory, else (`claude` is not on the box, or the
//! daemon refuses) a shell. [`pick`] is the
//! pure rule; [`open_at_start`] waits for the daemon's list of sessions, applies the rule and talks
//! to the daemon. What stays on show afterwards is [`Model::current`]'s business: it follows its
//! session when that closes (`net.rs`).

use std::time::{Duration, Instant};

use femme_proto::rpc::{Req, Resp};
use femme_proto::state::{Agent, Mode};
use femme_proto::Sid;
use gpui::{App, AsyncApp, Entity, WeakEntity};

use crate::net::{is_running, Link, Load, Model, Net};

/// How long the daemon's list of sessions is waited for once the link is up. The first `Changes`
/// comes a round trip after `Connected`; an old Claude session must not be missed, or it is duplicated.
const SETTLE: Duration = Duration::from_secs(2);
/// How long a link that does not come up is waited for before giving up (a later `Connected` asks again).
const LINK_WAIT: Duration = Duration::from_secs(30);
/// How often the wait looks at the model.
const TICK: Duration = Duration::from_millis(50);

/// What to show when the app opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// This agent session, which is already live on the box.
    Show(Sid),
    /// There is none: start a headless Claude session.
    CreateClaude,
}

/// Claude, then Codex, then Pi; a shell is not an agent.
fn rank(agent: Agent) -> Option<u8> {
    match agent {
        Agent::Claude => Some(0),
        Agent::Codex => Some(1),
        Agent::Pi => Some(2),
        Agent::Shell => None,
    }
}

/// The session to show at start: the newest live one of the best-ranked agent (Claude, else Codex,
/// else Pi), else `CreateClaude`. Closed sessions, shells, and sessions whose agent crashed, failed
/// or lost its holder ([`is_running`]) do not count: a dead Claude must not stand in for a new one.
pub fn pick(model: &Model) -> Choice {
    // `live` is newest first, and `min_by_key` keeps the first of equals: the newest of the agent.
    model
        .live()
        .into_iter()
        .filter(|s| is_running(s))
        .filter_map(|s| Some((rank(s.agent)?, s.id)))
        .min_by_key(|&(rank, _)| rank)
        .map_or(Choice::CreateClaude, |(_, sid)| Choice::Show(sid))
}

/// The terminal to fall back on when Claude cannot be started: the newest live shell session that
/// still runs, so that a box without `claude` does not collect one more shell at every start.
pub fn pick_shell(model: &Model) -> Option<Sid> {
    model
        .live()
        .into_iter()
        .find(|s| s.agent == Agent::Shell && is_running(s))
        .map(|s| s.id)
}

/// The model a new Claude session starts on (Opus 5.5; `femmed` adds `--effort=medium` itself).
pub(crate) const CLAUDE_MODEL: &str = "claude-opus-5-5";

/// A new session of `agent` in the daemon's home directory (an empty `cwd` is what `femmed` takes as
/// "your home": `~` is refused as not absolute), on `model` (`None`: the agent's default).
pub(crate) fn create(agent: Agent, mode: Mode, model: Option<&str>) -> Req {
    create_in("", agent, mode, model)
}

/// The sidebar's `+ claude`, a folder header's `+` and the projects picker: a headless Claude
/// session in `cwd` (empty: the daemon's home) on [`CLAUDE_MODEL`].
pub(crate) fn new_claude(cwd: &str) -> Req {
    create_in(cwd, Agent::Claude, Mode::Headless, Some(CLAUDE_MODEL))
}

/// [`create`] in the folder `cwd` (absolute; empty is the daemon's home).
pub(crate) fn create_in(cwd: &str, agent: Agent, mode: Mode, model: Option<&str>) -> Req {
    Req::SessionCreate {
        cwd: cwd.to_owned(),
        agent,
        mode,
        label: None,
        name: None,
        prompt: None,
        model: model.map(str::to_owned),
    }
}

/// Whether to stop waiting for the daemon's list: it is in, or the link has been up for [`SETTLE`]
/// without it.
fn ready(listed: bool, up_for: Option<Duration>) -> bool {
    listed || up_for.is_some_and(|d| d >= SETTLE)
}

/// The daemon's reason without its error kind ("Internal: ") and its own lead-in ("Claude did not
/// start: "), which the notes below say themselves.
fn reason(why: &str) -> &str {
    let why = why.strip_prefix("Internal: ").unwrap_or(why);
    why.strip_prefix("Claude did not start: ").unwrap_or(why)
}

/// What the app says when Claude could not be started and a shell stands in.
fn shell_note(why: &str) -> String {
    format!(
        "claude did not start ({}), a shell is open instead",
        reason(why)
    )
}

/// What the app says when not even a shell could be started.
fn no_session(claude: &str, shell: &str) -> String {
    format!(
        "no session could be started: claude did not start ({}), nor did a shell ({})",
        reason(claude),
        reason(shell)
    )
}

/// Called by the shell once connected (again after a reconnect: only the first call does anything
/// until it has decided): shows the newest live agent session; else starts a headless Claude
/// session in the home directory and shows that; else, when that fails (no `claude` on the box, the
/// daemon refuses), a shell, reusing a live one if there is one. If nothing at all can be started,
/// [`Net::error`] says why and no session is on show.
///
/// It waits for the daemon's list of sessions (the first `Changes`, at most 2 s after the link is
/// up) before it decides, and leaves alone what the user has already put on show. It returns at
/// once: the work runs as a task, so this may be called from inside an update of `net`.
pub fn open_at_start(net: &Entity<Net>, cx: &mut App) {
    let net = net.downgrade();
    cx.spawn(async move |cx| {
        if !net.update(cx, |n, _| n.opening.begin()).unwrap_or(false) {
            return;
        }
        let done = open(&net, cx).await;
        net.update(cx, |n, _| n.opening.end(done)).ok();
    })
    .detach();
}

/// The opening itself; whether it decided (a session is on show or was made).
async fn open(net: &WeakEntity<Net>, cx: &mut AsyncApp) -> bool {
    if !wait_for_list(net, cx).await {
        return false;
    }
    let chosen = net.update(cx, |n, cx| {
        // Somebody has put a session on show meanwhile: that is the choice.
        if n.model.current_session().is_some() {
            return Some(true);
        }
        match pick(&n.model) {
            Choice::Show(sid) => {
                n.set_current(Some(sid), cx);
                Some(true)
            }
            Choice::CreateClaude => None,
        }
    });
    match chosen {
        Ok(Some(done)) => return done,
        Ok(None) => {}
        Err(_) => return false,
    }
    let claude = match make(net, cx, "claude", Agent::Claude, Mode::Headless).await {
        Ok(sid) => return show(net, cx, sid, None).await,
        Err(why) => why,
    };
    // A terminal that is already there is as good as a new one.
    let have = net.update(cx, |n, _| pick_shell(&n.model)).ok().flatten();
    let shell = match have {
        Some(sid) => Ok(sid),
        None => make(net, cx, "a shell", Agent::Shell, Mode::Tui).await,
    };
    match shell {
        Ok(sid) => show(net, cx, sid, Some(shell_note(&claude))).await,
        Err(shell) => {
            net.update(cx, |n, cx| {
                n.error = Some(no_session(&claude, &shell));
                cx.notify();
            })
            .ok();
            false
        }
    }
}

/// Waits until the daemon's list of sessions is as complete as it will be soon (see [`ready`]).
/// False when the link did not come up in time or `net` is gone.
async fn wait_for_list(net: &WeakEntity<Net>, cx: &mut AsyncApp) -> bool {
    let begun = Instant::now();
    let mut up_since: Option<Instant> = None;
    loop {
        let Ok((up, listed)) = net.read_with(cx, |n, _| {
            (
                matches!(n.model.link, Link::Up(_)),
                n.model.load == Load::Done,
            )
        }) else {
            return false;
        };
        up_since = match (up, up_since) {
            (true, since) => since.or_else(|| Some(Instant::now())),
            (false, _) => None,
        };
        if ready(listed, up_since.map(|t| t.elapsed())) {
            return true;
        }
        if begun.elapsed() >= LINK_WAIT {
            return false;
        }
        cx.background_executor().timer(TICK).await;
    }
}

/// Asks the daemon for a new session, and says so in the status line until it answers.
async fn make(
    net: &WeakEntity<Net>,
    cx: &mut AsyncApp,
    what: &str,
    agent: Agent,
    mode: Mode,
) -> Result<Sid, String> {
    let sent = net
        .update(cx, |n, cx| {
            n.set_starting(Some(format!("starting {what}…")), cx);
            n.try_run(create(agent, mode, None), cx)
        })
        .map_err(|e| e.to_string())?;
    let answer = sent.await;
    net.update(cx, |n, cx| n.set_starting(None, cx)).ok();
    match answer {
        Ok(Resp::Session(sid)) => Ok(sid),
        Ok(other) => Err(format!("unexpected answer {other:?}")),
        Err(why) => Err(why),
    }
}

/// Puts `sid` on show, with `note` in the status line, unless the user has put something on show
/// while it was being made. True: the opening is decided.
async fn show(net: &WeakEntity<Net>, cx: &mut AsyncApp, sid: Sid, note: Option<String>) -> bool {
    net.update(cx, |n, cx| {
        if n.model.current_session().is_none() {
            n.set_current(Some(sid), cx);
        }
        if note.is_some() {
            n.error = note;
            cx.notify();
        }
    })
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_client::Event;
    use femme_proto::rpc::Scope;
    use femme_proto::state::{Attention, Row, Session, SessionStatus};
    use femme_proto::Id;

    fn session(n: u128, agent: Agent, created: i64) -> Session {
        Session {
            id: Id(n),
            handle: format!("h{n}"),
            rev: 1,
            name: None,
            owner: "board".into(),
            agent,
            mode: if agent == Agent::Shell {
                Mode::Tui
            } else {
                Mode::Headless
            },
            cwd: "/home/u".into(),
            label: None,
            agent_sid: None,
            model: None,
            status: SessionStatus::Idle,
            attention: Attention::None,
            created,
            closed: None,
            permission: None,
            context: None,
        }
    }

    fn model(sessions: Vec<Session>) -> Model {
        let mut m = Model::default();
        m.apply(&Event::Changes {
            scope: Scope::Global,
            rev: 1,
            rows: sessions.into_iter().map(Row::Session).collect(),
        });
        m
    }

    #[test]
    fn nothing_live_means_a_new_claude_session() {
        assert_eq!(pick(&Model::default()), Choice::CreateClaude);
        assert_eq!(pick(&model(vec![])), Choice::CreateClaude);
    }

    #[test]
    fn shells_and_closed_sessions_do_not_count() {
        let mut closed = session(2, Agent::Claude, 20);
        closed.closed = Some(21);
        let mut ended = session(3, Agent::Claude, 30);
        ended.status = SessionStatus::Closed;
        let m = model(vec![session(1, Agent::Shell, 10), closed, ended]);
        assert_eq!(pick(&m), Choice::CreateClaude);
    }

    #[test]
    fn the_newest_live_claude_session_is_shown() {
        let m = model(vec![
            session(1, Agent::Claude, 10),
            session(3, Agent::Claude, 30),
            session(2, Agent::Claude, 20),
            session(4, Agent::Shell, 40),
        ]);
        assert_eq!(pick(&m), Choice::Show(Id(3)));
    }

    #[test]
    fn claude_beats_a_newer_codex_and_codex_beats_pi() {
        let m = model(vec![
            session(1, Agent::Claude, 10),
            session(2, Agent::Codex, 20),
            session(3, Agent::Pi, 30),
        ]);
        assert_eq!(pick(&m), Choice::Show(Id(1)));
        let m = model(vec![
            session(2, Agent::Codex, 20),
            session(3, Agent::Pi, 30),
            session(5, Agent::Codex, 50),
        ]);
        assert_eq!(pick(&m), Choice::Show(Id(5)));
        let m = model(vec![
            session(3, Agent::Pi, 30),
            session(4, Agent::Shell, 40),
        ]);
        assert_eq!(pick(&m), Choice::Show(Id(3)));
    }

    #[test]
    fn a_closed_claude_gives_way_to_a_live_codex() {
        let mut closed = session(1, Agent::Claude, 10);
        closed.closed = Some(11);
        let m = model(vec![closed, session(2, Agent::Codex, 20)]);
        assert_eq!(pick(&m), Choice::Show(Id(2)));
    }

    #[test]
    fn a_crashed_claude_gives_way_to_a_new_one() {
        // The box's `kn0iyl`: Claude was killed, "starting it again in 2 s", and did not come back.
        // Nothing runs in it any more, but the daemon lists it (not closed) until the user closes it.
        let dead = |n: u128, status| Session {
            status,
            ..session(n, Agent::Claude, n as i64 * 10)
        };
        let m = model(vec![
            dead(1, SessionStatus::Crashed),
            dead(2, SessionStatus::Failed),
            dead(3, SessionStatus::Interrupted),
        ]);
        assert_eq!(pick(&m), Choice::CreateClaude);
        // A live Claude beside it is shown, though the dead one is newer.
        let m = model(vec![
            session(1, Agent::Claude, 10),
            dead(5, SessionStatus::Crashed),
        ]);
        assert_eq!(pick(&m), Choice::Show(Id(1)));
        // One that is being started again still counts.
        let m = model(vec![
            session(1, Agent::Claude, 10),
            dead(5, SessionStatus::Starting),
        ]);
        assert_eq!(pick(&m), Choice::Show(Id(5)));
    }

    #[test]
    fn equal_creation_times_fall_back_on_the_id() {
        let m = model(vec![
            session(7, Agent::Claude, 10),
            session(9, Agent::Claude, 10),
            session(8, Agent::Claude, 10),
        ]);
        assert_eq!(pick(&m), Choice::Show(Id(9)));
    }

    #[test]
    fn the_fallback_terminal_is_the_newest_live_shell() {
        assert_eq!(pick_shell(&Model::default()), None);
        let mut closed = session(4, Agent::Shell, 40);
        closed.closed = Some(41);
        let m = model(vec![
            session(1, Agent::Shell, 10),
            session(3, Agent::Shell, 30),
            session(2, Agent::Claude, 20),
            closed,
        ]);
        assert_eq!(pick_shell(&m), Some(Id(3)));
        assert_eq!(
            pick_shell(&model(vec![session(2, Agent::Claude, 20)])),
            None
        );
        // A shell that lost its holder (a reboot) is a dead terminal: an older live one is reused.
        let mut lost = session(5, Agent::Shell, 50);
        lost.status = SessionStatus::Interrupted;
        let m = model(vec![session(1, Agent::Shell, 10), lost.clone()]);
        assert_eq!(pick_shell(&m), Some(Id(1)));
        assert_eq!(pick_shell(&model(vec![lost])), None);
    }

    #[test]
    fn the_wait_ends_with_the_list_or_after_the_settle_time() {
        assert!(!ready(false, None), "the link is not even up");
        assert!(!ready(false, Some(Duration::from_millis(1999))));
        assert!(ready(false, Some(SETTLE)));
        assert!(
            ready(true, None),
            "the list came: no reason to wait for a link"
        );
        assert!(ready(true, Some(Duration::ZERO)));
    }

    #[test]
    fn new_sessions_start_in_the_home_directory_unnamed() {
        // `femmed` takes an empty `cwd` as its home; `~` or a relative path is `Bad`.
        assert_eq!(
            create(Agent::Claude, Mode::Headless, None),
            Req::SessionCreate {
                cwd: String::new(),
                agent: Agent::Claude,
                mode: Mode::Headless,
                label: None,
                name: None,
                prompt: None,
                model: None,
            }
        );
        let Req::SessionCreate { agent, mode, .. } = create(Agent::Shell, Mode::Tui, None) else {
            panic!("not a create")
        };
        assert_eq!((agent, mode), (Agent::Shell, Mode::Tui));
    }

    #[test]
    fn a_new_claude_session_starts_on_opus() {
        let Req::SessionCreate {
            cwd,
            agent,
            mode,
            model,
            ..
        } = new_claude("/srv/x")
        else {
            panic!("not a create")
        };
        assert_eq!(
            (cwd.as_str(), agent, mode, model.as_deref()),
            (
                "/srv/x",
                Agent::Claude,
                Mode::Headless,
                Some("claude-opus-5-5")
            )
        );
    }

    #[test]
    fn the_notes_name_the_reason_once() {
        // What femmed says when `claude` is not on the box, as the client prints it.
        let note = shell_note("Internal: Claude did not start: claude: not found on PATH");
        assert_eq!(
            note,
            "claude did not start (claude: not found on PATH), a shell is open instead"
        );
        // Other words are kept as they are.
        let none = no_session("connection closed", "Internal: no holder");
        assert_eq!(
            none,
            "no session could be started: claude did not start (connection closed), nor did a shell (no holder)"
        );
    }
}
