//! Integration tests: every test starts the real `femme-holder` binary (with `--no-sd-notify` and
//! temporary runtime/state dirs) and talks to it over its unix socket, the way `femmed` does.
//!
//! Connections are blocking sockets with one reader thread each, so a test never loses half a
//! message to a read timeout and every message carries the time it arrived.
//!
//! The panic-isolation path (`catch_unwind` around the emulator) is tested through a debug-build
//! hook: `FEMME_HOLDER_PANIC_AT=<site>` makes the holder's first emulator call at that site panic.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use femme_holder::paths;
use femme_holder::snap::Snapshot;
use femme_holder::wire;
use femme_proto::holder::{HolderEvent, HolderInfo, HolderReq, HolderResp, ScreenFmt, Target};
use femme_proto::term::{Frame, Key, KeyCode, Mods, RowPatch, TermDown, TermUp};
use femme_proto::{PaneId, Sid, HOLDER_VERSION};
use femme_term::client::{ApplyError, RowCache};
use rustix::process::{kill_process, Pid, Resource, Rlimit, Signal};

const LONG: Duration = Duration::from_secs(15);

fn key(c: char) -> Key {
    Key { code: KeyCode::Char(c), mods: Mods::empty() }
}

fn enter() -> Key {
    Key { code: KeyCode::Enter, mods: Mods::empty() }
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn row_text(p: &RowPatch) -> String {
    p.runs.iter().map(|(_, t)| t.as_str()).collect()
}

// ---- the holder process --------------------------------------------------------------------

struct Holder {
    child: Child,
    dir: tempfile::TempDir,
    sid: Sid,
}

impl Holder {
    fn start() -> Holder {
        Self::start_with(&[], &[])
    }

    fn start_with(args: &[&str], env: &[(&str, &str)]) -> Holder {
        Self::start_at(tempfile::tempdir().unwrap(), Sid::new(), args, env)
    }

    fn start_at(dir: tempfile::TempDir, sid: Sid, args: &[&str], env: &[(&str, &str)]) -> Holder {
        let child = Self::command(dir.path(), sid, args, env).spawn().expect("spawn femme-holder");
        Self::listening(child, dir, sid)
    }

    /// A holder that ignores SIGXFSZ, so a file that hits its size limit (see `limit_files`) gets
    /// EFBIG from `write` instead of killing the process.
    ///
    /// Its stderr is a file that is already past the limits the test sets, so every log line it
    /// writes fails too (EFBIG): a holder that panics over that, or loses its spool task, fails.
    fn start_ignoring_xfsz() -> Holder {
        let (dir, sid) = (tempfile::tempdir().unwrap(), Sid::new());
        let log = dir.path().join("holder.log");
        std::fs::write(&log, vec![b'-'; 4096]).unwrap();
        let log = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        let mut cmd = Self::command_in(dir.path(), sid, &[], &[], Some("trap '' XFSZ"));
        let child = cmd.stderr(Stdio::from(log)).spawn().expect("spawn femme-holder");
        Self::listening(child, dir, sid)
    }

    fn listening(child: Child, dir: tempfile::TempDir, sid: Sid) -> Holder {
        let mut h = Holder { child, dir, sid };
        let sock = h.sock();
        let t0 = Instant::now();
        while UnixStream::connect(&sock).is_err() {
            if let Some(st) = h.child.try_wait().unwrap() {
                panic!("holder exited during startup: {st}");
            }
            assert!(t0.elapsed() < LONG, "holder did not start listening");
            thread::sleep(Duration::from_millis(10));
        }
        h
    }

    fn command(dir: &Path, sid: Sid, args: &[&str], env: &[(&str, &str)]) -> Command {
        Self::command_in(dir, sid, args, env, None)
    }

    /// The holder's command line, optionally run by `sh` after a shell prelude.
    fn command_in(dir: &Path, sid: Sid, args: &[&str], env: &[(&str, &str)], prelude: Option<&str>) -> Command {
        let bin = env!("CARGO_BIN_EXE_femme-holder");
        let mut c = match prelude {
            None => Command::new(bin),
            Some(p) => {
                let mut c = Command::new("sh");
                c.arg("-c").arg(format!("{p}; exec \"$0\" \"$@\"")).arg(bin);
                c
            }
        };
        c.arg("--sid")
            .arg(sid.to_string())
            .arg("--no-sd-notify")
            .arg("--runtime-dir")
            .arg(dir.join("rt"))
            .arg("--state-dir")
            .arg(dir.join("st"))
            .args(args)
            .env("RUST_LOG", "warn")
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        c
    }

    fn sock(&self) -> PathBuf {
        paths::socket_path(&self.dir.path().join("rt"), self.sid)
    }

    fn state_dir(&self) -> PathBuf {
        self.dir.path().join("st")
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn ctl(&self) -> Ctl {
        Ctl::connect(&self.sock())
    }

    /// A fresh view of `pane` (full snapshot first).
    fn view(&self, pane: u16) -> View {
        self.view_as(pane, 0, 0, u128::from(pane) + 1)
    }

    fn view_as(&self, pane: u16, epoch: u64, have_rev: u64, client: u128) -> View {
        View::attach(&self.sock(), self.pane_id(pane), epoch, have_rev, client, RowCache::new())
    }

    fn pane_id(&self, no: u16) -> PaneId {
        PaneId { sid: self.sid, no }
    }

    /// Cap the size of any file the holder writes (`None` lifts it): a full disk, in miniature.
    fn limit_files(&self, bytes: Option<u64>) {
        let soft = Rlimit { current: bytes, maximum: None };
        rustix::process::prlimit(Pid::from_raw(self.pid() as i32), Resource::Fsize, soft).expect("prlimit the holder");
    }

    fn spool(&self, from: u64) -> Conn {
        let mut c = Conn::open(&self.sock());
        c.send(&HolderReq::SpoolRead { from });
        c
    }

    fn wait_exit(&mut self, d: Duration) -> Option<ExitStatus> {
        let end = Instant::now() + d;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return Some(st);
            }
            if Instant::now() >= end {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// utime + stime of the holder in clock ticks.
    fn cpu_ticks(&self) -> u64 {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.pid())).unwrap();
        let after_comm = &stat[stat.rfind(')').unwrap() + 2..];
        let f: Vec<&str> = after_comm.split(' ').collect();
        f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()
    }

    fn rss_kb(&self) -> u64 {
        rss_kb(self.pid())
    }
}

fn rss_kb(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

impl Drop for Holder {
    /// SIGTERM is the holder's own "stop everything" signal; SIGKILL only if it hangs.
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            if let Some(pid) = Pid::from_raw(self.child.id() as i32) {
                let _ = kill_process(pid, Signal::TERM);
            }
            if self.wait_exit(Duration::from_secs(6)).is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
}

// ---- connections ---------------------------------------------------------------------------

struct Conn {
    wr: UnixStream,
    rx: Receiver<(Instant, HolderResp)>,
}

impl Conn {
    fn open(sock: &Path) -> Conn {
        let wr = UnixStream::connect(sock).expect("connect to the holder");
        let mut rd = wr.try_clone().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut len = [0u8; 4];
            while rd.read_exact(&mut len).is_ok() {
                let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
                if rd.read_exact(&mut buf).is_err() {
                    return;
                }
                let at = Instant::now();
                let msg: HolderResp = postcard::from_bytes(&buf).expect("the holder sent a message that does not decode");
                if tx.send((at, msg)).is_err() {
                    return;
                }
            }
        });
        Conn { wr, rx }
    }

    /// Send a request; returns the instant just before the write.
    fn send(&mut self, req: &HolderReq) -> Instant {
        self.try_send(req).expect("write to the holder")
    }

    fn try_send(&mut self, req: &HolderReq) -> std::io::Result<Instant> {
        let bytes = wire::encode(req).unwrap();
        let t = Instant::now();
        self.wr.write_all(&bytes)?;
        Ok(t)
    }

    fn recv(&self, d: Duration) -> Option<(Instant, HolderResp)> {
        self.rx.recv_timeout(d).ok()
    }

    /// True once the holder has closed the connection (everything still queued is discarded).
    fn closed_within(&self, d: Duration) -> bool {
        let end = Instant::now() + d;
        loop {
            match self.rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                Err(mpsc::RecvTimeoutError::Disconnected) => return true,
                Err(mpsc::RecvTimeoutError::Timeout) => return false,
                Ok(_) => {}
            }
        }
    }

    /// Spool bytes from this connection until `n` have arrived, checking the offsets as it goes.
    fn spool_bytes(&self, from: u64, n: usize) -> Vec<u8> {
        let mut got = Vec::new();
        while got.len() < n {
            match self.recv(LONG) {
                Some((_, HolderResp::Spool { offset, data })) => {
                    assert_eq!(offset, from + got.len() as u64, "spool chunks must be contiguous");
                    assert!(data.len() <= 64 * 1024, "spool chunk of {} bytes", data.len());
                    got.extend(data);
                }
                other => panic!("expected spool data, got {other:?}"),
            }
        }
        got
    }
}

impl Drop for Conn {
    /// The reader thread keeps a dup of the socket, so closing `wr` alone would not hang up.
    fn drop(&mut self) {
        let _ = self.wr.shutdown(std::net::Shutdown::Both);
    }
}

/// A control connection: requests and replies in order, events collected on the side.
struct Ctl {
    c: Conn,
    events: VecDeque<HolderEvent>,
    hello: HolderInfo,
}

impl Ctl {
    fn connect(sock: &Path) -> Ctl {
        let mut c = Conn::open(sock);
        c.send(&HolderReq::Hello { v: HOLDER_VERSION });
        match c.recv(LONG) {
            Some((_, HolderResp::Info(hello))) => Ctl { c, events: VecDeque::new(), hello },
            other => panic!("expected Info after Hello, got {other:?}"),
        }
    }

    fn req(&mut self, r: HolderReq) -> HolderResp {
        self.c.send(&r);
        loop {
            match self.c.recv(LONG) {
                Some((_, HolderResp::Event(e))) => self.events.push_back(e),
                Some((_, resp)) => return resp,
                None => panic!("no reply to {r:?}"),
            }
        }
    }

