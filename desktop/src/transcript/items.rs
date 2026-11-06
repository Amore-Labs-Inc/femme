//! The items of one session as the daemon replicates them, and the text of the item in progress.
//! Plain data fed by the client's events and tested without gpui.
//!
//! Rows are rev-based: a row replaces the one held for its `item_id` unless that one is as new or
//! newer. Streaming text arrives apart from the rows, as `Delta`s that name a byte offset into the
//! item's text; they may come out of order, repeat after a reconnect (the daemon replays the open
//! item's text), start mid-item for a client that joined late, and even precede the row that
//! opens the item. Each item in progress therefore has a [`Stream`] that merges them by offset.

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use femme_proto::state::{Entity, Item, ItemStatus, Row};
use femme_proto::Sid;

use super::body::{self, Parsed};

/// The most text kept for one item in progress; later deltas are dropped (the final row carries
/// the whole body, up to the daemon's own blob cap of 1 MiB).
pub const MAX_STREAM: usize = 8 << 20;

/// How many items that have no row yet may hold streamed text: a delta can precede its row, but
/// not forever.
const MAX_ORPHANS: usize = 64;

/// Marks where text is missing from a stream (a client that joined mid-item never saw the head).
const GAP: &str = "…";

/// The text of an item in progress, assembled from deltas by byte offset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stream {
    /// The text from offset 0 up to the first gap.
    head: String,
    /// Text past a gap, by start offset; the segments never overlap or touch.
    tail: BTreeMap<usize, String>,
}

impl Stream {
    /// Takes in `text` that starts at byte `offset`; whether the stream grew. Bytes already held
    /// win over a repeat, so a replay is harmless.
    pub fn push(&mut self, offset: usize, text: &str) -> bool {
        let end = offset.saturating_add(text.len());
        if text.is_empty() || end > MAX_STREAM || end <= self.head.len() {
            return false;
        }
        if offset <= self.head.len() {
            let Some(new) = text.get(self.head.len() - offset..) else {
                return false; // the offset falls inside a character: not our text
            };
            self.head.push_str(new);
            self.absorb();
        } else {
            self.insert(offset, text);
        }
        true
    }

    /// Moves the tail segments that now touch the head into it.
    fn absorb(&mut self) {
        while let Some((&start, _)) = self.tail.first_key_value() {
            if start > self.head.len() {
                break;
            }
            let seg = self.tail.remove(&start).unwrap_or_default();
            if let Some(new) = seg.get(self.head.len() - start..) {
                self.head.push_str(new);
            }
        }
    }

    /// Adds a segment past a gap, merging it with the segments it overlaps or touches.
    fn insert(&mut self, offset: usize, text: &str) {
        let end = offset + text.len();
        let touching: Vec<usize> = self
            .tail
            .iter()
            .filter(|(s, t)| **s <= end && **s + t.len() >= offset)
            .map(|(s, _)| *s)
            .collect();
        if touching.is_empty() {
            self.tail.insert(offset, text.to_owned());
            return;
        }
        let first = touching[0].min(offset);
        let last = touching
            .iter()
            .map(|s| s + self.tail[s].len())
            .max()
            .unwrap_or(end)
            .max(end);
        let mut buf = vec![0u8; last - first];
        buf[offset - first..end - first].copy_from_slice(text.as_bytes());
        for s in touching {
            // What is already held wins.
            let seg = self.tail.remove(&s).unwrap_or_default();
            buf[s - first..s - first + seg.len()].copy_from_slice(seg.as_bytes());
        }
        self.tail
            .insert(first, String::from_utf8_lossy(&buf).into_owned());
    }

    /// The text so far, with a `…` where some is missing.
    pub fn text(&self) -> Cow<'_, str> {
        if self.tail.is_empty() {
            return Cow::Borrowed(&self.head);
        }
        let mut out = self.head.clone();
        let mut at = self.head.len();
        for (start, seg) in &self.tail {
            if *start > at {
                out.push_str(GAP);
            }
            out.push_str(seg);
            at = start + seg.len();
        }
        Cow::Owned(out)
    }

    /// Whether nothing has arrived.
    pub fn is_empty(&self) -> bool {
        self.head.is_empty() && self.tail.is_empty()
    }
}

