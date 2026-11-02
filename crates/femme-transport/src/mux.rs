//! The SSH fallback: the same framed streams over any byte pipe.
//!
//! [`mux_client`] and [`mux_server`] turn an `AsyncRead` + `AsyncWrite` pair into a [`Conn`] with the
//! same semantics as the QUIC one: independent bidirectional streams, strict priorities, flow
//! control, datagrams. Normally the pair is the stdio of `ssh -T host femmed proxy`
//! ([`ssh_connect`]); the daemon end runs on the unix socket the proxy relays into.
//!
//! # Wire format
//!
//! Every frame is `varint stream_id | u8 flags | varint len | data[len]` with `len` at most 16 KiB.
//! A frame carries exactly one flag; 0 means stream data.
//!
//! | flag | value | stream | data |
//! |---|---|---|---|
//! | (data) | 0 | the stream | payload bytes, 0 to 16384 (an empty frame is ignored) |
//! | `OPEN` | 1 | new id | none. Odd ids come from the client, even from the server. The stream's kind frame follows at once, in the control tier |
//! | `FIN` | 2 | the stream | none. The sender is done: the peer reads `None` after the data |
//! | `RST` | 4 | the stream | none. The sender aborts its direction: the peer's reads fail |
//! | `DGRAM` | 8 | 0 | an application datagram, at most [`MAX_DATAGRAM`](crate::MAX_DATAGRAM) (1000) bytes |
//! | `WINDOW` | 16 | the stream, or 0 | varint credit. On a stream: bytes the peer may send. On id 0: streams the peer may open |
//! | `STOP` | 32 | the stream | none. The receiver stops reading: the peer's writes fail |
//! | `PING`, `PONG` | 64, 128 | 0 | 8 bytes, echoed back; both ends ping, for [`Conn::rtt`] and to tell a dead peer from a quiet one |
//!
//! `STOP`, `PING` and `PONG` go beyond the five flags the plan lists: `STOP` is QUIC's
//! `STOP_SENDING` (dropping an `Rx` must stop a bulk sender), the other two keep application
//! datagrams free of a marker byte.
//!
//! A frame for a stream that was already forgotten (a late `WINDOW`, `STOP` or data) is dropped. A
//! frame for an id that was never opened, with a flag that does not belong on that id, or with more
//! credit than the sender could have been given, is a protocol error and kills the connection
//! ([`Error::Proto`] from [`Conn::closed`], [`Error::Closed`] from everything else).
//!
//! # Scheduling and flow control
//!
//! One writer task owns the pipe and writes, in this order: every queued control frame (`OPEN`,
//! `RST`, `STOP`, `WINDOW`), every queued datagram, then ONE data chunk of at most 16 KiB, picked
//! as highest priority first and oldest first among equals. It then starts over, so a keystroke
//! waits behind at most the one chunk already being written (plus whatever the pipe itself and
//! `ssh` buffer, which no code here can reorder).
//!
//! Each stream direction starts with 64 KiB of credit. A write consumes credit as it queues data and
//! waits when none is left; the receiver grants credit back (`WINDOW`) as the application reads,
//! in steps of 16 KiB. One stream therefore moves at most 64 KiB per round trip.
//!
//! Opening is limited the same way: each end starts with credit for 128 streams, spends one per
//! `OPEN`, and gets one back (`WINDOW` on id 0) when the peer forgets a stream that end opened, so a
//! slow acceptor slows the opener down instead of losing its streams. A peer that opens more than it
//! has credit for is cut off.
//!
//! # Liveness and closing
//!
//! Both ends ping every second, every 15 s after [`Conn::set_background`], and a peer from which
//! nothing at all arrives for three round trips (at least 1.5 s) after a ping is declared dead
//! (plan rule 7). Any bytes count as a sign of life, so a busy link is never mistaken for a dead one.
//!
//! [`Conn::close`] is abrupt. Dropping the last handle is not: the writer first sends what the
//! handles queued (a last message, the end of its stream), for at most a second, and then closes.
//!
//! ponytail: the 64 KiB window caps one bulk stream at about 0.8 MB/s on an 80 ms path; make the
//! window per kind (or grow it with the measured RTT) if bulk over SSH ever matters.
//!
//! ponytail: the plan gives keystrokes their own TCP connection over SSH to dodge head-of-line
//! blocking. Not built: priorities and the credit window keep a keystroke behind at most the data
//! `ssh` itself has already buffered (up to its 2 MiB channel window, which nothing here can
//! reorder). Open a second `ssh` for `Term` streams if p2_latency shows that tail matters.
//!
//! ponytail: the credit counts payload bytes, not the ~64 bytes of bookkeeping a frame costs in
//! memory, so a hostile peer could queue about 4 MiB of one-byte frames per stream (500 MiB over 128
//! streams). The peer here is the user's own device behind SSH; charge a minimum per frame against
//! the credit (both ends) if that ever stops being true.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Buf, Bytes, BytesMut};
use femme_proto::codec::{get_varint, put_varint, Framer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::{mpsc, watch, Mutex, Notify, Semaphore};
use tokio::time::Instant;

use crate::{prio, read_kind, Conn, Error, Rx, RxIo, StreamKind, Tx, TxIo, KIND_TIMEOUT};

type Accepted = (StreamKind, Tx, Rx);

const OPEN: u8 = 1;
const FIN: u8 = 2;
const RST: u8 = 4;
const DGRAM: u8 = 8;
const WINDOW: u8 = 16;
const STOP: u8 = 32;
const PING: u8 = 64;
const PONG: u8 = 128;

/// Largest frame body, and the largest chunk the scheduler writes at once.
const CHUNK: usize = 16 << 10;
/// Credit every stream direction starts with.
const INIT_WINDOW: usize = 64 << 10;
/// The receiver grants credit back once the application has read this much.
const GRANT_AT: usize = 16 << 10;
/// Streams each end may have open at the other end. The opener starts with this much credit and
/// waits when it is spent; the acceptor returns one unit per stream it forgets.
const MAX_STREAMS: usize = 128;
/// Backstop for the control queue. Stream credit already bounds it (at most 128 streams each way,
/// a handful of control frames per stream: about 1800 in all), so passing this many means the
/// accounting is broken or the peer found a way around it; the connection is cut rather than
/// letting the queue grow.
const MAX_CTL: usize = 32 * MAX_STREAMS;
const MAX_DGRAM: usize = crate::MAX_DATAGRAM;
/// Datagrams queued in either direction; the oldest is dropped beyond this.
const DGRAM_QUEUE: usize = 64;
const READ_BUF: usize = 64 << 10;
/// Ping interval with the app in the foreground (plan rule 7).
const PING_EVERY: Duration = Duration::from_secs(1);
/// Ping interval after [`Conn::set_background`].
const PING_BACKGROUND: Duration = Duration::from_secs(15);
/// A peer is dead when nothing at all has come from it for three round trips after a ping, and never
/// in less than this (plan rule 7).
const DEAD_MIN: Duration = Duration::from_millis(1500);
/// How long dropping the last handle waits for queued data to reach the pipe before closing anyway.
const DRAIN_MAX: Duration = Duration::from_secs(1);
/// How long [`command_connect`] waits for the far end to answer its first ping.
const READY_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------------------------------------
// frames

#[derive(Debug, Clone, PartialEq, Eq)]
struct Frame {
    id: u64,
    flags: u8,
    data: Bytes,
}

impl Frame {
    fn ctl(id: u64, flags: u8) -> Frame {
        Frame {
            id,
            flags,
            data: Bytes::new(),
        }
    }
}

fn encode(out: &mut Vec<u8>, id: u64, flags: u8, data: &[u8]) {
    put_varint(out, id);
    out.push(flags);
    put_varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

/// Take one frame off the front of `buf`; `None` when it holds only part of one. The length is
/// checked as soon as the header is in, so a hostile length cannot make us wait for it.
fn parse(buf: &mut BytesMut) -> Result<Option<Frame>, Error> {
    let Some((id, n_id)) = get_varint(buf)? else {
        return Ok(None);
    };
    let Some(&flags) = buf.get(n_id) else {
        return Ok(None);
    };
    let Some((len, n_len)) = get_varint(&buf[n_id + 1..])? else {
        return Ok(None);
    };
    if len > CHUNK as u64 {
        return Err(Error::Proto(format!(
            "mux frame of {len} bytes (the limit is {CHUNK})"
        )));
    }
    let head = n_id + 1 + n_len;
    let len = len as usize;
    if buf.len() < head + len {
        return Ok(None);
    }
    // Copy the payload: a slice of `buf` would keep the whole 64 KiB read block alive for as long as
    // the frame sits unread, and the credit (payload bytes) would no longer bound memory.
    let data = Bytes::copy_from_slice(&buf[head..head + len]);
    buf.advance(head + len);
    Ok(Some(Frame { id, flags, data }))
}

// ---------------------------------------------------------------------------------------------
// state

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SendState {
    /// Accepting writes.
    Open,
    /// `finish` was called; the FIN marker waits in the queue behind the data.
    Fin,
    /// We reset this direction.
    Rst,
    /// The peer sent STOP.
    Stop,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RecvState {
    Open,
    /// The peer finished; what is buffered is still readable.
    Fin,
    /// The peer reset; buffered data was discarded.
    Reset,
    /// Our `Rx` was dropped and the peer told to stop; anything still arriving is discarded.
    Stopped,
}

struct Stream {
    // sending direction
    prio: i32,
    /// Bytes we may still queue.
    credit: usize,
    /// Chunks not yet on the wire, with their global order number. An empty chunk is the FIN marker.
    out: VecDeque<(u64, Bytes)>,
    send: SendState,
    fin_sent: bool,
    send_wake: Arc<Notify>,
    // receiving direction
    buf: VecDeque<Bytes>,
    /// Credit the peer still has; it may not send more than this.
    window: usize,
    /// Bytes read by the application and not yet granted back.
    unacked: usize,
    recv: RecvState,
    recv_wake: Arc<Notify>,
    // handles
    tx_alive: bool,
    rx_alive: bool,
    /// The peer opened it: forgetting it gives the peer a unit of stream credit back.
    peer: bool,
}

impl Stream {
    fn new(prio: i32, peer: bool) -> Stream {
        Stream {
            prio,
            credit: INIT_WINDOW,
            out: VecDeque::new(),
            send: SendState::Open,
            fin_sent: false,
            send_wake: Arc::new(Notify::new()),
            buf: VecDeque::new(),
            window: INIT_WINDOW,
            unacked: 0,
            recv: RecvState::Open,
            recv_wake: Arc::new(Notify::new()),
            tx_alive: true,
            rx_alive: true,
            peer,
        }
    }

    /// Nothing more of our sending direction will go on the wire.
    fn send_over(&self) -> bool {
        matches!(self.send, SendState::Rst | SendState::Stop) || self.fin_sent
    }
}

struct Inner {
    streams: HashMap<u64, Stream>,
    /// OPEN (with the kind frame behind it), RST, STOP and WINDOW frames: always written first.
    ctl: VecDeque<Frame>,
    /// Datagrams, pings and pongs: written after `ctl`, before any stream data.
    dgram_out: VecDeque<Frame>,
    dgram_in: VecDeque<Bytes>,
    /// Next id we open with.
    next_id: u64,
    /// Lowest id the peer may open next (ids only go up).
    next_peer: u64,
    /// Streams the peer opened that we have not forgotten yet; never more than `MAX_STREAMS`.
    peer_open: usize,
    /// Order number for queued chunks: FIFO among streams of equal priority.
    seq: u64,
}

impl Inner {
    fn new(client: bool) -> Inner {
        Inner {
            streams: HashMap::new(),
            ctl: VecDeque::new(),
            dgram_out: VecDeque::new(),
            dgram_in: VecDeque::new(),
            next_id: if client { 1 } else { 2 },
            next_peer: if client { 2 } else { 1 },
            peer_open: 0,
            seq: 0,
        }
    }

    /// Forget a stream once both directions are over and both handles are gone. Returns `true` when
    /// that queued a control frame, which is the credit for a stream the peer opened going back to
    /// it; the caller must wake the writer.
    fn reap(&mut self, id: u64) -> bool {
        let Some(s) = self.streams.get(&id) else {
            return false;
        };
        if s.tx_alive || s.rx_alive || !s.send_over() || s.recv == RecvState::Open {
            return false;
        }
        let peer = s.peer;
        self.streams.remove(&id);
        if peer {
            self.peer_open -= 1;
            self.ctl.push_back(Frame {
                id: 0,
                flags: WINDOW,
                data: Bytes::from_static(&[1]),
            });
        }
        peer
    }

    /// Register a stream we open. OPEN and the kind frame (the stream's first data frame) both go
    /// into the control queue, so the kind cannot wait behind higher-priority data and trip the
    /// peer's kind timeout.
    fn open_stream(&mut self, prio: i32, kind: &StreamKind) -> Result<u64, Error> {
        let mut kind_frame = Vec::new();
        Framer::new(false).frame(kind, &mut kind_frame)?;
        let id = self.next_id;
        self.next_id += 2;
        let mut s = Stream::new(prio, false);
        s.credit -= kind_frame.len();
        self.streams.insert(id, s);
        self.ctl.push_back(Frame::ctl(id, OPEN));
        self.ctl.push_back(Frame {
            id,
            flags: 0,
            data: kind_frame.into(),
        });
        Ok(id)
    }

    /// Queue the end of a sending direction behind its data. No effect unless it is still open.
    fn finish(&mut self, id: u64) {
        let seq = self.seq;
        if let Some(s) = self.streams.get_mut(&id) {
            if s.send == SendState::Open {
                s.send = SendState::Fin;
                s.out.push_back((seq, Bytes::new()));
                self.seq += 1;
            }
        }
    }

    /// Everything the next write to the pipe should carry: all control frames, all datagrams, and
    /// one data chunk from the best stream. Empty when there is nothing to send.
    fn drain_into(&mut self, buf: &mut Vec<u8>) {
        while let Some(f) = self.ctl.pop_front() {
            encode(buf, f.id, f.flags, &f.data);
        }
        while let Some(f) = self.dgram_out.pop_front() {
            encode(buf, f.id, f.flags, &f.data);
        }
        // Highest priority, then the oldest chunk. A scan is fine: at most 128 streams.
        let best = self
            .streams
            .iter()
            .filter_map(|(id, s)| {
                s.out
                    .front()
                    .map(|(seq, _)| (s.prio, std::cmp::Reverse(*seq), *id))
            })
            .max();
        let Some((_, _, id)) = best else { return };
        let Some(s) = self.streams.get_mut(&id) else {
            return;
        };
        let Some((_, chunk)) = s.out.pop_front() else {
            return;
        };
        if chunk.is_empty() {
            encode(buf, id, FIN, &[]);
            s.fin_sent = true;
            self.reap(id);
        } else {
            encode(buf, id, 0, &chunk);
        }
    }

    fn queue_dgram(&mut self, f: Frame) {
        if self.dgram_out.len() >= DGRAM_QUEUE {
            self.dgram_out.pop_front();
        }
        self.dgram_out.push_back(f);
    }
}

struct Shared {
    inner: StdMutex<Inner>,
    /// Wakes the writer task (one waiter; the permit is kept if it is busy).
    writer_wake: Notify,
    dgram_wake: Notify,
    /// Pinged when a pong comes in.
    ready_wake: Notify,
    /// `Some(reason)` once the connection is dead; the first reason wins.
    closed: watch::Sender<Option<Error>>,
    /// Credit for streams we open: one permit per stream, returned only when the peer says (WINDOW on
    /// id 0) that it forgot one.
    slots: Arc<Semaphore>,
    /// Smoothed round trip in microseconds, 0 until the first pong.
    rtt_us: AtomicU64,
    /// How long the peer may stay silent before its first pong: `ssh` may still be logging in.
    startup: Duration,
    /// Bumped whenever bytes arrive from the peer; the pinger waits on it.
    heard: watch::Sender<()>,
    /// The app is out of sight: ping every `PING_BACKGROUND` instead of every `PING_EVERY`.
    background: AtomicBool,
    /// Wakes the pinger to ping now (the app went away or came back).
    ping_wake: Notify,
    /// The last handle is gone: the writer sends what is queued, then closes.
    draining: AtomicBool,
    start: Instant,
    client: bool,
    /// The `ssh` process, once [`command_connect`] has handed it over; killed and reaped when the
    /// connection dies.
    child: StdMutex<Option<Child>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn closed_err(&self) -> Option<Error> {
        self.closed.borrow().clone()
    }

    /// What a stream, accept or datagram call reports once the connection is dead: always
    /// [`Error::Closed`], as the crate docs promise, with the reason (a protocol violation included)
    /// in the text. `Conn::closed` keeps the original error.
    fn op_err(&self) -> Option<Error> {
        self.closed_err().map(|e| match e {
            Error::Closed(_) => e,
            other => Error::Closed(other.to_string()),
        })
    }

    fn dead_err(&self) -> Error {
        self.op_err()
            .unwrap_or_else(|| Error::Closed("connection closed".into()))
    }

    /// Did this end open stream `id` (odd ids are the client's)?
    fn is_mine(&self, id: u64) -> bool {
        (id % 2 == 1) == self.client
    }

    fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }

    /// Mark the connection dead and wake everything that waits on it. Never call with `inner` locked.
    fn fail(&self, e: Error) {
        let first = self.closed.send_if_modified(|v| {
            if v.is_some() {
                return false;
            }
            *v = Some(e);
            true
        });
        if !first {
            return;
        }
        self.slots.close();
        self.reap_child();
        self.writer_wake.notify_one();
        self.dgram_wake.notify_one();
        self.ready_wake.notify_one();
        for s in self.lock().streams.values() {
            s.send_wake.notify_one();
            s.recv_wake.notify_one();
        }
    }

    /// Kill the `ssh` process and reap it: killed but never waited for, it stays a zombie for as long
    /// as handles to the dead connection are held.
    fn reap_child(&self) {
        let Some(mut c) = self
            .child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let _ = c.start_kill();
        // Without a runtime (the last handle dropped on a foreign thread) tokio's orphan queue reaps it
        // when `c` drops.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = c.wait().await;
            });
        }
    }

    /// How long a peer may stay silent after a ping before it is declared dead: three round trips, at
    /// least 1.5 s; before the first pong there is no round trip to count, so `startup`.
    fn silence_limit(&self) -> Duration {
        match self.rtt_us.load(Ordering::Relaxed) {
            0 => self.startup,
            us => (Duration::from_micros(us) * 3).max(DEAD_MIN),
        }
    }

    /// Resolves once the first pong has set the RTT.
    async fn ready(&self) {
        while self.rtt_us.load(Ordering::Relaxed) == 0 {
            self.ready_wake.notified().await;
        }
    }

    fn on_frame(
        &self,
        f: Frame,
        guard: &Weak<Guard>,
        out: &mpsc::UnboundedSender<Accepted>,
    ) -> Result<(), Error> {
        if self.draining.load(Ordering::Relaxed) {
            return Ok(()); // closing: nobody is left to read what the peer sends
        }
        let Frame { id, flags, data } = f;
        if id == 0 {
            return self.on_conn_frame(flags, data);
        }
        if flags == OPEN {
            // Upgrade before locking: if this is the last handle, dropping it closes the connection.
            let Some(guard) = guard.upgrade() else {
                return Ok(());
            }; // the last handle just went
            return self.on_open(id, guard, out);
        }
        if !matches!(flags, 0 | FIN | RST | WINDOW | STOP) {
            return Err(Error::Proto(format!(
                "stream {id}: unknown frame flags {flags:#x}"
            )));
        }
        let mut g = self.lock();
        let inner = &mut *g;
        let Some(s) = inner.streams.get_mut(&id) else {
            // A frame for a stream we already forgot (late WINDOW, STOP or data) is dropped; one for
            // an id nobody opened is the peer's mistake.
            let used = if self.is_mine(id) {
                id < inner.next_id
            } else {
                id < inner.next_peer
            };
            return if used {
                Ok(())
            } else {
                Err(Error::Proto(format!(
                    "stream {id}: frame for a stream that was never opened"
                )))
            };
        };
        match flags {
            0 => match s.recv {
                RecvState::Open => {
                    if data.len() > s.window {
                        return Err(Error::Proto(format!(
                            "stream {id}: the peer sent past its credit"
                        )));
                    }
                    s.window -= data.len();
                    if !data.is_empty() {
                        s.buf.push_back(data);
                        s.recv_wake.notify_one();
                    }
                }
                RecvState::Stopped => {}
                _ => {
                    return Err(Error::Proto(format!(
                        "stream {id}: data after the end of the stream"
                    )))
                }
            },
            FIN => {
                if s.recv == RecvState::Open {
                    s.recv = RecvState::Fin;
                    s.recv_wake.notify_one();
                }
            }
            RST => {
                if s.recv == RecvState::Open {
                    s.recv = RecvState::Reset;
                    s.buf.clear();
                    s.recv_wake.notify_one();
                }
            }
            STOP => {
                if !s.send_over() {
                    s.send = SendState::Stop;
                    s.out.clear();
                    s.send_wake.notify_one();
                }
            }
            WINDOW => {
                let Some((n, _)) = get_varint(&data)? else {
                    return Err(Error::Proto(format!("stream {id}: truncated WINDOW frame")));
                };
                // We only ever sent as much as we had credit for, so a correct peer cannot hand back more
                // than the initial window; anything else would let `write` run without waiting.
                match usize::try_from(n)
                    .ok()
                    .and_then(|n| s.credit.checked_add(n))
                {
                    Some(credit) if credit <= INIT_WINDOW => s.credit = credit,
                    _ => {
                        return Err(Error::Proto(format!(
                            "stream {id}: credit past the initial window"
                        )))
                    }
                }
                s.send_wake.notify_one();
            }
            _ => {} // the flags were checked above
        }
        let granted = inner.reap(id);
        let backlog = inner.ctl.len();
        drop(g);
        if granted {
            self.writer_wake.notify_one();
        }
        if backlog > MAX_CTL {
            return Err(Error::Proto(
                "the peer is not reading our control frames".into(),
            ));
        }
        Ok(())
    }

    fn on_open(
        &self,
        id: u64,
        guard: Arc<Guard>,
        out: &mpsc::UnboundedSender<Accepted>,
    ) -> Result<(), Error> {
        let mut g = self.lock();
        let inner = &mut *g;
        let bad = || Error::Proto(format!("bad stream id {id}"));
        if self.is_mine(id) || id < inner.next_peer {
            return Err(bad());
        }
        let next = id.checked_add(2).ok_or_else(bad)?;
        if inner.peer_open >= MAX_STREAMS {
            return Err(Error::Proto(format!("stream {id}: the peer opened more than the {MAX_STREAMS} streams it had credit for")));
        }
        inner.next_peer = next;
        inner.peer_open += 1;
        inner.streams.insert(id, Stream::new(0, true));
        let send = MuxSend {
            guard: guard.clone(),
            id,
        };
        let recv = MuxRecv { guard, id };
        tokio::spawn(accept_stream(send, recv, out.clone()));
        Ok(())
    }

    fn on_conn_frame(&self, flags: u8, data: Bytes) -> Result<(), Error> {
        match flags {
            DGRAM => {
                if data.len() <= MAX_DGRAM {
                    let mut g = self.lock();
                    if g.dgram_in.len() >= DGRAM_QUEUE {
                        g.dgram_in.pop_front();
                    }
                    g.dgram_in.push_back(data);
                    drop(g);
                    self.dgram_wake.notify_one();
                }
            }
            PING => {
                if data.len() <= MAX_DGRAM {
                    self.lock().queue_dgram(Frame {
                        id: 0,
                        flags: PONG,
                        data,
                    });
                    self.writer_wake.notify_one();
                }
            }
            PONG => {
                if let Ok(sent) = <[u8; 8]>::try_from(&data[..]) {
                    let sample = self
                        .now_us()
                        .saturating_sub(u64::from_be_bytes(sent))
                        .max(1);
                    let old = self.rtt_us.load(Ordering::Relaxed);
                    let smoothed = if old == 0 {
                        sample
                    } else {
                        (old * 7 + sample) / 8
                    };
                    self.rtt_us.store(smoothed.max(1), Ordering::Relaxed);
                    self.ready_wake.notify_one();
                }
            }
            WINDOW => {
                let Some((n, _)) = get_varint(&data)? else {
                    return Err(Error::Proto("truncated WINDOW frame".into()));
                };
                // Credit is only ever returned for streams we opened, so it cannot pass the initial grant.
                match usize::try_from(n)
                    .ok()
                    .and_then(|n| self.slots.available_permits().checked_add(n))
                {
                    Some(total) if total <= MAX_STREAMS => self
                        .slots
                        .add_permits(total - self.slots.available_permits()),
                    _ => return Err(Error::Proto("stream credit past the initial limit".into())),
                }
            }
            other => {
                return Err(Error::Proto(format!(
                    "unknown connection-level frame flags {other:#x}"
                )))
            }
        }
        Ok(())
    }
}

