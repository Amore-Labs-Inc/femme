//! Prompt history: Up in the composer brings back what the user sent before in the session, as a
//! shell or Claude Code's terminal does. The list is the session's `UserMessage` items as the
//! daemon replicates them (so it survives restarts and holds what was sent from elsewhere), plus
//! what this composer sent and the daemon has not echoed yet. Plain data, tested without gpui.

use std::collections::BTreeMap;

use femme_proto::state::{Item, ItemKind};

use crate::transcript::{attach, body, outbox};

/// Claude Code's compaction hand-off, which it sends as a user message: not something the user typed.
const HANDOFF: &str = "This session is being continued from a previous conversation";

/// One session's sent prompts.
#[derive(Debug, Default)]
pub struct History {
    /// The echoed `UserMessage` items by `seq`, each after the prompts sent from here before it that
    /// never came back as one (a slash command the agent keeps to itself).
    echoed: BTreeMap<u64, (Vec<String>, String)>,
    /// Sent from here and not echoed yet, oldest first.
    local: Vec<String>,
}

impl History {
    /// A row of the session's scope; only a main-thread `UserMessage` is taken.
    pub fn item(&mut self, item: &Item) {
        if item.kind != ItemKind::UserMessage || item.parent.is_some() {
            return;
        }
        let (text, whole) = match item.body.as_deref().and_then(body::text) {
            Some(t) => (t, true),
            None => (item.preview.clone(), false),
        };
        self.echo(item.seq, &attach::split(&text).0, whole);
    }

    /// The daemon stored `text` as message `seq`; `whole` is false when it is only the preview of a
    /// long one (the body is a blob), which is kept only when this composer sent the full text.
    pub fn echo(&mut self, seq: u64, text: &str, whole: bool) {
        if text.trim_start().starts_with(HANDOFF) {
            return;
        }
        if self.echoed.contains_key(&seq) {
            // A later rev of a message already taken: its sent copy is gone already.
            if let (true, Some(e)) = (whole, self.echoed.get_mut(&seq)) {
                e.1 = text.to_owned();
            }
            return;
        }
        let mine = self
            .local
            .iter()
            .position(|s| outbox::matches(text, whole, s));
        let (before, sent) = match mine {
            Some(i) => {
                let mut before: Vec<String> = self.local.drain(..=i).collect();
                let sent = before.pop();
                (before, sent)
            }
            None => (Vec::new(), None),
        };
        let text = match (sent, whole) {
            (_, true) => text.to_owned(),
            (Some(sent), false) => sent,
            // ponytail: a blob-stored message (over 4 KiB) sent elsewhere is skipped, not fetched
            (None, false) => return,
        };
        self.echoed.insert(seq, (before, text));
    }

    /// This composer sent `text`.
    pub fn sent(&mut self, text: &str) {
        // An attachment-only prompt: nothing to recall, and its empty text would match any echo.
        if !text.trim().is_empty() {
            self.local.push(text.to_owned());
        }
    }

    /// The scope was reset: its rows come again.
    pub fn reset(&mut self) {
        self.echoed.clear();
    }

    /// Every prompt, oldest first: no empty ones, none twice in a row.
    pub fn list(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let echoed = self
            .echoed
            .values()
            .flat_map(|(before, text)| before.iter().chain([text]));
        for text in echoed.chain(&self.local) {
            let text = text.trim_end();
            if text.trim().is_empty() || out.last().is_some_and(|l| l == text) {
                continue;
            }
            out.push(text.to_owned());
        }
        out
    }
}

/// Where Up and Down have got to in a list of prompts.
#[derive(Debug, Default)]
pub struct Browse {
    /// How many back from the newest the input shows (0: the newest); `None`: not browsing.
    at: Option<usize>,
    /// What the input held when browsing began, given back past the newest.
    draft: String,
}

impl Browse {
    /// Whether a recalled prompt is in the input.
    pub fn active(&self) -> bool {
        self.at.is_some()
    }

    /// Up with `current` in the input: the older prompt to show, if there is one.
    // ponytail: an edit made to a recalled prompt is dropped when browsing moves on, as in a shell
    // that keeps no per-entry edits; the draft from before browsing is what is kept.
    pub fn up(&mut self, list: &[String], current: &str) -> Option<String> {
        let next = self.at.map_or(0, |i| i + 1);
        let text = list.len().checked_sub(next + 1).map(|ix| list[ix].clone())?;
        if self.at.is_none() {
            self.draft = current.to_owned();
        }
        self.at = Some(next);
        Some(text)
    }

    /// Down: the newer prompt to show, or past the newest the draft from before; `None` when not
    /// browsing.
    pub fn down(&mut self, list: &[String]) -> Option<String> {
        match self.at? {
            0 => {
                self.at = None;
                Some(std::mem::take(&mut self.draft))
            }
            i => {
                let i = (i - 1).min(list.len().saturating_sub(1));
                self.at = Some(i);
                list.len().checked_sub(i + 1).map(|ix| list[ix].clone()).or_else(|| {
                    self.at = None;
                    Some(std::mem::take(&mut self.draft))
                })
            }
        }
    }

    /// Browsing is over (Esc, a send, another session): the input keeps what it shows.
    pub fn end(&mut self) {
        self.at = None;
        self.draft.clear();
    }
}

/// The caret at byte `cursor` of `text`: whether it is on the first line and on the last line.
pub fn edges(text: &str, cursor: usize) -> (bool, bool) {
    let cursor = cursor.min(text.len());
    let (before, after) = (text.get(..cursor), text.get(cursor..));
    (
        before.is_none_or(|b| !b.contains('\n')),
        after.is_none_or(|a| !a.contains('\n')),
    )
}

