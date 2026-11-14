//! `femmed local` as the desktop runs it: a subprocess on a temporary state dir that serves
//! `<state>/femmed.sock` and nothing else, says `Welcome` to a `Hello` over it, opens sessions on
//! a real `femme-holder`, and on SIGTERM exits 0 and removes its socket. Its sessions reach it:
//! their `femmed hook` through the loopback hook door, their `show`/`open` through `FEMME_SOCK`.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use femme_proto::rpc::{Cursors, Req, Resp, Scope, StreamKind, C2S, S2C};
use femme_proto::state::{Agent, Mode, Row, SessionStatus};
use femme_proto::{OpId, Sid, PROTO_VERSION};
use femme_transport::{mux_client, Conn, Rx, Tx};

/// The longest any one wait may take.
const WAIT: Duration = Duration::from_secs(20);

async fn within<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, f)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// A `femmed local` subprocess and one desktop connection to it, past `Hello`.
struct Local {
    child: Child,
    conn: Arc<dyn Conn>,
    tx: Tx,
    rx: Rx,
    events: Rx,
    _events_tx: Tx,
    home: PathBuf,
    rt: PathBuf,
    state: PathBuf,
    // Last: the files go after the process and its holders.
    dir: tempfile::TempDir,
}

impl Local {
    /// Starts `femmed local` with `<home>/.local/bin` holding what `setup` puts there, and connects.
    async fn start(setup: impl FnOnce(&Path)) -> Local {
        // /tmp: a unix socket path must fit in 108 bytes.
        let dir = tempfile::Builder::new()
            .prefix("femmed-local")
            .tempdir_in("/tmp")
            .unwrap();
        let home = dir.path().join("home");
        let rt = dir.path().join("rt");
        let state = dir.path().join("state");
        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        setup(&home.join(".local/bin"));
        let log = std::fs::File::create(dir.path().join("log")).unwrap();
        // HOME and XDG_RUNTIME_DIR never default: the developer's `.env` and live holders stay out.
        let mut child = Command::new(env!("CARGO_BIN_EXE_femmed"))
            .args(["local", "--state"])
            .arg(&state)
            .env_clear()
            .env("HOME", &home)
            .env("XDG_RUNTIME_DIR", &rt)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("SHELL", "/bin/sh")
            .env("FEMME_HOLDER_DIRECT", "1")
            .env("FEMME_HOLDER_BIN", common::holder_bin())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // A file, not a pipe: the holders it starts inherit it and outlive it.
            .stderr(log)
            .spawn()
            .unwrap();
        let sock = state.join("femmed.sock");
        let started = Instant::now();
        while !sock.exists() {
            assert!(
                started.elapsed() < WAIT && child.try_wait().unwrap().is_none(),
                "no socket; log:\n{}",
                std::fs::read_to_string(dir.path().join("log")).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let (r, w) = stream.into_split();
        let conn = mux_client(r, w);
        let (mut tx, mut rx) = conn.open_kind(StreamKind::Ctrl).await.expect("ctrl stream");
        tx.send(&C2S::Hello {
            proto: PROTO_VERSION,
            client_instance: 1,
            cursors: Cursors::default(),
        })
        .await
        .unwrap();
        match within("Welcome", rx.recv_msg::<S2C>()).await {
            Ok(Some(S2C::Welcome { proto, .. })) => assert_eq!(proto, PROTO_VERSION),
            other => panic!(
                "expected Welcome, got {other:?}; log:\n{}",
                std::fs::read_to_string(dir.path().join("log")).unwrap_or_default()
            ),
        }
        let (kind, events_tx, events) = within("the events stream", conn.accept()).await.unwrap();
        assert_eq!(kind, StreamKind::Events);
        Local {
            child,
            conn,
            tx,
            rx,
            events,
            _events_tx: events_tx,
            home,
            rt,
            state,
            dir,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("log")).unwrap_or_default()
    }

    async fn call(&mut self, req: Req) -> Resp {
        let op = OpId::new();
        self.tx
            .send(&C2S::Req { op, req })
            .await
            .expect("sends a request");
        match within("an answer", self.rx.recv_msg::<S2C>()).await {
            Ok(Some(S2C::Resp { op: got, result })) => {
                assert_eq!(got, op);
                result.unwrap_or_else(|e| panic!("{e:?}; log:\n{}", self.log()))
            }
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    async fn create(&mut self, agent: Agent) -> Sid {
        let req = Req::SessionCreate {
            cwd: self.home.display().to_string(),
            agent,
            mode: Mode::Tui,
            label: None,
            name: None,
            prompt: None,
            model: None,
            resume: None,
        };
        match self.call(req).await {
            Resp::Session(sid) => sid,
            other => panic!("SessionCreate answered {other:?}"),
        }
    }

    /// SIGTERM, then the exit status (it must come within [`WAIT`]).
    async fn stop(&mut self) -> std::process::ExitStatus {
        let pid = self.child.id().to_string();
        assert!(Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success());
        let started = Instant::now();
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st;
            }
            assert!(started.elapsed() < WAIT, "still running after SIGTERM");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for Local {
    /// Stops what a failed run left behind: the process, and its holders (they outlive it by design).
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = Command::new("pkill")
            .args([
                "-f",
                &format!(
                    "[f]emme-holder --sid .* --runtime-dir {}",
                    self.rt.display()
                ),
            ])
            .status();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_serves_its_socket_only_and_stops_cleanly() {
    let mut l = Local::start(|_| {}).await;
    // Nothing else listens: no QUIC identity made, no runtime-dir socket of `serve`.
    assert!(!l.state.join("quic").exists() && !l.rt.join("femme/femmed.sock").exists());

    // Sessions run here, on a holder of this daemon's own.
    let sid = l.create(Agent::Shell).await;
    assert!(l.state.join("femme.db").exists());
    l.call(Req::SessionClose { sid }).await;
    l.conn.close();

    let status = l.stop().await;
    assert!(status.success(), "{status}; log:\n{}", l.log());
    assert!(
        !l.state.join("femmed.sock").exists(),
        "the socket file is left behind"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_from_a_local_session_reaches_the_local_daemon() {
    // A `claude` that reports a prompt the way Claude's hooks do, notes its FEMME_SOCK, and waits.
    let femmed = env!("CARGO_BIN_EXE_femmed");
    let mut l = Local::start(|bin| {
        let script = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$FEMME_SOCK\" > \"$HOME/femme-sock\"\n\
             printf '{{}}' | {femmed} hook claude UserPromptSubmit 2> \"$HOME/hook-err\"\n\
             exec sleep 600\n"
        );
        let path = bin.join("claude");
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    })
    .await;
    l.call(Req::Subscribe {
        scope: Scope::Global,
        after: None,
    })
    .await;
    let sid = l.create(Agent::Claude).await;

    // The hook's `UserPromptSubmit` makes the session `working`: only this daemon knows the sid.
    let started = Instant::now();
    'wait: loop {
        assert!(
            started.elapsed() < WAIT,
            "no `working` from the hook; hook stderr: {:?}; log:\n{}",
            std::fs::read_to_string(l.home.join("hook-err")),
            l.log()
        );
        let msg = within("changes", l.events.recv_msg::<S2C>()).await;
        if let Ok(Some(S2C::Changes { rows, .. })) = msg {
            for r in rows {
                if matches!(&r, Row::Session(s) if s.id == sid && s.status == SessionStatus::Working)
                {
                    break 'wait;
                }
            }
        }
    }
    // And its `femmed show` / `open` would find this daemon's socket.
    assert_eq!(
        std::fs::read_to_string(l.home.join("femme-sock"))
            .unwrap()
            .trim(),
        l.state.join("femmed.sock").display().to_string()
    );
    l.call(Req::SessionClose { sid }).await;
    assert!(l.stop().await.success());
}

#[test]
fn sock_path_honours_femme_sock() {
    let default = femmed::config::sock_path();
    std::env::set_var("FEMME_SOCK", "/somewhere/femmed.sock");
    let set = femmed::config::sock_path();
    std::env::set_var("FEMME_SOCK", "");
    let empty = femmed::config::sock_path();
    std::env::remove_var("FEMME_SOCK");
    assert_eq!(set, PathBuf::from("/somewhere/femmed.sock"));
    assert_eq!(empty, default);
    assert!(default.ends_with("femme/femmed.sock"));
}
