//! The connection task: one loop that dials (QUIC raced against the SSH mux), says `Hello`, serves
//! the session until it ends, and dials again. All wire traffic of `ctrl` and `events` goes through
//! here, so events leave in the order the daemon sent them and cursors move before they do.
//!
//! The streams of a session: `ctrl` (opened here, `Hello` first, then requests and their
//! answers), `events` (opened by the daemon after `Welcome`; this side accepts it and keeps its
//! send half), and one `term` stream per pane, which belongs to the pane's own task
//! ([`crate::term`]) and finds the connection through [`Live`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use femme_proto::rpc::{Cursors, Err as WireErr, Resp, Scope, StreamKind, C2S, S2C};
use femme_proto::state::Row;
use femme_proto::{OpId, PROTO_VERSION};
use femme_transport::quic::{KEEPALIVE_BACKGROUND, KEEPALIVE_FOREGROUND};
use femme_transport::race::{race, ConnFuture};
use femme_transport::{
    command_connect, mux_client, ssh_connect, Conn, DeviceIdentity, Error, QuicClient, Rx, Tx,
};
use tokio::task::JoinSet;
use tokio::time::{interval, sleep, sleep_until, timeout, Instant};

use crate::{Event, Link, Live, RequestError, Shared};

/// How long to wait before dial number 1, 2, 3, ... after the last connection was lost (and before
/// the very first). The last entry is the cap.
const BACKOFF: [Duration; 5] = [
    Duration::ZERO,
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
];
/// How long one dial of QUIC may take. A dead UDP port does not refuse, it stays silent.
const QUIC_WAIT: Duration = Duration::from_secs(2);
/// While QUIC has not answered, another attempt starts this often (each is a fresh endpoint and a
/// fresh Initial packet). Quinn retransmits its Initial after 0.3 s, then 0.6 s later, then 1.2 s:
/// one attempt per window leaves holes in which a daemon that comes back is not heard, and this
/// is what makes "back within a second or two" a property of the client, not of the timing.
const QUIC_STAGGER: Duration = Duration::from_millis(400);
/// `Hello` to `Welcome` and the events stream.
const HELLO_WAIT: Duration = Duration::from_secs(10);
/// The longest one QUIC probe from SSH may take, whatever `quic_reprobe` is.
const PROBE_MAX: Duration = Duration::from_secs(3);
/// A connection that did not last this long counts as a failed dial for the backoff.
const FLAPPING: Duration = Duration::from_secs(1);
/// QUIC is back but a request is out on SSH: look again shortly.
const PROBE_AGAIN: Duration = Duration::from_millis(250);

/// A welcomed connection with its streams.
struct Session {
    conn: Arc<dyn Conn>,
    link: Link,
    ctrl_tx: Tx,
    ctrl_rx: Rx,
    events_rx: Rx,
    /// The send half of the stream the daemon opened: kept so it is not finished behind its back.
    _events_tx: Tx,
    server_version: String,
    epoch: u64,
    quic_error: Option<String>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // Whoever else holds the connection (the terminals) must not keep it alive.
        self.conn.close();
    }
}

type Probe = Pin<Box<dyn Future<Output = Result<Session, String>> + Send>>;

pub(crate) async fn run(sh: Arc<Shared>) {
    let mut failures = 0usize;
    // The last reason told while down, so a daemon that stays away is reported once.
    let mut told: Option<String> = None;
    loop {
        nap(&sh, BACKOFF[failures.min(BACKOFF.len() - 1)]).await;
        let dialed = tokio::select! {
            r = dial(&sh) => Some(r),
            // rebind() while down: the network changed, whatever was going on starts over now.
            () = kicked(&sh) => None,
        };
        match dialed {
            None => failures = 0,
            Some(Ok(s)) => {
                let up = Instant::now();
                let reason = serve(&sh, s).await;
                tracing::info!(%reason, "disconnected");
                failures = if up.elapsed() < FLAPPING {
                    failures + 1
                } else {
                    0
                };
                disconnected(&sh, reason.clone());
                told = Some(reason);
            }
            Some(Err(reason)) => {
                failures += 1;
                tracing::debug!(%reason, failures, "dial failed");
                if told.as_ref() != Some(&reason) {
                    sh.emit(Event::Disconnected {
                        reason: reason.clone(),
                    });
                    told = Some(reason);
                }
            }
        }
    }
}

/// Sleeps `d`, less if `rebind()` says so.
async fn nap(sh: &Shared, d: Duration) {
    if d.is_zero() || take_kick(sh) {
        return;
    }
    let end = Instant::now() + d;
    loop {
        tokio::select! {
            () = sleep_until(end) => return,
            () = sh.wake.notified() => if take_kick(sh) { return },
        }
    }
}

