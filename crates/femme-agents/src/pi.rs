//! Pi driver: `pi --mode rpc`, one JSON object per line in both directions. Verified against
//! pi 1.3.0; the findings are in the "Pi" section of `NOTES.md`, the fixtures in `fixtures/pi/`
//! (captured by `scripts/capture-pi.py`).
//!
//! # Process and session
//!
//! [`PiDriver`] spawns `pi --mode rpc [--model <provider/id>] [--session-id <id>]`. Pi prints
//! nothing until it gets a command and needs no handshake, so [`Driver::on_start`] only sends
//! `get_state`; the answer carries `sessionId` and the model, which become
//! [`AgentEvent::Started`]. Prompts can be written right away. The session id is what
//! [`Launch::resume`] takes: `--session-id <id>` reopens that session (or creates one with that id
//! when pi never wrote it; pi writes the file at the first user message). A resume value that
//! looks like a path goes to `--session <path>`. Never pass a prefix of an id: pi session ids are
//! UUIDv7, so prefixes of ids made around the same time collide.
//!
//! # Turns
//!
//! A FEMME turn is one pi *run*: `agent_start` .. `agent_settled` (pi's own `turn_start`/`turn_end`
//! are single LLM calls, and retries, compaction recovery and queued messages can add more
//! `agent_start`/`agent_end` pairs before the settle). `agent_settled{aborted}` ends the turn.
//! Usage is the sum over the run's assistant messages; the stop reason is the last message's
//! (`stop` -> `EndTurn`, `length` -> `MaxTokens`, `aborted` -> `Interrupted`, `error` ->
//! `Error(errorMessage)`); the duration is pi's own clock, from the first message of the run to
//! the end of the last one.
//!
//! # Items
//!
//! | pi | item |
//! |---|---|
//! | `message_end` role `user` (prompts and delivered steers) | `UserMessage` `o<offset of its message_start>` |
//! | `message_update` `text_*`, `thinking_*` | `AssistantMessage`, `Thinking` `o<offset of the message_start>:<contentIndex>` |
//! | `toolcall_start`, `toolcall_end`, `tool_execution_start` | `ToolCall` with pi's `toolCallId`; `ItemUpdated` with the arguments |
//! | `tool_execution_end` | `ItemCompleted(ToolCall)` and `ItemCompleted(ToolResult)` `<toolCallId>:result` |
//! | `extension_ui_request` `select`, `confirm`, `input`, `editor` | `QuestionAsked`, item id = request id |
//! | `notify`, `extension_error`, `auto_retry_*`, `compaction_end`, refused commands, dropped steers | `Event` `o<offset>` |
//!
//! No events: the leading `system` message, `queue_update`, `tool_execution_update` (partial
//! tool output does not stream), the UI calls `setStatus`/`setWidget`/`setTitle`/`set_editor_text`,
//! answers to commands that worked. Unknown types become [`AgentEvent::Raw`].
//!
//! # Prompts
//!
//! Idle: `prompt`. While a run goes (or pi has not answered a prompt yet), `Queue` is held here
//! and sent as a `prompt` after `agent_settled`, so every prompt gets its own turn (pi's
//! `follow_up` would fold it into the running turn, one `agent_settled` for both). `Steer` is a
//! `prompt` with `streamingBehavior: "steer"`: pi delivers it after the current LLM call and its
//! tools, inside the same turn (a steer before the run has started is queued instead).
//! `InterruptThenSend` and [`Driver::interrupt`] cancel the open dialogs (see Questions), then send
//! `clear_queue` + `abort` (pi would otherwise carry undelivered steers into the next run; the
//! steers pi drops are reported as an `Event` and one [`AgentEvent::PromptDropped`] each) and the
//! text goes out after `agent_settled`. Prompts the driver itself holds are **kept** by an
//! interrupt and run, in order, after the interrupted run; only pi's own steer queue is cleared.
//! A prompt pi refuses because a run it had not reported yet is going is held again, ahead of the
//! other held prompts and in the order they were written; one it refuses otherwise is announced
//! with `PromptDropped`. Held prompts live in driver memory only ([`Driver::held_prompts`]); the
//! ones still held when pi exits, and those pi never acknowledged, are announced as
//! `PromptDropped` too (the held ones also as an `Event`).
//!
//! # Questions
//!
//! Pi has no permission prompts; only extensions ask, through `ctx.ui` dialogs. Pi blocks the
//! extension until an `extension_ui_response` with the request id arrives, and never echoes the
//! answer, so after [`Driver::answer`] the `QuestionResolved` comes with the next line. Like
//! Codex, the driver emits no `ItemCompleted` for the question: the daemon records the answer it
//! sent. A dialog pi closed by itself (its `timeout`, an abort) counts as resolved once the tools
//! running when it was asked have finished, at the next LLM call, or when the turn ends.
//! [`Driver::interrupt`] answers every open dialog with `cancelled: true` before it aborts (pi
//! would keep the extension blocked on it), exactly as `answer(.., Cancel)` does.
//!
//! # Daemon restart (re-attach)
//!
//! The daemon calls [`Driver::attach`] on a fresh driver and replays the spool. `attach` hands over
//! the committed offset, the session id and model (pi says neither again on its own; without a
//! session id the driver asks `get_state`), and the answers given to dialogs. Pi never echoes an
//! answer, so a replay from before one meets the dialog again: a dialog whose id is among the
//! answers is not asked again (no `QuestionAsked`, no `Blocked`) and [`Driver::answer`] refuses it;
//! [`Driver::pending_answers`] lists the answers a replay from [`Driver::resume_offset`] can still
//! meet. The driver's command ids are `f<n>` for the first process and `f<epoch>-<n>` after a
//! re-attach ([`Attach::epoch`]), so a replayed response never matches a new command.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::{Agent, ItemKind, ItemStatus, Question, Usage};
use serde_json::{json, Value};

use crate::common::{self, Offsets};
use crate::items::{self, body};
use crate::{
    AgentEvent, AgentStatus, Attach, Driver, DropReason, Error, HeldPrompt, Launch, Out, Spawn,
    StopReason,
};

