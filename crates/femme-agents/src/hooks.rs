//! PTY/TUI-mode status reporting. In PTY mode (M1, and takeover) the agent runs its own TUI and
//! femmed sees only a terminal. To still know whether it is working, idle or waiting for the user,
//! the daemon hands each agent a config that makes the agent itself run
//! `femmed hook <agent> <event> [<payload>]` at the moments that matter. The `hook` subcommand reads
//! the payload, tags it with `$FEMME_SID` and POSTs it to the daemon, which calls [`parse_hook`].
//!
//! | agent | daemon passes | the agent runs | payload |
//! |---|---|---|---|
//! | Claude | `claude --settings <file-or-json>` ([`claude_settings_json`]) | `femmed hook claude <Event>` (exec form, no shell) | JSON on stdin |
//! | Codex | `codex --no-daemon <args>` ([`codex_hook_args`]) | `$SHELL -c "<femmed> hook codex <Event>"` | JSON on stdin |
//! | Codex, fallback | `codex <args>` ([`codex_notify_args`]) | `femmed hook codex notify <json>` (no shell) | JSON as the LAST argv element |
//! | Pi | `pi -e <file.ts>` ([`pi_extension_source`]) | `femmed hook pi <event>` (no shell) | one JSON line on stdin |
//!
//! `<Event>` is the agent's own hook event name (`SessionStart`, `Stop`, ...), the pi event name
//! (`session_start`, ...) or `notify`; femmed passes it to [`parse_hook`] unchanged.
//!
//! Environment: every agent passes its environment on to the hook process, so `FEMME_SID` set on
//! the agent process is all `femmed hook` needs to name the session. Nothing else is required.
//!
//! Rules for `femmed hook`: print nothing on stdout (Claude and Codex read hook stdout: context for
//! the model, or even a decision that answers a permission dialog), always exit 0 (exit 2 blocks
//! the action in Claude and Codex), finish in well under [`HOOK_TIMEOUT_SECS`] (Claude and Codex
//! wait for their hooks), read the payload from the last argument when there are 4 (Codex notify)
//! and from stdin until EOF otherwise, and treat an empty or broken payload as `{}`.
//!
//! Verified against `claude` 2.1.302, `codex` 0.163.0 and `pi` 1.3.0; the payloads are in
//! `fixtures/hooks/` (captured by `scripts/capture-hooks.py`), the findings in `NOTES.md`.

use femme_proto::state::Agent;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// What an agent's hook told us. The daemon turns these into the session's status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    /// The agent session began, resumed, or was replaced (`/clear` starts a new session id in
    /// Claude and Codex).
    Started {
        /// The agent's own session id (`claude --resume <id>`, Codex thread id, Pi session id).
        agent_sid: Option<String>,
        /// Its transcript file, when the agent says (Claude and Codex `transcript_path`, Pi session
        /// file): what femmed reads to backfill a takeover gap.
        transcript: Option<String>,
    },
    /// A turn is running (prompt submitted, or a tool finished after the user answered a dialog).
    Working,
    /// The turn finished or was interrupted; the agent waits for a prompt.
    Idle,
    /// The agent waits for the user: a permission dialog, a question, a plan approval.
    Blocked {
        /// What it needs, one short line for the board and ntfy (`A or B? (A, B)`, `Allow Bash: rm -rf x`).
        text: String,
    },
    /// The agent session ended (the process is exiting).
    Stopped,
}

/// Timeout, in seconds, of every hook command in the generated configs (Codex `SessionEnd` and
/// `Interrupt`: 3, the most Codex allows). `femmed hook` must answer long before: Claude and Codex
/// wait for their hooks, and `SessionEnd` hooks hold up the exit.
pub const HOOK_TIMEOUT_SECS: u64 = 5;

// ---------------------------------------------------------------------------------------- Claude

