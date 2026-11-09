//! The pure part of the terminal panel: which panes are terminals, which of them are tabs, which
//! tab is on show and what "close" asks of the daemon. Plain functions over [`Model`], tested
//! without gpui.

use femme_proto::rpc::Req;
use femme_proto::state::{Agent, Mode, PaneKind};
use femme_proto::{PaneId, Sid};

use crate::net::Model;

/// Characters of a tab's label the strip shows (`Model::tab_label` allows more: it is for wider
/// places).
pub const TAB_CHARS: usize = 16;

/// What the centre draws, as far as the panel knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Centre {
    /// Nobody said: the first terminal of a shell session or of an agent that runs its own TUI,
    /// which is what the shell puts in the centre for them (docs/desktop-ui.md).
    Auto,
    /// The shell said: this pane (`None`: no terminal, as for a transcript).
    Is(Option<PaneId>),
}

impl Centre {
    /// The terminal of session `sid` the centre draws, if it draws one of that session's.
    pub fn pane(self, model: &Model, sid: Option<Sid>) -> Option<PaneId> {
        match self {
            Centre::Is(p) => p.filter(|p| Some(p.sid) == sid),
            Centre::Auto => auto_centre(model, sid),
        }
    }
}

/// The tab a session shows: the one the user picked and where it sat in the strip, so that when
/// it goes the tab that took its place is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shown {
    /// The tab picked.
    pub pane: PaneId,
    /// Its place in the strip when last drawn; `usize::MAX` while the daemon has not listed it
    /// yet (the last tab stands in).
    pub ix: usize,
}

/// Whether the daemon has a terminal behind `pane`: a headless agent's pane 1 stands for its
/// transcript, its holder has no pty, so attaching it only ends in `Exited`.
pub fn is_terminal(model: &Model, pane: PaneId) -> bool {
    match (model.panes.get(&pane), model.sessions.get(&pane.sid)) {
        (Some(p), Some(s)) => !(s.mode == Mode::Headless && p.kind == PaneKind::Agent),
        _ => false,
    }
}

/// The terminals of the live sessions, oldest session first, then by number.
pub fn terminals(model: &Model) -> Vec<PaneId> {
    model
        .tabs()
        .into_iter()
        .filter(|p| is_terminal(model, *p))
        .collect()
}

/// The first (lowest numbered) pane of live session `sid`, if it is a terminal: what the centre
/// draws for a shell session or an agent in its TUI. A headless agent has none: its first pane is
/// the transcript's.
pub fn main_of(model: &Model, sid: Sid) -> Option<PaneId> {
    let (from, to) = (PaneId { sid, no: 0 }, PaneId { sid, no: u16::MAX });
    let first = *model.panes.range(from..=to).next()?.0;
    terminals(model).contains(&first).then_some(first)
}

/// What the centre draws when nobody said: the first terminal of a shell session, or of an agent
/// that runs its own TUI (it streams no transcript, the terminal is all there is to see). Any
/// other agent shows its transcript there.
pub fn auto_centre(model: &Model, sid: Option<Sid>) -> Option<PaneId> {
    let sid = sid.filter(|sid| {
        model
            .sessions
            .get(sid)
            .is_some_and(|s| s.agent == Agent::Shell || s.mode == Mode::Tui)
    })?;
    main_of(model, sid)
}

/// The tabs of `sid`: its terminals but the one the centre draws.
pub fn tabs_of(model: &Model, sid: Option<Sid>, centre: Option<PaneId>) -> Vec<PaneId> {
    let Some(sid) = sid else { return Vec::new() };
    terminals(model)
        .into_iter()
        .filter(|p| p.sid == sid && Some(*p) != centre)
        .collect()
}

/// The tab on show: the one picked if it is in `tabs`, else the one that took its place (the
/// same position, or the last), else the first.
pub fn on_show(tabs: &[PaneId], picked: Option<Shown>) -> Option<usize> {
    let last = tabs.len().checked_sub(1)?;
    Some(match picked {
        Some(s) => tabs
            .iter()
            .position(|p| *p == s.pane)
            .unwrap_or(s.ix.min(last)),
        None => 0,
    })
}

