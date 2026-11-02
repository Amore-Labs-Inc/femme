//! Racing QUIC against SSH. The rules run on a paused clock with stand-in connections; what a daemon
//! that refuses a device does to them runs over real QUIC on loopback.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use femme_proto::Id;
use femme_transport::quic::Allow;
use femme_transport::race::{race, ConnFuture};
use femme_transport::{
    connect, connect_best, listen, Conn, DeviceIdentity, Error, Rx, ServerIdentity, StreamKind, Tx,
};
use tokio::sync::watch;
use tokio::time::{sleep, timeout, Instant};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A connection that is nothing but a label and a note of whether it was closed.
struct Fake {
    quic: bool,
    closed: watch::Sender<bool>,
}

impl Fake {
    fn new(quic: bool) -> Arc<Fake> {
        Arc::new(Fake {
            quic,
            closed: watch::channel(false).0,
        })
    }
}

#[async_trait]
impl Conn for Fake {
    async fn open(&self, _: StreamKind, _: i32) -> Result<(Tx, Rx), Error> {
        Err(Error::Closed("fake".into()))
    }
    async fn accept(&self) -> Result<(StreamKind, Tx, Rx), Error> {
        Err(Error::Closed("fake".into()))
    }
    fn send_datagram(&self, _: Bytes) -> Result<(), Error> {
        Err(Error::Closed("fake".into()))
    }
    async fn recv_datagram(&self) -> Result<Bytes, Error> {
        Err(Error::Closed("fake".into()))
    }
    fn rtt(&self) -> Duration {
        Duration::ZERO
    }
    fn is_quic(&self) -> bool {
        self.quic
    }
    fn close(&self) {
        self.closed.send_replace(true);
    }
    async fn closed(&self) -> Error {
        let _ = self.closed.subscribe().wait_for(|c| *c).await;
        Error::Closed("fake closed".into())
    }
}

/// An attempt that succeeds with `conn` after `delay`.
fn after(delay: Duration, conn: Arc<Fake>) -> ConnFuture {
    Box::pin(async move {
        sleep(delay).await;
        Ok(conn as Arc<dyn Conn>)
    })
}

/// An attempt that never finishes.
fn never() -> ConnFuture {
    Box::pin(std::future::pending::<Result<Arc<dyn Conn>, Error>>())
}

/// An attempt that fails with `e` after `delay`.
fn fails_after(delay: Duration, e: Error) -> ConnFuture {
    Box::pin(async move {
        sleep(delay).await;
        Err(e)
    })
}

