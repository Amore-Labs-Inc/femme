//! Sans-IO drivers for the coding agents FEMME runs headless: Claude Code (`stream-json` +
//! control requests), Codex (`app-server` JSON-RPC) and Pi (`--mode rpc`). Each speaks one
//! protocol on the agent's stdin/stdout and turns it into the same [`AgentEvent`] stream.
//!
//! **No IO here.** A driver takes one stdout line at a time and returns events, and pushes the
//! bytes it wants written to the agent's stdin into an [`Out`]. The session actor in `femmed`
//! owns the process (it lives in `femme-holder`), the spool file and the clock.
//!
//! # How the daemon uses a driver
//!
//! ```text
//! let mut d = driver_for(Agent::Claude);
//! let sp = d.spawn(&launch);            // -> holder SpawnAgent { cmd, cwd, env }
//! d.on_start(&launch, &mut out);        // fresh process only; flush out.stdin via AgentWrite
//! for (offset, line) in spool_lines {   // offset = absolute byte offset of the line in the spool
//!     for ev in d.on_line(offset, line, &mut out) { /* persist / broadcast */ }
//!     /* flush out.stdin via AgentWrite */
//! }
//! d.prompt("fix it", PromptHow::Queue, &mut out)?;   // user pressed send
//! d.resume_offset()                     // -> SpoolCommit { offset } after the events are durable
//! ```
//!
//! After a **daemon restart** the agent process is still alive in its holder. Make a fresh
//! driver, do *not* call [`Driver::on_start`]: call [`Driver::attach`] with what the daemon
//! persisted ([`Attach`]: the committed offset, the agent's session id and model, the answers
//! given to open questions, the attach counter), then replay the spool from `spool_committed`
//! (which is a previous [`Driver::resume_offset`]). Replay is idempotent by construction: item
//! ids come from the agent (or from the spool offset), so the same line always yields the same
//! ids, and the daemon upserts items by `(sid, item_id)` and dedups questions by `request_id`.
//! A driver never writes to stdin because of a *replayed* line, with two exceptions that only a
//! re-attached driver makes: it finishes a handshake the previous daemon died in the middle of
//! (Codex), and it refuses a server request it does not know (Codex, a JSON-RPC error that an
//! agent which no longer waits for it drops). Every other reply is explicit, via
//! [`Driver::answer`].
//!
//! # What a driver keeps that the spool does not hold
//!
//! * **Held prompts**: text accepted by [`Driver::prompt`] but not yet written to the agent
//!   (the session is not ready, or a turn runs and the agent has no queue of its own). They live
//!   in the driver only. [`Driver::held_prompts`] lists them so the daemon can persist them, and
//!   a daemon that wants them back after a restart calls [`Driver::prompt`] again for each
//!   (after [`Driver::attach`]). A prompt the driver gives up on is announced with one
//!   [`AgentEvent::PromptDropped`], never dropped silently: when the process exits
//!   ([`Driver::on_exit`]) and when the agent itself throws it away.
//! * **Answers**: [`Driver::pending_answers`] returns the answers given to questions that are
//!   still open (Claude holds the answers of a multi-question request until the last one is in;
//!   Codex and Pi write each at once but forget it on restart). Persist them and hand them back
//!   in [`Attach::answers`]; the replayed question is then not asked again, the status does not
//!   go `Blocked` for it, and [`Driver::answer`] refuses it.
//!
//! # Conventions shared by all drivers
//!
//! * A line is one complete JSON message without its `\n`. The next line starts at
//!   `offset + line.len() + 1`. Feed **complete lines only**: [`Driver::resume_offset`] moves to
//!   the end of every line it is given, so a partial line (bytes whose `\n` has not arrived)
//!   would put the committed offset in the middle of a line and the replay would start there.
//!   A line that contains a `\n` is a caller bug and trips a `debug_assert!`.
//! * Unknown or malformed messages never panic: known-but-uninteresting types produce no events,
//!   everything else produces [`AgentEvent::Raw`].
//! * Ids: the agent's own id where it has one (Claude message id + block index, tool-use ids,
//!   Codex item ids); otherwise `o<spool offset>`. Never random, so replay reproduces them.
//! * A tool call and its result are two items, [`ItemKind::ToolCall`] and
//!   [`ItemKind::ToolResult`], linked by `call_id` in the body JSON (see [`items`]). The result
//!   item's id is `<call_id>:result`.
//! * Item bodies are JSON shaped by the structs in [`items`]. `ItemUpdated`/`ItemCompleted`
//!   carry the **whole** new body, not a patch.
//! * `ItemDelta` text is appended to what the client already shows for that id; only
//!   assistant text and thinking stream.
//! * Lifecycles. Text/thinking: `ItemStarted`, `ItemDelta`*, `ItemCompleted` (full text).
//!   Tool call: `ItemStarted(ToolCall)` when it begins (input may still be streaming),
//!   `ItemUpdated` with the final input, then when its result arrives `ItemCompleted(ToolCall,
//!   Completed|Failed)` plus `ItemCompleted(ToolResult)` for `<call_id>:result` (a result needs no
//!   `ItemStarted` unless its output streams). The daemon creates a row for an `ItemCompleted`
//!   whose id it has not seen.
//! * Turn/status order: a turn opens with `TurnStarted` and `Status(Working)`. `Status(Working)`
//!   comes *first* when the agent had work queued before the turn began (Claude's `queued`
//!   frame, a prompt written while the CLI is still starting), `TurnStarted` first when the turn
//!   itself is the first sign of work. Then `QuestionAsked`, `Status(Blocked)` ...
//!   `QuestionResolved`, `Status(Working)` ... `TurnEnded`, `Status(Idle)` (unless more work is
//!   queued). `Status` is only emitted on change. `TurnEnded` is never emitted without an open
//!   turn, and a `TurnStarted` is never invented just to close one: a failure with no turn
//!   behind it (Claude resuming an unknown session) is an `Event` item instead.
//! * A prompt given before the session is ready (handshake not finished) is buffered by the
//!   driver and sent when it is. `Queue` while a turn runs is buffered and written when the turn
//!   ends (unless the agent natively queues); `Steer`/`InterruptThenSend` per [`Driver::prompt`].
//!   Buffered prompts are visible through [`Driver::held_prompts`] and announced by
//!   [`AgentEvent::PromptDropped`] if they are lost.

