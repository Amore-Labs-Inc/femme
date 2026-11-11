//! Sessions and their panes.
//!
//! A session row carries the replicated fields of [`Session`] plus `spool_committed`, a number
//! the daemon keeps for itself (how much of the agent's spool file is already in the transcript);
//! it is not replicated and changing it does not bump `rev`.
//!
//! Closing a session keeps its row (clients show it for 30 days, see [`crate::retain`]) and
//! drops its panes with a tombstone each. Deleting one removes everything and leaves tombstones
//! for the session and its panes; the session tombstone also means "forget its items", so items
//! get no tombstones of their own. Issues that pointed at the session keep a dangling
//! `session` id: clear it with [`Tx::update_issue`](crate::Tx::update_issue) if that matters.

use femme_proto::state::{Context, Entity, Goal, Pane, Session, SessionStatus};
use femme_proto::{Id, PaneId, Sid};
use rusqlite::{params, Connection, Row};

use crate::enums::*;
use crate::{all, blob_id, enum_col, exists, id_blob, one, Error, Result, Tx};

macro_rules! session_cols {
    () => {
        "id, handle, rev, name, owner, agent, mode, cwd, label, agent_sid, model, status, attention, created, closed, permission, ctx_used, ctx_window, compactions, effort, goal, ctx_compact_at"
    };
}
macro_rules! pane_cols {
    () => {
        "session_id, no, rev, kind, title"
    };
}
pub(crate) use pane_cols;
pub(crate) use session_cols;

pub(crate) fn session_from_row(r: &Row<'_>) -> Result<Session> {
    Ok(Session {
        id: blob_id(r.get(0)?),
        handle: r.get(1)?,
        rev: r.get(2)?,
        name: r.get(3)?,
        owner: r.get(4)?,
        agent: enum_col(r, 5, agent_parse)?,
        mode: enum_col(r, 6, mode_parse)?,
        cwd: r.get(7)?,
        label: r.get(8)?,
        agent_sid: r.get(9)?,
        model: r.get(10)?,
        status: enum_col(r, 11, session_status_parse)?,
        attention: enum_col(r, 12, attention_parse)?,
        created: r.get(13)?,
        closed: r.get(14)?,
        permission: r.get(15)?,
        context: context_cols(r.get(16)?, r.get(17)?, r.get(18)?, r.get(21)?),
        effort: r.get(19)?,
        goal: goal_col(r.get(20)?),
    })
}

/// The `goal` column (JSON) as a `Goal`; a value this build cannot read counts as none.
fn goal_col(json: Option<String>) -> Option<Goal> {
    serde_json::from_str(&json?).ok()
}

/// A `Goal` for the `goal` column.
fn goal_json(g: Option<&Goal>) -> Option<String> {
    g.map(|g| serde_json::to_string(g).unwrap_or_default())
}

/// Four nullable columns make one `Context`: NULL `ctx_used` means the agent never said.
fn context_cols(
    used: Option<i64>,
    window: Option<i64>,
    compactions: Option<i64>,
    compact_at: Option<i64>,
) -> Option<Context> {
    Some(Context {
        used: used? as u64,
        window: window.unwrap_or(0) as u64,
        compactions: compactions.unwrap_or(0) as u32,
        compact_at: compact_at.map(|n| n as u64),
    })
}

pub(crate) fn pane_from_row(r: &Row<'_>) -> Result<Pane> {
    Ok(Pane {
        id: PaneId {
            sid: blob_id(r.get(0)?),
            no: r.get(1)?,
        },
        rev: r.get(2)?,
        kind: enum_col(r, 3, pane_kind_parse)?,
        title: r.get(4)?,
    })
}

/// The tombstone id of a pane: `<session ULID>:<pane no>`.
pub(crate) fn pane_key(id: PaneId) -> String {
    format!("{}:{}", id.sid, id.no)
}

/// The session with this id, open or closed.
pub fn get_session(conn: &Connection, id: Sid) -> Result<Option<Session>> {
    one(
        conn,
        concat!("SELECT ", session_cols!(), " FROM sessions WHERE id = ?1"),
        [id_blob(id)],
        session_from_row,
    )
}

