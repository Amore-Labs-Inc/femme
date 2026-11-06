//! Black-box tests of the `femmed` binary's command line, run as a subprocess:
//!
//! - `serve` exits 1 with the reason when it cannot bind;
//! - `hook <agent> <event> [PAYLOAD]` posts its JSON envelope to `http://$HTTP_BIND/internal/hook`
//!   as a bare HTTP/1.1 request, reads stdin for at most 2 s (not at all when the payload is an
//!   argument), sends nothing without `FEMME_SID`, and always exits 0;
//! - `proxy` relays bytes both ways between stdio and `$XDG_RUNTIME_DIR/femme/femmed.sock`: EOF on
//!   stdin half-closes the socket (the daemon's answer still comes), and it exits as soon as the
//!   daemon hangs up, even while its stdin is still open.
//!
//! Every run starts from an empty environment plus `HOME` (a fresh temporary directory, so the
//! developer's `~/femme/.env` cannot leak in), the test's `PATH` and the variables under test. The
//! daemon is played by std listeners on threads, and every wait has a ceiling, so a regression
//! fails its test instead of hanging the suite.

use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;

/// How long one `femmed` run may take, from spawn, before it is killed and its test fails.
const CEILING: Duration = Duration::from_secs(10);

/// What a Claude `Stop` hook writes to stdin.
const STOP_PAYLOAD: &[u8] = br#"{"session_id":"abc"}"#;

/// What `femmed hook claude Stop` posts for [`STOP_PAYLOAD`] with `FEMME_SID=s1`.
fn stop_envelope() -> Value {
    json!({"session_id": "abc", "agent": "claude", "event": "Stop", "sid": "s1"})
}

// ---- running femmed ------------------------------------------------------------------------------

/// A fresh empty directory under /tmp: a run's `HOME`, and its `XDG_RUNTIME_DIR` for `proxy`. Not
/// under `$TMPDIR`: a unix socket path must fit in 108 bytes.
fn sandbox() -> TempDir {
    tempfile::Builder::new()
        .prefix("femmed")
        .tempdir_in("/tmp")
        .expect("create a temporary directory")
}

/// `femmed <args>` with nothing in its environment but `HOME=<home>`, `XDG_RUNTIME_DIR=<home>` and
/// the test's `PATH`, and /dev/null on stdin and stdout; the test adds what it is about. The runtime
/// dir is never left to default: unset, femmed falls back to `/run/user/<uid>`, where the live
/// daemon's holders are, and a `serve` there stops every one of them as "of no open session".
fn femmed(home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_femmed"));
    cmd.args(args)
        .env_clear()
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd
}

/// A running `femmed`. Its stderr is collected on a thread, for failure messages; dropping it kills
/// the process if it is still running, so a test that fails half-way leaves nothing behind.
struct Running {
    child: Child,
    started: Instant,
    stderr: Option<JoinHandle<String>>,
}

/// How a run ended.
struct Exited {
    status: ExitStatus,
    /// From spawn to exit, give or take the 20 ms poll.
    elapsed: Duration,
    stderr: String,
}

impl fmt::Display for Exited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "femmed {} after {:.2?}, stderr: {:?}",
            self.status, self.elapsed, self.stderr
        )
    }
}

