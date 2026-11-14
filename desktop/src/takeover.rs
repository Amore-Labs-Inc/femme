//! The chat ↔ terminal toggle of a Claude session: `open in terminal` hands a headless chat to
//! Claude Code's own TUI (`Req::Takeover { to: Mode::Tui }`), `back to chat` hands it back
//! (`to: Mode::Headless`). The daemon does the switch and the session's row brings the new `mode`
//! through the `Changes` stream; the centre follows `Session::mode` ([`crate::frame`]), never a
//! local flag. This module only knows the words and when the button is waiting.

use std::time::{Duration, Instant};

use femme_proto::state::{Agent, Mode, Session, SessionStatus};
use femme_proto::Sid;

/// How long the button stays disabled after the daemon took the request (or said a switch was
/// already running) while the session's row still shows the old mode. Past it the button is live
/// again, so a switch whose row never came does not lock the session out.
pub const WAIT: Duration = Duration::from_secs(20);

/// A switch the user asked for: the session, and the mode it was in when asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Switch {
    /// The session to switch.
    pub sid: Sid,
    /// Its mode when the user asked.
    pub from: Mode,
    /// When the user asked.
    pub since: Instant,
}

impl Switch {
    /// Whether the switch still holds the button of session `s` at `now`: same session, its row
    /// has not left the mode it was in, and the wait is not over.
    pub fn holds(&self, s: &Session, now: Instant) -> bool {
        self.sid == s.id && s.mode == self.from && now.duration_since(self.since) < WAIT
    }
}

/// The button's label for a session in `mode`.
pub fn label(mode: Mode) -> &'static str {
    match mode {
        Mode::Headless => "open in terminal",
        Mode::Tui => "back to chat",
    }
}

/// The mode the button asks for.
pub fn target(mode: Mode) -> Mode {
    match mode {
        Mode::Headless => Mode::Tui,
        Mode::Tui => Mode::Headless,
    }
}

/// Whether session `s` gets the button: a live Claude session (shells and other agents have no
/// TUI to hand over to; an ended session has nothing to switch).
pub fn offered(s: &Session) -> bool {
    s.agent == Agent::Claude && s.closed.is_none() && s.status != SessionStatus::Closed
}

/// The daemon's `Busy` answer, as the client prints it (`Busy: <msg>`): a switch is running.
pub fn is_busy(error: &str) -> bool {
    error.starts_with("Busy")
}

/// The tooltip of the button: what a click does, or why it is waiting.
pub fn tip(mode: Mode, waiting: bool) -> &'static str {
    match (mode, waiting) {
        (_, true) => "switching… the session's mode changes when the daemon is done",
        (Mode::Headless, false) => "hand this chat to Claude Code's own terminal (ctrl-shift-t)",
        (Mode::Tui, false) => "back to the FEMME chat (ctrl-shift-t)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::state::Attention;
    use femme_proto::Id;

    fn session(agent: Agent, mode: Mode) -> Session {
        Session {
            id: Id(1),
            handle: "h1".into(),
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
            created: 1,
            closed: None,
            permission: None,
            context: None,
            effort: None,
            goal: None,
        }
    }

    #[test]
    fn the_label_names_where_the_click_goes() {
        assert_eq!(label(Mode::Headless), "open in terminal");
        assert_eq!(label(Mode::Tui), "back to chat");
        assert_eq!(target(Mode::Headless), Mode::Tui);
        assert_eq!(target(Mode::Tui), Mode::Headless);
    }

    #[test]
    fn only_a_live_claude_session_gets_the_button() {
        assert!(offered(&session(Agent::Claude, Mode::Headless)));
        assert!(offered(&session(Agent::Claude, Mode::Tui)));
        assert!(!offered(&session(Agent::Shell, Mode::Tui)));
        let mut ended = session(Agent::Claude, Mode::Headless);
        ended.closed = Some(2);
        assert!(!offered(&ended));
    }

    #[test]
    fn busy_is_read_from_the_clients_text() {
        assert!(is_busy("Busy: a switch is running"));
        assert!(!is_busy("NotFound: no such session"));
    }

    #[test]
    fn a_switch_holds_until_the_mode_flips_or_the_wait_ends() {
        let now = Instant::now();
        let s = session(Agent::Claude, Mode::Headless);
        let sw = Switch {
            sid: s.id,
            from: Mode::Headless,
            since: now,
        };
        assert!(sw.holds(&s, now));
        assert!(!sw.holds(&s, now + WAIT));
        let flipped = session(Agent::Claude, Mode::Tui);
        assert!(!sw.holds(&flipped, now));
        let mut other = session(Agent::Claude, Mode::Headless);
        other.id = Id(2);
        assert!(!sw.holds(&other, now));
    }
}
