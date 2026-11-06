//! Prompts on their way: what the composer has sent and the daemon has not yet echoed as a
//! `UserMessage`. The transcript draws them as faded "you" bubbles at its end, so a prompt shows
//! the instant it is sent instead of when the daemon stores it (0.3–1.5 s for a herdr session).
//! Plain data, tested without gpui.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use femme_proto::state::{Item, ItemKind, Row};
use femme_proto::Sid;

use super::{attach, body};

/// A prompt not echoed by then is dropped anyway: the echo was missed or never comes.
pub const KEEP: Duration = Duration::from_secs(30);

/// How far the daemon's clock may lag ours and a text match still count.
const SKEW_MS: i64 = 10_000;

/// One prompt on its way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    /// Names it for [`Outbox::forget`].
    pub id: u64,
    /// The text as sent.
    pub text: String,
    /// The names of the files attached to it.
    pub files: Vec<String>,
    /// When it was sent, in ms since the epoch: the echo's `ts` is compared with it.
    at_ms: i64,
    /// When it was sent, for [`KEEP`].
    at: Instant,
}

/// The prompts on their way, per session, oldest first.
#[derive(Debug, Default)]
pub struct Outbox {
    next: u64,
    /// Changes whenever a prompt comes or goes, so a view can tell when to redraw.
    pub ver: u64,
    by_sid: HashMap<Sid, Vec<Sent>>,
}

impl Outbox {
    /// A prompt for `sid` was sent at `at` (`at_ms` since the epoch); its id.
    pub fn push(
        &mut self,
        sid: Sid,
        text: String,
        files: Vec<String>,
        at: Instant,
        at_ms: i64,
    ) -> u64 {
        self.next += 1;
        self.ver += 1;
        let id = self.next;
        let list = self.by_sid.entry(sid).or_default();
        list.retain(|s| at.saturating_duration_since(s.at) < KEEP);
        list.push(Sent {
            id,
            text,
            files,
            at_ms,
            at,
        });
        id
    }

    /// Drops prompt `id` of `sid` (the daemon refused it); whether it was held.
    pub fn forget(&mut self, sid: Sid, id: u64) -> bool {
        let Some(list) = self.by_sid.get_mut(&sid) else {
            return false;
        };
        let n = list.len();
        list.retain(|s| s.id != id);
        let gone = list.len() != n;
        self.ver += u64::from(gone);
        gone
    }

    /// Rows of `sid`'s scope: each `UserMessage` takes off the oldest prompt whose text it starts
    /// with (the daemon appends `Attached:` lines), else, when none matches, the oldest one sent
    /// before it. Whether any went.
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
            let (real, whole) = text_of(item);
            let hit = list
                .iter()
                .position(|s| item.ts >= s.at_ms - SKEW_MS && matches(&real, whole, &s.text))
                .or_else(|| list.iter().position(|s| item.ts >= s.at_ms));
            if let Some(i) = hit {
                list.remove(i);
            }
        }
        let gone = list.len() != n;
        self.ver += u64::from(gone);
        gone
    }

    /// The prompts of `sid` still on their way at `now`, oldest first.
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
        Row::Item(Item {
            sid: SID,
            item_id: id.into(),
            seq: 1,
            rev: 1,
            kind: ItemKind::UserMessage,
            status: ItemStatus::Completed,
            preview: text.chars().take(8).collect(),
            body: Some(serde_json::json!({ "text": text }).to_string()),
            blob_hash: None,
            ts,
        })
    }

    fn texts(o: &Outbox, now: Instant) -> Vec<String> {
        o.shown(SID, now).map(|s| s.text.clone()).collect()
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
    fn the_echo_takes_off_the_oldest_prompt_with_its_text() {
        let (t, now) = (Instant::now(), 1_000_000);
        let mut o = Outbox::default();
        o.push(SID, "hey".into(), vec![], t, now);
        o.push(SID, "there".into(), vec![], t, now + 10);
        o.push(SID, "hey".into(), vec![], t, now + 20);
        let v = o.ver;
        assert!(o.echo(SID, &[user("a", now + 30, "there")]));
        assert!(o.ver > v);
        assert_eq!(texts(&o, t), ["hey", "hey"]);
        assert!(o.echo(SID, &[user("b", now + 40, "hey")]));
        assert_eq!(o.shown(SID, t).count(), 1);
        assert!(o.echo(SID, &[user("c", now + 50, "hey")]));
        assert_eq!(o.shown(SID, t).count(), 0);
        assert!(!o.echo(SID, &[user("d", now + 60, "hey")]));
    }

    #[test]
    fn attachment_lines_the_daemon_appends_still_match() {
        let (t, now) = (Instant::now(), 1_000_000);
        let mut o = Outbox::default();
        o.push(SID, "look".into(), vec!["a.png".into()], t, now);
        let echo = "look\n\nAttached: /home/u/.femme/up/a.png";
        assert!(o.echo(SID, &[user("a", now, echo)]));
        assert_eq!(o.shown(SID, t).count(), 0);
    }

    #[test]
    fn an_old_message_does_not_take_a_new_prompt() {
        let (t, now) = (Instant::now(), 1_000_000);
        let mut o = Outbox::default();
        o.push(SID, "hey".into(), vec![], t, now);
        // History coming in when the scope is joined: same text, long before.
        assert!(!o.echo(SID, &[user("old", now - 3_600_000, "hey")]));
        // A little behind our clock still counts for the same text...
        assert!(o.echo(SID, &[user("a", now - 2_000, "hey")]));
        // ...and with no text match, any message after the send takes the oldest.
        o.push(SID, "one".into(), vec![], t, now);
        o.push(SID, "two".into(), vec![], t, now);
        assert!(!o.echo(SID, &[user("b", now - 2_000, "other")]));
        assert!(o.echo(SID, &[user("c", now + 5, "other")]));
        assert_eq!(texts(&o, t), ["two"]);
    }

    #[test]
    fn other_sessions_and_kinds_are_left_alone() {
        let (t, now) = (Instant::now(), 1_000_000);
        let mut o = Outbox::default();
        o.push(SID, "hey".into(), vec![], t, now);
        assert!(!o.echo(Id(8), &[user("a", now, "hey")]));
        let Row::Item(mut said) = user("a", now, "hey") else {
            unreachable!()
        };
        said.kind = ItemKind::AssistantMessage;
        assert!(!o.echo(SID, &[Row::Item(said)]));
        assert_eq!(o.shown(SID, t).count(), 1);
        assert_eq!(o.shown(Id(8), t).count(), 0);
    }

    #[test]
    fn a_refused_prompt_goes_and_an_unanswered_one_runs_out() {
        let (t, now) = (Instant::now(), 1_000_000);
        let mut o = Outbox::default();
        let a = o.push(SID, "a".into(), vec![], t, now);
        o.push(SID, "b".into(), vec![], t + Duration::from_secs(5), now);
        assert!(o.forget(SID, a));
        assert!(!o.forget(SID, a));
        assert!(!o.forget(Id(8), a));
        assert_eq!(texts(&o, t), ["b"]);
        assert_eq!(
            o.next_expiry(SID, t),
            Some(t + Duration::from_secs(5) + KEEP)
        );
        let late = t + Duration::from_secs(35);
        assert_eq!(o.shown(SID, late).count(), 0);
        assert_eq!(o.next_expiry(SID, late), None);
        // The next send clears what ran out.
        o.push(SID, "c".into(), vec![], late, now);
        assert_eq!(o.by_sid[&SID].len(), 1);
    }
}
