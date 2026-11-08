//! PTY-mode sessions (M1): one `femme-holder` per session, the agent's own TUI (or a shell) in
//! pane 1, its status from the hooks the agent runs (`femmed hook`, see `femme_agents::hooks`).
//! Headless sessions (M3, [`crate::headless`]) share the holder and the store rows; what differs
//! is who runs the agent: here the TUI in a pane, there a driver in an actor.
//!
//! The daemon keeps one view on pane 1 per live session: input (`/prompt`, `/input`) goes through
//! it, its frames are acked and dropped. Holders outlive the daemon: on start, [`Daemon::reconcile`]
//! reattaches the open sessions in the store to their holders; a session whose holder is gone
//! becomes `Interrupted`, a holder no session claims is stopped.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use femme_agents::hooks::{self, HookEvent};
use femme_proto::holder::{HolderEvent, ScreenFmt, Target};
use femme_proto::state::{Agent, Attention, Mode, Pane, PaneKind, Session, SessionStatus};
use femme_proto::term::{Key, KeyCode, Mods, TermDown, TermUp};
use femme_proto::{Id, PaneId, Sid};
use femme_store::sessions;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use crate::config::Config;
use crate::core::{agent_status, CoreError, CreateSession, PaneRef, Screen, SessionInfo};
use crate::daemon::{core_err, info, Daemon};
use crate::headless::{self, Head};
use crate::holder_client::{Dirs, Error as HolderError, Holder, Limits};

/// Pane 1: the agent's TUI, or the shell.
pub(crate) const MAIN: u16 = 1;
/// A new pane's size until a desktop client owns it (M2); the phone never resizes.
pub(crate) const COLS: u16 = 120;
pub(crate) const ROWS: u16 = 40;
/// How long a holder may take to answer after it was started.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `/prompt` waits for a `Starting` agent to report in with its first hook.
const START_WAIT: Duration = Duration::from_secs(15);
/// Between the pasted text and the Enter that submits it (plan §5).
const ENTER_DELAY: Duration = Duration::from_millis(50);

/// A session with a holder: the daemon's view on its main pane.
pub(crate) struct Live {
    holder: Holder,
    input: mpsc::Sender<TermUp>,
    /// Input sequence on the view: 0 is "untracked", so it starts at 1. Taken under `gate`, so
    /// what the holder sees only ever goes up.
    seq: AtomicU32,
    gate: tokio::sync::Mutex<()>,
}

impl Live {
    async fn send_locked(&self, m: impl FnOnce(u32) -> TermUp) -> Result<(), CoreError> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.input.send(m(seq)).await.map_err(|_| gone())
    }
}

pub struct Sessions {
    pub(crate) dirs: Dirs,
    holder_bin: PathBuf,
    femmed_bin: PathBuf,
    /// Plain child processes instead of transient units (dev, tests).
    direct: bool,
    /// `<state>/hooks`: the agents' hook configs, written at start.
    hooks_dir: PathBuf,
    live: Mutex<HashMap<Sid, Arc<Live>>>,
    /// The actors of headless sessions ([`crate::headless`]).
    heads: Mutex<HashMap<Sid, Head>>,
}

impl Sessions {
    pub fn new(cfg: &Config) -> Sessions {
        let exe = std::env::current_exe().unwrap_or_default();
        let holder_bin = cfg.holder_bin.clone().unwrap_or_else(|| {
            let beside = exe
                .parent()
                .map(|d| d.join("femme-holder"))
                .filter(|p| p.is_file());
            beside.unwrap_or_else(|| cfg.home.join(".local/bin/femme-holder"))
        });
        Sessions {
            dirs: Dirs {
                runtime: cfg.runtime_dir.join("femme"),
                state: cfg.state_dir.clone(),
            },
            holder_bin,
            femmed_bin: cfg.femmed_bin.clone().unwrap_or(exe),
            direct: cfg.holder_direct,
            hooks_dir: cfg.state_dir.join("hooks"),
            live: Mutex::default(),
            heads: Mutex::default(),
        }
    }

