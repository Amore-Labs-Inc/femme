//! Integration tests for `femme-store`. In-memory SQLite unless the file itself is the point
//! (pragmas, vacuum, two handles on one file), then a tempfile database.
//!
//! Run the micro-benchmark with
//! `cargo test --release -p femme-store -- --ignored --nocapture bench_`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Barrier;

use femme_proto::rpc::{IssueField, Scope};
use femme_proto::state::{
    Agent, Attention, Context, Entity, Issue, IssueStatus, Item, ItemKind, ItemStatus, Mode, Pane,
    PaneKind, Row, Session, SessionStatus,
};
use femme_proto::{Id, PaneId, Sid};
use femme_store::items::{
    BLOB_BYTES_MAX, BLOB_MAX, INLINE_MAX, INLINE_RAW_MAX, PREVIEW_MAX, TRUNCATED,
};
use femme_store::sync::INITIAL_ITEMS;
use femme_store::{auth, issues, items, sessions, sha256, Caps, Error, Store};
use rusqlite::Connection;

const NOW: i64 = 1_793_000_000_000;
const DAY: i64 = 86_400_000;
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

type R<T = ()> = femme_store::Result<T>;

// ---------------------------------------------------------------------------------------------
// helpers

fn store() -> Store {
    Store::open_memory().unwrap()
}

fn session(name: Option<&str>) -> Session {
    Session {
        id: Id::new(),
        handle: String::new(),
        rev: 0,
        name: name.map(String::from),
        owner: "board".into(),
        agent: Agent::Claude,
        mode: Mode::Headless,
        cwd: "/work/acme".into(),
        label: None,
        agent_sid: None,
        model: None,
        status: SessionStatus::Idle,
        attention: Attention::None,
        created: NOW,
        closed: None,
        permission: None,
        context: None,
        effort: None,
        goal: None,
    }
}

fn pane(sid: Sid, no: u16, title: &str) -> Pane {
    Pane {
        id: PaneId { sid, no },
        rev: 0,
        kind: PaneKind::Shell,
        title: title.into(),
    }
}

fn item(sid: Sid, item_id: &str, body: &str) -> Item {
    Item {
        sid,
        item_id: item_id.into(),
        seq: 0,
        rev: 0,
        kind: ItemKind::AssistantMessage,
        status: ItemStatus::Completed,
        preview: String::new(),
        body: Some(body.into()),
        blob_hash: None,
        parent: None,
        ts: NOW,
    }
}

/// Deterministic printable-ASCII noise: roughly incompressible text of exactly `len` bytes.
fn noise(len: usize, seed: u64) -> String {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (b' ' + ((x >> 33) % 95) as u8) as char
        })
        .collect()
}

fn insert_session(s: &Store, name: Option<&str>) -> Session {
    s.write(|tx| tx.insert_session(&session(name))).unwrap()
}

/// Everything the global feed has: live rows and tombstones, ordered by rev.
fn global_all(s: &Store) -> Vec<Row> {
    s.changes_since(Scope::Global, Some(0), usize::MAX)
        .unwrap()
        .rows
}

fn tomb(entity: Entity, id: impl ToString, rev: u64) -> Row {
    Row::Tombstone {
        entity,
        id: id.to_string(),
        rev,
    }
}

fn raw_body(s: &Store, sid: Sid, item_id: &str) -> Option<Vec<u8>> {
    s.read(|c| {
        Ok(c.query_row(
            "SELECT body FROM items WHERE session_id = ?1 AND item_id = ?2",
            rusqlite::params![sid.0.to_be_bytes(), item_id],
            |r| r.get(0),
        )?)
    })
    .unwrap()
}

fn count(s: &Store, table: &str) -> usize {
    s.read(|c| Ok(c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?))
        .unwrap()
}

fn tempdb() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state").join("femme.db"); // the parent does not exist yet
    (dir, path)
}

// ---------------------------------------------------------------------------------------------
// the store itself

fn check_pragmas(c: &Connection, reader: bool) {
    let int = |name: &str| -> i64 { c.pragma_query_value(None, name, |r| r.get(0)).unwrap() };
    let mode: String = c
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    assert_eq!(int("synchronous"), 1, "NORMAL");
    assert_eq!(int("foreign_keys"), 1);
    assert_eq!(int("busy_timeout"), 5000);
    assert_eq!(int("auto_vacuum"), 2, "INCREMENTAL");
    assert_eq!(int("temp_store"), 2, "MEMORY");
    assert_eq!(int("query_only"), i64::from(reader));
}

#[test]
fn every_connection_gets_the_pragmas() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, path) = tempdb();
    let s = Store::open(&path).unwrap();
    s.write(|tx| {
        check_pragmas(tx, false);
        Ok(())
    })
    .unwrap();
    // four readers held at once: the pool has to hand out four different connections. With fewer
    // the barrier would never open, so a watchdog turns that hang into a failure.
    let (done, watchdog) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if watchdog
            .recv_timeout(std::time::Duration::from_secs(30))
            .is_err()
        {
            eprintln!("the read pool cannot hold four connections at once");
            std::process::exit(1);
        }
    });
    let barrier = Barrier::new(4);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                s.read(|c| {
                    check_pragmas(c, true);
                    barrier.wait();
                    Ok(())
                })
                .unwrap();
            });
        }
    });
    done.send(()).unwrap();
    let version: i64 = s
        .read(|c| Ok(c.pragma_query_value(None, "user_version", |r| r.get(0))?))
        .unwrap();
    assert_eq!(version, 7);
    // the file holds API keys: private from the first byte, and SQLite gives its side files the same mode
    for file in [
        path.clone(),
        path.with_extension("db-wal"),
        path.with_extension("db-shm"),
    ] {
        let mode = std::fs::metadata(&file)
            .unwrap_or_else(|e| panic!("{}: {e}", file.display()))
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{}", file.display());
    }
}

#[test]
fn memory_store_has_foreign_keys_and_no_wal() {
    let s = store();
    let (fk, mode): (i64, String) = s
        .read(|c| {
            Ok((
                c.pragma_query_value(None, "foreign_keys", |r| r.get(0))?,
                c.pragma_query_value(None, "journal_mode", |r| r.get(0))?,
            ))
        })
        .unwrap();
    assert_eq!((fk, mode.as_str()), (1, "memory"));
}

#[test]
fn reopening_keeps_data_epoch_and_instance() {
    let (_dir, path) = tempdb();
    let (epoch, instance, sid, rev) = {
        let s = Store::open(&path).unwrap();
        let sess = insert_session(&s, Some("keeper"));
        s.write(|tx| tx.put_item(&item(sess.id, "i1", "hello")))
            .unwrap();
        (
            s.epoch(),
            s.instance_id().to_string(),
            sess.id,
            s.rev().unwrap(),
        )
    };
    let s = Store::open(&path).unwrap();
    assert_eq!(
        (s.epoch(), s.instance_id(), s.rev().unwrap()),
        (epoch, instance.as_str(), rev)
    );
    assert_ne!(epoch, 0);
    assert!(s.read(|c| items::get_item(c, sid, "i1")).unwrap().is_some());
    assert_ne!(
        store().epoch(),
        store().epoch(),
        "every new database picks its own epoch"
    );
}

#[test]
fn a_newer_schema_is_refused() {
    let (_dir, path) = tempdb();
    drop(Store::open(&path).unwrap());
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();
    match Store::open(&path) {
        Err(Error::Corrupt(why)) => assert!(why.contains("v99"), "{why}"),
        other => panic!("expected Corrupt, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn rev_is_one_global_counter_and_rolls_back() {
    let s = store();
    assert_eq!(s.rev().unwrap(), 0);
    let revs = s
        .write(|tx| {
            let sess = tx.insert_session(&session(Some("a")))?;
            let p = tx.upsert_pane(&pane(sess.id, 0, "sh"))?;
            let it = tx.put_item(&item(sess.id, "i1", "hello"))?;
            let issue = tx.create_issue("acme/webshop", "t", "b", NOW)?;
            Ok([sess.rev, p.rev, it.rev, issue.rev])
        })
        .unwrap();
    assert_eq!(revs, [1, 2, 3, 4], "one counter across all four tables");
    assert_eq!(s.rev().unwrap(), 4);

    let failed = s.write(|tx| {
        tx.create_issue("acme/webshop", "x", "", NOW)?;
        Err::<(), _>(Error::NotFound)
    });
    assert!(matches!(failed, Err(Error::NotFound)));
    assert_eq!(
        s.rev().unwrap(),
        4,
        "a rolled-back transaction gives its revs back"
    );
    assert_eq!(count(&s, "issues"), 1);

    // a closure that panics rolls back too, and the store stays usable
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = s.write(|tx| -> R {
            tx.create_issue("acme/webshop", "y", "", NOW)?;
            panic!("boom");
        });
    }));
    assert!(outcome.is_err());
    assert_eq!((s.rev().unwrap(), count(&s, "issues")), (4, 1));
    s.write(|tx| tx.create_issue("acme/webshop", "z", "", NOW))
        .unwrap();
}

#[test]
fn two_handles_on_one_file_never_hand_out_the_same_rev() {
    // Two Store handles are two writer connections, like a daemon and a CLI process: only the
    // SQLite write lock serializes them, so the rev must be read after BEGIN IMMEDIATE.
    let (_dir, path) = tempdb();
    let (a, b) = (Store::open(&path).unwrap(), Store::open(&path).unwrap());
    let revs = std::sync::Mutex::new(Vec::new());
    let go = Barrier::new(4);
    std::thread::scope(|scope| {
        for (n, store) in [&a, &a, &b, &b].into_iter().enumerate() {
            let (revs, go) = (&revs, &go);
            scope.spawn(move || {
                go.wait();
                for i in 0..60 {
                    let issue = store
                        .write(|tx| tx.create_issue("acme/webshop", &format!("t{n}-{i}"), "", NOW))
                        .unwrap();
                    revs.lock().unwrap().push(issue.rev);
                }
            });
        }
    });
    let mut revs = revs.into_inner().unwrap();
    revs.sort_unstable();
    assert_eq!(
        revs,
        (1..=240).collect::<Vec<u64>>(),
        "every rev handed out exactly once, none skipped"
    );
    assert_eq!((a.rev().unwrap(), b.rev().unwrap()), (240, 240));
}

#[test]
fn rotating_the_epoch_changes_it_for_good() {
    let (_dir, path) = tempdb();
    let mut s = Store::open(&path).unwrap();
    let (old, instance) = (s.epoch(), s.instance_id().to_string());
    let new = s.rotate_epoch().unwrap();
    assert_ne!(new, old);
    assert_eq!(s.epoch(), new);
    drop(s);
    let s = Store::open(&path).unwrap();
    assert_eq!(
        (s.epoch(), s.instance_id()),
        (new, instance.as_str()),
        "the new epoch is in the file; the instance id is not touched"
    );
}

#[test]
#[should_panic(expected = "that deadlocks")]
fn a_read_inside_a_read_panics_instead_of_deadlocking() {
    let s = store(); // in memory: the inner read would wait for the lock the outer one holds
    let _ = s.read(|_| s.read(|_| Ok(())));
}

#[test]
#[should_panic(expected = "that deadlocks")]
fn a_write_inside_a_write_panics_instead_of_deadlocking() {
    let s = store();
    let _ = s.write(|_| s.write(|_| Ok(())));
}

#[test]
#[should_panic(expected = "that deadlocks")]
fn the_reading_helpers_of_the_store_count_as_reads() {
    let (_dir, path) = tempdb();
    let s = Store::open(&path).unwrap(); // a file store: four of these at once would drain the pool
    let _ = s.read(|_| s.rev());
}

#[test]
fn the_store_works_again_after_a_nested_call_panicked() {
    let s = store();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = s.write(|tx| {
            tx.create_issue("acme/webshop", "lost", "", NOW)?;
            s.rev()
        });
    }));
    assert!(outcome.is_err());
    assert_eq!(
        (s.rev().unwrap(), count(&s, "issues")),
        (0, 0),
        "the outer transaction rolled back"
    );
    s.write(|tx| tx.create_issue("acme/webshop", "kept", "", NOW))
        .unwrap();
    assert_eq!(count(&s, "issues"), 1);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "after the commit")]
fn changes_since_inside_a_write_closure_is_refused_in_debug_builds() {
    let s = store();
    let _ = s.write(|tx| {
        tx.create_issue("acme/webshop", "t", "", NOW)?;
        femme_store::sync::changes_since(tx, Scope::Global, Some(0), usize::MAX)
    });
}

#[test]
fn readers_see_one_snapshot_while_a_writer_commits() {
    let (_dir, path) = tempdb();
    let s = Store::open(&path).unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        for _ in 0..3 {
            scope.spawn(|| {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    // each iteration inserts one session and one issue in the same transaction:
                    // a reader must never see one without the other
                    let (sessions, issues): (i64, i64) = s
                        .read(|c| {
                            let n = |t: &str| {
                                c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
                            };
                            Ok((n("sessions")?, n("issues")?))
                        })
                        .unwrap();
                    assert_eq!(sessions, issues);
                }
            });
        }
        for i in 0..150 {
            s.write(|tx| {
                tx.insert_session(&session(None))?;
                tx.create_issue("acme/webshop", &format!("t{i}"), "", NOW)?;
                Ok(())
            })
            .unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    assert_eq!(count(&s, "sessions"), 150);
}

// ---------------------------------------------------------------------------------------------
// sessions and panes

#[test]
fn handles_are_short_and_unique() {
    let s = store();
    let handles: Vec<String> = s
        .write(|tx| {
            (0..300)
                .map(|_| Ok(tx.insert_session(&session(None))?.handle))
                .collect::<R<Vec<_>>>()
        })
        .unwrap();
    assert_eq!(handles.iter().collect::<HashSet<_>>().len(), 300);
    for h in &handles {
        assert_eq!(h.len(), 6, "{h}");
        assert!(
            h.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "{h}"
        );
    }
    let one = s
        .read(|c| sessions::session_by_handle(c, &handles[17]))
        .unwrap()
        .unwrap();
    assert_eq!(one.handle, handles[17]);
    assert!(s
        .read(|c| sessions::session_by_handle(c, "nope00"))
        .unwrap()
        .is_none());
}

#[test]
fn names_are_unique_among_open_sessions_only() {
    let s = store();
    let a = insert_session(&s, Some("alpha"));
    let dup = s
        .write(|tx| Ok(tx.insert_session(&session(Some("alpha")))))
        .unwrap();
    assert!(matches!(dup, Err(Error::Exists("name"))), "{dup:?}");
    assert_eq!(count(&s, "sessions"), 1);
    assert_eq!(
        s.read(|c| sessions::session_by_name(c, "alpha"))
            .unwrap()
            .unwrap()
            .id,
        a.id
    );

    s.write(|tx| tx.close_session(a.id, NOW)).unwrap();
    assert!(
        s.read(|c| sessions::session_by_name(c, "alpha"))
            .unwrap()
            .is_none(),
        "closed sessions do not answer to their name"
    );
    let b = insert_session(&s, Some("alpha"));
    assert_eq!(
        s.read(|c| sessions::session_by_name(c, "alpha"))
            .unwrap()
            .unwrap()
            .id,
        b.id
    );
    assert_eq!(
        s.read(|c| Ok(sessions::list_sessions(c, false)?.len()))
            .unwrap(),
        1
    );
    assert_eq!(
        s.read(|c| Ok(sessions::list_sessions(c, true)?.len()))
            .unwrap(),
        2
    );

    // renaming into a taken name is refused; into a free one is fine
    let c = insert_session(&s, Some("gamma"));
    let clash = s
        .write(|tx| Ok(tx.update_session(c.id, |s| s.name = Some("alpha".into()))))
        .unwrap();
    assert!(matches!(clash, Err(Error::Exists("name"))));
    s.write(|tx| tx.update_session(c.id, |s| s.name = Some("delta".into())))
        .unwrap();
    // two sessions without a name are fine
    insert_session(&s, None);
    insert_session(&s, None);
}

#[test]
fn a_session_keeps_its_context_window() {
    let s = store();
    let a = insert_session(&s, Some("alpha"));
    assert_eq!(a.context, None);
    let ctx = Context {
        used: 123_456,
        window: 1_000_000,
        compactions: 2,
    };
    let up = s
        .write(|tx| tx.update_session(a.id, |s| s.context = Some(ctx)))
        .unwrap();
    assert_eq!(up.context, Some(ctx));
    assert!(up.rev > a.rev);
    let back = s.read(|c| sessions::get_session(c, a.id)).unwrap().unwrap();
    assert_eq!(back.context, Some(ctx));
    // the same context again is not a change
    let same = s
        .write(|tx| tx.update_session(a.id, |s| s.context = Some(ctx)))
        .unwrap();
    assert_eq!(same.rev, up.rev);
}

