//! One PTY pane: the child process, its terminal emulator, the views attached to it, and the
//! frame scheduler. A single task owns all of it, so there are no locks around the emulator.
//!
//! Frame scheduling (plan §3 rules 5, 6):
//! - After a PTY read, if any view sent input in the last 50 ms, frames go out at once.
//! - Otherwise output is coalesced: a frame goes out once the PTY has been quiet for 8 ms, and at
//!   the latest 16 ms after the first unsent output (so a flood still shows at ~60 Hz).
//! - Each view may have at most 8 frames / 128 KiB unacked. While it is blocked the emulator
//!   keeps parsing; the next `Ack` that unblocks it triggers one merged frame.

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsFd, OwnedFd};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use femme_holder::snap::SnapPane;
use femme_holder::wire;
use femme_proto::holder::{HolderEvent, HolderPane, HolderResp, ScreenFmt, Target};
use femme_proto::term::{TermDown, TermUp, Timing};
use femme_proto::PaneId;
use femme_term::{EmuEvent, Emulator, View};
use pty_process::{OwnedReadPty, OwnedWritePty, Size};
use rustix::io::Errno;
use rustix::process::Signal;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::Sleep;

use crate::proc;
use crate::shared::{lock, Shared};

const SCROLLBACK: usize = 10_000;
const READ_BUF: usize = 64 * 1024;
const FRAME_QUIET: Duration = Duration::from_millis(8);
const FRAME_MAX_WAIT: Duration = Duration::from_millis(16);
const INPUT_WINDOW: Duration = Duration::from_millis(50);
const MAX_UNACKED_FRAMES: usize = 8;
const MAX_UNACKED_BYTES: usize = 128 * 1024;
const RESIZE_LOCK: Duration = Duration::from_secs(2);
/// Reflowing the scrollback is not free and a window drag sends a resize per frame: after the
/// first, a pane applies at most one resize per this long (the latest size wins).
const RESIZE_GAP: Duration = Duration::from_millis(100);
const SEQ_MEMORY: Duration = Duration::from_secs(10 * 60);
const MAX_ROWS_PER_REQUEST: u64 = 2000;
const EXIT_DRAIN: Duration = Duration::from_millis(300);
const CLOSE_GRACE: Duration = Duration::from_secs(2);
const BELL_GAP: Duration = Duration::from_millis(100);
/// `Title` events of one pane are at least this far apart; the latest title always goes out.
const TITLE_GAP: Duration = Duration::from_millis(50);
/// Notifications one pane may announce per second (a program looping on OSC 9 must not flood the daemon).
const NOTIFY_PER_SEC: u32 = 10;
const SNAP_ROWS: u64 = 200;
/// Sizes are clamped to this range (the emulator's own minimum is 2 columns by 1 row).
const MIN_COLS: u16 = 2;
const MAX_COLS: u16 = 1000;
const MAX_ROWS: u16 = 500;
/// Input waiting for a child that is not reading its terminal; beyond this it is dropped.
// ponytail: drops silently past 8 MiB, tell the view to stop sending (or ack input) if a client ever pastes that much
const MAX_PENDING_INPUT: usize = 8 << 20;
/// Messages queued towards one view connection before it is considered stuck and closed.
pub const VIEW_QUEUE: usize = 64;
const CMD_QUEUE: usize = 256;

/// What the connection tasks and the control handlers ask of a pane task.
pub enum PaneCmd {
    Attach(Attach),
    Detach(u64),
    /// Input or flow control from view `id`. `recv_ns` is when the connection read it.
    Up {
        id: u64,
        up: TermUp,
        recv_ns: u64,
    },
    Screen {
        fmt: ScreenFmt,
        reply: oneshot::Sender<String>,
    },
    Snap(oneshot::Sender<SnapPane>),
    /// `ClosePane`: SIGHUP, SIGKILL after 2 s, then the pane is gone.
    Close,
}

pub struct Attach {
    pub id: u64,
    pub out: mpsc::Sender<Vec<u8>>,
    pub kill: Arc<Notify>,
    pub epoch: u64,
    pub have_rev: u64,
    pub client: u128,
}

#[derive(Clone)]
pub struct PaneHandle {
    pub tx: mpsc::Sender<PaneCmd>,
    pub info: Arc<Mutex<HolderPane>>,
}

struct InTiming {
    seq: u32,
    recv_ns: u64,
    write_ns: u64,
}

struct ViewState {
    id: u64,
    out: mpsc::Sender<Vec<u8>>,
    kill: Arc<Notify>,
    view: View,
    client: u128,
    /// Highest input seq accepted from this client (dedup of resends).
    queued_seq: u32,
    /// Highest seq whose bytes have reached the PTY (`Frame::applied_seq`).
    applied_seq: u32,
    /// `applied_seq` as of the last PTY read (`Frame::echo_seq`).
    echo_seq: u32,
    last_input: Option<Instant>,
    /// `(rev, wire bytes)` of frames sent and not yet acked.
    unacked: VecDeque<(u64, usize)>,
    unacked_bytes: usize,
    exit_sent: bool,
    dead: bool,
    t_in: Option<InTiming>,
    t_read: Option<u64>,
}

