//! `femme-store`: the SQLite persistence layer of the VPS daemon `femmed`.
//!
//! # Model
//!
//! - **Rev-based rows.** Sessions, panes, transcript items and issues each carry a `rev` taken
//!   from one global counter (`meta.rev`), so every version of every row has a distinct rev and
//!   "everything newer than rev N" is a complete change feed ([`sync::changes_since`]).
//! - **Tombstones.** A delete leaves a row in `tombstones` (kept 14 days, see [`retain`]) so a
//!   client that was offline learns about it. A cursor older than the purge horizon cannot be
//!   trusted: `changes_since` answers with `reset = true` and a full set instead.
//! - **Heavy bodies stay out of the tables.** Item bodies up to 512 B are stored raw, up to 4 KiB
//!   zstd'd, and anything bigger goes to the content-addressed `blobs` table ([`items`]).
//! - **Retention** is a daily job ([`Store::retain`]); [`Store::db_size_bytes`] feeds the 1 GiB
//!   alarm. Terminal bytes are never stored here.
//! - **Import/export** of the old web board's JSON files: [`Store::import_board_json`],
//!   [`Store::export_board_json`].
//!
//! # Using it from `femmed`
//!
//! The API is synchronous: call it from `spawn_blocking` or a dedicated writer task. `Store`
//! is `Send + Sync`; share it as `Arc<Store>`.
//!
//! - **Every mutation goes through [`Store::write`]**: one `BEGIN IMMEDIATE .. COMMIT`, rolled
//!   back if the closure returns `Err`. Grouped commits (the plan's "≤ 5 ms / 64 ops") are
//!   femmed's job: put several operations in one closure. The store never batches on its own.
//!   Inside the closure, `tx.insert_session(..)`, `tx.put_item(..)` and friends are methods on
//!   [`Tx`]; each row it writes gets `rev = tx.next_rev()`.
//! - **Reads** are [`Store::read`] (a pooled connection, one consistent snapshot per closure) and
//!   take a `&Connection`. The read functions in the modules ([`sessions::get_session`],
//!   [`items::page_items`], [`issues::list_issues`], ..) are free functions over `&Connection`;
//!   a [`Tx`] derefs to `Connection`, so the same functions work on `&*tx` inside a write
//!   closure (and then see that closure's own writes). The exception is
//!   [`sync::changes_since`]: `meta.rev` is only written at commit, so inside a write it reports
//!   the last *committed* rev; call it after the commit.
//! - **Never call a `Store` method from inside a `read` or `write` closure** (`read`, `write`,
//!   but also `rev`, `changes_since`, `get_blob`, `db_size_bytes`, which read). The writer lock
//!   is not re-entrant, an in-memory store's reads share it, and four nested reads exhaust the
//!   file store's pool: each of these deadlocks, so the store panics instead and says why. Use
//!   the `Connection` or `Tx` the closure was handed.
//! - **Time** is unix milliseconds (`i64`). Operations that stamp a row take `now` explicitly;
//!   the few that do not (the `delete_*` functions that leave a tombstone) use [`Tx::now`], the
//!   clock at the start of the transaction. [`Store::write_at`] fixes that clock, for tests.
//! - A second process may open the same file (a CLI such as `femmed enroll`): WAL plus a 5 s
//!   busy timeout make that safe, and revs are allocated after `BEGIN IMMEDIATE`, so they stay
//!   unique across processes. A running daemon will not *notice* such a write until it re-reads.
//! - **After restoring a backup over the file, call [`Store::rotate_epoch`]** before serving, so
//!   every client resyncs from scratch (see [`sync`]).

#![warn(missing_docs)]

mod enums;
mod schema;

