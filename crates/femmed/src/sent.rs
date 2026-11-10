//! Save first: a prompt a client sends to a headless session is stored as the session's
//! `UserMessage` before the agent gets it, so it shows on every device at once and outlives a
//! restart, however long the agent takes to read it. The row waits (`Pending`) until the agent's
//! echo of it comes through [`super::headless`]'s `put`, which pairs the two: the row turns
//! `Completed` with `read_at`, and the echo is stored as a thin `Event` where the agent read it
//! (`read` names the row). The row never moves.
//!
//! Pairing, from sure to unsure:
//!
//! - A Claude chat of FEMME's own writes the message under a uuid of ours (the row's `echo`) and
//!   the CLI echoes it under that uuid: the echo with that id is the row's.
//! - Any other agent, and a herdr pane (text typed into a terminal), carries no id: the oldest
//!   waiting row without one whose text is the echo's, `Attached:` lines and `[Image #N]` marks
//!   left out and spaces collapsed, is the row's.
//! - Nothing else pairs. An echo that matches no row is a row of its own (a message typed in the
//!   pane is one), and the row it may have been stays waiting: shown twice at worst, never read
//!   when it was not.
//!
//! A message the agent did not get is `Failed` (a client offers to send it again) only on a
//! definite word: the agent refused or discarded it, the session moved to the terminal or failed
//! with it still held, or the agent went idle with the row waiting and nothing still holding it.
//! A wrong `Failed` heals: pairing takes `Failed` rows too, so a late echo still reads the row,
//! and nothing is shown twice. Nothing here deletes a message the agent may have read.

use femme_proto::rpc::PromptHow;
use femme_proto::rpc::Scope;
use femme_proto::state::{Agent, Item, ItemKind, ItemStatus, Mode, Session};
use femme_proto::{OpId, Sid};
use femme_store::{items as store_items, sessions, Tx};
use serde_json::{json, Value};
use ulid::Ulid;

use crate::core::CoreError;
use crate::daemon::Daemon;
use crate::session::{agent_label, gone};

/// A row saved by [`Daemon::save_prompt`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Saved {
    pub item_id: String,
    /// The id the agent is to echo the message under, for an agent that takes one.
    pub echo: Option<String>,
}

/// The id a session's agent echoes a message under: a fresh uuid for a Claude chat of FEMME's own,
/// none for other agents and for herdr panes (whose text is typed into a terminal).
fn echo_for(s: &Session) -> Option<String> {
    let own = !s.owner.starts_with(crate::headless::herdr::OWNER);
    (s.agent == Agent::Claude && own).then(femme_agents::uuid_v4)
}

/// Whether a prompt is saved first: a headless session's message. A `/` command is the agent's
/// business (its echo, if any, is an event), and an empty text has nothing to show.
fn saves(s: &Session, text: &str) -> bool {
    let t = text.trim_start();
    s.mode == Mode::Headless && !t.is_empty() && !t.starts_with('/')
}

/// The row of a prompt as sent, waiting for its echo.
fn saved_item(sid: Sid, text: &str, op: Option<OpId>, echo: Option<&str>, now: i64) -> Item {
    let mut b = json!({ "text": text });
    if let Some(op) = op {
        b["op"] = Value::String(op.to_string());
    }
    if let Some(e) = echo {
        b["echo"] = Value::String(e.to_owned());
    }
    Item {
        sid,
        item_id: Ulid::generate().to_string(),
        seq: 0,
        rev: 0,
        kind: ItemKind::UserMessage,
        status: ItemStatus::Pending,
        preview: text.to_owned(),
        body: Some(b.to_string()),
        blob_hash: None,
        ts: now,
        parent: None,
    }
}

