//! Claude Code driver: `claude -p --input-format stream-json --output-format stream-json`.
//!
//! The protocol was verified against claude 2.1.302 (see `NOTES.md` and `fixtures/claude/`).
//! Short version:
//!
//! * stdin: one JSON per line. User turns `{"type":"user","message":{..},"uuid":U}` and control
//!   requests `{"type":"control_request","request_id":R,"request":{"subtype":..}}` (`initialize`,
//!   `interrupt`, `set_model`), plus `control_response`s to the CLI's own `can_use_tool` requests.
//! * stdout: `system` (init, status, hooks, tasks...), `stream_event` (partial messages),
//!   `assistant`, `user` (tool results, and with `--replay-user-messages` an echo of everything
//!   the host sent, including our `control_response`s), `result` (end of turn),
//!   `rate_limit_event`, `command_lifecycle` (per-uuid queued/started/completed/cancelled),
//!   `control_request{can_use_tool}` (with `--permission-prompt-tool stdio`; under bypass only for
//!   `AskUserQuestion` and `ExitPlanMode`), `control_request{elicitation}` and
//!   `control_cancel_request`. Any other `control_request` gets an error reply at once (the CLI
//!   would wait for one forever) and an Event item, once per subtype.
//!
//! Mapping decisions worth knowing before reading the code:
//!
//! * A turn opens on the first sign of work (lifecycle `started`, `system/status`, any message
//!   of the main thread) and closes on `result`. A `result` without an open turn and without
//!   turns (`num_turns == 0`, e.g. the empty one a resumed session emits for a background-task
//!   notification) is bookkeeping and ignored.
//! * `AskUserQuestion` becomes `Question` items, not a tool call: id `<tool_use_id>` (or
//!   `<tool_use_id>#<i>` when it asks several questions at once; the daemon sees one
//!   `QuestionAsked` per question and the CLI gets one `control_response` once all are answered).
//!   `ExitPlanMode` becomes a `Plan` item `<tool_use_id>` plus, when the CLI really asks (plan mode),
//!   a `Question` item `<tool_use_id>:ask` (Approve / Keep planning). Any other `can_use_tool`
//!   (only after a plan-mode approval returned the session to `default`) is a `Question` item
//!   `<tool_use_id>:ask` (Allow / Deny): the driver never answers on its own.
//! * An MCP server's `elicitation` (form or URL mode) is a `Question` item `<request_id>:elicit`
//!   (Accept / Decline, no free text; Accept sends the form with only its schema's defaults). It
//!   has no tool call: the CLI's echo of our answer ends it `Answered`. A URL one the server
//!   reports complete (`system/elicitation_complete`) while still open is accepted then, the one
//!   time the driver answers on its own: the CLI would wait for us forever.
//! * Subagent traffic (`parent_tool_use_id` set, the id of the `Agent` call that spawned it) goes
//!   through the same handlers with its own stream state, and its items carry that id as
//!   `parent`. It never opens or closes a turn nor changes the status, and its AskUserQuestion or
//!   ExitPlanMode calls are plain tool calls. Ids are the message/tool/line ids as for the main
//!   thread; a spool-offset id (`o<offset>`, no id on the line) becomes `<parent>/o<offset>`.
//! * Thinking items exist only when the CLI sends readable thinking text (it normally sends
//!   none, only a signature and a token estimate).
//! * Context: every model call of the main thread says what it read (`usage` of its
//!   `message_start` and of its `assistant` line: input + cache read + cache write), which is
//!   [`AgentEvent::Context`], said again only when the number changes; a subagent's calls are its
//!   own conversation and say nothing. The window comes with the turn's `result`
//!   (`modelUsage[<model>].contextWindow`; the result's own `usage` is the turn's total over all
//!   its calls, not a context size). `system/compact_boundary` is the "Conversation compacted"
//!   event item and [`AgentEvent::Compacted`].
//! * Prompts: every user message gets a fresh `uuid`; the CLI echoes it (`isReplay`) when it starts
//!   working on the message, and that echo is what becomes the `UserMessage` item (id = the uuid).
//! * Commands: the CLI reports every user message as a `command_lifecycle` `queued`, `started`,
//!   `completed` (or `cancelled`/`discarded`/`refused`). While a command is unstarted its `queued`
//!   frame stays open in the resume offset, so a restart replays it and the new driver still
//!   knows the CLI is busy. `session_state_changed idle` means nothing is running or queued: it
//!   clears commands the CLI reported and never ended. A message of ours that is lost this way,
//!   cancelled by the CLI, or still unstarted when the process exits, is announced with
//!   `PromptDropped`. `session_state_changed requires_action` (the CLI waits for someone) is
//!   `Blocked` until its next state, also when no request of ours says what it waits for.
//! * Questions after a restart: the answers the daemon persisted ([`Driver::pending_answers`],
//!   [`crate::Attach::answers`]) are applied to the replayed `can_use_tool`. A request whose answer
//!   is complete is not asked again (no `QuestionAsked`, never `Blocked`), one that is part way
//!   (several questions, some answered) asks only the rest.
//! * Outcomes of a question item: `Answered` with the user's own answer (also a deliberate
//!   Cancel, Deny or "Keep planning"), or `Failed` with a `reason` when it was never answered
//!   because the turn was interrupted, the process exited or the CLI withdrew the request. A
//!   `control_cancel_request` fails the request's items at once ("withdrawn by Claude"), not when
//!   the call's result or the turn's end comes.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::{Agent, Goal, GoalState, ItemKind, ItemStatus, Question, Usage};
use serde_json::{json, Value};

use crate::common::{
    drop_held, held, hold, parse, preview, raw_text, set_status, uuid_v4, Offsets,
};
use crate::items::{
    body, ArtifactLink, AssistantMessage, Event, PeerMessage, Plan, QuestionItem, TaskEnd,
    TaskState, Thinking, ToolCall, ToolResult, UserMessage,
};
use crate::{
    AgentEvent, AgentStatus, Attach, Driver, DropReason, Error, HeldPrompt, Launch, Out, Spawn,
    StopReason,
};

/// `request_id` of the `initialize` control request [`ClaudeDriver::on_start`] sends.
const INIT_ID: &str = "femme-init";
/// Tool that asks the user questions.
const ASK: &str = "AskUserQuestion";
/// Tool that proposes a plan for approval.
const PLAN: &str = "ExitPlanMode";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    Tool,
}

/// A streamed content block that has started and not stopped.
#[derive(Debug)]
struct Block {
    id: String,
    kind: BlockKind,
    name: String,
    /// Text so far, or the partial JSON of a tool input.
    text: String,
    /// Whether `ItemStarted` was emitted (thinking starts lazily, hidden tools never).
    started: bool,
}

/// Stream state of one thread: the main one lives in [`ClaudeDriver`]'s fields, each subagent's
/// in [`ClaudeDriver::subs`] and is swapped in while one of its lines is handled.
#[derive(Debug, Default)]
struct Thread {
    msg: Option<String>,
    blocks: BTreeMap<u64, Block>,
    streamed: BTreeSet<String>,
    ord: BTreeMap<String, usize>,
    calls: BTreeMap<String, Call>,
}

impl Thread {
    /// Nothing streams or waits for a result.
    fn idle(&self) -> bool {
        self.blocks.is_empty() && self.calls.is_empty()
    }
}

/// A tool call whose result has not arrived.
#[derive(Debug)]
struct Call {
    name: String,
    input: Value,
}

/// One question shown to the user.
#[derive(Debug)]
struct Part {
    request_id: String,
    item_id: String,
    q: Question,
    /// The AskUserQuestion question text (key of the `answers` map).
    text: String,
    /// Labels the CLI expects back, same order as `q.options`.
    labels: Vec<String>,
    answer: Option<Answer>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendKind {
    /// AskUserQuestion: answers go back as labels in `updatedInput.answers`.
    Ask,
    /// ExitPlanMode or an ordinary tool permission: option 0 allows, anything else denies.
    Allow,
    /// An MCP server's `elicitation`: Accept (0), Decline (1) or Cancel go back as its `action`.
    /// It has no tool call: the item ends when the CLI echoes our answer.
    Elicit,
}

/// A user message the CLI has not finished (an entry of [`ClaudeDriver::cmds`]).
#[derive(Debug, Default)]
struct Cmd {
    /// Order of first sight, so the lost ones are announced oldest first.
    n: u64,
    /// The prompt, while it is ours and the CLI has not started it: what a dead process or a
    /// discarded command takes with it.
    ours: Option<HeldPrompt>,
    /// The CLI reported the command (`queued`/`started`), so it will report its end too. A message
    /// of ours the CLI has not mentioned yet is in flight and is never cleared as stale.
    acked: bool,
}

/// One `can_use_tool` control request waiting for the user.
#[derive(Debug)]
struct Pending {
    kind: PendKind,
    tool_use_id: String,
    input: Value,
    parts: Vec<Part>,
    /// We wrote the `control_response`.
    sent: bool,
    /// `QuestionResolved` was emitted.
    resolved: bool,
    /// The CLI withdrew it (`control_cancel_request`): its items already failed, the entry only
    /// swallows the call's late result.
    closed: bool,
}

/// Driver for `claude` in stream-json mode. One instance per agent process; after a daemon
/// restart make a fresh one and replay the spool from the persisted `resume_offset` (no
/// [`Driver::on_start`]).
#[derive(Debug)]
pub struct ClaudeDriver {
    /// Session id given to a new conversation (`--session-id`); only exposed after `on_start`.
    planned: String,
    sid: Option<String>,
    announced: Option<String>,
    model: Option<String>,
    /// The permission mode the CLI last reported.
    permission: Option<String>,
    /// Tokens the main thread's latest model call read, once a call said.
    ctx_used: Option<u64>,
    awaiting_init: bool,
    /// `on_exit` ran: nothing more is written.
    exited: bool,
    /// Reads a TUI's transcript file (a herdr pane), not a stream-json process: nobody sends
    /// `control_request`s, so an `AskUserQuestion` tool call on the main thread is the question,
    /// and the file's `tool_use_result` says how the user answered in the TUI. `ExitPlanMode` is
    /// only its Plan item: the pane's approval dialog is asked from the screen.
    mirror: bool,
    seq: u64,
    off: Offsets,
    status: AgentStatus,
    turn: bool,
    /// User messages queued or running, by uuid (ours when written, the CLI's from lifecycle frames).
    cmds: BTreeMap<String, Cmd>,
    cmd_n: u64,
    /// Prompts held back until the turn ends or the handshake finishes.
    queue: VecDeque<HeldPrompt>,
    /// Answers the daemon persisted before a restart, applied when the question is replayed.
    restored: BTreeMap<String, Answer>,
    msg: Option<String>,
    blocks: BTreeMap<u64, Block>,
    /// Message ids whose content arrived through `stream_event`s.
    streamed: BTreeSet<String>,
    /// Blocks seen per message id in `assistant` lines of non-streamed messages.
    ord: BTreeMap<String, usize>,
    calls: BTreeMap<String, Call>,
    pending: BTreeMap<String, Pending>,
    rl: BTreeMap<String, (i64, Option<i64>)>,
    /// The subagent whose line is being handled (its `Agent` tool call id); `None` for the main
    /// thread.
    parent: Option<String>,
    /// Stream state of the other subagents, by `Agent` tool call id.
    subs: BTreeMap<String, Thread>,
    /// Tasks the CLI reported (`task_started`) and has not ended, by task id.
    tasks: BTreeMap<String, Task>,
    /// The CLI's last `session_state_changed` was `requires_action`: it waits for someone, also
    /// when no request of ours says what for (`Blocked` until its next state).
    waits: bool,
    /// A request of ours was opened since that `requires_action`.
    waits_ours: bool,
    /// A wait without a request of ours was logged (once per driver).
    waits_logged: bool,
    /// `control_request` subtypes we refused, each said once as an Event item.
    refused: BTreeSet<String>,
}

/// A running task (a Bash command or a subagent) of a tool call.
#[derive(Debug)]
struct Task {
    /// The tool call's id.
    call: String,
    background: bool,
}

impl Default for ClaudeDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeDriver {
    /// A driver for a new or resumed session (decided by [`Launch::resume`] in `spawn`).
    pub fn new() -> Self {
        Self {
            planned: uuid_v4(),
            sid: None,
            announced: None,
            model: None,
            permission: None,
            ctx_used: None,
            awaiting_init: false,
            exited: false,
            mirror: false,
            seq: 0,
            off: Offsets::default(),
            status: AgentStatus::Idle,
            turn: false,
            cmds: BTreeMap::new(),
            cmd_n: 0,
            queue: VecDeque::new(),
            restored: BTreeMap::new(),
            msg: None,
            blocks: BTreeMap::new(),
            streamed: BTreeSet::new(),
            ord: BTreeMap::new(),
            calls: BTreeMap::new(),
            pending: BTreeMap::new(),
            rl: BTreeMap::new(),
            parent: None,
            subs: BTreeMap::new(),
            tasks: BTreeMap::new(),
            waits: false,
            waits_ours: false,
            waits_logged: false,
            refused: BTreeSet::new(),
        }
    }

    /// Where the call's task runs, if it has one.
    fn task_of(&self, call: &str) -> Option<TaskState> {
        self.tasks.values().find(|t| t.call == call).map(|t| {
            if t.background {
                TaskState::Background
            } else {
                TaskState::Foreground
            }
        })
    }

    /// The call `id` with its current `task`, from whichever thread holds it, for an `ItemUpdated`.
    fn task_body(&self, id: &str) -> Option<Value> {
        let c = self
            .calls
            .get(id)
            .or_else(|| self.subs.values().find_map(|t| t.calls.get(id)))?;
        Some(body(&ToolCall {
            name: c.name.clone(),
            input: c.input.clone(),
            call_id: id.to_owned(),
            task: self.task_of(id),
        }))
    }

    /// A task began, or moved to the background: the call's row learns it.
    fn task_set(
        &mut self,
        task_id: &str,
        call: Option<&str>,
        background: bool,
        ev: &mut Vec<AgentEvent>,
    ) {
        let call = match (self.tasks.get_mut(task_id), call) {
            (Some(t), _) => {
                if t.background == background {
                    return;
                }
                t.background = background;
                t.call.clone()
            }
            (None, Some(call)) => {
                self.tasks.insert(
                    task_id.to_owned(),
                    Task {
                        call: call.to_owned(),
                        background,
                    },
                );
                call.to_owned()
            }
            (None, None) => return, // a task we never saw start
        };
        if let Some(body) = self.task_body(&call) {
            ev.push(AgentEvent::ItemUpdated { id: call, body });
        }
    }

