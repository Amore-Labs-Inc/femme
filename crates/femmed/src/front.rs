//! The desktop's way in: `femmed` listens for `femme-proto` on QUIC (UDP `QUIC_PORT`, devices
//! pinned by the SPKI hash `femmed enroll` recorded) and, as the SSH fallback, on the unix socket
//! `femmed proxy` relays to. Both give the same [`Conn`], and everything above it is one handler.
//!
//! A connection is one `ctrl` stream (the desktop opens it and says `Hello`; requests and their
//! answers follow, concurrently and in any order), one `events` stream (opened by the daemon after
//! `Welcome`: the replicated state the connection subscribed to) and one `term` stream per
//! attached pane ([`term`]). There is no session layer: resume is a new connection and a `Hello`
//! carrying the revs the desktop holds, which subscribes it to those scopes from there (another
//! epoch: from scratch). A `Hello` without cursors subscribes to nothing until the desktop asks.
//! A connection that has not said `Hello` within [`HELLO_WAIT`] is closed, and no terminal is
//! served before the `Welcome`. A device revoked while connected loses its connections within
//! [`RECHECK`].
//!
//! A request that changes something is remembered by `(device, op)` for 24 h ([`Ctx::run`]): a
//! retry after a dropped connection gets the recorded answer instead of running the op again. The
//! op id belongs to the request it came with: another request under it is refused.
//!
//! [`stats`] is the terminal relay's latency, measured per hop and from a key to its echo, because
//! the plan says the daemon adds under 3 ms to a keystroke and that wants a number.

pub(crate) mod admin;
mod board;
mod caps;
mod fs;
mod panes;
mod prs;
mod req;
mod term;
pub(crate) mod ws;

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use anyhow::Context;
use femme_proto::codec;
use femme_proto::rpc::{Err as WireErr, ErrCode, Req, Resp, Scope, StreamKind, C2S, S2C};
use femme_proto::{DeviceId, Id, OpId, PROTO_VERSION};
use femme_store::{auth, sha256};
use femme_transport::quic::Allow;
use femme_transport::{fp_to_hex, listen, mux_server, Conn, Listener, ServerIdentity, Tx};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch, OnceCell, OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinHandle, JoinSet};

pub use term::{stats, Quantiles, Stats};

use crate::config::Config;
use crate::daemon::{now_ms, Daemon};
use crate::hub::{ClientId, SubscribeError};
use term::Hops;

/// Requests one connection may have running at once; the next gets `Busy`.
const MAX_INFLIGHT: usize = 64;
/// Messages queued for one connection's `events` stream before the hub calls it lagging.
const EVENTS_QUEUE: usize = 256;
/// Answers queued for one connection's `ctrl` stream.
const ANSWER_QUEUE: usize = 64;
/// The most a request may weigh on the wire: what the board's API takes (`MAX_BODY` there, private
/// to it) and answers 413 past. A bigger one is `Bad`, so nothing gets in through here that the
/// board would refuse, and no row outgrows what a subscription can carry.
const BODY_MAX: usize = 1 << 20;
/// How long a refused client gets to read why before the connection is closed on it.
const LINGER: Duration = Duration::from_secs(5);
/// How often the terminal relay's latency is logged (when it has changed).
const REPORT_EVERY: Duration = Duration::from_secs(60);
/// How long a connection may take to open its ctrl stream and say `Hello`.
const HELLO_WAIT: Duration = Duration::from_secs(10);
/// How often a connection's device is checked to be still enrolled: how long a revoked device's
/// open connections last.
const RECHECK: Duration = Duration::from_secs(30);

/// The device behind the SSH fallback: whoever logged in as our user. No row in `devices`.
const SSH: DeviceId = Id(0);

/// What a request comes to: its answer on the wire.
type Outcome = Result<Resp, WireErr>;

/// The answer to a request that comes under an op id another request has.
const OP_REUSED: &str = "that op id belongs to another request";

