//! The change feed behind `Subscribe`: "everything newer than rev N".
//!
//! [`changes_since`] must run on one snapshot of the database; [`Store::changes_since`](crate::Store::changes_since) and
//! [`Store::read`](crate::Store::read) provide that. Call it after a commit, not inside a write
//! closure: `meta.rev` is only written at commit, so there it would report the last committed
//! rev while listing the transaction's own rows, and those rows would come again after the
//! commit (debug builds panic on it).
//!
//! # What a cursor means
//!
//! A client keeps `rev`, the cursor of the last response it applied, per scope, and asks for
//! `changes_since(scope, Some(rev), limit)`. The answer lists, ordered by `rev`, every row of
//! that scope changed after the cursor, tombstones included:
//!
//! - [`Scope::Global`]: sessions (closed ones until retention purges them at 30 days), panes and
//!   issues, plus the tombstones of those three.
//! - [`Scope::Session`]: that session's items, plus its item tombstones. A session that does not
//!   exist is `NotFound`.
//!
//! The byte budget `limit_bytes` is checked against the postcard size of each row; when the next
//! row would pass it the answer stops with `more = true`, and `rev` is the rev of the last row
//! included: ask again from there. At least one row is always returned, however big. When
//! `more = false`, `rev` is the database's current rev as of the snapshot, so the client's
//! cursor also moves past revs spent on other scopes.
//!
//! # Full sets and `reset`
//!
//! With `after = None` the answer is the *full set*: every live row of the scope, no tombstones,
//! in one piece (`more = false`, `limit_bytes` is ignored; `femmed` chunks it for the bulk
//! stream if it is big). For [`Scope::Session`] the full set is just the newest
//! [`INITIAL_ITEMS`] items (ascending `seq`); the client pages older ones with
//! [`items::page_items`](crate::items::page_items).
//!
//! The same full set, with `reset = true`, answers a cursor that cannot be trusted: older than
//! the tombstone horizon (retention has deleted tombstones the client never saw, so it may be
//! holding rows that were deleted) or newer than the database's rev (a restored backup). The
//! client must discard what it holds for that scope before applying the rows.
//!
//! The second check is only a tripwire: once new writes push the restored database's rev past
//! the client's cursor, the cursor looks fine again and the client would silently miss the rows
//! in between. After restoring a backup call [`Store::rotate_epoch`](crate::Store::rotate_epoch),
//! which makes every client resync.
//!
//! The horizon is the highest rev among the tombstones retention has purged
//! (`meta.tomb_horizon`): a cursor `c` is trustworthy iff `c >= tomb_horizon`.

use femme_proto::rpc::Scope;
use femme_proto::state::Row as StateRow;
use femme_proto::Sid;
use rusqlite::{params, Connection, Row, Rows};

use crate::enums::*;
use crate::issues::{issue_cols, issue_from_row};
use crate::items::{item_cols, item_from_row};
use crate::sessions::{pane_cols, pane_from_row, session_cols, session_from_row};
use crate::{current_rev, enum_col, exists, id_blob, meta_get, wire_size, Error, Result};

/// How many of the newest items a session's full set holds.
pub const INITIAL_ITEMS: usize = 200;

/// The answer to [`changes_since`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changes {
    /// The rows, ordered by `rev` (a full set: unordered by `seq` only for sessions, see above).
    pub rows: Vec<StateRow>,
    /// The cursor to continue from: the last included rev when `more`, else the database rev.
    pub rev: u64,
    /// The byte budget stopped the list; ask again from `rev`.
    pub more: bool,
    /// The client's cursor was unusable: this is a full set and it must drop what it had.
    pub reset: bool,
}