/// One item of the transcript.
#[derive(Debug)]
pub struct Entry {
    /// The row as the daemon sent it.
    pub item: Item,
    /// Changes whenever what the item shows changes (a newer row, a delta, its blob arriving), so
    /// a view can tell which rows to measure again.
    pub ver: u64,
    parsed: OnceCell<Parsed>,
}

/// What an item shows as text, and whether that is all of it.
#[derive(Debug, PartialEq, Eq)]
pub struct Shown<'a> {
    /// The text.
    pub text: Cow<'a, str>,
    /// The item's body: nothing more will come. False while the text is only what has streamed
    /// in (or the preview), which is the case for an item in progress and, until its blob is
    /// fetched, for a long one.
    pub complete: bool,
}

/// The items of one session, by `seq`, and everything that streams or is fetched for them.
#[derive(Debug, Default)]
pub struct Items {
    sid: Option<Sid>,
    entries: BTreeMap<u64, Entry>,
    by_id: HashMap<String, u64>,
    streams: HashMap<String, Stream>,
    /// Streamed text of an item whose final row came without a body (a long one: its body is a
    /// blob): shown until the blob arrives.
    kept: HashMap<String, String>,
    blobs: HashMap<[u8; 32], Arc<str>>,
    clock: u64,
    /// Items older than this `seq` may exist on the daemon.
    older: Option<u64>,
    /// A page of history has been loaded: `older` comes from the daemon, not a guess.
    paged: bool,
}

impl Items {
    /// An empty set for `sid`: only its own rows are taken in.
    pub fn new(sid: Option<Sid>) -> Self {
        Self {
            sid,
            ..Self::default()
        }
    }

    /// Forgets the items, streams and fetched blobs and starts over for `sid`.
    pub fn switch(&mut self, sid: Option<Sid>) {
        *self = Self::new(sid);
    }

    /// Forgets the items and streams (the daemon cannot vouch for what we hold; its full set
    /// follows). Fetched blobs stay: they are named by their hash.
    pub fn reset(&mut self) {
        let blobs = std::mem::take(&mut self.blobs);
        let sid = self.sid;
        *self = Self::new(sid);
        self.blobs = blobs;
    }

    /// The session this set belongs to.
    pub fn sid(&self) -> Option<Sid> {
        self.sid
    }

