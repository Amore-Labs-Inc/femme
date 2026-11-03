//! The shell's pure parts: which view the centre shows for which session, what the right-hand hover
//! panel does with the pointer and the keyboard focus, how the focus cycle walks, and the sizes of
//! the frame. No gpui in here, so every rule is unit-tested; [`crate::app`] only wires them to
//! events and draws.

use femme_proto::state::{Agent, Mode, Session, SessionStatus};
use femme_proto::Sid;

use crate::net::{Link, Load, Model};

/// The desk margin around the frame, and the gutter between its surfaces ([`crate::theme::GUTTER`]).
/// The width rules below are pure, so they hold the sizes as `f32`; a test pins them to the theme's.
pub const MARGIN: f32 = 6.0;
/// Height of the top bar ([`crate::theme::TOP_BAR_H`]).
pub const TOP_BAR: f32 = 36.0;
/// Width of the right-hand hover strip ([`crate::theme::STRIP_W`]).
pub const STRIP_W: f32 = 28.0;
/// Narrowest the right-hand panel gets.
pub const RIGHT_MIN: f32 = 320.0;
/// Widest the right-hand panel gets.
pub const RIGHT_MAX: f32 = 480.0;
/// Width the right-hand panel opens with.
pub const RIGHT_DEFAULT: f32 = 380.0;

/// Why the centre shows a card instead of a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Card {
    /// The app was started without a daemon to talk to.
    NoDaemon,
    /// The first connection is still being made.
    Connecting,
    /// The daemon is out of reach, in the transport's words.
    Offline(String),
    /// Connected, with no session on show; `live` sessions could be picked.
    NoSession {
        /// How many live sessions the list has.
        live: usize,
        /// What the app says it is doing about it ("starting claude…") or why it failed; may be empty.
        note: String,
    },
    /// A session was picked and the daemon has not listed it yet.
    Opening,
    /// The session on show has ended and has nothing left to look at.
    Closed,
}

impl Card {
    /// The card's headline.
    pub fn title(&self) -> &'static str {
        match self {
            Card::NoDaemon => "no daemon configured",
            Card::Connecting => "connecting…",
            Card::Offline(_) => "offline",
            Card::NoSession { .. } => "no session",
            Card::Opening => "opening the session…",
            Card::Closed => "session closed",
        }
    }

    /// What to do about it, or why.
    pub fn detail(&self) -> String {
        match self {
            Card::NoDaemon => "start femme-desktop with --daemon ADDR:PORT --fingerprint HEX, \
                 or --ssh HOST (FEMME_DAEMON, FEMME_FINGERPRINT and FEMME_SSH work too)"
                .into(),
            Card::Connecting => String::new(),
            Card::Offline(why) => why.clone(),
            Card::NoSession { note, .. } if !note.is_empty() => note.clone(),
            Card::NoSession { live: 0, .. } => "nothing is running on the daemon".into(),
            Card::NoSession { .. } => "pick one in the list on the left".into(),
            Card::Opening => String::new(),
            Card::Closed => "pick another one in the list on the left".into(),
        }
    }
}

/// What the centre of the window shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Centre {
    /// A card instead of a session.
    Card(Card),
    /// An agent session that streams a structured transcript: the transcript above the composer.
    Transcript(Sid),
    /// A terminal pane fills it: a shell, or an agent running its own TUI (it streams no
    /// transcript, the pane is all there is to see).
    Terminal(Sid),
}

/// The session on show: what the modules below the shell are told about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    /// The session `Net` says is current, listed by the daemon yet or not.
    pub sid: Option<Sid>,
    /// What the centre makes of it.
    pub centre: Centre,
    /// The session's directory, empty while it is not listed.
    pub cwd: String,
}

impl Shown {
    /// The frame of an app that has no daemon.
    pub fn no_daemon() -> Self {
        Shown {
            sid: None,
            centre: Centre::Card(Card::NoDaemon),
            cwd: String::new(),
        }
    }