/// Driver for Pi's RPC mode (`pi --mode rpc`). See the module docs for the mapping.
#[derive(Debug, Default)]
pub struct PiDriver {
    off: Offsets,
    /// Offset just past the last line seen.
    next_line: u64,
    status: AgentStatus,
    sid: Option<String>,
    /// `provider/id` in use, when known.
    model: Option<String>,
    /// `Started` was emitted for this process.
    started: bool,
    /// The process exited: every call that would write is refused.
    exited: bool,
    /// [`Attach::epoch`]: commands carry `"id": "f<n>"`, or `"f<epoch>-<n>"` once it is not 0.
    epoch: u32,
    next_cmd: u64,
    /// Between `agent_start` and `agent_settled`.
    in_turn: bool,
    /// Pi answered `started` to a prompt whose run has not settled yet.
    announced: bool,
    /// `prompt` commands written and not answered yet, oldest first: (command id, prompt). A
    /// `Vec`, not a map keyed by id: `f10` sorts before `f9`.
    pending: Vec<(String, HeldPrompt)>,
    /// Prompts waiting for pi to go idle. In memory only: the daemon persists them through
    /// [`Driver::held_prompts`] if it wants them back after a restart.
    queue: VecDeque<HeldPrompt>,
    /// Prompts pi refused because a run it had not reported yet was going. They run before
    /// `queue`, in the order they were written (a `push_front` per refusal would reverse two).
    retry: VecDeque<HeldPrompt>,
    turn: Turn,
    /// Offset of the `message_start` of the assistant message being streamed.
    msg: Option<u64>,
    /// contentIndex -> item id, for that message.
    blocks: BTreeMap<u64, String>,
    /// Tool call ids of that message.
    msg_tools: Vec<String>,
    /// Offset of the `message_start` of the user message being echoed.
    user_start: Option<u64>,
    /// Text, thinking and tool call items started and not completed.
    open: BTreeMap<String, Open>,
    /// Tool calls of the current run.
    tools: BTreeMap<String, Tool>,
    /// Tool calls executing now.
    running: BTreeSet<String>,
    /// Dialogs waiting for an answer, by request id.
    questions: BTreeMap<String, Dialog>,
    /// Requests answered since the last line; their `QuestionResolved` goes out with the next one.
    answered: Vec<String>,
    /// Every answer given (or handed over by [`Driver::attach`]) with the offset of its request
    /// line: pi never echoes an answer, so a replay from before it meets the dialog again and must
    /// not ask it twice. [`Driver::pending_answers`] lists those a replay can still reach.
    // ponytail: kept for the life of the driver, a few bytes per answered dialog; prune at turn
    // end if a session ever answers thousands.
    given: BTreeMap<String, (u64, Answer)>,
}

/// An open item: its kind and the text streamed so far.
#[derive(Debug, Clone)]
struct Open {
    kind: ItemKind,
    text: String,
}

/// A tool call of the current run.
#[derive(Debug, Clone)]
struct Tool {
    name: String,
    input: Value,
    done: bool,
}

/// An `extension_ui_request` dialog waiting for an answer.
#[derive(Debug, Clone)]
struct Dialog {
    q: Question,
    method: String,
    /// Offset of the request line.
    offset: u64,
    /// Tool calls running when it was asked; once they have all ended, so has the dialog.
    waiting_on: BTreeSet<String>,
}

/// What the running turn has added up.
#[derive(Debug, Default, Clone)]
struct Turn {
    usage: Option<Usage>,
    stop: Option<StopReason>,
    /// Pi's ms timestamps: start of the first message, end of the last.
    first: Option<i64>,
    last: Option<i64>,
}

impl Turn {
    fn saw(&mut self, start: Option<i64>, end: Option<i64>) {
        if let Some(s) = start {
            self.first = Some(self.first.map_or(s, |f| f.min(s)));
        }
        if let Some(e) = end {
            self.last = Some(self.last.map_or(e, |l| l.max(e)));
        }
    }

    fn duration(&self) -> Option<u64> {
        u64::try_from(self.last?.checked_sub(self.first?)?).ok()
    }
}

impl PiDriver {
    /// A driver for one pi process (fresh, or re-attached after a daemon restart).
    pub fn new() -> Self {
        Self::default()
    }

    fn busy(&self) -> bool {
        self.in_turn || self.announced || !self.pending.is_empty()
    }

    /// Write `cmd` with the next id (`f<n>`, `f<epoch>-<n>` after a re-attach); returns the id.
    fn send(&mut self, out: &mut Out, mut cmd: Value) -> String {
        self.next_cmd += 1;
        let id = match self.epoch {
            0 => format!("f{}", self.next_cmd),
            e => format!("f{e}-{}", self.next_cmd),
        };
        cmd["id"] = Value::String(id.clone());
        out.line(&cmd);
        id
    }

    fn send_prompt(&mut self, h: HeldPrompt, steer: bool, out: &mut Out) {
        let mut cmd = json!({"type": "prompt", "message": h.text});
        if steer {
            cmd["streamingBehavior"] = json!("steer");
        }
        let id = self.send(out, cmd);
        self.pending.push((id, h));
    }

    /// Cancel every open dialog (pi keeps the extension blocked on it otherwise, even after an
    /// abort), as `answer(.., Cancel)` would.
    fn cancel_dialogs(&mut self, out: &mut Out) {
        for id in self.questions.keys().cloned().collect::<Vec<_>>() {
            // a cancel fits every dialog, so this cannot fail
            let _ = self.answer(&id, &Answer::Cancel, out);
        }
    }

    fn send_interrupt(&mut self, out: &mut Out) {
        self.cancel_dialogs(out);
        self.send(out, json!({"type": "clear_queue"}));
        self.send(out, json!({"type": "abort"}));
    }

    /// Send the next held prompt once pi is idle.
    fn flush(&mut self, out: &mut Out) {
        if !self.busy() {
            if let Some(h) = self.retry.pop_front().or_else(|| self.queue.pop_front()) {
                self.send_prompt(h, false, out);
            }
        }
    }

    fn update_status(&mut self, ev: &mut Vec<AgentEvent>) {
        let new = if !self.questions.is_empty() {
            AgentStatus::Blocked
        } else if self.in_turn {
            AgentStatus::Working
        } else {
            AgentStatus::Idle
        };
        common::set_status(&mut self.status, new, ev);
    }

    // -- turns ----------------------------------------------------------------------------------

    fn begin_turn(&mut self, offset: u64, ev: &mut Vec<AgentEvent>) {
        if self.in_turn {
            return; // a retry or a queued message inside the same run
        }
        self.in_turn = true;
        self.off.turn_begin(offset);
        self.turn = Turn::default();
        ev.push(AgentEvent::TurnStarted);
        self.update_status(ev);
    }