/// See the [module docs](crate::sync). `after = None` asks for the full set.
pub fn changes_since(
    conn: &Connection,
    scope: Scope,
    after: Option<u64>,
    limit_bytes: usize,
) -> Result<Changes> {
    debug_assert!(
        !matches!(conn.transaction_state(None::<&str>), Ok(rusqlite::TransactionState::Write)),
        "changes_since inside a write closure reports the last committed rev: call it after the commit"
    );
    if let Scope::Session(sid) = scope {
        if !exists(conn, "SELECT 1 FROM sessions WHERE id = ?1", [id_blob(sid)])? {
            return Err(Error::NotFound);
        }
    }
    let now_rev = current_rev(conn)?;
    let horizon: u64 = meta_get(conn, "tomb_horizon")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let reset = after.is_some_and(|a| a < horizon || a > now_rev);
    let after = if reset { None } else { after };
    match (scope, after) {
        (Scope::Global, None) => global_full(conn, now_rev, reset),
        (Scope::Global, Some(a)) => global_since(conn, a, now_rev, limit_bytes),
        (Scope::Session(sid), None) => session_newest(conn, sid, now_rev, reset),
        (Scope::Session(sid), Some(a)) => session_since(conn, sid, a, now_rev, limit_bytes),
    }
}

type Decode = fn(&Row<'_>) -> Result<StateRow>;

/// One ordered stream of rows (a statement's result) with its next row decoded and waiting.
struct Source<'s> {
    rows: Rows<'s>,
    decode: Decode,
    head: Option<StateRow>,
}

impl<'s> Source<'s> {
    fn new(rows: Rows<'s>, decode: Decode) -> Self {
        Source {
            rows,
            decode,
            head: None,
        }
    }

    fn advance(&mut self) -> Result<()> {
        self.head = match self.rows.next()? {
            Some(row) => Some((self.decode)(row)?),
            None => None,
        };
        Ok(())
    }
}