async fn kicked(sh: &Shared) {
    loop {
        sh.wake.notified().await;
        if take_kick(sh) {
            return;
        }
    }
}

fn take_kick(sh: &Shared) -> bool {
    std::mem::take(&mut sh.state().kick)
}

// ---- dialing --------------------------------------------------------------------------------

/// One attempt: the race of the configured transports, then `Hello`.
async fn dial(sh: &Arc<Shared>) -> Result<Session, String> {
    if let Some(sock) = &sh.unix {
        // A daemon on this machine: no race, no QUIC, the mux straight over its socket.
        let stream = tokio::net::UnixStream::connect(sock)
            .await
            .map_err(|e| format!("{}: {e}", sock.display()))?;
        let (r, w) = stream.into_split();
        return hello(sh, mux_client(r, w), None).await;
    }
    let best = race(quic_leg(sh, QUIC_WAIT), ssh_leg(sh))
        .await
        .map_err(|e| e.to_string())?;
    hello(sh, best.conn, best.quic_error.map(|e| e.to_string())).await
}

/// A QUIC attempt for the race, or for the probe from SSH. It is several attempts, a fresh one
/// every [`QUIC_STAGGER`] until `wait` is up; the first answer, good or bad, is the answer (a
/// refused device is refused by every attempt).
fn quic_leg(sh: &Shared, wait: Duration) -> Option<ConnFuture> {
    let (addr, fp) = sh.quic?;
    let dev = sh.device.clone();
    // Quinn fixes the keepalive when the connection is made.
    let keepalive = if sh.state().background {
        KEEPALIVE_BACKGROUND
    } else {
        KEEPALIVE_FOREGROUND
    };
    Some(Box::pin(async move {
        let mut attempts = JoinSet::new();
        let mut stagger = interval(QUIC_STAGGER);
        let end = sleep(wait);
        tokio::pin!(end);
        loop {
            tokio::select! {
                _ = stagger.tick() => {
                    let dev = dev.clone();
                    attempts.spawn(async move { quic_once(addr, fp, &dev, keepalive).await });
                }
                Some(done) = attempts.join_next() => {
                    return done.unwrap_or_else(|e| Err(Error::Connect(format!("QUIC attempt: {e}"))));
                }
                () = &mut end => {
                    return Err(Error::Connect(format!("no QUIC handshake with {addr} within {wait:?}")));
                }
            }
        }
    }))
}

/// `femme_transport::connect` with the keepalive of the moment: the connection, once the daemon
/// has had the round trip it needs to refuse the device (TLS 1.3 lets the client finish first).
async fn quic_once(
    addr: std::net::SocketAddr,
    fp: [u8; 32],
    dev: &DeviceIdentity,
    keepalive: Duration,
) -> Result<Arc<dyn Conn>, Error> {
    let conn: Arc<dyn Conn> = QuicClient::connect(addr, fp, dev, Some(keepalive)).await?;
    let settle = (conn.rtt() * 2).max(Duration::from_millis(50));
    tokio::select! {
        e = conn.closed() => Err(e),
        () = sleep(settle) => Ok(conn),
    }
}

fn ssh_leg(sh: &Shared) -> Option<ConnFuture> {
    if let Some(cmd) = &sh.ssh_command {
        return Some(Box::pin(command_connect(fresh(cmd))));
    }
    let host = sh.ssh_host.clone()?;
    Some(Box::pin(async move {
        ssh_connect(&host, &["femmed", "proxy"]).await
    }))
}

/// A connection consumes its command, and every dial needs one: a copy made of what can be read
/// back from it (program, arguments, environment set or removed, folder).
// ponytail: `env_clear` and stdio settings are not readable and not carried over; the test hook
// that needs them can set the variables it cares about instead.
fn fresh(cmd: &tokio::process::Command) -> tokio::process::Command {
    let std = cmd.as_std();
    let mut copy = tokio::process::Command::new(std.get_program());
    copy.args(std.get_args());
    for (key, value) in std.get_envs() {
        match value {
            Some(v) => copy.env(key, v),
            None => copy.env_remove(key),
        };
    }
    if let Some(dir) = std.get_current_dir() {
        copy.current_dir(dir);
    }
    copy
}