    /// What the daemon's state says is on show. `note` is what the app says about having no session:
    /// what it is starting, or why that failed ([`crate::net::Net::starting`], `Net::error`).
    pub fn of(model: &Model, note: Option<&str>) -> Self {
        let Some(sid) = model.current else {
            let card = match &model.link {
                Link::Connecting => Card::Connecting,
                Link::Down(why) => Card::Offline(why.clone()),
                // The list of sessions is not in yet: whether there is one to show is not known.
                Link::Up(_) if model.load != Load::Done => Card::Connecting,
                Link::Up(_) => Card::NoSession {
                    live: model.live().len(),
                    note: note.unwrap_or_default().into(),
                },
            };
            return Shown {
                sid: None,
                centre: Centre::Card(card),
                cwd: String::new(),
            };
        };
        match model.sessions.get(&sid) {
            Some(s) => Shown {
                sid: Some(sid),
                centre: centre_of(s),
                cwd: s.cwd.clone(),
            },
            None => Shown {
                sid: Some(sid),
                centre: Centre::Card(match &model.link {
                    Link::Down(why) => Card::Offline(why.clone()),
                    _ => Card::Opening,
                }),
                cwd: String::new(),
            },
        }
    }

    /// What is on show next, given what is on show now. After a `Reset` the daemon lists its sessions
    /// again, and for that while the session on show is not in the list: it stays as it is instead
    /// of passing through an "opening" card, which would make the modules forget it (the composer's
    /// draft, the file tree, the transcript's place).
    pub fn after(&self, model: &Model, note: Option<&str>) -> Self {
        let next = Shown::of(model, note);
        let relisting =
            model.load == Load::Rebuilding && next.centre == Centre::Card(Card::Opening);
        if relisting && next.sid == self.sid {
            return self.clone();
        }
        next
    }

    /// The session whose transcript is on show, if any.
    pub fn transcript(&self) -> Option<Sid> {
        match self.centre {
            Centre::Transcript(sid) => Some(sid),
            _ => None,
        }
    }
}

/// The centre for a listed session: a shell, and an agent that runs its own TUI, show their terminal
/// (a TUI session writes no transcript items); every other agent shows its transcript. An ended
/// session with no terminal left gets a card; an ended transcript can still be read.
fn centre_of(s: &Session) -> Centre {
    let ended = s.closed.is_some() || s.status == SessionStatus::Closed;
    match (s.agent, s.mode) {
        (Agent::Shell, _) | (_, Mode::Tui) if ended => Centre::Card(Card::Closed),
        (Agent::Shell, _) | (_, Mode::Tui) => Centre::Terminal(s.id),
        _ => Centre::Transcript(s.id),
    }
}

/// The tabs of the right-hand panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    /// The panes of the session on show.
    Terminal,
    /// The session's directory.
    Files,
}

impl Tab {
    /// Both tabs, in the order of the strip's icons.
    pub const ALL: [Tab; 2] = [Tab::Terminal, Tab::Files];

    /// The tab's name: its icon's label and the panel's title.
    pub fn label(self) -> &'static str {
        match self {
            Tab::Terminal => "terminal",
            Tab::Files => "files",
        }
    }
}

/// What [`Right::settle`] did to the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settle {
    /// Nothing moved.
    Nothing,
    /// The panel opened, or switched to this tab.
    Open(Tab),
    /// The panel closed.
    Close,
}

/// The right-hand hover panel: which tab is open, where the pointer is, how wide it is.
///
/// It opens on the tab whose icon the pointer rests on (the strip itself opens the tab used last),
/// and closes when the pointer is on neither the strip nor the panel and nothing inside the panel has
/// the keyboard. A drag of its edge never closes it. The caller reports the pointer with
/// `hover_*`, asks [`Right::settle`] after a short delay, and tells it what it cannot know
/// (the focus, a drag in progress).
#[derive(Clone, Debug, PartialEq)]
pub struct Right {
    open: Option<Tab>,
    last: Tab,
    over_strip: bool,
    icon: Option<Tab>,
    over_panel: bool,
    /// The pointer is on the resize grip: it occludes the panel, which then reads as not hovered.
    over_grip: bool,
    /// Closed by hand while the pointer was on the strip: it stays closed until the pointer leaves.
    suppressed: bool,
    width: f32,
}