    /// The hook configs the agents start with; they depend only on the femmed path.
    pub fn write_hook_files(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.hooks_dir)?;
        let bin = self.femmed_bin.to_string_lossy();
        std::fs::write(
            self.hooks_dir.join("claude.json"),
            hooks::claude_settings_json(&bin),
        )?;
        std::fs::write(
            self.hooks_dir.join("pi.ts"),
            hooks::pi_extension_source(&bin),
        )
    }

    /// The `PATH` an agent runs with. Under systemd neither the daemon nor the holder has the
    /// user's `~/.local/bin` or mise, where `claude`, `codex`, `pi` and `gh` live (bootstrap.sh).
    pub(crate) fn path(&self, cfg: &Config) -> String {
        if let Some(p) = &cfg.tool_path {
            return p.clone();
        }
        let own = std::env::var("PATH").unwrap_or_default();
        let mut dirs: Vec<String> = vec![
            cfg.home.join(".local/bin"),
            cfg.home.join(".local/share/mise/shims"),
        ]
        .into_iter()
        .map(|d| d.to_string_lossy().into_owned())
        .filter(|d| !own.split(':').any(|p| p == d))
        .collect();
        dirs.push(own);
        dirs.join(":")
    }

    /// The agent's command line, its binary found on `path` (the holder execs without a shell).
    fn command(&self, agent: Agent, path: &str) -> Result<Vec<String>, CoreError> {
        let bin = self.femmed_bin.to_string_lossy();
        let file = |name: &str| self.hooks_dir.join(name).to_string_lossy().into_owned();
        let (prog, args): (String, Vec<String>) = match agent {
            Agent::Shell => (
                std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into()),
                vec![],
            ),
            Agent::Claude => (
                "claude".into(),
                vec!["--settings".into(), file("claude.json")],
            ),
            Agent::Codex => (
                "codex".into(),
                std::iter::once("--no-daemon".to_string())
                    .chain(hooks::codex_hook_args(&bin))
                    .collect(),
            ),
            Agent::Pi => ("pi".into(), vec!["-e".into(), file("pi.ts")]),
        };
        let prog = find_on_path(&prog, path)
            .ok_or_else(|| CoreError::Internal(format!("{prog}: not found on PATH")))?;
        Ok(std::iter::once(prog).chain(args).collect())
    }

    /// What a pane's child gets on top of the holder's allowlist. `femmed hook` finds the daemon
    /// through its config: the same `.env` and bind as ours.
    pub(crate) fn child_env(&self, cfg: &Config, path: &str) -> Vec<(String, String)> {
        let mut env = vec![
            ("PATH".to_string(), path.to_string()),
            ("HOME".to_string(), cfg.home.to_string_lossy().into_owned()),
            ("HTTP_BIND".to_string(), cfg.http_bind.clone()),
            (
                "FEMME_STATE".to_string(),
                cfg.state_dir.to_string_lossy().into_owned(),
            ),
        ];
        if let Some(f) = std::env::var_os("FEMME_ENV") {
            env.push(("FEMME_ENV".to_string(), f.to_string_lossy().into_owned()));
        }
        env
    }

    pub(crate) async fn spawn_holder(&self, sid: Sid) -> Result<Holder, HolderError> {
        if self.direct {
            Holder::spawn_direct(&self.dirs, sid, &self.holder_bin, SPAWN_TIMEOUT).await
        } else {
            Holder::spawn(
                &self.dirs,
                sid,
                &self.holder_bin,
                &Limits::default(),
                SPAWN_TIMEOUT,
            )
            .await
        }
    }

    fn live(&self, sid: Sid) -> Result<Arc<Live>, CoreError> {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&sid)
            .cloned()
            .ok_or_else(gone)
    }

    pub(crate) fn forget(&self, sid: Sid) -> Option<Arc<Live>> {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&sid)
    }

    /// The hook file the agent's TUI starts with, for the agents that take one as a path.
    pub(crate) fn hooks_path(&self, agent: Agent) -> Option<String> {
        let file = match agent {
            Agent::Claude => "claude.json",
            Agent::Pi => "pi.ts",
            Agent::Shell | Agent::Codex => return None,
        };
        Some(self.hooks_dir.join(file).to_string_lossy().into_owned())
    }

    pub(crate) fn head(&self, sid: Sid) -> Option<Head> {
        self.heads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&sid)
            .cloned()
    }

    pub(crate) fn set_head(&self, sid: Sid, head: Head) {
        self.heads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(sid, head);
    }

    pub(crate) fn unhead(&self, sid: Sid) {
        self.heads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&sid);
    }
}

