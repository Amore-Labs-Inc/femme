//! The phone's carrier: the same streams as QUIC and the mux, over one WebSocket, as JSON. Where
//! UDP is dropped (hotel wifi, mobile carriers) and a TLS client certificate cannot pass through the
//! HTTPS front, a WebSocket behind it goes everywhere a browser does; and JSON is what a phone
//! decodes in one line, where postcard would be a codec of its own to keep in step.
//!
//! One text message per carrier message, a JSON object:
//!
//! - `{"s":N,"open":KIND}` opens stream `N` (`KIND` is a [`StreamKind`] as serde_json writes it).
//!   The phone's ids are odd, the daemon's even.
//! - `{"s":N,"m":MSG}` carries one message of the stream, the type the stream kind says
//!   (`C2S`/`S2C` on `Ctrl`, `TermUp`/`TermDown` on `Term`, ...), as serde_json writes it. Ids are
//!   ULID text, bytes are arrays, bit flags are `"A | B"`.
//! - `{"s":N,"fin":true}` finishes the sender's direction, `{"s":N,"rst":true}` resets it.
//!
//! This module knows no sockets: [`WsConn::start`] takes the text going out and coming in as
//! channels, and whoever holds the socket (axum in `femmed`) pumps them. Datagrams do not exist here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use femme_proto::rpc::StreamKind;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex};

use crate::{Conn, Error, Rx, RxIo, Tx, TxIo};

/// How many messages a stream may have waiting for its reader before the carrier stops reading
/// the socket. ponytail: every stream shares the socket, so one slow reader holds them all; the
/// holder's own `Ack` window keeps terminals in check, and the ctrl reader never falls behind.
const STREAM_QUEUE: usize = 256;

#[derive(Serialize, Deserialize)]
struct Envelope<'a> {
    s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    open: Option<StreamKind>,
    #[serde(default, borrow, skip_serializing_if = "Option::is_none")]
    m: Option<&'a RawValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fin: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rst: Option<bool>,
}

/// What a stream's reader gets from the carrier.
enum In {
    Msg(Bytes),
    Fin,
    Rst,
}

type Accepted = (StreamKind, Tx, Rx);

/// A [`Conn`] over one WebSocket.
pub struct WsConn {
    out: mpsc::Sender<String>,
    /// The carrier's end of every open stream's inbound queue, by stream id.
    streams: Mutex<HashMap<u64, mpsc::Sender<In>>>,
    accept_tx: Mutex<Option<mpsc::Sender<Accepted>>>,
    accepted: AsyncMutex<mpsc::Receiver<Accepted>>,
    /// The id this side opens its next stream with (even).
    next: AtomicU64,
    /// `Some` once the connection is dead, with why.
    dead: watch::Sender<Option<Error>>,
}

impl WsConn {
    /// A connection whose socket writes what comes out of `out`'s receiver and feeds what it reads
    /// into `inbound`'s sender; the connection is dead once `inbound` closes.
    pub fn start(out: mpsc::Sender<String>, mut inbound: mpsc::Receiver<String>) -> Arc<WsConn> {
        let (accept_tx, accepted) = mpsc::channel(64);
        let conn = Arc::new(WsConn {
            out,
            streams: Mutex::default(),
            accept_tx: Mutex::new(Some(accept_tx)),
            accepted: AsyncMutex::new(accepted),
            next: AtomicU64::new(2),
            dead: watch::Sender::new(None),
        });
        // Weak: the router ends with the connection's last handle, not the other way round.
        let router = Arc::downgrade(&conn);
        tokio::spawn(async move {
            while let Some(text) = inbound.recv().await {
                let Some(conn) = router.upgrade() else { return };
                if let Err(why) = conn.route(&text).await {
                    conn.die(Error::Proto(why));
                    return;
                }
            }
            if let Some(conn) = router.upgrade() {
                conn.die(Error::Closed("socket closed".into()));
            }
        });
        conn
    }

    /// A new stream `id` of `kind`: its queue in the map, its halves for whoever reads and writes it.
    fn stream(&self, id: u64, kind: &StreamKind) -> Result<(Tx, Rx), String> {
        let (in_tx, in_rx) = mpsc::channel(STREAM_QUEUE);
        let mut streams = self.streams.lock().unwrap_or_else(PoisonError::into_inner);
        if streams.contains_key(&id) {
            return Err(format!("stream {id} is already open"));
        }
        streams.insert(id, in_tx);
        let tx = Tx::new(
            TxIo::Ws(WsSend {
                id,
                out: self.out.clone(),
            }),
            kind,
        );
        let rx = Rx::new(RxIo::Ws(WsRecv { rx: in_rx }));
        Ok((tx, rx))
    }

