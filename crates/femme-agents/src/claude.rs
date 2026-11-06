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
//!   `AskUserQuestion` and `ExitPlanMode`) and `control_cancel_request`.
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
//! * Subagent traffic (`parent_tool_use_id` set) is dropped.
//! * Thinking items exist only when the CLI sends readable thinking text (it normally sends
//!   none, only a signature and a token estimate).
//! * Prompts: every user message gets a fresh `uuid`; the CLI echoes it (`isReplay`) when it starts
//!   working on the message, and that echo is what becomes the `UserMessage` item (id = the uuid).
//! * Commands: the CLI reports every user message as a `command_lifecycle` `queued`, `started`,
//!   `completed` (or `cancelled`/`discarded`/`refused`). While a command is unstarted its `queued`
//!   frame stays open in the resume offset, so a restart replays it and the new driver still
//!   knows the CLI is busy. `session_state_changed idle` means nothing is running or queued: it
//!   clears commands the CLI reported and never ended. A message of ours that is lost this way,
//!   cancelled by the CLI, or still unstarted when the process exits, is announced with
//!   `PromptDropped`.
//! * Questions after a restart: the answers the daemon persisted ([`Driver::pending_answers`],
//!   [`crate::Attach::answers`]) are applied to the replayed `can_use_tool`. A request whose answer
//!   is complete is not asked again (no `QuestionAsked`, never `Blocked`), one that is part way
//!   (several questions, some answered) asks only the rest.
//! * Outcomes of a question item: `Answered` with the user's own answer (also a deliberate
//!   Cancel, Deny or "Keep planning"), or `Failed` with a `reason` when it was never answered
//!   because the turn was interrupted, the process exited or the CLI withdrew the request.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::{Agent, ItemKind, ItemStatus, Question, Usage};
use serde_json::{json, Value};