pub mod auth;
pub mod import;
pub mod issues;
pub mod items;
pub mod retain;
pub mod sessions;
pub mod sync;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::ops::Deref;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use femme_proto::state::Entity;
use rusqlite::{Connection, Params, Row, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

pub use auth::{ApiKey, Device};
pub use import::{ExportReport, ImportReport};
pub use retain::{Caps, RetainReport};
pub use sync::Changes;

/// What the store can fail with.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// SQLite (or the binding layer) failed. Also covers constraint violations that are bugs in
    /// the caller, such as a duplicate session id.
    #[error("sqlite: {0}")]
    Sqlite(rusqlite::Error),
    /// The row asked for does not exist. A foreign-key failure (a pane or item for a session
    /// that is not there) is reported as this too.
    #[error("not found")]
    NotFound,
    /// [`Tx::patch_issue`] found that a field it would change was changed by someone else after
    /// the caller's `base_rev`. `field_revs` holds just the conflicting fields, each with the rev
    /// of its latest change. The caller should refetch the issue and decide.
    #[error("conflict on issue field(s): {}", .field_revs.keys().cloned().collect::<Vec<_>>().join(", "))]
    Conflict {
        /// Conflicting field name (`"title"`, `"status"`, ..) to the rev that last changed it.
        field_revs: BTreeMap<String, u64>,
    },
    /// Stored data does not parse: an unknown enum string, a bad zstd frame, an unreadable
    /// import file. Names the thing; never retried.
    #[error("corrupt: {0}")]
    Corrupt(String),
    /// Filesystem error (opening the database, import/export files).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON (de)serialization failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// A unique value is already taken; names what (`"name"` for a session or API-key name).
    #[error("already exists: {0}")]
    Exists(&'static str),
}

/// The store's result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        use rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY;
        match &e {
            rusqlite::Error::QueryReturnedNoRows => Error::NotFound,
            rusqlite::Error::SqliteFailure(f, _)
                if f.extended_code == SQLITE_CONSTRAINT_FOREIGNKEY =>
            {
                Error::NotFound
            }
            rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::IntegralValueOutOfRange(..) => Error::Corrupt(e.to_string()),
            _ => Error::Sqlite(e),
        }
    }
}

/// Number of pooled read connections.
const READERS: usize = 4;

/// How long a connection waits for a lock held by another connection or process.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// The database. One writer connection behind a mutex, a small pool of read-only connections.
///
/// See the [crate docs](crate) for the rules of use. An in-memory store ([`Store::open_memory`])
/// has no pool: its reads share the writer connection.
pub struct Store {
    writer: Mutex<Connection>,
    readers: Pool,
    memory: bool,
    epoch: u64,
    instance_id: String,
}

impl Store {
    /// Opens (creating, with mode 0600, if needed) the database at `path`, migrates it to the
    /// current schema and opens the read pool. The parent directory is created if missing.
    ///
    /// Every connection gets: `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`,
    /// `busy_timeout=5000`, `auto_vacuum=INCREMENTAL` (only takes effect on a fresh file, before
    /// the first table exists), `temp_store=MEMORY`, `journal_size_limit=64 MiB`; readers are
    /// also `query_only`.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        // The file holds API keys and GitHub tokens: private from the first byte. SQLite gives
        // the -wal and -shm files the same mode.
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;

        let mut writer = Connection::open(path)?;
        configure(&writer, true, false)?;
        schema::migrate(&mut writer)?;
        let (epoch, instance_id) = schema::init_meta(&writer)?;