impl Default for Right {
    fn default() -> Self {
        Right {
            open: None,
            last: Tab::Terminal,
            over_strip: false,
            icon: None,
            over_panel: false,
            over_grip: false,
            suppressed: false,
            width: RIGHT_DEFAULT,
        }
    }
}

impl Right {
    /// A closed panel at its default width.
    pub fn new() -> Self {
        Self::default()
    }

    /// The tab on show, `None` while the panel is closed.
    pub fn open(&self) -> Option<Tab> {
        self.open
    }

    /// The tab the strip opens when the pointer is on it but on no icon.
    pub fn last(&self) -> Tab {
        self.last
    }

    /// The panel's width, in logical pixels.
    pub fn width(&self) -> f32 {
        self.width
    }

    /// The icon under the pointer, if any.
    pub fn icon(&self) -> Option<Tab> {
        self.icon
    }

    /// Opens `tab` (a click, a key): it also becomes the tab the strip opens next time.
    pub fn show(&mut self, tab: Tab) {
        self.open = Some(tab);
        self.last = tab;
        self.suppressed = false;
    }

    /// Closes the panel (a key). With the pointer still on the strip it stays closed until the
    /// pointer has left it, or the next hover would open it again at once.
    pub fn hide(&mut self) {
        self.open = None;
        self.over_panel = false;
        self.over_grip = false;
        self.suppressed = self.over_strip;
    }

    /// The pointer entered or left the strip.
    pub fn hover_strip(&mut self, on: bool) {
        self.over_strip = on;
        if !on {
            self.suppressed = false;
            self.icon = None;
        }
    }

    /// The pointer entered or left `tab`'s icon.
    pub fn hover_icon(&mut self, tab: Tab, on: bool) {
        if on {
            self.icon = Some(tab);
        } else if self.icon == Some(tab) {
            self.icon = None;
        }
    }

    /// The pointer entered or left the panel.
    pub fn hover_panel(&mut self, on: bool) {
        self.over_panel = on;
    }

    /// The pointer entered or left the panel's resize grip (which occludes the panel under it).
    pub fn hover_grip(&mut self, on: bool) {
        self.over_grip = on;
    }

    /// The tab the pointer asks for: its icon's, else the strip's (the last one used).
    pub fn wanted(&self) -> Option<Tab> {
        if self.suppressed {
            return None;
        }
        self.icon.or(self.over_strip.then_some(self.last))
    }

    /// Whether the panel is due to close: it is open, the pointer is on neither it nor the strip,
    /// nothing inside has the keyboard and no drag is under way.
    pub fn should_close(&self, focus_inside: bool, dragging: bool) -> bool {
        self.open.is_some()
            && self.wanted().is_none()
            && !self.over_strip
            && !self.over_panel
            && !self.over_grip
            && !focus_inside
            && !dragging
    }

    /// Applies what the pointer and the focus ask for now: opens or switches to the wanted tab, or
    /// closes a panel that is due to.
    pub fn settle(&mut self, focus_inside: bool, dragging: bool) -> Settle {
        if let Some(tab) = self.wanted() {
            if self.open != Some(tab) {
                self.show(tab);
                return Settle::Open(tab);
            }
            return Settle::Nothing;
        }
        if self.should_close(focus_inside, dragging) {
            self.hide();
            return Settle::Close;
        }
        Settle::Nothing
    }

    /// Sets the width from a drag, kept inside [`RIGHT_MIN`]..=[`RIGHT_MAX`] and the window.
    pub fn set_width(&mut self, width: f32, viewport: f32) {
        self.width = clamp_width(width, viewport);
    }

    /// Keeps the width inside the window after it was resized.
    pub fn fit(&mut self, viewport: f32) {
        self.width = clamp_width(self.width, viewport);
    }
}

/// `width` kept inside [`RIGHT_MIN`]..=[`RIGHT_MAX`], and inside a window `viewport` wide.
pub fn clamp_width(width: f32, viewport: f32) -> f32 {
    let room = (viewport - 2.0 * MARGIN - STRIP_W).max(0.0);
    width.clamp(RIGHT_MIN, RIGHT_MAX).min(room)
}

