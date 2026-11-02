//! femmed's side of the daemon ↔ holder protocol (`femme_proto::holder`): spawn a holder as its own
//! systemd user unit, connect to its unix socket, send control requests, receive events, attach to
//! panes and tail the agent spool. One `Holder` per session; cheap to clone (handles only).
//!
//! Framing: `u32` little-endian length + postcard. The holder answers control requests in order
//! on one connection (no request ids), so responses are matched to a FIFO of waiters; events
//! (`HolderResp::Event`) are pushed on the side.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use femme_proto::holder::{HolderEvent, HolderInfo, HolderReq, HolderResp, ScreenFmt, Target};
use femme_proto::term::{TermDown, TermUp};
use femme_proto::{codec, Sid, HOLDER_VERSION};
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("codec: {0}")]
    Codec(#[from] codec::Error),
    #[error("holder connection closed")]
    Closed,
    #[error("holder: {0}")]
    Holder(String),
    #[error("holder answered {0} to a request that expected something else")]
    Unexpected(&'static str),
    #[error("systemd-run failed: {0}")]
    Spawn(String),
    #[error("holder did not come up within {0:?}")]
    Timeout(Duration),
}

type Frame = Framed<UnixStream, LengthDelimitedCodec>;

fn framed(s: UnixStream) -> Frame {
    LengthDelimitedCodec::builder()
        .little_endian()
        .length_field_length(4)
        .max_frame_length(codec::MAX_FRAME)
        .new_framed(s)
}

async fn send(f: &mut Frame, req: &HolderReq) -> Result<(), Error> {
    let b = codec::encode(req)?;
    f.send(Bytes::from(b)).await?;
    Ok(())
}

async fn recv(f: &mut Frame) -> Result<Option<HolderResp>, Error> {
    match f.next().await {
        None => Ok(None),
        Some(Err(e)) => Err(e.into()),
        Some(Ok(b)) => Ok(Some(codec::decode::<HolderResp>(&b)?)),
    }
}

/// Resource limits for a holder's transient unit (plan §5).
#[derive(Clone, Debug)]
pub struct Limits {
    pub cpu_weight: u32,
    pub memory_high: &'static str,
    pub memory_max: &'static str,
    pub tasks_max: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            cpu_weight: 100,
            memory_high: "5G",
            memory_max: "7G",
            tasks_max: 4096,
        }
    }
}

/// Where holders put their sockets, spools and snapshots.
#[derive(Clone, Debug)]
pub struct Dirs {
    /// `$XDG_RUNTIME_DIR/femme`
    pub runtime: PathBuf,
    /// `$XDG_STATE_HOME/femme`
    pub state: PathBuf,
}

impl Dirs {
    pub fn socket(&self, sid: Sid) -> PathBuf {
        self.runtime.join("s").join(format!("{sid}.sock"))
    }
    pub fn spool(&self, sid: Sid) -> PathBuf {
        self.state.join("spool").join(format!("{sid}.jsonl"))
    }
    pub fn snap(&self, sid: Sid) -> PathBuf {
        self.state.join("snap").join(format!("{sid}.snap"))
    }
    /// Sessions whose holder socket exists (reconcile on daemon start).
    pub fn live_sockets(&self) -> Vec<Sid> {
        let Ok(rd) = std::fs::read_dir(self.runtime.join("s")) else {
            return vec![];
        };
        rd.filter_map(|e| e.ok())
            .filter_map(|e| {
                e.file_name()
                    .to_str()?
                    .strip_suffix(".sock")?
                    .parse::<Sid>()
                    .ok()
            })
            .collect()
    }
}

pub fn unit_name(sid: Sid) -> String {
    format!("femme-s-{sid}")
}

type Waiters = Arc<Mutex<VecDeque<oneshot::Sender<HolderResp>>>>;

/// A connected holder. Clone freely.
#[derive(Clone)]
pub struct Holder {
    pub sid: Sid,
    pub dirs: Dirs,
    /// What the holder said at connect time (epoch, panes, agent).
    pub info: Arc<HolderInfo>,
    tx: mpsc::Sender<(HolderReq, oneshot::Sender<HolderResp>)>,
    events: broadcast::Sender<HolderEvent>,
    /// `false` once the control connection is gone (either task ended).
    alive: watch::Receiver<bool>,
}

