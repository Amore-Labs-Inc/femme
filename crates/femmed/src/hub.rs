//! Live state fan-out to connected clients (plan §6 "State sync"). Rows committed to the store are
//! `publish`ed here; subscribed clients get them as `S2C::Changes` per scope with t3code's rhythm:
//! the first change goes out at once, then changes are coalesced for 50 ms keeping the latest row
//! per entity. Streaming text (`S2C::Delta`) is batched for 16 ms per item. A client whose channel
//! is full is marked lagged and, once it drains, re-fetched from the rev it last got (no replay log).
//!
//! Subscribe registers for live rows *before* fetching the backlog, so nothing is lost in between;
//! a row may then arrive twice (backlog + live) and the client keeps the higher rev — harmless.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use femme_proto::rpc::{Scope, S2C};
use femme_proto::state::{Entity, Row};
use femme_proto::Sid;
use tokio::sync::mpsc;

pub const COALESCE: Duration = Duration::from_millis(50);
pub const DELTA_COALESCE: Duration = Duration::from_millis(16);
/// Rows per `Changes` message when sending a backlog (≈ keeps each message well under 1 MiB).
pub const BACKLOG_CHUNK: usize = 256;

/// What the store returns for "rows of `scope` after `rev`".
pub struct Backlog {
    pub rows: Vec<Row>,
    pub rev: u64,
    /// The cursor was unusable (older than the tombstone horizon / other epoch): `rows` is a full set.
    pub reset: bool,
}

pub type Fetch = Arc<dyn Fn(Scope, Option<u64>) -> Result<Backlog, String> + Send + Sync>;

pub type ClientId = u64;

struct Sub {
    rev_sent: u64,
    lagged: bool,
}

struct Client {
    tx: mpsc::Sender<S2C>,
    subs: HashMap<Scope, Sub>,
}

#[derive(Default)]
struct Pending {
    /// Latest row per entity key, in arrival order of first sight.
    rows: Vec<(String, Row)>,
    window_open: bool,
}

#[derive(Default)]
struct DeltaPending {
    /// (sid, item_id) -> (offset of the first piece, concatenated text)
    items: Vec<((Sid, String), (u32, String))>,
    window_open: bool,
}

struct Inner {
    clients: HashMap<ClientId, Client>,
    pending: HashMap<Scope, Pending>,
    deltas: HashMap<Sid, DeltaPending>,
    next_id: ClientId,
}

#[derive(Clone)]
pub struct Hub {
    inner: Arc<Mutex<Inner>>,
    fetch: Fetch,
}

/// Which scope a row belongs to.
pub fn scope_of(row: &Row) -> Scope {
    match row {
        Row::Item(i) => Scope::Session(i.sid),
        Row::Tombstone {
            entity: Entity::Item,
            id,
            ..
        } => id
            .split_once('/')
            .and_then(|(s, _)| s.parse::<Sid>().ok())
            .map(Scope::Session)
            .unwrap_or(Scope::Global),
        _ => Scope::Global,
    }
}

/// Key that identifies the entity a row is about, so a later row replaces an earlier one.
fn entity_key(row: &Row) -> String {
    match row {
        Row::Session(s) => format!("s/{}", s.id),
        Row::Pane(p) => format!("p/{}:{}", p.id.sid, p.id.no),
        Row::Item(i) => format!("i/{}/{}", i.sid, i.item_id),
        Row::Issue(i) => format!("n/{}", i.id),
        Row::Tombstone { entity, id, .. } => format!("t/{entity:?}/{id}"),
    }
}

impl Hub {
    pub fn new(fetch: Fetch) -> Hub {
        Hub {
            inner: Arc::new(Mutex::new(Inner {
                clients: HashMap::new(),
                pending: HashMap::new(),
                deltas: HashMap::new(),
                next_id: 1,
            })),
            fetch,
        }
    }

    /// Register a client; `tx` is its events stream. Bounded: a full channel marks subscriptions lagged.
    pub fn add_client(&self, tx: mpsc::Sender<S2C>) -> ClientId {
        let mut g = self.inner.lock().unwrap();
        let id = g.next_id;
        g.next_id += 1;
        g.clients.insert(
            id,
            Client {
                tx,
                subs: HashMap::new(),
            },
        );
        id
    }

