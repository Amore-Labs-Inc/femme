//! State shared by every task: the registries of panes, the agent and control connections.
//!
//! Everything here runs on one thread (`current_thread` runtime), so the mutexes are never
//! contended; they exist to make the shared handles `Sync`, and are never held across an `.await`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use femme_holder::wire;
use femme_proto::holder::{HolderAgent, HolderEvent, HolderInfo, HolderResp, Target};
use femme_proto::{Sid, HOLDER_VERSION};
use tokio::sync::{mpsc, watch, Notify};

use crate::agent::AgentHandle;
use crate::pane::PaneHandle;
use crate::proc;

/// Lock, ignoring poisoning (a panic elsewhere must not take the holder down with it).
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A control connection, as seen by event broadcasting.
pub struct Ctl {
    pub id: u64,
    pub out: mpsc::Sender<Vec<u8>>,
}

/// Slots of a control connection's queue that chatty events leave free, so a reply (and an exit
/// event) finds room however busy the panes are.
const EVENT_RESERVE: usize = 32;

/// Queue `bytes` for one control connection; `false` means it was dropped. Essential events
/// (exits) may use the whole queue, the others stop short of the reserve.
fn offer(out: &mpsc::Sender<Vec<u8>>, essential: bool, bytes: Vec<u8>) -> bool {
    (essential || out.capacity() > EVENT_RESERVE) && out.try_send(bytes).is_ok()
}

pub struct Shared {
    pub sid: Sid,
    pub epoch: u64,
    pub started_ms: i64,
    pub t0: Instant,
    /// Every Nth frame carries a `Timing` record (0 = never).
    pub timing_every: u64,
    pub sock_path: PathBuf,
    pub spool_path: PathBuf,
    pub snap_path: PathBuf,
    pub panes: Mutex<BTreeMap<u16, PaneHandle>>,
    pub agent: Mutex<Option<AgentHandle>>,
    pub controls: Mutex<Vec<Ctl>>,
    /// End of the spool file; followers wait on changes.
    pub spool_hi: watch::Sender<u64>,
    pub spool_committed: std::sync::atomic::AtomicU64,
    /// Below this offset the spool is a hole: the daemon stored it, `punch_spool` freed it.
    pub spool_lo: AtomicU64,
    /// Next pane epoch (starts at the holder epoch, so a re-spawned pane never reuses one).
    pub next_epoch: AtomicU64,
    pub next_id: AtomicU64,
    /// Open connections of any role, for the idle exit.
    pub conns: AtomicUsize,
    /// Something a snapshot would record has changed.
    pub dirty: AtomicBool,
    pub stopping: AtomicBool,
    pub shutdown: Notify,
}

impl Shared {
    /// Monotonic nanoseconds since the holder started (the clock `Timing` uses).
    pub fn now_ns(&self) -> u64 {
        self.t0.elapsed().as_nanos() as u64
    }

    pub fn alloc_epoch(&self) -> u64 {
        self.next_epoch.fetch_add(1, Relaxed)
    }

