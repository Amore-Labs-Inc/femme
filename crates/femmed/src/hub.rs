//! Live state fan-out to connected clients (plan §6 "State sync"). Rows committed to the store are
//! `publish`ed here; subscribed clients get them as `S2C::Changes` per scope with t3code's rhythm:
//! the first change goes out at once, then changes are coalesced for 50 ms keeping the latest row
//! per entity. Streaming text (`S2C::Delta`) is batched for 16 ms per item.
//!
//! A subscription starts *lagged*: while a catch-up reads the store (the backlog from the client's
//! cursor, sent in messages of about [`CHUNK_BYTES`]) live rows are not sent to it, and a publish
//! skipped that way marks it *missed*, so the catch-up looks again before it lets go. It clears
//! `lagged` under the lock that finds nothing missed: a row committed at any moment is in a catch-up
//! or goes out live, and nothing goes out ahead of the backlog. A client whose channel is full is
//! lagged the same way and re-fetched from the rev it last got (no replay log). A row may arrive
//! twice (a catch-up and the publish of the same commit): the client keeps the higher rev.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use femme_proto::codec;
use femme_proto::rpc::{Scope, S2C};
use femme_proto::state::{Entity, Row};
use femme_proto::Sid;
use tokio::sync::mpsc;

pub const COALESCE: Duration = Duration::from_millis(50);
pub const DELTA_COALESCE: Duration = Duration::from_millis(16);
/// Wire size (postcard) of the rows in one backlog or resync message: plan §6's "≤ 1 MiB inline". A
/// row is never split, so one bigger than this travels alone.
pub const CHUNK_BYTES: usize = 1 << 20;

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
    /// Which `subscribe` made it: a catch-up that finds another number (the client unsubscribed and
    /// subscribed again) is stale and stops.
    gen: u64,
    /// A catch-up is reading the store for this subscription: live rows are not sent to it.
    lagged: bool,
    /// A publish was skipped because of `lagged`: the catch-up has to look again before it ends.
    missed: bool,
}

/// Why a subscription could not be served.
#[derive(Debug, thiserror::Error)]
pub enum SubscribeError {
    /// The client is gone, or its events stream is: there is nobody to send to.
    #[error("the connection is gone")]
    ClientGone,
    /// The scope could not be read (a session purged since the client last heard of it, or the
    /// database failing). The subscription is dropped.
    #[error("{0}")]
    Scope(String),
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
    next_gen: u64,
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
                next_gen: 1,
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

    /// How many clients are registered: a connection that ends takes its client with it.
    pub fn clients(&self) -> usize {
        self.inner.lock().unwrap().clients.len()
    }

    /// Subscribe `client` to `scope` from `after` (None = everything / the newest page for a session).
    /// Sends the backlog in chunks, then live changes. Returns the rev the client is now at.
    pub async fn subscribe(
        &self,
        client: ClientId,
        scope: Scope,
        after: Option<u64>,
    ) -> Result<u64, SubscribeError> {
        let gen = {
            let mut g = self.inner.lock().unwrap();
            let gen = g.next_gen;
            g.next_gen += 1;
            let c = g
                .clients
                .get_mut(&client)
                .ok_or(SubscribeError::ClientGone)?;
            c.subs.insert(
                scope,
                Sub {
                    rev_sent: after.unwrap_or(0),
                    gen,
                    lagged: true,
                    missed: false,
                },
            );
            gen
        };
        self.catch_up(client, scope, gen, after).await
    }

    /// Sends `client` what `scope` holds after `after` (None: the full set), then, pass by pass,
    /// what was committed while that went out; the pass that finds nothing was missed ends `lagged`
    /// and live rows flow. Stops quietly, with the rev it got to, when the subscription is dropped
    /// or replaced meanwhile. The first pass always sends a message, an empty one if need be: a
    /// subscriber is waiting for it, and a client that stalled is told it is current again.
    // ponytail: a pass reads the whole backlog into memory before the first chunk goes out, and a
    // lagged client's is read before its channel has room; page the fetch into the sender when a
    // backlog outgrows memory (plan §6 sends a big one as `SnapshotOnBulk`, which nothing does yet).
    // The cursor is `rev_sent`: a row published out of rev order (two writers) while the client is
    // lagged can be passed over; it comes back with the next reset or reconnect.
    async fn catch_up(
        &self,
        client: ClientId,
        scope: Scope,
        gen: u64,
        mut after: Option<u64>,
    ) -> Result<u64, SubscribeError> {
        let mut first = true;
        let mut sent = after.unwrap_or(0);
        loop {
            let tx = {
                let mut g = self.inner.lock().unwrap();
                let c = g
                    .clients
                    .get_mut(&client)
                    .ok_or(SubscribeError::ClientGone)?;
                let Some(sub) = c.subs.get_mut(&scope).filter(|s| s.gen == gen) else {
                    return Ok(sent);
                };
                sub.missed = false;
                c.tx.clone()
            };
            let fetch = self.fetch.clone();
            let fetched = tokio::task::spawn_blocking(move || fetch(scope, after))
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r);
            let backlog = match fetched {
                Ok(b) => b,
                Err(why) => {
                    let mut g = self.inner.lock().unwrap();
                    if let Some(c) = g.clients.get_mut(&client) {
                        if c.subs.get(&scope).is_some_and(|s| s.gen == gen) {
                            c.subs.remove(&scope);
                        }
                    }
                    return Err(SubscribeError::Scope(why));
                }
            };
            let (rev, reset) = (backlog.rev, backlog.reset);
            for msg in messages(scope, backlog.rows, rev, reset, first) {
                tx.send(msg).await.map_err(|_| SubscribeError::ClientGone)?;
            }
            first = false;
            // A reset's cursor is the database's, however low that is next to the one it replaces.
            sent = if reset { rev } else { sent.max(rev) };
            let mut g = self.inner.lock().unwrap();
            let Some(sub) = g
                .clients
                .get_mut(&client)
                .and_then(|c| c.subs.get_mut(&scope))
                .filter(|s| s.gen == gen)
            else {
                return Ok(sent);
            };
            sub.rev_sent = sent;
            if !sub.missed {
                sub.lagged = false;
                return Ok(sent);
            }
            after = Some(sent);
        }
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