    /// A driver for a TUI's transcript file (see `mirror`).
    pub fn mirror() -> Self {
        Self {
            mirror: true,
            ..Self::new()
        }
    }

    /// Exchanges the current thread's stream state with `t`.
    fn swap_thread(&mut self, t: &mut Thread) {
        std::mem::swap(&mut self.msg, &mut t.msg);
        std::mem::swap(&mut self.blocks, &mut t.blocks);
        std::mem::swap(&mut self.streamed, &mut t.streamed);
        std::mem::swap(&mut self.ord, &mut t.ord);
        std::mem::swap(&mut self.calls, &mut t.calls);
    }

    /// Runs `f` as the subagent `pid`: its stream state swapped in, and the items it emits tagged
    /// with `parent` (all but `Question`s, which are the main thread's).
    fn in_sub(
        &mut self,
        pid: &str,
        ev: &mut Vec<AgentEvent>,
        f: impl FnOnce(&mut Self, &mut Vec<AgentEvent>),
    ) {
        let mut t = self.subs.remove(pid).unwrap_or_default();
        self.swap_thread(&mut t);
        self.parent = Some(pid.to_owned());
        let from = ev.len();
        f(self, ev);
        self.parent = None;
        self.swap_thread(&mut t);
        self.subs.insert(pid.to_owned(), t);
        for e in &mut ev[from..] {
            match e {
                AgentEvent::ItemStarted { id, parent, .. } => {
                    scope(pid, id);
                    *parent = Some(pid.to_owned());
                }
                AgentEvent::ItemCompleted {
                    id, kind, parent, ..
                } if *kind != ItemKind::Question => {
                    scope(pid, id);
                    *parent = Some(pid.to_owned());
                }
                AgentEvent::ItemDelta { id, .. } | AgentEvent::ItemUpdated { id, .. } => {
                    scope(pid, id)
                }
                _ => {}
            }
        }
    }

    /// The subagent `pid` will say no more: what it left open fails, its state goes.
    fn close_sub(&mut self, pid: &str, why: &str, ev: &mut Vec<AgentEvent>) {
        if self.subs.contains_key(pid) {
            self.in_sub(pid, ev, |d, ev| d.close_open(why, ev));
            self.subs.remove(pid);
        }
    }

    /// Whether the tool `name` is shown as Question/Plan items instead of a ToolCall: only on the
    /// main thread.
    fn hides(&self, name: &str) -> bool {
        self.parent.is_none() && hidden(name)
    }

    /// The command `u`, created on first sight.
    fn cmd(&mut self, u: &str) -> &mut Cmd {
        let n = self.cmd_n;
        let c = self.cmds.entry(u.to_owned()).or_insert_with(|| Cmd {
            n,
            ..Cmd::default()
        });
        if c.n == n {
            self.cmd_n += 1;
        }
        c
    }

    /// The user message for `h`, under its own id when it has one; it becomes a command of ours
    /// until the CLI starts it.
    fn user_line(&mut self, h: HeldPrompt) -> Value {
        let uuid = h.id.clone().unwrap_or_else(uuid_v4);
        let line = json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":h.text}]},"uuid":uuid});
        self.cmd(&uuid).ours = Some(h);
        line
    }

    /// [`Driver::prompt`] of a prompt already stamped.
    fn send(&mut self, h: HeldPrompt, out: &mut Out) -> Result<(), Error> {
        if self.exited {
            return Err(Error::State("claude has exited"));
        }

        match h.how {
            // Written mid-turn the CLI folds a message into the running turn at the next tool
            // boundary (and runs it as its own turn only when there is none), so a real "after
            // this turn" needs the driver to hold it back.
            PromptHow::Queue if self.busy() || !self.queue.is_empty() => hold(&mut self.queue, h),
            PromptHow::Steer if self.awaiting_init => hold(&mut self.queue, h),
            PromptHow::InterruptThenSend if self.busy() => {
                // ponytail: with a message accepted but its turn not started yet the interrupt
                // would hit nothing, so it is only sent once a turn runs; the caller can repeat it
                if self.turn {
                    self.control("int", json!({"subtype":"interrupt"}), out);
                }
                hold(&mut self.queue, h);
            }
            _ => {
                let l = self.user_line(h);
                out.line(&l);
            }
        }
        Ok(())
    }

    /// The command `u` is over. `lost` is why when it never ran, which is announced for a prompt
    /// of ours (a command that started has dropped its `ours`).
    fn cmd_end(&mut self, u: &str, lost: Option<DropReason>, ev: &mut Vec<AgentEvent>) {
        self.off.item_close(&cmd_anchor(u));
        if let (Some(Cmd { ours: Some(h), .. }), Some(reason)) = (self.cmds.remove(u), lost) {
            ev.push(AgentEvent::PromptDropped {
                text: h.text,
                how: h.how,
                reason,
                id: h.id,
            });
        }
    }

    /// End every command, or with `only_acked` just those the CLI reported (a message of ours it
    /// has not mentioned yet is in flight), oldest first.
    fn end_cmds(&mut self, only_acked: bool, lost: Option<DropReason>, ev: &mut Vec<AgentEvent>) {
        let mut gone: Vec<(u64, String)> = self
            .cmds
            .iter()
            .filter(|(_, c)| c.acked || !only_acked)
            .map(|(u, c)| (c.n, u.clone()))
            .collect();
        gone.sort();
        for (_, u) in gone {
            self.cmd_end(&u, lost.clone(), ev);
        }
    }

    fn control(&mut self, what: &str, request: Value, out: &mut Out) {
        self.seq += 1;
        out.line(&json!({"type":"control_request","request_id":format!("femme-{what}-{}", self.seq),"request":request}));
    }

    /// The CLI is not ready for a message that must run after everything before it: still
    /// starting, running a turn, or holding a message of ours it has not finished yet (between
    /// our write and its `started` frame there is no turn, but there is a `cmds` entry).
    fn busy(&self) -> bool {
        self.awaiting_init || self.turn || !self.cmds.is_empty()
    }

    /// Write the next held-back prompt if nothing is running. Called after every stdout line.
    fn flush_one(&mut self, out: &mut Out) {
        if !self.busy() {
            if let Some(h) = self.queue.pop_front() {
                let l = self.user_line(h);
                out.line(&l);
            }
        }
    }

    fn activity(&mut self, offset: u64, ev: &mut Vec<AgentEvent>) {
        if !self.turn && self.parent.is_none() {
            self.turn = true;
            self.off.turn_begin(offset);
            ev.push(AgentEvent::TurnStarted);
            set_status(&mut self.status, AgentStatus::Working, ev);
        }
    }

    /// Status follows from state: blocked while a question is unanswered or the CLI says it waits
    /// for action, working while a turn or a queued message exists, else idle. Called at the end
    /// of every line.
    fn sync(&mut self, ev: &mut Vec<AgentEvent>) {
        let want = if self.waits || self.pending.values().any(|p| !p.resolved) {
            AgentStatus::Blocked
        } else if self.turn || !self.cmds.is_empty() {
            AgentStatus::Working
        } else {
            AgentStatus::Idle
        };
        set_status(&mut self.status, want, ev);
    }

    // ---- stdout handlers ------------------------------------------------------------------

    fn system(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        match v["subtype"].as_str().unwrap_or("") {
            "init" => self.init(v, ev),
            "status" => {
                if v["status"].as_str().is_some_and(|s| !s.is_empty()) {
                    self.activity(offset, ev);
                }
                self.permission_mode(v, ev);
            }
            "task_started" => self.task_set(
                v["task_id"].as_str().unwrap_or(""),
                v["tool_use_id"].as_str(),
                v["is_backgrounded"].as_bool().unwrap_or(false),
                ev,
            ),
            "task_updated" => {
                if let Some(bg) = v["patch"]["is_backgrounded"].as_bool() {
                    self.task_set(v["task_id"].as_str().unwrap_or(""), None, bg, ev);
                }
            }
            // the list of tasks running in the background
            "background_tasks_changed" => {
                let ids: Vec<String> = v["tasks"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| t["task_id"].as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default();
                for id in ids {
                    self.task_set(&id, None, true, ev);
                }
            }
            "task_notification" => {
                if let Some(id) = v["task_id"].as_str() {
                    self.tasks.remove(id);
                }
                ev.push(task_end(item_id(v, offset), v));
            }
            "compact_boundary" => {
                ev.push(event_item(item_id(v, offset), "Conversation compacted"));
                // the stream says `compact_metadata`, the transcript on disk `compactMetadata`
                ev.push(AgentEvent::Compacted {
                    pre_tokens: v["compact_metadata"]["pre_tokens"]
                        .as_u64()
                        .or_else(|| v["compactMetadata"]["preTokens"].as_u64()),
                });
            }
            "session_state_changed" => {
                let state = v["state"].as_str().unwrap_or("");
                self.cli_state(state);
                // nothing is running and nothing is queued: a command the CLI reported and never
                // ended (it has no terminal frame, or ours was dropped silently) would keep `busy()`
                // true forever
                if state == "idle" {
                    self.end_cmds(
                        true,
                        Some(DropReason::Refused(
                            "claude went idle without running it".into(),
                        )),
                        ev,
                    );
                }
            }
            // hooks, tasks, thinking_tokens, other session states, ...: nothing the transcript needs
            _ => {}
        }
    }

    /// The CLI's session state. `requires_action` comes just before each request it sends us
    /// (`can_use_tool`, `elicitation`), and also for a wait no request of ours shows (a dialog kind
    /// we never declared): `Blocked` either way, until the next state. A wait that ended without a
    /// request of ours is logged, once.
    fn cli_state(&mut self, state: &str) {
        if self.waits && !self.waits_ours && !self.waits_logged {
            self.waits_logged = true;
            tracing::warn!(
                session = self.sid.as_deref().unwrap_or(""),
                "claude waited for action without asking FEMME anything"
            );
        }
        self.waits = state == "requires_action";
        self.waits_ours = false;
    }

    fn init(&mut self, v: &Value, ev: &mut Vec<AgentEvent>) {
        let model = v["model"].as_str().map(str::to_owned);
        let mut started = false;
        if let Some(sid) = v["session_id"].as_str() {
            self.sid = Some(sid.to_owned());
            if self.announced.as_deref() != Some(sid) {
                self.announced = Some(sid.to_owned());
                ev.push(AgentEvent::Started {
                    agent_sid: sid.to_owned(),
                    model: model.clone(),
                });
                started = true;
            }
        }
        if let Some(m) = model {
            if !started && self.model.as_deref() != Some(m.as_str()) {
                ev.push(AgentEvent::ModelChanged(m.clone()));
            }
            self.model = Some(m);
        }
        if let Some(all) = v["slash_commands"].as_array() {
            // the TUI's own commands do nothing under `-p`
            let tui = &v["terminal_slash_commands"];
            let names = all
                .iter()
                .filter_map(Value::as_str)
                .filter(|n| !tui.as_array().is_some_and(|t| t.iter().any(|t| t == n)))
                .map(str::to_owned)
                .collect();
            ev.push(AgentEvent::Commands(names));
        }
        self.permission_mode(v, ev);
    }

    /// A line that carries `permissionMode` (init, status) says the mode when it changed.
    fn permission_mode(&mut self, v: &Value, ev: &mut Vec<AgentEvent>) {
        if let Some(m) = v["permissionMode"].as_str() {
            if self.permission.as_deref() != Some(m) {
                self.permission = Some(m.to_owned());
                ev.push(AgentEvent::PermissionMode(m.to_owned()));
            }
        }
    }

    /// The main thread's latest model call read what `usage` says. A message says it more than
    /// once (its `message_start`, then an `assistant` line per content block, all with the same
    /// number): the repeats are not news. A subagent's calls are its own conversation, not the
    /// session's context.
    fn context(&mut self, usage: &Value, ev: &mut Vec<AgentEvent>) {
        if self.parent.is_some() {
            return;
        }
        if let Some(used) = context_of(usage).filter(|u| self.ctx_used != Some(*u)) {
            self.ctx_used = Some(used);
            ev.push(AgentEvent::Context {
                used,
                window: known_window(self.model.as_deref()),
            });
        }
    }

    /// The window of the session's model, from a `result`'s `modelUsage` (one entry per model the
    /// turn used): our model's entry, else the first.
    fn window_of(&self, model_usage: &Value) -> Option<u64> {
        let models = model_usage.as_object()?;
        let ours = self.model.as_deref().map(base_model);
        let entry = ours
            .and_then(|m| models.iter().find(|(k, _)| base_model(k) == m))
            .or_else(|| models.iter().next())?
            .1;
        entry["contextWindow"].as_u64().filter(|w| *w > 0)
    }

    fn stream_event(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        self.activity(offset, ev);
        let e = &v["event"];
        match e["type"].as_str().unwrap_or("") {
            "message_start" => {
                self.context(&e["message"]["usage"], ev);
                self.close_blocks(ev);
                let id = e["message"]["id"]
                    .as_str()
                    .or(v["api_message_id"].as_str())
                    .map_or_else(|| format!("o{offset}"), str::to_owned);
                self.streamed.insert(id.clone());
                self.msg = Some(id);
            }
            "content_block_start" => self.block_start(offset, e, ev),
            "content_block_delta" => self.block_delta(offset, e, ev),
            "content_block_stop" => self.block_stop(e, ev),
            _ => {} // message_delta, message_stop, ping
        }
    }

    fn block_start(&mut self, offset: u64, e: &Value, ev: &mut Vec<AgentEvent>) {
        let idx = e["index"].as_u64().unwrap_or(0);
        let cb = &e["content_block"];
        let msg = self.msg.clone().unwrap_or_else(|| format!("o{offset}"));
        let (id, kind, name) = match cb["type"].as_str() {
            Some("text") => (format!("{msg}:{idx}"), BlockKind::Text, String::new()),
            Some("thinking") => (format!("{msg}:{idx}"), BlockKind::Thinking, String::new()),
            Some("tool_use" | "server_tool_use" | "mcp_tool_use") => (
                cb["id"]
                    .as_str()
                    .map_or_else(|| format!("{msg}:{idx}"), str::to_owned),
                BlockKind::Tool,
                cb["name"].as_str().unwrap_or("tool").to_owned(),
            ),
            _ => return, // redacted thinking, server tool results, ...
        };
        // a second start at an index nobody stopped: the first block is over, or its open item
        // would pin the resume offset forever
        if let Some(prev) = self.blocks.remove(&idx) {
            self.fail_block(prev, ev);
        }
        let mut started = false;
        if kind == BlockKind::Text {
            started = true;
            ev.push(AgentEvent::ItemStarted {
                id: id.clone(),
                kind: ItemKind::AssistantMessage,
                preview: String::new(),
                parent: None,
            });
            self.off.item_open(&id, offset);
        } else if kind == BlockKind::Tool && !self.hides(&name) {
            started = true;
            ev.push(AgentEvent::ItemStarted {
                id: id.clone(),
                kind: ItemKind::ToolCall,
                preview: preview(&name),
                parent: None,
            });
            self.off.item_open(&id, offset);
        }
        self.blocks.insert(
            idx,
            Block {
                id,
                kind,
                name,
                text: String::new(),
                started,
            },
        );
    }

    fn block_delta(&mut self, offset: u64, e: &Value, ev: &mut Vec<AgentEvent>) {
        let Some(b) = self.blocks.get_mut(&e["index"].as_u64().unwrap_or(0)) else {
            return;
        };
        let d = &e["delta"];
        match (b.kind, d["type"].as_str()) {
            (BlockKind::Text, Some("text_delta")) => {
                let t = d["text"].as_str().unwrap_or("");
                if !t.is_empty() {
                    b.text.push_str(t);
                    ev.push(AgentEvent::ItemDelta {
                        id: b.id.clone(),
                        text: t.to_owned(),
                    });
                }
            }
            (BlockKind::Thinking, Some("thinking_delta")) => {
                let t = d["thinking"].as_str().unwrap_or("");
                if t.is_empty() {
                    return;
                }
                if !b.started {
                    b.started = true;
                    ev.push(AgentEvent::ItemStarted {
                        id: b.id.clone(),
                        kind: ItemKind::Thinking,
                        preview: String::new(),
                        parent: None,
                    });
                    self.off.item_open(&b.id, offset);
                }
                b.text.push_str(t);
                ev.push(AgentEvent::ItemDelta {
                    id: b.id.clone(),
                    text: t.to_owned(),
                });
            }
            (BlockKind::Tool, Some("input_json_delta")) => {
                b.text.push_str(d["partial_json"].as_str().unwrap_or(""))
            }
            _ => {}
        }
    }

    fn block_stop(&mut self, e: &Value, ev: &mut Vec<AgentEvent>) {
        let Some(b) = self.blocks.remove(&e["index"].as_u64().unwrap_or(0)) else {
            return;
        };
        match b.kind {
            BlockKind::Text => {
                self.off.item_close(&b.id);
                ev.push(done(
                    b.id,
                    ItemKind::AssistantMessage,
                    ItemStatus::Completed,
                    body(&AssistantMessage { text: b.text }),
                ));
            }
            BlockKind::Thinking if b.started => {
                self.off.item_close(&b.id);
                ev.push(done(
                    b.id,
                    ItemKind::Thinking,
                    ItemStatus::Completed,
                    body(&Thinking { text: b.text }),
                ));
            }
            BlockKind::Tool if !self.calls.contains_key(&b.id) => {
                // normally the `assistant` line (which carries the CLI's final input) came first
                let input = if b.text.is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&b.text).unwrap_or(Value::String(b.text))
                };
                self.tool_ready(b.id, b.name, input, ev);
            }
            _ => {}
        }
    }