#[test]
fn update_session_bumps_the_rev_only_when_something_changed() {
    let s = store();
    let a = insert_session(&s, Some("alpha"));
    let up = s
        .write(|tx| tx.update_session(a.id, |s| s.status = SessionStatus::Working))
        .unwrap();
    assert!(up.rev > a.rev);
    assert_eq!(up.status, SessionStatus::Working);
    let again = s
        .write(|tx| tx.update_session(a.id, |s| s.status = SessionStatus::Working))
        .unwrap();
    assert_eq!(
        again.rev, up.rev,
        "repeating a status update writes nothing"
    );
    assert_eq!(s.rev().unwrap(), up.rev);

    // id, handle and created are not the caller's to change
    let same = s
        .write(|tx| {
            tx.update_session(a.id, |s| {
                s.handle = "zzzzzz".into();
                s.created = 5;
                s.id = Id::new();
            })
        })
        .unwrap();
    assert_eq!(
        (same.rev, same.handle.as_str(), same.created, same.id),
        (up.rev, a.handle.as_str(), a.created, a.id)
    );

    let ghost = s.write(|tx| tx.update_session(Id::new(), |_| {}));
    assert!(matches!(ghost, Err(Error::NotFound)));
}

#[test]
fn spool_offset_is_not_replicated() {
    let s = store();
    let a = insert_session(&s, None);
    let rev = s.rev().unwrap();
    s.write(|tx| tx.set_spool_committed(a.id, 123_456)).unwrap();
    assert_eq!(
        s.read(|c| sessions::spool_committed(c, a.id)).unwrap(),
        123_456
    );
    assert_eq!(s.rev().unwrap(), rev, "no rev: clients never see it");
    assert!(matches!(
        s.write(|tx| tx.set_spool_committed(Id::new(), 1)),
        Err(Error::NotFound)
    ));
}

