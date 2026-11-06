//! The subagents of the turn on show: the `Agent` calls of the main thread since the last turn
//! ended, one chip each in the strip under the transcript. A chip opens the subagent's own view
//! (its items are the ones whose `parent` is the call's id).

use femme_proto::state::{ItemKind, ItemStatus};

use super::body::{Parsed, ToolCall};
use super::items::Items;
use super::turns;

/// One subagent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chip {
    /// The `Agent` call's id: the `parent` of the subagent's items.
    pub id: String,
    /// What it was asked to do (the call's `description`), else its kind.
    pub label: String,
    /// Its call has no result yet.
    pub running: bool,
    /// Unix ms of the call.
    pub since: i64,
}

/// Tools that spawn a subagent (Claude Code: `Agent`, called `Task` before 2.0).
fn spawns(name: &str) -> bool {
    matches!(name, "Agent" | "Task")
}

/// The subagents of the current turn, oldest first: none once the turn has ended.
pub fn chips(items: &Items) -> Vec<Chip> {
    let mut out = Vec::new();
    for e in items.view(None) {
        match e.item.kind {
            ItemKind::TurnEnd => out.clear(),
            ItemKind::ToolCall => {
                let line = turns::tool_line(items, e);
                if !spawns(&line.name) {
                    continue;
                }
                let label = match items.parsed(e) {
                    Parsed::ToolCall(ToolCall {
                        input: Some(input), ..
                    }) => input["description"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .or_else(|| input["subagent_type"].as_str())
                        .unwrap_or("agent")
                        .to_owned(),
                    // A long prompt makes the body a blob: the preview is `Agent <description>`.
                    _ => Some(line.hint.as_str())
                        .filter(|h| !h.is_empty())
                        .unwrap_or("agent")
                        .to_owned(),
                };
                out.push(Chip {
                    id: e.item.item_id.clone(),
                    label,
                    running: line.running
                        || (line.result.is_none() && e.item.status != ItemStatus::Failed),
                    since: e.item.ts,
                });
            }
            _ => {}
        }
    }
    out
}

/// `1m 12s` since `since` at `now` (both unix ms).
pub fn elapsed(since: i64, now: i64) -> String {
    let s = ((now - since) / 1000).max(0);
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m {:02}s", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::state::{Item, Row};
    use femme_proto::Id;

    fn item(seq: u64, kind: ItemKind, id: &str, preview: &str, body: Option<&str>) -> Row {
        Row::Item(Item {
            sid: Id(1),
            item_id: id.into(),
            seq,
            rev: 1,
            kind,
            status: ItemStatus::Completed,
            preview: preview.into(),
            body: body.map(Into::into),
            blob_hash: None,
            ts: seq as i64 * 1000,
            parent: None,
        })
    }

    #[test]
    fn the_turns_agent_calls_are_chips_until_the_turn_ends() {
        use ItemKind::*;
        let call = |seq, id, desc: &str| {
            item(
                seq,
                ToolCall,
                id,
                &format!("Agent {desc}"),
                Some(&format!(
                    r#"{{"name":"Agent","input":{{"description":"{desc}","prompt":"…"}},"call_id":"{id}"}}"#
                )),
            )
        };
        let result = |seq, id: &str| {
            item(
                seq,
                ToolResult,
                &format!("{id}:result"),
                "done",
                Some(&format!(
                    r#"{{"call_id":"{id}","output":"done","is_error":false}}"#
                )),
            )
        };
        let mut it = Items::new(Some(Id(1)));
        it.apply_rows(&[
            call(1, "t1", "old"),
            result(2, "t1"),
            item(3, TurnEnd, "e1", "", Some(r#"{"stop":"done"}"#)),
            item(4, UserMessage, "u2", "go", Some(r#"{"text":"go"}"#)),
            call(5, "t2", "bench the strip"),
            result(6, "t2"),
            // A big prompt: no body at hand, read from the preview.
            item(7, ToolCall, "t3", "Agent review the diff", None),
            item(
                8,
                ToolCall,
                "t4",
                "Bash ls",
                Some(r#"{"name":"Bash","input":{},"call_id":"t4"}"#),
            ),
        ]);
        let c = chips(&it);
        assert_eq!(
            c.iter()
                .map(|c| (c.id.as_str(), c.label.as_str(), c.running))
                .collect::<Vec<_>>(),
            vec![
                ("t2", "bench the strip", false),
                ("t3", "review the diff", true)
            ]
        );
        assert_eq!(c[0].since, 5000);
        assert_eq!(elapsed(5000, 77_500), "1m 12s");
        assert_eq!(elapsed(5000, 9000), "4s");
    }
}
