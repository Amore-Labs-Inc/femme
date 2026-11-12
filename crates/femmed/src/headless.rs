//! Headless sessions (M3): the agent runs with piped stdio in its session's holder, and one actor
//! task per session drives it through the sans-IO driver of `femme-agents`. The actor owns the
//! driver: commands (prompt, interrupt, answer, model, takeover, close) arrive on a channel, the
//! agent's stdout arrives from the holder's spool, and after EVERY driver call what the driver
//! wants written goes to the agent's stdin. What the driver reports lands in the store (transcript
//! items, the session's status) and on the hub, and the spool offset that covers it is committed in
//! the same transaction, then told to the holder.
//!
//! A daemon restart leaves the agent running in its holder: [`Daemon::reconcile`] calls
//! [`reattach`], which builds a fresh driver, `attach`es it with what was persisted (`agent_state`)
//! and replays the spool from `spool_committed`. A replay writes nothing that is already there
//! ([`put`]), so the transcript after a restart is the one an uninterrupted run would have made.
//!
//! The takeover machine ([`crate::takeover`]) runs in the actor too: it moves a session between its
//! headless agent and the agent's own TUI in pane 1, never with two agent processes at once.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Weak;
use std::time::Duration;

use bytes::Bytes;
use femme_agents::hooks::HookEvent;
use femme_agents::items::{body, Event, QuestionItem, TurnEnd};
use femme_agents::{
    driver_for, AgentEvent, AgentStatus, Attach, Driver, DropReason, Error as DriverError,
    HeldPrompt, Launch, Out, StopReason,
};
use femme_proto::holder::{HolderEvent, Target};
use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::{
    Agent, Attention, Context, Goal, GoalState, Item, ItemKind, ItemStatus, Mode, Question,
    Session, SessionStatus,
};
use femme_proto::{Id, Sid};
use femme_store::{items as store_items, sessions, Store, Tx};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::Instant;

use crate::core::CoreError;
use crate::daemon::{core_err, now_ms, Daemon};
use crate::holder_client::{Holder, SpoolLines};
use crate::scheduler::UsageSnapshot;
use crate::sent;
use crate::session::{
    agent_label, find_on_path, gateway_env, gone, holder_err, label, COLS, MAIN, ROWS,
};
use crate::takeover::{Act, Ev, Phase, Takeover};

pub(crate) mod btw;
pub(crate) mod herdr;

/// What ends the agent's TUI politely. ponytail: Claude's, for every agent; Codex and Pi get theirs
/// when their TUIs are wired.
const EXIT_CMD: &str = "/exit\r";
/// From an unrequested exit to the restart.
const RESTART_DELAY: Duration = Duration::from_secs(2);
/// A second unrequested exit this soon after a restart fails the session.
const FAIL_WINDOW: Duration = Duration::from_secs(30);
/// How long, after an exit, the actor waits for the spool bytes the agent wrote before it died.
const DRAIN: Duration = Duration::from_secs(2);
/// The takeover machine's clock.
const TICK: Duration = Duration::from_millis(100);
/// Items on a headless session's `/screen`, and the most of one item shown.
// ponytail: the last 50 items, each cut at 4000 bytes, no paging; the desktop reads the store's
// pages, add a cursor to `/screen` if a client wants the rest.
const SCREEN_ITEMS: usize = 50;
const SCREEN_ITEM_MAX: usize = 4000;
/// The most of the agent's stderr that goes into an `Event` item.
const STDERR_MAX: usize = 4096;

// ---- what the driver keeps that the spool does not hold -----------------------------------------

/// `agent_state.json`: [`Driver::pending_answers`] and the prompts that wait for the agent
/// ([`Driver::held_prompts`], and the ones waiting for the next process), written whenever they
/// change.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Saved {
    answers: Vec<(String, Answer)>,
    held: Vec<HeldPrompt>,
}

impl Saved {
    pub(crate) fn json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

// ---- the handle ---------------------------------------------------------------------------------

type Reply = oneshot::Sender<Result<(), CoreError>>;

enum Cmd {
    Prompt {
        text: String,
        how: PromptHow,
        /// The id the agent is to echo it under ([`crate::sent`]).
        id: Option<String>,
        reply: Reply,
    },
    Interrupt {
        reply: Reply,
    },
    /// Ctrl+B: the foreground task of the call (or every one) goes to the background.
    Background {
        tool_use_id: Option<String>,
        reply: Reply,
    },
    Answer {
        request_id: String,
        answer: Answer,
        reply: Reply,
    },
    SetModel {
        model: String,
        reply: Reply,
    },
    /// Claude's permission mode (`plan`, `auto`, ...); a herdr pane steps to its next one.
    SetPermission {
        mode: String,
        reply: Reply,
    },
    Takeover {
        to: Mode,
        reply: Reply,
    },
    /// Stop everything: the holder and its children go.
    Close {
        done: oneshot::Sender<()>,
    },
    /// A hook of the agent's TUI (takeover phases that wait for a turn to end).
    Hook(HookEvent),
    /// The text streamed so far of the items still open (the store has them when they are done).
    Streams {
        reply: oneshot::Sender<HashMap<String, String>>,
    },
}

/// The way into a session's actor. Dropping the last one stops the actor and leaves the agent be.
#[derive(Clone)]
pub(crate) struct Head {
    tx: mpsc::Sender<Cmd>,
}

impl Head {
    async fn call(&self, cmd: impl FnOnce(Reply) -> Cmd) -> Result<(), CoreError> {
        let (reply, answer) = oneshot::channel();
        self.tx.send(cmd(reply)).await.map_err(|_| gone())?;
        answer.await.map_err(|_| gone())?
    }

    #[cfg(test)]
    pub(crate) async fn prompt(&self, text: &str, how: PromptHow) -> Result<(), CoreError> {
        self.prompt_as(text, how, None).await
    }

    /// [`Head::prompt`], echoed by the agent under `id` when it takes one.
    pub(crate) async fn prompt_as(
        &self,
        text: &str,
        how: PromptHow,
        id: Option<String>,
    ) -> Result<(), CoreError> {
        self.call(|reply| Cmd::Prompt {
            text: text.into(),
            how,
            id,
            reply,
        })
        .await
    }

    pub(crate) async fn interrupt(&self) -> Result<(), CoreError> {
        self.call(|reply| Cmd::Interrupt { reply }).await
    }

    pub(crate) async fn background(&self, tool_use_id: Option<&str>) -> Result<(), CoreError> {
        self.call(|reply| Cmd::Background {
            tool_use_id: tool_use_id.map(str::to_owned),
            reply,
        })
        .await
    }

    pub(crate) async fn answer(&self, request_id: &str, answer: Answer) -> Result<(), CoreError> {
        self.call(|reply| Cmd::Answer {
            request_id: request_id.into(),
            answer,
            reply,
        })
        .await
    }

    pub(crate) async fn set_model(&self, model: &str) -> Result<(), CoreError> {
        self.call(|reply| Cmd::SetModel {
            model: model.into(),
            reply,
        })
        .await
    }

    pub(crate) async fn set_permission(&self, mode: &str) -> Result<(), CoreError> {
        self.call(|reply| Cmd::SetPermission {
            mode: mode.into(),
            reply,
        })
        .await
    }

    pub(crate) async fn takeover(&self, to: Mode) -> Result<(), CoreError> {
        self.call(|reply| Cmd::Takeover { to, reply }).await
    }

    pub(crate) async fn close(&self) {
        let (done, wait) = oneshot::channel();
        if self.tx.send(Cmd::Close { done }).await.is_ok() {
            let _ = wait.await;
        }
    }

    /// Passes a TUI hook on (fire and forget: the hook route must not wait for the actor).
    pub(crate) fn hook(&self, ev: HookEvent) {
        let _ = self.tx.try_send(Cmd::Hook(ev));
    }

    /// What the open items have streamed so far (empty when the actor is gone).
    pub(crate) async fn streams(&self) -> HashMap<String, String> {
        let (reply, answer) = oneshot::channel();
        if self.tx.send(Cmd::Streams { reply }).await.is_err() {
            return HashMap::new();
        }
        answer.await.unwrap_or_default()
    }
}

fn driver_err(e: DriverError) -> CoreError {
    match e {
        DriverError::State(_) => CoreError::Busy,
        DriverError::UnknownRequest(_) => CoreError::Conflict(e.to_string()),
        DriverError::Unsupported(_) | DriverError::Bad(_) => CoreError::Bad(e.to_string()),
    }
}

// ---- entry points -------------------------------------------------------------------------------

/// Starts the agent of a new session in its holder and the actor that drives it. On failure the
/// holder is stopped (the caller deletes the session).
pub(crate) async fn start(d: &Daemon, s: &Session, holder: Holder) -> Result<(), String> {
    let mut a = Actor::new(d, s, holder.clone(), 0, 0, Saved::default());
    let events = holder.subscribe();
    let up = async {
        a.spool = Some(holder.spool_read(0).await.map_err(|e| e.to_string())?);
        a.spawn_process(d, None).await
    };
    if let Err(e) = up.await {
        let _ = holder.shutdown().await;
        return Err(e);
    }
    a.launch(d, events);
    Ok(())
}

/// Takes over the actor of a session whose holder outlived the daemon. A headless session gets a
/// fresh driver, `attach`ed, and the spool replayed from `spool_committed`; one taken over by its
/// TUI gets the machine back in its TUI phase (and resumes headless if the TUI ended meanwhile).
pub(crate) async fn reattach(d: &Daemon, s: &Session, holder: Holder) -> Result<(), CoreError> {
    let info = holder.info().await.map_err(holder_err)?;
    let (committed, state) = d
        .store
        .read(|c| {
            Ok((
                sessions::spool_committed(c, s.id)?,
                sessions::agent_state(c, s.id)?,
            ))
        })
        .map_err(core_err)?;
    let (epoch, saved) = state.map_or((0, Saved::default()), |(epoch, json)| {
        (epoch, serde_json::from_str(&json).unwrap_or_default())
    });
    let (hi, alive, exit) = info.agent.as_ref().map_or((0, false, -1), |a| {
        (a.spool_hi, a.alive, a.exit.unwrap_or(-1))
    });
    let mut a = Actor::new(d, s, holder.clone(), committed, epoch + 1, saved.clone());
    let events = holder.subscribe();
    if s.mode == Mode::Tui {
        // ponytail: what the headless agent wrote while the daemon was away, if anything, is not read
        a.committed = committed.max(hi);
        a.lines = SpoolLines::new(a.committed);
        a.recv_end = a.committed;
        a.spool = Some(holder.spool_read(a.committed).await.map_err(holder_err)?);
        if info.panes.iter().any(|p| p.no == MAIN && p.alive) {
            d.view(&holder, s.id).await?;
        } else {
            a.boot_acts = a.machine.step(Ev::PaneExited);
        }
    } else {
        let from = committed.min(hi);
        a.committed = from;
        a.lines = SpoolLines::new(from);
        a.recv_end = from;
        let path = d.sessions.path(&d.cfg);
        let launch = Launch {
            cwd: s.cwd.clone(),
            model: s.model.clone(),
            resume: s.agent_sid.clone(),
            extra_env: d.sessions.child_env(&d.cfg, &path),
            hooks_path: None,
            effort: None,
        };
        let attach = Attach {
            resume_offset: from,
            agent_sid: s.agent_sid.clone(),
            model: s.model.clone(),
            answers: saved.answers.clone(),
            epoch: u32::try_from(a.epoch).unwrap_or(u32::MAX),
        };
        a.driver.attach(&launch, &attach, &mut a.out);
        d.write(|tx, _| tx.set_agent_state(s.id, a.epoch, &saved.json()))?;
        a.flush().await;
        a.spool = Some(holder.spool_read(from).await.map_err(holder_err)?);
        a.replay_until = Some(hi);
        a.live = alive;
        if !alive {
            a.dead_at_attach = Some(exit);
        }
        a.resend = saved.held;
    }
    a.launch(d, events);
    Ok(())
}

// ---- the actor ----------------------------------------------------------------------------------

struct Actor {
    /// The daemon, weakly: dropping it stops the actor (upgraded while handling one event).
    d: Weak<Daemon>,
    sid: Sid,
    agent: Agent,
    holder: Holder,
    driver: Box<dyn Driver>,
    out: Out,
    machine: Takeover,
    t0: Instant,
    /// An agent process runs: spawned or found, its exit not seen yet.
    live: bool,
    /// Nothing of this process has been read yet: its first line makes a `Starting` session ready.
    starting: bool,
    spool: Option<mpsc::Receiver<(u64, Bytes)>>,
    lines: SpoolLines,
    /// End of the spool bytes received so far.
    recv_end: u64,
    /// The spool offset the transcript is stored up to (the store's `spool_committed`).
    committed: u64,
    epoch: u64,
    /// What `agent_state` holds.
    saved: Saved,
    /// Replaying the spool of an agent that outlived the daemon, up to where it stood at attach.
    replay_until: Option<u64>,
    /// The agent was already dead at attach: its exit is handled when the replay is through.
    dead_at_attach: Option<i32>,
    /// Takeover steps to run first (the TUI ended while the daemon was away).
    boot_acts: Vec<Act>,
    fold: FoldState,
    /// Writes a commit could not store, tried again first by the next one: the offset they
    /// complete is not committed before they are.
    // ponytail: unbounded while the store keeps failing, each batch is only a few KiB; cap it (and
    // fail the session) if a store that stays broken is ever seen.
    pending_puts: Vec<Put>,
    /// What the agent said about the saved messages, for the next commit ([`sent::settle`]).
    missed: Vec<sent::Gone>,
    /// Prompts for the next process: the ones the last one dropped, the ones saved before a restart.
    resend: Vec<HeldPrompt>,
    restart_at: Option<Instant>,
    restarted_at: Option<Instant>,
    tick_at: Option<Instant>,
    /// The agent's stderr tail from its last exit.
    stderr: String,
    takeover_reply: Option<Reply>,
    /// The agent died twice or would not start: nothing runs until the session is closed.
    failed: bool,
    /// The agent was stopped to start again on another provider's model.
    switching: bool,
    stop: bool,
    /// Claude's `/goal` is on (set, or judged not met yet): the transcript on disk is followed for
    /// the checker's verdicts, which the stream never carries (see [`GoalTail`]).
    goal: Option<GoalTail>,
    /// Claude called `Monitor`: the transcript on disk is followed for the events that start a
    /// turn of their own, which the stream does not replay (it replays those taken mid-turn).
    // ponytail: followed from then on, not until the Monitor ends; close it on the
    // Monitor's task end if the read per chunk ever shows
    monitor: Option<GoalTail>,
    /// Claude's `PushNotification` calls sent to the phone, by call id: the outcome.
    pushes: HashMap<String, String>,
}

/// The transcript on disk of a session with a `/goal` (or a `Monitor`), read from where the goal
/// was set (the Monitor called).
struct GoalTail {
    path: Option<PathBuf>,
    read: u64,
}

/// Why a batch of events is being applied.
#[derive(Clone, Copy)]
enum Src {
    /// The agent's stdout line at this spool offset.
    Line(u64),
    /// The process exited ([`Driver::on_exit`]).
    Exit,
}

impl Actor {
    fn new(
        d: &Daemon,
        s: &Session,
        holder: Holder,
        committed: u64,
        epoch: u64,
        saved: Saved,
    ) -> Actor {
        Actor {
            d: d.me.clone(),
            sid: s.id,
            agent: s.agent,
            holder,
            driver: driver_for(s.agent),
            out: Out::default(),
            machine: Takeover::new(s.mode, EXIT_CMD),
            t0: Instant::now(),
            live: false,
            starting: false,
            spool: None,
            lines: SpoolLines::new(committed),
            recv_end: committed,
            committed,
            epoch,
            saved,
            replay_until: None,
            dead_at_attach: None,
            boot_acts: Vec::new(),
            fold: FoldState::default(),
            pending_puts: Vec::new(),
            missed: Vec::new(),
            resend: Vec::new(),
            restart_at: None,
            restarted_at: None,
            tick_at: None,
            stderr: String::new(),
            takeover_reply: None,
            failed: false,
            switching: false,
            stop: false,
            goal: None,
            monitor: None,
            pushes: HashMap::new(),
        }
    }

