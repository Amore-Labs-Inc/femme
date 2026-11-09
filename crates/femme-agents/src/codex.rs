//! Codex driver: one `codex app-server` process per session, JSON-RPC over stdio. One JSON object
//! per line; requests carry `id` + `method` (+ `params`), responses `id` + `result|error`,
//! notifications only `method`; there is no `"jsonrpc"` member. Verified against codex-cli
//! 0.163.0; fixtures in `fixtures/codex/`, the method index in `fixtures/codex/schema.json`
//! (`codex app-server generate-json-schema`), findings in `NOTES.md` ("## Codex").
//!
//! # Flow
//! * [`Driver::on_start`] sends `initialize` (opting out of the notifications in [`OPT_OUT`]).
//!   Its response triggers the `initialized` notification and `thread/start` (or `thread/resume`
//!   when [`Launch::resume`] holds a thread id), both with `approvalPolicy: "never"` and
//!   `sandbox: "danger-full-access"`. The thread response makes the session ready:
//!   [`AgentEvent::Started`], then the first buffered prompt goes out. A `thread/resume` response
//!   carries the whole history; every item in it is re-emitted as `ItemCompleted` (backfill after
//!   a TUI takeover: ids are Codex's own, so the daemon's upsert makes it idempotent).
//! * A prompt is `turn/start {threadId, input}`. Codex does not queue: a second `turn/start`
//!   during a turn is injected into the running turn (a steer). So `Queue` is buffered here and
//!   sent when `turn/completed` arrives, one turn per prompt. `Steer` is `turn/steer` with
//!   `expectedTurnId`; [`Driver::interrupt`] is `turn/interrupt {threadId, turnId}`.
//! * Questions are server *requests* `item/tool/requestUserInput` (the `request_user_input`
//!   tool, which [`FLAGS`] enables outside plan mode). Each question in the request becomes one
//!   [`AgentEvent::QuestionAsked`]; once all are answered the driver writes the JSON-RPC response
//!   `{answers: {<question id>: {answers: [...]}}}`. `serverRequest/resolved` closes them.
//! * Approvals are off ([`FLAGS`] and the thread params): no `*/requestApproval` arrives for
//!   shell commands or file writes. Should one come anyway it becomes an Accept/Decline question
//!   instead of a turn that hangs forever.
//! * Usage: `thread/tokenUsage/updated` arrives after every model call; the `last` breakdowns of
//!   a turn add up to its [`Usage`] in `TurnEnded`. Rate limits: `account/rateLimits/updated`
//!   (after every model call) and [`CodexDriver::read_rate_limits`] become
//!   [`AgentEvent::RateLimit`].
//!
//! # Ids
//! Item ids are Codex's (`msg_...`, `exec-...`, UUIDv7 for user messages); a tool item `X` yields
//! `X` (`ToolCall`) and `X:result` (`ToolResult`). A question's `request_id` (and item id) is the
//! `itemId` of its tool call (`call_...`, `<itemId>#<n>` when one request asks several questions),
//! not the JSON-RPC id: those restart at 0 in every app-server process, so they would collide
//! inside one FEMME session after a takeover or restart. Notices without an id use `o<offset>`.
//! The driver's own JSON-RPC ids (its requests, not Codex's) are `n` for the first process of a
//! session and `(epoch << 32) + n` after a re-attach, `epoch` being [`Attach::epoch`].
//!
//! # Daemon restart (re-attach)
//! The daemon calls [`Driver::attach`] on a fresh driver (not [`Driver::on_start`]) and replays the
//! spool. A driver assumes a running, initialised process, and learns the rest from `attach`
//! (committed offset, thread id, answers already given) and from the replayed lines.
//! * **Ids.** The ids of two incarnations never meet (see above), and responses are matched to
//!   requests **by id and nothing else**: a replayed response to a request this incarnation did not
//!   send (the old driver's `turn/start`, `account/rateLimits/read`, ...) is ignored, not read by
//!   its shape. Only a driver that never ran `on_start` accepts two kinds of stray response: a
//!   `thread/start` / `thread/resume` result while it does not know the thread yet (the thread id,
//!   plus the history of a resume), and an `initialize` result while it knows neither the thread
//!   nor that the handshake is over, which means the previous daemon died inside the handshake:
//!   the driver finishes it (`initialized`, then `thread/start` or `thread/resume`).
//! * **Thread.** Without a thread id from `attach` the driver takes it from a replayed response or
//!   notification; when the spool is replayed from its end (idle session) the first prompt asks
//!   `thread/loaded/list` and takes the oldest loaded thread (Codex ids are UUIDv7, the session's
//!   own thread predates its sub-agents).
//! * **Questions.** A replayed request whose question id is in [`Attach::answers`] is not asked
//!   again: no [`AgentEvent::QuestionAsked`], no `Blocked`, no `QuestionResolved` when it closes,
//!   and [`Driver::answer`] refuses it. The response is not written again (the previous daemon wrote
//!   it before it persisted the answer). A request with several questions, some answered, asks
//!   only the rest.
//! * **Unknown requests.** A server request the driver does not serve (any method but the ones in
//!   [`HANDLED`]) is answered with the JSON-RPC error -32601 and still becomes [`AgentEvent::Raw`].
//!   Replayed, the reply goes out again; an agent that stopped waiting drops it.
//! * **Held prompts** are in driver memory only: see [`Driver::held_prompts`]. Those written to
//!   Codex whose `turn/start` or `turn/steer` response never came are announced as
//!   [`AgentEvent::PromptDropped`] when the process exits, and a `turn/start` Codex refuses is
//!   announced with its error.

use std::collections::{BTreeMap, VecDeque};

use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::{Agent, ItemKind, ItemStatus, Question, Usage};
use serde_json::{json, Map, Value};

use crate::common::{drop_held, held, hold, parse, preview, raw_text, set_status, Offsets};
use crate::items::{self, body};
use crate::{
    AgentEvent, AgentStatus, Attach, Driver, DropReason, Error, HeldPrompt, Launch, Out, Spawn,
    StopReason,
};

/// Program looked up on PATH.
pub const PROGRAM: &str = "codex";

/// Flags on both the headless and the TUI command line: approvals and sandbox off (FEMME runs
/// "bypass everywhere"), the `request_user_input` tool outside plan mode (an under-development
/// feature in 0.163, so its startup warning is switched off too).
pub const FLAGS: &[&str] = &[
    "-c",
    "approval_policy=\"never\"",
    "-c",
    "sandbox_mode=\"danger-full-access\"",
    "--enable",
    "default_mode_request_user_input",
    "-c",
    "suppress_unstable_features_warning=true",
];

/// Notifications the driver opts out of at `initialize` (`capabilities.optOutNotificationMethods`).
/// None is mapped, and some are big: `turn/diff/updated` repeats the whole turn diff after every
/// edit, `turn/moderationMetadata` is ~2 KiB per model call, output deltas duplicate
/// `aggregatedOutput`, `remoteControl/status/changed` carries the installation id.
pub const OPT_OUT: &[&str] = &[
    "turn/moderationMetadata",
    "turn/diff/updated",
    "thread/readState/changed",
    "mcpServer/startupStatus/updated",
    "remoteControl/status/changed",
    "account/updated",
    "item/commandExecution/outputDelta",
    "item/fileChange/outputDelta",
    "item/fileChange/patchUpdated",
    "hook/started",
    "hook/completed",
];