/// The session with this handle (the short id inside compat pane ids, `w<handle>:p<no>`).
pub fn session_by_handle(conn: &Connection, handle: &str) -> Result<Option<Session>> {
    one(
        conn,
        concat!(
            "SELECT ",
            session_cols!(),
            " FROM sessions WHERE handle = ?1"
        ),
        [handle],
        session_from_row,
    )
}

/// The *open* session called `name` (`/api/agents/{name}`). Closed sessions keep their name for
/// display but do not hold it, so at most one session answers.
pub fn session_by_name(conn: &Connection, name: &str) -> Result<Option<Session>> {
    one(
        conn,
        concat!(
            "SELECT ",
            session_cols!(),
            " FROM sessions WHERE name = ?1 AND closed IS NULL"
        ),
        [name],
        session_from_row,
    )
}

/// Sessions, oldest first. With `include_closed = false`, only those without a `closed` time.
pub fn list_sessions(conn: &Connection, include_closed: bool) -> Result<Vec<Session>> {
    let sql = if include_closed {
        concat!(
            "SELECT ",
            session_cols!(),
            " FROM sessions ORDER BY created, id"
        )
    } else {
        concat!(
            "SELECT ",
            session_cols!(),
            " FROM sessions WHERE closed IS NULL ORDER BY created, id"
        )
    };
    all(conn, sql, [], session_from_row)
}

/// How far into the agent's spool file the transcript is committed (bytes). `NotFound` if the
/// session does not exist.
pub fn spool_committed(conn: &Connection, id: Sid) -> Result<u64> {
    one(
        conn,
        "SELECT spool_committed FROM sessions WHERE id = ?1",
        [id_blob(id)],
        |r| Ok(r.get(0)?),
    )?
    .ok_or(Error::NotFound)
}