    // ponytail: a live batch is one message however big (rows committed in 50 ms, so a few MiB at
    // worst with requests capped at 1 MiB); chunk it like a backlog if a writer ever commits more.
    fn send_changes(&self, scope: Scope, rows: Vec<Row>) {
        let rev = rows.iter().map(Row::rev).max().unwrap_or(0);
        let mut resync: Vec<(ClientId, u64, u64)> = vec![];
        {
            let mut g = self.inner.lock().unwrap();
            for (id, c) in g.clients.iter_mut() {
                let Some(sub) = c.subs.get_mut(&scope) else {
                    continue;
                };
                if sub.lagged {
                    sub.missed = true;
                    continue;
                }
                match c.tx.try_send(S2C::Changes {
                    scope,
                    rev,
                    rows: rows.clone(),
                    reset: false,
                }) {
                    Ok(()) => sub.rev_sent = sub.rev_sent.max(rev),
                    // Full: this batch is in the store, so the resync from `rev_sent` has it.
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        sub.lagged = true;
                        resync.push((*id, sub.gen, sub.rev_sent));
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {}
                }
            }
        }
        for (id, gen, after) in resync {
            let hub = self.clone();
            tokio::spawn(async move {
                if let Err(SubscribeError::Scope(e)) =
                    hub.catch_up(id, scope, gen, Some(after)).await
                {
                    tracing::warn!(
                        ?scope,
                        "resync of a lagging client failed, subscription dropped: {e}"
                    );
                }
            });
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

/// One fetch as messages: its rows by rev, in chunks of about [`CHUNK_BYTES`], each chunk's `rev`
/// the highest inside it (the last chunk's the fetch's own, which also counts revs spent on other
/// scopes) so a client that stops after any chunk holds everything up to that rev. `reset` goes on
/// the first only: the client drops its copy of the scope once, then applies every chunk. An empty
/// fetch is one empty message when `first` (a subscriber waiting) or `reset`, else nothing.
fn messages(scope: Scope, mut rows: Vec<Row>, rev: u64, reset: bool, first: bool) -> Vec<S2C> {
    if rows.is_empty() && !first && !reset {
        return vec![];
    }
    // A session's full set comes by seq, not by rev.
    rows.sort_by_key(Row::rev);
    let mut chunks: Vec<Vec<Row>> = vec![vec![]];
    let mut used = 0;
    for row in rows {
        let size = codec::encode(&row).map_or(0, |b| b.len());
        if used > 0 && used + size > CHUNK_BYTES {
            chunks.push(vec![]);
            used = 0;
        }
        used += size;
        chunks.last_mut().expect("always one chunk").push(row);
    }
    let n = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, rows)| {
            let top = rows.iter().map(Row::rev).max().unwrap_or(0);
            S2C::Changes {
                scope,
                rev: if i + 1 == n { top.max(rev) } else { top },
                rows,
                reset: reset && i == 0,
            }
        })
        .collect()
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

    /// An issue whose body makes it about `n` bytes on the wire.
    fn fat(id: u64, rev: u64, n: usize) -> Row {
        let Row::Issue(mut i) = issue(id, rev) else {
            unreachable!()
        };
        i.body = "x".repeat(n);
        Row::Issue(i)
    }

    /// A hub over rows the test can add to.
    fn hub_over(rows: Arc<Mutex<Vec<Row>>>) -> Hub {
        Hub::new(Arc::new(move |_scope, after| {
            let after = after.unwrap_or(0);
            let rows: Vec<Row> = rows
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.rev() > after)
                .cloned()
                .collect();
            let rev = rows.iter().map(Row::rev).max().unwrap_or(after);
            Ok(Backlog {
                rows,
                rev,
                reset: false,
            })
        }))
    }

