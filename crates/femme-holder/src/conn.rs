//! Connections. The first message decides the role (plan §5):
//! `Hello` is a control connection, `Attach` a view of one pane, `SpoolRead` a tail of the
//! agent's stdout. Each connection is a reader (this task) plus a writer task fed by a bounded
//! queue, so a peer that stops reading can only ever lose its own connection.

use std::io;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use femme_holder::wire;
use femme_proto::holder::{HolderReq, HolderResp};
use femme_proto::HOLDER_VERSION;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch, Notify};
use tokio::task::JoinHandle;

use crate::agent::{self, StdinCmd};
use crate::pane::{self, Attach, PaneCmd, PaneHandle, VIEW_QUEUE};
use crate::shared::{lock, Ctl, Shared};

/// How long a new connection may stay silent before it is dropped.
const FIRST_MSG_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a control request waits for a pane to answer.
const PANE_REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const CONTROL_QUEUE: usize = 256;
const SPOOL_CHUNK: usize = 64 * 1024;
const SPOOL_QUEUE: usize = 4;

struct ConnGuard<'a>(&'a Shared);
impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        self.0.conns.fetch_sub(1, Relaxed);
    }
}

/// The write side of a connection: a queue of already-framed messages and the task draining it.
struct Link {
    out: mpsc::Sender<Vec<u8>>,
    writer: JoinHandle<()>,
}

impl Link {
    fn new(wr: OwnedWriteHalf, cap: usize) -> Link {
        let (out, rx) = mpsc::channel(cap);
        Link {
            out,
            writer: tokio::task::spawn_local(write_loop(wr, rx)),
        }
    }

    /// Queue a reply; `false` when the connection is gone. A reply that does not fit in a message
    /// (a `Screen` over 16 MiB) becomes an `Err` saying so: replies are strictly one per request,
    /// and closing the connection instead would leave the asker without one.
    async fn send(&self, resp: &HolderResp) -> bool {
        let bytes = wire::encode(resp)
            .or_else(|e| wire::encode(&HolderResp::Err(format!("reply not sent: {e}"))));
        match bytes {
            Ok(bytes) => self.out.send(bytes).await.is_ok(),
            Err(_) => false,
        }
    }

    /// Let queued messages reach the socket (for a second), then close it, reader or not.
    async fn finish(self) {
        drop(self.out);
        let mut writer = self.writer;
        stop_writer(&mut writer).await;
    }
}

/// Give a writer a second to flush what is queued, then stop it: a peer that stopped reading
/// would keep it (and the socket) stuck in `write_all` for ever. Aborting at once instead would
/// drop its queue's receiver, and a pane that has not yet looked at a just-sent `Attach` would
/// take that for a dead view and lose the input queued behind it.
async fn stop_writer(writer: &mut JoinHandle<()>) {
    if !writer.is_finished()
        && tokio::time::timeout(Duration::from_secs(1), &mut *writer)
            .await
            .is_err()
    {
        writer.abort();
    }
}

/// Drain the queue into the socket. After a write error (the peer hung up) it keeps draining and
/// drops what it was given: the reading side still holds whatever the peer sent before hanging up
/// and decides when the connection ends, so a client that sends a key and closes does not lose it.
async fn write_loop(mut wr: OwnedWriteHalf, mut rx: mpsc::Receiver<Vec<u8>>) {
    let mut open = true;
    while let Some(bytes) = rx.recv().await {
        open = open && wr.write_all(&bytes).await.is_ok();
    }
    let _ = wr.shutdown().await;
}

/// Serve one accepted connection until it ends.
pub async fn serve(sh: Arc<Shared>, stream: UnixStream) {
    sh.conns.fetch_add(1, Relaxed);
    let _guard = ConnGuard(&sh);
    if let Err(e) = serve_inner(&sh, stream).await {
        tracing::debug!("connection ended: {e}");
    }
}

async fn serve_inner(sh: &Arc<Shared>, stream: UnixStream) -> io::Result<()> {
    let cred = rustix::net::sockopt::socket_peercred(&stream)?;
    if cred.uid != rustix::process::getuid() {
        tracing::warn!(
            uid = cred.uid.as_raw(),
            pid = cred.pid.as_raw_pid(),
            "refusing a connection from another user"
        );
        return Ok(());
    }
    let (mut rd, wr) = stream.into_split();
    let mut buf = Vec::new();
    let first = match tokio::time::timeout(
        FIRST_MSG_TIMEOUT,
        wire::read::<HolderReq, _>(&mut rd, &mut buf),
    )
    .await
    {
        Ok(r) => r?,
        Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "no first message")),
    };
    match first {
        None => Ok(()),
        Some(HolderReq::Hello { v }) => control(sh, rd, buf, Link::new(wr, CONTROL_QUEUE), v).await,
        Some(HolderReq::Attach {
            pane,
            epoch,
            have_rev,
            client_instance,
        }) => {
            let at = AttachReq {
                pane,
                epoch,
                have_rev,
                client: client_instance,
            };
            view(sh, rd, buf, Link::new(wr, VIEW_QUEUE), at).await
        }
        Some(HolderReq::SpoolRead { from }) => {
            spool(sh, rd, Link::new(wr, SPOOL_QUEUE), from).await
        }
        Some(_) => {
            let link = Link::new(wr, 1);
            link.send(&HolderResp::Err(
                "first message must be Hello, Attach or SpoolRead".into(),
            ))
            .await;
            link.finish().await;
            Ok(())
        }
    }
}

