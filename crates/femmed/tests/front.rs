//! The desktop's front of `femmed`, with the desktop's side written out in the tests: a real QUIC
//! endpoint on loopback (and the unix socket behind `femmed proxy`), a device enrolled by its key,
//! and terminals relayed to a real `femme-holder`. Needs the `femme-holder` binary (`common` builds
//! it when it is missing).

mod common;

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::Duration;

use common::Box_;
use femme_proto::rpc::{
    Cursors, Err as WireErr, ErrCode, FsEntry, IssueField, Req, Resp, Scope, StreamKind, C2S, S2C,
};
use femme_proto::state::{Agent, Entity, Item, ItemKind, ItemStatus, Mode, PaneKind, Row};
use femme_proto::term::{Key, KeyCode, Mods, TermDown, TermUp};
use femme_proto::{Id, OpId, PaneId, Sid, PROTO_VERSION};
use femme_store::{auth, sessions};
use femme_term::client::RowCache;
use femme_transport::{
    command_connect, connect, mux_client, Conn, DeviceIdentity, Error, Rx, ServerIdentity, Tx,
};
use femmed::core::{Core, CreateSession};
use femmed::daemon::Daemon;
use femmed::front::{serve_front_with, FrontHandle, Waits};
use tokio::sync::RwLock;

/// The longest any one wait on the daemon may take.
const WAIT: Duration = Duration::from_secs(20);
/// This desktop process, as terminals know it across reconnects.
const INSTANCE: u128 = 0xFE33E;

async fn within<T>(what: &str, f: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, f)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Every test holds a read lock; the latency test takes the write lock, so it runs with the other
/// tests of this file (and their holders and shells) stopped, not competing for the CPU.
static QUIET: RwLock<()> = RwLock::const_new(());

/// Shell sessions run `$SHELL`: pin one that does not ask a new user questions in an empty home.
fn pin_shell() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| std::env::set_var("SHELL", "/bin/sh"));
}

/// A daemon in a box with its front on loopback.
struct Rig {
    // Field order is drop order: the front and daemon go before the box that holds their files.
    /// `None` only inside `restart_front`, between the old front going and the new one coming.
    front: Option<FrontHandle>,
    d: Arc<Daemon>,
    _stop: tokio::sync::oneshot::Sender<()>,
    b: Box_,
    _quiet: Box<dyn Send>,
}

async fn rig() -> Rig {
    Rig::new(false, Waits::default()).await
}

impl Rig {
    async fn new(alone: bool, waits: Waits) -> Rig {
        pin_shell();
        let quiet: Box<dyn Send> = if alone {
            Box::new(QUIET.write().await)
        } else {
            Box::new(QUIET.read().await)
        };
        let b = Box_::new();
        let (d, stop) = b.start(None).await;
        let front = serve_front_with(d.clone(), SocketAddr::from(([127, 0, 0, 1], 0)), waits)
            .await
            .expect("the front starts");
        Rig {
            front: Some(front),
            d,
            _stop: stop,
            b,
            _quiet: quiet,
        }
    }

    fn home(&self) -> PathBuf {
        self.b.home()
    }

    /// A device, enrolled the way `femmed enroll` does it.
    fn enroll(&self, name: &str) -> DeviceIdentity {
        let dev = DeviceIdentity::generate(name);
        let spki = dev.spki_sha256().expect("a generated certificate parses");
        self.d
            .store
            .write(|tx| tx.enroll_device(name, &spki, 1_000))
            .expect("enrolls");
        dev
    }

    async fn connect(&self, dev: &DeviceIdentity) -> Result<Arc<dyn Conn>, Error> {
        connect(self.front().quic_addr(), self.front().fingerprint(), dev).await
    }

    fn front(&self) -> &FrontHandle {
        self.front.as_ref().expect("the front is up")
    }

    /// The front stopped and started again on the same daemon, as a restart of the service would
    /// (less the daemon's own state): the old one's connections, socket and port go first.
    async fn restart_front(&mut self) {
        drop(self.front.take());
        let quic = SocketAddr::from(([127, 0, 0, 1], 0));
        let front = serve_front_with(self.d.clone(), quic, Waits::default()).await;
        self.front = Some(front.expect("the front starts again"));
    }

    /// A connection of `dev`, past `Hello`.
    async fn desktop(&self, dev: &DeviceIdentity, cursors: Cursors) -> Desktop {
        let conn = within("the connection", self.connect(dev))
            .await
            .expect("an enrolled device connects");
        Desktop::hello(conn, cursors).await
    }

    /// A shell session made through the core, outside any connection.
    async fn shell(&self) -> femmed::core::SessionInfo {
        self.d
            .create_session(CreateSession {
                cwd: self.home().display().to_string(),
                agent: Agent::Shell,
                mode: Mode::Tui,
                label: None,
                name: None,
                owner: "board".into(),
            })
            .await
            .expect("a shell opens")
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
    }
}

/// The desktop's end of one connection: the `ctrl` stream (past `Hello`) and the `events` stream
/// the daemon opened.
struct Desktop {
    conn: Arc<dyn Conn>,
    tx: Tx,
    rx: Rx,
    events: Rx,
    epoch: u64,
    _events_tx: Tx,
}

impl Desktop {
    async fn hello(conn: Arc<dyn Conn>, cursors: Cursors) -> Desktop {
        let (mut tx, mut rx) = conn.open_kind(StreamKind::Ctrl).await.expect("ctrl stream");
        let hello = C2S::Hello {
            proto: PROTO_VERSION,
            client_instance: INSTANCE,
            cursors,
        };
        tx.send(&hello).await.expect("sends Hello");
        let welcome = within("Welcome", rx.recv_msg::<S2C>())
            .await
            .expect("ctrl stream")
            .expect("the stream ended before Welcome");
        let S2C::Welcome {
            proto,
            server_version,
            epoch,
        } = welcome
        else {
            panic!("expected Welcome, got {welcome:?}")
        };
        assert_eq!(
            (proto, server_version.as_str()),
            (PROTO_VERSION, env!("CARGO_PKG_VERSION"))
        );
        let (kind, events_tx, events) = within("the events stream", conn.accept())
            .await
            .expect("accepts");
        assert_eq!(kind, StreamKind::Events);
        Desktop {
            conn,
            tx,
            rx,
            events,
            epoch,
            _events_tx: events_tx,
        }
    }

    async fn send(&mut self, op: OpId, req: Req) {
        self.tx
            .send(&C2S::Req { op, req })
            .await
            .expect("sends a request");
    }