/// The caret at byte `cursor` of `text` as a logical line and the byte column in it.
pub fn line_col(text: &str, cursor: usize) -> (usize, usize) {
    let before = text.get(..cursor.min(text.len())).unwrap_or(text);
    let line = before.matches('\n').count();
    let col = before.len() - before.rfind('\n').map_or(0, |i| i + 1);
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn up_walks_back_and_down_comes_home_to_the_draft() {
        let l = list(&["one", "two", "three"]);
        let mut b = Browse::default();
        assert!(!b.active());
        assert_eq!(b.up(&l, "half typed").as_deref(), Some("three"));
        assert!(b.active());
        assert_eq!(b.up(&l, "three").as_deref(), Some("two"));
        assert_eq!(b.up(&l, "two").as_deref(), Some("one"));
        assert_eq!(b.up(&l, "one"), None, "nothing older: stays");
        assert_eq!(b.down(&l).as_deref(), Some("two"));
        assert_eq!(b.down(&l).as_deref(), Some("three"));
        assert_eq!(b.down(&l).as_deref(), Some("half typed"));
        assert!(!b.active());
        assert_eq!(b.down(&l), None, "not browsing: Down is the input's");
    }

    #[test]
    fn an_empty_history_takes_nothing() {
        let mut b = Browse::default();
        assert_eq!(b.up(&[], "x"), None);
        assert!(!b.active());
    }

    #[test]
    fn end_keeps_what_is_shown_and_forgets_the_draft() {
        let l = list(&["a", "b"]);
        let mut b = Browse::default();
        b.up(&l, "draft");
        b.end();
        assert!(!b.active());
        assert_eq!(b.up(&l, "b").as_deref(), Some("b"));
        assert_eq!(b.down(&l).as_deref(), Some("b"), "the new draft is what was shown");
    }

    #[test]
    fn a_list_that_shrank_while_browsing_falls_back_to_the_draft() {
        let mut b = Browse::default();
        b.up(&list(&["a", "b", "c"]), "d");
        b.up(&list(&["a", "b", "c"]), "c");
        b.up(&list(&["a", "b", "c"]), "b");
        assert_eq!(b.down(&list(&["a"])).as_deref(), Some("a"));
        assert_eq!(b.down(&list(&["a"])).as_deref(), Some("d"));
    }

    #[test]
    fn the_list_skips_empty_and_repeated_prompts() {
        let mut h = History::default();
        h.echo(1, "ls", true);
        h.echo(2, "ls", true);
        h.echo(3, "   ", true);
        h.echo(4, "pwd\n", true);
        h.echo(5, "ls", true);
        assert_eq!(h.list(), ["ls", "pwd", "ls"]);
    }

    #[test]
    fn echoes_are_in_seq_order_and_a_rev_replaces_its_text() {
        let mut h = History::default();
        h.echo(5, "later", true);
        h.echo(2, "earlier", true);
        h.echo(5, "later, edited", true);
        assert_eq!(h.list(), ["earlier", "later, edited"]);
    }

    #[test]
    fn a_sent_prompt_counts_before_its_echo_and_once_after() {
        let mut h = History::default();
        h.echo(1, "first", true);
        h.sent("second");
        h.sent("third");
        assert_eq!(h.list(), ["first", "second", "third"]);
        h.echo(2, "second", true);
        assert_eq!(h.list(), ["first", "second", "third"]);
        h.echo(3, "third", true);
        assert_eq!(h.list(), ["first", "second", "third"]);
        assert!(h.local.is_empty());
    }

    #[test]
    fn a_sent_prompt_never_echoed_stays_where_it_was_sent() {
        let mut h = History::default();
        h.sent("/model");
        h.sent("hello");
        h.sent("");
        h.echo(1, "hello", true);
        h.echo(2, "later, from the phone", true);
        assert_eq!(h.list(), ["/model", "hello", "later, from the phone"]);
    }

    #[test]
    fn a_cut_preview_takes_the_full_text_sent_from_here_or_is_skipped() {
        let mut h = History::default();
        let long = format!("{}tail", "x".repeat(600));
        h.sent(&long);
        h.echo(1, &format!("{}…", &long[..512]), false);
        h.echo(2, "someone else's long…", false);
        assert_eq!(h.list(), [long]);
    }

    #[test]
    fn handoffs_and_subagent_prompts_are_not_history() {
        let mut h = History::default();
        h.echo(1, "This session is being continued from a previous conversation...", true);
        let item = Item {
            sid: femme_proto::Id(1),
            item_id: "u".into(),
            seq: 2,
            rev: 1,
            kind: ItemKind::UserMessage,
            status: femme_proto::state::ItemStatus::Completed,
            preview: "do it".into(),
            body: Some(r#"{"text":"do it\nAttached: /tmp/a.png"}"#.into()),
            blob_hash: None,
            parent: None,
            ts: 0,
        };
        h.item(&Item {
            parent: Some("agent".into()),
            seq: 3,
            ..item.clone()
        });
        h.item(&item);
        assert_eq!(h.list(), ["do it"]);
        h.reset();
        assert!(h.list().is_empty());
    }

    #[test]
    fn caret_edges_and_lines() {
        assert_eq!(edges("", 0), (true, true));
        assert_eq!(edges("one line", 3), (true, true));
        assert_eq!(edges("a\nb\nc", 0), (true, false));
        assert_eq!(edges("a\nb\nc", 2), (false, false));
        assert_eq!(edges("a\nb\nc", 5), (false, true));
        assert_eq!(line_col("a\nbc", 4), (1, 2));
        assert_eq!(line_col("abc", 1), (0, 1));
        assert_eq!(line_col("a\n", 2), (1, 0));
    }
}