impl Running {
    fn spawn(cmd: &mut Command) -> Running {
        let started = Instant::now();
        let mut child = cmd.stderr(Stdio::piped()).spawn().expect("spawn femmed");
        let mut pipe = child.stderr.take().expect("piped stderr");
        let stderr = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        });
        Running {
            child,
            started,
            stderr: Some(stderr),
        }
    }

    /// Waits for the exit, polling every 20 ms. Past [`CEILING`] the process is killed and the test
    /// fails: a hook stuck on its stdin or a proxy that never exits must not hang the suite.
    fn wait(&mut self) -> Exited {
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return Exited {
                    status,
                    elapsed: self.started.elapsed(),
                    stderr: self.take_stderr(),
                };
            }
            if self.started.elapsed() > CEILING {
                self.fail(&format!("still running after {CEILING:?}"));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Kills the process and fails the test with `why` and what the process wrote to stderr.
    fn fail(&mut self, why: &str) -> ! {
        let _ = self.child.kill();
        let status = self
            .child
            .wait()
            .map_or_else(|e| e.to_string(), |s| s.to_string());
        panic!(
            "{why}; femmed killed ({status}), stderr: {:?}",
            self.take_stderr()
        );
    }

    fn take_stderr(&mut self) -> String {
        self.stderr
            .take()
            .map(|reader| reader.join().unwrap_or_default())
            .unwrap_or_default()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Runs `cmd` to its exit with `input` on its stdin, then EOF.
fn run(cmd: &mut Command, input: &[u8]) -> Exited {
    let mut running = Running::spawn(cmd.stdin(Stdio::piped()));
    let mut stdin = running.child.stdin.take().expect("piped stdin");
    // A run that exits without reading stdin (no FEMME_SID, a usage error) breaks the pipe. Not an
    // error here: an input that did not get through shows in the test's own assertions.
    let _ = stdin.write_all(input);
    drop(stdin);
    running.wait()
}

// ---- a fake daemon for `hook` ----------------------------------------------------------------------

/// A request as the fake daemon read it: the head (request line, headers, blank line) and the body.
struct Request {
    head: String,
    body: Vec<u8>,
}

impl Request {
    /// The header lines as (name, value), in order.
    fn headers(&self) -> Vec<(&str, &str)> {
        self.head
            .split("\r\n")
            .skip(1)
            .filter(|line| !line.is_empty())
            .map(|line| {
                line.split_once(':')
                    .map_or((line, ""), |(name, value)| (name.trim(), value.trim()))
            })
            .collect()
    }

    /// The value of the header `name`, in any case.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers()
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }

    /// The body as JSON, or a failed test that shows it.
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "the body is not JSON ({e}): {:?}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }
}

/// A listener on a free loopback port, and its address for `HTTP_BIND`.
fn http_listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr").to_string();
    (listener, addr)
}

/// Plays the daemon's `/internal/hook` for one connection: reads the head up to the blank line and
/// `Content-Length` bytes of body, hands the request to the test, and answers 204.
fn serve_one(listener: TcpListener) -> mpsc::Receiver<Request> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        conn.set_read_timeout(Some(CEILING))
            .expect("set_read_timeout");
        let mut buf = Vec::new();
        let mut chunk = [0; 4096];
        let head_len = loop {
            if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            let n = conn.read(&mut chunk).expect("read the request head");
            assert!(
                n > 0,
                "EOF inside the request head: {:?}",
                String::from_utf8_lossy(&buf)
            );
            buf.extend_from_slice(&chunk[..n]);
        };
        let body = buf.split_off(head_len);
        let mut req = Request {
            head: String::from_utf8(buf).expect("a UTF-8 request head"),
            body,
        };
        let len: usize = req
            .header("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("no Content-Length: {:?}", req.head));
        let have = req.body.len();
        assert!(
            have <= len,
            "more body than Content-Length {len}: {:?}",
            String::from_utf8_lossy(&req.body)
        );
        req.body.resize(len, 0);
        conn.read_exact(&mut req.body[have..])
            .expect("read the request body");
        let _ = tx.send(req);
        // Whether femmed waits for the answer is its own business.
        let _ = conn.write_all(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
    });
    rx
}

/// The request the fake daemon got, or a failed test that says how the run ended.
fn received(requests: &mpsc::Receiver<Request>, out: &Exited) -> Request {
    requests
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("the fake daemon got no request ({e}); {out}"))
}

// ---- a fake daemon for `proxy` ---------------------------------------------------------------------

/// Binds the daemon's local socket, `<runtime>/femme/femmed.sock`.
fn daemon_socket(runtime: &Path) -> UnixListener {
    std::fs::create_dir(runtime.join("femme")).expect("create <runtime>/femme");
    let sock = runtime.join("femme/femmed.sock");
    UnixListener::bind(&sock).unwrap_or_else(|e| panic!("bind {}: {e}", sock.display()))
}