/// `Hello`, `Welcome`, and the events stream the daemon opens right after.
async fn hello(
    sh: &Shared,
    conn: Arc<dyn Conn>,
    quic_error: Option<String>,
) -> Result<Session, String> {
    let link = if conn.is_quic() {
        Link::Quic
    } else if sh.unix.is_some() {
        Link::Local
    } else {
        Link::Ssh
    };
    // The epoch only: the scopes are subscribed one by one right after, from the cursors of then
    // (a `Hello` that carried them would subscribe on its own and the daemon would send twice).
    let cursors = Cursors {
        epoch: sh.state().cursors.epoch,
        ..Cursors::default()
    };
    let steps = async {
        let e = |e: Error| e.to_string();
        let (mut ctrl_tx, mut ctrl_rx) = conn.open_kind(StreamKind::Ctrl).await.map_err(e)?;
        let hello = C2S::Hello {
            proto: PROTO_VERSION,
            client_instance: sh.instance,
            cursors,
        };
        ctrl_tx.send(&hello).await.map_err(e)?;
        let (server_version, epoch) = match ctrl_rx.recv_msg::<S2C>().await.map_err(e)? {
            Some(S2C::Welcome {
                server_version,
                epoch,
                ..
            }) => (server_version, epoch),
            // `S2C` has no error before `Welcome`: a refusal is a `Resp` for the nil op.
            Some(S2C::Resp {
                result: Err(refused),
                ..
            }) => return Err(format!("the daemon refused this client: {}", refused.msg)),
            Some(other) => return Err(format!("expected Welcome, got {other:?}")),
            None => return Err("the daemon closed the control stream before Welcome".into()),
        };
        let (kind, events_tx, events_rx) = conn.accept().await.map_err(e)?;
        if kind != StreamKind::Events {
            return Err(format!("expected the events stream, got {kind:?}"));
        }
        Ok::<_, String>((
            ctrl_tx,
            ctrl_rx,
            events_rx,
            events_tx,
            server_version,
            epoch,
        ))
    };
    let (ctrl_tx, ctrl_rx, events_rx, events_tx, server_version, epoch) =
        match timeout(HELLO_WAIT, steps).await {
            Ok(Ok(ok)) => ok,
            Ok(Err(why)) => {
                conn.close();
                return Err(why);
            }
            Err(_) => {
                conn.close();
                return Err(format!("no Welcome within {HELLO_WAIT:?}"));
            }
        };
    Ok(Session {
        conn,
        link,
        ctrl_tx,
        ctrl_rx,
        events_rx,
        _events_tx: events_tx,
        server_version,
        epoch,
        quic_error,
    })
}

/// A QUIC connection, welcomed, while the live one is SSH.
async fn dial_quic(sh: Arc<Shared>) -> Result<Session, String> {
    let wait = sh.reprobe.min(PROBE_MAX);
    let Some(leg) = quic_leg(&sh, wait) else {
        return Err("no QUIC address".into());
    };
    let conn = leg.await.map_err(|e| e.to_string())?;
    hello(&sh, conn, None).await
}

// ---- serving ----------------------------------------------------------------------------------

/// Makes a welcomed connection the live one: the epoch is checked (another database drops every
/// cursor and tells the caller what to drop), whatever is owed is marked to go out again, the
/// caller hears of it, and the terminals are pointed at it. Returns the connection's generation.
fn install(sh: &Shared, s: &Session) -> u64 {
    let (gen, reset) = {
        let mut st = sh.state();
        st.gen += 1;
        st.kick = false;
        let reset = st.adopt_epoch(s.epoch);
        st.conn = Some(s.conn.clone());
        st.link = Some(s.link);
        st.welcomed();
        (st.gen, reset)
    };
    tracing::info!(link = ?s.link, epoch = s.epoch, gen, "connected");
    sh.emit(Event::Connected {
        link: s.link,
        server_version: s.server_version.clone(),
        epoch: s.epoch,
        quic_error: s.quic_error.clone(),
    });
    for scope in reset {
        sh.emit(Event::Reset { scope });
    }
    sh.live.send_replace(Live {
        gen,
        conn: Some(s.conn.clone()),
        stopped: false,
    });
    gen
}

fn disconnected(sh: &Shared, reason: String) {
    let gen = {
        let mut st = sh.state();
        st.conn = None;
        st.link = None;
        st.gen
    };
    sh.live.send_replace(Live {
        gen,
        conn: None,
        stopped: false,
    });
    sh.emit(Event::Disconnected { reason });
}