    fn ok(&mut self, r: HolderReq) {
        let resp = self.req(r.clone());
        assert_eq!(resp, HolderResp::Ok, "reply to {r:?}");
    }

    fn err(&mut self, r: HolderReq) -> String {
        match self.req(r.clone()) {
            HolderResp::Err(e) => e,
            other => panic!("expected Err for {r:?}, got {other:?}"),
        }
    }

    fn info(&mut self) -> HolderInfo {
        match self.req(HolderReq::Hello { v: HOLDER_VERSION }) {
            HolderResp::Info(i) => i,
            other => panic!("expected Info, got {other:?}"),
        }
    }

    fn screen(&mut self, pane: u16) -> String {
        match self.req(HolderReq::Screen { pane, fmt: ScreenFmt::Text }) {
            HolderResp::Screen(s) => s,
            other => panic!("expected Screen, got {other:?}"),
        }
    }

    /// Whether `Screen` answers (not with an error) and `pred` likes the text.
    fn screen_is(&mut self, pred: impl Fn(&str) -> bool) -> bool {
        matches!(self.req(HolderReq::Screen { pane: 0, fmt: ScreenFmt::Text }), HolderResp::Screen(t) if pred(&t))
    }

    /// Wait until the agent's spool holds `end` bytes.
    fn wait_spool(&mut self, end: u64) {
        let t0 = Instant::now();
        while self.info().agent.is_some_and(|a| a.spool_hi < end) {
            assert!(t0.elapsed() < LONG * 2, "the spool never reached {end} bytes");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn spawn_pty(&mut self, no: u16, cmd: &[&str]) {
        self.spawn_pty_sized(no, cmd, 80, 24, &[]);
    }

    fn spawn_pty_sized(&mut self, no: u16, cmd: &[&str], cols: u16, rows: u16, env: &[(&str, &str)]) {
        let env = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.ok(HolderReq::SpawnPty { no, cmd: strings(cmd), cwd: String::new(), env, cols, rows });
    }

    fn spawn_agent(&mut self, cmd: &[&str]) -> HolderResp {
        self.req(HolderReq::SpawnAgent { cmd: strings(cmd), cwd: String::new(), env: Vec::new() })
    }

    /// Wait for an event `pred` accepts (looking at the ones already received first).
    fn event<T>(&mut self, d: Duration, mut pred: impl FnMut(&HolderEvent) -> Option<T>) -> Option<T> {
        let mut found = None;
        let mut keep = VecDeque::new();
        while let Some(e) = self.events.pop_front() {
            match found.is_none().then(|| pred(&e)).flatten() {
                Some(t) => found = Some(t),
                None => keep.push_back(e),
            }
        }
        self.events = keep;
        if found.is_some() {
            return found;
        }
        let end = Instant::now() + d;
        loop {
            match self.c.recv(end.saturating_duration_since(Instant::now())) {
                Some((_, HolderResp::Event(e))) => match pred(&e) {
                    Some(t) => return Some(t),
                    None => self.events.push_back(e),
                },
                Some((_, other)) => panic!("unexpected message on a control connection: {other:?}"),
                None => return None,
            }
        }
    }

    fn child_exit(&mut self, target: Target) -> Option<i32> {
        self.event(LONG, |e| match e {
            HolderEvent::ChildExited { target: t, status } if *t == target => Some(*status),
            _ => None,
        })
    }
}

/// A view of one pane: applies frames to a `RowCache` and acks them (unless told not to).
struct View {
    c: Conn,
    pane: PaneId,
    client: u128,
    cache: RowCache,
    frames: Vec<(Instant, Frame)>,
    rows: Vec<Vec<RowPatch>>,
    exited: Option<i32>,
    bells: usize,
    ack: bool,
    /// Answer a frame the cache refuses the way femme-term's contract says (re-send `Open`) instead
    /// of failing the test; `resyncs` records why.
    resync: bool,
    resyncs: Vec<ApplyError>,
    /// Frames to let fall on the floor before applying any: a client that lost some.
    lose: usize,
    /// Record an `Err` on the stream (in `errs`) instead of failing the test.
    allow_err: bool,
    errs: Vec<String>,
}

impl View {
    fn attach(sock: &Path, pane: PaneId, epoch: u64, have_rev: u64, client: u128, cache: RowCache) -> View {
        let mut c = Conn::open(sock);
        c.send(&HolderReq::Attach { pane: pane.no, epoch, have_rev, client_instance: client });
        View {
            c,
            pane,
            client,
            cache,
            frames: Vec::new(),
            rows: Vec::new(),
            exited: None,
            bells: 0,
            ack: true,
            resync: false,
            resyncs: Vec::new(),
            lose: 0,
            allow_err: false,
            errs: Vec::new(),
        }
    }

    fn up(&mut self, up: TermUp) -> Instant {
        self.c.send(&HolderReq::Term(up))
    }

    fn key(&mut self, seq: u32, c: char) -> Instant {
        self.up(TermUp::Key { seq, key: key(c) })
    }

    fn last(&self) -> &Frame {
        &self.frames.last().expect("no frame received yet").1
    }

    fn handle(&mut self, at: Instant, down: TermDown) {
        match down {
            TermDown::Frame(_) if self.lose > 0 => self.lose -= 1,
            TermDown::Frame(f) => match self.cache.apply(&f) {
                Ok(()) => {
                    let rev = f.rev;
                    self.frames.push((at, f));
                    if self.ack {
                        // An ack for a frame that arrived just before the holder closed the view has nowhere to go.
                        let _ = self.c.try_send(&HolderReq::Term(TermUp::Ack { rev }));
                    }
                }
                Err(e) if self.resync => {
                    self.resyncs.push(e);
                    let have_rev = match e {
                        ApplyError::NeedResync { have_rev } => have_rev,
                        ApplyError::EpochChanged => {
                            self.cache = RowCache::new();
                            0
                        }
                    };
                    let epoch = if have_rev == 0 { f.epoch } else { self.cache.epoch };
                    self.up(TermUp::Open { pane: self.pane, epoch, have_rev, client_instance: self.client });
                }
                Err(_) => panic!("frame does not apply to the cache: epoch {} base_rev {} rev {}", f.epoch, f.base_rev, f.rev),
            },
            TermDown::Rows(r) => self.rows.push(r),
            TermDown::Exited { status } => self.exited = Some(status),
            TermDown::Bell => self.bells += 1,
        }
    }

    /// Read until `done` holds or `d` has passed; says whether it held.
    fn pump(&mut self, d: Duration, mut done: impl FnMut(&View) -> bool) -> bool {
        let end = Instant::now() + d;
        loop {
            if done(self) {
                return true;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            match self.c.recv(left) {
                Some((at, HolderResp::Term(down))) => self.handle(at, down),
                Some((_, HolderResp::Err(e))) if self.allow_err => self.errs.push(e),
                Some((_, other)) => panic!("unexpected message on a view: {other:?}"),
                None => return done(self),
            }
        }
    }

    fn settle(&mut self, d: Duration) {
        self.pump(d, |_| false);
    }

    fn until_text(&mut self, d: Duration, pred: impl Fn(&str) -> bool) -> bool {
        self.pump(d, |v| pred(&v.cache.text()))
    }

    /// Wait for a frame the holder marks as reflecting input up to `seq`.
    fn until_echo(&mut self, seq: u32) {
        assert!(self.pump(LONG, |v| v.frames.iter().any(|(_, f)| f.echo_seq >= seq)), "no frame with echo_seq >= {seq}");
    }
}

// ---- 1. control connections ----------------------------------------------------------------

#[test]
fn control_hello_versions_and_shared_events() {
    let h = Holder::start();
    let mut a = h.ctl();
    assert_eq!(a.hello.v, HOLDER_VERSION);
    assert_eq!(a.hello.sid, h.sid);
    assert!(a.hello.epoch > 1_700_000_000_000, "epoch is unix ms: {}", a.hello.epoch);
    assert!(a.hello.panes.is_empty() && a.hello.agent.is_none());

    // The previous version is accepted, anything else is refused and the connection closed.
    let mut old = Conn::open(&h.sock());
    old.send(&HolderReq::Hello { v: HOLDER_VERSION - 1 });
    assert!(matches!(old.recv(LONG), Some((_, HolderResp::Info(_)))));
    let mut bad = Conn::open(&h.sock());
    bad.send(&HolderReq::Hello { v: 99 });
    assert!(matches!(bad.recv(LONG), Some((_, HolderResp::Err(_)))));
    assert!(bad.closed_within(LONG));

    // Two control connections see the same events.
    let mut b = h.ctl();
    a.spawn_pty(0, &["bash", "-c", r#"printf '\033]0;hi\007'; sleep 0.2; exit 4"#]);
    for c in [&mut a, &mut b] {
        let title = c.event(LONG, |e| match e {
            HolderEvent::Title { pane: 0, title } => Some(title.clone()),
            _ => None,
        });
        assert_eq!(title.as_deref(), Some("hi"));
        assert_eq!(c.child_exit(Target::Pane(0)), Some(4));
    }
    // A control connection that goes away changes nothing for the others.
    drop(a);
    let info = b.info();
    assert_eq!(info.panes.len(), 1);
    assert_eq!((info.panes[0].alive, info.panes[0].exit, info.panes[0].title.as_str()), (false, Some(4), "hi"));
}

#[test]
fn requests_are_checked() {
    let h = Holder::start();
    let mut c = h.ctl();
    assert!(c.err(HolderReq::Screen { pane: 9, fmt: ScreenFmt::Text }).contains("no pane 9"));
    assert!(c.err(HolderReq::ClosePane { no: 9 }).contains("no pane 9"));
    assert!(c.err(HolderReq::Signal { target: Target::Agent, sig: 15 }).contains("no agent"));
    assert!(c.err(HolderReq::AgentWrite(b"x".to_vec())).contains("no agent"));
    assert!(c.err(HolderReq::SpawnPty { no: 0, cmd: vec![], cwd: String::new(), env: vec![], cols: 80, rows: 24 }).contains("empty command"));
    assert!(c.err(HolderReq::SpawnPty { no: 0, cmd: strings(&["/nonexistent/prog"]), cwd: String::new(), env: vec![], cols: 80, rows: 24 }).contains("spawn"));
    assert!(c.err(HolderReq::SpawnPty { no: 0, cmd: strings(&["true"]), cwd: "/nonexistent".into(), env: vec![], cols: 80, rows: 24 }).contains("cwd"));
    c.spawn_pty(0, &["sleep", "30"]);
    assert!(c.err(HolderReq::SpawnPty { no: 0, cmd: strings(&["true"]), cwd: String::new(), env: vec![], cols: 80, rows: 24 }).contains("already exists"));
    // Role-bound messages belong on their own connection; this one stays usable.
    assert!(matches!(c.req(HolderReq::Attach { pane: 0, epoch: 0, have_rev: 0, client_instance: 1 }), HolderResp::Err(_)));
    assert!(matches!(c.req(HolderReq::SpoolRead { from: 0 }), HolderResp::Err(_)));
    assert_eq!(c.info().panes.len(), 1);

    // The first message decides the role; anything but Hello/Attach/SpoolRead is refused.
    let mut x = Conn::open(&h.sock());
    x.send(&HolderReq::Shutdown);
    assert!(matches!(x.recv(LONG), Some((_, HolderResp::Err(_)))));
    assert!(x.closed_within(LONG));
    let mut x = Conn::open(&h.sock());
    x.send(&HolderReq::Attach { pane: 5, epoch: 0, have_rev: 0, client_instance: 1 });
    assert!(matches!(x.recv(LONG), Some((_, HolderResp::Err(e))) if e.contains("no pane 5")));
    assert!(h.child.id() > 0 && c.info().panes.len() == 1, "the holder survived all of that");
}

// ---- 2. a pane, start to finish ------------------------------------------------------------

#[test]
fn pane_round_trip() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["bash", "-c", r#"printf "hello\n"; seq 1 300; printf "\033]0;T\007"; sleep 0.3; exit 3"#]);
    let mut v = h.view(0);
    assert!(v.pump(LONG, |v| v.exited.is_some()), "the view never got Exited");
    assert_eq!(v.exited, Some(3));
    assert_eq!(v.frames[0].1.base_rev, 0, "the first frame is a snapshot");
    assert_eq!(ctl.child_exit(Target::Pane(0)), Some(3));
    let title = ctl.event(LONG, |e| match e {
        HolderEvent::Title { pane: 0, title } => Some(title.clone()),
        _ => None,
    });
    assert_eq!(title.as_deref(), Some("T"));

    // Exited comes after the last frame: the cache is final and equals the holder's screen.
    let screen = ctl.screen(0);
    assert_eq!(v.cache.text(), screen);
    assert!(screen.contains("300"), "screen: {screen:?}");
    let last = v.last();
    assert!(last.top_abs > 200 && last.history_lo_abs == 0, "top_abs {} history_lo_abs {}", last.top_abs, last.history_lo_abs);
    let info = ctl.info();
    assert_eq!((info.panes[0].alive, info.panes[0].exit, info.panes[0].title.as_str()), (false, Some(3), "T"));
    assert_eq!((info.panes[0].cols, info.panes[0].rows), (80, 24));

    // Scrollback is fetched lazily: `hello` is row 0, then 1, 2, ...
    v.up(TermUp::GetRows { from_abs: 0, to_abs: 5 });
    assert!(v.pump(LONG, |v| !v.rows.is_empty()));
    let texts: Vec<String> = v.rows[0].iter().map(row_text).collect();
    assert_eq!(texts, ["hello", "1", "2", "3", "4"]);
    assert_eq!(v.rows[0][0].abs, 0);
    v.cache.add_rows(v.rows[0].clone());
}

#[test]
fn env_is_a_short_allowlist_plus_what_the_caller_adds() {
    let h = Holder::start_with(&[], &[("SECRET_TOKEN", "hunter2"), ("NOTIFY_SOCKET", "/nope")]);
    let mut ctl = h.ctl();
    let script = r#"echo "sid=$FEMME_SID pane=$FEMME_PANE term=$TERM color=$COLORTERM foo=$FOO"; echo "secret=${SECRET_TOKEN-unset} notify=${NOTIFY_SOCKET-unset}"; sleep 30"#;
    ctl.spawn_pty_sized(2, &["bash", "-c", script], 100, 30, &[("FOO", "bar"), ("FEMME_PANE", "99")]);
    let mut v = h.view(2);
    let want = format!("sid={} pane=2 term=xterm-256color color=truecolor foo=bar", h.sid);
    assert!(v.until_text(LONG, |t| t.contains("secret=")), "screen: {:?}", v.cache.text());
    let text = v.cache.text();
    assert!(text.contains(&want), "screen: {text:?}");
    assert!(text.contains("secret=unset notify=unset"), "screen: {text:?}");
    assert_eq!((v.last().cols, v.last().rows), (100, 30));
}

// ---- 3. input ------------------------------------------------------------------------------

#[test]
fn input_is_written_once_and_confirmed() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["cat"]);
    let mut v = h.view(0);
    v.key(1, 'a');
    v.up(TermUp::Key { seq: 2, key: enter() });
    assert!(
        v.pump(LONG, |v| v.frames.iter().any(|(_, f)| f.echo_seq >= 2 && f.row_patches.iter().any(|p| row_text(p).contains('a')))),
        "no frame with echo_seq >= 2 showing the key; frames: {:?}",
        v.frames.iter().map(|(_, f)| (f.rev, f.applied_seq, f.echo_seq)).collect::<Vec<_>>()
    );
    assert!(v.last().applied_seq >= 2);
    // cat prints the line back a moment after the tty echoed it.
    assert!(v.until_text(LONG, |t| t.matches('a').count() == 2), "screen: {:?}", v.cache.text());

    // A resent seq (a reconnect replaying its queue) is dropped, not typed again. `b` is a barrier:
    // the tty echoes input in the order written, so the resend would show before `b`.
    v.key(1, 'a');
    v.key(3, 'b');
    v.until_echo(3);
    assert!(v.until_text(LONG, |t| t.contains('b')));
    let screen = ctl.screen(0);
    // The tty echoes `a` and cat prints it back: two. A third would mean the resend was applied.
    assert_eq!(screen.matches('a').count(), 2, "screen: {screen:?}");
    assert_eq!(v.cache.text(), screen);

    // Paste and raw bytes go through too.
    v.up(TermUp::Paste { seq: 4, text: "pasted".into() });
    v.up(TermUp::Bytes { seq: 5, data: b"raw\n".to_vec() });
    assert!(v.until_text(LONG, |t| t.contains("pasted") && t.contains("raw")), "screen: {:?}", v.cache.text());
    // seq 0 is untracked input (the compat layer's /input): always written, never deduped.
    v.up(TermUp::Bytes { seq: 0, data: b"z".to_vec() });
    v.up(TermUp::Bytes { seq: 0, data: b"z".to_vec() });
    assert!(v.until_text(LONG, |t| t.contains("zz")));
}

#[test]
fn input_seq_survives_a_reconnect_of_the_same_client() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["cat"]);
    let client = 0xC0FFEE;
    let mut v = h.view_as(0, 0, 0, client);
    v.key(1, 'p');
    v.key(2, 'q');
    v.until_echo(2);
    drop(v);

    // Same client_instance: the holder remembers it got seq 2, so the first frame says so and a
    // replayed seq 2 is dropped. A different client starts from zero.
    let mut v = h.view_as(0, 0, 0, client);
    assert!(v.pump(LONG, |v| !v.frames.is_empty()));
    assert_eq!(v.last().applied_seq, 2);
    v.key(2, 'X');
    v.key(3, 'r');
    v.until_echo(3);
    let screen = ctl.screen(0);
    assert!(screen.contains("pqr") && !screen.contains('X'), "screen: {screen:?}");
    let mut other = h.view_as(0, 0, 0, client + 1);
    assert!(other.pump(LONG, |v| !v.frames.is_empty()));
    assert_eq!(other.last().applied_seq, 0);
    other.key(1, 's');
    other.until_echo(1);
    assert!(ctl.screen(0).contains("pqrs"));
}

#[test]
fn keystroke_to_frame_latency() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["cat"]);
    let mut v = h.view(0);
    assert!(v.pump(LONG, |v| !v.frames.is_empty()));
    v.settle(Duration::from_millis(100));

    let mut lat = Vec::new();
    for seq in 1..=200u32 {
        let sent = v.key(seq, 'x');
        v.until_echo(seq);
        let (at, _) = v.frames.iter().find(|(_, f)| f.echo_seq >= seq).unwrap();
        lat.push(at.duration_since(sent));
        v.frames.clear(); // keep the search short; the cache has the screen
        // Typing is not instantaneous; frames for a key must not wait for the 8 ms quiet timer.
        thread::sleep(Duration::from_millis(3));
    }
    lat.sort();
    let pct = |p: f64| lat[((lat.len() - 1) as f64 * p).round() as usize];
    eprintln!("keystroke -> frame over {} keys: p50 {:?}  p99 {:?}  max {:?}", lat.len(), pct(0.5), pct(0.99), lat[lat.len() - 1]);
    assert!(pct(0.5) < Duration::from_millis(8), "p50 {:?}: keystroke frames are waiting for the coalescing timer", pct(0.5));
    assert_eq!(v.cache.text(), ctl.screen(0));
}