/// Merges ordered streams into one list ordered by rev, stopping before `limit_bytes` (if any)
/// would be passed. Returns the rows and whether it stopped early.
fn merge(sources: &mut [Source<'_>], limit_bytes: Option<usize>) -> Result<(Vec<StateRow>, bool)> {
    for s in sources.iter_mut() {
        s.advance()?;
    }
    let (mut out, mut used) = (Vec::new(), 0usize);
    loop {
        let next = (0..sources.len())
            .filter_map(|i| sources[i].head.as_ref().map(|row| (row.rev(), i)))
            .min();
        let Some((_, i)) = next else {
            return Ok((out, false));
        };
        let size = wire_size(
            sources[i]
                .head
                .as_ref()
                .expect("head exists: it was just selected"),
        )?;
        if limit_bytes.is_some_and(|limit| !out.is_empty() && used + size > limit) {
            return Ok((out, true));
        }
        used += size;
        out.push(
            sources[i]
                .head
                .take()
                .expect("head exists: it was just selected"),
        );
        sources[i].advance()?;
    }
}

fn dec_session(r: &Row<'_>) -> Result<StateRow> {
    Ok(StateRow::Session(session_from_row(r)?))
}
fn dec_pane(r: &Row<'_>) -> Result<StateRow> {
    Ok(StateRow::Pane(pane_from_row(r)?))
}
fn dec_issue(r: &Row<'_>) -> Result<StateRow> {
    Ok(StateRow::Issue(issue_from_row(r)?))
}
fn dec_item(r: &Row<'_>) -> Result<StateRow> {
    Ok(StateRow::Item(item_from_row(r)?))
}
fn dec_tombstone(r: &Row<'_>) -> Result<StateRow> {
    Ok(StateRow::Tombstone {
        entity: enum_col(r, 0, entity_parse)?,
        id: r.get(1)?,
        rev: r.get(2)?,
    })
}

const SESSIONS_AFTER: &str = concat!(
    "SELECT ",
    session_cols!(),
    " FROM sessions WHERE rev > ?1 ORDER BY rev"
);
const PANES_AFTER: &str = concat!(
    "SELECT ",
    pane_cols!(),
    " FROM panes WHERE rev > ?1 ORDER BY rev"
);
const ISSUES_AFTER: &str = concat!(
    "SELECT ",
    issue_cols!(),
    " FROM issues WHERE rev > ?1 ORDER BY rev"
);
const GLOBAL_TOMBSTONES_AFTER: &str =
    "SELECT entity, id, rev FROM tombstones WHERE rev > ?1 AND entity IN ('session', 'pane', 'issue') ORDER BY rev";

/// Every live session, pane and issue.
fn global_full(conn: &Connection, now_rev: u64, reset: bool) -> Result<Changes> {
    let (mut a, mut b, mut c) = (
        conn.prepare_cached(SESSIONS_AFTER)?,
        conn.prepare_cached(PANES_AFTER)?,
        conn.prepare_cached(ISSUES_AFTER)?,
    );
    let mut sources = [
        Source::new(a.query([0u64])?, dec_session),
        Source::new(b.query([0u64])?, dec_pane),
        Source::new(c.query([0u64])?, dec_issue),
    ];
    let (rows, _) = merge(&mut sources, None)?;
    Ok(Changes {
        rows,
        rev: now_rev,
        more: false,
        reset,
    })
}

fn global_since(
    conn: &Connection,
    after: u64,
    now_rev: u64,
    limit_bytes: usize,
) -> Result<Changes> {
    let (mut a, mut b, mut c, mut d) = (
        conn.prepare_cached(SESSIONS_AFTER)?,
        conn.prepare_cached(PANES_AFTER)?,
        conn.prepare_cached(ISSUES_AFTER)?,
        conn.prepare_cached(GLOBAL_TOMBSTONES_AFTER)?,
    );
    let mut sources = [
        Source::new(a.query([after])?, dec_session),
        Source::new(b.query([after])?, dec_pane),
        Source::new(c.query([after])?, dec_issue),
        Source::new(d.query([after])?, dec_tombstone),
    ];
    let (rows, more) = merge(&mut sources, Some(limit_bytes))?;
    Ok(finish(rows, more, now_rev, false))
}

/// The newest [`INITIAL_ITEMS`] items of a session, oldest first.
fn session_newest(conn: &Connection, sid: Sid, now_rev: u64, reset: bool) -> Result<Changes> {
    let mut stmt = conn.prepare_cached(concat!(
        "SELECT ",
        item_cols!(),
        " FROM items WHERE session_id = ?1 ORDER BY seq DESC LIMIT ?2"
    ))?;
    let mut rows = stmt.query(params![id_blob(sid), INITIAL_ITEMS])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        items.push(StateRow::Item(item_from_row(row)?));
    }
    items.reverse();
    Ok(Changes {
        rows: items,
        rev: now_rev,
        more: false,
        reset,
    })
}

fn session_since(
    conn: &Connection,
    sid: Sid,
    after: u64,
    now_rev: u64,
    limit_bytes: usize,
) -> Result<Changes> {
    let mut items = conn.prepare_cached(concat!(
        "SELECT ",
        item_cols!(),
        " FROM items WHERE session_id = ?1 AND rev > ?2 ORDER BY rev"
    ))?;
    // Item tombstone ids are "<sid>/<item_id>": the range from "<sid>/" to "<sid>0" ('0' follows '/').
    let mut tombs = conn.prepare_cached(
        "SELECT entity, id, rev FROM tombstones WHERE entity = 'item' AND id >= ?1 AND id < ?2 AND rev > ?3 ORDER BY rev",
    )?;
    let mut sources = [
        Source::new(items.query(params![id_blob(sid), after])?, dec_item),
        Source::new(
            tombs.query(params![format!("{sid}/"), format!("{sid}0"), after])?,
            dec_tombstone,
        ),
    ];
    let (rows, more) = merge(&mut sources, Some(limit_bytes))?;
    Ok(finish(rows, more, now_rev, false))
}

fn finish(rows: Vec<StateRow>, more: bool, now_rev: u64, reset: bool) -> Changes {
    let rev = if more {
        rows.last().map_or(now_rev, StateRow::rev)
    } else {
        now_rev
    };
    Changes {
        rows,
        rev,
        more,
        reset,
    }
}