/// SHA-256 of a request's encoding: what ties an op id to the request it came with.
type Digest = [u8; 32];

/// Mutating requests being run, by `(device, op)`: the digest of the request that came first, and
/// what it came to.
type Inflight = Mutex<HashMap<(DeviceId, OpId), Arc<OnceCell<(Digest, Outcome)>>>>;

/// The front's clocks; tests shorten them.
#[derive(Clone, Copy)]
pub struct Waits {
    /// How long a connection may take to say `Hello`, and a terminal stream to be allowed (it waits
    /// for the `Welcome`).
    pub hello: Duration,
    /// How often a connection checks that its device is still enrolled and not revoked.
    pub recheck: Duration,
}

impl Default for Waits {
    fn default() -> Waits {
        Waits {
            hello: HELLO_WAIT,
            recheck: RECHECK,
        }
    }
}

/// What every connection of one front shares.
struct Front {
    /// Weak: the accept loops live as long as the [`FrontHandle`], not as long as the daemon.
    daemon: Weak<Daemon>,
    waits: Waits,
    hops: Arc<Hops>,
    /// A retry that arrives on a new connection while the old connection's attempt still runs
    /// joins that attempt instead of running the op a second time.
    inflight: Inflight,
}

/// One connection's view of the daemon, for the requests it makes.
pub(crate) struct Ctx {
    daemon: Arc<Daemon>,
    front: Arc<Front>,
    device: DeviceId,
    /// Where the hub sends this connection's `events`.
    client: ClientId,
}

/// A running front. Dropping it stops it: the listeners and every connection close, and the socket
/// file goes.
pub struct FrontHandle {
    quic: SocketAddr,
    fingerprint: [u8; 32],
    sock: PathBuf,
    hops: Arc<Hops>,
    gate: Arc<WsGate>,
    tasks: Vec<JoinHandle<()>>,
}

/// The phone's way to the front: the HTTP layer's `/femme/ws` route ([`ws`]) serves its
/// connections on the front this points at, for as long as that front runs.
pub struct WsGate {
    front: Weak<Front>,
}

impl WsGate {
    fn front(&self) -> Option<Arc<Front>> {
        self.front.upgrade()
    }
}

impl FrontHandle {
    /// What the HTTP layer needs to serve phones on this front.
    pub fn gate(&self) -> Arc<WsGate> {
        self.gate.clone()
    }

    /// Where QUIC listens (the port is the real one when the front was asked for port 0).
    pub fn quic_addr(&self) -> SocketAddr {
        self.quic
    }

    /// SHA-256 of the daemon's certificate: what a desktop pins.
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// The unix socket of the SSH fallback.
    pub fn sock_path(&self) -> &Path {
        &self.sock
    }

    /// The terminal relay's latency through this front.
    pub fn stats(&self) -> Stats {
        self.hops.stats()
    }
}

impl Drop for FrontHandle {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        let _ = std::fs::remove_file(&self.sock);
    }
}

/// The daemon's TLS identity, made on first use: `<state>/quic/server.{crt,key}`. `femmed enroll`
/// reads it too, so the fingerprint it prints is the one the daemon will present.
pub(crate) fn identity(cfg: &Config) -> anyhow::Result<ServerIdentity> {
    let dir = cfg.state_dir.join("quic");
    ServerIdentity::load_or_generate(&dir, std::slice::from_ref(&cfg.host))
        .with_context(|| format!("QUIC identity in {}", dir.display()))
}

/// [`serve_front_on`] at `0.0.0.0:<QUIC_PORT>`.
pub async fn serve_front(daemon: Arc<Daemon>) -> anyhow::Result<FrontHandle> {
    // ponytail: IPv4 only, so a desktop on an IPv6-only network cannot connect. Bind `[::]` (dual
    // stack) instead, or a second endpoint beside this one, when one has to.
    let bind = SocketAddr::from(([0, 0, 0, 0], daemon.cfg.quic_port));
    serve_front_on(daemon, bind).await
}

