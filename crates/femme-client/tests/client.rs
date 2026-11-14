//! The client against the real front: a daemon in process around a real `femme-holder`, its front on
//! loopback (QUIC, and the unix socket behind `femmed proxy`), and a device enrolled by its key.
//! Needs the `femmed` and `femme-holder` binaries (`common` builds them when they are stale).

mod common;

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::Box_;
use femme_client::{Client, Config, Event, Link, RequestError, Term};
use femme_proto::rpc::{Cursors, ErrCode, Req, Resp, Scope};
use femme_proto::state::{Agent, Mode, Row};
use femme_proto::term::{TermDown, TermUp};
use femme_proto::{Id, PaneId, Sid};
use femme_store::auth;
use femme_term::client::RowCache;
use femme_transport::DeviceIdentity;
use femmed::core::{Core, CreateSession, PaneRef};
use femmed::daemon::Daemon;
use femmed::front::{serve_front_on, FrontHandle};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex, MutexGuard};
use ulid::Ulid;

/// The longest any one wait may take.
const WAIT: Duration = Duration::from_secs(20);

async fn within<T>(what: &str, f: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, f)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// One test at a time: what the timing tests measure is the client, not four tests and their shells
/// competing for the CPU.
static ONE_AT_A_TIME: Mutex<()> = Mutex::const_new(());

/// A daemon in a box with its front on loopback, and the device that may use it.
struct Rig {
    // Field order is drop order: the front and daemon go before the box that holds their files.
    front: Option<FrontHandle>,
    d: Arc<Daemon>,
    b: Box_,
    dev: DeviceIdentity,
    quic: SocketAddr,
    fp: [u8; 32],
    _turn: MutexGuard<'static, ()>,
}

impl Rig {
    async fn new() -> Rig {
        let turn = ONE_AT_A_TIME.lock().await;
        common::logs();
        let b = Box_::new();
        let d = b.daemon();
        let dev = DeviceIdentity::generate("laptop");
        let spki = dev.spki_sha256().expect("a generated certificate parses");
        d.store
            .write(|tx| tx.enroll_device("laptop", &spki, 1_000))
            .expect("enrolls");
        let front = serve_front_on(d.clone(), SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("the front starts");
        let (quic, fp) = (front.quic_addr(), front.fingerprint());
        Rig {
            front: Some(front),
            d,
            b,
            dev,
            quic,
            fp,
            _turn: turn,
        }
    }

    /// QUIC to the front, nothing else.
    fn config(&self) -> Config {
        let mut cfg = Config::new(self.dev.clone(), Ulid(0xC11E));
        cfg.quic = Some((self.quic, self.fp));
        cfg
    }

    /// The real `femmed proxy`, which finds the front's socket by `XDG_RUNTIME_DIR`.
    fn proxy(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&common::bins().0);
        cmd.arg("proxy")
            .env("HOME", self.b.home())
            .env("XDG_RUNTIME_DIR", self.b.runtime());
        cmd
    }

    fn start(&self, cfg: Config) -> (Client, Events) {
        let client = Client::start(cfg);
        let rx = client.events();
        (client, Events { rx, seen: vec![] })
    }

    /// The front goes: every connection closes, the UDP port and the socket are free.
    fn drop_front(&mut self) {
        self.front = None;
    }

    /// The front comes back on the same ports (the old one's UDP socket frees a moment after it is
    /// dropped); when it is up.
    async fn restart_front(&mut self) -> Instant {
        let t0 = Instant::now();
        loop {
            match serve_front_on(self.d.clone(), self.quic).await {
                Ok(front) => {
                    self.front = Some(front);
                    return Instant::now();
                }
                Err(_) if t0.elapsed() < WAIT => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(e) => panic!("the front does not come back: {e:#}"),
            }
        }
    }

    /// The front comes back on a port of its own and `relay` follows it: the client's address
    /// stays, the way a restarted daemon's does. Not on the old port, because quinn-proto
    /// (0.11.19) can leave a half-open connection behind that holds the old UDP socket for the
    /// 30 s idle timeout: a client that closed an attempt before the daemon had seen its Initial
    /// (the daemon was down) has the close as the first packet, and `handle_first_packet` never
    /// arms the close timer for it. A restarted daemon is a new process and frees the socket at once.
    async fn restart_front_behind(&mut self, relay: &Relay) {
        let any = SocketAddr::from(([127, 0, 0, 1], 0));
        let front = serve_front_on(self.d.clone(), any)
            .await
            .expect("the front comes back");
        relay.to(front.quic_addr().port());
        self.front = Some(front);
    }