/// Methods the driver sends (client requests, plus the `initialized` notification).
pub const SENT: &[&str] = &[
    "initialize",
    "initialized",
    "thread/start",
    "thread/resume",
    "turn/start",
    "turn/steer",
    "turn/interrupt",
    "thread/loaded/list",
    "account/rateLimits/read",
];

/// Server notifications and requests that produce events.
pub const HANDLED: &[&str] = &[
    "thread/started",
    "turn/started",
    "turn/completed",
    "item/started",
    "item/completed",
    "item/agentMessage/delta",
    "item/reasoning/summaryTextDelta",
    "item/plan/delta",
    "turn/plan/updated",
    "thread/tokenUsage/updated",
    "account/rateLimits/updated",
    "serverRequest/resolved",
    "model/rerouted",
    "error",
    "warning",
    "configWarning",
    "item/tool/requestUserInput",
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
];

/// Server notifications that are known and produce no events (`deprecationNotice` is logged);
/// anything not in [`HANDLED`] or here becomes [`AgentEvent::Raw`].
pub const IGNORED: &[&str] = &[
    "deprecationNotice",
    "thread/status/changed",
    "thread/settings/updated",
    "thread/name/updated",
    "thread/goal/updated",
    "thread/goal/cleared",
    "thread/compacted",
    "thread/closed",
    "thread/archived",
    "thread/unarchived",
    "thread/queue/changed",
    "thread/readState/changed",
    "skills/changed",
    "turn/diff/updated",
    "turn/moderationMetadata",
    "item/reasoning/summaryPartAdded",
    "item/reasoning/textDelta",
    "item/commandExecution/outputDelta",
    "item/commandExecution/terminalInteraction",
    "item/fileChange/outputDelta",
    "item/fileChange/patchUpdated",
    "item/mcpToolCall/progress",
    "hook/started",
    "hook/completed",
    "mcpServer/startupStatus/updated",
    "remoteControl/status/changed",
    "account/updated",
    "app/list/updated",
    "fs/changed",
];

/// A request we sent and whose response is still due.
#[derive(Debug, Clone)]
enum Pending {
    Init,
    Thread(&'static str),
    /// `held`: the prompt it carries, announced if Codex refuses it or the process dies first.
    TurnStart {
        model: Option<String>,
        held: Option<HeldPrompt>,
    },
    Steer(HeldPrompt),
    Interrupt,
    LoadedList,
    RateLimits,
}

/// One question of a server request.
#[derive(Debug, Clone)]
struct Slot {
    request_id: String,
    /// Codex's question id (key of the answers map); empty for approvals.
    qid: String,
    options: Vec<String>,
    free_text: bool,
    /// The answer strings once given (`Some(vec![])` = cancelled).
    answer: Option<Vec<String>>,
    /// The answer as the caller gave it, for [`Driver::pending_answers`].
    given: Option<Answer>,
    /// Answered before a restart ([`Attach::answers`]): the replay says nothing about it.
    restored: bool,
}

/// A server request waiting for the user.
#[derive(Debug, Clone)]
struct Ask {
    rpc_id: Value,
    approval: bool,
    slots: Vec<Slot>,
    sent: bool,
}

/// An item between `item/started` and `item/completed`.
#[derive(Debug, Clone)]
struct Open {
    kind: ItemKind,
    text: String,
    call: Option<items::ToolCall>,
    /// Last reasoning summary part streamed (parts are joined with a blank line).
    part: i64,
}

/// A Codex thread item in FEMME terms.
struct Mapped {
    id: String,
    kind: ItemKind,
    text: String,
    tool: Option<Tool>,
}

struct Tool {
    call: items::ToolCall,
    preview: String,
    output: String,
    failed: bool,
}

/// Driver for `codex app-server` (see the module docs).
#[derive(Debug, Default)]
pub struct CodexDriver {
    off: Offsets,
    next_off: u64,
    next_id: u64,
    /// `on_start` ran on this instance (a fresh process).
    started: bool,
    /// `attach` ran on this instance (re-attached after a restart).
    attached: bool,
    /// The process exited: every call that would write is refused.
    exited: bool,
    /// [`Attach::epoch`]: the high half of every request id this instance sends.
    epoch: u32,
    /// The `initialize` response arrived.
    inited: bool,
    /// `on_start` ran and the thread response has not arrived yet.
    handshake: bool,
    launch: Launch,
    pending: BTreeMap<u64, Pending>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    last_turn: Option<String>,
    turn_running: bool,
    turn_start_inflight: bool,
    interrupt_wanted: bool,
    resolving: bool,
    want_limits: bool,
    queue: VecDeque<HeldPrompt>,
    /// Steers Codex turned down because the turn was already over. They run before `queue`, in the
    /// order they were given (a `push_front` per refusal would reverse two of them).
    retry: VecDeque<HeldPrompt>,
    /// Answers given before the restart, taken by the replayed question they belong to.
    restored: BTreeMap<String, Answer>,
    model_override: Option<String>,
    usage: Option<Usage>,
    asks: Vec<Ask>,
    open: BTreeMap<String, Open>,
    /// Last `(percent, resets_at)` per rate-limit kind: updates repeat after every model call.
    limits: BTreeMap<String, (f32, Option<i64>)>,
    status: AgentStatus,
}

impl CodexDriver {
    /// New driver; call [`Driver::on_start`] for a fresh process, or [`Driver::attach`] and then
    /// feed the spool to re-attach (see the module docs).
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask for the account's usage windows with `account/rateLimits/read` (no model turn). The
    /// response becomes [`AgentEvent::RateLimit`] events, one per window. Called before the
    /// `initialize` response arrived, the request goes out right after it.
    pub fn read_rate_limits(&mut self, out: &mut Out) {
        if self.exited {
            return;
        }
        if self.handshake && !self.inited {
            self.want_limits = true;
        } else {
            self.send(out, "account/rateLimits/read", None, Pending::RateLimits);
        }
    }

    fn alloc(&mut self) -> u64 {
        if !self.started && !self.attached {
            // ponytail: a driver that was neither started nor attached (a bare replay) numbers from
            // the spool offset so it cannot reuse an id an earlier driver may still wait on. The
            // daemon gives `attach` an epoch instead, which is exact.
            self.next_id = self.next_id.max(self.next_off);
        }
        self.next_id = self.next_id.max(1);
        let id = self.next_id;
        self.next_id += 1;
        // the epoch keeps ids of two incarnations apart; 31 bits so the id stays a positive i64
        (u64::from(self.epoch & 0x7fff_ffff) << 32) + id
    }

    fn send(&mut self, out: &mut Out, method: &str, params: Option<Value>, p: Pending) {
        let id = self.alloc();
        let mut m = Map::new();
        m.insert("id".into(), id.into());
        m.insert("method".into(), method.into());
        if let Some(params) = params {
            m.insert("params".into(), params);
        }
        out.line(&Value::Object(m));
        self.pending.insert(id, p);
    }

    fn busy(&self) -> bool {
        self.turn_running || self.turn_start_inflight || self.turn_id.is_some()
    }

    /// Whether a notification's `threadId` is this session's thread (or cannot be told).
    fn ours(&self, p: &Value) -> bool {
        match (p["threadId"].as_str(), &self.thread_id) {
            (Some(t), Some(id)) => t == id,
            _ => true,
        }
    }