#[test]
fn timing_is_stamped_on_sampled_inputs() {
    let h = Holder::start_with(&["--timing-every", "10"], &[]);
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["cat"]);
    let mut v = h.view(0);
    for seq in 1..=30u32 {
        v.key(seq, 'k');
        v.until_echo(seq);
        thread::sleep(Duration::from_millis(5));
    }
    let timed: Vec<_> = v.frames.iter().filter_map(|(_, f)| f.timing).filter(|t| t.seq != 0).collect();
    let seqs: Vec<u32> = timed.iter().map(|t| t.seq).collect();
    assert!(seqs.contains(&10) && seqs.contains(&20) && seqs.contains(&30), "timed seqs: {seqs:?}");
    for t in timed {
        assert!(t.input_recv_ns <= t.pty_write_ns && t.pty_write_ns <= t.pty_read_ns && t.pty_read_ns <= t.frame_sent_ns, "{t:?}");
    }
    assert!(v.frames.iter().filter(|(_, f)| f.timing.is_some()).count() < v.frames.len(), "not every frame is sampled");
}

// ---- 4. flow control -----------------------------------------------------------------------

#[test]
fn a_view_that_never_acks_is_stopped_at_8_frames_and_catches_up() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["bash", "-c", "seq 1 200000; for i in $(seq 1 40); do echo tick$i; sleep 0.02; done"]);
    let mut v = h.view(0);
    v.ack = false;

    // The holder keeps answering while the flood runs and the view is stuck. (The bound is loose
    // for the worst probe: this runs next to other tests and, often, a compiler.)
    let mut probes = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(1500) {
        let t = Instant::now();
        ctl.screen(0);
        probes.push(t.elapsed());
        v.settle(Duration::from_millis(25));
    }
    probes.sort();
    let (median, worst) = (probes[probes.len() / 2], probes[probes.len() - 1]);
    eprintln!("Screen round trip during a flood with a stuck view: median {median:?}, worst {worst:?} over {} probes", probes.len());
    assert!(median < Duration::from_millis(100), "the holder answered in {median:?} (median)");
    assert!(worst < Duration::from_secs(1), "the holder stalled for {worst:?}");
    // The child produced far more than 8 frames' worth of changes; only 8 were sent.
    assert_eq!(v.frames.len(), 8, "unacked frames are capped at 8");

    // Acking lets one merged frame through, then the rest; the view ends up equal to the screen.
    v.ack = true;
    let rev = v.last().rev;
    v.up(TermUp::Ack { rev });
    assert!(v.pump(LONG, |v| v.exited.is_some()), "no Exited after acking");
    assert!(v.frames.len() > 8 && v.frames.len() < 60, "frames after catching up: {}", v.frames.len());
    assert_eq!(v.cache.text(), ctl.screen(0));
    assert!(v.cache.text().contains("tick40"));
}