/// A session the daemon lost: no holder to reach, or one it could not take over again.
fn interrupted(s: &mut Session) {
    s.status = SessionStatus::Interrupted;
    s.attention = Attention::None;
}

/// `name` as an absolute path: itself when it has a slash, else the first match on `path`.
/// OpenGateway's Anthropic-compatible endpoint (https://opengateway.ai/docs/coding-agents/claude-code).
const GATEWAY: &str = "https://apis.opengateway.ai";

/// What a Claude process needs on top of its usual environment when its `model` names its maker
/// (`deepseek/deepseek-v4.1-flash-ultrafast`): the gateway as its API, the key from
/// `<home>/.config/opengateway/key`, and that model in every slot (subagents too). Nothing for
/// Anthropic's own models or none. `server/claude-deepseek.sh` does the same for a terminal.
pub(crate) fn gateway_env(
    home: &Path,
    model: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let Some(model) = model.filter(|m| m.contains('/')) else {
        return Ok(Vec::new());
    };
    let file = home.join(".config/opengateway/key");
    let key = std::fs::read_to_string(&file)
        .map(|k| k.trim().to_owned())
        .ok()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| {
            format!(
                "{model} runs through OpenGateway: no key in {}",
                file.display()
            )
        })?;
    let mut env = vec![
        ("ANTHROPIC_BASE_URL".to_owned(), GATEWAY.to_owned()),
        ("ANTHROPIC_AUTH_TOKEN".to_owned(), key),
    ];
    for slot in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ] {
        env.push((slot.to_owned(), model.to_owned()));
    }
    Ok(env)
}

pub(crate) fn find_on_path(name: &str, path: &str) -> Option<String> {
    if name.contains('/') {
        return Path::new(name).is_file().then(|| name.to_string());
    }
    path.split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

pub(crate) fn gone() -> CoreError {
    CoreError::Internal("the session's terminal is gone".into())
}

pub(crate) fn holder_err(e: HolderError) -> CoreError {
    CoreError::Internal(format!("holder: {e}"))
}

pub(crate) fn agent_label(agent: Agent) -> &'static str {
    match agent {
        Agent::Shell => "The shell",
        Agent::Claude => "Claude",
        Agent::Codex => "Codex",
        Agent::Pi => "pi",
    }
}

/// What a session is called in a push: its label, name or handle.
pub(crate) fn label(s: &Session) -> String {
    s.label
        .clone()
        .or_else(|| s.name.clone())
        .unwrap_or_else(|| format!("w{}", s.handle))
}

/// A hook's effect on the session (pure).
pub fn apply_hook(s: &mut Session, ev: &HookEvent) {
    match ev {
        HookEvent::Started { agent_sid, .. } => {
            if agent_sid.is_some() {
                s.agent_sid = agent_sid.clone();
            }
            if s.status == SessionStatus::Starting {
                s.status = SessionStatus::Idle;
            }
        }
        HookEvent::Working => {
            s.status = SessionStatus::Working;
            s.attention = Attention::None;
        }
        HookEvent::Idle => {
            // A turn that ended unseen is `done` until someone looks; a dismissed dialog is just idle.
            s.attention = if s.status == SessionStatus::Working {
                Attention::Done
            } else {
                Attention::None
            };
            s.status = SessionStatus::Idle;
        }
        HookEvent::Blocked { .. } => {
            s.status = SessionStatus::Blocked;
            s.attention = Attention::Blocked;
        }
        // The process is exiting: the pane's exit closes the session a moment later.
        HookEvent::Stopped => {
            s.status = SessionStatus::Idle;
            s.attention = Attention::None;
        }
    }
}

enum Exit {
    /// The main pane's child ended with this status.
    Pane(i32),
    /// The holder itself is gone.
    Gone,
}

/// Acks every frame of the daemon's view and drops it (the view exists for input).
async fn ack_frames(mut down: mpsc::Receiver<TermDown>, up: mpsc::WeakSender<TermUp>) {
    while let Some(m) = down.recv().await {
        if let TermDown::Frame(f) = m {
            let Some(up) = up.upgrade() else { break };
            if up.send(TermUp::Ack { rev: f.rev }).await.is_err() {
                break;
            }
        }
    }
}