    fn thread_params(&self) -> Value {
        let mut p = json!({"cwd": self.launch.cwd, "approvalPolicy": "never", "sandbox": "danger-full-access"});
        if let Some(m) = &self.launch.model {
            p["model"] = json!(m);
        }
        p
    }

    fn start_turn(&mut self, thread: &str, h: HeldPrompt, out: &mut Out) {
        let mut p = json!({"threadId": thread, "input": [{"type": "text", "text": h.text}]});
        let model = self.model_override.take();
        if let Some(m) = &model {
            p["model"] = json!(m);
        }
        self.turn_start_inflight = true;
        let held = Some(h);
        self.send(
            out,
            "turn/start",
            Some(p),
            Pending::TurnStart { model, held },
        );
    }

    /// Send the next buffered prompt when the session is ready and idle.
    fn flush(&mut self, out: &mut Out) {
        if self.handshake || self.busy() {
            return;
        }
        let Some(thread) = self.thread_id.clone() else {
            return;
        };
        if let Some(h) = self.retry.pop_front().or_else(|| self.queue.pop_front()) {
            self.start_turn(&thread, h, out);
        }
    }

    /// The handshake after an `initialize` result: `initialized`, then the thread.
    fn after_init(&mut self, out: &mut Out) {
        self.inited = true;
        out.line(&json!({"method": "initialized"}));
        let mut p = self.thread_params();
        let method = match self.launch.resume.clone() {
            Some(t) => {
                p["threadId"] = json!(t);
                "thread/resume"
            }
            None => "thread/start",
        };
        self.send(out, method, Some(p), Pending::Thread(method));
        if std::mem::take(&mut self.want_limits) {
            self.send(out, "account/rateLimits/read", None, Pending::RateLimits);
        }
    }

    /// Answer a server request we do not serve, so the agent does not wait for it forever.
    fn refuse(out: &mut Out, id: &Value, code: i64, message: &str) {
        out.line(&json!({"id": id, "error": {"code": code, "message": message}}));
    }

    /// Re-attached and the thread id is still unknown: ask which thread is loaded.
    fn resolve_thread(&mut self, out: &mut Out) {
        if !self.handshake && self.thread_id.is_none() && !self.resolving && !self.queue.is_empty()
        {
            self.resolving = true;
            self.send(
                out,
                "thread/loaded/list",
                Some(json!({})),
                Pending::LoadedList,
            );
        }
    }

    fn interrupt_now(&mut self, out: &mut Out) -> bool {
        match (self.thread_id.clone(), self.turn_id.clone()) {
            (Some(thread), Some(turn)) => {
                self.send(
                    out,
                    "turn/interrupt",
                    Some(json!({"threadId": thread, "turnId": turn})),
                    Pending::Interrupt,
                );
                true
            }
            _ => false,
        }
    }

    fn learn_turn(&mut self, turn: &str, out: &mut Out) {
        if self.last_turn.as_deref() == Some(turn) {
            return; // a late response for a turn that already completed
        }
        self.turn_id = Some(turn.to_owned());
        if self.interrupt_wanted {
            self.interrupt_wanted = false;
            self.interrupt_now(out);
        }
    }

    /// Pass on the `RateLimit` events whose value changed since the last one of that kind.
    fn changed_limits(&mut self, fresh: Vec<AgentEvent>, ev: &mut Vec<AgentEvent>) {
        for e in fresh {
            if let AgentEvent::RateLimit {
                kind,
                percent,
                resets_at,
            } = &e
            {
                if self.limits.insert(kind.clone(), (*percent, *resets_at))
                    == Some((*percent, *resets_at))
                {
                    continue;
                }
            }
            ev.push(e);
        }
    }

    fn notice(id: String, failed: bool, text: String, ev: &mut Vec<AgentEvent>) {
        let status = if failed {
            ItemStatus::Failed
        } else {
            ItemStatus::Completed
        };
        ev.push(AgentEvent::ItemCompleted {
            id,
            kind: ItemKind::Event,
            status,
            body: body(&items::Event { text, task: None }),
            parent: None,
        });
    }