pub mod claude;
pub mod codex;
mod common;
pub mod hooks;
pub mod items;
pub mod pi;

use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::{Agent, ItemKind, ItemStatus, Question, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What to launch: filled in by the session actor from the session row and config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Launch {
    /// Working directory of the agent.
    pub cwd: String,
    /// Model alias or id the agent understands (`claude-haiku-5-5`, `gpt-5-codex`, ...).
    pub model: Option<String>,
    /// The agent's own session/thread id to continue (`claude --resume`, Codex `thread/resume`,
    /// Pi `--session-id` or `--session <path>`); `None` starts a new conversation.
    pub resume: Option<String>,
    /// Extra environment for the process (FEMME_SID and the like).
    pub extra_env: Vec<(String, String)>,
    /// Settings/hook file the daemon wrote (see [`hooks`]); drivers pass it to the agent when it
    /// has a place for it (Claude `--settings`).
    pub hooks_path: Option<String>,
}

/// What the daemon hands a fresh driver when it re-attaches to an agent process that outlived the
/// daemon ([`Driver::attach`]). Everything here is something the daemon persisted from the
/// events and calls of the previous incarnation; none of it can be rebuilt from the spool tail.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attach {
    /// The `spool_committed` offset the replay starts at (a previous [`Driver::resume_offset`]).
    /// [`Driver::resume_offset`] reports it until the replay has moved past it, so a restart
    /// before the first replayed line does not commit offset 0.
    pub resume_offset: u64,
    /// The agent's own session id, from the last [`AgentEvent::Started`]. Without it a driver
    /// cannot address the session until the agent says it again (Pi never does, Claude only at the
    /// next turn, Codex only through a thread lookup).
    pub agent_sid: Option<String>,
    /// The model in use, from the last `Started` or [`AgentEvent::ModelChanged`] (Pi needs it to
    /// complete a bare model id into `provider/id`).
    pub model: Option<String>,
    /// Answers the daemon got from [`Driver::pending_answers`] and persisted: `(request_id,
    /// answer)` for every question that was already answered when the daemon went down.
    pub answers: Vec<(String, Answer)>,
    /// How many times this session has been re-attached, counting this one (the daemon persists
    /// and increments it; 0 is a fresh start). Drivers that number their own requests (Codex,
    /// Pi) put it into the ids, so the ids of two incarnations never meet in one spool and a
    /// replayed response of the old one cannot be taken for the answer to a new request.
    pub epoch: u32,
}