/// [`serve_front_with`] on the real clocks.
pub async fn serve_front_on(daemon: Arc<Daemon>, quic: SocketAddr) -> anyhow::Result<FrontHandle> {
    serve_front_with(daemon, quic, Waits::default()).await
}

/// Starts the front: QUIC on `quic` (port 0 picks one; see [`FrontHandle::quic_addr`]) and the
/// SSH fallback on `<runtime>/femme/femmed.sock`, the path `femmed proxy` connects to. Fails
/// with a "cannot bind" message when either is taken.
pub async fn serve_front_with(
    daemon: Arc<Daemon>,
    quic: SocketAddr,
    waits: Waits,
) -> anyhow::Result<FrontHandle> {
    let identity = identity(&daemon.cfg)?;
    let fingerprint = identity.fingerprint();
    let allow: Allow = {
        let store = daemon.store.clone();
        Arc::new(move |spki| {
            // ponytail: a database read on the connection's I/O task (a pooled, indexed lookup,
            // microseconds); a set kept in memory and updated by enroll/revoke if handshakes pile up.
            store
                .read(|c| auth::device_by_spki(c, spki))
                .inspect_err(|e| tracing::warn!(%e, "device lookup failed; refusing the handshake"))
                .ok()
                .flatten()
                .map(|d| d.id)
        })
    };
    let listener = listen(quic, &identity, allow)
        .await
        .with_context(|| format!("cannot bind QUIC {quic}"))?;
    let quic = listener.local_addr().context("QUIC address")?;
    let sock = daemon.sessions.dirs.runtime.join("femmed.sock");
    let unix = bind_sock(&sock).await?;
    // The socket is ours, so its owner is who may connect: the same user's ssh login.
    let uid = std::fs::metadata(&sock).context("socket owner")?.uid();
    let hops = Arc::new(Hops::new());
    let front = Arc::new(Front {
        daemon: Arc::downgrade(&daemon),
        waits,
        hops: hops.clone(),
        inflight: Mutex::default(),
    });
    tracing::info!(
        fingerprint = %fp_to_hex(&fingerprint),
        %quic,
        sock = %sock.display(),
        "desktop front up"
    );
    let gate = Arc::new(WsGate {
        front: Arc::downgrade(&front),
    });
    let tasks = vec![
        tokio::spawn(quic_loop(front.clone(), listener)),
        tokio::spawn(sock_loop(front, unix, uid)),
        tokio::spawn(report(hops.clone())),
    ];
    Ok(FrontHandle {
        quic,
        fingerprint,
        sock,
        hops,
        gate,
        tasks,
    })
}

/// Claims the socket path: its folder is ours alone (0700), a leftover from a crash is replaced,
/// another live femmed is an error, and the socket itself is 0600.
async fn bind_sock(path: &Path) -> anyhow::Result<UnixListener> {
    let cannot = || format!("cannot bind {}", path.display());
    let dir = path.parent().context("a socket path has a folder")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(cannot)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).with_context(cannot)?;
    match UnixStream::connect(path).await {
        Ok(_) => anyhow::bail!("{}: another femmed is serving it", cannot()),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            let _ = std::fs::remove_file(path);
        }
        Err(e) => return Err(e).with_context(cannot),
    }
    let listener = UnixListener::bind(path).with_context(cannot)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).with_context(cannot)?;
    Ok(listener)
}

async fn quic_loop(front: Arc<Front>, listener: Listener) {
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((device, conn)) => {
                    conns.spawn(serve_conn(front.clone(), device, conn));
                }
                // `accept` fails only when the endpoint is closed. The daemon would go on without its
                // way in and look well to systemd; out is one line, and `Restart=always` brings a
                // new one up in a second without touching the sessions (the holders are units of
                // their own). Binding the endpoint again here would be more code for the same.
                Err(e) => {
                    tracing::error!("QUIC listener stopped: {e}; exiting for a restart");
                    std::process::exit(1);
                }
            },
            // Reaped as they end, so a long-lived daemon does not collect results.
            Some(_) = conns.join_next() => {}
        }
    }
}