    /// The final input of a tool call is known.
    fn tool_ready(&mut self, id: String, name: String, input: Value, ev: &mut Vec<AgentEvent>) {
        let shown = if self.hides(&name) { name.as_str() } else { "" };
        match shown {
            PLAN => {
                let text = input["plan"].as_str().unwrap_or("").to_owned();
                ev.push(done(
                    id.clone(),
                    ItemKind::Plan,
                    ItemStatus::Completed,
                    body(&Plan { text }),
                ));
                // a mirror asks nothing here: the pane's own "Would you like to proceed?" dialog is
                // read from its screen (femmed's herdr side), with the TUI's real options
            }
            ASK => {
                if self.mirror {
                    let at = self.off.open_at(&id);
                    self.ask(at, &id, &id, ASK, &input, None, ev);
                }
            }
            _ => ev.push(AgentEvent::ItemUpdated {
                id: id.clone(),
                body: body(&ToolCall {
                    name: name.clone(),
                    input: input.clone(),
                    call_id: id.clone(),
                    task: self.task_of(&id),
                }),
            }),
        }
        self.calls.insert(id, Call { name, input });
    }

    /// Close streamed blocks that never saw `content_block_stop` (aborted stream).
    fn close_blocks(&mut self, ev: &mut Vec<AgentEvent>) {
        for (_, b) in std::mem::take(&mut self.blocks) {
            self.fail_block(b, ev);
        }
    }

    /// A streamed block that will never stop: its item is final, `Failed`.
    fn fail_block(&mut self, b: Block, ev: &mut Vec<AgentEvent>) {
        if !b.started {
            return;
        }
        self.off.item_close(&b.id);
        let (kind, body) = match b.kind {
            BlockKind::Text => (
                ItemKind::AssistantMessage,
                body(&AssistantMessage { text: b.text }),
            ),
            BlockKind::Thinking => (ItemKind::Thinking, body(&Thinking { text: b.text })),
            BlockKind::Tool => (
                ItemKind::ToolCall,
                body(&ToolCall {
                    name: b.name,
                    input: Value::Null,
                    call_id: b.id.clone(),
                    task: None,
                }),
            ),
        };
        ev.push(done(b.id, kind, ItemStatus::Failed, body));
    }

