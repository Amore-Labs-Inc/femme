//! Which rows the list shows for the items held: tool results folded into their calls, and the
//! work of a finished turn (thinking, tool calls) collapsed into one thin line that opens on a
//! click. Pure: the view feeds it the items and the set of things the user opened.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

use femme_proto::state::{ItemKind, ItemStatus};

use super::body::{Parsed, Stop};
use super::items::{Entry, Items};

/// What a list row stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// The header above the oldest item held while older ones may exist.
    More,
    /// One item, by its `seq`.
    Item(u64),
    /// A tool call by its `seq`, its result (the item `<id>:result`) shown in the same row.
    Tool(u64),
    /// The work of a finished turn, by the `seq` of its `TurnEnd`.
    Fold(u64),
}

/// One row of the list: what it shows (`key`) and which state of it (`ver`): rows whose `ver`
/// changed have to be measured again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// What the row stands for.
    pub key: Key,
    /// Changes when anything the row shows changes.
    pub ver: u64,
}

/// What a fold line says about the work it hides.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FoldInfo {
    /// Tool calls.
    pub tools: usize,
    /// Of those, the ones that failed.
    pub failed: usize,
    /// Thinking blocks.
    pub thoughts: usize,
    /// How long the turn ran.
    pub duration_ms: Option<u64>,
}

impl FoldInfo {
    /// The line: "worked for 3m · 12 tools", "thought for 5s".
    pub fn label(&self) -> String {
        let how_long = self.duration_ms.map(duration);
        let mut out = match (self.tools, how_long) {
            (0, Some(d)) => format!("thought for {d}"),
            (0, None) => "thought".to_owned(),
            (_, Some(d)) => format!("worked for {d}"),
            (_, None) => "worked".to_owned(),
        };
        if self.tools > 0 {
            out.push_str(&format!(
                " · {} tool{}",
                self.tools,
                if self.tools == 1 { "" } else { "s" }
            ));
        }
        if self.failed > 0 {
            out.push_str(&format!(" · {} failed", self.failed));
        }
        out
    }
}

/// A span of time in the largest two units: `<1s`, `42s`, `3m`, `3m 12s`, `1h 5m`.
pub fn duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0 => "<1s".to_owned(),
        1..=59 => format!("{s}s"),
        60..=3599 => match s % 60 {
            0 => format!("{}m", s / 60),
            r => format!("{}m {r}s", s / 60),
        },
        _ => match s / 60 % 60 {
            0 => format!("{}h", s / 3600),
            m => format!("{}h {m}m", s / 3600),
        },
    }
}

/// The rows of the list, and what the fold lines among them say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    /// The rows, top to bottom.
    pub rows: Vec<Row>,
    /// The fold lines' details, by the `seq` of their `TurnEnd`.
    pub folds: HashMap<u64, FoldInfo>,
}

fn is_work(kind: ItemKind) -> bool {
    matches!(
        kind,
        ItemKind::Thinking | ItemKind::ToolCall | ItemKind::ToolResult
    )
}

fn mix(parts: &[u64]) -> u64 {
    let mut h = DefaultHasher::new();
    parts.hash(&mut h);
    h.finish()
}

/// The result item of a tool call: its id is `<call id>:result`.
pub fn result_of<'a>(items: &'a Items, call: &Entry) -> Option<&'a Entry> {
    items
        .by_id(&format!("{}:result", call.item.item_id))
        .filter(|r| r.item.kind == ItemKind::ToolResult)
}

/// Whether a `TurnEnd` is worth a row of its own: a normal end is not (the fold line, or the
/// answer itself, says it).
pub fn is_notable_end(items: &Items, e: &Entry) -> bool {
    !matches!(
        items.parsed(e),
        Parsed::TurnEnd(t) if t.stop == Stop::EndTurn
    )
}