    fn end_turn(&mut self, aborted: bool, ev: &mut Vec<AgentEvent>, out: &mut Out) {
        self.resolve_dialogs(ev);
        self.close_blocks(ev);
        let calls: Vec<String> = self
            .open
            .iter()
            .filter(|(_, o)| o.kind == ItemKind::ToolCall)
            .map(|(id, _)| id.clone())
            .collect();
        for id in calls {
            self.fail_tool(&id, ev);
        }
        if self.in_turn {
            let stop = if aborted {
                StopReason::Interrupted
            } else {
                self.turn.stop.take().unwrap_or(StopReason::EndTurn)
            };
            ev.push(AgentEvent::TurnEnded {
                stop,
                usage: self.turn.usage,
                duration_ms: self.turn.duration(),
            });
            self.in_turn = false;
            self.off.turn_end();
        }
        self.announced = false;
        self.tools.clear();
        self.running.clear();
        self.msg = None;
        self.blocks.clear();
        self.msg_tools.clear();
        self.update_status(ev);
        self.flush(out);
    }

    // -- command responses ----------------------------------------------------------------------

    fn on_response(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>, out: &mut Out) {
        let ok = v["success"].as_bool().unwrap_or(false);
        let data = &v["data"];
        let cmd = str_of(v, "command");
        match cmd {
            "get_state" if ok => self.on_state(data, ev),
            "prompt" => {
                let held = self
                    .pending
                    .iter()
                    .position(|(id, _)| id == str_of(v, "id"))
                    .map(|i| self.pending.remove(i).1);
                if ok {
                    if str_of(data, "disposition") == "started" {
                        self.announced = true;
                    }
                } else if self.in_turn {
                    // Pi was already running something we had not seen start: try again after it
                    // (it is older than everything in the queue, so it goes first).
                    if let Some(h) = held {
                        self.retry.push_back(h);
                    }
                } else {
                    ev.push(note(
                        offset,
                        format!("pi did not take the prompt: {}", str_of(v, "error")),
                    ));
                    // a response to a command of an earlier incarnation has no prompt to announce
                    if let Some(h) = held {
                        ev.push(AgentEvent::PromptDropped {
                            text: h.text,
                            how: h.how,
                            reason: DropReason::Refused(str_of(v, "error").to_owned()),
                        });
                    }
                }
                self.flush(out);
            }
            "set_model" if ok => {
                if let Some(m) = model_name(data) {
                    self.model = Some(m.clone());
                    ev.push(AgentEvent::ModelChanged(m));
                }
            }
            "clear_queue" if ok => {
                let mut dropped = Vec::new();
                for (key, how) in [
                    ("steering", PromptHow::Steer),
                    ("followUp", PromptHow::Queue),
                ] {
                    for t in data[key].as_array().into_iter().flatten() {
                        dropped.extend(t.as_str().map(|t| (t, how)));
                    }
                }
                if !dropped.is_empty() {
                    let all: Vec<&str> = dropped.iter().map(|(t, _)| *t).collect();
                    ev.push(note(
                        offset,
                        format!("Not delivered (interrupted): {}", all.join(" | ")),
                    ));
                }
                for (text, how) in dropped {
                    ev.push(AgentEvent::PromptDropped {
                        text: text.to_owned(),
                        how,
                        reason: DropReason::Refused("interrupted before pi delivered it".into()),
                    });
                }
            }
            _ if !ok => ev.push(note(
                offset,
                format!("pi {cmd} failed: {}", str_of(v, "error")),
            )),
            _ => {}
        }
    }

    fn on_state(&mut self, data: &Value, ev: &mut Vec<AgentEvent>) {
        let sid = str_of(data, "sessionId");
        let model = model_name(&data["model"]);
        if !sid.is_empty() && (!self.started || self.sid.as_deref() != Some(sid)) {
            self.started = true;
            self.sid = Some(sid.to_owned());
            self.model.clone_from(&model);
            ev.push(AgentEvent::Started {
                agent_sid: sid.to_owned(),
                model,
            });
        } else if let Some(m) = model.filter(|m| self.model.as_ref() != Some(m)) {
            self.model = Some(m.clone());
            ev.push(AgentEvent::ModelChanged(m));
        }
    }

    // -- messages -------------------------------------------------------------------------------

    fn on_message_start(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        match str_of(&v["message"], "role") {
            "user" => self.user_start = Some(offset),
            "assistant" => {
                self.close_blocks(ev);
                self.blocks.clear();
                self.msg_tools.clear();
                self.msg = Some(offset);
                // ponytail: a new LLM call means every tool, and so every dialog, has finished;
                // a dialog an extension opens without awaiting it would be dropped early.
                self.resolve_dialogs(ev);
            }
            _ => {}
        }
    }

    /// The item of content block `idx` of the current assistant message, started if new.
    fn block(&mut self, idx: u64, kind: ItemKind, offset: u64, ev: &mut Vec<AgentEvent>) -> String {
        if let Some(id) = self.blocks.get(&idx) {
            return id.clone();
        }
        let id = format!("o{}:{idx}", *self.msg.get_or_insert(offset));
        self.blocks.insert(idx, id.clone());
        self.open.insert(
            id.clone(),
            Open {
                kind,
                text: String::new(),
            },
        );
        self.off.item_open(&id, offset);
        ev.push(AgentEvent::ItemStarted {
            id: id.clone(),
            kind,
            preview: String::new(),
        });
        id
    }

    /// Complete an open text/thinking item, with `text` or what streamed.
    fn complete_text(&mut self, id: &str, text: Option<String>, ev: &mut Vec<AgentEvent>) {
        let Some(o) = self.open.remove(id) else {
            return;
        };
        self.off.item_close(id);
        let text = text.unwrap_or(o.text);
        ev.push(done(
            id,
            o.kind,
            ItemStatus::Completed,
            text_body(o.kind, text),
        ));
    }

    /// Complete the open text/thinking blocks of the current message.
    fn close_blocks(&mut self, ev: &mut Vec<AgentEvent>) {
        let ids: Vec<String> = self
            .blocks
            .values()
            .filter(|id| {
                self.open
                    .get(*id)
                    .is_some_and(|o| o.kind != ItemKind::ToolCall)
            })
            .cloned()
            .collect();
        for id in ids {
            self.complete_text(&id, None, ev);
        }
    }