async fn sock_loop(front: Arc<Front>, listener: UnixListener, uid: u32) {
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => match stream.peer_cred() {
                    Ok(peer) if peer.uid() == uid => {
                        let (r, w) = stream.into_split();
                        conns.spawn(serve_conn(front.clone(), SSH, mux_server(r, w)));
                    }
                    Ok(peer) => tracing::warn!(uid = peer.uid(), "socket connection from another user dropped"),
                    Err(e) => tracing::warn!("socket connection dropped, peer unknown: {e}"),
                },
                Err(e) => {
                    // Out of file descriptors and the like: do not spin on it.
                    tracing::warn!("accept on the daemon socket: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = conns.join_next() => {}
        }
    }
}

/// Logs the relay's latency every [`REPORT_EVERY`] while it is being used.
async fn report(hops: Arc<Hops>) {
    let mut every = tokio::time::interval(REPORT_EVERY);
    every.tick().await; // the first tick is immediate
    let mut seen = 0;
    loop {
        every.tick().await;
        let s = hops.stats();
        if s.up.n + s.down.n != seen {
            seen = s.up.n + s.down.n;
            tracing::info!(up = %s.up, down = %s.down, key = %s.key, "terminal relay latency");
        }
    }
}

/// One authenticated connection, until it ends. Streams are routed by kind: the first `Ctrl` is
/// the request channel (its end ends the connection), each `Term(pane)` is a relay; the
/// daemon-opened `Events` stream is the ctrl handler's. Everything the connection started goes
/// with it, except the requests already running: those finish and are recorded.
async fn serve_conn(front: Arc<Front>, device: DeviceId, conn: Arc<dyn Conn>) {
    let Some(daemon) = front.daemon.upgrade() else {
        return conn.close();
    };
    if !device.is_nil() {
        // ponytail: a write per connection; a minute's throttle if a client ever reconnects in a loop.
        if let Err(e) = daemon.store.write(|tx| tx.touch_device(device, now_ms())) {
            tracing::debug!(%device, %e, "last_seen not noted");
        }
    }
    tracing::info!(%device, quic = conn.is_quic(), "desktop connected");
    let mut tasks = JoinSet::new();
    let mut ctrl = false;
    // Set once the ctrl stream has sent `Welcome`: terminals are served from then on.
    let (welcome_sent, welcomed) = watch::channel(false);
    let welcome_sent = Arc::new(welcome_sent);
    let hello_by = tokio::time::sleep(front.waits.hello);
    tokio::pin!(hello_by);
    let mut recheck = tokio::time::interval(front.waits.recheck);
    recheck.tick().await; // the first tick is immediate
    loop {
        tokio::select! {
            accepted = conn.accept() => match accepted {
                Ok((StreamKind::Ctrl, tx, rx)) if !ctrl => {
                    ctrl = true;
                    let (front, daemon, conn) = (front.clone(), daemon.clone(), conn.clone());
                    let welcome_sent = welcome_sent.clone();
                    tasks.spawn(async move {
                        run_ctrl(front, daemon, device, conn.clone(), tx, rx, welcome_sent).await;
                        // The connection is this stream's: when it ends, so does the connection.
                        conn.close();
                    });
                }
                Ok((StreamKind::Term(pane), tx, rx)) => {
                    let (hops, wait) = (front.hops.clone(), front.waits.hello);
                    tasks.spawn(term::relay(daemon.clone(), hops, pane, tx, rx, welcomed.clone(), wait));
                }
                Ok((kind, mut tx, _)) => {
                    tracing::debug!(?kind, "stream refused");
                    tx.reset();
                }
                Err(_) => break,
            },
            _ = conn.closed() => break,
            // No ctrl stream in time: nobody is going to say Hello.
            () = &mut hello_by, if !ctrl => break,
            // The handshake checked the device once; a revoke since then ends its connections.
            _ = recheck.tick(), if !device.is_nil() => {
                if !allowed(&daemon, device) {
                    tracing::info!(%device, "device revoked, connection closed");
                    break;
                }
            }
            Some(_) = tasks.join_next() => {}
        }
    }
    conn.close();
    tracing::info!(%device, "desktop gone");
}