#[tokio::test(start_paused = true)]
async fn one_transport_alone_is_used_as_is_and_none_is_an_error() {
    let best = race(Some(after(Duration::ZERO, Fake::new(true))), None)
        .await
        .unwrap();
    assert!(best.conn.is_quic() && best.quic_error.is_none());
    let best = race(None, Some(after(Duration::ZERO, Fake::new(false))))
        .await
        .unwrap();
    assert!(!best.conn.is_quic() && best.quic_error.is_none());
    assert!(matches!(race(None, None).await, Err(Error::Connect(_))));
    assert!(matches!(
        connect_best(None, None).await,
        Err(Error::Connect(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn a_quic_that_never_answers_costs_ssh_one_grace_period_and_is_not_an_error() {
    let started = Instant::now();
    let best = race(Some(never()), Some(after(Duration::ZERO, Fake::new(false))))
        .await
        .unwrap();
    assert!(!best.conn.is_quic());
    assert!(
        best.quic_error.is_none(),
        "slow is not failed: {:?}",
        best.quic_error
    );
    assert!(
        (ms(300)..ms(400)).contains(&started.elapsed()),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn quic_that_arrives_within_the_grace_period_beats_a_ready_ssh_and_the_loser_is_closed() {
    let ssh = Fake::new(false);
    let best = race(
        Some(after(ms(100), Fake::new(true))),
        Some(after(Duration::ZERO, ssh.clone())),
    )
    .await
    .unwrap();
    assert!(
        best.conn.is_quic(),
        "QUIC 100 ms behind SSH should still win"
    );
    assert!(matches!(
        timeout(Duration::from_secs(2), ssh.closed())
            .await
            .expect("the losing SSH connection stayed open"),
        Error::Closed(_)
    ));
}

#[tokio::test(start_paused = true)]
async fn ssh_wins_when_quic_is_slower_than_the_grace_period() {
    let started = Instant::now();
    let best = race(
        Some(after(ms(900), Fake::new(true))),
        Some(after(Duration::ZERO, Fake::new(false))),
    )
    .await
    .unwrap();
    assert!(!best.conn.is_quic());
    assert!(
        started.elapsed() < ms(800),
        "waited {:?} for a QUIC that was never going to make it",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn quic_first_wins_at_once_and_abandons_a_slow_ssh() {
    let started = Instant::now();
    let best = race(
        Some(after(Duration::ZERO, Fake::new(true))),
        Some(after(Duration::from_secs(30), Fake::new(false))),
    )
    .await
    .unwrap();
    assert!(best.conn.is_quic() && best.quic_error.is_none());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn when_one_transport_fails_the_other_is_used_and_a_quic_failure_comes_with_it() {
    let refused = || Error::Handshake("device revoked".into());
    // QUIC fails first: SSH is waited for, and QUIC's reason comes back with it.
    let best = race(
        Some(fails_after(Duration::ZERO, refused())),
        Some(after(ms(50), Fake::new(false))),
    )
    .await
    .unwrap();
    assert!(!best.conn.is_quic());
    assert!(
        matches!(&best.quic_error, Some(Error::Handshake(_))),
        "got {:?}",
        best.quic_error
    );

    // QUIC fails while SSH is ready and waiting in the grace period: the same answer at once, not a
    // silent "QUIC was slow".
    let started = Instant::now();
    let best = race(
        Some(fails_after(ms(100), refused())),
        Some(after(Duration::ZERO, Fake::new(false))),
    )
    .await
    .unwrap();
    assert!(!best.conn.is_quic());
    assert!(
        matches!(&best.quic_error, Some(Error::Handshake(_))),
        "got {:?}",
        best.quic_error
    );
    assert!(
        started.elapsed() < ms(200),
        "waited out the grace period for an answer that had come: {:?}",
        started.elapsed()
    );

    // SSH fails: QUIC alone, with nothing to report.
    let best = race(
        Some(after(ms(50), Fake::new(true))),
        Some(fails_after(
            Duration::ZERO,
            Error::Connect("ssh: no such host".into()),
        )),
    )
    .await
    .unwrap();
    assert!(best.conn.is_quic() && best.quic_error.is_none());
}

#[tokio::test(start_paused = true)]
async fn when_both_fail_the_error_names_both_and_keeps_a_handshake_refusal() {
    let err = race(
        Some(fails_after(
            Duration::ZERO,
            Error::Connect("udp blocked".into()),
        )),
        Some(fails_after(
            ms(10),
            Error::Connect("ssh: permission denied".into()),
        )),
    )
    .await
    .err()
    .unwrap();
    assert!(
        matches!(&err, Error::Connect(m) if m.contains("udp blocked") && m.contains("permission denied")),
        "got {err:?}"
    );

    let err = race(
        Some(fails_after(
            Duration::ZERO,
            Error::Handshake("device revoked".into()),
        )),
        Some(fails_after(ms(10), Error::Connect("ssh: timed out".into()))),
    )
    .await
    .err()
    .unwrap();
    assert!(
        matches!(&err, Error::Handshake(m) if m.contains("device revoked") && m.contains("timed out")),
        "got {err:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// a real daemon that says no (real time: the sockets are real)

struct Daemon {
    _listener: femme_transport::Listener,
    addr: std::net::SocketAddr,
    fp: [u8; 32],
    device: DeviceIdentity,
    /// Set to revoke the device.
    revoked: Arc<AtomicBool>,
}

async fn daemon() -> Daemon {
    let server = ServerIdentity::generate(&[]);
    let device = DeviceIdentity::generate("alice");
    let spki = device.spki_sha256().unwrap();
    let (id, revoked) = (Id::new(), Arc::new(AtomicBool::new(false)));
    let flag = revoked.clone();
    let allow: Allow =
        Arc::new(move |h| (*h == spki && !flag.load(Ordering::SeqCst)).then_some(id));
    let listener = listen("127.0.0.1:0".parse().unwrap(), &server, allow)
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    Daemon {
        _listener: listener,
        addr,
        fp: server.fingerprint(),
        device,
        revoked,
    }
}

#[tokio::test]
async fn a_refused_device_gets_ssh_and_the_reason_never_a_quic_connection() {
    let d = daemon().await;
    let stranger = DeviceIdentity::generate("mallory");
    let (addr, fp) = (d.addr, d.fp);
    let quic = || -> ConnFuture {
        let stranger = stranger.clone();
        Box::pin(async move { connect(addr, fp, &stranger).await })
    };
    // SSH is up at once: QUIC's refusal arrives inside the grace period.
    let best = race(Some(quic()), Some(after(Duration::ZERO, Fake::new(false))))
        .await
        .unwrap();
    assert!(
        !best.conn.is_quic(),
        "a refused device was handed a QUIC connection"
    );
    assert!(
        matches!(&best.quic_error, Some(Error::Handshake(_))),
        "got {:?}",
        best.quic_error
    );
    // SSH is slower than the refusal.
    let best = race(Some(quic()), Some(after(ms(500), Fake::new(false))))
        .await
        .unwrap();
    assert!(
        !best.conn.is_quic(),
        "a refused device was handed a QUIC connection"
    );
    assert!(
        matches!(&best.quic_error, Some(Error::Handshake(_))),
        "got {:?}",
        best.quic_error
    );
    // And with QUIC alone it is an error, not a connection that dies on first use.
    let err = connect_best(Some((addr, fp, stranger.clone())), None)
        .await
        .err()
        .expect("a refused device connected");
    assert!(matches!(err, Error::Handshake(_)), "got {err:?}");
}

#[tokio::test]
async fn a_revoked_device_never_gets_a_quic_connection_even_when_revocation_races_the_handshake() {
    let d = daemon().await;
    let conn = connect(d.addr, d.fp, &d.device)
        .await
        .expect("the enrolled device must connect");
    assert!(conn.is_quic());
    d.revoked.store(true, Ordering::SeqCst);
    let err = connect(d.addr, d.fp, &d.device)
        .await
        .err()
        .expect("a revoked device was handed a connection");
    assert!(matches!(err, Error::Handshake(_)), "got {err:?}");

    // The daemon asks `allow` twice per connection: in the TLS verifier and once more after the
    // handshake. A revocation between the two is the daemon closing the connection itself.
    let server = ServerIdentity::generate(&[]);
    let device = DeviceIdentity::generate("alice");
    let spki = device.spki_sha256().unwrap();
    let asked = Arc::new(AtomicUsize::new(0));
    let (id, n) = (Id::new(), asked.clone());
    let allow: Allow =
        Arc::new(move |h| (*h == spki && n.fetch_add(1, Ordering::SeqCst) == 0).then_some(id));
    let listener = listen("127.0.0.1:0".parse().unwrap(), &server, allow)
        .await
        .unwrap();
    let err = connect(
        listener.local_addr().unwrap(),
        server.fingerprint(),
        &device,
    )
    .await
    .err()
    .expect("a device revoked mid-handshake was handed a connection");
    assert!(matches!(err, Error::Handshake(_)), "got {err:?}");
}