    /// A shell session made through the core, outside any connection.
    async fn core_shell(&self) -> Sid {
        let made = self
            .d
            .create_session(CreateSession {
                cwd: self.b.home().display().to_string(),
                agent: Agent::Shell,
                mode: Mode::Tui,
                label: None,
                name: None,
                owner: "board".into(),
                model: None,
                resume: None,
            })
            .await
            .expect("a shell opens");
        made.sid
    }

    /// Waits until the holder's own screen of `sid` shows `needle`, asked through the core: what
    /// the shell did, whatever the client's connection heard of it.
    async fn holder_shows(&self, sid: Sid, needle: &str) {
        within(needle, async {
            loop {
                let found = self.d.terminals().await.unwrap();
                let s = found.iter().find(|s| s.sid == sid).expect("the session");
                let pane = PaneRef {
                    machine: s.machine.clone(),
                    handle: s.handle.clone(),
                    no: 1,
                };
                if self
                    .d
                    .screen(&pane, false)
                    .await
                    .unwrap()
                    .text
                    .contains(needle)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
    }

    /// A shell session made by the client's request.
    async fn shell(&self, c: &Client) -> Sid {
        match c.request(shell_req(&self.b.home())).await {
            Ok(Resp::Session(sid)) => sid,
            other => panic!("SessionCreate: {other:?}"),
        }
    }
}

fn shell_req(home: &Path) -> Req {
    Req::SessionCreate {
        cwd: home.display().to_string(),
        agent: Agent::Shell,
        mode: Mode::Tui,
        label: None,
        name: None,
        prompt: None,
        model: None,
        resume: None,
    }
}

fn has_session(rows: &[Row], sid: Sid) -> bool {
    rows.iter()
        .any(|r| matches!(r, Row::Session(s) if s.id == sid))
}

/// What the client says, in order; what a wait went past is kept for the failure message.
struct Events {
    rx: mpsc::UnboundedReceiver<Event>,
    seen: Vec<Event>,
}

impl Events {
    /// The next event `ok` accepts, within `limit`.
    async fn expect(&mut self, what: &str, limit: Duration, ok: impl Fn(&Event) -> bool) -> Event {
        let end = tokio::time::Instant::now() + limit;
        loop {
            match tokio::time::timeout_at(end, self.rx.recv()).await {
                Ok(Some(e)) => {
                    self.seen.push(e.clone());
                    if ok(&e) {
                        return e;
                    }
                }
                Ok(None) => panic!("the events ended waiting for {what}; saw {:#?}", self.seen),
                Err(_) => panic!("{limit:?} passed waiting for {what}; saw {:#?}", self.seen),
            }
        }
    }

    async fn connected(&mut self, link: Link, limit: Duration) -> Event {
        let what = format!("Connected over {link:?}");
        self.expect(
            &what,
            limit,
            |e| matches!(e, Event::Connected { link: l, .. } if *l == link),
        )
        .await
    }

    async fn disconnected(&mut self) {
        let down = |e: &Event| matches!(e, Event::Disconnected { .. });
        self.expect("Disconnected", WAIT, down).await;
    }

    /// The `Changes` of the global scope that carry the row of `sid`; its rev.
    async fn session_row(&mut self, sid: Sid) -> u64 {
        let ev = self
            .expect("the session's row", WAIT, |e| {
                matches!(e, Event::Changes { scope: Scope::Global, rows, .. } if has_session(rows, sid))
            })
            .await;
        match ev {
            Event::Changes { rev, .. } => rev,
            _ => unreachable!(),
        }
    }
}

/// A pane as the app keeps it: the client's `Term` and the screen its frames build.
struct Pane {
    term: Term,
    screen: RowCache,
    /// Frames that were a snapshot though the screen was held already: a replay.
    replays: usize,
}

impl Pane {
    fn new(term: Term) -> Pane {
        Pane {
            term,
            screen: RowCache::new(),
            replays: 0,
        }
    }

    /// Types `data` as raw bytes (the client numbers them).
    fn bytes(&self, data: impl AsRef<[u8]>) {
        let up = TermUp::Bytes {
            seq: 0,
            data: data.as_ref().to_vec(),
        };
        self.term.send(up).expect("input is taken");
    }

    /// Draws a message of the pane; a frame is acknowledged once applied, as the app does.
    fn take(&mut self, msg: TermDown) {
        match msg {
            TermDown::Frame(f) => {
                self.replays += usize::from(f.base_rev == 0 && self.screen.rev != 0);
                self.screen.apply(&f).expect("frames follow one another");
                self.term.applied(f.rev);
            }
            TermDown::Exited { status } => panic!("the pane exited ({status})"),
            _ => {}
        }
    }

    /// Frames until the screen satisfies `ok`.
    async fn until(&mut self, what: &str, ok: impl Fn(&RowCache) -> bool) {
        within(what, async {
            while !ok(&self.screen) {
                let msg = self.term.recv().await.expect("the pane ended");
                self.take(msg);
            }
        })
        .await;
    }

    /// Frames for `d`: the app drawing while the test does something else.
    async fn pump(&mut self, d: Duration) {
        let end = tokio::time::Instant::now() + d;
        while let Ok(msg) = tokio::time::timeout_at(end, self.term.recv()).await {
            self.take(msg.expect("the pane ended"));
        }
    }
}

/// A UDP relay in front of the front's QUIC port that can drop what goes toward the daemon, what
/// comes back, or both: a path that is dead, or one that only hears and cannot answer.
struct Relay {
    addr: SocketAddr,
    /// `[toward the daemon, back to the client]`: datagrams are dropped while set.
    cut: Arc<[AtomicBool; 2]>,
    /// The loopback port the daemon's front listens on now; 0 is nowhere (what is sent is lost).
    port: Arc<AtomicU16>,
    task: tokio::task::JoinHandle<()>,
}

impl Relay {
    async fn new(to: SocketAddr, cut: bool) -> Relay {
        let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = front.local_addr().unwrap();
        let cut = Arc::new([AtomicBool::new(cut), AtomicBool::new(cut)]);
        let port = Arc::new(AtomicU16::new(to.port()));
        let task = tokio::spawn({
            let (front, cut, port) = (front.clone(), cut.clone(), port.clone());
            async move {
                // One socket toward the daemon per client address, or its answers could not find
                // their way back. ponytail: the answer tasks end with the runtime, not with the
                // relay; fine for a test.
                let mut legs: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
                let mut buf = vec![0u8; 65536];
                while let Ok((n, from)) = front.recv_from(&mut buf).await {
                    if cut[0].load(Relaxed) {
                        continue;
                    }
                    let leg = legs.entry(from).or_insert_with(|| {
                        let std = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                        std.set_nonblocking(true).unwrap();
                        let leg = Arc::new(UdpSocket::from_std(std).unwrap());
                        let (back, front, cut) = (leg.clone(), front.clone(), cut.clone());
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 65536];
                            while let Ok((n, _)) = back.recv_from(&mut buf).await {
                                if !cut[1].load(Relaxed) {
                                    let _ = front.send_to(&buf[..n], from).await;
                                }
                            }
                        });
                        leg
                    });
                    let to = SocketAddr::from(([127, 0, 0, 1], port.load(Relaxed)));
                    if to.port() != 0 {
                        let _ = leg.send_to(&buf[..n], to).await;
                    }
                }
            }
        });
        Relay {
            addr,
            cut,
            port,
            task,
        }
    }