/// The Claude hook events the generated settings subscribe to, with their matcher.
///
/// * `SessionStart` -> `Started` (sources startup, resume, clear, compact, fork).
/// * `UserPromptSubmit` -> `Working` (also fires for injected prompts such as background-task
///   notifications; not for `/clear` or `/exit`).
/// * `PermissionRequest` -> `Blocked`: fires the moment any dialog is shown, including
///   AskUserQuestion and ExitPlanMode, and also under `bypassPermissions`.
/// * `PostToolUse`, `PostToolUseFailure` -> `Working`: the dialog was answered and the turn goes on.
///   Exception: a `PostToolUseFailure` whose payload has `is_interrupt: true` -> `Idle`. The flag
///   is in the hook's schema (the captured failure,
///   `fixtures/hooks/claude-PostToolUseFailure-Bash.json`, has `is_interrupt: false`) and marks a
///   tool the user interrupted, which ends the turn without a `Stop`. claude 2.1.302 never sent
///   one: an Esc during a running Bash call fired no hook at all (see below), so this arm has not
///   been seen against a real interrupt. It is the right answer if a later version reports one.
/// * `Notification` (`idle_prompt`, `elicitation_dialog`, `elicitation_url_dialog` only) ->
///   `Idle` / `Blocked`. `idle_prompt` comes 60 s after the last interaction and repairs the one
///   gap hooks have: an Esc interrupt fires no hook at all (no `Stop`, no `PostToolUseFailure`,
///   not even with `is_interrupt`). `permission_prompt` is left out on
///   purpose: it repeats `PermissionRequest` 6 s late and could race the `PostToolUse` of an answer
///   given at that moment ([`parse_hook`] still understands it).
/// * `Stop`, `StopFailure` -> `Idle`.
/// * `SessionEnd` -> `Stopped`, except reasons `clear` and `resume` (a new `SessionStart` follows
///   in the same process).
pub const CLAUDE_HOOKS: &[(&str, Option<&str>)] = &[
    ("SessionStart", None),
    ("UserPromptSubmit", None),
    ("PermissionRequest", None),
    ("PostToolUse", None),
    ("PostToolUseFailure", None),
    (
        "Notification",
        Some("idle_prompt|elicitation_dialog|elicitation_url_dialog"),
    ),
    ("Stop", None),
    ("StopFailure", None),
    ("SessionEnd", None),
];

/// Settings JSON for `claude --settings <file-or-json>`: one command hook per [`CLAUDE_HOOKS`]
/// event, in exec form (`command` + `args`, spawned without a shell, so any `femmed_bin` path
/// works unquoted), running `femmed hook claude <Event>` with the payload on stdin. These hooks
/// are merged with the user's own settings and plugin hooks; nothing is overridden.
pub fn claude_settings_json(femmed_bin: &str) -> String {
    let mut hooks = Map::new();
    for &(event, matcher) in CLAUDE_HOOKS {
        let mut entry = json!({ "hooks": [{
            "type": "command",
            "command": femmed_bin,
            "args": ["hook", "claude", event],
            "timeout": HOOK_TIMEOUT_SECS,
        }]});
        if let Some(m) = matcher {
            entry["matcher"] = json!(m);
        }
        hooks.insert(event.to_owned(), json!([entry]));
    }
    serde_json::to_string_pretty(&json!({ "hooks": hooks })).unwrap_or_default()
}

// ----------------------------------------------------------------------------------------- Codex

/// The Codex hook events [`codex_hook_args`] registers, with the snake-case label Codex uses in
/// hook keys and trust hashes.
///
/// * `SessionStart` -> `Started` (sources `startup`, `resume`, `clear`: `/clear` starts a new
///   thread id in the same process).
/// * `UserPromptSubmit` -> `Working` (also the answer to a question, which Codex submits as the
///   next prompt).
/// * `PreToolUse` -> `Blocked` for the question tool (`request_user_input*`), else nothing.
/// * `PermissionRequest` -> `Blocked`: the approval overlay is shown.
/// * `PostToolUse` -> `Working` (an approval was granted and the turn goes on); nothing for the
///   async question tool, whose question stays open after the call returns.
/// * `Stop` -> `Idle`; `Interrupt` (Esc) -> `Idle`. **A `Stop` can come while a question is still
///   on screen.** The TUI's question tool (`request_user_input_async`) returns at once and the
///   turn ends, so `PreToolUse` (`Blocked`), `PostToolUse` (ignored) and, 1.9 s after the first,
///   a `Stop` follow each other while the question waits for its answer, which arrives as the
///   next `UserPromptSubmit`. That `Stop` says nothing about an open question: the hook cannot
///   tell, and [`parse_hook`] reports `Idle` for it (`fixtures/hooks/codex-Stop-question.json`).
///   The only hints are the `turn_id` of the `Blocked` call and `last_assistant_message: null`
///   (a finished turn has its text, `codex-Stop.json`). A daemon that wants the board to keep
///   saying "needs you" through that gap holds `Blocked` when a `Stop` follows a
///   `PreToolUse request_user_input*` of the same turn within a few seconds (1.9 s in the
///   capture) and lets the next `UserPromptSubmit` clear it; `parse_hook` itself is stateless.
/// * `SessionEnd` -> `Stopped` (on `/quit`, once per thread of the process, also the threads
///   `/clear` left behind; not on SIGTERM).
pub const CODEX_HOOKS: &[(&str, &str)] = &[
    ("SessionStart", "session_start"),
    ("UserPromptSubmit", "user_prompt_submit"),
    ("PreToolUse", "pre_tool_use"),
    ("PermissionRequest", "permission_request"),
    ("PostToolUse", "post_tool_use"),
    ("Stop", "stop"),
    ("Interrupt", "interrupt"),
    ("SessionEnd", "session_end"),
];