    fn on_update(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        let e = &v["assistantMessageEvent"];
        let idx = num(&e["contentIndex"]);
        let t = str_of(e, "type");
        let kind = if t.starts_with("thinking") {
            ItemKind::Thinking
        } else {
            ItemKind::AssistantMessage
        };
        match t {
            "text_start" | "thinking_start" => {
                self.block(idx, kind, offset, ev);
            }
            "text_delta" | "thinking_delta" => {
                let id = self.block(idx, kind, offset, ev);
                let d = str_of(e, "delta");
                if !d.is_empty() {
                    if let Some(o) = self.open.get_mut(&id) {
                        o.text.push_str(d);
                    }
                    ev.push(AgentEvent::ItemDelta {
                        id,
                        text: d.to_owned(),
                    });
                }
            }
            "text_end" | "thinking_end" => {
                let id = self.block(idx, kind, offset, ev);
                self.complete_text(&id, e["content"].as_str().map(str::to_owned), ev);
            }
            "toolcall_start" => {
                let id = str_of(e, "id").to_owned();
                if !id.is_empty() {
                    self.tool_seen(&id, str_of(e, "toolName"), offset, ev);
                    self.block_tool(idx, &id);
                }
            }
            "toolcall_end" => {
                let tc = &e["toolCall"];
                let id = self.call_id(str_of(tc, "id"), idx, offset);
                self.tool_seen(&id, str_of(tc, "name"), offset, ev);
                self.block_tool(idx, &id);
                self.tool_input(&id, &tc["arguments"], ev);
            }
            "toolcall_delta" | "start" | "done" | "error" => {}
            _ => ev.push(AgentEvent::Raw(v.clone())),
        }
    }

    fn on_message_end(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        let m = &v["message"];
        let ts = int(&m["timestamp"]);
        match str_of(m, "role") {
            "user" => {
                let start = self.user_start.take().unwrap_or(offset);
                self.turn.saw(ts, None);
                let text = flatten(&m["content"]);
                ev.push(done(
                    &format!("o{start}"),
                    ItemKind::UserMessage,
                    ItemStatus::Completed,
                    body(&items::UserMessage { text }),
                ));
            }
            "assistant" => self.on_assistant_end(offset, m, ts, ev),
            "toolResult" => {
                // tool_execution_end has normally completed it already.
                let id = str_of(m, "toolCallId").to_owned();
                if !id.is_empty() {
                    self.finish_tool(
                        &id,
                        str_of(m, "toolName"),
                        &m["content"],
                        m["isError"].as_bool().unwrap_or(false),
                        ev,
                    );
                }
                self.turn.saw(None, ts);
            }
            "custom" => {
                if m["display"].as_bool() == Some(true) {
                    ev.push(note(offset, flatten(&m["content"])));
                }
            }
            "system" | "bashExecution" | "branchSummary" | "compactionSummary" => {}
            _ => ev.push(AgentEvent::Raw(v.clone())),
        }
    }

    fn on_assistant_end(
        &mut self,
        offset: u64,
        m: &Value,
        ts: Option<i64>,
        ev: &mut Vec<AgentEvent>,
    ) {
        let base = *self.msg.get_or_insert(offset);
        for (i, b) in m["content"].as_array().into_iter().flatten().enumerate() {
            let idx = i as u64;
            match str_of(b, "type") {
                t @ ("text" | "thinking") => {
                    let kind = if t == "text" {
                        ItemKind::AssistantMessage
                    } else {
                        ItemKind::Thinking
                    };
                    let text = str_of(b, t).to_owned();
                    match self.blocks.get(&idx).cloned() {
                        Some(id) => self.complete_text(&id, Some(text), ev),
                        None if !text.is_empty() => {
                            // Not streamed: the whole block at once.
                            let id = format!("o{base}:{idx}");
                            ev.push(done(
                                &id,
                                kind,
                                ItemStatus::Completed,
                                text_body(kind, text),
                            ));
                            self.blocks.insert(idx, id);
                        }
                        None => {}
                    }
                }
                "toolCall" => {
                    let id = self.call_id(str_of(b, "id"), idx, offset);
                    self.tool_seen(&id, str_of(b, "name"), offset, ev);
                    self.block_tool(idx, &id);
                    self.tool_input(&id, &b["arguments"], ev);
                }
                _ => {}
            }
        }
        self.close_blocks(ev);
        let stop = stop_reason(m);
        if matches!(stop, StopReason::Interrupted | StopReason::Error(_)) {
            // Pi does not run the tool calls of an aborted or failed message.
            for id in std::mem::take(&mut self.msg_tools) {
                self.fail_tool(&id, ev);
            }
        }
        let u = &m["usage"];
        if u.is_object() {
            let t = self.turn.usage.get_or_insert_with(Usage::default);
            t.input = t.input.saturating_add(num(&u["input"]));
            t.output = t.output.saturating_add(num(&u["output"]));
            t.cache_read = t.cache_read.saturating_add(num(&u["cacheRead"]));
            t.cache_write = t.cache_write.saturating_add(num(&u["cacheWrite"]));
        }
        self.turn.stop = Some(stop);
        self.turn.saw(
            ts,
            ts.map(|t| t.saturating_add(int(&m["durationMs"]).unwrap_or(0))),
        );
        if self.model.is_none() {
            self.model = model_name(&json!({"provider": m["provider"], "id": m["model"]}));
        }
        self.msg = None;
        self.blocks.clear();
        self.msg_tools.clear();
    }

    // -- tools ----------------------------------------------------------------------------------

    /// Pi's call id, else the one already on block `idx`, else `o<message offset>:<idx>`.
    fn call_id(&self, id: &str, idx: u64, offset: u64) -> String {
        if !id.is_empty() {
            return id.to_owned();
        }
        if let Some(known) = self.blocks.get(&idx) {
            return known.clone();
        }
        format!("o{}:{idx}", self.msg.unwrap_or(offset))
    }

    fn block_tool(&mut self, idx: u64, id: &str) {
        self.blocks.insert(idx, id.to_owned());
        if !self.msg_tools.iter().any(|t| t == id) {
            self.msg_tools.push(id.to_owned());
        }
    }

    /// Start the `ToolCall` item the first time its id shows up.
    fn tool_seen(&mut self, id: &str, name: &str, offset: u64, ev: &mut Vec<AgentEvent>) {
        if let Some(t) = self.tools.get_mut(id) {
            if t.name.is_empty() {
                t.name = name.to_owned();
            }
            return;
        }
        self.tools.insert(
            id.to_owned(),
            Tool {
                name: name.to_owned(),
                input: Value::Null,
                done: false,
            },
        );
        self.open.insert(
            id.to_owned(),
            Open {
                kind: ItemKind::ToolCall,
                text: String::new(),
            },
        );
        self.off.item_open(id, offset);
        ev.push(AgentEvent::ItemStarted {
            id: id.to_owned(),
            kind: ItemKind::ToolCall,
            preview: common::preview(name),
        });
    }