    fn on_response(&mut self, offset: u64, v: &Value, out: &mut Out, ev: &mut Vec<AgentEvent>) {
        let req = v["id"].as_u64().and_then(|id| self.pending.remove(&id));
        if let Some(err) = v.get("error") {
            let msg = err["message"].as_str().unwrap_or("error").to_owned();
            match req {
                Some(Pending::Init) => Self::notice(
                    format!("o{offset}"),
                    true,
                    format!("codex initialize failed: {msg}"),
                    ev,
                ),
                Some(Pending::Thread("thread/resume")) => {
                    // the rollout is gone or unreadable: carry on in a new thread (Started says which)
                    let text = format!("codex thread/resume failed: {msg}; starting a new thread");
                    Self::notice(format!("o{offset}"), true, text, ev);
                    let p = self.thread_params();
                    self.send(
                        out,
                        "thread/start",
                        Some(p),
                        Pending::Thread("thread/start"),
                    );
                }
                Some(Pending::Thread(m)) => Self::notice(
                    format!("o{offset}"),
                    true,
                    format!("codex {m} failed: {msg}"),
                    ev,
                ),
                Some(Pending::TurnStart { held, .. }) => {
                    self.turn_start_inflight = false;
                    Self::notice(
                        format!("o{offset}"),
                        true,
                        format!("codex turn/start failed: {msg}"),
                        ev,
                    );
                    if let Some(h) = held {
                        ev.push(AgentEvent::PromptDropped {
                            text: h.text,
                            how: h.how,
                            reason: DropReason::Refused(msg),
                            id: h.id,
                        });
                    }
                    self.flush(out);
                }
                Some(Pending::Steer(h)) => {
                    // the turn ended before the steer landed: run it as the next turn, ahead of
                    // the prompts that were only queued (it is the more urgent one)
                    self.retry.push_back(h);
                    self.flush(out);
                }
                Some(Pending::Interrupt) => {}
                Some(Pending::LoadedList) => {
                    self.resolving = false;
                    Self::notice(
                        format!("o{offset}"),
                        true,
                        format!("codex thread/loaded/list failed: {msg}"),
                        ev,
                    );
                }
                Some(Pending::RateLimits) | None => ev.push(AgentEvent::Raw(v.clone())),
            }
            return;
        }
        let r = &v["result"];
        match req {
            Some(Pending::Init) => self.after_init(out),
            Some(Pending::TurnStart { model, .. }) => {
                self.turn_start_inflight = false;
                if let Some(m) = model {
                    ev.push(AgentEvent::ModelChanged(m));
                }
                if let Some(t) = r["turn"]["id"].as_str() {
                    self.learn_turn(t, out);
                }
            }
            Some(Pending::LoadedList) => {
                self.resolving = false;
                let oldest = r["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .min();
                match oldest {
                    Some(t) => {
                        self.thread_id = Some(t.to_owned());
                        self.flush(out);
                    }
                    None => Self::notice(
                        format!("o{offset}"),
                        true,
                        "codex has no loaded thread".into(),
                        ev,
                    ),
                }
            }
            Some(Pending::RateLimits) => {
                let mut fresh = Vec::new();
                rate_limits_read(r, &mut fresh);
                self.changed_limits(fresh.clone(), &mut Vec::new());
                ev.extend(fresh); // asked for: always answered
            }
            Some(Pending::Steer(_) | Pending::Interrupt) => {}
            Some(Pending::Thread(_)) => {
                if r["thread"]["id"].is_string() {
                    self.on_thread(r, ev);
                    self.flush(out);
                }
            }
            // Not the answer to anything this incarnation sent: a replayed response. Never read
            // by its shape (a stale `turn/start` result would make the driver think a finished
            // turn runs, a stale rate-limit read would show old numbers as new); only a driver
            // that did not start the process takes the two replies below (see the module docs; the
            // first is only for an attached one, a bare replay never writes).
            None if !self.started => {
                if self.attached
                    && r["userAgent"].is_string()
                    && !self.inited
                    && self.thread_id.is_none()
                {
                    // ponytail: if the previous daemon had already sent `thread/start` (its reply
                    // sits later in the spool) this asks for a second, empty thread and the reply
                    // to that one wins; narrow (the daemon died within ms of the handshake) and
                    // harmless (no prompt ever ran in either). Exact would be an end-of-replay signal.
                    self.handshake = true;
                    self.after_init(out);
                } else if r["thread"]["id"].is_string() && self.thread_id.is_none() {
                    self.on_thread(r, ev);
                    self.flush(out);
                }
            }
            None => {}
        }
    }

    /// `thread/start` / `thread/resume` response: the session is ready.
    fn on_thread(&mut self, r: &Value, ev: &mut Vec<AgentEvent>) {
        let Some(id) = r["thread"]["id"].as_str() else {
            return;
        };
        self.thread_id = Some(id.to_owned());
        self.handshake = false;
        self.resolving = false;
        ev.push(AgentEvent::Started {
            agent_sid: id.to_owned(),
            model: r["model"].as_str().map(str::to_owned),
        });
        for turn in r["thread"]["turns"].as_array().into_iter().flatten() {
            for item in turn["items"].as_array().into_iter().flatten() {
                self.item_completed(item, ev);
            }
        }
    }

    fn on_request(
        &mut self,
        offset: u64,
        method: &str,
        v: &Value,
        out: &mut Out,
        ev: &mut Vec<AgentEvent>,
    ) {
        let p = &v["params"];
        let rpc_id = v["id"].clone();
        if self.asks.iter().any(|a| a.rpc_id == rpc_id) {
            return;
        }
        let base = p["itemId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map_or_else(|| format!("o{offset}"), str::to_owned);
        let (approval, mut slots) = match method {
            "item/tool/requestUserInput" => {
                let qs = p["questions"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let slots = qs
                    .iter()
                    .enumerate()
                    .map(|(i, q)| {
                        let options: Vec<String> = q["options"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|o| o["label"].as_str().map(str::to_owned))
                            .collect();
                        let prompt = q["question"]
                            .as_str()
                            .or(q["header"].as_str())
                            .unwrap_or_default()
                            .to_owned();
                        let slot = Slot {
                            request_id: if qs.len() == 1 {
                                base.clone()
                            } else {
                                format!("{base}#{i}")
                            },
                            qid: q["id"]
                                .as_str()
                                .map_or_else(|| i.to_string(), str::to_owned),
                            free_text: q["isOther"].as_bool().unwrap_or(false)
                                || options.is_empty(),
                            options,
                            answer: None,
                            given: None,
                            restored: false,
                        };
                        (slot, prompt)
                    })
                    .collect::<Vec<_>>();
                (false, slots)
            }
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                let what = match p["command"].as_str() {
                    Some(c) => format!("Allow Codex to run `{c}`?"),
                    None if method.starts_with("item/fileChange") => {
                        "Allow Codex to change files?".to_owned()
                    }
                    None => "Allow Codex to run a command?".to_owned(),
                };
                let prompt = match p["reason"].as_str().filter(|r| !r.is_empty()) {
                    Some(r) => format!("{what}\n{r}"),
                    None => what,
                };
                let request_id = match p["approvalId"].as_str() {
                    Some(a) => format!("{base}:{a}"),
                    None => format!("{base}:approval"),
                };
                let slot = Slot {
                    request_id,
                    qid: String::new(),
                    options: vec!["Accept".into(), "Decline".into()],
                    free_text: false,
                    answer: None,
                    given: None,
                    restored: false,
                };
                (true, vec![(slot, prompt)])
            }
            _ => {
                // nobody else will answer it, and Codex waits for the reply
                let why = format!("femme does not handle {method}");
                Self::refuse(out, &rpc_id, -32601, &why);
                ev.push(AgentEvent::Raw(v.clone()));
                return;
            }
        };
        if slots.is_empty() {
            Self::refuse(out, &rpc_id, -32602, "no question to ask");
            ev.push(AgentEvent::Raw(v.clone()));
            return;
        }
        // questions the previous daemon already answered stay answered
        for (s, _) in &mut slots {
            let Some(a) = self.restored.remove(&s.request_id) else {
                continue;
            };
            if let Ok(strings) = slot_answer(s, &a) {
                s.answer = Some(strings);
                s.given = Some(a);
                s.restored = true;
            }
        }
        let answered = slots.iter().all(|(s, _)| s.answer.is_some());
        let mut kept = Vec::with_capacity(slots.len());
        for (s, prompt) in slots {
            self.off.item_open(&s.request_id, offset);
            if !s.restored {
                let q = Question {
                    request_id: s.request_id.clone(),
                    prompt,
                    options: s.options.clone(),
                    multi: false,
                    free_text: s.free_text,
                };
                ev.push(AgentEvent::QuestionAsked {
                    item_id: s.request_id.clone(),
                    q,
                });
            }
            kept.push(s);
        }
        if !answered {
            set_status(&mut self.status, AgentStatus::Blocked, ev);
        }
        self.asks.push(Ask {
            rpc_id,
            approval,
            slots: kept,
            sent: answered, // all restored: the response went out before the restart
        });
    }

    fn resolve_ask(&mut self, i: usize, ev: &mut Vec<AgentEvent>) {
        let ask = self.asks.remove(i);
        for s in ask.slots {
            self.off.item_close(&s.request_id);
            if !s.restored {
                ev.push(AgentEvent::QuestionResolved {
                    request_id: s.request_id,
                });
            }
        }
    }

    fn resolve_all(&mut self, ev: &mut Vec<AgentEvent>) {
        while !self.asks.is_empty() {
            self.resolve_ask(0, ev);
        }
    }

    /// Open items become `Failed` (turn ended or process gone).
    fn close_open(&mut self, ev: &mut Vec<AgentEvent>) {
        for (id, o) in std::mem::take(&mut self.open) {
            self.off.item_close(&id);
            let b = match &o.call {
                Some(call) => body(call),
                None => text_body(o.kind, o.text),
            };
            ev.push(AgentEvent::ItemCompleted {
                id,
                kind: o.kind,
                status: ItemStatus::Failed,
                body: b,
                parent: None,
            });
        }
    }

    fn item_started(&mut self, offset: u64, item: &Value, ev: &mut Vec<AgentEvent>) -> bool {
        let Some(m) = map_item(item) else {
            return false;
        };
        self.off.item_open(&m.id, offset);
        match m.tool {
            Some(t) => {
                ev.push(AgentEvent::ItemStarted {
                    id: m.id.clone(),
                    kind: ItemKind::ToolCall,
                    preview: preview(&t.preview),
                    parent: None,
                });
                ev.push(AgentEvent::ItemUpdated {
                    id: m.id.clone(),
                    body: body(&t.call),
                });
                self.open.insert(
                    m.id,
                    Open {
                        kind: ItemKind::ToolCall,
                        text: String::new(),
                        call: Some(t.call),
                        part: 0,
                    },
                );
            }
            None => {
                ev.push(AgentEvent::ItemStarted {
                    id: m.id.clone(),
                    kind: m.kind,
                    preview: preview(&m.text),
                    parent: None,
                });
                self.open.insert(
                    m.id,
                    Open {
                        kind: m.kind,
                        text: m.text,
                        call: None,
                        part: 0,
                    },
                );
            }
        }
        true
    }

    fn item_completed(&mut self, item: &Value, ev: &mut Vec<AgentEvent>) -> bool {
        let Some(m) = map_item(item) else {
            return false;
        };
        self.off.item_close(&m.id);
        self.open.remove(&m.id);
        match m.tool {
            Some(t) => {
                let status = if t.failed {
                    ItemStatus::Failed
                } else {
                    ItemStatus::Completed
                };
                let result = items::ToolResult {
                    call_id: m.id.clone(),
                    output: t.output,
                    is_error: t.failed,
                    link: None,
                };
                ev.push(AgentEvent::ItemCompleted {
                    id: m.id.clone(),
                    kind: ItemKind::ToolCall,
                    status,
                    body: body(&t.call),
                    parent: None,
                });
                ev.push(AgentEvent::ItemCompleted {
                    id: format!("{}:result", m.id),
                    kind: ItemKind::ToolResult,
                    status,
                    body: body(&result),
                    parent: None,
                });
            }
            None => {
                ev.push(AgentEvent::ItemCompleted {
                    id: m.id,
                    kind: m.kind,
                    status: ItemStatus::Completed,
                    body: text_body(m.kind, m.text),
                    parent: None,
                });
            }
        }
        true
    }

    fn delta(&mut self, method: &str, p: &Value, ev: &mut Vec<AgentEvent>) {
        let (Some(id), Some(d)) = (p["itemId"].as_str(), p["delta"].as_str()) else {
            return;
        };
        if d.is_empty() {
            return;
        }
        let mut text = d.to_owned();
        if let Some(o) = self.open.get_mut(id) {
            if method == "item/reasoning/summaryTextDelta" {
                let part = p["summaryIndex"].as_i64().unwrap_or(0);
                if part > o.part && !o.text.is_empty() {
                    text.insert_str(0, "\n\n");
                }
                o.part = o.part.max(part);
            }
            o.text.push_str(&text);
        }
        ev.push(AgentEvent::ItemDelta {
            id: id.to_owned(),
            text,
        });
    }

    fn turn_completed(&mut self, p: &Value, out: &mut Out, ev: &mut Vec<AgentEvent>) {
        let turn = &p["turn"];
        let stop = match turn["status"].as_str().unwrap_or_default() {
            "completed" => StopReason::EndTurn,
            "interrupted" => StopReason::Interrupted,
            "failed" => StopReason::Error(
                turn["error"]["message"]
                    .as_str()
                    .unwrap_or("failed")
                    .to_owned(),
            ),
            other => StopReason::Other(other.to_owned()),
        };
        self.close_open(ev);
        self.resolve_all(ev);
        let usage = self.usage.take();
        // A completion without a started turn (a replay that began after `turn/started`, a
        // duplicate) closes what is open but ends nothing: `TurnEnded` always has its `TurnStarted`.
        if self.turn_running {
            ev.push(AgentEvent::TurnEnded {
                stop,
                usage,
                duration_ms: turn["durationMs"].as_u64(),
            });
        }
        set_status(&mut self.status, AgentStatus::Idle, ev);
        self.off.turn_end();
        self.last_turn = turn["id"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| self.turn_id.clone());
        self.turn_id = None;
        self.turn_running = false;
        self.interrupt_wanted = false;
        self.flush(out);
    }

    fn on_notification(
        &mut self,
        offset: u64,
        method: &str,
        v: &Value,
        out: &mut Out,
        ev: &mut Vec<AgentEvent>,
    ) {
        let p = &v["params"];
        match method {
            "turn/started" => {
                if !self.ours(p) {
                    return;
                }
                let Some(turn) = p["turn"]["id"].as_str() else {
                    ev.push(AgentEvent::Raw(v.clone()));
                    return;
                };
                if self.thread_id.is_none() {
                    self.thread_id = p["threadId"].as_str().map(str::to_owned);
                }
                self.turn_running = true;
                self.turn_start_inflight = false;
                self.usage = None;
                self.last_turn = None;
                self.off.turn_begin(offset);
                ev.push(AgentEvent::TurnStarted);
                set_status(&mut self.status, AgentStatus::Working, ev);
                self.learn_turn(turn, out);
            }
            "turn/completed" => {
                if self.ours(p) {
                    self.turn_completed(p, out, ev);
                }
            }
            "item/started" => {
                if self.ours(p) && !self.item_started(offset, &p["item"], ev) {
                    ev.push(AgentEvent::Raw(v.clone()));
                }
            }
            "item/completed" => {
                if self.ours(p) && !self.item_completed(&p["item"], ev) {
                    ev.push(AgentEvent::Raw(v.clone()));
                }
            }
            "item/agentMessage/delta" | "item/reasoning/summaryTextDelta" | "item/plan/delta" => {
                if self.ours(p) {
                    self.delta(method, p, ev);
                }
            }
            "thread/started" => {
                let t = &p["thread"];
                if self.thread_id.is_none() && t["parentThreadId"].is_null() {
                    if let Some(id) = t["id"].as_str() {
                        self.thread_id = Some(id.to_owned());
                        ev.push(AgentEvent::Started {
                            agent_sid: id.to_owned(),
                            model: t["model"].as_str().map(str::to_owned),
                        });
                    }
                }
            }
            "thread/tokenUsage/updated" => {
                if self.ours(p) {
                    let last = &p["tokenUsage"]["last"];
                    let n = |k: &str| last[k].as_u64().unwrap_or(0);
                    let u = self.usage.get_or_insert_with(Usage::default);
                    u.input = u
                        .input
                        .saturating_add(n("inputTokens").saturating_sub(n("cachedInputTokens")));
                    u.output = u.output.saturating_add(n("outputTokens"));
                    u.cache_read = u.cache_read.saturating_add(n("cachedInputTokens"));
                    u.cache_write = u.cache_write.saturating_add(n("cacheWriteInputTokens"));
                }
            }
            "account/rateLimits/updated" => {
                let mut fresh = Vec::new();
                rate_limits(&p["rateLimits"], &mut fresh);
                self.changed_limits(fresh, ev);
            }
            "serverRequest/resolved" => {
                if let Some(i) = self.asks.iter().position(|a| a.rpc_id == p["requestId"]) {
                    self.resolve_ask(i, ev);
                    if self.asks.is_empty() {
                        let s = if self.turn_running {
                            AgentStatus::Working
                        } else {
                            AgentStatus::Idle
                        };
                        set_status(&mut self.status, s, ev);
                    }
                }
            }
            "model/rerouted" => {
                if let Some(m) = p["toModel"].as_str().filter(|_| self.ours(p)) {
                    ev.push(AgentEvent::ModelChanged(m.to_owned()));
                }
            }
            "turn/plan/updated" => {
                if self.ours(p) {
                    let turn = p["turnId"].as_str().unwrap_or("turn");
                    let text = plan_text(p);
                    ev.push(AgentEvent::ItemCompleted {
                        id: format!("{turn}:plan"),
                        kind: ItemKind::Plan,
                        status: ItemStatus::Completed,
                        body: body(&items::Plan { text }),
                        parent: None,
                    });
                }
            }
            "error" => {
                if self.ours(p) {
                    let msg = p["error"]["message"].as_str().unwrap_or("error");
                    let retry = p["willRetry"].as_bool().unwrap_or(false);
                    let text = if retry {
                        format!("{msg} (retrying)")
                    } else {
                        msg.to_owned()
                    };
                    Self::notice(format!("o{offset}"), !retry, text, ev);
                }
            }
            "warning" => {
                if self.ours(p) {
                    Self::notice(
                        format!("o{offset}"),
                        false,
                        p["message"].as_str().unwrap_or_default().to_owned(),
                        ev,
                    );
                }
            }
            // developer-facing (`thread/resume` with full history always sends one in 0.163)
            "deprecationNotice" => {
                tracing::warn!(summary = %p["summary"], "codex deprecation notice")
            }
            "configWarning" => {
                let mut text = p["summary"].as_str().unwrap_or_default().to_owned();
                if let Some(d) = p["details"].as_str().filter(|d| !d.is_empty()) {
                    text.push('\n');
                    text.push_str(d);
                }
                Self::notice(format!("o{offset}"), false, text, ev);
            }
            m if IGNORED.contains(&m) => {}
            _ => ev.push(AgentEvent::Raw(v.clone())),
        }
    }
}

impl Driver for CodexDriver {
    fn agent(&self) -> Agent {
        Agent::Codex
    }

    /// `codex app-server` + [`FLAGS`]; the model goes into the thread params, not the command.
    fn spawn(&self, l: &Launch) -> Spawn {
        let mut cmd = vec![PROGRAM.to_owned(), "app-server".to_owned()];
        cmd.extend(FLAGS.iter().map(|s| (*s).to_owned()));
        Spawn {
            cmd,
            env: l.extra_env.clone(),
            cwd: l.cwd.clone(),
        }
    }

    /// `codex resume [FLAGS] --no-daemon [-m <model>] <thread id>` (plain `codex` without a thread
    /// id). `--no-daemon` keeps the TUI's app-server inside the PTY's process tree instead of
    /// starting the shared background daemon, which would keep the thread loaded after takeover.
    fn tui(&self, l: &Launch) -> Option<Spawn> {
        let mut cmd = vec![PROGRAM.to_owned()];
        if l.resume.is_some() {
            cmd.push("resume".into());
        }
        cmd.extend(FLAGS.iter().map(|s| (*s).to_owned()));
        cmd.push("--no-daemon".into());
        if let Some(m) = &l.model {
            cmd.push("-m".into());
            cmd.push(m.clone());
        }
        if let Some(t) = &l.resume {
            cmd.push(t.clone());
        }
        Some(Spawn {
            cmd,
            env: l.extra_env.clone(),
            cwd: l.cwd.clone(),
        })
    }

    fn on_start(&mut self, l: &Launch, out: &mut Out) {
        self.started = true;
        self.handshake = true;
        self.launch = l.clone();
        let params = json!({
            "clientInfo": {"name": "femme", "title": "FEMME", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"optOutNotificationMethods": OPT_OUT},
        });
        self.send(out, "initialize", Some(params), Pending::Init);
    }

    /// Re-attach: the thread id is known from the start (no `thread/loaded/list` round trip),
    /// the request ids carry the epoch, and the answers given before the restart are taken by the
    /// replayed questions. `a.model` is not needed: Codex keeps the model on its side and a
    /// `turn/start` only carries one after [`Driver::set_model`]. See the module docs.
    fn attach(&mut self, l: &Launch, a: &Attach, _: &mut Out) {
        self.attached = true;
        self.launch = l.clone();
        // epoch 0 would reuse the first process's ids: an attach is always at least the first
        self.epoch = a.epoch.max(1);
        self.off.start_at(a.resume_offset);
        self.next_off = a.resume_offset;
        self.thread_id.clone_from(&a.agent_sid);
        self.restored = a.answers.iter().cloned().collect();
    }

    fn on_line(&mut self, offset: u64, line: &[u8], out: &mut Out) -> Vec<AgentEvent> {
        self.off.seen(offset, line);
        self.next_off = offset + line.len() as u64 + 1;
        let Some(v) = parse(line) else {
            return if line.iter().all(u8::is_ascii_whitespace) {
                Vec::new()
            } else {
                vec![raw_text(line)]
            };
        };
        let mut ev = Vec::new();
        match (v.get("method").and_then(Value::as_str), v.get("id")) {
            (Some(m), Some(_)) => self.on_request(offset, m, &v, out, &mut ev),
            (Some(m), None) => self.on_notification(offset, m, &v, out, &mut ev),
            (None, Some(_)) if v.get("result").is_some() || v.get("error").is_some() => {
                self.on_response(offset, &v, out, &mut ev)
            }
            _ => ev.push(AgentEvent::Raw(v)),
        }
        ev
    }

    /// `Queue`: buffered while a turn runs (Codex would steer a second `turn/start` into it).
    /// `Steer`: `turn/steer` into the running turn; when idle it starts a turn. If the turn ends
    /// before the steer lands, the text runs as the next turn, ahead of the held prompts and in the
    /// order the steers were given. `InterruptThenSend`: `turn/interrupt` and the text goes first in
    /// the queue, behind earlier `InterruptThenSend` prompts. Never `Unsupported`; after the process
    /// exited it is `Error::State`.
    fn prompt(&mut self, text: &str, how: PromptHow, out: &mut Out) -> Result<(), Error> {
        if self.exited {
            return Err(Error::State("codex has exited"));
        }
        let h = held(text, how);
        let ready = !self.handshake && self.thread_id.is_some();
        match how {
            PromptHow::Steer if ready && self.turn_running && self.turn_id.is_some() => {
                let (thread, turn) = (
                    self.thread_id.clone().unwrap_or_default(),
                    self.turn_id.clone().unwrap_or_default(),
                );
                let p = json!({"threadId": thread, "input": [{"type": "text", "text": text}], "expectedTurnId": turn});
                self.send(out, "turn/steer", Some(p), Pending::Steer(h));
                return Ok(());
            }
            PromptHow::Steer if ready && self.turn_start_inflight => {
                // the turn id is not known yet; a second turn/start lands in the starting turn
                let thread = self.thread_id.clone().unwrap_or_default();
                self.start_turn(&thread, h, out);
                return Ok(());
            }
            PromptHow::InterruptThenSend if self.busy() => {
                if !self.interrupt_now(out) {
                    self.interrupt_wanted = true;
                }
                hold(&mut self.queue, h);
            }
            _ => hold(&mut self.queue, h),
        }
        self.flush(out);
        self.resolve_thread(out);
        Ok(())
    }

    /// `turn/interrupt` for the running turn. Held prompts stay and run after it. With no turn
    /// there is nothing to stop: `Ok`, nothing written.
    fn interrupt(&mut self, out: &mut Out) -> Result<(), Error> {
        if self.exited {
            return Err(Error::State("codex has exited"));
        }
        if self.busy() && !self.interrupt_now(out) {
            self.interrupt_wanted = true;
        }
        Ok(())
    }

    /// Questions: `Option(i)`/`Options` send the option labels, `Text` the text (only when the
    /// question allows free text), `Cancel` leaves the question out of the answers map. The
    /// response goes out once every question of the request has an answer. Approvals:
    /// `Option(0)` accept, `Option(1)` decline, `Cancel` cancel.
    fn answer(&mut self, request_id: &str, answer: &Answer, out: &mut Out) -> Result<(), Error> {
        if self.exited {
            return Err(Error::State("codex has exited"));
        }
        let unknown = || Error::UnknownRequest(request_id.to_owned());
        let ask = self
            .asks
            .iter_mut()
            .find(|a| a.slots.iter().any(|s| s.request_id == request_id))
            .ok_or_else(unknown)?;
        if ask.sent {
            return Err(unknown());
        }
        let slot = ask
            .slots
            .iter_mut()
            .find(|s| s.request_id == request_id)
            .ok_or_else(unknown)?;
        if slot.answer.is_some() {
            return Err(unknown());
        }
        slot.answer = Some(slot_answer(slot, answer)?);
        slot.given = Some(answer.clone());
        if ask.slots.iter().all(|s| s.answer.is_some()) {
            let result = if ask.approval {
                let decision = match ask.slots[0].answer.as_deref() {
                    Some([a]) if a == "Accept" => "accept",
                    Some([d]) if d == "Decline" => "decline",
                    _ => "cancel",
                };
                json!({"decision": decision})
            } else {
                let mut answers = Map::new();
                for s in &ask.slots {
                    if let Some(a) = s.answer.as_ref().filter(|a| !a.is_empty()) {
                        answers.insert(s.qid.clone(), json!({"answers": a}));
                    }
                }
                json!({"answers": answers})
            };
            out.line(&json!({"id": ask.rpc_id, "result": result}));
            ask.sent = true;
        }
        Ok(())
    }

    /// Codex takes the model per turn: the next `turn/start` carries it (and it sticks for later
    /// turns); `ModelChanged` follows that request's response.
    fn set_model(&mut self, model: &str, _out: &mut Out) -> Result<(), Error> {
        if self.exited {
            return Err(Error::State("codex has exited"));
        }
        self.model_override = Some(model.to_owned());
        Ok(())
    }

    fn resume_offset(&self) -> u64 {
        self.off.resume()
    }

    fn held_prompts(&self) -> Vec<HeldPrompt> {
        self.retry.iter().chain(&self.queue).cloned().collect()
    }

    fn pending_answers(&self) -> Vec<(String, Answer)> {
        self.asks
            .iter()
            .flat_map(|a| &a.slots)
            .filter_map(|s| s.given.clone().map(|g| (s.request_id.clone(), g)))
            .collect()
    }

    fn agent_session_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    fn status(&self) -> AgentStatus {
        self.status
    }

    fn on_exit(&mut self, code: Option<i32>) -> Vec<AgentEvent> {
        let mut ev = Vec::new();
        self.exited = true;
        self.close_open(&mut ev);
        self.resolve_all(&mut ev);
        if self.turn_running {
            ev.push(AgentEvent::TurnEnded {
                stop: StopReason::Interrupted,
                usage: self.usage.take(),
                duration_ms: None,
            });
        }
        set_status(&mut self.status, AgentStatus::Idle, &mut ev);
        self.off.turn_end();
        self.turn_running = false;
        self.turn_start_inflight = false;
        self.turn_id = None;
        self.handshake = false;
        self.interrupt_wanted = false;
        // Prompts that never ran: first those already written whose reply never came (oldest id
        // first), then those still held back.
        for p in std::mem::take(&mut self.pending).into_values() {
            if let Pending::TurnStart { held: Some(h), .. } | Pending::Steer(h) = p {
                ev.push(AgentEvent::PromptDropped {
                    text: h.text,
                    how: h.how,
                    reason: DropReason::Exited,
                    id: h.id,
                });
            }
        }
        drop_held(&mut self.retry, &DropReason::Exited, &mut ev);
        drop_held(&mut self.queue, &DropReason::Exited, &mut ev);
        ev.push(AgentEvent::Exited { code });
        ev
    }
}

/// The strings an answer sends for one question: option labels, free text, or nothing for a
/// cancel. Refuses what does not fit the question.
fn slot_answer(slot: &Slot, answer: &Answer) -> Result<Vec<String>, Error> {
    let pick = |i: u16| {
        slot.options
            .get(usize::from(i))
            .cloned()
            .ok_or_else(|| Error::Bad(format!("no option {i}")))
    };
    Ok(match answer {
        Answer::Option(i) => vec![pick(*i)?],
        Answer::Options(v) if !v.is_empty() => {
            v.iter().map(|i| pick(*i)).collect::<Result<Vec<_>, _>>()?
        }
        Answer::Options(_) => return Err(Error::Bad("no option chosen".into())),
        Answer::Text(t) if slot.free_text => vec![t.clone()],
        Answer::Text(_) => return Err(Error::Bad("this question takes one of its options".into())),
        Answer::Cancel => Vec::new(),
    })
}

/// Body for a text-like item kind (all of them are `{text}`).
fn text_body(kind: ItemKind, text: String) -> Value {
    match kind {
        ItemKind::UserMessage => body(&items::UserMessage { text }),
        ItemKind::Thinking => body(&items::Thinking { text }),
        ItemKind::Plan => body(&items::Plan { text }),
        ItemKind::Event => body(&items::Event { text, task: None }),
        _ => body(&items::AssistantMessage { text }),
    }
}

/// A Codex `ThreadItem` in FEMME terms; `None` without an `id`.
fn map_item(item: &Value) -> Option<Mapped> {
    let id = item["id"].as_str()?.to_owned();
    let ty = item["type"].as_str().unwrap_or_default();
    let s = |k: &str| item[k].as_str().unwrap_or_default().to_owned();
    let lines = |k: &str, sep: &str| {
        item[k]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(sep)
    };
    let (kind, text) = match ty {
        "userMessage" => (ItemKind::UserMessage, content_text(&item["content"])),
        "agentMessage" => (ItemKind::AssistantMessage, s("text")),
        "reasoning" => {
            let summary = lines("summary", "\n\n");
            (
                ItemKind::Thinking,
                if summary.is_empty() {
                    lines("content", "\n\n")
                } else {
                    summary
                },
            )
        }
        "plan" => (ItemKind::Plan, s("text")),
        "contextCompaction" => (ItemKind::Event, "Context compacted".to_owned()),
        "enteredReviewMode" => (ItemKind::Event, format!("Review started: {}", s("review"))),
        "exitedReviewMode" => (ItemKind::Event, format!("Review finished: {}", s("review"))),
        "hookPrompt" => {
            let t: Vec<&str> = item["fragments"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| f["text"].as_str())
                .collect();
            (ItemKind::Event, t.join("\n"))
        }
        "subAgentActivity" => (
            ItemKind::Event,
            format!("Sub-agent {} {}", s("agentPath"), s("kind")),
        ),
        _ => {
            return Some(Mapped {
                kind: ItemKind::ToolCall,
                text: String::new(),
                tool: Some(tool(ty, &id, item)),
                id,
            })
        }
    };
    Some(Mapped {
        id,
        kind,
        text,
        tool: None,
    })
}

/// Tool-like items: `commandExecution`, `fileChange`, `mcpToolCall`, `dynamicToolCall`,
/// `webSearch`, `collabAgentToolCall`; any other (or future) type becomes a generic call named
/// after the type with the item's other fields as input.
fn tool(ty: &str, id: &str, item: &Value) -> Tool {
    let s = |k: &str| item[k].as_str().unwrap_or_default().to_owned();
    let mut failed = matches!(item["status"].as_str(), Some("failed" | "declined"));
    let (name, input, preview, output) = match ty {
        "commandExecution" => {
            let mut out = s("aggregatedOutput");
            if let Some(code) = item["exitCode"].as_i64().filter(|c| *c != 0) {
                failed = true;
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&format!("[exit code {code}]"));
            }
            (
                ty.to_owned(),
                json!({"command": item["command"], "cwd": item["cwd"]}),
                s("command"),
                out,
            )
        }
        "fileChange" => {
            let changes: Vec<String> = item["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| {
                    format!(
                        "{} {}",
                        c["kind"]["type"].as_str().unwrap_or("update"),
                        c["path"].as_str().unwrap_or_default()
                    )
                })
                .collect();
            (
                ty.to_owned(),
                json!({"changes": item["changes"]}),
                changes.join(", "),
                changes.join("\n"),
            )
        }
        "mcpToolCall" => {
            let name = format!("{}.{}", s("server"), s("tool"));
            let out = match item["error"]["message"].as_str() {
                Some(e) => {
                    failed = true;
                    e.to_owned()
                }
                None => content_text(&item["result"]["content"]),
            };
            (name.clone(), item["arguments"].clone(), name, out)
        }
        "dynamicToolCall" => {
            failed |= item["success"] == Value::Bool(false);
            (
                s("tool"),
                item["arguments"].clone(),
                s("tool"),
                content_text(&item["contentItems"]),
            )
        }
        "webSearch" => (
            ty.to_owned(),
            json!({"query": item["query"], "action": item["action"]}),
            s("query"),
            String::new(),
        ),
        "collabAgentToolCall" => {
            let states: Vec<&str> = item["agentsStates"]
                .as_object()
                .into_iter()
                .flat_map(|m| m.values())
                .filter_map(|st| st["message"].as_str())
                .collect();
            let input = json!({"prompt": item["prompt"], "receiverThreadIds": item["receiverThreadIds"], "model": item["model"]});
            (s("tool"), input, s("tool"), states.join("\n"))
        }
        _ => {
            let mut rest = item.as_object().cloned().unwrap_or_default();
            rest.remove("id");
            rest.remove("type");
            (
                ty.to_owned(),
                Value::Object(rest),
                ty.to_owned(),
                String::new(),
            )
        }
    };
    Tool {
        call: items::ToolCall {
            name,
            input,
            call_id: id.to_owned(),
            task: None,
        },
        preview,
        output,
        failed,
    }
}

/// User input / tool output content arrays flattened to text; non-text parts become `[type]`.
fn content_text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_owned();
    }
    let parts: Vec<String> = v
        .as_array()
        .into_iter()
        .flatten()
        .map(
            |c| match (c["type"].as_str().unwrap_or_default(), c["text"].as_str()) {
                (_, Some(t)) => t.to_owned(),
                (ty, None) => match c["name"].as_str().or(c["path"].as_str()) {
                    Some(n) => format!("[{ty}: {n}]"),
                    None => format!("[{ty}]"),
                },
            },
        )
        .collect();
    parts.join("\n")
}