/// Writes whatever is owed to the daemon on this connection. Sends are not cancel-safe, so this
/// only ever runs as a statement of the loop, never as a `select!` branch.
async fn flush(sh: &Shared, s: &mut Session, gen: u64) -> Result<(), String> {
    let batch = sh.state().unsent(gen);
    for (op, req) in batch {
        s.ctrl_tx
            .send(&C2S::Req { op, req })
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Runs the session until it ends; returns why. On SSH it also tries QUIC every `quic_reprobe`
/// and moves everything over when it connects.
async fn serve(sh: &Arc<Shared>, mut s: Session) -> String {
    let mut gen = install(sh, &s);
    let mut probe: Option<Probe> = None;
    let mut next_probe = probe_due(sh, &s);
    loop {
        if let Err(e) = flush(sh, &mut s, gen).await {
            return e;
        }
        let due = next_probe.filter(|_| probe.is_none());
        let probing = probe.is_some();
        tokio::select! {
            e = s.conn.closed() => return e.to_string(),
            m = s.ctrl_rx.recv_msg::<S2C>() => match m {
                Ok(Some(msg)) => on_ctrl(sh, msg),
                Ok(None) => return "the daemon ended the control stream".into(),
                Err(e) => return e.to_string(),
            },
            m = s.events_rx.recv_msg::<S2C>() => match m {
                Ok(Some(msg)) => on_event(sh, msg),
                Ok(None) => return "the daemon ended the events stream".into(),
                Err(e) => return e.to_string(),
            },
            // New work (a request, a subscription): the top of the loop writes it.
            () = sh.wake.notified() => {}
            () = async {
                match due {
                    Some(t) => sleep_until(t).await,
                    None => std::future::pending().await,
                }
            } => {
                probe = Some(Box::pin(dial_quic(sh.clone())));
                next_probe = Some(Instant::now() + sh.reprobe);
            }
            r = async {
                match probe.as_mut() {
                    Some(p) => p.await,
                    None => std::future::pending().await,
                }
            }, if probing => {
                probe = None;
                match r {
                    Ok(quic) if sh.state().in_flight(gen) => {
                        // A request written on SSH may already have run; its retry on another link
                        // is another device to the daemon's op table. Let it land first.
                        tracing::debug!("QUIC is back; a request is out on SSH, switching a moment later");
                        drop(quic);
                        next_probe = Some(Instant::now() + PROBE_AGAIN);
                    }
                    Ok(quic) => {
                        tracing::info!("QUIC is back; leaving SSH");
                        let ssh = std::mem::replace(&mut s, quic);
                        gen = install(sh, &s);
                        drop(ssh);
                        next_probe = None;
                    }
                    Err(why) => tracing::debug!(%why, "QUIC probe failed"),
                }
            }
        }
    }
}

fn probe_due(sh: &Shared, s: &Session) -> Option<Instant> {
    (s.link == Link::Ssh && sh.quic.is_some()).then(|| Instant::now() + sh.reprobe)
}

/// Answers on `ctrl` are `Resp`; anything else there is not ours to understand.
fn on_ctrl(sh: &Shared, msg: S2C) {
    let S2C::Resp { op, result } = msg else {
        return tracing::warn!(?msg, "unexpected message on the control stream");
    };
    answered(sh, op, result);
}

fn answered(sh: &Shared, op: OpId, result: Result<Resp, WireErr>) {
    let Some(p) = sh.state().answered(op, result.is_err()) else {
        return tracing::debug!(%op, "answer to a request nobody waits for");
    };
    if let Some(done) = p.done {
        let _ = done.send(result.map_err(|e| RequestError::Server {
            code: e.code,
            msg: e.msg,
        }));
    }
}

fn on_event(sh: &Shared, msg: S2C) {
    match msg {
        S2C::Changes {
            scope,
            rev,
            rows,
            reset,
        } => changes(sh, scope, rev, rows, reset),
        S2C::Delta {
            sid,
            item_id,
            offset,
            text,
        } => {
            if sh.state().subs.contains(&Scope::Session(sid)) {
                sh.emit(Event::Delta {
                    sid,
                    item_id,
                    offset,
                    text,
                });
            }
        }
        // ponytail: bulk streams are M5; the daemon does not send this yet.
        S2C::SnapshotOnBulk { scope, .. } => {
            tracing::warn!(?scope, "snapshot on a bulk stream: not supported")
        }
        other => tracing::warn!(?other, "unexpected message on the events stream"),
    }
}

/// The one place a `Changes` of the wire becomes events. The cursor moves first, so whoever gets
/// the event and asks for `cursors()` sees it; `reset` (the daemon could not honour the cursor
/// and sent the scope's full set) is told as `Reset` ahead of the rows; a scope nobody subscribed
/// to (or no longer) is dropped.
fn changes(sh: &Shared, scope: Scope, rev: u64, rows: Vec<Row>, reset: bool) {
    let mut st = sh.state();
    if !st.subs.contains(&scope) {
        return tracing::debug!(?scope, "changes for a scope nobody subscribed to dropped");
    }
    st.advance(scope, rev, reset);
    if reset {
        sh.emit(Event::Reset { scope });
    }
    sh.emit(Event::Changes { scope, rev, rows });
}
