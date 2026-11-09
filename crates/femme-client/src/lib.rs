//! The desktop's network layer: a tokio client of the daemon's front (QUIC, with the SSH stdio mux
//! as the fallback). No GUI and no global state; the app owns a [`Client`], drains its
//! [`events`](Client::events) and makes requests.
//!
//! There is no session layer: a lost connection is a new connection and a `Hello` (plan §6).
//! The client keeps what that needs and nothing more:
//!
//! - **State** is rev-based. [`Client::subscribe`] remembers the scope; after every reconnect it is
//!   subscribed again from the cursor of the moment ([`Client::cursors`]), and the daemon sends
//!   only what is newer. A `Changes` moves the cursor before the app gets it. If the daemon's
//!   database is another one, or the cursor is past what it can vouch for, the app hears
//!   [`Event::Reset`] and drops its copy of the scope; the full rows follow.
//! - **Requests** carry an op id minted once. [`Client::request`] sends it again, same id, on every
//!   new connection until it is answered, and the daemon answers a repeated op from its record
//!   instead of running it twice. It fails on the caller's deadline or on an error the daemon
//!   answered; connection trouble is not an error, it is a wait.
//! - **Terminals** are state too. A [`Term`] reopens its pane on every new connection with the
//!   rev of the last frame it delivered, so what follows is a delta, never a replay, and resends
//!   the input the holder has not confirmed with the same sequence numbers, which the holder
//!   drops if it already has them.
//!
//! Connecting races QUIC against SSH (QUIC wins within 300 ms of SSH being ready); on SSH, QUIC is
//! tried again every [`Config::quic_reprobe`] and everything moves over when it answers. A device
//! the daemon refuses over QUIC shows up as [`Event::Connected::quic_error`] when SSH works.
//!
//! Redials wait 0 ms, 250 ms, 1 s, 2 s, then 5 s between attempts, each a fresh race.
//!
//! Call [`Client::start`] from inside a tokio runtime. Nothing here blocks, and nothing ticks
//! while connected except the transport's keepalive (and the QUIC probe while on SSH).

#![warn(missing_docs)]

use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use femme_proto::rpc::{Cursors, ErrCode, Req, Resp, Scope};
use femme_proto::state::Row;
use femme_proto::{OpId, PaneId, Sid};
use femme_transport::{Conn, DeviceIdentity};
use tokio::sync::{mpsc, oneshot, watch, Notify};
use ulid::Ulid;

mod conn;
mod req;
mod term;

pub use term::Term;

use req::{Kind, State};

/// How long [`Client::request`] waits for the answer.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long [`Client::unsubscribe`] waits for the daemon to confirm.
const UNSUBSCRIBE_WAIT: Duration = Duration::from_secs(5);

/// What [`Client::start`] needs.
#[derive(Debug)]
pub struct Config {
    /// The daemon's UDP address and the SHA-256 of its certificate (what `femmed enroll` prints).
    pub quic: Option<(SocketAddr, [u8; 32])>,
    /// The ssh alias of the box: the fallback runs `ssh -T <host> femmed proxy`.
    pub ssh_host: Option<String>,
    /// Test hook: a command that speaks the mux on its stdio (`femmed proxy`), used instead of
    /// `ssh_host` when set. A connection consumes its command, so every dial runs a copy made of
    /// its program, arguments, set or removed environment and folder (not `env_clear`).
    pub ssh_command: Option<tokio::process::Command>,
    /// This device's TLS identity, enrolled with the daemon.
    pub device: DeviceIdentity,
    /// This process, as terminals know it across reconnects: mint it once per process.
    pub client_instance: Ulid,
    /// What the app last saw. The client keeps it current; read it back with [`Client::cursors`].
    pub cursors: Cursors,
    /// While on SSH, QUIC is tried again this often. [`Config::new`] sets 60 s.
    pub quic_reprobe: Duration,
}