/// A message's text for comparing: without the `Attached:` lines the daemon adds (and the bare
/// `Attached:` Claude's terminal leaves when it takes the file as a picture), and the `[Image #N]`
/// marks and `<pasted_content id="..">` wrapping it adds to what is pasted into it, every run of
/// spaces and newlines one space.
pub(crate) fn normalize(text: &str) -> String {
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| {
            let l = l.trim();
            !l.starts_with("Attached: ") && l != "Attached:"
        })
        .collect();
    let mut s = kept.join("\n");
    // the closing tag carries the id too: `</pasted_content id="23c2">`
    while let Some(i) = [s.find("<pasted_content"), s.find("</pasted_content")]
        .into_iter()
        .flatten()
        .min()
    {
        let Some(end) = s[i..].find('>') else { break };
        s.replace_range(i..i + end + 1, " ");
    }
    while let Some(i) = s.find("[Image #") {
        let after = &s[i + "[Image #".len()..];
        let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 || !after[digits..].starts_with(']') {
            break;
        }
        s.replace_range(i..i + "[Image #".len() + digits + 1, " ");
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn text_of(b: &Value) -> &str {
    b["text"].as_str().unwrap_or("")
}

/// The base names of a text's `Attached:` files, sorted.
fn attached(text: &str) -> Vec<&str> {
    let mut names: Vec<&str> = text
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("Attached: "))
        .map(|p| p.trim().rsplit('/').next().unwrap_or(""))
        .filter(|n| !n.is_empty())
        .collect();
    names.sort_unstable();
    names
}

/// How much the user may have typed into a pane's input ahead of a message FEMME typed after it,
/// and the least a message must be to be known by that end alone.
const LEFTOVER_MAX: usize = 32;
const KNOWN_MIN: usize = 12;

/// Whether the saved text `a` is the echo `b`: the same words ([`normalize`]); the echo may start
/// with a little the user had typed into the pane's input before FEMME typed the message after
/// it (`th` + `i talked…`). With no words at all, the same attached files.
pub(crate) fn same_message(a: &str, b: &str) -> bool {
    let (na, nb) = (normalize(a), normalize(b));
    if !na.is_empty() || !nb.is_empty() {
        return na == nb
            || (na.len() >= KNOWN_MIN
                && nb.len() - na.len().min(nb.len()) <= LEFTOVER_MAX
                && nb.ends_with(&na));
    }
    let (fa, fb) = (attached(a), attached(b));
    !fa.is_empty() && fa == fb
}

/// Which of the saved rows (oldest first, with their bodies) the echo `echo_id` saying `echo_text`
/// is: the row with that `echo` id, else the oldest row without an id that is the same message,
/// a `Pending` one before a `Failed` one. `None` when none is sure.
pub(crate) fn pick(rows: &[(Item, Value)], echo_id: &str, echo_text: &str) -> Option<usize> {
    if let Some(i) = rows
        .iter()
        .position(|(_, b)| b["echo"].as_str() == Some(echo_id))
    {
        return Some(i);
    }
    let like = |status: ItemStatus| {
        rows.iter().position(|(i, b)| {
            i.status == status && b.get("echo").is_none() && same_message(text_of(b), echo_text)
        })
    };
    like(ItemStatus::Pending).or_else(|| like(ItemStatus::Failed))
}

/// A stored item's body as JSON, from the blob when it is one (`Null` when it is gone).
fn body_of(tx: &Tx<'_>, i: &Item) -> femme_store::Result<Value> {
    let raw = match (&i.body, i.blob_hash) {
        (Some(b), _) => Some(b.clone()),
        (None, Some(h)) => {
            store_items::get_blob(tx, &h)?.map(|b| String::from_utf8_lossy(&b).into_owned())
        }
        (None, None) => None,
    };
    Ok(raw
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or(Value::Null))
}

/// The session's saved rows in `statuses`, oldest first, with their bodies. Echoes of the agent's
/// own (no `op`, `Completed`) are never among them: only a saved row is ever `Pending` or `Failed`.
fn waiting(
    tx: &Tx<'_>,
    sid: Sid,
    statuses: &[ItemStatus],
) -> femme_store::Result<Vec<(Item, Value)>> {
    let rows = store_items::items_with_status(tx, sid, ItemKind::UserMessage, statuses)?;
    rows.into_iter()
        .filter(|i| i.parent.is_none())
        .map(|i| body_of(tx, &i).map(|b| (i, b)))
        .collect()
}

/// Stores `item` with `status` and `b` as its body.
fn rewrite(
    tx: &mut Tx<'_>,
    mut item: Item,
    status: ItemStatus,
    b: &Value,
    published: &mut Vec<Item>,
) -> femme_store::Result<()> {
    item.status = status;
    item.body = Some(b.to_string());
    item.blob_hash = None;
    published.push(tx.put_item(&item)?);
    Ok(())
}