    pub fn remove_client(&self, id: ClientId) {
        self.inner.lock().unwrap().clients.remove(&id);
    }

    /// Subscribe `client` to `scope` from `after` (None = everything / the newest page for a session).
    /// Sends the backlog in chunks, then live changes. Returns the rev the client is now at.
    pub async fn subscribe(
        &self,
        client: ClientId,
        scope: Scope,
        after: Option<u64>,
    ) -> Result<u64, String> {
        {
            let mut g = self.inner.lock().unwrap();
            let Some(c) = g.clients.get_mut(&client) else {
                return Err("no such client".into());
            };
            c.subs.insert(
                scope,
                Sub {
                    rev_sent: after.unwrap_or(0),
                    lagged: false,
                },
            );
        }
        let tx = {
            let g = self.inner.lock().unwrap();
            g.clients
                .get(&client)
                .map(|c| c.tx.clone())
                .ok_or("no such client")?
        };
        let fetch = self.fetch.clone();
        let backlog = tokio::task::spawn_blocking(move || fetch(scope, after))
            .await
            .map_err(|e| e.to_string())??;
        let rev = backlog.rev;
        // ponytail: a reset is just a full backlog; the client drops its scope state when it sees
        // rows whose revs predate what it holds (or we add a flag to Changes later).
        let mut rows = backlog.rows;
        if rows.is_empty() {
            let _ = tx
                .send(S2C::Changes {
                    scope,
                    rev,
                    rows: vec![],
                })
                .await;
        }
        while !rows.is_empty() {
            let rest = rows.split_off(rows.len().min(BACKLOG_CHUNK));
            let chunk_rev = rows.iter().map(Row::rev).max().unwrap_or(rev);
            let msg = S2C::Changes {
                scope,
                rev: if rest.is_empty() { rev } else { chunk_rev },
                rows,
            };
            if tx.send(msg).await.is_err() {
                return Err("client gone".into());
            }
            rows = rest;
        }
        let mut g = self.inner.lock().unwrap();
        if let Some(s) = g
            .clients
            .get_mut(&client)
            .and_then(|c| c.subs.get_mut(&scope))
        {
            s.rev_sent = s.rev_sent.max(rev);
        }
        Ok(rev)
    }

    pub fn unsubscribe(&self, client: ClientId, scope: Scope) {
        if let Some(c) = self.inner.lock().unwrap().clients.get_mut(&client) {
            c.subs.remove(&scope);
        }
    }