impl Config {
    /// A config with no transport yet: set `quic` and/or `ssh_host`.
    pub fn new(device: DeviceIdentity, client_instance: Ulid) -> Config {
        Config {
            quic: None,
            ssh_host: None,
            ssh_command: None,
            device,
            client_instance,
            cursors: Cursors::default(),
            quic_reprobe: Duration::from_secs(60),
        }
    }
}

/// Which transport a connection is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// QUIC, straight to the daemon.
    Quic,
    /// The SSH stdio mux through `femmed proxy`.
    Ssh,
}

/// What the app drains from [`Client::events`], in the order it happened.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A connection is up and welcomed (also when the client moves from SSH to QUIC), and what was
    /// owed is being sent on it.
    Connected {
        /// The transport.
        link: Link,
        /// The daemon's version.
        server_version: String,
        /// The daemon's database epoch.
        epoch: u64,
        /// QUIC refused this device while SSH worked (or failed in the race for another reason):
        /// the user should hear it, because the cheap link will never come.
        quic_error: Option<String>,
    },
    /// The connection is gone, or could not be made. The client is dialing again; a daemon that
    /// stays away for the same reason is reported once.
    Disconnected {
        /// Why, in the transport's words.
        reason: String,
    },
    /// Rows of a subscribed scope newer than the app's cursor, which already includes `rev`.
    Changes {
        /// The scope the rows belong to.
        scope: Scope,
        /// The highest rev the message covers.
        rev: u64,
        /// The rows, tombstones included.
        rows: Vec<Row>,
    },
    /// Streaming text of an item in progress in a subscribed session.
    Delta {
        /// The session.
        sid: Sid,
        /// The item the text belongs to.
        item_id: String,
        /// Where in the item the text starts.
        offset: u32,
        /// The text.
        text: String,
    },
    /// The daemon could not honour the cursor of `scope` (another database, or older than what it
    /// remembers of deletions): drop the copy of the scope. Its full set follows as `Changes`.
    Reset {
        /// The scope to drop.
        scope: Scope,
    },
}

/// Why [`Client::request`] failed. Trouble with the connection is not among them: the request waits
/// for the next one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// The deadline the caller gave passed. The request is not sent again, but it may have run.
    Timeout,
    /// The daemon answered with an error (`Unsupported` among them). It is not retried.
    Server {
        /// What kind of error.
        code: ErrCode,
        /// The daemon's message.
        msg: String,
    },
    /// The client was dropped.
    Stopped,
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestError::Timeout => f.write_str("no answer within the deadline"),
            RequestError::Server { code, msg } => write!(f, "{code:?}: {msg}"),
            RequestError::Stopped => f.write_str("the client was stopped"),
        }
    }
}

impl std::error::Error for RequestError {}

/// The connection the terminals should use: a new `gen` is a new connection.
#[derive(Clone)]
pub(crate) struct Live {
    pub gen: u64,
    /// `None` while down.
    pub conn: Option<Arc<dyn Conn>>,
    /// The client is gone.
    pub stopped: bool,
}

/// What the client's tasks share.
pub(crate) struct Shared {
    pub quic: Option<(SocketAddr, [u8; 32])>,
    pub ssh_host: Option<String>,
    pub ssh_command: Option<tokio::process::Command>,
    pub device: DeviceIdentity,
    pub instance: u128,
    pub reprobe: Duration,
    st: Mutex<State>,
    /// Something for the connection task to look at: a request, a subscription, a `rebind`.
    pub wake: Notify,
    events: mpsc::UnboundedSender<Event>,
    pub live: watch::Sender<Live>,
}

impl Shared {
    /// The state behind its lock. Never hold the guard across an await.
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // ponytail: unbounded, because the connection task must never wait for the app (it reads the
    // sockets, answers requests and acks nothing on the app's behalf); the daemon bounds what it
    // sends (a client that falls behind is resynced from its cursor), so the queue is the app's
    // lag, not a leak. Bound it if an app ever stops draining.
    pub fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

/// Takes a request back when the caller stops waiting for it.
struct Withdraw<'a> {
    shared: &'a Shared,
    op: OpId,
}

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        self.shared.state().pending.remove(&self.op);
    }
}