/// The panel's width when its left edge is dragged to `pointer_x` (window coordinates) in a window
/// `viewport` wide: the panel ends where the strip begins.
pub fn width_for(pointer_x: f32, viewport: f32) -> f32 {
    viewport - MARGIN - STRIP_W - pointer_x
}

/// The places the keyboard walks through with F6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The sessions column.
    Sidebar,
    /// The composer, or the terminal pane that fills the centre.
    Centre,
    /// The right-hand panel (the cycle opens it).
    Right,
}

/// The stop after (or, with `back`, before) `from`; without a current stop the walk starts in the
/// centre. The sidebar is a stop whether it is open or collapsed to its strip: the strip takes keys too.
pub fn next_stop(from: Option<Stop>, back: bool) -> Stop {
    const STOPS: [Stop; 3] = [Stop::Sidebar, Stop::Centre, Stop::Right];
    let Some(at) = from.and_then(|f| STOPS.iter().position(|s| *s == f)) else {
        return Stop::Centre;
    };
    let n = STOPS.len();
    STOPS[if back { (at + n - 1) % n } else { (at + 1) % n }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_client::Link as Transport;
    use femme_proto::state::Attention;
    use femme_proto::Id;

    use crate::theme;

    #[test]
    fn the_sizes_are_the_themes() {
        assert_eq!(MARGIN, f32::from(theme::GUTTER));
        assert_eq!(TOP_BAR, f32::from(theme::TOP_BAR_H));
        assert_eq!(STRIP_W, f32::from(theme::STRIP_W));
    }

    fn session(n: u128, agent: Agent, mode: Mode) -> Session {
        Session {
            id: Id(n),
            handle: format!("h{n}"),
            rev: 1,
            name: None,
            owner: "board".into(),
            agent,
            mode,
            cwd: format!("/home/u/work{n}"),
            label: None,
            agent_sid: None,
            model: None,
            status: SessionStatus::Idle,
            attention: Attention::None,
            created: n as i64,
            closed: None,
        }
    }

    fn model(link: Link, current: Option<u128>, sessions: Vec<Session>) -> Model {
        let mut m = Model {
            link,
            current: current.map(Id),
            load: Load::Done,
            ..Model::default()
        };
        for s in sessions {
            m.sessions.insert(s.id, s);
        }
        m
    }

    fn up() -> Link {
        Link::Up(Transport::Quic)
    }

    fn no_session(live: usize, note: &str) -> Card {
        Card::NoSession {
            live,
            note: note.into(),
        }
    }

    #[test]
    fn no_current_session_says_why_there_is_none() {
        let card = |m: &Model| Shown::of(m, None).centre;
        assert_eq!(
            card(&model(Link::Connecting, None, vec![])),
            Centre::Card(Card::Connecting)
        );
        assert_eq!(
            card(&model(Link::Down("refused".into()), None, vec![])),
            Centre::Card(Card::Offline("refused".into()))
        );
        assert_eq!(
            card(&model(up(), None, vec![])),
            Centre::Card(no_session(0, ""))
        );
        let two = vec![
            session(1, Agent::Shell, Mode::Tui),
            session(2, Agent::Claude, Mode::Headless),
        ];
        assert_eq!(
            card(&model(up(), None, two)),
            Centre::Card(no_session(2, ""))
        );
        assert_eq!(Shown::no_daemon().centre, Centre::Card(Card::NoDaemon));
        assert_eq!(Shown::no_daemon().sid, None);
    }

    #[test]
    fn a_link_that_is_up_without_the_list_is_still_connecting() {
        for load in [Load::Waiting, Load::Rebuilding] {
            let m = Model {
                load,
                ..model(up(), None, vec![])
            };
            assert_eq!(Shown::of(&m, None).centre, Centre::Card(Card::Connecting));
        }
        // A session that is named stays named while the list is being rebuilt.
        let m = Model {
            load: Load::Rebuilding,
            ..model(up(), Some(4), vec![])
        };
        assert_eq!(Shown::of(&m, None).centre, Centre::Card(Card::Opening));
    }

    #[test]
    fn the_session_on_show_stays_while_the_daemon_lists_its_sessions_again() {
        let agent = || vec![session(2, Agent::Claude, Mode::Headless)];
        let listed = model(up(), Some(2), agent());
        let on_show = Shown::of(&listed, None);
        assert_eq!(on_show.centre, Centre::Transcript(Id(2)));
        // A `Reset` emptied the list and the session is still named: nothing moves.
        let relisting = Model {
            load: Load::Rebuilding,
            ..model(up(), Some(2), vec![])
        };
        assert_eq!(on_show.after(&relisting, None), on_show);
        // The full set is back: the same, from the list.
        assert_eq!(on_show.after(&listed, None), on_show);
        // Done listing and the session is not there: it is opening (just created), as before.
        let done = Model {
            load: Load::Done,
            ..model(up(), Some(2), vec![])
        };
        assert_eq!(
            on_show.after(&done, None).centre,
            Centre::Card(Card::Opening)
        );
        // Another session is named: that is what shows, even while relisting.
        let other = Model {
            load: Load::Rebuilding,
            ..model(up(), Some(3), vec![])
        };
        let next = on_show.after(&other, None);
        assert_eq!(
            (next.sid, next.centre),
            (Some(Id(3)), Centre::Card(Card::Opening))
        );
        // Nothing was on show before: the card says so.
        let first = Shown::of(&model(up(), None, vec![]), None);
        assert_eq!(
            first.after(&relisting, None).centre,
            Centre::Card(Card::Opening)
        );
    }

    #[test]
    fn the_no_session_card_carries_what_the_app_says() {
        let m = model(up(), None, vec![]);
        let shown = Shown::of(&m, Some("starting claude…"));
        assert_eq!(
            shown.centre,
            Centre::Card(no_session(0, "starting claude…"))
        );
        assert_eq!(shown.sid, None);
        // A different note is a different `Shown`: the shell redraws for it.
        assert_ne!(shown, Shown::of(&m, None));
    }

    #[test]
    fn a_shell_fills_the_centre_with_its_terminal() {
        let m = model(up(), Some(1), vec![session(1, Agent::Shell, Mode::Tui)]);
        let shown = Shown::of(&m, None);
        assert_eq!(shown.sid, Some(Id(1)));
        assert_eq!(shown.centre, Centre::Terminal(Id(1)));
        assert_eq!(shown.cwd, "/home/u/work1");
        assert_eq!(shown.transcript(), None);
    }

    #[test]
    fn a_headless_agent_shows_transcript_and_composer() {
        for agent in [Agent::Claude, Agent::Codex, Agent::Pi] {
            let m = model(up(), Some(2), vec![session(2, agent, Mode::Headless)]);
            let shown = Shown::of(&m, None);
            assert_eq!(shown.centre, Centre::Transcript(Id(2)), "{agent:?}");
            assert_eq!(shown.transcript(), Some(Id(2)));
        }
    }

    #[test]
    fn an_agent_in_its_own_tui_shows_the_terminal() {
        // A TUI session writes no transcript items: its pane is all there is to see.
        let m = model(up(), Some(3), vec![session(3, Agent::Claude, Mode::Tui)]);
        assert_eq!(Shown::of(&m, None).centre, Centre::Terminal(Id(3)));
    }

    #[test]
    fn an_ended_session_keeps_what_can_still_be_read() {
        let mut shell = session(1, Agent::Shell, Mode::Tui);
        shell.closed = Some(5);
        let mut claude = session(2, Agent::Claude, Mode::Headless);
        claude.status = SessionStatus::Closed;
        let mut tui = session(3, Agent::Claude, Mode::Tui);
        tui.status = SessionStatus::Closed;
        let m = model(up(), Some(1), vec![shell, claude, tui]);
        assert_eq!(Shown::of(&m, None).centre, Centre::Card(Card::Closed));
        let m = Model {
            current: Some(Id(2)),
            ..m
        };
        assert_eq!(Shown::of(&m, None).centre, Centre::Transcript(Id(2)));
        let m = Model {
            current: Some(Id(3)),
            ..m
        };
        assert_eq!(Shown::of(&m, None).centre, Centre::Card(Card::Closed));
    }

    #[test]
    fn a_session_the_daemon_has_not_listed_yet_is_opening() {
        let m = model(up(), Some(9), vec![]);
        let shown = Shown::of(&m, None);
        assert_eq!(shown.sid, Some(Id(9)));
        assert_eq!(shown.centre, Centre::Card(Card::Opening));
        assert_eq!(shown.cwd, "");
        let m = model(Link::Down("gone".into()), Some(9), vec![]);
        assert_eq!(
            Shown::of(&m, None).centre,
            Centre::Card(Card::Offline("gone".into()))
        );
    }

    #[test]
    fn a_listed_session_stays_on_show_while_the_link_is_down() {
        let m = model(
            Link::Down("gone".into()),
            Some(1),
            vec![session(1, Agent::Shell, Mode::Tui)],
        );
        assert_eq!(Shown::of(&m, None).centre, Centre::Terminal(Id(1)));
    }

    #[test]
    fn cards_have_words() {
        let all = [
            Card::NoDaemon,
            Card::Connecting,
            Card::Offline("x".into()),
            no_session(0, ""),
            no_session(3, ""),
            Card::Opening,
            Card::Closed,
        ];
        for c in &all {
            assert!(!c.title().is_empty(), "{c:?}");
        }
        assert!(Card::NoDaemon.detail().contains("--daemon"));
        assert_eq!(Card::Offline("refused".into()).detail(), "refused");
        assert_ne!(no_session(0, "").detail(), no_session(2, "").detail());
        assert_eq!(
            no_session(0, "no session could be started").detail(),
            "no session could be started"
        );
    }

    fn on_strip() -> Right {
        let mut r = Right::new();
        r.hover_strip(true);
        r
    }

    #[test]
    fn hovering_an_icon_opens_its_tab_and_leaving_closes_it() {
        let mut r = Right::new();
        assert_eq!(r.settle(false, false), Settle::Nothing);
        r.hover_strip(true);
        r.hover_icon(Tab::Files, true);
        assert_eq!(r.settle(false, false), Settle::Open(Tab::Files));
        assert_eq!(r.open(), Some(Tab::Files));
        // The pointer crosses from the strip into the panel.
        r.hover_panel(true);
        r.hover_icon(Tab::Files, false);
        r.hover_strip(false);
        assert_eq!(r.settle(false, false), Settle::Nothing);
        assert_eq!(r.open(), Some(Tab::Files));
        // And leaves the panel.
        r.hover_panel(false);
        assert_eq!(r.settle(false, false), Settle::Close);
        assert_eq!(r.open(), None);
    }

    #[test]
    fn the_strip_alone_opens_the_tab_used_last() {
        let mut r = on_strip();
        assert_eq!(r.settle(false, false), Settle::Open(Tab::Terminal));
        r.show(Tab::Files);
        r.hide();
        r.hover_strip(false);
        r.hover_strip(true);
        assert_eq!(r.settle(false, false), Settle::Open(Tab::Files));
    }

    #[test]
    fn hovering_the_other_icon_switches_tabs() {
        let mut r = on_strip();
        r.hover_icon(Tab::Terminal, true);
        assert_eq!(r.settle(false, false), Settle::Open(Tab::Terminal));
        r.hover_icon(Tab::Terminal, false);
        r.hover_icon(Tab::Files, true);
        assert_eq!(r.settle(false, false), Settle::Open(Tab::Files));
        assert_eq!(r.settle(false, false), Settle::Nothing);
        assert_eq!(r.last(), Tab::Files);
    }

    #[test]
    fn the_keyboard_inside_keeps_it_open() {
        let mut r = Right::new();
        r.show(Tab::Terminal);
        assert_eq!(r.settle(true, false), Settle::Nothing);
        assert_eq!(r.open(), Some(Tab::Terminal));
        // The focus moved on: now it closes.
        assert_eq!(r.settle(false, false), Settle::Close);
    }

    #[test]
    fn a_drag_of_the_edge_keeps_it_open() {
        let mut r = Right::new();
        r.show(Tab::Files);
        assert!(!r.should_close(false, true));
        assert_eq!(r.settle(false, true), Settle::Nothing);
        assert!(r.should_close(false, false));
    }

    #[test]
    fn the_resize_grip_counts_as_the_panel() {
        // The grip occludes the panel: crossing onto it reads as leaving the panel.
        let mut r = Right::new();
        r.show(Tab::Terminal);
        r.hover_panel(true);
        r.hover_grip(true);
        r.hover_panel(false);
        assert_eq!(r.settle(false, false), Settle::Nothing);
        r.hover_grip(false);
        assert_eq!(r.settle(false, false), Settle::Close);
    }

    #[test]
    fn a_click_shows_the_tab_at_once() {
        let mut r = Right::new();
        r.show(Tab::Files);
        assert_eq!(r.open(), Some(Tab::Files));
        assert_eq!(r.last(), Tab::Files);
    }

    #[test]
    fn hiding_under_the_pointer_does_not_reopen_until_it_left() {
        let mut r = on_strip();
        r.hover_icon(Tab::Terminal, true);
        assert_eq!(r.settle(false, false), Settle::Open(Tab::Terminal));
        r.hide();
        assert_eq!(r.wanted(), None);
        assert_eq!(r.settle(false, false), Settle::Nothing);
        assert_eq!(r.open(), None);
        r.hover_icon(Tab::Terminal, false);
        r.hover_strip(false);
        r.hover_strip(true);
        assert_eq!(r.settle(false, false), Settle::Open(Tab::Terminal));
    }

    #[test]
    fn hiding_forgets_the_pointer_in_the_panel() {
        // The panel is gone: no leave event will come for it.
        let mut r = Right::new();
        r.show(Tab::Terminal);
        r.hover_panel(true);
        r.hide();
        r.show(Tab::Files);
        assert_eq!(r.settle(false, false), Settle::Close);
    }

    #[test]
    fn the_width_stays_between_the_bounds_and_inside_the_window() {
        let mut r = Right::new();
        assert!((RIGHT_MIN..=RIGHT_MAX).contains(&r.width()));
        r.set_width(100.0, 1400.0);
        assert_eq!(r.width(), RIGHT_MIN);
        r.set_width(900.0, 1400.0);
        assert_eq!(r.width(), RIGHT_MAX);
        r.set_width(400.0, 1400.0);
        assert_eq!(r.width(), 400.0);
        // A window narrower than the panel's minimum plus the strip and the margins.
        r.set_width(400.0, 300.0);
        assert_eq!(r.width(), 300.0 - 2.0 * MARGIN - STRIP_W);
        r.set_width(400.0, 10.0);
        assert_eq!(r.width(), 0.0);
        let mut r = Right::new();
        r.set_width(470.0, 1400.0);
        r.fit(1400.0);
        assert_eq!(r.width(), 470.0);
        r.fit(400.0);
        assert_eq!(r.width(), 400.0 - 2.0 * MARGIN - STRIP_W);
    }

    #[test]
    fn dragging_the_left_edge_sets_the_width() {
        // The panel ends where the strip begins: 1400 - 6 - 28 = 1366.
        assert_eq!(width_for(1000.0, 1400.0), 366.0);
        assert_eq!(width_for(900.0, 1400.0), 466.0);
        assert!(width_for(1400.0, 1400.0) < 0.0);
    }

    #[test]
    fn the_focus_cycle_walks_sidebar_centre_right() {
        use Stop::*;
        assert_eq!(next_stop(Some(Sidebar), false), Centre);
        assert_eq!(next_stop(Some(Centre), false), Right);
        assert_eq!(next_stop(Some(Right), false), Sidebar);
        assert_eq!(next_stop(Some(Sidebar), true), Right);
        assert_eq!(next_stop(Some(Right), true), Centre);
        assert_eq!(next_stop(Some(Centre), true), Sidebar);
    }

    #[test]
    fn the_focus_cycle_starts_in_the_centre() {
        assert_eq!(next_stop(None, false), Stop::Centre);
        assert_eq!(next_stop(None, true), Stop::Centre);
    }
}