    /// Registers the actor's handle and runs it.
    fn launch(self, d: &Daemon, events: broadcast::Receiver<HolderEvent>) {
        let (tx, rx) = mpsc::channel(32);
        d.sessions.set_head(self.sid, Head { tx });
        tokio::spawn(self.run(rx, events));
    }

    fn ms(&self) -> u64 {
        u64::try_from(self.t0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    async fn run(
        mut self,
        mut rx: mpsc::Receiver<Cmd>,
        mut events: broadcast::Receiver<HolderEvent>,
    ) {
        if let Some(d) = self.d.upgrade() {
            self.begin(&d).await;
        }
        while !self.stop {
            let wake = [self.restart_at, self.tick_at].into_iter().flatten().min();
            tokio::select! {
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    let Some(d) = self.d.upgrade() else { break };
                    self.on_cmd(&d, cmd).await;
                }
                chunk = next_chunk(&mut self.spool) => match chunk {
                    Some((offset, data)) => {
                        let Some(d) = self.d.upgrade() else { break };
                        self.on_chunk(&d, offset, data).await;
                    }
                    None => {
                        // ponytail: a stream that ends alone (holder error, bad frame) is not
                        // reopened; read again from `recv_end` if that is ever seen.
                        tracing::warn!(sid = %self.sid, "agent spool stream ended");
                        self.spool = None;
                    }
                },
                ev = events.recv() => {
                    let Some(d) = self.d.upgrade() else { break };
                    self.on_holder(&d, ev).await;
                }
                () = self.holder.closed() => {
                    if let Some(d) = self.d.upgrade() {
                        self.holder_gone(&d);
                    }
                    break;
                }
                () = sleep_until_opt(wake) => {
                    let Some(d) = self.d.upgrade() else { break };
                    self.on_timer(&d).await;
                }
            }
        }
    }

    /// What is left to do after attaching: a replay with nothing in it, a TUI that ended.
    async fn begin(&mut self, d: &Daemon) {
        self.replay_check(d).await;
        let acts = std::mem::take(&mut self.boot_acts);
        if !acts.is_empty() {
            self.run_acts(d, acts).await;
        }
    }

    // ---- commands -------------------------------------------------------------------------------

    async fn on_cmd(&mut self, d: &Daemon, cmd: Cmd) {
        match cmd {
            Cmd::Prompt {
                text,
                how,
                id,
                reply,
            } => {
                let r = self.prompt(d, &text, how, id.as_deref()).await;
                let _ = reply.send(r);
            }
            Cmd::Interrupt { reply } => {
                let r = self.interrupt().await;
                let _ = reply.send(r);
            }
            Cmd::Background { tool_use_id, reply } => {
                let r = self.background(tool_use_id.as_deref()).await;
                let _ = reply.send(r);
            }
            Cmd::Answer {
                request_id,
                answer,
                reply,
            } => {
                let r = self.answer(d, &request_id, &answer).await;
                let _ = reply.send(r);
            }
            Cmd::SetModel { model, reply } => {
                let r = self.set_model(d, &model).await;
                let _ = reply.send(r);
            }
            Cmd::SetPermission { mode, reply } => {
                let r = self.set_permission(&mode).await;
                let _ = reply.send(r);
            }
            Cmd::Takeover { to, reply } => self.takeover(d, to, reply).await,
            Cmd::Close { done } => {
                let _ = self.holder.shutdown().await;
                self.stop = true;
                let _ = done.send(());
            }
            Cmd::Hook(ev) => {
                let acts = match ev {
                    HookEvent::Working => self.machine.step(Ev::TurnStarted),
                    HookEvent::Idle => self.machine.step(Ev::TurnEnded { now: self.ms() }),
                    _ => return,
                };
                self.run_acts(d, acts).await;
            }
            Cmd::Streams { reply } => {
                let _ = reply.send(self.fold.streams.clone());
            }
        }
    }

    /// The driver takes calls: headless, not replaying, not failed, no takeover under way.
    fn ready(&self) -> Result<(), CoreError> {
        if self.failed || self.replay_until.is_some() || self.machine.phase != Phase::Headless {
            return Err(CoreError::Busy);
        }
        Ok(())
    }

    /// `how` goes to the driver as is: a Steer joins the running turn at its next tool boundary,
    /// InterruptThenSend stops it first, Queue waits for it to end.
    async fn prompt(
        &mut self,
        d: &Daemon,
        text: &str,
        how: PromptHow,
        id: Option<&str>,
    ) -> Result<(), CoreError> {
        self.ready()?;
        let row = d.session(self.sid)?;
        // `/model deepseek` and back: the CLI cannot change provider, the daemon restarts it
        if let Some(m) = model_switch(text) {
            if is_gateway(row.model.as_deref()) != is_gateway(Some(&m)) {
                return self.set_model(d, &m).await;
            }
        }
        let takes = matches!(
            row.status,
            SessionStatus::Starting
                | SessionStatus::Idle
                | SessionStatus::Working
                | SessionStatus::Paused
        );
        if !takes || row.attention == Attention::Blocked {
            return Err(CoreError::Busy);
        }
        match id {
            Some(id) => self.driver.prompt_with_id(id, text, how, &mut self.out),
            None => self.driver.prompt(text, how, &mut self.out),
        }
        .map_err(driver_err)?;
        self.flush().await;
        let effort = effort_switch(text);
        let goal = goal_switch(text);
        if goal.as_ref().is_some_and(|g| g.state == GoalState::Active) {
            self.open_goal_tail(d);
        }
        // The agent's own turn starts a moment later: say `working` first, so a reader never takes
        // the last task as finished in between.
        self.update(d, None, |s| {
            s.status = SessionStatus::Working;
            s.attention = Attention::None;
            if effort.is_some() {
                s.effort = effort;
            }
            match goal {
                Some(g) => s.goal = Some(g),
                // an achieved goal stays on show until the next prompt
                None if s.goal.as_ref().is_some_and(|g| g.state == GoalState::Met) => {
                    s.goal = None;
                }
                None => {}
            }
        });
        self.commit(d, Vec::new()).await;
        Ok(())
    }

    /// Starts following the transcript on disk for the goal's verdicts, from its end now (the
    /// sentinel of the goal being set comes after). No file yet (a session's first prompt): it is
    /// looked up again on the next chunk.
    fn open_goal_tail(&mut self, d: &Daemon) {
        self.goal = Some(self.tail_from_now(d));
    }

    /// The transcript on disk, to be read from its end now.
    fn tail_from_now(&self, d: &Daemon) -> GoalTail {
        let path = d
            .session(self.sid)
            .ok()
            .and_then(|s| s.agent_sid)
            .and_then(|uuid| herdr::find(&d.cfg.home, &uuid));
        let read = path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len());
        GoalTail { path, read }
    }

    /// The goal verdicts the transcript on disk gained since the last look: the newest goes to the
    /// row; `Met` or `Cleared` ends the following.
    async fn tail_goal(&mut self, d: &Daemon) {
        let Some(t) = &mut self.goal else {
            return;
        };
        let Some(data) = tail_read(t, d, self.sid).await else {
            return;
        };
        let last = data.split(|b| *b == b'\n').rev().find_map(goal_line);
        if let Some(g) = last {
            if matches!(g.state, GoalState::Met | GoalState::Cleared) {
                self.goal = None;
            }
            self.update(d, None, |s| s.goal = Some(g));
        }
    }

    /// The Monitor events the transcript on disk gained since the last look, each as its row. The
    /// row's id is the line's own, so a line read twice is one row.
    async fn tail_monitor(&mut self, d: &Daemon) {
        let Some(t) = &mut self.monitor else {
            return;
        };
        let Some(data) = tail_read(t, d, self.sid).await else {
            return;
        };
        for (id, text) in data.split(|b| *b == b'\n').filter_map(monitor_line) {
            let item = event_item(self.sid, id, &text, now_ms());
            if let Err(e) =
                d.write_items(|tx, _, published| put(tx, self.sid, &Put::Done(item), published))
            {
                tracing::warn!(sid = %self.sid, %e, "monitor event not stored");
            }
        }
    }

    async fn interrupt(&mut self) -> Result<(), CoreError> {
        self.ready()?;
        self.driver.interrupt(&mut self.out).map_err(driver_err)?;
        self.flush().await;
        Ok(())
    }

    async fn background(&mut self, tool_use_id: Option<&str>) -> Result<(), CoreError> {
        self.ready()?;
        self.driver
            .background(tool_use_id, &mut self.out)
            .map_err(driver_err)?;
        self.flush().await;
        Ok(())
    }

    async fn answer(&mut self, d: &Daemon, request_id: &str, a: &Answer) -> Result<(), CoreError> {
        self.ready()?;
        self.driver
            .answer(request_id, a, &mut self.out)
            .map_err(driver_err)?;
        // One part of a question with several: the driver keeps the answer until the others are
        // in and writes nothing, and the CLI says nothing either. Its item is `Answered` now, so
        // the next `/input` reaches the next part.
        let part = if self.out.stdin.is_empty() {
            d.answered_part(self.sid, request_id, a)
        } else {
            None
        };
        self.flush().await;
        // the answers and the resume offset go together (the driver's contract)
        self.commit(d, part.into_iter().collect()).await;
        Ok(())
    }

    async fn set_model(&mut self, d: &Daemon, model: &str) -> Result<(), CoreError> {
        self.ready()?;
        let now = d.session(self.sid)?.model;
        if is_gateway(now.as_deref()) != is_gateway(Some(model)) {
            // Another provider is another process: this one ends between turns, and the next
            // continues the conversation on the new model (`agent_exited`).
            if self.driver.status() != AgentStatus::Idle {
                return Err(CoreError::Busy);
            }
            self.update(d, None, |s| s.model = Some(model.to_owned()));
            self.switching = true;
            return self.holder.agent_close_stdin().await.map_err(holder_err);
        }
        self.driver
            .set_model(model, &mut self.out)
            .map_err(driver_err)?;
        self.flush().await;
        Ok(())
    }

    async fn set_permission(&mut self, mode: &str) -> Result<(), CoreError> {
        self.ready()?;
        self.driver
            .set_permission_mode(mode, &mut self.out)
            .map_err(driver_err)?;
        self.flush().await;
        Ok(())
    }

    async fn takeover(&mut self, d: &Daemon, to: Mode, reply: Reply) {
        if self.takeover_reply.is_some()
            || self.machine.in_transition()
            || self.replay_until.is_some()
            || self.failed
        {
            let _ = reply.send(Err(CoreError::Busy));
            return;
        }
        // Closing the agent's stdin ends its turn: the user interrupts it or waits, as the way back
        // waits for a turn in the terminal. A prompt written and not started yet counts (the driver
        // is `Working`), the machine only knows turns the agent began.
        if to == Mode::Tui
            && self.machine.phase == Phase::Headless
            && (self.machine.turn_active || self.driver.status() != AgentStatus::Idle)
        {
            tracing::info!(
                sid = %self.sid,
                "takeover refused: a turn is running, interrupt it or wait"
            );
            let _ = reply.send(Err(CoreError::Busy));
            return;
        }
        let mut acts = self.machine.step(Ev::Takeover { to, now: self.ms() });
        if !self.live && matches!(self.machine.phase, Phase::StoppingAgent { .. }) {
            // no process to stop (it died and its restart is pending): its exit is behind us
            self.restart_at = None;
            acts = self.machine.step(Ev::AgentExited);
        }
        if acts == [Act::Noop] {
            let _ = reply.send(Ok(()));
            return;
        }
        self.takeover_reply = Some(reply);
        self.run_acts(d, acts).await;
    }

    // ---- the holder -----------------------------------------------------------------------------

    async fn on_holder(
        &mut self,
        d: &Daemon,
        ev: Result<HolderEvent, broadcast::error::RecvError>,
    ) {
        use broadcast::error::RecvError;
        match ev {
            Ok(HolderEvent::AgentStderr(tail)) => self.stderr = tail,
            Ok(HolderEvent::ChildExited {
                target: Target::Agent,
                status,
            }) => self.agent_exited(d, status).await,
            Ok(HolderEvent::ChildExited {
                target: Target::Pane(MAIN),
                ..
            }) => {
                let acts = self.machine.step(Ev::PaneExited);
                self.run_acts(d, acts).await;
            }
            // ponytail: a TUI's title is not followed (the PTY path shows it), add when a client wants it
            Ok(_) => {}
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(sid = %self.sid, n, "holder events were missed");
                self.catch_up(d).await;
            }
            Err(RecvError::Closed) => {
                self.holder_gone(d);
            }
        }
    }

    /// Events were missed: what the holder says now stands in for an exit among them, which
    /// nothing else would announce again (a takeover would wait for it for good).
    async fn catch_up(&mut self, d: &Daemon) {
        let Ok(info) = self.holder.info().await else {
            return;
        };
        match self.machine.phase {
            Phase::Headless | Phase::StoppingAgent { .. } => {
                if let Some(a) = info.agent.filter(|a| self.live && !a.alive) {
                    self.agent_exited(d, a.exit.unwrap_or(-1)).await;
                }
            }
            Phase::Tui | Phase::WaitingStop | Phase::StoppingTui { .. } => {
                if !self.failed && !info.panes.iter().any(|p| p.no == MAIN && p.alive) {
                    let acts = self.machine.step(Ev::PaneExited);
                    self.run_acts(d, acts).await;
                }
            }
        }
    }

