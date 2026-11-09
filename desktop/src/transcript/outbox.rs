//! Prompts on their way: what the composer has sent and whose stored message has not come back
//! yet. The daemon stores a prompt before the agent gets it, with the request's op id in its body,
//! so a prompt is on its way only for the network round trip; the transcript draws it as a faded
//! "you" bubble at its end meanwhile. Only the stored message with that op id takes it off (live,
//! or in any load or page of the session); a daemon that stores none (one from before) leaves it
//! to [`KEEP`]. Plain data, tested without gpui.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use femme_proto::state::{Item, ItemKind, Row};
use femme_proto::{OpId, Sid};

/// A bubble whose stored message never came goes after this anyway.
pub const KEEP: Duration = Duration::from_secs(600);

/// One prompt on its way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    /// The op id it was sent under: its stored message carries it.
    pub op: OpId,
    /// The text as sent.
    pub text: String,
    /// The names of the files attached to it.
    pub files: Vec<String>,
    /// When it was sent, for [`KEEP`].
    at: Instant,
}

/// The prompts on their way, per session, oldest first.
#[derive(Debug, Default)]
pub struct Outbox {
    /// Changes whenever a prompt comes or goes, so a view can tell when to redraw.
    pub ver: u64,
    by_sid: HashMap<Sid, Vec<Sent>>,
}

