//! The line under the transcript while the agent works, as Claude Code draws it: a turning star,
//! what the agent is at, how long the turn has run, and about how much it has written
//! (`✻ Thinking… (5m 42s · ↓ 15.0k tokens)`). No tips.

use femme_proto::state::{ItemKind, ItemStatus};

use super::items::Items;

/// The star's frames, one per second (Claude Code's spinner).
const STARS: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✽"];

/// What the current turn is doing, from its items.
#[derive(Debug, PartialEq, Eq)]
pub struct Working {
    /// `Thinking`, `Writing` or `Running a tool`.
    pub verb: &'static str,
    /// Since the user's last message (unix ms of its row), `None` when it is not loaded.
    pub since: Option<i64>,
    /// Bytes the agent has produced this turn: its text, thoughts and tool calls.
    pub bytes: usize,
}

/// The turn so far: everything after the last user message.
pub fn working(items: &Items) -> Working {
    let all: Vec<_> = items.iter().collect();
    let start = all
        .iter()
        .rposition(|e| e.item.kind == ItemKind::UserMessage);
    let turn = &all[start.map_or(0, |i| i + 1)..];
    let bytes = turn
        .iter()
        .filter(|e| {
            matches!(
                e.item.kind,
                ItemKind::AssistantMessage | ItemKind::Thinking | ItemKind::ToolCall
            )
        })
        .map(|e| items.shown(e).text.len())
        .sum();
    // The newest item still in progress says what is happening; between items it is thinking.
    let verb = match turn
        .iter()
        .rev()
        .find(|e| e.item.status == ItemStatus::InProgress)
        .map(|e| e.item.kind)
    {
        Some(ItemKind::AssistantMessage) => "Writing",
        Some(ItemKind::ToolCall | ItemKind::ToolResult) => "Running a tool",
        _ => "Thinking",
    };
    Working {
        verb,
        since: start.map(|i| all[i].item.ts),
        bytes,
    }
}

/// `42s`, `5m 42s`, `1h 3m`.
fn elapsed(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

/// `850`, `15.0k`: about four bytes to a token.
fn tokens(bytes: usize) -> String {
    let t = bytes / 4;
    if t < 1000 {
        t.to_string()
    } else {
        format!("{:.1}k", t as f64 / 1000.0)
    }
}

/// The line at `now` (unix ms).
pub fn line(w: &Working, now: i64) -> String {
    let secs = w.since.map(|t| (now - t) / 1000);
    let star = STARS[secs.unwrap_or(0).rem_euclid(STARS.len() as i64) as usize];
    let mut facts = Vec::new();
    if let Some(s) = secs {
        facts.push(elapsed(s));
    }
    if w.bytes > 0 {
        facts.push(format!("↓ {} tokens", tokens(w.bytes)));
    }
    if facts.is_empty() {
        format!("{star} {}…", w.verb)
    } else {
        format!("{star} {}… ({})", w.verb, facts.join(" · "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::state::{Item, Row};
    use femme_proto::Id;

    fn item(seq: u64, kind: ItemKind, status: ItemStatus, text: &str, ts: i64) -> Row {
        Row::Item(Item {
            sid: Id(1),
            item_id: format!("i{seq}"),
            seq,
            rev: 1,
            kind,
            status,
            preview: text.into(),
            body: None,
            blob_hash: None,
            parent: None,
            ts,
        })
    }

    fn items(rows: Vec<Row>) -> Items {
        let mut it = Items::new(Some(Id(1)));
        it.apply_rows(&rows);
        it
    }

    #[test]
    fn the_line_names_the_work_the_time_and_the_tokens() {
        use ItemKind::*;
        use ItemStatus::*;
        let it = items(vec![
            item(1, UserMessage, Completed, "old question", 1_000),
            item(2, AssistantMessage, Completed, "old answer, not counted", 2_000),
            item(3, UserMessage, Completed, "fix the build", 10_000),
            item(4, Thinking, Completed, &"t".repeat(4000), 11_000),
            item(5, AssistantMessage, InProgress, &"w".repeat(2000), 12_000),
        ]);
        let w = working(&it);
        assert_eq!(w.verb, "Writing");
        assert_eq!(w.since, Some(10_000));
        // Five minutes and 42 s later: the star is frame 342 % 6 = 0.
        assert_eq!(line(&w, 10_000 + 342_000), "· Writing… (5m 42s · ↓ 1.5k tokens)");
    }

    #[test]
    fn between_items_it_is_thinking_and_an_empty_turn_says_only_that() {
        use ItemKind::*;
        use ItemStatus::*;
        let it = items(vec![
            item(1, UserMessage, Completed, "hi", 0),
            item(2, ToolCall, Completed, "{}", 0),
        ]);
        assert_eq!(working(&it).verb, "Thinking");
        let tool = items(vec![item(1, ToolCall, InProgress, "", 0)]);
        assert_eq!(working(&tool).verb, "Running a tool");
        let none = working(&Items::new(Some(Id(1))));
        assert_eq!(line(&none, 0), "· Thinking…");
        assert_eq!(elapsed(3_725), "1h 2m");
        assert_eq!(tokens(3_999), "999");
    }
}