// ---- 5. reconnect --------------------------------------------------------------------------

#[test]
fn reconnect_gets_a_delta_nothing_or_a_snapshot() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["bash", "-c", "echo one; read x; echo two:$x; sleep 30"]);
    let mut v = h.view_as(0, 0, 0, 7);
    assert!(v.until_text(LONG, |t| t.contains("one")));
    v.settle(Duration::from_millis(100));
    let (epoch, rev) = (v.last().epoch, v.last().rev);
    let View { cache, .. } = v;

    // Nothing changed: attaching with the rev we hold sends nothing.
    let mut quiet = View::attach(&h.sock(), h.pane_id(0), epoch, rev, 7, cache);
    quiet.settle(Duration::from_millis(300));
    assert!(quiet.frames.is_empty(), "unchanged screen sent {} frames", quiet.frames.len());

    // Output happens while nobody is watching; the next attach brings a delta onto the same cache.
    quiet.up(TermUp::Bytes { seq: 1, data: b"hi\n".to_vec() });
    thread::sleep(Duration::from_millis(50));
    let View { cache, .. } = quiet;
    thread::sleep(Duration::from_millis(300));
    let mut back = View::attach(&h.sock(), h.pane_id(0), epoch, rev, 7, cache);
    assert!(back.until_text(LONG, |t| t.contains("two:hi")), "screen: {:?}", back.cache.text());
    back.settle(Duration::from_millis(100));
    assert_eq!(back.frames[0].1.base_rev, rev, "a delta starts from the rev the client held");
    assert_eq!(back.cache.text(), ctl.screen(0));

    // A wrong epoch, or a rev from the future, gets a full snapshot into an empty cache.
    for (e, r) in [(epoch ^ 0xdead, rev), (epoch, u64::MAX)] {
        let mut snap = h.view_as(0, e, r, 7);
        assert!(snap.pump(LONG, |v| !v.frames.is_empty()));
        let f = snap.last();
        assert_eq!((f.base_rev, f.epoch), (0, epoch));
        assert_eq!(snap.cache.text(), ctl.screen(0));
    }
}

// ---- 6. the headless agent -----------------------------------------------------------------

/// A long-lived agent's spool is freed below what the daemon committed (a hole, offsets stay
/// absolute), a megabyte at a time; the kept part still reads and the hole is refused.
#[test]
fn agent_spool_is_freed_below_the_committed_offset() {
    use std::os::unix::fs::MetadataExt;
    let h = Holder::start();
    let mut ctl = h.ctl();
    assert_eq!(ctl.spawn_agent(&["sh", "-c", "head -c 3145728 /dev/zero | tr '\\0' x"]), HolderResp::Ok);
    let path = paths::spool_path(&h.state_dir(), h.sid);
    let t0 = Instant::now();
    while ctl.info().agent.unwrap().spool_hi < 3 << 20 {
        assert!(t0.elapsed() < LONG, "3 MiB never reached the spool");
        thread::sleep(Duration::from_millis(20));
    }
    let blocks = || std::fs::metadata(&path).unwrap().blocks() * 512;
    let full = blocks();
    assert!(full >= 3 << 20, "{full} bytes allocated");
    ctl.ok(HolderReq::SpoolCommit { offset: 1000 });
    assert_eq!(ctl.info().agent.unwrap().spool_lo, 0, "under a megabyte nothing is freed");
    ctl.ok(HolderReq::SpoolCommit { offset: 2 << 20 });
    let a = ctl.info().agent.unwrap();
    assert_eq!((a.spool_lo, a.spool_hi), (2 << 20, 3 << 20));
    let kept = blocks();
    assert!(kept <= (1 << 20) + 8192, "{full} -> {kept} bytes allocated");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 3 << 20, "the size is kept, offsets stay absolute");
    let tail = h.spool(2 << 20);
    assert!(tail.spool_bytes(2 << 20, 8).iter().all(|b| *b == b'x'), "the kept part reads from the hole's end");
    let below = h.spool(0);
    assert!(matches!(below.recv(LONG), Some((_, HolderResp::Err(e))) if e.contains("outside its kept part")));
    assert!(below.closed_within(LONG));
}

#[test]
fn agent_spool_follow_and_exit() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    let mut other = h.ctl();
    assert_eq!(ctl.spawn_agent(&["cat"]), HolderResp::Ok);
    assert_eq!(ctl.spawn_agent(&["cat"]), HolderResp::Err("agent already running".into()));
    let first: &[u8] = b"{\"a\":1}\n";
    ctl.ok(HolderReq::AgentWrite(first.to_vec()));

    // The spool tail from 0 gets the line, and keeps following.
    let tail = h.spool(0);
    assert_eq!(tail.spool_bytes(0, first.len()), first);
    let info = ctl.info();
    let a = info.agent.expect("agent in Info");
    assert!(a.alive && a.exit.is_none() && a.pid > 0);
    assert_eq!((a.spool_lo, a.spool_hi), (0, first.len() as u64));
    let second: &[u8] = b"{\"b\":22}\n";
    ctl.ok(HolderReq::AgentWrite(second.to_vec()));
    assert_eq!(tail.spool_bytes(first.len() as u64, second.len()), second);
    // Another reader starts mid-file; one past the end is refused.
    let mid = h.spool(first.len() as u64);
    assert_eq!(mid.spool_bytes(first.len() as u64, second.len()), second);
    let mut past = h.spool(10_000);
    assert!(matches!(past.recv(LONG), Some((_, HolderResp::Err(_)))));
    assert!(past.closed_within(LONG));
    let _ = &mut past;
    ctl.ok(HolderReq::SpoolCommit { offset: 16 });
    assert_eq!(ctl.info().agent.unwrap().spool_lo, 0, "under a megabyte nothing is freed");

    // Closing stdin ends cat; every control connection hears about it.
    ctl.ok(HolderReq::AgentCloseStdin);
    assert_eq!(ctl.child_exit(Target::Agent), Some(0));
    assert_eq!(other.child_exit(Target::Agent), Some(0));
    let a = ctl.info().agent.unwrap();
    assert!((a.alive, a.exit) == (false, Some(0)));
    assert!(ctl.err(HolderReq::AgentWrite(b"x".to_vec())).contains("exited"));
    let on_disk = std::fs::read(paths::spool_path(&h.state_dir(), h.sid)).unwrap();
    assert_eq!(on_disk, [first, second].concat());
    assert_eq!(std::fs::metadata(paths::spool_path(&h.state_dir(), h.sid)).unwrap().permissions().mode() & 0o777, 0o600);

    // A new agent may follow; the spool carries on from where it was.
    assert_eq!(ctl.spawn_agent(&["bash", "-c", "echo third"]), HolderResp::Ok);
    let n = first.len() + second.len();
    assert_eq!(tail.spool_bytes(n as u64, 6), b"third\n");
    assert_eq!(ctl.child_exit(Target::Agent), Some(0));
}

#[test]
fn agent_stderr_tail_and_status() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    assert_eq!(ctl.spawn_agent(&["bash", "-c", "echo err >&2; exit 7"]), HolderResp::Ok);
    assert_eq!(ctl.child_exit(Target::Agent), Some(7));
    // The stderr tail was sent just before ChildExited.
    assert!(ctl.events.contains(&HolderEvent::AgentStderr("err\n".into())), "events: {:?}", ctl.events);
    ctl.events.clear();

    // The tail is capped at 64 KiB and keeps the end.
    let script = r#"head -c 200000 /dev/zero | tr '\0' 'e' >&2; echo END >&2; exit 0"#;
    assert_eq!(ctl.spawn_agent(&["bash", "-c", script]), HolderResp::Ok);
    assert_eq!(ctl.child_exit(Target::Agent), Some(0));
    let tail = ctl.events.iter().find_map(|e| if let HolderEvent::AgentStderr(s) = e { Some(s.clone()) } else { None }).unwrap();
    assert!(tail.len() <= 64 * 1024 && tail.ends_with("eeeEND\n"), "tail of {} bytes", tail.len());
    ctl.events.clear();

    // A signalled agent reports -signal.
    assert_eq!(ctl.spawn_agent(&["sleep", "30"]), HolderResp::Ok);
    ctl.ok(HolderReq::Signal { target: Target::Agent, sig: 15 });
    assert_eq!(ctl.child_exit(Target::Agent), Some(-15));
    assert_eq!(ctl.info().agent.unwrap().exit, Some(-15));
}