        let mut idle = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            let conn = Connection::open(path)?;
            configure(&conn, true, true)?;
            idle.push(conn);
        }
        Ok(Store {
            writer: Mutex::new(writer),
            readers: Pool {
                idle: Mutex::new(idle),
                ready: Condvar::new(),
            },
            memory: false,
            epoch,
            instance_id,
        })
    }

    /// A private in-memory database, for tests. Reads share the writer connection (an in-memory
    /// database cannot be shared between connections without shared-cache locking quirks), so
    /// `read` blocks while a `write` is running.
    pub fn open_memory() -> Result<Store> {
        let mut writer = Connection::open_in_memory()?;
        configure(&writer, false, false)?;
        schema::migrate(&mut writer)?;
        let (epoch, instance_id) = schema::init_meta(&writer)?;
        Ok(Store {
            writer: Mutex::new(writer),
            readers: Pool {
                idle: Mutex::new(Vec::new()),
                ready: Condvar::new(),
            },
            memory: true,
            epoch,
            instance_id,
        })
    }

    /// Runs `f` inside one write transaction (`BEGIN IMMEDIATE .. COMMIT`). If `f` returns `Err`
    /// (or panics) everything it did, including the rev counter, is rolled back.
    ///
    /// Timestamps that `f` does not pass explicitly come from the system clock, read once at the
    /// start ([`Tx::now`]). Do not call `read`/`write` from inside `f`.
    pub fn write<T>(&self, f: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        self.write_at(now_ms(), f)
    }

    /// [`Store::write`] with an explicit transaction clock (unix ms): [`Tx::now`] returns `now`.
    /// Retention uses it so a purge is a function of the `now` it was given; tests use it to age
    /// tombstones without sleeping.
    pub fn write_at<T>(&self, now: i64, f: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        let _nest = NoNest::enter("write");
        let mut conn = lock(&self.writer);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Read after BEGIN IMMEDIATE: this is the serialization point, also across processes.
        let rev = current_rev(&tx)?;
        let mut tx = Tx {
            tx,
            rev,
            start_rev: rev,
            now,
        };
        let out = f(&mut tx)?; // an early return drops the transaction: rollback
        if tx.rev != tx.start_rev {
            tx.tx.execute(
                "UPDATE meta SET value = ?1 WHERE key = 'rev'",
                [tx.rev.to_string()],
            )?;
        }
        tx.tx.commit()?;
        Ok(out)
    }

    /// Runs `f` on a pooled read-only connection, inside one read transaction: every statement in
    /// `f` sees the same snapshot, even while the writer commits. Blocks if all 4 connections are
    /// busy. Do not call from inside a `write` closure.
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let _nest = NoNest::enter("read");
        if self.memory {
            let conn = lock(&self.writer);
            return in_snapshot(&conn, f);
        }
        let lease = self.readers.take();
        in_snapshot(&lease, f)
    }

    /// Runs `f` on the writer connection *outside* a transaction: maintenance that SQLite refuses
    /// inside one (`wal_checkpoint`). Holds the writer lock meanwhile.
    pub(crate) fn with_writer<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        f(&lock(&self.writer))
    }

    /// Random id of this database, fixed at creation. Clients that present another epoch
    /// (`Cursors::epoch`) must discard what they cached and resync from scratch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Gives the database a new random epoch and returns it. Call this after a backup was
    /// restored over the file, before the store is shared: every client then sees another epoch
    /// and resyncs from scratch. Without it a restored file keeps the epoch of the database it
    /// was copied from, and a client whose cursor is ahead of the restored revs is only caught
    /// until new writes push the rev past that cursor (the "cursor newer than the database"
    /// check in [`sync`]), after which it silently misses the rows in between.
    ///
    /// Takes `&mut self` so it cannot run while the store is shared.
    pub fn rotate_epoch(&mut self) -> Result<u64> {
        let epoch = schema::new_epoch();
        lock_mut(&mut self.writer).execute(
            "UPDATE meta SET value = ?1 WHERE key = 'epoch'",
            [epoch.to_string()],
        )?;
        self.epoch = epoch;
        Ok(epoch)
    }

    /// Stable random id (a ULID string) of this database; survives restarts.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// The highest rev handed out so far (committed).
    pub fn rev(&self) -> Result<u64> {
        self.read(current_rev)
    }

    /// [`sync::changes_since`] on a read snapshot.
    pub fn changes_since(
        &self,
        scope: femme_proto::rpc::Scope,
        after: Option<u64>,
        limit_bytes: usize,
    ) -> Result<Changes> {
        self.read(|c| sync::changes_since(c, scope, after, limit_bytes))
    }

    /// [`items::get_blob`] on a read snapshot: the decompressed body stored under `hash`.
    pub fn get_blob(&self, hash: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        self.read(|c| items::get_blob(c, hash))
    }

    /// Size of the main database file in bytes (`page_count * page_size`; the `-wal` file is not
    /// included). `femmed` alarms when this passes 1 GiB.
    pub fn db_size_bytes(&self) -> Result<u64> {
        self.read(|c| {
            let pages: u64 = c.pragma_query_value(None, "page_count", |r| r.get(0))?;
            let size: u64 = c.pragma_query_value(None, "page_size", |r| r.get(0))?;
            Ok(pages * size)
        })
    }
}

