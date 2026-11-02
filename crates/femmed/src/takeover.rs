//! Headless ↔ TUI takeover (plan §5). Pure: feed events and ticks, get actions for the session
//! actor to perform on the holder. Never two agent processes: the headless agent is gone before
//! the TUI starts, and the TUI pane has exited before the headless agent is resumed.
//!
//! ```text
//! Headless --Takeover(Tui)--> StoppingAgent --AgentExited--> Tui
//!   StoppingAgent: close stdin; after 2 s SIGTERM; after 4 s SIGKILL
//! Tui --Takeover(Headless) / PaneExited--> (WaitingStop while a turn runs) --> StoppingTui --PaneExited--> Headless
//!   StoppingTui: send the exit command; after 5 s SIGTERM; after 7 s SIGKILL; then Backfill + resume headless
//! ```

use femme_proto::state::Mode;

pub const SIGTERM: i32 = 15;
pub const SIGKILL: i32 = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Graceful,
    Term,
    Kill,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Headless,
    StoppingAgent {
        since: u64,
        stage: Stage,
    },
    Tui,
    /// A turn is running in the TUI; the exit command goes once the agent's Stop hook fires.
    WaitingStop,
    StoppingTui {
        since: u64,
        stage: Stage,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ev {
    Takeover {
        to: Mode,
        now: u64,
    },
    AgentExited,
    PaneExited,
    /// The agent's Stop hook (TUI mode) or TurnEnded (headless) was seen.
    TurnEnded {
        now: u64,
    },
    TurnStarted,
    Tick {
        now: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Act {
    AgentCloseStdin,
    SignalAgent(i32),
    /// Start the agent's TUI in pane 0 (`claude --resume <id>` etc.).
    SpawnTui,
    /// Type the agent's exit command into pane 0.
    SendPaneText(String),
    SignalPane(i32),
    ClosePane,
    /// Read the agent's own transcript for what happened in the TUI.
    Backfill,
    SpawnHeadless {
        resume: bool,
    },
    Mode(Mode),
    /// Nothing to do: already in that mode.
    Noop,
}

pub struct Takeover {
    pub phase: Phase,
    pub turn_active: bool,
    exit_cmd: String,
}

impl Takeover {
    /// `exit_cmd` is what ends the TUI politely (`"/exit\r"` for Claude Code).
    pub fn new(mode: Mode, exit_cmd: impl Into<String>) -> Takeover {
        let phase = match mode {
            Mode::Headless => Phase::Headless,
            Mode::Tui => Phase::Tui,
        };
        Takeover {
            phase,
            turn_active: false,
            exit_cmd: exit_cmd.into(),
        }
    }

    pub fn mode(&self) -> Mode {
        match self.phase {
            Phase::Headless | Phase::StoppingAgent { .. } => Mode::Headless,
            Phase::Tui | Phase::WaitingStop | Phase::StoppingTui { .. } => Mode::Tui,
        }
    }

    pub fn in_transition(&self) -> bool {
        !matches!(self.phase, Phase::Headless | Phase::Tui)
    }

    pub fn step(&mut self, ev: Ev) -> Vec<Act> {
        use Phase::*;
        match (self.phase, ev) {
            (_, Ev::TurnStarted) => {
                self.turn_active = true;
                vec![]
            }
            (WaitingStop, Ev::TurnEnded { now }) => {
                self.turn_active = false;
                self.phase = StoppingTui {
                    since: now,
                    stage: Stage::Graceful,
                };
                vec![Act::SendPaneText(self.exit_cmd.clone())]
            }
            (_, Ev::TurnEnded { .. }) => {
                self.turn_active = false;
                vec![]
            }
            (
                Headless,
                Ev::Takeover {
                    to: Mode::Headless, ..
                },
            )
            | (Tui, Ev::Takeover { to: Mode::Tui, .. }) => vec![Act::Noop],
            (Headless, Ev::Takeover { to: Mode::Tui, now }) => {
                self.phase = StoppingAgent {
                    since: now,
                    stage: Stage::Graceful,
                };
                vec![Act::AgentCloseStdin]
            }
            (StoppingAgent { .. }, Ev::AgentExited) => {
                self.phase = Tui;
                vec![Act::SpawnTui, Act::Mode(Mode::Tui)]
            }
            (StoppingAgent { since, stage }, Ev::Tick { now }) => match stage {
                Stage::Graceful if now >= since + 2000 => {
                    self.phase = StoppingAgent {
                        since,
                        stage: Stage::Term,
                    };
                    vec![Act::SignalAgent(SIGTERM)]
                }
                Stage::Term if now >= since + 4000 => {
                    self.phase = StoppingAgent {
                        since,
                        stage: Stage::Kill,
                    };
                    vec![Act::SignalAgent(SIGKILL)]
                }
                _ => vec![],
            },
            (
                Tui,
                Ev::Takeover {
                    to: Mode::Headless,
                    now,
                },
            ) => {
                if self.turn_active {
                    self.phase = WaitingStop;
                    vec![]
                } else {
                    self.phase = StoppingTui {
                        since: now,
                        stage: Stage::Graceful,
                    };
                    vec![Act::SendPaneText(self.exit_cmd.clone())]
                }
            }
            (StoppingTui { since, stage }, Ev::Tick { now }) => match stage {
                Stage::Graceful if now >= since + 5000 => {
                    self.phase = StoppingTui {
                        since,
                        stage: Stage::Term,
                    };
                    vec![Act::SignalPane(SIGTERM)]
                }
                Stage::Term if now >= since + 7000 => {
                    self.phase = StoppingTui {
                        since,
                        stage: Stage::Kill,
                    };
                    vec![Act::SignalPane(SIGKILL)]
                }
                _ => vec![],
            },
            (Tui | WaitingStop | StoppingTui { .. }, Ev::PaneExited) => {
                self.phase = Headless;
                self.turn_active = false;
                vec![
                    Act::ClosePane,
                    Act::Backfill,
                    Act::SpawnHeadless { resume: true },
                    Act::Mode(Mode::Headless),
                ]
            }
            (Headless, Ev::AgentExited) => vec![], // the actor's restart-once policy handles it
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_to_tui_graceful() {
        let mut t = Takeover::new(Mode::Headless, "/exit\r");
        assert_eq!(
            t.step(Ev::Takeover {
                to: Mode::Tui,
                now: 0
            }),
            vec![Act::AgentCloseStdin]
        );
        assert!(t.in_transition());
        assert_eq!(t.step(Ev::Tick { now: 1000 }), vec![]);
        assert_eq!(
            t.step(Ev::AgentExited),
            vec![Act::SpawnTui, Act::Mode(Mode::Tui)]
        );
        assert_eq!(t.phase, Phase::Tui);
    }

    #[test]
    fn headless_to_tui_escalates() {
        let mut t = Takeover::new(Mode::Headless, "/exit\r");
        t.step(Ev::Takeover {
            to: Mode::Tui,
            now: 0,
        });
        assert_eq!(
            t.step(Ev::Tick { now: 2000 }),
            vec![Act::SignalAgent(SIGTERM)]
        );
        assert_eq!(t.step(Ev::Tick { now: 3000 }), vec![]);
        assert_eq!(
            t.step(Ev::Tick { now: 4000 }),
            vec![Act::SignalAgent(SIGKILL)]
        );
        assert_eq!(
            t.step(Ev::AgentExited),
            vec![Act::SpawnTui, Act::Mode(Mode::Tui)]
        );
    }

    #[test]
    fn tui_back_to_headless_waits_for_turn() {
        let mut t = Takeover::new(Mode::Tui, "/exit\r");
        t.step(Ev::TurnStarted);
        assert_eq!(
            t.step(Ev::Takeover {
                to: Mode::Headless,
                now: 10
            }),
            vec![]
        );
        assert_eq!(t.phase, Phase::WaitingStop);
        assert_eq!(
            t.step(Ev::TurnEnded { now: 50 }),
            vec![Act::SendPaneText("/exit\r".into())]
        );
        assert_eq!(
            t.step(Ev::Tick { now: 5050 }),
            vec![Act::SignalPane(SIGTERM)]
        );
        assert_eq!(
            t.step(Ev::PaneExited),
            vec![
                Act::ClosePane,
                Act::Backfill,
                Act::SpawnHeadless { resume: true },
                Act::Mode(Mode::Headless)
            ]
        );
        assert_eq!(t.phase, Phase::Headless);
    }

    #[test]
    fn tui_exit_by_user_resumes_headless() {
        let mut t = Takeover::new(Mode::Tui, "/exit\r");
        assert!(t
            .step(Ev::PaneExited)
            .contains(&Act::SpawnHeadless { resume: true }));
    }

    #[test]
    fn tui_stop_escalates_and_same_mode_is_noop() {
        let mut t = Takeover::new(Mode::Tui, "/exit\r");
        assert_eq!(
            t.step(Ev::Takeover {
                to: Mode::Tui,
                now: 0
            }),
            vec![Act::Noop]
        );
        assert_eq!(
            t.step(Ev::Takeover {
                to: Mode::Headless,
                now: 100
            }),
            vec![Act::SendPaneText("/exit\r".into())]
        );
        assert_eq!(
            t.step(Ev::Tick { now: 5100 }),
            vec![Act::SignalPane(SIGTERM)]
        );
        assert_eq!(
            t.step(Ev::Tick { now: 7100 }),
            vec![Act::SignalPane(SIGKILL)]
        );
    }
}
