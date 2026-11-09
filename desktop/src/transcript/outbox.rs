//! Prompts on their way: what the composer has sent and whose stored message has not come back
//! yet. The daemon stores a prompt before the agent gets it, with the request's op id in its body,
//! so a prompt is on its way only for the network round trip; the transcript draws it as a faded
//! "you" bubble at its end meanwhile. A daemon from before that stored nothing until the agent's
//! echo: then the echo with the prompt's text takes it off. Plain data, tested without gpui.

use std::collections::HashMap;

use femme_proto::state::{Item, ItemKind, Row};
use femme_proto::{OpId, Sid};

use super::{attach, body};

/// How far the daemon's clock may lag ours and a text match still count.
const SKEW_MS: i64 = 10_000;

/// One prompt on its way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    /// The op id it was sent under: its stored message carries it.
    pub op: OpId,
    /// The text as sent.
    pub text: String,
    /// The names of the files attached to it.
    pub files: Vec<String>,
    /// When it was sent, in ms since the epoch: an old daemon's echo `ts` is compared with it.
    at_ms: i64,
}

/// The prompts on their way, per session, oldest first.
#[derive(Debug, Default)]
pub struct Outbox {
    /// Changes whenever a prompt comes or goes, so a view can tell when to redraw.
    pub ver: u64,
    by_sid: HashMap<Sid, Vec<Sent>>,
}

impl Outbox {
    /// A prompt for `sid` was sent under `op` at `at_ms` since the epoch.
    pub fn push(&mut self, sid: Sid, op: OpId, text: String, files: Vec<String>, at_ms: i64) {
        self.ver += 1;
        self.by_sid.entry(sid).or_default().push(Sent {
            op,
            text,
            files,
            at_ms,
        });
    }

    /// Drops prompt `op` of `sid` (the daemon refused it); whether it was held.
    pub fn forget(&mut self, sid: Sid, op: OpId) -> bool {
        let Some(list) = self.by_sid.get_mut(&sid) else {
            return false;
        };
        let n = list.len();
        list.retain(|s| s.op != op);
        let gone = list.len() != n;
        self.ver += u64::from(gone);
        gone
    }

    /// Rows of `sid`'s scope: a `UserMessage` whose body names an op takes off the prompt sent
    /// under it. One that names none (an old daemon's echo) takes off the oldest prompt whose text
    /// it starts with (the daemon appends `Attached:` lines), else the oldest one sent before it.
    /// Whether any went. A `PeerMessage` (another session's) takes nothing off.
    pub fn echo(&mut self, sid: Sid, rows: &[Row]) -> bool {
        let Some(list) = self.by_sid.get_mut(&sid) else {
            return false;
        };
        let n = list.len();
        for row in rows {
            let Row::Item(item) = row else { continue };
            if item.sid != sid || item.kind != ItemKind::UserMessage || list.is_empty() {
                continue;
            }
            let hit = match op_of(item) {
                Some(op) => list.iter().position(|s| s.op.to_string() == op),
                None => {
                    let (real, whole) = text_of(item);
                    list.iter()
                        .position(|s| {
                            item.ts >= s.at_ms - SKEW_MS && matches(&real, whole, &s.text)
                        })
                        .or_else(|| list.iter().position(|s| item.ts >= s.at_ms))
                }
            };
            if let Some(i) = hit {
                list.remove(i);
            }
        }
        let gone = list.len() != n;
        self.ver += u64::from(gone);
        gone
    }

    /// The prompts of `sid` on their way, oldest first.
    pub fn shown(&self, sid: Sid) -> impl Iterator<Item = &Sent> {
        self.by_sid.get(&sid).into_iter().flatten()
    }
}

/// The op id a stored message names, when the daemon stored it before the agent had it.
fn op_of(item: &Item) -> Option<String> {
    let b: serde_json::Value = serde_json::from_str(item.body.as_deref()?).ok()?;
    b.get("op")?.as_str().map(str::to_owned)
}

/// What a `UserMessage` says without its attachment lines, and whether that is all of it (its body,
/// not the preview of a long one).
fn text_of(item: &Item) -> (String, bool) {
    let (text, whole) = match item.body.as_deref().and_then(body::text) {
        Some(t) => (t, true),
        None => (item.preview.clone(), false),
    };
    (attach::split(&text).0, whole)
}