    fn call_body(&self, id: &str) -> Value {
        let (name, input) = self
            .tools
            .get(id)
            .map(|t| (t.name.clone(), t.input.clone()))
            .unwrap_or_default();
        body(&items::ToolCall {
            name,
            input,
            call_id: id.to_owned(),
        })
    }

    /// New (or changed) arguments: `ItemUpdated` with the whole body.
    fn tool_input(&mut self, id: &str, input: &Value, ev: &mut Vec<AgentEvent>) {
        let Some(t) = self.tools.get_mut(id) else {
            return;
        };
        if t.done || input.is_null() || t.input == *input {
            return;
        }
        t.input = input.clone();
        ev.push(AgentEvent::ItemUpdated {
            id: id.to_owned(),
            body: self.call_body(id),
        });
    }

    fn on_tool_start(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        let id = str_of(v, "toolCallId").to_owned();
        if id.is_empty() {
            return ev.push(AgentEvent::Raw(v.clone()));
        }
        self.running.insert(id.clone());
        self.tool_seen(&id, str_of(v, "toolName"), offset, ev);
        self.tool_input(&id, &v["args"], ev);
    }

    fn on_tool_end(&mut self, v: &Value, ev: &mut Vec<AgentEvent>) {
        let id = str_of(v, "toolCallId").to_owned();
        if id.is_empty() {
            return ev.push(AgentEvent::Raw(v.clone()));
        }
        self.running.remove(&id);
        self.finish_tool(
            &id,
            str_of(v, "toolName"),
            &v["result"]["content"],
            v["isError"].as_bool().unwrap_or(false),
            ev,
        );
        let over: Vec<String> = self
            .questions
            .iter_mut()
            .filter_map(|(q, d)| {
                (d.waiting_on.remove(&id) && d.waiting_on.is_empty()).then(|| q.clone())
            })
            .collect();
        for q in over {
            self.resolve_dialog(&q, ev);
        }
        self.update_status(ev);
    }

    /// The tool returned: complete the call and add its result item.
    fn finish_tool(
        &mut self,
        id: &str,
        name: &str,
        content: &Value,
        is_error: bool,
        ev: &mut Vec<AgentEvent>,
    ) {
        let known = match self.tools.get_mut(id) {
            Some(t) if t.done => return,
            Some(t) => {
                t.done = true;
                if t.name.is_empty() {
                    t.name = name.to_owned();
                }
                true
            }
            None => {
                // A result for a call this run never showed (a late one after the settle, or a
                // replay that began after the call): remember it so a repeat is ignored.
                let (name, input) = (name.to_owned(), Value::Null);
                self.tools.insert(
                    id.to_owned(),
                    Tool {
                        name,
                        input,
                        done: true,
                    },
                );
                false
            }
        };
        let status = if is_error {
            ItemStatus::Failed
        } else {
            ItemStatus::Completed
        };
        self.open.remove(id);
        self.off.item_close(id);
        // The call's own item keeps what it has: completing a call we know nothing about would
        // overwrite the real name and input with blanks.
        if known {
            ev.push(done(id, ItemKind::ToolCall, status, self.call_body(id)));
        }
        let res = items::ToolResult {
            call_id: id.to_owned(),
            output: flatten(content),
            is_error,
        };
        ev.push(done(
            &format!("{id}:result"),
            ItemKind::ToolResult,
            status,
            body(&res),
        ));
    }

    /// The call never ran (its message was aborted or failed, or the run ended under it).
    fn fail_tool(&mut self, id: &str, ev: &mut Vec<AgentEvent>) {
        match self.tools.get_mut(id) {
            Some(t) if !t.done => t.done = true,
            _ => return,
        }
        self.open.remove(id);
        self.off.item_close(id);
        ev.push(done(
            id,
            ItemKind::ToolCall,
            ItemStatus::Failed,
            self.call_body(id),
        ));
    }

    // -- extension UI ---------------------------------------------------------------------------

    fn on_ui(&mut self, offset: u64, v: &Value, ev: &mut Vec<AgentEvent>) {
        let id = str_of(v, "id").to_owned();
        let method = str_of(v, "method");
        let title = str_of(v, "title");
        let (prompt, options, free_text) = match method {
            "select" => (
                title.to_owned(),
                v["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                false,
            ),
            "confirm" => (
                join(title, str_of(v, "message")),
                vec!["Yes".to_owned(), "No".to_owned()],
                false,
            ),
            "input" => (title.to_owned(), vec![], true),
            "editor" => (join(title, str_of(v, "prefill")), vec![], true),
            "notify" => {
                let text = match str_of(v, "notifyType") {
                    "" | "info" => str_of(v, "message").to_owned(),
                    kind => format!("{kind}: {}", str_of(v, "message")),
                };
                return ev.push(note(offset, text));
            }
            "setStatus" | "setWidget" | "setTitle" | "set_editor_text" => return,
            _ => return ev.push(AgentEvent::Raw(v.clone())),
        };
        if id.is_empty() {
            return ev.push(AgentEvent::Raw(v.clone()));
        }
        if self.questions.contains_key(&id) {
            return;
        }
        if let Some(g) = self.given.get_mut(&id) {
            // answered, before a restart or earlier in this replay: pi never echoes an answer, so
            // the request line is all a replay shows. Not a question any more.
            g.0 = offset;
            return;
        }
        let q = Question {
            request_id: id.clone(),
            prompt,
            options,
            multi: false,
            free_text,
        };
        self.questions.insert(
            id.clone(),
            Dialog {
                q: q.clone(),
                method: method.to_owned(),
                offset,
                waiting_on: self.running.clone(),
            },
        );
        self.off.item_open(&id, offset);
        ev.push(AgentEvent::QuestionAsked { item_id: id, q });
        self.update_status(ev);
    }

    fn resolve_dialog(&mut self, id: &str, ev: &mut Vec<AgentEvent>) {
        if self.questions.remove(id).is_some() {
            self.off.item_close(id);
            ev.push(AgentEvent::QuestionResolved {
                request_id: id.to_owned(),
            });
        }
    }

    fn resolve_dialogs(&mut self, ev: &mut Vec<AgentEvent>) {
        let ids: Vec<String> = self.questions.keys().cloned().collect();
        for id in ids {
            self.resolve_dialog(&id, ev);
        }
        self.update_status(ev);
    }

    /// `QuestionResolved` for the answers given since the last line (pi does not echo them).
    fn report_answers(&mut self, ev: &mut Vec<AgentEvent>) {
        if self.answered.is_empty() {
            return;
        }
        for id in std::mem::take(&mut self.answered) {
            ev.push(AgentEvent::QuestionResolved { request_id: id });
        }
        self.update_status(ev);
    }
}

impl Driver for PiDriver {
    fn agent(&self) -> Agent {
        Agent::Pi
    }