    /// The items in order.
    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    /// The items of one thread, in order: the main one (`None`) or the subagent spawned by the
    /// `Agent` call `parent`.
    pub fn view<'a>(&'a self, parent: Option<&'a str>) -> impl Iterator<Item = &'a Entry> + 'a {
        self.entries
            .values()
            .filter(move |e| e.item.parent.as_deref() == parent)
    }

    /// How many items are held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no item is held.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The item with this `seq`.
    pub fn get(&self, seq: u64) -> Option<&Entry> {
        self.entries.get(&seq)
    }

    /// The item with this id.
    pub fn by_id(&self, id: &str) -> Option<&Entry> {
        self.entries.get(self.by_id.get(id)?)
    }

    /// Where older items may be: ask the daemon for the page before this `seq`. `None` when the
    /// start of the session is held.
    pub fn older(&self) -> Option<u64> {
        self.older
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Takes in rows of this session's scope; whether anything changed. Item rows replace the
    /// held one unless it is as new; a tombstone removes the item it names.
    pub fn apply_rows(&mut self, rows: &[Row]) -> bool {
        let mut changed = false;
        for row in rows {
            changed |= match row {
                Row::Item(item) if Some(item.sid) == self.sid => self.put(item.clone()),
                Row::Tombstone {
                    entity: Entity::Item,
                    id,
                    rev,
                } => self.remove(id, *rev),
                _ => false,
            };
        }
        if changed && !self.paged {
            self.older = self.entries.keys().next().copied().filter(|s| *s > 1);
        }
        changed
    }

    fn put(&mut self, item: Item) -> bool {
        if self
            .by_id(&item.item_id)
            .is_some_and(|old| old.item.rev >= item.rev)
        {
            return false;
        }
        if let Some(old_seq) = self.by_id.insert(item.item_id.clone(), item.seq) {
            if old_seq != item.seq {
                self.entries.remove(&old_seq);
            }
        }
        // A final row is the whole truth; a long one (blob) keeps what streamed in until it is
        // fetched.
        if matches!(
            item.status,
            ItemStatus::Completed | ItemStatus::Failed | ItemStatus::Answered
        ) {
            let streamed = self.streams.remove(&item.item_id);
            match (item.body.is_none(), streamed) {
                (true, Some(s)) if !s.is_empty() => {
                    self.kept
                        .insert(item.item_id.clone(), s.text().into_owned());
                }
                _ => {
                    self.kept.remove(&item.item_id);
                }
            }
        }
        let ver = self.tick();
        self.entries.insert(
            item.seq,
            Entry {
                item,
                ver,
                parsed: OnceCell::new(),
            },
        );
        true
    }

    /// A tombstone's `id` is `<sid>/<item_id>`.
    fn remove(&mut self, id: &str, rev: u64) -> bool {
        let Some((sid, item_id)) = id.split_once('/') else {
            return false;
        };
        if Some(sid) != self.sid.map(|s| s.to_string()).as_deref() {
            return false;
        }
        let Some(&seq) = self.by_id.get(item_id) else {
            return false;
        };
        if self.entries.get(&seq).is_some_and(|e| e.item.rev > rev) {
            return false; // the item was written again after the deletion this row reports
        }
        self.by_id.remove(item_id);
        self.streams.remove(item_id);
        self.kept.remove(item_id);
        self.entries.remove(&seq).is_some()
    }

    /// Streaming text of `item_id`, from byte `offset`; whether a row that is on show changed.
    /// Text for an item whose final row is in is ignored (the row has all of it); text for an
    /// item without a row is held for when it comes.
    pub fn apply_delta(&mut self, item_id: &str, offset: u32, text: &str) -> bool {
        let seq = self.by_id.get(item_id).copied();
        if let Some(e) = seq.and_then(|s| self.entries.get(&s)) {
            if !matches!(e.item.status, ItemStatus::InProgress | ItemStatus::Pending) {
                return false;
            }
        } else if !self.streams.contains_key(item_id) && self.streams.len() >= MAX_ORPHANS {
            // Streams of items that have rows end with them; these have none, so cap them.
            let orphans = self
                .streams
                .keys()
                .filter(|k| !self.by_id.contains_key(*k))
                .count();
            if orphans >= MAX_ORPHANS {
                return false;
            }
        }
        let grew = self
            .streams
            .entry(item_id.to_owned())
            .or_default()
            .push(offset as usize, text);
        let Some(seq) = seq.filter(|_| grew) else {
            return false;
        };
        let ver = self.tick();
        if let Some(e) = self.entries.get_mut(&seq) {
            e.ver = ver;
        }
        true
    }

    /// Takes in a page of older items (`TranscriptPage`, newest first); `next_before` is where
    /// the page after it starts, `None` at the start of the session. Whether anything changed.
    pub fn apply_page(&mut self, items: Vec<Item>, next_before: Option<u64>) -> bool {
        let mut changed = false;
        for item in items {
            if Some(item.sid) == self.sid {
                changed |= self.put(item);
            }
        }
        self.paged = true;
        self.older = next_before;
        changed
    }

    /// The JSON body of an item: inline, or the blob once it has been fetched.
    pub fn json<'a>(&'a self, e: &'a Entry) -> Option<&'a str> {
        e.item.body.as_deref().or_else(|| {
            let blob = e.item.blob_hash.as_ref()?;
            self.blobs.get(blob).map(|b| &**b)
        })
    }

    /// The body of an item, read (once per version of it).
    pub fn parsed<'a>(&'a self, e: &'a Entry) -> &'a Parsed {
        e.parsed
            .get_or_init(|| body::parse(e.item.kind, self.json(e)))
    }

    /// The text of an item: its body when there is one, else what has streamed in, else the
    /// preview.
    pub fn shown<'a>(&'a self, e: &'a Entry) -> Shown<'a> {
        if let Parsed::Text(t) = self.parsed(e) {
            return Shown {
                text: Cow::Borrowed(t),
                complete: true,
            };
        }
        let id = &e.item.item_id;
        let text = match (self.streams.get(id), self.kept.get(id)) {
            (Some(s), _) if !s.is_empty() => s.text(),
            (_, Some(k)) => Cow::Borrowed(k.as_str()),
            _ => Cow::Borrowed(e.item.preview.as_str()),
        };
        Shown {
            text,
            complete: false,
        }
    }

    /// The blob an item's body waits for, if it does and it has not been fetched.
    pub fn wants_blob(&self, e: &Entry) -> Option<[u8; 32]> {
        let hash = e.item.blob_hash?;
        (e.item.body.is_none() && !self.blobs.contains_key(&hash)).then_some(hash)
    }

    /// A fetched blob: the body of the items that named it. Its bytes are text; anything that is
    /// not UTF-8 is replaced.
    pub fn set_blob(&mut self, hash: [u8; 32], bytes: &[u8]) -> bool {
        let text: Arc<str> = String::from_utf8_lossy(bytes).into();
        self.blobs.insert(hash, text);
        let ver = self.tick();
        let mut any = false;
        for e in self
            .entries
            .values_mut()
            .filter(|e| e.item.blob_hash == Some(hash))
        {
            e.parsed = OnceCell::new();
            e.ver = ver;
            any = true;
        }
        any
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::state::ItemKind;
    use femme_proto::Id;

    const SID: Sid = Id(7);

    fn item(seq: u64, rev: u64, kind: ItemKind, status: ItemStatus, id: &str) -> Item {
        Item {
            sid: SID,
            item_id: id.into(),
            seq,
            rev,
            kind,
            status,
            preview: String::new(),
            body: None,
            blob_hash: None,
            parent: None,
            ts: 1_000 * seq as i64,
        }
    }

    fn done(seq: u64, rev: u64, id: &str, text: &str) -> Item {
        let mut i = item(
            seq,
            rev,
            ItemKind::AssistantMessage,
            ItemStatus::Completed,
            id,
        );
        i.body = Some(serde_json::json!({ "text": text }).to_string());
        i.preview = text.into();
        i
    }

    fn open(seq: u64, rev: u64, id: &str) -> Item {
        item(
            seq,
            rev,
            ItemKind::AssistantMessage,
            ItemStatus::InProgress,
            id,
        )
    }

    fn rows(items: Vec<Item>) -> Vec<Row> {
        items.into_iter().map(Row::Item).collect()
    }

    fn text(items: &Items, id: &str) -> String {
        items.shown(items.by_id(id).unwrap()).text.into_owned()
    }

    fn seqs(items: &Items) -> Vec<u64> {
        items.iter().map(|e| e.item.seq).collect()
    }

    #[test]
    fn items_are_ordered_by_seq_whatever_the_arrival_order() {
        let mut it = Items::new(Some(SID));
        assert!(it.apply_rows(&rows(vec![done(3, 1, "c", "3"), done(1, 1, "a", "1")])));
        assert!(it.apply_rows(&rows(vec![done(2, 1, "b", "2")])));
        assert_eq!(seqs(&it), [1, 2, 3]);
        assert_eq!(it.len(), 3);
    }

    #[test]
    fn a_row_replaces_the_held_one_only_when_newer() {
        let mut it = Items::new(Some(SID));
        it.apply_rows(&rows(vec![done(1, 5, "a", "new")]));
        let before = it.by_id("a").unwrap().ver;
        assert!(!it.apply_rows(&rows(vec![done(1, 5, "a", "same rev")])));
        assert!(!it.apply_rows(&rows(vec![done(1, 4, "a", "older")])));
        assert_eq!(it.by_id("a").unwrap().ver, before);
        assert_eq!(text(&it, "a"), "new");
        assert!(it.apply_rows(&rows(vec![done(1, 6, "a", "newer")])));
        assert_eq!(text(&it, "a"), "newer");
        assert!(it.by_id("a").unwrap().ver > before);
        assert_eq!(it.len(), 1);
    }

    #[test]
    fn rows_of_other_sessions_and_other_entities_are_ignored() {
        let mut it = Items::new(Some(SID));
        let mut other = done(1, 1, "x", "other session");
        other.sid = Id(8);
        assert!(!it.apply_rows(&[Row::Item(other)]));
        assert!(it.is_empty());
        let mut it = Items::new(None);
        assert!(!it.apply_rows(&rows(vec![done(1, 1, "x", "no session")])));
    }

    #[test]
    fn a_tombstone_removes_its_item_and_the_text_streamed_for_it() {
        let mut it = Items::new(Some(SID));
        it.apply_rows(&rows(vec![done(1, 1, "a", "keep"), open(2, 1, "b")]));
        it.apply_delta("b", 0, "partial");
        let tomb = |id: String, rev| Row::Tombstone {
            entity: Entity::Item,
            id,
            rev,
        };
        // Another session's, a malformed id, an unknown item, another entity: nothing happens.
        assert!(!it.apply_rows(&[tomb(format!("{}/b", Id(8)), 9)]));
        assert!(!it.apply_rows(&[tomb("b".into(), 9)]));
        assert!(!it.apply_rows(&[tomb(format!("{SID}/zzz"), 9)]));
        assert!(!it.apply_rows(&[Row::Tombstone {
            entity: Entity::Session,
            id: format!("{SID}/b"),
            rev: 9
        }]));
        assert_eq!(seqs(&it), [1, 2]);
        // An item written after the deletion this row reports stays.
        assert!(!it.apply_rows(&[tomb(format!("{SID}/b"), 0)]));
        assert!(it.apply_rows(&[tomb(format!("{SID}/b"), 9)]));
        assert_eq!(seqs(&it), [1]);
        assert!(it.by_id("b").is_none());
        // Its text is gone with it: a new item of that id starts empty.
        it.apply_rows(&rows(vec![open(5, 10, "b")]));
        assert_eq!(text(&it, "b"), "");
    }

    #[test]
    fn deltas_append_to_the_item_in_progress() {
        let mut it = Items::new(Some(SID));
        let mut start = open(1, 1, "a");
        start.preview = "pre".into();
        it.apply_rows(&rows(vec![start]));
        assert_eq!(text(&it, "a"), "pre", "no text yet: the preview");
        let v0 = it.by_id("a").unwrap().ver;
        assert!(it.apply_delta("a", 0, "Hello"));
        assert!(it.apply_delta("a", 5, ", wörld"));
        let v1 = it.by_id("a").unwrap().ver;
        assert!(v1 > v0);
        let e = it.by_id("a").unwrap();
        let shown = it.shown(e);
        assert_eq!(shown.text, "Hello, wörld");
        assert!(!shown.complete);
        // A repeat (a replay after a reconnect) is not a change.
        assert!(!it.apply_delta("a", 0, "Hello"));
        assert!(!it.apply_delta("a", 3, "lo, w"));
        assert_eq!(it.by_id("a").unwrap().ver, v1);
        // Overlapping the end extends by the new part only.
        assert!(it.apply_delta("a", 10, "rld!"));
        assert_eq!(text(&it, "a"), "Hello, wörld!");
    }

    #[test]
    fn deltas_out_of_order_wait_for_what_comes_before() {
        let mut it = Items::new(Some(SID));
        it.apply_rows(&rows(vec![open(1, 1, "a")]));
        it.apply_delta("a", 6, "world");
        it.apply_delta("a", 12, "!!");
        assert_eq!(text(&it, "a"), "…world…!!", "gaps are marked");
        it.apply_delta("a", 0, "Hello ");
        assert_eq!(text(&it, "a"), "Hello world…!!");
        it.apply_delta("a", 11, " ");
        assert_eq!(text(&it, "a"), "Hello world !!");
    }

    #[test]
    fn a_client_that_joined_late_shows_what_it_has() {
        let mut it = Items::new(Some(SID));
        it.apply_rows(&rows(vec![open(1, 1, "a")]));
        // The head (0..100) was streamed before we subscribed.
        it.apply_delta("a", 100, "the rest");
        it.apply_delta("a", 108, " of it");
        assert_eq!(text(&it, "a"), "…the rest of it");
        // The final row has all of it.
        it.apply_rows(&rows(vec![done(
            1,
            2,
            "a",
            "the whole text, the rest of it",
        )]));
        assert_eq!(text(&it, "a"), "the whole text, the rest of it");
        assert!(it.shown(it.by_id("a").unwrap()).complete);
    }

    #[test]
    fn overlapping_tail_segments_merge() {
        let mut s = Stream::default();
        s.push(10, "cdef");
        s.push(14, "ghij");
        s.push(12, "efgh");
        assert_eq!(s.text(), "…cdefghij");
        s.push(30, "xyz");
        assert_eq!(s.text(), "…cdefghij…xyz");
        s.push(0, "0123456789");
        assert_eq!(s.text(), "0123456789cdefghij…xyz");
        s.push(18, "0123456789ab");
        assert_eq!(s.text(), "0123456789cdefghij0123456789abxyz");
        assert!(!s.is_empty());
    }

    #[test]
    fn an_offset_inside_a_character_is_not_taken() {
        let mut s = Stream::default();
        assert!(s.push(0, "ab"));
        // Byte 1 of this text is inside its é: it cannot be the continuation of "ab".
        assert!(!s.push(1, "éa"));
        assert!(!s.push(0, ""));
        assert_eq!(s.text(), "ab");
    }

    #[test]
    fn a_delta_may_come_before_its_row() {
        let mut it = Items::new(Some(SID));
        assert!(
            !it.apply_delta("a", 0, "early "),
            "nothing on show to change"
        );
        it.apply_delta("a", 6, "bird");
        it.apply_rows(&rows(vec![open(1, 1, "a")]));
        assert_eq!(text(&it, "a"), "early bird");
    }

    #[test]
    fn streams_of_items_without_a_row_are_capped() {
        let mut it = Items::new(Some(SID));
        for i in 0..MAX_ORPHANS + 10 {
            it.apply_delta(&format!("orphan{i}"), 0, "x");
        }
        assert_eq!(it.streams.len(), MAX_ORPHANS);
        // An item that has a row still streams.
        it.apply_rows(&rows(vec![open(1, 1, "a")]));
        assert!(it.apply_delta("a", 0, "fine"));
    }

    #[test]
    fn deltas_for_a_final_item_are_ignored() {
        let mut it = Items::new(Some(SID));
        it.apply_rows(&rows(vec![open(1, 1, "a")]));
        it.apply_delta("a", 0, "partial");
        it.apply_rows(&rows(vec![done(1, 2, "a", "final")]));
        let v = it.by_id("a").unwrap().ver;
        assert!(!it.apply_delta("a", 7, " late"));
        assert_eq!(it.by_id("a").unwrap().ver, v);
        assert_eq!(text(&it, "a"), "final");
        assert!(it.streams.is_empty(), "the final row ended the stream");
    }

    #[test]
    fn a_long_item_keeps_its_streamed_text_until_its_blob_arrives() {
        let mut it = Items::new(Some(SID));
        it.apply_rows(&rows(vec![open(1, 1, "a")]));
        it.apply_delta("a", 0, "streamed text so far");
        let mut fin = item(1, 2, ItemKind::AssistantMessage, ItemStatus::Completed, "a");
        fin.preview = "streamed".into();
        fin.blob_hash = Some([9; 32]);
        it.apply_rows(&rows(vec![fin]));
        let e = it.by_id("a").unwrap();
        assert_eq!(it.wants_blob(e), Some([9; 32]));
        let shown = it.shown(e);
        assert_eq!(shown.text, "streamed text so far");
        assert!(!shown.complete);
        let v = e.ver;
        assert!(it.set_blob(
            [9; 32],
            serde_json::json!({"text": "the full body"})
                .to_string()
                .as_bytes()
        ));
        let e = it.by_id("a").unwrap();
        assert!(e.ver > v);
        assert_eq!(it.wants_blob(e), None);
        let shown = it.shown(e);
        assert_eq!(
            (shown.text.as_ref(), shown.complete),
            ("the full body", true)
        );
        // A blob nobody names changes no row.
        assert!(!it.set_blob([1; 32], b"{}"));
    }

    #[test]
    fn a_blob_that_is_not_utf8_is_replaced_not_dropped() {
        let mut it = Items::new(Some(SID));
        let mut fin = item(1, 1, ItemKind::Plan, ItemStatus::Completed, "p");
        fin.blob_hash = Some([2; 32]);
        it.apply_rows(&rows(vec![fin]));
        let mut raw = br#"{"text":"ok "#.to_vec();
        raw.extend([0xff, 0xfe]);
        raw.extend(br#" end"}"#);
        it.set_blob([2; 32], &raw);
        assert_eq!(text(&it, "p"), "ok \u{FFFD}\u{FFFD} end");
    }

    #[test]
    fn reset_drops_the_items_but_keeps_fetched_blobs() {
        let mut it = Items::new(Some(SID));
        let mut fin = item(1, 1, ItemKind::Plan, ItemStatus::Completed, "p");
        fin.blob_hash = Some([2; 32]);
        it.apply_rows(&rows(vec![fin.clone(), open(2, 1, "a")]));
        it.apply_delta("a", 0, "x");
        it.set_blob([2; 32], br#"{"text":"plan"}"#);
        it.reset();
        assert!(it.is_empty());
        assert_eq!(it.sid(), Some(SID));
        assert!(it.streams.is_empty());
        // What follows a reset is the full set again; the blob is still held.
        it.apply_rows(&rows(vec![fin]));
        assert_eq!(it.wants_blob(it.by_id("p").unwrap()), None);
        assert_eq!(text(&it, "p"), "plan");
        // Switching session forgets everything.
        it.switch(Some(Id(9)));
        assert!(it.is_empty());
        it.apply_rows(&rows(vec![done(1, 1, "z", "z")]));
        assert!(it.is_empty(), "rows of the old session no longer belong");
    }

    #[test]
    fn older_history_is_guessed_from_the_first_seq_then_told_by_the_pages() {
        let mut it = Items::new(Some(SID));
        assert_eq!(it.older(), None);
        it.apply_rows(&rows(vec![done(150, 1, "x", "x"), done(151, 1, "y", "y")]));
        assert_eq!(it.older(), Some(150), "items before 150 may exist");
        // A page, newest first; more before it.
        let page = vec![done(149, 1, "w", "w"), done(148, 1, "v", "v")];
        assert!(it.apply_page(page, Some(148)));
        assert_eq!(it.older(), Some(148));
        assert_eq!(seqs(&it), [148, 149, 150, 151]);
        // New rows do not move what the daemon told us.
        it.apply_rows(&rows(vec![done(152, 1, "u", "u")]));
        assert_eq!(it.older(), Some(148));
        // The start of the session.
        assert!(it.apply_page(vec![done(147, 1, "t", "t")], None));
        assert_eq!(it.older(), None);
        // A page we already hold changes nothing.
        assert!(!it.apply_page(vec![done(147, 1, "t", "t")], None));
    }

    #[test]
    fn a_session_that_starts_at_one_has_no_older_history() {
        let mut it = Items::new(Some(SID));
        it.apply_rows(&rows(vec![done(1, 1, "a", "a"), done(2, 1, "b", "b")]));
        assert_eq!(it.older(), None);
    }

    #[test]
    fn parsed_bodies_follow_the_row_version() {
        let mut it = Items::new(Some(SID));
        let mut call = item(1, 1, ItemKind::ToolCall, ItemStatus::InProgress, "t1");
        call.preview = "Bash".into();
        it.apply_rows(&rows(vec![call.clone()]));
        assert_eq!(*it.parsed(it.by_id("t1").unwrap()), Parsed::Empty);
        call.rev = 2;
        call.body = Some(
            serde_json::json!({"name":"Bash","input":{"command":"ls"},"call_id":"t1"}).to_string(),
        );
        it.apply_rows(&rows(vec![call]));
        let Parsed::ToolCall(c) = it.parsed(it.by_id("t1").unwrap()) else {
            panic!("not parsed")
        };
        assert_eq!(c.hint(), "ls");
    }
}