impl ViewState {
    fn blocked(&self) -> bool {
        self.unacked.len() >= MAX_UNACKED_FRAMES || self.unacked_bytes >= MAX_UNACKED_BYTES
    }
}

struct Pane {
    sh: Arc<Shared>,
    no: u16,
    emu: Emulator,
    cols: u16,
    rows: u16,
    epoch: u64,
    info: Arc<Mutex<HolderPane>>,
    pid: u32,
    rd: OwnedReadPty,
    wr: OwnedWritePty,
    /// A dup of the PTY master: for `tcgetpgrp` and for writes that must not wait.
    fd: OwnedFd,
    child: Child,
    /// Bytes for the child, from `wpos` on.
    wbuf: Vec<u8>,
    wpos: usize,
    views: Vec<ViewState>,
    /// `client_instance` to the last input seq it sent, kept 10 minutes across reconnects.
    seqs: HashMap<u128, (u32, Instant)>,
    /// The view that resized last, and when: others may not resize within 2 s of it.
    size_owner: Option<(u64, Instant)>,
    exit: Option<i32>,
    announced: bool,
    pty_eof: bool,
    last_bell: Option<Instant>,
    title: String,
    /// The title changed and its `Title` event has not gone out yet.
    title_pending: bool,
    last_title_event: Option<Instant>,
    title_timer: Pin<Box<Sleep>>,
    title_armed: bool,
    /// Start of the current one-second notification window, and how many it has let through.
    notify_window: Option<Instant>,
    notify_n: u32,
    dirty_since: Option<Instant>,
    frame_timer: Pin<Box<Sleep>>,
    frame_timer_armed: bool,
    drain: Pin<Box<Sleep>>,
    drain_armed: bool,
    /// Fires at the emulator's synchronized-update deadline (DEC 2026), when set.
    sync_timer: Pin<Box<Sleep>>,
    sync_armed: bool,
    resize_timer: Pin<Box<Sleep>>,
    resize_armed: bool,
    /// The size the resize owner last asked for; `apply_resize` makes it so.
    want: (u16, u16),
    resized_at: Option<Instant>,
    last_read_ns: u64,
    frames: u64,
    poisoned: bool,
}

/// Run `$e` against the emulator, containing a panic: the pane is flagged and reset by the loop.
macro_rules! guard {
    ($self:ident, $e:expr) => {
        match catch_unwind(AssertUnwindSafe(|| $e)) {
            Ok(v) => Some(v),
            Err(_) => {
                $self.poisoned = true;
                None
            }
        }
    };
}

/// Test hook, debug builds only: `FEMME_HOLDER_PANIC_AT=feed|sync|attach|snap` makes the first
/// emulator call at that site panic (once per process), so the tests can see a panic there contained.
#[cfg(debug_assertions)]
fn inject_panic(site: &str) {
    use std::sync::atomic::AtomicBool;
    use std::sync::OnceLock;
    static AT: OnceLock<Option<String>> = OnceLock::new();
    static FIRED: AtomicBool = AtomicBool::new(false);
    let at = AT.get_or_init(|| std::env::var("FEMME_HOLDER_PANIC_AT").ok());
    if at.as_deref() == Some(site) && !FIRED.swap(true, Relaxed) {
        // Unwinds like a panic but skips the panic hook, which would only print noise.
        std::panic::resume_unwind(Box::new(format!("injected emulator panic at {site}")));
    }
}

#[cfg(not(debug_assertions))]
fn inject_panic(_site: &str) {}