/// Closes the connection when the last user-facing handle (`Conn`, `Tx`, `Rx`) goes away. The
/// background tasks hold `Shared` but not this, so they cannot keep the connection alive.
struct Guard {
    shared: Arc<Shared>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let sh = self.shared.clone();
        if sh.closed.borrow().is_some() {
            return;
        }
        // Not `Conn::close`: let the writer send what the handles queued (a last message, the end of
        // its stream) and close when it is done. A pipe that will not take it is closed after DRAIN_MAX.
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return sh.fail(Error::Closed("every handle was dropped".into())); // no runtime to time a drain
        };
        sh.draining.store(true, Ordering::Relaxed);
        sh.writer_wake.notify_one();
        rt.spawn(async move {
            let mut closed = sh.closed.subscribe();
            tokio::select! {
                () = tokio::time::sleep(DRAIN_MAX) => sh.fail(Error::Closed("every handle was dropped".into())),
                () = dead(&mut closed) => {}
            }
        });
    }
}

/// Resolves when `rx` shows the connection dead.
async fn dead(rx: &mut watch::Receiver<Option<Error>>) {
    let _ = rx.wait_for(|c| c.is_some()).await;
}

// ---------------------------------------------------------------------------------------------
// tasks

async fn reader<R: AsyncRead + Unpin>(
    sh: Arc<Shared>,
    guard: Weak<Guard>,
    mut r: R,
    out: mpsc::UnboundedSender<Accepted>,
) {
    let e = read_loop(&sh, &guard, &mut r, &out).await;
    sh.fail(e);
}