// ---- panes: resize, GetRows, bell, close ---------------------------------------------------

#[test]
fn resize_belongs_to_the_last_resizer() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    // The child reports its tty size whenever it is told the size changed.
    ctl.spawn_pty(0, &["bash", "-c", r#"trap 'echo size=$(stty size)' WINCH; echo ready; for i in $(seq 600); do sleep 0.05; done"#]);
    let mut a = h.view_as(0, 0, 0, 1);
    let mut b = h.view_as(0, 0, 0, 2);
    assert!(a.until_text(LONG, |t| t.contains("ready")) && b.until_text(LONG, |t| t.contains("ready")));

    a.up(TermUp::Resize { cols: 100, rows: 30 });
    assert!(a.until_text(LONG, |t| t.contains("size=30 100")), "screen: {:?}", a.cache.text());
    assert_eq!((a.last().cols, a.last().rows), (100, 30));
    assert!(b.pump(LONG, |v| v.frames.last().is_some_and(|(_, f)| f.cols == 100)), "other views follow the resize");
    assert_eq!((ctl.info().panes[0].cols, ctl.info().panes[0].rows), (100, 30));

    // b is the phone: it may not resize within 2 s of a.
    b.up(TermUp::Resize { cols: 60, rows: 20 });
    a.settle(Duration::from_millis(400));
    assert_eq!((ctl.info().panes[0].cols, ctl.info().panes[0].rows), (100, 30), "a's size must hold");

    // After 2 s the lock has lapsed: b may resize and now owns the size, so a is the one locked out.
    thread::sleep(Duration::from_millis(1700));
    b.up(TermUp::Resize { cols: 60, rows: 20 });
    assert!(b.until_text(LONG, |t| t.contains("size=20 60")), "screen: {:?}", b.cache.text());
    assert_eq!((ctl.info().panes[0].cols, ctl.info().panes[0].rows), (60, 20));
    a.up(TermUp::Resize { cols: 100, rows: 30 });
    a.settle(Duration::from_millis(400));
    assert_eq!((ctl.info().panes[0].cols, ctl.info().panes[0].rows), (60, 20), "b's size must hold");
    assert_eq!(a.cache.text(), ctl.screen(0));
}

#[test]
fn resize_bursts_are_throttled_and_the_last_size_wins() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["bash", "-c", r#"trap 'echo size=$(stty size)' WINCH; echo ready; for i in $(seq 600); do sleep 0.02; done"#]);
    let mut v = h.view(0);
    assert!(v.until_text(LONG, |t| t.contains("ready")));
    // A window drag: a burst of resizes. The pane applies the first at once, then at most one per
    // 100 ms with the latest size. Every applied resize gives each view a snapshot frame.
    for i in 0..20u16 {
        v.up(TermUp::Resize { cols: 81 + i, rows: 24 });
    }
    assert!(v.until_text(LONG, |t| t.contains("size=24 100")), "the last size must be applied: {:?}", v.cache.text());
    v.settle(Duration::from_millis(300));
    let snapshots = v.frames.iter().filter(|(_, f)| f.base_rev == 0).count();
    assert!(snapshots <= 5, "20 resizes became {snapshots} snapshots (the first one is the attach)");
    assert_eq!((ctl.info().panes[0].cols, ctl.info().panes[0].rows), (100, 24));
    assert_eq!(v.cache.text(), ctl.screen(0));
}

#[test]
fn a_synchronized_update_that_never_ends_still_shows_up() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    // `CSI ? 2026 h` asks the terminal to hold the screen back until `CSI ? 2026 l`; this program
    // goes quiet in the middle, so the emulator has to be nudged when its timeout passes.
    ctl.spawn_pty(0, &["bash", "-c", r#"printf '\033[?2026hheld back'; sleep 30"#]);
    let mut v = h.view(0);
    assert!(v.until_text(LONG, |t| t.contains("held back")), "screen: {:?}", v.cache.text());
    assert!(ctl.screen(0).contains("held back"));
}

#[test]
fn get_rows_is_capped_and_bell_reaches_everyone() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["bash", "-c", r#"seq 1 3000; sleep 0.3; printf '\007'; sleep 30"#]);
    let mut v = h.view(0);
    assert!(v.until_text(LONG, |t| t.contains("3000")));
    v.up(TermUp::GetRows { from_abs: 0, to_abs: 100_000 });
    v.up(TermUp::GetRows { from_abs: 10, to_abs: 5 });
    assert!(v.pump(LONG, |v| v.rows.len() == 2));
    assert_eq!(v.rows[0].len(), 2000, "one request returns at most 2000 rows");
    assert_eq!(v.rows[0][0].abs, 0);
    assert!(v.rows[1].is_empty(), "an empty range is empty");

    assert!(v.pump(LONG, |v| v.bells > 0), "the view never got a bell");
    let bell = ctl.event(LONG, |e| matches!(e, HolderEvent::Bell { pane: 0 }).then_some(()));
    assert!(bell.is_some());
}

#[test]
fn closing_a_pane_hangs_up_then_kills() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["sleep", "30"]);
    ctl.spawn_pty(1, &["bash", "-c", "trap '' HUP; echo ready; sleep 30"]);
    let (mut v0, mut v1) = (h.view(0), h.view(1));
    assert!(v0.pump(LONG, |v| !v.frames.is_empty()));
    assert!(v1.until_text(LONG, |t| t.contains("ready")), "the trap is installed once `ready` shows");

    let t = Instant::now();
    ctl.ok(HolderReq::ClosePane { no: 0 });
    assert_eq!(ctl.child_exit(Target::Pane(0)), Some(-1), "SIGHUP ends a well-behaved child");
    assert!(t.elapsed() < Duration::from_secs(1));
    assert!(v0.pump(LONG, |v| v.exited == Some(-1)));
    assert!(ctl.info().panes.iter().all(|p| p.no != 0), "the pane is gone from Info");
    assert!(ctl.err(HolderReq::ClosePane { no: 0 }).contains("no pane 0"));

    let t = Instant::now();
    ctl.ok(HolderReq::ClosePane { no: 1 });
    assert_eq!(ctl.child_exit(Target::Pane(1)), Some(-9), "a child that ignores SIGHUP is killed");
    let waited = t.elapsed();
    assert!(waited >= Duration::from_millis(1800) && waited < Duration::from_secs(5), "SIGKILL came after {waited:?}");
    // The number can be used again.
    ctl.spawn_pty(0, &["true"]);
    assert_eq!(ctl.child_exit(Target::Pane(0)), Some(0));
}

#[test]
fn focus_events_follow_the_terminal_mode() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    // Pane 0 asked for focus reports (mode 1004), pane 1 did not.
    ctl.spawn_pty(0, &["bash", "-c", r#"printf '\033[?1004h'; echo ready; exec cat -v"#]);
    ctl.spawn_pty(1, &["bash", "-c", "echo ready; exec cat -v"]);
    let (mut a, mut b) = (h.view(0), h.view(1));
    assert!(a.until_text(LONG, |t| t.contains("ready")) && b.until_text(LONG, |t| t.contains("ready")));
    for v in [&mut a, &mut b] {
        v.up(TermUp::Focus(true));
        v.up(TermUp::Bytes { seq: 1, data: b"x\n".to_vec() });
    }
    assert!(a.until_text(LONG, |t| t.contains("^[[I")), "screen: {:?}", a.cache.text());
    // The tty echoes in the order written, so b's `x` shows after any focus report it was sent.
    assert!(b.until_text(LONG, |t| t.contains('x')));
    assert!(!ctl.screen(1).contains("^[[I"), "a program that did not ask must not get focus reports");
}

#[test]
fn terminal_queries_are_answered_to_the_program() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    // A cursor position report: the emulator's reply has to travel back through the pty into the
    // program's stdin. The pane is fresh, so the cursor is at row 1, column 1.
    let script = r#"stty raw -echo; printf '\033[6n'; IFS= read -r -N 6 -t 5 pos; stty sane; printf 'got:%s\n' "${pos:1}""#;
    ctl.spawn_pty(0, &["bash", "-c", script]);
    let mut v = h.view(0);
    assert!(v.until_text(LONG, |t| t.contains("got:[1;1R")), "screen: {:?}", v.cache.text());
}