/// `SpawnPty`.
pub fn spawn(
    sh: &Arc<Shared>,
    no: u16,
    cmd: Vec<String>,
    cwd: String,
    env: Vec<(String, String)>,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    if sh.stopping.load(Relaxed) {
        return Err("holder is shutting down".into());
    }
    let Some((prog, args)) = cmd.split_first() else {
        return Err("empty command".into());
    };
    if lock(&sh.panes).contains_key(&no) {
        return Err(format!("pane {no} already exists"));
    }
    if !cwd.is_empty() && !Path::new(&cwd).is_dir() {
        return Err(format!("cwd {cwd} is not a directory"));
    }
    let (cols, rows) = (cols.clamp(MIN_COLS, MAX_COLS), rows.clamp(1, MAX_ROWS));
    let (pty, pts) = pty_process::open().map_err(|e| format!("open pty: {e}"))?;
    pty.resize(Size::new(rows, cols))
        .map_err(|e| format!("size pty: {e}"))?;
    set_iutf8(&pty);
    let mut c = pty_process::Command::new(prog)
        .args(args)
        .env_clear()
        .envs(proc::child_env(proc::holder_env(), &env, sh.sid, Some(no)));
    if !cwd.is_empty() {
        c = c.current_dir(&cwd);
    }
    let child = c.spawn(pts).map_err(|e| format!("spawn {prog}: {e}"))?;
    let pid = child.id().unwrap_or(0);
    let fd = pty
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("dup pty: {e}"))?;
    let (rd, wr) = pty.into_split();

    let info = Arc::new(Mutex::new(HolderPane {
        no,
        pid,
        alive: true,
        exit: None,
        cols,
        rows,
        title: String::new(),
    }));
    let (tx, rx) = mpsc::channel(CMD_QUEUE);
    lock(&sh.panes).insert(
        no,
        PaneHandle {
            tx,
            info: info.clone(),
        },
    );
    sh.mark_dirty();
    tracing::info!(pane = no, pid, cmd = %prog, cols, rows, "pane started");

    let pane = Pane {
        sh: sh.clone(),
        no,
        emu: Emulator::new(cols, rows, SCROLLBACK),
        cols,
        rows,
        epoch: sh.alloc_epoch(),
        info,
        pid,
        rd,
        wr,
        fd,
        child,
        wbuf: Vec::new(),
        wpos: 0,
        views: Vec::new(),
        seqs: HashMap::new(),
        size_owner: None,
        exit: None,
        announced: false,
        pty_eof: false,
        last_bell: None,
        title: String::new(),
        title_pending: false,
        last_title_event: None,
        title_timer: Box::pin(tokio::time::sleep(Duration::ZERO)),
        title_armed: false,
        notify_window: None,
        notify_n: 0,
        dirty_since: None,
        frame_timer: Box::pin(tokio::time::sleep(Duration::ZERO)),
        frame_timer_armed: false,
        drain: Box::pin(tokio::time::sleep(Duration::ZERO)),
        drain_armed: false,
        sync_timer: Box::pin(tokio::time::sleep(Duration::ZERO)),
        sync_armed: false,
        resize_timer: Box::pin(tokio::time::sleep(Duration::ZERO)),
        resize_armed: false,
        want: (cols, rows),
        resized_at: None,
        last_read_ns: 0,
        frames: 0,
        poisoned: false,
    };
    tokio::task::spawn_local(pane.run(rx));
    Ok(())
}

/// Programs that edit lines in cooked mode (`cat`, plain `read`) need IUTF8 to erase a whole
/// multibyte character; new ptys do not have it. Failure is harmless.
fn set_iutf8(pty: &pty_process::Pty) {
    use rustix::termios::{tcgetattr, tcsetattr, InputModes, OptionalActions};
    if let Ok(mut t) = tcgetattr(pty) {
        t.input_modes |= InputModes::IUTF8;
        let _ = tcsetattr(pty, OptionalActions::Now, &t);
    }
}

fn cap(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut n = max;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
    }
    s
}

impl Pane {
    async fn run(mut self, mut rx: mpsc::Receiver<PaneCmd>) {
        let mut buf = vec![0u8; READ_BUF];
        loop {
            // Biased: commands first, then input to the child, then output from it. Writing to the
            // child before reading from it is what lets Ctrl-C through while a flood is printing.
            tokio::select! {
                biased;
                cmd = rx.recv() => match cmd {
                    None => break,
                    Some(PaneCmd::Close) => {
                        self.close().await;
                        break;
                    }
                    Some(cmd) => self.on_cmd(cmd),
                },
                r = self.wr.write(&self.wbuf[self.wpos..]), if self.wpos < self.wbuf.len() => self.on_written(r),
                r = self.rd.read(&mut buf), if !self.pty_eof => self.on_read(r, &buf),
                st = self.child.wait(), if self.exit.is_none() => self.on_child_exit(st),
                _ = &mut self.frame_timer, if self.frame_timer_armed => self.flush_all(),
                _ = &mut self.sync_timer, if self.sync_armed => self.on_sync_deadline(),
                _ = &mut self.resize_timer, if self.resize_armed => self.apply_resize(),
                _ = &mut self.title_timer, if self.title_armed => {
                    self.title_armed = false;
                    self.announce_title();
                }
                _ = &mut self.drain, if self.drain_armed => {
                    self.drain_armed = false;
                    self.announce_exit();
                }
            }
            if self.poisoned {
                self.reset_emulator();
            }
        }
        // Dropping the views closes their connections; dropping the PTY hangs up on the child.
    }

