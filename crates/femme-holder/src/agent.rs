//! The headless agent: a child with piped stdio, not a PTY. Its stdout is appended to the spool
//! file (so a restarted daemon can resume reading by offset), stderr keeps a 64 KiB tail, stdin
//! stays open for as long as the daemon wants.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use femme_proto::holder::{HolderAgent, HolderEvent, Target};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, ChildStdout};
use tokio::sync::mpsc;

use crate::proc;
use crate::ring::Ring;
use crate::shared::{lock, Shared};

const STDERR_TAIL: usize = 64 * 1024;
const STDIN_QUEUE: usize = 256;
/// Stdin bytes that may wait for the agent. Past it `AgentWrite` fails instead of growing the heap
/// (the queue above bounds messages, not bytes).
const STDIN_BUDGET: usize = 8 << 20;
/// After the child exits, how long to wait for its stdout/stderr pipes to reach EOF (a grandchild
/// holding them open would otherwise delay the exit event forever).
const PIPE_GRACE: Duration = Duration::from_secs(1);

/// Ordered stdin operations; a close queues behind the writes before it.
pub enum StdinCmd {
    Write(Vec<u8>),
    Close,
}

#[derive(Clone)]
pub struct AgentHandle {
    pub info: Arc<Mutex<HolderAgent>>,
    pub tx: mpsc::Sender<StdinCmd>,
    /// Bytes of queued `Write`s, counting the one being written.
    queued: Arc<AtomicUsize>,
    /// Set once the exit has been announced: the child is reaped (`alive` is false) well before,
    /// while its pipes drain (up to `PIPE_GRACE`).
    pub finished: Arc<AtomicBool>,
}

/// `SpawnAgent`. At most one agent is alive at a time; a new one may follow an exited one and
/// keeps appending to the same spool.
pub async fn spawn(sh: &Arc<Shared>, cmd: Vec<String>, cwd: String, env: Vec<(String, String)>) -> Result<(), String> {
    if sh.stopping.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("holder is shutting down".into());
    }
    let Some((prog, args)) = cmd.split_first() else { return Err("empty command".into()) };
    if !cwd.is_empty() && !Path::new(&cwd).is_dir() {
        return Err(format!("cwd {cwd} is not a directory"));
    }
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&sh.spool_path)
        .await
        .map_err(|e| format!("open spool {}: {e}", sh.spool_path.display()))?;

    // No await from here to the registration: two racing SpawnAgents cannot both pass the check.
    let current = lock(&sh.agent).clone();
    if let Some(a) = current {
        if lock(&a.info).alive {
            return Err("agent already running".into());
        }
        // Its exit is not announced yet; a new agent now would make that `ChildExited` ambiguous.
        if !a.finished.load(Relaxed) {
            return Err("the previous agent is still shutting down".into());
        }
    }
    let mut c = tokio::process::Command::new(prog);
    c.args(args)
        .env_clear()
        .envs(proc::child_env(proc::holder_env(), &env, sh.sid, None))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if !cwd.is_empty() {
        c.current_dir(&cwd);
    }
    let mut child = c.spawn().map_err(|e| format!("spawn {prog}: {e}"))?;
    let pid = child.id().unwrap_or(0);
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let (Some(mut stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
        let _ = child.start_kill();
        return Err("agent pipes were not created".into());
    };

    let info = Arc::new(Mutex::new(HolderAgent { pid, alive: true, exit: None, spool_lo: 0, spool_hi: 0 }));
    let (tx, mut rx) = mpsc::channel::<StdinCmd>(STDIN_QUEUE);
    let (queued, finished) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
    *lock(&sh.agent) = Some(AgentHandle { info: info.clone(), tx, queued: queued.clone(), finished: finished.clone() });
    sh.mark_dirty();
    tracing::info!(pid, cmd = %prog, "agent started");

    tokio::task::spawn_local(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                StdinCmd::Write(b) => {
                    let r = async { stdin.write_all(&b).await?; stdin.flush().await }.await;
                    queued.fetch_sub(b.len(), Relaxed);
                    if let Err(e) = r {
                        tracing::debug!("agent stdin closed: {e}");
                        break;
                    }
                }
                StdinCmd::Close => break,
            }
        }
        // `stdin` drops here: the agent sees EOF.
    });
    let out_task = tokio::task::spawn_local(pump_stdout(sh.clone(), stdout, file));
    let ring = Arc::new(Mutex::new(Ring::new(STDERR_TAIL)));
    let err_task = tokio::task::spawn_local(pump_stderr(stderr, ring.clone()));

    let sh = sh.clone();
    tokio::task::spawn_local(async move {
        let status = child.wait().await;
        let code = status.map(proc::exit_code).unwrap_or(-1);
        // Reaped, so the pid may be reused from now on: nothing may signal it any more. No await
        // between `wait()` returning and this, so no other task can run in between.
        {
            let mut i = lock(&info);
            i.alive = false;
            i.exit = Some(code);
        }
        sh.mark_dirty();
        tracing::info!(pid, code, "agent exited");
        let _ = tokio::time::timeout(PIPE_GRACE, async {
            let _ = out_task.await;
            let _ = err_task.await;
        })
        .await;
        // The tail goes first, so whoever handles `ChildExited` already has it.
        let tail = lock(&ring).tail_string();
        sh.emit(HolderEvent::AgentStderr(tail));
        sh.emit(HolderEvent::ChildExited { target: Target::Agent, status: code });
        finished.store(true, Relaxed);
    });
    Ok(())
}