/// The daemon's side of one proxy connection: reads exactly `n` bytes, writes them back, then holds
/// the connection open until `hang_up` fires (or its sender is dropped).
fn echo_once(listener: UnixListener, n: usize, hang_up: mpsc::Receiver<()>) -> JoinHandle<()> {
    thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        conn.set_read_timeout(Some(CEILING))
            .expect("set_read_timeout");
        let mut buf = vec![0; n];
        conn.read_exact(&mut buf).expect("read what the proxy sent");
        conn.write_all(&buf).expect("echo it back");
        let _ = hang_up.recv();
    })
}

/// The daemon's side of one proxy connection: reads exactly `n` bytes, writes them back, waits for
/// the proxy's EOF (its stdin closed), answers `bye` and hangs up.
fn echo_then_answer_eof(listener: UnixListener, n: usize) -> JoinHandle<()> {
    thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        conn.set_read_timeout(Some(CEILING))
            .expect("set_read_timeout");
        let mut buf = vec![0; n];
        conn.read_exact(&mut buf).expect("read what the proxy sent");
        conn.write_all(&buf).expect("echo it back");
        assert_eq!(
            conn.read(&mut [0; 16]).expect("read to the proxy's EOF"),
            0,
            "stdin's EOF must reach the daemon"
        );
        conn.write_all(b"bye").expect("answer after EOF");
    })
}

/// `femmed proxy` with `XDG_RUNTIME_DIR=<runtime>` and piped stdin and stdout.
fn spawn_proxy(runtime: &Path) -> Running {
    Running::spawn(
        femmed(runtime, &["proxy"])
            .env("XDG_RUNTIME_DIR", runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped()),
    )
}

/// Reads exactly `n` bytes of the proxy's stdout, which comes back with them (still open), or kills
/// the proxy and fails the test after 5 s.
fn read_exactly(
    running: &mut Running,
    mut stdout: ChildStdout,
    n: usize,
) -> (Vec<u8>, ChildStdout) {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = vec![0; n];
        let read = stdout.read_exact(&mut buf).map(|()| buf);
        let _ = tx.send((read, stdout));
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok((Ok(buf), stdout)) => (buf, stdout),
        Ok((Err(e), _)) => running.fail(&format!("reading femmed's stdout: {e}")),
        Err(_) => running.fail("nothing came back on femmed's stdout within 5 s"),
    }
}

// ---- serve ---------------------------------------------------------------------------------------

#[test]
fn serve_exits_1_when_it_cannot_bind() {
    let home = sandbox();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let out = run(
        femmed(home.path(), &["serve"]).env("HTTP_BIND", taken.local_addr().unwrap().to_string()),
        b"",
    );
    assert_eq!(out.status.code(), Some(1), "{out}");
    assert!(out.stderr.contains("cannot bind"), "{out}");
}

// ---- hook ----------------------------------------------------------------------------------------

#[test]
fn hook_posts_the_envelope_to_internal_hook() {
    let home = sandbox();
    let (listener, addr) = http_listener();
    let requests = serve_one(listener);
    let out = run(
        femmed(home.path(), &["hook", "claude", "Stop"])
            .env("FEMME_SID", "s1")
            .env("HTTP_BIND", addr),
        STOP_PAYLOAD,
    );
    assert_eq!(out.status.code(), Some(0), "{out}");
    let req = received(&requests, &out);
    assert!(
        req.head.starts_with("POST /internal/hook HTTP/1.1\r\n"),
        "{:?}",
        req.head
    );
    // These four and nothing else: the route is loopback-only and refuses anything that looks
    // proxied (X-Forwarded-For, Forwarded, X-Real-IP).
    let mut names: Vec<String> = req
        .headers()
        .iter()
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["connection", "content-length", "content-type", "host"],
        "{:?}",
        req.head
    );
    assert_eq!(
        req.header("content-type"),
        Some("application/json"),
        "{:?}",
        req.head
    );
    assert_eq!(req.header("connection"), Some("close"), "{:?}", req.head);
    assert_eq!(req.json(), stop_envelope());
}