/// Extra `codex` arguments registering `femmed hook codex <Event>` for every [`CODEX_HOOKS`] event
/// in Codex's session-flags config layer (nothing is written to `~/.codex`), plus one `hooks.state`
/// override that marks exactly these hooks as trusted, so they run without the "Hooks need review"
/// dialog and without `--dangerously-bypass-hook-trust` (which would also run the user's
/// untrusted hooks). Codex runs each command with `$SHELL -c` in a new session (no controlling
/// tty), the payload JSON on stdin; the femmed path is shell-quoted.
///
/// The override does not replace the user's own `[hooks.state]` (their trust decisions and
/// `enabled = false` entries): Codex merges config layers table by table, and every key here
/// starts with `/<session-flags>/config.toml:`, which no key of the user's file can equal (theirs
/// start with the file's path). Checked on codex 0.163.0 with `hooks/list`: a user hook switched
/// off in `~/.codex/config.toml` stayed disabled and trusted while all generated hooks were
/// trusted (`codex_hook_state_merges_with_the_users`, ignored test, offline).
///
/// Launch with `--no-daemon` as well (`codex --no-daemon <these args>`, `codex resume <id> ...`):
/// hooks then run as children of the TUI and inherit `FEMME_SID`; under a shared background server
/// they would get the server's environment. `SessionStart` only fires with the first prompt, not at
/// launch. A user's own untrusted hook still raises the review dialog at startup (the user's call).
pub fn codex_hook_args(femmed_bin: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut state = Vec::new();
    for &(event, label) in CODEX_HOOKS {
        let timeout = match event {
            "SessionEnd" | "Interrupt" => HOOK_TIMEOUT_SECS.min(3),
            _ => HOOK_TIMEOUT_SECS,
        };
        let command = format!("{} hook codex {event}", sh_quote(femmed_bin));
        args.push("-c".to_owned());
        args.push(format!(
            "hooks.{event}=[{{hooks=[{{type=\"command\",command={},timeout={timeout}}}]}}]",
            toml_str(&command)
        ));
        let key = format!("/<session-flags>/config.toml:{label}:0:0");
        let hash = codex_hook_hash(label, &command, timeout);
        state.push(format!(
            "{}={{trusted_hash={}}}",
            toml_str(&key),
            toml_str(&hash)
        ));
    }
    // One inline table: `-c` splits its key at every '.', quoted or not, and a second
    // `-c hooks.state.<x>=...` would replace the first.
    args.push("-c".to_owned());
    args.push(format!("hooks.state={{{}}}", state.join(",")));
    args
}

/// Codex's `trusted_hash` of one hook handler: sha256 over the key-sorted compact JSON of
/// `{event_name, hooks: [handler]}` (codex 0.163 `hooks/src/engine/discovery.rs`; checked against
/// the hashes its `hooks/list` reports).
fn codex_hook_hash(label: &str, command: &str, timeout: u64) -> String {
    // Keys written in sorted order, so the output is the same with serde_json's preserve_order.
    let ident = json!({
        "event_name": label,
        "hooks": [{ "async": false, "command": command, "timeout": timeout, "type": "command" }],
    });
    format!("sha256:{:x}", Sha256::digest(ident.to_string().as_bytes()))
}