    /// A message as `(rev, revs of its rows, reset)`.
    fn shape(m: &S2C) -> (u64, Vec<u64>, bool) {
        match m {
            S2C::Changes {
                rev, rows, reset, ..
            } => (*rev, rows.iter().map(Row::rev).collect(), *reset),
            m => panic!("{m:?}"),
        }
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

    #[tokio::test]
    async fn a_backlog_goes_out_in_chunks_by_size_and_rev() {
        // The fetch is out of rev order (a session's full set is by seq), and one row is over the
        // budget: it travels alone, the rest pack up to the budget.
        let kib = 1 << 10;
        let rows = vec![
            fat(3, 3, 400 * kib),
            fat(1, 1, 400 * kib),
            fat(5, 5, 100 * kib),
            fat(2, 2, 400 * kib),
            fat(4, 4, 1500 * kib),
            fat(6, 6, 100 * kib),
        ];
        let hub = Hub::new(Arc::new(move |_, _| {
            Ok(Backlog {
                rows: rows.clone(),
                rev: 9,
                reset: false,
            })
        }));
        let (tx, mut rx) = mpsc::channel(16);
        let id = hub.add_client(tx);
        assert_eq!(hub.subscribe(id, Scope::Global, None).await.unwrap(), 9);
        let mut got = vec![];
        while let Ok(m) = rx.try_recv() {
            got.push(m);
        }
        let shapes: Vec<_> = got.iter().map(shape).collect();
        // Each chunk's rev is the highest inside it, the last one the fetch's own.
        assert_eq!(
            shapes,
            vec![
                (2, vec![1, 2], false),
                (3, vec![3], false),
                (4, vec![4], false),
                (9, vec![5, 6], false),
            ]
        );
        for m in &got {
            let S2C::Changes { rows, .. } = m else {
                unreachable!()
            };
            let bytes: usize = rows.iter().map(|r| codec::encode(r).unwrap().len()).sum();
            assert!(rows.len() == 1 || bytes <= CHUNK_BYTES, "{bytes} bytes");
        }
    }

    #[tokio::test]
    async fn a_reset_backlog_marks_only_its_first_message_and_sets_the_cursor() {
        let rows = vec![fat(1, 1, 600 << 10), fat(2, 2, 600 << 10)];
        let seen = Arc::new(Mutex::new(vec![]));
        let hub = Hub::new(Arc::new({
            let seen = seen.clone();
            move |_, after| {
                seen.lock().unwrap().push(after);
                let reset = after == Some(99); // the client's cursor is above the database's
                Ok(Backlog {
                    rows: if reset { rows.clone() } else { vec![] },
                    rev: 2,
                    reset,
                })
            }
        }));
        let (tx, mut rx) = mpsc::channel(2);
        let id = hub.add_client(tx);
        hub.subscribe(id, Scope::Global, Some(99)).await.unwrap(); // two chunks fill the channel
        hub.publish(vec![issue(3, 3)]); // so this one finds it full: lagged, resynced
        let resynced = async {
            while seen.lock().unwrap().len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(2), resynced)
            .await
            .unwrap();
        // From the database's rev, not from the 99 the client claimed.
        assert_eq!(*seen.lock().unwrap(), [Some(99), Some(2)]);
        assert_eq!(shape(&rx.recv().await.unwrap()), (1, vec![1], true));
        assert_eq!(shape(&rx.recv().await.unwrap()), (2, vec![2], false));
    }

    #[tokio::test]
    async fn a_lagging_clients_resync_is_chunked_too() {
        let rows = Arc::new(Mutex::new(vec![issue(1, 1)]));
        let hub = hub_over(rows.clone());
        let (tx, mut rx) = mpsc::channel(1);
        let id = hub.add_client(tx);
        hub.subscribe(id, Scope::Global, None).await.unwrap(); // fills the one slot
        {
            let mut r = rows.lock().unwrap();
            r.extend((2..=4).map(|i| fat(i, i, 600 << 10)));
        }
        hub.publish(vec![issue(2, 2)]); // the channel is full: lagged, resynced from rev 1
        assert_eq!(shape(&rx.recv().await.unwrap()), (1, vec![1], false));
        for rev in 2..=4 {
            assert_eq!(shape(&rx.recv().await.unwrap()), (rev, vec![rev], false));
        }
    }

    #[tokio::test]
    async fn subscribe_tells_a_scope_that_cannot_be_read_from_a_client_that_is_gone() {
        let hub = Hub::new(Arc::new(|_, _| Err("no such session".into())));
        let (tx, _rx) = mpsc::channel(16);
        let id = hub.add_client(tx);
        let e = hub
            .subscribe(id, Scope::Session(femme_proto::Id(5)), None)
            .await;
        assert!(matches!(e, Err(SubscribeError::Scope(m)) if m == "no such session"));
        assert_eq!(hub.clients(), 1);
        hub.remove_client(id);
        assert_eq!(hub.clients(), 0);
        let e = hub.subscribe(id, Scope::Global, None).await;
        assert!(matches!(e, Err(SubscribeError::ClientGone)));
    }
}