    /// Follows the front to `port` (0: it is gone, and what is sent meanwhile is lost).
    fn to(&self, port: u16) {
        self.port.store(port, Relaxed);
    }

    /// Drops what goes toward the daemon (`up`) and what comes back (`down`) while set.
    fn cut(&self, up: bool, down: bool) {
        self.cut[0].store(up, Relaxed);
        self.cut[1].store(down, Relaxed);
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// xorshift64: the fuzz needs a seed it can print, not a good generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

// ---- connecting and state -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_connects_over_quic_says_hello_and_sees_changes() {
    let rig = Rig::new().await;
    let (client, mut ev) = rig.start(rig.config());

    let Event::Connected {
        server_version,
        epoch,
        quic_error,
        ..
    } = ev.connected(Link::Quic, WAIT).await
    else {
        unreachable!()
    };
    assert_eq!(server_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(epoch, rig.d.store.epoch());
    assert_eq!(quic_error, None);
    assert_eq!(client.link(), Some(Link::Quic));

    client.subscribe(Scope::Global).await.expect("subscribes");
    let sid = rig.shell(&client).await;
    let rev = ev.session_row(sid).await;
    // The cursor moved before the app was told, and follows the database's epoch.
    let held = client.cursors();
    assert_eq!(held.epoch, epoch);
    assert!(held.global_rev >= Some(rev), "{held:?} is behind rev {rev}");

    // An error of the daemon's comes back as that error, not as a retry.
    let nobody = client.request(Req::SessionClose { sid: Id::new() }).await;
    assert!(
        matches!(
            nobody,
            Err(RequestError::Server {
                code: ErrCode::NotFound,
                ..
            })
        ),
        "{nobody:?}"
    );
}

/// A cursor of another database, and one the database has not reached: either way the app hears
/// `Reset` before the rows that replace what it holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cursor_the_daemon_cannot_honour_is_reset_and_the_full_set_follows() {
    let rig = Rig::new().await;
    let sid = rig.core_shell().await;
    let epoch = rig.d.store.epoch();
    let now = rig.d.store.rev().unwrap();
    let ahead = Cursors {
        epoch,
        global_rev: Some(now + 1_000),
        sessions: vec![],
    };
    let other = Cursors {
        epoch: epoch ^ 1,
        global_rev: Some(now),
        sessions: vec![],
    };
    for (what, cursors) in [("past the database", ahead), ("another database", other)] {
        let mut cfg = rig.config();
        cfg.cursors = cursors;
        let (client, mut ev) = rig.start(cfg);
        ev.connected(Link::Quic, WAIT).await;
        client.subscribe(Scope::Global).await.expect("subscribes");
        let rev = ev.session_row(sid).await;
        let at = |p: fn(&Event) -> bool| ev.seen.iter().position(p);
        let reset = at(|e| {
            matches!(
                e,
                Event::Reset {
                    scope: Scope::Global
                }
            )
        });
        let rows = at(|e| matches!(e, Event::Changes { .. }));
        assert!(reset.is_some() && reset < rows, "{what}: {:#?}", ev.seen);
        // The cursor is the database's now, not the one the app brought.
        let held = client.cursors();
        let db = rig.d.store.rev().unwrap();
        assert_eq!(held.epoch, epoch, "{what}");
        assert!(
            held.global_rev >= Some(rev) && held.global_rev <= Some(db),
            "{what}: {held:?} is not between rev {rev} and the database's {db}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_made_while_disconnected_is_answered_once_after_the_reconnect() {
    let mut rig = Rig::new().await;
    let (client, mut ev) = rig.start(rig.config());
    ev.connected(Link::Quic, WAIT).await;
    rig.drop_front();
    ev.disconnected().await;
    let req = shell_req(&rig.b.home());

    // A caller that gives up takes its request back: it is not sent when the daemon returns.
    let short = client
        .request_timeout(req.clone(), Duration::from_millis(200))
        .await;
    assert_eq!(short, Err(RequestError::Timeout));
    // One that waits is answered after the reconnect, and the daemon ran it once.
    let (answer, ()) = tokio::join!(client.request(req), async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        rig.restart_front().await;
    });
    let Ok(Resp::Session(sid)) = answer else {
        panic!("SessionCreate: {answer:?}")
    };
    ev.connected(Link::Quic, WAIT).await;
    let sessions: Vec<Sid> = rig
        .d
        .terminals()
        .await
        .unwrap()
        .iter()
        .map(|s| s.sid)
        .collect();
    assert_eq!(sessions, [sid]);
}

// ---- terminals ------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_resumes_after_a_lost_connection_without_replaying_bytes_and_input_is_applied_once(
) {
    let mut rig = Rig::new().await;
    let (client, mut ev) = rig.start(rig.config());
    ev.connected(Link::Quic, WAIT).await;
    let sid = rig.shell(&client).await;
    let mut pane = Pane::new(client.open_term(PaneId { sid, no: 1 }).await);
    pane.until("the first frame", |s| s.rev > 0).await;

    // Flow control: 12 commands 30 ms apart make a frame or more each, and with no `applied` the
    // holder stops after 8 of them; the screen is not up to date and stays so while it is quiet.
    for n in 0..12 {
        pane.bytes(format!("echo s-$((100+{n}))\r"));
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let (mut unacked, mut last) = (0, 0);
    while let Ok(msg) = tokio::time::timeout(Duration::from_millis(700), pane.term.recv()).await {
        let Some(TermDown::Frame(f)) = msg else {
            panic!("expected a frame, got {msg:?}")
        };
        pane.screen.apply(&f).expect("frames follow one another");
        (unacked, last) = (unacked + 1, f.rev);
    }
    assert!(unacked <= 8, "{unacked} frames came without an ack");
    assert!(
        !pane.screen.text().contains("s-111"),
        "the holder did not stop"
    );
    // Then `applied` lets it go on, and everything arrives.
    pane.term.applied(last);
    pane.until("the held-back output", |s| s.text().contains("s-111"))
        .await;

    // The connection goes with a command on its way; two more are typed with no connection at all.
    pane.bytes("echo m-$((10+1))\r");
    rig.drop_front();
    pane.bytes("echo m-$((10+2))\r");
    ev.disconnected().await;
    pane.bytes("echo m-$((10+3))\r");
    rig.restart_front().await;
    let all = ["m-11", "m-12", "m-13"];
    pane.until("the three commands", |s| {
        all.iter().all(|m| s.text().contains(m))
    })
    .await;
    pane.bytes("echo m-$((10+4))\r");
    pane.until("a command after the reconnect", |s| {
        s.text().contains("m-14")
    })
    .await;

    // Every marker once: nothing replayed, nothing run twice. And every frame since the first was a
    // delta of what the app held (`apply` refused anything that was not).
    let text = pane.screen.text();
    for m in ["m-11", "m-12", "m-13", "m-14", "s-111"] {
        assert_eq!(text.matches(m).count(), 1, "{m} on the screen:\n{text}");
    }
    assert_eq!(pane.replays, 0, "the screen was sent again");
    ev.connected(Link::Quic, WAIT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_fuzzing_leaves_pty_input_byte_exact() {
    let mut rig = Rig::new().await;
    // The front goes and comes back behind a relay: see `restart_front_behind`.
    let relay = Relay::new(rig.quic, false).await;
    let mut cfg = rig.config();
    cfg.quic = Some((relay.addr, rig.fp));
    let (client, mut ev) = rig.start(cfg);
    ev.connected(Link::Quic, WAIT).await;
    let sid = rig.shell(&client).await;
    let mut pane = Pane::new(client.open_term(PaneId { sid, no: 1 }).await);
    let file = rig.b.dir.path().join("typed.txt");
    pane.bytes(format!("cat > {}\r", file.display()));

    let seed = std::env::var("FEMME_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64
                | 1
        });
    eprintln!("fuzz seed {seed} (FEMME_FUZZ_SEED={seed} replays it)");
    let mut rng = Rng(seed);
    // 200 keystrokes, a line every 16, the last one ending a line: what `cat` has to write down.
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789 ";
    let typed: Vec<u8> = (0..200)
        .map(|i| match i % 16 == 15 || i == 199 {
            true => b'\n',
            false => alphabet[(rng.next() % alphabet.len() as u64) as usize],
        })
        .collect();
    // Five cuts, one per stretch of the typing, far enough apart for what is typed in the dark.
    let cuts: Vec<usize> = (0..5)
        .map(|n| 10 + n * 36 + (rng.next() % 30) as usize)
        .collect();
    eprintln!("the front goes after keystrokes {cuts:?}");

    let mut i = 0;
    while i < typed.len() {
        pane.bytes([typed[i]]);
        i += 1;
        if cuts.contains(&i) {
            // A burst, so that some keys are on the wire, run by the shell and not yet confirmed
            // when the front goes: anywhere from before the first left to after the last was
            // confirmed.
            for _ in 0..rng.next() % 4 {
                pane.bytes([typed[i]]);
                i += 1;
            }
            pane.pump(Duration::from_micros(rng.next() % 3_000)).await;
            rig.drop_front();
            relay.to(0);
            for _ in 0..rng.next() % 6 {
                pane.bytes([typed[i]]);
                i += 1;
                pane.pump(Duration::from_millis(20)).await;
            }
            pane.pump(Duration::from_millis(rng.next() % 300)).await;
            rig.restart_front_behind(&relay).await;
        }
        pane.pump(Duration::from_millis(3)).await;
    }
    // The end of the input, then a command that can only run once `cat` is gone.
    pane.bytes([4u8]);
    pane.bytes("echo done-$((1+2))\r");
    pane.until("the shell after cat", |s| s.text().contains("done-3"))
        .await;

    let got = std::fs::read(&file).expect("cat wrote the file");
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&typed),
        "the file is not what was typed (seed {seed})"
    );
    assert_eq!(pane.replays, 0, "the screen was sent again");
}

// ---- the transports -------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_blocked_falls_back_to_ssh_within_two_seconds_and_returns_to_quic_when_it_works() {
    let rig = Rig::new().await;
    // QUIC goes through a relay that starts out cutting every datagram: that path is dead, and the
    // daemon (and the unix socket behind `femmed proxy`) is fine.
    let relay = Relay::new(rig.quic, true).await;
    let mut cfg = rig.config();
    cfg.quic = Some((relay.addr, rig.fp));
    cfg.ssh_command = Some(rig.proxy());
    cfg.quic_reprobe = Duration::from_secs(1);
    let t0 = Instant::now();
    let (client, mut ev) = rig.start(cfg);