    /// `pi --mode rpc [--model m] [--session-id id | --session path]`. The hook extension is not
    /// loaded: in RPC mode the stream itself says what pi is doing.
    fn spawn(&self, l: &Launch) -> Spawn {
        let mut cmd = vec!["pi".to_owned(), "--mode".to_owned(), "rpc".to_owned()];
        cmd.extend(model_args(l));
        cmd.extend(session_args(l));
        Spawn {
            cmd,
            env: l.extra_env.clone(),
            cwd: l.cwd.clone(),
        }
    }

    /// `pi [--model m] [--session-id id] [-e <hooks_path>]`: the same session in pi's TUI, with the
    /// status extension from [`crate::hooks::pi_extension_source`] when the daemon wrote one.
    fn tui(&self, l: &Launch) -> Option<Spawn> {
        let mut cmd = vec!["pi".to_owned()];
        cmd.extend(model_args(l));
        cmd.extend(session_args(l));
        if let Some(h) = l.hooks_path.as_deref().filter(|h| !h.is_empty()) {
            cmd.extend(["-e".to_owned(), h.to_owned()]);
        }
        Some(Spawn {
            cmd,
            env: l.extra_env.clone(),
            cwd: l.cwd.clone(),
        })
    }

    fn on_start(&mut self, _l: &Launch, out: &mut Out) {
        self.started = false;
        self.send(out, json!({"type": "get_state"}));
    }

    /// Re-attach (see the module docs). Pi says its session id and model only when asked, so a
    /// driver that was not given them asks `get_state`; its answer becomes `Started`.
    fn attach(&mut self, _l: &Launch, a: &Attach, out: &mut Out) {
        self.epoch = a.epoch.max(1);
        self.off.start_at(a.resume_offset);
        self.next_line = a.resume_offset;
        self.sid.clone_from(&a.agent_sid);
        self.model.clone_from(&a.model);
        // `Started` was already emitted for this session if the daemon knows its id
        self.started = a.agent_sid.is_some();
        // The request is somewhere at or after the replay's start; `on_ui` fixes the offset when
        // the line shows up, until then this keeps the answer listed.
        self.given = a
            .answers
            .iter()
            .map(|(id, ans)| (id.clone(), (a.resume_offset, ans.clone())))
            .collect();
        if a.agent_sid.is_none() {
            self.send(out, json!({"type": "get_state"}));
        }
    }

    fn on_line(&mut self, offset: u64, line: &[u8], out: &mut Out) -> Vec<AgentEvent> {
        self.off.seen(offset, line);
        self.next_line = offset.saturating_add(line.len() as u64).saturating_add(1);
        let mut ev = Vec::new();
        self.report_answers(&mut ev);
        let v = match common::parse(line) {
            Some(v) if v.is_object() => v,
            Some(v) => return [ev, vec![AgentEvent::Raw(v)]].concat(),
            None if line.iter().all(u8::is_ascii_whitespace) => return ev,
            None => return [ev, vec![common::raw_text(line)]].concat(),
        };
        match str_of(&v, "type") {
            "response" => self.on_response(offset, &v, &mut ev, out),
            "agent_start" => self.begin_turn(offset, &mut ev),
            "agent_settled" => self.end_turn(v["aborted"].as_bool().unwrap_or(false), &mut ev, out),
            "message_start" => self.on_message_start(offset, &v, &mut ev),
            "message_update" => self.on_update(offset, &v, &mut ev),
            "message_end" => self.on_message_end(offset, &v, &mut ev),
            "tool_execution_start" => self.on_tool_start(offset, &v, &mut ev),
            "tool_execution_end" => self.on_tool_end(&v, &mut ev),
            "extension_ui_request" => self.on_ui(offset, &v, &mut ev),
            "extension_error" => {
                let text = format!(
                    "Extension error ({}, {}): {}",
                    str_of(&v, "extensionPath"),
                    str_of(&v, "event"),
                    str_of(&v, "error")
                );
                ev.push(note(offset, text));
            }
            "auto_retry_start" => {
                let text = format!(
                    "Retrying ({}/{}) in {} ms: {}",
                    num(&v["attempt"]),
                    num(&v["maxAttempts"]),
                    num(&v["delayMs"]),
                    str_of(&v, "errorMessage")
                );
                ev.push(note(offset, text));
            }
            "auto_retry_end" => {
                if v["success"].as_bool() == Some(false) {
                    ev.push(note(
                        offset,
                        format!("Retries gave up: {}", str_of(&v, "finalError")),
                    ));
                }
            }
            "compaction_end" => {
                let text = if v["aborted"].as_bool() == Some(true) {
                    "Compaction aborted".to_owned()
                } else if v["result"].is_object() {
                    format!(
                        "Context compacted ({}), {} tokens before",
                        str_of(&v, "reason"),
                        num(&v["result"]["tokensBefore"])
                    )
                } else {
                    format!("Compaction failed: {}", str_of(&v, "errorMessage"))
                };
                ev.push(note(offset, text));
            }
            "agent_end"
            | "turn_start"
            | "turn_end"
            | "queue_update"
            | "tool_execution_update"
            | "compaction_start"
            | "summarization_retry_scheduled"
            | "summarization_retry_attempt_start"
            | "summarization_retry_finished"
            | "session_info_changed"
            | "thinking_level_changed"
            | "entry_appended"
            | "bash_execution_update" => {}
            _ => ev.push(AgentEvent::Raw(v)),
        }
        ev
    }

    /// Idle: `prompt`. Busy: `Queue` is held until pi settles, `Steer` goes into the run,
    /// `InterruptThenSend` is held in front (behind earlier ones) and interrupts. A prompt pi turns
    /// down because a run it had not reported goes on is held again, ahead of the held ones and in
    /// the order given. Refused with `Error::State` once pi exited.
    fn prompt(&mut self, text: &str, how: PromptHow, out: &mut Out) -> Result<(), Error> {
        if self.exited {
            return Err(Error::State("pi has exited"));
        }
        let h = common::held(text, how);
        if !self.busy() {
            self.send_prompt(h, false, out);
            return Ok(());
        }
        match how {
            PromptHow::Steer if self.in_turn => self.send_prompt(h, true, out),
            PromptHow::Queue | PromptHow::Steer => common::hold(&mut self.queue, h),
            PromptHow::InterruptThenSend => {
                common::hold(&mut self.queue, h);
                self.send_interrupt(out);
            }
        }
        Ok(())
    }

