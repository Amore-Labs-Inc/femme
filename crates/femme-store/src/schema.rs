//! Schema migrations: `PRAGMA user_version` plus an ordered, append-only list of SQL scripts.
//!
//! Never edit a script that has shipped; add the next one. `user_version == MIGRATIONS.len()`
//! means up to date, a larger number means the file was written by a newer build.

use rusqlite::{Connection, TransactionBehavior};

use crate::{Error, Result};

/// Every migration, oldest first. Index + 1 is the `user_version` it leaves behind.
pub(crate) const MIGRATIONS: &[&str] = &[V1, V2, V3, V4, V5, V6];

// Notes on the shape (plan §7):
// - STRICT: SQLite refuses a TEXT in a BLOB column and the like, so an encoding slip is an error
//   at the write instead of a mystery at the read.
// - One rev counter for every replicated table (`meta.rev`), so every row version has a distinct
//   rev and `WHERE rev > ?` is a complete change feed. The extra `*_rev` indexes keep that feed
//   an index range scan.
// - `sessions_name` is partial: a closed session keeps its name for display but frees it for the
//   next session (herdr agent names were unique among *running* workspaces).
// - `items_blob` answers "does any item still use this blob?" (replaced bodies, retention sweep).
// - `tombstones` and `ops` are WITHOUT ROWID: pure key-value tables, one b-tree instead of two.
// - ponytail: `items_fts` (FTS5 over items.preview) is milestone M4. Build it as the next migration, an
//   external-content table kept in step by triggers, and backfill it from `items` in that script.
const V1: &str = r#"
CREATE TABLE meta (
    key   TEXT NOT NULL PRIMARY KEY,
    value TEXT NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE sessions (
    id              BLOB NOT NULL PRIMARY KEY,
    handle          TEXT NOT NULL UNIQUE,
    rev             INTEGER NOT NULL,
    name            TEXT,
    owner           TEXT NOT NULL,
    agent           TEXT NOT NULL,
    mode            TEXT NOT NULL,
    cwd             TEXT NOT NULL,
    label           TEXT,
    agent_sid       TEXT,
    model           TEXT,
    status          TEXT NOT NULL,
    attention       TEXT NOT NULL,
    spool_committed INTEGER NOT NULL DEFAULT 0,
    created         INTEGER NOT NULL,
    closed          INTEGER
) STRICT;
CREATE UNIQUE INDEX sessions_name ON sessions(name) WHERE name IS NOT NULL AND closed IS NULL;
CREATE INDEX sessions_rev ON sessions(rev);
CREATE INDEX sessions_closed ON sessions(closed) WHERE closed IS NOT NULL;

CREATE TABLE panes (
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    no         INTEGER NOT NULL,
    rev        INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    title      TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (session_id, no)
) STRICT;
CREATE INDEX panes_rev ON panes(rev);

CREATE TABLE items (
    session_id BLOB NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    item_id    TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    rev        INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    status     TEXT NOT NULL,
    preview    TEXT NOT NULL,
    body       BLOB,
    blob_hash  BLOB,
    ts         INTEGER NOT NULL,
    PRIMARY KEY (session_id, item_id)
) STRICT;
CREATE INDEX items_rev ON items(session_id, rev);
CREATE UNIQUE INDEX items_seq ON items(session_id, seq);
CREATE INDEX items_blob ON items(blob_hash) WHERE blob_hash IS NOT NULL;

CREATE TABLE blobs (
    hash BLOB NOT NULL PRIMARY KEY,
    len  INTEGER NOT NULL,
    data BLOB NOT NULL,
    ts   INTEGER NOT NULL
) STRICT;
CREATE INDEX blobs_ts ON blobs(ts);

CREATE TABLE issues (
    id         INTEGER PRIMARY KEY,
    rev        INTEGER NOT NULL,
    repo       TEXT NOT NULL,
    title      TEXT NOT NULL,
    body       TEXT NOT NULL,
    status     TEXT NOT NULL,
    pr_url     TEXT,
    note       TEXT,
    session_id BLOB,
    resume_at  INTEGER,
    field_revs TEXT NOT NULL,
    created    INTEGER NOT NULL,
    updated    INTEGER NOT NULL
) STRICT;
CREATE INDEX issues_rev ON issues(rev);
CREATE INDEX issues_session ON issues(session_id) WHERE session_id IS NOT NULL;

CREATE TABLE tombstones (
    entity TEXT NOT NULL,
    id     TEXT NOT NULL,
    rev    INTEGER NOT NULL,
    ts     INTEGER NOT NULL,
    PRIMARY KEY (entity, id)
) STRICT, WITHOUT ROWID;
CREATE INDEX tomb_rev ON tombstones(rev);

CREATE TABLE ops (
    device BLOB NOT NULL,
    op_id  BLOB NOT NULL,
    resp   BLOB NOT NULL,
    ts     INTEGER NOT NULL,
    PRIMARY KEY (device, op_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX ops_ts ON ops(ts);

CREATE TABLE devices (
    id          BLOB NOT NULL PRIMARY KEY,
    name        TEXT NOT NULL,
    spki_sha256 BLOB NOT NULL UNIQUE,
    created     INTEGER NOT NULL,
    last_seen   INTEGER,
    revoked     INTEGER
) STRICT;

CREATE TABLE web_sessions (
    token_sha256 BLOB NOT NULL PRIMARY KEY,
    expires      INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE api_keys (
    name         TEXT NOT NULL PRIMARY KEY,
    key          TEXT NOT NULL UNIQUE,
    github_token TEXT,
    github_login TEXT,
    git_name     TEXT,
    git_email    TEXT,
    created      INTEGER NOT NULL
) STRICT;

CREATE TABLE keybindings (
    device BLOB NOT NULL PRIMARY KEY,
    json   TEXT NOT NULL
) STRICT;
"#;

// V2 (M3): what a headless session needs to re-attach to its agent after a daemon restart. `epoch`
// is the attach epoch, `json` the driver's pending answers and held prompts (the daemon owns that
// shape). Not replicated, no rev; it dies with the session.
const V2: &str = r#"
CREATE TABLE agent_state (
    session_id BLOB NOT NULL PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    epoch      INTEGER NOT NULL,
    json       TEXT NOT NULL
) STRICT;
"#;

/// Subagent items name the tool call that spawned them.
const V3: &str = "ALTER TABLE items ADD COLUMN parent TEXT;";

/// A Claude session's permission mode.
const V4: &str = "ALTER TABLE sessions ADD COLUMN permission TEXT;";

/// How full the agent's context window is (`Session::context`): NULL until the agent said.
const V5: &str = "ALTER TABLE sessions ADD COLUMN ctx_used INTEGER; ALTER TABLE sessions ADD COLUMN ctx_window INTEGER; ALTER TABLE sessions ADD COLUMN compactions INTEGER;";

/// A Claude session's effort (`/effort`).
const V6: &str = "ALTER TABLE sessions ADD COLUMN effort TEXT;";

/// Brings the database up to [`MIGRATIONS`]'s last version. Safe to run from two processes at
/// once: each script runs inside `BEGIN IMMEDIATE` after re-reading the version.
pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    let newest = MIGRATIONS.len();
    let seen = user_version(conn)?;
    if seen > newest {
        return Err(Error::Corrupt(format!(
            "database schema is v{seen} but this build only knows v{newest}: refusing to touch it"
        )));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(seen) {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if user_version(&tx)? > i {
            continue; // another process got there first
        }
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

fn user_version(conn: &Connection) -> Result<usize> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    Ok(v.max(0) as usize)
}

/// A random non-zero epoch (see [`init_meta`]).
pub(crate) fn new_epoch() -> u64 {
    (femme_proto::Id::new().0 as u64).max(1)
}

/// Inserts the `meta` rows a fresh database starts with (existing rows are left alone) and
/// returns `(epoch, instance_id)`.
///
/// `epoch` is a random non-zero u64: a client that presents a different epoch is talking to a
/// database that was recreated and must forget everything it cached.
pub(crate) fn init_meta(conn: &Connection) -> Result<(u64, String)> {
    let id = femme_proto::Id::new();
    let epoch = new_epoch();
    conn.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES ('epoch', ?1), ('instance_id', ?2), ('rev', '0'), ('tomb_horizon', '0')",
        (epoch.to_string(), id.to_string()),
    )?;
    let epoch = crate::meta_get(conn, "epoch")?
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| Error::Corrupt("meta.epoch is not a number".into()))?;
    let instance_id = crate::meta_get(conn, "instance_id")?
        .ok_or_else(|| Error::Corrupt("meta.instance_id missing".into()))?;
    Ok((epoch, instance_id))
}
