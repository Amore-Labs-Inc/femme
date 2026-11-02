//! FEMME transport: one connection abstraction, two implementations.
//!
//! [`Conn`] is a multiplexed, prioritised connection between the desktop and `femmed`. It is
//! implemented by QUIC ([`quic`]: quinn, mutual TLS pinned by fingerprints) and by an SSH-tunnelled
//! multiplexer ([`mux`]: the same framed streams over any byte pipe, normally `ssh -T host femmed
//! proxy`). Callers never learn which one they got; [`Conn::is_quic`] exists for the status bar.
//!
//! # Streams
//!
//! Every stream starts with one frame carrying its [`StreamKind`], written by whoever opens it, so
//! the peer's [`Conn::accept`] can route it. Everything after that is frames in the shared codec
//! (`femme_proto::codec`: `varint len | u8 flags | postcard`). A stream is a pair [`Tx`] / [`Rx`]:
//! send and receive whole messages with [`Tx::send`] / [`Rx::recv_msg`], or raw payloads with
//! [`Tx::send_bytes`] / [`Rx::recv`]. Compression (zstd) is on for `Ctrl`, `Events` and `Bulk`
//! streams and off for `Term` streams; datagrams are never framed or compressed.
//!
//! Priorities ([`prio`]) keep keystrokes ahead of bulk transfers: QUIC maps them to
//! `SendStream::set_priority`; the mux has a strict-priority writer with 16 KiB chunks, so a
//! keystroke never waits behind more than one chunk (plus whatever the pipe itself buffers).
//!
//! # Rules for callers
//!
//! - [`Tx::send`] / [`Tx::send_bytes`] are **not cancel-safe**: a dropped send may have written half a
//!   frame. Do not put them in a `select!` branch that can lose; if one is dropped, drop the stream.
//! - [`Rx::recv`], [`Rx::recv_msg`], [`Conn::accept`], [`Conn::recv_datagram`] and [`Conn::closed`] are
//!   cancel-safe: use them freely in `select!` loops.
//! - Dropping a [`Tx`] without calling [`Tx::finish`] or [`Tx::reset`] finishes the stream.
//! - Dropping the last handle (the `Arc<dyn Conn>` and every stream) closes the connection. The mux
//!   first flushes what is queued (a last message and the end of its stream still arrive; it gives
//!   up on a stuck pipe after a second); QUIC closes at once, as quinn does, and the peer may lose
//!   what had not been acknowledged. [`Conn::close`] is abrupt on both.
//! - Reconnecting is the caller's job: there is no session layer. A dead connection reports itself
//!   through [`Conn::closed`] and through [`Error::Closed`] on every stream; build a new one, send
//!   `Hello` with your cursors, and carry on (plan §6).

#![warn(missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use femme_proto::codec::{Deframer, Framer};
use serde::de::DeserializeOwned;
use serde::Serialize;

pub mod mux;
pub mod quic;
pub mod race;

pub use femme_proto::rpc::StreamKind;
pub use mux::{command_connect, ssh_connect};
pub use mux::{mux_client, mux_server};
pub use quic::{connect, DeviceIdentity, QuicClient};
pub use quic::{listen, Listener, ServerIdentity};
pub use race::{connect_best, Best};

/// Stream priorities (higher goes first). Keystrokes beat everything; bulk yields to everything.
pub mod prio {
    use super::StreamKind;

    /// Control RPC stream.
    pub const CTRL: i32 = 5;
    /// State-push stream.
    pub const EVENTS: i32 = 3;
    /// A terminal pane's stream: keys up, frames down.
    pub const TERM: i32 = 10;
    /// Bulk transfers.
    // ponytail: priority is all that keeps bulk off the keystrokes' back. Plan rule 3 (pace bulk to
    // 50 % of the estimated bandwidth for 2 s after terminal input) is M5; build it where `open`
    // applies this value once p2_latency shows the tail needs it.
    pub const BULK: i32 = -10;

    /// The priority a stream of `kind` should have. [`Conn::open`](crate::Conn::open) takes the
    /// priority explicitly; the accepting side applies this one to its own sending direction.
    pub fn of(kind: &StreamKind) -> i32 {
        match kind {
            StreamKind::Ctrl => CTRL,
            StreamKind::Events => EVENTS,
            StreamKind::Term(_) => TERM,
            StreamKind::Bulk(_) => BULK,
        }
    }
}

/// Largest datagram [`Conn::send_datagram`] accepts, on either transport. QUIC's minimum path MTU
/// leaves about 1170 bytes for a datagram, so this is what is safe everywhere.
pub const MAX_DATAGRAM: usize = 1000;

