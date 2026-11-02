//! One attached pane. A task per [`Term`] owns the pane's `term` stream and finds the connection
//! through [`Shared::live`]: when a new connection is published it opens the pane there with the
//! rev of the last frame it delivered (so the first frame is a delta from what the app holds, not
//! a replay) and writes the input the holder has not confirmed again, with the same sequence
//! numbers, which the holder drops if it already has them.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;

use femme_proto::rpc::StreamKind;
use femme_proto::term::{TermDown, TermUp};
use femme_proto::PaneId;
use femme_transport::Conn;
use tokio::sync::{mpsc, watch};

use crate::{Live, Shared};

/// The most unconfirmed input one [`Term`] holds: past it, [`Term::send`] hands the input back.
const MAX_QUEUED: usize = 64 << 10;

/// A pane attached through the client: `Frame`s and the like come out of [`recv`](Term::recv), input
/// goes in with [`send`](Term::send), and a lost connection is none of the caller's business.
pub struct Term {
    pane: PaneId,
    shared: Arc<Shared>,
    up: mpsc::UnboundedSender<TermUp>,
    down: mpsc::UnboundedReceiver<TermDown>,
    /// Bytes of input sent and not yet confirmed by a frame's `applied_seq`.
    queued: Arc<AtomicUsize>,
}

/// What an input costs against [`MAX_QUEUED`]; 0 for what is not kept for a resend.
fn weight(up: &TermUp) -> usize {
    match up {
        TermUp::Key { .. } | TermUp::Mouse { .. } => 16,
        TermUp::Paste { text, .. } => 16 + text.len(),
        TermUp::Bytes { data, .. } => 16 + data.len(),
        _ => 0,
    }
}

fn seq_of(up: &TermUp) -> Option<u32> {
    match up {
        TermUp::Key { seq, .. }
        | TermUp::Paste { seq, .. }
        | TermUp::Bytes { seq, .. }
        | TermUp::Mouse { seq, .. } => Some(*seq),
        _ => None,
    }
}

fn stamp(up: &mut TermUp, to: u32) {
    match up {
        TermUp::Key { seq, .. }
        | TermUp::Paste { seq, .. }
        | TermUp::Bytes { seq, .. }
        | TermUp::Mouse { seq, .. } => *seq = to,
        _ => {}
    }
}

impl Term {
    pub(crate) fn open(shared: Arc<Shared>, pane: PaneId) -> Term {
        let (up, inbox) = mpsc::unbounded_channel();
        let (outbox, down) = mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let task = Task {
            live: shared.live.subscribe(),
            sh: shared.clone(),
            pane,
            inbox,
            down: outbox,
            queued: queued.clone(),
            epoch: 0,
            rev: 0,
            inputs: VecDeque::new(),
            resize: None,
            focus: None,
        };
        tokio::spawn(task.run());
        Term {
            pane,
            shared,
            up,
            down,
            queued,
        }
    }

    /// Sends input (`Key`, `Paste`, `Bytes`, `Mouse`), `Resize`, `Focus` or `GetRows`. Input gets
    /// its sequence number here (whatever `seq` it carries is replaced) and is kept until a frame
    /// says the holder wrote it to the pty; connected or not, it goes out in order, once. Past
    /// 64 KiB of unconfirmed input, or for `Open` and `Ack` (the client's own business), the
    /// message comes back as the error. A `Resize` or `Focus` is resent after a reconnect, a
    /// `GetRows` is not.
    pub fn send(&self, mut up: TermUp) -> Result<(), TermUp> {
        if matches!(up, TermUp::Open { .. } | TermUp::Ack { .. }) {
            return Err(up);
        }
        let cost = weight(&up);
        let sent = {
            // The cap, the number and the place in the queue are taken together: seqs must not
            // overtake each other, and two senders must not both squeeze under the cap.
            let mut st = self.shared.state();
            if cost > 0 {
                if self.queued.load(Relaxed) + cost > MAX_QUEUED {
                    return Err(up);
                }
                self.queued.fetch_add(cost, Relaxed);
                stamp(&mut up, st.next_seq(self.pane));
            }
            self.up.send(up)
        };
        sent.map_err(|failed| {
            self.queued.fetch_sub(cost, Relaxed);
            failed.0
        })
    }

    /// The next message from the pane: a `Frame`, `Rows` (the answer to `GetRows`), `Bell`, or
    /// `Exited`, after which there is nothing more. A lost connection does not end it: the next
    /// frame is the delta from the last one delivered. `None` once the pane has exited or the
    /// client is gone.
    pub async fn recv(&mut self) -> Option<TermDown> {
        self.down.recv().await
    }

    /// The app applied every frame up to `rev`: this is the `Ack` that keeps the holder sending.
    /// It stops after 8 frames or 128 KiB the app has not acknowledged.
    pub fn applied(&self, rev: u64) {
        let _ = self.up.send(TermUp::Ack { rev });
    }

    /// Detaches the pane (also what dropping the `Term` does). The pane itself keeps running.
    pub fn close(self) {}
}

/// How one stream of a pane ended.
enum End {
    /// The connection went; wait for the next one.
    Lost,
    /// `Exited` was delivered, or the stream failed with the connection up.
    Exited,
    /// The `Term` or the client is gone.
    Closed,
}