/// Follows a holder's events until its main pane exits or it goes away, then settles the session.
async fn follow(
    daemon: Weak<Daemon>,
    holder: Holder,
    mut events: broadcast::Receiver<HolderEvent>,
    sid: Sid,
) {
    let exit = loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Ok(HolderEvent::ChildExited { target: Target::Pane(MAIN), status }) => break Exit::Pane(status),
                // A shell pane the desktop opened ended, on its own or hung up by `close_pane`: its
                // row goes, and the desktop hears it as any other change. `NotFound` is
                // `close_pane` having been first.
                // ponytail: a shell that ends before `open_pane` has written its row leaves the
                // row; the desktop's close removes it.
                Ok(HolderEvent::ChildExited { target: Target::Pane(no), .. }) => {
                    let Some(d) = daemon.upgrade() else { return };
                    let _ = d.write(|tx, _| tx.delete_pane(PaneId { sid, no }));
                }
                Ok(HolderEvent::Title { pane: MAIN, title }) => {
                    let Some(d) = daemon.upgrade() else { return };
                    let id = PaneId { sid, no: MAIN };
                    let _ = d.write(|tx, _| {
                        let p = sessions::get_pane(tx, id)?.ok_or(femme_store::Error::NotFound)?;
                        tx.upsert_pane(&Pane { title, ..p })
                    });
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break Exit::Gone,
            },
            _ = holder.closed() => break Exit::Gone,
        }
    };
    let Some(d) = daemon.upgrade() else { return };
    d.sessions.forget(sid);
    match exit {
        Exit::Pane(status) => {
            tracing::info!(%sid, status, "main pane exited; closing the session");
            let _ = holder.shutdown().await;
            if let Err(e) = d.write(|tx, now| tx.close_session(sid, now)) {
                tracing::warn!(%sid, %e, "session not closed in the store");
            }
        }
        Exit::Gone => {
            let r = d.write(|tx, _| {
                tx.update_session(sid, |s| {
                    if s.closed.is_none() {
                        s.status = SessionStatus::Crashed;
                        s.attention = Attention::None;
                    }
                })
            });
            match r {
                Ok(s) if s.closed.is_none() => tracing::warn!(%sid, "holder gone; session crashed"),
                Ok(_) => {}
                Err(e) => tracing::warn!(%sid, %e, "holder gone; session not marked"),
            }
        }
    }
}

impl Daemon {
    /// The open session a compat pane id names, with the pane.
    pub(crate) fn resolve(&self, pane: &PaneRef) -> Result<(Session, Pane), CoreError> {
        let found = self
            .store
            .read(|c| {
                let Some(s) = sessions::session_by_handle(c, &pane.handle)? else {
                    return Ok(None);
                };
                Ok(sessions::get_pane(
                    c,
                    PaneId {
                        sid: s.id,
                        no: pane.no,
                    },
                )?
                .map(|p| (s, p)))
            })
            .map_err(core_err)?;
        match found {
            // Sessions with a name live on the `instinct` machine, the rest on none.
            Some((s, p))
                if s.closed.is_none()
                    && pane.machine.as_deref() == s.name.is_some().then_some("instinct") =>
            {
                Ok((s, p))
            }
            _ => Err(CoreError::NotFound),
        }
    }

    pub(crate) fn session(&self, sid: Sid) -> Result<Session, CoreError> {
        self.store
            .read(|c| sessions::get_session(c, sid))
            .map_err(core_err)?
            .filter(|s| s.closed.is_none())
            .ok_or(CoreError::NotFound)
    }

    pub(crate) fn update(
        &self,
        sid: Sid,
        f: impl FnOnce(&mut Session),
    ) -> Result<Session, CoreError> {
        self.write(|tx, _| tx.update_session(sid, f))
    }

    /// The daemon's view on a holder's main pane: where `/prompt` and `/input` go.
    pub(crate) async fn view(&self, holder: &Holder, sid: Sid) -> Result<(), CoreError> {
        // A fresh client instance per attach: the holder's seq dedup starts over with us.
        let (input, down) = holder
            .attach(MAIN, holder.info.epoch, 0, Id::new().0)
            .await
            .map_err(holder_err)?;
        tokio::spawn(ack_frames(down, input.downgrade()));
        let live = Arc::new(Live {
            holder: holder.clone(),
            input,
            seq: AtomicU32::new(1),
            gate: tokio::sync::Mutex::new(()),
        });
        self.sessions
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(sid, live);
        Ok(())
    }