/// Whether a stored item is the echo of a saved row, already paired: a replay of the echo must
/// leave it be.
pub(crate) fn is_read_mark(i: &Item) -> bool {
    i.kind == ItemKind::Event
        && i.body
            .as_deref()
            .and_then(|b| serde_json::from_str::<Value>(b).ok())
            .is_some_and(|b| b.get("read").is_some())
}

/// Pairs a new echo (a top-level `UserMessage` the store does not hold yet) with the saved row it
/// is: the row is read at the echo's time, and the echo is stored as the `Event` that says so.
/// `false` (nothing written) when it is no saved row's.
pub(crate) fn reconcile(
    tx: &mut Tx<'_>,
    sid: Sid,
    echo: &Item,
    published: &mut Vec<Item>,
) -> femme_store::Result<bool> {
    if echo.kind != ItemKind::UserMessage || echo.parent.is_some() {
        return Ok(false);
    }
    let rows = waiting(tx, sid, &[ItemStatus::Pending, ItemStatus::Failed])?;
    if rows.is_empty() {
        return Ok(false);
    }
    let said = echo
        .body
        .as_deref()
        .and_then(|b| serde_json::from_str::<Value>(b).ok())
        .map(|b| text_of(&b).to_owned())
        .unwrap_or_else(|| echo.preview.clone());
    let Some(at) = pick(&rows, &echo.item_id, &said) else {
        return Ok(false);
    };
    let (row, mut b) = rows.into_iter().nth(at).expect("picked from rows");
    let row_id = row.item_id.clone();
    b["read_at"] = json!(echo.ts);
    rewrite(tx, row, ItemStatus::Completed, &b, published)?;
    let who = sessions::get_session(tx, sid)?.map_or("The agent", |s| agent_label(s.agent));
    let text = format!("{who} read your message");
    let mark = Item {
        sid,
        item_id: echo.item_id.clone(),
        seq: 0,
        rev: 0,
        kind: ItemKind::Event,
        status: ItemStatus::Completed,
        preview: text.clone(),
        body: Some(json!({ "text": text, "read": row_id }).to_string()),
        blob_hash: None,
        ts: echo.ts,
        parent: None,
    };
    published.push(tx.put_item(&mark)?);
    Ok(true)
}

/// Pairs, once more, the saved rows marked not received whose echo was stored as a message of its
/// own (the match was missed when it came: a shape of echo not known then): the row is read at
/// the echo's time and the echo becomes the mark that says so. Returns how many. Run at start,
/// before the store is shared; it is cheap when no row waits.
pub(crate) fn repair(store: &femme_store::Store) -> femme_store::Result<usize> {
    let mut n = 0;
    for s in store.read(|c| sessions::list_sessions(c, true))? {
        let failed = store.read(|c| {
            store_items::items_with_status(c, s.id, ItemKind::UserMessage, &[ItemStatus::Failed])
        })?;
        if failed.is_empty() {
            continue;
        }
        n += store.write(|tx| {
            let mut done = 0;
            let all = waiting(tx, s.id, &[ItemStatus::Failed, ItemStatus::Completed])?;
            for (row, b) in all.iter().filter(|(i, b)| {
                i.status == ItemStatus::Failed && b.get("op").is_some() && b.get("echo").is_none()
            }) {
                // the echo: the first later message of the agent's own that is this one
                let echo = all
                    .iter()
                    .filter(|(i, eb)| {
                        i.seq > row.seq
                            && i.status == ItemStatus::Completed
                            && eb.get("op").is_none()
                            && eb.get("read_at").is_none()
                    })
                    .take(3)
                    .find(|(_, eb)| same_message(text_of(b), text_of(eb)));
                let Some((echo, _)) = echo else { continue };
                let mut b = b.clone();
                b["read_at"] = json!(echo.ts);
                let mut published = Vec::new();
                rewrite(tx, row.clone(), ItemStatus::Completed, &b, &mut published)?;
                let who = agent_label(s.agent);
                let text = format!("{who} read your message");
                let mut mark = echo.clone();
                mark.kind = ItemKind::Event;
                mark.preview = text.clone();
                mark.body = Some(json!({ "text": text, "read": row.item_id }).to_string());
                mark.blob_hash = None;
                tx.put_item(&mark)?;
                done += 1;
            }
            Ok(done)
        })?;
    }
    Ok(n)
}