#[test]
fn hook_sends_what_it_read_when_stdin_stays_open() {
    let home = sandbox();
    let (listener, addr) = http_listener();
    let requests = serve_one(listener);
    let mut cmd = femmed(home.path(), &["hook", "claude", "Stop"]);
    let mut running = Running::spawn(
        cmd.env("FEMME_SID", "s1")
            .env("HTTP_BIND", addr)
            .stdin(Stdio::piped()),
    );
    let mut stdin = running.child.stdin.take().expect("piped stdin");
    stdin.write_all(STOP_PAYLOAD).expect("write femmed's stdin");
    let out = running.wait();
    drop(stdin); // held open until femmed exited
    assert_eq!(out.status.code(), Some(0), "{out}");
    assert!(
        out.elapsed < Duration::from_secs(4),
        "an open stdin may cost its 2 s wait, no more: {out}"
    );
    assert_eq!(received(&requests, &out).json(), stop_envelope());
}

#[test]
fn hook_without_femme_sid_sends_nothing() {
    for sid in [None, Some("")] {
        let home = sandbox();
        let (listener, addr) = http_listener();
        let mut cmd = femmed(home.path(), &["hook", "claude", "Stop"]);
        cmd.env("HTTP_BIND", addr);
        if let Some(sid) = sid {
            cmd.env("FEMME_SID", sid);
        }
        let out = run(&mut cmd, STOP_PAYLOAD);
        assert_eq!(out.status.code(), Some(0), "FEMME_SID={sid:?}: {out}");
        // femmed has exited, so a connection it made would already sit in the listen backlog.
        listener.set_nonblocking(true).expect("set_nonblocking");
        match listener.accept() {
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Ok((_, peer)) => panic!("FEMME_SID={sid:?}: femmed connected from {peer}; {out}"),
            Err(e) => panic!("FEMME_SID={sid:?}: accept: {e}; {out}"),
        }
    }
}

#[test]
fn hook_exits_0_when_the_daemon_is_down() {
    let home = sandbox();
    let dead = http_listener().1; // bound, then dropped: nothing listens there
    let out = run(
        femmed(home.path(), &["hook", "claude", "Stop"])
            .env("FEMME_SID", "s1")
            .env("HTTP_BIND", dead),
        STOP_PAYLOAD,
    );
    assert_eq!(out.status.code(), Some(0), "{out}");
    assert!(out.elapsed < Duration::from_secs(5), "{out}");
}