/// The client of one daemon. Dropping it closes the connection and ends every [`Term`].
pub struct Client {
    shared: Arc<Shared>,
    events: Mutex<Option<mpsc::UnboundedReceiver<Event>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Client {
    /// Starts connecting and returns at once. Needs a tokio runtime to spawn on.
    pub fn start(cfg: Config) -> Client {
        let (events, events_rx) = mpsc::unbounded_channel();
        let (live, _) = watch::channel(Live {
            gen: 0,
            conn: None,
            stopped: false,
        });
        let shared = Arc::new(Shared {
            quic: cfg.quic,
            ssh_host: cfg.ssh_host,
            ssh_command: cfg.ssh_command,
            device: cfg.device,
            instance: cfg.client_instance.0,
            reprobe: cfg.quic_reprobe,
            st: Mutex::new(State::new(cfg.cursors)),
            wake: Notify::new(),
            events,
            live,
        });
        let task = tokio::spawn(conn::run(shared.clone()));
        Client {
            shared,
            events: Mutex::new(Some(events_rx)),
            task,
        }
    }

    /// The events, once: a second call gets a receiver that is already closed.
    pub fn events(&self) -> mpsc::UnboundedReceiver<Event> {
        let taken = self
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        taken.unwrap_or_else(|| {
            tracing::warn!("Client::events called twice: the second receiver is closed");
            mpsc::unbounded_channel().1
        })
    }

    /// Sends `req` and waits for the answer, up to 30 s. It is sent again, with the same op id, on
    /// every connection until it is answered, so a mutating request runs once. Use
    /// [`subscribe`](Client::subscribe) and [`unsubscribe`](Client::unsubscribe) for scopes.
    pub async fn request(&self, req: Req) -> Result<Resp, RequestError> {
        self.request_timeout(req, REQUEST_TIMEOUT).await
    }

    /// [`request`](Client::request) under an op id of the caller's choosing: the daemon keeps a
    /// prompt's op id with the message it stores, so the caller knows that message when it comes.
    pub async fn request_as(&self, op: OpId, req: Req) -> Result<Resp, RequestError> {
        if matches!(req, Req::Subscribe { .. } | Req::Unsubscribe { .. }) {
            return Err(RequestError::Server {
                code: ErrCode::Bad,
                msg: "scopes are subscribed with Client::subscribe".into(),
            });
        }
        self.call_as(op, Kind::Req(req), REQUEST_TIMEOUT).await
    }

    /// [`request`](Client::request) with a deadline of the caller's choosing.
    pub async fn request_timeout(&self, req: Req, limit: Duration) -> Result<Resp, RequestError> {
        if matches!(req, Req::Subscribe { .. } | Req::Unsubscribe { .. }) {
            return Err(RequestError::Server {
                code: ErrCode::Bad,
                msg: "scopes are subscribed with Client::subscribe".into(),
            });
        }
        self.call(Kind::Req(req), limit).await
    }

    /// Queues `kind`, wakes the connection task and waits for its answer.
    async fn call(&self, kind: Kind, limit: Duration) -> Result<Resp, RequestError> {
        self.call_as(OpId::new(), kind, limit).await
    }

    /// [`call`](Client::call) under `op`.
    async fn call_as(&self, op: OpId, kind: Kind, limit: Duration) -> Result<Resp, RequestError> {
        let (done, answer) = oneshot::channel();
        self.shared.state().add(op, kind, Some(done));
        self.shared.wake.notify_one();
        let _withdraw = Withdraw {
            shared: &self.shared,
            op,
        };
        match tokio::time::timeout(limit, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RequestError::Stopped),
            Err(_) => Err(RequestError::Timeout),
        }
    }