    fn assistant(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        self.activity(offset, ev);
        let m = &v["message"];
        // a transcript read from disk has no `init`: the message says the model
        if self.parent.is_none() && self.model.is_none() {
            if let Some(model) = m["model"].as_str() {
                self.model = Some(model.to_owned());
                ev.push(AgentEvent::ModelChanged(model.to_owned()));
            }
        }
        self.context(&m["usage"], ev);
        let mid = m["id"]
            .as_str()
            .map_or_else(|| format!("o{offset}"), str::to_owned);
        let streamed = self.streamed.contains(&mid);
        let content = m["content"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let first = *self.ord.entry(mid.clone()).or_default();
        self.ord.insert(mid.clone(), first + content.len());
        for (i, b) in content.iter().enumerate() {
            let bid = format!("{mid}:{}", first + i);
            match b["type"].as_str() {
                Some("tool_use" | "server_tool_use" | "mcp_tool_use") => {
                    let id = b["id"].as_str().map_or(bid, str::to_owned);
                    let name = b["name"].as_str().unwrap_or("tool").to_owned();
                    let input = b["input"].clone();
                    if !streamed && !self.hides(&name) {
                        ev.push(AgentEvent::ItemStarted {
                            id: id.clone(),
                            kind: ItemKind::ToolCall,
                            preview: preview(&name),
                            parent: None,
                        });
                        self.off.item_open(&id, offset);
                    }
                    // the line carries the input as the CLI finally sent it (ExitPlanMode gains
                    // its plan here), which can differ from the streamed one
                    if self.calls.get(&id).map(|c| &c.input) != Some(&input) {
                        self.tool_ready(id, name, input, ev);
                    }
                }
                Some("text") if !streamed => {
                    let text = b["text"].as_str().unwrap_or("").to_owned();
                    ev.push(done(
                        bid,
                        ItemKind::AssistantMessage,
                        ItemStatus::Completed,
                        body(&AssistantMessage { text }),
                    ));
                }
                Some("thinking") if !streamed => {
                    if let Some(text) = b["thinking"].as_str().filter(|t| !t.is_empty()) {
                        ev.push(done(
                            bid,
                            ItemKind::Thinking,
                            ItemStatus::Completed,
                            body(&Thinking {
                                text: text.to_owned(),
                            }),
                        ));
                    }
                }
                _ => {}
            }
        }
    }

    fn user(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        let content = &v["message"]["content"];
        let results: Vec<&Value> = content
            .as_array()
            .map(|a| a.iter().filter(|b| b["type"] == "tool_result").collect())
            .unwrap_or_default();
        if !results.is_empty() {
            // not `activity()`: a result belongs to a turn that is already open, and one that
            // arrives without a turn (late, or for a call we never saw) must not open a turn
            // no `result` line will close
            for b in results {
                self.tool_result(b, v, ev);
            }
            return;
        }
        let text = flatten(content);
        if text.is_empty() {
            return;
        }
        let id = item_id(v, offset);
        if v["origin"]["kind"] == "task-notification"
            || text.trim_start().starts_with("<task-notification>")
        {
            // the CLI's own report of background work, queued as a user line (and replayed since
            // 2.1.319): it starts a turn and is never the user's message. A task's end is the
            // `task_notification` row already; a Monitor event gets its own row.
            self.activity(offset, ev);
            if let Some(row) = monitor_event(&text) {
                ev.push(event_item(id, &row));
            }
            return;
        }
        let text = command_line(&text).unwrap_or(text);
        if let Some(out) = text.strip_prefix("<local-command-stdout>") {
            // output of a slash command the host sent, e.g. set_model's "Set model to `x`"
            let out = out
                .strip_suffix("</local-command-stdout>")
                .unwrap_or(out)
                .trim();
            let out = &strip_ansi(out);
            if let Some(m) = out
                .strip_prefix("Set model to `")
                .and_then(|r| r.split('`').next())
            {
                if self.model.as_deref() != Some(m) {
                    self.model = Some(m.to_owned());
                    ev.push(AgentEvent::ModelChanged(m.to_owned()));
                }
            }
            if let Some(c) = out.strip_prefix("Goal set:") {
                ev.push(AgentEvent::Goal(goal(c, GoalState::Active, None)));
            } else if let Some(c) = out.strip_prefix("Goal cleared:") {
                ev.push(AgentEvent::Goal(goal(c, GoalState::Cleared, None)));
            }
            ev.push(event_item(id, out));
        } else if let Some((c, r)) = goal_feedback(&text) {
            // the goal's Stop hook said not yet: the verdict, and a row with the reason whole
            ev.push(AgentEvent::Goal(goal(c, GoalState::NotMet, Some(r))));
            ev.push(event_item(id, &format!("◎ goal not met yet · {r}")));
        } else if v["origin"]["kind"] == "peer" {
            // another Claude session's SendMessage, delivered as a user line the user did not type
            self.activity(offset, ev);
            ev.push(done(
                id,
                ItemKind::PeerMessage,
                ItemStatus::Completed,
                body(&peer_message(&v["origin"], &text)),
            ));
        } else if v["isReplay"].as_bool() == Some(true) || self.parent.is_some() {
            // a subagent's text is the prompt it was given
            self.activity(offset, ev);
            ev.push(done(
                id,
                ItemKind::UserMessage,
                ItemStatus::Completed,
                body(&UserMessage { text }),
            ));
        } else {
            // markers such as "[Request interrupted by user]"
            ev.push(event_item(id, &text));
        }
    }

    fn tool_result(&mut self, b: &Value, line: &Value, ev: &mut Vec<AgentEvent>) {
        let Some(id) = b["tool_use_id"].as_str() else {
            return;
        };
        let is_error = b["is_error"].as_bool().unwrap_or(false);
        let output = flatten(&b["content"]);
        let call = self.calls.remove(id);
        self.off.item_close(id);
        let asked = self.take_pending(id, is_error, &output, &line["tool_use_result"], ev);
        // the Question/Plan items are the record of this call; a hidden call nobody asked about (a
        // mirror's ExitPlanMode) has no ToolCall item, and one would overwrite its Plan item
        if (asked && call.is_none()) || call.as_ref().is_some_and(|c| self.hides(&c.name)) {
            return;
        }
        let status = if is_error {
            ItemStatus::Failed
        } else {
            ItemStatus::Completed
        };
        let link = call
            .as_ref()
            .filter(|c| c.name == "Artifact")
            .and_then(|c| artifact_link(&c.input, &output, &line["tool_use_result"]));
        let result = done(
            format!("{id}:result"),
            ItemKind::ToolResult,
            status,
            body(&ToolResult {
                call_id: id.to_owned(),
                output,
                is_error,
                link,
            }),
        );
        let Some(call) = call else {
            // A result for a call we do not hold: it came late (its turn ended and `close_open`
            // already failed the call) or we never saw the call. The call item keeps the body it
            // has, completing it here would overwrite name and input with blanks; only the result
            // is new.
            ev.push(result);
            return;
        };
        ev.push(done(
            id.to_owned(),
            ItemKind::ToolCall,
            status,
            body(&ToolCall {
                name: call.name,
                input: call.input,
                call_id: id.to_owned(),
                task: self.task_of(id),
            }),
        ));
        ev.push(result);
    }

    /// The tool call `tool_use_id` finished: complete its Question items. Returns whether there were any.
    ///
    /// A question is `Answered` with what the user chose: what `answer()` recorded, or what the echo
    /// of our response says ([`echoed_answers`]), also a deliberate Cancel, Deny or "Keep planning"
    /// (the CLI reports those as errors, and they are still the user's answer). Without an answer of
    /// ours it is what the CLI recorded for an AskUserQuestion, or, when the call failed, the
    /// question was never answered (the turn was interrupted or the CLI withdrew it): `Failed`,
    /// with the CLI's own words as the reason. [`Self::drop_pending`] is the same `Failed` for a
    /// question that gets no result at all.
    fn take_pending(
        &mut self,
        tool_use_id: &str,
        is_error: bool,
        content: &str,
        result: &Value,
        ev: &mut Vec<AgentEvent>,
    ) -> bool {
        let Some(cid) = self
            .pending
            .iter()
            .find(|(_, p)| p.tool_use_id == tool_use_id)
            .map(|(k, _)| k.clone())
        else {
            return false;
        };
        let Some(p) = self.pending.remove(&cid) else {
            return false;
        };
        if p.closed {
            return true;
        }
        for part in p.parts {
            if !p.resolved {
                ev.push(AgentEvent::QuestionResolved {
                    request_id: part.request_id.clone(),
                });
            }
            let recorded = || match p.kind {
                PendKind::Ask => result["answers"][&part.text]
                    .as_str()
                    .map(|a| parse_answer(a, &part.labels, part.q.multi)),
                // an elicitation has no tool call, so no result reaches here
                PendKind::Allow | PendKind::Elicit => Some(Answer::Option(0)),
            };
            let (status, answered, reason) = match (is_error, part.answer.clone()) {
                (true, Some(a)) => (ItemStatus::Answered, Some(a), None),
                (true, None) => {
                    let why = if content.is_empty() {
                        "the tool call failed before the question was answered"
                    } else {
                        content
                    };
                    (ItemStatus::Failed, None, Some(why.to_owned()))
                }
                // the CLI's own record of an AskUserQuestion answer; an approval is ours, else the call
                // ran and so it was allowed
                (false, given) if p.kind == PendKind::Ask => {
                    (ItemStatus::Answered, recorded().or(given), None)
                }
                (false, given) => (ItemStatus::Answered, given.or_else(recorded), None),
            };
            self.off.item_close(&part.item_id);
            ev.push(done(
                part.item_id,
                ItemKind::Question,
                status,
                body(&QuestionItem {
                    q: part.q,
                    answered,
                    reason,
                }),
            ));
        }
        true
    }

    fn result(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        // the window, before the turn's end: the row that says the turn is over carries it
        if let (Some(used), Some(window)) = (self.ctx_used, self.window_of(&v["modelUsage"])) {
            ev.push(AgentEvent::Context {
                used,
                window: Some(window),
            });
        }
        if self.turn {
            // a subagent whose `Agent` call is still open ends with the turn; one that runs on in
            // the background (its call has returned) keeps its state while it has something open
            let ended: Vec<String> = self
                .subs
                .keys()
                .filter(|p| self.calls.contains_key(*p))
                .cloned()
                .collect();
            for p in ended {
                self.close_sub(&p, "the turn ended first", ev);
            }
            self.subs.retain(|_, t| !t.idle());
            self.close_open("the turn ended before it was answered", ev);
            self.streamed.clear();
            self.ord.clear();
            ev.push(AgentEvent::TurnEnded {
                stop: stop_reason(v),
                usage: usage_of(&v["usage"]),
                duration_ms: v["duration_ms"].as_u64(),
            });
            self.turn = false;
            self.off.turn_end();
        } else if v["is_error"].as_bool() == Some(true) {
            // A failure with no turn behind it: resuming a session the CLI does not know fails before
            // any turn begins. `TurnEnded` needs an open turn and a `TurnStarted` made up to carry the
            // error would break that pairing (and `Status(Working)` for a turn that never ran), so it
            // is an Event item. A `result` of an empty background-task turn (`num_turns` 0, no error)
            // is bookkeeping and says nothing.
            let why = match stop_reason(v) {
                StopReason::Error(m) | StopReason::Other(m) => m,
                _ => "error".to_owned(),
            };
            ev.push(event_item(
                item_id(v, offset),
                &format!("claude failed before it ran a turn: {why}"),
            ));
        }
        match v["user_message_uuids"].as_array() {
            Some(us) => us
                .iter()
                .filter_map(Value::as_str)
                .for_each(|u| self.cmd_end(u, None, ev)),
            None => self.end_cmds(true, None, ev),
        }
    }

    /// Everything the turn left open becomes final: unfinished blocks, calls without a result,
    /// questions nobody answered (`why` is recorded on those).
    fn close_open(&mut self, why: &str, ev: &mut Vec<AgentEvent>) {
        self.close_blocks(ev);
        for (id, c) in std::mem::take(&mut self.calls) {
            self.off.item_close(&id);
            if !self.hides(&c.name) {
                ev.push(done(
                    id.clone(),
                    ItemKind::ToolCall,
                    ItemStatus::Failed,
                    body(&ToolCall {
                        name: c.name,
                        input: c.input,
                        task: self.task_of(&id),
                        call_id: id,
                    }),
                ));
            }
        }
        if self.parent.is_some() {
            return; // questions are the main thread's
        }
        for cid in self.pending.keys().cloned().collect::<Vec<_>>() {
            self.drop_pending(&cid, why, ev);
        }
    }

    fn rate_limit(&mut self, v: &Value, ev: &mut Vec<AgentEvent>) {
        let info = &v["rate_limit_info"];
        let rejected = info["status"] == "rejected";
        let rtype = info["rateLimitType"].as_str();
        let mut seen = false;
        for (k, w) in info["unifiedWindows"].as_object().into_iter().flatten() {
            let Some(u) = w["utilization"].as_f64() else {
                continue;
            };
            seen = true;
            let pct = if rejected && rtype == Some(k.as_str()) {
                (u * 100.0).max(100.0)
            } else {
                u * 100.0
            };
            self.emit_rate_limit(k, pct, w["resetsAt"].as_i64(), ev);
        }
        if !seen && rejected {
            self.emit_rate_limit(
                rtype.unwrap_or("five_hour"),
                100.0,
                info["resetsAt"].as_i64(),
                ev,
            );
        }
    }

    fn emit_rate_limit(
        &mut self,
        window: &str,
        percent: f64,
        resets_at: Option<i64>,
        ev: &mut Vec<AgentEvent>,
    ) {
        let kind = match window {
            "five_hour" => "session",
            "seven_day" => "weekly",
            other => other,
        };
        let key = ((percent * 10.0).round() as i64, resets_at);
        if self.rl.insert(kind.to_owned(), key) != Some(key) {
            ev.push(AgentEvent::RateLimit {
                kind: kind.to_owned(),
                percent: percent as f32,
                resets_at,
            });
        }
    }

    fn lifecycle(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        let Some(u) = v["command_uuid"].as_str() else {
            return;
        };
        match v["state"].as_str() {
            Some("queued") => {
                self.cmd(u).acked = true;
                // The only thing in the spool that says "a message is waiting" until it starts: keep
                // it in the resume offset so a replay restores the busy state (and `Working`). While
                // a turn runs the anchor is that turn's start, not the frame itself: the turn ends
                // before the message starts, and a replay that began in the middle of a finished
                // turn would number its items wrongly.
                let at = self.off.turn_start().unwrap_or(offset);
                self.off.item_open(&cmd_anchor(u), at);
            }
            Some("started") => {
                let c = self.cmd(u);
                c.acked = true;
                c.ours = None; // running now; its echo becomes the UserMessage item
                self.off.item_close(&cmd_anchor(u)); // the turn that opens here holds the offset from now on
                self.activity(offset, ev);
            }
            Some("completed") => self.cmd_end(u, None, ev),
            Some(s @ ("cancelled" | "discarded" | "refused")) => {
                self.cmd_end(u, Some(DropReason::Refused(format!("claude {s} it"))), ev)
            }
            _ => {}
        }
    }

    fn can_use_tool(&mut self, offset: u64, rid: &str, req: &Value, ev: &mut Vec<AgentEvent>) {
        self.activity(offset, ev);
        let tool = req["tool_name"].as_str().unwrap_or("");
        let tid = req["tool_use_id"].as_str().unwrap_or(rid).to_owned();
        let title = req["title"]
            .as_str()
            .or(req["description"].as_str())
            .or(req["display_name"].as_str());
        self.ask(offset, rid, &tid, tool, &req["input"], title, ev);
    }

    /// An MCP server asks the user through the CLI (`elicitation/create`, claude 2.1.319:
    /// `{subtype:"elicitation", mcp_server_name, message, mode:"form"|"url", url?, elicitation_id?,
    /// requested_schema?, property_order?}`). One Question item `<rid>:elicit`, Accept / Decline.
    fn elicitation(&mut self, offset: u64, rid: &str, req: &Value, ev: &mut Vec<AgentEvent>) {
        self.activity(offset, ev);
        if self.pending.contains_key(rid) {
            return; // the request we already show, repeated
        }
        let server = req["mcp_server_name"].as_str().unwrap_or("an MCP server");
        let mut prompt = format!("{server} asks: {}", req["message"].as_str().unwrap_or(""));
        if req["mode"] == "url" {
            if let Some(url) = req["url"].as_str() {
                prompt = format!("{prompt}\n{url}");
            }
        } else {
            // ponytail: no form fields in a Question; Accept answers the form with its fields
            // empty (only the schema's defaults), so the prompt names what is left empty
            let fields = form_fields(req);
            if !fields.is_empty() {
                prompt = format!(
                    "{prompt}\nAccept leaves the form's fields empty: {}",
                    fields.join(", ")
                );
            }
        }
        let labels: Vec<String> = vec!["Accept".into(), "Decline".into()];
        let part = Part {
            request_id: rid.to_owned(),
            item_id: format!("{rid}:elicit"),
            q: Question {
                request_id: rid.to_owned(),
                prompt,
                options: labels.clone(),
                multi: false,
                free_text: false,
            },
            text: String::new(),
            labels,
            answer: None,
        };
        self.open_pending(
            offset,
            rid,
            rid,
            PendKind::Elicit,
            req.clone(),
            vec![part],
            ev,
        );
    }

    /// An elicitation of ours is over (the CLI read our answer, or the server said it is
    /// complete): its item is `Answered` with what was sent.
    fn finish_elicit(&mut self, cid: &str, ev: &mut Vec<AgentEvent>) {
        self.resolve(cid, ev);
        if let Some(p) = self.pending.remove(cid) {
            for part in p.parts {
                self.off.item_close(&part.item_id);
                ev.push(done(
                    part.item_id,
                    ItemKind::Question,
                    ItemStatus::Answered,
                    body(&QuestionItem {
                        q: part.q,
                        answered: part.answer,
                        reason: None,
                    }),
                ));
            }
        }
    }

    /// `system/elicitation_complete`: the MCP server confirms a URL-mode elicitation is done.
    /// Normally after our answer (its item is closed). One still open was finished in the browser
    /// without an answer here, and the CLI still waits for one: it is accepted.
    fn elicitation_complete(&mut self, v: &Value, out: &mut Out, ev: &mut Vec<AgentEvent>) {
        let Some(cid) = self
            .pending
            .iter()
            .find(|(_, p)| {
                p.kind == PendKind::Elicit
                    && !p.closed
                    && p.input["elicitation_id"] == v["elicitation_id"]
                    && p.input["mcp_server_name"] == v["mcp_server_name"]
            })
            .map(|(k, _)| k.clone())
        else {
            return;
        };
        if let Some(p) = self.pending.get_mut(&cid) {
            if !p.sent {
                p.sent = true;
                for part in &mut p.parts {
                    part.answer.get_or_insert(Answer::Option(0));
                }
                let resp = response_body(p);
                out.line(&json!({"type":"control_response","response":{"subtype":"success","request_id":cid,"response":resp}}));
            }
        }
        self.finish_elicit(&cid, ev);
    }

    /// The agent waits on the user: the `AskUserQuestion` call `tid` asks its questions, an
    /// `ExitPlanMode` or any other tool `tool` asks for an approval. Opens the `Question` items
    /// and remembers the request `rid` (the tool call's own id for a mirror) until its result.
    #[allow(clippy::too_many_arguments)]
    fn ask(
        &mut self,
        offset: u64,
        rid: &str,
        tid: &str,
        tool: &str,
        input: &Value,
        title: Option<&str>,
        ev: &mut Vec<AgentEvent>,
    ) {
        let input = input.clone();
        match self.pending.get(rid) {
            // the request we already show (or have answered), repeated: keep what we know about it
            Some(old) if old.tool_use_id == tid => return,
            // another call under the same request id replaces it, and the replaced one is over
            Some(_) => {
                self.drop_pending(rid, "the agent asked again under the same request id", ev)
            }
            None => {}
        }
        let mut parts = if tool == ASK {
            ask_parts(rid, tid, &input)
        } else {
            Vec::new()
        };
        let kind = if parts.is_empty() {
            PendKind::Allow
        } else {
            PendKind::Ask
        };
        if parts.is_empty() {
            let (prompt, options) = if tool == PLAN {
                (
                    "Approve this plan and start coding?".to_owned(),
                    ["Approve", "Keep planning"],
                )
            } else {
                (allow_prompt(title.unwrap_or(tool)), ["Allow", "Deny"])
            };
            let labels: Vec<String> = options.iter().map(|s| (*s).to_owned()).collect();
            parts.push(Part {
                request_id: rid.to_owned(),
                item_id: format!("{tid}:ask"),
                q: Question {
                    request_id: rid.to_owned(),
                    prompt,
                    options: labels.clone(),
                    multi: false,
                    free_text: true,
                },
                text: String::new(),
                labels,
                answer: None,
            });
        }
        self.open_pending(offset, rid, tid, kind, input, parts, ev);
    }

    /// The request `rid` (of the tool call `tid`) waits for the user: its `parts` are asked.
    #[allow(clippy::too_many_arguments)]
    fn open_pending(
        &mut self,
        offset: u64,
        rid: &str,
        tid: &str,
        kind: PendKind,
        input: Value,
        mut parts: Vec<Part>,
        ev: &mut Vec<AgentEvent>,
    ) {
        // Answers the daemon kept from before a restart. A request whose answer is complete (every
        // part answered, or one cancelled, which is sent at once) was already sent: it is not asked
        // again and never makes the session `Blocked`; one part way asks only the parts still open.
        for p in &mut parts {
            p.answer = self.restored.remove(&p.request_id);
        }
        let sent = parts.iter().any(|p| p.answer == Some(Answer::Cancel))
            || parts.iter().all(|p| p.answer.is_some());
        self.waits_ours = true;
        for p in &parts {
            self.off.item_open(&p.item_id, offset);
            if p.answer.is_none() {
                ev.push(AgentEvent::QuestionAsked {
                    item_id: p.item_id.clone(),
                    q: p.q.clone(),
                });
            }
        }
        self.pending.insert(
            rid.to_owned(),
            Pending {
                kind,
                tool_use_id: tid.to_owned(),
                input,
                parts,
                sent,
                resolved: sent,
                closed: false,
            },
        );
    }

    /// The request `cid` is over without a tool result: its questions are resolved and fail.
    fn drop_pending(&mut self, cid: &str, why: &str, ev: &mut Vec<AgentEvent>) {
        let Some(p) = self.pending.remove(cid) else {
            return;
        };
        if p.closed {
            return;
        }
        for part in p.parts {
            if !p.resolved {
                ev.push(AgentEvent::QuestionResolved {
                    request_id: part.request_id.clone(),
                });
            }
            self.off.item_close(&part.item_id);
            // an elicitation has no result to wait for: one we answered was answered
            let (status, answered, reason) = match part.answer {
                Some(a) if p.kind == PendKind::Elicit && p.sent => {
                    (ItemStatus::Answered, Some(a), None)
                }
                _ => (ItemStatus::Failed, None, Some(why.to_owned())),
            };
            ev.push(done(
                part.item_id,
                ItemKind::Question,
                status,
                body(&QuestionItem {
                    q: part.q,
                    answered,
                    reason,
                }),
            ));
        }
    }

    /// A `control_request` we have no answer for (`hook_callback`, `mcp_message`: we registered
    /// none; whatever a later CLI adds). The CLI would wait for our reply forever, so it gets an
    /// error reply now (which it takes as "no answer" and carries on, checked on 2.1.319 with a
    /// hook callback), and the user hears of it once per kind.
    fn refuse(
        &mut self,
        offset: u64,
        rid: &str,
        subtype: &str,
        out: &mut Out,
        ev: &mut Vec<AgentEvent>,
    ) {
        tracing::warn!(
            session = self.sid.as_deref().unwrap_or(""),
            subtype,
            request_id = rid,
            "claude sent a control request FEMME does not handle"
        );
        out.line(&json!({"type":"control_response","response":{
            "subtype":"error","request_id":rid,"error":format!("FEMME does not handle {subtype}")}}));
        if self.refused.insert(subtype.to_owned()) {
            ev.push(event_item(
                format!("o{offset}"),
                &format!("Claude asked something FEMME cannot answer: {subtype}"),
            ));
        }
    }

    /// The CLI read our answer to `cid` (its echo).
    fn resolve(&mut self, cid: &str, ev: &mut Vec<AgentEvent>) {
        if let Some(p) = self.pending.get_mut(cid) {
            if !p.resolved {
                p.resolved = true;
                ev.extend(p.parts.iter().map(|x| AgentEvent::QuestionResolved {
                    request_id: x.request_id.clone(),
                }));
            }
        }
    }

    /// The CLI withdrew the request `cid` (`control_cancel_request`, e.g. the turn was
    /// interrupted): its questions fail now, so nobody is offered one the CLI no longer reads.
    /// The entry stays, `closed`, until the call's result (or the turn's end) removes it.
    fn withdraw(&mut self, cid: &str, ev: &mut Vec<AgentEvent>) {
        let Some(p) = self.pending.get_mut(cid).filter(|p| !p.closed) else {
            return;
        };
        p.closed = true;
        p.sent = true;
        let resolved = std::mem::replace(&mut p.resolved, true);
        let parts: Vec<(String, String, Question)> = p
            .parts
            .iter()
            .map(|x| (x.request_id.clone(), x.item_id.clone(), x.q.clone()))
            .collect();
        for (request_id, item_id, q) in parts {
            if !resolved {
                ev.push(AgentEvent::QuestionResolved { request_id });
            }
            self.off.item_close(&item_id);
            ev.push(done(
                item_id,
                ItemKind::Question,
                ItemStatus::Failed,
                body(&QuestionItem {
                    q,
                    answered: None,
                    reason: Some("withdrawn by Claude".to_owned()),
                }),
            ));
        }
    }

    fn control_response(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        let r = &v["response"];
        let rid = r["request_id"].as_str().unwrap_or("");
        if rid == INIT_ID {
            self.awaiting_init = false;
        } else if let Some(p) = self.pending.get_mut(rid) {
            // `--replay-user-messages` echoes our answer: a replay (no `answer()` call in this
            // incarnation) learns what the user chose from it, so the outcome is the same one
            echoed_answers(p, &r["response"]);
            if p.kind == PendKind::Elicit {
                self.finish_elicit(rid, ev);
            } else {
                self.resolve(rid, ev);
            }
        } else if r["subtype"] == "error" {
            let why = r["error"].as_str().unwrap_or("unknown error");
            ev.push(event_item(
                format!("o{offset}"),
                &format!("claude rejected a control request: {why}"),
            ));
        }
    }
}

impl Driver for ClaudeDriver {
    fn agent(&self) -> Agent {
        Agent::Claude
    }

