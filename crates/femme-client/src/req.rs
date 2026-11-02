//! What the client owes the daemon across connections: the requests nobody has answered yet, the
//! scopes it is subscribed to and the cursors it holds for them. One struct behind one mutex
//! ([`State`]); every method is a short synchronous edit, never held across an await.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use femme_proto::rpc::{Cursors, Req, Resp, Scope};
use femme_proto::{OpId, PaneId};
use femme_transport::Conn;
use tokio::sync::oneshot;

use crate::{Link, RequestError};

/// What a pending request puts on the wire.
pub(crate) enum Kind {
    /// An ordinary request of the caller's.
    Req(Req),
    /// `Subscribe` from the cursor of the moment it is written, so a resend after a reconnect
    /// starts where the client is now, not where it was when the caller asked.
    Sub(Scope),
    /// `Unsubscribe`. Never resent: a new connection starts with no subscriptions.
    Unsub(Scope),
}

pub(crate) struct Pending {
    pub kind: Kind,
    /// Submission order: op ids minted within one millisecond have none of their own.
    pub n: u64,
    /// The generation of the connection it was last written to (0: never).
    pub sent: u64,
    pub done: Option<oneshot::Sender<Result<Resp, RequestError>>>,
}

pub(crate) struct State {
    /// What the caller holds, kept current from `Changes`.
    pub cursors: Cursors,
    /// Remembered across reconnects, in the order the caller asked.
    pub subs: Vec<Scope>,
    /// Requests written but not answered, and requests not written yet, by op id.
    pub pending: BTreeMap<OpId, Pending>,
    /// The live connection (`None` while down), for `rtt`, `rebind` and `set_background`.
    pub conn: Option<Arc<dyn Conn>>,
    pub link: Option<Link>,
    /// Counts connections; 0 before the first.
    pub gen: u64,
    pub background: bool,
    /// `rebind` while down: stop waiting and dial now.
    pub kick: bool,
    next_n: u64,
    /// The last input seq handed out per pane. Kept here, not in the `Term`: the holder remembers
    /// the highest seq of this process per pane for ten minutes and drops anything at or below it,
    /// so a second `Term` on the same pane must carry on counting.
    seqs: HashMap<PaneId, u32>,
}

fn cursor_of(c: &Cursors, scope: Scope) -> Option<u64> {
    match scope {
        Scope::Global => c.global_rev,
        Scope::Session(sid) => c.sessions.iter().find(|(s, _)| *s == sid).map(|&(_, r)| r),
    }
}

fn moved(old: u64, rev: u64, reset: bool) -> u64 {
    if reset {
        rev
    } else {
        old.max(rev)
    }
}

impl State {
    pub fn new(cursors: Cursors) -> State {
        State {
            cursors,
            subs: Vec::new(),
            pending: BTreeMap::new(),
            conn: None,
            link: None,
            gen: 0,
            background: false,
            kick: false,
            next_n: 0,
            seqs: HashMap::new(),
        }
    }

    pub fn add(
        &mut self,
        op: OpId,
        kind: Kind,
        done: Option<oneshot::Sender<Result<Resp, RequestError>>>,
    ) {
        self.next_n += 1;
        let n = self.next_n;
        self.pending.insert(
            op,
            Pending {
                kind,
                n,
                sent: 0,
                done,
            },
        );
    }

    /// Moves the cursor of `scope` to `rev` (never backwards, unless `reset`: the daemon answered
    /// a cursor it could not honour with the full set, and its rev may be behind ours).
    pub fn advance(&mut self, scope: Scope, rev: u64, reset: bool) {
        match scope {
            Scope::Global => {
                self.cursors.global_rev = Some(
                    self.cursors
                        .global_rev
                        .map_or(rev, |old| moved(old, rev, reset)),
                );
            }
            Scope::Session(sid) => {
                match self.cursors.sessions.iter_mut().find(|(s, _)| *s == sid) {
                    Some((_, r)) => *r = moved(*r, rev, reset),
                    None => self.cursors.sessions.push((sid, rev)),
                }
            }
        }
    }

    /// Not subscribed any more: the scope, its cursor and a subscribe still waiting to go out.
    pub fn forget(&mut self, scope: Scope) {
        self.subs.retain(|s| *s != scope);
        match scope {
            Scope::Global => self.cursors.global_rev = None,
            Scope::Session(sid) => self.cursors.sessions.retain(|(s, _)| *s != sid),
        }
        self.pending
            .retain(|_, p| !matches!(p.kind, Kind::Sub(s) if s == scope));
    }

    /// The daemon's database is not the one the cursors come from: they mean nothing. Returns the
    /// scopes the caller held rows of, which it has to drop.
    pub fn adopt_epoch(&mut self, epoch: u64) -> Vec<Scope> {
        if self.cursors.epoch == epoch {
            return Vec::new();
        }
        let held = self
            .cursors
            .global_rev
            .map(|_| Scope::Global)
            .into_iter()
            .chain(
                self.cursors
                    .sessions
                    .iter()
                    .map(|&(sid, _)| Scope::Session(sid)),
            )
            .collect();
        self.cursors = Cursors {
            epoch,
            global_rev: None,
            sessions: Vec::new(),
        };
        held
    }

    /// A connection was welcomed: everything still owed goes out on it, and every remembered scope
    /// is subscribed again (a subscribe nobody waits for is made up for those the caller got an
    /// answer to long ago).
    pub fn welcomed(&mut self) {
        self.pending
            .retain(|_, p| !matches!(p.kind, Kind::Unsub(_)));
        for p in self.pending.values_mut() {
            p.sent = 0;
        }
        for scope in self.subs.clone() {
            let owed = self
                .pending
                .values()
                .any(|p| matches!(p.kind, Kind::Sub(s) if s == scope));
            if !owed {
                self.add(OpId::new(), Kind::Sub(scope), None);
            }
        }
    }

    /// The requests to write on connection `gen`, oldest first, marked as written.
    pub fn unsent(&mut self, gen: u64) -> Vec<(OpId, Req)> {
        let mut out = Vec::new();
        for (op, p) in self.pending.iter_mut().filter(|(_, p)| p.sent != gen) {
            p.sent = gen;
            let req = match &p.kind {
                Kind::Req(r) => r.clone(),
                Kind::Sub(scope) => Req::Subscribe {
                    scope: *scope,
                    after: cursor_of(&self.cursors, *scope),
                },
                Kind::Unsub(scope) => Req::Unsubscribe { scope: *scope },
            };
            out.push((p.n, *op, req));
        }
        out.sort_by_key(|(n, ..)| *n);
        out.into_iter().map(|(_, op, req)| (op, req)).collect()
    }

    /// Whether something written on connection `gen` has not been answered.
    pub fn in_flight(&self, gen: u64) -> bool {
        self.pending.values().any(|p| p.sent == gen)
    }

    /// The daemon answered `op`: it is no longer owed. A subscribe it refused is forgotten.
    pub fn answered(&mut self, op: OpId, refused: bool) -> Option<Pending> {
        let p = self.pending.remove(&op)?;
        if let (true, Kind::Sub(scope)) = (refused, &p.kind) {
            self.forget(*scope);
        }
        Some(p)
    }

    /// The next input seq of `pane` (never 0: that means "untracked" to the holder).
    pub fn next_seq(&mut self, pane: PaneId) -> u32 {
        let last = self.seqs.entry(pane).or_insert(0);
        *last = last.wrapping_add(1).max(1);
        *last
    }
}