/// Everything the transport can fail with. `Clone` so one failure can be handed to every waiter.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// The connection is gone: closed by either side, idle timeout, the peer went silent, `ssh`
    /// exited. Build a new one.
    #[error("connection closed: {0}")]
    Closed(String),
    /// The peer aborted the stream: it reset its sending direction (seen by [`Rx::recv`]; QUIC
    /// `RESET_STREAM`, mux `RST`) or stopped reading ours (seen by [`Tx::send`]; QUIC `STOP_SENDING`,
    /// mux `STOP`, i.e. it dropped its [`Rx`]).
    #[error("stream reset by the peer")]
    Reset,
    /// The TLS handshake failed or the peer refused us: wrong server fingerprint, device not
    /// enrolled or revoked. Retrying will not help; tell the user.
    #[error("handshake failed: {0}")]
    Handshake(String),
    /// Malformed or unexpected data from the peer (bad frame, bad postcard, unknown stream kind).
    #[error("protocol error: {0}")]
    Proto(String),
    /// A datagram could not be sent (too large, or the peer has datagrams off).
    #[error("datagram: {0}")]
    Datagram(String),
    /// Local I/O: binding a socket, spawning `ssh`, reading or writing identity files.
    #[error("io: {0}")]
    Io(Arc<std::io::Error>),
    /// No transport could be established.
    #[error("could not connect: {0}")]
    Connect(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(Arc::new(e))
    }
}

impl From<femme_proto::codec::Error> for Error {
    fn from(e: femme_proto::codec::Error) -> Self {
        Error::Proto(e.to_string())
    }
}

/// A multiplexed, prioritised connection. QUIC and the SSH mux both implement it; `femmed` and the
/// desktop never know which they got. See the crate docs for the rules on cancel-safety and drops.
#[async_trait]
pub trait Conn: Send + Sync + 'static {
    /// Open a bidirectional stream. Writes `kind` as the first frame so the peer's
    /// [`accept`](Conn::accept) can route it. `prio`: higher wins ([`prio`] has the standard values;
    /// [`open_kind`](Conn::open_kind) picks the standard one). Waits while the peer still holds 128
    /// streams this side opened; a stream counts until the peer lets go of it.
    async fn open(&self, kind: StreamKind, prio: i32) -> Result<(Tx, Rx), Error>;

    /// [`open`](Conn::open) with the standard priority for `kind` ([`prio::of`]).
    async fn open_kind(&self, kind: StreamKind) -> Result<(Tx, Rx), Error> {
        self.open(kind, prio::of(&kind)).await
    }

    /// The next stream the peer opened, with its kind already read. The returned [`Tx`] sends at
    /// the standard priority for that kind. Streams whose kind frame does not arrive within 10 s
    /// (or is not a valid kind) are reset and never returned. Streams are not guaranteed to come out
    /// in the order the peer opened them. Cancel-safe.
    async fn accept(&self) -> Result<(StreamKind, Tx, Rx), Error>;

    /// Send an unreliable, unordered message of at most [`MAX_DATAGRAM`] bytes (QUIC DATAGRAM; the
    /// mux carries it in a DGRAM frame). May be dropped, duplicated by the network, or reordered.
    /// Never blocks: when the send queue is full the oldest datagram is discarded.
    fn send_datagram(&self, data: Bytes) -> Result<(), Error>;

    /// The next datagram the peer sent. Cancel-safe.
    async fn recv_datagram(&self) -> Result<Bytes, Error>;

    /// Like [`send_datagram`](Conn::send_datagram), twice: now, and again after `gap`. For the
    /// small datagrams that carry keystrokes and their echoes (plan rule 3) on a path with 1-2 %
    /// loss, where a lost packet costs the `Term` stream a loss-recovery round. The copies are
    /// spaced because QUIC packs datagrams queued together into one packet, so back-to-back copies
    /// are lost together. `p2_latency` at 80 ms RTT, 1 % loss each way, keystroke p99: 219 ms with
    /// the stream alone, 211 ms with back-to-back copies, 91.3 ms with copies 10 ms apart. Both
    /// sends are best effort and the receiver must tolerate the duplicate (`applied_seq` /
    /// `echo_seq` do). Spawns a task for the second copy, so call it from inside the runtime.
    fn send_datagram_dup(self: Arc<Self>, data: Bytes, gap: Duration) {
        let _ = self.send_datagram(data.clone());
        tokio::spawn(async move {
            tokio::time::sleep(gap).await;
            let _ = self.send_datagram(data);
        });
    }

    /// Smoothed round-trip time. QUIC: the transport's own estimate. Mux: EWMA of the ping/pong
    /// each end sends every second, measured through the pipe (so it includes whatever the pipe and
    /// `ssh` have buffered); zero until the first pong.
    fn rtt(&self) -> Duration;

    /// `true` for QUIC, `false` for the SSH mux.
    fn is_quic(&self) -> bool;

    /// Close the connection now. Pending and later operations fail with [`Error::Closed`].
    /// Idempotent. Abrupt: data queued but not yet written is discarded (finish the streams and
    /// wait for what you need to hear, or just drop every handle, which on the mux flushes first).
    /// Telling the peer is best effort: QUIC withholds its CONNECTION_CLOSE while the congestion
    /// window is full of unacknowledged data (right after a big transfer), and then the peer finds
    /// out only from its next send (a stateless reset) or the 30 s idle timeout.
    fn close(&self);

    /// Move the connection to a fresh network path after a wake from sleep or a network change
    /// (QUIC: a new UDP socket, so the streams survive without a reconnect). Does nothing on the
    /// mux, whose pipe is `ssh`'s business, and `Ok` there.
    fn rebind(&self) -> Result<(), Error> {
        Ok(())
    }

    /// Say whether the app is out of sight. The mux then pings every 15 s instead of every second
    /// (plan rule 7), and pings at once when it comes back, which also finds out whether the link
    /// survived. QUIC fixes its keepalive when the connection is made, so this changes nothing there:
    /// connect with [`KEEPALIVE_BACKGROUND`](quic::KEEPALIVE_BACKGROUND) for a connection made in
    /// the background.
    fn set_background(&self, _background: bool) {}

    /// Resolves when the connection is dead (closed by either side, `ssh` exited, or the peer
    /// went silent: three round trips, at least 1.5 s, after a keepalive ping with nothing coming
    /// back; the QUIC server end, which sends no pings, has the 30 s idle timeout), with the reason.
    /// Resolves immediately if it already is. Cancel-safe.
    async fn closed(&self) -> Error;
}

