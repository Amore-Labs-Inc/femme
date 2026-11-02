//! Retention: the daily job that keeps the database bounded. [`Store::retain`] is meant to run
//! once a day (and at start-up); it is idempotent and cheap when there is nothing to do.
//!
//! What it deletes, and only this, in this order, each step in a write transaction of its own:
//!
//! 1. sessions closed more than 30 days ago, with their panes and items (a `Session` tombstone
//!    stays, so a client that still shows the session drops it);
//! 2. blobs no item refers to any more;
//! 3. tombstones older than 14 days. This is what makes old cursors untrustworthy: the largest
//!    rev among the tombstones just purged becomes `meta.tomb_horizon`, and
//!    [`changes_since`](crate::sync::changes_since) answers `reset` to any cursor below it;
//! 4. request-dedup rows ([`ops`](crate::Tx::op_record)) older than 24 hours;
//! 5. expired web sessions;
//! 6. the oldest blobs while the stored blob bytes exceed the cap (the items keep their preview
//!    and lose the full text; reading such a blob answers `None`; the items' revs do not change,
//!    so a client may keep a `blob_hash` whose blob is gone);
//! 7. free pages go back to the filesystem (incremental vacuum in chunks of 1000 pages, each in
//!    its own short write transaction, so writers are never held up for long) and the WAL is
//!    checkpointed and truncated, without waiting for readers.
//!
//! The caller deletes what the store does not own: [`RetainReport::purged_sessions`] lists the
//! sessions whose pane spool files must go.
//!
//! Issues, devices, API keys, keybindings and open sessions are never touched.

use std::time::Duration;

use femme_proto::Sid;

use crate::{all, blob_id, id_blob, meta_get, Result, Store, BUSY_TIMEOUT};

const DAY_MS: i64 = 86_400_000;
const VACUUM_CHUNK: u64 = 1000;

/// How much each kind of data is kept. [`Caps::default`] is what production uses; tests shrink it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caps {
    /// A closed session is purged this long after it was closed (default 30 days).
    pub closed_session_ttl_ms: i64,
    /// A tombstone is purged this long after it was written (default 14 days).
    pub tombstone_ttl_ms: i64,
    /// A request-dedup row is purged this long after it was written (default 24 hours).
    pub ops_ttl_ms: i64,
    /// Most bytes the `blobs` table may hold, measured as stored (zstd'd; default 512 MiB).
    pub blob_total_bytes: u64,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            closed_session_ttl_ms: 30 * DAY_MS,
            tombstone_ttl_ms: 14 * DAY_MS,
            ops_ttl_ms: DAY_MS,
            blob_total_bytes: 512 << 20,
        }
    }
}

/// What one retention run removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetainReport {
    /// Closed sessions purged.
    pub sessions: usize,
    /// Their ids. The caller owns files the store knows nothing about (the pane spools): it
    /// deletes them for exactly these sessions.
    pub purged_sessions: Vec<Sid>,
    /// Items that went with them.
    pub items: usize,
    /// Tombstones purged.
    pub tombstones: usize,
    /// Request-dedup rows purged.
    pub ops: usize,
    /// Expired web sessions purged.
    pub web_sessions: usize,
    /// Blobs removed because no item referred to them.
    pub orphan_blobs: usize,
    /// Blobs evicted because the cap was exceeded.
    pub evicted_blobs: usize,
    /// Free pages returned to the filesystem.
    pub pages_vacuumed: u64,
    /// Steps that failed, as `"<step>: <error>"`; log them. See [`Store::retain_with`].
    pub warnings: Vec<String>,
}

/// Records a failed step in `warnings` and answers `None`.
fn warn<T>(warnings: &mut Vec<String>, step: &str, result: Result<T>) -> Option<T> {
    result
        .map_err(|e| warnings.push(format!("{step}: {e}")))
        .ok()
}

impl Store {
    /// Runs retention with the default [`Caps`]; `now` is unix ms.
    pub fn retain(&self, now: i64) -> Result<RetainReport> {
        self.retain_with(now, &Caps::default())
    }