impl Holder {
    /// Connect to a running holder's socket and perform `Hello`.
    pub async fn connect(dirs: &Dirs, sid: Sid) -> Result<Holder, Error> {
        // A holder still running the previous binary (upgrades rename, running holders stay)
        // accepts only its own version and the one before: offer ours, then the previous one.
        let mut f = framed(UnixStream::connect(dirs.socket(sid)).await?);
        send(&mut f, &HolderReq::Hello { v: HOLDER_VERSION }).await?;
        let mut hello = recv(&mut f).await?;
        if let (Some(HolderResp::Err(_)), Some(older)) =
            (&hello, HOLDER_VERSION.checked_sub(1).filter(|v| *v > 0))
        {
            f = framed(UnixStream::connect(dirs.socket(sid)).await?);
            send(&mut f, &HolderReq::Hello { v: older }).await?;
            hello = recv(&mut f).await?;
        }
        let info = match hello {
            Some(HolderResp::Info(i)) => i,
            Some(HolderResp::Err(e)) => return Err(Error::Holder(e)),
            Some(_) => return Err(Error::Unexpected("non-Info reply to Hello")),
            None => return Err(Error::Closed),
        };
        let (tx, mut rx) = mpsc::channel::<(HolderReq, oneshot::Sender<HolderResp>)>(64);
        let (events, _) = broadcast::channel(256);
        let waiters: Waiters = Arc::new(Mutex::new(VecDeque::new()));
        let (alive_tx, alive) = watch::channel(true);
        let alive_tx = Arc::new(alive_tx);
        let (mut sink, mut stream) = f.split();
        // Writer: one request at a time, waiter queued before the bytes go out.
        let w = waiters.clone();
        let a = alive_tx.clone();
        tokio::spawn(async move {
            while let Some((req, done)) = rx.recv().await {
                let Ok(b) = codec::encode(&req) else { continue };
                w.lock().await.push_back(done);
                if sink.send(Bytes::from(b)).await.is_err() {
                    break;
                }
            }
            a.send_replace(false);
        });
        // Reader: events to the broadcast, everything else to the oldest waiter.
        let w = waiters.clone();
        let ev = events.clone();
        let a = alive_tx;
        tokio::spawn(async move {
            while let Some(Ok(b)) = stream.next().await {
                match codec::decode::<HolderResp>(&b) {
                    Ok(HolderResp::Event(e)) => {
                        let _ = ev.send(e);
                    }
                    Ok(resp) => {
                        if let Some(done) = w.lock().await.pop_front() {
                            let _ = done.send(resp);
                        }
                    }
                    // Out of step for good: every later reply would reach the wrong caller.
                    Err(e) => {
                        tracing::warn!(%e, "bad frame from holder; dropping the connection");
                        break;
                    }
                }
            }
            a.send_replace(false);
            // Nobody will answer the pending requests: drop their senders so callers get Closed.
            w.lock().await.clear();
        });
        Ok(Holder {
            sid,
            dirs: dirs.clone(),
            info: Arc::new(info),
            tx,
            events,
            alive,
        })
    }

    /// Connects as soon as the socket answers, within `timeout` (a holder that was just started).
    async fn wait_connect(dirs: &Dirs, sid: Sid, timeout: Duration) -> Result<Holder, Error> {
        let start = tokio::time::Instant::now();
        let mut wait = Duration::from_millis(10);
        loop {
            match Holder::connect(dirs, sid).await {
                Ok(h) => return Ok(h),
                Err(Error::Io(_)) if start.elapsed() < timeout => {
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(Duration::from_millis(200));
                }
                Err(Error::Io(_)) => return Err(Error::Timeout(timeout)),
                Err(e) => return Err(e),
            }
        }
    }