// ---- control -------------------------------------------------------------------------------

async fn control(
    sh: &Arc<Shared>,
    mut rd: OwnedReadHalf,
    mut buf: Vec<u8>,
    link: Link,
    v: u16,
) -> io::Result<()> {
    if v != HOLDER_VERSION && v.checked_add(1) != Some(HOLDER_VERSION) {
        let msg = format!(
            "unsupported holder protocol v{v}; this holder speaks v{HOLDER_VERSION} and v{}",
            HOLDER_VERSION.saturating_sub(1)
        );
        link.send(&HolderResp::Err(msg)).await;
        link.finish().await;
        return Ok(());
    }
    // Register and build `Info` without an await in between: no event can fall in the gap.
    let id = sh.alloc_id();
    lock(&sh.controls).push(Ctl {
        id,
        out: link.out.clone(),
    });
    let info = HolderResp::Info(sh.info());
    let shutdown = if link.send(&info).await {
        control_loop(sh, &mut rd, &mut buf, &link).await
    } else {
        false
    };
    lock(&sh.controls).retain(|c| c.id != id);
    link.finish().await;
    if shutdown {
        tracing::info!("shutdown requested");
        sh.stopping.store(true, Relaxed);
        sh.shutdown.notify_one();
    }
    Ok(())
}

/// Requests and replies, strictly in order (the protocol has no request ids). Returns whether
/// the connection asked for `Shutdown`.
async fn control_loop(
    sh: &Arc<Shared>,
    rd: &mut OwnedReadHalf,
    buf: &mut Vec<u8>,
    link: &Link,
) -> bool {
    loop {
        let req = match wire::read::<HolderReq, _>(rd, buf).await {
            Ok(Some(req)) => req,
            Ok(None) => return false,
            Err(e) => {
                tracing::debug!("control connection: {e}");
                return false;
            }
        };
        let shutdown = matches!(req, HolderReq::Shutdown);
        let resp = handle(sh, req).await;
        if !link.send(&resp).await || shutdown {
            return shutdown;
        }
    }
}

async fn handle(sh: &Arc<Shared>, req: HolderReq) -> HolderResp {
    let r: Result<(), String> = match req {
        HolderReq::Hello { .. } => return HolderResp::Info(sh.info()),
        HolderReq::SpawnPty {
            no,
            cmd,
            cwd,
            env,
            cols,
            rows,
        } => pane::spawn(sh, no, cmd, cwd, env, cols, rows),
        HolderReq::ClosePane { no } => {
            let removed = lock(&sh.panes).remove(&no);
            match removed {
                Some(p) => {
                    sh.mark_dirty();
                    // The pane task does the killing (SIGHUP, SIGKILL after 2 s) and the announcing.
                    let _ = p.tx.send(PaneCmd::Close).await;
                    Ok(())
                }
                None => Err(format!("no pane {no}")),
            }
        }
        HolderReq::SpawnAgent { cmd, cwd, env } => agent::spawn(sh, cmd, cwd, env).await,
        HolderReq::AgentWrite(bytes) => agent::stdin(sh, StdinCmd::Write(bytes)),
        HolderReq::AgentCloseStdin => agent::stdin(sh, StdinCmd::Close),
        HolderReq::SpoolCommit { offset } => {
            sh.spool_committed.fetch_max(offset, Relaxed);
            sh.punch_spool(offset); // a fallocate on the commit path: microseconds, and only once per MiB
            Ok(())
        }
        HolderReq::Signal { target, sig } => sh.signal(target, sig),
        HolderReq::Screen { pane, fmt } => {
            let Some(p) = pane_handle(sh, pane) else {
                return HolderResp::Err(format!("no pane {pane}"));
            };
            let (reply, rx) = oneshot::channel();
            if p.tx.send(PaneCmd::Screen { fmt, reply }).await.is_err() {
                return HolderResp::Err(format!("pane {pane} is gone"));
            }
            return match tokio::time::timeout(PANE_REPLY_TIMEOUT, rx).await {
                Ok(Ok(text)) => HolderResp::Screen(text),
                _ => HolderResp::Err(format!("pane {pane} did not answer")),
            };
        }
        HolderReq::Shutdown => Ok(()),
        HolderReq::Attach { .. } | HolderReq::Term(_) | HolderReq::SpoolRead { .. } => Err(
            "that message belongs on its own connection: send it as the first message of a new one"
                .into(),
        ),
    };
    match r {
        Ok(()) => HolderResp::Ok,
        Err(e) => HolderResp::Err(e),
    }
}