    /// Rows just committed. First change in a quiet scope goes out now; the rest wait ≤ 50 ms.
    pub fn publish(&self, rows: Vec<Row>) {
        let mut now: HashMap<Scope, Vec<Row>> = HashMap::new();
        let mut arm: Vec<Scope> = vec![];
        {
            let mut g = self.inner.lock().unwrap();
            for row in rows {
                let scope = scope_of(&row);
                let p = g.pending.entry(scope).or_default();
                if p.window_open {
                    let key = entity_key(&row);
                    match p.rows.iter_mut().find(|(k, _)| *k == key) {
                        Some(slot) => slot.1 = row,
                        None => p.rows.push((key, row)),
                    }
                } else {
                    p.window_open = true;
                    arm.push(scope);
                    now.entry(scope).or_default().push(row);
                }
            }
        }
        for (scope, rows) in now {
            self.send_changes(scope, rows);
        }
        for scope in arm {
            let hub = self.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(COALESCE).await;
                    let rows: Vec<Row> = {
                        let mut g = hub.inner.lock().unwrap();
                        let p = g.pending.entry(scope).or_default();
                        if p.rows.is_empty() {
                            p.window_open = false;
                            return;
                        }
                        std::mem::take(&mut p.rows)
                            .into_iter()
                            .map(|(_, r)| r)
                            .collect()
                    };
                    hub.send_changes(scope, rows);
                }
            });
        }
    }

    fn send_changes(&self, scope: Scope, rows: Vec<Row>) {
        let rev = rows.iter().map(Row::rev).max().unwrap_or(0);
        let mut resync: Vec<ClientId> = vec![];
        {
            let mut g = self.inner.lock().unwrap();
            for (id, c) in g.clients.iter_mut() {
                let Some(sub) = c.subs.get_mut(&scope) else {
                    continue;
                };
                if sub.lagged {
                    continue;
                }
                match c.tx.try_send(S2C::Changes {
                    scope,
                    rev,
                    rows: rows.clone(),
                }) {
                    Ok(()) => sub.rev_sent = sub.rev_sent.max(rev),
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        sub.lagged = true;
                        resync.push(*id);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                }
            }
        }
        for id in resync {
            let hub = self.clone();
            tokio::spawn(async move { hub.resync(id, scope).await });
        }
    }

    /// The client's channel was full: wait for room, then send everything since its last rev.
    async fn resync(&self, client: ClientId, scope: Scope) {
        let (tx, after) = {
            let g = self.inner.lock().unwrap();
            let Some(c) = g.clients.get(&client) else {
                return;
            };
            let Some(s) = c.subs.get(&scope) else { return };
            (c.tx.clone(), s.rev_sent)
        };
        let Ok(permit) = tx.reserve().await else {
            return;
        };
        let fetch = self.fetch.clone();
        let Ok(Ok(b)) = tokio::task::spawn_blocking(move || fetch(scope, Some(after))).await else {
            return;
        };
        let rev = b.rev.max(after);
        permit.send(S2C::Changes {
            scope,
            rev,
            rows: b.rows,
        });
        let mut g = self.inner.lock().unwrap();
        if let Some(s) = g
            .clients
            .get_mut(&client)
            .and_then(|c| c.subs.get_mut(&scope))
        {
            s.rev_sent = s.rev_sent.max(rev);
            s.lagged = false;
        }
    }

    /// Streaming text for an in-progress item: batched ≤ 16 ms per item, dropped for lagged clients
    /// (the completed item arrives as a row anyway).
    pub fn publish_delta(&self, sid: Sid, item_id: String, offset: u32, text: String) {
        let arm = {
            let mut g = self.inner.lock().unwrap();
            let d = g.deltas.entry(sid).or_default();
            let key = (sid, item_id);
            match d.items.iter_mut().find(|(k, _)| *k == key) {
                Some((_, (_, buf))) => buf.push_str(&text),
                None => d.items.push((key, (offset, text))),
            }
            if d.window_open {
                false
            } else {
                d.window_open = true;
                true
            }
        };
        if arm {
            let hub = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(DELTA_COALESCE).await;
                let items = {
                    let mut g = hub.inner.lock().unwrap();
                    let d = g.deltas.entry(sid).or_default();
                    d.window_open = false;
                    std::mem::take(&mut d.items)
                };
                let g = hub.inner.lock().unwrap();
                for c in g.clients.values() {
                    let Some(sub) = c.subs.get(&Scope::Session(sid)) else {
                        continue;
                    };
                    if sub.lagged {
                        continue;
                    }
                    for ((sid, item_id), (offset, text)) in &items {
                        let _ = c.tx.try_send(S2C::Delta {
                            sid: *sid,
                            item_id: item_id.clone(),
                            offset: *offset,
                            text: text.clone(),
                        });
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::state::{Issue, IssueStatus};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn issue(id: u64, rev: u64) -> Row {
        Row::Issue(Issue {
            id,
            rev,
            repo: "r".into(),
            title: format!("t{rev}"),
            body: String::new(),
            status: IssueStatus::Open,
            pr_url: None,
            note: None,
            session: None,
            resume_at: None,
            created: 0,
            updated: 0,
        })
    }

    fn hub_with(rows: Vec<Row>) -> (Hub, Arc<AtomicU64>) {
        let calls = Arc::new(AtomicU64::new(0));
        let c = calls.clone();
        let hub = Hub::new(Arc::new(move |_scope, after| {
            c.fetch_add(1, Ordering::SeqCst);
            let rows: Vec<Row> = rows
                .iter()
                .filter(|r| r.rev() > after.unwrap_or(0))
                .cloned()
                .collect();
            let rev = rows
                .iter()
                .map(Row::rev)
                .max()
                .unwrap_or(after.unwrap_or(0));
            Ok(Backlog {
                rows,
                rev,
                reset: false,
            })
        }));
        (hub, calls)
    }

    #[tokio::test]
    async fn backlog_then_live_first_now_rest_coalesced() {
        let (hub, _) = hub_with(vec![issue(1, 1), issue(2, 2)]);
        let (tx, mut rx) = mpsc::channel(16);
        let id = hub.add_client(tx);
        let rev = hub.subscribe(id, Scope::Global, None).await.unwrap();
        assert_eq!(rev, 2);
        match rx.recv().await.unwrap() {
            S2C::Changes { rev: 2, rows, .. } => assert_eq!(rows.len(), 2),
            m => panic!("{m:?}"),
        }
        hub.publish(vec![issue(3, 3)]);
        hub.publish(vec![issue(3, 4)]);
        hub.publish(vec![issue(4, 5)]);
        hub.publish(vec![issue(3, 6)]);
        // first change immediately
        match rx.recv().await.unwrap() {
            S2C::Changes { rev: 3, rows, .. } => assert_eq!(rows.len(), 1),
            m => panic!("{m:?}"),
        }
        // the rest after the window, latest per entity: issue 3 @6, issue 4 @5
        let t = std::time::Instant::now();
        match rx.recv().await.unwrap() {
            S2C::Changes { rev: 6, rows, .. } => {
                assert_eq!(rows.len(), 2);
                assert!(rows.iter().any(|r| r.rev() == 6) && rows.iter().any(|r| r.rev() == 5));
            }
            m => panic!("{m:?}"),
        }
        assert!(
            t.elapsed() >= Duration::from_millis(40) && t.elapsed() < Duration::from_millis(200)
        );
        assert!(tokio::time::timeout(Duration::from_millis(80), rx.recv())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn unsubscribed_scope_gets_nothing() {
        let (hub, _) = hub_with(vec![]);
        let (tx, mut rx) = mpsc::channel(16);
        let id = hub.add_client(tx);
        hub.subscribe(id, Scope::Session(femme_proto::Id(7)), None)
            .await
            .unwrap();
        let _ = rx.recv().await; // empty backlog
        hub.publish(vec![issue(1, 1)]);
        assert!(tokio::time::timeout(Duration::from_millis(80), rx.recv())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn lagged_client_resyncs_from_its_rev() {
        let (hub, calls) = hub_with((1..=10).map(|i| issue(i, i)).collect());
        let (tx, mut rx) = mpsc::channel(1); // tiny: fills at once
        let id = hub.add_client(tx);
        hub.subscribe(id, Scope::Global, Some(3)).await.unwrap(); // backlog 4..=10 fills the slot
        let before = calls.load(Ordering::SeqCst);
        hub.publish(vec![issue(11, 11)]); // channel full -> lagged
        tokio::time::sleep(Duration::from_millis(20)).await;
        let first = rx.recv().await.unwrap(); // drains the backlog message
        assert!(matches!(first, S2C::Changes { rev: 10, .. }));
        let second = rx.recv().await.unwrap(); // the resync, fetched from rev 10
        assert!(matches!(second, S2C::Changes { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), before + 1);
    }

    #[tokio::test]
    async fn deltas_batch_per_item() {
        let (hub, _) = hub_with(vec![]);
        let (tx, mut rx) = mpsc::channel(16);
        let id = hub.add_client(tx);
        let sid = femme_proto::Id(9);
        hub.subscribe(id, Scope::Session(sid), None).await.unwrap();
        let _ = rx.recv().await;
        hub.publish_delta(sid, "a".into(), 0, "hel".into());
        hub.publish_delta(sid, "a".into(), 3, "lo".into());
        hub.publish_delta(sid, "b".into(), 0, "x".into());
        let mut got = vec![];
        while let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(60), rx.recv()).await {
            got.push(m);
        }
        assert_eq!(got.len(), 2);
        assert!(got.iter().any(|m| matches!(m, S2C::Delta { item_id, offset: 0, text, .. } if item_id == "a" && text == "hello")));
    }
}