    fn on_cmd(&mut self, cmd: PaneCmd) {
        match cmd {
            PaneCmd::Attach(a) => self.on_attach(a),
            PaneCmd::Detach(id) => {
                self.views.retain(|v| v.id != id);
                if self.views.is_empty() {
                    self.dirty_since = None;
                    self.frame_timer_armed = false;
                }
            }
            PaneCmd::Up { id, up, recv_ns } => {
                if let Some(i) = self.views.iter().position(|v| v.id == id) {
                    self.on_up(i, up, recv_ns);
                }
            }
            PaneCmd::Screen { fmt, reply } => {
                let text = guard!(
                    self,
                    match fmt {
                        ScreenFmt::Text => self.emu.screen_text(),
                        ScreenFmt::Ansi => self.emu.screen_ansi(),
                    }
                )
                .unwrap_or_default();
                let _ = reply.send(text);
            }
            PaneCmd::Snap(reply) => {
                let _ = reply.send(self.snap());
            }
            PaneCmd::Close => {}
        }
    }

    fn snap(&mut self) -> SnapPane {
        let (top, lo) = guard!(self, {
            inject_panic("snap");
            (self.emu.top_abs(), self.emu.history_lo_abs())
        })
        .unwrap_or_default();
        let bottom = top + u64::from(self.rows);
        let from = lo.max(bottom.saturating_sub(SNAP_ROWS));
        let rows = guard!(self, self.emu.rows(from, bottom)).unwrap_or_default();
        let screen_text = guard!(self, self.emu.screen_text()).unwrap_or_default();
        SnapPane {
            no: self.no,
            title: self.title.clone(),
            screen_text,
            last_200_rows: rows,
        }
    }

    // ---- output from the child -------------------------------------------------------------

    fn on_read(&mut self, r: std::io::Result<usize>, buf: &[u8]) {
        match r {
            Ok(n) if n > 0 => self.on_output(&buf[..n]),
            Ok(_) => self.on_pty_eof(),
            Err(e) => {
                if e.raw_os_error() != Some(Errno::IO.raw_os_error()) {
                    tracing::warn!(pane = self.no, "pty read: {e}");
                }
                self.on_pty_eof();
            }
        }
    }

    fn on_pty_eof(&mut self) {
        self.pty_eof = true;
        if self.exit.is_some() {
            self.announce_exit();
        }
    }

    fn on_output(&mut self, data: &[u8]) {
        let now_ns = self.sh.now_ns();
        guard!(self, {
            inject_panic("feed");
            self.emu.feed(data)
        });
        self.last_read_ns = now_ns;
        for v in &mut self.views {
            // The first read after a write confirms everything written before it.
            v.echo_seq = v.applied_seq;
            if v.t_in.is_some() && v.t_read.is_none() {
                v.t_read = Some(now_ns);
            }
        }
        self.fed();
    }

    /// The emulator has parsed something (output, or the end of a synchronized update): act on its
    /// events, note the change for the snapshot, and schedule frames.
    fn fed(&mut self) {
        let now = Instant::now();
        self.drain_events();
        self.sh.mark_dirty();
        self.arm_sync();
        if self.views.is_empty() || self.poisoned {
            return;
        }
        let typing = self.views.iter().any(|v| {
            v.last_input
                .is_some_and(|t| now.duration_since(t) < INPUT_WINDOW)
        });
        if typing {
            self.flush_all();
            return;
        }
        let since = *self.dirty_since.get_or_insert(now);
        let waited = now.duration_since(since);
        if waited >= FRAME_MAX_WAIT {
            // Under a flood the PTY never goes quiet and the select loop never reaches the timer.
            self.flush_all();
            return;
        }
        let wait = FRAME_QUIET.min(FRAME_MAX_WAIT - waited);
        self.frame_timer
            .as_mut()
            .reset(tokio::time::Instant::now() + wait);
        self.frame_timer_armed = true;
    }

    /// While a synchronized update (`CSI ? 2026 h`) is pending the emulator holds the screen back;
    /// if the program goes quiet it must be told when the update times out.
    fn arm_sync(&mut self) {
        let deadline = guard!(self, {
            inject_panic("sync");
            self.emu.sync_deadline()
        });
        match deadline.flatten() {
            Some(at) => {
                self.sync_timer
                    .as_mut()
                    .reset(tokio::time::Instant::from_std(at));
                self.sync_armed = true;
            }
            None => self.sync_armed = false,
        }
    }

    fn on_sync_deadline(&mut self) {
        self.sync_armed = false;
        guard!(self, self.emu.flush_sync());
        self.fed();
    }

    fn drain_events(&mut self) {
        for ev in guard!(self, self.emu.take_events()).unwrap_or_default() {
            match ev {
                EmuEvent::PtyWrite(bytes) => self.queue_write(&bytes),
                EmuEvent::Title(t) => self.set_title(cap(t, 2048)),
                EmuEvent::ResetTitle => self.set_title(String::new()),
                EmuEvent::Bell => self.bell(),
                EmuEvent::Notify { title, body } => {
                    if self.notify_allowed() {
                        self.sh.emit(HolderEvent::Notify {
                            pane: self.no,
                            title: cap(title, 2048),
                            body: cap(body, 2048),
                        });
                    }
                }
            }
        }
    }