/// A write transaction, handed to the closure of [`Store::write`].
///
/// Wraps the SQLite transaction plus the rev allocator. It derefs to [`Connection`], so the read
/// functions of the modules work on it (`sessions::get_session(tx, id)` sees this transaction's
/// own writes). The mutating methods are defined next to their data: [`sessions`], [`items`],
/// [`issues`], [`auth`].
pub struct Tx<'c> {
    tx: Transaction<'c>,
    rev: u64,
    start_rev: u64,
    now: i64,
}

impl Deref for Tx<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.tx
    }
}

impl Tx<'_> {
    /// Allocates the next global rev. One counter serves every replicated table; it is read when
    /// the transaction starts and written back at commit.
    pub fn next_rev(&mut self) -> u64 {
        self.rev += 1;
        self.rev
    }

    /// The transaction's clock, unix ms (see [`Store::write_at`]).
    pub fn now(&self) -> i64 {
        self.now
    }

    /// Sets a `meta` key/value (upsert). Not public: the keys are the store's own bookkeeping
    /// (`tomb_horizon`, `issue_seq`, `json_imported`, `repos`), and a caller overwriting one
    /// would corrupt it; each has its own method.
    pub(crate) fn meta_set(&mut self, key: &str, value: &str) -> Result<()> {
        self.tx.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (key, value),
        )?;
        Ok(())
    }

    /// Replaces the list returned by [`repos`]. Not replicated: no rev change.
    pub fn set_repos(&mut self, repos: &[String]) -> Result<()> {
        self.meta_set("repos", &serde_json::to_string(repos)?)
    }

    /// Records a tombstone (replacing an older one for the same row) with a fresh rev; returns it.
    pub(crate) fn tombstone(&mut self, entity: Entity, id: &str) -> Result<u64> {
        let rev = self.next_rev();
        self.tx
            .prepare_cached(
                "INSERT INTO tombstones(entity, id, rev, ts) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(entity, id) DO UPDATE SET rev = excluded.rev, ts = excluded.ts",
            )?
            .execute((enums::entity_str(entity), id, rev, self.now))?;
        Ok(rev)
    }

    /// Drops a tombstone: the row it described exists again. Keeps `changes_since` from ever
    /// returning a live row together with a tombstone for it.
    pub(crate) fn clear_tombstone(&mut self, entity: Entity, id: &str) -> Result<()> {
        self.tx
            .prepare_cached("DELETE FROM tombstones WHERE entity = ?1 AND id = ?2")?
            .execute((enums::entity_str(entity), id))?;
        Ok(())
    }
}

/// Reads a `meta` value.
pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    one(conn, "SELECT value FROM meta WHERE key = ?1", [key], |r| {
        Ok(r.get(0)?)
    })
}

/// The `owner/name` repos the board offers in its new-issue form. The scheduler refreshes the
/// list from `gh repo list`; the import seeds it from the old `meta.json`. Empty if never set.
pub fn repos(conn: &Connection) -> Result<Vec<String>> {
    match meta_get(conn, "repos")? {
        Some(json) => Ok(serde_json::from_str(&json)?),
        None => Ok(Vec::new()),
    }
}

/// SHA-256 of `data`: how web-session tokens and blob contents are keyed.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Current wall clock, unix milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

// ---------------------------------------------------------------------------------------------
// Plumbing

/// The committed rev counter.
pub(crate) fn current_rev(conn: &Connection) -> Result<u64> {
    meta_get(conn, "rev")?
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| Error::Corrupt("meta.rev missing or not a number".into()))
}

/// Per-connection settings; see [`Store::open`].
fn configure(conn: &Connection, file: bool, reader: bool) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.set_prepared_statement_cache_capacity(64);
    // auto_vacuum first: it only takes effect before the first table exists.
    conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    if file {
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(mode, "femme-store: could not switch the database to WAL");
        }
        conn.pragma_update(None, "journal_size_limit", 64i64 << 20)?;
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    if reader {
        conn.pragma_update(None, "query_only", "ON")?;
    }
    Ok(())
}

/// Runs `f` inside a deferred (read) transaction on `conn`: one snapshot for all its statements.
fn in_snapshot<T>(conn: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)?;
    f(&tx) // dropped without commit: rollback, which for a read changes nothing
}