    /// The next answer, whichever request it is for.
    async fn next_answer(&mut self) -> (OpId, Result<Resp, WireErr>) {
        match within("an answer", self.rx.recv_msg::<S2C>())
            .await
            .expect("ctrl stream")
        {
            Some(S2C::Resp { op, result }) => (op, result),
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    async fn call_op(&mut self, op: OpId, req: Req) -> Result<Resp, WireErr> {
        self.send(op, req).await;
        let (got, result) = self.next_answer().await;
        assert_eq!(got, op, "the answer is for another request");
        result
    }

    async fn call(&mut self, req: Req) -> Result<Resp, WireErr> {
        self.call_op(OpId::new(), req).await
    }

    /// The next `Changes` of the events stream.
    async fn changes(&mut self) -> (Scope, u64, Vec<Row>) {
        match within("changes", self.events.recv_msg::<S2C>())
            .await
            .expect("events stream")
        {
            Some(S2C::Changes {
                scope, rev, rows, ..
            }) => (scope, rev, rows),
            other => panic!("expected changes, got {other:?}"),
        }
    }

    async fn session(&mut self, home: &Path) -> Sid {
        match self.call(shell_req(home)).await {
            Ok(Resp::Session(sid)) => sid,
            other => panic!("SessionCreate: {other:?}"),
        }
    }

    /// Attaches `pane`, resuming from what `screen` holds.
    async fn term(&self, pane: PaneId, screen: RowCache) -> Term {
        let (mut tx, rx) = self
            .conn
            .open_kind(StreamKind::Term(pane))
            .await
            .expect("term stream");
        let open = TermUp::Open {
            pane,
            epoch: screen.epoch,
            have_rev: screen.rev,
            client_instance: INSTANCE,
        };
        tx.send(&open).await.expect("sends Open");
        Term {
            tx,
            rx,
            screen,
            seq: 0,
        }
    }
}

/// One attached pane, as the desktop sees it: its stream and the screen the frames build.
struct Term {
    tx: Tx,
    rx: Rx,
    screen: RowCache,
    /// Input sequence numbers, continued across reconnects.
    seq: u32,
}

impl Term {
    /// The next message; a frame is applied to the screen and acknowledged first.
    async fn next(&mut self) -> TermDown {
        let m = within("the terminal", self.rx.recv_msg::<TermDown>())
            .await
            .expect("term stream")
            .expect("the term stream ended");
        if let TermDown::Frame(f) = &m {
            self.screen.apply(f).expect("frames follow one another");
            self.tx
                .send(&TermUp::Ack { rev: f.rev })
                .await
                .expect("acks");
        }
        m
    }

    /// Frames until the screen satisfies `ok`.
    async fn until(&mut self, what: &str, ok: impl Fn(&RowCache) -> bool) {
        within(what, async {
            while !ok(&self.screen) {
                self.next().await;
            }
        })
        .await;
    }

    /// Types `data` as raw bytes with the next sequence number; returns it.
    async fn bytes(&mut self, data: &str) -> u32 {
        self.seq += 1;
        let m = TermUp::Bytes {
            seq: self.seq,
            data: data.as_bytes().to_vec(),
        };
        self.tx.send(&m).await.expect("sends input");
        self.seq
    }
}

fn session_ids(rows: &[Row]) -> Vec<Sid> {
    rows.iter()
        .filter_map(|r| match r {
            Row::Session(s) => Some(s.id),
            _ => None,
        })
        .collect()
}

// ---- who may connect --------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_enrolled_by_spki_connects_over_quic_says_hello_and_is_welcomed() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    // `desktop` checks the Welcome (this protocol, this build) and that the daemon opens `events`.
    let c = rig.desktop(&dev, Cursors::default()).await;
    assert_eq!(c.epoch, rig.d.store.epoch());
    assert!(c.conn.is_quic());

    // The fingerprint the front presents is the one saved for `femmed enroll` to print.
    let saved = ServerIdentity::load(&rig.b.dir.path().join("state/quic")).expect("identity saved");
    assert_eq!(saved.fingerprint(), rig.front().fingerprint());
    // It noted the device was here.
    let devices = rig.d.store.read(auth::list_devices).unwrap();
    assert!(devices[0].last_seen.is_some(), "{devices:?}");

    // A key nobody enrolled is refused in the handshake.
    let stranger = DeviceIdentity::generate("stranger");
    let refused = rig.connect(&stranger).await;
    assert!(
        matches!(refused, Err(Error::Handshake(_))),
        "an unenrolled device got {:?}",
        refused.map(|_| ())
    );
    // So is an enrolled one once it is revoked.
    let id = devices[0].id;
    rig.d.store.write(|tx| tx.revoke_device(id)).unwrap();
    let refused = rig.connect(&dev).await;
    assert!(
        matches!(refused, Err(Error::Handshake(_))),
        "a revoked device got {:?}",
        refused.map(|_| ())
    );
}

/// A connection has one request channel: another `ctrl` stream is refused, and the first goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_ctrl_stream_is_refused_and_the_first_one_goes_on() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let (mut tx, mut rx) = c
        .conn
        .open_kind(StreamKind::Ctrl)
        .await
        .expect("a stream opens");
    let req = Req::BlobGet { hash: [0; 32] };
    let _ = tx
        .send(&C2S::Req {
            op: OpId::new(),
            req: req.clone(),
        })
        .await;
    let got = within("the second stream's end", rx.recv_msg::<S2C>()).await;
    assert!(
        !matches!(got, Ok(Some(_))),
        "the second ctrl stream was served: {got:?}"
    );
    assert_eq!(c.call(req).await.unwrap_err().code, ErrCode::NotFound);
}