/// stdout to the spool: every read is written and flushed at once, so the daemon (tailing by
/// offset) sees partial lines promptly. A failing disk stalls the agent (the pipe fills) instead
/// of losing output; the write is retried every second.
async fn pump_stdout(sh: Arc<Shared>, mut out: ChildStdout, mut file: tokio::fs::File) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match out.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("agent stdout: {e}");
                break;
            }
        };
        loop {
            match async { file.write_all(&buf[..n]).await?; file.flush().await }.await {
                Ok(()) => break,
                Err(e) => {
                    tracing::error!("spool write failed ({e}); the agent is stalled until it succeeds");
                    // A write that failed half way (disk full) left bytes past the recorded end, and
                    // the retry writes them again: cut them off, or the spool would hold them twice.
                    loop {
                        let end = *sh.spool_hi.borrow();
                        match file.set_len(end).await {
                            Ok(()) => break,
                            Err(e) => {
                                tracing::error!("spool truncate to {end} failed: {e}");
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        sh.spool_hi.send_modify(|hi| *hi += n as u64);
    }
}

async fn pump_stderr(mut err: ChildStderr, ring: Arc<Mutex<Ring>>) {
    let mut buf = vec![0u8; 8 * 1024];
    while let Ok(n) = err.read(&mut buf).await {
        if n == 0 {
            break;
        }
        lock(&ring).push(&buf[..n]);
    }
}

/// Queue stdin bytes (`AgentWrite`) or an EOF (`AgentCloseStdin`). Never waits for the agent: a
/// stuck agent must not wedge the control connection that has to be able to kill it.
pub fn stdin(sh: &Shared, cmd: StdinCmd) -> Result<(), String> {
    let agent = lock(&sh.agent).clone().ok_or("no agent")?;
    if !lock(&agent.info).alive {
        return Err("agent has exited".into());
    }
    // A write that finds nothing queued always fits, however large (the client cannot split what
    // it cannot send); beyond that the backlog is capped.
    let n = match &cmd {
        StdinCmd::Write(b) => b.len(),
        StdinCmd::Close => 0,
    };
    let queued = agent.queued.load(Relaxed);
    if n != 0 && queued != 0 && queued + n > STDIN_BUDGET {
        return Err(format!("agent stdin backlog is full ({queued} bytes waiting)"));
    }
    agent.queued.fetch_add(n, Relaxed);
    agent.tx.try_send(cmd).map_err(|e| {
        agent.queued.fetch_sub(n, Relaxed);
        match e {
            mpsc::error::TrySendError::Full(_) => "agent stdin backlog is full".to_string(),
            mpsc::error::TrySendError::Closed(_) => "agent stdin is closed".to_string(),
        }
    })
}