    /// [`Holder::spawn`] without systemd: a plain child in its own process group, reaped by a
    /// task, outliving this process if it must (development, tests). No resource limits.
    pub async fn spawn_direct(
        dirs: &Dirs,
        sid: Sid,
        holder_bin: &Path,
        timeout: Duration,
    ) -> Result<Holder, Error> {
        let mut child = Command::new(holder_bin)
            .args(["--sid", &sid.to_string(), "--no-sd-notify"])
            .arg("--runtime-dir")
            .arg(&dirs.runtime)
            .arg("--state-dir")
            .arg(&dirs.state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .process_group(0)
            .kill_on_drop(false)
            .spawn()?;
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        Holder::wait_connect(dirs, sid, timeout).await
    }

    /// Start a holder as a transient user unit and connect once its socket answers (≤ `timeout`).
    pub async fn spawn(
        dirs: &Dirs,
        sid: Sid,
        holder_bin: &Path,
        limits: &Limits,
        timeout: Duration,
    ) -> Result<Holder, Error> {
        // The socket file is the holder's own (`claim_socket` refuses a second holder for the sid
        // and clears a stale file itself); deleting it here would hide a live holder.
        let out = Command::new("systemd-run")
            .args([
                "--user",
                "--quiet",
                "--collect",
                &format!("--unit={}", unit_name(sid)),
                "--slice=femme-sessions.slice",
                "--service-type=notify",
                &format!("--description=FEMME session {sid}"),
                "-p",
                "KillMode=control-group",
                "-p",
                "OOMPolicy=continue",
                "-p",
                &format!("CPUWeight={}", limits.cpu_weight),
                "-p",
                &format!("MemoryHigh={}", limits.memory_high),
                "-p",
                &format!("MemoryMax={}", limits.memory_max),
                "-p",
                &format!("TasksMax={}", limits.tasks_max),
                &format!(
                    "--setenv=XDG_STATE_HOME={}",
                    dirs.state.parent().unwrap_or(&dirs.state).display()
                ),
                &format!(
                    "--setenv=XDG_RUNTIME_DIR={}",
                    dirs.runtime.parent().unwrap_or(&dirs.runtime).display()
                ),
            ])
            .arg(holder_bin)
            .args(["--sid", &sid.to_string()])
            .args(["--runtime-dir", &dirs.runtime.to_string_lossy()])
            .args(["--state-dir", &dirs.state.to_string_lossy()])
            // Under systemd the daemon's NOTIFY_SOCKET would be inherited, and systemd-run reports
            // to it as if it were the daemon: the journal gets a "reception only permitted for main
            // PID" line per session. The holder's unit gives it a socket of its own.
            .env_remove("NOTIFY_SOCKET")
            .output()
            .await?;
        if !out.status.success() {
            return Err(Error::Spawn(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Holder::wait_connect(dirs, sid, timeout).await
    }

    pub fn is_alive(&self) -> bool {
        *self.alive.borrow()
    }

    /// Completes once the control connection is gone (the holder exited, crashed or hung up).
    pub async fn closed(&self) {
        let mut alive = self.alive.clone();
        let _ = alive.wait_for(|a| !*a).await;
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HolderEvent> {
        self.events.subscribe()
    }

    pub async fn request(&self, req: HolderReq) -> Result<HolderResp, Error> {
        let (done, wait) = oneshot::channel();
        self.tx.send((req, done)).await.map_err(|_| Error::Closed)?;
        match wait.await.map_err(|_| Error::Closed)? {
            HolderResp::Err(e) => Err(Error::Holder(e)),
            r => Ok(r),
        }
    }

    async fn ok(&self, req: HolderReq) -> Result<(), Error> {
        match self.request(req).await? {
            HolderResp::Ok => Ok(()),
            _ => Err(Error::Unexpected("expected Ok")),
        }
    }

    pub async fn info(&self) -> Result<HolderInfo, Error> {
        match self.request(HolderReq::Hello { v: HOLDER_VERSION }).await? {
            HolderResp::Info(i) => Ok(i),
            _ => Err(Error::Unexpected("expected Info")),
        }
    }

    pub async fn spawn_pty(
        &self,
        no: u16,
        cmd: Vec<String>,
        cwd: String,
        env: Vec<(String, String)>,
        cols: u16,
        rows: u16,
    ) -> Result<(), Error> {
        self.ok(HolderReq::SpawnPty {
            no,
            cmd,
            cwd,
            env,
            cols,
            rows,
        })
        .await
    }
    pub async fn close_pane(&self, no: u16) -> Result<(), Error> {
        self.ok(HolderReq::ClosePane { no }).await
    }
    pub async fn spawn_agent(
        &self,
        cmd: Vec<String>,
        cwd: String,
        env: Vec<(String, String)>,
    ) -> Result<(), Error> {
        self.ok(HolderReq::SpawnAgent { cmd, cwd, env }).await
    }
    pub async fn agent_write(&self, data: Vec<u8>) -> Result<(), Error> {
        self.ok(HolderReq::AgentWrite(data)).await
    }
    pub async fn agent_close_stdin(&self) -> Result<(), Error> {
        self.ok(HolderReq::AgentCloseStdin).await
    }
    pub async fn spool_commit(&self, offset: u64) -> Result<(), Error> {
        self.ok(HolderReq::SpoolCommit { offset }).await
    }
    pub async fn signal(&self, target: Target, sig: i32) -> Result<(), Error> {
        self.ok(HolderReq::Signal { target, sig }).await
    }
    pub async fn screen(&self, pane: u16, fmt: ScreenFmt) -> Result<String, Error> {
        match self.request(HolderReq::Screen { pane, fmt }).await? {
            HolderResp::Screen(s) => Ok(s),
            _ => Err(Error::Unexpected("expected Screen")),
        }
    }
    /// Asks the holder to stop everything and exit; the unit then disappears (`--collect`).
    pub async fn shutdown(&self) -> Result<(), Error> {
        match self.request(HolderReq::Shutdown).await {
            Ok(_) | Err(Error::Closed) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// A pane view on its own socket connection: frames arrive on the receiver, input goes on the
    /// sender. Dropping either end closes the view; the pane itself is unaffected.
    pub async fn attach(
        &self,
        pane: u16,
        epoch: u64,
        have_rev: u64,
        client_instance: u128,
    ) -> Result<(mpsc::Sender<TermUp>, mpsc::Receiver<TermDown>), Error> {
        let mut f = framed(UnixStream::connect(self.dirs.socket(self.sid)).await?);
        send(
            &mut f,
            &HolderReq::Attach {
                pane,
                epoch,
                have_rev,
                client_instance,
            },
        )
        .await?;
        let (up_tx, mut up_rx) = mpsc::channel::<TermUp>(64);
        let (down_tx, down_rx) = mpsc::channel::<TermDown>(64);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    up = up_rx.recv() => match up {
                        Some(m) => {
                            let Ok(b) = codec::encode(&HolderReq::Term(m)) else { continue };
                            if f.send(Bytes::from(b)).await.is_err() { break; }
                        }
                        None => break,
                    },
                    down = f.next() => match down {
                        Some(Ok(b)) => match codec::decode::<HolderResp>(&b) {
                            Ok(HolderResp::Term(t)) => { if down_tx.send(t).await.is_err() { break; } }
                            Ok(HolderResp::Err(e)) => { tracing::warn!(%e, "view error"); break; }
                            Ok(_) => {}
                            Err(e) => { tracing::warn!(%e, "bad view frame"); break; }
                        },
                        _ => break,
                    },
                }
            }
        });
        Ok((up_tx, down_rx))
    }

    /// Tail the agent spool from `from`: chunks of `(offset, bytes)` until the connection drops.
    pub async fn spool_read(&self, from: u64) -> Result<mpsc::Receiver<(u64, Bytes)>, Error> {
        let mut f = framed(UnixStream::connect(self.dirs.socket(self.sid)).await?);
        send(&mut f, &HolderReq::SpoolRead { from }).await?;
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            while let Some(Ok(b)) = f.next().await {
                match codec::decode::<HolderResp>(&b) {
                    Ok(HolderResp::Spool { offset, data }) => {
                        if tx.send((offset, Bytes::from(data))).await.is_err() {
                            break;
                        }
                    }
                    Ok(HolderResp::Err(e)) => {
                        tracing::warn!(%e, "spool error");
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(%e, "bad spool frame");
                        break;
                    }
                }
            }
        });
        Ok(rx)
    }
}

/// Splits spool bytes into complete lines, carrying the absolute offset of each line's first byte.
/// Partial trailing data is kept until the next chunk. Used by the session actor to feed drivers.
#[derive(Default)]
pub struct SpoolLines {
    buf: BytesMut,
    /// Absolute offset of `buf[0]`.
    base: u64,
}

impl SpoolLines {
    pub fn new(start: u64) -> Self {
        SpoolLines {
            buf: BytesMut::new(),
            base: start,
        }
    }