/// What a notable `TurnEnd` says.
pub fn end_text(items: &Items, e: &Entry) -> String {
    match items.parsed(e) {
        Parsed::TurnEnd(t) => match &t.stop {
            Stop::EndTurn => "turn ended".to_owned(),
            Stop::MaxTokens => "stopped: output limit reached".to_owned(),
            Stop::Interrupted => "interrupted".to_owned(),
            Stop::Error(m) => format!("turn failed: {m}"),
            Stop::Other(m) => format!("turn ended: {m}"),
        },
        _ => "turn ended".to_owned(),
    }
}

/// What a tool row says while it is folded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolLine {
    /// The tool's name.
    pub name: String,
    /// What it was asked to do: the command, the path, the pattern.
    pub hint: String,
    /// The first line of its answer; `None` while it has not answered.
    pub result: Option<String>,
    /// The call or its result failed.
    pub failed: bool,
    /// It has not answered yet and the daemon still says the call is running.
    pub running: bool,
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// The folded line of a tool call, with its result when it has one. A call whose body is not at
/// hand (still arriving, or a blob) is read from its preview, which is `<name> <hint>`.
pub fn tool_line(items: &Items, call: &Entry) -> ToolLine {
    let (name, hint) = match items.parsed(call) {
        Parsed::ToolCall(t) => (t.name.clone(), first_line(&t.hint())),
        _ => {
            let preview = call.item.preview.trim();
            match preview.split_once(char::is_whitespace) {
                Some((name, hint)) => (name.to_owned(), first_line(hint)),
                None => (preview.to_owned(), String::new()),
            }
        }
    };
    let result = result_of(items, call);
    let (answer, answer_failed) = match result {
        None => (None, false),
        Some(r) => match items.parsed(r) {
            Parsed::ToolResult(t) => (Some(first_line(&t.output)), t.is_error),
            _ => (Some(first_line(&r.item.preview)), false),
        },
    };
    let failed = answer_failed
        || call.item.status == ItemStatus::Failed
        || result.is_some_and(|r| r.item.status == ItemStatus::Failed);
    ToolLine {
        name,
        hint,
        running: result.is_none() && call.item.status == ItemStatus::InProgress,
        result: answer,
        failed,
    }
}