/// A prompt [`Driver::prompt`] accepted that the driver has not written to the agent yet: it waits
/// for the handshake, for the running turn to end, or for an interrupt to land. In driver memory
/// only (see [`Driver::held_prompts`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldPrompt {
    /// The text, as given to `prompt`.
    pub text: String,
    /// The mode it was given with (`InterruptThenSend` prompts go first, in order).
    pub how: PromptHow,
    /// Unix milliseconds at which the driver accepted it.
    pub queued_at_ms: u64,
}

/// Why a prompt will never run ([`AgentEvent::PromptDropped`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DropReason {
    /// The process exited before the agent ran it; sending it again to a new process is safe.
    Exited,
    /// The agent took it off its own queue or refused it, in its own words (Claude's
    /// `command_lifecycle` `cancelled`/`discarded`/`refused`, Codex's `turn/start` error, Pi
    /// refusing a `prompt`).
    Refused(String),
}

/// A command line for the holder to run (`SpawnAgent` for headless, `SpawnPty` for a TUI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    /// argv, `cmd[0]` is the program (looked up on PATH).
    pub cmd: Vec<String>,
    /// Environment entries to add to the holder's.
    pub env: Vec<(String, String)>,
    /// Working directory.
    pub cwd: String,
}

/// Bytes the driver wants written to the agent's stdin. The caller drains `stdin` after every
/// driver call (`std::mem::take`) and sends it with `HolderReq::AgentWrite`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Out {
    /// Raw bytes, each message already newline-terminated.
    pub stdin: Vec<u8>,
}

impl Out {
    /// Append one JSON message and its newline.
    pub fn line(&mut self, v: &Value) {
        if let Ok(mut b) = serde_json::to_vec(v) {
            self.stdin.append(&mut b);
            self.stdin.push(b'\n');
        }
    }

    /// Take the pending bytes, leaving the buffer empty.
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.stdin)
    }

    /// The pending messages as text lines (for tests and logs).
    pub fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.stdin)
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// Why a turn ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// The agent finished normally.
    EndTurn,
    /// Output limit reached.
    MaxTokens,
    /// The user (or the daemon) interrupted it.
    Interrupted,
    /// The turn failed; the text is the agent's own message.
    Error(String),
    /// Anything else the agent reported, verbatim.
    Other(String),
}

/// Coarse state the daemon shows and notifies on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AgentStatus {
    /// Running a turn.
    Working,
    /// Waiting for a prompt.
    #[default]
    Idle,
    /// Waiting for the user to answer a question.
    Blocked,
}