    ev.connected(Link::Ssh, Duration::from_secs(2)).await;
    eprintln!("udp blocked: connected over ssh after {:?}", t0.elapsed());
    assert_eq!(client.link(), Some(Link::Ssh));
    client
        .subscribe(Scope::Global)
        .await
        .expect("subscribes over ssh");

    // The path opens; the next probe finds it and everything moves over.
    let t1 = Instant::now();
    relay.cut(false, false);
    ev.connected(Link::Quic, Duration::from_secs(5)).await;
    eprintln!("udp unblocked: back on quic after {:?}", t1.elapsed());
    assert_eq!(client.link(), Some(Link::Quic));

    // The subscription went with it.
    let sid = rig.core_shell().await;
    ev.session_row(sid).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_unix_socket_is_the_local_link_and_needs_no_quic() {
    let rig = Rig::new().await;
    let mut cfg = rig.config();
    cfg.quic = None;
    cfg.unix = Some(rig.b.runtime().join("femme/femmed.sock"));
    let (client, mut ev) = rig.start(cfg);
    ev.connected(Link::Local, WAIT).await;
    assert_eq!(client.link(), Some(Link::Local));
    client
        .subscribe(Scope::Global)
        .await
        .expect("subscribes over the socket");
    let sid = rig.core_shell().await;
    ev.session_row(sid).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_device_is_reported_even_when_ssh_works() {
    let rig = Rig::new().await;
    let id = rig.d.store.read(auth::list_devices).unwrap()[0].id;
    rig.d.store.write(|tx| tx.revoke_device(id)).unwrap();
    let mut cfg = rig.config();
    cfg.ssh_command = Some(rig.proxy());
    let (client, mut ev) = rig.start(cfg);

    let Event::Connected {
        link, quic_error, ..
    } = ev
        .expect("Connected", WAIT, |e| matches!(e, Event::Connected { .. }))
        .await
    else {
        unreachable!()
    };
    assert_eq!(link, Link::Ssh);
    let why = quic_error.expect("the refusal is reported");
    eprintln!("refused device: {why}");
    assert!(!why.is_empty());
    // And ssh does the work.
    rig.shell(&client).await;
}

/// The daemon is away for 3 s (the front dropped, then started again): `Connected` follows within
/// 1.5 s of the new front being up. This is the dense end of the real thing: a daemon that is away
/// for 30 s is found again at the next redial, and the 5 s backoff cap bounds how late that is; the
/// netem run of `crates/femme-transport/scripts/netem.sh` (P2) is the measurement of loss itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thirty_seconds_of_loss_recover_within_one_and_a_half_seconds() {
    let mut rig = Rig::new().await;
    let (client, mut ev) = rig.start(rig.config());
    ev.connected(Link::Quic, WAIT).await;
    client.subscribe(Scope::Global).await.expect("subscribes");
    rig.drop_front();
    ev.disconnected().await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    let up = rig.restart_front().await;
    ev.connected(Link::Quic, WAIT).await;
    let back = up.elapsed();
    eprintln!("a 3 s outage: connected {back:?} after the front was up");
    assert!(back < Duration::from_millis(1500), "took {back:?}");

    // What was subscribed before is delivering again.
    let sid = rig.core_shell().await;
    ev.session_row(sid).await;
}

// ---- what the daemon did and could not say -----------------------------------------------------

/// The path hears the client and cannot answer: the daemon runs a request and writes an input to
/// the pty, and nothing comes back. The client finds the connection dead, connects again when the
/// path is back, and sends both again, the request with its op and the input with its seq. The
/// daemon answers the op from its record and the holder drops the input it already has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_the_daemon_did_but_could_not_say_is_not_done_again_after_the_reconnect() {
    let rig = Rig::new().await;
    let relay = Relay::new(rig.quic, false).await;
    let mut cfg = rig.config();
    cfg.quic = Some((relay.addr, rig.fp));
    let (client, mut ev) = rig.start(cfg);
    let client = Arc::new(client);
    ev.connected(Link::Quic, WAIT).await;
    let sid = rig.shell(&client).await;
    let mut pane = Pane::new(client.open_term(PaneId { sid, no: 1 }).await);
    pane.until("the first frame", |s| s.rev > 0).await;

