//! Process setup and lifetime: the socket, the accept loop, snapshots, the idle exit, systemd.

use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use femme_holder::paths;
use femme_holder::snap::Snapshot;
use sd_notify::NotifyState;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{oneshot, watch, Notify};

use crate::pane::PaneCmd;
use crate::shared::{lock, Shared};
use crate::{conn, Args};

/// `sun_path` holds 108 bytes including the NUL.
const MAX_SOCKET_PATH: usize = 107;
/// A pane gets this long to answer a snapshot request.
const SNAP_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

fn unix_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn private_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).with_context(|| format!("create {}", dir.display()))
}

/// Make the socket path ours: a stale socket (nobody listening) is removed; a live one means
/// another holder already serves this session, and starting a second would split-brain the PTYs.
async fn claim_socket(path: &Path) -> anyhow::Result<()> {
    match UnixStream::connect(path).await {
        Ok(_) => bail!("another holder is already serving {}", path.display()),
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused) => {
            let _ = std::fs::remove_file(path);
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("probe {}", path.display())),
    }
}

/// Runs until `Shutdown`, SIGTERM/SIGINT or the idle exit; returns the process exit code.
pub async fn run(args: Args) -> anyhow::Result<i32> {
    let runtime_dir = args
        .runtime_dir
        .or_else(paths::default_runtime_dir)
        .context("no --runtime-dir and XDG_RUNTIME_DIR is not set")?;
    let state_dir = args
        .state_dir
        .or_else(paths::default_state_dir)
        .context("no --state-dir and neither XDG_STATE_HOME nor HOME is set")?;
    let sock_path = paths::socket_path(&runtime_dir, args.sid);
    let spool_path = paths::spool_path(&state_dir, args.sid);
    let snap_path = paths::snap_path(&state_dir, args.sid);
    if sock_path.as_os_str().len() > MAX_SOCKET_PATH {
        bail!("socket path {} is longer than {MAX_SOCKET_PATH} bytes; use a shorter --runtime-dir", sock_path.display());
    }
    for p in [&sock_path, &spool_path, &snap_path] {
        private_dir(p.parent().context("path without a parent")?)?;
    }
    claim_socket(&sock_path).await?;
    let listener = UnixListener::bind(&sock_path).with_context(|| format!("bind {}", sock_path.display()))?;
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))?;

    let started_ms = unix_ms();
    let epoch = started_ms as u64;
    let spool_len = std::fs::metadata(&spool_path).map_or(0, |m| m.len());
    let sh = Arc::new(Shared {
        sid: args.sid,
        epoch,
        started_ms,
        t0: Instant::now(),
        timing_every: args.timing_every,
        sock_path,
        spool_path,
        snap_path,
        panes: Mutex::new(BTreeMap::new()),
        agent: Mutex::new(None),
        controls: Mutex::new(Vec::new()),
        spool_hi: watch::channel(spool_len).0,
        spool_committed: AtomicU64::new(0),
        next_epoch: AtomicU64::new(epoch),
        next_id: AtomicU64::new(1),
        conns: AtomicUsize::new(0),
        dirty: AtomicBool::new(false),
        stopping: AtomicBool::new(false),
        shutdown: Notify::new(),
    });

    tokio::task::spawn_local(accept_loop(sh.clone(), listener));
    tokio::task::spawn_local(housekeeping(sh.clone(), Duration::from_millis(args.tick_ms), Duration::from_secs(args.idle_exit_secs)));
    let sd = !args.no_sd_notify;
    if sd {
        if let Err(e) = sd_notify::notify(&[NotifyState::Ready]) {
            tracing::warn!("sd_notify READY: {e}");
        }
        if let Some(every) = sd_notify::watchdog_enabled() {
            tokio::task::spawn_local(watchdog(every / 2));
        }
    }
    tracing::info!(sid = %args.sid, socket = %sh.sock_path.display(), epoch, "holder ready");

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sh.shutdown.notified() => {}
        _ = term.recv() => tracing::info!("SIGTERM"),
        _ = int.recv() => tracing::info!("SIGINT"),
    }
    if sd {
        let _ = sd_notify::notify(&[NotifyState::Stopping]);
    }
    sh.terminate_children().await;
    snapshot(&sh).await;
    let _ = std::fs::remove_file(&sh.sock_path);
    tracing::info!("holder stopped");
    Ok(0)
}

async fn accept_loop(sh: Arc<Shared>, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::task::spawn_local(conn::serve(sh.clone(), stream));
            }
            Err(e) => {
                // Out of file descriptors and the like: back off instead of spinning.
                tracing::warn!("accept: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// systemd `WatchdogSec`: ping at half the interval. Only started when systemd asked for it.
async fn watchdog(every: Duration) {
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        let _ = sd_notify::notify(&[NotifyState::Watchdog]);
    }
}

/// The holder's only periodic work: every `tick` (30 s) write a snapshot if anything changed, and
/// exit once there has been nothing alive and nobody connected for `idle` (10 min).
///
/// ponytail: the idle exit is checked on the tick, so it fires up to one tick late; make it a
/// one-shot timer re-armed by connection/child events if that precision ever matters.
async fn housekeeping(sh: Arc<Shared>, tick: Duration, idle: Duration) {
    let mut every = tokio::time::interval(tick);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    every.tick().await; // the first tick is immediate
    let mut idle_since: Option<Instant> = None;
    loop {
        every.tick().await;
        snapshot(&sh).await;
        if sh.conns.load(Relaxed) == 0 && !sh.any_child_alive() {
            let since = *idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= idle {
                tracing::info!("nothing alive and nobody connected for {}s: exiting", idle.as_secs());
                sh.shutdown.notify_one();
            }
        } else {
            idle_since = None;
        }
    }
}

/// Write `<state>/snap/<sid>.snap` if anything changed since the last one.
async fn snapshot(sh: &Arc<Shared>) {
    if !sh.dirty.swap(false, Relaxed) {
        return;
    }
    let handles: Vec<_> = lock(&sh.panes).values().cloned().collect();
    let mut panes = Vec::new();
    for h in handles {
        let (tx, rx) = oneshot::channel();
        if h.tx.send(PaneCmd::Snap(tx)).await.is_ok() {
            if let Ok(Ok(p)) = tokio::time::timeout(SNAP_REPLY_TIMEOUT, rx).await {
                panes.push(p);
            }
        }
    }
    panes.sort_by_key(|p| p.no);
    let snap = Snapshot { sid: sh.sid, written: unix_ms(), panes, agent_alive: sh.agent_info().is_some_and(|a| a.alive) };
    let path = sh.snap_path.clone();
    match tokio::task::spawn_blocking(move || snap.write_atomic(&path)).await {
        Ok(Ok(())) => {}
        other => {
            tracing::warn!("snapshot not written: {other:?}");
            sh.mark_dirty(); // try again on the next tick
        }
    }
}