struct Task {
    sh: Arc<Shared>,
    pane: PaneId,
    inbox: mpsc::UnboundedReceiver<TermUp>,
    down: mpsc::UnboundedSender<TermDown>,
    queued: Arc<AtomicUsize>,
    live: watch::Receiver<Live>,
    // What outlives a connection:
    /// Holder instance and rev of the last frame delivered.
    epoch: u64,
    rev: u64,
    /// Input sent and not confirmed, in seq order.
    inputs: VecDeque<TermUp>,
    resize: Option<TermUp>,
    focus: Option<TermUp>,
}

impl Task {
    async fn run(mut self) {
        let mut used = 0;
        loop {
            let Some((gen, conn)) = self.next_conn(used).await else {
                return;
            };
            used = gen;
            match self.stream(&conn, gen).await {
                End::Lost => {}
                End::Exited | End::Closed => return,
            }
        }
    }

    /// Waits for a connection newer than `used`, keeping the input that arrives meanwhile.
    async fn next_conn(&mut self, used: u64) -> Option<(u64, Arc<dyn Conn>)> {
        loop {
            {
                let live = self.live.borrow_and_update();
                if live.stopped {
                    return None;
                }
                if let Some(conn) = &live.conn {
                    if live.gen != used {
                        return Some((live.gen, conn.clone()));
                    }
                }
            }
            tokio::select! {
                changed = self.live.changed() => changed.ok()?,
                up = self.inbox.recv() => { self.keep(up?); }
            }
        }
    }

    /// Notes a message of the `Term` (input is kept until confirmed, the latest `Resize` and
    /// `Focus` are kept for a reconnect) and returns it, to be written now if there is a stream.
    fn keep(&mut self, up: TermUp) -> TermUp {
        match &up {
            TermUp::Key { .. }
            | TermUp::Paste { .. }
            | TermUp::Bytes { .. }
            | TermUp::Mouse { .. } => self.inputs.push_back(up.clone()),
            TermUp::Resize { .. } => self.resize = Some(up.clone()),
            TermUp::Focus(_) => self.focus = Some(up.clone()),
            _ => {}
        }
        up
    }

    /// Hands a message of the holder to the `Term`; whether it was `Exited`.
    fn deliver(&mut self, msg: TermDown) -> bool {
        let exited = matches!(msg, TermDown::Exited { .. });
        if let TermDown::Frame(f) = &msg {
            (self.epoch, self.rev) = (f.epoch, f.rev);
            // What the holder has written to the pty is never sent again.
            while self
                .inputs
                .front()
                .and_then(seq_of)
                .is_some_and(|seq| seq <= f.applied_seq)
            {
                if let Some(done) = self.inputs.pop_front() {
                    self.queued.fetch_sub(weight(&done), Relaxed);
                }
            }
        }
        let _ = self.down.send(msg);
        exited
    }

    /// One stream of the pane on `conn`, until it ends.
    async fn stream(&mut self, conn: &Arc<dyn Conn>, gen: u64) -> End {
        let Ok((mut tx, mut rx)) = conn.open_kind(StreamKind::Term(self.pane)).await else {
            return self.lost(conn, gen).await;
        };
        // The pane is asked for from what the app holds; the input the holder may not have is
        // written again behind it, same seqs.
        tracing::debug!(
            pane = ?self.pane,
            epoch = self.epoch,
            have_rev = self.rev,
            resent = self.inputs.len(),
            "pane opened"
        );
        let mut start = vec![TermUp::Open {
            pane: self.pane,
            epoch: self.epoch,
            have_rev: self.rev,
            client_instance: self.sh.instance,
        }];
        start.extend(self.inputs.iter().cloned());
        start.extend(self.resize.clone());
        start.extend(self.focus.clone());
        for up in &start {
            if tx.send(up).await.is_err() {
                return self.lost(conn, gen).await;
            }
        }
        loop {
            tokio::select! {
                msg = rx.recv_msg::<TermDown>() => match msg {
                    Ok(Some(msg)) => {
                        if self.deliver(msg) {
                            return End::Exited;
                        }
                    }
                    _ => return self.lost(conn, gen).await,
                },
                up = self.inbox.recv() => match up {
                    Some(up) => {
                        let up = self.keep(up);
                        if tx.send(&up).await.is_err() {
                            return self.lost(conn, gen).await;
                        }
                    }
                    None => {
                        let _ = tx.finish().await;
                        return End::Closed;
                    }
                },
                changed = self.live.changed() => {
                    if changed.is_err() {
                        return End::Closed;
                    }
                    let live = self.live.borrow_and_update();
                    if live.stopped {
                        return End::Closed;
                    }
                    if live.gen != gen || live.conn.is_none() {
                        return End::Lost;
                    }
                }
            }
        }
    }

    /// The stream failed. With the connection gone (or already replaced) that is the usual end
    /// and the next connection is waited for; a stream that fails on a connection that is fine
    /// means the pane is not there to follow, which is told as an exit.
    async fn lost(&mut self, conn: &Arc<dyn Conn>, gen: u64) -> End {
        let dead = tokio::select! {
            biased;
            _ = conn.closed() => true,
            () = std::future::ready(()) => false,
        };
        let moved = {
            let live = self.live.borrow();
            live.stopped || live.gen != gen || live.conn.is_none()
        };
        if dead || moved {
            return End::Lost;
        }
        tracing::warn!(pane = ?self.pane, "terminal stream failed on a live connection");
        let _ = self.down.send(TermDown::Exited { status: -1 });
        End::Exited
    }
}