async fn read_loop<R: AsyncRead + Unpin>(
    sh: &Shared,
    guard: &Weak<Guard>,
    r: &mut R,
    out: &mpsc::UnboundedSender<Accepted>,
) -> Error {
    let mut closed = sh.closed.subscribe();
    let mut buf = BytesMut::with_capacity(READ_BUF);
    loop {
        loop {
            match parse(&mut buf) {
                Ok(Some(f)) => {
                    if let Err(e) = sh.on_frame(f, guard, out) {
                        return e;
                    }
                }
                Ok(None) => break,
                Err(e) => return e,
            }
        }
        buf.reserve(READ_BUF);
        tokio::select! {
            n = r.read_buf(&mut buf) => match n {
                Ok(0) => return Error::Closed("the peer closed the pipe".into()),
                Ok(_) => {
                    sh.heard.send_replace(());
                }
                Err(e) => return Error::Closed(format!("reading the pipe: {e}")),
            },
            () = dead(&mut closed) => return Error::Closed("closed".into()),
        }
    }
}

async fn writer<W: AsyncWrite + Unpin>(sh: Arc<Shared>, mut w: W) {
    let mut closed = sh.closed.subscribe();
    let mut buf = Vec::with_capacity(CHUNK + 64);
    let res: std::io::Result<()> = async {
        loop {
            if sh.closed.borrow().is_some() {
                return Ok(());
            }
            buf.clear();
            // Read before draining the queues: the handles queued everything before this was set, so an
            // empty drain after it means the last of it is out.
            let draining = sh.draining.load(Ordering::Relaxed);
            sh.lock().drain_into(&mut buf);
            if buf.is_empty() {
                tokio::select! {
                    r = w.flush() => r?,
                    () = dead(&mut closed) => return Ok(()),
                }
                if draining {
                    sh.fail(Error::Closed("every handle was dropped".into()));
                    return Ok(());
                }
                sh.writer_wake.notified().await;
                continue;
            }
            tokio::select! {
                r = w.write_all(&buf) => r?,
                () = dead(&mut closed) => return Ok(()),
            }
        }
    }
    .await;
    if let Err(e) = res {
        sh.fail(Error::Closed(format!("writing the pipe: {e}")));
    }
    // Closing our end tells the peer (and `ssh`) that we are done.
    let _ = tokio::time::timeout(Duration::from_secs(1), w.shutdown()).await;
}