/// What "close" asks of the daemon for the tab `pane`, if it may be closed from here. Never the
/// agent's own pane: its session is not closed from a terminal tab. The first pane of a shell
/// session is the session's own (the daemon refuses `PaneClose` for it), so closing it ends the
/// session; every other pane just closes.
pub fn close_req(model: &Model, pane: PaneId) -> Option<Req> {
    let p = model.panes.get(&pane)?;
    let s = model.sessions.get(&pane.sid)?;
    if p.kind == PaneKind::Agent {
        return None;
    }
    let first = model.panes.keys().filter(|q| q.sid == pane.sid).min()?;
    Some(if s.agent == Agent::Shell && *first == pane {
        Req::SessionClose { sid: pane.sid }
    } else {
        Req::PaneClose { pane }
    })
}

/// A tab's label cut to what the strip has room for.
pub fn short(label: &str) -> String {
    match label.char_indices().nth(TAB_CHARS) {
        Some((end, _)) => format!("{}…", &label[..end]),
        None => label.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::state::{Attention, Pane, Session, SessionStatus};
    use femme_proto::Id;

    fn session(n: u128, agent: Agent, mode: Mode) -> Session {
        Session {
            id: Id(n),
            handle: format!("h{n}"),
            rev: 1,
            name: None,
            owner: "board".into(),
            agent,
            mode,
            cwd: "/home/u".into(),
            label: None,
            agent_sid: None,
            model: None,
            status: SessionStatus::Idle,
            attention: Attention::None,
            created: n as i64,
            closed: None,
            permission: None,
            context: None,
            effort: None,
            goal: None,
        }
    }

    fn pane(sid: u128, no: u16, kind: PaneKind) -> Pane {
        Pane {
            id: PaneId { sid: Id(sid), no },
            rev: 1,
            kind,
            title: String::new(),
        }
    }

    fn id(sid: u128, no: u16) -> PaneId {
        PaneId { sid: Id(sid), no }
    }

    /// Session 1: a shell with panes 1 and 2. Session 2: a headless Claude (its pane 1 is the
    /// transcript's) with a shell as pane 2. Session 3: Claude in its TUI, a shell as pane 2.
    fn model() -> Model {
        let mut m = Model::default();
        for s in [
            session(1, Agent::Shell, Mode::Tui),
            session(2, Agent::Claude, Mode::Headless),
            session(3, Agent::Claude, Mode::Tui),
        ] {
            m.sessions.insert(s.id, s);
        }
        for p in [
            pane(1, 1, PaneKind::Shell),
            pane(1, 2, PaneKind::Shell),
            pane(2, 1, PaneKind::Agent),
            pane(2, 2, PaneKind::Shell),
            pane(3, 1, PaneKind::Agent),
            pane(3, 2, PaneKind::Shell),
        ] {
            m.panes.insert(p.id, p);
        }
        m
    }

    #[test]
    fn a_headless_agents_pane_is_not_a_terminal() {
        let m = model();
        assert!(is_terminal(&m, id(1, 1)));
        assert!(
            !is_terminal(&m, id(2, 1)),
            "the transcript's, no pty behind it"
        );
        assert!(is_terminal(&m, id(2, 2)));
        assert!(
            is_terminal(&m, id(3, 1)),
            "an agent in its TUI is a terminal"
        );
        assert!(
            !is_terminal(&m, id(9, 1)),
            "a pane whose session is not known"
        );
        assert_eq!(
            terminals(&m),
            [id(1, 1), id(1, 2), id(2, 2), id(3, 1), id(3, 2)]
        );
    }

    #[test]
    fn closed_sessions_have_no_terminals() {
        let mut m = model();
        m.sessions.get_mut(&Id(1)).unwrap().closed = Some(5);
        assert_eq!(terminals(&m), [id(2, 2), id(3, 1), id(3, 2)]);
    }

    #[test]
    fn the_main_pane_is_the_first_one_when_it_is_a_terminal() {
        let m = model();
        assert_eq!(main_of(&m, Id(1)), Some(id(1, 1)));
        assert_eq!(
            main_of(&m, Id(2)),
            None,
            "its first pane is the transcript's"
        );
        assert_eq!(main_of(&m, Id(3)), Some(id(3, 1)), "the agent's TUI");
        assert_eq!(main_of(&m, Id(9)), None);
        let mut closed = model();
        closed.sessions.get_mut(&Id(1)).unwrap().closed = Some(5);
        assert_eq!(main_of(&closed, Id(1)), None);
    }

    #[test]
    fn the_centre_draws_the_first_terminal_of_a_shell_or_an_agent_in_its_tui() {
        let m = model();
        assert_eq!(auto_centre(&m, Some(Id(1))), Some(id(1, 1)));
        assert_eq!(auto_centre(&m, Some(Id(2))), None, "a transcript is there");
        assert_eq!(
            auto_centre(&m, Some(Id(3))),
            Some(id(3, 1)),
            "no transcript"
        );
        assert_eq!(auto_centre(&m, None), None);
        assert_eq!(auto_centre(&m, Some(Id(9))), None);
    }

    #[test]
    fn the_centre_told_or_assumed() {
        let m = model();
        let told = |p| Centre::Is(p).pane(&m, Some(Id(1)));
        assert_eq!(Centre::Auto.pane(&m, Some(Id(1))), Some(id(1, 1)));
        assert_eq!(Centre::Auto.pane(&m, Some(Id(3))), Some(id(3, 1)));
        assert_eq!(Centre::Auto.pane(&m, Some(Id(2))), None);
        assert_eq!(
            told(None),
            None,
            "the shell says there is no terminal there"
        );
        assert_eq!(told(Some(id(1, 2))), Some(id(1, 2)));
        assert_eq!(
            Centre::Is(Some(id(3, 1))).pane(&m, Some(Id(1))),
            None,
            "another session's pane is not this session's"
        );
        assert_eq!(Centre::Is(Some(id(1, 1))).pane(&m, None), None);
    }

    #[test]
    fn the_strip_lists_the_session_without_the_centres_pane() {
        let m = model();
        assert_eq!(tabs_of(&m, Some(Id(1)), None), [id(1, 1), id(1, 2)]);
        assert_eq!(tabs_of(&m, Some(Id(1)), Some(id(1, 1))), [id(1, 2)]);
        assert_eq!(tabs_of(&m, Some(Id(2)), None), [id(2, 2)]);
        assert_eq!(
            tabs_of(&m, Some(Id(3)), Some(id(1, 1))),
            [id(3, 1), id(3, 2)]
        );
        assert_eq!(tabs_of(&m, Some(Id(3)), Some(id(3, 1))), [id(3, 2)]);
        assert!(tabs_of(&m, None, None).is_empty());
        assert!(tabs_of(&m, Some(Id(9)), None).is_empty());
    }

    #[test]
    fn the_tab_on_show_is_the_one_picked_or_the_one_that_took_its_place() {
        let tabs = [id(1, 2), id(1, 3), id(1, 4)];
        let picked = |pane, ix| Some(Shown { pane, ix });
        assert_eq!(on_show(&[], None), None);
        assert_eq!(on_show(&[], picked(id(1, 2), 0)), None);
        assert_eq!(on_show(&tabs, None), Some(0));
        assert_eq!(on_show(&tabs, picked(id(1, 3), 1)), Some(1));
        assert_eq!(
            on_show(&tabs, picked(id(1, 4), 0)),
            Some(2),
            "where it is, not where it was"
        );
        // The one picked went: its neighbour on the right, else the last.
        assert_eq!(on_show(&tabs, picked(id(1, 9), 1)), Some(1));
        assert_eq!(on_show(&tabs, picked(id(1, 9), 7)), Some(2));
        // A pane just opened and not listed yet: the last tab stands in.
        assert_eq!(on_show(&tabs, picked(id(1, 5), usize::MAX)), Some(2));
    }

    #[test]
    fn close_asks_for_what_the_daemon_allows_and_never_ends_an_agent() {
        let m = model();
        assert_eq!(
            close_req(&m, id(1, 1)),
            Some(Req::SessionClose { sid: Id(1) }),
            "a shell session's own pane is the session"
        );
        assert_eq!(
            close_req(&m, id(1, 2)),
            Some(Req::PaneClose { pane: id(1, 2) })
        );
        assert_eq!(close_req(&m, id(3, 1)), None, "the agent's TUI");
        assert_eq!(close_req(&m, id(2, 1)), None, "the agent's transcript");
        assert_eq!(
            close_req(&m, id(3, 2)),
            Some(Req::PaneClose { pane: id(3, 2) }),
            "a shell next to an agent is only a pane"
        );
        assert_eq!(
            close_req(&m, id(2, 2)),
            Some(Req::PaneClose { pane: id(2, 2) }),
            "the first shell of a headless agent is no session of its own"
        );
        assert_eq!(close_req(&m, id(9, 1)), None);
    }

    #[test]
    fn labels_are_cut_for_the_strip() {
        assert_eq!(short("bash"), "bash");
        assert_eq!(short(&"a".repeat(TAB_CHARS)), "a".repeat(TAB_CHARS));
        assert_eq!(
            short(&"é".repeat(40)),
            format!("{}…", "é".repeat(TAB_CHARS))
        );
    }
}