/// `turn/plan/updated` (the agent's TODO list) as a markdown checklist; the step in progress is bold.
fn plan_text(p: &Value) -> String {
    let mut text = String::new();
    if let Some(e) = p["explanation"].as_str().filter(|e| !e.is_empty()) {
        text.push_str(e);
        text.push_str("\n\n");
    }
    for st in p["plan"].as_array().into_iter().flatten() {
        let step = st["step"].as_str().unwrap_or_default();
        let line = match st["status"].as_str() {
            Some("completed") => format!("- [x] {step}\n"),
            Some("inProgress") => format!("- [ ] **{step}**\n"),
            _ => format!("- [ ] {step}\n"),
        };
        text.push_str(&line);
    }
    text.trim_end().to_owned()
}

/// Name of a usage window by its length: `session` (5 h), `daily`, `weekly`, `monthly`, else
/// `<minutes>min`; without a length, `fallback` (the snapshot's slot).
pub fn window_kind(mins: Option<u64>, fallback: &str) -> String {
    match mins {
        Some(300) => "session".into(),
        Some(1440) => "daily".into(),
        Some(10080) => "weekly".into(),
        Some(40320..=44640) => "monthly".into(),
        Some(m) => format!("{m}min"),
        None => fallback.into(),
    }
}

/// One `RateLimitSnapshot` (`primary` / `secondary` windows) as `RateLimit` events. Buckets other
/// than the main `codex` one prefix the kind with their limit id (`<limitId>:<kind>`).
fn rate_limits(snap: &Value, ev: &mut Vec<AgentEvent>) {
    let limit = snap["limitId"].as_str().filter(|l| *l != "codex");
    for (slot, fallback) in [("primary", "session"), ("secondary", "weekly")] {
        let w = &snap[slot];
        let Some(percent) = w["usedPercent"].as_f64() else {
            continue;
        };
        let kind = window_kind(w["windowDurationMins"].as_u64(), fallback);
        let kind = match limit {
            Some(l) => format!("{l}:{kind}"),
            None => kind,
        };
        ev.push(AgentEvent::RateLimit {
            kind,
            percent: percent as f32,
            resets_at: w["resetsAt"].as_i64(),
        });
    }
}