#[test]
fn hook_payload_argument_is_posted_and_stdin_is_not_read() {
    let home = sandbox();
    let (listener, addr) = http_listener();
    let requests = serve_one(listener);
    let mut cmd = femmed(
        home.path(),
        &[
            "hook",
            "codex",
            "agent-turn-complete",
            r#"{"type":"agent-turn-complete"}"#,
        ],
    );
    let mut running = Running::spawn(
        cmd.env("FEMME_SID", "s1")
            .env("HTTP_BIND", addr)
            .stdin(Stdio::piped()),
    );
    let mut stdin = running.child.stdin.take().expect("piped stdin");
    // Kept open, so reading it would cost the 2 s stdin wait. A broken pipe only means femmed is done.
    let _ = stdin.write_all(br#"{"from":"stdin"}"#);
    let out = running.wait();
    drop(stdin);
    assert_eq!(out.status.code(), Some(0), "{out}");
    let body = received(&requests, &out).json();
    assert!(body.get("from").is_none(), "stdin was read: {body}");
    assert_eq!(
        body,
        json!({"type": "agent-turn-complete", "agent": "codex", "event": "agent-turn-complete", "sid": "s1"})
    );
    assert!(
        out.elapsed < Duration::from_secs(2),
        "waited on stdin although the payload was an argument: {out}"
    );
}

#[test]
fn usage_errors_exit_0_under_hook_and_2_otherwise() {
    let home = sandbox();
    // A hook that exits non-zero can block its agent (for Claude, 2 blocks the tool call), so a
    // malformed `hook` line still exits 0. With a FEMME_SID and a dead daemon address, the usage
    // error is the only thing left to react to.
    let dead = http_listener().1;
    let out = run(
        femmed(home.path(), &["hook", "nosuchagent", "x"])
            .env("FEMME_SID", "s1")
            .env("HTTP_BIND", dead),
        b"{}",
    );
    assert_eq!(out.status.code(), Some(0), "{out}");
    let out = run(&mut femmed(home.path(), &["nosuchcommand"]), b"");
    assert_eq!(out.status.code(), Some(2), "{out}");
}

// ---- proxy ---------------------------------------------------------------------------------------

#[test]
fn proxy_relays_bytes_and_exits_when_the_daemon_hangs_up() {
    let dir = sandbox();
    let listener = daemon_socket(dir.path());
    // Not UTF-8 (a NUL, 0xff 0xfe, a broken sequence, a CR LF), then every byte value.
    let mut sent = b"\x00\xff\xfe\xc3\x28\r\n".to_vec();
    sent.extend((0..=255u8).cycle().take(4096));
    let (hang_up, hung_up) = mpsc::channel();
    let daemon = echo_once(listener, sent.len(), hung_up);
    let mut running = spawn_proxy(dir.path());
    let mut stdin = running.child.stdin.take().expect("piped stdin");
    let stdout = running.child.stdout.take().expect("piped stdout");
    stdin.write_all(&sent).expect("write femmed's stdin");
    let (got, stdout) = read_exactly(&mut running, stdout, sent.len());
    assert!(
        got == sent,
        "the bytes came back changed, first at byte {:?}",
        got.iter().zip(&sent).position(|(a, b)| a != b)
    );
    // The daemon hangs up while stdin stays open. tokio reads stdin on a blocking thread that cannot
    // be cancelled, and femmed must exit anyway.
    let hung_up_at = Instant::now();
    hang_up.send(()).expect("the daemon thread waits for this");
    let out = running.wait();
    let took = hung_up_at.elapsed();
    drop(stdin);
    drop(stdout);
    assert_eq!(out.status.code(), Some(0), "{out}");
    assert!(
        took < Duration::from_secs(3),
        "femmed took {took:.2?} to follow the daemon's hang-up: {out}"
    );
    daemon.join().expect("the daemon thread");
}

#[test]
fn proxy_half_closes_on_stdin_eof_and_exits_when_the_daemon_hangs_up() {
    let dir = sandbox();
    let listener = daemon_socket(dir.path());
    let sent = b"ping \x00\xff".to_vec();
    let daemon = echo_then_answer_eof(listener, sent.len());
    let mut running = spawn_proxy(dir.path());
    let mut stdin = running.child.stdin.take().expect("piped stdin");
    let stdout = running.child.stdout.take().expect("piped stdout");
    stdin.write_all(&sent).expect("write femmed's stdin");
    let (got, stdout) = read_exactly(&mut running, stdout, sent.len());
    assert_eq!(got, sent);
    // EOF on stdin reaches the daemon as EOF (`printf req | ssh host femmed proxy`): its answer
    // still comes, and femmed ends when it hangs up.
    let closed_at = Instant::now();
    drop(stdin);
    let (bye, stdout) = read_exactly(&mut running, stdout, 3);
    assert_eq!(bye, b"bye");
    let out = running.wait();
    let took = closed_at.elapsed();
    drop(stdout);
    assert_eq!(out.status.code(), Some(0), "{out}");
    assert!(
        took < Duration::from_secs(3),
        "femmed took {took:.2?} to follow the daemon's hang-up: {out}"
    );
    daemon.join().expect("the daemon thread");
}

#[test]
fn proxy_without_a_daemon_exits_1_naming_the_socket() {
    let dir = sandbox();
    let sock = dir.path().join("femme/femmed.sock");
    let out = run(
        femmed(dir.path(), &["proxy"]).env("XDG_RUNTIME_DIR", dir.path()),
        b"",
    );
    assert_eq!(out.status.code(), Some(1), "{out}");
    assert!(out.stderr.contains("cannot connect"), "{out}");
    assert!(out.stderr.contains(&sock.display().to_string()), "{out}");
}