use crate::common::{
    drop_held, held, hold, parse, preview, raw_text, set_status, uuid_v4, Offsets,
};
use crate::items::{
    body, AssistantMessage, Event, Plan, QuestionItem, Thinking, ToolCall, ToolResult, UserMessage,
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
    awaiting_init: bool,
    /// `on_exit` ran: nothing more is written.
    exited: bool,
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
            awaiting_init: false,
            exited: false,
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
        }
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

    /// The user message for `h`; it becomes a command of ours until the CLI starts it.
    fn user_line(&mut self, h: HeldPrompt) -> Value {
        let uuid = uuid_v4();
        let line = json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":h.text}]},"uuid":uuid});
        self.cmd(&uuid).ours = Some(h);
        line
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
        if !self.turn {
            self.turn = true;
            self.off.turn_begin(offset);
            ev.push(AgentEvent::TurnStarted);
            set_status(&mut self.status, AgentStatus::Working, ev);
        }
    }

    /// Status follows from state: blocked while a question is unanswered, working while a turn
    /// or a queued message exists, else idle. Called at the end of every line.
    fn sync(&mut self, ev: &mut Vec<AgentEvent>) {
        let want = if self.pending.values().any(|p| !p.resolved) {
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
            }
            "compact_boundary" => ev.push(event_item(item_id(v, offset), "Conversation compacted")),
            // nothing is running and nothing is queued: a command the CLI reported and never ended
            // (it has no terminal frame, or ours was dropped silently) would keep `busy()` true forever
            "session_state_changed" if v["state"] == "idle" => self.end_cmds(
                true,
                Some(DropReason::Refused(
                    "claude went idle without running it".into(),
                )),
                ev,
            ),
            // hooks, tasks, thinking_tokens, other session states, ...: nothing the transcript needs
            _ => {}
        }
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
    }

    fn stream_event(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        self.activity(offset, ev);
        let e = &v["event"];
        match e["type"].as_str().unwrap_or("") {
            "message_start" => {
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
            });
            self.off.item_open(&id, offset);
        } else if kind == BlockKind::Tool && !hidden(&name) {
            started = true;
            ev.push(AgentEvent::ItemStarted {
                id: id.clone(),
                kind: ItemKind::ToolCall,
                preview: preview(&name),
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
        match name.as_str() {
            PLAN => {
                let text = input["plan"].as_str().unwrap_or("").to_owned();
                ev.push(done(
                    id.clone(),
                    ItemKind::Plan,
                    ItemStatus::Completed,
                    body(&Plan { text }),
                ));
            }
            ASK => {}
            _ => ev.push(AgentEvent::ItemUpdated {
                id: id.clone(),
                body: body(&ToolCall {
                    name: name.clone(),
                    input: input.clone(),
                    call_id: id.clone(),
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
                }),
            ),
        };
        ev.push(done(b.id, kind, ItemStatus::Failed, body));
    }

    fn assistant(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        self.activity(offset, ev);
        let m = &v["message"];
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
                    if !streamed && !hidden(&name) {
                        ev.push(AgentEvent::ItemStarted {
                            id: id.clone(),
                            kind: ItemKind::ToolCall,
                            preview: preview(&name),
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
            ev.push(event_item(id, out));
        } else if v["isReplay"].as_bool() == Some(true) {
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
        if asked && call.as_ref().is_none_or(|c| hidden(&c.name)) {
            return; // the Question/Plan items are the record of this call
        }
        let status = if is_error {
            ItemStatus::Failed
        } else {
            ItemStatus::Completed
        };
        let result = done(
            format!("{id}:result"),
            ItemKind::ToolResult,
            status,
            body(&ToolResult {
                call_id: id.to_owned(),
                output,
                is_error,
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
                PendKind::Allow => Some(Answer::Option(0)),
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
        if self.turn {
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
            if !hidden(&c.name) {
                ev.push(done(
                    id.clone(),
                    ItemKind::ToolCall,
                    ItemStatus::Failed,
                    body(&ToolCall {
                        name: c.name,
                        input: c.input,
                        call_id: id,
                    }),
                ));
            }
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
        let input = req["input"].clone();
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
            ask_parts(rid, &tid, &input)
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
                let what = req["title"]
                    .as_str()
                    .or(req["description"].as_str())
                    .or(req["display_name"].as_str())
                    .unwrap_or(tool);
                (format!("Allow {what}?"), ["Allow", "Deny"])
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
        // Answers the daemon kept from before a restart. A request whose answer is complete (every
        // part answered, or one cancelled, which is sent at once) was already sent: it is not asked
        // again and never makes the session `Blocked`; one part way asks only the parts still open.
        for p in &mut parts {
            p.answer = self.restored.remove(&p.request_id);
        }
        let sent = parts.iter().any(|p| p.answer == Some(Answer::Cancel))
            || parts.iter().all(|p| p.answer.is_some());
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
                tool_use_id: tid,
                input,
                parts,
                sent,
                resolved: sent,
            },
        );
    }

    /// The request `cid` is over without a tool result: its questions are resolved and fail.
    fn drop_pending(&mut self, cid: &str, why: &str, ev: &mut Vec<AgentEvent>) {
        let Some(p) = self.pending.remove(cid) else {
            return;
        };
        for part in p.parts {
            if !p.resolved {
                ev.push(AgentEvent::QuestionResolved {
                    request_id: part.request_id.clone(),
                });
            }
            self.off.item_close(&part.item_id);
            ev.push(done(
                part.item_id,
                ItemKind::Question,
                ItemStatus::Failed,
                body(&QuestionItem {
                    q: part.q,
                    answered: None,
                    reason: Some(why.to_owned()),
                }),
            ));
        }
    }

    /// The CLI no longer waits for `cid` (cancelled, or our answer was echoed).
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

    fn control_response(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        let r = &v["response"];
        let rid = r["request_id"].as_str().unwrap_or("");
        if rid == INIT_ID {
            self.awaiting_init = false;
        } else if let Some(p) = self.pending.get_mut(rid) {
            // `--replay-user-messages` echoes our answer: a replay (no `answer()` call in this
            // incarnation) learns what the user chose from it, so the outcome is the same one
            echoed_answers(p, &r["response"]);
            self.resolve(rid, ev);
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
        // ponytail: subagent (Task tool) transcripts are dropped, show them nested when the UI wants them
        if matches!(ty, "stream_event" | "assistant" | "user")
            && v["parent_tool_use_id"].is_string()
        {
            return ev;
        }
        match ty {
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
                _ => ev.push(AgentEvent::Raw(v)), // hook_callback, mcp_message: we registered none
            },
            "control_cancel_request" => {
                self.resolve(v["request_id"].as_str().unwrap_or(""), &mut ev)
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
        if self.exited {
            return Err(Error::State("claude has exited"));
        }
        let h = held(text, how);
        match how {
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

/// Tools whose call is shown as Question/Plan items instead of a ToolCall.
fn hidden(name: &str) -> bool {
    name == ASK || name == PLAN
}

fn done(id: String, kind: ItemKind, status: ItemStatus, body: Value) -> AgentEvent {
    AgentEvent::ItemCompleted {
        id,
        kind,
        status,
        body,
    }
}

fn event_item(id: String, text: &str) -> AgentEvent {
    done(
        id,
        ItemKind::Event,
        ItemStatus::Completed,
        body(&Event {
            text: text.to_owned(),
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
    let tag = |t: &str| {
        let (_, rest) = text.split_once(&format!("<{t}>"))?;
        rest.split_once(&format!("</{t}>")).map(|(v, _)| v.trim())
    };
    if !text.trim_start().starts_with("<command-") {
        return None;
    }
    let name = tag("command-name")?;
    Some(match tag("command-args").filter(|a| !a.is_empty()) {
        Some(args) => format!("{name} {args}"),
        None => name.to_owned(),
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
    }
}

/// Fill in the parts of `p` nobody answered in this incarnation from the response we wrote to
/// the CLI (its echo), the inverse of [`response_body`]. What `response_body` cannot tell apart
/// comes back as the nearer answer: a refusal of an approval is `Option(1)` ("Deny", "Keep
/// planning"), whether it was that option or a `Cancel`.
fn echoed_answers(p: &mut Pending, resp: &Value) {
    let deny = resp["behavior"] == "deny";
    for part in p.parts.iter_mut().filter(|x| x.answer.is_none()) {
        part.answer = match (p.kind, deny) {
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
        assert!(d.on_line(0, note.as_bytes(), &mut out).is_empty());
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
    }
}