/// Fallback for Codex builds without lifecycle hooks: `-c notify=[<femmed>, "hook", "codex",
/// "notify"]`. Codex execs it (no shell) after every completed turn, the JSON payload appended as
/// the last argv element. Not needed with [`codex_hook_args`]: its `Stop` says the same, and notify
/// also fires for Codex's internal title-generation turn in the middle of the first real turn
/// ([`parse_hook`] ignores those).
pub fn codex_notify_args(femmed_bin: &str) -> Vec<String> {
    vec![
        "-c".to_owned(),
        format!(
            "notify=[{}, \"hook\", \"codex\", \"notify\"]",
            toml_str(femmed_bin)
        ),
    ]
}

// -------------------------------------------------------------------------------------------- Pi

/// TypeScript source of the pi extension; femmed writes it to a file and starts `pi -e <file>`.
/// It does nothing unless `FEMME_SID` is set. Then, for `session_start`, `agent_start`,
/// `turn_start`, `agent_settled`, `ui_prompt_start`, `ui_prompt_end` and `session_shutdown`, it
/// runs `femmed hook pi <event>` (argv, no shell, detached, killed after 3 s) with one JSON line on
/// stdin: `{event, seq, ts, pid, mode, session_id, session_file, idle, ...event fields}`. Calls
/// run one at a time in event order; `session_shutdown` waits up to 1.5 s for its call.
pub fn pi_extension_source(femmed_bin: &str) -> String {
    PI_EXTENSION.replacen("__FEMMED_BIN__", &js_str(femmed_bin), 1)
}

const PI_EXTENSION: &str = r#"// @ts-nocheck
// FEMME status hook for pi (generated by femmed on every launch; do not edit).
//
// femmed loads this file for one run only:  pi -e <this file> ...  (FEMME_SID set in the env).
// Without FEMME_SID it does nothing. For each forwarded pi event it runs
//     <femmed> hook pi <event>          (argv array, no shell)
// and writes one JSON line on the child's stdin:
//     {"event","seq","ts","pid","mode","session_id","session_file","idle", ...event fields}
// Calls run one at a time in event order through a promise queue kept on globalThis, so order
// and seq survive /reload and session replacement (/new, /resume, /fork re-run this factory).
// Each call is SIGKILLed after CALL_TIMEOUT_MS and the queue moves on. Handlers never throw and
// never make pi wait, except session_shutdown, which waits up to SHUTDOWN_WAIT_MS so the last
// report leaves before pi exits. Children are detached (own session): they survive the PTY
// hangup and process-group kills of pi; pi reaps them while it lives.
import { spawn } from "node:child_process";

const FEMMED_BIN = __FEMMED_BIN__;
const CALL_TIMEOUT_MS = 3000;
const SHUTDOWN_WAIT_MS = 1500;
const MAX_TEXT = 300;

const G = (globalThis.__femmePiHook ??= { seq: 0, tail: Promise.resolve(), closed: false });

function get(read) {
  try {
    const v = read();
    return v === undefined || v === "" ? null : v;
  } catch {
    return null; // ctx getters throw once pi has invalidated a replaced or disposed session
  }
}

function str(v, max) {
  return typeof v === "string" && v !== "" ? (max ? v.slice(0, max) : v) : null;
}

function call(event, line) {
  return new Promise((resolve) => {
    let timer;
    const done = () => {
      clearTimeout(timer);
      resolve();
    };
    try {
      const child = spawn(FEMMED_BIN, ["hook", "pi", event], {
        stdio: ["pipe", "ignore", "ignore"],
        detached: true,
        env: process.env,
      });
      child.once("error", done);
      child.once("exit", done);
      child.stdin.once("error", () => {});
      child.stdin.end(line);
      timer = setTimeout(() => {
        try {
          child.kill("SIGKILL");
        } catch {}
        done();
      }, CALL_TIMEOUT_MS);
      timer.unref?.();
    } catch {
      done();
    }
  });
}

function send(event, ctx, fields) {
  if (G.closed) return Promise.resolve();
  let line;
  try {
    line =
      JSON.stringify({
        event,
        seq: ++G.seq,
        ts: Date.now(),
        pid: process.pid,
        mode: get(() => ctx.mode),
        session_id: get(() => ctx.sessionManager.getSessionId()),
        session_file: get(() => ctx.sessionManager.getSessionFile()),
        idle: get(() => ctx.isIdle()),
        ...fields,
      }) + "\n";
  } catch {
    return Promise.resolve();
  }
  const next = () => call(event, line);
  G.tail = G.tail.then(next, next);
  return G.tail;
}