    /// Feed a chunk that begins at `offset`; chunks must be contiguous (a gap resets the buffer).
    pub fn push(&mut self, offset: u64, data: &[u8]) {
        if offset != self.base + self.buf.len() as u64 {
            self.buf.clear();
            self.base = offset;
        }
        self.buf.extend_from_slice(data);
    }

    /// Next complete line (without the newline) and its absolute offset.
    pub fn next_line(&mut self) -> Option<(u64, Bytes)> {
        let nl = self.buf.iter().position(|&b| b == b'\n')?;
        let line = self.buf.split_to(nl + 1);
        let off = self.base;
        self.base += nl as u64 + 1;
        Some((off, line.freeze().slice(..nl)))
    }

    /// Offset just past the last complete line returned.
    pub fn offset(&self) -> u64 {
        self.base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spool_lines_split_and_offsets() {
        let mut s = SpoolLines::new(100);
        s.push(100, b"ab\nc");
        assert_eq!(s.next_line(), Some((100, Bytes::from_static(b"ab"))));
        assert_eq!(s.next_line(), None);
        s.push(104, b"d\n\n");
        assert_eq!(s.next_line(), Some((103, Bytes::from_static(b"cd"))));
        assert_eq!(s.next_line(), Some((106, Bytes::from_static(b""))));
        assert_eq!(s.offset(), 107);
        // a gap resets
        s.push(200, b"x\n");
        assert_eq!(s.next_line(), Some((200, Bytes::from_static(b"x"))));
    }
}