fn pane_handle(sh: &Shared, no: u16) -> Option<PaneHandle> {
    lock(&sh.panes).get(&no).cloned()
}

// ---- view ----------------------------------------------------------------------------------

struct AttachReq {
    pane: u16,
    epoch: u64,
    have_rev: u64,
    client: u128,
}

async fn view(
    sh: &Arc<Shared>,
    mut rd: OwnedReadHalf,
    mut buf: Vec<u8>,
    link: Link,
    at: AttachReq,
) -> io::Result<()> {
    let Some(pane) = pane_handle(sh, at.pane) else {
        link.send(&HolderResp::Err(format!("no pane {}", at.pane)))
            .await;
        link.finish().await;
        return Ok(());
    };
    let Link { out, mut writer } = link;
    let id = sh.alloc_id();
    let kill = Arc::new(Notify::new());
    let attach = Attach {
        id,
        out,
        kill: kill.clone(),
        epoch: at.epoch,
        have_rev: at.have_rev,
        client: at.client,
    };
    if pane.tx.send(PaneCmd::Attach(attach)).await.is_err() {
        return Ok(());
    }
    // From here the pane task holds the only sender: when it drops the view, the writer ends.
    let r = view_loop(sh, &mut rd, &mut buf, &pane, id, &kill, &mut writer).await;
    let _ = pane.tx.send(PaneCmd::Detach(id)).await;
    // The pane drops its end of the queue on `Detach`, which lets the writer finish.
    stop_writer(&mut writer).await;
    r
}

async fn view_loop(
    sh: &Shared,
    rd: &mut OwnedReadHalf,
    buf: &mut Vec<u8>,
    pane: &PaneHandle,
    id: u64,
    kill: &Notify,
    writer: &mut JoinHandle<()>,
) -> io::Result<()> {
    loop {
        let msg = tokio::select! {
            biased;
            _ = kill.notified() => return Ok(()),
            _ = &mut *writer => return Ok(()),
            msg = wire::read::<HolderReq, _>(rd, buf) => match msg? {
                Some(msg) => msg,
                None => return Ok(()),
            },
        };
        // Anything but `Term` on a view connection is ignored. `TermUp::Open` is a resync request.
        if let HolderReq::Term(up) = msg {
            let recv_ns = sh.now_ns();
            if pane.tx.send(PaneCmd::Up { id, up, recv_ns }).await.is_err() {
                return Ok(());
            }
        }
    }
}

// ---- spool ---------------------------------------------------------------------------------

async fn spool(sh: &Arc<Shared>, rd: OwnedReadHalf, link: Link, from: u64) -> io::Result<()> {
    let hi_rx = sh.spool_hi.subscribe();
    let (lo, end) = (sh.spool_lo.load(Relaxed), *hi_rx.borrow());
    if from > end || from < lo {
        link.send(&HolderResp::Err(format!(
            "spool read from {from} is outside its kept part {lo}..{end}"
        )))
        .await;
        link.finish().await;
        return Ok(());
    }
    let Link { out, mut writer } = link;
    let r = spool_loop(sh, rd, &out, &mut writer, hi_rx, from).await;
    writer.abort();
    r
}

async fn spool_loop(
    sh: &Shared,
    mut rd: OwnedReadHalf,
    out: &mpsc::Sender<Vec<u8>>,
    writer: &mut JoinHandle<()>,
    mut hi_rx: watch::Receiver<u64>,
    from: u64,
) -> io::Result<()> {
    let mut file: Option<tokio::fs::File> = None;
    let mut chunk = vec![0u8; SPOOL_CHUNK];
    let mut pos = from;
    let mut probe = [0u8; 1];
    loop {
        let hi = *hi_rx.borrow_and_update();
        while pos < hi {
            let f = match file.as_mut() {
                Some(f) => f,
                None => {
                    let mut f = tokio::fs::File::open(&sh.spool_path).await?;
                    f.seek(io::SeekFrom::Start(pos)).await?;
                    file.insert(f)
                }
            };
            let want = SPOOL_CHUNK.min((hi - pos) as usize);
            let n = f.read(&mut chunk[..want]).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "spool file is shorter than its recorded end",
                ));
            }
            let msg = wire::encode(&HolderResp::Spool {
                offset: pos,
                data: chunk[..n].to_vec(),
            })?;
            if out.send(msg).await.is_err() {
                return Ok(());
            }
            pos += n as u64;
        }
        tokio::select! {
            changed = hi_rx.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
            }
            _ = &mut *writer => return Ok(()),
            // The peer sends nothing after its first message; any read result means it hung up.
            _ = rd.read(&mut probe) => return Ok(()),
        }
    }
}