/// Whether `real`, the echoed text, is the prompt `sent`. A preview (`whole` false) may be cut, so
/// then a prompt that starts with it counts too.
pub fn matches(real: &str, whole: bool, sent: &str) -> bool {
    let (real, sent) = (real.trim(), sent.trim());
    if real.starts_with(sent) {
        return true;
    }
    let cut = real.trim_end_matches('…').trim_end();
    !whole && !cut.is_empty() && sent.starts_with(cut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::state::ItemStatus;
    use femme_proto::Id;

    const SID: Sid = Id(7);

    fn user(id: &str, ts: i64, text: &str) -> Row {
        user_body(id, ts, text, serde_json::json!({ "text": text }))
    }

    fn user_body(id: &str, ts: i64, text: &str, body: serde_json::Value) -> Row {
        Row::Item(Item {
            sid: SID,
            item_id: id.into(),
            seq: 1,
            rev: 1,
            kind: ItemKind::UserMessage,
            status: ItemStatus::Completed,
            preview: text.chars().take(8).collect(),
            body: Some(body.to_string()),
            blob_hash: None,
            parent: None,
            ts,
        })
    }

    fn texts(o: &Outbox) -> Vec<String> {
        o.shown(SID).map(|s| s.text.clone()).collect()
    }

    #[test]
    fn text_matching() {
        assert!(matches("hey", true, "hey"));
        assert!(matches("  hey\n", true, "hey  "));
        assert!(matches("hey", true, ""), "attachments only: any text");
        assert!(matches("/btw what now", true, "/btw what now"));
        assert!(!matches("hello", true, "hey"));
        assert!(!matches("he", true, "hey"));
        // A preview is cut: a longer prompt that starts with it is the one.
        assert!(matches("a long pro…", false, "a long prompt"));
        assert!(!matches("a long pro…", true, "a long prompt"));
        assert!(!matches("", false, "x"));
    }

    #[test]
    fn the_stored_message_takes_off_the_prompt_sent_under_its_op() {
        let mut o = Outbox::default();
        let (a, b) = (Id(100), Id(200));
        o.push(SID, a, "yes".into(), vec![], 1_000);
        o.push(SID, b, "yes".into(), vec![], 1_000);
        let stored = |op: Id| {
            user_body(
                "01JC",
                1_001,
                "yes",
                serde_json::json!({ "text": "yes", "op": op.to_string() }),
            )
        };
        let v = o.ver;
        assert!(o.echo(SID, &[stored(b)]));
        assert!(o.ver > v);
        assert_eq!(o.shown(SID).map(|s| s.op).collect::<Vec<_>>(), [a]);
        assert!(
            !o.echo(SID, &[stored(Id(300))]),
            "another device's message is not ours"
        );
        assert!(o.echo(SID, &[stored(a)]));
        assert_eq!(o.shown(SID).count(), 0);
    }

    #[test]
    fn an_old_daemons_echo_takes_off_the_oldest_prompt_with_its_text() {
        let now = 1_000_000;
        let mut o = Outbox::default();
        o.push(SID, Id(1), "hey".into(), vec![], now);
        o.push(SID, Id(2), "there".into(), vec![], now + 10);
        o.push(SID, Id(3), "hey".into(), vec![], now + 20);
        assert!(o.echo(SID, &[user("a", now + 30, "there")]));
        assert_eq!(texts(&o), ["hey", "hey"]);
        assert!(o.echo(SID, &[user("b", now + 40, "hey")]));
        assert!(o.echo(SID, &[user("c", now + 50, "hey")]));
        assert_eq!(o.shown(SID).count(), 0);
        assert!(!o.echo(SID, &[user("d", now + 60, "hey")]));
    }

    #[test]
    fn attachment_lines_the_daemon_appends_still_match() {
        let now = 1_000_000;
        let mut o = Outbox::default();
        o.push(SID, Id(1), "look".into(), vec!["a.png".into()], now);
        let echo = "look\n\nAttached: /home/u/.femme/up/a.png";
        assert!(o.echo(SID, &[user("a", now, echo)]));
        assert_eq!(o.shown(SID).count(), 0);
    }

    #[test]
    fn an_old_message_does_not_take_a_new_prompt() {
        let now = 1_000_000;
        let mut o = Outbox::default();
        o.push(SID, Id(1), "hey".into(), vec![], now);
        // History coming in when the scope is joined: same text, long before.
        assert!(!o.echo(SID, &[user("old", now - 3_600_000, "hey")]));
        // A little behind our clock still counts for the same text...
        assert!(o.echo(SID, &[user("a", now - 2_000, "hey")]));
        // ...and with no text match, any message after the send takes the oldest.
        o.push(SID, Id(2), "one".into(), vec![], now);
        o.push(SID, Id(3), "two".into(), vec![], now);
        assert!(!o.echo(SID, &[user("b", now - 2_000, "other")]));
        assert!(o.echo(SID, &[user("c", now + 5, "other")]));
        assert_eq!(texts(&o), ["two"]);
    }

    #[test]
    fn other_sessions_and_kinds_are_left_alone() {
        let now = 1_000_000;
        let mut o = Outbox::default();
        o.push(SID, Id(1), "hey".into(), vec![], now);
        assert!(!o.echo(Id(8), &[user("a", now, "hey")]));
        let Row::Item(mut said) = user("a", now, "hey") else {
            unreachable!()
        };
        said.kind = ItemKind::AssistantMessage;
        assert!(!o.echo(SID, &[Row::Item(said)]));
        assert_eq!(o.shown(SID).count(), 1);
        assert_eq!(o.shown(Id(8)).count(), 0);
    }

    #[test]
    fn a_refused_prompt_goes_and_nothing_runs_out() {
        let mut o = Outbox::default();
        o.push(SID, Id(1), "a".into(), vec![], 0);
        o.push(SID, Id(2), "b".into(), vec![], 0);
        assert!(o.forget(SID, Id(1)));
        assert!(!o.forget(SID, Id(1)));
        assert!(!o.forget(Id(8), Id(2)));
        assert_eq!(
            texts(&o),
            ["b"],
            "no timer: it waits for its stored message"
        );
    }
}