#[test]
fn agent_state_is_stored_per_session_replaced_in_place_and_deleted_with_it() {
    let s = store();
    let a = insert_session(&s, None);
    assert_eq!(s.read(|c| sessions::agent_state(c, a.id)).unwrap(), None);
    let rev = s.rev().unwrap();
    s.write(|tx| tx.set_agent_state(a.id, 1, r#"{"a":1}"#))
        .unwrap();
    s.write(|tx| tx.set_agent_state(a.id, 2, r#"{"a":2}"#))
        .unwrap();
    assert_eq!(
        s.read(|c| sessions::agent_state(c, a.id)).unwrap(),
        Some((2, r#"{"a":2}"#.to_string()))
    );
    assert_eq!(s.rev().unwrap(), rev, "no rev: clients never see it");
    assert!(matches!(
        s.write(|tx| tx.set_agent_state(Id::new(), 1, "{}")),
        Err(Error::NotFound)
    ));
    s.write(|tx| tx.delete_session(a.id)).unwrap();
    assert_eq!(count(&s, "agent_state"), 0);
}

#[test]
fn panes_upsert_delete_and_come_back() {
    let s = store();
    let a = insert_session(&s, None);
    let p = s
        .write(|tx| tx.upsert_pane(&pane(a.id, 0, "claude")))
        .unwrap();
    let same = s
        .write(|tx| tx.upsert_pane(&pane(a.id, 0, "claude")))
        .unwrap();
    assert_eq!(same.rev, p.rev, "unchanged pane: no write");
    let renamed = s
        .write(|tx| tx.upsert_pane(&pane(a.id, 0, "other")))
        .unwrap();
    assert!(renamed.rev > p.rev);
    s.write(|tx| tx.upsert_pane(&pane(a.id, 1, "shell")))
        .unwrap();
    assert_eq!(s.read(|c| Ok(sessions::panes(c, a.id)?.len())).unwrap(), 2);
    assert_eq!(
        s.read(|c| sessions::get_pane(c, PaneId { sid: a.id, no: 0 }))
            .unwrap()
            .unwrap()
            .title,
        "other"
    );

    s.write(|tx| tx.delete_pane(PaneId { sid: a.id, no: 1 }))
        .unwrap();
    assert!(matches!(
        s.write(|tx| tx.delete_pane(PaneId { sid: a.id, no: 1 })),
        Err(Error::NotFound)
    ));
    let gone = s
        .changes_since(Scope::Global, Some(0), usize::MAX)
        .unwrap()
        .rows;
    assert!(gone.iter().any(|r| matches!(r, Row::Tombstone { entity: Entity::Pane, id, .. } if *id == format!("{}:1", a.id))));

    // the number is reused: the stale tombstone must not ride along with the live pane
    s.write(|tx| tx.upsert_pane(&pane(a.id, 1, "again")))
        .unwrap();
    let rows = global_all(&s);
    assert!(
        !rows.iter().any(|r| matches!(
            r,
            Row::Tombstone {
                entity: Entity::Pane,
                ..
            }
        )),
        "{rows:?}"
    );
    assert!(matches!(
        s.write(|tx| tx.upsert_pane(&pane(Id::new(), 0, "x"))),
        Err(Error::NotFound)
    ));
}

#[test]
fn closing_a_session_drops_its_panes_and_is_idempotent() {
    let s = store();
    let a = insert_session(&s, Some("alpha"));
    s.write(|tx| {
        tx.upsert_pane(&pane(a.id, 0, "claude"))?;
        tx.upsert_pane(&pane(a.id, 1, "shell"))?;
        tx.set_spool_committed(a.id, 99)
    })
    .unwrap();
    let before = s.rev().unwrap();
    let closed = s.write(|tx| tx.close_session(a.id, NOW + 5)).unwrap();
    assert_eq!(
        (closed.status, closed.closed),
        (SessionStatus::Closed, Some(NOW + 5))
    );
    assert_eq!(s.read(|c| Ok(sessions::panes(c, a.id)?.len())).unwrap(), 0);
    assert_eq!(s.read(|c| sessions::spool_committed(c, a.id)).unwrap(), 99);
    let delta = s
        .changes_since(Scope::Global, Some(before), usize::MAX)
        .unwrap()
        .rows;
    let tombs = delta
        .iter()
        .filter(|r| {
            matches!(
                r,
                Row::Tombstone {
                    entity: Entity::Pane,
                    ..
                }
            )
        })
        .count();
    assert_eq!(tombs, 2);
    assert!(delta.iter().any(
        |r| matches!(r, Row::Session(x) if x.id == a.id && x.status == SessionStatus::Closed)
    ));

    let rev = s.rev().unwrap();
    let again = s.write(|tx| tx.close_session(a.id, NOW + 999)).unwrap();
    assert_eq!((again.closed, s.rev().unwrap()), (Some(NOW + 5), rev));
    assert!(matches!(
        s.write(|tx| tx.close_session(Id::new(), NOW)),
        Err(Error::NotFound)
    ));
}

#[test]
fn deleting_a_session_cascades_and_leaves_tombstones() {
    let s = store();
    let a = insert_session(&s, Some("alpha"));
    let keep = insert_session(&s, Some("keep"));
    s.write(|tx| {
        tx.upsert_pane(&pane(a.id, 0, "claude"))?;
        tx.put_item(&item(a.id, "i1", "hello"))?;
        tx.put_item(&item(a.id, "i2", &noise(6000, 1)))?;
        tx.put_item(&item(keep.id, "k1", "stay"))?;
        tx.put_item(&item(a.id, "i3", "deleted on its own first"))?;
        tx.delete_item(a.id, "i3")
    })
    .unwrap();
    assert_eq!(count(&s, "tombstones"), 1, "the item's tombstone");
    let before = s.rev().unwrap();
    s.write(|tx| tx.delete_session(a.id)).unwrap();
    assert_eq!(
        s.read(|c| Ok(c.query_row("SELECT COUNT(*) FROM tombstones WHERE entity = 'item'", [], |r| r.get::<_, i64>(0))?)).unwrap(),
        0,
        "the session's tombstone covers its items; their own tombstones are dropped, not left to age out"
    );
    assert!(matches!(
        s.write(|tx| tx.delete_session(a.id)),
        Err(Error::NotFound)
    ));
    assert_eq!(count(&s, "items"), 1);
    assert_eq!(count(&s, "panes"), 0);
    assert!(s
        .read(|c| sessions::get_session(c, a.id))
        .unwrap()
        .is_none());
    let delta = s
        .changes_since(Scope::Global, Some(before), usize::MAX)
        .unwrap()
        .rows;
    let ids: Vec<(Entity, String)> = delta
        .iter()
        .map(|r| match r {
            Row::Tombstone { entity, id, .. } => (*entity, id.clone()),
            other => panic!("only tombstones expected, got {other:?}"),
        })
        .collect();
    // the session first, then its pane; none per item
    assert_eq!(
        ids,
        vec![
            (Entity::Session, a.id.to_string()),
            (Entity::Pane, format!("{}:0", a.id))
        ]
    );
    assert!(matches!(
        s.changes_since(Scope::Session(a.id), None, 1000),
        Err(Error::NotFound)
    ));
    // the other session's items are untouched
    assert_eq!(
        s.read(|c| Ok(items::page_items(c, keep.id, None, 10)?.len()))
            .unwrap(),
        1
    );
}

#[test]
fn every_enum_survives_the_database() {
    let s = store();
    let agents = [Agent::Shell, Agent::Claude, Agent::Codex, Agent::Pi];
    let modes = [Mode::Headless, Mode::Tui];
    let statuses = [
        SessionStatus::Starting,
        SessionStatus::Idle,
        SessionStatus::Working,
        SessionStatus::Blocked,
        SessionStatus::Paused,
        SessionStatus::Interrupted,
        SessionStatus::Crashed,
        SessionStatus::Failed,
        SessionStatus::Closed,
    ];
    let attentions = [Attention::None, Attention::Blocked, Attention::Done];
    let kinds = [
        ItemKind::UserMessage,
        ItemKind::AssistantMessage,
        ItemKind::Thinking,
        ItemKind::ToolCall,
        ItemKind::ToolResult,
        ItemKind::Question,
        ItemKind::Plan,
        ItemKind::Event,
        ItemKind::TurnStart,
        ItemKind::TurnEnd,
    ];
    let item_statuses = [
        ItemStatus::InProgress,
        ItemStatus::Completed,
        ItemStatus::Failed,
        ItemStatus::Pending,
        ItemStatus::Answered,
    ];
    let issue_statuses = [
        IssueStatus::Open,
        IssueStatus::Working,
        IssueStatus::Waiting,
        IssueStatus::Paused,
        IssueStatus::Pr,
        IssueStatus::Done,
        IssueStatus::Failed,
    ];

    let mut sess = Vec::new();
    for (i, &status) in statuses.iter().enumerate() {
        let mut want = session(None);
        want.agent = agents[i % agents.len()];
        want.mode = modes[i % modes.len()];
        want.status = status;
        want.attention = attentions[i % attentions.len()];
        sess.push(s.write(|tx| tx.insert_session(&want)).unwrap());
    }
    for a in agents {
        for m in modes {
            let mut want = session(None);
            (want.agent, want.mode) = (a, m);
            sess.push(s.write(|tx| tx.insert_session(&want)).unwrap());
        }
    }
    for want in &sess {
        let got = s
            .read(|c| sessions::get_session(c, want.id))
            .unwrap()
            .unwrap();
        assert_eq!(&got, want);
    }

    let sid = sess[0].id;
    for (i, &kind) in kinds.iter().enumerate() {
        for (j, &status) in item_statuses.iter().enumerate() {
            let mut want = item(sid, &format!("i{i}-{j}"), "x");
            (want.kind, want.status) = (kind, status);
            let put = s.write(|tx| tx.put_item(&want)).unwrap();
            let got = s
                .read(|c| items::get_item(c, sid, &want.item_id))
                .unwrap()
                .unwrap();
            assert_eq!(got, put);
            assert_eq!((got.kind, got.status), (kind, status));
        }
    }
    for (n, kind) in [PaneKind::Agent, PaneKind::Shell].into_iter().enumerate() {
        let p = s
            .write(|tx| {
                tx.upsert_pane(&Pane {
                    id: PaneId { sid, no: n as u16 },
                    rev: 0,
                    kind,
                    title: String::new(),
                })
            })
            .unwrap();
        assert_eq!(s.read(|c| sessions::get_pane(c, p.id)).unwrap().unwrap(), p);
    }
    for status in issue_statuses {
        let issue = s
            .write(|tx| tx.create_issue("acme/webshop", "t", "", NOW))
            .unwrap();
        let set = s
            .write(|tx| tx.set_issue_status(issue.id, status, None, NOW))
            .unwrap();
        assert_eq!(
            s.read(|c| issues::get_issue(c, issue.id)).unwrap().unwrap(),
            set
        );
        assert_eq!(set.status, status);
    }
}

// ---------------------------------------------------------------------------------------------
// changes_since

#[test]
fn changes_since_is_exactly_the_newer_rows_in_rev_order() {
    let s = store();
    // a history that touches every table, deletes included
    let a = insert_session(&s, Some("a"));
    let b = insert_session(&s, Some("b"));
    let c = insert_session(&s, Some("c"));
    s.write(|tx| {
        tx.upsert_pane(&pane(a.id, 0, "claude"))?;
        tx.upsert_pane(&pane(a.id, 1, "shell"))?;
        tx.upsert_pane(&pane(c.id, 0, "doomed"))?;
        Ok(())
    })
    .unwrap();
    let i1 = s
        .write(|tx| tx.create_issue("acme/webshop", "one", "", NOW))
        .unwrap();
    let i2 = s
        .write(|tx| tx.create_issue("acme/webshop", "two", "", NOW))
        .unwrap();
    s.write(|tx| tx.update_session(a.id, |s| s.status = SessionStatus::Working))
        .unwrap();
    s.write(|tx| tx.patch_issue(i1.id, i1.rev, &[IssueField::Title("one!".into())], NOW))
        .unwrap();
    s.write(|tx| tx.delete_pane(PaneId { sid: a.id, no: 1 }))
        .unwrap();
    s.write(|tx| tx.delete_issue(i2.id)).unwrap();
    s.write(|tx| tx.close_session(b.id, NOW)).unwrap();
    s.write(|tx| tx.delete_session(c.id)).unwrap();
    let head = s.rev().unwrap();

    let all = global_all(&s);
    let revs: Vec<u64> = all.iter().map(Row::rev).collect();
    assert!(
        revs.windows(2).all(|w| w[0] < w[1]),
        "strictly increasing, no duplicates: {revs:?}"
    );
    // what the history must have left: live a, b (closed), pane a:0, issue 1; tombstones for pane a:1, issue 2,
    // session c and its pane
    let live = all
        .iter()
        .filter(|r| !matches!(r, Row::Tombstone { .. }))
        .count();
    let tombs = all
        .iter()
        .filter(|r| matches!(r, Row::Tombstone { .. }))
        .count();
    assert_eq!((live, tombs), (4, 4), "{all:?}");
    for (entity, id) in [
        (Entity::Pane, format!("{}:1", a.id)),
        (Entity::Issue, i2.id.to_string()),
        (Entity::Session, c.id.to_string()),
        (Entity::Pane, format!("{}:0", c.id)),
    ] {
        assert!(
            all.iter().any(
                |r| matches!(r, Row::Tombstone { entity: e, id: i, .. } if *e == entity && *i == id)
            ),
            "{entity:?} {id}"
        );
    }

    for cursor in 0..=head + 1 {
        let got = s.changes_since(Scope::Global, Some(cursor), usize::MAX);
        if cursor > head {
            assert!(
                got.unwrap().reset,
                "a cursor from the future cannot be trusted"
            );
            continue;
        }
        let got = got.unwrap();
        let want: Vec<Row> = all.iter().filter(|r| r.rev() > cursor).cloned().collect();
        assert_eq!(got.rows, want, "cursor {cursor}");
        assert_eq!(
            (got.rev, got.more, got.reset),
            (head, false, false),
            "cursor {cursor}"
        );
    }

    // the full set: live rows only
    let full = s.changes_since(Scope::Global, None, 1).unwrap();
    assert_eq!(full.rows.len(), 4);
    assert!(full
        .rows
        .iter()
        .all(|r| !matches!(r, Row::Tombstone { .. })));
    assert_eq!(
        (full.rev, full.more, full.reset),
        (head, false, false),
        "limit_bytes does not cut a full set"
    );
}

#[test]
fn changes_since_pages_by_the_byte_budget() {
    let s = store();
    s.write(|tx| {
        (0..25).try_for_each(|i| {
            tx.create_issue("acme/webshop", &format!("issue {i}"), &"x".repeat(40), NOW)
                .map(drop)
        })
    })
    .unwrap();
    let head = s.rev().unwrap();
    let whole = s.changes_since(Scope::Global, Some(0), usize::MAX).unwrap();
    assert_eq!((whole.rows.len(), whole.more), (25, false));

    for budget in [1, 150, 400, 100_000] {
        let (mut got, mut cursor, mut pages) = (Vec::new(), 0u64, 0);
        loop {
            let page = s
                .changes_since(Scope::Global, Some(cursor), budget)
                .unwrap();
            assert!(
                !page.rows.is_empty(),
                "an unfinished feed must always make progress"
            );
            let size: usize = page
                .rows
                .iter()
                .map(|r| postcard::experimental::serialized_size(r).unwrap())
                .sum();
            assert!(
                size <= budget || page.rows.len() == 1,
                "budget {budget}: {} rows, {size} bytes",
                page.rows.len()
            );
            got.extend(page.rows.iter().cloned());
            pages += 1;
            if !page.more {
                assert_eq!(page.rev, head);
                break;
            }
            assert_eq!(
                page.rev,
                got.last().unwrap().rev(),
                "mid-feed the cursor is the last row's rev"
            );
            cursor = page.rev;
        }
        assert_eq!(got, whole.rows, "budget {budget}");
        if budget == 1 {
            assert_eq!(pages, 25, "one row per page, however small the budget");
        }
        if budget == 100_000 {
            assert_eq!(pages, 1);
        }
    }
}

#[test]
fn the_cursor_moves_past_revs_spent_on_other_scopes() {
    let s = store();
    let a = insert_session(&s, None);
    let first = s
        .changes_since(Scope::Session(a.id), Some(0), 10_000)
        .unwrap();
    assert!(first.rows.is_empty());
    assert_eq!(
        first.rev, 1,
        "the session's own rev was spent, the client may skip it"
    );
    s.write(|tx| tx.put_item(&item(a.id, "i1", "hello")))
        .unwrap();
    s.write(|tx| tx.create_issue("acme/webshop", "other scope", "", NOW))
        .unwrap();
    let next = s
        .changes_since(Scope::Session(a.id), Some(first.rev), 10_000)
        .unwrap();
    assert_eq!((next.rows.len(), next.rev), (1, 3));
}

#[test]
fn session_scope_carries_only_that_sessions_items_and_tombstones() {
    let s = store();
    let (a, b) = (insert_session(&s, None), insert_session(&s, None));
    s.write(|tx| {
        for i in 0..4 {
            tx.put_item(&item(a.id, &format!("a{i}"), "a"))?;
            tx.put_item(&item(b.id, &format!("b{i}"), "b"))?;
        }
        Ok(())
    })
    .unwrap();
    let cursor = s.rev().unwrap();
    s.write(|tx| {
        tx.delete_item(a.id, "a1")?;
        tx.delete_item(b.id, "b1")?;
        tx.put_item(&item(a.id, "a2", "changed"))?;
        tx.put_item(&item(a.id, "a9", "new"))
    })
    .unwrap();

    let got = s
        .changes_since(Scope::Session(a.id), Some(cursor), usize::MAX)
        .unwrap();
    let shape: Vec<String> = got
        .rows
        .iter()
        .map(|r| match r {
            Row::Item(i) => format!("item {}", i.item_id),
            Row::Tombstone {
                entity: Entity::Item,
                id,
                ..
            } => format!("tomb {id}"),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        shape,
        [
            format!("tomb {}/a1", a.id),
            "item a2".into(),
            "item a9".into()
        ]
    );
    // and the global feed knows nothing about items
    assert!(s
        .changes_since(Scope::Global, Some(cursor), usize::MAX)
        .unwrap()
        .rows
        .is_empty());

    // a deleted item that comes back loses its tombstone
    s.write(|tx| tx.put_item(&item(a.id, "a1", "back")))
        .unwrap();
    let again = s
        .changes_since(Scope::Session(a.id), Some(cursor), usize::MAX)
        .unwrap();
    assert!(
        !again
            .rows
            .iter()
            .any(|r| matches!(r, Row::Tombstone { .. })),
        "{again:?}"
    );
    assert_eq!(again.rows.len(), 3);
}

#[test]
fn a_sessions_full_set_is_its_newest_200_items_oldest_first() {
    let s = store();
    let a = insert_session(&s, None);
    s.write(|tx| {
        (1..=250).try_for_each(|i| tx.put_item(&item(a.id, &format!("i{i}"), "x")).map(drop))
    })
    .unwrap();
    let full = s.changes_since(Scope::Session(a.id), None, 1).unwrap();
    assert_eq!(INITIAL_ITEMS, 200);
    let seqs: Vec<u64> = full
        .rows
        .iter()
        .map(|r| match r {
            Row::Item(i) => i.seq,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(seqs, (51..=250).collect::<Vec<u64>>());
    assert_eq!(
        (full.more, full.reset, full.rev),
        (false, false, s.rev().unwrap())
    );
    // older ones come by paging
    let older = s
        .read(|c| items::page_items(c, a.id, Some(51), 100))
        .unwrap();
    assert_eq!(older.first().map(|i| i.seq), Some(50));
    assert_eq!(older.len(), 50);
    assert!(matches!(
        s.changes_since(Scope::Session(Id::new()), Some(0), 100),
        Err(Error::NotFound)
    ));
}

#[test]
fn a_cursor_older_than_the_tombstone_horizon_gets_a_reset_full_set() {
    let s = store();
    let keep = s
        .write_at(NOW, |tx| tx.create_issue("acme/webshop", "keep", "", NOW))
        .unwrap();
    let doomed = s
        .write_at(NOW, |tx| tx.create_issue("acme/webshop", "doomed", "", NOW))
        .unwrap();
    let stale = s.rev().unwrap(); // a client that last synced here
    s.write_at(NOW + DAY, |tx| tx.delete_issue(doomed.id))
        .unwrap();
    let tomb_rev = s.rev().unwrap();
    s.write_at(NOW + 2 * DAY, |tx| {
        tx.create_issue("acme/webshop", "later", "", NOW)
    })
    .unwrap();

    // before retention the stale client learns of the delete from the tombstone
    let delta = s
        .changes_since(Scope::Global, Some(stale), usize::MAX)
        .unwrap();
    assert!(!delta.reset);
    assert!(delta
        .rows
        .contains(&tomb(Entity::Issue, doomed.id, tomb_rev)));

    // 14 days and a bit after the delete, the tombstone is gone, and so is the right to a delta
    let report = s.retain(NOW + DAY + 15 * DAY).unwrap();
    assert_eq!(report.tombstones, 1);
    assert_eq!(
        s.read(|c| femme_store::meta_get(c, "tomb_horizon"))
            .unwrap(),
        Some(tomb_rev.to_string())
    );
    let reset = s.changes_since(Scope::Global, Some(stale), 1).unwrap();
    assert!(reset.reset, "the client cannot know what it missed");
    let titles: Vec<&str> = reset
        .rows
        .iter()
        .map(|r| match r {
            Row::Issue(i) => i.title.as_str(),
            other => panic!("a full set has live rows only: {other:?}"),
        })
        .collect();
    assert_eq!(titles, ["keep", "later"]);
    assert!(!reset.more, "a reset is a full set in one piece");
    // a cursor at or past the horizon is fine again
    let ok = s
        .changes_since(Scope::Global, Some(tomb_rev), usize::MAX)
        .unwrap();
    assert!(!ok.reset);
    assert_eq!(ok.rows.len(), 1);
    let _ = keep;
    // a session scope resets the same way
    let a = insert_session(&s, None);
    assert!(
        s.changes_since(Scope::Session(a.id), Some(stale), 100)
            .unwrap()
            .reset
    );
    assert!(
        !s.changes_since(Scope::Session(a.id), Some(s.rev().unwrap()), 100)
            .unwrap()
            .reset
    );
}

// ---------------------------------------------------------------------------------------------
// items and blobs

#[test]
fn small_bodies_are_stored_raw() {
    let s = store();
    let a = insert_session(&s, None);
    for (id, body) in [
        ("empty", String::new()),
        ("tiny", "hello".to_string()),
        ("edge", "é".repeat(INLINE_RAW_MAX / 2)),
    ] {
        assert!(body.len() <= INLINE_RAW_MAX);
        let put = s.write(|tx| tx.put_item(&item(a.id, id, &body))).unwrap();
        assert_eq!(put.body.as_deref(), Some(body.as_str()));
        assert_eq!(
            raw_body(&s, a.id, id).as_deref(),
            Some(body.as_bytes()),
            "{id}: stored as is"
        );
        assert_eq!(
            s.read(|c| items::get_item(c, a.id, id)).unwrap().unwrap(),
            put
        );
    }
    assert_eq!(count(&s, "blobs"), 0);
}

#[test]
fn medium_bodies_are_zstd_when_that_is_smaller() {
    let s = store();
    let a = insert_session(&s, None);
    let squishy = r#"{"text":"the same sentence again and again. "#.repeat(60); // ~2.6 KiB, very compressible
    assert!(squishy.len() > INLINE_RAW_MAX && squishy.len() <= INLINE_MAX);
    s.write(|tx| tx.put_item(&item(a.id, "squishy", &squishy)))
        .unwrap();
    let stored = raw_body(&s, a.id, "squishy").unwrap();
    assert!(stored.starts_with(&ZSTD_MAGIC), "zstd frame");
    assert!(stored.len() < squishy.len() / 4);
    let got = s
        .read(|c| items::get_item(c, a.id, "squishy"))
        .unwrap()
        .unwrap();
    assert_eq!(
        (got.body.as_deref(), got.blob_hash),
        (Some(squishy.as_str()), None)
    );

    // Whatever the content, what is stored is either the text itself or a smaller zstd frame,
    // and it always reads back unchanged.
    let mut zstd_seen = 0;
    for (n, len) in [513usize, 700, 1024, 2000, 3000, 4095, 4096]
        .into_iter()
        .enumerate()
    {
        let body = noise(len, n as u64);
        let id = format!("noise{len}");
        s.write(|tx| tx.put_item(&item(a.id, &id, &body))).unwrap();
        let stored = raw_body(&s, a.id, &id).unwrap();
        if stored.starts_with(&ZSTD_MAGIC) {
            zstd_seen += 1;
            assert!(
                stored.len() < body.len(),
                "{len}: zstd kept although it is not smaller"
            );
        } else {
            assert_eq!(stored, body.as_bytes(), "{len}");
        }
        let got = s.read(|c| items::get_item(c, a.id, &id)).unwrap().unwrap();
        assert_eq!(got.body.as_deref(), Some(body.as_str()), "{len}");
    }
    assert!(zstd_seen > 0, "ordinary text does compress");
    assert_eq!(count(&s, "blobs"), 0, "up to 4 KiB stays in the row");
}

#[test]
fn big_bodies_go_to_the_blob_store() {
    let s = store();
    let a = insert_session(&s, None);
    let body = noise(INLINE_MAX + 1, 7);
    let put = s
        .write(|tx| tx.put_item(&item(a.id, "big", &body)))
        .unwrap();
    assert_eq!(put.body, None);
    assert_eq!(put.blob_hash, Some(sha256(body.as_bytes())));
    assert_eq!(raw_body(&s, a.id, "big"), None, "the row holds no body");
    let got = s
        .read(|c| items::get_item(c, a.id, "big"))
        .unwrap()
        .unwrap();
    assert_eq!(got, put);
    assert_eq!(
        s.get_blob(&put.blob_hash.unwrap()).unwrap().as_deref(),
        Some(body.as_bytes())
    );
    assert_eq!(s.get_blob(&[7; 32]).unwrap(), None);
    // an item read from the store can be written back unchanged
    let again = s.write(|tx| tx.put_item(&got)).unwrap();
    assert_eq!((again.blob_hash, again.body), (put.blob_hash, None));
    assert_eq!(count(&s, "blobs"), 1);
}

#[test]
fn blobs_are_capped_at_one_mebibyte_and_say_so() {
    let s = store();
    let a = insert_session(&s, None);
    let exactly = "a".repeat(BLOB_MAX);
    let p = s
        .write(|tx| tx.put_item(&item(a.id, "exact", &exactly)))
        .unwrap();
    let stored = s.get_blob(&p.blob_hash.unwrap()).unwrap().unwrap();
    assert_eq!(stored.len(), BLOB_MAX, "exactly the cap is kept whole");
    assert!(!stored.ends_with(TRUNCATED.as_bytes()));

    // 2 MiB of 2-byte characters: cut on a character boundary, marked
    let huge = "é".repeat(BLOB_MAX);
    let p = s
        .write(|tx| tx.put_item(&item(a.id, "huge", &huge)))
        .unwrap();
    let stored = String::from_utf8(s.get_blob(&p.blob_hash.unwrap()).unwrap().unwrap())
        .expect("still valid UTF-8");
    assert!(stored.len() <= BLOB_MAX && stored.len() > BLOB_MAX - 8);
    assert!(stored.ends_with(TRUNCATED));
    assert_eq!(
        p.blob_hash,
        Some(sha256(stored.as_bytes())),
        "the hash covers what is stored"
    );
}

#[test]
fn uploaded_bytes_are_kept_whole_up_to_their_own_cap() {
    let s = store();
    // empty, not UTF-8, and longer than a transcript blob may be
    let long: Vec<u8> = (0..=255u8).cycle().take(BLOB_MAX + 123).collect();
    let cases: [&[u8]; 3] = [b"", b"\xff\xfe not utf-8 \x00", &long];
    for data in cases {
        let h = s.write(|tx| tx.put_blob_bytes(data)).unwrap();
        assert_eq!(h, sha256(data));
        assert_eq!(
            s.get_blob(&h).unwrap().as_deref(),
            Some(data),
            "{} bytes",
            data.len()
        );
    }
    let most = vec![7u8; BLOB_BYTES_MAX];
    let h = s.write(|tx| tx.put_blob_bytes(&most)).unwrap();
    assert_eq!(s.get_blob(&h).unwrap().unwrap().len(), BLOB_BYTES_MAX);

    let over = vec![7u8; BLOB_BYTES_MAX + 1];
    let refused = s.write(|tx| tx.put_blob_bytes(&over));
    assert!(
        matches!(refused, Err(Error::TooLarge { max }) if max == BLOB_BYTES_MAX),
        "{refused:?}"
    );
    assert_eq!(count(&s, "blobs"), 4, "the refused one left nothing behind");
}

#[test]
fn an_upload_is_one_row_however_often_put_and_retention_sweeps_it_as_an_orphan() {
    let s = store();
    let data = noise(10_000, 3).into_bytes();
    let h = s.write_at(NOW, |tx| tx.put_blob_bytes(&data)).unwrap();
    let again = s.write_at(NOW + 5, |tx| tx.put_blob_bytes(&data)).unwrap();
    assert_eq!(again, h);
    assert_eq!(count(&s, "blobs"), 1);
    let ts: i64 = s
        .read(|c| Ok(c.query_row("SELECT ts FROM blobs", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(ts, NOW + 5, "putting it again refreshes it");

    // A transcript body of the same text is the same blob, and then an item keeps it.
    let a = insert_session(&s, None);
    let text = String::from_utf8(data).unwrap();
    let put = s
        .write(|tx| tx.put_item(&item(a.id, "same", &text)))
        .unwrap();
    assert_eq!(put.blob_hash, Some(h));
    assert_eq!(s.retain(NOW + 10).unwrap().orphan_blobs, 0);
    assert!(s.get_blob(&h).unwrap().is_some());

    // Alone, it is an orphan like any blob no item refers to.
    let lone = s.write(|tx| tx.put_blob_bytes(b"attached")).unwrap();
    assert_eq!(s.retain(NOW + 20).unwrap().orphan_blobs, 1);
    assert_eq!(s.get_blob(&lone).unwrap(), None);
    assert!(s.get_blob(&h).unwrap().is_some());
}

#[test]
fn identical_blobs_are_stored_once_and_replaced_ones_go_away() {
    let s = store();
    let a = insert_session(&s, None);
    let (one, two) = (noise(6000, 1), noise(6000, 2));
    let h1 = s
        .write_at(NOW, |tx| tx.put_item(&item(a.id, "x", &one)))
        .unwrap()
        .blob_hash
        .unwrap();
    let h1b = s
        .write_at(NOW + 10, |tx| tx.put_item(&item(a.id, "y", &one)))
        .unwrap()
        .blob_hash
        .unwrap();
    assert_eq!(h1, h1b);
    assert_eq!(count(&s, "blobs"), 1, "same text, one blob");
    let ts: i64 = s
        .read(|c| Ok(c.query_row("SELECT ts FROM blobs", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(ts, NOW + 10, "writing it again refreshes it");

    // x streams on with new text: its old blob is still y's, so it stays
    let h2 = s
        .write(|tx| tx.put_item(&item(a.id, "x", &two)))
        .unwrap()
        .blob_hash
        .unwrap();
    assert_ne!(h1, h2);
    assert_eq!(count(&s, "blobs"), 2);
    assert!(s.get_blob(&h1).unwrap().is_some());
    // y moves on too: now nothing uses the first text, and it is gone at once, not at the nightly sweep
    s.write(|tx| tx.put_item(&item(a.id, "y", "short now")))
        .unwrap();
    assert_eq!(count(&s, "blobs"), 1);
    assert!(s.get_blob(&h1).unwrap().is_none());
    assert!(s.get_blob(&h2).unwrap().is_some());
    // growing a streamed body: every delta replaces the last blob, none piles up
    for n in 1..=20 {
        s.write(|tx| tx.put_item(&item(a.id, "stream", &noise(5000 + n * 10, 99 + n as u64))))
            .unwrap();
        assert_eq!(count(&s, "blobs"), 2, "after delta {n}");
    }
}

#[test]
fn previews_are_at_most_512_bytes() {
    let s = store();
    let a = insert_session(&s, None);
    // none given: the head of the body, cut on a character boundary
    let body = "é".repeat(2000);
    let p = s
        .write(|tx| tx.put_item(&item(a.id, "derived", &body)))
        .unwrap();
    assert_eq!(p.preview.len(), PREVIEW_MAX);
    assert!(body.starts_with(&p.preview));
    // given: kept, but still capped
    let mut it = item(a.id, "given", "the body");
    it.preview = "my own preview".into();
    assert_eq!(
        s.write(|tx| tx.put_item(&it)).unwrap().preview,
        "my own preview"
    );
    it.item_id = "long".into();
    it.preview = "ü".repeat(400); // 800 bytes
    let p = s.write(|tx| tx.put_item(&it)).unwrap();
    assert_eq!(p.preview.len(), PREVIEW_MAX);
    assert_eq!(
        s.read(|c| items::get_item(c, a.id, "long"))
            .unwrap()
            .unwrap()
            .preview,
        p.preview
    );
    // a body-less item keeps the preview it brings
    let mut bare = item(a.id, "bare", "");
    (bare.body, bare.preview) = (None, "just a preview".into());
    let p = s.write(|tx| tx.put_item(&bare)).unwrap();
    assert_eq!((p.body, p.preview.as_str()), (None, "just a preview"));
}

#[test]
fn seq_orders_the_transcript_and_survives_updates() {
    let s = store();
    let (a, b) = (insert_session(&s, None), insert_session(&s, None));
    let mut seqs = Vec::new();
    for i in 1..=10 {
        seqs.push(
            s.write(|tx| tx.put_item(&item(a.id, &format!("i{i}"), "x")))
                .unwrap()
                .seq,
        );
        s.write(|tx| tx.put_item(&item(b.id, &format!("j{i}"), "x")))
            .unwrap(); // another session does not disturb it
    }
    assert_eq!(seqs, (1..=10).collect::<Vec<u64>>());
    let edited = s
        .write(|tx| tx.put_item(&item(a.id, "i3", "edited")))
        .unwrap();
    assert_eq!(edited.seq, 3, "an update keeps the item's place");
    assert!(
        edited.rev
            > s.read(|c| Ok(items::get_item(c, a.id, "i10")?.unwrap().rev))
                .unwrap(),
        "but its rev is the newest"
    );

    // an explicit seq is honoured for a new item (a driver replaying history)
    let mut late = item(a.id, "late", "x");
    late.seq = 500;
    assert_eq!(s.write(|tx| tx.put_item(&late)).unwrap().seq, 500);
    assert_eq!(
        s.write(|tx| tx.put_item(&item(a.id, "after", "x")))
            .unwrap()
            .seq,
        501
    );

    // pages, newest first
    let page = |before: Option<u64>, n: usize| -> Vec<u64> {
        s.read(|c| {
            Ok(items::page_items(c, a.id, before, n)?
                .iter()
                .map(|i| i.seq)
                .collect())
        })
        .unwrap()
    };
    assert_eq!(page(None, 3), [501, 500, 10]);
    assert_eq!(
        page(None, usize::MAX).len(),
        12,
        "no limit is a legal limit"
    );
    assert_eq!(page(Some(10), 4), [9, 8, 7, 6]);
    assert_eq!(page(Some(6), 100), [5, 4, 3, 2, 1]);
    assert!(page(Some(1), 10).is_empty());
    assert!(page(Some(1), 0).is_empty());
}

#[test]
fn an_explicit_seq_must_be_free_and_an_existing_item_keeps_its_own() {
    let s = store();
    let a = insert_session(&s, None);
    let at = |id: &str, seq: u64| {
        let mut i = item(a.id, id, "x");
        i.seq = seq;
        i
    };
    s.write(|tx| tx.put_item(&at("x", 5))).unwrap();
    let dup = s.write(|tx| Ok(tx.put_item(&at("y", 5)))).unwrap();
    assert!(matches!(dup, Err(Error::Exists("seq"))), "{dup:?}");
    assert_eq!(count(&s, "items"), 1);
    // another session may use the same seq
    let b = insert_session(&s, None);
    let mut other = item(b.id, "x", "x");
    other.seq = 5;
    assert_eq!(s.write(|tx| tx.put_item(&other)).unwrap().seq, 5);

    // an existing item ignores the seq it is given, even one that belongs to someone else
    s.write(|tx| tx.put_item(&at("z", 6))).unwrap();
    assert_eq!(s.write(|tx| tx.put_item(&at("x", 6))).unwrap().seq, 5);
    // paging by seq therefore never meets a tie
    let page = s.read(|c| items::page_items(c, a.id, None, 10)).unwrap();
    assert_eq!(
        page.iter()
            .map(|i| (i.item_id.as_str(), i.seq))
            .collect::<Vec<_>>(),
        [("z", 6), ("x", 5)]
    );
    // the unique index is the backstop for anything that bypasses put_item
    let raw = s
        .write(|tx| {
            Ok(tx.execute(
                "UPDATE items SET seq = 6 WHERE session_id = ?1 AND item_id = 'x'",
                [a.id.0.to_be_bytes()],
            ))
        })
        .unwrap();
    assert!(raw.is_err());
}

#[test]
fn put_item_replaces_the_row_as_a_whole() {
    let s = store();
    let a = insert_session(&s, None);
    s.write(|tx| tx.put_item(&item(a.id, "i1", "the body")))
        .unwrap();
    // an item without a body drops the stored one: only what the caller gives survives
    let mut bare = item(a.id, "i1", "");
    bare.body = None;
    bare.status = ItemStatus::Failed;
    bare.preview = "the body".into();
    s.write(|tx| tx.put_item(&bare)).unwrap();
    assert_eq!(raw_body(&s, a.id, "i1"), None);
    let got = s.read(|c| items::get_item(c, a.id, "i1")).unwrap().unwrap();
    assert_eq!(
        (got.body, got.preview.as_str(), got.status),
        (None, "the body", ItemStatus::Failed)
    );

    // the way to change one field: read the item, modify it, write it back
    s.write(|tx| tx.put_item(&item(a.id, "i2", "keep me")))
        .unwrap();
    s.write(|tx| {
        let mut i = items::get_item(tx, a.id, "i2")?.unwrap();
        i.status = ItemStatus::Failed;
        tx.put_item(&i)
    })
    .unwrap();
    let got = s.read(|c| items::get_item(c, a.id, "i2")).unwrap().unwrap();
    assert_eq!(
        (got.body.as_deref(), got.status),
        (Some("keep me"), ItemStatus::Failed)
    );
}

#[test]
fn items_since_respects_the_budget_but_always_moves() {
    let s = store();
    let a = insert_session(&s, None);
    s.write(|tx| {
        (0..10).try_for_each(|i| {
            tx.put_item(&item(a.id, &format!("i{i}"), &"y".repeat(100)))
                .map(drop)
        })
    })
    .unwrap();
    let all = s
        .read(|c| items::items_since(c, a.id, 0, usize::MAX))
        .unwrap();
    assert_eq!((all.0.len(), all.1), (10, false));
    assert!(all.0.windows(2).all(|w| w[0].rev < w[1].rev));
    let (page, more) = s.read(|c| items::items_since(c, a.id, 0, 1)).unwrap();
    assert_eq!(
        (page.len(), more),
        (1, true),
        "an oversized row still comes out alone"
    );
    let (rest, more) = s
        .read(|c| items::items_since(c, a.id, page[0].rev, usize::MAX))
        .unwrap();
    assert_eq!((rest.len(), more), (9, false));
}

#[test]
fn deleting_items_leaves_tombstones() {
    let s = store();
    let a = insert_session(&s, None);
    s.write(|tx| {
        (0..5).try_for_each(|i| tx.put_item(&item(a.id, &format!("i{i}"), "x")).map(drop))
    })
    .unwrap();
    let before = s.rev().unwrap();
    s.write(|tx| tx.delete_item(a.id, "i2")).unwrap();
    assert!(matches!(
        s.write(|tx| tx.delete_item(a.id, "i2")),
        Err(Error::NotFound)
    ));
    assert_eq!(s.write(|tx| tx.delete_items(a.id)).unwrap(), 4);
    assert_eq!(count(&s, "items"), 0);
    let delta = s
        .changes_since(Scope::Session(a.id), Some(before), usize::MAX)
        .unwrap();
    assert_eq!(delta.rows.len(), 5, "one tombstone per deleted item");
    assert!(delta.rows.iter().all(|r| matches!(
        r,
        Row::Tombstone {
            entity: Entity::Item,
            ..
        }
    )));
    // a session that does not exist cannot take items
    let ghost = s.write(|tx| tx.put_item(&item(Id::new(), "x", "y")));
    assert!(matches!(ghost, Err(Error::NotFound)), "{ghost:?}");
}

// ---------------------------------------------------------------------------------------------
// issues

fn issue_of(s: &Store, id: u64) -> Issue {
    s.read(|c| issues::get_issue(c, id)).unwrap().unwrap()
}

#[test]
fn a_new_issue_is_open_and_numbered_from_one() {
    let s = store();
    let i = s
        .write(|tx| tx.create_issue("acme/webshop", "Fix it", "details", NOW))
        .unwrap();
    assert_eq!(
        (i.id, i.status, i.created, i.updated),
        (1, IssueStatus::Open, NOW, NOW)
    );
    assert_eq!(
        (
            i.note.as_deref(),
            i.pr_url.as_deref(),
            i.session,
            i.resume_at
        ),
        (None, None, None, None)
    );
    assert_eq!(issue_of(&s, 1), i);
    assert_eq!(
        s.write(|tx| tx.create_issue("a/b", "two", "", NOW))
            .unwrap()
            .id,
        2
    );
    assert_eq!(s.read(|c| Ok(issues::list_issues(c)?.len())).unwrap(), 2);
}

#[test]
fn issue_ids_are_never_reused() {
    let s = store();
    for _ in 0..3 {
        s.write(|tx| tx.create_issue("a/b", "t", "", NOW)).unwrap();
    }
    s.write(|tx| tx.delete_issue(3)).unwrap();
    assert!(matches!(
        s.write(|tx| tx.delete_issue(3)),
        Err(Error::NotFound)
    ));
    assert_eq!(
        s.write(|tx| tx.create_issue("a/b", "t", "", NOW))
            .unwrap()
            .id,
        4,
        "max(id)+1 would give 3 again"
    );
}

#[test]
fn edits_to_different_fields_merge_and_to_the_same_field_conflict() {
    let s = store();
    let i0 = s
        .write(|tx| tx.create_issue("acme/webshop", "Title", "Body", NOW))
        .unwrap();
    // two devices hold i0
    let a = s
        .write(|tx| {
            tx.patch_issue(
                i0.id,
                i0.rev,
                &[IssueField::Title("Title from A".into())],
                NOW + 1,
            )
        })
        .unwrap();
    let b = s
        .write(|tx| {
            tx.patch_issue(
                i0.id,
                i0.rev,
                &[IssueField::Body("Body from B".into())],
                NOW + 2,
            )
        })
        .unwrap();
    assert!(b.rev > a.rev);
    let now = issue_of(&s, i0.id);
    assert_eq!(
        (now.title.as_str(), now.body.as_str()),
        ("Title from A", "Body from B"),
        "nobody lost their work"
    );
    assert_eq!(now.updated, NOW + 2);

    // a third device, still on i0, edits the title A already changed: conflict naming the field and its rev
    let before = s.rev().unwrap();
    let c = s
        .write(|tx| {
            Ok(tx.patch_issue(
                i0.id,
                i0.rev,
                &[IssueField::Title("Title from C".into())],
                NOW + 3,
            ))
        })
        .unwrap();
    match c {
        Err(Error::Conflict { field_revs }) => {
            assert_eq!(
                field_revs.into_iter().collect::<Vec<_>>(),
                [("title".to_string(), a.rev)]
            );
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(s.rev().unwrap(), before, "a conflict writes nothing");
    assert_eq!(issue_of(&s, i0.id).title, "Title from A");

    // the same edit with the current rev goes through
    let c2 = s
        .write(|tx| {
            tx.patch_issue(
                i0.id,
                b.rev,
                &[IssueField::Title("Title from C".into())],
                NOW + 4,
            )
        })
        .unwrap();
    assert_eq!(c2.title, "Title from C");
    // the boundary: a client that has seen the very change that last touched the field is up to date,
    // one that is a single rev behind it is not
    let exact = s
        .write(|tx| {
            tx.patch_issue(
                i0.id,
                c2.rev,
                &[IssueField::Title("Title from D".into())],
                NOW + 5,
            )
        })
        .unwrap();
    assert_eq!(exact.title, "Title from D");
    let behind = s
        .write(|tx| {
            Ok(tx.patch_issue(
                i0.id,
                exact.rev - 1,
                &[IssueField::Title("Title from E".into())],
                NOW + 6,
            ))
        })
        .unwrap();
    assert!(matches!(behind, Err(Error::Conflict { .. })), "{behind:?}");
    let c2 = exact;

    // several fields at once: one conflicting field sinks the whole patch, and the error lists only the culprits
    let both = s.write(|tx| {
        Ok(tx.patch_issue(
            i0.id,
            i0.rev,
            &[
                IssueField::Title("T".into()),
                IssueField::Body("B".into()),
                IssueField::Repo("acme/other".into()),
            ],
            NOW,
        ))
    });
    match both.unwrap() {
        Err(Error::Conflict { field_revs }) => {
            assert_eq!(
                field_revs.keys().map(String::as_str).collect::<Vec<_>>(),
                ["body", "title"]
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        issue_of(&s, i0.id).repo,
        "acme/webshop",
        "the non-conflicting repo edit was not applied either"
    );
    let revs = s.read(|c| issues::issue_field_revs(c, i0.id)).unwrap();
    assert_eq!(revs.get("title"), Some(&c2.rev));
    assert_eq!(revs.get("body"), Some(&b.rev));
    assert_eq!(revs.get("repo"), None);
}

#[test]
fn an_unchanged_field_never_conflicts_and_a_noop_writes_nothing() {
    let s = store();
    let i0 = s
        .write(|tx| tx.create_issue("acme/webshop", "Title", "Body", NOW))
        .unwrap();
    let a = s
        .write(|tx| tx.patch_issue(i0.id, i0.rev, &[IssueField::Title("New".into())], NOW + 1))
        .unwrap();
    let head = s.rev().unwrap();
    // a stale client re-sends the whole form: title as it was last seen would conflict, so it is
    // "unchanged" only when it equals the current value
    let same = s
        .write(|tx| {
            tx.patch_issue(
                i0.id,
                i0.rev,
                &[
                    IssueField::Title("New".into()),
                    IssueField::Body("Body".into()),
                ],
                NOW + 9,
            )
        })
        .unwrap();
    assert_eq!(
        (same.rev, same.updated),
        (a.rev, NOW + 1),
        "nothing changed: no new rev, no new timestamp"
    );
    assert_eq!(s.rev().unwrap(), head);
    // note and pr_url: "" and none are the same thing
    let n = s
        .write(|tx| {
            tx.patch_issue(
                i0.id,
                a.rev,
                &[
                    IssueField::Note(Some("hello".into())),
                    IssueField::PrUrl(Some(String::new())),
                ],
                NOW + 2,
            )
        })
        .unwrap();
    assert_eq!(
        (n.note.as_deref(), n.pr_url.as_deref()),
        (Some("hello"), None)
    );
    let cleared = s
        .write(|tx| {
            tx.patch_issue(
                i0.id,
                n.rev,
                &[IssueField::Note(Some(String::new()))],
                NOW + 3,
            )
        })
        .unwrap();
    assert_eq!(cleared.note, None);
    assert!(matches!(
        s.write(|tx| tx.patch_issue(404, 0, &[], NOW)),
        Err(Error::NotFound)
    ));
    let text = Error::Conflict {
        field_revs: [("title".to_string(), 3)].into(),
    }
    .to_string();
    assert_eq!(text, "conflict on issue field(s): title");
}

#[test]
fn the_schedulers_transitions_never_conflict_but_stale_edits_of_those_fields_do() {
    let s = store();
    let i0 = s
        .write(|tx| tx.create_issue("acme/webshop", "Title", "Body", NOW))
        .unwrap();
    // the user opens the issue (rev i0.rev); meanwhile the scheduler picks it up
    let working = s
        .write(|tx| {
            tx.set_issue_status(i0.id, IssueStatus::Working, Some("started".into()), NOW + 1)
        })
        .unwrap();
    assert_eq!(
        (working.status, working.note.as_deref()),
        (IssueStatus::Working, Some("started"))
    );
    // the user edits the title from the old copy: fine, the scheduler did not touch it
    s.write(|tx| {
        tx.patch_issue(
            i0.id,
            i0.rev,
            &[IssueField::Title("Better title".into())],
            NOW + 2,
        )
    })
    .unwrap();
    // but pressing "reopen" from the old copy conflicts on status: the user has to look first
    let reopen = s
        .write(|tx| {
            Ok(tx.patch_issue(
                i0.id,
                i0.rev,
                &[IssueField::Status(IssueStatus::Open)],
                NOW + 3,
            ))
        })
        .unwrap();
    assert!(
        matches!(&reopen, Err(Error::Conflict { field_revs }) if field_revs.contains_key("status")),
        "{reopen:?}"
    );
    // set_issue_status(.., None) clears the note, as the worker's `note working ""` did
    let cleared = s
        .write(|tx| tx.set_issue_status(i0.id, IssueStatus::Working, None, NOW + 4))
        .unwrap();
    assert_eq!(cleared.note, None);
    // repeating it is free
    let head = s.rev().unwrap();
    s.write(|tx| tx.set_issue_status(i0.id, IssueStatus::Working, None, NOW + 5))
        .unwrap();
    assert_eq!(s.rev().unwrap(), head);

    // update_issue: the scheduler's multi-field change; session and resume time are its own
    let sess = insert_session(&s, None);
    let paused = s
        .write(|tx| {
            tx.update_issue(i0.id, NOW + 6, |i| {
                i.status = IssueStatus::Paused;
                i.resume_at = Some(NOW + DAY);
                i.session = Some(sess.id);
                i.id = 999; // not the caller's to change
                i.created = 1;
            })
        })
        .unwrap();
    assert_eq!((paused.id, paused.created), (i0.id, NOW));
    assert_eq!(
        (paused.status, paused.resume_at, paused.session),
        (IssueStatus::Paused, Some(NOW + DAY), Some(sess.id))
    );
    assert_eq!(
        s.read(|c| issues::issue_by_session(c, sess.id))
            .unwrap()
            .unwrap()
            .id,
        i0.id
    );
    let revs = s.read(|c| issues::issue_field_revs(c, i0.id)).unwrap();
    assert!(
        ["status", "resume_at", "session"]
            .iter()
            .all(|f| revs[*f] == paused.rev),
        "{revs:?}"
    );
    assert!(matches!(
        s.write(|tx| tx.update_issue(404, NOW, |_| {})),
        Err(Error::NotFound)
    ));
}

// ---------------------------------------------------------------------------------------------
// auth, ops, keybindings

fn spki(n: u8) -> [u8; 32] {
    [n; 32]
}

#[test]
fn devices_enroll_authenticate_and_revoke() {
    let s = store();
    let id = s
        .write(|tx| tx.enroll_device("laptop", &spki(1), NOW))
        .unwrap();
    let dev = s
        .read(|c| auth::device_by_spki(c, &spki(1)))
        .unwrap()
        .unwrap();
    assert_eq!(
        (dev.id, dev.name.as_str(), dev.spki_sha256, dev.created),
        (id, "laptop", spki(1), NOW)
    );
    assert_eq!((dev.last_seen, dev.revoked), (None, None));
    assert!(
        s.read(|c| auth::device_by_spki(c, &spki(2)))
            .unwrap()
            .is_none(),
        "an unknown certificate is nobody"
    );

    s.write(|tx| tx.touch_device(id, NOW + 5)).unwrap();
    assert_eq!(
        s.read(|c| auth::device_by_spki(c, &spki(1)))
            .unwrap()
            .unwrap()
            .last_seen,
        Some(NOW + 5)
    );

    // revoked: it can no longer connect, but it stays on the list with the time
    s.write_at(NOW + 10, |tx| tx.revoke_device(id)).unwrap();
    assert!(s
        .read(|c| auth::device_by_spki(c, &spki(1)))
        .unwrap()
        .is_none());
    let listed = s.read(auth::list_devices).unwrap();
    assert_eq!((listed.len(), listed[0].revoked), (1, Some(NOW + 10)));
    s.write_at(NOW + 99, |tx| tx.revoke_device(id)).unwrap();
    assert_eq!(
        s.read(auth::list_devices).unwrap()[0].revoked,
        Some(NOW + 10),
        "revoking twice keeps the first time"
    );
    assert!(matches!(
        s.write(|tx| tx.revoke_device(Id::new())),
        Err(Error::NotFound)
    ));

    // enrolling the same key again is an explicit act: same device, new name, back in
    let again = s
        .write(|tx| tx.enroll_device("laptop (new name)", &spki(1), NOW + 20))
        .unwrap();
    assert_eq!(again, id);
    let dev = s
        .read(|c| auth::device_by_spki(c, &spki(1)))
        .unwrap()
        .unwrap();
    assert_eq!(
        (dev.name.as_str(), dev.revoked, dev.created),
        ("laptop (new name)", None, NOW)
    );

    let phone = s
        .write(|tx| tx.enroll_device("phone", &spki(2), NOW + 30))
        .unwrap();
    assert_ne!(phone, id);
    let names: Vec<String> = s
        .read(auth::list_devices)
        .unwrap()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(names, ["laptop (new name)", "phone"]);
}

#[test]
fn web_sessions_are_hashed_and_expire() {
    let s = store();
    let (t1, t2) = (sha256(b"token-one"), sha256(b"token-two"));
    s.write(|tx| {
        tx.web_session_insert(&t1, NOW + 1000)?;
        tx.web_session_insert(&t2, NOW + 5000)
    })
    .unwrap();
    let valid = |t: &[u8; 32], at: i64| s.read(|c| auth::web_session_valid(c, t, at)).unwrap();
    assert!(valid(&t1, NOW) && valid(&t1, NOW + 999));
    assert!(!valid(&t1, NOW + 1000), "expired at its expiry");
    assert!(valid(&t2, NOW + 1000));
    assert!(!valid(&sha256(b"someone else"), NOW));
    let stored: Vec<Vec<u8>> = s
        .read(|c| {
            let mut st = c.prepare("SELECT token_sha256 FROM web_sessions")?;
            let rows = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            Ok(rows)
        })
        .unwrap();
    assert!(
        stored.iter().all(|t| t.len() == 32 && t != b"token-one"),
        "only digests are kept"
    );

    s.write(|tx| tx.web_session_insert(&t1, NOW + 9000))
        .unwrap(); // signing in again extends it
    assert!(valid(&t1, NOW + 5000));
    assert_eq!(
        s.write(|tx| tx.web_sessions_purge(NOW + 6000)).unwrap(),
        1,
        "only t2 had expired"
    );
    assert_eq!(count(&s, "web_sessions"), 1);
    s.write(|tx| tx.web_session_delete(&t1)).unwrap(); // logout
    s.write(|tx| tx.web_session_delete(&t1)).unwrap(); // and again: fine
    assert!(!valid(&t1, NOW));
}

#[test]
fn api_keys_are_unique_found_by_secret_and_can_carry_a_github_account() {
    let s = store();
    let k = s
        .write(|tx| tx.api_key_insert("instinct", "secret-1", NOW))
        .unwrap();
    assert_eq!(
        (k.name.as_str(), k.key.as_str(), k.created, k.github_token),
        ("instinct", "secret-1", NOW, None)
    );
    let dup = s
        .write(|tx| Ok(tx.api_key_insert("instinct", "secret-2", NOW)))
        .unwrap();
    assert!(matches!(dup, Err(Error::Exists("name"))));
    let dup = s
        .write(|tx| Ok(tx.api_key_insert("other", "secret-1", NOW)))
        .unwrap();
    assert!(matches!(dup, Err(Error::Exists("key"))));
    s.write(|tx| tx.api_key_insert("zeta", "secret-0", NOW + 1))
        .unwrap();

    let get = |secret: &str| s.read(|c| auth::api_key_get(c, secret)).unwrap();
    assert_eq!(get("secret-1").unwrap().name, "instinct");
    assert_eq!(get("secret-0").unwrap().name, "zeta");
    for wrong in [
        "",
        "secret",
        "secret-11",
        "SECRET-1",
        "secret-2",
        " secret-1",
    ] {
        assert!(get(wrong).is_none(), "{wrong:?} must not match");
    }
    assert_eq!(
        s.read(|c| auth::api_key_by_name(c, "zeta"))
            .unwrap()
            .unwrap()
            .key,
        "secret-0"
    );
    let names: Vec<String> = s
        .read(auth::api_keys_list)
        .unwrap()
        .into_iter()
        .map(|k| k.name)
        .collect();
    assert_eq!(names, ["instinct", "zeta"]);

    let with = s
        .write(|tx| {
            tx.api_key_update_github(
                "instinct",
                Some("gh-token"),
                Some("octocat"),
                Some("Octo Cat"),
                Some("1+octocat@users.noreply.github.com"),
            )
        })
        .unwrap();
    assert_eq!(
        (with.github_token.as_deref(), with.github_login.as_deref()),
        (Some("gh-token"), Some("octocat"))
    );
    assert_eq!(get("secret-1").unwrap(), with);
    let cleared = s
        .write(|tx| tx.api_key_update_github("instinct", None, None, None, None))
        .unwrap();
    assert_eq!((cleared.github_token, cleared.git_email), (None, None));
    assert!(matches!(
        s.write(|tx| tx.api_key_update_github("ghost", None, None, None, None)),
        Err(Error::NotFound)
    ));

    s.write(|tx| tx.api_key_delete("instinct")).unwrap();
    assert!(matches!(
        s.write(|tx| tx.api_key_delete("instinct")),
        Err(Error::NotFound)
    ));
    assert!(get("secret-1").is_none(), "a revoked key stops working");
}

#[test]
fn debug_output_never_contains_a_key_or_a_token() {
    let s = store();
    s.write(|tx| tx.api_key_insert("instinct", "sk-live-0123456789", NOW))
        .unwrap();
    let k = s
        .write(|tx| {
            tx.api_key_update_github(
                "instinct",
                Some("ghp_secret_token"),
                Some("octocat"),
                Some("Octo Cat"),
                None,
            )
        })
        .unwrap();
    let shown = format!(
        "{k:?} | {k:#?} | {:?}",
        s.read(auth::api_keys_list).unwrap()
    );
    assert!(
        !shown.contains("sk-live-0123456789") && !shown.contains("ghp_secret_token"),
        "{shown}"
    );
    assert!(
        shown.contains("instinct") && shown.contains("octocat") && shown.contains("<redacted>"),
        "{shown}"
    );
}

#[test]
fn a_retried_request_gets_the_first_answer_and_does_not_run_twice() {
    let s = store();
    let (dev, op) = (Id::new(), Id::new());
    assert!(s.read(|c| auth::op_lookup(c, dev, op)).unwrap().is_none());
    // the pattern for femmed: look up, else do the work and record the answer, in ONE transaction
    let request = |answer: &[u8]| {
        s.write(|tx| {
            if let Some(previous) = auth::op_lookup(tx, dev, op)? {
                return Ok((true, previous));
            }
            tx.create_issue("acme/webshop", "created once", "", NOW)?;
            tx.op_record(dev, op, answer, NOW)?;
            Ok((false, answer.to_vec()))
        })
        .unwrap()
    };
    assert_eq!(request(b"first"), (false, b"first".to_vec()));
    assert_eq!(request(b"second"), (true, b"first".to_vec()));
    assert_eq!(count(&s, "issues"), 1);

    // a direct second record does not overwrite either
    s.write(|tx| tx.op_record(dev, op, b"overwrite?", NOW + 1))
        .unwrap();
    assert_eq!(
        s.read(|c| auth::op_lookup(c, dev, op)).unwrap().unwrap(),
        b"first"
    );
    // the same op id from another device is another request
    assert!(s
        .read(|c| auth::op_lookup(c, Id::new(), op))
        .unwrap()
        .is_none());
    // recorded in a transaction that rolls back: forgotten with it
    let other = Id::new();
    let _ = s.write(|tx| {
        tx.op_record(dev, other, b"x", NOW)?;
        Err::<(), _>(Error::NotFound)
    });
    assert!(s
        .read(|c| auth::op_lookup(c, dev, other))
        .unwrap()
        .is_none());
}

#[test]
fn keybindings_are_json_per_device() {
    let s = store();
    let dev = Id::new();
    assert_eq!(s.read(|c| auth::keybindings_get(c, dev)).unwrap(), None);
    let rules = r#"[{"key":"cmd-k","command":"palette.open","when":null}]"#;
    s.write(|tx| tx.keybindings_set(dev, rules)).unwrap();
    assert_eq!(
        s.read(|c| auth::keybindings_get(c, dev))
            .unwrap()
            .as_deref(),
        Some(rules)
    );
    s.write(|tx| tx.keybindings_set(dev, "[]")).unwrap();
    assert_eq!(
        s.read(|c| auth::keybindings_get(c, dev))
            .unwrap()
            .as_deref(),
        Some("[]")
    );
    let bad = s.write(|tx| tx.keybindings_set(dev, "[{not json"));
    assert!(matches!(bad, Err(Error::Json(_))), "{bad:?}");
    assert_eq!(
        s.read(|c| auth::keybindings_get(c, dev))
            .unwrap()
            .as_deref(),
        Some("[]"),
        "a bad save changes nothing"
    );
    assert_eq!(
        s.read(|c| auth::keybindings_get(c, Id::new())).unwrap(),
        None
    );
}

// ---------------------------------------------------------------------------------------------
// retention

fn blob_hashes(s: &Store) -> HashSet<Vec<u8>> {
    s.read(|c| {
        let mut st = c.prepare("SELECT hash FROM blobs")?;
        let rows = st
            .query_map([], |r| r.get(0))?
            .collect::<Result<HashSet<Vec<u8>>, _>>()?;
        Ok(rows)
    })
    .unwrap()
}

#[test]
fn retention_purges_closed_sessions_after_30_days_and_nothing_else() {
    let s = store();
    let (old, recent, open) = s
        .write_at(NOW - 40 * DAY, |tx| {
            let (old, recent, open) = (
                tx.insert_session(&session(Some("old")))?,
                tx.insert_session(&session(Some("recent")))?,
                tx.insert_session(&session(Some("open")))?,
            );
            tx.upsert_pane(&pane(old.id, 0, "x"))?;
            for i in 0..3 {
                tx.put_item(&item(old.id, &format!("i{i}"), "small"))?;
            }
            tx.put_item(&item(old.id, "big", &noise(6000, 1)))?; // a blob only `old` uses
            tx.put_item(&item(recent.id, "i0", "small"))?;
            tx.put_item(&item(recent.id, "big", &noise(6000, 2)))?;
            tx.put_item(&item(open.id, "i0", "small"))?;
            tx.put_item(&item(open.id, "shared", &noise(6000, 2)))?; // the same text as recent's: one shared blob
            Ok((old, recent, open))
        })
        .unwrap();
    s.write(|tx| {
        tx.close_session(old.id, NOW - 31 * DAY)?;
        tx.close_session(recent.id, NOW - 29 * DAY)?;
        tx.create_issue("acme/webshop", "keep me", "", NOW - 100 * DAY)?;
        tx.api_key_insert("k", "secret", NOW - 100 * DAY)?;
        tx.enroll_device("laptop", &spki(1), NOW - 100 * DAY)?;
        Ok(())
    })
    .unwrap();
    assert_eq!((count(&s, "items"), count(&s, "blobs")), (8, 2));
    let before = s.rev().unwrap();

    let rep = s.retain(NOW).unwrap();
    assert_eq!(
        (rep.sessions, rep.items, rep.orphan_blobs, rep.evicted_blobs),
        (1, 4, 1, 0),
        "{rep:?}"
    );
    assert_eq!(
        rep.purged_sessions,
        vec![old.id],
        "the caller deletes the spool files of exactly these"
    );
    assert!(s
        .read(|c| sessions::get_session(c, old.id))
        .unwrap()
        .is_none());
    assert!(
        s.read(|c| sessions::get_session(c, recent.id))
            .unwrap()
            .is_some(),
        "closed 29 days ago: kept"
    );
    assert!(
        s.read(|c| sessions::get_session(c, open.id))
            .unwrap()
            .is_some(),
        "open sessions are never purged, however old"
    );
    assert_eq!(count(&s, "items"), 4);
    assert_eq!(
        count(&s, "blobs"),
        1,
        "the shared blob stays, the one only `old` used is swept"
    );
    assert_eq!(
        (
            count(&s, "issues"),
            count(&s, "api_keys"),
            count(&s, "devices")
        ),
        (1, 1, 1)
    );

    // clients learn of it as a session tombstone
    let delta = s
        .changes_since(Scope::Global, Some(before), usize::MAX)
        .unwrap()
        .rows;
    assert_eq!(delta.len(), 1, "{delta:?}");
    assert!(
        matches!(&delta[0], Row::Tombstone { entity: Entity::Session, id, .. } if *id == old.id.to_string())
    );
    // nothing left to do
    assert_eq!(s.retain(NOW).unwrap(), femme_store::RetainReport::default());
    // 30 days is the line: at exactly the ttl it is still kept, a moment later it goes
    let line = s.retain(NOW - 29 * DAY + 30 * DAY).unwrap();
    assert_eq!(line.sessions, 0);
    let gone = s.retain(NOW - 29 * DAY + 30 * DAY + 1).unwrap();
    assert_eq!((gone.sessions, gone.purged_sessions), (1, vec![recent.id]));
}

#[test]
fn a_failing_later_step_is_a_warning_and_the_purged_sessions_are_still_reported() {
    let (_dir, path) = tempdb();
    let s = Store::open(&path).unwrap();
    let old = s
        .write_at(NOW - 40 * DAY, |tx| {
            tx.insert_session(&session(Some("old")))
        })
        .unwrap();
    s.write(|tx| tx.close_session(old.id, NOW - 39 * DAY))
        .unwrap();
    s.write(|tx| {
        tx.op_record(Id::new(), Id::new(), b"x", NOW - 2 * DAY)?;
        tx.web_session_insert(&sha256(b"expired"), NOW - DAY)
    })
    .unwrap();
    // something breaks the ops cleanup (a second connection, like any other process on the file)
    let other = Connection::open(&path).unwrap();
    other.execute_batch("CREATE TRIGGER no_ops_delete BEFORE DELETE ON ops BEGIN SELECT RAISE(ABORT, 'boom'); END;").unwrap();

    let rep = s.retain(NOW).unwrap();
    assert_eq!(
        (rep.sessions, rep.purged_sessions.clone()),
        (1, vec![old.id]),
        "the caller must still learn what to delete: {rep:?}"
    );
    assert_eq!(rep.warnings.len(), 1, "{rep:?}");
    assert!(
        rep.warnings[0].starts_with("ops: ") && rep.warnings[0].contains("boom"),
        "{rep:?}"
    );
    assert_eq!(
        (rep.ops, rep.web_sessions),
        (0, 1),
        "the steps after the failed one still ran"
    );
    assert_eq!(count(&s, "ops"), 1);

    other.execute_batch("DROP TRIGGER no_ops_delete").unwrap();
    let again = s.retain(NOW).unwrap();
    assert_eq!(
        (
            again.ops,
            again.warnings.is_empty(),
            again.purged_sessions.len()
        ),
        (1, true, 0),
        "the next run does the work: {again:?}"
    );
}

#[test]
fn the_wal_checkpoint_does_not_make_anyone_wait_for_a_reader() {
    let (_dir, path) = tempdb();
    let s = Store::open(&path).unwrap();
    insert_session(&s, Some("something in the wal"));
    let (reading, release) = (Barrier::new(2), Barrier::new(2));
    let took = std::thread::scope(|scope| {
        scope.spawn(|| {
            s.read(|c| {
                c.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))?; // the snapshot starts here
                reading.wait();
                release.wait();
                Ok(())
            })
            .unwrap();
        });
        reading.wait();
        let t = std::time::Instant::now();
        s.retain(NOW).unwrap();
        s.write(|tx| tx.create_issue("acme/webshop", "written while a reader is open", "", NOW))
            .unwrap();
        let took = t.elapsed();
        release.wait();
        took
    });
    assert!(
        took < std::time::Duration::from_secs(2),
        "retention and the write waited {took:?} for a reader"
    );
    // the reader is gone: the next run empties the WAL
    s.retain(NOW).unwrap();
    assert_eq!(
        std::fs::metadata(path.with_extension("db-wal")).map_or(0, |m| m.len()),
        0
    );
}

#[test]
fn retention_expires_tombstones_ops_and_web_sessions_on_their_own_clocks() {
    let s = store();
    let dev = Id::new();
    let (old_op, new_op) = (Id::new(), Id::new());
    let issues: Vec<Issue> = (0..3)
        .map(|i| {
            s.write_at(NOW, |tx| {
                tx.create_issue("acme/webshop", &format!("t{i}"), "", NOW)
            })
            .unwrap()
        })
        .collect();
    // tombstones: 14 d 1 h old (purged), 13 d 23 h old (kept)
    s.write_at(NOW - 14 * DAY - 3_600_000, |tx| {
        tx.delete_issue(issues[0].id)
    })
    .unwrap();
    let purged_rev = s.rev().unwrap();
    s.write_at(NOW - 14 * DAY + 3_600_000, |tx| {
        tx.delete_issue(issues[1].id)
    })
    .unwrap();
    let kept_rev = s.rev().unwrap();
    // ops: 25 h old (purged), 23 h old (kept)
    s.write(|tx| {
        tx.op_record(dev, old_op, b"a", NOW - 25 * 3_600_000)?;
        tx.op_record(dev, new_op, b"b", NOW - 23 * 3_600_000)
    })
    .unwrap();
    // web sessions: expired a moment ago / still good
    let (gone, good) = (sha256(b"gone"), sha256(b"good"));
    s.write(|tx| {
        tx.web_session_insert(&gone, NOW - 1)?;
        tx.web_session_insert(&good, NOW + 1)
    })
    .unwrap();

    let rep = s.retain(NOW).unwrap();
    assert_eq!(
        (rep.tombstones, rep.ops, rep.web_sessions),
        (1, 1, 1),
        "{rep:?}"
    );
    assert_eq!((rep.sessions, rep.items, rep.orphan_blobs), (0, 0, 0));
    assert!(s
        .read(|c| auth::op_lookup(c, dev, old_op))
        .unwrap()
        .is_none());
    assert!(s
        .read(|c| auth::op_lookup(c, dev, new_op))
        .unwrap()
        .is_some());
    assert!(!s
        .read(|c| auth::web_session_valid(c, &gone, NOW - 2))
        .unwrap());
    assert!(s.read(|c| auth::web_session_valid(c, &good, NOW)).unwrap());
    // the horizon is the newest rev among the tombstones that went
    let horizon = s
        .read(|c| femme_store::meta_get(c, "tomb_horizon"))
        .unwrap();
    assert_eq!(horizon, Some(purged_rev.to_string()));
    assert!(
        s.changes_since(Scope::Global, Some(purged_rev - 1), usize::MAX)
            .unwrap()
            .reset
    );
    let at = s
        .changes_since(Scope::Global, Some(purged_rev), usize::MAX)
        .unwrap();
    assert!(!at.reset);
    assert!(
        at.rows
            .contains(&tomb(Entity::Issue, issues[1].id, kept_rev)),
        "the younger tombstone is still served"
    );
    // the horizon never goes back
    s.retain(NOW + 30 * DAY).unwrap();
    let later = s
        .read(|c| femme_store::meta_get(c, "tomb_horizon"))
        .unwrap()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert_eq!(later, kept_rev);
    s.retain(NOW).unwrap();
    assert_eq!(
        s.read(|c| femme_store::meta_get(c, "tomb_horizon"))
            .unwrap()
            .unwrap()
            .parse::<u64>()
            .unwrap(),
        later
    );
}

#[test]
fn retention_evicts_the_oldest_blobs_once_over_the_cap() {
    let s = store();
    let a = insert_session(&s, None);
    let mut hashes = Vec::new();
    for n in 0..4u64 {
        let put = s
            .write_at(NOW + n as i64, |tx| {
                tx.put_item(&item(a.id, &format!("b{n}"), &noise(8000, 10 + n)))
            })
            .unwrap();
        hashes.push(put.blob_hash.unwrap());
    }
    let sizes: Vec<u64> = s
        .read(|c| {
            let mut st = c.prepare("SELECT length(data) FROM blobs ORDER BY ts")?;
            let rows = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            Ok(rows)
        })
        .unwrap();
    let caps = Caps {
        blob_total_bytes: sizes[2] + sizes[3],
        ..Caps::default()
    };
    let rev = s.rev().unwrap();

    let rep = s.retain_with(NOW + 10, &caps).unwrap();
    assert_eq!(rep.evicted_blobs, 2);
    assert_eq!(
        blob_hashes(&s),
        HashSet::from([hashes[2].to_vec(), hashes[3].to_vec()]),
        "the two newest stay"
    );
    assert_eq!(
        s.rev().unwrap(),
        rev + 2,
        "each evicted item gets a rev, so a client that holds its hash learns it is gone"
    );
    let delta = s
        .changes_since(femme_proto::rpc::Scope::Session(a.id), Some(rev), 1 << 20)
        .unwrap();
    assert_eq!(delta.rows.len(), 2);
    for (n, gone) in [(0, true), (1, true), (2, false), (3, false)] {
        let it = s
            .read(|c| items::get_item(c, a.id, &format!("b{n}")))
            .unwrap()
            .unwrap();
        assert_eq!(it.blob_hash.is_none(), gone, "b{n}");
        assert_eq!(it.body, None);
        assert_eq!(
            it.preview.len(),
            PREVIEW_MAX,
            "the preview outlives the blob"
        );
        assert_eq!(s.get_blob(&hashes[n]).unwrap().is_none(), gone);
    }
    // now at the cap: nothing more to evict
    assert_eq!(s.retain_with(NOW + 11, &caps).unwrap().evicted_blobs, 0);
    // a cap below one blob evicts everything
    let rep = s
        .retain_with(
            NOW + 12,
            &Caps {
                blob_total_bytes: 1,
                ..Caps::default()
            },
        )
        .unwrap();
    assert_eq!(rep.evicted_blobs, 2);
    assert_eq!(count(&s, "blobs"), 0);
}

#[test]
fn retention_returns_disk_to_the_filesystem() {
    let (_dir, path) = tempdb();
    let s = Store::open(&path).unwrap();
    let a = insert_session(&s, Some("big"));
    s.write(|tx| {
        (0..60u64).try_for_each(|n| {
            tx.put_item(&item(a.id, &format!("b{n}"), &noise(100_000, n)))
                .map(drop)
        })
    })
    .unwrap();
    let full = s.db_size_bytes().unwrap();
    assert!(full > 3_000_000, "{full}");
    s.write(|tx| tx.delete_session(a.id)).unwrap(); // rows gone; the blobs have no owner now
    assert!(
        s.db_size_bytes().unwrap() >= full * 9 / 10,
        "a delete alone frees nothing"
    );

    let rep = s.retain(NOW).unwrap();
    assert_eq!(rep.orphan_blobs, 60);
    assert!(rep.pages_vacuumed > 500, "{rep:?}");
    let after = s.db_size_bytes().unwrap();
    assert!(after < full / 10, "{after} of {full}");
    // the checkpoint truncated the WAL too, so the file on disk really is that size
    assert_eq!(std::fs::metadata(&path).unwrap().len(), after);
    let wal = std::fs::metadata(path.with_extension("db-wal")).map_or(0, |m| m.len());
    assert_eq!(wal, 0);
}

// ---------------------------------------------------------------------------------------------
// import / export of the old board's files

fn fixture_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/board");
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
    }
    dir
}

fn json_file(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

const BOARD_FILES: [&str; 4] = ["issues.json", "sessions.json", "api-keys.json", "meta.json"];

#[test]
fn the_boards_files_import_once_and_move_to_backups() {
    let s = store();
    let dir = fixture_dir();
    let rep = s.import_board_json(dir.path(), NOW).unwrap();
    assert!(!rep.already_imported);
    assert_eq!(
        (
            rep.issues,
            rep.web_sessions,
            rep.web_sessions_expired,
            rep.api_keys,
            rep.repos
        ),
        (6, 1, 1, 3, 3),
        "{rep:?}"
    );
    let dropped: Vec<(&str, usize)> = rep
        .dropped_fields
        .iter()
        .map(|(k, v)| (k.as_str(), *v))
        .collect();
    assert_eq!(dropped, [("github_error", 1), ("pane", 3), ("report", 1)]);
    assert_eq!(
        rep.warnings.len(),
        1,
        "the one triage issue: {:?}",
        rep.warnings
    );
    assert!(rep.warnings[0].contains("triage"));

    // moved, not left: the originals are gone and copies are in backups/<unix seconds>/
    let backup = rep.backup_dir.clone().unwrap();
    assert_eq!(backup, dir.path().join("backups").join("1793000000"));
    for f in BOARD_FILES {
        assert!(
            !dir.path().join(f).exists(),
            "{f} is still where the board looks"
        );
        assert!(backup.join(f).exists(), "{f} is not in the backup");
    }

    // issues: ids kept, seconds became milliseconds, "" became none
    let ids: Vec<u64> = s
        .read(|c| Ok(issues::list_issues(c)?.iter().map(|i| i.id).collect()))
        .unwrap();
    assert_eq!(ids, [1, 2, 3, 5, 6, 7]);
    let i1 = issue_of(&s, 1);
    assert_eq!(
        (i1.repo.as_str(), i1.status, i1.created, i1.updated),
        (
            "acme/webshop",
            IssueStatus::Done,
            1_790_000_000_000,
            1_790_003_600_000
        )
    );
    assert_eq!(
        i1.pr_url.as_deref(),
        Some("https://github.com/acme/webshop/pull/12")
    );
    assert_eq!((i1.note, i1.session), (None, None));
    assert_eq!(
        i1.body,
        "Clicking Pay shows no spinner.\n\nRepro: Safari 17, cart with 2 items."
    );
    let i3 = issue_of(&s, 3);
    assert_eq!(
        (i3.status, i3.resume_at),
        (IssueStatus::Paused, Some(1_793_010_000_000))
    );
    assert_eq!(
        i3.note.as_deref(),
        Some("paused at 61%: resumes when the window resets")
    );
    let i5 = issue_of(&s, 5);
    assert_eq!(
        i5.status,
        IssueStatus::Failed,
        "a triage issue must not come back as open: the scheduler would start it"
    );
    assert_eq!(i5.note.as_deref(), Some("was \"triage\" on the old board"));
    assert_eq!(issue_of(&s, 6).title, "Fix déjà-vu in the naïve parser 🙂");
    let i7 = issue_of(&s, 7);
    assert_eq!(
        (i7.status, i7.body.as_str(), i7.created, i7.updated),
        (IssueStatus::Open, "", 1_792_950_000_000, 1_792_950_000_000)
    );
    let revs: HashSet<u64> = s
        .read(|c| Ok(issues::list_issues(c)?.iter().map(|i| i.rev).collect()))
        .unwrap();
    assert_eq!(revs.len(), 6, "every imported row got its own rev");
    assert_eq!(
        s.write(|tx| tx.create_issue("a/b", "new", "", NOW))
            .unwrap()
            .id,
        8,
        "numbering continues after the highest id"
    );

    // web sessions: hashed, expired one skipped
    let (valid, expired) = (
        sha256(b"fixture-token-valid-0001"),
        sha256(b"fixture-token-expired-0002"),
    );
    assert!(s.read(|c| auth::web_session_valid(c, &valid, NOW)).unwrap());
    assert!(
        !s.read(|c| auth::web_session_valid(c, &expired, NOW - 8 * DAY))
            .unwrap(),
        "never stored"
    );
    assert_eq!(count(&s, "web_sessions"), 1);
    assert!(s
        .read(|c| auth::web_session_valid(c, &valid, 1_793_604_800_000 - 1))
        .unwrap());
    assert!(
        !s.read(|c| auth::web_session_valid(c, &valid, 1_793_604_800_000))
            .unwrap(),
        "expiry kept to the second"
    );

    // api keys
    let reviewer = s
        .read(|c| auth::api_key_get(c, &format!("fixture-key-{}", "b".repeat(56))))
        .unwrap()
        .unwrap();
    assert_eq!(
        (reviewer.name.as_str(), reviewer.created),
        ("reviewer", 1_791_000_000_000)
    );
    assert_eq!(
        reviewer.github_token.as_deref(),
        Some("fixture_github_token_0001")
    );
    assert_eq!(
        reviewer.git_email.as_deref(),
        Some("1234+acme-reviewer@users.noreply.github.com")
    );
    let plain = s
        .read(|c| auth::api_key_by_name(c, "instinct-bot"))
        .unwrap()
        .unwrap();
    assert_eq!((plain.github_token, plain.git_name), (None, None));
    assert_eq!(s.read(auth::api_keys_list).unwrap().len(), 3);

    // repos
    assert_eq!(
        s.read(femme_store::repos).unwrap(),
        ["acme/webshop", "acme/infra", "octo/hello-world"]
    );

    // one shot: files that appear later are not read again
    for f in BOARD_FILES {
        std::fs::copy(backup.join(f), dir.path().join(f)).unwrap();
    }
    let again = s.import_board_json(dir.path(), NOW + 1).unwrap();
    assert!(again.already_imported);
    assert_eq!(
        again,
        femme_store::ImportReport {
            already_imported: true,
            ..Default::default()
        }
    );
    assert_eq!(count(&s, "issues"), 7);
    assert!(
        dir.path().join("issues.json").exists(),
        "an import that did not run does not touch the files"
    );
}

#[test]
fn export_writes_the_boards_format_and_re_imports_to_the_same_state() {
    let s = store();
    let dir = fixture_dir();
    s.import_board_json(dir.path(), NOW).unwrap();
    s.write(|tx| tx.web_session_insert(&sha256(b"still signed in"), 4_102_444_800_000))
        .unwrap(); // year 2100
    let out = tempfile::tempdir().unwrap().keep().join("state"); // does not exist yet
    let rep = s.export_board_json(&out).unwrap();
    assert_eq!((rep.issues, rep.api_keys, rep.repos), (6, 3, 3));
    assert!(
        rep.web_sessions_lost >= 1,
        "their tokens are not stored, so they cannot go back"
    );

    use std::os::unix::fs::PermissionsExt;
    for f in BOARD_FILES {
        let mode = std::fs::metadata(out.join(f)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{f}");
    }
    assert!(
        !out.join("issues.json.femme-tmp").exists(),
        "temp files are renamed away"
    );
    assert_eq!(json_file(&out.join("sessions.json")), serde_json::json!({}));
    let meta = json_file(&out.join("meta.json"));
    assert_eq!(
        meta["repos"],
        serde_json::json!(["acme/webshop", "acme/infra", "octo/hello-world"])
    );
    assert!(meta["updated"].is_number() && meta["cap"].is_number() && meta["state"].is_string());

    // issues: seconds again, optional fields absent when none; what the old board never had is not invented
    let want = serde_json::json!([
        {"id": 1, "repo": "acme/webshop", "title": "Checkout button does nothing on Safari",
         "body": "Clicking Pay shows no spinner.\n\nRepro: Safari 17, cart with 2 items.", "status": "done",
         "created": 1_790_000_000, "updated": 1_790_003_600, "pr_url": "https://github.com/acme/webshop/pull/12"},
        {"id": 2, "repo": "acme/webshop", "title": "Add dark mode", "body": "", "status": "working",
         "created": 1_792_000_000, "updated": 1_792_990_000},
        {"id": 3, "repo": "acme/infra", "title": "Rotate the staging certificates", "body": "Both certs expire next month.",
         "status": "paused", "created": 1_792_100_000, "updated": 1_792_900_000, "resume_at": 1_793_010_000,
         "note": "paused at 61%: resumes when the window resets"},
        {"id": 5, "repo": "acme/webshop", "title": "Is the cart total rounded correctly?", "body": "Customer says 0.5 cent differences.",
         "status": "failed", "created": 1_791_000_000, "updated": 1_791_000_500, "note": "was \"triage\" on the old board"},
        {"id": 6, "repo": "octo/hello-world", "title": "Fix déjà-vu in the naïve parser 🙂", "body": "The parser re-reads its input.",
         "status": "failed", "created": 1_792_500_000, "updated": 1_792_600_000, "note": "its workspace was closed"},
        {"id": 7, "repo": "octo/hello-world", "title": "Write the README", "body": "", "status": "open",
         "created": 1_792_950_000, "updated": 1_792_950_000},
    ]);
    assert_eq!(json_file(&out.join("issues.json")), want);
    let keys = json_file(&out.join("api-keys.json"));
    assert_eq!(keys.as_array().unwrap().len(), 3);
    assert_eq!(keys[1]["github_login"], "acme-reviewer");
    assert_eq!(keys[1]["created"], 1_791_000_000);
    assert!(keys[0].get("github_token").is_none() && keys[2].get("github_error").is_none());

    // the board can read it back: importing it into a fresh store gives the same issues, keys and repos
    let twin = store();
    let report = twin.import_board_json(&out, NOW).unwrap();
    assert_eq!(
        (
            report.issues,
            report.api_keys,
            report.repos,
            report.web_sessions
        ),
        (6, 3, 3, 0)
    );
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(
        twin.read(issues::list_issues).unwrap(),
        s.read(issues::list_issues).unwrap()
    );
    assert_eq!(
        twin.read(auth::api_keys_list).unwrap(),
        s.read(auth::api_keys_list).unwrap()
    );
    assert_eq!(
        twin.read(femme_store::repos).unwrap(),
        s.read(femme_store::repos).unwrap()
    );
}

#[test]
fn export_ignores_stale_temp_files_and_never_widens_the_mode() {
    use std::os::unix::fs::PermissionsExt;
    let s = store();
    let dir = fixture_dir();
    s.import_board_json(dir.path(), NOW).unwrap();
    let out = tempfile::tempdir().unwrap();
    for f in BOARD_FILES {
        // left behind by a crashed export, and world-readable
        let stale = out.path().join(format!("{f}.femme-tmp"));
        std::fs::write(&stale, "stale").unwrap();
        std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    std::fs::write(out.path().join("meta.tmp"), "worker.sh is writing this").unwrap(); // the board's own temp name
    s.export_board_json(out.path()).unwrap();
    for f in BOARD_FILES {
        let mode = std::fs::metadata(out.path().join(f))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{f}");
        assert!(!out.path().join(format!("{f}.femme-tmp")).exists(), "{f}");
    }
    assert_eq!(
        std::fs::read_to_string(out.path().join("meta.tmp")).unwrap(),
        "worker.sh is writing this"
    );
}

#[test]
fn a_file_that_is_not_the_boards_aborts_the_import_before_any_change() {
    let bad = [
        ("api-keys.json", "{ not json"),
        ("issues.json", "{}"),
        ("issues.json", r#"[{"id":1,"repo":"a/b"}]"#),
        ("issues.json", r#"[{"id":0,"repo":"a/b","title":"t"}]"#),
        (
            "issues.json",
            r#"[{"id":9223372036854775807,"repo":"a/b","title":"t"}]"#,
        ), // no room for the next id
        (
            "issues.json",
            r#"[{"id":18446744073709551615,"repo":"a/b","title":"t"}]"#,
        ), // not an SQLite integer
        ("issues.json", r#"[{"id":1,"repo":7,"title":"t"}]"#),
        (
            "issues.json",
            r#"[{"id":1,"repo":"a/b","title":"t","created":"yesterday"}]"#,
        ),
        ("sessions.json", r#"{"tok-secret-value":"soon"}"#),
        ("sessions.json", "[]"),
        ("api-keys.json", r#"[{"name":"x"}]"#),
        ("meta.json", "[]"),
    ];
    for (file, text) in bad {
        let s = store();
        let dir = fixture_dir();
        std::fs::write(dir.path().join(file), text).unwrap();
        let err = s.import_board_json(dir.path(), NOW).unwrap_err();
        match &err {
            Error::Corrupt(why) => {
                assert!(
                    why.contains(file.trim_end_matches(".json")),
                    "{file}: {why}"
                );
                assert!(
                    !why.contains("tok-secret-value"),
                    "an error must not print a session token"
                );
            }
            other => panic!("{file} {text}: expected Corrupt, got {other:?}"),
        }
        // nothing happened: no data, no backup, the originals untouched, and not marked as done
        assert_eq!(
            (
                count(&s, "issues"),
                count(&s, "api_keys"),
                count(&s, "web_sessions")
            ),
            (0, 0, 0)
        );
        assert!(!dir.path().join("backups").exists(), "{file}");
        for f in BOARD_FILES {
            assert!(dir.path().join(f).exists(), "{f} was removed");
        }
        assert_eq!(
            s.read(|c| femme_store::meta_get(c, "json_imported"))
                .unwrap(),
            None
        );
    }
    // fix the file and it goes through
    let s = store();
    let dir = fixture_dir();
    std::fs::write(dir.path().join("api-keys.json"), "oops").unwrap();
    assert!(s.import_board_json(dir.path(), NOW).is_err());
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/board/api-keys.json"),
        dir.path().join("api-keys.json"),
    )
    .unwrap();
    assert_eq!(s.import_board_json(dir.path(), NOW).unwrap().issues, 6);
}

#[test]
fn an_empty_state_dir_imports_nothing_and_counts_as_done() {
    let s = store();
    let dir = tempfile::tempdir().unwrap();
    let rep = s.import_board_json(dir.path(), NOW).unwrap();
    assert_eq!(rep, femme_store::ImportReport::default());
    assert!(!dir.path().join("backups").exists());
    // a board file that shows up later must not be imported over live state
    std::fs::write(
        dir.path().join("issues.json"),
        r#"[{"id":1,"repo":"a/b","title":"late"}]"#,
    )
    .unwrap();
    assert!(
        s.import_board_json(dir.path(), NOW)
            .unwrap()
            .already_imported
    );
    assert_eq!(count(&s, "issues"), 0);
}

#[test]
fn duplicates_and_odd_statuses_are_reported_not_fatal() {
    let s = store();
    s.write(|tx| tx.api_key_insert("other", "s1", NOW)).unwrap(); // already there when the import runs
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("issues.json"),
        r#"[{"id":1,"repo":"a/b","title":"first"},
            {"id":1,"repo":"a/b","title":"second"},
            {"id":2,"repo":"a/b","title":"odd","status":"weird","note":"keep this"}]"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("api-keys.json"),
        r#"[{"name":"k","key":"s0"},{"name":"k","key":"s2"},{"name":"third","key":"s1"}]"#,
    )
    .unwrap();
    let rep = s.import_board_json(dir.path(), NOW).unwrap();
    assert_eq!((rep.issues, rep.api_keys), (2, 1), "{rep:?}");
    assert_eq!(rep.warnings.len(), 4, "{:?}", rep.warnings);
    assert_eq!(issue_of(&s, 1).title, "first");
    let odd = issue_of(&s, 2);
    assert_eq!(odd.status, IssueStatus::Failed);
    assert_eq!(
        odd.note.as_deref(),
        Some("keep this (was \"weird\" on the old board)")
    );
    assert!(s.read(|c| auth::api_key_get(c, "s0")).unwrap().is_some());
    assert!(
        s.read(|c| auth::api_key_get(c, "s2")).unwrap().is_none(),
        "second `k` lost to the first"
    );
    assert_eq!(
        s.read(|c| auth::api_key_get(c, "s1"))
            .unwrap()
            .unwrap()
            .name,
        "other",
        "the existing key wins"
    );
}

// ---------------------------------------------------------------------------------------------
// micro-benchmark (not part of the normal run)

fn percentile(sorted: &[std::time::Duration], p: usize) -> std::time::Duration {
    sorted[(sorted.len() * p / 100).min(sorted.len() - 1)]
}

fn report(
    name: &str,
    mut samples: Vec<std::time::Duration>,
) -> (std::time::Duration, std::time::Duration) {
    samples.sort();
    let (p50, p99) = (percentile(&samples, 50), percentile(&samples, 99));
    println!(
        "{name:<52} n={:<5} p50={p50:>9.1?}  p99={p99:>9.1?}  max={:>9.1?}",
        samples.len(),
        samples[samples.len() - 1]
    );
    (p50, p99)
}

/// `cargo test --release -p femme-store -- --ignored --nocapture bench_`
#[test]
#[ignore = "benchmark: run with --release --ignored --nocapture"]
fn bench_put_item_and_changes_since_on_100k_items() {
    use std::time::Instant;
    const SESSIONS: usize = 50;
    const FILL: usize = 100_000;
    let (_dir, path) = tempdb();
    let s = Store::open(&path).unwrap();
    let sids: Vec<Sid> = (0..SESSIONS)
        .map(|i| insert_session(&s, Some(&format!("s{i}"))).id)
        .collect();
    // what a real transcript looks like: mostly short JSON, some medium, a few big tool results
    let body_for = |n: usize| match n % 100 {
        0 => noise(6000, n as u64),
        1..=9 => noise(1500, n as u64),
        _ => format!(r#"{{"text":"message {n}","tool":"Bash","cmd":"ls -la /work/acme/{n}"}}"#),
    };

    let t = Instant::now();
    for chunk in 0..FILL / 1000 {
        s.write(|tx| {
            for k in 0..1000 {
                let n = chunk * 1000 + k;
                tx.put_item(&item(
                    sids[n % SESSIONS],
                    &format!("item-{n}"),
                    &body_for(n),
                ))?;
            }
            Ok(())
        })
        .unwrap();
    }
    println!(
        "filled {FILL} items in {:.2?} (1000 per transaction): db {} MiB, {} blobs",
        t.elapsed(),
        s.db_size_bytes().unwrap() >> 20,
        count(&s, "blobs")
    );

    // one transaction per put: what the daemon does if it does not group commits
    let mut insert = Vec::new();
    for n in FILL..FILL + 5000 {
        let it = item(sids[n % SESSIONS], &format!("item-{n}"), &body_for(n));
        let t = Instant::now();
        s.write(|tx| tx.put_item(&it)).unwrap();
        insert.push(t.elapsed());
    }
    let (put_p50, put_p99) = report("put_item, new item, 1 per transaction", insert);

    // a streaming message: the same item rewritten again and again
    let mut update = Vec::new();
    for i in 0..5000 {
        let n = (i * 7919) % FILL;
        let it = item(
            sids[n % SESSIONS],
            &format!("item-{n}"),
            &format!(r#"{{"text":"message {n}, edit {i}"}}"#),
        );
        let t = Instant::now();
        s.write(|tx| tx.put_item(&it)).unwrap();
        update.push(t.elapsed());
    }
    report("put_item, existing item, 1 per transaction", update);

    // and grouped, as femmed will do it (64 per commit), per item
    let mut grouped = Vec::new();
    for batch in 0..100 {
        let its: Vec<Item> = (0..64)
            .map(|k| {
                let n = FILL + 5000 + batch * 64 + k;
                item(sids[n % SESSIONS], &format!("item-{n}"), &body_for(n))
            })
            .collect();
        let t = Instant::now();
        s.write(|tx| its.iter().try_for_each(|it| tx.put_item(it).map(drop)))
            .unwrap();
        grouped.push(t.elapsed() / 64);
    }
    report("put_item, new item, 64 per transaction (per item)", grouped);

    let head = s.rev().unwrap();
    let sid = sids[7];
    let mut tail = Vec::new();
    for _ in 0..2000 {
        let t = Instant::now();
        let got = s
            .changes_since(Scope::Session(sid), Some(head - 50), 64 * 1024)
            .unwrap();
        tail.push(t.elapsed());
        assert!(!got.rows.is_empty() && got.rows.len() <= 50);
    }
    let (tail_p50, tail_p99) = report("changes_since(session, 50 revs behind, 64 KiB)", tail);

    let mut global = Vec::new();
    for _ in 0..2000 {
        let t = Instant::now();
        s.changes_since(Scope::Global, Some(head - 50), 64 * 1024)
            .unwrap();
        global.push(t.elapsed());
    }
    report("changes_since(global, 50 revs behind, 64 KiB)", global);

    let mut catch_up = Vec::new();
    for _ in 0..500 {
        let t = Instant::now();
        let got = s
            .changes_since(Scope::Session(sid), Some(head - 20_000), 64 * 1024)
            .unwrap();
        catch_up.push(t.elapsed());
        assert!(got.more, "20k revs behind does not fit 64 KiB");
    }
    report(
        "changes_since(session, 20k revs behind, 64 KiB page)",
        catch_up,
    );

    let mut cold = Vec::new();
    for _ in 0..500 {
        let t = Instant::now();
        let got = s
            .changes_since(Scope::Session(sid), None, 64 * 1024)
            .unwrap();
        cold.push(t.elapsed());
        assert_eq!(got.rows.len(), INITIAL_ITEMS);
    }
    report("changes_since(session, full set: newest 200 items)", cold);

    // generous ceilings: an accidental table scan at 100k rows is an order of magnitude over these
    assert!(
        put_p50 < std::time::Duration::from_millis(2)
            && put_p99 < std::time::Duration::from_millis(50),
        "{put_p50:?} {put_p99:?}"
    );
    assert!(
        tail_p50 < std::time::Duration::from_millis(2)
            && tail_p99 < std::time::Duration::from_millis(20),
        "{tail_p50:?} {tail_p99:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// model-based: clients that follow changes_since stay equal to the store, retention included

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> Option<&'a T> {
        (!v.is_empty()).then(|| &v[self.below(v.len() as u64) as usize])
    }
}

/// What a client holds: the global scope, plus the transcript of the one session it has open.
#[derive(Default)]
struct Replica {
    sessions: std::collections::BTreeMap<Sid, Session>,
    panes: std::collections::BTreeMap<(Sid, u16), Pane>,
    issues: std::collections::BTreeMap<u64, Issue>,
    items: std::collections::BTreeMap<String, Item>,
}

struct Client {
    cursor: Option<u64>,
    open: Option<(Sid, Option<u64>)>,
    rep: Replica,
}

#[derive(Default, Debug)]
struct Seen {
    resets: usize,
    pages: usize,
    more: usize,
    purged_sessions: usize,
    purged_tombstones: usize,
    future: usize,
}

fn apply(rep: &mut Replica, rows: Vec<Row>) {
    for row in rows {
        match row {
            Row::Session(x) => {
                rep.sessions.insert(x.id, x);
            }
            Row::Pane(p) => {
                rep.panes.insert((p.id.sid, p.id.no), p);
            }
            Row::Issue(i) => {
                rep.issues.insert(i.id, i);
            }
            Row::Item(i) => {
                rep.items.insert(i.item_id.clone(), i);
            }
            Row::Tombstone { entity, id, .. } => match entity {
                // nothing but a tombstone removes a row: a pane of a deleted session needs its own
                Entity::Session => {
                    rep.sessions.remove(&id.parse::<Sid>().unwrap());
                }
                Entity::Pane => {
                    let (sid, no) = id.split_once(':').unwrap();
                    rep.panes
                        .remove(&(sid.parse().unwrap(), no.parse().unwrap()));
                }
                Entity::Issue => {
                    rep.issues.remove(&id.parse().unwrap());
                }
                Entity::Item => {
                    rep.items.remove(id.split_once('/').unwrap().1);
                }
            },
        }
    }
}

/// One full catch-up of every scope the client follows, paging with `limit` bytes.
fn sync(s: &Store, c: &mut Client, limit: usize, seen: &mut Seen) {
    loop {
        let ch = s.changes_since(Scope::Global, c.cursor, limit).unwrap();
        seen.pages += 1;
        if ch.reset {
            seen.resets += 1;
            c.rep.sessions.clear();
            c.rep.panes.clear();
            c.rep.issues.clear();
        }
        apply(&mut c.rep, ch.rows);
        c.cursor = Some(ch.rev);
        if !ch.more {
            break;
        }
        seen.more += 1;
    }
    let Some((sid, mut cursor)) = c.open.take() else {
        return;
    };
    loop {
        match s.changes_since(Scope::Session(sid), cursor, limit) {
            Err(Error::NotFound) => {
                c.rep.items.clear(); // the session is gone; the client saw its tombstone
                return;
            }
            Err(e) => panic!("{e}"),
            Ok(ch) => {
                seen.pages += 1;
                if ch.reset {
                    seen.resets += 1;
                    c.rep.items.clear();
                }
                apply(&mut c.rep, ch.rows);
                cursor = Some(ch.rev);
                if !ch.more {
                    break;
                }
                seen.more += 1;
            }
        }
    }
    c.open = Some((sid, cursor));
}

fn assert_same<K: std::fmt::Debug + Ord, V: PartialEq + std::fmt::Debug>(
    what: &str,
    ctx: &str,
    replica: &std::collections::BTreeMap<K, V>,
    truth: &std::collections::BTreeMap<K, V>,
) {
    let missing: Vec<&K> = truth.keys().filter(|k| !replica.contains_key(*k)).collect();
    let extra: Vec<&K> = replica.keys().filter(|k| !truth.contains_key(*k)).collect();
    let stale: Vec<&K> = truth
        .iter()
        .filter(|(k, v)| replica.get(*k).is_some_and(|r| r != *v))
        .map(|(k, _)| k)
        .collect();
    assert!(
        missing.is_empty() && extra.is_empty() && stale.is_empty(),
        "{ctx}: {what} differ. missing {missing:?}, extra {extra:?}, stale {stale:?}"
    );
}

fn check(s: &Store, c: &Client, ctx: &str) {
    let (sess, panes, issues, items) = s
        .read(|conn| {
            let sess = sessions::list_sessions(conn, true)?;
            let mut panes = std::collections::BTreeMap::new();
            for x in &sess {
                for p in sessions::panes(conn, x.id)? {
                    panes.insert((p.id.sid, p.id.no), p);
                }
            }
            let issues = issues::list_issues(conn)?
                .into_iter()
                .map(|i| (i.id, i))
                .collect::<std::collections::BTreeMap<_, _>>();
            let mut items = std::collections::BTreeMap::new();
            if let Some((sid, _)) = c.open {
                for i in items::page_items(conn, sid, None, 1000)? {
                    items.insert(i.item_id.clone(), i);
                }
            }
            Ok((
                sess.into_iter()
                    .map(|x| (x.id, x))
                    .collect::<std::collections::BTreeMap<_, _>>(),
                panes,
                issues,
                items,
            ))
        })
        .unwrap();
    assert_same("sessions", ctx, &c.rep.sessions, &sess);
    assert_same("panes", ctx, &c.rep.panes, &panes);
    assert_same("issues", ctx, &c.rep.issues, &issues);
    assert_same("items", ctx, &c.rep.items, &items);
}

/// One random mutation, in its own transaction at `clock` (errors such as a taken name are fine:
/// the transaction rolls back and the world goes on).
fn random_step(s: &Store, rng: &mut Rng, clock: i64) {
    let all: Vec<Session> = s.read(|c| sessions::list_sessions(c, true)).unwrap();
    let open: Vec<Session> = all.iter().filter(|x| x.closed.is_none()).cloned().collect();
    let issue_ids: Vec<u64> = s
        .read(|c| Ok(issues::list_issues(c)?.iter().map(|i| i.id).collect()))
        .unwrap();
    let roll = rng.below(100);
    let _ = s.write_at(clock, |tx| -> R {
        match roll {
            0..=9 => {
                let name = (rng.below(2) == 0).then(|| format!("n{}", rng.below(6)));
                let mut x = session(name.as_deref());
                x.created = clock;
                tx.insert_session(&x)?;
            }
            10..=17 => {
                if let Some(x) = rng.pick(&open) {
                    let status = [
                        SessionStatus::Working,
                        SessionStatus::Idle,
                        SessionStatus::Blocked,
                    ][rng.below(3) as usize];
                    tx.update_session(x.id, |s| s.status = status)?;
                }
            }
            18..=23 => {
                if let Some(x) = rng.pick(&open) {
                    tx.close_session(x.id, clock)?;
                }
            }
            24..=27 => {
                if let Some(x) = rng.pick(&all) {
                    tx.delete_session(x.id)?;
                }
            }
            28..=37 => {
                if let Some(x) = rng.pick(&open) {
                    tx.upsert_pane(&pane(
                        x.id,
                        rng.below(3) as u16,
                        &format!("t{}", rng.below(4)),
                    ))?;
                }
            }
            38..=41 => {
                if let Some(x) = rng.pick(&open) {
                    tx.delete_pane(PaneId {
                        sid: x.id,
                        no: rng.below(3) as u16,
                    })?;
                }
            }
            42..=47 => {
                tx.create_issue(
                    "acme/webshop",
                    &format!("issue {}", rng.below(1000)),
                    "",
                    clock,
                )?;
            }
            48..=53 => {
                if let Some(&id) = rng.pick(&issue_ids) {
                    let status = [
                        IssueStatus::Working,
                        IssueStatus::Waiting,
                        IssueStatus::Pr,
                        IssueStatus::Done,
                    ][rng.below(4) as usize];
                    tx.set_issue_status(id, status, None, clock)?;
                }
            }
            54..=57 => {
                if let Some(&id) = rng.pick(&issue_ids) {
                    tx.patch_issue(
                        id,
                        u64::MAX,
                        &[IssueField::Title(format!("retitled {}", rng.below(1000)))],
                        clock,
                    )?;
                }
            }
            58..=60 => {
                if let Some(&id) = rng.pick(&issue_ids) {
                    tx.delete_issue(id)?;
                }
            }
            61..=86 => {
                if let Some(x) = rng.pick(&all) {
                    let body = match rng.below(10) {
                        0 => noise(5000 + rng.below(500) as usize, rng.below(50)), // a blob
                        1..=3 => noise(600 + rng.below(1500) as usize, rng.below(50)),
                        _ => format!(r#"{{"n":{}}}"#, rng.below(100)),
                    };
                    tx.put_item(&item(x.id, &format!("i{}", rng.below(10)), &body))?;
                }
            }
            87..=91 => {
                if let Some(x) = rng.pick(&all) {
                    tx.delete_item(x.id, &format!("i{}", rng.below(10)))?;
                }
            }
            92..=93 => {
                if let Some(x) = rng.pick(&all) {
                    tx.delete_items(x.id)?;
                }
            }
            _ => {}
        }
        Ok(())
    });
}

#[test]
fn clients_that_follow_changes_since_always_equal_the_store() {
    let mut seen = Seen::default();
    // FEMME_MODEL_SEEDS=250 for a long soak (release: a few minutes).
    let seeds: u64 = std::env::var("FEMME_MODEL_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    for seed in 1..=seeds {
        let s = store();
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9));
        let mut clock = NOW;
        let sleepiness = [120u64, 12, 3]; // client 0 is mostly offline: it will fall behind the tombstone horizon
        let mut clients: Vec<Client> = (0..3)
            .map(|_| Client {
                cursor: None,
                open: None,
                rep: Replica::default(),
            })
            .collect();
        for step in 0..500 {
            clock += rng.below(3 * DAY as u64 / 2) as i64; // 0.75 days a step: 14-day and 30-day limits pass often
            random_step(&s, &mut rng, clock);
            if rng.below(12) == 0 {
                let r = s.retain(clock).unwrap();
                seen.purged_sessions += r.sessions;
                seen.purged_tombstones += r.tombstones;
            }
            for (i, c) in clients.iter_mut().enumerate() {
                if rng.below(sleepiness[i]) != 0 {
                    continue;
                }
                if c.open.is_none() && rng.below(2) == 0 {
                    let all = s.read(|conn| sessions::list_sessions(conn, true)).unwrap();
                    c.open = rng.pick(&all).map(|x| (x.id, None));
                    c.rep.items.clear();
                }
                let limit = *rng.pick(&[1usize, 60, 700, 9000, usize::MAX]).unwrap();
                // now and then a client's cursor is ahead of the database, as after a restored backup
                let from_the_future = rng.below(20) == 0;
                if from_the_future {
                    let ahead = s.rev().unwrap() + 1 + rng.below(40);
                    c.cursor = Some(ahead);
                    if let Some((_, cursor)) = c.open.as_mut() {
                        *cursor = Some(ahead);
                    }
                }
                let resets = seen.resets;
                sync(&s, c, limit, &mut seen);
                if from_the_future {
                    seen.future += 1;
                    assert!(seen.resets > resets, "a cursor ahead of the database must reset (seed {seed} step {step} client {i})");
                }
                check(
                    &s,
                    c,
                    &format!("seed {seed} step {step} client {i} limit {limit}"),
                );
            }
        }
        for (i, c) in clients.iter_mut().enumerate() {
            sync(&s, c, 700, &mut seen);
            check(&s, c, &format!("seed {seed} final client {i}"));
        }
    }
    // the run must actually have walked the interesting paths
    println!("{seen:?}");
    assert!(
        seen.resets > 0,
        "no client ever fell behind the tombstone horizon: {seen:?}"
    );
    assert!(seen.more > 100, "paging was barely exercised: {seen:?}");
    assert!(
        seen.purged_sessions > 0 && seen.purged_tombstones > 0,
        "retention never purged anything: {seen:?}"
    );
    assert!(
        seen.future > 0,
        "no client ever presented a cursor from the future: {seen:?}"
    );
}

#[test]
fn items_inserted_before_others_take_their_place_in_order() {
    let s = store();
    let sid = insert_session(&s, None).id;
    s.write(|tx| {
        for id in ["a", "b", "c", "d"] {
            tx.put_item(&item(sid, id, id))?;
        }
        Ok(())
    })
    .unwrap();
    let before = s.rev().unwrap();
    s.write(|tx| {
        let mut new = vec![
            (3, item(sid, "x2", "x2")), // before c
            (2, item(sid, "x1", "x1")), // before b
            (3, item(sid, "x3", "x3")), // also before c, after x2
        ];
        tx.insert_items_before(sid, &mut new)
    })
    .unwrap();
    let got = s.read(|c| items::page_items(c, sid, None, 100)).unwrap();
    let order: Vec<(u64, &str)> = got
        .iter()
        .rev()
        .map(|i| (i.seq, i.item_id.as_str()))
        .collect();
    assert_eq!(
        order,
        [
            (1, "a"),
            (2, "x1"),
            (3, "b"),
            (4, "x2"),
            (5, "x3"),
            (6, "c"),
            (7, "d")
        ]
    );
    assert!(
        got.iter()
            .filter(|i| i.item_id != "a")
            .all(|i| i.rev > before),
        "every moved or new row is newer"
    );
    assert!(got.iter().find(|i| i.item_id == "a").unwrap().rev <= before);
}

#[test]
fn items_with_a_status_are_listed_oldest_first() {
    let s = store();
    let sid = insert_session(&s, None).id;
    let put = |id: &str, kind: ItemKind, status: ItemStatus| {
        let mut i = item(sid, id, r#"{"text":"hi"}"#);
        i.kind = kind;
        i.status = status;
        s.write(|tx| tx.put_item(&i)).unwrap();
    };
    put("a", ItemKind::UserMessage, ItemStatus::Pending);
    put("b", ItemKind::UserMessage, ItemStatus::Completed);
    put("c", ItemKind::Question, ItemStatus::Pending);
    put("d", ItemKind::UserMessage, ItemStatus::Failed);
    put("e", ItemKind::UserMessage, ItemStatus::Pending);
    let ids = |statuses: &[ItemStatus]| -> Vec<String> {
        s.read(|c| items::items_with_status(c, sid, ItemKind::UserMessage, statuses))
            .unwrap()
            .into_iter()
            .map(|i| i.item_id)
            .collect()
    };
    assert_eq!(ids(&[ItemStatus::Pending]), ["a", "e"]);
    assert_eq!(
        ids(&[ItemStatus::Pending, ItemStatus::Failed]),
        ["a", "d", "e"]
    );
    assert!(ids(&[ItemStatus::Answered]).is_empty());
}
