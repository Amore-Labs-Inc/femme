//! The subagents on show: the `Agent` calls of the main thread since the last turn ended, plus
//! the ones an earlier turn left running in the background, one chip each in the strip under
//! the transcript. A chip opens the subagent's own view (its items are the ones whose `parent`
//! is the call's id).

use femme_proto::state::{ItemKind, ItemStatus};

use super::body::{Parsed, ToolCall};
use super::items::{Entry, Items};
use super::turns;

/// One subagent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chip {
    /// The `Agent` call's id: the `parent` of the subagent's items.
    pub id: String,
    /// What it was asked to do (the call's `description`), else its kind.
    pub label: String,
    /// Still at work: its call has no result yet, or it runs in the background and the agent
    /// has not said it stopped.
    pub running: bool,
    /// Unix ms of the call.
    pub since: i64,
    /// Launched in the background: its result came at once and its items follow on their own.
    background: bool,
    /// Called by a turn that has ended: it goes as soon as it stops.
    earlier: bool,
}

/// Tools that spawn a subagent (Claude Code: `Agent`, called `Task` before 2.0).
fn spawns(name: &str) -> bool {
    matches!(name, "Agent" | "Task")
}

/// What the result of a background `Agent` call says (Claude Code): the agent goes on working.
fn launched(items: &Items, result: &Entry) -> bool {
    result.item.status != ItemStatus::Failed
        && matches!(items.parsed(result), Parsed::ToolResult(r)
            if !r.is_error && r.output.starts_with("Async agent launched"))
}

/// The call an event line says stopped (`femme_agents::items::TaskEnd`).
fn task_end(items: &Items, e: &Entry) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(items.json(e)?).ok()?;
    v["task"]["call"].as_str().map(str::to_owned)
}

/// The subagents on show, oldest first: the `Agent` calls of the current turn, and the
/// background ones of earlier turns until the agent says they stopped (an `Event` naming the
/// call).
pub fn chips(items: &Items) -> Vec<Chip> {
    let mut out: Vec<Chip> = Vec::new();
    for e in items.iter() {
        if let Some(parent) = e.item.parent.as_deref() {
            // Its items keep coming: a background agent at work (or started again).
            if let Some(c) = out.iter_mut().find(|c| c.background && c.id == parent) {
                c.running = true;
            }
            continue;
        }
        match e.item.kind {
            ItemKind::TurnEnd => {
                out.retain(|c| c.running);
                out.iter_mut().for_each(|c| c.earlier = true);
            }
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
                    background: false,
                    earlier: false,
                });
            }
            ItemKind::ToolResult => {
                let Some(call) = e.item.item_id.strip_suffix(":result") else {
                    continue;
                };
                if let Some(c) = out.iter_mut().find(|c| c.id == call) {
                    c.background = launched(items, e);
                    c.running = c.background;
                }
            }
            ItemKind::Event => {
                if let Some(call) = task_end(items, e) {
                    if let Some(i) = out.iter().position(|c| c.id == call) {
                        if out[i].earlier {
                            out.remove(i);
                        } else {
                            out[i].running = false;
                        }
                    }
                }
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
        child(seq, kind, id, preview, body, None)
    }

    fn child(
        seq: u64,
        kind: ItemKind,
        id: &str,
        preview: &str,
        body: Option<&str>,
        parent: Option<&str>,
    ) -> Row {
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
            parent: parent.map(Into::into),
        })
    }

    fn call(seq: u64, id: &str, desc: &str) -> Row {
        item(
            seq,
            ItemKind::ToolCall,
            id,
            &format!("Agent {desc}"),
            Some(&format!(
                r#"{{"name":"Agent","input":{{"description":"{desc}","prompt":"…"}},"call_id":"{id}"}}"#
            )),
        )
    }

    fn result(seq: u64, id: &str, output: &str) -> Row {
        item(
            seq,
            ItemKind::ToolResult,
            &format!("{id}:result"),
            output,
            Some(&format!(
                r#"{{"call_id":"{id}","output":"{output}","is_error":false}}"#
            )),
        )
    }

    fn ended(seq: u64, call: &str) -> Row {
        item(
            seq,
            ItemKind::Event,
            &format!("n{seq}"),
            "done: completed",
            Some(&format!(
                r#"{{"text":"done: completed","task":{{"call":"{call}","status":"completed"}}}}"#
            )),
        )
    }

    fn on(c: &[Chip]) -> Vec<(&str, bool)> {
        c.iter().map(|c| (c.id.as_str(), c.running)).collect()
    }

    #[test]
    fn the_turns_agent_calls_are_chips_until_the_turn_ends() {
        use ItemKind::*;
        let mut it = Items::new(Some(Id(1)));
        it.apply_rows(&[
            call(1, "t1", "old"),
            result(2, "t1", "done"),
            item(3, TurnEnd, "e1", "", Some(r#"{"stop":"done"}"#)),
            item(4, UserMessage, "u2", "go", Some(r#"{"text":"go"}"#)),
            call(5, "t2", "bench the strip"),
            result(6, "t2", "done"),
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

    #[test]
    fn a_background_agent_stays_on_show_until_the_agent_says_it_stopped() {
        use ItemKind::*;
        let mut it = Items::new(Some(Id(1)));
        it.apply_rows(&[
            call(1, "b1", "bench in the background"),
            result(2, "b1", "Async agent launched successfully"),
            call(3, "f1", "in the foreground"),
            child(4, UserMessage, "f1u", "…", None, Some("f1")),
            result(5, "f1", "done"),
            item(6, TurnEnd, "e1", "", Some(r#"{"stop":"done"}"#)),
            // the next turn: the background agent is still at work
            item(7, UserMessage, "u2", "go", Some(r#"{"text":"go"}"#)),
            child(8, ToolCall, "b1c", "Bash ls", None, Some("b1")),
        ]);
        assert_eq!(on(&chips(&it)), vec![("b1", true)]);
        // it stops: the chip goes at once, its turn being over
        it.apply_rows(&[ended(9, "b1")]);
        assert!(chips(&it).is_empty());
        // one launched and stopped in the same turn shows as done until the turn ends
        it.apply_rows(&[
            call(10, "b2", "again"),
            result(11, "b2", "Async agent launched successfully"),
            ended(12, "b2"),
        ]);
        assert_eq!(on(&chips(&it)), vec![("b2", false)]);
        it.apply_rows(&[item(13, TurnEnd, "e2", "", Some(r#"{"stop":"done"}"#))]);
        assert!(chips(&it).is_empty());
    }
}
