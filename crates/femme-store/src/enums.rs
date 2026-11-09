//! Enum columns are lowercase text. One table per proto enum drives `to_str`, `from_str` and the
//! list of every variant, so the three cannot drift; the `match` in `to_str` is exhaustive, so
//! a variant added to `femme-proto` does not compile here until it has a spelling.

use femme_proto::state::{
    Agent, Attention, Entity, IssueStatus, ItemKind, ItemStatus, Mode, PaneKind, SessionStatus,
};

macro_rules! text_enum {
    ($to:ident, $from:ident, $all:ident, $ty:ty { $($v:path => $s:literal),+ $(,)? }) => {
        pub(crate) fn $to(v: $ty) -> &'static str {
            match v { $($v => $s),+ }
        }
        pub(crate) fn $from(s: &str) -> Option<$ty> {
            match s { $($s => Some($v),)+ _ => None }
        }
        #[allow(dead_code)]
        pub(crate) const $all: &[$ty] = &[$($v),+];
    };
}

text_enum!(agent_str, agent_parse, AGENTS, Agent {
    Agent::Shell => "shell",
    Agent::Claude => "claude",
    Agent::Codex => "codex",
    Agent::Pi => "pi",
});

text_enum!(mode_str, mode_parse, MODES, Mode {
    Mode::Headless => "headless",
    Mode::Tui => "tui",
});

text_enum!(session_status_str, session_status_parse, SESSION_STATUSES, SessionStatus {
    SessionStatus::Starting => "starting",
    SessionStatus::Idle => "idle",
    SessionStatus::Working => "working",
    SessionStatus::Blocked => "blocked",
    SessionStatus::Paused => "paused",
    SessionStatus::Interrupted => "interrupted",
    SessionStatus::Crashed => "crashed",
    SessionStatus::Failed => "failed",
    SessionStatus::Closed => "closed",
});

text_enum!(attention_str, attention_parse, ATTENTIONS, Attention {
    Attention::None => "none",
    Attention::Blocked => "blocked",
    Attention::Done => "done",
});

text_enum!(pane_kind_str, pane_kind_parse, PANE_KINDS, PaneKind {
    PaneKind::Agent => "agent",
    PaneKind::Shell => "shell",
});

text_enum!(item_kind_str, item_kind_parse, ITEM_KINDS, ItemKind {
    ItemKind::UserMessage => "user_message",
    ItemKind::PeerMessage => "peer_message",
    ItemKind::AssistantMessage => "assistant_message",
    ItemKind::Thinking => "thinking",
    ItemKind::ToolCall => "tool_call",
    ItemKind::ToolResult => "tool_result",
    ItemKind::Question => "question",
    ItemKind::Plan => "plan",
    ItemKind::Event => "event",
    ItemKind::TurnStart => "turn_start",
    ItemKind::TurnEnd => "turn_end",
});

text_enum!(item_status_str, item_status_parse, ITEM_STATUSES, ItemStatus {
    ItemStatus::InProgress => "in_progress",
    ItemStatus::Completed => "completed",
    ItemStatus::Failed => "failed",
    ItemStatus::Pending => "pending",
    ItemStatus::Answered => "answered",
});

// The issue spellings are the board's own `STATUSES` (board/src/main.rs), so imported and
// exported JSON need no translation.
text_enum!(issue_status_str, issue_status_parse, ISSUE_STATUSES, IssueStatus {
    IssueStatus::Open => "open",
    IssueStatus::Working => "working",
    IssueStatus::Waiting => "waiting",
    IssueStatus::Paused => "paused",
    IssueStatus::Pr => "pr",
    IssueStatus::Done => "done",
    IssueStatus::Failed => "failed",
});

text_enum!(entity_str, entity_parse, ENTITIES, Entity {
    Entity::Session => "session",
    Entity::Pane => "pane",
    Entity::Item => "item",
    Entity::Issue => "issue",
});

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::fmt::Debug;

    /// Every variant maps to a distinct lowercase string that parses back to the same variant,
    /// and nothing else parses.
    fn check<T: Copy + PartialEq + Debug>(
        all: &[T],
        to: fn(T) -> &'static str,
        from: fn(&str) -> Option<T>,
    ) {
        let mut seen = HashSet::new();
        for &v in all {
            let s = to(v);
            assert!(
                !s.is_empty() && s == s.to_lowercase(),
                "{v:?} -> {s:?} must be lowercase text"
            );
            assert!(seen.insert(s), "{s:?} is used twice");
            assert_eq!(from(s), Some(v), "{s:?} must parse back to {v:?}");
        }
        assert_eq!(from(""), None);
        assert_eq!(from("nope"), None);
        let upper = to(all[0]).to_uppercase();
        assert_eq!(from(&upper), None, "parsing is exact, not case-insensitive");
    }

    #[test]
    fn every_enum_round_trips() {
        check(AGENTS, agent_str, agent_parse);
        check(MODES, mode_str, mode_parse);
        check(SESSION_STATUSES, session_status_str, session_status_parse);
        check(ATTENTIONS, attention_str, attention_parse);
        check(PANE_KINDS, pane_kind_str, pane_kind_parse);
        check(ITEM_KINDS, item_kind_str, item_kind_parse);
        check(ITEM_STATUSES, item_status_str, item_status_parse);
        check(ISSUE_STATUSES, issue_status_str, issue_status_parse);
        check(ENTITIES, entity_str, entity_parse);
    }

    #[test]
    fn variant_counts_match_the_proto() {
        // A new proto variant breaks the exhaustive `match` above at compile time; these counts
        // additionally catch a table that lists a variant twice or forgets one the match covers.
        assert_eq!(AGENTS.len(), 4);
        assert_eq!(MODES.len(), 2);
        assert_eq!(SESSION_STATUSES.len(), 9);
        assert_eq!(ATTENTIONS.len(), 3);
        assert_eq!(PANE_KINDS.len(), 2);
        assert_eq!(ITEM_KINDS.len(), 11);
        assert_eq!(ITEM_STATUSES.len(), 5);
        assert_eq!(ISSUE_STATUSES.len(), 7);
        assert_eq!(ENTITIES.len(), 4);
    }

    #[test]
    fn issue_statuses_are_the_boards_spellings() {
        let board = [
            "open", "working", "waiting", "paused", "pr", "done", "failed",
        ];
        let ours: Vec<&str> = ISSUE_STATUSES
            .iter()
            .map(|&s| issue_status_str(s))
            .collect();
        assert_eq!(
            ours, board,
            "same strings, same order as STATUSES in board/src/main.rs"
        );
    }
}