    /// The holder itself is gone: nothing can run any more.
    fn holder_gone(&mut self, d: &Daemon) {
        self.stop = true;
        d.sessions.unhead(self.sid);
        d.sessions.forget(self.sid);
        let r = d.write(|tx, _| {
            tx.update_session(self.sid, |s| {
                if s.closed.is_none() {
                    s.status = SessionStatus::Crashed;
                    s.attention = Attention::None;
                }
            })
        });
        match r {
            Ok(s) if s.closed.is_none() => {
                tracing::warn!(sid = %self.sid, "holder gone; session crashed");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(sid = %self.sid, %e, "holder gone; session not marked"),
        }
    }

    async fn on_timer(&mut self, d: &Daemon) {
        let now = Instant::now();
        if self.restart_at.is_some_and(|t| t <= now) {
            self.restart_at = None;
            self.restart(d).await;
        }
        if self.tick_at.is_some_and(|t| t <= now) {
            self.tick_at = Some(now + TICK);
            let acts = self.machine.step(Ev::Tick { now: self.ms() });
            self.run_acts(d, acts).await;
        }
    }

    // ---- the spool ------------------------------------------------------------------------------

    async fn on_chunk(&mut self, d: &Daemon, offset: u64, data: Bytes) {
        // first: an event that starts a turn is in the file before the turn's first line here
        self.tail_monitor(d).await;
        self.ingest(d, offset, data).await;
        self.replay_check(d).await;
        self.tail_goal(d).await;
    }

    /// Feeds the driver the complete lines of a chunk.
    async fn ingest(&mut self, d: &Daemon, offset: u64, data: Bytes) {
        self.lines.push(offset, &data);
        self.recv_end = self.recv_end.max(offset + data.len() as u64);
        while let Some((at, line)) = self.lines.next_line() {
            let events = self.driver.on_line(at, &line, &mut self.out);
            self.flush().await;
            self.apply(d, Src::Line(at), events).await;
        }
    }

    async fn replay_check(&mut self, d: &Daemon) {
        if self.replay_until.is_some_and(|end| self.recv_end >= end) {
            self.end_replay(d).await;
        }
    }

    /// Every driver call is followed by this: what it wants written goes to the agent.
    async fn flush(&mut self) {
        if self.out.stdin.is_empty() {
            return;
        }
        let bytes = self.out.take();
        if let Err(e) = self.holder.agent_write(bytes).await {
            // the exit event follows and says what happened
            tracing::debug!(sid = %self.sid, %e, "agent stdin not written");
        }
    }

    /// The replay is through: the driver knows where the agent stands. Catch the clients up and
    /// the session row, and give the agent the prompts that were waiting.
    async fn end_replay(&mut self, d: &Daemon) {
        self.replay_until = None;
        self.fold.done = false;
        for (id, text) in &self.fold.streams {
            if !text.is_empty() {
                d.hub.publish_delta(self.sid, id.clone(), 0, text.clone());
            }
        }
        if let Some(code) = self.dead_at_attach.take() {
            self.live = true;
            self.agent_exited(d, code).await;
            return;
        }
        let asked = match self.driver.status() {
            AgentStatus::Blocked => d
                .pending_question(self.sid)
                .ok()
                .flatten()
                .map(|q| q.prompt),
            _ => None,
        };
        let ds = self.driver.status();
        self.update(d, asked.as_deref(), |s| match ds {
            AgentStatus::Working => {
                s.status = SessionStatus::Working;
                s.attention = Attention::None;
            }
            AgentStatus::Blocked => {
                s.status = SessionStatus::Blocked;
                s.attention = Attention::Blocked;
            }
            AgentStatus::Idle => {
                // a turn that ended while the daemon was away is `done` until someone looks
                let done = s.status == SessionStatus::Working || s.attention == Attention::Done;
                s.status = SessionStatus::Idle;
                s.attention = if done {
                    Attention::Done
                } else {
                    Attention::None
                };
            }
        });
        self.send_held(d).await;
        self.commit(d, Vec::new()).await;
    }

    /// Hands the agent the prompts that are waiting for a process (saved before a restart, dropped
    /// by the last process).
    async fn send_held(&mut self, d: &Daemon) {
        if !self.live || self.resend.is_empty() {
            return;
        }
        for h in std::mem::take(&mut self.resend) {
            let r = match &h.id {
                Some(id) => self
                    .driver
                    .prompt_with_id(id, &h.text, h.how, &mut self.out),
                None => self.driver.prompt(&h.text, h.how, &mut self.out),
            };
            if let Err(e) = r {
                tracing::warn!(sid = %self.sid, %e, "a held prompt was not sent again");
            }
        }
        self.flush().await;
        self.update(d, None, |s| {
            if matches!(s.status, SessionStatus::Idle | SessionStatus::Starting) {
                s.status = SessionStatus::Working;
                s.attention = Attention::None;
            }
        });
        self.commit(d, Vec::new()).await;
    }

    // ---- what the driver reports ----------------------------------------------------------------

    /// Stores and publishes what the driver reported. The session row goes first when the agent is
    /// starting work or asking (a client sees `working` before the first item of the turn), last
    /// when it is done (a client that hears `done` finds the whole answer).
    async fn apply(&mut self, d: &Daemon, src: Src, mut events: Vec<AgentEvent>) {
        let replay = self.replay_until.is_some();
        let live_line = !replay && matches!(src, Src::Line(_));
        if live_line {
            let title = || {
                d.session(self.sid)
                    .map_or_else(|_| String::new(), |s| push_title(&s))
            };
            let (n, done) = (&d.notifier, &mut self.pushes);
            forward_pushes(n, self.sid, title, &events, done, false).await;
        }
        stamp_pushes(&mut events, &self.pushes);
        if live_line && self.monitor.is_none() && events.iter().any(calls_monitor) {
            self.monitor = Some(self.tail_from_now(d));
        }
        let base = match src {
            Src::Line(at) => format!("d{at}"),
            Src::Exit => format!("x{}", Id::new()),
        };
        let f = fold(&mut self.fold, self.sid, &base, events, now_ms());
        if !replay {
            for (id, offset, text) in f.deltas {
                d.hub.publish_delta(self.sid, id, offset, text);
            }
        }
        // a replayed init too: after a daemon restart that is the only one there is
        if let Some(c) = &f.commands {
            save_commands(d, c);
        }
        if live_line {
            if let Some(u) = f.usage {
                *d.usage.lock().unwrap_or_else(|e| e.into_inner()) = Some(u);
            }
        }
        if !replay {
            self.resend.extend(f.dropped);
            if !f.missed.is_empty() {
                self.missed.push(sent::Gone::Prompts(f.missed));
            }
            // idle with a saved message still waiting and nothing holding it: not got
            if live_line && matches!(f.state, Some((SessionStatus::Idle, _))) {
                let held = self
                    .driver
                    .held_prompts()
                    .into_iter()
                    .chain(self.resend.iter().cloned());
                self.missed.push(sent::Gone::Idle(held_of(held.collect())));
            }
        }
        if f.turn_started {
            let _ = self.machine.step(Ev::TurnStarted);
        }
        if f.turn_ended {
            let _ = self.machine.step(Ev::TurnEnded { now: self.ms() });
        }
        let row = RowFx {
            agent_sid: f.agent_sid,
            model: f.model,
            state: if live_line { f.state } else { None },
            ready: live_line && std::mem::take(&mut self.starting),
            driver: self.driver.status(),
            asked: f.asked.filter(|_| live_line),
            permission: f.permission,
            // what the agent last said is the same row on a replay; a compaction is counted once,
            // when its line is live
            ctx_used: f.ctx_used,
            ctx_window: f.ctx_window,
            ctx_compact_at: f.ctx_compact_at,
            compacted: if live_line { f.compacted } else { 0 },
            goal: f.goal,
        };
        let first = row_first(row.state);
        if first {
            self.row(d, &row);
        }
        self.commit(d, f.puts).await;
        if !first {
            self.row(d, &row);
        }
    }

    fn row(&self, d: &Daemon, fx: &RowFx) {
        if fx.agent_sid.is_none()
            && fx.model.is_none()
            && fx.state.is_none()
            && fx.permission.is_none()
            && fx.ctx_used.is_none()
            && fx.ctx_window.is_none()
            && fx.ctx_compact_at.is_none()
            && fx.compacted == 0
            && fx.goal.is_none()
            && !fx.ready
        {
            return;
        }
        self.update(d, fx.asked.as_deref(), |s| {
            if let Some(p) = &fx.permission {
                s.permission = Some(p.clone());
            }
            if let Some(a) = &fx.agent_sid {
                // a new conversation (`/clear`) has no goal
                if s.agent_sid.as_ref().is_some_and(|old| old != a) {
                    s.goal = None;
                }
                s.agent_sid = Some(a.clone());
            }
            if let Some(g) = &fx.goal {
                s.goal = Some(g.clone());
            }
            if let Some(m) = &fx.model {
                s.model = Some(m.clone());
            }
            if fx.ready && s.status == SessionStatus::Starting {
                s.status = match fx.driver {
                    AgentStatus::Working => SessionStatus::Working,
                    AgentStatus::Blocked => SessionStatus::Blocked,
                    AgentStatus::Idle => SessionStatus::Idle,
                };
            }
            if let Some((status, attention)) = fx.state {
                s.status = status;
                s.attention = attention;
            }
            s.context = with_context(
                s.context,
                fx.ctx_used,
                fx.ctx_window,
                fx.ctx_compact_at,
                fx.compacted,
            );
        });
    }

    /// Updates the session row, and tells the notifier when its attention changed (`asked` is what
    /// a blocked agent asks, for the push).
    fn update(&self, d: &Daemon, asked: Option<&str>, f: impl FnOnce(&mut Session)) {
        let mut before = Attention::None;
        match d.update(self.sid, |s| {
            before = s.attention;
            f(s);
        }) {
            Ok(after) if after.attention != before => {
                let who = match asked {
                    Some(q) => format!("{}: {q}", label(&after)),
                    None => label(&after),
                };
                d.notifier.attention(self.sid, &who, after.attention);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(sid = %self.sid, %e, "session row not updated"),
        }
    }

    /// One transaction: the items, the spool offset they complete, and the attach state when it
    /// changed. Then the holder hears the offset. A transaction that fails leaves its items in
    /// `pending_puts` for the next one: the offset only moves with the items it covers.
    async fn commit(&mut self, d: &Daemon, puts: Vec<Put>) {
        let puts: Vec<Put> = std::mem::take(&mut self.pending_puts)
            .into_iter()
            .chain(puts)
            .collect();
        let committed = self.committed.max(self.driver.resume_offset());
        let saved = Saved {
            answers: self.driver.pending_answers(),
            held: self
                .driver
                .held_prompts()
                .into_iter()
                .chain(self.resend.iter().cloned())
                .collect(),
        };
        // not while replaying: the driver has not seen the questions it holds answers for yet
        let state = (self.replay_until.is_none() && saved != self.saved).then(|| saved.json());
        let before = self.committed;
        let missed = std::mem::take(&mut self.missed);
        if puts.is_empty() && committed == before && state.is_none() && missed.is_empty() {
            return;
        }
        let (sid, epoch) = (self.sid, self.epoch);
        let removed = removes(d, &puts);
        let r = d.write_items(|tx, _, published| {
            for p in &puts {
                put(tx, sid, p, published)?;
            }
            for g in &missed {
                sent::settle(tx, sid, g, published)?;
            }
            if committed != before {
                tx.set_spool_committed(sid, committed)?;
            }
            if let Some(json) = &state {
                tx.set_agent_state(sid, epoch, json)?;
            }
            Ok(())
        });
        if let Some(before) = removed {
            d.say_removed(sid, before);
        }
        match r {
            Ok(()) => {
                self.committed = committed;
                self.saved = saved;
                // ponytail: at-least-once, a daemon that dies between writing a held prompt to the
                // agent and saving this state sends that prompt again after its restart
                if committed != before {
                    if let Err(e) = self.holder.spool_commit(committed).await {
                        tracing::debug!(sid = %self.sid, %e, "spool commit not sent");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(sid = %self.sid, %e, "transcript not stored, kept for later");
                self.pending_puts = puts;
                self.missed = missed;
            }
        }
    }

    /// An `Event` item of the daemon's own (a restart, a failure).
    fn event(&self, d: &Daemon, text: &str) {
        let item = event_item(self.sid, format!("x{}", Id::new()), text, now_ms());
        if let Err(e) =
            d.write_items(|tx, _, published| put(tx, self.sid, &Put::Done(item), published))
        {
            tracing::warn!(sid = %self.sid, %e, "event not stored");
        }
    }

    // ---- processes ------------------------------------------------------------------------------

    /// A fresh driver and a new agent process (`resume`: the agent's session to continue).
    async fn spawn_process(&mut self, d: &Daemon, resume: Option<String>) -> Result<(), String> {
        let s = d.session(self.sid).map_err(|e| e.to_string())?;
        let path = d.sessions.path(&d.cfg);
        let mut extra_env = d.sessions.child_env(&d.cfg, &path);
        extra_env.extend(gateway_env(&d.cfg.home, s.model.as_deref())?);
        extra_env.extend(artifact_env(self.agent, s.model.as_deref()));
        // ponytail: the desktop's chats think at medium; /effort changes it until the next start
        let effort = (s.owner.starts_with("device:") && !is_gateway(s.model.as_deref()))
            .then(|| "medium".to_owned());
        self.update(d, None, |s| s.effort = effort.clone());
        let launch = Launch {
            cwd: s.cwd,
            model: s.model,
            resume,
            extra_env,
            // the stream is the status here; the hooks are for the TUI
            hooks_path: None,
            effort,
        };
        self.driver = driver_for(self.agent);
        let mut sp = self.driver.spawn(&launch);
        let prog = sp.cmd.first().cloned().ok_or("empty command")?;
        sp.cmd[0] =
            find_on_path(&prog, &path).ok_or_else(|| format!("{prog}: not found on PATH"))?;
        self.holder
            .spawn_agent(sp.cmd, sp.cwd, sp.env)
            .await
            .map_err(|e| e.to_string())?;
        self.driver.on_start(&launch, &mut self.out);
        self.flush().await;
        self.live = true;
        self.starting = true;
        self.stderr.clear();
        Ok(())
    }

    /// The agent's TUI in pane 1, on the same conversation.
    async fn spawn_tui(&mut self, d: &Daemon) -> Result<(), String> {
        let s = d.session(self.sid).map_err(|e| e.to_string())?;
        let path = d.sessions.path(&d.cfg);
        let mut extra_env = d.sessions.child_env(&d.cfg, &path);
        extra_env.extend(gateway_env(&d.cfg.home, s.model.as_deref())?);
        let launch = Launch {
            cwd: s.cwd,
            model: s.model,
            resume: s.agent_sid,
            extra_env,
            hooks_path: d.sessions.hooks_path(self.agent),
            effort: None,
        };
        let mut sp = self
            .driver
            .tui(&launch)
            .ok_or_else(|| format!("{} has no TUI", agent_label(self.agent)))?;
        let prog = sp.cmd.first().cloned().ok_or("empty command")?;
        sp.cmd[0] =
            find_on_path(&prog, &path).ok_or_else(|| format!("{prog}: not found on PATH"))?;
        self.holder
            .spawn_pty(MAIN, sp.cmd, sp.cwd, sp.env, COLS, ROWS)
            .await
            .map_err(|e| e.to_string())?;
        d.view(&self.holder, self.sid)
            .await
            .map_err(|e| e.to_string())
    }

    /// The agent exited. Its last output is read first, then the driver closes what was open; the
    /// takeover machine says whether we asked for it, and if not the restart policy decides.
    async fn agent_exited(&mut self, d: &Daemon, code: i32) {
        if !self.live {
            return;
        }
        self.live = false;
        // The holder announces the exit once the agent's pipes are empty, but our spool stream is
        // another connection: take what it still carries.
        if let Some(hi) = self
            .holder
            .info()
            .await
            .ok()
            .and_then(|i| i.agent)
            .map(|a| a.spool_hi)
        {
            self.drain(d, hi).await;
        }
        // ponytail: a line the agent died in the middle of is lost (the driver takes whole lines
        // only); the turn it belonged to ends Interrupted through `on_exit`.
        self.lines = SpoolLines::new(self.recv_end);
        self.replay_until = None;
        let events = self.driver.on_exit(Some(code));
        self.apply(d, Src::Exit, events).await;
        self.fold.streams.clear();
        let acts = self.machine.step(Ev::AgentExited);
        if !acts.is_empty() {
            self.run_acts(d, acts).await;
            return;
        }
        if std::mem::take(&mut self.switching) {
            let model = d.session(self.sid).ok().and_then(|s| s.model);
            self.event(
                d,
                &format!("Model: {}", model.as_deref().unwrap_or("default")),
            );
            self.restart(d).await;
            // a switch is not a crash: the next exit may restart it again
            self.restarted_at = None;
            return;
        }
        let tail = tail_of(&std::mem::take(&mut self.stderr), STDERR_MAX);
        let what = format!("{} exited ({})", agent_label(self.agent), status_text(code));
        let note = if tail.trim().is_empty() {
            String::new()
        } else {
            format!("\n{}", tail.trim_end())
        };
        if self.restarted_at.is_some_and(|t| t.elapsed() < FAIL_WINDOW) {
            let text = format!("{what} again right after a restart: not started again.{note}");
            self.fail(d, &text).await;
        } else {
            self.event(d, &format!("{what}; starting it again in 2 s.{note}"));
            self.update(d, None, |s| {
                s.status = SessionStatus::Starting;
                s.attention = Attention::None;
            });
            self.restart_at = Some(Instant::now() + RESTART_DELAY);
        }
    }

    /// Reads the spool up to `hi`, which an exited agent had written.
    async fn drain(&mut self, d: &Daemon, hi: u64) {
        let deadline = Instant::now() + DRAIN;
        while self.recv_end < hi {
            let Some(rx) = self.spool.as_mut() else { break };
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some((offset, data))) => self.ingest(d, offset, data).await,
                _ => break,
            }
        }
    }

    /// The restart after an exit nobody asked for: the same conversation, the prompts that were
    /// dropped sent again.
    async fn restart(&mut self, d: &Daemon) {
        let resume = d.session(self.sid).ok().and_then(|s| s.agent_sid);
        self.restarted_at = Some(Instant::now());
        match self.spawn_process(d, resume).await {
            Ok(()) => self.send_held(d).await,
            Err(e) => {
                let text = format!("{} did not start again: {e}", agent_label(self.agent));
                self.fail(d, &text).await;
            }
        }
    }

    /// Gives up: the session is `Failed` and says why; closing it is the user's.
    async fn fail(&mut self, d: &Daemon, text: &str) {
        self.failed = true;
        self.restart_at = None;
        // nothing runs now: every saved message still waiting was not got
        self.resend.clear();
        self.missed.push(sent::Gone::All);
        self.commit(d, Vec::new()).await;
        self.event(d, text);
        self.update(d, None, |s| {
            s.status = SessionStatus::Failed;
            s.attention = Attention::None;
        });
        if let Some(r) = self.takeover_reply.take() {
            let _ = r.send(Err(CoreError::Internal(text.into())));
        }
    }

    // ---- takeover -------------------------------------------------------------------------------

    async fn run_acts(&mut self, d: &Daemon, acts: Vec<Act>) {
        // the row's mode first: a hook of the new side must find it
        if let Some(m) = acts.iter().find_map(|a| match a {
            Act::Mode(m) => Some(*m),
            _ => None,
        }) {
            self.update(d, None, |s| {
                s.mode = m;
                s.status = SessionStatus::Starting;
                s.attention = Attention::None;
            });
        }
        for act in acts {
            let critical = matches!(act, Act::SpawnTui | Act::SpawnHeadless { .. });
            let r: Result<(), String> = match act {
                Act::Mode(_) | Act::Noop => Ok(()),
                Act::AgentCloseStdin => self
                    .holder
                    .agent_close_stdin()
                    .await
                    .map_err(|e| e.to_string()),
                Act::SignalAgent(sig) => self
                    .holder
                    .signal(Target::Agent, sig)
                    .await
                    .map_err(|e| e.to_string()),
                Act::SignalPane(sig) => self
                    .holder
                    .signal(Target::Pane(MAIN), sig)
                    .await
                    .map_err(|e| e.to_string()),
                Act::SendPaneText(t) => d
                    .type_into(self.sid, t.trim_end_matches('\r'))
                    .await
                    .map_err(|e| e.to_string()),
                Act::SpawnTui => {
                    // The prompts that waited for a headless process each have an Event item that
                    // says they did not run, and the conversation moves to the terminal: none may
                    // come back as a turn nobody asked for after the next restart.
                    let held = std::mem::take(&mut self.resend);
                    self.missed.push(sent::Gone::Prompts(held_of(held)));
                    self.commit(d, Vec::new()).await;
                    self.spawn_tui(d).await
                }
                Act::ClosePane => {
                    let r = self.holder.close_pane(MAIN).await;
                    d.sessions.forget(self.sid);
                    r.map_err(|e| e.to_string())
                }
                // ponytail: M4 reads what happened in the TUI from the agent's own transcript
                Act::Backfill => Ok(()),
                Act::SpawnHeadless { resume } => {
                    let id = if resume {
                        d.session(self.sid).ok().and_then(|s| s.agent_sid)
                    } else {
                        None
                    };
                    self.spawn_process(d, id).await
                }
            };
            if let Err(e) = r {
                if critical {
                    let text = format!("{} did not start: {e}", agent_label(self.agent));
                    self.fail(d, &text).await;
                } else {
                    tracing::debug!(sid = %self.sid, %e, "takeover step failed");
                }
            }
        }
        self.settle();
    }

    /// A takeover that reached a stable phase is answered; one under way is clocked.
    fn settle(&mut self) {
        if self.machine.in_transition() {
            self.tick_at.get_or_insert_with(|| Instant::now() + TICK);
        } else {
            self.tick_at = None;
            if let Some(r) = self.takeover_reply.take() {
                let _ = r.send(Ok(()));
            }
        }
    }
}

async fn next_chunk(spool: &mut Option<mpsc::Receiver<(u64, Bytes)>>) -> Option<(u64, Bytes)> {
    match spool {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The session row goes out before the items of a batch when it says the agent is working or
/// asking, after them when it says idle: a client sees `working` before the turn's first item, and
/// never `done` before the turn's last.
fn row_first(state: Option<(SessionStatus, Attention)>) -> bool {
    matches!(
        state,
        Some((SessionStatus::Working | SessionStatus::Blocked, _))
    )
}

/// The row's context window after a batch: the agent's latest word on what it has read, on its
/// window and on where it compacts by itself (each kept when a batch does not repeat it), and the
/// compactions the batch adds. An agent that
/// never says leaves `None`; a window of 0 is "not said yet".
fn with_context(
    old: Option<Context>,
    used: Option<u64>,
    window: Option<u64>,
    compact_at: Option<u64>,
    compacted: u32,
) -> Option<Context> {
    if used.is_none() && window.is_none() && compact_at.is_none() && compacted == 0 {
        return old;
    }
    let mut c = old.unwrap_or_default();
    if let Some(u) = used {
        c.used = u;
    }
    if let Some(w) = window {
        c.window = w;
    }
    c.compact_at = compact_at.or(c.compact_at);
    c.compactions = c.compactions.saturating_add(compacted);
    Some(c)
}

fn status_text(code: i32) -> String {
    if code < 0 {
        format!("killed by signal {}", -code)
    } else {
        format!("status {code}")
    }
}

/// The last `max` bytes of `s`, from a character boundary.
fn tail_of(s: &str, max: usize) -> String {
    let mut from = s.len().saturating_sub(max);
    while !s.is_char_boundary(from) {
        from += 1;
    }
    s[from..].to_string()
}

fn cut(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Claude Code's Artifact tool (a page on claude.ai) is off in `claude -p` unless the environment
/// asks for it. A Claude chat gets it; the DeepSeek gateway route cannot use claude.ai, Codex and
/// Pi have no such tool. The box has no display, so the CLI never opens a browser itself.
pub(crate) fn artifact_env(agent: Agent, model: Option<&str>) -> Vec<(String, String)> {
    if agent != Agent::Claude || is_gateway(model) {
        return Vec::new();
    }
    vec![
        ("CLAUDE_CODE_ARTIFACT".to_owned(), "1".to_owned()),
        ("CLAUDE_CODE_ARTIFACT_AUTO_OPEN".to_owned(), "0".to_owned()),
    ]
}

/// A model of OpenGateway's (`deepseek/...`): another provider than Anthropic's.
pub(crate) fn is_gateway(model: Option<&str>) -> bool {
    model.is_some_and(|m| m.contains('/'))
}

/// DeepSeek through OpenGateway, the `+ deepseek` model.
pub(crate) const DEEPSEEK: &str = "deepseek/deepseek-v4.1-flash-ultrafast";

/// The model a `/model <name>` prompt asks for, when the daemon knows it: `deepseek`, an
/// OpenGateway id, Claude's aliases and ids.
pub(crate) fn model_switch(text: &str) -> Option<String> {
    let name = text.trim().strip_prefix("/model ")?.trim();
    Some(match name {
        "deepseek" => DEEPSEEK.to_owned(),
        "opus" => "claude-opus-5-5".to_owned(),
        "sonnet" => "claude-sonnet-5-5".to_owned(),
        "haiku" => "claude-haiku-5-5".to_owned(),
        "fable" => "claude-fable-5-2".to_owned(),
        m if m.contains('/') || m.starts_with("claude-") => m.to_owned(),
        _ => return None,
    })
}

/// The level an `/effort <level>` prompt sets.
fn effort_switch(text: &str) -> Option<String> {
    let level = text.trim().strip_prefix("/effort ")?.trim();
    (!level.is_empty() && !level.contains(char::is_whitespace)).then(|| level.to_lowercase())
}

/// What a `/goal` prompt does to the session's goal: `/goal <condition>` sets one (`Active`,
/// the checker's sentinel confirms it), `/goal clear` ends it; `/goal` alone only asks.
fn goal_switch(text: &str) -> Option<Goal> {
    let rest = text.trim().strip_prefix("/goal")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let condition = rest.trim();
    let state = match condition {
        "" => return None,
        "clear" => GoalState::Cleared,
        _ => GoalState::Active,
    };
    Some(Goal {
        condition: condition.to_owned(),
        state,
        reason: None,
    })
}

/// The whole lines `t`'s file gained since the last look, which are then read. `None` when there
/// are none, or no file yet (it is looked up again next time).
async fn tail_read(t: &mut GoalTail, d: &Daemon, sid: Sid) -> Option<Vec<u8>> {
    if t.path.is_none() {
        t.path = d
            .session(sid)
            .ok()
            .and_then(|s| s.agent_sid)
            .and_then(|uuid| herdr::find(&d.cfg.home, &uuid));
    }
    let path = t.path.clone()?;
    let at = t.read;
    let mut data = match tokio::task::spawn_blocking(move || herdr::read_from(&path, at)).await {
        Ok(Ok(Some(data))) => data,
        Ok(Ok(None)) => {
            t.read = 0; // rewritten: start over
            return None;
        }
        _ => return None,
    };
    let end = data.iter().rposition(|b| *b == b'\n')?;
    t.read += end as u64 + 1;
    data.truncate(end);
    Some(data)
}

/// Claude Code's tool for telling the user's phone something (2.1.322).
const PUSH_TOOL: &str = "PushNotification";
/// What Claude Code answers the tool when its own push went nowhere: it only delivers through
/// Remote Control ("Mobile push not sent (Remote Control inactive).").
const PUSH_NOT_SENT: &str = "Mobile push not sent";
/// The outcome of a push that arrived, as the call's body says it (`push`); any other is the reason.
const PUSH_SENT: &str = "sent";

/// The `PushNotification` call an event holds, once its input is final: its id and message.
fn push_call(e: &AgentEvent) -> Option<(&str, &str)> {
    let (id, body) = match e {
        AgentEvent::ItemUpdated { id, body } => (id, body),
        AgentEvent::ItemCompleted {
            id,
            kind: ItemKind::ToolCall,
            body,
            ..
        } => (id, body),
        _ => return None,
    };
    let message = body["input"]["message"].as_str()?;
    (body["name"] == PUSH_TOOL && !message.trim().is_empty()).then_some((id, message))
}

/// Whether `events` hold call `id`'s result, and it says Claude Code's own push went nowhere.
fn push_unsent(events: &[AgentEvent], id: &str) -> bool {
    events.iter().any(|e| {
        matches!(e, AgentEvent::ItemCompleted { kind: ItemKind::ToolResult, body, .. }
            if body["call_id"] == id
                && body["output"].as_str().is_some_and(|o| o.contains(PUSH_NOT_SENT)))
    })
}

/// What a session is called in Claude's push: its label or name, else its folder.
pub(crate) fn push_title(s: &Session) -> String {
    let folder = || {
        let dir = std::path::Path::new(&s.cwd).file_name()?;
        Some(dir.to_string_lossy().into_owned())
    };
    (s.label.clone())
        .or_else(|| s.name.clone())
        .or_else(folder)
        .unwrap_or_else(|| label(s))
}

/// Sends the phone what Claude's `PushNotification` calls in `events` say, once per call, and
/// keeps each outcome in `done` by call id. `needs_result`: only a call whose result is among the
/// events and says Claude Code's own push went nowhere (a terminal pane, where Claude Code may
/// have delivered it, or held it back because the user is at the terminal).
// ponytail: the caller waits for ntfy (10 s at worst) so the outcome is on the row when the call
// shows; spawn it and update the item later if a slow ntfy ever stalls a chat
pub(crate) async fn forward_pushes(
    n: &crate::notify::Notifier,
    sid: Sid,
    title: impl FnOnce() -> String,
    events: &[AgentEvent],
    done: &mut HashMap<String, String>,
    needs_result: bool,
) {
    let calls: Vec<(&str, &str)> = events
        .iter()
        .filter_map(push_call)
        .filter(|(id, _)| !done.contains_key(*id) && (!needs_result || push_unsent(events, id)))
        .collect();
    if calls.is_empty() {
        return;
    }
    let title = title();
    for (id, message) in calls {
        if done.contains_key(id) {
            continue; // twice among the events
        }
        let outcome = match n.claude_push(sid, &title, message).await {
            Ok(()) => PUSH_SENT,
            Err(why) => why,
        };
        done.insert(id.to_owned(), outcome.to_owned());
    }
}

/// Writes the outcome of each forwarded push on its call's body (`push`), for the clients' row.
// ponytail: `done` lives in memory, so a call replayed after a daemon restart loses its outcome
// and the row shows Claude Code's own answer again
pub(crate) fn stamp_pushes(events: &mut [AgentEvent], done: &HashMap<String, String>) {
    for e in events {
        let (id, body) = match e {
            AgentEvent::ItemUpdated { id, body } => (id, body),
            AgentEvent::ItemCompleted {
                id,
                kind: ItemKind::ToolCall,
                body,
                ..
            } => (id, body),
            _ => continue,
        };
        if let (Some(outcome), Some(body)) = (done.get(id.as_str()), body.as_object_mut()) {
            body.insert("push".into(), Value::String(outcome.clone()));
        }
    }
}

/// The agent opened a `Monitor` call (Claude names the tool in the item's preview).
fn calls_monitor(e: &AgentEvent) -> bool {
    matches!(e, AgentEvent::ItemStarted { kind: ItemKind::ToolCall, preview, .. } if preview == "Monitor")
}

/// A transcript line that is a Monitor event starting a turn of its own (a `user` line; one taken
/// mid-turn is a `queued_command` attachment there, and the stream replays it): its id and row.
fn monitor_line(line: &[u8]) -> Option<(String, String)> {
    let v: Value = serde_json::from_slice(line).ok()?;
    if v["type"] != "user" || v["origin"]["kind"] != "task-notification" {
        return None;
    }
    let text = v["message"]["content"].as_str()?;
    Some((
        v["uuid"].as_str()?.to_owned(),
        femme_agents::claude::monitor_event(text)?,
    ))
}

/// A transcript line's goal verdict (`{"type":"attachment","attachment":{"type":"goal_status"..}}`).
fn goal_line(line: &[u8]) -> Option<Goal> {
    let v: Value = serde_json::from_slice(line).ok()?;
    (v["type"] == "attachment")
        .then(|| femme_agents::claude::goal_status(&v["attachment"]))
        .flatten()
}

/// The mode after `now` in Claude's Shift+Tab order.
fn next_permission(now: Option<&str>) -> &'static str {
    match now {
        Some("acceptEdits") => "plan",
        Some("plan") => "auto",
        Some("auto") => "bypassPermissions",
        Some("default" | "manual") => "acceptEdits",
        // bypass, dontAsk, unknown
        _ => "default",
    }
}

/// `<state>/commands.json`: the `/` commands of the last `claude` that started, for the composer's
/// menu ([`commands`]).
// ponytail: one list for the box; project commands differ per folder, the last start wins.
fn save_commands(d: &Daemon, names: &[String]) {
    let path = d.cfg.state_path("commands.json");
    let json = serde_json::to_string(names).unwrap_or_default();
    if std::fs::read_to_string(&path).ok().as_deref() != Some(json.as_str()) {
        if let Err(e) = std::fs::write(&path, json) {
            tracing::debug!(%e, "commands not saved");
        }
    }
}

/// The `/` commands a Claude session takes: the saved list plus FEMME's own `btw`, sorted.
pub(crate) fn commands(d: &Daemon) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_to_string(d.cfg.state_path("commands.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    names.push("btw".into());
    names.sort();
    names.dedup();
    names
}

/// Saves the commands once when no session has yet: a `claude -p /context` is answered without
/// the model, and its init line lists them.
pub(crate) async fn seed_commands(d: Weak<Daemon>) {
    let Some(dm) = d.upgrade() else { return };
    if dm.cfg.state_path("commands.json").exists() {
        return;
    }
    let path = dm.sessions.path(&dm.cfg);
    let Some(claude) = find_on_path("claude", &path) else {
        return;
    };
    let mut cmd = tokio::process::Command::new(claude);
    cmd.args([
        "-p",
        "--output-format=stream-json",
        "--verbose",
        "--no-session-persistence",
        "/context",
    ])
    .current_dir(&dm.cfg.home)
    .envs(dm.sessions.child_env(&dm.cfg, &path))
    .stdin(std::process::Stdio::null())
    .kill_on_drop(true);
    drop(dm);
    let Ok(out) = tokio::time::timeout(Duration::from_secs(60), cmd.output()).await else {
        return;
    };
    let Ok(out) = out else { return };
    let mut driver = driver_for(Agent::Claude);
    for line in out.stdout.split(|&b| b == b'\n') {
        for ev in driver.on_line(0, line, &mut Out::default()) {
            if let (AgentEvent::Commands(c), Some(d)) = (ev, d.upgrade()) {
                save_commands(&d, &c);
                return;
            }
        }
    }
}

// ---- events to rows (pure) ----------------------------------------------------------------------

/// What the driver's events leave behind between batches.
#[derive(Default)]
struct FoldState {
    /// The text streamed so far of the items still open: the offsets of the next delta, and what a
    /// client that missed it (a replay) is caught up with.
    streams: HashMap<String, String>,
    /// A turn ended (not interrupted) since the agent was last working: its idle is `done`.
    done: bool,
}

/// One write to the transcript.
enum Put {
    /// An item opens: never written over an item that is already there (a replay).
    Start(Item),
    /// An item is final (or a daemon-made one): not written again when it is the same.
    Done(Item),
    /// An open item's body changes.
    Update(String, Value),
    /// The agent took an item back: deleted, with a tombstone ([`Daemon::say_removed`]).
    Remove(String),
}

/// The store's rev before a write that holds a [`Put::Remove`], for [`Daemon::say_removed`].
fn removes(d: &Daemon, puts: &[Put]) -> Option<u64> {
    puts.iter()
        .any(|p| matches!(p, Put::Remove(_)))
        .then(|| d.store.rev().ok())
        .flatten()
}

#[derive(Default)]
struct Folded {
    puts: Vec<Put>,
    /// `(item id, offset, text)` of streamed text, for the hub only.
    deltas: Vec<(String, u32, String)>,
    /// The session's status after the batch, if it said.
    state: Option<(SessionStatus, Attention)>,
    /// What a question asked, for the push.
    asked: Option<String>,
    agent_sid: Option<String>,
    model: Option<String>,
    usage: Option<UsageSnapshot>,
    /// The `/` commands the agent said it takes.
    commands: Option<Vec<String>>,
    permission: Option<String>,
    /// What the agent's latest model call read, in tokens.
    ctx_used: Option<u64>,
    /// The agent's context window in tokens, when the batch said.
    ctx_window: Option<u64>,
    /// Where the agent compacts by itself, when the batch said.
    ctx_compact_at: Option<u64>,
    /// How many times the batch says the conversation was compacted.
    compacted: u32,
    /// Prompts the process dropped without running them: sent again to the next one.
    dropped: Vec<HeldPrompt>,
    /// Prompts the agent refused: they will not run ([`crate::sent::settle`]).
    missed: Vec<sent::Held>,
    turn_started: bool,
    turn_ended: bool,
    /// Claude's `/goal` as the batch last said it.
    goal: Option<Goal>,
    /// A prompt of the main thread landed (a mirrored pane's typed line).
    prompted: bool,
}

/// The session row's share of a batch.
struct RowFx {
    agent_sid: Option<String>,
    model: Option<String>,
    state: Option<(SessionStatus, Attention)>,
    /// The first line of a new process: `Starting` is over.
    ready: bool,
    driver: AgentStatus,
    asked: Option<String>,
    permission: Option<String>,
    ctx_used: Option<u64>,
    ctx_window: Option<u64>,
    ctx_compact_at: Option<u64>,
    /// Compactions to count: only those of a live line. A restart replays the lines of the turn
    /// that was running, and the row counted them the first time; the lines the agent wrote while
    /// the daemon was away are read as that same replay, so they are not counted either.
    compacted: u32,
    goal: Option<Goal>,
}

fn new_item(
    sid: Sid,
    id: String,
    kind: ItemKind,
    status: ItemStatus,
    b: Option<&Value>,
    now: i64,
) -> Item {
    Item {
        sid,
        item_id: id,
        seq: 0,
        rev: 0,
        kind,
        status,
        preview: b.map(|b| preview_of(kind, b)).unwrap_or_default(),
        body: b.map(Value::to_string),
        blob_hash: None,
        parent: None,
        ts: now,
    }
}

fn event_item(sid: Sid, id: String, text: &str, now: i64) -> Item {
    new_item(
        sid,
        id,
        ItemKind::Event,
        ItemStatus::Completed,
        Some(&body(&Event {
            text: text.into(),
            ..Default::default()
        })),
        now,
    )
}

/// Turns the driver's events of one line (or one exit) into writes and effects. `base` names the
/// items the daemon makes itself (`<base>-<n>`): the same spool line always gives the same ids.
fn fold(st: &mut FoldState, sid: Sid, base: &str, events: Vec<AgentEvent>, now: i64) -> Folded {
    let mut f = Folded::default();
    let mut own = 0;
    let mut next_id = || {
        own += 1;
        format!("{base}-{own}")
    };
    for ev in events {
        match ev {
            AgentEvent::Started { agent_sid, model } => {
                f.agent_sid = Some(agent_sid);
                if model.is_some() {
                    f.model = model;
                }
            }
            AgentEvent::TurnStarted => {
                st.done = false;
                f.turn_started = true;
                f.state = Some((SessionStatus::Working, Attention::None));
            }
            AgentEvent::TurnEnded {
                stop,
                usage,
                duration_ms,
            } => {
                st.done = stop != StopReason::Interrupted;
                f.turn_ended = true;
                let b = body(&TurnEnd {
                    stop,
                    usage,
                    duration_ms,
                });
                f.puts.push(Put::Done(new_item(
                    sid,
                    next_id(),
                    ItemKind::TurnEnd,
                    ItemStatus::Completed,
                    Some(&b),
                    now,
                )));
            }
            AgentEvent::ItemStarted {
                id,
                kind,
                preview,
                parent,
            } => {
                let mut item = new_item(sid, id, kind, ItemStatus::InProgress, None, now);
                item.preview = preview;
                item.parent = parent;
                f.puts.push(Put::Start(item));
            }
            AgentEvent::ItemDelta { id, text } => {
                let acc = st.streams.entry(id.clone()).or_default();
                let offset = u32::try_from(acc.len()).unwrap_or(u32::MAX);
                acc.push_str(&text);
                f.deltas.push((id, offset, text));
            }
            AgentEvent::ItemUpdated { id, body } => f.puts.push(Put::Update(id, body)),
            AgentEvent::ItemRetracted { id } => {
                st.streams.remove(&id);
                f.puts.push(Put::Remove(id));
            }
            AgentEvent::ItemCompleted {
                id,
                kind,
                status,
                body,
                parent,
            } => {
                st.streams.remove(&id);
                if kind == ItemKind::UserMessage && parent.is_none() {
                    f.prompted = true;
                }
                let mut item = new_item(sid, id, kind, status, Some(&body), now);
                item.parent = parent;
                f.puts.push(Put::Done(item));
            }
            AgentEvent::QuestionAsked { item_id, q } => {
                f.asked = Some(q.prompt.clone());
                let b = body(&QuestionItem {
                    q,
                    answered: None,
                    reason: None,
                });
                f.puts.push(Put::Start(new_item(
                    sid,
                    item_id,
                    ItemKind::Question,
                    ItemStatus::Pending,
                    Some(&b),
                    now,
                )));
                f.state = Some((SessionStatus::Blocked, Attention::Blocked));
            }
            // the question's item is completed by its own event, the status follows from `Status`
            AgentEvent::QuestionResolved { .. } => {}
            AgentEvent::PromptDropped {
                text,
                how,
                reason,
                id,
            } => {
                let why = match &reason {
                    DropReason::Exited => "the agent exited first".to_string(),
                    DropReason::Refused(r) => r.clone(),
                };
                let line = format!("A message was not run ({why}): {}", cut(&text, 300));
                f.puts
                    .push(Put::Done(event_item(sid, next_id(), &line, now)));
                if reason != DropReason::Exited {
                    f.missed.push((id, text));
                } else {
                    f.dropped.push(HeldPrompt {
                        text,
                        how,
                        queued_at_ms: u64::try_from(now).unwrap_or(0),
                        id,
                    });
                }
            }
            AgentEvent::RateLimit {
                kind,
                percent,
                resets_at,
            } => {
                if kind == "session" {
                    f.usage = Some(UsageSnapshot {
                        percent: f64::from(percent),
                        resets_at: resets_at.map(|s| s * 1000),
                    });
                }
            }
            AgentEvent::ModelChanged(m) => f.model = Some(m),
            AgentEvent::Commands(c) => f.commands = Some(c),
            AgentEvent::PermissionMode(m) => f.permission = Some(m),
            AgentEvent::Status(AgentStatus::Working) => {
                st.done = false;
                f.state = Some((SessionStatus::Working, Attention::None));
            }
            AgentEvent::Status(AgentStatus::Blocked) => {
                f.state = Some((SessionStatus::Blocked, Attention::Blocked));
            }
            AgentEvent::Status(AgentStatus::Idle) => {
                let attention = if st.done {
                    Attention::Done
                } else {
                    Attention::None
                };
                st.done = false;
                f.state = Some((SessionStatus::Idle, attention));
            }
            AgentEvent::Context {
                used,
                window,
                compact_at,
            } => {
                f.ctx_used = Some(used);
                f.ctx_window = window.or(f.ctx_window);
                f.ctx_compact_at = compact_at.or(f.ctx_compact_at);
            }
            // The bar empties at the level up; the next model call says the new size.
            AgentEvent::Compacted { .. } => {
                f.compacted += 1;
                f.ctx_used = Some(0);
            }
            AgentEvent::Goal(g) => f.goal = Some(g),
            AgentEvent::Exited { .. } | AgentEvent::Raw(_) => {}
        }
    }
    f
}

/// Held prompts as [`sent`] compares them with saved messages.
fn held_of(held: Vec<HeldPrompt>) -> Vec<sent::Held> {
    held.into_iter().map(|h| (h.id, h.text)).collect()
}

fn is_final(s: ItemStatus) -> bool {
    matches!(
        s,
        ItemStatus::Completed | ItemStatus::Failed | ItemStatus::Answered
    )
}

// ponytail: the store cuts a body at 1 MiB, so a longer one never equals what it stored and is
// written again (rev bumped) on a replay; compare against the cut body if that ever shows.
fn same(old: &Item, new: &Item) -> bool {
    old.kind == new.kind
        && old.status == new.status
        && old.parent == new.parent
        && match (&old.body, old.blob_hash, &new.body) {
            (Some(a), _, Some(b)) => a == b,
            (None, Some(h), Some(b)) => femme_store::sha256(b.as_bytes()) == h,
            (None, None, None) => true,
            _ => false,
        }
}

/// Applies one write inside the transaction; the items it stores go into `published`.
fn put(tx: &mut Tx<'_>, sid: Sid, p: &Put, published: &mut Vec<Item>) -> femme_store::Result<()> {
    match p {
        Put::Start(item) => {
            if store_items::get_item(tx, sid, &item.item_id)?.is_none() {
                published.push(tx.put_item(item)?);
            }
        }
        Put::Done(item) => {
            let mut item = item.clone();
            match store_items::get_item(tx, sid, &item.item_id)? {
                // the echo of a saved message, paired when it first came
                Some(old) if sent::is_read_mark(&old) => return Ok(()),
                Some(old) => {
                    if same(&old, &item) {
                        return Ok(());
                    }
                    item.ts = old.ts;
                }
                None => {
                    if sent::reconcile(tx, sid, &item, published)? {
                        return Ok(());
                    }
                }
            }
            published.push(tx.put_item(&item)?);
        }
        Put::Update(id, b) => {
            let Some(mut item) = store_items::get_item(tx, sid, id)? else {
                return Ok(());
            };
            if is_final(item.status) {
                return Ok(());
            }
            let preview = preview_of(item.kind, b);
            if !preview.is_empty() {
                item.preview = preview;
            }
            item.body = Some(b.to_string());
            item.blob_hash = None;
            published.push(tx.put_item(&item)?);
        }
        Put::Remove(id) => {
            if store_items::get_item(tx, sid, id)?.is_some() {
                tx.delete_item(sid, id)?;
            }
        }
    }
    Ok(())
}

/// The first line of an item for list rows (the store cuts it to 512 bytes).
fn preview_of(kind: ItemKind, b: &Value) -> String {
    let s = |k: &str| b[k].as_str().unwrap_or("");
    let first = |t: &str| t.lines().next().unwrap_or("").to_string();
    match kind {
        ItemKind::UserMessage
        | ItemKind::PeerMessage
        | ItemKind::AssistantMessage
        | ItemKind::Thinking
        | ItemKind::Plan
        | ItemKind::Event => s("text").to_string(),
        ItemKind::ToolCall => {
            let hint = [
                "command",
                "file_path",
                "path",
                "pattern",
                "url",
                "query",
                "description",
            ]
            .iter()
            .find_map(|k| b["input"][k].as_str())
            .unwrap_or("");
            first(format!("{} {hint}", s("name")).trim())
        }
        ItemKind::ToolResult => first(s("output")),
        ItemKind::Question => s("prompt").to_string(),
        ItemKind::TurnStart | ItemKind::TurnEnd => String::new(),
    }
}

// ---- the screen and raw input -------------------------------------------------------------------

/// The text of a headless session's `/screen`: the items, oldest first, one block each; a pending
/// question lists its options with numbers (`input` answers with the number). An item still open
/// has no body in the store: `streams` holds the text it has so far.
fn render(items: &[(Item, Value)], streams: &HashMap<String, String>) -> String {
    let mut out = String::new();
    for (item, b) in items {
        let text = |k: &str| match b[k].as_str() {
            Some(t) => t.to_string(),
            None => streams
                .get(&item.item_id)
                .unwrap_or(&item.preview)
                .to_string(),
        };
        let line = match item.kind {
            ItemKind::UserMessage => match item.status {
                ItemStatus::Pending => format!("> {} (queued)", text("text")),
                ItemStatus::Failed => format!("> {} (not received)", text("text")),
                _ => format!("> {}", text("text")),
            },
            ItemKind::PeerMessage => format!("@{}: {}", text("from"), text("text")),
            ItemKind::AssistantMessage | ItemKind::Plan => text("text"),
            ItemKind::ToolCall => format!("* {}", item.preview),
            ItemKind::ToolResult => format!("  {}", item.preview),
            ItemKind::Event => format!("- {}", text("text")),
            ItemKind::Question => question_lines(item, b),
            ItemKind::Thinking | ItemKind::TurnStart | ItemKind::TurnEnd => continue,
        };
        out.push_str(cut(&line, SCREEN_ITEM_MAX));
        out.push('\n');
    }
    out
}

fn question_lines(item: &Item, b: &Value) -> String {
    let Ok(q) = serde_json::from_value::<QuestionItem>(b.clone()) else {
        return format!("? {}", item.preview);
    };
    let mut s = format!("? {}", q.q.prompt);
    if item.status == ItemStatus::Pending {
        for (n, o) in q.q.options.iter().enumerate() {
            s += &format!("\n  {}. {o}", n + 1);
        }
    } else if let Some(a) = &q.answered {
        s += &format!(" -> {}", answer_text(a, &q.q.options));
    } else if let Some(r) = &q.reason {
        s += &format!(" (not answered: {r})");
    }
    s
}

fn answer_text(a: &Answer, options: &[String]) -> String {
    let label = |i: &u16| {
        options
            .get(usize::from(*i))
            .cloned()
            .unwrap_or_else(|| format!("#{i}"))
    };
    match a {
        Answer::Option(i) => label(i),
        Answer::Options(v) => v.iter().map(label).collect::<Vec<_>>().join(", "),
        Answer::Text(t) => t.clone(),
        Answer::Cancel => "cancelled".into(),
    }
}

/// What raw input answers a question with: Esc cancels, a number picks that option (`Options` for a
/// multiple-choice question), `y` is the first option and `n` the second (Allow/Deny, Approve/Keep
/// planning). Anything else answers nothing.
pub(crate) fn answer_from_input(bytes: &[u8], q: &Question) -> Option<Answer> {
    if bytes == b"\x1b" {
        return Some(Answer::Cancel);
    }
    let s = std::str::from_utf8(bytes)
        .ok()?
        .trim_matches(|c| matches!(c, '\r' | '\n' | ' '));
    let pick = |i: usize| {
        let n = u16::try_from(i).ok().filter(|_| i < q.options.len())?;
        Some(if q.multi {
            Answer::Options(vec![n])
        } else {
            Answer::Option(n)
        })
    };
    match s {
        "y" | "Y" => pick(0),
        "n" | "N" => pick(1),
        _ => s
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1)
            .and_then(|n| pick(n - 1)),
    }
}

/// An item's body as JSON (`Null` when it has none, or it is gone).
fn body_value(store: &Store, i: &Item) -> Value {
    let text = match (&i.body, i.blob_hash) {
        (Some(b), _) => Some(b.clone()),
        (None, Some(h)) => store
            .get_blob(&h)
            .ok()
            .flatten()
            .and_then(|b| String::from_utf8(b).ok()),
        (None, None) => None,
    };
    text.and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

// ---- the daemon's side of it --------------------------------------------------------------------

impl Daemon {
    /// The open questions of a headless session, oldest first (a request with several has one
    /// item each, answered one by one).
    fn pending_questions(&self, sid: Sid) -> Result<Vec<(Item, QuestionItem)>, CoreError> {
        let items = self
            .store
            .read(|c| store_items::page_items(c, sid, None, 200))
            .map_err(core_err)?;
        Ok(items
            .into_iter()
            .rev()
            .filter(|i| i.kind == ItemKind::Question && i.status == ItemStatus::Pending)
            .filter_map(|i| {
                let q = serde_json::from_value(body_value(&self.store, &i)).ok()?;
                Some((i, q))
            })
            .collect())
    }

    /// The oldest open question of a headless session.
    pub(crate) fn pending_question(&self, sid: Sid) -> Result<Option<Question>, CoreError> {
        Ok(self
            .pending_questions(sid)?
            .into_iter()
            .next()
            .map(|(_, q)| q.q))
    }

    /// The write that turns the open question `request_id` into `Answered` with `a`.
    fn answered_part(&self, sid: Sid, request_id: &str, a: &Answer) -> Option<Put> {
        let (item, mut q) = self
            .pending_questions(sid)
            .ok()?
            .into_iter()
            .find(|(_, q)| q.q.request_id == request_id)?;
        q.answered = Some(a.clone());
        Some(Put::Done(new_item(
            sid,
            item.item_id,
            ItemKind::Question,
            ItemStatus::Answered,
            Some(&body(&q)),
            item.ts,
        )))
    }

    /// The text a headless session shows as its screen. The actor is asked first: an item that is
    /// done by the time the store is read is read in full.
    pub(crate) async fn headless_screen(&self, sid: Sid) -> Result<String, CoreError> {
        let streams = match self.sessions.head(sid) {
            Some(head) => head.streams().await,
            None => HashMap::new(),
        };
        let mut items = self
            .store
            .read(|c| store_items::page_items(c, sid, None, SCREEN_ITEMS))
            .map_err(core_err)?;
        items.reverse();
        let items: Vec<(Item, Value)> = items
            .into_iter()
            .map(|i| {
                let b = body_value(&self.store, &i);
                (i, b)
            })
            .collect();
        Ok(render(&items, &streams))
    }

    /// Raw input to a headless session: it answers the question the session waits on, or it is
    /// `Busy`.
    pub(crate) async fn headless_input(&self, sid: Sid, bytes: &[u8]) -> Result<(), CoreError> {
        let head = self.sessions.head(sid).ok_or_else(gone)?;
        let q = self.pending_question(sid)?.ok_or(CoreError::Busy)?;
        let a = answer_from_input(bytes, &q).ok_or(CoreError::Busy)?;
        head.answer(&q.request_id, a).await
    }

    pub(crate) async fn interrupt_session(&self, sid: Sid) -> Result<(), CoreError> {
        let s = self.session(sid)?;
        if s.agent == Agent::Shell {
            return Err(CoreError::Busy);
        }
        if s.mode == Mode::Tui {
            return self.send_bytes(sid, b"\x1b").await;
        }
        self.sessions
            .head(sid)
            .ok_or(CoreError::Busy)?
            .interrupt()
            .await
    }

    pub(crate) async fn background_task(
        &self,
        sid: Sid,
        tool_use_id: Option<&str>,
    ) -> Result<(), CoreError> {
        let s = self.session(sid)?;
        if s.agent == Agent::Shell {
            return Err(CoreError::Busy);
        }
        if s.mode == Mode::Tui {
            return self.send_bytes(sid, b"\x02").await; // Ctrl+B
        }
        self.sessions
            .head(sid)
            .ok_or(CoreError::Busy)?
            .background(tool_use_id)
            .await
    }

    pub(crate) async fn answer_question(
        &self,
        sid: Sid,
        request_id: &str,
        answer: Answer,
    ) -> Result<(), CoreError> {
        let s = self.session(sid)?;
        if s.mode != Mode::Headless {
            return Err(CoreError::Bad(
                "only a headless session has questions to answer".into(),
            ));
        }
        self.sessions
            .head(sid)
            .ok_or_else(gone)?
            .answer(request_id, answer)
            .await
    }

    pub(crate) async fn switch_model(&self, sid: Sid, model: &str) -> Result<(), CoreError> {
        let s = self.session(sid)?;
        if s.mode != Mode::Headless {
            return Err(CoreError::Bad(
                "the model of a PTY session is the TUI's to change".into(),
            ));
        }
        self.sessions
            .head(sid)
            .ok_or_else(gone)?
            .set_model(model)
            .await
    }

    /// Claude's next permission mode, the order Shift+Tab steps through them.
    pub(crate) async fn cycle_permission(&self, sid: Sid) -> Result<(), CoreError> {
        let s = self.session(sid)?;
        if s.agent != Agent::Claude || s.mode != Mode::Headless {
            return Err(CoreError::Bad(
                "only a Claude chat has permission modes".into(),
            ));
        }
        self.sessions
            .head(sid)
            .ok_or_else(gone)?
            .set_permission(next_permission(s.permission.as_deref()))
            .await
    }

    pub(crate) async fn switch_mode(&self, sid: Sid, to: Mode) -> Result<(), CoreError> {
        let s = self.session(sid)?;
        if s.agent == Agent::Shell {
            return Err(CoreError::Bad("a shell has no headless mode".into()));
        }
        let head = self.sessions.head(sid).ok_or_else(|| {
            CoreError::Bad(
                "this session was not started headless, there is nothing to switch".into(),
            )
        })?;
        head.takeover(to).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use femme_proto::holder::{HolderAgent, HolderInfo, HolderPane, HolderReq, HolderResp};
    use femme_proto::{codec, HOLDER_VERSION};
    use futures::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio::net::{UnixListener, UnixStream};
    use tokio_util::codec::LengthDelimitedCodec;

    use crate::config::Config;

    const SID: Sid = Id(7);

    fn question() -> Question {
        Question {
            request_id: "r1".into(),
            prompt: "Which?".into(),
            options: vec!["a".into(), "b".into()],
            multi: false,
            free_text: false,
        }
    }

    fn turn(stop: StopReason) -> Vec<AgentEvent> {
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::ItemCompleted {
                id: "m1".into(),
                kind: ItemKind::AssistantMessage,
                status: ItemStatus::Completed,
                body: json!({"text": "hi"}),
                parent: None,
            },
            AgentEvent::TurnEnded {
                stop,
                usage: None,
                duration_ms: Some(5),
            },
            AgentEvent::Status(AgentStatus::Idle),
        ]
    }

    fn item(kind: ItemKind, status: ItemStatus, preview: &str) -> Item {
        Item {
            sid: SID,
            item_id: "i".into(),
            seq: 1,
            rev: 1,
            kind,
            status,
            preview: preview.into(),
            body: None,
            blob_hash: None,
            parent: None,
            ts: 0,
        }
    }

    fn session_row(sid: Sid, mode: Mode, status: SessionStatus) -> Session {
        Session {
            id: sid,
            handle: String::new(),
            rev: 0,
            name: None,
            owner: "board".into(),
            agent: Agent::Claude,
            mode,
            cwd: "/".into(),
            label: None,
            agent_sid: None,
            model: None,
            status,
            attention: Attention::None,
            created: 0,
            closed: None,
            permission: None,
            context: None,
            effort: None,
            goal: None,
        }
    }

    /// A store with one session, and a way to apply writes to it: what each one published.
    fn store_with_session() -> (Store, Sid) {
        let store = Store::open_memory().unwrap();
        let row = session_row(Id(0), Mode::Headless, SessionStatus::Starting);
        let sid = store.write(|tx| Ok(tx.insert_session(&row)?.id)).unwrap();
        (store, sid)
    }

    fn apply_puts(store: &Store, sid: Sid, puts: Vec<Put>) -> Vec<Item> {
        let mut published = Vec::new();
        store
            .write(|tx| {
                for p in &puts {
                    put(tx, sid, p, &mut published)?;
                }
                Ok(())
            })
            .unwrap();
        published
    }

    /// The echo of a saved message reads it, once: a replay of the same line changes nothing.
    #[test]
    fn a_saved_message_is_read_by_its_echo_and_a_replay_leaves_it_read() {
        let (store, sid) = store_with_session();
        let mut saved = item(ItemKind::UserMessage, ItemStatus::Pending, "fix it");
        saved.sid = sid;
        saved.item_id = "01JC0SAVED".into();
        saved.body =
            Some(r#"{"text":"fix it","echo":"c8eed47b-e7a7-4739-8260-f78e891051e8"}"#.into());
        saved.ts = 1_000;
        saved.seq = 0;
        apply_puts(&store, sid, vec![Put::Done(saved)]);
        let mut echo = item(ItemKind::UserMessage, ItemStatus::Completed, "fix it");
        echo.sid = sid;
        echo.item_id = "c8eed47b-e7a7-4739-8260-f78e891051e8".into();
        echo.body = Some(r#"{"text":"fix it"}"#.into());
        echo.ts = 4_000;
        echo.seq = 0;
        let first = apply_puts(&store, sid, vec![Put::Done(echo.clone())]);
        assert_eq!(
            first.iter().map(|i| (i.kind, i.status)).collect::<Vec<_>>(),
            [
                (ItemKind::UserMessage, ItemStatus::Completed),
                (ItemKind::Event, ItemStatus::Completed)
            ]
        );
        assert!(apply_puts(&store, sid, vec![Put::Done(echo)]).is_empty());
        let all = store
            .read(|c| store_items::page_items(c, sid, None, 10))
            .unwrap();
        assert_eq!(all.len(), 2, "the message and where it was read: {all:?}");
    }

    #[test]
    fn a_finished_turn_ends_done_and_an_interrupted_one_ends_quiet() {
        let mut st = FoldState::default();
        let f = fold(&mut st, SID, "d0", turn(StopReason::EndTurn), 1);
        assert_eq!(f.state, Some((SessionStatus::Idle, Attention::Done)));
        assert!(f.turn_started && f.turn_ended);
        assert_eq!(f.puts.len(), 2, "the message and the end of the turn");
        let f = fold(&mut st, SID, "d9", turn(StopReason::Interrupted), 1);
        assert_eq!(f.state, Some((SessionStatus::Idle, Attention::None)));
    }

    #[test]
    fn a_question_blocks_the_session_and_says_what_it_asks() {
        let mut st = FoldState::default();
        let ev = AgentEvent::QuestionAsked {
            item_id: "q1".into(),
            q: question(),
        };
        let f = fold(&mut st, SID, "d0", vec![ev], 1);
        assert_eq!(f.state, Some((SessionStatus::Blocked, Attention::Blocked)));
        assert_eq!(f.asked.as_deref(), Some("Which?"));
        match &f.puts[..] {
            [Put::Start(i)] => assert_eq!(
                (i.kind, i.status),
                (ItemKind::Question, ItemStatus::Pending)
            ),
            _ => panic!("one pending question item"),
        }
    }

    #[test]
    fn a_prompt_the_exit_dropped_is_kept_for_the_next_process_and_a_refused_one_is_not() {
        let drop = |text: &str, reason| AgentEvent::PromptDropped {
            text: text.into(),
            how: PromptHow::Queue,
            reason,
            id: None,
        };
        let evs = vec![
            drop("a", DropReason::Exited),
            drop("b", DropReason::Refused("no".into())),
        ];
        let f = fold(&mut FoldState::default(), SID, "x1", evs, 5);
        let kept: Vec<_> = f.dropped.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(kept, ["a"]);
        let ids: Vec<_> = f
            .puts
            .iter()
            .map(|p| match p {
                Put::Done(i) => i.item_id.as_str(),
                _ => panic!("events are final"),
            })
            .collect();
        assert_eq!(
            ids,
            ["x1-1", "x1-2"],
            "each leaves an event, ids of their own"
        );
    }

    #[test]
    fn streamed_text_is_numbered_by_the_bytes_before_it() {
        let mut st = FoldState::default();
        let delta = |text: &str| AgentEvent::ItemDelta {
            id: "m1".into(),
            text: text.into(),
        };
        let f = fold(&mut st, SID, "d0", vec![delta("ab"), delta("cd")], 1);
        let got: Vec<_> = f
            .deltas
            .iter()
            .map(|(_, at, t)| (*at, t.as_str()))
            .collect();
        assert_eq!(got, [(0, "ab"), (2, "cd")]);
        assert_eq!(
            st.streams["m1"], "abcd",
            "kept for a client that missed them"
        );
        fold(&mut st, SID, "d1", turn(StopReason::EndTurn), 1);
        assert!(
            st.streams.is_empty(),
            "the completed item holds the text now"
        );
    }

    #[test]
    fn only_the_session_window_is_the_usage_and_its_reset_is_in_milliseconds() {
        let limit = |kind: &str| AgentEvent::RateLimit {
            kind: kind.into(),
            percent: 31.0,
            resets_at: Some(1000),
        };
        let f = fold(
            &mut FoldState::default(),
            SID,
            "d0",
            vec![limit("session")],
            1,
        );
        assert_eq!(
            f.usage,
            Some(UsageSnapshot {
                percent: 31.0,
                resets_at: Some(1_000_000)
            })
        );
        let f = fold(
            &mut FoldState::default(),
            SID,
            "d0",
            vec![limit("weekly")],
            1,
        );
        assert_eq!(f.usage, None);
    }

    fn say(used: u64, window: Option<u64>) -> AgentEvent {
        AgentEvent::Context {
            used,
            window,
            compact_at: None,
        }
    }

    fn compacted() -> AgentEvent {
        AgentEvent::Compacted {
            pre_tokens: Some(45_000),
        }
    }

    fn ctx(used: u64, window: u64, compactions: u32) -> Option<Context> {
        Some(Context {
            used,
            window,
            compactions,
            compact_at: None,
        })
    }

    /// What the row's context is after the batches, each folded as one spool line.
    fn context_after(batches: Vec<Vec<AgentEvent>>) -> Option<Context> {
        let mut st = FoldState::default();
        let mut row = None;
        for (i, events) in batches.into_iter().enumerate() {
            let f = fold(&mut st, SID, &format!("d{i}"), events, 1);
            row = with_context(row, f.ctx_used, f.ctx_window, f.ctx_compact_at, f.compacted);
        }
        row
    }

    #[test]
    fn a_context_event_sets_what_was_read_and_a_window_stays_until_one_is_said_again() {
        assert_eq!(
            context_after(vec![vec![say(44_302, None)]]),
            ctx(44_302, 0, 0),
            "the window is 0 until the agent says"
        );
        assert_eq!(
            context_after(vec![
                vec![say(44_302, None)],
                vec![say(44_302, Some(1_000_000))]
            ]),
            ctx(44_302, 1_000_000, 0)
        );
        assert_eq!(
            context_after(vec![
                vec![say(44_302, Some(1_000_000))],
                vec![say(45_000, None)]
            ]),
            ctx(45_000, 1_000_000, 0),
            "a call that does not repeat the window leaves the one the row has"
        );
    }

    #[test]
    fn a_compaction_counts_empties_what_was_read_and_keeps_the_window() {
        assert_eq!(
            context_after(vec![
                vec![say(45_000, Some(1_000_000))],
                vec![compacted()],
                vec![compacted()]
            ]),
            ctx(0, 1_000_000, 2)
        );
        assert_eq!(
            context_after(vec![vec![
                say(45_000, Some(1_000_000)),
                compacted(),
                say(9_000, None)
            ]]),
            ctx(9_000, 1_000_000, 1),
            "a call after the compaction in the same batch says the new size"
        );
        assert_eq!(
            context_after(vec![vec![compacted()]]),
            ctx(0, 0, 1),
            "nothing read is known yet, the level is already one up"
        );
    }

    #[test]
    fn an_agent_that_never_says_leaves_the_row_without_a_context() {
        assert_eq!(context_after(vec![turn(StopReason::EndTurn)]), None);
        let kept = Context {
            used: 1,
            window: 2,
            compactions: 3,
            compact_at: Some(4),
        };
        assert_eq!(with_context(Some(kept), None, None, None, 0), Some(kept));
        // where the agent compacts stays until it is said again
        assert_eq!(
            with_context(Some(kept), Some(5), None, None, 0)
                .unwrap()
                .compact_at,
            Some(4)
        );
        let said = AgentEvent::Context {
            used: 5,
            window: None,
            compact_at: Some(9),
        };
        let row = context_after(vec![vec![said], vec![say(6, None)]]).unwrap();
        assert_eq!((row.used, row.compact_at), (6, Some(9)));
    }

    #[test]
    fn within_one_line_the_last_word_wins_and_the_last_window_said_stays() {
        let evs = vec![say(10, Some(100)), say(20, None), compacted(), compacted()];
        let f = fold(&mut FoldState::default(), SID, "d0", evs, 1);
        assert_eq!(
            (f.ctx_used, f.ctx_window, f.compacted),
            (Some(0), Some(100), 2),
            "the compactions came last: the bar is empty"
        );
        let evs = vec![say(10, Some(100)), compacted(), say(20, None)];
        let f = fold(&mut FoldState::default(), SID, "d0", evs, 1);
        assert_eq!(
            (f.ctx_used, f.ctx_window, f.compacted),
            (Some(20), Some(100), 1)
        );
    }

    #[test]
    fn a_replayed_context_changes_nothing_that_is_stored() {
        let (store, sid) = store_with_session();
        let write = |used, window, compacted| {
            store
                .write(|tx| {
                    tx.update_session(sid, |s| {
                        s.context = with_context(s.context, used, window, None, compacted)
                    })
                })
                .unwrap()
        };
        let first = write(Some(44_302), Some(1_000_000), 0);
        assert_eq!(first.context, ctx(44_302, 1_000_000, 0));
        // the same lines again, as a restart replays them
        let again = write(Some(44_302), Some(1_000_000), 0);
        assert_eq!(again.rev, first.rev, "nothing changed, nothing was written");
        // a compaction is not part of a replay (the actor adds 0 for it): it only counts live
        let live = write(None, None, 1);
        assert_eq!(live.context, ctx(44_302, 1_000_000, 1));
        assert!(live.rev > first.rev);
    }

    /// Claude took a refused reply back (`ItemRetracted`): its item is deleted and the session's
    /// change feed says so with a tombstone; an id the store does not hold is a no-op.
    #[test]
    fn a_retracted_item_is_deleted_with_a_tombstone() {
        let (store, sid) = store_with_session();
        let partial = new_item(
            sid,
            "m1:0".into(),
            ItemKind::AssistantMessage,
            ItemStatus::Completed,
            Some(&json!({"text": "Step one is"})),
            1,
        );
        apply_puts(&store, sid, vec![Put::Done(partial)]);
        let mut st = FoldState::default();
        st.streams.insert("m1:0".into(), "Step one is".into());
        let retract = |id: &str| AgentEvent::ItemRetracted { id: id.into() };
        let f = fold(
            &mut st,
            sid,
            "o9",
            vec![retract("m1:0"), retract("gone")],
            2,
        );
        assert!(st.streams.is_empty());
        assert!(
            matches!(&f.puts[..], [Put::Remove(a), Put::Remove(b)] if a == "m1:0" && b == "gone")
        );
        let before = store.rev().unwrap();
        assert!(apply_puts(&store, sid, f.puts).is_empty());
        assert!(store
            .read(|c| store_items::get_item(c, sid, "m1:0"))
            .unwrap()
            .is_none());
        let rows = store
            .changes_since(
                femme_proto::rpc::Scope::Session(sid),
                Some(before),
                usize::MAX,
            )
            .unwrap()
            .rows;
        assert!(
            matches!(&rows[..], [femme_proto::state::Row::Tombstone { id, .. }] if *id == format!("{sid}/m1:0")),
            "{rows:?}"
        );
    }

    #[test]
    fn a_replayed_write_changes_nothing_that_is_stored() {
        let (store, sid) = store_with_session();
        let open = || {
            vec![Put::Start(new_item(
                sid,
                "m1".into(),
                ItemKind::AssistantMessage,
                ItemStatus::InProgress,
                None,
                1,
            ))]
        };
        let close = || {
            vec![Put::Done(new_item(
                sid,
                "m1".into(),
                ItemKind::AssistantMessage,
                ItemStatus::Completed,
                Some(&json!({"text": "hi"})),
                2,
            ))]
        };
        assert_eq!(apply_puts(&store, sid, open()).len(), 1);
        let done = apply_puts(&store, sid, close());
        assert_eq!(done.len(), 1);
        // the same lines again, as a restart replays them
        assert!(apply_puts(&store, sid, open()).is_empty());
        assert!(apply_puts(&store, sid, close()).is_empty());
        let late = vec![Put::Update("m1".into(), json!({"text": "h"}))];
        assert!(
            apply_puts(&store, sid, late).is_empty(),
            "never over a final item"
        );
        let stored = store
            .read(|c| store_items::get_item(c, sid, "m1"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (stored.rev, stored.ts),
            (done[0].rev, 1),
            "the item's rev and its first timestamp are the original's"
        );
    }

    // What this proves is the 4 KiB inline limit (the next test is about the 1 MiB cap).
    #[test]
    fn a_big_body_goes_to_a_blob_with_a_short_preview_and_is_not_written_twice() {
        let (store, sid) = store_with_session();
        let big = json!({"text": "x".repeat(store_items::INLINE_MAX + 1000)});
        let close = || {
            vec![Put::Done(new_item(
                sid,
                "m1".into(),
                ItemKind::AssistantMessage,
                ItemStatus::Completed,
                Some(&big),
                2,
            ))]
        };
        let stored = apply_puts(&store, sid, close());
        assert_eq!(stored.len(), 1);
        assert!(stored[0].blob_hash.is_some() && stored[0].body.is_none());
        assert!(
            stored[0].preview.len() <= 512,
            "{}",
            stored[0].preview.len()
        );
        assert!(apply_puts(&store, sid, close()).is_empty());
    }

    #[test]
    fn raw_input_answers_a_question_by_number_y_n_or_escape() {
        let q = question();
        assert_eq!(answer_from_input(b"1\n", &q), Some(Answer::Option(0)));
        assert_eq!(answer_from_input(b"2\r", &q), Some(Answer::Option(1)));
        assert_eq!(answer_from_input(b"y", &q), Some(Answer::Option(0)));
        assert_eq!(answer_from_input(b"N\r", &q), Some(Answer::Option(1)));
        assert_eq!(answer_from_input(b"\x1b", &q), Some(Answer::Cancel));
        assert_eq!(
            answer_from_input(b"3\n", &q),
            None,
            "there is no third option"
        );
        assert_eq!(answer_from_input(b"0", &q), None);
        assert_eq!(answer_from_input(b"hello", &q), None);
        let multi = Question { multi: true, ..q };
        assert_eq!(
            answer_from_input(b"2", &multi),
            Some(Answer::Options(vec![1]))
        );
    }

    #[test]
    fn the_screen_numbers_the_options_of_a_pending_question() {
        let asked = |status, answered| {
            let b = serde_json::to_value(QuestionItem {
                q: question(),
                answered,
                reason: None,
            })
            .unwrap();
            vec![
                (
                    item(ItemKind::UserMessage, ItemStatus::Completed, "hi"),
                    json!({"text": "hi"}),
                ),
                (
                    item(
                        ItemKind::AssistantMessage,
                        ItemStatus::Completed,
                        "echo: hi",
                    ),
                    json!({"text": "echo: hi"}),
                ),
                (item(ItemKind::Question, status, "Which?"), b),
                (
                    item(ItemKind::TurnEnd, ItemStatus::Completed, ""),
                    json!({}),
                ),
            ]
        };
        assert_eq!(
            render(&asked(ItemStatus::Pending, None), &HashMap::new()),
            "> hi\necho: hi\n? Which?\n  1. a\n  2. b\n"
        );
        assert_eq!(
            render(
                &asked(ItemStatus::Answered, Some(Answer::Option(1))),
                &HashMap::new()
            ),
            "> hi\necho: hi\n? Which? -> b\n"
        );
    }

    #[test]
    fn a_tool_call_previews_as_its_name_and_first_line_of_what_it_runs() {
        let b = json!({"name": "Bash", "input": {"command": "ls -la\nmore"}, "call_id": "c"});
        assert_eq!(preview_of(ItemKind::ToolCall, &b), "Bash ls -la");
        let r = json!({"call_id": "c", "output": "one\ntwo", "is_error": false});
        assert_eq!(preview_of(ItemKind::ToolResult, &r), "one");
    }

    #[test]
    fn model_switches_and_permission_steps() {
        assert_eq!(model_switch("/model deepseek").as_deref(), Some(DEEPSEEK));
        assert_eq!(
            model_switch("/model opus").as_deref(),
            Some("claude-opus-5-5")
        );
        assert_eq!(
            model_switch("/model claude-haiku-5-5").as_deref(),
            Some("claude-haiku-5-5")
        );
        assert_eq!(model_switch("/model whatever"), None);
        assert_eq!(
            goal_switch("/goal the file says done").map(|g| (g.state, g.condition)),
            Some((GoalState::Active, "the file says done".into()))
        );
        assert_eq!(
            goal_switch("/goal clear").map(|g| g.state),
            Some(GoalState::Cleared)
        );
        assert_eq!(goal_switch("/goal"), None);
        assert_eq!(goal_switch("/goals x"), None);
        let line = br#"{"type":"attachment","attachment":{"type":"goal_status","met":true,"condition":"c","reason":"r"}}"#;
        assert_eq!(
            goal_line(line).map(|g| (g.state, g.reason)),
            Some((GoalState::Met, Some("r".into())))
        );
        assert_eq!(goal_line(br#"{"type":"user"}"#), None);
        let tick = |kind: &str, status: &str| {
            json!({"type":kind,"uuid":"u1","origin":{"kind":"task-notification"},"message":{"content":
                format!("<task-notification>\n<summary>Monitor event: \"ticks\"</summary>\n{status}<event>tick 2</event>\n</task-notification>")}})
            .to_string()
        };
        assert_eq!(
            monitor_line(tick("user", "").as_bytes()),
            Some(("u1".into(), "◷ ticks · tick 2".into()))
        );
        assert_eq!(
            monitor_line(tick("user", "<status>completed</status>").as_bytes()),
            None
        );
        assert_eq!(monitor_line(tick("attachment", "").as_bytes()), None);
        assert_eq!(
            monitor_line(br#"{"type":"user","message":{"content":"hi"}}"#),
            None
        );
        assert!(calls_monitor(&AgentEvent::ItemStarted {
            id: "t1".into(),
            kind: ItemKind::ToolCall,
            preview: "Monitor".into(),
            parent: None,
        }));
        assert_eq!(effort_switch(" /effort High ").as_deref(), Some("high"));
        assert_eq!(effort_switch("/effort"), None);
        assert_eq!(effort_switch("/effort a b"), None);
        assert_eq!(model_switch("use the opus model"), None);
        assert!(is_gateway(Some(DEEPSEEK)) && !is_gateway(Some("claude-opus-5-5")));
    }

    #[test]
    fn a_claude_chat_gets_the_artifact_tool_the_others_do_not() {
        let on = |m| artifact_env(Agent::Claude, m);
        assert!(on(None).contains(&("CLAUDE_CODE_ARTIFACT".to_owned(), "1".to_owned())));
        assert!(!on(Some("claude-opus-5-5")).is_empty());
        assert!(on(Some(DEEPSEEK)).is_empty());
        assert!(artifact_env(Agent::Codex, None).is_empty());
        assert!(artifact_env(Agent::Pi, None).is_empty());
        let mut at = None;
        let mut seen = Vec::new();
        for _ in 0..5 {
            let next = next_permission(at);
            seen.push(next);
            at = Some(next);
        }
        assert_eq!(
            seen,
            [
                "default",
                "acceptEdits",
                "plan",
                "auto",
                "bypassPermissions"
            ]
        );
        assert_eq!(next_permission(Some("bypassPermissions")), "default");
        assert_eq!(next_permission(Some("manual")), "acceptEdits");
    }

    #[test]
    fn cuts_land_on_character_boundaries() {
        assert_eq!(cut("héllo", 2), "h");
        assert_eq!(tail_of("éa", 2), "a");
        assert_eq!(tail_of("abc", 10), "abc");
    }

    // ---- the actor, against a holder that only answers ----------------------------------------

    /// A holder for a test: `Hello` gets `info`, every other control request `Ok`, and what it was
    /// asked is kept. Nothing runs in it and its spool stays empty. With `once` it stops
    /// accepting after the first connection, so the next ones (the spool, a pane's view) fail.
    struct FakeHolder {
        info: Arc<Mutex<HolderInfo>>,
        asked: Arc<Mutex<Vec<HolderReq>>>,
    }

    impl FakeHolder {
        fn start(d: &Daemon, sid: Sid, once: bool) -> FakeHolder {
            let path = d.sessions.dirs.socket(sid);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let listener = UnixListener::bind(&path).unwrap();
            let info = Arc::new(Mutex::new(HolderInfo {
                v: HOLDER_VERSION,
                sid,
                epoch: 1,
                started: 0,
                panes: Vec::new(),
                agent: Some(agent(true, None)),
            }));
            let asked = Arc::new(Mutex::new(Vec::new()));
            let (i, a) = (info.clone(), asked.clone());
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(hold(stream, i.clone(), a.clone()));
                    if once {
                        break;
                    }
                }
            });
            FakeHolder { info, asked }
        }

        fn asked(&self) -> Vec<HolderReq> {
            self.asked.lock().unwrap().clone()
        }
    }

    fn agent(alive: bool, exit: Option<i32>) -> HolderAgent {
        HolderAgent {
            pid: 1,
            alive,
            exit,
            spool_lo: 0,
            spool_hi: 0,
        }
    }

    async fn hold(
        stream: UnixStream,
        info: Arc<Mutex<HolderInfo>>,
        asked: Arc<Mutex<Vec<HolderReq>>>,
    ) {
        let mut f = LengthDelimitedCodec::builder()
            .little_endian()
            .length_field_length(4)
            .max_frame_length(codec::MAX_FRAME)
            .new_framed(stream);
        while let Some(Ok(frame)) = f.next().await {
            let Ok(req) = codec::decode::<HolderReq>(&frame) else {
                return;
            };
            let reply = match &req {
                HolderReq::Hello { .. } => Some(HolderResp::Info(info.lock().unwrap().clone())),
                // a pane's view, the spool: the connection stays open, and silent
                HolderReq::Attach { .. } | HolderReq::SpoolRead { .. } | HolderReq::Term(_) => None,
                _ => Some(HolderResp::Ok),
            };
            asked.lock().unwrap().push(req);
            if let Some(r) = reply {
                let sent = f.send(Bytes::from(codec::encode(&r).unwrap())).await;
                if sent.is_err() {
                    return;
                }
            }
        }
    }

    /// A daemon over temp dirs whose only tool is an empty `claude` (the holder is fake: nothing
    /// is run).
    fn daemon() -> (Arc<Daemon>, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix("femmed-u-")
            .tempdir_in("/tmp")
            .unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("claude"), "").unwrap();
        let mut cfg = Config::from_vars([("HOME".to_string(), dir.path().display().to_string())]);
        cfg.state_dir = dir.path().join("state");
        cfg.runtime_dir = dir.path().join("rt");
        cfg.tool_path = Some(bin.display().to_string());
        (Daemon::open(cfg).unwrap(), dir)
    }

    /// The session's row and its (empty) attach state go into the store, as `open_session` does.
    fn insert_row(d: &Daemon, s: &Session) {
        d.write(|tx, _| {
            tx.insert_session(s)?;
            tx.set_agent_state(s.id, 0, &Saved::default().json())
        })
        .unwrap();
    }

    /// An actor for session `SID` in `mode` over a fake holder. The session has no row in the
    /// store until [`Rig::row`].
    struct Rig {
        d: Arc<Daemon>,
        a: Actor,
        fake: FakeHolder,
        mode: Mode,
        _dir: tempfile::TempDir,
    }

    impl Rig {
        async fn new(mode: Mode) -> Rig {
            let (d, dir) = daemon();
            let fake = FakeHolder::start(&d, SID, false);
            let holder = Holder::connect(&d.sessions.dirs, SID).await.unwrap();
            let s = session_row(SID, mode, SessionStatus::Starting);
            let a = Actor::new(&d, &s, holder, 0, 0, Saved::default());
            Rig {
                d,
                a,
                fake,
                mode,
                _dir: dir,
            }
        }

        fn row(&self) {
            let s = session_row(SID, self.mode, SessionStatus::Starting);
            insert_row(&self.d, &s);
        }

        fn item(&self, id: &str) -> Item {
            let got = self.d.store.read(|c| store_items::get_item(c, SID, id));
            got.unwrap().unwrap()
        }
    }

    /// What the CLI prints for a turn that is one assistant message: its lines, newline ended.
    fn turn_lines(mid: &str, text: &str) -> String {
        [
            json!({"type": "assistant", "message": {"id": mid, "content": [{"type": "text", "text": text}]}}),
            json!({"type": "result", "subtype": "success", "stop_reason": "end_turn", "duration_ms": 5}),
        ]
        .iter()
        .map(|l| format!("{l}\n"))
        .collect()
    }

    #[tokio::test]
    async fn a_commit_the_store_refused_is_made_again_with_the_next_one_and_the_offset_waits_for_it(
    ) {
        let mut rig = Rig::new(Mode::Headless).await;
        let (first, second) = (turn_lines("m1", "one"), turn_lines("m2", "two"));
        // The session has no row: the store refuses every write (a foreign key), as a store that
        // fails would.
        rig.a.ingest(&rig.d, 0, Bytes::from(first.clone())).await;
        assert_eq!(
            rig.a.committed, 0,
            "no offset for items that are not stored"
        );
        assert_eq!(
            rig.a.pending_puts.len(),
            2,
            "the message and the end of the turn wait"
        );

        rig.row();
        let at = first.len() as u64;
        rig.a.ingest(&rig.d, at, Bytes::from(second.clone())).await;
        let (items, committed) = rig
            .d
            .store
            .read(|c| {
                Ok((
                    store_items::page_items(c, SID, None, 100)?,
                    sessions::spool_committed(c, SID)?,
                ))
            })
            .unwrap();
        let said: Vec<String> = items
            .iter()
            .rev()
            .filter(|i| i.kind == ItemKind::AssistantMessage)
            .map(|i| {
                body_value(&rig.d.store, i)["text"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(said, ["one", "two"], "the first turn is not lost");
        assert_eq!(
            items.iter().filter(|i| i.kind == ItemKind::TurnEnd).count(),
            2
        );
        assert_eq!(committed, (first.len() + second.len()) as u64);
        assert!(rig.a.pending_puts.is_empty());
    }

    #[tokio::test]
    async fn the_session_row_goes_out_before_the_items_when_work_starts_and_after_them_when_it_ends(
    ) {
        let mut rig = Rig::new(Mode::Headless).await;
        rig.row();
        let mut start = turn(StopReason::EndTurn);
        let end = start.split_off(2);

        rig.a.apply(&rig.d, Src::Line(0), start).await;
        let row = rig.d.session(SID).unwrap();
        assert_eq!(row.status, SessionStatus::Working);
        let message = rig.item("m1");
        assert!(
            row.rev < message.rev,
            "a client sees `working` before the turn's first item: row {} item {}",
            row.rev,
            message.rev
        );

        rig.a.apply(&rig.d, Src::Line(100), end).await;
        let row = rig.d.session(SID).unwrap();
        assert_eq!(
            (row.status, row.attention),
            (SessionStatus::Idle, Attention::Done)
        );
        let last = rig.item("d100-1");
        assert!(
            last.rev < row.rev,
            "and never `done` before its last: item {} row {}",
            last.rev,
            row.rev
        );
    }

    #[tokio::test]
    async fn a_prompt_the_dead_agent_dropped_is_forgotten_when_the_terminal_takes_over() {
        let mut rig = Rig::new(Mode::Headless).await;
        rig.row();
        rig.a.spawn_process(&rig.d, None).await.unwrap();
        // the agent has not answered `initialize`: the driver holds the prompt back
        rig.a
            .prompt(&rig.d, "second", PromptHow::Queue, None)
            .await
            .unwrap();
        assert_eq!(rig.a.driver.held_prompts().len(), 1);
        // it dies: the prompt is dropped and kept for the restart, two seconds away
        rig.a.agent_exited(&rig.d, -9).await;
        let kept: Vec<_> = rig.a.resend.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(kept, ["second"]);
        assert!(rig.a.restart_at.is_some());

        // the terminal takes over before it: there is no next headless process for it
        let (reply, mut answer) = oneshot::channel();
        rig.a.takeover(&rig.d, Mode::Tui, reply).await;
        assert_eq!(answer.try_recv().unwrap(), Ok(()));
        assert!(rig.a.resend.is_empty());
        assert!(rig.a.restart_at.is_none());
        let state = rig.d.store.read(|c| sessions::agent_state(c, SID));
        let saved: Saved = serde_json::from_str(&state.unwrap().unwrap().1).unwrap();
        assert!(
            saved.held.is_empty(),
            "nothing is saved for a restart either"
        );
    }

    #[tokio::test]
    async fn a_lost_exit_of_the_agent_is_taken_from_the_holders_report_when_events_were_missed() {
        use broadcast::error::RecvError;
        let mut rig = Rig::new(Mode::Headless).await;
        rig.row();
        rig.a.spawn_process(&rig.d, None).await.unwrap();
        let (reply, mut answer) = oneshot::channel();
        rig.a.takeover(&rig.d, Mode::Tui, reply).await;
        assert!(matches!(rig.a.machine.phase, Phase::StoppingAgent { .. }));

        // events were missed, but the holder still has the agent: nothing to take
        rig.a.on_holder(&rig.d, Err(RecvError::Lagged(300))).await;
        assert!(matches!(rig.a.machine.phase, Phase::StoppingAgent { .. }));
        // it is gone, and the event that said so was among them
        rig.fake.info.lock().unwrap().agent = Some(agent(false, Some(0)));
        rig.a.on_holder(&rig.d, Err(RecvError::Lagged(300))).await;
        assert_eq!(rig.a.machine.phase, Phase::Tui);
        assert_eq!(
            answer.try_recv().unwrap(),
            Ok(()),
            "the takeover is through"
        );
    }

    #[tokio::test]
    async fn a_lost_exit_of_the_terminal_is_taken_from_the_holders_report_when_events_were_missed()
    {
        use broadcast::error::RecvError;
        let mut rig = Rig::new(Mode::Tui).await;
        rig.row();
        {
            let mut info = rig.fake.info.lock().unwrap();
            info.agent = None;
            info.panes = vec![HolderPane {
                no: MAIN,
                pid: 2,
                alive: false,
                exit: Some(0),
                cols: 80,
                rows: 24,
                title: String::new(),
            }];
        }
        rig.a.on_holder(&rig.d, Err(RecvError::Lagged(300))).await;
        assert_eq!(rig.a.machine.phase, Phase::Headless, "the terminal ended");
        assert!(rig.a.live, "the agent runs headless again");
        assert!(rig
            .fake
            .asked()
            .iter()
            .any(|r| matches!(r, HolderReq::SpawnAgent { .. })));
    }

    #[tokio::test]
    async fn a_headless_session_the_daemon_cannot_reattach_is_marked_interrupted() {
        let (d, _dir) = daemon();
        let (broken, healthy) = (Id(8), Id(9));
        for sid in [broken, healthy] {
            insert_row(
                &d,
                &session_row(sid, Mode::Headless, SessionStatus::Working),
            );
        }
        // the first answers its hello and then refuses connections: its spool cannot be read
        let _first = FakeHolder::start(&d, broken, true);
        let _second = FakeHolder::start(&d, healthy, false);
        d.reconcile().await;

        let row = d.session(broken).unwrap();
        assert_eq!(
            (row.status, row.attention),
            (SessionStatus::Interrupted, Attention::None)
        );
        assert!(d.sessions.head(broken).is_none(), "no actor drives it");
        assert!(
            d.sessions.head(healthy).is_some(),
            "the other is reattached as usual"
        );
        assert_ne!(
            d.session(healthy).unwrap().status,
            SessionStatus::Interrupted
        );
    }

    #[test]
    fn the_screen_shows_what_an_open_item_has_streamed_so_far() {
        let mut open = item(ItemKind::AssistantMessage, ItemStatus::InProgress, "");
        open.item_id = "m1:0".into();
        let items = vec![
            (
                item(ItemKind::UserMessage, ItemStatus::Completed, "hi"),
                json!({"text": "hi"}),
            ),
            (open, Value::Null),
        ];
        let streams = HashMap::from([("m1:0".to_string(), "tick 1\ntick 2".to_string())]);
        assert_eq!(render(&items, &streams), "> hi\ntick 1\ntick 2\n");
    }

    #[test]
    fn a_body_past_the_blob_cap_is_stored_cut_with_its_marker_and_a_replay_leaves_it_as_it_is() {
        let (store, sid) = store_with_session();
        let huge = json!({"text": "x".repeat(store_items::BLOB_MAX + 10_000)});
        let write = || {
            vec![Put::Done(new_item(
                sid,
                "m1".into(),
                ItemKind::AssistantMessage,
                ItemStatus::Completed,
                Some(&huge),
                2,
            ))]
        };
        let stored = apply_puts(&store, sid, write());
        assert_eq!(stored.len(), 1);
        let hash = stored[0].blob_hash.expect("a blob");
        let blob = String::from_utf8(store.get_blob(&hash).unwrap().unwrap()).unwrap();
        assert!(blob.ends_with(store_items::TRUNCATED), "cut, and says so");
        assert!(
            blob.len() <= store_items::BLOB_MAX && blob.len() > store_items::BLOB_MAX - 100,
            "cut at the cap: {} bytes",
            blob.len()
        );
        // Replayed, it is the same text again. (It is written again, as `same` says it must be
        // for a body past the cap: that is not asserted, only that nothing else changes.)
        let again = apply_puts(&store, sid, write());
        assert!(again.iter().all(|i| i.blob_hash == Some(hash)));
        let after = store
            .read(|c| store_items::get_item(c, sid, "m1"))
            .unwrap()
            .unwrap();
        assert_eq!((after.blob_hash, after.ts), (Some(hash), 2));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn claudes_push_call_is_sent_to_the_phone_once() {
        let (base, rx) = crate::notify::tests::stub();
        let n = crate::notify::Notifier::new(Some("owner".into())).with_base(base);
        let call = |id: &str| AgentEvent::ItemUpdated {
            id: id.into(),
            body: json!({"name": "PushNotification", "call_id": id,
                "input": {"message": "tests green", "status": "proactive"}}),
        };
        let mut done = HashMap::new();
        // the call opens without its input: nothing to send yet
        let opened = [AgentEvent::ItemStarted {
            id: "t1".into(),
            kind: ItemKind::ToolCall,
            preview: "PushNotification".into(),
            parent: None,
        }];
        forward_pushes(&n, SID, || "fix-ci".into(), &opened, &mut done, false).await;
        assert!(done.is_empty());

        let mut events = vec![call("t1")];
        forward_pushes(&n, SID, || "fix-ci".into(), &events, &mut done, false).await;
        stamp_pushes(&mut events, &done);
        let got = crate::notify::tests::drain(&rx);
        assert_eq!(got.len(), 1);
        assert_eq!(got["owner"]["title"], "fix-ci");
        assert_eq!(got["owner"]["message"], "tests green");
        let AgentEvent::ItemUpdated { body, .. } = &events[0] else {
            unreachable!()
        };
        assert_eq!(body["push"], "sent");

        // the same call again (its task state, its end): not pushed again, the outcome stays on it
        let mut again = vec![
            call("t1"),
            AgentEvent::ItemCompleted {
                id: "t1".into(),
                kind: ItemKind::ToolCall,
                status: ItemStatus::Completed,
                body: json!({"name": "PushNotification", "call_id": "t1",
                    "input": {"message": "tests green", "status": "proactive"}}),
                parent: None,
            },
        ];
        forward_pushes(&n, SID, || "fix-ci".into(), &again, &mut done, false).await;
        stamp_pushes(&mut again, &done);
        assert!(crate::notify::tests::drain(&rx).is_empty());
        let AgentEvent::ItemCompleted { body, .. } = &again[1] else {
            unreachable!()
        };
        assert_eq!(body["push"], "sent");

        // another tool's call is nobody's push
        let other = [AgentEvent::ItemUpdated {
            id: "t2".into(),
            body: json!({"name": "Bash", "call_id": "t2", "input": {"message": "x"}}),
        }];
        forward_pushes(&n, SID, || "fix-ci".into(), &other, &mut done, false).await;
        assert!(crate::notify::tests::drain(&rx).is_empty());

        // no topic and no phone: the reason is the outcome
        let none = crate::notify::Notifier::new(None);
        let mut events = vec![call("t3")];
        forward_pushes(&none, SID, || "fix-ci".into(), &events, &mut done, false).await;
        stamp_pushes(&mut events, &done);
        let AgentEvent::ItemUpdated { body, .. } = &events[0] else {
            unreachable!()
        };
        assert_eq!(body["push"], "no phone registered");
    }

    #[test]
    fn a_push_is_titled_by_the_sessions_label_or_its_folder() {
        let mut s = session_row(SID, Mode::Headless, SessionStatus::Working);
        s.cwd = "/home/u/femme".into();
        assert_eq!(push_title(&s), "femme");
        s.label = Some("fix-ci".into());
        assert_eq!(push_title(&s), "fix-ci");
    }
}