/// The hub keeps a client for each connection that got as far as `Welcome`, and gives it up
/// however the connection ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hub_forgets_a_client_however_its_connection_ends() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let hub = &rig.d.hub;
    let clients = |n: usize| {
        within("the hub's clients", async move {
            while hub.clients() != n {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };
    assert_eq!(hub.clients(), 0);

    // The desktop hangs up.
    let c = rig.desktop(&dev, Cursors::default()).await;
    clients(1).await;
    c.conn.close();
    clients(0).await;
    // It ends its request stream.
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    clients(1).await;
    c.tx.finish().await.unwrap();
    clients(0).await;
    // It breaks the protocol: on `ctrl`, after `Hello`, nothing but requests.
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    clients(1).await;
    let again = C2S::Hello {
        proto: PROTO_VERSION,
        client_instance: INSTANCE,
        cursors: Cursors::default(),
    };
    c.tx.send(&again).await.unwrap();
    clients(0).await;
}

/// Stopping the front closes what it had open and frees its socket and port; a new one on the same
/// daemon takes the desktop back, which resumes from the cursor it holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_front_stopped_and_started_again_takes_the_desktop_back_from_its_cursor() {
    let mut rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let all = Req::Subscribe {
        scope: Scope::Global,
        after: None,
    };
    assert_eq!(c.call(all).await, Ok(Resp::Ok));
    let (_, held, _) = c.changes().await;
    let epoch = c.epoch;
    let sock = rig.front().sock_path().to_path_buf();

    rig.restart_front().await;
    within("the old connection to close", c.conn.closed()).await;
    assert!(sock.exists(), "the new front has the socket back");
    within("the hub to forget the old client", async {
        while rig.d.hub.clients() != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;

    // Something happens while no desktop is there; it comes back with what it held.
    let s = rig.shell().await;
    let cursors = Cursors {
        epoch,
        global_rev: Some(held),
        sessions: vec![],
    };
    let mut again = rig.desktop(&dev, cursors).await;
    let (scope, _, rows) = again.changes().await;
    assert_eq!(scope, Scope::Global);
    assert!(rows.iter().all(|r| r.rev() > held), "{rows:?}");
    assert!(session_ids(&rows).contains(&s.sid));
}

// ---- requests ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_are_answered_by_op_id_and_a_repeated_op_is_not_run_twice() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let create = shell_req(&rig.home());
    let sessions = || async { rig.d.terminals().await.unwrap().len() };

    // The same op again is the same answer, and nothing runs.
    let op = OpId::new();
    let Ok(Resp::Session(sid)) = c.call_op(op, create.clone()).await else {
        panic!("SessionCreate failed")
    };
    assert_eq!(c.call_op(op, create.clone()).await, Ok(Resp::Session(sid)));
    assert_eq!(sessions().await, 1);
    // From a new connection too: the desktop reconnected without knowing whether it was heard.
    let mut again = rig.desktop(&dev, Cursors::default()).await;
    assert_eq!(
        again.call_op(op, create.clone()).await,
        Ok(Resp::Session(sid))
    );
    assert_eq!(sessions().await, 1);
    // A copy that arrives while the first is still running joins it.
    let op2 = OpId::new();
    c.send(op2, create.clone()).await;
    c.send(op2, create.clone()).await;
    let (first, second) = (c.next_answer().await, c.next_answer().await);
    assert_eq!((first.0, second.0), (op2, op2));
    assert_eq!(first.1, second.1);
    let Ok(Resp::Session(other)) = first.1 else {
        panic!("{first:?}")
    };
    assert_ne!(other, sid);
    assert_eq!(sessions().await, 2);

    // Answers carry their op, whatever order the requests finish in.
    let (slow, fast) = (OpId::new(), OpId::new());
    c.send(slow, create).await;
    c.send(fast, Req::BlobGet { hash: [0; 32] }).await;
    let mut got = HashMap::new();
    for _ in 0..2 {
        let (op, result) = c.next_answer().await;
        got.insert(op, result);
    }
    assert!(matches!(got[&slow], Ok(Resp::Session(_))), "{got:?}");
    assert_eq!(got[&fast].as_ref().unwrap_err().code, ErrCode::NotFound);
    assert_eq!(sessions().await, 3);

    // A protocol that is not this one (0 is none: there is nothing before 1) is told so, and the
    // connection ends.
    let hello = |proto| C2S::Hello {
        proto,
        client_instance: INSTANCE,
        cursors: Cursors::default(),
    };
    for proto in [0, PROTO_VERSION + 1] {
        let conn = within("the connection", rig.connect(&dev)).await.unwrap();
        let (mut tx, mut rx) = conn.open_kind(StreamKind::Ctrl).await.unwrap();
        tx.send(&hello(proto)).await.unwrap();
        let told = within("the refusal", rx.recv_msg::<S2C>()).await;
        let Ok(Some(S2C::Resp {
            result: Err(refusal),
            ..
        })) = told
        else {
            panic!("protocol {proto}: expected an Unsupported error, got {told:?}")
        };
        assert_eq!(
            refusal.code,
            ErrCode::Unsupported,
            "protocol {proto}: {refusal:?}"
        );
        assert!(matches!(rx.recv_msg::<S2C>().await, Ok(None)));
        conn.close();
    }
    // The one before this is welcome, once there is one.
    if let Some(previous) = PROTO_VERSION.checked_sub(1).filter(|v| *v > 0) {
        let conn = within("the connection", rig.connect(&dev)).await.unwrap();
        let (mut tx, mut rx) = conn.open_kind(StreamKind::Ctrl).await.unwrap();
        tx.send(&hello(previous)).await.unwrap();
        let welcome = within("Welcome", rx.recv_msg::<S2C>()).await;
        assert!(
            matches!(welcome, Ok(Some(S2C::Welcome { proto, .. })) if proto == previous),
            "{welcome:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_keep_their_meaning_and_what_is_not_built_says_so() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let code = |r: Result<Resp, WireErr>| r.expect_err("an error").code;
    let nobody = Id::new();

    assert_eq!(
        code(c.call(Req::SessionClose { sid: nobody }).await),
        ErrCode::NotFound
    );
    let req = Req::SessionCreate {
        cwd: "/no/such/folder".into(),
        agent: Agent::Shell,
        mode: Mode::Tui,
        label: None,
        name: None,
        prompt: None,
    };
    assert_eq!(code(c.call(req).await), ErrCode::Bad);
    // A shell has no agent to prompt: the core's Busy.
    let sid = c.session(&rig.home()).await;
    let prompt = |attachments| Req::Prompt {
        sid,
        text: "hi".into(),
        attachments,
        how: femme_proto::rpc::PromptHow::Queue,
    };
    assert_eq!(code(c.call(prompt(vec![])).await), ErrCode::Busy);
    let attached = vec![femme_proto::rpc::Attachment {
        name: "a".into(),
        blob: [0; 32],
    }];
    assert_eq!(code(c.call(prompt(attached)).await), ErrCode::Unsupported);
    // M5.
    let upload = Req::Upload {
        name: "a".into(),
        len: 1,
    };
    assert_eq!(code(c.call(upload).await), ErrCode::Unsupported);
    // Replayed, an error is the same error.
    let op = OpId::new();
    let first = c.call_op(op, Req::SessionClose { sid: nobody }).await;
    assert_eq!(
        c.call_op(op, Req::SessionClose { sid: nobody }).await,
        first
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_over_one_mebibyte_is_refused_and_the_global_scope_stays_subscribable() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let create = |body: String| Req::IssueCreate {
        repo: "o/r".into(),
        title: "t".into(),
        body,
    };
    // The board answers 413 there. Two of 9 MiB once got in, and their backlog passed the frame
    // limit: the connection died and every reconnect hit the same message.
    for _ in 0..2 {
        let refused = c.call(create("x".repeat(9 << 20))).await;
        assert_eq!(refused.unwrap_err().code, ErrCode::Bad);
    }
    let fits = c.call(create("x".repeat((1 << 20) - 200))).await;
    assert!(matches!(fits, Ok(Resp::Issue(_))), "{fits:?}");
    c.conn.close();
    drop(c);
    for _ in 0..2 {
        let mut c = rig.desktop(&dev, Cursors::default()).await;
        let all = Req::Subscribe {
            scope: Scope::Global,
            after: None,
        };
        assert_eq!(c.call(all).await, Ok(Resp::Ok));
        let (scope, _, rows) = c.changes().await;
        assert_eq!(scope, Scope::Global);
        assert_eq!(
            rows.iter().filter(|r| matches!(r, Row::Issue(_))).count(),
            1
        );
        c.conn.close();
    }
}

/// The board refuses, before anything reaches the store or the scheduler, what would end up on a
/// command line or under an agent's name (`compat::util`). The front says no to the same things,
/// and keeps what it takes the way the board keeps it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_front_checks_repos_titles_names_and_folders_the_way_the_board_does() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let mut tried: Vec<(String, Result<Resp, WireErr>)> = Vec::new();

    for (repo, title) in [
        ("--upload-pack=x/y", "t"),
        ("o/r", ""),
        ("o/r", "  "),
        ("", ""),
    ] {
        let r = c
            .call(Req::IssueCreate {
                repo: repo.into(),
                title: title.into(),
                body: String::new(),
            })
            .await;
        tried.push((format!("IssueCreate repo={repo:?} title={title:?}"), r));
    }
    let Ok(Resp::Issue(id)) = c
        .call(Req::IssueCreate {
            repo: "o/r".into(),
            title: "  t ".into(),
            body: " b\n".into(),
        })
        .await
    else {
        panic!("IssueCreate o/r failed")
    };
    let issue = || async {
        let all = rig.d.issues().await.unwrap();
        all.into_iter().find(|i| i.id == id).unwrap()
    };
    let made = issue().await;
    assert_eq!((made.title.as_str(), made.body.as_str()), ("t", "b"));

    // One bad field refuses the whole patch: the title that came with it is not applied.
    for (what, field) in [
        ("a repo with a space", IssueField::Repo("a b/c".into())),
        (
            "a repo that is an option",
            IssueField::Repo("--upload-pack=x/y".into()),
        ),
        ("a session link", IssueField::Session(Some(Id::new()))),
        ("an unlinked session", IssueField::Session(None)),
        ("resume_at before 1970", IssueField::ResumeAt(Some(-1))),
        (
            "resume_at past the year 9999",
            IssueField::ResumeAt(Some(253_402_300_800_000)),
        ),
    ] {
        let patch = Req::IssuePatch {
            id,
            base_rev: 0,
            fields: vec![IssueField::Title("changed".into()), field],
        };
        tried.push((format!("IssuePatch with {what}"), c.call(patch).await));
    }
    assert_eq!(issue().await, made, "a refused patch changed the issue");
    let kept = Req::IssuePatch {
        id,
        base_rev: 0,
        fields: vec![
            IssueField::Title(" new ".into()),
            IssueField::Note(Some("  ".into())),
            IssueField::PrUrl(Some(" https://x ".into())),
            IssueField::ResumeAt(Some(1_700_000_000_000)),
        ],
    };
    assert_eq!(c.call(kept).await, Ok(Resp::Issue(id)));
    let patched = issue().await;
    assert_eq!(
        (
            patched.title.as_str(),
            patched.note.as_deref(),
            patched.pr_url.as_deref(),
            patched.resume_at
        ),
        ("new", None, Some("https://x"), Some(1_700_000_000_000))
    );

    // A shell under an agent's name: /api/agents/bob would type the agent's text into it.
    let session = |cwd: &str, agent, label: Option<&str>, name: Option<&str>| Req::SessionCreate {
        cwd: cwd.into(),
        agent,
        mode: Mode::Tui,
        label: label.map(Into::into),
        name: name.map(Into::into),
        prompt: None,
    };
    let home = rig.home().display().to_string();
    for (what, req) in [
        (
            "a shell named bob",
            session(&home, Agent::Shell, None, Some("bob")),
        ),
        (
            "a name with capitals",
            session(&home, Agent::Claude, None, Some("Bob")),
        ),
        (
            "a name that is an option",
            session(&home, Agent::Claude, None, Some("-x")),
        ),
        (
            "a name of 33 characters",
            session(&home, Agent::Claude, None, Some(&"a".repeat(33))),
        ),
        // Relative: it would be the daemon's own working directory.
        ("the folder .", session(".", Agent::Shell, None, None)),
        (
            "a relative folder",
            session("some/where", Agent::Shell, None, None),
        ),
    ] {
        tried.push((format!("SessionCreate {what}"), c.call(req).await));
    }
    let accepted: Vec<_> = tried
        .iter()
        .filter(|(_, r)| !matches!(r, Err(e) if e.code == ErrCode::Bad))
        .collect();
    assert!(accepted.is_empty(), "{accepted:#?}");
    assert!(
        rig.d.terminals().await.unwrap().is_empty(),
        "a refused session was made"
    );

    // The label is trimmed and bounded the way the board's terminals do it.
    let wide = format!("  {}  ", "é".repeat(80));
    let Ok(Resp::Session(sid)) = c
        .call(session(&home, Agent::Shell, Some(&wide), None))
        .await
    else {
        panic!("SessionCreate with a long label failed")
    };
    let label = rig
        .d
        .store
        .read(|c| sessions::get_session(c, sid))
        .unwrap()
        .unwrap()
        .label;
    assert_eq!(label, Some("é".repeat(60)));
}

fn issue_req(title: &str) -> Req {
    Req::IssueCreate {
        repo: "o/r".into(),
        title: title.into(),
        body: String::new(),
    }
}

/// The op id is the retry's key, so it has to be one request's: a second request under it was once
/// answered with the first one's answer, a success for something that never ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_op_id_used_for_another_request_is_refused_not_answered_with_the_first_answer() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let (op, other) = (OpId::new(), Req::IssueDelete { id: 987_654 });
    let made = c.call_op(op, issue_req("t")).await;
    assert!(matches!(made, Ok(Resp::Issue(_))), "{made:?}");
    // The same request again is the retry: the same answer.
    assert_eq!(c.call_op(op, issue_req("t")).await, made);
    // Another one is refused, on this connection and (the row is in the database) on a new one.
    let refused = c.call_op(op, other.clone()).await;
    assert_eq!(refused.unwrap_err().code, ErrCode::Bad);
    let mut again = rig.desktop(&dev, Cursors::default()).await;
    let refused = again.call_op(op, other).await;
    assert_eq!(refused.unwrap_err().code, ErrCode::Bad);

    // Two different requests under one op, sent together: one runs, the other is refused.
    let op = OpId::new();
    c.send(op, issue_req("a")).await;
    c.send(op, issue_req("b")).await;
    let answers = [c.next_answer().await.1, c.next_answer().await.1];
    let ran = answers
        .iter()
        .filter(|r| matches!(r, Ok(Resp::Issue(_))))
        .count();
    let refused = answers
        .iter()
        .filter(|r| matches!(r, Err(e) if e.code == ErrCode::Bad))
        .count();
    assert_eq!((ran, refused), (1, 1), "{answers:?}");
    assert_eq!(rig.d.issues().await.unwrap().len(), 2, "t and one of a, b");
}

/// The nil id is what an answer to no request carries (the refusal of a `Hello`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_under_the_nil_op_id_is_refused() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    for req in [issue_req("t"), Req::BlobGet { hash: [0; 32] }] {
        let refused = c.call_op(Id(0), req).await;
        assert_eq!(refused.unwrap_err().code, ErrCode::Bad);
    }
    assert!(rig.d.issues().await.unwrap().is_empty());
}

/// "Not now" answers (`Busy`, `Internal`) are not the op's answer: the retry has to run it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_internal_failure_is_not_remembered_so_the_retry_runs_the_op_again() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let (claude, away) = (
        rig.b.dir.path().join("bin/claude"),
        rig.b.dir.path().join("claude.away"),
    );
    let req = Req::SessionCreate {
        cwd: rig.home().display().to_string(),
        agent: Agent::Claude,
        mode: Mode::Tui,
        label: None,
        name: None,
        prompt: None,
    };
    // No claude on the box's PATH: the core's Internal.
    std::fs::rename(&claude, &away).unwrap();
    let op = OpId::new();
    let failed = c.call_op(op, req.clone()).await;
    assert_eq!(failed.unwrap_err().code, ErrCode::Internal);
    let device = rig.d.store.read(auth::list_devices).unwrap()[0].id;
    let row = rig
        .d
        .store
        .read(|c| auth::op_lookup(c, device, op))
        .unwrap();
    assert_eq!(row, None, "the failure was recorded as the op's answer");
    // The box is mended; the same op, retried, runs.
    std::fs::rename(&away, &claude).unwrap();
    let again = c.call_op(op, req).await;
    assert!(matches!(again, Ok(Resp::Session(_))), "{again:?}");
}