/// Locks a mutex, ignoring poisoning: a panic in a closure rolls its transaction back, so the
/// connection behind the lock is still good.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// [`lock`] for a mutex we own exclusively.
fn lock_mut<T>(m: &mut Mutex<T>) -> &mut T {
    m.get_mut().unwrap_or_else(PoisonError::into_inner)
}

thread_local! {
    /// Whether this thread is inside a `Store::read` / `Store::write` closure.
    static IN_STORE: Cell<bool> = const { Cell::new(false) };
}

/// Marks the thread as inside a store closure for as long as it lives. Entering twice means a
/// `Store` method was called from inside a closure, which deadlocks (see the crate docs); this
/// panics with the reason instead, before taking any lock. Cheap enough to be always on.
struct NoNest;

impl NoNest {
    fn enter(call: &str) -> NoNest {
        IN_STORE.with(|inside| {
            assert!(
                !inside.replace(true),
                "Store::{call} called from inside a Store::read or Store::write closure: that deadlocks. \
                 Use the Connection or Tx the closure was handed"
            );
        });
        NoNest
    }
}

impl Drop for NoNest {
    fn drop(&mut self) {
        IN_STORE.with(|inside| inside.set(false));
    }
}

struct Pool {
    idle: Mutex<Vec<Connection>>,
    ready: Condvar,
}

struct Lease<'a> {
    pool: &'a Pool,
    conn: Option<Connection>,
}

impl Pool {
    /// Takes an idle connection, waiting for one to come back if all are out.
    fn take(&self) -> Lease<'_> {
        let mut idle = lock(&self.idle);
        loop {
            if let Some(conn) = idle.pop() {
                return Lease {
                    pool: self,
                    conn: Some(conn),
                };
            }
            idle = self
                .ready
                .wait(idle)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Deref for Lease<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("lease holds a connection until dropped")
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            lock(&self.pool.idle).push(conn);
            self.pool.ready.notify_one();
        }
    }
}

/// One optional row, through the connection's statement cache; `f` may fail with the store's own
/// error (enum parsing, decompression).
pub(crate) fn one<T>(
    conn: &Connection,
    sql: &str,
    params: impl Params,
    f: impl FnOnce(&Row<'_>) -> Result<T>,
) -> Result<Option<T>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = stmt.query(params)?;
    match rows.next()? {
        Some(row) => Ok(Some(f(row)?)),
        None => Ok(None),
    }
}

/// All rows, through the connection's statement cache.
pub(crate) fn all<T>(
    conn: &Connection,
    sql: &str,
    params: impl Params,
    mut f: impl FnMut(&Row<'_>) -> Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(f(row)?);
    }
    Ok(out)
}

/// Whether a query returns at least one row.
pub(crate) fn exists(conn: &Connection, sql: &str, params: impl Params) -> Result<bool> {
    Ok(conn.prepare_cached(sql)?.exists(params)?)
}

/// Reads an enum column stored as text.
pub(crate) fn enum_col<T>(row: &Row<'_>, idx: usize, parse: fn(&str) -> Option<T>) -> Result<T> {
    let s = row
        .get_ref(idx)?
        .as_str()
        .map_err(|e| Error::Corrupt(format!("column {idx}: {e}")))?;
    parse(s).ok_or_else(|| Error::Corrupt(format!("column {idx}: unknown value {s:?}")))
}

/// A ULID as its 16 big-endian bytes (byte order = time order, so blobs sort like the ULIDs).
pub(crate) fn id_blob(id: femme_proto::Id) -> [u8; 16] {
    id.0.to_be_bytes()
}

/// Inverse of [`id_blob`].
pub(crate) fn blob_id(b: [u8; 16]) -> femme_proto::Id {
    femme_proto::Id(u128::from_be_bytes(b))
}

/// Size of `v` as postcard would write it: what `limit_bytes` budgets count.
pub(crate) fn wire_size<T: serde::Serialize>(v: &T) -> Result<usize> {
    postcard::experimental::serialized_size(v).map_err(|e| Error::Corrupt(format!("postcard: {e}")))
}

/// Treats `Some("")` as `None`: the board stored "no note" as an empty string.
pub(crate) fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.is_empty())
}

#[allow(dead_code)]
fn assert_send_sync() {
    fn is<T: Send + Sync>() {}
    is::<Store>();
}
