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
//! The agent did not get a message (`Failed`, which a client offers to send again) when Claude
//! said so (a dropped prompt that is not sent to the next process), when the session moved to the
//! terminal or failed with it still held, and, for a row without an id, when a turn that began
//! after the row was saved ended without its echo.

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

/// How long before a turn began a row must have been saved for that turn to have had it. A turn's
/// start is its end less its duration, both measured a little after the fact.
const SLACK_MS: i64 = 2000;

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

/// A message's text for comparing: without the `Attached:` lines the daemon adds and the
/// `[Image #N]` marks Claude's terminal adds, every run of spaces and newlines one space.
pub(crate) fn normalize(text: &str) -> String {
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("Attached: "))
        .collect();
    let mut s = kept.join("\n");
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

/// Which of the waiting rows (oldest first, with their bodies) the echo `echo_id` saying
/// `echo_text` is: the row with that `echo` id, else the oldest `Pending` row without an id whose
/// text is the echo's. `None` when none is sure.
pub(crate) fn pick(rows: &[(Item, Value)], echo_id: &str, echo_text: &str) -> Option<usize> {
    if let Some(i) = rows
        .iter()
        .position(|(_, b)| b["echo"].as_str() == Some(echo_id))
    {
        return Some(i);
    }
    let said = normalize(echo_text);
    if said.is_empty() {
        return None;
    }
    rows.iter().position(|(i, b)| {
        i.status == ItemStatus::Pending && b.get("echo").is_none() && normalize(text_of(b)) == said
    })
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

/// The agent will not get the messages written under the echo ids `ids`: their rows are `Failed`.
pub(crate) fn missed(
    tx: &mut Tx<'_>,
    sid: Sid,
    ids: &[String],
    published: &mut Vec<Item>,
) -> femme_store::Result<()> {
    for (row, b) in waiting(tx, sid, &[ItemStatus::Pending])? {
        if b["echo"]
            .as_str()
            .is_some_and(|e| ids.iter().any(|i| i == e))
        {
            rewrite(tx, row, ItemStatus::Failed, &b, published)?;
        }
    }
    Ok(())
}

/// A turn ended (`end`, a new `TurnEnd`): a row without an echo id that was saved before the turn
/// began and is still not read was not got. A turn that says no duration says nothing.
pub(crate) fn missed_by_turn(
    tx: &mut Tx<'_>,
    sid: Sid,
    end: &Item,
    published: &mut Vec<Item>,
) -> femme_store::Result<()> {
    let took = end
        .body
        .as_deref()
        .and_then(|b| serde_json::from_str::<Value>(b).ok())
        .and_then(|b| b["duration_ms"].as_i64());
    let Some(took) = took else {
        return Ok(());
    };
    let began = end.ts - took;
    for (row, b) in waiting(tx, sid, &[ItemStatus::Pending])? {
        if b.get("echo").is_none() && row.ts + SLACK_MS < began {
            rewrite(tx, row, ItemStatus::Failed, &b, published)?;
        }
    }
    Ok(())
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
    /// client keeps the text to send again).
    pub(crate) fn unsave(&self, sid: Sid, item_id: &str) {
        let r = (|| -> femme_store::Result<()> {
            let before = self.store.rev()?;
            self.store.write(|tx| tx.delete_item(sid, item_id))?;
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

    /// `Req::Resend`: the message `item_id` the agent did not get waits again and goes to it
    /// again, after the turn that runs. `Conflict` for any other item.
    pub(crate) async fn resend(&self, sid: Sid, item_id: &str) -> Result<(), CoreError> {
        let s = self.session(sid)?;
        let head = self.sessions.head(sid).ok_or_else(gone)?;
        let echo = echo_for(&s);
        let flip = |status: ItemStatus, echo: Option<&str>| {
            self.write_items(|tx, _, published| {
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
    fn a_missed_row_without_an_id_is_not_paired_by_its_text() {
        let rows = [
            row("old", ItemStatus::Failed, json!({"text": "yes"})),
            row("new", ItemStatus::Pending, json!({"text": "yes"})),
        ];
        assert_eq!(pick(&rows, "e1", "yes"), Some(1));
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

    #[test]
    fn a_turn_that_began_after_the_save_and_ended_without_the_echo_missed_it() {
        let (store, sid) = store();
        let early = saved_item(sid, "one", None, None, 1_000);
        let late = saved_item(sid, "two", None, None, 9_000);
        let claude = saved_item(sid, "three", None, Some("u-3"), 1_000);
        for i in [&early, &late, &claude] {
            store.write(|tx| tx.put_item(i)).unwrap();
        }
        let mut end = echo(sid, "d900-1", "", 20_000);
        end.kind = ItemKind::TurnEnd;
        end.body =
            Some(json!({"stop": "EndTurn", "usage": null, "duration_ms": 12_000}).to_string());
        store
            .write(|tx| missed_by_turn(tx, sid, &end, &mut Vec::new()))
            .unwrap();
        assert_eq!(get(&store, sid, &early.item_id).status, ItemStatus::Failed);
        assert_eq!(
            get(&store, sid, &late.item_id).status,
            ItemStatus::Pending,
            "saved while the turn ran: the next one may take it"
        );
        assert_eq!(
            get(&store, sid, &claude.item_id).status,
            ItemStatus::Pending,
            "a row with an id waits for Claude's own word"
        );
        store
            .write(|tx| missed(tx, sid, &["u-3".into()], &mut Vec::new()))
            .unwrap();
        assert_eq!(get(&store, sid, &claude.item_id).status, ItemStatus::Failed);
    }
}