/// Sending half of a stream: framed messages, written in order.
///
/// Dropping it without [`finish`](Tx::finish) or [`reset`](Tx::reset) finishes the stream.
pub struct Tx {
    framer: Framer,
    out: Vec<u8>,
    io: TxIo,
}

pub(crate) enum TxIo {
    Quic(quinn::SendStream),
    Mux(mux::MuxSend),
}

/// Whether frames on a stream of this kind may be zstd-compressed: keystroke streams never are
/// (tiny frames, compression only costs).
pub(crate) fn compress_for(kind: &StreamKind) -> bool {
    !matches!(kind, StreamKind::Term(_))
}

impl Tx {
    pub(crate) fn new(io: TxIo, kind: &StreamKind) -> Tx {
        Tx {
            framer: Framer::new(compress_for(kind)),
            out: Vec::new(),
            io,
        }
    }

    /// Serialize `msg` with postcard, frame it (compressed when the stream allows and it pays off)
    /// and write it. Not cancel-safe. Returns once the frame is handed to the transport, not once
    /// the peer has it.
    pub async fn send<T: Serialize + ?Sized>(&mut self, msg: &T) -> Result<(), Error> {
        self.out.clear();
        self.framer.frame(msg, &mut self.out)?;
        self.write_out().await
    }

    /// Like [`send`](Tx::send) for a payload that is already encoded. Not cancel-safe.
    pub async fn send_bytes(&mut self, payload: &[u8]) -> Result<(), Error> {
        self.out.clear();
        self.framer.frame_bytes(payload, &mut self.out)?;
        self.write_out().await
    }

    async fn write_out(&mut self) -> Result<(), Error> {
        match &mut self.io {
            TxIo::Quic(s) => s.write_all(&self.out).await.map_err(quic::write_err),
            TxIo::Mux(m) => m.write(&self.out).await,
        }
    }

    /// Say that nothing more will be sent: the peer's [`Rx::recv`] yields `None` once it has read
    /// everything. Does not wait for the peer. Calling it twice is fine.
    pub async fn finish(&mut self) -> Result<(), Error> {
        match &mut self.io {
            TxIo::Quic(s) => {
                // A stream that is already finished or reset is not an error here.
                let _ = s.finish();
                Ok(())
            }
            TxIo::Mux(m) => m.finish(),
        }
    }

    /// Abort this direction: queued data is discarded and the peer's [`Rx::recv`] fails with
    /// [`Error::Reset`]. The receiving direction is unaffected.
    pub fn reset(&mut self) {
        match &mut self.io {
            TxIo::Quic(s) => {
                let _ = s.reset(quinn::VarInt::from_u32(0));
            }
            TxIo::Mux(m) => m.reset(),
        }
    }

    /// Change this stream's priority (higher goes first). Takes effect for data not yet sent.
    pub fn set_priority(&mut self, prio: i32) {
        match &mut self.io {
            TxIo::Quic(s) => {
                let _ = s.set_priority(prio);
            }
            TxIo::Mux(m) => m.set_priority(prio),
        }
    }
}