/// A prompt as the agent's driver holds it: the echo id it was given, and its text.
pub(crate) type Held = (Option<String>, String);

/// What the agent said about the messages it was given.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Gone {
    /// These will not run.
    Prompts(Vec<Held>),
    /// The agent is idle and still holds only these: every other waiting row was not got.
    Idle(Vec<Held>),
    /// Nothing will run any more.
    All,
}

/// Whether the saved row (body `b`) is the prompt `h`: by the echo id when the row has one.
fn is_prompt(b: &Value, h: &Held) -> bool {
    match (b["echo"].as_str(), &h.0) {
        (Some(e), Some(id)) => e == id,
        (None, None) => same_message(text_of(b), &h.1),
        _ => false,
    }
}

/// Marks `Failed` the waiting rows `gone` says the agent did not get.
pub(crate) fn settle(
    tx: &mut Tx<'_>,
    sid: Sid,
    gone: &Gone,
    published: &mut Vec<Item>,
) -> femme_store::Result<()> {
    let rows = waiting(tx, sid, &[ItemStatus::Pending])?;
    let mut failed = vec![false; rows.len()];
    match gone {
        Gone::All => failed.fill(true),
        Gone::Prompts(prompts) => {
            for h in prompts {
                let hit = (0..rows.len()).find(|&i| !failed[i] && is_prompt(&rows[i].1, h));
                if let Some(i) = hit {
                    failed[i] = true;
                }
            }
        }
        Gone::Idle(held) => {
            let mut kept = vec![false; rows.len()];
            for h in held {
                let hit = (0..rows.len()).find(|&i| !kept[i] && is_prompt(&rows[i].1, h));
                if let Some(i) = hit {
                    kept[i] = true;
                }
            }
            failed = kept.iter().map(|k| !k).collect();
        }
    }
    for ((row, b), f) in rows.into_iter().zip(failed) {
        if f {
            rewrite(tx, row, ItemStatus::Failed, &b, published)?;
        }
    }
    Ok(())
}