    /// At most `NOTIFY_PER_SEC` notifications per one-second window; the rest are dropped.
    // ponytail: fixed windows, so up to twice the rate across a boundary; a token bucket if that ever matters
    fn notify_allowed(&mut self) -> bool {
        let now = Instant::now();
        if self
            .notify_window
            .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1))
        {
            self.notify_window = Some(now);
            self.notify_n = 0;
        }
        self.notify_n += 1;
        self.notify_n <= NOTIFY_PER_SEC
    }

    fn set_title(&mut self, title: String) {
        if title == self.title {
            return;
        }
        self.title = title.clone();
        lock(&self.info).title = title;
        self.title_pending = true;
        self.announce_title();
    }

    /// Send the pending `Title` event, but at most one per `TITLE_GAP`: a program that sets its title
    /// in a loop costs the daemon a few events, the last of which carries the final title.
    fn announce_title(&mut self) {
        if !self.title_pending {
            return;
        }
        let now = Instant::now();
        match self
            .last_title_event
            .map(|t| t + TITLE_GAP)
            .filter(|due| *due > now)
        {
            None => self.flush_title(),
            Some(due) => {
                self.title_timer
                    .as_mut()
                    .reset(tokio::time::Instant::from_std(due));
                self.title_armed = true;
            }
        }
    }

    /// The pending `Title` event now, gap or not (before `ChildExited`, so the last title is not lost).
    fn flush_title(&mut self) {
        if !std::mem::take(&mut self.title_pending) {
            return;
        }
        self.title_armed = false;
        self.last_title_event = Some(Instant::now());
        self.sh.emit(HolderEvent::Title {
            pane: self.no,
            title: self.title.clone(),
        });
    }

    fn bell(&mut self) {
        let now = Instant::now();
        if self
            .last_bell
            .is_some_and(|t| now.duration_since(t) < BELL_GAP)
        {
            return;
        }
        self.last_bell = Some(now);
        self.sh.emit(HolderEvent::Bell { pane: self.no });
        for i in 0..self.views.len() {
            self.send_to(i, &HolderResp::Term(TermDown::Bell));
        }
        self.reap_views();
    }

    // ---- the child's exit ------------------------------------------------------------------

    fn on_child_exit(&mut self, st: std::io::Result<std::process::ExitStatus>) {
        self.record_exit(st.map(proc::exit_code).unwrap_or(-1));
        if self.pty_eof {
            self.announce_exit();
        } else {
            // The child's last output may still sit in the PTY: read it before announcing.
            self.drain
                .as_mut()
                .reset(tokio::time::Instant::now() + EXIT_DRAIN);
            self.drain_armed = true;
        }
    }

    fn record_exit(&mut self, code: i32) {
        self.exit = Some(code);
        self.wbuf.clear();
        self.wpos = 0;
        let mut i = lock(&self.info);
        i.alive = false;
        i.exit = Some(code);
        drop(i);
        self.sh.mark_dirty();
        tracing::info!(pane = self.no, code, "pane's child exited");
    }

    /// Final frames, then `ChildExited` to the daemon and `Exited` to the views (a view that is
    /// blocked gets its `Exited` after the frame that finally unblocks it).
    fn announce_exit(&mut self) {
        let Some(status) = self.exit else { return };
        if self.announced {
            return;
        }
        self.announced = true;
        self.drain_armed = false;
        self.flush_all();
        self.flush_title();
        self.sh.emit(HolderEvent::ChildExited {
            target: Target::Pane(self.no),
            status,
        });
        for i in 0..self.views.len() {
            self.maybe_send_exited(i);
        }
        self.reap_views();
    }

    fn maybe_send_exited(&mut self, i: usize) {
        let Some(status) = self.exit.filter(|_| self.announced) else {
            return;
        };
        let v = &self.views[i];
        if v.exit_sent || v.dead || v.blocked() {
            return;
        }
        self.views[i].exit_sent = true;
        self.send_to(i, &HolderResp::Term(TermDown::Exited { status }));
    }

    /// `ClosePane`: hang up, wait up to 2 s, kill; then tell everyone and let the task end.
    async fn close(&mut self) {
        if self.exit.is_none() {
            let _ = proc::send(self.pid, Signal::HUP, true);
            let st = match tokio::time::timeout(CLOSE_GRACE, self.child.wait()).await {
                Ok(st) => st,
                Err(_) => {
                    tracing::warn!(pane = self.no, "child ignored SIGHUP, killing");
                    let _ = proc::send(self.pid, Signal::KILL, true);
                    self.child.wait().await
                }
            };
            self.record_exit(st.map(proc::exit_code).unwrap_or(-1));
        }
        self.announce_exit();
        tracing::info!(pane = self.no, "pane closed");
    }

    // ---- input to the child ----------------------------------------------------------------

    fn queue_write(&mut self, data: &[u8]) {
        if self.exit.is_some() || data.is_empty() {
            return;
        }
        if self.wbuf.len() - self.wpos + data.len() > MAX_PENDING_INPUT {
            tracing::warn!(
                pane = self.no,
                "child is not reading its terminal; dropping {} input bytes",
                data.len()
            );
            return;
        }
        self.wbuf.extend_from_slice(data);
        self.pump_writes();
    }

    /// Write what the PTY takes right now (the master is non-blocking); the rest waits for the
    /// loop's write branch. Never blocks, so output keeps being read while input is stuck.
    fn pump_writes(&mut self) {
        while self.wpos < self.wbuf.len() {
            match rustix::io::write(&self.fd, &self.wbuf[self.wpos..]) {
                Ok(n) => self.wpos += n,
                Err(Errno::AGAIN) => return,
                Err(Errno::INTR) => {}
                Err(e) => {
                    tracing::debug!(pane = self.no, "pty write: {e}");
                    self.wpos = self.wbuf.len();
                }
            }
        }
        self.wbuf.clear();
        self.wpos = 0;
        self.mark_applied();
    }

    fn on_written(&mut self, r: std::io::Result<usize>) {
        match r {
            Ok(n) => self.wpos += n,
            Err(e) => {
                tracing::debug!(pane = self.no, "pty write: {e}");
                self.wpos = self.wbuf.len();
            }
        }
        if self.wpos >= self.wbuf.len() {
            self.wbuf.clear();
            self.wpos = 0;
            self.mark_applied();
        }
    }

    /// Everything accepted so far has reached the PTY.
    fn mark_applied(&mut self) {
        for v in &mut self.views {
            v.applied_seq = v.queued_seq;
        }
    }

    // ---- views -----------------------------------------------------------------------------

    fn on_attach(&mut self, a: Attach) {
        let now = Instant::now();
        self.seqs
            .retain(|_, (_, t)| now.duration_since(*t) < SEQ_MEMORY);
        let last = self.seqs.get(&a.client).map_or(0, |e| e.0);
        let view = self.view_for(a.epoch, a.have_rev);
        tracing::debug!(
            pane = self.no,
            view = a.id,
            have_rev = a.have_rev,
            "view attached"
        );
        self.views.push(ViewState {
            id: a.id,
            out: a.out,
            kill: a.kill,
            view,
            client: a.client,
            queued_seq: last,
            applied_seq: last,
            echo_seq: last,
            last_input: None,
            unacked: VecDeque::new(),
            unacked_bytes: 0,
            exit_sent: false,
            dead: false,
            t_in: None,
            t_read: None,
        });
        let i = self.views.len() - 1;
        // A pane whose emulator just panicked is reset at the end of this loop turn, which sends
        // every view, this one included, a frame.
        if !self.poisoned {
            self.send_frame(i);
        }
        self.reap_views();
    }

    /// The state of a view for a client that says it holds `have_rev` of `epoch`: the same pane
    /// incarnation and a rev we have means a delta (the emulator decides whether it can be one),
    /// anything else a full snapshot.
    fn view_for(&mut self, epoch: u64, have_rev: u64) -> View {
        guard!(self, {
            inject_panic("attach");
            if epoch == self.epoch && have_rev != 0 && have_rev <= self.emu.rev() {
                View::with_have_rev(have_rev)
            } else {
                self.emu.new_view()
            }
        })
        .unwrap_or_default()
    }

    /// `Open` on a view that is already open: the client's cache could not take a frame
    /// (`NeedResync`: it says what it holds) or was dropped (`EpochChanged`: it holds nothing,
    /// `have_rev` 0). Start the view over from what it says, as `Attach` does, and answer at once.
    fn on_open(&mut self, i: usize, pane: PaneId, epoch: u64, have_rev: u64) {
        if pane.no != self.no || pane.sid != self.sh.sid {
            tracing::debug!(pane = self.no, "Open for another pane ignored");
            return;
        }
        let view = self.view_for(epoch, have_rev);
        let v = &mut self.views[i];
        v.view = view;
        // Frames in flight were made for the state the client has just given up: they must not
        // count against its flow-control window, or a client that stopped acking never gets this answer.
        v.unacked.clear();
        v.unacked_bytes = 0;
        if !self.poisoned {
            self.send_frame(i);
        }
        self.reap_views();
    }

    fn on_up(&mut self, i: usize, up: TermUp, recv_ns: u64) {
        match up {
            TermUp::Open {
                pane,
                epoch,
                have_rev,
                ..
            } => self.on_open(i, pane, epoch, have_rev),
            TermUp::Key { seq, key } => {
                if !self.stale(i, seq) {
                    let bytes = guard!(self, self.emu.encode_key(key)).unwrap_or_default();
                    self.input(i, seq, &bytes, recv_ns);
                }
            }
            TermUp::Paste { seq, text } => {
                if !self.stale(i, seq) {
                    let bytes = guard!(self, self.emu.encode_paste(&text)).unwrap_or_default();
                    self.input(i, seq, &bytes, recv_ns);
                }
            }
            TermUp::Bytes { seq, data } => {
                if !self.stale(i, seq) {
                    self.input(i, seq, &data, recv_ns);
                }
            }
            TermUp::Mouse { seq, ev } => {
                if !self.stale(i, seq) {
                    let bytes = guard!(self, self.emu.encode_mouse(ev))
                        .flatten()
                        .unwrap_or_default();
                    self.input(i, seq, &bytes, recv_ns);
                }
            }
            TermUp::Resize { cols, rows } => self.on_resize(i, cols, rows),
            TermUp::Focus(on) => {
                if let Some(bytes) = guard!(self, self.emu.encode_focus(on)).flatten() {
                    self.queue_write(&bytes);
                }
            }
            TermUp::Ack { rev } => self.on_ack(i, rev),
            TermUp::GetRows { from_abs, to_abs } => {
                let to = to_abs.min(from_abs.saturating_add(MAX_ROWS_PER_REQUEST));
                let rows = if from_abs < to {
                    guard!(self, self.emu.rows(from_abs, to)).unwrap_or_default()
                } else {
                    Vec::new()
                };
                self.send_to(i, &HolderResp::Term(TermDown::Rows(rows)));
                self.reap_views();
            }
        }
    }

    /// A resent input (`seq` already accepted from this client, on this view or on another one it
    /// still has, like the connection it is leaving) is dropped. `seq` 0 is untracked input (the
    /// compat layer's `/input`): always written, never deduped.
    fn stale(&self, i: usize, seq: u32) -> bool {
        let v = &self.views[i];
        let known = self
            .seqs
            .get(&v.client)
            .map_or(0, |e| e.0)
            .max(v.queued_seq);
        seq != 0 && seq <= known
    }

    fn input(&mut self, i: usize, seq: u32, bytes: &[u8], recv_ns: u64) {
        let now = Instant::now();
        let every = self.sh.timing_every;
        let v = &mut self.views[i];
        v.last_input = Some(now);
        if seq != 0 {
            v.queued_seq = seq;
            self.seqs.insert(v.client, (seq, now));
        }
        self.queue_write(bytes);
        if self.wbuf.is_empty() {
            self.mark_applied();
        }
        if seq != 0 && every != 0 && u64::from(seq) % every == 0 {
            let write_ns = self.sh.now_ns();
            self.views[i].t_in = Some(InTiming {
                seq,
                recv_ns,
                write_ns,
            });
            self.views[i].t_read = None;
        }
    }

    /// The most recent resizer owns the size; another view may not resize within 2 s of it.
    fn on_resize(&mut self, i: usize, cols: u16, rows: u16) {
        let (cols, rows) = (cols.clamp(MIN_COLS, MAX_COLS), rows.clamp(1, MAX_ROWS));
        let (id, now) = (self.views[i].id, Instant::now());
        if let Some((owner, at)) = self.size_owner {
            if owner != id && now.duration_since(at) < RESIZE_LOCK {
                tracing::debug!(
                    pane = self.no,
                    view = id,
                    "resize ignored: another view resized within 2 s"
                );
                return;
            }
        }
        self.size_owner = Some((id, now));
        self.want = (cols, rows);
        match self
            .resized_at
            .map(|t| t + RESIZE_GAP)
            .filter(|due| *due > now)
        {
            None => self.apply_resize(),
            Some(due) => {
                self.resize_timer
                    .as_mut()
                    .reset(tokio::time::Instant::from_std(due));
                self.resize_armed = true;
            }
        }
    }

    /// Make the wanted size real: emulator, PTY (the child gets SIGWINCH), and a fresh frame for
    /// every view (the emulator makes it a snapshot).
    fn apply_resize(&mut self) {
        self.resize_armed = false;
        self.resized_at = Some(Instant::now());
        let (cols, rows) = self.want;
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        guard!(self, self.emu.resize(cols, rows));
        if let Err(e) = self.wr.resize(Size::new(rows, cols)) {
            tracing::warn!(pane = self.no, "pty resize: {e}");
        }
        (self.cols, self.rows) = (cols, rows);
        {
            let mut info = lock(&self.info);
            info.cols = cols;
            info.rows = rows;
        }
        self.sh.mark_dirty();
        self.flush_all();
    }

    fn on_ack(&mut self, i: usize, rev: u64) {
        let v = &mut self.views[i];
        let was_blocked = v.blocked();
        while let Some(&(r, n)) = v.unacked.front() {
            if r > rev {
                break;
            }
            v.unacked.pop_front();
            v.unacked_bytes -= n;
        }
        // Only an ack that unblocks sends at once (the merged frame); otherwise the scheduler decides.
        if was_blocked && !v.blocked() {
            self.send_frame(i);
            self.reap_views();
        }
    }

    // ---- frames ----------------------------------------------------------------------------

    fn flush_all(&mut self) {
        self.dirty_since = None;
        self.frame_timer_armed = false;
        for i in 0..self.views.len() {
            self.send_frame(i);
        }
        self.reap_views();
    }

    /// Produce and send view `i`'s next frame, unless it is blocked on acks or already current.
    fn send_frame(&mut self, i: usize) {
        if self.views[i].dead || self.views[i].blocked() {
            return;
        }
        let timing = self.timing_for(i);
        let epoch = self.epoch;
        let v = &mut self.views[i];
        let r = catch_unwind(AssertUnwindSafe(|| {
            self.emu
                .frame(&mut v.view, epoch, v.applied_seq, v.echo_seq, timing)
        }));
        match r {
            Ok(Some(frame)) => {
                let rev = frame.rev;
                let bytes = match wire::encode(&HolderResp::Term(TermDown::Frame(frame))) {
                    Ok(b) => b,
                    Err(e) => {
                        self.too_big(i, &e);
                        return;
                    }
                };
                let n = bytes.len();
                if v.out.try_send(bytes).is_err() {
                    v.dead = true;
                    return;
                }
                v.unacked.push_back((rev, n));
                v.unacked_bytes += n;
                if timing.is_some_and(|t| t.seq != 0) {
                    // The sampled input has had its first response.
                    v.t_in = None;
                    v.t_read = None;
                }
                self.frames += 1;
            }
            Ok(None) => {}
            Err(_) => {
                self.poisoned = true;
                return;
            }
        }
        self.maybe_send_exited(i);
    }

    /// A `Timing` record for the next frame of view `i`: the first response to a sampled input
    /// (`seq % N == 0`), else every Nth frame.
    fn timing_for(&mut self, i: usize) -> Option<Timing> {
        let every = self.sh.timing_every;
        if every == 0 {
            return None;
        }
        let now = self.sh.now_ns();
        let v = &self.views[i];
        if let (Some(t), Some(r)) = (&v.t_in, v.t_read) {
            return Some(Timing {
                seq: t.seq,
                input_recv_ns: t.recv_ns,
                pty_write_ns: t.write_ns,
                pty_read_ns: r,
                frame_sent_ns: now,
            });
        }
        (self.frames % every == every - 1).then_some(Timing {
            pty_read_ns: self.last_read_ns,
            frame_sent_ns: now,
            ..Timing::default()
        })
    }

    fn send_to(&mut self, i: usize, resp: &HolderResp) {
        let bytes = match wire::encode(resp) {
            Ok(b) => b,
            Err(e) => return self.too_big(i, &e),
        };
        if self.views[i].out.try_send(bytes).is_err() {
            self.views[i].dead = true;
        }
    }

    /// A message for view `i` does not encode: it passes the 16 MiB limit (a row holding millions of
    /// combining characters, say). For a frame the emulator has already counted it as sent, so the
    /// view cannot go on with the next delta, and a snapshot would not fit either: say why and
    /// close the view. The client hears the same on every attach until the row is gone.
    fn too_big(&mut self, i: usize, e: &std::io::Error) {
        tracing::warn!(
            pane = self.no,
            view = self.views[i].id,
            "a view's message does not encode: {e}"
        );
        if let Ok(bytes) = wire::encode(&HolderResp::Err(format!("screen too large to send: {e}")))
        {
            let _ = self.views[i].out.try_send(bytes);
        }
        self.views[i].dead = true;
    }

    /// Drop views whose connection is gone or too slow to drain its queue.
    fn reap_views(&mut self) {
        self.views.retain(|v| {
            if v.dead {
                v.kill.notify_one();
            }
            !v.dead
        });
    }

    // ---- recovery --------------------------------------------------------------------------

    /// The emulator panicked: start a fresh one of the same size under a new epoch (clients
    /// reset), and ask the child's foreground job to redraw.
    fn reset_emulator(&mut self) {
        tracing::error!(
            pane = self.no,
            "terminal emulator panicked; resetting this pane's screen"
        );
        self.poisoned = false;
        self.emu = Emulator::new(self.cols, self.rows, SCROLLBACK);
        self.epoch = self.sh.alloc_epoch();
        for v in &mut self.views {
            v.view = self.emu.new_view();
            v.unacked.clear();
            v.unacked_bytes = 0;
        }
        self.winch();
        self.flush_all();
    }

    fn winch(&self) {
        let r = match rustix::termios::tcgetpgrp(&self.fd) {
            Ok(pg) => rustix::process::kill_process_group(pg, Signal::WINCH),
            Err(_) => proc::send(self.pid, Signal::WINCH, false),
        };
        if let Err(e) = r {
            tracing::debug!(pane = self.no, "SIGWINCH: {e}");
        }
    }
}

impl Drop for Pane {
    /// However the task ends, `Info` must not keep claiming the child is alive.
    fn drop(&mut self) {
        let mut i = lock(&self.info);
        if i.alive {
            i.alive = false;
            i.exit = Some(-1);
        }
    }
}