/// A session that was made but could not be prompted is told as a `Conflict`, which is
/// remembered: a retry gets the same answer and no second session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_made_but_not_prompted_is_one_session_however_often_the_op_is_retried() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let req = Req::SessionCreate {
        cwd: rig.home().display().to_string(),
        agent: Agent::Shell, // a shell has no agent to prompt
        mode: Mode::Tui,
        label: None,
        name: None,
        prompt: Some("hello".into()),
    };
    let op = OpId::new();
    let told = c.call_op(op, req.clone()).await.unwrap_err();
    assert_eq!(told.code, ErrCode::Conflict, "{told:?}");
    assert!(
        told.msg
            .contains("was created but its prompt was not delivered"),
        "{told:?}"
    );
    assert_eq!(c.call_op(op, req).await.unwrap_err(), told);
    assert_eq!(rig.d.terminals().await.unwrap().len(), 1);
}

// ---- who may open a terminal ------------------------------------------------------------------

/// The Term stream of a connection that is not welcomed ends without a frame.
async fn assert_no_terminal(rx: &mut Rx) {
    let got = within("the stream's end", rx.recv_msg::<TermDown>()).await;
    assert!(
        !matches!(got, Ok(Some(_))),
        "a terminal was served to a connection that is not welcomed: {got:?}"
    );
}