/// Deletes the saved row `item_id` if it still waits; whether it did.
fn take_back(tx: &mut Tx<'_>, sid: Sid, item_id: &str) -> femme_store::Result<bool> {
    match store_items::get_item(tx, sid, item_id)? {
        Some(i) if i.kind == ItemKind::UserMessage && i.status == ItemStatus::Pending => {
            tx.delete_item(sid, item_id)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

impl Daemon {
    /// Stores `text` as `s`'s message waiting for the agent ([`saves`] says which are), with the
    /// client's op id; `None` for a message that is not saved.
    pub(crate) fn save_prompt(
        &self,
        s: &Session,
        text: &str,
        op: Option<OpId>,
    ) -> Result<Option<Saved>, CoreError> {
        if !saves(s, text) {
            return Ok(None);
        }
        let echo = echo_for(s);
        let id = self.write_items(|tx, now, published| {
            let item = saved_item(s.id, text, op, echo.as_deref(), now);
            let stored = tx.put_item(&item)?;
            let id = stored.item_id.clone();
            published.push(stored);
            Ok(id)
        })?;
        Ok(Some(Saved { item_id: id, echo }))
    }

    /// Takes back a saved message the agent was never handed (the send failed at once, and the
    /// client keeps the text to send again), unless it is waiting no longer: an echo that paired
    /// with it meanwhile means the agent has it after all.
    pub(crate) fn unsave(&self, sid: Sid, item_id: &str) {
        let r = (|| -> femme_store::Result<()> {
            let before = self.store.rev()?;
            if !self.store.write(|tx| take_back(tx, sid, item_id))? {
                return Ok(());
            }
            let ch = self
                .store
                .changes_since(Scope::Session(sid), Some(before), usize::MAX)?;
            self.hub.publish(ch.rows);
            Ok(())
        })();
        if let Err(e) = r {
            tracing::warn!(%sid, item_id, %e, "unsent message not taken back");
        }
    }

    /// A herdr pane's Claude went idle: a waiting message it has not echoed was not got (the
    /// terminal holds nothing FEMME can see).
    pub(crate) fn idle_unread(&self, sid: Sid) {
        let r = self
            .write_items(|tx, _, published| settle(tx, sid, &Gone::Idle(Vec::new()), published));
        if let Err(e) = r {
            tracing::warn!(%sid, %e, "unread messages not marked");
        }
    }

    /// `Req::Resend`: the message `item_id` the agent did not get waits again and goes to it
    /// again, after the turn that runs. `Conflict` for any other item.
    pub(crate) async fn resend(&self, sid: Sid, item_id: &str) -> Result<(), CoreError> {
        let s = self.session(sid)?;
        let head = self.sessions.head(sid).ok_or_else(gone)?;
        let echo = echo_for(&s);
        let flip = |status: ItemStatus, echo: Option<&str>| {
            self.write_items(|tx, now, published| {
                let Some(item) = store_items::get_item(tx, sid, item_id)? else {
                    return Ok(None);
                };
                let want = match status {
                    ItemStatus::Pending => ItemStatus::Failed,
                    _ => ItemStatus::Pending,
                };
                if item.kind != ItemKind::UserMessage || item.status != want {
                    return Ok(None);
                }
                let mut b = body_of(tx, &item)?;
                let text = text_of(&b).to_owned();
                if let Some(e) = echo {
                    b["echo"] = Value::String(e.to_owned());
                }
                if status == ItemStatus::Pending {
                    b["sent_at"] = json!(now);
                }
                rewrite(tx, item, status, &b, published)?;
                Ok(Some(text))
            })
        };
        let Some(text) = flip(ItemStatus::Pending, echo.as_deref())? else {
            return Err(CoreError::Conflict(
                "only a message the agent did not get is sent again".into(),
            ));
        };
        let r = head.prompt_as(&text, PromptHow::Queue, echo).await;
        if r.is_err() {
            if let Err(e) = flip(ItemStatus::Failed, None) {
                tracing::warn!(%sid, item_id, %e, "resent message not marked missed again");
            }
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, status: ItemStatus, b: Value) -> (Item, Value) {
        let mut i = saved_item(Sid::default(), text_of(&b), None, None, 1_000);
        i.item_id = id.into();
        i.status = status;
        (i, b)
    }

    #[test]
    fn the_echo_with_the_rows_own_uuid_is_its_whatever_it_says() {
        let rows = [
            row(
                "01JC0",
                ItemStatus::Pending,
                json!({"text": "fix the build", "echo": "c8eed47b-e7a7-4739-8260-f78e891051e8"}),
            ),
            row(
                "01JC1",
                ItemStatus::Failed,
                json!({"text": "also run the tests", "echo": "ff4010ac-34a1-4ae7-b4b4-ec021ede5a7f"}),
            ),
        ];
        assert_eq!(
            pick(&rows, "ff4010ac-34a1-4ae7-b4b4-ec021ede5a7f", "anything"),
            Some(1),
            "a missed row that turns out read is read"
        );
        assert_eq!(
            pick(
                &rows,
                "5a71f239-4d5d-4222-9a35-577682031d81",
                "fix the build"
            ),
            None,
            "a row with an id pairs by the id only"
        );
    }

    #[test]
    fn a_typed_echo_takes_the_oldest_waiting_row_with_its_text() {
        let rows = [
            row("a", ItemStatus::Pending, json!({"text": "yes"})),
            row("b", ItemStatus::Pending, json!({"text": "deploy it"})),
            row("c", ItemStatus::Pending, json!({"text": "yes"})),
        ];
        assert_eq!(pick(&rows, "d3f0", "yes"), Some(0));
        assert_eq!(pick(&rows, "d3f1", "  deploy\n it "), Some(1));
        assert_eq!(
            pick(&rows, "d3f2", "no"),
            None,
            "no match: a row of its own"
        );
        assert_eq!(pick(&rows, "d3f3", ""), None);
    }

    #[test]
    fn a_waiting_row_pairs_before_a_missed_one_and_a_late_echo_heals_a_missed_one() {
        let rows = [
            row("old", ItemStatus::Failed, json!({"text": "yes"})),
            row("new", ItemStatus::Pending, json!({"text": "yes"})),
        ];
        assert_eq!(pick(&rows, "e1", "yes"), Some(1));
        assert_eq!(pick(&rows[..1], "e2", "yes"), Some(0));
    }

    #[test]
    fn a_picture_alone_pairs_by_its_file_name() {
        let sent = "Attached: /home/u/.local/state/femme/uploads/01JC/shot.png";
        let rows = [
            row("a", ItemStatus::Pending, json!({ "text": sent })),
            row(
                "b",
                ItemStatus::Pending,
                json!({ "text": "Attached: /x/other.png" }),
            ),
        ];
        assert_eq!(
            pick(&rows, "d1", "[Image #1]\n\nAttached: /tmp/shot.png"),
            Some(0)
        );
        assert_eq!(pick(&rows, "d2", "Attached: /tmp/third.png"), None);
        assert_eq!(
            pick(&rows, "d3", "[Image #1]"),
            None,
            "no words, no file: unsure"
        );
        assert!(!same_message("", ""));
    }

    #[test]
    fn a_missed_pair_is_made_at_start() {
        let (store, sid) = store();
        let saved = Item {
            sid,
            item_id: "01SAVED".into(),
            seq: 0,
            rev: 0,
            kind: ItemKind::UserMessage,
            status: ItemStatus::Failed,
            preview: "a".into(),
            body: Some(json!({"op": "01OP", "text": "o things to decide"}).to_string()),
            blob_hash: None,
            parent: None,
            ts: 10,
        };
        let echo = Item {
            item_id: "uuid-echo".into(),
            status: ItemStatus::Completed,
            body: Some(json!({"text": "<pasted_content id=\"1\">\no things to decide\n</pasted_content id=\"1\">"}).to_string()),
            ts: 11,
            ..saved.clone()
        };
        let other = Item {
            item_id: "uuid-other".into(),
            status: ItemStatus::Completed,
            body: Some(json!({"text": "something else"}).to_string()),
            ts: 12,
            ..saved.clone()
        };
        store
            .write(|tx| {
                tx.put_item(&saved)?;
                tx.put_item(&other)?;
                tx.put_item(&echo)?;
                Ok(())
            })
            .unwrap();
        assert_eq!(repair(&store).unwrap(), 1);
        assert_eq!(repair(&store).unwrap(), 0, "once");
        let get = |id: &str| {
            store
                .read(|c| store_items::get_item(c, sid, id))
                .unwrap()
                .unwrap()
        };
        let row = get("01SAVED");
        assert_eq!(row.status, ItemStatus::Completed);
        let b: Value = serde_json::from_str(row.body.as_deref().unwrap()).unwrap();
        assert_eq!(b["read_at"], 11);
        let mark = get("uuid-echo");
        assert_eq!(mark.kind, ItemKind::Event);
        assert!(is_read_mark(&mark));
        assert_eq!(get("uuid-other").kind, ItemKind::UserMessage);
    }

    #[test]
    fn attachment_lines_and_image_marks_do_not_count() {
        let sent = "look at this\n\nAttached: /home/u/.local/state/femme/uploads/01JC/shot.png";
        let echo = "[Image #1] look at this\n\nAttached: /home/u/.local/state/femme/uploads/01JC/paste-3fa2c1.png";
        assert_eq!(normalize(sent), "look at this");
        assert_eq!(normalize(echo), "look at this");
        let rows = [row("a", ItemStatus::Pending, json!({ "text": sent }))];
        assert_eq!(pick(&rows, "d9", echo), Some(0));
        assert_eq!(normalize("a [Image #x] b"), "a [Image #x] b");
        assert_eq!(normalize("Attached: /a.png"), "");
        // what Claude's terminal wrote for a paste into a herdr pane (from the box, 2026-11-09)
        let pasted = "\n\n<pasted_content id=\"b150\">\nlets restart with a blue squirrel\nMerhaba x!\n</pasted_content>";
        assert_eq!(
            normalize(pasted),
            normalize("lets restart with a blue squirrel\nMerhaba x!")
        );
        // the three shapes that did not pair on the box (2026-11-10)
        let closed = "\n\n<pasted_content id=\"23c2\">\no things to decide\n\nWhere the kid lands\n</pasted_content id=\"23c2\">";
        assert!(same_message(
            "o things to decide\n\nWhere the kid lands",
            closed
        ));
        let saved = "where is it? did you use femmed cli?\n\nAttached: /u/paste-3-4.png";
        let echo =
            "where is it? did you use femmed cli?\nAttached:\n\nAttached: /u/paste-304d7f.jpg";
        assert!(
            same_message(saved, echo),
            "the terminal took the file as a picture"
        );
        let saved = "i talked with someone about onboarding data\n\nAttached: /u/p.png";
        let echo = "thi talked with someone about onboarding data\nAttached:\n\nAttached: /u/q.jpg";
        assert!(same_message(saved, echo), "`th` typed into the pane before");
        assert!(
            !same_message("ok", "ok great, what now"),
            "short: only the same"
        );
        assert!(!same_message(
            "fix the squirrel head",
            "fix the squirrel head too"
        ));
        assert!(!same_message(
            "x".repeat(40).as_str(),
            &format!("{}{}", "y".repeat(40), "x".repeat(40))
        ));
    }

    use femme_store::Store;

    fn store() -> (Store, Sid) {
        let store = Store::open_memory().unwrap();
        let s = Session {
            id: Sid::default(),
            handle: String::new(),
            rev: 0,
            name: None,
            owner: "board".into(),
            agent: Agent::Claude,
            mode: Mode::Headless,
            cwd: "/".into(),
            label: None,
            agent_sid: None,
            model: None,
            status: femme_proto::state::SessionStatus::Working,
            attention: femme_proto::state::Attention::None,
            created: 0,
            closed: None,
            permission: None,
            context: None,
            effort: None,
            goal: None,
        };
        let sid = store.write(|tx| Ok(tx.insert_session(&s)?.id)).unwrap();
        (store, sid)
    }

    fn echo(sid: Sid, id: &str, text: &str, ts: i64) -> Item {
        let mut i = saved_item(sid, text, None, None, ts);
        i.item_id = id.into();
        i.status = ItemStatus::Completed;
        i.body = Some(json!({ "text": text }).to_string());
        i
    }

    fn get(store: &Store, sid: Sid, id: &str) -> Item {
        store
            .read(|c| store_items::get_item(c, sid, id))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_paired_echo_reads_the_row_and_marks_where() {
        let (store, sid) = store();
        let saved = saved_item(sid, "fix it", None, Some("u-1"), 1_000);
        let id = saved.item_id.clone();
        store.write(|tx| tx.put_item(&saved)).unwrap();
        let mut published = Vec::new();
        let paired = store
            .write(|tx| reconcile(tx, sid, &echo(sid, "u-1", "fix it", 5_000), &mut published))
            .unwrap();
        assert!(paired);
        let row = get(&store, sid, &id);
        assert_eq!(row.status, ItemStatus::Completed);
        let b: Value = serde_json::from_str(row.body.as_deref().unwrap()).unwrap();
        assert_eq!(b["read_at"], 5_000);
        let mark = get(&store, sid, "u-1");
        assert!(is_read_mark(&mark));
        assert!(
            mark.seq > row.seq,
            "the mark is where it was read, the row stays"
        );
        assert_eq!(published.len(), 2);
        // the same echo once more (a replay) is a stored mark: `put` leaves it be
        let unknown = store
            .write(|tx| reconcile(tx, sid, &echo(sid, "u-2", "hello", 6_000), &mut Vec::new()))
            .unwrap();
        assert!(!unknown, "an echo nobody saved is a row of its own");
    }

    fn put(store: &Store, i: &Item) {
        store.write(|tx| tx.put_item(i)).unwrap();
    }

    fn settled(store: &Store, sid: Sid, g: Gone) {
        store
            .write(|tx| settle(tx, sid, &g, &mut Vec::new()))
            .unwrap();
    }

    fn pair(store: &Store, sid: Sid, e: &Item) -> bool {
        store
            .write(|tx| reconcile(tx, sid, e, &mut Vec::new()))
            .unwrap()
    }

    /// Codex runs held prompts one turn at a time: going idle in between, the second is still
    /// held, so it waits; both end read, each once.
    #[test]
    fn two_queued_messages_on_an_agent_without_ids_both_end_read() {
        let (store, sid) = store();
        let one = saved_item(sid, "run the tests", None, None, 1_000);
        let two = saved_item(sid, "then the linter", None, None, 1_100);
        put(&store, &one);
        put(&store, &two);
        assert!(pair(&store, sid, &echo(sid, "i-1", "run the tests", 2_000)));
        settled(
            &store,
            sid,
            Gone::Idle(vec![(None, "then the linter".into())]),
        );
        assert_eq!(get(&store, sid, &two.item_id).status, ItemStatus::Pending);
        assert!(pair(
            &store,
            sid,
            &echo(sid, "i-2", "then the linter", 3_000)
        ));
        settled(&store, sid, Gone::Idle(Vec::new()));
        for id in [&one.item_id, &two.item_id] {
            assert_eq!(get(&store, sid, id).status, ItemStatus::Completed);
        }
        let all = store
            .read(|c| store_items::page_items(c, sid, None, 10))
            .unwrap();
        assert_eq!(all.len(), 4, "two messages, two read marks: {all:?}");
    }

    /// Idle with a message nobody holds: not got. Its echo coming later still reads it.
    #[test]
    fn idle_without_the_echo_misses_a_message_and_a_late_echo_reads_it() {
        let (store, sid) = store();
        let m = saved_item(sid, "deploy", None, Some("u-9"), 1_000);
        put(&store, &m);
        settled(&store, sid, Gone::Idle(Vec::new()));
        assert_eq!(get(&store, sid, &m.item_id).status, ItemStatus::Failed);
        assert!(pair(&store, sid, &echo(sid, "u-9", "deploy", 5_000)));
        assert_eq!(get(&store, sid, &m.item_id).status, ItemStatus::Completed);
    }

    /// A resend while the agent is busy is held by the driver: going idle does not miss it.
    #[test]
    fn a_resent_message_the_driver_holds_still_waits_when_the_agent_goes_idle() {
        let (store, sid) = store();
        let mut m = saved_item(sid, "again", None, Some("u-old"), 1_000);
        m.status = ItemStatus::Failed;
        put(&store, &m);
        // what Daemon::resend writes: waiting, with the new echo id
        let mut again = get(&store, sid, &m.item_id);
        again.status = ItemStatus::Pending;
        again.body = Some(json!({"text": "again", "echo": "u-new", "sent_at": 9_000}).to_string());
        put(&store, &again);
        settled(
            &store,
            sid,
            Gone::Idle(vec![(Some("u-new".into()), "again".into())]),
        );
        assert_eq!(get(&store, sid, &m.item_id).status, ItemStatus::Pending);
        settled(
            &store,
            sid,
            Gone::Prompts(vec![(Some("u-new".into()), "again".into())]),
        );
        assert_eq!(get(&store, sid, &m.item_id).status, ItemStatus::Failed);
    }

    #[test]
    fn a_refused_prompt_without_an_id_and_a_failed_session_miss_their_messages() {
        let (store, sid) = store();
        let a = saved_item(sid, "one", None, None, 1_000);
        let b = saved_item(sid, "two", None, None, 1_100);
        put(&store, &a);
        put(&store, &b);
        settled(&store, sid, Gone::Prompts(vec![(None, "two".into())]));
        assert_eq!(get(&store, sid, &a.item_id).status, ItemStatus::Pending);
        assert_eq!(get(&store, sid, &b.item_id).status, ItemStatus::Failed);
        settled(&store, sid, Gone::All);
        assert_eq!(get(&store, sid, &a.item_id).status, ItemStatus::Failed);
    }

    /// A send that failed takes its row back, unless an echo paired with it first.
    #[test]
    fn taking_back_never_deletes_a_message_the_agent_read() {
        let (store, sid) = store();
        let a = saved_item(sid, "hi", None, None, 1_000);
        let b = saved_item(sid, "typed too", None, None, 1_100);
        put(&store, &a);
        put(&store, &b);
        assert!(pair(&store, sid, &echo(sid, "d7", "typed too", 2_000)));
        assert!(!store.write(|tx| take_back(tx, sid, &b.item_id)).unwrap());
        assert_eq!(get(&store, sid, &b.item_id).status, ItemStatus::Completed);
        assert!(store.write(|tx| take_back(tx, sid, &a.item_id)).unwrap());
        assert!(store
            .read(|c| store_items::get_item(c, sid, &a.item_id))
            .unwrap()
            .is_none());
    }
}