function lastStop(messages) {
  try {
    for (let i = messages.length - 1; i >= 0; i--) {
      const m = messages[i];
      if (m && m.role === "assistant") {
        const err = typeof m.errorMessage === "string" ? m.errorMessage.split("\n")[0] : null;
        return { stop_reason: str(m.stopReason), error: str(err, MAX_TEXT) };
      }
    }
  } catch {}
  return {};
}

export default function femmePiHook(pi) {
  if (!process.env.FEMME_SID) return;
  let stop = {};
  const on = (name, handler) => {
    try {
      pi.on(name, (event, ctx) => {
        try {
          return handler(event ?? {}, ctx);
        } catch {}
      });
    } catch {}
  };

  on("session_start", (e, ctx) => {
    void send("session_start", ctx, {
      reason: str(e.reason),
      previous_session_file: str(e.previousSessionFile),
    });
  });
  on("agent_start", (_e, ctx) => {
    stop = {};
    void send("agent_start", ctx, {});
  });
  on("turn_start", (e, ctx) => {
    void send("turn_start", ctx, { turn_index: typeof e.turnIndex === "number" ? e.turnIndex : null });
  });
  on("agent_end", (e) => {
    stop = lastStop(e.messages);
  });
  on("agent_settled", (e, ctx) => {
    void send("agent_settled", ctx, { aborted: e.aborted === true, ...stop });
  });
  on("ui_prompt_start", (e, ctx) => {
    void send("ui_prompt_start", ctx, { kind: str(e.kind), title: str(e.title, MAX_TEXT) });
  });
  on("ui_prompt_end", (e, ctx) => {
    void send("ui_prompt_end", ctx, { kind: str(e.kind), title: str(e.title, MAX_TEXT) });
  });
  on("session_shutdown", async (e, ctx) => {
    const sent = send("session_shutdown", ctx, {
      reason: str(e.reason),
      target_session_file: str(e.targetSessionFile),
    });
    if (e.reason === "quit" || e.reason === undefined) G.closed = true; // drop late events (an aborted run settling during exit)
    await Promise.race([sent, new Promise((r) => setTimeout(r, SHUTDOWN_WAIT_MS))]);
  });
}
"#;

// ----------------------------------------------------------------------------------------- parse

/// Interpret one `femmed hook <agent> <event>` call. `event` is the CLI event argument (the
/// agent's hook event name, `notify` for Codex notify, the pi event; when empty, the event named
/// in the payload); `payload` is the JSON the agent sent (stdin, or Codex notify's last argv
/// element). Tolerant: unknown events, unknown fields, empty or broken payloads and events that do
/// not change the status give `None`; never panics.
pub fn parse_hook(agent: Agent, event: &str, payload: &str) -> Option<HookEvent> {
    let p: Value = serde_json::from_str(payload.trim()).unwrap_or(Value::Null);
    let named = |key| match event {
        "" => s(&p, key).unwrap_or_default(),
        e => e,
    };
    match agent {
        Agent::Claude => parse_claude(named("hook_event_name"), &p),
        Agent::Codex => parse_codex(named("hook_event_name"), &p),
        Agent::Pi => parse_pi(named("event"), &p),
        Agent::Shell => None,
    }
}

fn parse_claude(event: &str, p: &Value) -> Option<HookEvent> {
    // Subagents (Agent tool) report their own lifecycle with an `agent_id`; only the main
    // session's start/stop counts. Their tool and permission events still mean what they say.
    let subagent = s(p, "agent_id").is_some_and(|a| !a.is_empty());
    match event {
        "SessionStart" if !subagent => Some(started(p, "transcript_path")),
        "UserPromptSubmit" => Some(HookEvent::Working),
        "PermissionRequest" => Some(blocked(p)),
        "PostToolUse" => Some(HookEvent::Working),
        "PostToolUseFailure" if p.get("is_interrupt").and_then(Value::as_bool) == Some(true) => {
            Some(HookEvent::Idle)
        }
        "PostToolUseFailure" => Some(HookEvent::Working),
        "Notification" => match s(p, "notification_type")? {
            "permission_prompt" | "elicitation_dialog" | "elicitation_url_dialog" => {
                Some(HookEvent::Blocked {
                    text: clip(s(p, "message").unwrap_or("needs your input")),
                })
            }
            "idle_prompt" => Some(HookEvent::Idle),
            _ => None,
        },
        "Stop" | "StopFailure" if !subagent => Some(HookEvent::Idle),
        "SessionEnd" if !subagent => match s(p, "reason") {
            Some("clear" | "resume") => None,
            _ => Some(HookEvent::Stopped),
        },
        _ => None,
    }
}