    relay.cut(false, true);
    pane.bytes("echo once-$((1+1))\r");
    let asked = tokio::spawn({
        let (client, req) = (client.clone(), shell_req(&rig.b.home()));
        async move { client.request(req).await }
    });
    // Both got there: the shell ran the input, the daemon made the session. The client heard nothing.
    rig.holder_shows(sid, "once-2").await;
    within("the second session", async {
        while rig.d.terminals().await.unwrap().len() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(!pane.screen.text().contains("once-2"));
    assert!(!asked.is_finished());

    // The transport notices the silence; when the path is back the client is too.
    ev.disconnected().await;
    relay.cut(false, false);
    ev.connected(Link::Quic, WAIT).await;
    let answer = within("the answer", asked).await.unwrap();
    let Ok(Resp::Session(made)) = answer else {
        panic!("SessionCreate: {answer:?}")
    };
    pane.bytes("echo then-$((1+2))\r");
    pane.until("a command after the reconnect", |s| {
        s.text().contains("then-3")
    })
    .await;

    let sessions: Vec<Sid> = rig
        .d
        .terminals()
        .await
        .unwrap()
        .iter()
        .map(|s| s.sid)
        .collect();
    assert_eq!(sessions.len(), 2, "the request ran twice: {sessions:?}");
    assert!(sessions.contains(&made));
    let text = pane.screen.text();
    assert_eq!(
        text.matches("once-2").count(),
        1,
        "the input ran twice:\n{text}"
    );
    assert_eq!(pane.replays, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebind_keeps_a_quic_connection_and_restarts_an_ssh_one_at_once() {
    let rig = Rig::new().await;
    // QUIC moves to a new socket and the connection carries on.
    let (client, mut ev) = rig.start(rig.config());
    ev.connected(Link::Quic, WAIT).await;
    client.rebind();
    rig.shell(&client).await;
    while let Ok(e) = ev.rx.try_recv() {
        ev.seen.push(e);
    }
    assert!(
        !ev.seen
            .iter()
            .any(|e| matches!(e, Event::Disconnected { .. })),
        "{:#?}",
        ev.seen
    );
    drop(client);

    // SSH is dropped and dialed again without waiting out a backoff.
    let mut cfg = rig.config();
    cfg.quic = None;
    cfg.ssh_command = Some(rig.proxy());
    let (client, mut ev) = rig.start(cfg);
    ev.connected(Link::Ssh, WAIT).await;
    client.rebind();
    ev.disconnected().await;
    let t0 = Instant::now();
    ev.connected(Link::Ssh, WAIT).await;
    let back = t0.elapsed();
    eprintln!("rebind on ssh: connected {back:?} after Disconnected");
    assert!(back < Duration::from_secs(1), "took {back:?}");
}