    fn spawn(&self, l: &Launch) -> Spawn {
        let mut cmd: Vec<String> = [
            "claude",
            "-p",
            "--input-format=stream-json",
            "--output-format=stream-json",
            "--verbose",
            "--include-partial-messages",
            "--replay-user-messages",
            "--permission-mode=bypassPermissions",
            "--permission-prompt-tool=stdio",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        if let Some(m) = &l.model {
            cmd.push(format!("--model={m}"));
        }
        if let Some(e) = &l.effort {
            cmd.push(format!("--effort={e}"));
        }
        match &l.resume {
            Some(id) => cmd.push(format!("--resume={id}")),
            None => cmd.push(format!("--session-id={}", self.planned)),
        }
        if let Some(p) = &l.hooks_path {
            cmd.push(format!("--settings={p}"));
        }
        let mut env = vec![(
            "CLAUDE_CODE_SDK_READS_SESSION_STATE".to_owned(),
            "1".to_owned(),
        )];
        env.extend(l.extra_env.iter().cloned());
        Spawn {
            cmd,
            env,
            cwd: l.cwd.clone(),
        }
    }

    fn tui(&self, l: &Launch) -> Option<Spawn> {
        let mut cmd = vec![
            "claude".to_owned(),
            "--dangerously-skip-permissions".to_owned(),
        ];
        // The same conversation as the headless process: the id the CLI reported (its transcript
        // exists, so resume it), else the one the daemon launched with, else the id this driver
        // planned for a new one, which `spawn` passed as `--session-id`.
        match self.announced.as_deref().or(l.resume.as_deref()) {
            Some(id) => cmd.push(format!("--resume={id}")),
            None => cmd.push(format!("--session-id={}", self.planned)),
        }
        if let Some(m) = &l.model {
            cmd.push(format!("--model={m}"));
        }
        if let Some(e) = &l.effort {
            cmd.push(format!("--effort={e}"));
        }
        if let Some(p) = &l.hooks_path {
            cmd.push(format!("--settings={p}"));
        }
        Some(Spawn {
            cmd,
            env: l.extra_env.clone(),
            cwd: l.cwd.clone(),
        })
    }

    fn on_start(&mut self, l: &Launch, out: &mut Out) {
        self.sid = l.resume.clone().or_else(|| Some(self.planned.clone()));
        self.awaiting_init = true;
        out.line(&json!({"type":"control_request","request_id":INIT_ID,"request":{"subtype":"initialize"}}));
    }

    fn attach(&mut self, _: &Launch, a: &Attach, _: &mut Out) {
        self.off.start_at(a.resume_offset);
        // the CLI repeats `system/init` only with the next turn; until then these are all we have
        self.sid.clone_from(&a.agent_sid);
        self.announced.clone_from(&a.agent_sid);
        self.model.clone_from(&a.model);
        self.restored = a.answers.iter().cloned().collect();
    }

    fn on_line(&mut self, offset: u64, line: &[u8], out: &mut Out) -> Vec<AgentEvent> {
        self.off.seen(offset, line);
        let mut ev = Vec::new();
        let Some(v) = parse(line) else {
            if !line.iter().all(u8::is_ascii_whitespace) {
                ev.push(raw_text(line));
            }
            return ev;
        };
        let ty = v["type"].as_str().unwrap_or("");
        if ty == "attachment" {
            // the transcript on disk (a herdr mirror, the daemon's goal tail): only a goal verdict
            ev.extend(goal_status(&v["attachment"]).map(AgentEvent::Goal));
            return ev;
        }
        if let (true, Some(pid)) = (
            matches!(ty, "stream_event" | "assistant" | "user"),
            v["parent_tool_use_id"].as_str(),
        ) {
            self.in_sub(pid, &mut ev, |d, ev| match ty {
                "stream_event" => d.stream_event(offset, &v, ev),
                "assistant" => d.assistant(offset, &v, ev),
                _ => d.user(offset, &v, ev),
            });
            self.flush_one(out);
            self.sync(&mut ev);
            return ev;
        }
        match ty {
            "system" if v["subtype"] == "elicitation_complete" => {
                self.elicitation_complete(&v, out, &mut ev)
            }
            "system" => self.system(offset, &v, &mut ev),
            "stream_event" => self.stream_event(offset, &v, &mut ev),
            "assistant" => self.assistant(offset, &v, &mut ev),
            "user" => self.user(offset, &v, &mut ev),
            "result" => self.result(offset, &v, &mut ev),
            "rate_limit_event" => self.rate_limit(&v, &mut ev),
            "command_lifecycle" => self.lifecycle(offset, &v, &mut ev),
            "control_request" => match v["request"]["subtype"].as_str() {
                Some("can_use_tool") => self.can_use_tool(
                    offset,
                    v["request_id"].as_str().unwrap_or(""),
                    &v["request"],
                    &mut ev,
                ),
                Some("elicitation") => self.elicitation(
                    offset,
                    v["request_id"].as_str().unwrap_or(""),
                    &v["request"],
                    &mut ev,
                ),
                other => self.refuse(
                    offset,
                    v["request_id"].as_str().unwrap_or(""),
                    other.unwrap_or(""),
                    out,
                    &mut ev,
                ),
            },
            "control_cancel_request" => {
                self.withdraw(v["request_id"].as_str().unwrap_or(""), &mut ev)
            }
            "control_response" => self.control_response(offset, &v, &mut ev),
            "keep_alive"
            | "tool_progress"
            | "transcript_mirror"
            | "tool_use_summary"
            | "prompt_suggestion"
            | "auth_status"
            | "update_environment_variables" => {}
            _ => ev.push(AgentEvent::Raw(v)),
        }
        self.flush_one(out);
        self.sync(&mut ev);
        ev
    }

    fn prompt(&mut self, text: &str, how: PromptHow, out: &mut Out) -> Result<(), Error> {
        self.send(held(text, how), out)
    }

    /// The CLI echoes a user message with the `uuid` it was written with (checked on 2.1.319, a
    /// message steered into a running turn too), so `id` is that uuid.
    fn prompt_with_id(
        &mut self,
        id: &str,
        text: &str,
        how: PromptHow,
        out: &mut Out,
    ) -> Result<(), Error> {
        let mut h = held(text, how);
        h.id = Some(id.to_owned());
        self.send(h, out)
    }

    fn interrupt(&mut self, out: &mut Out) -> Result<(), Error> {
        if self.exited {
            return Err(Error::State("claude has exited"));
        }
        if self.awaiting_init {
            return Err(Error::State("claude has not finished starting"));
        }
        // Held prompts stay: they run, in order, once the interrupted turn is over. With no turn
        // running there is nothing to stop (a message the CLI has not started yet would not be hit).
        if self.turn {
            self.control("int", json!({"subtype":"interrupt"}), out);
        }
        Ok(())
    }

    fn background(&mut self, tool_use_id: Option<&str>, out: &mut Out) -> Result<(), Error> {
        if self.exited {
            return Err(Error::State("claude has exited"));
        }
        if self.awaiting_init {
            return Err(Error::State("claude has not finished starting"));
        }
        let foreground = |t: &Task| !t.background && tool_use_id.is_none_or(|id| t.call == id);
        if !self.tasks.values().any(foreground) {
            return Err(Error::State("nothing runs in the foreground"));
        }
        let mut req = json!({"subtype":"background_tasks"});
        if let Some(id) = tool_use_id {
            req["tool_use_id"] = Value::String(id.to_owned());
        }
        self.control("bg", req, out);
        Ok(())
    }

    fn answer(&mut self, request_id: &str, answer: &Answer, out: &mut Out) -> Result<(), Error> {
        if self.exited {
            return Err(Error::State("claude has exited"));
        }
        let Some((cid, idx)) = self.pending.iter().find_map(|(cid, p)| {
            p.parts
                .iter()
                .position(|x| x.request_id == request_id)
                .map(|i| (cid.clone(), i))
        }) else {
            return Err(Error::UnknownRequest(request_id.to_owned()));
        };
        let p = self
            .pending
            .get_mut(&cid)
            .ok_or_else(|| Error::UnknownRequest(request_id.to_owned()))?;
        // an answer is final: also for one part of several (given before a restart, or just now)
        if p.sent || p.resolved || p.parts[idx].answer.is_some() {
            return Err(Error::UnknownRequest(request_id.to_owned()));
        }
        check_answer(p.kind, &p.parts[idx], answer)?;
        p.parts[idx].answer = Some(answer.clone());
        let cancelled = matches!(answer, Answer::Cancel);
        if cancelled || p.parts.iter().all(|x| x.answer.is_some()) {
            p.sent = true;
            let resp = response_body(p);
            out.line(&json!({"type":"control_response","response":{"subtype":"success","request_id":cid,"response":resp}}));
        }
        Ok(())
    }

    fn set_model(&mut self, model: &str, out: &mut Out) -> Result<(), Error> {
        if self.exited {
            return Err(Error::State("claude has exited"));
        }
        self.control("model", json!({"subtype":"set_model","model":model}), out);
        Ok(())
    }

    fn set_permission_mode(&mut self, mode: &str, out: &mut Out) -> Result<(), Error> {
        if self.exited {
            return Err(Error::State("claude has exited"));
        }
        self.control(
            "mode",
            json!({"subtype":"set_permission_mode","mode":mode}),
            out,
        );
        Ok(())
    }

    fn resume_offset(&self) -> u64 {
        self.off.resume()
    }

    fn held_prompts(&self) -> Vec<HeldPrompt> {
        self.queue.iter().cloned().collect()
    }

    fn pending_answers(&self) -> Vec<(String, Answer)> {
        self.pending
            .values()
            .flat_map(|p| &p.parts)
            .filter_map(|x| x.answer.clone().map(|a| (x.request_id.clone(), a)))
            .collect()
    }

    fn agent_session_id(&self) -> Option<&str> {
        self.sid.as_deref()
    }

    fn status(&self) -> AgentStatus {
        self.status
    }

    fn on_exit(&mut self, code: Option<i32>) -> Vec<AgentEvent> {
        let mut ev = Vec::new();
        self.exited = true;
        for p in self.subs.keys().cloned().collect::<Vec<_>>() {
            self.close_sub(&p, "claude exited first", &mut ev);
        }
        self.close_open("claude exited before it was answered", &mut ev);
        if self.turn {
            self.turn = false;
            self.off.turn_end();
            ev.push(AgentEvent::TurnEnded {
                stop: StopReason::Interrupted,
                usage: None,
                duration_ms: None,
            });
        }
        // Prompts that never ran: first those already written (the CLI queued them, or has not said
        // yet), then those the driver still held back. A command that started is not lost, its
        // turn just ended above.
        self.end_cmds(false, Some(DropReason::Exited), &mut ev);
        drop_held(&mut self.queue, &DropReason::Exited, &mut ev);
        self.awaiting_init = false;
        self.waits = false;
        set_status(&mut self.status, AgentStatus::Idle, &mut ev);
        ev.push(AgentEvent::Exited { code });
        ev
    }
}

// ---- helpers -----------------------------------------------------------------------------

/// Key of the resume-offset entry that keeps a command's `queued` line until it starts.
fn cmd_anchor(uuid: &str) -> String {
    format!("cmd:{uuid}")
}

/// A subagent's item id: a spool-offset one (`o<offset>...`, the line had no id of its own) is
/// made `<parent>/o<offset>...` so subagents tailed from other files cannot collide.
fn scope(pid: &str, id: &mut String) {
    if id
        .strip_prefix('o')
        .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
    {
        *id = format!("{pid}/{id}");
    }
}

/// Tools whose call is shown as Question/Plan items instead of a ToolCall.
/// The page an `Artifact` call points at: the structured result of a publish (`url`, `title`,
/// `seq`, `updated`), else the first claude.ai/artifact address in the text, else the `url` the
/// call read. The title falls back to the file's name, then to the call's action.
fn artifact_link(input: &Value, output: &str, data: &Value) -> Option<ArtifactLink> {
    const PAGE: &str = "https://claude.ai/artifact/";
    let in_text = output.find(PAGE).map(|i| {
        let rest = &output[i..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == ')' || c == ',')
            .unwrap_or(rest.len());
        rest[..end].to_owned()
    });
    let url = data["url"]
        .as_str()
        .map(str::to_owned)
        .or(in_text)
        .or_else(|| {
            input["url"]
                .as_str()
                .filter(|u| u.starts_with(PAGE))
                .map(str::to_owned)
        })?;
    let title = data["title"]
        .as_str()
        .filter(|t| !t.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| {
            let path = input["file_path"].as_str()?;
            Some(path.rsplit('/').next().unwrap_or(path).to_owned())
        })
        .or_else(|| input["action"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "page".to_owned());
    Some(ArtifactLink {
        url,
        title,
        version: data["seq"].as_u64(),
        updated: data["updated"].as_bool() == Some(true),
    })
}

/// The question of a permission ask about `what` (its title, description or tool name). A text
/// that already asks (the sandbox's "Allow network connection to example.com?") is the question.
fn allow_prompt(what: &str) -> String {
    let what = what.trim();
    if what.ends_with('?') {
        what.to_owned()
    } else {
        format!("Allow {what}?")
    }
}

fn hidden(name: &str) -> bool {
    name == ASK || name == PLAN
}

fn done(id: String, kind: ItemKind, status: ItemStatus, body: Value) -> AgentEvent {
    AgentEvent::ItemCompleted {
        id,
        kind,
        status,
        body,
        parent: None,
    }
}

/// `Stop hook feedback:\n[<condition>]: <reason>`, the goal checker's "not yet" as the stream
/// says it: `(condition, reason)`.
fn goal_feedback(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("Stop hook feedback:")?.trim_start();
    let rest = rest.strip_prefix('[')?;
    // ponytail: the first "]: " splits; a condition holding one gets cut short, the reason is whole
    let (c, r) = rest.split_once("]: ")?;
    Some((c.trim(), r.trim()))
}

/// The transcript's `goal_status` attachment as a [`Goal`]: the `sentinel` line (the goal was
/// set) is `Active`, a verdict `NotMet` or `Met` with the checker's reason.
pub fn goal_status(a: &Value) -> Option<Goal> {
    if a["type"] != "goal_status" {
        return None;
    }
    let condition = a["condition"].as_str().unwrap_or_default();
    let state = match (a["sentinel"] == true, a["met"] == true) {
        (true, _) => GoalState::Active,
        (false, true) => GoalState::Met,
        (false, false) => GoalState::NotMet,
    };
    Some(goal(condition, state, a["reason"].as_str()))
}

fn goal(condition: &str, state: GoalState, reason: Option<&str>) -> Goal {
    Goal {
        condition: condition.trim().to_owned(),
        state,
        reason: reason.map(|r| r.trim().to_owned()),
    }
}

fn event_item(id: String, text: &str) -> AgentEvent {
    done(
        id,
        ItemKind::Event,
        ItemStatus::Completed,
        body(&Event {
            text: text.to_owned(),
            task: None,
        }),
    )
}

/// A background task stopped (an `Agent` run in the background, a shell command): an event line
/// that names the call, so a client showing the agent at work can let it go.
fn task_end(id: String, v: &Value) -> AgentEvent {
    let status = v["status"].as_str().unwrap_or("stopped");
    let summary = v["summary"].as_str().unwrap_or("background task");
    done(
        id,
        ItemKind::Event,
        ItemStatus::Completed,
        body(&Event {
            text: format!("{summary}: {status}"),
            task: v["tool_use_id"].as_str().map(|call| TaskEnd {
                call: call.to_owned(),
                status: status.to_owned(),
            }),
        }),
    )
}

/// The line's own uuid, else a spool-offset id.
fn item_id(v: &Value, offset: u64) -> String {
    v["uuid"]
        .as_str()
        .map_or_else(|| format!("o{offset}"), str::to_owned)
}

/// A slash command as the TUI records it (`<command-name>/x</command-name>` and its
/// `<command-args>`) as the line typed: `/x args`.
fn command_line(text: &str) -> Option<String> {
    if !text.trim_start().starts_with("<command-") {
        return None;
    }
    let name = tag(text, "command-name")?;
    Some(match tag(text, "command-args").filter(|a| !a.is_empty()) {
        Some(args) => format!("{name} {args}"),
        None => name.to_owned(),
    })
}

/// A peer's message from the line's `origin` (`name`, `body`), else from the
/// `<cross-session-message from-name="…">…</cross-session-message>` framing in `text`.
fn peer_message(origin: &Value, text: &str) -> PeerMessage {
    let from = origin["name"].as_str().map(str::to_owned).or_else(|| {
        let (_, rest) = text.split_once("from-name=\"")?;
        rest.split_once('"').map(|(n, _)| n.to_owned())
    });
    let body = origin["body"].as_str().map(str::to_owned).or_else(|| {
        let (_, rest) = text.split_once("<cross-session-message")?;
        let (_, rest) = rest.split_once('>')?;
        rest.split_once("</cross-session-message>")
            .map(|(b, _)| b.trim().to_owned())
    });
    PeerMessage {
        from: from.unwrap_or_default(),
        text: body.unwrap_or_else(|| text.to_owned()),
    }
}

/// What the first `<t>...</t>` in `text` holds, trimmed.
fn tag<'a>(text: &'a str, t: &str) -> Option<&'a str> {
    let (_, rest) = text.split_once(&format!("<{t}>"))?;
    rest.split_once(&format!("</{t}>")).map(|(v, _)| v.trim())
}

/// The row of a `<task-notification>` that is not a task's end (no `<status>`), i.e. a Monitor
/// event: `◷ <description> · <event>`. The first line is the row, a longer event's other lines
/// follow it; the note to the model after `</event>` is left out.
fn monitor_event(text: &str) -> Option<String> {
    if tag(text, "status").is_some() {
        return None;
    }
    let summary = tag(text, "summary").unwrap_or("background task");
    let what = summary
        .strip_prefix("Monitor event:")
        .map_or(summary, |d| d.trim().trim_matches('"'));
    // up to the last `</event>`: the event is a command's output and may say the tag itself
    let event = text
        .split_once("<event>")
        .and_then(|(_, r)| r.rsplit_once("</event>"))
        .map_or("", |(e, _)| e.trim());
    Some(if event.is_empty() {
        format!("◷ {what}")
    } else {
        format!("◷ {what} · {event}")
    })
}

/// Text without its ANSI escape sequences (`ESC [ ... letter`).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\u{1b}' {
            out.push(c);
        } else if it.clone().next() == Some('[') {
            for c in it.by_ref().skip(1) {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

/// Message or tool-result content as text: strings as is, text blocks joined, other blocks as `[type]`.
fn flatten(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().map(str::to_owned),
                Some("image") => Some("[image]".to_owned()),
                Some(t) => Some(format!("[{t}]")),
                None => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Token counts of one `result`. An aborted turn reports all zeros although the model did run, so
/// all-zero means "unknown".
fn usage_of(u: &Value) -> Option<Usage> {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    let us = Usage {
        input: n("input_tokens"),
        output: n("output_tokens"),
        cache_read: n("cache_read_input_tokens"),
        cache_write: n("cache_creation_input_tokens"),
    };
    (u.is_object() && us != Usage::default()).then_some(us)
}

/// Tokens a model call read, from its `usage`: input plus cache read plus cache write. A call
/// that was aborted reports zeros, which is "unknown", not an empty context (as in [`usage_of`]).
fn context_of(u: &Value) -> Option<u64> {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    let used = n("input_tokens")
        .saturating_add(n("cache_read_input_tokens"))
        .saturating_add(n("cache_creation_input_tokens"));
    (u.is_object() && used > 0).then_some(used)
}

/// A model id without the context-size suffix the CLI takes on `--model` (`opus[1m]`).
fn base_model(m: &str) -> &str {
    m.split_once('[').map_or(m, |(base, _)| base)
}

/// The window of a model the CLI has reported one for. Its `result` line says the real one at
/// the turn's end (and wins), but a transcript read from disk never has that line.
// ponytail: the two models seen so far; extend as the result lines report others
fn known_window(model: Option<&str>) -> Option<u64> {
    match base_model(model?) {
        "claude-fable-5-1" | "claude-fable-5-2" | "claude-opus-5-5" => Some(1_000_000),
        _ => None,
    }
}

fn stop_reason(r: &Value) -> StopReason {
    if r["terminal_reason"]
        .as_str()
        .is_some_and(|t| t.starts_with("aborted"))
    {
        return StopReason::Interrupted;
    }
    let sub = r["subtype"].as_str().unwrap_or("");
    if matches!(
        sub,
        "error_max_turns" | "error_max_budget_usd" | "error_max_structured_output_retries"
    ) {
        return StopReason::Other(sub.to_owned());
    }
    if r["is_error"].as_bool() == Some(true) || sub.starts_with("error") {
        let msg = r["result"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let errs: Vec<&str> = r["errors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                if errs.is_empty() {
                    sub.to_owned()
                } else {
                    errs.join("; ")
                }
            });
        return StopReason::Error(msg);
    }
    match r["stop_reason"].as_str() {
        None | Some("end_turn" | "stop_sequence") => StopReason::EndTurn,
        Some("max_tokens") => StopReason::MaxTokens,
        Some(o) => StopReason::Other(o.to_owned()),
    }
}

/// The questions of an AskUserQuestion call; request ids and item ids get a `#<i>` suffix when
/// there is more than one.
fn ask_parts(rid: &str, tid: &str, input: &Value) -> Vec<Part> {
    let qs = input["questions"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    qs.iter()
        .enumerate()
        .map(|(i, q)| {
            let (request_id, item_id) = if qs.len() == 1 {
                (rid.to_owned(), tid.to_owned())
            } else {
                (format!("{rid}#{i}"), format!("{tid}#{i}"))
            };
            let opts = q["options"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            let labels: Vec<String> = opts
                .iter()
                .map(|o| o["label"].as_str().unwrap_or("").to_owned())
                .collect();
            let options = opts
                .iter()
                .zip(&labels)
                .map(
                    |(o, l)| match o["description"].as_str().filter(|d| !d.is_empty()) {
                        Some(d) => format!("{l} \u{2014} {d}"),
                        None => l.clone(),
                    },
                )
                .collect();
            let text = q["question"].as_str().unwrap_or("").to_owned();
            let multi = q["multiSelect"].as_bool().unwrap_or(false);
            Part {
                q: Question {
                    request_id: request_id.clone(),
                    prompt: text.clone(),
                    options,
                    multi,
                    free_text: true,
                },
                request_id,
                item_id,
                text,
                labels,
                answer: None,
            }
        })
        .collect()
}

/// Reject answers that cannot be sent, so `answer()` fails instead of the CLI getting nonsense.
fn check_answer(kind: PendKind, p: &Part, a: &Answer) -> Result<(), Error> {
    match (kind, a) {
        (PendKind::Elicit, Answer::Text(_)) => Err(Error::Bad(
            "an MCP server's question takes Accept, Decline or Cancel, no text".into(),
        )),
        (_, Answer::Cancel | Answer::Text(_)) => Ok(()),
        (_, Answer::Option(i)) if usize::from(*i) < p.labels.len() => Ok(()),
        // an empty pick would go to the CLI as the empty answer text: say "cancel" or give a pick
        (_, Answer::Options(v)) if v.is_empty() => Err(Error::Bad(
            "no option chosen (use Cancel to decline)".into(),
        )),
        (PendKind::Ask, Answer::Options(v))
            if (p.q.multi || v.len() == 1)
                && v.iter().all(|i| usize::from(*i) < p.labels.len()) =>
        {
            Ok(())
        }
        _ => Err(Error::Bad(format!(
            "answer {a:?} does not fit a question with {} options (multi: {})",
            p.labels.len(),
            p.q.multi
        ))),
    }
}

/// The `response` of the `control_response` for a fully answered request.
fn response_body(p: &Pending) -> Value {
    let deny = |m: &str| json!({"behavior":"deny","message":m});
    match p.kind {
        PendKind::Ask => {
            let mut answers = serde_json::Map::new();
            for part in &p.parts {
                let label = match &part.answer {
                    Some(Answer::Option(i)) => part
                        .labels
                        .get(usize::from(*i))
                        .cloned()
                        .unwrap_or_default(),
                    Some(Answer::Options(v)) => v
                        .iter()
                        .filter_map(|i| part.labels.get(usize::from(*i)))
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                    Some(Answer::Text(t)) => t.clone(),
                    Some(Answer::Cancel) | None => return deny("The user declined to answer."),
                };
                answers.insert(part.text.clone(), Value::String(label));
            }
            let mut input = p.input.clone();
            if let Some(o) = input.as_object_mut() {
                o.insert("answers".to_owned(), Value::Object(answers));
            }
            json!({"behavior":"allow","updatedInput":input})
        }
        PendKind::Allow => match p.parts.first().and_then(|x| x.answer.as_ref()) {
            Some(Answer::Option(0)) => json!({"behavior":"allow","updatedInput":p.input}),
            Some(Answer::Text(t)) => deny(t),
            _ => deny("The user said no."),
        },
        // the CLI hands `{action, content?}` to the MCP server as is (`oDs` in 2.1.319)
        PendKind::Elicit => match p.parts.first().and_then(|x| x.answer.as_ref()) {
            Some(Answer::Option(0)) if p.input["mode"] == "url" => json!({"action":"accept"}),
            Some(Answer::Option(0)) => {
                json!({"action":"accept","content":form_defaults(&p.input["requested_schema"])})
            }
            Some(Answer::Option(1)) => json!({"action":"decline"}),
            _ => json!({"action":"cancel"}),
        },
    }
}

/// The names of an elicitation form's fields, in the order the CLI gives (`property_order`), else
/// the schema's.
fn form_fields(req: &Value) -> Vec<String> {
    let names = |v: &Value| -> Vec<String> {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    };
    let ordered = names(&req["property_order"]);
    if !ordered.is_empty() {
        return ordered;
    }
    req["requested_schema"]["properties"]
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// The content of an accepted form: only the defaults its schema gives, every other field empty
/// (left out).
fn form_defaults(schema: &Value) -> Value {
    let o: serde_json::Map<String, Value> = schema["properties"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, f)| f.get("default").map(|d| (k.clone(), d.clone())))
        .collect();
    Value::Object(o)
}

/// Fill in the parts of `p` nobody answered in this incarnation from the response we wrote to
/// the CLI (its echo), the inverse of [`response_body`]. What `response_body` cannot tell apart
/// comes back as the nearer answer: a refusal of an approval is `Option(1)` ("Deny", "Keep
/// planning"), whether it was that option or a `Cancel`.
fn echoed_answers(p: &mut Pending, resp: &Value) {
    let deny = resp["behavior"] == "deny";
    for part in p.parts.iter_mut().filter(|x| x.answer.is_none()) {
        part.answer = match (p.kind, deny) {
            (PendKind::Elicit, _) => Some(match resp["action"].as_str() {
                Some("accept") => Answer::Option(0),
                Some("decline") => Answer::Option(1),
                _ => Answer::Cancel,
            }),
            (PendKind::Ask, true) => Some(Answer::Cancel),
            (PendKind::Ask, false) => resp["updatedInput"]["answers"][&part.text]
                .as_str()
                .map(|a| parse_answer(a, &part.labels, part.q.multi)),
            (PendKind::Allow, false) => Some(Answer::Option(0)),
            (PendKind::Allow, true) => Some(match resp["message"].as_str() {
                None | Some("The user said no.") => Answer::Option(1),
                Some(t) => Answer::Text(t.to_owned()),
            }),
        };
    }
}

/// Turn the CLI's recorded answer string back into an [`Answer`] (labels joined with `, ` for
/// multi-select; anything else was typed text).
fn parse_answer(s: &str, labels: &[String], multi: bool) -> Answer {
    if let Some(i) = labels.iter().position(|l| l == s) {
        return if multi {
            Answer::Options(vec![i as u16])
        } else {
            Answer::Option(i as u16)
        };
    }
    let picks: Vec<u16> = s
        .split(", ")
        .filter_map(|x| labels.iter().position(|l| l == x).map(|i| i as u16))
        .collect();
    if multi && !picks.is_empty() && picks.len() == s.split(", ").count() {
        Answer::Options(picks)
    } else {
        Answer::Text(s.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_round_trip_through_labels() {
        let labels: Vec<String> = ["A", "B", "C"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(parse_answer("B", &labels, false), Answer::Option(1));
        assert_eq!(
            parse_answer("A, C", &labels, true),
            Answer::Options(vec![0, 2])
        );
        assert_eq!(parse_answer("B", &labels, true), Answer::Options(vec![1]));
        assert_eq!(
            parse_answer("something else", &labels, false),
            Answer::Text("something else".into())
        );
        assert_eq!(
            parse_answer("A, nope", &labels, true),
            Answer::Text("A, nope".into())
        );
    }

    #[test]
    fn stop_reasons() {
        let r = |v: Value| stop_reason(&v);
        assert_eq!(
            r(json!({"subtype":"success","stop_reason":"end_turn","is_error":false})),
            StopReason::EndTurn
        );
        assert_eq!(
            r(json!({"subtype":"success","stop_reason":"max_tokens"})),
            StopReason::MaxTokens
        );
        assert_eq!(
            r(
                json!({"subtype":"error_during_execution","terminal_reason":"aborted_tools","is_error":true})
            ),
            StopReason::Interrupted
        );
        assert_eq!(
            r(json!({"subtype":"success","is_error":true,"result":"boom"})),
            StopReason::Error("boom".into())
        );
        assert_eq!(
            r(json!({"subtype":"error_during_execution","is_error":true,"errors":["a","b"]})),
            StopReason::Error("a; b".into())
        );
        assert_eq!(
            r(json!({"subtype":"error_max_turns","is_error":true})),
            StopReason::Other("error_max_turns".into())
        );
        assert_eq!(
            r(json!({"subtype":"success","stop_reason":"tool_use"})),
            StopReason::Other("tool_use".into())
        );
    }

    /// After a resume the CLI says its previous background task died, then reports an empty
    /// `result` (0 turns, origin task-notification) that is not a turn of ours (claude 2.1.302).
    #[test]
    fn empty_task_notification_result_is_not_a_turn() {
        let note = r#"{"type":"system","subtype":"task_notification","task_id":"bb8jg4woe","tool_use_id":"toolu_01QY","status":"stopped","output_file":"","summary":"Background shell command didn't finish before the previous session ended","uuid":"26ebf08c","session_id":"0cbc4120"}"#;
        let result = r#"{"is_error":false,"duration_api_ms":0,"num_turns":0,"stop_reason":null,"session_id":"0cbc4120","total_cost_usd":0,"usage":{"input_tokens":0,"output_tokens":0},"modelUsage":{},"origin":{"kind":"task-notification"},"subtype":"success","result":"","type":"result","duration_ms":276,"uuid":"8fa0f031","queued_turn_count":0,"result_index":0}"#;
        let (mut d, mut out) = (ClaudeDriver::new(), Out::default());
        // the note itself is an event line naming the call, for the subagent strip
        let ev = d.on_line(0, note.as_bytes(), &mut out);
        assert_eq!(ev.len(), 1);
        match &ev[0] {
            AgentEvent::ItemCompleted { kind, body, .. } => {
                assert_eq!(*kind, ItemKind::Event);
                assert_eq!(
                    body["text"],
                    "Background shell command didn't finish before the previous session ended: stopped"
                );
                assert_eq!(body["task"]["call"], "toolu_01QY");
                assert_eq!(body["task"]["status"], "stopped");
            }
            other => panic!("{other:?}"),
        }
        assert!(d
            .on_line(note.len() as u64 + 1, result.as_bytes(), &mut out)
            .is_empty());
        assert_eq!(d.status(), AgentStatus::Idle);
        assert!(out.stdin.is_empty());
    }

    #[test]
    fn flatten_handles_every_shape() {
        assert_eq!(flatten(&json!("hi")), "hi");
        assert_eq!(
            flatten(
                &json!([{"type":"text","text":"a"},{"type":"image"},{"type":"tool_reference","tool_name":"x"}])
            ),
            "a\n[image]\n[tool_reference]"
        );
        assert_eq!(flatten(&Value::Null), "");
    }

    #[test]
    fn a_command_reads_as_typed_and_its_output_without_escapes() {
        let typed = "<command-name>/model</command-name>\n  <command-message>model</command-message>\n  <command-args>opus</command-args>";
        assert_eq!(command_line(typed).as_deref(), Some("/model opus"));
        let bare = "<command-name>/compact</command-name><command-args></command-args>";
        assert_eq!(command_line(bare).as_deref(), Some("/compact"));
        assert_eq!(command_line("hello"), None);
        assert_eq!(
            strip_ansi("\u{1b}[2mCompacted\u{1b}[22m ok"),
            "Compacted ok"
        );
    }

    #[test]
    fn init_says_the_commands_but_not_the_terminals_own() {
        let mut d = ClaudeDriver::new();
        let line = json!({"type":"system","subtype":"init","session_id":"s",
            "slash_commands":["compact","doctor","review"],"terminal_slash_commands":["doctor"]});
        let ev = d.on_line(0, line.to_string().as_bytes(), &mut Out::default());
        assert!(ev.contains(&AgentEvent::Commands(vec![
            "compact".into(),
            "review".into()
        ])));
        let status =
            json!({"type":"system","subtype":"status","status":null,"permissionMode":"plan"});
        let ev = d.on_line(1, status.to_string().as_bytes(), &mut Out::default());
        assert_eq!(ev, [AgentEvent::PermissionMode("plan".into())]);
        // the same mode again says nothing
        assert!(d
            .on_line(2, status.to_string().as_bytes(), &mut Out::default())
            .is_empty());
        let mut out = Out::default();
        d.set_permission_mode("auto", &mut out).unwrap();
        assert!(String::from_utf8_lossy(&out.stdin).contains(r#""subtype":"set_permission_mode""#));
    }

    // ---- the Artifact tool (shapes of claude 2.1.319) ----------------------------------------

    #[test]
    fn an_artifact_publish_links_its_page() {
        let input = json!({"file_path":"/tmp/x/page.html"});
        let text = "Published /tmp/x/page.html at https://claude.ai/artifact/AbC123 (Version 2, version id 1-2)";
        let data = json!({"url":"https://claude.ai/artifact/AbC123","title":"Lesson tree","seq":2,"updated":true});
        assert_eq!(
            artifact_link(&input, text, &data),
            Some(ArtifactLink {
                url: "https://claude.ai/artifact/AbC123".into(),
                title: "Lesson tree".into(),
                version: Some(2),
                updated: true,
            })
        );
        // no structured data: the address is in the text, the title is the file's name
        assert_eq!(
            artifact_link(&input, text, &Value::Null),
            Some(ArtifactLink {
                url: "https://claude.ai/artifact/AbC123".into(),
                title: "page.html".into(),
                version: None,
                updated: false,
            })
        );
        // a read: the input's url, the action as title
        let read = json!({"action":"read","url":"https://claude.ai/artifact/Zz9"});
        let l = artifact_link(&read, "<html>…", &Value::Null).unwrap();
        assert_eq!(
            (l.url.as_str(), l.title.as_str()),
            ("https://claude.ai/artifact/Zz9", "read")
        );
        assert_eq!(
            artifact_link(&json!({"action":"list"}), "none", &Value::Null),
            None
        );
    }

    #[test]
    fn an_artifact_result_item_carries_the_link() {
        let mut f = Feed::new();
        f.line(json!({"type":"assistant","message":{"id":"m1","type":"message","role":"assistant","content":[
            {"type":"tool_use","id":"t1","name":"Artifact","input":{"file_path":"/tmp/x/page.html"}}]},
            "session_id":"s","parent_tool_use_id":null,"uuid":"u1"}));
        let ev = f.line(json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result",
            "tool_use_id":"t1","content":"Published /tmp/x/page.html at https://claude.ai/artifact/AbC123 (Version 1, version id 1-1)"}]},
            "session_id":"s","parent_tool_use_id":null,"uuid":"u2",
            "tool_use_result":{"url":"https://claude.ai/artifact/AbC123","title":"FEMME artifact test","seq":1,"updated":false}}));
        let link = ev
            .iter()
            .find_map(|e| match e {
                AgentEvent::ItemCompleted { id, body, .. } if id == "t1:result" => {
                    Some(body["link"].clone())
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(link["url"], "https://claude.ai/artifact/AbC123");
        assert_eq!(link["title"], "FEMME artifact test");
        assert_eq!(link["version"], 1);
    }

    // ---- the context window (shapes of claude 2.1.302, made-up numbers) ----------------------

    /// A driver fed whole lines at their offsets, as the daemon does.
    struct Feed {
        d: ClaudeDriver,
        at: u64,
    }

    impl Feed {
        fn new() -> Self {
            Feed {
                d: ClaudeDriver::new(),
                at: 0,
            }
        }

        fn line(&mut self, v: Value) -> Vec<AgentEvent> {
            let text = v.to_string();
            let ev = self
                .d
                .on_line(self.at, text.as_bytes(), &mut Out::default());
            self.at += text.len() as u64 + 1;
            ev
        }
    }

    #[test]
    fn a_goals_stop_hook_feedback_is_a_verdict_row_not_a_note() {
        let mut f = Feed::new();
        let ev = f.line(json!({"type":"user","message":{"content":[
            {"type":"text","text":"Stop hook feedback:\n[c]: not yet"}]}}));
        assert!(
            ev.contains(&AgentEvent::Goal(Goal {
                condition: "c".into(),
                state: GoalState::NotMet,
                reason: Some("not yet".into()),
            })),
            "{ev:#?}"
        );
        let texts: Vec<String> = ev
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ItemCompleted { kind, body, .. } if *kind == ItemKind::Event => {
                    body["text"].as_str().map(str::to_owned)
                }
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["◎ goal not met yet · not yet"]);
    }

    #[test]
    fn goal_lines_set_judge_and_clear() {
        let mut f = Feed::new();
        let state = |ev: Vec<AgentEvent>| {
            ev.into_iter().find_map(|e| match e {
                AgentEvent::Goal(g) => Some((g.state, g.reason)),
                _ => None,
            })
        };
        let out = |s: &str| {
            json!({"type":"user","message":{"content":[{"type":"text",
                "text":format!("<local-command-stdout>{s}</local-command-stdout>")}]}})
        };
        assert_eq!(
            state(f.line(out("Goal set: c"))),
            Some((GoalState::Active, None))
        );
        let att = |a: Value| json!({"type":"attachment","attachment":a});
        assert_eq!(
            state(f.line(att(
                json!({"type":"goal_status","met":false,"sentinel":true,"condition":"c"})
            ))),
            Some((GoalState::Active, None))
        );
        assert_eq!(
            state(f.line(att(
                json!({"type":"goal_status","met":false,"condition":"c","reason":"r1"})
            ))),
            Some((GoalState::NotMet, Some("r1".into())))
        );
        assert_eq!(
            state(f.line(att(
                json!({"type":"goal_status","met":true,"condition":"c","reason":"r2"})
            ))),
            Some((GoalState::Met, Some("r2".into())))
        );
        assert_eq!(
            state(f.line(out("Goal cleared: c"))),
            Some((GoalState::Cleared, None))
        );
        assert_eq!(state(f.line(att(json!({"type":"other"})))), None);
    }

    fn contexts(ev: Vec<AgentEvent>) -> Vec<AgentEvent> {
        ev.into_iter()
            .filter(|e| matches!(e, AgentEvent::Context { .. }))
            .collect()
    }

    fn ctx(used: u64, window: Option<u64>) -> AgentEvent {
        AgentEvent::Context { used, window }
    }

    /// `usage` as a call reports it: a fresh part, what it read from the cache, what it wrote.
    fn usage(input: u64, read: u64, write: u64) -> Value {
        json!({"input_tokens":input,"cache_creation_input_tokens":write,"cache_read_input_tokens":read,
            "cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":write},"output_tokens":6})
    }

    fn message_start(usage: Value, parent: Value) -> Value {
        json!({"type":"stream_event","event":{"type":"message_start","message":{"id":"msg_1","type":"message",
            "role":"assistant","content":[],"usage":usage}},"session_id":"s","parent_tool_use_id":parent,"uuid":"u1"})
    }

    fn assistant_line(usage: Value, parent: Value) -> Value {
        json!({"type":"assistant","message":{"id":"msg_1","type":"message","role":"assistant",
            "content":[{"type":"text","text":"hi"}],"usage":usage},"session_id":"s","parent_tool_use_id":parent,"uuid":"u2"})
    }

    fn init(model: &str) -> Value {
        json!({"type":"system","subtype":"init","session_id":"s","model":model})
    }

    fn result_line(total: Value, models: Value) -> Value {
        json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","num_turns":1,
            "session_id":"s","duration_ms":9,"usage":total,"modelUsage":models,"uuid":"r1"})
    }

    #[test]
    fn a_model_call_says_what_it_read_once_per_number() {
        let mut f = Feed::new();
        let first = usage(2, 0, 44300);
        assert_eq!(
            contexts(f.line(message_start(first.clone(), Value::Null))),
            [ctx(44302, None)]
        );
        // the assistant line of the same message repeats the number: nothing new
        assert!(contexts(f.line(assistant_line(first, Value::Null))).is_empty());
        // the next call read more (what was written is now read)
        let next = usage(1, 44300, 500);
        assert_eq!(
            contexts(f.line(message_start(next.clone(), Value::Null))),
            [ctx(44801, None)]
        );
        assert!(contexts(f.line(assistant_line(next, Value::Null))).is_empty());
        // an assistant line that comes alone (no stream) is a call too
        assert_eq!(
            contexts(f.line(assistant_line(usage(1, 44801, 70), Value::Null))),
            [ctx(44872, None)]
        );
    }

    #[test]
    fn a_known_model_says_its_window_with_every_call() {
        let mut f = Feed::new();
        f.line(init("claude-fable-5-1"));
        assert_eq!(
            contexts(f.line(message_start(usage(2, 0, 44300), Value::Null))),
            [ctx(44302, Some(1_000_000))]
        );
        // a transcript on disk has no init line: its first message says the model
        let mut f = Feed::new();
        let mut line = assistant_line(usage(2, 40_000, 500), Value::Null);
        line["message"]["model"] = json!("claude-opus-5-5");
        let ev = f.line(line);
        assert!(ev.contains(&AgentEvent::ModelChanged("claude-opus-5-5".into())));
        assert_eq!(contexts(ev), [ctx(40_502, Some(1_000_000))]);
        assert_eq!(known_window(Some("claude-opus-5-5[1m]")), Some(1_000_000));
        assert_eq!(
            known_window(Some("deepseek/deepseek-v4.1-flash-ultrafast")),
            None
        );
        assert_eq!(known_window(None), None);
    }

    #[test]
    fn an_agent_without_a_cache_still_says_its_input() {
        // another provider behind the CLI reports no cache fields at all
        let mut f = Feed::new();
        let plain = json!({"input_tokens":36654,"output_tokens":0});
        assert_eq!(
            contexts(f.line(message_start(plain, Value::Null))),
            [ctx(36654, None)]
        );
    }

    #[test]
    fn a_call_that_read_nothing_is_unknown_not_empty() {
        let mut f = Feed::new();
        let zeros = usage(0, 0, 0);
        assert!(contexts(f.line(message_start(zeros.clone(), Value::Null))).is_empty());
        assert!(contexts(f.line(assistant_line(zeros, Value::Null))).is_empty());
        assert!(contexts(f.line(message_start(Value::Null, Value::Null))).is_empty());
        // and it does not make the driver forget what the call before said
        assert_eq!(
            contexts(f.line(message_start(usage(2, 0, 100), Value::Null))),
            [ctx(102, None)]
        );
        assert!(contexts(f.line(message_start(usage(0, 0, 0), Value::Null))).is_empty());
        assert!(contexts(f.line(assistant_line(usage(2, 0, 100), Value::Null))).is_empty());
    }

    #[test]
    fn a_subagents_calls_are_not_the_sessions_context() {
        let mut f = Feed::new();
        let main = usage(2, 0, 90_000);
        assert_eq!(
            contexts(f.line(message_start(main.clone(), Value::Null))),
            [ctx(90_002, None)]
        );
        // an `Agent` call's own conversation is small, and says nothing about ours
        let sub = Value::from("toolu_agent");
        let small = usage(2, 0, 5_000);
        assert!(contexts(f.line(message_start(small.clone(), sub.clone()))).is_empty());
        assert!(contexts(f.line(assistant_line(small, sub))).is_empty());
        assert!(contexts(f.line(assistant_line(main, Value::Null))).is_empty());
    }

    #[test]
    fn the_result_says_the_window_and_its_own_usage_is_no_context_size() {
        let mut f = Feed::new();
        f.line(message_start(usage(2, 0, 44300), Value::Null));
        // the turn's total over all its calls: far more than any one call read
        let total = usage(5, 900_000, 44_300);
        let models = json!({"claude-fable-5-1":{"inputTokens":5,"outputTokens":9,
            "cacheReadInputTokens":900_000,"cacheCreationInputTokens":44_300,"webSearchRequests":0,
            "costUSD":0.9,"contextWindow":1_000_000,"maxOutputTokens":64_000}});
        let ev = f.line(result_line(total, models));
        let said = ev.iter().position(|e| *e == ctx(44302, Some(1_000_000)));
        let ended = ev
            .iter()
            .position(|e| matches!(e, AgentEvent::TurnEnded { .. }));
        assert!(
            said.is_some() && said < ended,
            "the window comes before the end of the turn: {ev:?}"
        );
        assert_eq!(contexts(ev).len(), 1, "and used stays the last call's");
        // the next call is the news again, without a window
        assert_eq!(
            contexts(f.line(message_start(usage(2, 44302, 90), Value::Null))),
            [ctx(44394, None)]
        );
    }

    #[test]
    fn a_result_before_any_call_or_without_a_window_says_nothing() {
        let models = json!({"claude-fable-5-1":{"contextWindow":1_000_000}});
        let mut f = Feed::new();
        assert!(contexts(f.line(result_line(usage(0, 0, 0), models.clone()))).is_empty());
        f.line(message_start(usage(2, 0, 100), Value::Null));
        assert!(contexts(f.line(result_line(usage(2, 0, 100), json!({})))).is_empty());
        let none = json!({"claude-fable-5-1":{"inputTokens":2}});
        assert!(contexts(f.line(result_line(usage(2, 0, 100), none))).is_empty());
        let zero = json!({"claude-fable-5-1":{"contextWindow":0}});
        assert!(contexts(f.line(result_line(usage(2, 0, 100), zero))).is_empty());
        assert_eq!(
            contexts(f.line(result_line(usage(2, 0, 100), models))),
            [ctx(102, Some(1_000_000))]
        );
    }

    #[test]
    fn the_window_is_the_sessions_models_when_a_turn_used_two() {
        // a subagent on a smaller model: its entry sorts first, ours is the one that counts
        let models = json!({"claude-haiku-5-5":{"contextWindow":200_000},
            "claude-opus-5-5":{"contextWindow":1_000_000}});
        for model in ["claude-opus-5-5", "claude-opus-5-5[1m]"] {
            let mut f = Feed::new();
            f.line(init(model));
            f.line(message_start(usage(2, 0, 100), Value::Null));
            assert_eq!(
                contexts(f.line(result_line(usage(2, 0, 100), models.clone()))),
                [ctx(102, Some(1_000_000))],
                "model {model}"
            );
        }
        // a model the entries do not name: the first entry
        let mut f = Feed::new();
        f.line(init("an-alias"));
        f.line(message_start(usage(2, 0, 100), Value::Null));
        let one = json!({"claude-opus-5-5":{"contextWindow":1_000_000}});
        assert_eq!(
            contexts(f.line(result_line(usage(2, 0, 100), one))),
            [ctx(102, Some(1_000_000))]
        );
    }

    #[test]
    fn a_compact_boundary_is_an_event_item_and_a_compaction() {
        let mut f = Feed::new();
        let at = |trigger: &str, uuid: &str, pre: Option<u64>| {
            let mut v = json!({"type":"system","subtype":"compact_boundary","session_id":"s","uuid":uuid,
                "compact_metadata":{"trigger":trigger}});
            if let Some(p) = pre {
                v["compact_metadata"]["pre_tokens"] = json!(p);
            }
            v
        };
        let ev = f.line(at("auto", "c1", Some(123_456)));
        assert_eq!(
            ev,
            [
                event_item("c1".into(), "Conversation compacted"),
                AgentEvent::Compacted {
                    pre_tokens: Some(123_456)
                }
            ]
        );
        // a boundary that does not say how big the conversation was
        let ev = f.line(at("manual", "c2", None));
        assert_eq!(ev.last(), Some(&AgentEvent::Compacted { pre_tokens: None }));
        // the context itself is the next call's to say
        assert!(contexts(ev).is_empty());
        // the transcript on disk spells it its own way
        let ev = f.line(
            json!({"type":"system","subtype":"compact_boundary","session_id":"s","uuid":"c3",
            "compactMetadata":{"trigger":"auto","preTokens":310172,"postTokens":31302}}),
        );
        assert_eq!(
            ev.last(),
            Some(&AgentEvent::Compacted {
                pre_tokens: Some(310_172)
            })
        );
    }

    fn last_line(out: &Out) -> &[u8] {
        out.stdin
            .split(|b| *b == b'\n')
            .rfind(|l| !l.is_empty())
            .unwrap()
    }

    /// Claude 2.1.319: `task_started` says which call waits in the foreground, `task_updated`
    /// (after a `background_tasks` request) that it moved; the call's row sees both, and the
    /// request itself is the control line, with or without a call id.
    #[test]
    fn a_foreground_task_can_be_backgrounded() {
        let mut f = Feed::new();
        f.d.on_start(&Launch::default(), &mut Out::default());
        f.line(json!({"type":"control_response","response":{"subtype":"success","request_id":INIT_ID,"response":{}}}));
        f.line(
            json!({"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[
            {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"sleep 45"}}]},
            "parent_tool_use_id":null,"uuid":"u1"}),
        );
        let mut out = Out::default();
        assert!(
            matches!(f.d.background(None, &mut out), Err(Error::State(_))),
            "no task yet"
        );
        let ev = f.line(
            json!({"type":"system","subtype":"task_started","task_id":"t1","tool_use_id":"toolu_1",
            "description":"Sleep","is_backgrounded":false,"task_type":"local_bash","uuid":"u2"}),
        );
        match &ev[..] {
            [AgentEvent::ItemUpdated { id, body }] => {
                assert_eq!(id, "toolu_1");
                assert_eq!(body["task"], "foreground", "{body}");
                assert_eq!(body["name"], "Bash");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            f.d.background(Some("toolu_9"), &mut out),
            Err(Error::State(_))
        ));
        f.d.background(Some("toolu_1"), &mut out).unwrap();
        let line: Value = serde_json::from_slice(last_line(&out)).unwrap();
        assert_eq!(line["type"], "control_request");
        assert!(line["request_id"]
            .as_str()
            .unwrap()
            .starts_with("femme-bg-"));
        assert_eq!(
            line["request"],
            json!({"subtype":"background_tasks","tool_use_id":"toolu_1"})
        );
        f.d.background(None, &mut out).unwrap();
        let line: Value = serde_json::from_slice(last_line(&out)).unwrap();
        assert_eq!(line["request"], json!({"subtype":"background_tasks"}));

        let ev = f.line(json!({"type":"system","subtype":"task_updated","task_id":"t1","patch":{"is_backgrounded":true},"uuid":"u3"}));
        match &ev[..] {
            [AgentEvent::ItemUpdated { id, body }] => {
                assert_eq!(id, "toolu_1");
                assert_eq!(body["task"], "background", "{body}");
            }
            other => panic!("{other:?}"),
        }
        // said again by the task list: nothing new
        assert!(f
            .line(json!({"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"t1"}],"uuid":"u4"}))
            .is_empty());
        assert!(
            matches!(f.d.background(None, &mut out), Err(Error::State(_))),
            "nothing left in the foreground"
        );
        // the result keeps the state on the completed call
        let ev = f.line(json!({"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result",
            "content":"Command was manually backgrounded by user with ID: t1.","is_error":false}]},"parent_tool_use_id":null,"uuid":"u5"}));
        let call = ev
            .iter()
            .find_map(|e| match e {
                AgentEvent::ItemCompleted { id, body, .. } if id == "toolu_1" => Some(body.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(call["task"], "background", "{call}");
        let ev = f.line(json!({"type":"system","subtype":"task_notification","task_id":"t1","tool_use_id":"toolu_1",
            "status":"completed","summary":"Sleep","uuid":"u6"}));
        assert!(
            matches!(
                &ev[..],
                [AgentEvent::ItemCompleted {
                    kind: ItemKind::Event,
                    ..
                }]
            ),
            "{ev:?}"
        );
        assert!(f.d.tasks.is_empty());
    }

    /// A pane's transcript never carries a `control_request`: the mirror asks on the
    /// AskUserQuestion call. ExitPlanMode is its Plan item only, the pane's own dialog is asked
    /// from the screen.
    #[test]
    fn a_mirror_asks_on_the_ask_user_question_call_itself() {
        let run = |case: &str, mut d: ClaudeDriver, take: usize| {
            let text = std::fs::read_to_string(format!("fixtures/claude/{case}.jsonl")).unwrap();
            let mut ev = Vec::new();
            for (i, l) in text.lines().enumerate().take(take) {
                ev.extend(d.on_line(i as u64, l.as_bytes(), &mut Out::default()));
            }
            ev
        };
        let asked = |ev: &[AgentEvent]| {
            ev.iter()
                .filter(|e| matches!(e, AgentEvent::QuestionAsked { .. }))
                .count()
        };
        assert_eq!(asked(&run("question", ClaudeDriver::new(), 42)), 0);
        assert_eq!(asked(&run("question", ClaudeDriver::mirror(), 42)), 1);
        // the plan capture without its control_request lines, as a pane's transcript has it
        let text = std::fs::read_to_string("fixtures/claude/plan.jsonl").unwrap();
        let mut d = ClaudeDriver::mirror();
        let mut ev = Vec::new();
        for (i, l) in text.lines().enumerate() {
            if !l.contains("\"control_request\"") {
                ev.extend(d.on_line(i as u64, l.as_bytes(), &mut Out::default()));
            }
        }
        assert_eq!(asked(&ev), 0, "ExitPlanMode asks nothing in a mirror");
        let plans: Vec<_> = ev
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ItemCompleted { id, kind, .. } => Some((id.clone(), *kind)),
                _ => None,
            })
            .filter(|(_, k)| *k == ItemKind::Plan)
            .collect();
        assert_eq!(plans.len(), 1, "{ev:?}");
        // and its result does not turn the Plan item into a tool call
        assert!(!ev.iter().any(|e| matches!(e,
            AgentEvent::ItemCompleted { id, kind: ItemKind::ToolCall | ItemKind::ToolResult, .. }
            if id.starts_with(plans[0].0.as_str()))));
    }
}