    /// Takes a holder (just started, or found at start): the view on its main pane and the task
    /// that follows its events.
    async fn adopt(&self, holder: Holder, sid: Sid) -> Result<(), CoreError> {
        let events = holder.subscribe();
        self.view(&holder, sid).await?;
        tokio::spawn(follow(self.me.clone(), holder, events, sid));
        Ok(())
    }

    /// Types `text` into the main pane and submits it: the paste, a moment, the Enter (plan §5).
    pub(crate) async fn type_into(&self, sid: Sid, text: &str) -> Result<(), CoreError> {
        let live = self.sessions.live(sid)?;
        let _g = live.gate.lock().await;
        live.send_locked(|seq| TermUp::Paste {
            seq,
            text: text.to_string(),
        })
        .await?;
        tokio::time::sleep(ENTER_DELAY).await;
        live.send_locked(|seq| TermUp::Key {
            seq,
            key: Key {
                code: KeyCode::Enter,
                mods: Mods::empty(),
            },
        })
        .await
    }

    /// Raw bytes into the main pane.
    pub(crate) async fn send_bytes(&self, sid: Sid, bytes: &[u8]) -> Result<(), CoreError> {
        let live = self.sessions.live(sid)?;
        let _g = live.gate.lock().await;
        live.send_locked(|seq| TermUp::Bytes {
            seq,
            data: bytes.to_vec(),
        })
        .await
    }

    /// Reattaches every open session to its holder. No holder → `Interrupted`; holder but a
    /// dead main pane → closed; a holder no open session claims → stopped.
    pub async fn reconcile(&self) {
        let open = self
            .store
            .read(|c| sessions::list_sessions(c, false))
            .unwrap_or_default();
        let claimed: HashSet<Sid> = open.iter().map(|s| s.id).collect();
        for s in open {
            if s.owner.starts_with(headless::herdr::OWNER) {
                continue; // a herdr pane's: its poller keeps it
            }
            // Born headless (it has attach state): its actor takes it over, whatever mode it is in now.
            let managed = s.agent != Agent::Shell
                && self
                    .store
                    .read(|c| sessions::agent_state(c, s.id))
                    .ok()
                    .flatten()
                    .is_some();
            if managed && s.status == SessionStatus::Failed {
                continue; // nothing runs; closing it is the user's
            }
            match Holder::connect(&self.sessions.dirs, s.id).await {
                Ok(h) if managed => match headless::reattach(self, &s, h).await {
                    Ok(()) => tracing::info!(sid = %s.id, "headless session reattached"),
                    Err(e) => {
                        // No actor will drive it: its row must not go on saying `working`.
                        let _ = self.update(s.id, interrupted);
                        tracing::warn!(sid = %s.id, %e, "headless session not reattached");
                    }
                },
                Ok(h) if h.info.panes.iter().any(|p| p.no == MAIN && p.alive) => {
                    match self.adopt(h, s.id).await {
                        Ok(()) => tracing::info!(sid = %s.id, "session reattached"),
                        Err(e) => tracing::warn!(sid = %s.id, %e, "session not reattached"),
                    }
                }
                Ok(h) => {
                    let _ = h.shutdown().await;
                    let _ = self.write(|tx, now| tx.close_session(s.id, now));
                    tracing::info!(sid = %s.id, "session's pane had exited; closed");
                }
                Err(_) if s.status != SessionStatus::Interrupted => {
                    let _ = self.update(s.id, interrupted);
                    tracing::warn!(sid = %s.id, "no holder; session interrupted");
                }
                Err(_) => {}
            }
        }
        for sid in self.sessions.dirs.live_sockets() {
            if claimed.contains(&sid) {
                continue; // reattached above
            }
            // A read error counts as unknown: leaving a holder up is recoverable, stopping it is not.
            let known = matches!(
                self.store.read(|c| sessions::get_session(c, sid)),
                Ok(Some(_))
            );
            if !sweepable(known, claimed.contains(&sid)) {
                tracing::info!(%sid, "holder of a session this store never had; leaving it alone");
                continue;
            }
            if let Ok(h) = Holder::connect(&self.sessions.dirs, sid).await {
                tracing::warn!(%sid, "holder of no open session; stopping it");
                let _ = h.shutdown().await;
            }
        }
    }