/// The attach state of a headless session (`epoch`, opaque `json`), if [`Tx::set_agent_state`]
/// ever stored one.
pub fn agent_state(conn: &Connection, id: Sid) -> Result<Option<(u64, String)>> {
    one(
        conn,
        "SELECT epoch, json FROM agent_state WHERE session_id = ?1",
        [id_blob(id)],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
}

/// The panes of a session, by pane number.
pub fn panes(conn: &Connection, sid: Sid) -> Result<Vec<Pane>> {
    all(
        conn,
        concat!(
            "SELECT ",
            pane_cols!(),
            " FROM panes WHERE session_id = ?1 ORDER BY no"
        ),
        [id_blob(sid)],
        pane_from_row,
    )
}

/// One pane.
pub fn get_pane(conn: &Connection, id: PaneId) -> Result<Option<Pane>> {
    one(
        conn,
        concat!(
            "SELECT ",
            pane_cols!(),
            " FROM panes WHERE session_id = ?1 AND no = ?2"
        ),
        params![id_blob(id.sid), id.no],
        pane_from_row,
    )
}

/// Is `name` held by an open session other than `except`?
fn name_taken(conn: &Connection, name: &str, except: Sid) -> Result<bool> {
    exists(
        conn,
        "SELECT 1 FROM sessions WHERE name = ?1 AND closed IS NULL AND id != ?2",
        params![name, id_blob(except)],
    )
}

/// Six random characters from `[a-z0-9]`.
fn random_handle() -> String {
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    // The low 64 bits of a ULID are random.
    let mut n = Id::new().0 as u64;
    (0..6)
        .map(|_| {
            let c = ALPHABET[(n % 36) as usize] as char;
            n /= 36;
            c
        })
        .collect()
}

impl Tx<'_> {
    /// A handle no session has: 6 characters from `[a-z0-9]` (the compat layer mints pane ids
    /// as `w<handle>:p<no>`, which fits the OpenAPI pattern `w[A-Za-z0-9]+:p[A-Za-z0-9]+`).
    pub fn new_handle(&mut self) -> Result<String> {
        for _ in 0..32 {
            let handle = random_handle();
            if !exists(self, "SELECT 1 FROM sessions WHERE handle = ?1", [&handle])? {
                return Ok(handle);
            }
        }
        Err(Error::Corrupt(
            "no free session handle after 32 tries".into(),
        ))
    }

    /// Inserts a session and returns it as stored. `s.rev` is ignored (a fresh rev is assigned);
    /// a nil `s.id` gets a new ULID and an empty `s.handle` a new handle.
    ///
    /// `Exists("name")` if an open session already has `s.name`.
    pub fn insert_session(&mut self, s: &Session) -> Result<Session> {
        let mut s = s.clone();
        if s.id.is_nil() {
            s.id = Id::new();
        }
        if s.handle.is_empty() {
            s.handle = self.new_handle()?;
        }
        if let (Some(name), None) = (&s.name, s.closed) {
            if name_taken(self, name, s.id)? {
                return Err(Error::Exists("name"));
            }
        }
        s.rev = self.next_rev();
        self.prepare_cached(concat!(
            "INSERT INTO sessions (",
            session_cols!(),
            ") VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)"
        ))?
        .execute(params![
            id_blob(s.id),
            s.handle,
            s.rev,
            s.name,
            s.owner,
            agent_str(s.agent),
            mode_str(s.mode),
            s.cwd,
            s.label,
            s.agent_sid,
            s.model,
            session_status_str(s.status),
            attention_str(s.attention),
            s.created,
            s.closed,
            s.permission,
            s.context.map(|c| c.used as i64),
            s.context.map(|c| c.window as i64),
            s.context.map(|c| i64::from(c.compactions)),
            s.effort,
            goal_json(s.goal.as_ref()),
            s.context.and_then(|c| c.compact_at).map(|n| n as i64),
        ])?;
        self.clear_tombstone(Entity::Session, &s.id.to_string())?; // an explicit id may come back (restore)
        Ok(s)
    }

    /// Loads the session, lets `f` change it, stores it with a fresh rev and returns it. `id`,
    /// `handle` and `created` cannot be changed. If `f` changes nothing, nothing is written and
    /// the rev stays (so repeating a status update is free). `NotFound` if there is no such
    /// session; `Exists("name")` if `f` gives it the name of another open session.
    pub fn update_session(&mut self, id: Sid, f: impl FnOnce(&mut Session)) -> Result<Session> {
        let before = get_session(self, id)?.ok_or(Error::NotFound)?;
        let mut after = before.clone();
        f(&mut after);
        after.id = before.id;
        after.handle = before.handle.clone();
        after.created = before.created;
        after.rev = before.rev;
        if after == before {
            return Ok(before);
        }
        if let (Some(name), None) = (&after.name, after.closed) {
            if (after.name != before.name || before.closed.is_some()) && name_taken(self, name, id)?
            {
                return Err(Error::Exists("name"));
            }
        }
        after.rev = self.next_rev();
        self.prepare_cached(
            "UPDATE sessions SET rev = ?2, name = ?3, owner = ?4, agent = ?5, mode = ?6, cwd = ?7, label = ?8,
                    agent_sid = ?9, model = ?10, status = ?11, attention = ?12, closed = ?13,
                    permission = ?14, ctx_used = ?15, ctx_window = ?16, compactions = ?17, effort = ?18, goal = ?19, ctx_compact_at = ?20
             WHERE id = ?1",
        )?
        .execute(params![
            id_blob(id),
            after.rev,
            after.name,
            after.owner,
            agent_str(after.agent),
            mode_str(after.mode),
            after.cwd,
            after.label,
            after.agent_sid,
            after.model,
            session_status_str(after.status),
            attention_str(after.attention),
            after.closed,
            after.permission,
            after.context.map(|c| c.used as i64),
            after.context.map(|c| c.window as i64),
            after.context.map(|c| i64::from(c.compactions)),
            after.effort,
            goal_json(after.goal.as_ref()),
            after.context.and_then(|c| c.compact_at).map(|n| n as i64),
        ])?;
        Ok(after)
    }

    /// Closes a session: status `Closed`, `closed = now`, its panes deleted with a tombstone
    /// each. The row stays (retention purges it 30 days later). Closing a closed session is a
    /// no-op that returns it. Other fields (`attention`, ..) are left as they are.
    pub fn close_session(&mut self, id: Sid, now: i64) -> Result<Session> {
        let s = get_session(self, id)?.ok_or(Error::NotFound)?;
        if s.status == SessionStatus::Closed && s.closed.is_some() {
            return Ok(s);
        }
        let dropped = panes(self, id)?;
        self.execute("DELETE FROM panes WHERE session_id = ?1", [id_blob(id)])?;
        for p in &dropped {
            self.tombstone(Entity::Pane, &pane_key(p.id))?;
        }
        self.update_session(id, |s| {
            s.status = SessionStatus::Closed;
            s.closed = Some(now);
        })
    }

    /// Deletes a session with its panes and items; leaves tombstones for the session and each
    /// pane (see the module docs on why items get none). `NotFound` if it does not exist.
    pub fn delete_session(&mut self, id: Sid) -> Result<()> {
        if !exists(self, "SELECT 1 FROM sessions WHERE id = ?1", [id_blob(id)])? {
            return Err(Error::NotFound);
        }
        let dropped = panes(self, id)?;
        self.execute("DELETE FROM sessions WHERE id = ?1", [id_blob(id)])?; // cascades to panes and items
        self.tombstone(Entity::Session, &id.to_string())?;
        for p in &dropped {
            self.tombstone(Entity::Pane, &pane_key(p.id))?;
        }
        self.drop_item_tombstones(id)
    }

    /// Removes the item tombstones of a session that no longer exists.
    pub(crate) fn drop_item_tombstones(&mut self, sid: Sid) -> Result<()> {
        // ids are "<sid>/<item_id>": everything from "<sid>/" up to "<sid>0" ('0' follows '/').
        let (lo, hi) = (format!("{sid}/"), format!("{sid}0"));
        self.execute(
            "DELETE FROM tombstones WHERE entity = 'item' AND id >= ?1 AND id < ?2",
            (lo, hi),
        )?;
        Ok(())
    }

    /// Records how far into the agent's spool file the transcript is committed. Not replicated:
    /// no rev change. Write it in the same transaction as the items it covers.
    pub fn set_spool_committed(&mut self, id: Sid, offset: u64) -> Result<()> {
        let n = self.execute(
            "UPDATE sessions SET spool_committed = ?2 WHERE id = ?1",
            params![id_blob(id), offset],
        )?;
        if n == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Stores the attach state of a headless session: the attach `epoch` and the driver's pending
    /// answers and held prompts as `json` (opaque to the store). Not replicated: no rev change.
    /// Write it in the same transaction as the items it goes with. `NotFound` if the session does
    /// not exist.
    pub fn set_agent_state(&mut self, id: Sid, epoch: u64, json: &str) -> Result<()> {
        self.prepare_cached(
            "INSERT INTO agent_state (session_id, epoch, json) VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET epoch = excluded.epoch, json = excluded.json",
        )?
        .execute(params![id_blob(id), epoch, json])?;
        Ok(())
    }

    /// Creates or updates a pane (`kind` and `title`) and returns it as stored. If nothing
    /// changed, nothing is written and the existing rev is returned. `NotFound` if the session
    /// does not exist.
    pub fn upsert_pane(&mut self, p: &Pane) -> Result<Pane> {
        if let Some(old) = get_pane(self, p.id)? {
            if old.kind == p.kind && old.title == p.title {
                return Ok(old);
            }
        }
        let rev = self.next_rev();
        self.prepare_cached(concat!(
            "INSERT INTO panes (", pane_cols!(), ") VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(session_id, no) DO UPDATE SET rev = excluded.rev, kind = excluded.kind, title = excluded.title"
        ))?
        .execute(params![id_blob(p.id.sid), p.id.no, rev, pane_kind_str(p.kind), p.title])?;
        self.clear_tombstone(Entity::Pane, &pane_key(p.id))?; // a pane number can be reused
        Ok(Pane { rev, ..p.clone() })
    }

    /// Deletes a pane and leaves a tombstone. `NotFound` if there is no such pane.
    pub fn delete_pane(&mut self, id: PaneId) -> Result<()> {
        let n = self.execute(
            "DELETE FROM panes WHERE session_id = ?1 AND no = ?2",
            params![id_blob(id.sid), id.no],
        )?;
        if n == 0 {
            return Err(Error::NotFound);
        }
        self.tombstone(Entity::Pane, &pane_key(id))?;
        Ok(())
    }
}