    /// Cancel the open dialogs, then `clear_queue` + `abort`. Held prompts stay and run after the
    /// run; pi's own steer queue is cleared. With nothing running it only cancels dialogs, if
    /// any: `Ok`, and nothing is written when there are none.
    fn interrupt(&mut self, out: &mut Out) -> Result<(), Error> {
        if self.exited {
            return Err(Error::State("pi has exited"));
        }
        if self.busy() {
            self.send_interrupt(out);
        } else {
            self.cancel_dialogs(out);
        }
        Ok(())
    }

    /// `select`: `Option(i)` (or one-element `Options`, or the option's text); `confirm`: option 0
    /// is Yes, 1 is No (or `Text` yes/no); `input`/`editor`: `Text`; any dialog: `Cancel`.
    fn answer(&mut self, request_id: &str, answer: &Answer, out: &mut Out) -> Result<(), Error> {
        if self.exited {
            return Err(Error::State("pi has exited"));
        }
        let d = self
            .questions
            .get(request_id)
            .ok_or_else(|| Error::UnknownRequest(request_id.to_owned()))?;
        let opts = &d.q.options;
        let pick = |i: u16| {
            opts.get(usize::from(i))
                .cloned()
                .ok_or_else(|| Error::Bad(format!("no option {i}")))
        };
        let (key, val) = match (d.method.as_str(), answer) {
            (_, Answer::Cancel) => ("cancelled", json!(true)),
            ("confirm", Answer::Option(i)) => ("confirmed", json!(pick(*i)? == "Yes")),
            ("confirm", Answer::Options(v)) if v.len() == 1 => {
                ("confirmed", json!(pick(v[0])? == "Yes"))
            }
            ("confirm", Answer::Text(t)) => match t.trim().to_ascii_lowercase().as_str() {
                "yes" | "y" | "true" => ("confirmed", json!(true)),
                "no" | "n" | "false" => ("confirmed", json!(false)),
                _ => return Err(Error::Bad(format!("confirm takes yes or no, not {t:?}"))),
            },
            ("select", Answer::Option(i)) => ("value", json!(pick(*i)?)),
            ("select", Answer::Options(v)) if v.len() == 1 => ("value", json!(pick(v[0])?)),
            ("select", Answer::Text(t)) if opts.contains(t) => ("value", json!(t)),
            ("input" | "editor", Answer::Text(t)) => ("value", json!(t)),
            (m, a) => return Err(Error::Bad(format!("a pi {m} dialog cannot take {a:?}"))),
        };
        let mut resp = json!({"type": "extension_ui_response", "id": request_id});
        resp[key] = val;
        out.line(&resp);
        let offset = d.offset;
        self.questions.remove(request_id);
        self.off.item_close(request_id);
        self.answered.push(request_id.to_owned());
        self.given
            .insert(request_id.to_owned(), (offset, answer.clone()));
        Ok(())
    }

    /// `provider/model`; a bare model id keeps the current provider. Confirmed by
    /// [`AgentEvent::ModelChanged`] when pi answers.
    fn set_model(&mut self, model: &str, out: &mut Out) -> Result<(), Error> {
        if self.exited {
            return Err(Error::State("pi has exited"));
        }
        let model = model.trim();
        let (provider, id) = match model.split_once('/') {
            Some((p, m)) if !p.is_empty() && !m.is_empty() => (p.to_owned(), m.to_owned()),
            Some(_) => {
                return Err(Error::Bad(format!(
                    "pi wants provider/model, got {model:?}"
                )))
            }
            None => match self.model.as_deref().and_then(|cur| cur.split_once('/')) {
                Some((p, _)) if !model.is_empty() => (p.to_owned(), model.to_owned()),
                _ => {
                    return Err(Error::Bad(format!(
                        "pi wants provider/model, got {model:?}"
                    )))
                }
            },
        };
        self.send(
            out,
            json!({"type": "set_model", "provider": provider, "modelId": id}),
        );
        Ok(())
    }

    fn resume_offset(&self) -> u64 {
        self.off.resume()
    }

    fn held_prompts(&self) -> Vec<HeldPrompt> {
        self.retry.iter().chain(&self.queue).cloned().collect()
    }