/// Both ends: ping for the RTT every second (every 15 s in the background), and declare the peer dead
/// when nothing at all comes in for `silence_limit` after a ping (plan rule 7). Any bytes count, not
/// only the answer, so a link that is busy with data is never mistaken for a dead one.
async fn pinger(sh: Arc<Shared>) {
    let mut closed = sh.closed.subscribe();
    let mut heard = sh.heard.subscribe();
    loop {
        let sent = Instant::now();
        sh.lock().queue_dgram(Frame {
            id: 0,
            flags: PING,
            data: Bytes::copy_from_slice(&sh.now_us().to_be_bytes()),
        });
        sh.writer_wake.notify_one();
        heard.borrow_and_update();
        let limit = sh.silence_limit();
        tokio::select! {
            biased; // a sign of life beats a timer that fired at the same moment
            () = dead(&mut closed) => return,
            _ = heard.changed() => {}
            () = tokio::time::sleep(limit) => {
                return sh.fail(Error::Closed(format!("nothing came from the peer, not even a ping answer, for {limit:?}")));
            }
        }
        let every = if sh.background.load(Ordering::Relaxed) {
            PING_BACKGROUND
        } else {
            PING_EVERY
        };
        tokio::select! {
            () = tokio::time::sleep_until(sent + every) => {}
            () = sh.ping_wake.notified() => {}
            () = dead(&mut closed) => return,
        }
    }
}