#[test]
fn notifications_reach_every_control_connection() {
    let h = Holder::start();
    let (mut a, mut b) = (h.ctl(), h.ctl());
    // OSC 9 and OSC 777 (both terminators) are notifications; OSC 9;4 is a progress report, not one.
    a.spawn_pty(0, &["bash", "-c", r#"printf '\033]9;one\007\033]777;notify;Claude;two; ok\033\\\033]9;4;1;50\007'; sleep 30"#]);
    for c in [&mut a, &mut b] {
        let first = c.event(LONG, |e| match e {
            HolderEvent::Notify { pane: 0, title, body } => Some((title.clone(), body.clone())),
            _ => None,
        });
        assert_eq!(first, Some((String::new(), "one".to_string())));
        let second = c.event(LONG, |e| match e {
            HolderEvent::Notify { pane: 0, title, body } => Some((title.clone(), body.clone())),
            _ => None,
        });
        assert_eq!(second, Some(("Claude".to_string(), "two; ok".to_string())));
        let third = c.event(Duration::from_millis(300), |e| matches!(e, HolderEvent::Notify { .. }).then_some(()));
        assert!(third.is_none(), "a progress report is not a notification");
    }
}

// ---- isolation -----------------------------------------------------------------------------

#[test]
fn a_view_that_stops_reading_is_dropped_and_nothing_else_notices() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["bash", "-c", "seq 1 3000; exec cat"]);
    let mut good = h.view(0);
    assert!(good.until_text(LONG, |t| t.contains("3000")));

    // A client that asks for lots of rows and never reads a byte.
    let mut stuck = UnixStream::connect(h.sock()).unwrap();
    let mut msgs = wire::encode(&HolderReq::Attach { pane: 0, epoch: 0, have_rev: 0, client_instance: 99 }).unwrap();
    for _ in 0..400 {
        msgs.extend(wire::encode(&HolderReq::Term(TermUp::GetRows { from_abs: 0, to_abs: 2000 })).unwrap());
    }
    stuck.write_all(&msgs).unwrap();

    // The holder neither stalls nor buffers without bound: other views keep working throughout,
    // and the stuck connection is closed once its queue overflows.
    good.key(1, 'g');
    good.until_echo(1);
    let t = Instant::now();
    assert!(ctl.screen(0).contains('g'));
    assert!(t.elapsed() < Duration::from_millis(500), "Screen took {:?}", t.elapsed());
    stuck.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut sink = vec![0u8; 1 << 16];
    let closed = loop {
        match stuck.read(&mut sink) {
            Ok(0) => break true,
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionReset) => break true,
            Err(e) => panic!("the stuck connection was not closed: {e}"),
        }
    };
    assert!(closed);
    // And the pane is none the worse.
    good.key(2, 'h');
    good.until_echo(2);
    assert_eq!(good.cache.text(), ctl.screen(0));
    assert!(ctl.info().panes[0].alive);
}

#[test]
fn views_come_and_go_without_touching_the_pane() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["cat"]);
    let mut keep = h.view_as(0, 0, 0, 1);
    for i in 0..20u32 {
        let mut gone = h.view_as(0, 0, 0, 100 + u128::from(i));
        gone.key(1, char::from(b'A' + i as u8));
        // Hung up at once, with the first frame unread: the key it sent must still arrive.
    }
    keep.key(1, 'k');
    assert!(keep.until_text(LONG, |t| t.contains('k')), "screen: {:?}", keep.cache.text());
    assert!(ctl.info().panes[0].alive);
    // The unattached pane keeps its screen: a later attach shows everything typed meanwhile.
    drop(keep);
    let mut late = h.view_as(0, 0, 0, 1);
    let all_there = late.until_text(LONG, |t| t.contains('k') && ('A'..='T').all(|c| t.contains(c)));
    assert!(all_there, "view: {:?}\npane: {:?}", late.cache.text(), ctl.screen(0));
}

// ---- process lifetime ----------------------------------------------------------------------

#[test]
fn shutdown_stops_children_writes_a_snapshot_and_exits_zero() {
    let mut h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty_sized(0, &["bash", "-c", r#"echo snapshot-me; printf '\033]0;the title\007'; sleep 30"#], 80, 24, &[]);
    assert_eq!(ctl.spawn_agent(&["sleep", "30"]), HolderResp::Ok);
    let mut v = h.view(0);
    assert!(v.until_text(LONG, |t| t.contains("snapshot-me")));
    let pids: Vec<u32> = {
        let i = ctl.info();
        vec![i.panes[0].pid, i.agent.unwrap().pid]
    };
    assert!(ctl.title_is("the title"));

    let t = Instant::now();
    ctl.ok(HolderReq::Shutdown);
    let st = h.wait_exit(Duration::from_secs(5)).expect("the holder did not exit after Shutdown");
    assert_eq!(st.code(), Some(0), "exit status {st}");
    assert!(t.elapsed() < Duration::from_secs(3));
    for pid in pids {
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "child {pid} outlived the holder");
    }
    assert!(!h.sock().exists(), "the socket is unlinked");

    let snap = Snapshot::read(&paths::snap_path(&h.state_dir(), h.sid)).expect("a snapshot on Shutdown");
    assert_eq!(snap.sid, h.sid);
    assert_eq!(snap.panes.len(), 1);
    assert_eq!((snap.panes[0].no, snap.panes[0].title.as_str()), (0, "the title"));
    assert!(snap.panes[0].screen_text.contains("snapshot-me"));
    assert!(snap.panes[0].last_200_rows.iter().any(|r| row_text(r).contains("snapshot-me")));
    assert!(!snap.agent_alive, "the agent was stopped before the snapshot was taken");
}

impl Ctl {
    /// Whether the control connection has been told pane 0's title (events or `Info`).
    fn title_is(&mut self, title: &str) -> bool {
        let end = Instant::now() + LONG;
        while Instant::now() < end {
            if self.info().panes.iter().any(|p| p.title == title) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

#[test]
fn shutdown_kills_children_that_ignore_sigterm() {
    let mut h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["bash", "-c", "trap '' TERM; echo ready; sleep 60"]);
    assert_eq!(ctl.spawn_agent(&["bash", "-c", "trap '' TERM; echo ready; sleep 60"]), HolderResp::Ok);
    // `ready` means the trap is installed (the agent's goes to the spool).
    assert_eq!(h.spool(0).spool_bytes(0, 6), b"ready\n");
    assert!(h.view(0).until_text(LONG, |t| t.contains("ready")));
    let pids: Vec<u32> = {
        let i = ctl.info();
        vec![i.panes[0].pid, i.agent.unwrap().pid]
    };
    let t = Instant::now();
    ctl.ok(HolderReq::Shutdown);
    let st = h.wait_exit(Duration::from_secs(10)).expect("holder did not exit");
    assert_eq!(st.code(), Some(0));
    let took = t.elapsed();
    assert!(took >= Duration::from_millis(1900) && took < Duration::from_secs(5), "SIGKILL after the 2 s grace: took {took:?}");
    for pid in pids {
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "child {pid} survived");
    }
}

#[test]
fn sigterm_is_a_shutdown_too() {
    let mut h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["sleep", "60"]);
    kill_process(Pid::from_raw(h.pid() as i32).unwrap(), Signal::TERM).unwrap();
    assert_eq!(h.wait_exit(Duration::from_secs(5)).and_then(|s| s.code()), Some(0));
    assert!(!h.sock().exists());
}

#[test]
fn idle_holder_exits_but_not_while_something_is_alive() {
    // Nothing alive and nobody connected: gone after the idle period.
    let mut h = Holder::start_with(&["--idle-exit-secs", "1", "--tick-ms", "100"], &[]);
    let st = h.wait_exit(Duration::from_secs(5)).expect("an idle holder should exit");
    assert_eq!(st.code(), Some(0));
    assert!(!h.sock().exists());

    // A live child keeps it running with nobody connected (the whole point: femmed may be away).
    let mut h = Holder::start_with(&["--idle-exit-secs", "1", "--tick-ms", "100"], &[]);
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["sleep", "60"]);
    drop(ctl);
    assert!(h.wait_exit(Duration::from_millis(2500)).is_none(), "the holder exited with a live child");
    // ... and when the child is gone too, it follows.
    let mut ctl = h.ctl();
    ctl.ok(HolderReq::Signal { target: Target::Pane(0), sig: 9 });
    assert_eq!(ctl.child_exit(Target::Pane(0)), Some(-9));
    drop(ctl);
    assert!(h.wait_exit(Duration::from_secs(5)).is_some(), "the holder should exit once nothing is alive");
}

#[test]
fn a_holder_with_nobody_connected_uses_no_cpu() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["sleep", "60"]);
    assert_eq!(ctl.spawn_agent(&["sleep", "60"]), HolderResp::Ok);
    let mut v = h.view(0);
    assert!(v.pump(LONG, |v| !v.frames.is_empty()));
    drop(ctl);
    // `v` stays attached to an idle pane; no control connection at all.
    thread::sleep(Duration::from_millis(300));
    let before = h.cpu_ticks();
    v.settle(Duration::from_millis(2000));
    let spent = h.cpu_ticks() - before;
    assert!(spent <= 1, "an idle holder burned {spent} ticks of CPU in 2 s");
    assert_eq!(v.frames.len(), 1, "an idle pane sends no frames");
}

#[test]
fn stale_sockets_are_replaced_live_ones_respected() {
    let dir = tempfile::tempdir().unwrap();
    let sid = Sid::new();
    let sock = paths::socket_path(&dir.path().join("rt"), sid);
    std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700).create(sock.parent().unwrap()).unwrap();
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap()); // leaves the file, nobody listening
    assert!(sock.exists());

    let h = Holder::start_at(dir, sid, &[], &[]);
    let mut ctl = h.ctl();
    assert_eq!(ctl.info().sid, sid);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&h.sock()), 0o600);
    assert_eq!(mode(h.sock().parent().unwrap()), 0o700);
    assert_eq!(mode(paths::spool_path(&h.state_dir(), sid).parent().unwrap()), 0o700);

    // A second holder for the same session must not take over (it would split-brain the PTYs).
    let out = Holder::command(h.dir.path(), sid, &[], &[]).stderr(Stdio::piped()).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already"), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(ctl.info().sid, sid, "the first holder is untouched");
}

#[test]
fn memory_with_one_pane_and_one_agent() {
    let h = Holder::start();
    let idle = h.rss_kb();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["bash", "--norc", "-i"]);
    assert_eq!(ctl.spawn_agent(&["cat"]), HolderResp::Ok);
    let mut v = h.view(0);
    assert!(v.pump(LONG, |v| !v.frames.is_empty()));
    v.key(1, 'l');
    v.until_echo(1);
    thread::sleep(Duration::from_millis(200));
    let rss = h.rss_kb();
    eprintln!("holder RSS: {idle} kB idle, {rss} kB with one bash pane and one cat agent (pid {})", h.pid());
    assert!(rss < 60_000, "{rss} kB");
}