fn parse_codex(event: &str, p: &Value) -> Option<HookEvent> {
    let question = s(p, "tool_name").is_some_and(|t| t.starts_with("request_user_input"));
    match event {
        "SessionStart" => Some(started(p, "transcript_path")),
        "UserPromptSubmit" => Some(HookEvent::Working),
        "PreToolUse" if question => Some(blocked(p)),
        "PermissionRequest" => Some(blocked(p)),
        // The TUI's `request_user_input_async` returns `{"accepted":true}` at once and the turn
        // ends; the question stays on screen until the answer comes as the next prompt. The
        // `Stop` that follows is `Idle` below: nothing in it says a question is open ([`CODEX_HOOKS`]).
        "PostToolUse" if question => None,
        "PostToolUse" => Some(HookEvent::Working),
        "Stop" | "Interrupt" => Some(HookEvent::Idle),
        "SessionEnd" => Some(HookEvent::Stopped),
        "notify" if s(p, "type") == Some("agent-turn-complete") && !codex_title_turn(p) => {
            Some(HookEvent::Idle)
        }
        _ => None,
    }
}

/// Codex names a new thread with an internal turn of its own whose notify arrives while the first
/// real turn still runs (or waits for an approval).
// ponytail: matches codex 0.163's title prompt and its `{"title":...}` answer; re-check on upgrade.
fn codex_title_turn(p: &Value) -> bool {
    let prompt = p
        .get("input-messages")
        .and_then(|m| m.get(0))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let answer = s(p, "last-assistant-message")
        .and_then(|a| serde_json::from_str::<Value>(a).ok())
        .is_some_and(|a| a.get("title").is_some());
    prompt.starts_with("Generate a concise, single-line task title") || answer
}

fn parse_pi(event: &str, p: &Value) -> Option<HookEvent> {
    match event {
        // `idle: false` here means a `/reload` in the middle of a run: the run goes on.
        "session_start" => Some(started(p, "session_file")),
        "agent_start" | "turn_start" => Some(HookEvent::Working),
        "agent_settled" => Some(HookEvent::Idle),
        "ui_prompt_start" => Some(HookEvent::Blocked {
            text: clip(
                ["title", "kind"]
                    .iter()
                    .find_map(|k| s(p, k).filter(|t| !t.is_empty()))
                    .unwrap_or("pi needs your input"),
            ),
        }),
        "ui_prompt_end" if p.get("idle").and_then(Value::as_bool) == Some(true) => {
            Some(HookEvent::Idle)
        }
        "ui_prompt_end" => Some(HookEvent::Working),
        // Other reasons (`new`, `resume`, `fork`, `reload`) replace the session in the same
        // process; a `session_start` follows.
        "session_shutdown" => match s(p, "reason") {
            None | Some("quit") => Some(HookEvent::Stopped),
            _ => None,
        },
        _ => None,
    }
}

fn started(p: &Value, transcript_key: &str) -> HookEvent {
    HookEvent::Started {
        agent_sid: owned(p, "session_id"),
        transcript: owned(p, transcript_key),
    }
}

/// `Blocked` with one line describing the dialog or question a tool call opened.
fn blocked(p: &Value) -> HookEvent {
    let tool = s(p, "tool_name").unwrap_or("a tool");
    let input = p.get("tool_input").unwrap_or(&Value::Null);
    let text = if tool == "AskUserQuestion" || tool.starts_with("request_user_input") {
        questions_text(input).unwrap_or_else(|| "A question for you".to_owned())
    } else if tool == "ExitPlanMode" {
        let first = s(input, "plan").and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty()));
        first.map_or_else(
            || "Approve the plan".to_owned(),
            |l| format!("Approve the plan: {l}"),
        )
    } else {
        let detail = [
            "command",
            "file_path",
            "url",
            "path",
            "pattern",
            "description",
        ]
        .iter()
        .find_map(|k| s(input, k))
        .map(|d| d.lines().next().unwrap_or_default().to_owned());
        detail.map_or_else(|| format!("Allow {tool}"), |d| format!("Allow {tool}: {d}"))
    };
    HookEvent::Blocked { text: clip(&text) }
}