/// Read the kind frame of a stream the peer opened, then hand the stream to `accept`. One task per
/// stream, so a peer that opens a stream and says nothing holds up no one.
async fn accept_stream(mut send: MuxSend, recv: MuxRecv, out: mpsc::UnboundedSender<Accepted>) {
    let mut rx = Rx::new(RxIo::Mux(recv));
    match tokio::time::timeout(KIND_TIMEOUT, read_kind(&mut rx)).await {
        Ok(Ok(kind)) => {
            send.set_priority(prio::of(&kind));
            let _ = out.send((kind, Tx::new(TxIo::Mux(send), &kind), rx));
        }
        // No usable kind: abort our direction; dropping `rx` tells the peer to stop sending.
        _ => send.reset(),
    }
}

// ---------------------------------------------------------------------------------------------
// stream handles

/// Sending half of a mux stream (inside [`Tx`]).
pub(crate) struct MuxSend {
    guard: Arc<Guard>,
    id: u64,
}

/// Receiving half of a mux stream (inside [`Rx`]).
pub(crate) struct MuxRecv {
    guard: Arc<Guard>,
    id: u64,
}

impl MuxSend {
    /// Queue `data` in chunks, waiting for credit. Not cancel-safe.
    pub(crate) async fn write(&mut self, mut data: &[u8]) -> Result<(), Error> {
        let sh = &self.guard.shared;
        while !data.is_empty() {
            let wake = {
                let mut g = sh.lock();
                if let Some(e) = sh.op_err() {
                    return Err(e);
                }
                let inner = &mut *g;
                let seq = inner.seq;
                let s = inner
                    .streams
                    .get_mut(&self.id)
                    .ok_or_else(|| Error::Closed("stream gone".into()))?;
                match s.send {
                    SendState::Open => {}
                    SendState::Stop => return Err(Error::Reset),
                    SendState::Fin | SendState::Rst => {
                        return Err(Error::Closed("write after finish or reset".into()))
                    }
                }
                if s.credit > 0 {
                    let n = s.credit.min(data.len()).min(CHUNK);
                    s.out.push_back((seq, Bytes::copy_from_slice(&data[..n])));
                    s.credit -= n;
                    inner.seq += 1;
                    data = &data[n..];
                    sh.writer_wake.notify_one();
                    continue;
                }
                s.send_wake.clone()
            };
            wake.notified().await;
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self) -> Result<(), Error> {
        let sh = &self.guard.shared;
        sh.lock().finish(self.id);
        sh.writer_wake.notify_one();
        Ok(())
    }

    pub(crate) fn reset(&mut self) {
        let sh = &self.guard.shared;
        {
            let mut g = sh.lock();
            let inner = &mut *g;
            if let Some(s) = inner.streams.get_mut(&self.id) {
                if !s.send_over() {
                    s.send = SendState::Rst;
                    s.out.clear();
                    inner.ctl.push_back(Frame::ctl(self.id, RST));
                }
            }
            inner.reap(self.id);
        }
        sh.writer_wake.notify_one();
    }

    pub(crate) fn set_priority(&mut self, prio: i32) {
        if let Some(s) = self.guard.shared.lock().streams.get_mut(&self.id) {
            s.prio = prio;
        }
    }
}

impl Drop for MuxSend {
    fn drop(&mut self) {
        let sh = &self.guard.shared;
        {
            let mut g = sh.lock();
            g.finish(self.id);
            if let Some(s) = g.streams.get_mut(&self.id) {
                s.tx_alive = false;
            }
            g.reap(self.id);
        }
        sh.writer_wake.notify_one();
    }
}

impl MuxRecv {
    /// The next chunk of stream bytes, `None` once the peer finished. Cancel-safe.
    pub(crate) async fn read(&mut self) -> Result<Option<Bytes>, Error> {
        let sh = &self.guard.shared;
        loop {
            let wake = {
                let mut g = sh.lock();
                let inner = &mut *g;
                let s = inner
                    .streams
                    .get_mut(&self.id)
                    .ok_or_else(|| Error::Closed("stream gone".into()))?;
                if let Some(b) = s.buf.pop_front() {
                    s.unacked += b.len();
                    if s.unacked >= GRANT_AT && s.recv == RecvState::Open {
                        let n = std::mem::take(&mut s.unacked);
                        s.window += n;
                        let mut credit = Vec::with_capacity(4);
                        put_varint(&mut credit, n as u64);
                        inner.ctl.push_back(Frame {
                            id: self.id,
                            flags: WINDOW,
                            data: credit.into(),
                        });
                        sh.writer_wake.notify_one();
                    }
                    return Ok(Some(b));
                }
                match s.recv {
                    RecvState::Fin => return Ok(None),
                    RecvState::Reset => return Err(Error::Reset),
                    RecvState::Open | RecvState::Stopped => {}
                }
                if let Some(e) = sh.op_err() {
                    return Err(e);
                }
                s.recv_wake.clone()
            };
            wake.notified().await;
        }
    }
}

impl Drop for MuxRecv {
    fn drop(&mut self) {
        let sh = &self.guard.shared;
        {
            let mut g = sh.lock();
            let inner = &mut *g;
            if let Some(s) = inner.streams.get_mut(&self.id) {
                s.rx_alive = false;
                s.buf.clear();
                if s.recv == RecvState::Open {
                    s.recv = RecvState::Stopped;
                    inner.ctl.push_back(Frame::ctl(self.id, STOP));
                }
            }
            inner.reap(self.id);
        }
        sh.writer_wake.notify_one();
    }
}

// ---------------------------------------------------------------------------------------------
// the connection

/// One end of a mux connection.
pub(crate) struct MuxConn {
    guard: Arc<Guard>,
    accepted: Mutex<mpsc::UnboundedReceiver<Accepted>>,
}

fn spawn_mux<R, W>(r: R, w: W, client: bool, startup: Duration) -> Arc<MuxConn>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let shared = Arc::new(Shared {
        inner: StdMutex::new(Inner::new(client)),
        writer_wake: Notify::new(),
        dgram_wake: Notify::new(),
        ready_wake: Notify::new(),
        closed: watch::channel(None).0,
        slots: Arc::new(Semaphore::new(MAX_STREAMS)),
        rtt_us: AtomicU64::new(0),
        startup,
        heard: watch::channel(()).0,
        background: AtomicBool::new(false),
        ping_wake: Notify::new(),
        draining: AtomicBool::new(false),
        start: Instant::now(),
        client,
        child: StdMutex::new(None),
    });
    let guard = Arc::new(Guard {
        shared: shared.clone(),
    });
    let (out, accepted) = mpsc::unbounded_channel();
    tokio::spawn(reader(shared.clone(), Arc::downgrade(&guard), r, out));
    tokio::spawn(writer(shared.clone(), w));
    tokio::spawn(pinger(shared));
    Arc::new(MuxConn {
        guard,
        accepted: Mutex::new(accepted),
    })
}