    /// One message off the socket, to its stream.
    async fn route(&self, text: &str) -> Result<(), String> {
        let env: Envelope = serde_json::from_str(text).map_err(|e| format!("bad envelope: {e}"))?;
        if let Some(kind) = env.open {
            let (tx, rx) = self.stream(env.s, &kind)?;
            let acceptor = self
                .accept_tx
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(acceptor) = acceptor {
                // Nobody accepting any more means the connection is on its way out.
                let _ = acceptor.send((kind, tx, rx)).await;
            }
            return Ok(());
        }
        let queue = self
            .streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&env.s)
            .cloned();
        // A stream the reader let go of (or never opened): dropped on the floor, as QUIC does
        // after STOP_SENDING.
        let Some(queue) = queue else { return Ok(()) };
        if let Some(m) = env.m {
            // Waits while the stream's reader is behind: the socket is read no further meanwhile.
            if queue
                .send(In::Msg(Bytes::from(m.get().to_owned())))
                .await
                .is_err()
            {
                self.forget(env.s);
            }
        }
        if env.fin == Some(true) {
            let _ = queue.send(In::Fin).await;
            self.forget(env.s);
        }
        if env.rst == Some(true) {
            let _ = queue.send(In::Rst).await;
            self.forget(env.s);
        }
        Ok(())
    }

    fn forget(&self, id: u64) {
        self.streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }

    /// Ends everything: readers see their streams end, `accept` fails, `closed` resolves. The first
    /// reason is the one kept.
    fn die(&self, why: Error) {
        self.dead.send_if_modified(|d| {
            if d.is_some() {
                return false;
            }
            *d = Some(why);
            true
        });
        self.streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.accept_tx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    fn dead_err(&self) -> Option<Error> {
        self.dead.borrow().clone()
    }
}

#[async_trait]
impl Conn for WsConn {
    async fn open(&self, kind: StreamKind, _prio: i32) -> Result<(Tx, Rx), Error> {
        if let Some(e) = self.dead_err() {
            return Err(e);
        }
        let id = self.next.fetch_add(2, Ordering::Relaxed);
        let (tx, rx) = self.stream(id, &kind).map_err(Error::Proto)?;
        let kind = serde_json::to_string(&kind).map_err(|e| Error::Proto(e.to_string()))?;
        self.out
            .send(format!("{{\"s\":{id},\"open\":{kind}}}"))
            .await
            .map_err(|_| Error::Closed("socket writer gone".into()))?;
        Ok((tx, rx))
    }

    async fn accept(&self) -> Result<Accepted, Error> {
        match self.accepted.lock().await.recv().await {
            Some(a) => Ok(a),
            None => Err(self
                .dead_err()
                .unwrap_or_else(|| Error::Closed("closed".into()))),
        }
    }

    fn send_datagram(&self, _data: Bytes) -> Result<(), Error> {
        Err(Error::Datagram("a WebSocket carries no datagrams".into()))
    }

    async fn recv_datagram(&self) -> Result<Bytes, Error> {
        Err(self.closed().await)
    }

    fn rtt(&self) -> Duration {
        Duration::ZERO
    }

    fn is_quic(&self) -> bool {
        false
    }

    fn close(&self) {
        self.die(Error::Closed("closed".into()));
    }

    async fn closed(&self) -> Error {
        let mut dead = self.dead.subscribe();
        loop {
            let why = dead.borrow_and_update().clone();
            if let Some(why) = why {
                return why;
            }
            if dead.changed().await.is_err() {
                return Error::Closed("gone".into());
            }
        }
    }
}

/// The sending half of one stream: each write is one carrier message.
pub struct WsSend {
    id: u64,
    out: mpsc::Sender<String>,
}

impl WsSend {
    pub(crate) async fn write(&mut self, json: &[u8]) -> Result<(), Error> {
        let m = std::str::from_utf8(json).map_err(|e| Error::Proto(format!("not utf-8: {e}")))?;
        self.send(format!("{{\"s\":{},\"m\":{m}}}", self.id)).await
    }

    pub(crate) async fn finish(&mut self) -> Result<(), Error> {
        self.send(format!("{{\"s\":{},\"fin\":true}}", self.id))
            .await
    }

    pub(crate) fn reset(&mut self) {
        let _ = self
            .out
            .try_send(format!("{{\"s\":{},\"rst\":true}}", self.id));
    }

    async fn send(&self, text: String) -> Result<(), Error> {
        self.out
            .send(text)
            .await
            .map_err(|_| Error::Closed("socket writer gone".into()))
    }
}

impl Drop for WsSend {
    /// Dropping a `Tx` finishes the stream, as on the other carriers. Best effort: a full writer
    /// queue loses it, and a stream already finished gets a second `fin` the peer ignores.
    fn drop(&mut self) {
        let _ = self
            .out
            .try_send(format!("{{\"s\":{},\"fin\":true}}", self.id));
    }
}