    /// The answers whose dialog request a replay from [`Driver::resume_offset`] still reads. Pi
    /// does not echo an answer, so these are all that keeps the replay from asking again; persist
    /// them together with the offset.
    fn pending_answers(&self) -> Vec<(String, Answer)> {
        let floor = self.off.resume();
        self.given
            .iter()
            .filter(|(_, (at, _))| *at >= floor)
            .map(|(id, (_, a))| (id.clone(), a.clone()))
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
        self.report_answers(&mut ev);
        let ids: Vec<String> = self.questions.keys().cloned().collect();
        for id in ids {
            self.resolve_dialog(&id, &mut ev);
        }
        for (id, o) in std::mem::take(&mut self.open) {
            self.off.item_close(&id);
            let b = if o.kind == ItemKind::ToolCall {
                self.call_body(&id)
            } else {
                text_body(o.kind, o.text)
            };
            ev.push(done(&id, o.kind, ItemStatus::Failed, b));
        }
        if self.in_turn {
            ev.push(AgentEvent::TurnEnded {
                stop: StopReason::Interrupted,
                usage: self.turn.usage,
                duration_ms: self.turn.duration(),
            });
            self.in_turn = false;
            self.off.turn_end();
        }
        self.announced = false;
        self.tools.clear();
        self.running.clear();
        self.blocks.clear();
        self.msg_tools.clear();
        self.msg = None;
        self.user_start = None;
        self.update_status(&mut ev);
        // Prompts that never ran: first those written that pi never acknowledged (oldest id
        // first), then those still held back, which also get a note in the transcript.
        let exited = |h: HeldPrompt| AgentEvent::PromptDropped {
            text: h.text,
            how: h.how,
            reason: DropReason::Exited,
        };
        ev.extend(
            std::mem::take(&mut self.pending)
                .into_iter()
                .map(|(_, h)| exited(h)),
        );
        let unsent = std::mem::take(&mut self.retry)
            .into_iter()
            .chain(std::mem::take(&mut self.queue));
        for (i, h) in unsent.enumerate() {
            let id = format!("o{}:unsent{i}", self.next_line);
            let text = format!("Not sent, pi exited: {}", h.text);
            ev.push(exited(h));
            ev.push(done(
                &id,
                ItemKind::Event,
                ItemStatus::Completed,
                body(&items::Event { text }),
            ));
        }
        ev.push(AgentEvent::Exited { code });
        ev
    }
}

fn model_args(l: &Launch) -> Vec<String> {
    match l.model.as_deref().filter(|m| !m.is_empty()) {
        Some(m) => vec!["--model".to_owned(), m.to_owned()],
        None => vec![],
    }
}

/// `--session-id <id>` opens (or creates) exactly that session; a path goes to `--session`.
fn session_args(l: &Launch) -> Vec<String> {
    match l.resume.as_deref().filter(|r| !r.is_empty()) {
        Some(r) if r.contains('/') || r.contains('\\') || r.ends_with(".jsonl") => {
            vec!["--session".to_owned(), r.to_owned()]
        }
        Some(r) => vec!["--session-id".to_owned(), r.to_owned()],
        None => vec![],
    }
}

fn done(id: &str, kind: ItemKind, status: ItemStatus, body: Value) -> AgentEvent {
    AgentEvent::ItemCompleted {
        id: id.to_owned(),
        kind,
        status,
        body,
    }
}

/// An `Event` item for the line at `offset`.
fn note(offset: u64, text: String) -> AgentEvent {
    done(
        &format!("o{offset}"),
        ItemKind::Event,
        ItemStatus::Completed,
        body(&items::Event { text }),
    )
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

/// A count: integers as they are, floats truncated, anything else (negative, junk) 0.
fn num(v: &Value) -> u64 {
    v.as_u64()
        .or_else(|| {
            v.as_f64()
                .filter(|f| f.is_finite() && *f > 0.0)
                .map(|f| f as u64)
        })
        .unwrap_or(0)
}

fn int(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (_, true) => a.to_owned(),
        (true, false) => b.to_owned(),
        (false, false) => format!("{a}\n\n{b}"),
    }
}

/// Message content (a string, or blocks) as text; non-text blocks become `[image]` placeholders.
fn flatten(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b["type"].as_str()? {
                "text" => Some(str_of(b, "text").to_owned()),
                t => Some(format!("[{t}]")),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn text_body(kind: ItemKind, text: String) -> Value {
    if kind == ItemKind::Thinking {
        body(&items::Thinking { text })
    } else {
        body(&items::AssistantMessage { text })
    }
}

/// `provider/id` of a pi model object; `None` without an id and for pi's `unknown` placeholder
/// (no model configured).
fn model_name(m: &Value) -> Option<String> {
    let (p, id) = (str_of(m, "provider"), str_of(m, "id"));
    if id.is_empty() || (p == "unknown" && id == "unknown") {
        return None;
    }
    Some(if p.is_empty() {
        id.to_owned()
    } else {
        format!("{p}/{id}")
    })
}

fn stop_reason(msg: &Value) -> StopReason {
    match str_of(msg, "stopReason") {
        "stop" | "" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "aborted" => StopReason::Interrupted,
        "error" => StopReason::Error(match str_of(msg, "errorMessage") {
            "" => "error".to_owned(),
            e => e.to_owned(),
        }),
        other => StopReason::Other(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines() {
        let d = PiDriver::new();
        let sid = "01a19bac-79a1-76a6-a417-ebee80fd5e59";
        let mut l = Launch {
            cwd: "/w".into(),
            model: Some("anthropic/claude-haiku-5-5".into()),
            resume: Some(sid.into()),
            ..Launch::default()
        };
        assert_eq!(
            d.spawn(&l).cmd,
            [
                "pi",
                "--mode",
                "rpc",
                "--model",
                "anthropic/claude-haiku-5-5",
                "--session-id",
                sid
            ]
        );
        l.resume = Some("/home/u/.pi/agent/sessions/--w--/x_1.jsonl".into());
        l.hooks_path = Some("/run/femme/pi-hook.ts".into());
        let t = d.tui(&l).unwrap();
        assert_eq!(
            t.cmd,
            [
                "pi",
                "--model",
                "anthropic/claude-haiku-5-5",
                "--session",
                "/home/u/.pi/agent/sessions/--w--/x_1.jsonl",
                "-e",
                "/run/femme/pi-hook.ts"
            ]
        );
        assert_eq!(t.cwd, "/w");
        assert!(!d.spawn(&l).cmd.contains(&"-e".to_owned()));
        let bare = Launch::default();
        assert_eq!(d.tui(&bare).unwrap().cmd, ["pi"]);
    }

    #[test]
    fn helpers() {
        assert_eq!(flatten(&json!("hi")), "hi");
        assert_eq!(
            flatten(&json!([{"type":"text","text":"a"},{"type":"image","data":"x"},{"no":"type"}])),
            "a\n[image]"
        );
        assert_eq!(
            model_name(&json!({"provider":"unknown","id":"unknown"})),
            None
        );
        assert_eq!(
            model_name(&json!({"provider":"mock","id":"mock-b"})).as_deref(),
            Some("mock/mock-b")
        );
        assert_eq!(
            stop_reason(&json!({"stopReason":"error"})),
            StopReason::Error("error".into())
        );
        assert_eq!(
            stop_reason(&json!({"stopReason":"length"})),
            StopReason::MaxTokens
        );
        assert_eq!(
            stop_reason(&json!({"stopReason":"toolUse"})),
            StopReason::Other("toolUse".into())
        );
        assert_eq!(
            (num(&json!(-3)), num(&json!(2.9)), num(&json!("7"))),
            (0, 2, 0)
        );
        let mut t = Turn::default();
        t.saw(Some(100), None);
        t.saw(Some(90), Some(150));
        assert_eq!(t.duration(), Some(60));
    }

    #[test]
    fn set_model_needs_a_provider() {
        let mut d = PiDriver::new();
        let mut out = Out::default();
        assert!(matches!(
            d.set_model("mock-b", &mut out),
            Err(Error::Bad(_))
        ));
        assert!(matches!(
            d.set_model("/mock-b", &mut out),
            Err(Error::Bad(_))
        ));
        d.set_model("mock/mock-b", &mut out).unwrap();
        d.model = Some("mock/mock-a".into());
        d.set_model("mock-c", &mut out).unwrap();
        let lines: Vec<Value> = out
            .lines()
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            lines,
            [
                json!({"id":"f1","type":"set_model","provider":"mock","modelId":"mock-b"}),
                json!({"id":"f2","type":"set_model","provider":"mock","modelId":"mock-c"})
            ]
        );
    }
}