/// The initiating end of a multiplexed connection over the pipe `r` / `w` (the stdio of
/// `ssh host femmed proxy`, or any other byte pipe to a [`mux_server`]). Its streams get odd ids
/// and it pings the peer once a second to measure [`Conn::rtt`] and to notice that it is gone.
///
/// Spawns its tasks on the current Tokio runtime, so call it from inside one. The connection dies
/// when the pipe closes, when [`Conn::close`] is called, when nothing has come from the peer for
/// three round trips (at least 1.5 s) after a ping, or when the last handle to it is dropped
/// (after it has sent what was queued); dropping `w` closes the pipe.
pub fn mux_client<R, W>(r: R, w: W) -> Arc<dyn Conn>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    spawn_mux(r, w, true, DEAD_MIN)
}

/// The accepting end of a multiplexed connection: what `femmed` runs on each unix-socket
/// connection the proxy relays. Its streams get even ids; it pings too, so a client that went away is
/// noticed as fast as a daemon that did. Same lifetime rules as [`mux_client`].
pub fn mux_server<R, W>(r: R, w: W) -> Arc<dyn Conn>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    spawn_mux(r, w, false, DEAD_MIN)
}

/// Run `cmd` with its stdin and stdout piped (stderr inherited) and speak the mux over them. Waits
/// for the far end to answer a ping, so `Ok` means something on the other side really understands
/// the mux (a `ssh` that is still logging in, or a banner on stdout, fails here instead of later).
/// The process is killed when the connection dies or its last handle is dropped.
///
/// This is how [`ssh_connect`] works; use it directly for a custom `ssh` (jump hosts, other
/// binaries). Gives up after 15 s; until the first answer a silent peer is not declared dead sooner
/// (the 1.5 s rule would kill every `ssh` that takes longer than that to log in).
pub async fn command_connect(mut cmd: tokio::process::Command) -> Result<Arc<dyn Conn>, Error> {
    use std::process::Stdio;
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let what = format!("{:?}", cmd.as_std().get_program());
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Connect(format!("cannot run {what}: {e}")))?;
    let (stdin, stdout) = (
        child.stdin.take().expect("stdin is piped"),
        child.stdout.take().expect("stdout is piped"),
    );
    // The process belongs to us until the far end has answered, so a failure can wait for its exit
    // status; after that the connection owns it. Until then silence is not death: `ssh` is logging in.
    let conn = spawn_mux(stdout, stdin, true, READY_TIMEOUT);
    let sh = &conn.guard.shared;
    let mut closed = sh.closed.subscribe();
    let why = tokio::select! {
        () = sh.ready() => {
            *sh.child.lock().unwrap_or_else(PoisonError::into_inner) = Some(child);
            if sh.closed.borrow().is_some() {
                sh.reap_child(); // it died while we were handing the process over
            }
            return Ok(conn);
        }
        () = dead(&mut closed) => sh.dead_err().to_string(),
        () = tokio::time::sleep(READY_TIMEOUT) => format!("no answer within {READY_TIMEOUT:?}"),
    };
    let _ = child.start_kill();
    let status = match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
        Ok(Ok(s)) => format!(" ({what} {s})"),
        _ => String::new(),
    };
    Err(Error::Connect(format!("{what}: {why}{status}")))
}

/// Connect through `ssh -T -o BatchMode=yes -o ServerAliveInterval=5 -o ServerAliveCountMax=3 --
/// <host> <remote_cmd...>` (normally `["femmed", "proxy"]`), with the mux over its stdio; see
/// [`command_connect`] for the rules. SSH has to log in without asking anything (keys, agent).
///
/// Unlike the plan's sketch this returns only the connection: the `ssh` process belongs to it
/// (killed when the connection dies or is dropped), so [`connect_best`](crate::connect_best) can hand out a
/// plain `Arc<dyn Conn>` for either transport.
pub async fn ssh_connect(host: &str, remote_cmd: &[&str]) -> Result<Arc<dyn Conn>, Error> {
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args([
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=3",
        "--",
        host,
    ]);
    cmd.args(remote_cmd);
    command_connect(cmd).await
}

#[async_trait]
impl Conn for MuxConn {
    async fn open(&self, kind: StreamKind, prio: i32) -> Result<(Tx, Rx), Error> {
        let sh = &self.guard.shared;
        // Waits here while the peer still holds 128 streams we opened; fails once the connection is
        // dead. The credit is spent for good: it comes back when the peer says it forgot a stream.
        Arc::clone(&sh.slots)
            .acquire_owned()
            .await
            .map_err(|_| sh.dead_err())?
            .forget();
        let id = {
            let mut g = sh.lock();
            if let Some(e) = sh.op_err() {
                return Err(e);
            }
            match g.open_stream(prio, &kind) {
                Ok(id) => id,
                Err(e) => {
                    sh.slots.add_permits(1); // nothing was opened: give the credit back
                    return Err(e);
                }
            }
        };
        sh.writer_wake.notify_one();
        let tx = Tx::new(
            TxIo::Mux(MuxSend {
                guard: self.guard.clone(),
                id,
            }),
            &kind,
        );
        let rx = Rx::new(RxIo::Mux(MuxRecv {
            guard: self.guard.clone(),
            id,
        }));
        Ok((tx, rx))
    }

    async fn accept(&self) -> Result<Accepted, Error> {
        match self.accepted.lock().await.recv().await {
            Some(a) => Ok(a),
            None => Err(self.guard.shared.dead_err()),
        }
    }

    fn send_datagram(&self, data: Bytes) -> Result<(), Error> {
        let sh = &self.guard.shared;
        if data.len() > MAX_DGRAM {
            return Err(Error::Datagram(format!(
                "{} bytes, the limit is {MAX_DGRAM}",
                data.len()
            )));
        }
        if let Some(e) = sh.op_err() {
            return Err(e);
        }
        sh.lock().queue_dgram(Frame {
            id: 0,
            flags: DGRAM,
            data,
        });
        sh.writer_wake.notify_one();
        Ok(())
    }

    async fn recv_datagram(&self) -> Result<Bytes, Error> {
        let sh = &self.guard.shared;
        let mut closed = sh.closed.subscribe();
        loop {
            if let Some(d) = sh.lock().dgram_in.pop_front() {
                return Ok(d);
            }
            if let Some(e) = sh.op_err() {
                return Err(e);
            }
            // Every waiter watches for the end itself: `fail` can only wake one through the Notify.
            tokio::select! {
                () = sh.dgram_wake.notified() => {}
                () = dead(&mut closed) => {}
            }
        }
    }

    fn rtt(&self) -> Duration {
        Duration::from_micros(self.guard.shared.rtt_us.load(Ordering::Relaxed))
    }