impl Outbox {
    /// A prompt for `sid` was sent under `op` at `at`.
    pub fn push(&mut self, sid: Sid, op: OpId, text: String, files: Vec<String>, at: Instant) {
        self.ver += 1;
        let list = self.by_sid.entry(sid).or_default();
        list.retain(|s| at.saturating_duration_since(s.at) < KEEP);
        list.push(Sent {
            op,
            text,
            files,
            at,
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

    /// Rows of `sid`'s scope: see [`Outbox::seen`].
    pub fn echo(&mut self, sid: Sid, rows: &[Row]) -> bool {
        let items = rows.iter().filter_map(|r| match r {
            Row::Item(i) => Some(i),
            _ => None,
        });
        self.seen(sid, items)
    }

    /// Items of `sid` (a change, a load, a page): a `UserMessage` whose body names an op takes off
    /// the prompt sent under it. Whether any went.
    pub fn seen<'a>(&mut self, sid: Sid, items: impl IntoIterator<Item = &'a Item>) -> bool {
        let Some(list) = self.by_sid.get_mut(&sid) else {
            return false;
        };
        let n = list.len();
        for item in items {
            if list.is_empty() {
                break;
            }
            if item.sid != sid || item.kind != ItemKind::UserMessage {
                continue;
            }
            if let Some(op) = op_of(item) {
                list.retain(|s| s.op.to_string() != op);
            }
        }
        let gone = list.len() != n;
        self.ver += u64::from(gone);
        gone
    }

    /// The prompts of `sid` on their way at `now`, oldest first.
    pub fn shown(&self, sid: Sid, now: Instant) -> impl Iterator<Item = &Sent> {
        self.by_sid
            .get(&sid)
            .into_iter()
            .flatten()
            .filter(move |s| now.saturating_duration_since(s.at) < KEEP)
    }

    /// When the next of `sid`'s prompts runs out of [`KEEP`], if one is held.
    pub fn next_expiry(&self, sid: Sid, now: Instant) -> Option<Instant> {
        self.shown(sid, now).map(|s| s.at + KEEP).min()
    }
}

/// The op id a stored message names, when the daemon stored it before the agent had it.
fn op_of(item: &Item) -> Option<String> {
    let b: serde_json::Value = serde_json::from_str(item.body.as_deref()?).ok()?;
    b.get("op")?.as_str().map(str::to_owned)
}
/// Whether `real`, a message's text, is the prompt `sent`. A preview (`whole` false) may be cut,
/// so then a prompt that starts with it counts too.
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

    fn user(id: &str, text: &str, op: Option<Id>) -> Row {
        let mut body = serde_json::json!({ "text": text });
        if let Some(op) = op {
            body["op"] = serde_json::Value::String(op.to_string());
        }
        Row::Item(Item {
            sid: SID,
            item_id: id.into(),
            seq: 1,
            rev: 1,
            kind: ItemKind::UserMessage,
            status: ItemStatus::Pending,
            preview: text.into(),
            body: Some(body.to_string()),
            blob_hash: None,
            parent: None,
            ts: 1_000,
        })
    }

    fn ops(o: &Outbox, now: Instant) -> Vec<Id> {
        o.shown(SID, now).map(|s| s.op).collect()
    }

    #[test]
    fn text_matching() {
        assert!(matches("hey", true, "hey"));
        assert!(matches("  hey\n", true, "hey  "));
        assert!(matches("hey", true, ""), "attachments only: any text");
        assert!(!matches("hello", true, "hey"));
        assert!(matches("a long pro…", false, "a long prompt"));
        assert!(!matches("a long pro…", true, "a long prompt"));
        assert!(!matches("", false, "x"));
    }

    #[test]
    fn the_stored_message_takes_off_the_prompt_sent_under_its_op() {
        let t = Instant::now();
        let mut o = Outbox::default();
        o.push(SID, Id(100), "yes".into(), vec![], t);
        o.push(SID, Id(200), "yes".into(), vec![], t);
        let v = o.ver;
        assert!(o.echo(SID, &[user("01JC1", "yes", Some(Id(200)))]));
        assert!(o.ver > v);
        assert_eq!(ops(&o, t), [Id(100)]);
        assert!(
            !o.echo(SID, &[user("01JC2", "yes", Some(Id(300)))]),
            "another device's message is not ours"
        );
        assert!(
            !o.echo(SID, &[user("d9", "yes", None)]),
            "a message without an op (typed in the pane) takes nothing off"
        );
        assert_eq!(ops(&o, t), [Id(100)]);
    }

    #[test]
    fn a_page_or_a_load_takes_it_off_too() {
        let t = Instant::now();
        let mut o = Outbox::default();
        o.push(SID, Id(1), "hi".into(), vec![], t);
        let Row::Item(item) = user("01JC", "hi", Some(Id(1))) else {
            unreachable!()
        };
        assert!(o.seen(SID, [&item]));
        assert_eq!(o.shown(SID, t).count(), 0);
    }

    #[test]
    fn other_sessions_and_kinds_are_left_alone() {
        let t = Instant::now();
        let mut o = Outbox::default();
        o.push(SID, Id(1), "hey".into(), vec![], t);
        assert!(!o.echo(Id(8), &[user("a", "hey", Some(Id(1)))]));
        let Row::Item(mut said) = user("a", "hey", Some(Id(1))) else {
            unreachable!()
        };
        said.kind = ItemKind::AssistantMessage;
        assert!(!o.echo(SID, &[Row::Item(said)]));
        assert_eq!(o.shown(SID, t).count(), 1);
        assert_eq!(o.shown(Id(8), t).count(), 0);
    }

    #[test]
    fn a_refused_prompt_goes_and_an_unstored_one_runs_out_after_ten_minutes() {
        let t = Instant::now();
        let mut o = Outbox::default();
        o.push(SID, Id(1), "a".into(), vec![], t);
        o.push(SID, Id(2), "b".into(), vec![], t + Duration::from_secs(5));
        assert!(o.forget(SID, Id(1)));
        assert!(!o.forget(SID, Id(1)));
        assert!(!o.forget(Id(8), Id(2)));
        assert_eq!(ops(&o, t + Duration::from_secs(300)), [Id(2)]);
        assert_eq!(
            o.next_expiry(SID, t),
            Some(t + Duration::from_secs(5) + KEEP)
        );
        let late = t + Duration::from_secs(5) + KEEP;
        assert_eq!(o.shown(SID, late).count(), 0);
        assert_eq!(o.next_expiry(SID, late), None);
    }
}