    /// Subscribes to `scope` and waits until the daemon has accepted it. The scope is remembered:
    /// after every reconnect it is subscribed again from the cursor of the moment. Rows come as
    /// [`Event::Changes`]. Already subscribed is `Ok` and nothing else.
    pub async fn subscribe(&self, scope: Scope) -> Result<(), RequestError> {
        {
            let mut st = self.shared.state();
            if st.subs.contains(&scope) {
                return Ok(());
            }
            st.subs.push(scope);
        }
        match self.call(Kind::Sub(scope), REQUEST_TIMEOUT).await {
            Ok(_) => Ok(()),
            Err(e) => {
                // Not subscribed after all: the caller was told so.
                self.shared.state().forget(scope);
                Err(e)
            }
        }
    }

    /// Stops receiving `scope` and forgets its cursor: drop the copy of the scope, a later
    /// [`subscribe`](Client::subscribe) starts from its full set. Returns once the daemon has
    /// confirmed (or the connection is down, when there is nothing to confirm), so a subscribe that
    /// follows is not overtaken by it.
    pub async fn unsubscribe(&self, scope: Scope) {
        let up = {
            let mut st = self.shared.state();
            st.forget(scope);
            st.conn.is_some()
        };
        if up {
            let _ = self.call(Kind::Unsub(scope), UNSUBSCRIBE_WAIT).await;
        }
    }

    /// Attaches `pane`: a screen to read frames from and input to send, which survives reconnects.
    pub async fn open_term(&self, pane: PaneId) -> Term {
        Term::open(self.shared.clone(), pane)
    }

    /// What the app holds, as of the last event the client queued.
    pub fn cursors(&self) -> Cursors {
        self.shared.state().cursors.clone()
    }

    /// The network may have changed (a wake from sleep, a new Wi-Fi). QUIC: moves to a fresh UDP
    /// socket (the connection and its streams survive) and sends a packet that makes the daemon
    /// answer. SSH: drops the connection and dials again at once. Down: stops waiting out the
    /// backoff and dials now.
    pub fn rebind(&self) {
        let conn = {
            let mut st = self.shared.state();
            let conn = st.conn.clone();
            // Down, or on SSH (dropped below): the next dial does not wait out the backoff.
            if conn.as_ref().is_none_or(|c| !c.is_quic()) {
                st.kick = true;
            }
            conn
        };
        match conn {
            None => self.shared.wake.notify_one(),
            Some(c) if c.is_quic() => {
                if let Err(e) = c.rebind() {
                    tracing::warn!(%e, "rebind failed");
                }
                // Ack-eliciting, so a dead new path is noticed by the transport's own timers.
                let _ = c.send_datagram(Bytes::from_static(&[0]));
            }
            Some(c) => c.close(),
        }
    }

    /// The app went out of sight (or came back). The mux pings every 15 s instead of every second
    /// while it is; QUIC fixes its keepalive when a connection is made, so a QUIC connection made
    /// while in the background gets the slow one and the transport ignores the call on a live one.
    pub fn set_background(&self, background: bool) {
        let conn = {
            let mut st = self.shared.state();
            st.background = background;
            st.conn.clone()
        };
        if let Some(c) = conn {
            c.set_background(background);
        }
    }

    /// The live connection's smoothed round trip, if there is one and it has been measured.
    pub fn rtt(&self) -> Option<Duration> {
        let conn = self.shared.state().conn.clone()?;
        Some(conn.rtt()).filter(|d| !d.is_zero())
    }

    /// The live connection's transport, `None` while down.
    pub fn link(&self) -> Option<Link> {
        self.shared.state().link
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
        let conn = {
            let mut st = self.shared.state();
            st.pending.clear();
            st.conn.take()
        };
        if let Some(c) = conn {
            c.close();
        }
        self.shared.live.send_replace(Live {
            gen: 0,
            conn: None,
            stopped: true,
        });
    }
}