/// One thing that happened, in stream order. Serialize with JSON: bodies are `serde_json::Value`
/// (not postcard-safe). The daemon maps these onto `femme_proto::state::Item` rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentEvent {
    /// The agent told us its session id (`claude --resume <id>`, Codex thread id, Pi session id).
    Started {
        /// The agent's own session id.
        agent_sid: String,
        /// Model in use, if reported.
        model: Option<String>,
    },
    /// A turn began (the agent is working).
    TurnStarted,
    /// A turn ended; the agent is idle again (or blocked, never: questions end up in the turn).
    TurnEnded {
        /// Why.
        stop: StopReason,
        /// Token usage of the turn, if reported.
        usage: Option<Usage>,
        /// Wall-clock duration as the agent measured it.
        duration_ms: Option<u64>,
    },
    /// A transcript item opened. `preview` is a short label for list rows, known at that moment.
    ItemStarted {
        /// Item id, unique per session.
        id: String,
        /// What it is.
        kind: ItemKind,
        /// What the driver knows when the item opens, at most ~512 bytes (cut on a char
        /// boundary). For a tool call: the tool's name (Claude, Pi) or what Codex shows for the
        /// call (command line, changed paths, search query or tool name). For a Codex text item:
        /// the text the agent already has (usually none yet). Empty for Claude and Pi text and
        /// thinking, whose text arrives as [`AgentEvent::ItemDelta`]s. It is not the first bytes
        /// of the final text; take that from `ItemCompleted`.
        preview: String,
        /// The id of the `Agent` tool call whose subagent produced this item; `None` for the
        /// main thread.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    /// Streaming text appended to an open item (assistant text, thinking).
    ItemDelta {
        /// Item id.
        id: String,
        /// Appended text.
        text: String,
    },
    /// The item's body changed without finishing: tool input finalised, plan text, etc. Whole body.
    ItemUpdated {
        /// Item id.
        id: String,
        /// The new body (shape per [`items`]).
        body: Value,
    },
    /// The item is final.
    ItemCompleted {
        /// Item id.
        id: String,
        /// Kind (repeated so a replay that missed `ItemStarted` can still create the row).
        kind: ItemKind,
        /// `Completed`, `Failed`, `Answered`.
        status: ItemStatus,
        /// The final body (shape per [`items`]).
        body: Value,
        /// The id of the `Agent` tool call whose subagent produced this item; `None` for the
        /// main thread.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    /// The agent asks the user something and waits. Answer with [`Driver::answer`] using
    /// `q.request_id`. The daemon creates the `Question` item `item_id` as `Pending`.
    QuestionAsked {
        /// Id of the `Question` item (the AskUserQuestion tool call's id for Claude).
        item_id: String,
        /// The question; `q.request_id` goes back into [`Driver::answer`].
        q: Question,
    },
    /// A question is no longer pending (answered, cancelled by the agent, or the turn died).
    QuestionResolved {
        /// The request id from the matching `QuestionAsked`.
        request_id: String,
    },
    /// A prompt the driver had accepted will never run. One event per prompt, in the order they
    /// would have run, with the text so the daemon can show it, offer it again or resend it. See
    /// [`DropReason`]; [`Driver::on_exit`] emits these before its `Exited`.
    PromptDropped {
        /// The text, as given to [`Driver::prompt`].
        text: String,
        /// The mode it was given with.
        how: PromptHow,
        /// Why it was dropped.
        reason: DropReason,
    },
    /// A usage-limit reading the agent reported in-stream.
    RateLimit {
        /// `session` (5-hour) or `weekly`, `opus`, ... as the agent names it.
        kind: String,
        /// 0..=100.
        percent: f32,
        /// Unix seconds when the window resets.
        resets_at: Option<i64>,
    },
    /// The model in use changed (after a `set_model`, or a fallback).
    ModelChanged(String),
    /// The `/` commands the agent takes (names without the `/`), as it started.
    Commands(Vec<String>),
    /// Coarse state transition; only emitted when the state actually changes.
    Status(AgentStatus),
    /// The process exited (from [`Driver::on_exit`]).
    Exited {
        /// Exit code, `None` if it was signalled.
        code: Option<i32>,
    },
    /// A message the driver does not map, kept for debugging.
    Raw(Value),
}

/// Why a driver call was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// This agent (or this launch) cannot do that, e.g. `Steer` on an agent without mid-turn input.
    #[error("not supported: {0}")]
    Unsupported(&'static str),
    /// No pending request with that id: it was already answered, the agent cancelled it, or it
    /// was never asked.
    #[error("no pending request {0} (already answered, cancelled by the agent, or never asked)")]
    UnknownRequest(String),
    /// The call does not make sense in the current state: the session has not started, or the
    /// process has exited (every call that would write to the agent says so after
    /// [`Driver::on_exit`]). Interrupting an idle session is *not* an error: it is an `Ok` that
    /// writes nothing.
    #[error("{0}")]
    State(&'static str),
    /// The argument cannot be sent (e.g. an `Answer` that does not fit the question).
    #[error("{0}")]
    Bad(String),
}

/// One agent protocol. See the crate docs for the call sequence.
pub trait Driver: Send {
    /// Which agent this drives.
    fn agent(&self) -> Agent;

    /// The headless (pipe mode) command line.
    fn spawn(&self, l: &Launch) -> Spawn;

    /// The same session in the agent's interactive TUI (takeover), or `None` if it has none.
    fn tui(&self, l: &Launch) -> Option<Spawn>;

    /// A fresh process just started: write whatever handshake the protocol needs (Claude
    /// `initialize`, Codex `initialize` + `thread/start`). `l` is the launch used for
    /// [`Driver::spawn`]. Not called when re-attaching to a running agent after a daemon restart:
    /// that is [`Driver::attach`].
    fn on_start(&mut self, l: &Launch, out: &mut Out);

    /// Re-attach to an agent process that outlived the daemon. Call it once on a fresh driver,
    /// instead of [`Driver::on_start`] and **before** the spool replay starts; `l` is the launch
    /// the process was started with, `a` what the daemon persisted. Afterwards
    /// [`Driver::resume_offset`] starts at `a.resume_offset`, [`Driver::agent_session_id`] and the
    /// model are known, questions in `a.answers` are not asked again, and the ids of requests the
    /// driver itself sends carry `a.epoch`. `out` is for the one case where the driver has to ask
    /// the agent something to catch up (Pi without a known session id re-sends `get_state`).
    fn attach(&mut self, l: &Launch, a: &Attach, out: &mut Out);

    /// One stdout line at its absolute byte `offset` in the spool file. The line is **complete**
    /// and has no `\n`; the next one starts at `offset + line.len() + 1` and
    /// [`Driver::resume_offset`] never points into the middle of a line it was given. Debug builds
    /// assert that `line` holds no `\n`.
    fn on_line(&mut self, offset: u64, line: &[u8], out: &mut Out) -> Vec<AgentEvent>;

    /// Send the user's text. `Queue` runs after the current turn, `Steer` injects into the
    /// running turn, `InterruptThenSend` aborts the turn first. Returns `Error::Unsupported` when
    /// the agent cannot do the requested mode, and `Error::State` once the process has exited.
    ///
    /// Held prompts stay in order: `Queue` prompts run oldest first, and `InterruptThenSend`
    /// prompts go in front of them but behind earlier `InterruptThenSend` ones. Prompts that are
    /// still held when a turn is interrupted ([`Driver::interrupt`]) are **kept** and run after
    /// the interrupted turn, in that order. A `Steer` the agent turns down because the turn is
    /// already over (Codex, Pi) runs next, ahead of the held prompts, in the order the steers were
    /// given.
    fn prompt(&mut self, text: &str, how: PromptHow, out: &mut Out) -> Result<(), Error>;

    /// Stop the current turn (like Esc in the TUI). Held prompts are kept ([`Driver::prompt`]).
    /// While no turn runs this is an `Ok` that writes nothing (Pi still cancels an open dialog,
    /// Claude refuses with `Error::State` until its `initialize` is answered); after
    /// [`Driver::on_exit`] it is `Error::State`.
    fn interrupt(&mut self, out: &mut Out) -> Result<(), Error>;

    /// Answer a pending [`AgentEvent::QuestionAsked`]. A question that was already answered
    /// (also one the daemon reported as answered through [`Attach::answers`]) or that the agent
    /// withdrew is `Error::UnknownRequest`; after [`Driver::on_exit`] it is `Error::State`.
    fn answer(&mut self, request_id: &str, answer: &Answer, out: &mut Out) -> Result<(), Error>;

    /// Switch model for the following turns.
    fn set_model(&mut self, model: &str, out: &mut Out) -> Result<(), Error>;

    /// Smallest spool offset still needed to rebuild in-progress state after a daemon restart:
    /// the first line of the unfinished turn (or oldest unfinished item/question/queued command),
    /// or the offset just after the last line seen when nothing is in flight (the offset given to
    /// [`Driver::attach`] before any line was seen). It always sits on a line boundary, which
    /// needs [`Driver::on_line`] to be fed complete lines only.
    fn resume_offset(&self) -> u64;

    /// Prompts [`Driver::prompt`] accepted that are not written to the agent yet, in the order
    /// they will go out, so the daemon can persist them. Prompts the agent already holds (Claude
    /// queues messages natively) are not listed: the agent keeps those across a daemon restart.
    fn held_prompts(&self) -> Vec<HeldPrompt>;

    /// `(request_id, answer)` for every question that was answered through [`Driver::answer`] and
    /// whose request line a replay from [`Driver::resume_offset`] can still read: until the agent
    /// reports the question closed (Claude, Codex), or while the dialog's line is at or after the
    /// resume offset (Pi, which never echoes an answer). Persist the list together with the offset
    /// after every `answer` call and give it back in [`Attach::answers`] after a restart.
    fn pending_answers(&self) -> Vec<(String, Answer)>;

    /// The agent's own session id once known.
    fn agent_session_id(&self) -> Option<&str>;

    /// Current coarse status.
    fn status(&self) -> AgentStatus;

    /// The process exited (stdout EOF). Closes everything still open: in-flight items become
    /// `Failed`, a running turn ends as `Interrupted`, pending questions resolve, every prompt not
    /// run yet is announced with [`AgentEvent::PromptDropped`] ([`DropReason::Exited`]); the last
    /// event is [`AgentEvent::Exited`]. The driver is spent afterwards: `prompt`, `interrupt`,
    /// `answer` and `set_model` return `Error::State` and write nothing.
    fn on_exit(&mut self, code: Option<i32>) -> Vec<AgentEvent>;
}

/// The driver for an agent. `Agent::Shell` gets a driver with no protocol (it only knows its
/// command line); every other call on it returns [`Error::Unsupported`].
pub fn driver_for(agent: Agent) -> Box<dyn Driver> {
    match agent {
        Agent::Claude => Box::new(claude::ClaudeDriver::new()),
        Agent::Codex => Box::new(codex::CodexDriver::new()),
        Agent::Pi => Box::new(pi::PiDriver::new()),
        Agent::Shell => Box::new(common::ShellDriver),
    }
}