    fn is_quic(&self) -> bool {
        false
    }

    fn close(&self) {
        self.guard
            .shared
            .fail(Error::Closed("closed locally".into()));
    }

    fn set_background(&self, background: bool) {
        let sh = &self.guard.shared;
        sh.background.store(background, Ordering::Relaxed);
        sh.ping_wake.notify_one();
    }

    async fn closed(&self) -> Error {
        let mut rx = self.guard.shared.closed.subscribe();
        rx.wait_for(|c| c.is_some())
            .await
            .ok()
            .and_then(|r| r.clone())
            .unwrap_or_else(|| Error::Closed("connection closed".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    fn parse_all(wire: &[u8]) -> Vec<Frame> {
        let mut buf = BytesMut::from(wire);
        let mut frames = Vec::new();
        while let Some(f) = parse(&mut buf).unwrap() {
            frames.push(f);
        }
        assert!(buf.is_empty(), "trailing bytes");
        frames
    }

    fn drain(inner: &mut Inner) -> Vec<Frame> {
        let mut buf = Vec::new();
        inner.drain_into(&mut buf);
        parse_all(&buf)
    }

    fn queue(inner: &mut Inner, id: u64, tag: u8) {
        let seq = inner.seq;
        inner.seq += 1;
        inner
            .streams
            .get_mut(&id)
            .unwrap()
            .out
            .push_back((seq, Bytes::from(vec![tag; 3])));
    }

    #[test]
    fn writer_order_is_control_then_datagrams_then_priority_then_fifo() {
        let mut inner = Inner::new(true);
        for (id, prio) in [(1, -10), (3, 10), (5, -10)] {
            inner.streams.insert(id, Stream::new(prio, false));
        }
        for tag in 0..4 {
            queue(&mut inner, 1, tag); // bulk, queued first
        }
        queue(&mut inner, 3, 100); // keystroke, queued after it
        queue(&mut inner, 5, 200); // more bulk
        inner.ctl.push_back(Frame::ctl(9, STOP));
        inner.dgram_out.push_back(Frame {
            id: 0,
            flags: DGRAM,
            data: Bytes::from_static(b"d"),
        });

        let first = drain(&mut inner);
        let got: Vec<(u64, u8)> = first.iter().map(|f| (f.id, f.flags)).collect();
        assert_eq!(
            got,
            [(9, STOP), (0, DGRAM), (3, 0)],
            "control, datagram, then the keystroke before any bulk"
        );
        let mut order = Vec::new();
        loop {
            let frames = drain(&mut inner);
            if frames.is_empty() {
                break;
            }
            assert_eq!(frames.len(), 1, "one data chunk per write");
            order.push((frames[0].id, frames[0].data[0]));
        }
        assert_eq!(
            order,
            [(1, 0), (1, 1), (1, 2), (1, 3), (5, 200)],
            "FIFO within equal priority"
        );
    }

    #[test]
    fn fin_marker_is_ordered_after_data_and_reaps_the_stream() {
        let mut inner = Inner::new(false);
        inner.streams.insert(2, Stream::new(0, false));
        queue(&mut inner, 2, 7);
        inner.finish(2);
        inner.finish(2); // idempotent
        assert_eq!(inner.streams[&2].out.len(), 2);
        let data = drain(&mut inner);
        let fin = drain(&mut inner);
        assert_eq!((data[0].flags, fin[0].flags), (0, FIN));
        assert!(inner.streams[&2].fin_sent);
        // Both handles gone and the peer already finished: now it is forgotten.
        let s = inner.streams.get_mut(&2).unwrap();
        s.tx_alive = false;
        s.rx_alive = false;
        s.recv = RecvState::Fin;
        inner.reap(2);
        assert!(inner.streams.is_empty());
    }

    #[test]
    fn parse_rejects_oversized_and_overlong_headers() {
        let mut wire = Vec::new();
        put_varint(&mut wire, 1);
        wire.push(0);
        put_varint(&mut wire, CHUNK as u64 + 1);
        assert!(matches!(
            parse(&mut BytesMut::from(&wire[..])),
            Err(Error::Proto(_))
        ));
        // Eleven continuation bytes never end a varint.
        assert!(parse(&mut BytesMut::from(&[0xff; 11][..])).is_err());
    }

    #[test]
    fn a_parsed_frame_does_not_pin_the_read_block() {
        let mut wire = Vec::new();
        encode(&mut wire, 1, 0, b"hello");
        let mut buf = BytesMut::from(&wire[..]);
        let start = buf.as_ptr() as usize;
        let block = start..start + buf.len();
        let f = parse(&mut buf).unwrap().unwrap();
        assert_eq!(&f.data[..], b"hello");
        assert!(
            !block.contains(&(f.data.as_ptr() as usize)),
            "the payload is a slice of the read block"
        );
    }

    #[test]
    fn open_puts_the_kind_right_behind_open_ahead_of_datagrams_and_data() {
        let mut inner = Inner::new(true);
        let bulk = inner.open_stream(prio::BULK, &StreamKind::Bulk(1)).unwrap();
        assert_eq!(bulk, 1);
        drain(&mut inner); // its OPEN and kind go out
        queue(&mut inner, bulk, 9); // bulk data already waiting
        inner.dgram_out.push_back(Frame {
            id: 0,
            flags: DGRAM,
            data: Bytes::from_static(b"d"),
        });
        let kind = StreamKind::Term(femme_proto::PaneId {
            sid: femme_proto::Id::new(),
            no: 4,
        });
        assert_eq!(inner.open_stream(prio::TERM, &kind).unwrap(), 3);
        let got = drain(&mut inner);
        let shape: Vec<(u64, u8)> = got.iter().map(|f| (f.id, f.flags)).collect();
        assert_eq!(
            shape,
            [(3, OPEN), (3, 0), (0, DGRAM), (1, 0)],
            "OPEN and the kind first, then datagrams, then data"
        );
        let mut d = femme_proto::codec::Deframer::new();
        d.push(&got[1].data);
        assert_eq!(d.next_msg::<StreamKind>().unwrap(), Some(kind));
        assert_eq!(
            inner.streams[&3].credit,
            INIT_WINDOW - got[1].data.len(),
            "the kind frame spent some of the stream's credit"
        );
    }

    #[test]
    fn forgetting_a_stream_the_peer_opened_hands_its_credit_back_and_one_of_ours_does_not() {
        let mut inner = Inner::new(false);
        inner.peer_open = 1;
        inner.streams.insert(1, Stream::new(0, true));
        inner.streams.insert(2, Stream::new(0, false));
        for s in inner.streams.values_mut() {
            s.tx_alive = false;
            s.rx_alive = false;
            s.fin_sent = true;
            s.recv = RecvState::Fin;
        }
        assert!(inner.reap(1), "peer-opened: the credit goes back");
        assert!(
            !inner.reap(2),
            "ours: the peer returns the credit when it forgets the stream"
        );
        assert!(!inner.reap(1), "forgetting twice does nothing");
        assert!(inner.streams.is_empty());
        assert_eq!(inner.peer_open, 0);
        let sent = drain(&mut inner);
        assert_eq!(sent.len(), 1);
        assert_eq!(
            (sent[0].id, sent[0].flags, &sent[0].data[..]),
            (0, WINDOW, &[1u8][..])
        );
    }

    /// A mux whose peer is silent, with its internals in reach. Single-threaded test runtime: the
    /// tasks it spawns do not run until the test awaits.
    fn bare_mux() -> (Arc<MuxConn>, tokio::io::DuplexStream) {
        let (a, b) = tokio::io::duplex(64);
        let (ar, aw) = tokio::io::split(a);
        (spawn_mux(ar, aw, false, DEAD_MIN), b)
    }

    #[tokio::test]
    async fn a_control_queue_past_its_backstop_cuts_the_connection() {
        let (conn, _peer) = bare_mux();
        let sh = &conn.guard.shared;
        {
            // A stream the peer opened and that its FIN finishes; forgetting it queues one more frame.
            let mut g = sh.lock();
            g.peer_open = 1;
            let mut s = Stream::new(0, true);
            s.tx_alive = false;
            s.rx_alive = false;
            s.fin_sent = true;
            g.streams.insert(1, s);
            for i in 0..MAX_CTL as u64 {
                g.ctl.push_back(Frame::ctl(1000 + 2 * i, STOP));
            }
        }
        let (out, _accepted) = mpsc::unbounded_channel();
        let e = sh
            .on_frame(
                Frame {
                    id: 1,
                    flags: FIN,
                    data: Bytes::new(),
                },
                &Arc::downgrade(&conn.guard),
                &out,
            )
            .unwrap_err();
        assert!(
            matches!(&e, Error::Proto(m) if m.contains("control")),
            "got {e:?}"
        );
    }

    proptest! {
        #[test]
        fn frames_survive_any_chunking(
            frames in prop::collection::vec(
                (0u64..2_000_000, prop::sample::select(vec![0u8, OPEN, FIN, RST, DGRAM, WINDOW, STOP, PING, PONG]), prop::collection::vec(any::<u8>(), 0..400)),
                0..40,
            ),
            cuts in prop::collection::vec(1usize..97, 1..50),
        ) {
            let mut wire = Vec::new();
            for (id, flags, data) in &frames {
                encode(&mut wire, *id, *flags, data);
            }
            let (mut buf, mut got, mut at, mut c) = (BytesMut::new(), Vec::new(), 0, 0);
            while at < wire.len() {
                let n = cuts[c % cuts.len()].min(wire.len() - at);
                c += 1;
                buf.extend_from_slice(&wire[at..at + n]);
                at += n;
                while let Some(f) = parse(&mut buf).unwrap() {
                    got.push(f);
                }
            }
            prop_assert!(buf.is_empty());
            let want: Vec<Frame> = frames.into_iter().map(|(id, flags, data)| Frame { id, flags, data: data.into() }).collect();
            prop_assert_eq!(got, want);
        }

        #[test]
        fn parse_never_panics_on_garbage(bytes in prop::collection::vec(any::<u8>(), 0..2000)) {
            let mut buf = BytesMut::from(&bytes[..]);
            while let Ok(Some(_)) = parse(&mut buf) {}
        }
    }

    /// Writer half that records everything it passes on, so a test can see the wire order.
    struct Tap<W> {
        inner: W,
        log: Arc<StdMutex<Vec<u8>>>,
    }

    impl<W: AsyncWrite + Unpin> AsyncWrite for Tap<W> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let p = Pin::new(&mut self.inner).poll_write(cx, buf);
            if let Poll::Ready(Ok(n)) = &p {
                self.log.lock().unwrap().extend_from_slice(&buf[..*n]);
            }
            p
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    fn fill_random(buf: &mut [u8], state: &mut u64) {
        for b in buf {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *b = (*state >> 24) as u8;
        }
    }

    /// 16 bulk streams saturate a 4 KiB pipe while `n` keystrokes, one at a time, go through a
    /// stream of priority `key_prio`. Returns, per keystroke, the wire bytes written between
    /// handing it to `send_bytes` and its frame reaching the wire.
    async fn keystroke_gaps(key_prio: i32, n: usize) -> Vec<usize> {
        let (a, b) = tokio::io::duplex(4096);
        let log = Arc::new(StdMutex::new(Vec::new()));
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let client = mux_client(
            ar,
            Tap {
                inner: aw,
                log: log.clone(),
            },
        );
        let server = mux_server(br, bw);

        // The server drains bulk and acknowledges every keystroke with one byte.
        tokio::spawn(async move {
            while let Ok((kind, mut tx, mut rx)) = server.accept().await {
                let key = matches!(kind, StreamKind::Term(_));
                tokio::spawn(async move {
                    while let Ok(Some(_)) = rx.recv().await {
                        if key && tx.send_bytes(&[0]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        for i in 0..16 {
            let (mut tx, rx) = client.open(StreamKind::Bulk(i), prio::BULK).await.unwrap();
            tokio::spawn(async move {
                let _rx = rx;
                let (mut state, mut chunk) = (0x9e37_79b9_7f4a_7c15 ^ i, vec![0u8; 16_000]);
                loop {
                    // Random, so the zstd pass inside `send_bytes` cannot shrink it to nothing.
                    fill_random(&mut chunk, &mut state);
                    if tx.send_bytes(&chunk).await.is_err() {
                        break;
                    }
                }
            });
        }
        // Let the pipe fill with bulk.
        while log.lock().unwrap().len() < 256 << 10 {
            tokio::task::yield_now().await;
        }

        let (mut ktx, mut krx) = client
            .open(
                StreamKind::Term(femme_proto::PaneId {
                    sid: femme_proto::Id::new(),
                    no: 0,
                }),
                key_prio,
            )
            .await
            .unwrap();
        let term_id = 33; // the 16 bulk streams took the odd ids 1..=31
        let mut before = Vec::new();
        for i in 0..n {
            before.push(log.lock().unwrap().len());
            ktx.send_bytes(&[i as u8; 20]).await.unwrap();
            krx.recv().await.unwrap().expect("acknowledgement");
        }
        client.close();

        let wire = log.lock().unwrap().clone();
        let mut buf = BytesMut::from(&wire[..]);
        let (mut at, mut offset) = (Vec::new(), 0usize);
        loop {
            let left = buf.len();
            let Some(f) = parse(&mut buf).unwrap() else {
                break;
            };
            if f.id == term_id && f.flags == 0 {
                at.push(offset);
            }
            offset += left - buf.len();
        }
        // The first data frame on the stream is its kind frame; keystroke i is the one after it.
        assert_eq!(at.len(), n + 1, "kind frame plus {n} keystrokes");
        before
            .iter()
            .zip(&at[1..])
            .map(|(b, a)| a.saturating_sub(*b))
            .collect()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_keystroke_waits_behind_at_most_one_chunk_but_only_with_priority() {
        let with = keystroke_gaps(prio::TERM, 100).await;
        let max = *with.iter().max().unwrap();
        println!(
            "keystroke at prio {}: max {} wire bytes behind it (limit one 16 KiB chunk)",
            prio::TERM,
            max
        );
        assert!(max <= CHUNK + 128, "a keystroke waited behind {max} bytes");

        // Control experiment: at the bulk priority the same measurement sees the whole backlog,
        // so the number above really does come from the scheduler.
        let without = keystroke_gaps(prio::BULK, 20).await;
        let max_without = *without.iter().max().unwrap();
        println!(
            "keystroke at prio {}: max {} wire bytes behind it",
            prio::BULK,
            max_without
        );
        assert!(
            max_without > 4 * CHUNK,
            "no head-of-line blocking seen at equal priority ({max_without})"
        );
    }
}
