//! One terminal stream, relayed: the desktop's `TermUp` into a holder view, the view's `TermDown`
//! back, each as it comes. The relay adds no timers and no batching; what it does add is measured
//! per hop ([`Hops`]) so the plan's "the daemon adds under 3 ms" is a number, not a belief.

use std::fmt;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use femme_proto::term::{TermDown, TermUp};
use femme_proto::PaneId;
use femme_transport::{Rx, Tx};
use hdrhistogram::Histogram;
use tokio::sync::watch;

use crate::daemon::Daemon;

/// Serves one `Term(pane)` stream until either side ends it. Terminals are for a connection that
/// has said `Hello` and been welcomed (it speaks a protocol we do): a stream that arrives first
/// waits for that up to `wait`, and is reset if it does not come. The first message must be
/// `TermUp::Open`: it attaches a view of the pane in its holder (the holder answers with a
/// snapshot, or a delta from `have_rev`); every later `TermUp` goes to the view as it is, a second
/// `Open` included (that is the resync request). A pane that is not there, or a holder that ends
/// while the stream is up, gives `Exited { status: -1 }` and the end of the stream.
pub(crate) async fn relay(
    daemon: Arc<Daemon>,
    hops: Arc<Hops>,
    pane: PaneId,
    mut tx: Tx,
    mut rx: Rx,
    mut welcomed: watch::Receiver<bool>,
    wait: Duration,
) {
    if !matches!(
        tokio::time::timeout(wait, welcomed.wait_for(|w| *w)).await,
        Ok(Ok(_))
    ) {
        return tx.reset();
    }
    let Ok(Some(TermUp::Open {
        pane: opened,
        epoch,
        have_rev,
        client_instance,
    })) = rx.recv_msg::<TermUp>().await
    else {
        return tx.reset(); // not what a terminal stream starts with
    };
    if opened != pane {
        return tx.reset();
    }
    let view = async {
        let holder = daemon.holder(pane.sid).await.ok()?;
        holder
            .attach(pane.no, epoch, have_rev, client_instance)
            .await
            .ok()
    }
    .await;
    // No holder, or no such pane (the holder answers an unknown pane by hanging up).
    let Some((up, mut down)) = view else {
        return exited(&mut tx, -1).await;
    };
    // Two directions, each awaited in its own future: a send that waits on the other side's full
    // queue must not stop the other direction (Tx::send is only ever awaited in its own branch).
    let upward = async {
        while let Ok(Some(m)) = rx.recv_msg::<TermUp>().await {
            let t = Instant::now();
            if up.send(m).await.is_err() {
                // The view ended; `downward` drains what it left (its `Exited`) and finishes.
                return std::future::pending().await;
            }
            hops.note(Hop::Up, t);
        }
    };
    let downward = async {
        let mut exited = false;
        while let Some(m) = down.recv().await {
            exited |= matches!(m, TermDown::Exited { .. });
            let t = Instant::now();
            if tx.send(&m).await.is_err() {
                return true; // the desktop is gone: nobody to tell
            }
            hops.note(Hop::Down, t);
        }
        exited
    };
    let told = tokio::select! {
        () = upward => return, // the desktop closed the stream: the view goes with it
        told = downward => told,
    };
    if told {
        let _ = tx.finish().await;
    } else {
        exited(&mut tx, -1).await;
    }
}

async fn exited(tx: &mut Tx, status: i32) {
    let _ = tx.send(&TermDown::Exited { status }).await;
    let _ = tx.finish().await;
}

#[derive(Clone, Copy)]
enum Hop {
    /// A `TermUp` decoded off the desktop's stream → in the holder view's channel.
    Up,
    /// A `TermDown` out of the holder view's channel → handed to the QUIC (or mux) stream.
    Down,
}

/// What one hop took, over everything relayed so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quantiles {
    /// Messages measured.
    pub n: u64,
    pub p50: Duration,
    pub p99: Duration,
    pub p999: Duration,
    pub max: Duration,
}

/// Both hops of the relay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// `TermUp` read → written to the holder view's channel.
    pub up: Quantiles,
    /// `TermDown` received from the holder view → written to the stream.
    pub down: Quantiles,
}

impl fmt::Display for Quantiles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let us = |d: Duration| d.as_secs_f64() * 1e6;
        write!(
            f,
            "n={} p50={:.1}us p99={:.1}us p99.9={:.1}us max={:.1}us",
            self.n,
            us(self.p50),
            us(self.p99),
            us(self.p999),
            us(self.max)
        )
    }
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "term up (read -> holder channel): {}\nterm down (holder channel -> stream): {}",
            self.up, self.down
        )
    }
}

/// One histogram per hop, nanoseconds. 2 significant digits (1 %), up to 10 s: about 50 KiB each.
pub struct Hops {
    up: Mutex<Histogram<u64>>,
    down: Mutex<Histogram<u64>>,
}

/// Everything every front in this process relayed (a daemon has one; tests have several).
static ALL: LazyLock<Hops> = LazyLock::new(Hops::new);

impl Hops {
    pub(crate) fn new() -> Hops {
        let histogram = || {
            Mutex::new(Histogram::new_with_bounds(1, 10_000_000_000, 2).expect("bounds are valid"))
        };
        Hops {
            up: histogram(),
            down: histogram(),
        }
    }

    fn record(&self, hop: Hop, ns: u64) {
        let h = match hop {
            Hop::Up => &self.up,
            Hop::Down => &self.down,
        };
        h.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .saturating_record(ns.max(1));
    }

    /// Records the time since `since` for `hop`, here and in the process-wide totals.
    fn note(&self, hop: Hop, since: Instant) {
        let ns = u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.record(hop, ns);
        ALL.record(hop, ns);
    }

    pub fn stats(&self) -> Stats {
        let read = |h: &Mutex<Histogram<u64>>| {
            let h = h.lock().unwrap_or_else(PoisonError::into_inner);
            let ns = |v: u64| Duration::from_nanos(v);
            Quantiles {
                n: h.len(),
                p50: ns(h.value_at_quantile(0.5)),
                p99: ns(h.value_at_quantile(0.99)),
                p999: ns(h.value_at_quantile(0.999)),
                max: ns(h.max()),
            }
        };
        Stats {
            up: read(&self.up),
            down: read(&self.down),
        }
    }
}

/// The relay's latency so far, across every front in this process, as text (what the daemon logs
/// each minute while terminals are in use). Tests that assert on a number read
/// `FrontHandle::stats`, which is their own front's.
pub fn stats() -> String {
    ALL.stats().to_string()
}