    pub(crate) async fn open_session(&self, req: CreateSession) -> Result<SessionInfo, CoreError> {
        let path = self.sessions.path(&self.cfg);
        let not_started = |e: String| {
            CoreError::Internal(format!("{} did not start: {e}", agent_label(req.agent)))
        };
        // A shell is always a terminal.
        let headless = req.mode == Mode::Headless && req.agent != Agent::Shell;
        let cmd = if headless {
            Vec::new()
        } else {
            self.sessions
                .command(req.agent, &path)
                .map_err(|e| not_started(e.to_string()))?
        };
        let (kind, status) = match req.agent {
            Agent::Shell => (PaneKind::Shell, SessionStatus::Idle),
            _ => (PaneKind::Agent, SessionStatus::Starting),
        };
        let (session, pane) = self.write(|tx, now| {
            let s = tx.insert_session(&Session {
                id: Id(0),
                handle: String::new(),
                rev: 0,
                name: req.name.clone(),
                owner: req.owner.clone(),
                agent: req.agent,
                mode: if headless { Mode::Headless } else { Mode::Tui },
                cwd: req.cwd.clone(),
                label: req.label.clone(),
                agent_sid: None,
                model: req.model.clone(),
                status,
                attention: Attention::None,
                created: now,
                closed: None,
                permission: None,
                context: None,
            })?;
            let p = tx.upsert_pane(&Pane {
                id: PaneId {
                    sid: s.id,
                    no: MAIN,
                },
                rev: 0,
                kind,
                title: String::new(),
            })?;
            if headless {
                // its presence marks the session as the actor's, for `reconcile`
                tx.set_agent_state(s.id, 0, &headless::Saved::default().json())?;
            }
            Ok((s, p))
        })?;
        let sid = session.id;
        let started = async {
            let holder = self
                .sessions
                .spawn_holder(sid)
                .await
                .map_err(|e| e.to_string())?;
            if headless {
                return headless::start(self, &session, holder).await;
            }
            let env = self.sessions.child_env(&self.cfg, &path);
            if let Err(e) = holder
                .spawn_pty(MAIN, cmd, req.cwd.clone(), env, COLS, ROWS)
                .await
            {
                let _ = holder.shutdown().await;
                return Err(e.to_string());
            }
            self.adopt(holder, sid).await.map_err(|e| e.to_string())
        };
        match started.await {
            Ok(()) => Ok(info(&session, vec![pane])),
            Err(e) => {
                // It never ran: no row to keep.
                let _ = self.write(|tx, _| tx.delete_session(sid));
                Err(not_started(e))
            }
        }
    }

