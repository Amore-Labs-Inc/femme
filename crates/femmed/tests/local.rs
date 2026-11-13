//! `femmed local` as the desktop runs it: a subprocess on a temporary state dir that serves
//! `<state>/femmed.sock` and nothing else, says `Welcome` to a `Hello` over it, opens sessions on
//! a real `femme-holder`, and on SIGTERM exits 0 and removes its socket.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use femme_proto::rpc::{Cursors, Req, Resp, StreamKind, C2S, S2C};
use femme_proto::state::{Agent, Mode};
use femme_proto::{OpId, PROTO_VERSION};
use femme_transport::{mux_client, Rx, Tx};

/// The longest any one wait may take.
const WAIT: Duration = Duration::from_secs(20);

async fn within<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, f)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

async fn call(tx: &mut Tx, rx: &mut Rx, req: Req) -> Resp {
    let op = OpId::new();
    tx.send(&C2S::Req { op, req })
        .await
        .expect("sends a request");
    match within("an answer", rx.recv_msg::<S2C>()).await {
        Ok(Some(S2C::Resp { op: got, result })) => {
            assert_eq!(got, op);
            result.expect("the request succeeds")
        }
        other => panic!("expected an answer, got {other:?}"),
    }
}

/// Stops the holders a failed run left behind (they outlive daemons by design).
struct Holders<'a>(&'a Path);

impl Drop for Holders<'_> {
    fn drop(&mut self) {
        let _ = Command::new("pkill")
            .args([
                "-f",
                &format!("[f]emme-holder --sid .* --runtime-dir {}", self.0.display()),
            ])
            .status();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_serves_its_socket_only_and_stops_cleanly() {
    // /tmp: a unix socket path must fit in 108 bytes.
    let dir = tempfile::Builder::new()
        .prefix("femmed-local")
        .tempdir_in("/tmp")
        .unwrap();
    let home = dir.path().join("home");
    let rt = dir.path().join("rt");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&home).unwrap();
    let _holders = Holders(&rt);
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
    let log = || std::fs::read_to_string(dir.path().join("log")).unwrap_or_default();

    let sock = state.join("femmed.sock");
    let started = Instant::now();
    while !sock.exists() {
        assert!(
            started.elapsed() < WAIT && child.try_wait().unwrap().is_none(),
            "no socket; log:\n{}",
            log()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Nothing else listens: no QUIC identity made, no runtime-dir socket of `serve`.
    assert!(!state.join("quic").exists() && !rt.join("femme/femmed.sock").exists());

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
        other => panic!("expected Welcome, got {other:?}; log:\n{}", log()),
    }
    let (kind, _events_tx, _events) = within("the events stream", conn.accept()).await.unwrap();
    assert_eq!(kind, StreamKind::Events);

    // Sessions run here, on a holder of this daemon's own.
    let create = Req::SessionCreate {
        cwd: home.display().to_string(),
        agent: Agent::Shell,
        mode: Mode::Tui,
        label: None,
        name: None,
        prompt: None,
        model: None,
    };
    let Resp::Session(sid) = call(&mut tx, &mut rx, create).await else {
        panic!("SessionCreate answers a session");
    };
    assert!(state.join("femme.db").exists());
    call(&mut tx, &mut rx, Req::SessionClose { sid }).await;
    conn.close();

    let pid = child.id().to_string();
    assert!(Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap()
        .success());
    let started = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        assert!(started.elapsed() < WAIT, "still running after SIGTERM");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(status.success(), "{status}; log:\n{}", log());
    assert!(!sock.exists(), "the socket file is left behind");
}