#[test]
fn memory_with_a_full_scrollback() {
    let h = Holder::start();
    let idle = h.rss_kb();
    let mut ctl = h.ctl();
    // 30 000 numbered lines through an 80 column pane: the 10 000 line scrollback is full.
    ctl.spawn_pty(0, &["bash", "-c", "seq 1 30000; echo done-printing; sleep 30"]);
    let mut v = h.view(0);
    assert!(v.until_text(Duration::from_secs(30), |t| t.contains("done-printing")));
    thread::sleep(Duration::from_millis(200));
    let rss = h.rss_kb();
    eprintln!("holder RSS: {idle} kB idle, {rss} kB with one pane whose scrollback is full (pid {})", h.pid());
    assert!(rss < 120_000, "{rss} kB");
    // Only the last 10 000 lines (and the screen) are kept; abs row `a` holds the number `a + 1`.
    let (lo, top) = (v.last().history_lo_abs, v.last().top_abs);
    assert_eq!(top - lo, 10_000, "scrollback length");
    v.up(TermUp::GetRows { from_abs: lo, to_abs: lo + 3 });
    assert!(v.pump(LONG, |v| !v.rows.is_empty()));
    let first = v.rows[0].first().map(row_text).unwrap_or_default();
    assert_eq!(first.trim(), (lo + 1).to_string(), "oldest kept line");
}

// ---- 8. what the audit found ---------------------------------------------------------------

/// Events are lossy and never cost a control connection: 400 title changes and 200 notifications,
/// each in one write, become a handful of events, and both controls stay usable.
#[test]
fn title_and_notify_bursts_are_coalesced_and_cost_no_control_connection() {
    let h = Holder::start();
    let (mut a, mut b) = (h.ctl(), h.ctl());
    // The strings are built first so that each printf is one write to the pty.
    let script = r#"
        t=; for i in $(seq 400); do t="$t\033]0;burst$i\007"; done
        n=; for i in $(seq 200); do n="$n\033]9;note$i\007"; done
        printf "$t"; printf "$n"; exit 5"#;
    a.spawn_pty(0, &["bash", "-c", script]);
    for c in [&mut a, &mut b] {
        assert_eq!(c.child_exit(Target::Pane(0)), Some(5), "ChildExited must still arrive");
        // Everything that came before the exit is in `events`.
        let titles: Vec<_> = c.events.iter().filter_map(|e| if let HolderEvent::Title { title, .. } = e { Some(title.as_str()) } else { None }).collect();
        let notes = c.events.iter().filter(|e| matches!(e, HolderEvent::Notify { .. })).count();
        eprintln!("400 titles became {} events, 200 notifications became {notes}", titles.len());
        assert_eq!(titles.last(), Some(&"burst400"), "the last title goes out, before the exit: {titles:?}");
        assert!(titles.len() <= 6, "{} title events", titles.len());
        assert!((1..=20).contains(&notes), "{notes} notify events");
        // The connection still answers requests, and events after the burst still arrive.
        assert_eq!(c.info().panes[0].title, "burst400");
    }
    assert_eq!(a.spawn_agent(&["bash", "-c", "echo oops >&2; exit 3"]), HolderResp::Ok);
    for c in [&mut a, &mut b] {
        assert_eq!(c.child_exit(Target::Agent), Some(3));
        assert!(c.events.contains(&HolderEvent::AgentStderr("oops\n".into())), "events: {:?}", c.events);
    }
}

/// One cell with 4.6 million combining characters (U+1D167 after an `a`, repeated by `CSI 65535 b`)
/// makes a row that fits in no 16 MiB message: not in a frame, not in `Screen`. That must end in an
/// `Err` for whoever asked, never in silence, and nothing else may be harmed.
#[test]
fn a_screen_too_big_for_one_message_is_refused_with_an_error() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    let go = h.dir.path().join("go");
    let script = r#"printf 'a\360\235\205\247'; for i in $(seq 70); do printf '\033[65535b'; done; echo; echo flooded; while [ ! -e "$GO" ]; do sleep 0.05; done; printf '\033c'; echo cleared; sleep 30"#;
    ctl.spawn_pty_sized(0, &["bash", "-c", script], 80, 24, &[("GO", go.to_str().unwrap())]);
    let mut early = h.view(0);
    early.allow_err = true;

    // `Screen` says why it cannot answer once the flood is on the screen (the old holder closed the
    // control connection without a word), and the connection stays usable.
    let t0 = Instant::now();
    let err = loop {
        match ctl.req(HolderReq::Screen { pane: 0, fmt: ScreenFmt::Text }) {
            HolderResp::Err(e) => break e,
            HolderResp::Screen(t) if t.contains("flooded") => {
                eprintln!("skipped: the emulator caps a cell's combining characters now, so no screen is too big for a message");
                return;
            }
            HolderResp::Screen(_) => {}
            other => panic!("unexpected reply to Screen: {other:?}"),
        }
        assert!(t0.elapsed() < LONG, "the flood never showed up");
        thread::sleep(Duration::from_millis(50));
    };
    eprintln!("flood on screen after {:?}; Screen says: {err}", t0.elapsed());
    assert!(err.contains("reply not sent") && err.contains("exceeds"), "Screen error: {err}");
    assert!(ctl.info().panes[0].alive);

    // A view that was open gets the error too (its next frame did not fit) and is closed; before,
    // the frame vanished and the view stayed open, out of step for good.
    assert!(early.pump(LONG, |v| !v.errs.is_empty()), "the open view heard nothing");
    assert!(early.errs[0].contains("too large"), "view error: {:?}", early.errs);
    assert!(early.c.closed_within(LONG), "the view should be closed after its error");
    // So does a new one.
    let mut late = Conn::open(&h.sock());
    late.send(&HolderReq::Attach { pane: 0, epoch: 0, have_rev: 0, client_instance: 7 });
    match late.recv(LONG) {
        Some((_, HolderResp::Err(e))) => assert!(e.contains("too large"), "attach error: {e}"),
        other => panic!("an attach to a screen too big to send should get an Err, got {other:?}"),
    }
    assert!(late.closed_within(LONG));

    // Once the row is gone, everything works again.
    std::fs::write(&go, b"").unwrap();
    while !ctl.screen_is(|t| t.contains("cleared")) {
        assert!(t0.elapsed() < LONG * 2, "the screen was never cleared");
        thread::sleep(Duration::from_millis(50));
    }
    let mut v = h.view(0);
    assert!(v.until_text(LONG, |t| t.contains("cleared")), "screen: {:?}", v.cache.text());
    assert!(ctl.info().panes[0].alive);
}

#[test]
fn open_on_an_open_view_resyncs_it_from_what_the_client_holds() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    ctl.spawn_pty(0, &["bash", "-c", "echo one; read x; echo two:$x; read y; echo three:$y; sleep 30"]);
    let mut v = h.view(0);
    v.resync = true;
    assert!(v.until_text(LONG, |t| t.contains("one")));
    v.settle(Duration::from_millis(100));
    let (n, epoch) = (v.frames.len(), v.last().epoch);

    // `have_rev` 0 is what a client sends after `EpochChanged`: it holds nothing, and gets a full
    // snapshot, although the screen has not changed since its last frame.
    v.up(TermUp::Open { pane: h.pane_id(0), epoch, have_rev: 0, client_instance: 1 });
    assert!(v.pump(LONG, |v| v.frames.len() > n), "Open with have_rev 0 got no frame");
    assert_eq!(v.last().base_rev, 0, "a snapshot");
    assert_eq!(v.cache.text(), ctl.screen(0));

    // A client that lost a frame cannot apply the next one (`NeedResync`), re-sends `Open` with the
    // rev it holds, and is brought up to date by a delta from there.
    let held = v.cache.rev;
    v.lose = 1;
    v.up(TermUp::Bytes { seq: 1, data: b"a\n".to_vec() });
    assert!(v.pump(LONG, |v| v.lose == 0), "no frame to lose");
    v.up(TermUp::Bytes { seq: 2, data: b"b\n".to_vec() });
    assert!(v.until_text(LONG, |t| t.contains("two:a") && t.contains("three:b")), "stuck at: {:?}", v.cache.text());
    assert_eq!(v.resyncs.first(), Some(&ApplyError::NeedResync { have_rev: held }));
    assert!(v.frames.iter().any(|(_, f)| f.base_rev == held), "the answer is a delta from the rev the client held");
    assert_eq!(v.cache.text(), ctl.screen(0));
}

#[test]
fn agent_stdin_backlog_is_capped_in_bytes() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    let go = h.dir.path().join("go");
    // An agent that reads nothing until `go` exists, then copies its stdin to the spool.
    let env = vec![("GO".to_string(), go.to_str().unwrap().to_string())];
    let script = r#"while [ ! -e "$GO" ]; do sleep 0.05; done; exec cat"#;
    ctl.ok(HolderReq::SpawnAgent { cmd: strings(&["bash", "-c", script]), cwd: String::new(), env });
    let before = h.rss_kb();
    let mib = vec![b'x'; 1 << 20];
    let mut accepted = 0u64;
    let err = loop {
        match ctl.req(HolderReq::AgentWrite(mib.clone())) {
            HolderResp::Ok => accepted += 1,
            HolderResp::Err(e) => break e,
            other => panic!("unexpected reply: {other:?}"),
        }
        assert!(accepted < 64, "no backlog limit after {accepted} MiB");
    };
    let grew = h.rss_kb().saturating_sub(before);
    eprintln!("{accepted} MiB accepted before: {err}; the holder grew by {grew} kB");
    assert_eq!(accepted, 8, "an 8 MiB budget");
    assert!(err.contains("backlog"), "error: {err}");
    assert!(grew < 40_000, "the holder grew by {grew} kB");

    // Once the agent reads, the backlog drains into the spool and room is free again.
    std::fs::write(&go, b"").unwrap();
    ctl.wait_spool(accepted << 20);
    ctl.ok(HolderReq::AgentWrite(mib.clone()));
    ctl.wait_spool((accepted + 1) << 20);
    ctl.ok(HolderReq::AgentCloseStdin);
    assert_eq!(ctl.child_exit(Target::Agent), Some(0));

    // One message larger than the whole budget is taken when nothing waits (a client cannot split
    // what it cannot send); while it waits nothing else is, except the EOF, which is not bytes.
    assert_eq!(ctl.spawn_agent(&["sleep", "30"]), HolderResp::Ok);
    ctl.ok(HolderReq::AgentWrite(vec![b'y'; 12 << 20]));
    assert!(ctl.err(HolderReq::AgentWrite(b"z".to_vec())).contains("backlog"));
    ctl.ok(HolderReq::AgentCloseStdin);
}