    pub(crate) async fn end_session(&self, sid: Sid) -> Result<(), CoreError> {
        self.session(sid)?;
        // A headless session's actor stops (and its holder with it) before the row is closed, so
        // nothing of it writes over the closed row.
        if let Some(h) = self.sessions.head(sid) {
            h.close().await;
            self.sessions.unhead(sid);
        }
        self.write(|tx, now| tx.close_session(sid, now))?;
        match self.sessions.forget(sid) {
            Some(l) => {
                let _ = l.holder.shutdown().await;
            }
            // Nothing attached (found no holder at start, or it died): its unit may still be around.
            None => {
                if let Ok(h) = Holder::connect(&self.sessions.dirs, sid).await {
                    let _ = h.shutdown().await;
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn pane_screen(
        &self,
        pane: &PaneRef,
        ansi: bool,
    ) -> Result<Screen, CoreError> {
        let (s, p) = self.resolve(pane)?;
        let text = if s.mode == Mode::Headless {
            // ponytail: no colours on a transcript's text, `ansi` is ignored
            self.headless_screen(s.id).await?
        } else {
            self.sessions
                .live(s.id)?
                .holder
                .screen(
                    p.id.no,
                    if ansi {
                        ScreenFmt::Ansi
                    } else {
                        ScreenFmt::Text
                    },
                )
                .await
                .map_err(holder_err)?
        };
        let status =
            (s.agent != Agent::Shell).then(|| agent_status(s.status, s.attention).to_string());
        // Looked at: `done` shows once (herdr's did the same).
        if s.attention == Attention::Done {
            let _ = self.update(s.id, |s| s.attention = Attention::None);
        }
        Ok(Screen { text, status })
    }

    pub(crate) async fn pane_prompt(&self, pane: &PaneRef, text: &str) -> Result<(), CoreError> {
        let (mut s, p) = self.resolve(pane)?;
        if s.agent == Agent::Shell || p.kind != PaneKind::Agent {
            return Err(CoreError::Busy);
        }
        // a TUI has its own `/btw`
        if s.agent == Agent::Claude && s.mode == Mode::Headless {
            if let Some(q) = headless::btw::question(text) {
                return headless::btw::ask(self, &s, q);
            }
        }
        if s.mode == Mode::Headless {
            return self
                .sessions
                .head(s.id)
                .ok_or_else(gone)?
                .prompt(text)
                .await;
        }
        // A fresh agent reports in with its first hook; give it a moment.
        // ponytail: polls the store; a hub subscription when the desktop needs the same wait.
        let deadline = tokio::time::Instant::now() + START_WAIT;
        while s.status == SessionStatus::Starting && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
            s = self.session(s.id)?;
        }
        let takes_input = matches!(
            s.status,
            SessionStatus::Idle | SessionStatus::Working | SessionStatus::Paused
        );
        if !takes_input || s.attention == Attention::Blocked {
            return Err(CoreError::Busy);
        }
        self.sessions.live(s.id)?;
        // The agent's hook says `working` a moment after the Enter; say it first, so a reader never
        // takes the last task as finished in between, and so a fast agent's `Stop` cannot be
        // overwritten by this. A failed send leaves it `working` until the next hook or the
        // holder's exit corrects it.
        self.update(s.id, |s| {
            s.status = SessionStatus::Working;
            s.attention = Attention::None;
        })?;
        self.type_into(s.id, text).await
    }

    pub(crate) async fn pane_input(&self, pane: &PaneRef, bytes: &[u8]) -> Result<(), CoreError> {
        let (s, _) = self.resolve(pane)?;
        if s.mode == Mode::Headless {
            return self.headless_input(s.id, bytes).await;
        }
        self.send_bytes(s.id, bytes).await
    }

    pub(crate) fn agent_hook(&self, agent: Agent, payload: Value) -> Result<(), CoreError> {
        let sid: Sid = payload["sid"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| CoreError::Bad("sid: not a session id".into()))?;
        let s = self.session(sid)?;
        if s.agent != agent {
            return Err(CoreError::Bad(format!(
                "the session runs {}, not {}",
                agent_label(s.agent),
                agent_label(agent)
            )));
        }
        let event = payload["event"].as_str().unwrap_or("");
        let Some(ev) = hooks::parse_hook(agent, event, &payload.to_string()) else {
            return Ok(());
        };
        let head = self.sessions.head(sid);
        // A headless session's status is its stream's: a late hook of the TUI it left is dropped.
        if s.mode == Mode::Headless && head.is_some() {
            return Ok(());
        }
        let before = s.attention;
        let after = self.update(sid, |s| apply_hook(s, &ev))?;
        if after.attention != before {
            let who = match &ev {
                HookEvent::Blocked { text } => format!("{}: {text}", label(&after)),
                _ => label(&after),
            };
            self.notifier.attention(sid, &who, after.attention);
        }
        // the takeover machine follows the turns of the TUI it started
        if let Some(h) = head {
            h.hook(ev);
        }
        Ok(())
    }
}

/// Whether reconcile stops a live holder: only one whose session this store knows (a row, open or
/// closed) and no open session claims. A holder with no row at all belongs to another daemon's
/// state sharing the runtime dir (a test's femmed on the real `/run/user/<uid>`), not to us.
fn sweepable(known: bool, claimed: bool) -> bool {
    known && !claimed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(status: SessionStatus, attention: Attention) -> Session {
        Session {
            id: Id(1),
            handle: "h".into(),
            rev: 1,
            name: None,
            owner: "board".into(),
            agent: Agent::Claude,
            mode: Mode::Tui,
            cwd: "/".into(),
            label: None,
            agent_sid: None,
            model: None,
            status,
            attention,
            created: 0,
            closed: None,
            permission: None,
            context: None,
        }
    }

    #[test]
    fn the_sweep_stops_only_holders_this_store_knows_and_does_not_claim() {
        assert!(sweepable(true, false), "a known, unclaimed holder is an orphan of ours");
        assert!(!sweepable(true, true), "a claimed holder is reattached, not stopped");
        assert!(!sweepable(false, false), "a holder with no row is another daemon's");
        assert!(!sweepable(false, true));
    }

    #[test]
    fn hooks_move_the_status() {
        use SessionStatus::*;
        let mut s = session(Starting, Attention::None);
        apply_hook(
            &mut s,
            &HookEvent::Started {
                agent_sid: Some("abc".into()),
                transcript: None,
            },
        );
        assert_eq!((s.status, s.agent_sid.as_deref()), (Idle, Some("abc")));
        apply_hook(&mut s, &HookEvent::Working);
        assert_eq!(s.status, Working);
        apply_hook(&mut s, &HookEvent::Idle);
        assert_eq!(
            (s.status, s.attention),
            (Idle, Attention::Done),
            "a finished turn is done until seen"
        );
        apply_hook(&mut s, &HookEvent::Working);
        apply_hook(
            &mut s,
            &HookEvent::Blocked {
                text: "A or B?".into(),
            },
        );
        assert_eq!((s.status, s.attention), (Blocked, Attention::Blocked));
        apply_hook(&mut s, &HookEvent::Idle);
        assert_eq!(
            (s.status, s.attention),
            (Idle, Attention::None),
            "a dismissed dialog is just idle"
        );
        // a resume keeps the status and a Started without an id keeps the old one
        apply_hook(
            &mut s,
            &HookEvent::Started {
                agent_sid: None,
                transcript: None,
            },
        );
        assert_eq!((s.status, s.agent_sid.as_deref()), (Idle, Some("abc")));
    }

    #[test]
    fn path_lookup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tool"), "").unwrap();
        let path = format!(":/nonexistent:{}", dir.path().display());
        assert_eq!(
            find_on_path("tool", &path).as_deref(),
            Some(dir.path().join("tool").to_str().unwrap())
        );
        assert_eq!(find_on_path("other", &path), None);
        assert!(find_on_path(dir.path().join("tool").to_str().unwrap(), "").is_some());
        assert_eq!(find_on_path("/nonexistent/x", ""), None);
    }

    #[test]
    fn the_agents_path_gets_the_users_bins() {
        let cfg = Config::from_vars([("HOME".to_string(), "/home/u".to_string())]);
        let s = Sessions::new(&cfg);
        let p = s.path(&cfg);
        assert!(
            p.starts_with("/home/u/.local/bin:/home/u/.local/share/mise/shims:"),
            "{p}"
        );
        let cfg = Config {
            tool_path: Some("/fake".into()),
            ..cfg
        };
        assert_eq!(s.path(&cfg), "/fake");
    }

    #[test]
    fn a_model_that_names_its_maker_runs_through_opengateway() {
        let home = tempfile::tempdir().unwrap();
        // Anthropic's models, or none: nothing changes.
        assert_eq!(gateway_env(home.path(), None), Ok(vec![]));
        assert_eq!(gateway_env(home.path(), Some("opus")), Ok(vec![]));
        // A gateway model without a key fails with where the key goes.
        let m = "deepseek/deepseek-v4.1-flash-ultrafast";
        let err = gateway_env(home.path(), Some(m)).unwrap_err();
        assert!(err.contains(".config/opengateway/key"), "{err}");
        std::fs::create_dir_all(home.path().join(".config/opengateway")).unwrap();
        std::fs::write(home.path().join(".config/opengateway/key"), "sk-test\n").unwrap();
        let env = gateway_env(home.path(), Some(m)).unwrap();
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("ANTHROPIC_BASE_URL"), Some(GATEWAY));
        assert_eq!(get("ANTHROPIC_AUTH_TOKEN"), Some("sk-test"));
        assert_eq!(get("CLAUDE_CODE_SUBAGENT_MODEL"), Some(m));
        assert_eq!(get("ANTHROPIC_DEFAULT_HAIKU_MODEL"), Some(m));
    }
}