fn open(pane: PaneId) -> TermUp {
    TermUp::Open {
        pane,
        epoch: 0,
        have_rev: 0,
        client_instance: INSTANCE,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_stream_on_a_connection_that_never_said_hello_gets_no_terminal() {
    let waits = Waits {
        hello: Duration::from_millis(300),
        ..Waits::default()
    };
    let rig = Rig::new(false, waits).await;
    let dev = rig.enroll("laptop");
    let s = rig.shell().await;
    let pane = PaneId { sid: s.sid, no: 1 };
    let conn = within("the connection", rig.connect(&dev)).await.unwrap();
    let (mut tx, mut rx) = conn.open_kind(StreamKind::Term(pane)).await.unwrap();
    tx.send(&open(pane)).await.unwrap();
    assert_no_terminal(&mut rx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_stream_after_a_refused_hello_is_reset_while_the_refusal_is_read() {
    let waits = Waits {
        hello: Duration::from_millis(300),
        ..Waits::default()
    };
    let rig = Rig::new(false, waits).await;
    let dev = rig.enroll("laptop");
    let s = rig.shell().await;
    let pane = PaneId { sid: s.sid, no: 1 };
    let conn = within("the connection", rig.connect(&dev)).await.unwrap();
    let (mut ctx, mut crx) = conn.open_kind(StreamKind::Ctrl).await.unwrap();
    let hello = C2S::Hello {
        proto: PROTO_VERSION + 7,
        client_instance: INSTANCE,
        cursors: Cursors::default(),
    };
    ctx.send(&hello).await.unwrap();
    let told = within("the refusal", crx.recv_msg::<S2C>()).await;
    assert!(
        matches!(told, Ok(Some(S2C::Resp { result: Err(_), .. }))),
        "{told:?}"
    );
    // The connection lingers for the client to read why (the ctrl stream is still open), and in
    // that time the terminal is refused by the stream itself.
    let (mut tx, mut rx) = conn.open_kind(StreamKind::Term(pane)).await.unwrap();
    tx.send(&open(pane)).await.unwrap();
    assert_no_terminal(&mut rx).await;
    let open_still = tokio::time::timeout(Duration::from_millis(50), conn.closed()).await;
    assert!(open_still.is_err(), "the connection closed with the stream");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_that_does_not_say_hello_in_time_is_closed() {
    let waits = Waits {
        hello: Duration::from_millis(300),
        ..Waits::default()
    };
    let rig = Rig::new(false, waits).await;
    let dev = rig.enroll("laptop");
    // Not even a ctrl stream.
    let idle = within("the connection", rig.connect(&dev)).await.unwrap();
    // A ctrl stream that stays silent.
    let silent = within("the connection", rig.connect(&dev)).await.unwrap();
    let (_tx, _rx) = silent.open_kind(StreamKind::Ctrl).await.unwrap();
    for conn in [&idle, &silent] {
        within("the connection to be closed", conn.closed()).await;
    }
}

// ---- state ------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribing_with_a_cursor_delivers_only_what_changed_since_it() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let (s1, s2) = (rig.shell().await, rig.shell().await);
    let mut c = rig.desktop(&dev, Cursors::default()).await;

    // No cursor: everything. (A Hello without cursors subscribes to nothing, so it asks.)
    let all = Req::Subscribe {
        scope: Scope::Global,
        after: None,
    };
    assert_eq!(c.call(all).await, Ok(Resp::Ok));
    let (scope, _, rows) = c.changes().await;
    assert_eq!(scope, Scope::Global);
    let ids = session_ids(&rows);
    assert!(ids.contains(&s1.sid) && ids.contains(&s2.sid), "{ids:?}");
    let epoch = c.epoch;
    c.conn.close();
    drop(c);

    // What it holds is the store's rev now; one more session happens while it is away.
    let held = rig.d.store.rev().unwrap();
    let s3 = rig.shell().await;

    // Back with a cursor and no request: only the rows newer than it, s3 among them.
    let cursor = Cursors {
        epoch,
        global_rev: Some(held),
        sessions: vec![],
    };
    let mut c = rig.desktop(&dev, cursor).await;
    let (scope, rev, rows) = c.changes().await;
    assert_eq!(scope, Scope::Global);
    assert!(rows.iter().all(|r| r.rev() > held), "{rows:?}");
    assert!(session_ids(&rows).contains(&s3.sid));
    assert!(rev > held);
    c.conn.close();
    drop(c);

    // Another epoch is another database: the cursor means nothing, everything comes.
    let stale = Cursors {
        epoch: epoch ^ 1,
        global_rev: Some(rev),
        sessions: vec![],
    };
    let mut c = rig.desktop(&dev, stale).await;
    let (_, _, rows) = c.changes().await;
    let ids = session_ids(&rows);
    assert!(
        [s1.sid, s2.sid, s3.sid].iter().all(|s| ids.contains(s)),
        "{ids:?}"
    );
    c.conn.close();
    drop(c);

    // A session's transcript resumes the same way, by its own cursor.
    let put = |n: u64| {
        rig.d
            .store
            .write(|tx| tx.put_item(&item(s1.sid, n, "hello")))
            .unwrap()
    };
    let (first, second) = (put(1), put(2));
    let cursor = Cursors {
        epoch,
        global_rev: None,
        sessions: vec![(s1.sid, first.rev)],
    };
    let mut c = rig.desktop(&dev, cursor).await;
    let (scope, _, rows) = c.changes().await;
    assert_eq!(scope, Scope::Session(s1.sid));
    assert_eq!(rows, vec![Row::Item(second)]);
}

fn item(sid: Sid, n: u64, body: &str) -> Item {
    Item {
        sid,
        item_id: format!("i{n}"),
        seq: 0,
        rev: 0,
        kind: ItemKind::UserMessage,
        status: ItemStatus::Completed,
        preview: String::new(),
        body: Some(body.into()),
        blob_hash: None,
        ts: n as i64,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transcript_pages_come_newest_first_and_a_blob_comes_whole() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let s = rig.shell().await;
    let big = "x".repeat(10_000); // past what an item keeps inline: the body is a blob
    let stored: Vec<Item> = rig
        .d
        .store
        .write(|tx| {
            (1..=5)
                .map(|n| tx.put_item(&item(s.sid, n, if n == 5 { &big } else { "short" })))
                .collect()
        })
        .unwrap();
    let mut c = rig.desktop(&dev, Cursors::default()).await;

    let page = |before_seq, limit| Req::TranscriptPage {
        sid: s.sid,
        before_seq,
        limit,
    };
    let seqs = |r: Result<Resp, WireErr>| match r {
        Ok(Resp::Page { items, next_before }) => {
            (items.iter().map(|i| i.seq).collect::<Vec<_>>(), next_before)
        }
        other => panic!("{other:?}"),
    };
    // From the end (0, or the highest there is), two at a time, until there is no next page.
    assert_eq!(seqs(c.call(page(0, 2)).await), (vec![5, 4], Some(4)));
    assert_eq!(seqs(c.call(page(u64::MAX, 2)).await), (vec![5, 4], Some(4)));
    assert_eq!(seqs(c.call(page(4, 2)).await), (vec![3, 2], Some(2)));
    assert_eq!(seqs(c.call(page(2, 2)).await), (vec![1], None));
    // Asking for none gets one: a page is never empty while there is more.
    assert_eq!(seqs(c.call(page(0, 0)).await), (vec![5], Some(5)));
    let missing = Req::TranscriptPage {
        sid: Id::new(),
        before_seq: 0,
        limit: 10,
    };
    assert_eq!(c.call(missing).await.unwrap_err().code, ErrCode::NotFound);

    // The big one's body is a blob the desktop fetches by hash.
    let hash = stored[4].blob_hash.expect("the body went to a blob");
    assert_eq!(
        c.call(Req::BlobGet { hash }).await,
        Ok(Resp::Blob(big.into_bytes()))
    );
    let gone = c.call(Req::BlobGet { hash: [7; 32] }).await;
    assert_eq!(gone.unwrap_err().code, ErrCode::NotFound);
}

/// A message of the events stream as `(rev, revs of its rows, reset)`.
fn shape(m: &S2C) -> (u64, Vec<u64>, bool) {
    match m {
        S2C::Changes {
            rev, rows, reset, ..
        } => (*rev, rows.iter().map(Row::rev).collect(), *reset),
        other => panic!("expected changes, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_from_a_cursor_the_database_has_not_reached_is_told_to_reset() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let s = rig.shell().await;
    let epoch = rig.d.store.epoch();
    let now = rig.d.store.rev().unwrap();

    // A cursor that holds: the rows after it, none here, and no reset.
    let cursor = Cursors {
        epoch,
        global_rev: Some(now),
        sessions: vec![],
    };
    let mut c = rig.desktop(&dev, cursor).await;
    let got = within("the resume", c.events.recv_msg::<S2C>()).await;
    let (_, rows, reset) = shape(&got.unwrap().unwrap());
    assert!(rows.is_empty() && !reset, "{rows:?} {reset}");
    c.conn.close();
    drop(c);

    // One the database is behind (a restored backup): the full set, marked, to drop what it holds.
    let cursor = Cursors {
        epoch,
        global_rev: Some(now + 1_000),
        sessions: vec![],
    };
    let mut c = rig.desktop(&dev, cursor).await;
    let got = within("the resume", c.events.recv_msg::<S2C>()).await;
    let Some(S2C::Changes {
        scope: Scope::Global,
        rev,
        rows,
        reset: true,
    }) = got.unwrap()
    else {
        panic!("expected a reset of the global scope")
    };
    assert!(session_ids(&rows).contains(&s.sid), "{rows:?}");
    assert_eq!(
        rev,
        rig.d.store.rev().unwrap(),
        "the cursor is the database's"
    );
}

/// A live change committed while a subscription's backlog is being read (a long query) is sent
/// after the backlog, not ahead of it with a higher rev: a client that takes `rev` as its cursor
/// and drops there would resume past rows it never got.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_change_during_the_backlog_fetch_does_not_overtake_it() {
    use femme_proto::state::{Issue, IssueStatus};
    use femmed::hub::{Backlog, Hub};
    let issue = |id: u64, rev: u64| {
        Row::Issue(Issue {
            id,
            rev,
            repo: "o/r".into(),
            title: format!("t{rev}"),
            body: String::new(),
            status: IssueStatus::Open,
            pr_url: None,
            note: None,
            session: None,
            resume_at: None,
            created: 0,
            updated: 0,
        })
    };
    let rows = Arc::new(std::sync::Mutex::new(vec![issue(11, 11), issue(12, 12)]));
    let hub = Hub::new(Arc::new({
        let rows = rows.clone();
        move |_scope, after| {
            let after = after.unwrap_or(0);
            // The snapshot is taken first, then the query takes its time: what is committed
            // meanwhile is not in it.
            let got: Vec<Row> = rows
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.rev() > after)
                .cloned()
                .collect();
            std::thread::sleep(Duration::from_millis(300));
            let rev = got.iter().map(Row::rev).max().unwrap_or(after);
            Ok(Backlog {
                rows: got,
                rev,
                reset: false,
            })
        }
    }));
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let id = hub.add_client(tx);
    let h = hub.clone();
    let sub = tokio::spawn(async move { h.subscribe(id, Scope::Global, Some(10)).await });
    tokio::time::sleep(Duration::from_millis(100)).await; // inside the query
    rows.lock().unwrap().push(issue(13, 13));
    hub.publish(vec![issue(13, 13)]);
    let first = within("the backlog", rx.recv()).await.unwrap();
    let second = within("the change", rx.recv()).await.unwrap();
    assert_eq!(shape(&first), (12, vec![11, 12], false));
    assert_eq!(shape(&second), (13, vec![13], false));
    assert_eq!(sub.await.unwrap().unwrap(), 13);
    // Caught up, so the next change goes out live.
    hub.publish(vec![issue(14, 14)]);
    let third = within("the live change", rx.recv()).await.unwrap();
    assert_eq!(shape(&third), (14, vec![14], false));
}

/// Two issues too big for one frame between them (each is under the 1 MiB the board took; these
/// came by import): the backlog goes in messages by size, so the global scope still subscribes.
/// As one message it passed the frame limit, the connection died, and every reconnect did again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_issues_too_big_for_one_frame_do_not_lock_the_desktop_out_of_the_global_scope() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let body = "x".repeat(9 << 20);
    for _ in 0..2 {
        rig.d.issue_create("o/r", "t", &body).await.unwrap();
    }
    for _ in 0..2 {
        let mut c = rig.desktop(&dev, Cursors::default()).await;
        let all = Req::Subscribe {
            scope: Scope::Global,
            after: None,
        };
        // The events are read before the answer: 18 MiB the desktop is not reading yet must not
        // be what the answer waits behind.
        let op = OpId::new();
        c.send(op, all).await;
        let mut issues = 0;
        while issues < 2 {
            let (_, _, rows) = c.changes().await;
            issues += rows.iter().filter(|r| matches!(r, Row::Issue(_))).count();
        }
        assert_eq!(c.next_answer().await, (op, Ok(Resp::Ok)));
        c.conn.close();
    }
}

/// A row can be bigger than a frame (these came by import, or from a bug): its message cannot be
/// sent, but the connection must not die of it, and everything else still arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_event_too_big_for_a_frame_is_skipped_and_the_connection_goes_on() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    rig.d
        .issue_create("o/r", "too big", &"x".repeat(17 << 20))
        .await
        .unwrap();
    rig.d.issue_create("o/r", "small", "b").await.unwrap();
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let all = Req::Subscribe {
        scope: Scope::Global,
        after: None,
    };
    let op = OpId::new();
    c.send(op, all).await;
    let (_, _, rows) = c.changes().await;
    let titles: Vec<&str> = rows
        .iter()
        .filter_map(|r| match r {
            Row::Issue(i) => Some(i.title.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(titles, ["small"]);
    assert_eq!(c.next_answer().await, (op, Ok(Resp::Ok)));
    // And the connection still serves, and pushes.
    let sid = c.session(&rig.home()).await;
    while !session_ids(&c.changes().await.2).contains(&sid) {}
}

/// One cursor for a session the daemon no longer has (retention purged it) must not stop the
/// resume of the scopes after it; the desktop is told to drop the gone one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_goes_on_past_a_cursor_for_a_purged_session() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let s = rig.shell().await;
    rig.d
        .store
        .write(|tx| tx.put_item(&item(s.sid, 1, "hello")))
        .unwrap();
    let epoch = rig.d.store.epoch();
    let gone = Id::new();
    let cursor = Cursors {
        epoch,
        global_rev: None,
        sessions: vec![(gone, 0), (s.sid, 0)],
    };
    let mut c = rig.desktop(&dev, cursor).await;
    let first = within("the gone scope", c.events.recv_msg::<S2C>())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&first, S2C::Changes { scope, .. } if *scope == Scope::Session(gone)),
        "{first:?}"
    );
    assert_eq!(shape(&first), (0, vec![], true));
    let (scope, _, rows) = c.changes().await;
    assert_eq!(scope, Scope::Session(s.sid));
    assert!(rows.iter().any(|r| matches!(r, Row::Item(_))), "{rows:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_an_unsubscribe_a_scope_sends_nothing_until_it_is_subscribed_again() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let all = |after| Req::Subscribe {
        scope: Scope::Global,
        after,
    };
    assert_eq!(c.call(all(None)).await, Ok(Resp::Ok));
    let (_, held, _) = c.changes().await;
    let off = Req::Unsubscribe {
        scope: Scope::Global,
    };
    assert_eq!(c.call(off).await, Ok(Resp::Ok));

    // A change the desktop must not hear of. The hub coalesces for 50 ms: this is many of them.
    let s = rig.shell().await;
    let quiet = tokio::time::timeout(Duration::from_millis(500), c.events.recv_msg::<S2C>()).await;
    assert!(
        quiet.is_err(),
        "a change was sent after Unsubscribe: {quiet:?}"
    );
    // Subscribed again from where it was, it gets what it missed.
    assert_eq!(c.call(all(Some(held))).await, Ok(Resp::Ok));
    let (_, _, rows) = c.changes().await;
    assert!(session_ids(&rows).contains(&s.sid), "{rows:?}");
    // Leaving what was never joined is nothing.
    let never = Req::Unsubscribe {
        scope: Scope::Session(Id::new()),
    };
    assert_eq!(c.call(never).await, Ok(Resp::Ok));
}

// ---- terminals --------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_stream_carries_keys_in_and_frames_out() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let sid = c.session(&rig.home()).await;
    let mut t = c.term(PaneId { sid, no: 1 }, RowCache::new()).await;

    // The first frame is the whole screen.
    t.until("a first frame", |s| s.rev > 0).await;
    t.bytes("echo marker-$((40+2))\r").await;
    t.until("the output of the command", |s| {
        s.text().contains("marker-42")
    })
    .await;

    // A resize is a frame of the new size.
    t.tx.send(&TermUp::Resize { cols: 90, rows: 25 })
        .await
        .unwrap();
    t.until("a frame of the new size", |s| (s.cols, s.rows) == (90, 25))
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_connection_and_a_reopen_with_have_rev_resume_the_screen_without_replaying_bytes()
{
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let sid = c.session(&rig.home()).await;
    let pane = PaneId { sid, no: 1 };
    let mut t = c.term(pane, RowCache::new()).await;
    let first = t.bytes("echo first-$((1+1))\r").await;
    t.until("the first command's output", |s| {
        s.text().contains("first-2")
    })
    .await;
    let (screen, seq) = (t.screen.clone(), t.seq);

    // The connection goes away, and the screen changes while the desktop is not looking.
    c.conn.close();
    drop((t, c));
    let info = common::wait_for(&rig.d, sid, "the session", |s| s.is_some()).await;
    let behind = common::pane_of(&info.unwrap());
    rig.d
        .input(&behind, b"echo second-$((2+1))\r")
        .await
        .unwrap();
    common::wait_screen(&rig.d, &behind, "second-3").await;

    // A new connection reopens the pane with the rev it holds: what comes is the difference.
    let c = rig.desktop(&dev, Cursors::default()).await;
    let mut t = c.term(pane, screen.clone()).await;
    t.seq = seq;
    let TermDown::Frame(delta) = t.next().await else {
        panic!("the first message is a frame")
    };
    assert_eq!(
        delta.base_rev, screen.rev,
        "a delta from what the desktop held, not a snapshot"
    );
    assert_eq!(delta.epoch, screen.epoch);
    t.until("the second command's output", |s| {
        s.text().contains("second-3")
    })
    .await;
    let text = t.screen.text();
    assert_eq!(text.matches("first-2").count(), 1, "{text}");

    // Input the holder already applied is not applied twice when the desktop resends it.
    t.seq = first - 1;
    t.bytes("echo first-$((1+1))\r").await; // the first seq again
    t.seq = seq;
    let third = t.bytes("echo third-$((1+2))\r").await;
    t.until("the third command's output", |s| {
        s.text().contains("third-3")
    })
    .await;
    assert!(t.screen.applied_seq >= third);
    let text = t.screen.text();
    assert_eq!(
        text.matches("first-2").count(),
        1,
        "resent input ran again:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closed_session_ends_its_terminal_streams_with_exited() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let sid = c.session(&rig.home()).await;
    let mut t = c.term(PaneId { sid, no: 1 }, RowCache::new()).await;
    t.until("a first frame", |s| s.rev > 0).await;

    assert_eq!(c.call(Req::SessionClose { sid }).await, Ok(Resp::Ok));
    let exited = within("Exited", async {
        loop {
            if let TermDown::Exited { status } = t.next().await {
                break status;
            }
        }
    })
    .await;
    let _ = exited; // the holder's word for how the shell went, or -1 when it was cut off
    assert!(
        matches!(
            within("the end of the stream", t.rx.recv_msg::<TermDown>()).await,
            Ok(None)
        ),
        "nothing follows Exited"
    );

    // A pane that is not there is the same: Exited, then the end.
    for pane in [
        PaneId { sid, no: 1 }, // the session is closed
        PaneId {
            sid: Id::new(),
            no: 1,
        }, // there never was one
    ] {
        let mut t = c.term(pane, RowCache::new()).await;
        assert_eq!(t.next().await, TermDown::Exited { status: -1 }, "{pane:?}");
        assert!(matches!(t.rx.recv_msg::<TermDown>().await, Ok(None)));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pane_opens_next_to_the_agents_and_closes_again() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let sid = c.session(&rig.home()).await;

    let Ok(Resp::Pane(pane)) = c.call(Req::PaneOpen { sid }).await else {
        panic!("PaneOpen failed")
    };
    assert_eq!(pane, PaneId { sid, no: 2 });
    let panes = rig.d.store.read(|c| sessions::panes(c, sid)).unwrap();
    assert_eq!(
        panes.iter().map(|p| (p.id.no, p.kind)).collect::<Vec<_>>(),
        [(1, PaneKind::Shell), (2, PaneKind::Shell)]
    );
    // It is a shell like any other.
    let mut t = c.term(pane, RowCache::new()).await;
    t.bytes("echo pane-$((20+2))\r").await;
    t.until("the shell's answer", |s| s.text().contains("pane-22"))
        .await;

    // The session's own pane is closed by closing the session; a pane nobody opened is not there.
    let own = Req::PaneClose {
        pane: PaneId { sid, no: 1 },
    };
    assert_eq!(c.call(own).await.unwrap_err().code, ErrCode::Bad);
    let none = Req::PaneClose {
        pane: PaneId { sid, no: 9 },
    };
    assert_eq!(c.call(none).await.unwrap_err().code, ErrCode::NotFound);
    assert_eq!(c.call(Req::PaneClose { pane }).await, Ok(Resp::Ok));
    let panes = rig.d.store.read(|c| sessions::panes(c, sid)).unwrap();
    assert_eq!(panes.len(), 1);
    let mut again = c.term(pane, RowCache::new()).await;
    assert_eq!(again.next().await, TermDown::Exited { status: -1 });
}

/// Connecting to a holder takes up to 5 s when it does not answer. That wait is its own session's:
/// every other session's terminals open as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holder_that_does_not_answer_stalls_no_other_sessions_terminals() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let healthy = c.session(&rig.home()).await;
    // Warm: the healthy holder's connection is already kept.
    let mut warm = c
        .term(
            PaneId {
                sid: healthy,
                no: 1,
            },
            RowCache::new(),
        )
        .await;
    warm.until("a first frame", |s| s.rev > 0).await;

    // A holder that accepts and never answers Hello.
    let stuck = Id::new();
    let path = rig.b.socket(stuck);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let _hold = tokio::spawn(async move {
        let mut keep = vec![];
        while let Ok((s, _)) = listener.accept().await {
            keep.push(s);
        }
    });
    let _stuck_term = c.term(PaneId { sid: stuck, no: 1 }, RowCache::new()).await;
    tokio::time::sleep(Duration::from_millis(300)).await; // its connect is under way

    let t0 = std::time::Instant::now();
    let mut t = c
        .term(
            PaneId {
                sid: healthy,
                no: 1,
            },
            RowCache::new(),
        )
        .await;
    t.until("a first frame", |s| s.rev > 0).await;
    let waited = t0.elapsed();
    assert!(
        waited < Duration::from_secs(2),
        "a healthy pane waited {waited:?} behind a holder that does not answer"
    );
}

/// The handshake checks a device once; a revoke after that has to reach the connections it
/// already has, not only the next one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_device_revoked_while_connected_is_disconnected_at_the_next_check() {
    let waits = Waits {
        recheck: Duration::from_millis(200),
        ..Waits::default()
    };
    let rig = Rig::new(false, waits).await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    // Enrolled, several checks go by and it is served.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let blob = c.call(Req::BlobGet { hash: [0; 32] }).await;
    assert_eq!(blob.unwrap_err().code, ErrCode::NotFound);

    let id = rig.d.store.read(auth::list_devices).unwrap()[0].id;
    rig.d.store.write(|tx| tx.revoke_device(id)).unwrap();
    within("the connection to close", c.conn.closed()).await;
}

/// A shell the desktop opened ends when its user types `exit`: its row goes too, and the desktop is
/// told, instead of a pane that is dead but listed for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shell_pane_that_exits_on_its_own_loses_its_row_and_the_desktop_hears_of_it() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let sid = c.session(&rig.home()).await;
    let Ok(Resp::Pane(pane)) = c.call(Req::PaneOpen { sid }).await else {
        panic!("PaneOpen failed")
    };
    let all = Req::Subscribe {
        scope: Scope::Global,
        after: None,
    };
    assert_eq!(c.call(all).await, Ok(Resp::Ok));
    let (_, _, rows) = c.changes().await;
    assert!(rows
        .iter()
        .any(|r| matches!(r, Row::Pane(p) if p.id == pane)));

    let mut t = c.term(pane, RowCache::new()).await;
    t.until("a first frame", |s| s.rev > 0).await;
    t.bytes("exit\r").await;
    within("the pane's tombstone", async {
        loop {
            let (_, _, rows) = c.changes().await;
            if rows.iter().any(|r| {
                matches!(
                    r,
                    Row::Tombstone {
                        entity: Entity::Pane,
                        ..
                    }
                )
            }) {
                break;
            }
        }
    })
    .await;
    let panes = rig.d.store.read(|c| sessions::panes(c, sid)).unwrap();
    assert_eq!(panes.iter().map(|p| p.id.no).collect::<Vec<_>>(), [1]);
    // It is a pane that is not there now, like any other.
    let closed = c.call(Req::PaneClose { pane }).await;
    assert_eq!(closed.unwrap_err().code, ErrCode::NotFound);
}

/// What the daemon holds for a terminal stream, a socket to the pane's holder, goes when the stream
/// does, and a closed connection's go with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_stream_and_a_connection_give_back_their_holder_sockets_when_they_end() {
    fn open_files() -> usize {
        std::fs::read_dir("/proc/self/fd").unwrap().count()
    }
    async fn back_to(at_most: usize) {
        within("the sockets to close", async {
            while open_files() > at_most {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
    }
    // Alone: the process's open files are then this test's own.
    let rig = Rig::new(true, Waits::default()).await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let sid = c.session(&rig.home()).await;
    let pane = PaneId { sid, no: 1 };

    // Warm: the daemon keeps one connection to the holder, whatever the streams do.
    let mut t = c.term(pane, RowCache::new()).await;
    t.until("a first frame", |s| s.rev > 0).await;
    t.tx.finish().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let base = open_files();

    for _ in 0..10 {
        let mut t = c.term(pane, RowCache::new()).await;
        t.until("a first frame", |s| s.rev > 0).await;
        t.tx.finish().await.unwrap();
    }
    back_to(base + 2).await;

    // Three views at once hold three sockets; the connection ending takes them all.
    let mut views = Vec::new();
    for _ in 0..3 {
        let mut t = c.term(pane, RowCache::new()).await;
        t.until("a first frame", |s| s.rev > 0).await;
        views.push(t);
    }
    assert!(open_files() >= base + 3, "the count does not see the views");
    c.conn.close();
    back_to(base + 2).await;
}

// ---- the SSH fallback ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ssh_fallback_speaks_the_same_protocol_over_the_unix_socket() {
    let rig = rig().await;
    let sock = rig.front().sock_path().to_path_buf();
    // Only the user may get in: a private folder and a private socket.
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!((mode(&sock), mode(sock.parent().unwrap())), (0o600, 0o700));

    // Straight to the socket, the way `femmed proxy` relays to it.
    let stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
    let (r, w) = stream.into_split();
    let mut c = Desktop::hello(mux_client(r, w), Cursors::default()).await;
    assert!(!c.conn.is_quic());
    let sid = c.session(&rig.home()).await;
    let owners: Vec<String> = rig
        .d
        .terminals()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.owner)
        .collect();
    assert_eq!(owners, ["device:ssh"]);
    // Terminals ride the mux as well.
    let mut t = c.term(PaneId { sid, no: 1 }, RowCache::new()).await;
    t.bytes("echo ssh-$((1+2))\r").await;
    t.until("the shell's answer", |s| s.text().contains("ssh-3"))
        .await;

    // And through the real `femmed proxy`, which finds the socket by `XDG_RUNTIME_DIR`.
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_femmed"));
    cmd.arg("proxy")
        .env_clear()
        .env("HOME", rig.home())
        .env("XDG_RUNTIME_DIR", rig.b.dir.path().join("rt"));
    let conn = within("femmed proxy", command_connect(cmd))
        .await
        .expect("the proxy connects");
    let mut p = Desktop::hello(conn, Cursors::default()).await;
    let page = Req::TranscriptPage {
        sid,
        before_seq: 0,
        limit: 10,
    };
    assert_eq!(
        p.call(page).await,
        Ok(Resp::Page {
            items: vec![],
            next_before: None
        })
    );
    // The same op from the other connection is the same answer: both are the one ssh user.
    let op = OpId::new();
    let made = c.call_op(op, shell_req(&rig.home())).await;
    assert!(matches!(made, Ok(Resp::Session(_))));
    assert_eq!(p.call_op(op, shell_req(&rig.home())).await, made);
}

// ---- how long the daemon holds a keystroke ------------------------------------------------------

/// The plan's "the daemon adds under 3 ms to a keystroke", as far as it can be measured from here.
/// The daemon has two edges, the desktop's QUIC stream and the holder's socket. The first is
/// stamped where the bytes arrive and leave; the second is not reachable: the socket's legs run in
/// the task `Holder::attach` spawns (holder_client.rs), which stamps nothing, so `up` and `down`
/// stop at its channels. `key` runs from a key read off the stream to the frame that echoes it
/// written back to it, so the holder's socket, the holder, the shell and the scheduler are all
/// inside it: it over-states what the daemon adds. Its median is held to 3 ms (a delay the daemon
/// added to every key would show there); its p99 and max are printed, not asserted: on a 4-core
/// box with builds running the p99 reached 5 ms over a median of 0.65 ms, which is the machine's
/// noise, not the daemon's. Not in any of them: the network (loopback here) and the desktop's own
/// time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keystroke_relay_adds_under_three_milliseconds_in_the_daemon() {
    let rig = Rig::new(true, Waits::default()).await;
    let dev = rig.enroll("laptop");
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let sid = c.session(&rig.home()).await;
    let mut t = c.term(PaneId { sid, no: 1 }, RowCache::new()).await;
    t.until("a first frame", |s| s.rev > 0).await;

    // One key at a time, each waited for until its echo is on the screen: 'a' and Backspace,
    // so the line never grows.
    for i in 0..500u32 {
        let code = if i % 2 == 0 {
            KeyCode::Char('a')
        } else {
            KeyCode::Backspace
        };
        t.seq += 1;
        let seq = t.seq;
        let key = Key {
            code,
            mods: Mods::empty(),
        };
        t.tx.send(&TermUp::Key { seq, key }).await.unwrap();
        t.until("the key's echo", |s| s.echo_seq >= seq).await;
    }

    let s = rig.front().stats();
    // p50, p99, p99.9 and max of each, in microseconds.
    println!("daemon hops over 500 keys on loopback QUIC:\n{s}");
    println!(
        "everything this process relayed:\n{}",
        femmed::front::stats()
    );
    assert!(s.up.n >= 500 && s.down.n >= 500 && s.key.n >= 500, "{s}");
    let limit = Duration::from_millis(3);
    assert!(s.up.p99 < limit, "keys into the holder: {s}");
    assert!(s.down.p99 < limit, "frames out to the desktop: {s}");
    assert!(
        s.key.p50 < limit,
        "a key to its echo, holder and shell included: {s}"
    );
}

// ---- the command line ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enroll_lists_and_revokes_a_device_from_the_command_line() {
    let rig = rig().await;
    // The command the user runs over ssh, in the daemon's own environment.
    let femmed = |args: &[&str]| {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_femmed"));
        cmd.args(args)
            .env_clear()
            .env("HOME", rig.home())
            .env("FEMME_STATE", rig.b.dir.path().join("state"))
            .env("QUIC_PORT", rig.front().quic_addr().port().to_string())
            .env("HOST", "agentbox");
        cmd
    };
    let ok = |out: std::process::Output| {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let dev = DeviceIdentity::generate("laptop");
    let spki = femme_transport::fp_to_hex(&dev.spki_sha256().unwrap());

    // One line, what the desktop pins: the running daemon's fingerprint, its port and the host.
    let line = ok(femmed(&["enroll", "--name", "laptop", "--spki", &spki])
        .output()
        .await
        .unwrap());
    let want = format!(
        "fingerprint={} quic_port={} host=agentbox\n",
        femme_transport::fp_to_hex(&rig.front().fingerprint()),
        rig.front().quic_addr().port()
    );
    assert_eq!(line, want);
    // The daemon let it in without a restart.
    let c = rig.desktop(&dev, Cursors::default()).await;
    drop(c);
    let list = ok(femmed(&["devices"]).output().await.unwrap());
    assert!(
        list.contains("laptop") && list.contains(&spki) && !list.contains("REVOKED"),
        "{list}"
    );

    // Revoked by name, it is refused; enrolling the same key again lets it back in.
    ok(femmed(&["devices", "revoke", "laptop"])
        .output()
        .await
        .unwrap());
    assert!(matches!(rig.connect(&dev).await, Err(Error::Handshake(_))));
    let list = ok(femmed(&["devices", "list"]).output().await.unwrap());
    assert!(list.contains("REVOKED"), "{list}");
    ok(femmed(&["enroll", "--name", "laptop", "--spki", &spki])
        .output()
        .await
        .unwrap());
    drop(rig.desktop(&dev, Cursors::default()).await);

    // A key that is not a hash, and a device that is not there, say so and fail.
    for args in [
        &["enroll", "--name", "x", "--spki", "not-a-hash"][..],
        &["devices", "revoke", "nobody"][..],
    ] {
        let out = femmed(args).output().await.unwrap();
        assert!(!out.status.success(), "{args:?}");
        assert!(!out.stderr.is_empty(), "{args:?}");
    }
}

/// The files of a session's folder, for the desktop's file panel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sessions_folder_is_listed_and_its_files_read_relative_to_it() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let proj = rig.home().join("proj");
    std::fs::create_dir_all(proj.join("sub/deep")).unwrap();
    std::fs::write(proj.join("a.txt"), "alpha").unwrap();
    std::fs::write(proj.join(".hidden"), "h").unwrap();
    std::fs::write(proj.join("sub/b.txt"), "beta").unwrap();
    std::fs::write(proj.join("sub/c.bin"), [0u8, 255, 7]).unwrap();
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let sid = c.session(&proj).await;
    let entry = |name: &str, dir, len| FsEntry {
        name: name.into(),
        dir,
        len,
    };
    let list = |path: &str| Req::FsList {
        sid,
        path: path.into(),
    };
    let read = |path: &str| Req::FsRead {
        sid,
        path: path.into(),
    };

    // The session's own folder: folders first, then files, each by name, hidden ones too.
    let root = Ok(Resp::FsEntries(vec![
        entry("sub", true, 0),
        entry(".hidden", false, 1),
        entry("a.txt", false, 5),
    ]));
    assert_eq!(c.call(list("")).await, root);
    assert_eq!(c.call(list(".")).await, root);
    // A sub-folder, by its relative and by its absolute path.
    let sub = Ok(Resp::FsEntries(vec![
        entry("deep", true, 0),
        entry("b.txt", false, 4),
        entry("c.bin", false, 3),
    ]));
    assert_eq!(c.call(list("sub")).await, sub);
    assert_eq!(c.call(list("sub/")).await, sub);
    assert_eq!(
        c.call(list(&proj.join("sub").display().to_string())).await,
        sub
    );
    assert_eq!(c.call(list("sub/deep")).await, Ok(Resp::FsEntries(vec![])));
    // Another folder of home, by its absolute path.
    let from_home = c.call(list(&rig.home().display().to_string())).await;
    assert_eq!(from_home, Ok(Resp::FsEntries(vec![entry("proj", true, 0)])));
    // Files: what is in them, relative and absolute, binary too.
    assert_eq!(
        c.call(read("a.txt")).await,
        Ok(Resp::FsData(b"alpha".to_vec()))
    );
    assert_eq!(
        c.call(read(".hidden")).await,
        Ok(Resp::FsData(b"h".to_vec()))
    );
    assert_eq!(
        c.call(read("sub/c.bin")).await,
        Ok(Resp::FsData(vec![0, 255, 7]))
    );
    let absolute = proj.join("sub/b.txt").display().to_string();
    assert_eq!(
        c.call(read(&absolute)).await,
        Ok(Resp::FsData(b"beta".to_vec()))
    );
    // Reads change nothing: asked again they answer again (no op is recorded for them).
    assert_eq!(
        c.call(read("a.txt")).await,
        Ok(Resp::FsData(b"alpha".to_vec()))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_path_that_climbs_or_leaves_home_is_refused_and_a_missing_one_is_not_found() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let proj = rig.home().join("proj");
    let outside = rig.b.dir.path().join("outside");
    std::fs::create_dir_all(proj.join("sub")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(proj.join("a.txt"), "alpha").unwrap();
    std::fs::write(outside.join("secret"), "s").unwrap();
    std::os::unix::fs::symlink(&outside, proj.join("out")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret"), proj.join("leak")).unwrap();
    std::os::unix::fs::symlink(proj.join("sub"), proj.join("in")).unwrap();
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let sid = c.session(&proj).await;
    let list = |path: &str| Req::FsList {
        sid,
        path: path.into(),
    };
    let read = |path: &str| Req::FsRead {
        sid,
        path: path.into(),
    };
    let secret = outside.join("secret").display().to_string();

    // `..` anywhere, even where it would stay inside home.
    for req in [
        read("../outside/secret"),
        list(".."),
        list("sub/.."),
        read("sub/../a.txt"),
        read(&format!("{}/../proj/a.txt", rig.home().display())),
    ] {
        let tried = format!("{req:?}");
        assert_eq!(c.call(req).await.unwrap_err().code, ErrCode::Bad, "{tried}");
    }
    // Absolute paths out of home, and symlinks that lead out of it (to a folder or to a file).
    for req in [
        read(&secret),
        read("/etc/hostname"),
        list("/"),
        list(&outside.display().to_string()),
        list("out"),
        read("out/secret"),
        read("leak"),
    ] {
        let tried = format!("{req:?}");
        assert_eq!(c.call(req).await.unwrap_err().code, ErrCode::Bad, "{tried}");
    }
    // A symlink that stays inside home is just a folder.
    assert_eq!(c.call(list("in")).await, Ok(Resp::FsEntries(vec![])));
    // Missing, wrong kind, and no such session.
    for (req, want) in [
        (read("nope"), ErrCode::NotFound),
        (list("nope/deeper"), ErrCode::NotFound),
        (list("a.txt"), ErrCode::Bad),
        (read("sub"), ErrCode::Bad),
        (
            Req::FsList {
                sid: Id::new(),
                path: String::new(),
            },
            ErrCode::NotFound,
        ),
    ] {
        let tried = format!("{req:?}");
        assert_eq!(c.call(req).await.unwrap_err().code, want, "{tried}");
    }
    // A closed session shows nothing.
    assert_eq!(c.call(Req::SessionClose { sid }).await, Ok(Resp::Ok));
    assert_eq!(c.call(list("")).await.unwrap_err().code, ErrCode::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_over_sixteen_mebibytes_is_not_sent_and_a_big_one_below_it_is() {
    let rig = rig().await;
    let dev = rig.enroll("laptop");
    let proj = rig.home().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    // Past the old one-mebibyte request limit, which only requests have.
    let bytes: Vec<u8> = (0..3 << 20).map(|i: u32| (i % 251) as u8).collect();
    std::fs::write(proj.join("mid"), &bytes).unwrap();
    std::fs::File::create(proj.join("big"))
        .unwrap()
        .set_len((16 << 20) + 1)
        .unwrap();
    let mut c = rig.desktop(&dev, Cursors::default()).await;
    let sid = c.session(&proj).await;
    let read = |path: &str| Req::FsRead {
        sid,
        path: path.into(),
    };
    assert_eq!(c.call(read("mid")).await, Ok(Resp::FsData(bytes)));
    let refused = c.call(read("big")).await.unwrap_err();
    assert_eq!(refused.code, ErrCode::Bad, "{}", refused.msg);
    // Still a working connection afterwards.
    assert_eq!(
        c.call(Req::FsList {
            sid,
            path: String::new()
        })
        .await,
        Ok(Resp::FsEntries(vec![
            FsEntry {
                name: "big".into(),
                dir: false,
                len: (16 << 20) + 1
            },
            FsEntry {
                name: "mid".into(),
                dir: false,
                len: 3 << 20
            },
        ]))
    );
}