/// `account/rateLimits/read` result: every bucket of `rateLimitsByLimitId`, else `rateLimits`.
fn rate_limits_read(r: &Value, ev: &mut Vec<AgentEvent>) {
    match r["rateLimitsByLimitId"]
        .as_object()
        .filter(|m| !m.is_empty())
    {
        Some(m) => m.values().for_each(|snap| rate_limits(snap, ev)),
        None => rate_limits(&r["rateLimits"], ev),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_named_by_length() {
        assert_eq!(window_kind(Some(300), "x"), "session");
        assert_eq!(window_kind(Some(10080), "x"), "weekly");
        assert_eq!(window_kind(Some(43200), "x"), "monthly");
        assert_eq!(window_kind(Some(90), "x"), "90min");
        assert_eq!(window_kind(None, "weekly"), "weekly");
    }

    #[test]
    fn plan_renders_a_checklist() {
        let p = json!({"explanation": "why", "plan": [
            {"step": "a", "status": "completed"}, {"step": "b", "status": "inProgress"}, {"step": "c", "status": "pending"}]});
        assert_eq!(plan_text(&p), "why\n\n- [x] a\n- [ ] **b**\n- [ ] c");
    }

    #[test]
    fn content_parts_flatten() {
        let c = json!([{"type": "text", "text": "hi"}, {"type": "localImage", "path": "/a.png"}, {"type": "image", "url": "u"}]);
        assert_eq!(content_text(&c), "hi\n[localImage: /a.png]\n[image]");
    }

    #[test]
    fn failing_command_is_an_error_result() {
        let item = json!({"type": "commandExecution", "id": "e1", "command": "false", "cwd": "/w", "status": "failed",
            "aggregatedOutput": "", "exitCode": 1});
        let t = tool("commandExecution", "e1", &item);
        assert!(t.failed);
        assert_eq!(t.output, "[exit code 1]");
        assert_eq!(t.call.input, json!({"command": "false", "cwd": "/w"}));
    }
}