/// Receiving half of a stream: frames, in order.
///
/// Dropping it early tells the peer to stop sending (QUIC `STOP_SENDING`, mux `STOP`): its next
/// [`Tx::send`] fails with [`Error::Reset`].
pub struct Rx {
    de: Deframer,
    io: RxIo,
}

pub(crate) enum RxIo {
    Quic(quinn::RecvStream),
    Mux(mux::MuxRecv),
}

impl RxIo {
    /// Move the next available stream bytes into `de`. `false` means the stream finished cleanly.
    /// Cancel-safe: `de` is only touched after the wait is over.
    async fn fill(&mut self, de: &mut Deframer) -> Result<bool, Error> {
        match self {
            RxIo::Quic(r) => {
                let mut bufs: [Bytes; 16] = Default::default();
                match r.read_chunks(&mut bufs).await.map_err(quic::read_err)? {
                    Some(n) => {
                        for b in &bufs[..n] {
                            de.push(b);
                        }
                        Ok(true)
                    }
                    None => Ok(false),
                }
            }
            RxIo::Mux(m) => match m.read().await? {
                Some(b) => {
                    de.push(&b);
                    Ok(true)
                }
                None => Ok(false),
            },
        }
    }
}

impl Rx {
    pub(crate) fn new(io: RxIo) -> Rx {
        Rx {
            de: Deframer::new(),
            io,
        }
    }

    /// The next frame's payload (decompressed), or `None` when the peer finished the stream.
    /// Cancel-safe.
    pub async fn recv(&mut self) -> Result<Option<Bytes>, Error> {
        loop {
            if let Some(b) = self.de.next()? {
                return Ok(Some(b));
            }
            if !self.io.fill(&mut self.de).await? {
                return if self.de.is_empty() {
                    Ok(None)
                } else {
                    Err(Error::Proto("stream ended inside a frame".into()))
                };
            }
        }
    }

    /// [`recv`](Rx::recv) and decode the payload as a postcard message. Cancel-safe.
    pub async fn recv_msg<T: DeserializeOwned>(&mut self) -> Result<Option<T>, Error> {
        match self.recv().await? {
            Some(b) => Ok(Some(femme_proto::codec::decode(&b)?)),
            None => Ok(None),
        }
    }
}

/// The most a stream's kind frame may take: it is a few bytes, and a peer that claims more must not
/// be able to make us wait for it or buffer it.
const KIND_MAX: usize = 256;

/// Read the kind frame that every stream starts with.
pub(crate) async fn read_kind(rx: &mut Rx) -> Result<StreamKind, Error> {
    rx.de.set_max_frame(KIND_MAX);
    let kind = rx
        .recv_msg::<StreamKind>()
        .await?
        .ok_or_else(|| Error::Proto("stream ended before its kind frame".into()))?;
    rx.de.set_max_frame(femme_proto::codec::MAX_FRAME);
    Ok(kind)
}

/// How long an accepted stream may take to deliver its kind frame.
pub(crate) const KIND_TIMEOUT: Duration = Duration::from_secs(10);

/// Lowercase hex of a SHA-256 fingerprint (what `femmed enroll` prints and the desktop stores).
pub fn fp_to_hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse a fingerprint printed by [`fp_to_hex`] (64 hex digits, case-insensitive, optional `:`
/// separators as `openssl` prints them).
pub fn fp_from_hex(s: &str) -> Option<[u8; 32]> {
    let digits: Vec<u8> = s.bytes().filter(|b| *b != b':').collect();
    // `from_str_radix` below would also take a leading '+'.
    if digits.len() != 64 || !digits.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in digits.chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let mut fp = [0u8; 32];
        for (i, b) in fp.iter_mut().enumerate() {
            *b = (i * 8 + 3) as u8;
        }
        let s = fp_to_hex(&fp);
        assert_eq!(s.len(), 64);
        assert_eq!(fp_from_hex(&s), Some(fp));
        assert_eq!(fp_from_hex(&s.to_uppercase()), Some(fp));
        let colons: String = s
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(fp_from_hex(&colons), Some(fp));
        assert_eq!(fp_from_hex("zz"), None);
        assert_eq!(fp_from_hex(&s[..62]), None);
    }

    #[test]
    fn a_sign_is_not_a_hex_digit() {
        // `u8::from_str_radix` takes a leading '+', so this used to parse as 0x01.
        let s = fp_to_hex(&[0x11; 32]);
        assert_eq!(fp_from_hex(&format!("+1{}", &s[2..])), None);
        assert_eq!(fp_from_hex(&format!("-1{}", &s[2..])), None);
    }
}