/// A write to the spool that fails half way (disk full) must not leave its bytes behind: the retry
/// writes them again, and a copy before the recorded end is a corrupt line there.
#[test]
fn a_spool_write_that_fails_half_way_leaves_no_stray_bytes() {
    let h = Holder::start_ignoring_xfsz();
    let mut ctl = h.ctl();
    h.limit_files(Some(2048)); // room for 2048 of the 3000 bytes the agent is about to print
    let line = format!("{}\n", "a".repeat(2999));
    assert_eq!(ctl.spawn_agent(&["bash", "-c", "s=$(printf 'a%.0s' $(seq 2999)); echo $s; sleep 30"]), HolderResp::Ok);
    thread::sleep(Duration::from_millis(1500)); // at least one failed attempt
    let hi = ctl.info().agent.unwrap().spool_hi;
    assert_eq!(hi, 0, "the write must be stalled by the size limit");

    h.limit_files(None);
    ctl.wait_spool(line.len() as u64);
    let path = paths::spool_path(&h.state_dir(), h.sid);
    assert_eq!(std::fs::read(&path).unwrap(), line.as_bytes(), "the spool holds the line once");
    assert_eq!(h.spool(0).spool_bytes(0, line.len()), line.as_bytes());
}

#[test]
fn hello_with_the_largest_version_number_is_refused_not_a_crash() {
    let h = Holder::start();
    for v in [u16::MAX, HOLDER_VERSION + 1] {
        let mut c = Conn::open(&h.sock());
        c.send(&HolderReq::Hello { v });
        assert!(matches!(c.recv(LONG), Some((_, HolderResp::Err(e))) if e.contains("unsupported")), "Hello v{v}");
        assert!(c.closed_within(LONG));
    }
    assert_eq!(h.ctl().hello.v, HOLDER_VERSION, "the holder is fine");
}

/// `alive` must go false when the child is reaped, not when its pipes have drained: for up to a
/// second afterwards its pid could be reused while a `Signal` still aimed at it.
#[test]
fn a_reaped_agent_is_dead_in_info_before_its_exit_is_announced() {
    let h = Holder::start();
    let mut ctl = h.ctl();
    // Exits at once, leaving a grandchild that holds its stdout for 3 s: the holder waits out its
    // 1 s pipe grace before it announces the exit.
    assert_eq!(ctl.spawn_agent(&["bash", "-c", "sleep 3 & exit 3"]), HolderResp::Ok);
    let t0 = Instant::now();
    let a = loop {
        let a = ctl.info().agent.unwrap();
        if !a.alive {
            break a;
        }
        assert!(t0.elapsed() < LONG, "the agent never showed as dead");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(a.exit, Some(3));
    assert!(!ctl.events.iter().any(|e| matches!(e, HolderEvent::ChildExited { .. })), "Info said dead only after the exit was announced");
    assert!(ctl.err(HolderReq::Signal { target: Target::Agent, sig: 15 }).contains("no agent"));
    // The exit is not announced yet, so a new agent would make that announcement ambiguous.
    let respawn = ctl.spawn_agent(&["true"]);
    assert!(matches!(&respawn, HolderResp::Err(e) if e.contains("shutting down")), "{respawn:?}");
    assert_eq!(ctl.child_exit(Target::Agent), Some(3));
    assert_eq!(ctl.spawn_agent(&["true"]), HolderResp::Ok);
    assert_eq!(ctl.child_exit(Target::Agent), Some(0));
}

/// A panic in an emulator call at `site` is contained: the pane's emulator is replaced (new epoch,
/// every view gets a snapshot of it), the child keeps running, and its exit is still announced.
#[cfg(debug_assertions)]
fn panic_is_contained_at(site: &str, args: &[&str]) {
    let h = Holder::start_with(args, &[("FEMME_HOLDER_PANIC_AT", site)]);
    let mut ctl = h.ctl();
    let first_epoch = ctl.hello.epoch; // a pane's first epoch is the holder's own
    ctl.spawn_pty(0, &["bash", "-c", "echo first; read x; echo second:$x; exit 6"]);
    let mut v = h.view(0);
    v.resync = true; // the cache refuses the new epoch and the client opens the pane again, as the contract says
    assert!(
        v.pump(LONG, |v| v.frames.iter().any(|(_, f)| f.epoch == first_epoch + 1)),
        "{site}: no frame from a reset pane (epochs seen: {:?}, errors: {:?})",
        v.frames.iter().map(|(_, f)| f.epoch).collect::<Vec<_>>(),
        v.errs
    );
    assert!(ctl.info().panes[0].alive, "{site}: the pane task is gone");

    v.up(TermUp::Bytes { seq: 1, data: b"go\n".to_vec() });
    assert!(v.until_text(LONG, |t| t.contains("second:go")), "{site}: screen {:?}", v.cache.text());
    assert_eq!(v.cache.text(), ctl.screen(0));
    assert_eq!(ctl.child_exit(Target::Pane(0)), Some(6), "{site}: the exit must still be announced");
    assert!(v.pump(LONG, |v| v.exited == Some(6)));
    // A view attached afterwards gets the reset pane's screen, not a closed connection.
    let mut late = h.view(0);
    assert!(late.pump(LONG, |v| !v.frames.is_empty()), "{site}: a later attach got nothing");
    assert_eq!(late.last().epoch, first_epoch + 1);
    if site == "snap" {
        let snap = loop {
            if let Ok(s) = Snapshot::read(&paths::snap_path(&h.state_dir(), h.sid)) {
                break s;
            }
            thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(snap.panes.len(), 1, "the snapshot still lists the pane");
    }
}

#[cfg(debug_assertions)]
#[test]
fn an_emulator_panic_while_parsing_output_resets_the_pane() {
    panic_is_contained_at("feed", &[]);
}

#[cfg(debug_assertions)]
#[test]
fn an_emulator_panic_while_arming_the_sync_timer_resets_the_pane() {
    panic_is_contained_at("sync", &[]);
}

#[cfg(debug_assertions)]
#[test]
fn an_emulator_panic_while_attaching_a_view_resets_the_pane() {
    panic_is_contained_at("attach", &[]);
}

#[cfg(debug_assertions)]
#[test]
fn an_emulator_panic_while_taking_a_snapshot_resets_the_pane() {
    panic_is_contained_at("snap", &["--tick-ms", "200"]);
}

// ---- 9. under systemd ----------------------------------------------------------------------

fn systemctl(args: &[&str]) -> String {
    let out = Command::new("systemctl").arg("--user").args(args).output().expect("systemctl");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Stops the transient unit even when the test fails half way.
struct Unit(String);

impl Drop for Unit {
    fn drop(&mut self) {
        let _ = Command::new("systemctl").args(["--user", "stop", &self.0]).output();
        let _ = Command::new("systemctl").args(["--user", "reset-failed", &self.0]).output();
    }
}

#[test]
#[ignore = "needs a systemd user manager; run with `cargo test -p femme-holder -- --ignored`"]
fn runs_as_a_systemd_transient_unit() {
    let dir = tempfile::tempdir().unwrap();
    let sid = Sid::new();
    let unit = format!("femme-s-test-{}", sid.to_string()[16..].to_lowercase());
    let _guard = Unit(unit.clone());
    let out = Command::new("systemd-run")
        .args(["--user", &format!("--unit={unit}"), "--service-type=notify", "--collect"])
        .args(["-p", "KillMode=control-group", "-p", "OOMPolicy=continue", "-p", "CPUWeight=100", "-p", "TasksMax=4096"])
        // The holder must ping the watchdog by itself or systemd kills it two seconds in.
        .args(["-p", "WatchdogSec=2"])
        .arg(env!("CARGO_BIN_EXE_femme-holder"))
        .arg("--sid")
        .arg(sid.to_string())
        .arg("--runtime-dir")
        .arg(dir.path().join("rt"))
        .arg("--state-dir")
        .arg(dir.path().join("st"))
        .output()
        .expect("systemd-run");
    assert!(out.status.success(), "systemd-run failed: {}", String::from_utf8_lossy(&out.stderr));
    // `--service-type=notify` means systemd-run returned only after READY=1.
    assert_eq!(systemctl(&["is-active", &format!("{unit}.service")]), "active");

    let sock = paths::socket_path(&dir.path().join("rt"), sid);
    let mut ctl = Ctl::connect(&sock);
    ctl.spawn_pty(0, &["sleep", "60"]);
    let pid = ctl.info().panes[0].pid;
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    assert!(cgroup.contains(&format!("{unit}.service")), "the pane's child must live in the holder's unit: {cgroup}");

    thread::sleep(Duration::from_millis(4500)); // two missed watchdog periods
    assert_eq!(systemctl(&["is-active", &format!("{unit}.service")]), "active", "the watchdog killed the holder");
    assert_eq!(ctl.info().panes.len(), 1);

    ctl.ok(HolderReq::Shutdown);
    let end = Instant::now() + Duration::from_secs(10);
    while systemctl(&["is-active", &format!("{unit}.service")]) == "active" || systemctl(&["is-active", &format!("{unit}.service")]) == "deactivating" {
        assert!(Instant::now() < end, "the unit did not stop after Shutdown");
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(systemctl(&["is-active", &format!("{unit}.service")]), "inactive");
    assert_eq!(systemctl(&["show", "-p", "LoadState", "--value", &format!("{unit}.service")]), "not-found", "--collect removes the unit");
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    assert!(Snapshot::read(&paths::snap_path(&dir.path().join("st"), sid)).is_ok());
}