    pub fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Relaxed)
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Relaxed);
    }

    /// Send `ev` to every control connection. Events are lossy and never close a connection: a
    /// peer that is behind misses the chatty ones (`Bell`, `Title`, `Notify`; the title is in
    /// `Info` anyway) while `ChildExited` and `AgentStderr` keep getting through until its queue
    /// is truly full. Replies are not events and are never dropped.
    // ponytail: a peer that never reads keeps its connection (256 queued messages) until it hangs up; add a stall timeout if one ever does
    pub fn emit(&self, ev: HolderEvent) {
        let essential = matches!(
            ev,
            HolderEvent::ChildExited { .. } | HolderEvent::AgentStderr(_)
        );
        let Ok(bytes) = wire::encode(&HolderResp::Event(ev)) else {
            return;
        };
        for c in lock(&self.controls).iter() {
            if !offer(&c.out, essential, bytes.clone()) {
                if essential {
                    tracing::warn!(
                        control = c.id,
                        "control connection is not reading: an exit event was dropped"
                    );
                } else {
                    tracing::debug!(
                        control = c.id,
                        "control connection is behind: an event was dropped"
                    );
                }
            }
        }
    }

    pub fn agent_info(&self) -> Option<HolderAgent> {
        let a = lock(&self.agent).clone()?;
        let mut info = lock(&a.info).clone();
        info.spool_hi = *self.spool_hi.borrow();
        info.spool_lo = self.spool_lo.load(Relaxed);
        Some(info)
    }

    /// Frees the spool below `upto`, which the daemon has stored, a megabyte at a time: a hole is
    /// punched, so offsets stay absolute and a long-lived agent's spool stays near its unread part.
    /// ponytail: `spool_lo` is not persisted; a restarted holder starts it at 0 and the daemon never
    /// reads below what it committed, so the only cost is a reader of the hole getting zeros.
    pub fn punch_spool(&self, upto: u64) {
        const STEP: u64 = 1 << 20;
        let lo = self.spool_lo.load(Relaxed);
        if upto < lo + STEP {
            return;
        }
        let r = std::fs::File::options()
            .write(true)
            .open(&self.spool_path)
            .and_then(|f| {
                use rustix::fs::FallocateFlags as F;
                rustix::fs::fallocate(&f, F::PUNCH_HOLE | F::KEEP_SIZE, 0, upto)
                    .map_err(std::io::Error::from)
            });
        match r {
            Ok(()) => self.spool_lo.store(upto, Relaxed),
            Err(e) => tracing::warn!(%e, upto, "spool not freed"),
        }
    }

    pub fn info(&self) -> HolderInfo {
        HolderInfo {
            v: HOLDER_VERSION,
            sid: self.sid,
            epoch: self.epoch,
            started: self.started_ms,
            panes: lock(&self.panes)
                .values()
                .map(|p| lock(&p.info).clone())
                .collect(),
            agent: self.agent_info(),
        }
    }

    /// Pids of live children (panes and the agent). All of them lead their own process group.
    fn live_pids(&self) -> Vec<u32> {
        let mut v: Vec<u32> = lock(&self.panes)
            .values()
            .filter_map(|p| {
                let i = lock(&p.info);
                i.alive.then_some(i.pid)
            })
            .collect();
        if let Some(a) = self.agent_info() {
            if a.alive {
                v.push(a.pid);
            }
        }
        v
    }

    pub fn any_child_alive(&self) -> bool {
        !self.live_pids().is_empty()
    }

    /// The agent has been reaped but its task has not announced the exit yet.
    fn agent_settling(&self) -> bool {
        lock(&self.agent)
            .as_ref()
            .is_some_and(|a| !a.finished.load(Relaxed))
    }

    /// `Signal { target, sig }`: signal the child itself (not its process group).
    pub fn signal(&self, target: Target, sig: i32) -> Result<(), String> {
        let pid = match target {
            Target::Pane(no) => {
                let panes = lock(&self.panes);
                let pane = panes.get(&no).ok_or_else(|| format!("no pane {no}"))?;
                let info = lock(&pane.info);
                if !info.alive {
                    return Err(format!("pane {no}'s child has exited"));
                }
                info.pid
            }
            Target::Agent => match self.agent_info() {
                Some(a) if a.alive => a.pid,
                _ => return Err("no agent running".into()),
            },
        };
        proc::deliver(pid, sig, false)
    }

    /// `Shutdown`: SIGTERM every child, give them 2 s, SIGKILL whatever is left.
    pub async fn terminate_children(&self) {
        use rustix::process::Signal;
        self.stopping.store(true, Relaxed);
        for pid in self.live_pids() {
            let _ = proc::send(pid, Signal::TERM, true);
        }
        // ponytail: polls every 20 ms for at most 2 s, at shutdown only; wait on the pane and agent tasks instead if that ever matters
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while (self.any_child_alive() || self.agent_settling())
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        for pid in self.live_pids() {
            tracing::warn!(pid, "child ignored SIGTERM, killing");
            let _ = proc::send(pid, Signal::KILL, true);
        }
        // Let the pane tasks reap, announce the exits and flush them to the control connections.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chatty_events_stop_short_of_the_reserve_and_nothing_ever_closes_the_queue() {
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
        let mut chatty = 0;
        while offer(&tx, false, vec![0]) {
            chatty += 1;
        }
        assert_eq!(
            chatty,
            256 - EVENT_RESERVE,
            "chatty events leave the reserve free"
        );
        for _ in 0..EVENT_RESERVE {
            assert!(
                offer(&tx, true, vec![1]),
                "an exit event may use the reserve"
            );
        }
        assert!(
            !offer(&tx, true, vec![1]),
            "a full queue drops the event instead of waiting"
        );
        assert!(!tx.is_closed());
        // The peer catches up: the queue takes events again, in order, and is still open.
        assert_eq!(rx.try_recv().unwrap(), vec![0]);
        assert!(offer(&tx, true, vec![2]));
        assert!(!offer(&tx, false, vec![2]));
        while rx.try_recv().is_ok() {}
        assert!(offer(&tx, false, vec![3]));
    }
}