    /// Runs retention with explicit [`Caps`] (see the [module docs](self)). Every step is a
    /// write transaction of its own.
    ///
    /// Only the first step, purging the closed sessions, can make this return `Err`, and then
    /// nothing has changed. Once it has committed, `purged_sessions` must reach the caller (it
    /// has spool files to delete), so a failure of any later step is recorded in
    /// [`RetainReport::warnings`] instead, and the next run does that work again.
    pub fn retain_with(&self, now: i64, caps: &Caps) -> Result<RetainReport> {
        let mut rep = RetainReport::default();

        // 1. closed sessions. delete_session leaves the Session (and pane) tombstones.
        (rep.purged_sessions, rep.items) = self.write_at(now, |tx| {
            let old = all(
                tx,
                "SELECT id FROM sessions WHERE closed IS NOT NULL AND closed < ?1",
                [now.saturating_sub(caps.closed_session_ttl_ms)],
                |r| Ok(blob_id(r.get(0)?)),
            )?;
            let mut items = 0;
            for &sid in &old {
                let n: usize = tx.query_row(
                    "SELECT COUNT(*) FROM items WHERE session_id = ?1",
                    [id_blob(sid)],
                    |r| r.get(0),
                )?;
                items += n;
                tx.delete_session(sid)?;
            }
            Ok((old, items))
        })?;
        rep.sessions = rep.purged_sessions.len();

        // 2. blobs nothing refers to (items of purged sessions, replaced or deleted bodies)
        rep.orphan_blobs = warn(
            &mut rep.warnings,
            "orphan blobs",
            self.write_at(now, |tx| {
                Ok(tx.execute("DELETE FROM blobs WHERE NOT EXISTS (SELECT 1 FROM items WHERE items.blob_hash = blobs.hash)", [])?)
            }),
        )
        .unwrap_or(0);

        // 3. tombstones, and the horizon below which a cursor can no longer be trusted: one
        // transaction, so the horizon never lags behind what was deleted
        rep.tombstones = warn(
            &mut rep.warnings,
            "tombstones",
            self.write_at(now, |tx| {
                let cutoff = now.saturating_sub(caps.tombstone_ttl_ms);
                let newest: Option<u64> = tx.query_row(
                    "SELECT MAX(rev) FROM tombstones WHERE ts < ?1",
                    [cutoff],
                    |r| r.get(0),
                )?;
                let Some(newest) = newest else { return Ok(0) };
                let purged = tx.execute("DELETE FROM tombstones WHERE ts < ?1", [cutoff])?;
                let horizon: u64 = meta_get(tx, "tomb_horizon")?
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                if newest > horizon {
                    tx.meta_set("tomb_horizon", &newest.to_string())?;
                }
                Ok(purged)
            }),
        )
        .unwrap_or(0);

        // 4. + 5.
        rep.ops = warn(
            &mut rep.warnings,
            "ops",
            self.write_at(now, |tx| {
                Ok(tx.execute(
                    "DELETE FROM ops WHERE ts < ?1",
                    [now.saturating_sub(caps.ops_ttl_ms)],
                )?)
            }),
        )
        .unwrap_or(0);
        rep.web_sessions = warn(
            &mut rep.warnings,
            "web sessions",
            self.write_at(now, |tx| tx.web_sessions_purge(now)),
        )
        .unwrap_or(0);

        // 6. keep the newest blobs that fit the cap
        rep.evicted_blobs = warn(
            &mut rep.warnings,
            "blob cap",
            self.write_at(now, |tx| {
                let total: u64 = tx.query_row("SELECT COALESCE(SUM(length(data)), 0) FROM blobs", [], |r| r.get(0))?;
                if total <= caps.blob_total_bytes {
                    return Ok(0);
                }
                let evict = all(
                    tx,
                    "SELECT hash FROM (
                         SELECT hash, SUM(length(data)) OVER (ORDER BY ts DESC, hash ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running
                         FROM blobs
                     ) WHERE running > ?1",
                    [caps.blob_total_bytes],
                    |r| Ok(r.get::<_, [u8; 32]>(0)?),
                )?;
                for hash in &evict {
                    // The preview stays right, the full text is gone: a fresh rev per item, so a
                    // client that synced before the eviction stops offering it.
                    let items = all(tx, "SELECT session_id, item_id FROM items WHERE blob_hash = ?1", [hash], |r| {
                        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?))
                    })?;
                    for (sid, item_id) in items {
                        let rev = tx.next_rev();
                        tx.execute("UPDATE items SET blob_hash = NULL, rev = ?3 WHERE session_id = ?1 AND item_id = ?2", rusqlite::params![sid, item_id, rev])?;
                    }
                    tx.execute("DELETE FROM blobs WHERE hash = ?1", [hash])?;
                }
                Ok(evict.len())
            }),
        )
        .unwrap_or(0);

        // 7. incremental_vacuum frees one page per step of the statement: it must be stepped to
        // the end (execute() would stop after the first page).
        loop {
            let chunk = self.write_at(now, |tx| {
                let mut stmt = tx.prepare(&format!("PRAGMA incremental_vacuum({VACUUM_CHUNK})"))?;
                let mut rows = stmt.query([])?;
                let mut n = 0;
                while rows.next()?.is_some() {
                    n += 1;
                }
                Ok(n)
            });
            let Some(freed) = warn(&mut rep.warnings, "vacuum", chunk) else {
                break;
            };
            rep.pages_vacuumed += freed;
            if freed < VACUUM_CHUNK {
                break;
            }
        }

        // 8. Give the WAL back. TRUNCATE has to wait for every reader to finish, and this runs
        // under the writer lock: with the usual 5 s busy timeout one open reader would hold up
        // every writer for that long. So it gets no patience: what cannot be checkpointed now is
        // left for the next run.
        if !self.memory {
            let done = self.with_writer(|c| {
                c.busy_timeout(Duration::ZERO)?;
                let r = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
                c.busy_timeout(BUSY_TIMEOUT)?;
                Ok(r?)
            });
            warn(&mut rep.warnings, "wal checkpoint", done);
        }
        Ok(rep)
    }
}