/// The receiving half of one stream.
pub struct WsRecv {
    rx: mpsc::Receiver<In>,
}

impl WsRecv {
    /// The next message, `None` once the peer finished (or the connection is gone).
    pub(crate) async fn next(&mut self) -> Result<Option<Bytes>, Error> {
        match self.rx.recv().await {
            Some(In::Msg(b)) => Ok(Some(b)),
            Some(In::Fin) | None => Ok(None),
            Some(In::Rst) => Err(Error::Reset),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::rpc::{Cursors, C2S, S2C};
    use femme_proto::{PaneId, Sid};
    use serde_json::{json, Value};

    struct Peer {
        conn: Arc<WsConn>,
        to_conn: mpsc::Sender<String>,
        from_conn: mpsc::Receiver<String>,
    }

    fn peer() -> Peer {
        let (out, from_conn) = mpsc::channel(64);
        let (to_conn, inbound) = mpsc::channel(64);
        Peer {
            conn: WsConn::start(out, inbound),
            to_conn,
            from_conn,
        }
    }

    impl Peer {
        async fn say(&self, v: Value) {
            self.to_conn.send(v.to_string()).await.unwrap();
        }
        async fn hear(&mut self) -> Value {
            serde_json::from_str(&self.from_conn.recv().await.expect("a message out")).unwrap()
        }
    }

    #[tokio::test]
    async fn streams_open_carry_and_end_both_ways() {
        let mut p = peer();
        p.say(json!({"s": 1, "open": "Ctrl"})).await;
        let hello = C2S::Hello {
            proto: 1,
            client_instance: 5,
            cursors: Cursors::default(),
        };
        p.say(json!({"s": 1, "m": serde_json::to_value(&hello).unwrap()}))
            .await;
        let (kind, mut tx, mut rx) = p.conn.accept().await.unwrap();
        assert_eq!(kind, StreamKind::Ctrl);
        assert_eq!(rx.recv_msg::<C2S>().await.unwrap(), Some(hello));

        let welcome = S2C::Welcome {
            proto: 1,
            server_version: "x".into(),
            epoch: 7,
        };
        tx.send(&welcome).await.unwrap();
        assert_eq!(
            p.hear().await,
            json!({"s": 1, "m": serde_json::to_value(&welcome).unwrap()})
        );

        // the daemon opens a stream of its own, with an even id
        let (_etx, _erx) = p.conn.open_kind(StreamKind::Events).await.unwrap();
        assert_eq!(p.hear().await, json!({"s": 2, "open": "Events"}));

        // the peer finishes its side of ctrl; ours finishes with the Tx
        p.say(json!({"s": 1, "fin": true})).await;
        assert_eq!(rx.recv().await.unwrap(), None);
        drop(tx);
        assert_eq!(p.hear().await, json!({"s": 1, "fin": true}));

        // a term stream, reset by the peer
        let pane = PaneId {
            sid: Sid::new(),
            no: 0,
        };
        p.say(json!({"s": 3, "open": {"Term": serde_json::to_value(pane).unwrap()}}))
            .await;
        let (kind, _ttx, mut trx) = p.conn.accept().await.unwrap();
        assert_eq!(kind, StreamKind::Term(pane));
        p.say(json!({"s": 3, "rst": true})).await;
        assert!(matches!(trx.recv().await, Err(Error::Reset)));

        // the socket goes: the connection is dead
        drop(p.to_conn);
        assert!(matches!(p.conn.closed().await, Error::Closed(_)));
        assert!(p.conn.accept().await.is_err());
        assert!(p.conn.open_kind(StreamKind::Ctrl).await.is_err());
    }

    #[tokio::test]
    async fn garbage_ends_the_connection() {
        let p = peer();
        p.say(json!({"s": 1, "open": "Ctrl"})).await;
        let (_, _tx, mut rx) = p.conn.accept().await.unwrap();
        p.to_conn.send("not json".into()).await.unwrap();
        assert!(matches!(p.conn.closed().await, Error::Proto(_)));
        assert_eq!(rx.recv().await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_message_for_a_stream_nobody_reads_is_dropped() {
        let mut p = peer();
        p.say(json!({"s": 1, "open": "Ctrl"})).await;
        let (_, mut tx, rx) = p.conn.accept().await.unwrap();
        drop(rx);
        p.say(json!({"s": 1, "m": "whatever"})).await;
        p.say(json!({"s": 9, "m": "whatever"})).await;
        // still alive and writable
        tx.send(&S2C::Welcome {
            proto: 1,
            server_version: "x".into(),
            epoch: 1,
        })
        .await
        .unwrap();
        assert_eq!(p.hear().await["s"], 1);
        assert!(p.conn.dead_err().is_none());
    }
}