/// `Question? (option, option) / Next? (...)` from Claude's AskUserQuestion input
/// (`question`, options `{label}`) or Codex's request_user_input (`title`, options as strings).
fn questions_text(input: &Value) -> Option<String> {
    let qs: Vec<String> = input
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|q| {
            let text = ["question", "title", "header"]
                .iter()
                .find_map(|k| s(q, k).filter(|t| !t.is_empty()))?;
            let opts: Vec<&str> = q
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|o| o.as_str().or_else(|| s(o, "label")))
                .collect();
            Some(if opts.is_empty() {
                text.to_owned()
            } else {
                format!("{text} ({})", opts.join(", "))
            })
        })
        .collect();
    (!qs.is_empty()).then(|| qs.join(" / "))
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn owned(v: &Value, key: &str) -> Option<String> {
    s(v, key).filter(|x| !x.is_empty()).map(str::to_owned)
}

/// At most 300 bytes, cut on a char boundary, with an ellipsis when cut.
fn clip(t: &str) -> String {
    const MAX: usize = 300;
    if t.len() <= MAX {
        return t.to_owned();
    }
    let mut end = MAX;
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &t[..end])
}

/// A TOML basic string (what `codex -c key=value` parses), escaping quotes, backslashes and every
/// control character.
fn toml_str(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for c in v.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04X}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One word for a POSIX shell (`sh`, `bash`, `zsh`; also fish unless the word holds a backslash):
/// unchanged when it only has safe characters, else single-quoted.
fn sh_quote(v: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c);
    if !v.is_empty() && v.chars().all(safe) {
        return v.to_owned();
    }
    format!("'{}'", v.replace('\'', r#"'"'"'"#))
}

/// A JavaScript string literal: JSON, plus U+2028/U+2029 escaped for pre-ES2019 parsers.
fn js_str(v: &str) -> String {
    serde_json::to_string(v)
        .unwrap_or_else(|_| "\"\"".to_owned())
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_str_escapes() {
        assert_eq!(toml_str(r#"/a b/it's "x"\y"#), r#""/a b/it's \"x\"\\y""#);
        assert_eq!(toml_str("a\u{1}\u{7f}"), "\"a\\u0001\\u007F\"");
    }

    #[test]
    fn sh_quote_quotes_only_when_needed() {
        assert_eq!(sh_quote("/usr/local/bin/femmed"), "/usr/local/bin/femmed");
        assert_eq!(sh_quote("/a b/it's"), r#"'/a b/it'"'"'s'"#);
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn js_str_escapes_line_separators() {
        assert_eq!(js_str("a\u{2028}b'\""), r#""a\u2028b'\"""#);
    }

    #[test]
    fn clip_cuts_on_char_boundary() {
        let t = "é".repeat(200);
        let c = clip(&t);
        assert!(c.ends_with('…') && c.len() <= 300 + '…'.len_utf8());
    }

    /// Vectors from the capture's Python port, which matched codex 0.163's `hooks/list`.
    #[test]
    fn codex_hook_hash_matches_codex() {
        assert_eq!(
            codex_hook_hash(
                "session_start",
                "/usr/bin/femmed hook codex session-start",
                5
            ),
            "sha256:5900f1cdbdb28d004000d7ee6cebbbc6b692dba4d02247bb79e4fe0b540665f5"
        );
        assert_eq!(
            codex_hook_hash(
                "session_start",
                "/usr/local/bin/femmed hook codex SessionStart",
                5
            ),
            "sha256:f6e042afe2303aa39af1ead8b60e155bb6680d79ff75e83751e5acd197ac1a39"
        );
        assert_eq!(
            codex_hook_hash(
                "interrupt",
                r#"'/opt/my tools/it'"'"'s "femme"/femmed' hook codex Interrupt"#,
                3
            ),
            "sha256:6490b3774116d8e1e3d2e92500305e1f706c426134aaea777a9c76a35ff7ef08"
        );
    }
}
