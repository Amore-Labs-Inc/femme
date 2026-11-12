//! Esc right after sending takes the prompt back, as it does in Claude Code's terminal: while
//! the session works and nothing of a reply has shown yet (no assistant text, no tool row), Esc is
//! `Req::TakeBack` and the prompt's text returns to the input; later in the turn it is
//! `Req::Interrupt`. Plain data over the session's rows, tested without gpui.

use femme_proto::rpc::Req;
use femme_proto::state::{Item, ItemKind, ItemStatus};
use femme_proto::Sid;

/// What the placeholder says while Esc would take the prompt back.
pub const HINT: &str = "esc: take the message back to edit";

/// Whether a session's newest row is still the user's own message, from the rows of its scope.
#[derive(Debug, Default)]
pub struct Fresh {
    /// The newest top-level row that is a message, a tool row or a turn's end: its `seq`, and
    /// whether it is a message of the user's that the agent has or will get.
    top: Option<(u64, bool)>,
}

impl Fresh {
    /// A row of the session's scope; whether [`Fresh::is`] changed. Notes (`Event`) are not a
    /// reply: the daemon's "Claude read your message" follows every prompt.
    pub fn item(&mut self, item: &Item) -> bool {
        if item.parent.is_some() || matches!(item.kind, ItemKind::Event | ItemKind::TurnStart) {
            return false;
        }
        if self.top.is_some_and(|(seq, _)| item.seq < seq) {
            return false;
        }
        let was = self.is();
        let mine = item.kind == ItemKind::UserMessage && item.status != ItemStatus::Failed;
        self.top = Some((item.seq, mine));
        was != self.is()
    }

    /// A row left the transcript, or the scope starts over: what is newest is not known until
    /// the next row says. Whether [`Fresh::is`] changed.
    pub fn forget(&mut self) -> bool {
        let was = self.is();
        self.top = None;
        was
    }

    /// The user's message is the newest row: no reply to it has shown.
    pub fn is(&self) -> bool {
        self.top.is_some_and(|(_, mine)| mine)
    }
}

/// What Esc sends for `sid`: nothing unless the session works; `TakeBack` for a Claude session
/// whose newest row is still the prompt (`fresh`), else `Interrupt`.
pub fn esc(sid: Sid, working: bool, claude: bool, fresh: bool) -> Option<Req> {
    match (working, claude && fresh) {
        (false, _) => None,
        (true, true) => Some(Req::TakeBack { sid }),
        (true, false) => Some(Req::Interrupt { sid }),
    }
}

/// The text that goes back into the input: `typed` (what this composer sent, as it was typed)
/// when `taken`, the daemon's copy, is that prompt (`body`: what went out, to which the daemon
/// adds an `Attached:` line per file); else the daemon's copy, sent from somewhere else. What is
/// in the input already (`current`) stays, after it.
pub fn restored(taken: &str, sent: Option<(&str, &str)>, current: &str) -> (String, bool) {
    let (text, mine) = match sent {
        Some((body, typed)) if taken.starts_with(body) => (typed, true),
        _ => (taken, false),
    };
    let text = match current.is_empty() {
        true => text.to_owned(),
        false => format!("{text}\n{current}"),
    };
    (text, mine)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: Sid = femme_proto::Id(7);

    fn item(seq: u64, kind: ItemKind, status: ItemStatus) -> Item {
        Item {
            sid: SID,
            item_id: format!("i{seq}"),
            seq,
            rev: seq,
            kind,
            status,
            preview: String::new(),
            body: None,
            blob_hash: None,
            ts: 0,
            parent: None,
        }
    }

    /// The hand-off's rule: Esc with a pending turn and no reply rows sends TakeBack; Esc after
    /// an assistant row sends Interrupt.
    #[test]
    fn esc_takes_the_prompt_back_until_a_reply_row_shows() {
        let mut f = Fresh::default();
        assert_eq!(
            esc(SID, true, true, f.is()),
            Some(Req::Interrupt { sid: SID })
        );
        assert!(f.item(&item(4, ItemKind::UserMessage, ItemStatus::Pending)));
        assert_eq!(
            esc(SID, true, true, f.is()),
            Some(Req::TakeBack { sid: SID })
        );
        // the daemon's "read" note and the message turning `Completed` are no reply
        assert!(!f.item(&item(5, ItemKind::Event, ItemStatus::Completed)));
        assert!(!f.item(&item(4, ItemKind::UserMessage, ItemStatus::Completed)));
        assert_eq!(
            esc(SID, true, true, f.is()),
            Some(Req::TakeBack { sid: SID })
        );
        assert!(f.item(&item(6, ItemKind::AssistantMessage, ItemStatus::InProgress)));
        assert_eq!(
            esc(SID, true, true, f.is()),
            Some(Req::Interrupt { sid: SID })
        );
        // an older row's later revision does not bring the prompt back to the front
        assert!(!f.item(&item(4, ItemKind::UserMessage, ItemStatus::Completed)));
        assert!(!f.is());
    }

    #[test]
    fn a_tool_row_a_turn_end_or_a_subagent_decide_as_a_reply_does() {
        for kind in [ItemKind::ToolCall, ItemKind::Thinking, ItemKind::TurnEnd] {
            let mut f = Fresh::default();
            f.item(&item(1, ItemKind::UserMessage, ItemStatus::Pending));
            assert!(f.item(&item(2, kind, ItemStatus::Completed)), "{kind:?}");
            assert!(!f.is());
        }
        // a subagent's prompt is not the user's, and its rows are not the main thread's reply
        let mut f = Fresh::default();
        f.item(&item(1, ItemKind::UserMessage, ItemStatus::Pending));
        let mut sub = item(2, ItemKind::AssistantMessage, ItemStatus::Completed);
        sub.parent = Some("toolu_1".into());
        assert!(!f.item(&sub));
        assert!(f.is());
        // a message the agent did not get is not one to take back; a removed row is not known
        assert!(f.item(&item(1, ItemKind::UserMessage, ItemStatus::Failed)));
        f.item(&item(3, ItemKind::UserMessage, ItemStatus::Pending));
        assert!(f.forget());
        assert!(!f.is());
    }

    #[test]
    fn esc_is_left_alone_when_idle_and_interrupts_other_agents() {
        assert_eq!(esc(SID, false, true, true), None);
        assert_eq!(
            esc(SID, true, false, true),
            Some(Req::Interrupt { sid: SID })
        );
    }

    #[test]
    fn the_typed_text_comes_back_when_the_prompt_is_this_composers() {
        let sent = Some(("fix it", "fix it  "));
        assert_eq!(
            restored("fix it\nAttached: /u/a.png", sent, ""),
            ("fix it  ".into(), true)
        );
        assert_eq!(
            restored("something else", sent, ""),
            ("something else".into(), false)
        );
        assert_eq!(
            restored("a", None, "typed since"),
            ("a\ntyped since".into(), false)
        );
    }
}