/// Whether `device` is still enrolled and not revoked.
// ponytail: lists every device (a handful) on the connection's task, once per `RECHECK`; an indexed
// lookup by id if devices ever number in the hundreds. A failed read leaves the connection be.
fn allowed(daemon: &Daemon, device: DeviceId) -> bool {
    match daemon.store.read(auth::list_devices) {
        Ok(all) => all.iter().any(|d| d.id == device && d.revoked.is_none()),
        Err(e) => {
            tracing::warn!(%device, %e, "device recheck failed; the connection stays");
            true
        }
    }
}

/// The connection's place in the hub, given back however the connection ends.
struct Registered {
    daemon: Arc<Daemon>,
    id: ClientId,
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.daemon.hub.remove_client(self.id);
    }
}

/// The `ctrl` stream: `Hello`, `Welcome`, the `events` stream, then requests until the desktop
/// ends the stream, breaks the protocol, or the connection is no use any more.
async fn run_ctrl(
    front: Arc<Front>,
    daemon: Arc<Daemon>,
    device: DeviceId,
    conn: Arc<dyn Conn>,
    mut tx: Tx,
    mut rx: femme_transport::Rx,
    welcome_sent: Arc<watch::Sender<bool>>,
) {
    let hello = tokio::time::timeout(front.waits.hello, rx.recv_msg::<C2S>()).await;
    let Ok(Ok(Some(C2S::Hello { proto, cursors, .. }))) = hello else {
        return tx.reset(); // a ctrl stream starts with Hello, soon
    };
    // This protocol, or the one before it (there is none before 1: 0 is not a protocol).
    if proto != PROTO_VERSION && PROTO_VERSION.checked_sub(1).filter(|v| *v > 0) != Some(proto) {
        // `S2C` has no error before `Welcome`: a `Resp` for the nil op carries it.
        let why = format!(
            "protocol {proto} is not supported (this is femmed {} speaking {PROTO_VERSION})",
            env!("CARGO_PKG_VERSION")
        );
        let result = Err(req::err(ErrCode::Unsupported, why));
        let _ = tx.send(&S2C::Resp { op: Id(0), result }).await;
        let _ = tx.finish().await;
        // Closing now could cut the stream short (QUIC drops what is not acknowledged yet):
        // give the desktop time to read it and hang up.
        tokio::select! {
            _ = rx.recv() => {}
            _ = conn.closed() => {}
            () = tokio::time::sleep(LINGER) => {}
        }
        return;
    }
    let welcome = S2C::Welcome {
        proto,
        server_version: env!("CARGO_PKG_VERSION").into(),
        epoch: daemon.store.epoch(),
    };
    if tx.send(&welcome).await.is_err() {
        return;
    }
    welcome_sent.send_replace(true);
    let Ok((events, _unused)) = conn.open_kind(StreamKind::Events).await else {
        return;
    };
    let (push, pushed) = mpsc::channel(EVENTS_QUEUE);
    let (answer, answers) = mpsc::channel(ANSWER_QUEUE);
    let registered = Registered {
        id: daemon.hub.add_client(push.clone()),
        daemon: daemon.clone(),
    };
    let ctx = Arc::new(Ctx {
        daemon: daemon.clone(),
        front,
        device,
        client: registered.id,
    });
    // The writers die with this future; one of them ending means the stream is broken, and so
    // is the connection (the desktop reconnects with its cursors and loses nothing).
    let mut writers = JoinSet::new();
    writers.spawn(pump(events, pushed));
    writers.spawn(pump(tx, answers));
    // Resume: the scopes the desktop holds, from where it holds them. One task, in order, because
    // each subscribe waits for the events writer to drain its backlog. One scope that cannot be
    // resumed does not stop the rest: only the connection going away does.
    let fresh = cursors.epoch != daemon.store.epoch();
    let held = cursors
        .global_rev
        .map(|rev| (Scope::Global, rev))
        .into_iter()
        .chain(
            cursors
                .sessions
                .iter()
                .map(|&(sid, rev)| (Scope::Session(sid), rev)),
        )
        .map(|(scope, rev)| (scope, (!fresh).then_some(rev)))
        .collect::<Vec<_>>();
    if !held.is_empty() {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            for (scope, after) in held {
                match ctx.daemon.hub.subscribe(ctx.client, scope, after).await {
                    Ok(_) => {}
                    Err(SubscribeError::ClientGone) => return,
                    Err(SubscribeError::Scope(e)) => {
                        tracing::debug!(?scope, %e, "resume: scope not served");
                        // A session the daemon no longer has (retention purged it): the desktop is
                        // told to drop its copy. The global scope is never gone, a failure there is
                        // the database's, and the desktop keeps what it has.
                        let gone = S2C::Changes {
                            scope,
                            rev: 0,
                            rows: vec![],
                            reset: true,
                        };
                        if matches!(scope, Scope::Session(_)) && push.send(gone).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
    }
    let permits = Arc::new(Semaphore::new(MAX_INFLIGHT));
    loop {
        tokio::select! {
            msg = rx.recv_msg::<C2S>() => match msg {
                Ok(Some(C2S::Req { op, req })) => match permits.clone().try_acquire_owned() {
                    Ok(permit) => {
                        tokio::spawn(answered(ctx.clone(), op, req, answer.clone(), permit));
                    }
                    Err(_) => {
                        let result = Err(req::err(ErrCode::Busy, "too many requests in flight"));
                        if answer.send(S2C::Resp { op, result }).await.is_err() {
                            break;
                        }
                    }
                },
                // The end of the stream, an error, or anything but a request: the connection is over.
                _ => break,
            },
            Some(_) = writers.join_next() => break,
        }
    }
    drop(registered);
}

/// Writes what arrives on `queue` to the stream, in order, until the queue closes or the stream
/// fails. A reply too big to frame (a blob past the frame limit) is answered with an error for its
/// op instead of silence; an event too big (one row can be) is skipped and logged.
async fn pump(mut tx: Tx, mut queue: mpsc::Receiver<S2C>) {
    while let Some(msg) = queue.recv().await {
        match tx.send(&msg).await {
            Ok(()) => {}
            Err(femme_transport::Error::Proto(e)) => {
                let S2C::Resp { op, .. } = msg else {
                    // `send` refuses it before a byte is written, so the stream is whole; ending it
                    // would only make the desktop reconnect into the same row.
                    tracing::warn!(%e, "event too large to send, skipped");
                    continue;
                };
                let result = Err(req::err(
                    ErrCode::Internal,
                    format!("reply too large to send: {e}"),
                ));
                if tx.send(&S2C::Resp { op, result }).await.is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Runs one request and queues its answer. The op runs in a task of its own: a connection that
/// goes away mid-way does not cancel it half done (the retry finds its recorded answer), and a
/// panic in it is an `Internal` answer rather than a request that is never answered.
async fn answered(
    ctx: Arc<Ctx>,
    op: OpId,
    req: Req,
    answer: mpsc::Sender<S2C>,
    permit: OwnedSemaphorePermit,
) {
    let run = tokio::spawn({
        let ctx = ctx.clone();
        async move { ctx.run(op, req).await }
    });
    let result = run
        .await
        .unwrap_or_else(|e| Err(req::err(ErrCode::Internal, format!("request failed: {e}"))));
    // Held to here: a desktop that stops reading answers stops being served new requests.
    let _ = answer.send(S2C::Resp { op, result }).await;
    drop(permit);
}

// ponytail: the store calls made for a request (the op row's lookup and record here, the core's
// writes below) are synchronous SQLite on the runtime's workers: microseconds on indexed tables,
// but a stalled disk stalls the worker with them. `spawn_blocking` them, as the transcript page
// and the blob read already are, if a write-heavy hour ever shows up as relay latency.
impl Ctx {
    /// The answer to `req`. A request that changes something runs once per `(device, op)`: the
    /// first attempt's answer is recorded (and shared with a retry that arrives while it runs), and
    /// every later one gets that answer back. Reads are just run.
    async fn run(&self, op: OpId, req: Req) -> Outcome {
        let max = req::body_max(&req);
        let wire = match codec::encode(&req) {
            Ok(bytes) if bytes.len() <= max => bytes,
            _ => {
                let why = format!("a request is at most {max} bytes");
                return Err(req::err(ErrCode::Bad, why));
            }
        };
        // The nil id signs what is no request's answer (a refused `Hello`): a request under it
        // could not be told from that.
        if op.is_nil() {
            return Err(req::err(ErrCode::Bad, "an op id is never nil"));
        }
        if !req::mutates(&req) {
            return req::dispatch(self, op, req).await;
        }
        let digest = sha256(&wire);
        let key = (self.device, op);
        let cell = {
            let mut inflight = self
                .front
                .inflight
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            inflight.entry(key).or_default().clone()
        };
        let (first, out) = cell
            .get_or_init(|| async { (digest, self.once(op, digest, req).await) })
            .await
            .clone();
        // Recorded by now, so a later retry finds it in the database.
        self.front
            .inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
        if first == digest {
            out
        } else {
            Err(req::err(ErrCode::Bad, OP_REUSED))
        }
    }

    /// Runs `req` (whose encoding has `digest`) unless the database already holds this op's
    /// answer. A row is the 32 bytes of the request's digest and then the answer's encoding
    /// (`ops.resp` has no column of its own for the digest); one under another digest, or from
    /// before the digest was added, is not this request's answer.
    // ponytail: the answer is recorded after the op, not in its transaction (the core writes its
    // own): a crash in between runs the op again on retry. Pass the `Tx` through `Core` to close it.
    async fn once(&self, op: OpId, digest: Digest, req: Req) -> Outcome {
        let (store, device) = (&self.daemon.store, self.device);
        match store.read(|c| auth::op_lookup(c, device, op)) {
            Ok(Some(row)) => match row.split_at_checked(32) {
                Some((bound, answer)) if bound == digest => {
                    match codec::decode::<Outcome>(answer) {
                        Ok(out) => return out,
                        Err(e) => {
                            tracing::warn!(%op, %e, "recorded answer unreadable; running the op again")
                        }
                    }
                }
                _ => return Err(req::err(ErrCode::Bad, OP_REUSED)),
            },
            Ok(None) => {}
            Err(e) => tracing::warn!(%op, %e, "op lookup failed; running the op"),
        }
        let out = req::dispatch(self, op, req).await;
        // A "not now" is not the op's answer: remembered, it would be given to every retry for a
        // day, and the retry is what the desktop makes it for.
        if matches!(&out, Err(e) if matches!(e.code, ErrCode::Busy | ErrCode::Internal)) {
            return out;
        }
        match codec::encode(&out) {
            Ok(answer) => {
                let row = [&digest[..], &answer].concat();
                if let Err(e) = store.write(|tx| tx.op_record(device, op, &row, now_ms())) {
                    tracing::warn!(%op, %e, "answer not recorded");
                }
            }
            Err(e) => tracing::warn!(%op, %e, "answer not encodable, not recorded"),
        }
        out
    }
}