/// The rows for `items`. `open` holds the `seq`s the user opened: a thinking row, a tool row, a
/// fold line.
pub fn layout(items: &Items, open: &HashSet<u64>) -> Layout {
    let entries: Vec<&Entry> = items.iter().collect();
    let n = entries.len();

    // A result whose call is held is part of the call's row.
    let mut merged = vec![false; n];
    let index: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.item.item_id.as_str(), i))
        .collect();
    for e in &entries {
        if e.item.kind == ItemKind::ToolCall {
            if let Some(&r) = index.get(format!("{}:result", e.item.item_id).as_str()) {
                if entries[r].item.kind == ItemKind::ToolResult {
                    merged[r] = true;
                }
            }
        }
    }

    // A turn ends at its `TurnEnd` and starts after the previous end or at a `TurnStart`. Its
    // work is folded into a line standing where the first of it was.
    struct Fold {
        end: usize,
        work: Vec<usize>,
    }
    let mut folds_at: HashMap<usize, Fold> = HashMap::new();
    let mut hidden = vec![false; n];
    let mut folds: HashMap<u64, FoldInfo> = HashMap::new();
    let mut start = 0;
    for (j, end) in entries.iter().enumerate() {
        match end.item.kind {
            ItemKind::TurnStart => start = j + 1,
            ItemKind::TurnEnd => {
                let work: Vec<usize> = (start..j)
                    .filter(|&i| is_work(entries[i].item.kind) && !merged[i])
                    .collect();
                if let Some(&first) = work.first() {
                    let mut info = FoldInfo::default();
                    for &i in &work {
                        let e = entries[i];
                        match e.item.kind {
                            ItemKind::Thinking => info.thoughts += 1,
                            ItemKind::ToolCall => {
                                info.tools += 1;
                                let result_failed = result_of(items, e).is_some_and(|r| {
                                    matches!(items.parsed(r), Parsed::ToolResult(t) if t.is_error)
                                        || r.item.status == ItemStatus::Failed
                                });
                                info.failed += usize::from(
                                    result_failed || e.item.status == ItemStatus::Failed,
                                );
                            }
                            // A result whose call is older than what is held.
                            _ => {
                                info.tools += 1;
                                info.failed += usize::from(
                                    matches!(items.parsed(e), Parsed::ToolResult(t) if t.is_error),
                                );
                            }
                        }
                        hidden[i] = true;
                    }
                    info.duration_ms = match items.parsed(end) {
                        Parsed::TurnEnd(t) => t.duration_ms,
                        _ => None,
                    }
                    .or_else(|| {
                        u64::try_from(end.item.ts.saturating_sub(entries[first].item.ts)).ok()
                    });
                    folds.insert(end.item.seq, info);
                    folds_at.insert(first, Fold { end: j, work });
                }
                start = j + 1;
            }
            _ => {}
        }
    }

    let item_row = |i: usize| -> Option<Row> {
        let e = entries[i];
        let seq = e.item.seq;
        let opened = u64::from(open.contains(&seq));
        match e.item.kind {
            ItemKind::TurnStart => None,
            ItemKind::TurnEnd if !is_notable_end(items, e) => None,
            ItemKind::ToolCall => Some(Row {
                key: Key::Tool(seq),
                ver: mix(&[e.ver, result_of(items, e).map_or(0, |r| r.ver), opened]),
            }),
            _ => Some(Row {
                key: Key::Item(seq),
                ver: mix(&[e.ver, opened]),
            }),
        }
    };

    let mut rows = Vec::with_capacity(n + 1);
    if items.older().is_some() {
        rows.push(Row {
            key: Key::More,
            ver: 0,
        });
    }
    for i in 0..n {
        if let Some(fold) = folds_at.get(&i) {
            let seq = entries[fold.end].item.seq;
            let opened = open.contains(&seq);
            let info = &folds[&seq];
            rows.push(Row {
                key: Key::Fold(seq),
                ver: mix(&[
                    info.tools as u64,
                    info.failed as u64,
                    info.thoughts as u64,
                    info.duration_ms.unwrap_or(u64::MAX),
                    u64::from(opened),
                ]),
            });
            if opened {
                rows.extend(fold.work.iter().filter_map(|&w| item_row(w)));
            }
        } else if !hidden[i] && !merged[i] {
            rows.extend(item_row(i));
        }
    }
    Layout { rows, folds }
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::state::{Item, Row as ProtoRow};
    use femme_proto::{Id, Sid};
    use serde_json::json;

    const SID: Sid = Id(3);

    struct Build {
        items: Items,
        seq: u64,
        ts: i64,
    }

    impl Build {
        fn new() -> Self {
            Self {
                items: Items::new(Some(SID)),
                seq: 0,
                ts: 1_000_000,
            }
        }

        fn add(&mut self, kind: ItemKind, status: ItemStatus, id: &str, body: serde_json::Value) {
            self.seq += 1;
            self.ts += 1_000;
            self.items.apply_rows(&[ProtoRow::Item(Item {
                sid: SID,
                item_id: id.into(),
                seq: self.seq,
                rev: 1,
                kind,
                status,
                preview: String::new(),
                body: Some(body.to_string()),
                blob_hash: None,
                parent: None,
                ts: self.ts,
            })]);
        }

        fn user(&mut self, id: &str) {
            self.add(
                ItemKind::UserMessage,
                ItemStatus::Completed,
                id,
                json!({"text": "hi"}),
            );
        }

        fn say(&mut self, id: &str) {
            self.add(
                ItemKind::AssistantMessage,
                ItemStatus::Completed,
                id,
                json!({"text": "ok"}),
            );
        }

        fn think(&mut self, id: &str) {
            self.add(
                ItemKind::Thinking,
                ItemStatus::Completed,
                id,
                json!({"text": "hmm"}),
            );
        }

        fn call(&mut self, id: &str) {
            self.add(
                ItemKind::ToolCall,
                ItemStatus::Completed,
                id,
                json!({"name":"Bash","input":{"command":"ls"},"call_id":id}),
            );
        }

        fn result(&mut self, call: &str, is_error: bool) {
            self.add(
                ItemKind::ToolResult,
                ItemStatus::Completed,
                &format!("{call}:result"),
                json!({"call_id":call,"output":"out","is_error":is_error}),
            );
        }

        fn end(&mut self, id: &str, stop: serde_json::Value, ms: Option<u64>) {
            self.add(
                ItemKind::TurnEnd,
                ItemStatus::Completed,
                id,
                json!({"stop": stop, "usage": null, "duration_ms": ms}),
            );
        }
    }

    fn keys(l: &Layout) -> Vec<Key> {
        l.rows.iter().map(|r| r.key).collect()
    }

    fn none() -> HashSet<u64> {
        HashSet::new()
    }

    #[test]
    fn a_result_shares_the_row_of_its_call() {
        let mut b = Build::new();
        b.user("u");
        b.call("t1");
        b.result("t1", false);
        b.say("a");
        let l = layout(&b.items, &none());
        assert_eq!(
            keys(&l),
            [Key::Item(1), Key::Tool(2), Key::Item(4)],
            "no turn ended: nothing folds, the result is not a row of its own"
        );
        assert!(l.folds.is_empty());
    }

    #[test]
    fn parallel_calls_pair_by_id_not_by_position() {
        let mut b = Build::new();
        b.call("t1");
        b.call("t2");
        b.result("t2", false);
        b.result("t1", false);
        let l = layout(&b.items, &none());
        assert_eq!(keys(&l), [Key::Tool(1), Key::Tool(2)]);
    }

    #[test]
    fn a_result_without_its_call_is_a_row_of_its_own() {
        let mut b = Build::new();
        b.result("gone", false);
        b.say("a");
        assert_eq!(
            keys(&layout(&b.items, &none())),
            [Key::Item(1), Key::Item(2)]
        );
    }

    #[test]
    fn a_finished_turn_folds_its_work_into_one_line_where_the_work_began() {
        let mut b = Build::new();
        b.user("u");
        b.think("th");
        b.call("t1");
        b.result("t1", false);
        b.say("note");
        b.call("t2");
        b.result("t2", true);
        b.say("final");
        b.end("e", json!("EndTurn"), Some(192_000));
        let l = layout(&b.items, &none());
        // user, fold (standing for thinking, both tools), then the messages in order; the normal
        // end has no row.
        assert_eq!(
            keys(&l),
            [Key::Item(1), Key::Fold(9), Key::Item(5), Key::Item(8)]
        );
        let info = &l.folds[&9];
        assert_eq!(
            info,
            &FoldInfo {
                tools: 2,
                failed: 1,
                thoughts: 1,
                duration_ms: Some(192_000)
            }
        );
        assert_eq!(info.label(), "worked for 3m 12s · 2 tools · 1 failed");
    }

    #[test]
    fn opening_a_fold_lists_its_work_under_it() {
        let mut b = Build::new();
        b.user("u");
        b.think("th");
        b.call("t1");
        b.result("t1", false);
        b.say("final");
        b.end("e", json!("EndTurn"), None);
        let closed = layout(&b.items, &none());
        let open = layout(&b.items, &HashSet::from([6]));
        assert_eq!(keys(&closed), [Key::Item(1), Key::Fold(6), Key::Item(5)]);
        assert_eq!(
            keys(&open),
            [
                Key::Item(1),
                Key::Fold(6),
                Key::Item(2),
                Key::Tool(3),
                Key::Item(5)
            ]
        );
        assert_ne!(
            closed.rows[1].ver, open.rows[1].ver,
            "the fold line changed"
        );
        // No duration from the agent: from the items' own times (first work to the end).
        assert_eq!(closed.folds[&6].duration_ms, Some(4_000));
    }

    #[test]
    fn a_turn_in_progress_shows_everything() {
        let mut b = Build::new();
        b.user("u1");
        b.call("t1");
        b.result("t1", false);
        b.say("a1");
        b.end("e1", json!("EndTurn"), Some(5_000));
        b.user("u2");
        b.think("th");
        b.call("t2");
        let l = layout(&b.items, &none());
        assert_eq!(
            keys(&l),
            [
                Key::Item(1),
                Key::Fold(5),
                Key::Item(4),
                Key::Item(6),
                Key::Item(7),
                Key::Tool(8)
            ],
            "the first turn folds; the second has no end yet"
        );
        assert_eq!(l.folds.len(), 1);
    }

    #[test]
    fn turns_fold_separately() {
        let mut b = Build::new();
        for t in 0..2 {
            b.user(&format!("u{t}"));
            b.call(&format!("c{t}"));
            b.result(&format!("c{t}"), false);
            b.say(&format!("a{t}"));
            b.end(&format!("e{t}"), json!("EndTurn"), Some(1_000));
        }
        let l = layout(&b.items, &none());
        assert_eq!(
            keys(&l),
            [
                Key::Item(1),
                Key::Fold(5),
                Key::Item(4),
                Key::Item(6),
                Key::Fold(10),
                Key::Item(9)
            ]
        );
        assert_eq!(l.folds[&5].tools, 1);
        assert_eq!(l.folds[&10].tools, 1);
    }

    #[test]
    fn a_turn_without_work_has_no_fold() {
        let mut b = Build::new();
        b.user("u");
        b.say("a");
        b.end("e", json!("EndTurn"), Some(2_000));
        let l = layout(&b.items, &none());
        assert_eq!(keys(&l), [Key::Item(1), Key::Item(2)]);
        assert!(l.folds.is_empty());
    }

    #[test]
    fn a_turn_that_did_not_end_normally_says_so() {
        let mut b = Build::new();
        b.user("u");
        b.call("t1");
        b.end("e", json!("Interrupted"), Some(2_000));
        b.user("u2");
        b.end("e2", json!({"Error": "boom"}), None);
        let l = layout(&b.items, &none());
        assert_eq!(
            keys(&l),
            [
                Key::Item(1),
                Key::Fold(3),
                Key::Item(3),
                Key::Item(4),
                Key::Item(5)
            ]
        );
        let e = b.items.get(3).unwrap();
        assert_eq!(end_text(&b.items, e), "interrupted");
        assert_eq!(
            end_text(&b.items, b.items.get(5).unwrap()),
            "turn failed: boom"
        );
    }

    #[test]
    fn a_turn_start_begins_the_span_of_the_next_end() {
        let mut b = Build::new();
        // An earlier turn that never ended: its work stays on show.
        b.call("old");
        b.add(ItemKind::TurnStart, ItemStatus::Completed, "s", json!({}));
        b.call("t1");
        b.say("a");
        b.end("e", json!("EndTurn"), None);
        let l = layout(&b.items, &none());
        assert_eq!(keys(&l), [Key::Tool(1), Key::Fold(5), Key::Item(4)]);
        assert_eq!(l.folds[&5].tools, 1);
    }

    #[test]
    fn older_history_gets_a_header_row() {
        let mut b = Build::new();
        b.say("a");
        let mut items = Items::new(Some(SID));
        items.apply_rows(&[ProtoRow::Item(Item {
            sid: SID,
            item_id: "late".into(),
            seq: 50,
            rev: 1,
            kind: ItemKind::UserMessage,
            status: ItemStatus::Completed,
            preview: "x".into(),
            body: None,
            blob_hash: None,
            parent: None,
            ts: 0,
        })]);
        assert_eq!(keys(&layout(&items, &none())), [Key::More, Key::Item(50)]);
        assert_eq!(keys(&layout(&b.items, &none())), [Key::Item(1)]);
    }

    #[test]
    fn a_rows_version_moves_only_when_it_changed() {
        let mut b = Build::new();
        b.user("u");
        b.call("t1");
        b.say("a");
        let before = layout(&b.items, &none());
        assert_eq!(before, layout(&b.items, &none()), "same input, same rows");
        // The result of the call arrives: its row changed, the others did not.
        b.result("t1", false);
        let after = layout(&b.items, &none());
        assert_eq!(keys(&before), keys(&after));
        assert_eq!(before.rows[0], after.rows[0]);
        assert_ne!(before.rows[1].ver, after.rows[1].ver);
        assert_eq!(before.rows[2], after.rows[2]);
        // Opening a row changes its version, and only its.
        let opened = layout(&b.items, &HashSet::from([2]));
        assert_ne!(after.rows[1].ver, opened.rows[1].ver);
        assert_eq!(after.rows[0], opened.rows[0]);
    }

    #[test]
    fn fold_labels() {
        let info = |tools, failed, thoughts, ms| FoldInfo {
            tools,
            failed,
            thoughts,
            duration_ms: ms,
        };
        assert_eq!(
            info(12, 0, 3, Some(180_000)).label(),
            "worked for 3m · 12 tools"
        );
        assert_eq!(info(1, 0, 0, Some(8_000)).label(), "worked for 8s · 1 tool");
        assert_eq!(info(0, 0, 2, Some(5_000)).label(), "thought for 5s");
        assert_eq!(info(0, 0, 1, None).label(), "thought");
        assert_eq!(info(3, 0, 0, None).label(), "worked · 3 tools");
    }

    #[test]
    fn durations_read_in_two_units() {
        assert_eq!(duration(0), "<1s");
        assert_eq!(duration(999), "<1s");
        assert_eq!(duration(1_000), "1s");
        assert_eq!(duration(59_999), "59s");
        assert_eq!(duration(60_000), "1m");
        assert_eq!(duration(192_000), "3m 12s");
        assert_eq!(duration(3_600_000), "1h");
        assert_eq!(duration(3_900_000), "1h 5m");
    }

    #[test]
    fn a_folded_tool_line_has_the_name_the_hint_and_the_first_line_of_the_answer() {
        let mut b = Build::new();
        b.call("t1");
        b.add(
            ItemKind::ToolResult,
            ItemStatus::Completed,
            "t1:result",
            json!({"call_id":"t1","output":"\nfirst\nsecond","is_error":false}),
        );
        let call = b.items.get(1).unwrap();
        assert_eq!(
            tool_line(&b.items, call),
            ToolLine {
                name: "Bash".into(),
                hint: "ls".into(),
                result: Some("first".into()),
                failed: false,
                running: false,
            }
        );
    }

    #[test]
    fn a_call_without_answer_is_running_and_a_failed_result_marks_the_line() {
        let mut b = Build::new();
        b.add(
            ItemKind::ToolCall,
            ItemStatus::InProgress,
            "t1",
            json!({"name":"Read","input":{"file_path":"/a/b.rs"}}),
        );
        let line = tool_line(&b.items, b.items.get(1).unwrap());
        assert_eq!(
            (line.name.as_str(), line.hint.as_str()),
            ("Read", "/a/b.rs")
        );
        assert!(line.running && line.result.is_none() && !line.failed);

        b.call("t2");
        b.result("t2", true);
        let line = tool_line(&b.items, b.items.get(2).unwrap());
        assert!(line.failed && !line.running);
        assert_eq!(line.result.as_deref(), Some("out"));
    }

    #[test]
    fn a_call_whose_body_is_a_blob_is_read_from_its_preview() {
        let mut b = Build::new();
        b.items.apply_rows(&[ProtoRow::Item(Item {
            sid: SID,
            item_id: "big".into(),
            seq: 1,
            rev: 1,
            kind: ItemKind::ToolCall,
            status: ItemStatus::Completed,
            preview: "Write /tmp/x.txt".into(),
            body: None,
            blob_hash: Some([9; 32]),
            ts: 1,
            parent: None,
        })]);
        let line = tool_line(&b.items, b.items.get(1).unwrap());
        assert_eq!(
            (line.name.as_str(), line.hint.as_str()),
            ("Write", "/tmp/x.txt")
        );
    }
}
