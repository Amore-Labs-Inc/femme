//! Regression tests for the audit of the Claude driver. Each test names the finding it pins
//! (numbered as in the audit; `NOTES.md` has the story). No agent runs: lines are synthetic, shaped
//! like the captures in `fixtures/claude/`.
//!
//! Everything above the `NEW API` marker uses only calls the driver already had before the audit,
//! so on the unfixed crate those tests fail on an assertion. The module below the marker needs
//! `Driver::attach`, `held_prompts`, `pending_answers` or `PromptDropped`, which did not exist.

mod common;

use common::spool_lines;
use femme_agents::claude::ClaudeDriver;
use femme_agents::{AgentEvent, AgentStatus, Driver, Error, Launch, Out};
use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::{ItemKind, ItemStatus};
use serde_json::{json, Value};

fn launch() -> Launch {
    Launch {
        cwd: "/work/repo".into(),
        model: Some("claude-haiku-5-5".into()),
        ..Launch::default()
    }
}

// ---- synthetic stdout lines ---------------------------------------------------------------------

fn init_response() -> Value {
    json!({"type":"control_response","response":{"subtype":"success","request_id":"femme-init","response":{}}})
}
fn sys_init(sid: &str) -> Value {
    json!({"type":"system","subtype":"init","session_id":sid,"model":"claude-haiku-5-5"})
}
fn lifecycle(u: &str, state: &str) -> Value {
    json!({"type":"command_lifecycle","command_uuid":u,"state":state})
}
fn idle() -> Value {
    json!({"type":"system","subtype":"session_state_changed","state":"idle"})
}
fn result_ok(uuids: &[&str]) -> Value {
    json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","result":"ok","num_turns":1,"duration_ms":5,
           "usage":{"input_tokens":1,"output_tokens":2},"user_message_uuids":uuids})
}
fn tool_use(mid: &str, tid: &str, name: &str, input: Value) -> Value {
    json!({"type":"assistant","message":{"id":mid,"role":"assistant","content":[{"type":"tool_use","id":tid,"name":name,"input":input}]}})
}
fn tool_result(tid: &str, content: &str, is_error: bool, extra: Value) -> Value {
    let mut v = json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":tid,"content":content,"is_error":is_error}]}});
    if !extra.is_null() {
        v["tool_use_result"] = extra;
    }
    v
}
fn can_use_tool(rid: &str, tid: &str, tool: &str, input: Value) -> Value {
    json!({"type":"control_request","request_id":rid,"request":{"subtype":"can_use_tool","tool_name":tool,"tool_use_id":tid,"input":input}})
}
/// AskUserQuestion input: `(question, option labels, multi)`.
fn ask_input(qs: &[(&str, &[&str], bool)]) -> Value {
    let qs: Vec<Value> = qs
        .iter()
        .map(|(q, opts, multi)| {
            let opts: Vec<Value> = opts
                .iter()
                .map(|o| json!({"label":o,"description":""}))
                .collect();
            json!({"question":q,"header":"h","multiSelect":multi,"options":opts})
        })
        .collect();
    json!({ "questions": qs })
}
fn block_start(index: u64, block: Value) -> Value {
    json!({"type":"stream_event","event":{"type":"content_block_start","index":index,"content_block":block}})
}
fn block_stop(index: u64) -> Value {
    json!({"type":"stream_event","event":{"type":"content_block_stop","index":index}})
}
fn message_start(mid: &str) -> Value {
    json!({"type":"stream_event","event":{"type":"message_start","message":{"id":mid}}})
}

fn peer_line(name: &str, body: &str, origin: bool) -> Value {
    let text = format!("Another Claude session sent a message:\n<cross-session-message from=\"uds:/x.sock\" from-name=\"{name}\" from-mode=\"bypass\">\n{body}\n</cross-session-message>\n\nThis came from another Claude session");
    let mut v = json!({"type":"user","uuid":"p1","isReplay":true,"isSynthetic":true,"message":{"role":"user","content":text}});
    if origin {
        v["origin"] = json!({"kind":"peer","name":name,"body":body});
    } else {
        v["origin"] = json!({"kind":"peer"});
    }
    v
}

// ---- a driver and the spool it was fed ---------------------------------------------------------

/// A driver plus every line it has been fed, at its real offset, so a test can restart from
/// `resume_offset()` the way the daemon does.
struct S {
    d: ClaudeDriver,
    spool: Vec<u8>,
    out: Out,
}

impl S {
    /// Handshake done, nothing running.
    fn ready() -> Self {
        let mut s = S {
            d: ClaudeDriver::new(),
            spool: Vec::new(),
            out: Out::default(),
        };
        s.d.on_start(&launch(), &mut s.out);
        s.out.take();
        s.feed(init_response());
        s
    }

    fn feed(&mut self, v: Value) -> Vec<AgentEvent> {
        let off = self.end();
        let line = serde_json::to_vec(&v).unwrap();
        self.spool.extend_from_slice(&line);
        self.spool.push(b'\n');
        self.d.on_line(off, &line, &mut self.out)
    }

    /// Offset the next line will start at.
    fn end(&self) -> u64 {
        self.spool.len() as u64
    }

    /// Everything the driver wrote since the last call.
    fn wrote(&mut self) -> Vec<Value> {
        let b = self.out.take();
        String::from_utf8_lossy(&b)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Send `text` and return the uuid of the message the driver wrote, if it wrote one now.
    fn send(&mut self, text: &str, how: PromptHow) -> Option<String> {
        self.d.prompt(text, how, &mut self.out).unwrap();
        self.wrote()
            .iter()
            .find(|m| m["type"] == "user" && m["message"]["content"][0]["text"] == text)
            .map(|m| m["uuid"].as_str().unwrap().to_owned())
    }

    /// The CLI starts message `u` (queued, started) and the turn is open.
    fn start(&mut self, u: &str) {
        self.feed(lifecycle(u, "queued"));
        self.feed(lifecycle(u, "started"));
    }

    /// The CLI runs message `u` to its end.
    fn run_message(&mut self, u: &str) -> Vec<AgentEvent> {
        let mut ev = self.feed(lifecycle(u, "queued"));
        ev.extend(self.feed(lifecycle(u, "started")));
        ev.extend(self.feed(result_ok(&[u])));
        ev.extend(self.feed(lifecycle(u, "completed")));
        ev
    }

    /// Let the CLI run every message the driver writes, one after the other, until it writes no
    /// more; the texts in the order they were written.
    fn run_all(&mut self) -> Vec<String> {
        let mut order = Vec::new();
        loop {
            let users: Vec<Value> = self
                .wrote()
                .into_iter()
                .filter(|m| m["type"] == "user")
                .collect();
            if users.is_empty() {
                return order;
            }
            for m in users {
                order.push(
                    m["message"]["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
                self.run_message(m["uuid"].as_str().unwrap());
            }
        }
    }

    /// A fresh driver (no `attach`) that replays this spool from `from`.
    fn replay_bare(&self, from: u64) -> (ClaudeDriver, Vec<AgentEvent>, Out) {
        let mut d = ClaudeDriver::new();
        let mut out = Out::default();
        let mut ev = Vec::new();
        for (off, line) in spool_lines(&self.spool)
            .into_iter()
            .filter(|(o, _)| *o >= from)
        {
            ev.extend(d.on_line(off, line, &mut out));
        }
        (d, ev, out)
    }
}

fn completed<'a>(
    ev: &'a [AgentEvent],
    id: &str,
) -> Option<(&'a ItemKind, &'a ItemStatus, &'a Value)> {
    ev.iter().find_map(|e| match e {
        AgentEvent::ItemCompleted {
            id: i,
            kind,
            status,
            body,
            ..
        } if i == id => Some((kind, status, body)),
        _ => None,
    })
}
fn asked(ev: &[AgentEvent]) -> Vec<String> {
    ev.iter()
        .filter_map(|e| {
            if let AgentEvent::QuestionAsked { q, .. } = e {
                Some(q.request_id.clone())
            } else {
                None
            }
        })
        .collect()
}
fn resolved(ev: &[AgentEvent]) -> Vec<String> {
    ev.iter()
        .filter_map(|e| {
            if let AgentEvent::QuestionResolved { request_id } = e {
                Some(request_id.clone())
            } else {
                None
            }
        })
        .collect()
}
fn turn_events(ev: &[AgentEvent]) -> Vec<&'static str> {
    ev.iter()
        .filter_map(|e| match e {
            AgentEvent::TurnStarted => Some("TurnStarted"),
            AgentEvent::TurnEnded { .. } => Some("TurnEnded"),
            _ => None,
        })
        .collect()
}
fn statuses(ev: &[AgentEvent]) -> Vec<AgentStatus> {
    ev.iter()
        .filter_map(|e| {
            if let AgentEvent::Status(s) = e {
                Some(*s)
            } else {
                None
            }
        })
        .collect()
}

// ---- finding 9: a command nobody ended must not hold the queue forever ------------------------------

/// `session_state_changed idle` means nothing runs and nothing is queued. A command the CLI reported
/// (`queued`) and never ended (no `started`, no `completed`: it dropped it) used to keep `busy()`
/// true forever, so every later `Queue` prompt stayed in the driver.
#[test]
fn idle_frees_a_command_the_cli_never_ended() {
    let mut s = S::ready();
    let u = s
        .send("one", PromptHow::Queue)
        .expect("an idle driver writes at once");
    s.feed(lifecycle(&u, "queued"));
    assert_eq!(s.d.status(), AgentStatus::Working);
    assert_eq!(
        s.send("two", PromptHow::Queue),
        None,
        "held behind the queued message"
    );
    s.feed(idle());
    let w = s.wrote();
    assert_eq!(
        w.len(),
        1,
        "the held prompt goes out once the CLI says it is idle: {w:?}"
    );
    assert_eq!(w[0]["message"]["content"][0]["text"], "two");
}

/// The other side of the same rule: a message of ours the CLI has not mentioned yet is in flight, an
/// `idle` frame that was already on its way must not make the driver forget it.
#[test]
fn idle_leaves_a_message_the_cli_has_not_mentioned_yet() {
    let mut s = S::ready();
    s.send("one", PromptHow::Queue).unwrap();
    assert_eq!(s.send("two", PromptHow::Queue), None);
    s.feed(idle());
    assert!(
        s.wrote().is_empty(),
        "\"two\" must not overtake \"one\", which the CLI has not acknowledged"
    );
    assert_eq!(s.d.status(), AgentStatus::Working);
}

// ---- finding 10: the `queued` frame is anchored in the resume offset -----------------------------

/// While a command is queued and unstarted its `queued` frame is the only trace of it in the spool:
/// the resume offset keeps it until `started`, so a restart replays it.
#[test]
fn queued_frame_keeps_the_resume_offset_until_the_message_starts() {
    let mut s = S::ready();
    let u = s.send("one", PromptHow::Queue).unwrap();
    let queued_at = s.end();
    s.feed(lifecycle(&u, "queued"));
    assert_eq!(
        s.d.resume_offset(),
        queued_at,
        "anchored at the frame, not after it"
    );
    s.feed(json!({"type":"system","subtype":"hook_started"}));
    assert_eq!(s.d.resume_offset(), queued_at, "later lines do not move it");
    let started_at = s.end();
    s.feed(lifecycle(&u, "started"));
    assert_eq!(
        s.d.resume_offset(),
        started_at,
        "the turn that opens holds the offset from here"
    );
    s.feed(result_ok(&[&u]));
    s.feed(lifecycle(&u, "completed"));
    assert_eq!(s.d.resume_offset(), s.end(), "nothing in flight");
}

/// While a turn runs, a message the CLI queues is anchored at the *turn's* start: the turn ends
/// before the message starts, and a replay that began in the middle of a finished turn would number
/// its items wrongly.
#[test]
fn queued_frame_during_a_turn_anchors_at_the_turn_start() {
    let mut s = S::ready();
    let u1 = s.send("one", PromptHow::Queue).unwrap();
    s.feed(lifecycle(&u1, "queued"));
    let turn_at = s.end();
    s.feed(lifecycle(&u1, "started"));
    let u2 = "00000000-0000-4000-8000-0000000000b2";
    s.feed(lifecycle(u2, "queued"));
    assert_eq!(s.d.resume_offset(), turn_at);
    s.feed(result_ok(&[&u1]));
    s.feed(lifecycle(&u1, "completed"));
    assert_eq!(
        s.d.resume_offset(),
        turn_at,
        "the first turn is over, the second message still is not running"
    );
    let second_at = s.end();
    s.feed(lifecycle(u2, "started"));
    assert_eq!(s.d.resume_offset(), second_at);
}

/// After a restart the replay from the committed offset brings the queued frame back, and with it the
/// busy state: `Working`, and a `Queue` prompt waits instead of being written into the CLI's queue.
#[test]
fn restart_from_the_resume_offset_still_knows_a_message_is_queued() {
    let mut s = S::ready();
    let u = s.send("one", PromptHow::Queue).unwrap();
    s.feed(lifecycle(&u, "queued"));
    let from = s.d.resume_offset();
    let (mut d, _, mut out) = s.replay_bare(from);
    assert_eq!(d.status(), AgentStatus::Working, "a queued message is work");
    d.prompt("two", PromptHow::Queue, &mut out).unwrap();
    assert!(
        out.stdin.is_empty(),
        "held behind the queued message, as before the restart"
    );
}

// ---- finding 11: no TurnEnded without a turn, no invented TurnStarted ---------------------------

#[test]
fn a_failed_result_without_a_turn_is_an_event_not_a_turn() {
    let mut s = S::ready();
    let ev = s.feed(json!({"type":"result","subtype":"error_during_execution","is_error":true,"result":"No conversation found with session ID: x",
                           "num_turns":0,"duration_ms":3,"usage":{"input_tokens":0,"output_tokens":0}}));
    assert_eq!(turn_events(&ev), Vec::<&str>::new(), "{ev:?}");
    assert!(
        statuses(&ev).is_empty(),
        "no Working for a turn that never ran: {ev:?}"
    );
    let [AgentEvent::ItemCompleted {
        kind: ItemKind::Event,
        body,
        ..
    }] = ev.as_slice()
    else {
        panic!("one Event item expected: {ev:?}")
    };
    assert!(
        body["text"]
            .as_str()
            .unwrap()
            .contains("No conversation found with session ID: x"),
        "{body}"
    );
    assert_eq!(s.d.status(), AgentStatus::Idle);
}

// ---- finding 13: a tool_result that does not belong to an open call -----------------------------

/// A result that arrives after its turn ended (the call was already closed as failed) or for a call
/// we never saw adds only its own result item: it must not rewrite the call with blanks and must not
/// open a turn nothing would close.
#[test]
fn a_late_or_unknown_tool_result_only_adds_its_result() {
    let mut s = S::ready();
    let u = s.send("go", PromptHow::Queue).unwrap();
    s.start(&u);
    s.feed(tool_use("m1", "t1", "Bash", json!({"command":"ls"})));
    let ev = s.feed(result_ok(&[&u]));
    assert_eq!(
        completed(&ev, "t1").map(|c| *c.1),
        Some(ItemStatus::Failed),
        "the turn ended with the call still open"
    );
    assert_eq!(s.d.status(), AgentStatus::Idle);

    for (id, output) in [("t1", "late output"), ("never-seen", "stray output")] {
        let ev = s.feed(tool_result(id, output, false, Value::Null));
        assert_eq!(
            ev,
            vec![AgentEvent::ItemCompleted {
                id: format!("{id}:result"),
                kind: ItemKind::ToolResult,
                status: ItemStatus::Completed,
                body: json!({"call_id":id,"output":output,"is_error":false}),
                parent: None,
            }],
            "{id}"
        );
        assert_eq!(s.d.status(), AgentStatus::Idle, "{id}: no turn was opened");
        assert_eq!(s.d.resume_offset(), s.end(), "{id}: nothing is left open");
    }
}

// ---- finding 14: ordering, interrupt, after exit -----------------------------------------------------

/// Two `InterruptThenSend` prompts go out in the order they were given (the first used to be pushed
/// behind the second), in front of the `Queue` prompt that was already waiting.
#[test]
fn interrupt_then_send_prompts_run_in_the_order_given() {
    let mut s = S::ready();
    let u = s.send("running", PromptHow::Queue).unwrap();
    s.start(&u);
    assert_eq!(s.send("q", PromptHow::Queue), None);
    assert_eq!(s.send("i1", PromptHow::InterruptThenSend), None);
    assert_eq!(s.send("i2", PromptHow::InterruptThenSend), None);
    s.feed(result_ok(&[&u]));
    assert_eq!(s.run_all(), ["i1", "i2", "q"]);
}

/// A prompt still held when the turn is interrupted is kept and runs after it (documented on
/// `Driver::prompt` / `Driver::interrupt`), it is not thrown away with the turn.
#[test]
fn interrupt_keeps_the_held_prompts() {
    let mut s = S::ready();
    let u = s.send("running", PromptHow::Queue).unwrap();
    s.start(&u);
    assert_eq!(s.send("later", PromptHow::Queue), None);
    s.d.interrupt(&mut s.out).unwrap();
    let w = s.wrote();
    assert_eq!(w.len(), 1, "{w:?}");
    assert_eq!(w[0]["request"]["subtype"], "interrupt");
    s.feed(result_ok(&[&u]));
    assert_eq!(s.run_all(), ["later"]);
}

/// Interrupting while no turn runs is an `Ok` that writes nothing: a message the CLI has accepted
/// but not started would not be hit by an interrupt anyway.
#[test]
fn interrupt_is_a_no_op_unless_a_turn_runs() {
    let mut s = S::ready();
    s.d.interrupt(&mut s.out)
        .expect("idle interrupt is not an error");
    assert!(s.out.stdin.is_empty());

    let u = s.send("a", PromptHow::Queue).unwrap();
    s.d.interrupt(&mut s.out).expect("accepted but not started");
    assert!(s.out.stdin.is_empty());

    s.start(&u);
    s.d.interrupt(&mut s.out).unwrap();
    let w = s.wrote();
    assert_eq!(w.len(), 1, "{w:?}");
    assert_eq!(w[0]["request"]["subtype"], "interrupt");

    // before the handshake there is nothing to talk to yet
    let mut d = ClaudeDriver::new();
    let mut out = Out::default();
    d.on_start(&launch(), &mut out);
    assert!(matches!(d.interrupt(&mut out), Err(Error::State(_))));
}

/// After the process exited every call that would write says so and writes nothing.
#[test]
fn calls_after_exit_are_refused_and_write_nothing() {
    let mut s = S::ready();
    s.d.on_exit(Some(0));
    for how in [
        PromptHow::Queue,
        PromptHow::Steer,
        PromptHow::InterruptThenSend,
    ] {
        assert!(
            matches!(s.d.prompt("x", how, &mut s.out), Err(Error::State(_))),
            "{how:?}"
        );
    }
    assert!(matches!(s.d.interrupt(&mut s.out), Err(Error::State(_))));
    assert!(matches!(
        s.d.answer("r", &Answer::Cancel, &mut s.out),
        Err(Error::State(_))
    ));
    assert!(matches!(
        s.d.set_model("m", &mut s.out),
        Err(Error::State(_))
    ));
    assert!(
        s.out.stdin.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&s.out.stdin)
    );
}

// ---- finding 15: how a question ends --------------------------------------------------------------

fn plan_turn() -> S {
    let mut s = S::ready();
    let u = s.send("plan it", PromptHow::Queue).unwrap();
    s.start(&u);
    s.feed(tool_use(
        "m1",
        "tp",
        "ExitPlanMode",
        json!({"plan":"1. do it"}),
    ));
    s
}

/// Rejecting a plan is the user's answer ("Keep planning"), not a failed or cancelled question. The CLI
/// reports the rejection as an error result; the item must still say what the user chose.
#[test]
fn rejected_plan_records_the_users_choice() {
    let mut s = plan_turn();
    let ev = s.feed(can_use_tool(
        "rp",
        "tp",
        "ExitPlanMode",
        json!({"plan":"1. do it"}),
    ));
    assert_eq!(asked(&ev), ["rp"]);
    s.d.answer("rp", &Answer::Option(1), &mut s.out).unwrap();
    let resp = s.wrote().remove(0);
    assert_eq!(resp["response"]["response"]["behavior"], "deny");
    s.feed(resp); // the CLI echoes it
    let ev = s.feed(tool_result(
        "tp",
        "The user doesn't want to proceed with this tool use.",
        true,
        Value::Null,
    ));
    let (kind, status, body) = completed(&ev, "tp:ask").expect("the question item ends");
    assert_eq!((*kind, *status), (ItemKind::Question, ItemStatus::Answered));
    assert_eq!(body["answered"], json!({"Option":1}));
    assert!(body.get("reason").is_none(), "{body}");
}

/// A question nobody answered because the CLI withdrew it (interrupt) ends `Failed` with the CLI's
/// words, and never as an answered one.
#[test]
fn question_withdrawn_by_the_agent_fails_with_its_reason() {
    let mut s = S::ready();
    let u = s.send("ask", PromptHow::Queue).unwrap();
    s.start(&u);
    let ev = s.feed(can_use_tool(
        "rq",
        "tq",
        "AskUserQuestion",
        ask_input(&[("A or B?", &["A", "B"], false)]),
    ));
    assert_eq!(asked(&ev), ["rq"]);
    assert_eq!(s.d.status(), AgentStatus::Blocked);
    let ev = s.feed(json!({"type":"control_cancel_request","request_id":"rq"}));
    assert_eq!(resolved(&ev), ["rq"]);
    let why = "The user doesn't want to proceed with this tool use. The tool use was rejected.";
    let ev = s.feed(tool_result("tq", why, true, Value::Null));
    let (kind, status, body) = completed(&ev, "tq").expect("the question item ends");
    assert_eq!((*kind, *status), (ItemKind::Question, ItemStatus::Failed));
    assert_eq!(body["answered"], Value::Null);
    assert_eq!(body["reason"], why);
    assert!(
        resolved(&ev).is_empty(),
        "already resolved by the cancel: {ev:?}"
    );
}

/// A question that never gets a result at all (the turn ends, the process dies) ends `Failed` too,
/// with our own reason.
#[test]
fn a_question_nobody_answered_fails_when_the_turn_ends_or_the_process_exits() {
    for (how, reason) in [
        ("turn", "the turn ended before it was answered"),
        ("exit", "claude exited before it was answered"),
    ] {
        let mut s = S::ready();
        let u = s.send("ask", PromptHow::Queue).unwrap();
        s.start(&u);
        s.feed(can_use_tool(
            "rq",
            "tq",
            "AskUserQuestion",
            ask_input(&[("A or B?", &["A", "B"], false)]),
        ));
        let ev = if how == "turn" {
            s.feed(result_ok(&[&u]))
        } else {
            s.d.on_exit(Some(1))
        };
        assert_eq!(resolved(&ev), ["rq"], "{how}");
        let (_, status, body) = completed(&ev, "tq").unwrap_or_else(|| panic!("{how}: {ev:?}"));
        assert_eq!(*status, ItemStatus::Failed, "{how}");
        assert_eq!(body["reason"], reason, "{how}");
        assert_eq!(body["answered"], Value::Null, "{how}");
    }
}

/// An answered question carries the user's real answer, whether the driver saw `answer()` (live)
/// or only the spool (a restart replays our `control_response` as the CLI echoed it): both end up
/// with the same item.
#[test]
fn answered_question_has_the_real_answer_live_and_on_replay() {
    let mut s = S::ready();
    let u = s.send("ask", PromptHow::Queue).unwrap();
    s.start(&u);
    let from = s.d.resume_offset();
    s.feed(can_use_tool(
        "rq",
        "tq",
        "AskUserQuestion",
        ask_input(&[("A or B?", &["A", "B"], false)]),
    ));
    s.d.answer("rq", &Answer::Option(1), &mut s.out).unwrap();
    let resp = s.wrote().remove(0);
    assert_eq!(
        resp["response"]["response"]["updatedInput"]["answers"]["A or B?"],
        "B"
    );
    s.feed(resp);
    // the CLI's tool result; a replay has no `tool_use_result` to lean on in this variant
    let live = s.feed(tool_result(
        "tq",
        "User has answered your questions",
        false,
        json!({"answers":{"A or B?":"B"}}),
    ));
    let (_, status, body) = completed(&live, "tq").unwrap();
    assert_eq!(*status, ItemStatus::Answered);
    assert_eq!(body["answered"], json!({"Option":1}));

    // restart: a fresh driver reads the same lines without ever being asked to answer; the result
    // line carries no record of the answer this time, the echo of our response is all it has
    let bytes: Vec<(u64, Vec<u8>)> = spool_lines(&s.spool)
        .into_iter()
        .filter(|(o, _)| *o >= from)
        .map(|(o, l)| (o, l.to_vec()))
        .collect();
    let mut d = ClaudeDriver::new();
    let mut out = Out::default();
    let mut replayed = Vec::new();
    for (off, line) in bytes {
        let mut v: Value = serde_json::from_slice(&line).unwrap();
        if v["type"] == "user" {
            v.as_object_mut().unwrap().remove("tool_use_result");
        }
        replayed.extend(d.on_line(off, &serde_json::to_vec(&v).unwrap(), &mut out));
    }
    let (_, status, body) = completed(&replayed, "tq").unwrap();
    assert_eq!(*status, ItemStatus::Answered);
    assert_eq!(
        body["answered"],
        json!({"Option":1}),
        "the replay learns the choice from the echo"
    );
}

/// An empty multi-select pick would go to the CLI as an empty answer; it is refused, Cancel is the way
/// to decline.
#[test]
fn empty_multi_select_is_refused() {
    let mut s = S::ready();
    let u = s.send("ask", PromptHow::Queue).unwrap();
    s.start(&u);
    s.feed(can_use_tool(
        "rm",
        "tm",
        "AskUserQuestion",
        ask_input(&[("Which?", &["A", "B", "C"], true)]),
    ));
    assert!(matches!(
        s.d.answer("rm", &Answer::Options(vec![]), &mut s.out),
        Err(Error::Bad(_))
    ));
    assert!(s.out.stdin.is_empty());
    s.d.answer("rm", &Answer::Options(vec![0, 2]), &mut s.out)
        .unwrap();
    assert_eq!(
        s.wrote()[0]["response"]["response"]["updatedInput"]["answers"]["Which?"],
        "A, C"
    );
}

// ---- finding 16: repeated ids ---------------------------------------------------------------------

/// A second `content_block_start` at an index nobody stopped replaces the block; the first block's
/// open item must end, or it pins the resume offset for the rest of the session.
#[test]
fn a_second_block_at_the_same_index_does_not_pin_the_resume_offset() {
    let mut s = S::ready();
    let u = s.send("go", PromptHow::Queue).unwrap();
    s.start(&u);
    s.feed(message_start("m1"));
    s.feed(block_start(
        0,
        json!({"type":"tool_use","id":"t1","name":"Bash","input":{}}),
    ));
    let ev = s.feed(block_start(
        0,
        json!({"type":"tool_use","id":"t2","name":"Bash","input":{}}),
    ));
    assert_eq!(
        completed(&ev, "t1").map(|c| *c.1),
        Some(ItemStatus::Failed),
        "the replaced block is over: {ev:?}"
    );
    s.feed(block_stop(0));
    s.feed(result_ok(&[&u]));
    s.feed(lifecycle(&u, "completed"));
    assert_eq!(s.d.resume_offset(), s.end(), "nothing may stay open");
}

/// The CLI repeating a `can_use_tool` request we already show (or have answered) does not ask again.
#[test]
fn a_repeated_permission_request_does_not_reopen_an_answered_question() {
    let mut s = S::ready();
    let u = s.send("ask", PromptHow::Queue).unwrap();
    s.start(&u);
    let req = can_use_tool(
        "rq",
        "tq",
        "AskUserQuestion",
        ask_input(&[("A or B?", &["A", "B"], false)]),
    );
    assert_eq!(asked(&s.feed(req.clone())), ["rq"]);
    assert!(
        asked(&s.feed(req.clone())).is_empty(),
        "a repeat while it is open is not a new question"
    );
    s.d.answer("rq", &Answer::Option(0), &mut s.out).unwrap();
    s.wrote();
    let ev = s.feed(req);
    assert!(asked(&ev).is_empty(), "answered, not asked again: {ev:?}");
    assert!(matches!(
        s.d.answer("rq", &Answer::Option(1), &mut s.out),
        Err(Error::UnknownRequest(_))
    ));
    assert!(s.out.stdin.is_empty(), "no second response");
}

/// A different call under a request id still in use replaces the old one, and the old one is over.
#[test]
fn a_new_call_under_a_used_request_id_replaces_the_old_one() {
    let mut s = S::ready();
    let u = s.send("ask", PromptHow::Queue).unwrap();
    s.start(&u);
    s.feed(can_use_tool(
        "rq",
        "t1",
        "AskUserQuestion",
        ask_input(&[("First?", &["A", "B"], false)]),
    ));
    let ev = s.feed(can_use_tool(
        "rq",
        "t2",
        "AskUserQuestion",
        ask_input(&[("Second?", &["C", "D"], false)]),
    ));
    assert_eq!(
        resolved(&ev),
        ["rq"],
        "the replaced question is no longer open"
    );
    let (_, status, body) = completed(&ev, "t1").expect("the first question ends");
    assert_eq!(*status, ItemStatus::Failed);
    assert_eq!(
        body["reason"],
        "the agent asked again under the same request id"
    );
    assert_eq!(asked(&ev), ["rq"], "the new one is asked");
    s.d.answer("rq", &Answer::Option(1), &mut s.out).unwrap();
    assert_eq!(
        s.wrote()[0]["response"]["response"]["updatedInput"]["answers"]["Second?"],
        "D"
    );
}

// ---- finding 17: tui() --------------------------------------------------------------------------

fn flag(cmd: &[String], prefix: &str) -> Option<String> {
    cmd.iter()
        .find_map(|a| a.strip_prefix(prefix).map(str::to_owned))
}

/// The takeover TUI must continue the conversation the headless process runs: the session id the CLI
/// reported, else the one asked for, else the id this driver planned for a new conversation (the same
/// one `spawn` passed as `--session-id`).
#[test]
fn tui_resumes_the_session_the_headless_process_runs() {
    let d = ClaudeDriver::new();
    let planned = flag(&d.spawn(&launch()).cmd, "--session-id=")
        .expect("a new conversation gets a planned id");
    let tui = d.tui(&launch()).unwrap().cmd;
    assert_eq!(flag(&tui, "--session-id="), Some(planned), "{tui:?}");
    assert_eq!(flag(&tui, "--resume="), None, "{tui:?}");

    let asked = Launch {
        resume: Some("old-session".into()),
        ..launch()
    };
    let tui = d.tui(&asked).unwrap().cmd;
    assert_eq!(
        flag(&tui, "--resume="),
        Some("old-session".into()),
        "{tui:?}"
    );
    assert_eq!(flag(&tui, "--session-id="), None, "{tui:?}");

    // the CLI reported its own id (a resume can end up with a different one): that one wins
    let mut s = S::ready();
    s.feed(sys_init("reported-session"));
    let tui = s.d.tui(&asked).unwrap().cmd;
    assert_eq!(
        flag(&tui, "--resume="),
        Some("reported-session".into()),
        "{tui:?}"
    );
    let tui = s.d.tui(&launch()).unwrap().cmd;
    assert_eq!(
        flag(&tui, "--resume="),
        Some("reported-session".into()),
        "{tui:?}"
    );
}

// ==== NEW API: the pre-fix crate has no `attach`, `held_prompts`, `pending_answers`, `PromptDropped` ====

mod new_api {
    use super::*;
    use femme_agents::{Attach, DropReason, HeldPrompt};

    fn dropped(ev: &[AgentEvent]) -> Vec<(String, PromptHow, DropReason)> {
        ev.iter()
            .filter_map(|e| {
                if let AgentEvent::PromptDropped { text, how, reason, .. } = e {
                    Some((text.clone(), *how, reason.clone()))
                } else {
                    None
                }
            })
            .collect()
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// A fresh driver attached the way the daemon does after a restart, replaying `s`'s spool from the
    /// offset the old driver reported.
    fn restart(s: &S, answers: Vec<(String, Answer)>) -> (ClaudeDriver, Vec<AgentEvent>, Out) {
        let from = s.d.resume_offset();
        let mut d = ClaudeDriver::new();
        let mut out = Out::default();
        let a = Attach {
            resume_offset: from,
            agent_sid: s.d.agent_session_id().map(str::to_owned),
            model: Some("claude-haiku-5-5".into()),
            answers,
            epoch: 1,
        };
        d.attach(&launch(), &a, &mut out);
        let mut ev = Vec::new();
        for (off, line) in spool_lines(&s.spool)
            .into_iter()
            .filter(|(o, _)| *o >= from)
        {
            ev.extend(d.on_line(off, line, &mut out));
        }
        (d, ev, out)
    }

    // ---- finding 1 ----------------------------------------------------------------------------

    /// What the driver holds is visible, in the order it will go out, with the mode and when it was
    /// accepted. A prompt the CLI already has (written) is not listed: the CLI keeps those.
    #[test]
    fn held_prompts_are_listed_in_send_order_with_mode_and_time() {
        let mut s = S::ready();
        let u = s.send("running", PromptHow::Queue).unwrap();
        assert!(
            s.d.held_prompts().is_empty(),
            "written, so the CLI holds it"
        );
        s.start(&u);
        let before = now_ms();
        assert_eq!(s.send("q1", PromptHow::Queue), None);
        assert_eq!(s.send("i1", PromptHow::InterruptThenSend), None);
        let after = now_ms();
        let held = s.d.held_prompts();
        let got: Vec<(&str, PromptHow)> = held.iter().map(|h| (h.text.as_str(), h.how)).collect();
        assert_eq!(
            got,
            [
                ("i1", PromptHow::InterruptThenSend),
                ("q1", PromptHow::Queue)
            ]
        );
        assert!(
            held.iter()
                .all(|h| (before..=after).contains(&h.queued_at_ms)),
            "{held:?} not in {before}..={after}"
        );
        s.feed(result_ok(&[&u]));
        assert_eq!(
            s.d.held_prompts().len(),
            1,
            "i1 went out, q1 waits for its turn"
        );
    }

    /// Exit announces every prompt that did not run: first those already written (the CLI queued them
    /// and dies with them), then those the driver held; a command that started is not lost.
    #[test]
    fn exit_announces_every_prompt_that_did_not_run() {
        let mut s = S::ready();
        let ua = s.send("a", PromptHow::Queue).unwrap();
        s.feed(lifecycle(&ua, "queued")); // acknowledged, not started
        assert_eq!(s.send("b", PromptHow::Queue), None);
        assert_eq!(s.send("c", PromptHow::InterruptThenSend), None);
        let ev = s.d.on_exit(Some(137));
        assert_eq!(
            dropped(&ev),
            [
                ("a".into(), PromptHow::Queue, DropReason::Exited),
                ("c".into(), PromptHow::InterruptThenSend, DropReason::Exited),
                ("b".into(), PromptHow::Queue, DropReason::Exited)
            ]
        );
        assert!(
            matches!(ev.last(), Some(AgentEvent::Exited { code: Some(137) })),
            "Exited stays last: {ev:?}"
        );
        assert!(
            s.d.held_prompts().is_empty(),
            "announced once, not held any more"
        );

        // a started command is running, its turn ends as interrupted and it is not announced
        let mut s = S::ready();
        let u = s.send("a", PromptHow::Queue).unwrap();
        s.start(&u);
        let ev = s.d.on_exit(None);
        assert!(dropped(&ev).is_empty(), "{ev:?}");
        assert_eq!(turn_events(&ev), ["TurnEnded"]);
    }

    /// A command the CLI cancels, discards or refuses was ours and never ran: announced. One that
    /// had already started (an interrupted turn) was not lost.
    #[test]
    fn a_command_the_cli_drops_is_announced_once() {
        for state in ["cancelled", "discarded", "refused"] {
            let mut s = S::ready();
            let u = s.send("never ran", PromptHow::Queue).unwrap();
            s.feed(lifecycle(&u, "queued"));
            let ev = s.feed(lifecycle(&u, state));
            assert_eq!(
                dropped(&ev),
                [(
                    "never ran".into(),
                    PromptHow::Queue,
                    DropReason::Refused(format!("claude {state} it"))
                )],
                "{state}"
            );
            let again = s.feed(lifecycle(&u, state));
            assert!(dropped(&again).is_empty(), "{state}: once");
            assert!(
                dropped(&s.d.on_exit(None)).is_empty(),
                "{state}: and not again on exit"
            );
        }
        let mut s = S::ready();
        let u = s.send("ran", PromptHow::Queue).unwrap();
        s.start(&u);
        let ev = s.feed(lifecycle(&u, "cancelled")); // an interrupted turn, as in the ask_interrupt capture
        assert!(dropped(&ev).is_empty(), "{ev:?}");
    }

    // ---- finding 9 ----------------------------------------------------------------------------

    /// What `idle` clears is announced: the message is gone for good, the daemon may offer it again.
    #[test]
    fn idle_announces_the_message_it_clears() {
        let mut s = S::ready();
        let u = s.send("lost", PromptHow::Queue).unwrap();
        s.feed(lifecycle(&u, "queued"));
        let ev = s.feed(idle());
        assert_eq!(
            dropped(&ev),
            [(
                "lost".into(),
                PromptHow::Queue,
                DropReason::Refused("claude went idle without running it".into())
            )]
        );
        assert_eq!(s.d.status(), AgentStatus::Idle);
        // a command that is not ours (the CLI's own, e.g. from another host) is cleared silently
        s.feed(lifecycle("someone-elses", "queued"));
        assert_eq!(s.d.status(), AgentStatus::Working);
        let ev = s.feed(idle());
        assert!(dropped(&ev).is_empty());
        assert_eq!(s.d.status(), AgentStatus::Idle);
    }

    // ---- finding 4 ----------------------------------------------------------------------------

    /// Two questions in one request: the CLI gets one response once both are answered, so an answer
    /// given before a restart is only in the driver. The daemon persists `pending_answers()` and hands
    /// it back; the replayed request asks only the question that is still open, and the answered one
    /// is refused.
    #[test]
    fn an_answered_part_survives_a_restart() {
        let mut s = S::ready();
        let u = s.send("ask", PromptHow::Queue).unwrap();
        s.start(&u);
        let ev = s.feed(can_use_tool(
            "rq",
            "tq",
            "AskUserQuestion",
            ask_input(&[
                ("First?", &["A", "B"], false),
                ("Second?", &["C", "D"], false),
            ]),
        ));
        assert_eq!(asked(&ev), ["rq#0", "rq#1"]);
        s.d.answer("rq#1", &Answer::Option(1), &mut s.out).unwrap();
        assert!(
            s.out.stdin.is_empty(),
            "the CLI gets nothing until both are answered"
        );
        let kept = s.d.pending_answers();
        assert_eq!(kept, [("rq#1".to_owned(), Answer::Option(1))]);

        let (mut d, ev, mut out) = restart(&s, kept);
        assert_eq!(
            asked(&ev),
            ["rq#0"],
            "only the open question is asked again: {ev:?}"
        );
        assert_eq!(d.status(), AgentStatus::Blocked);
        assert!(
            matches!(
                d.answer("rq#1", &Answer::Option(0), &mut out),
                Err(Error::UnknownRequest(_))
            ),
            "already answered"
        );
        d.answer("rq#0", &Answer::Option(0), &mut out).unwrap();
        let resp: Value = serde_json::from_str(&out.lines()[0]).unwrap();
        let answers = &resp["response"]["response"]["updatedInput"]["answers"];
        assert_eq!(
            (answers["First?"].as_str(), answers["Second?"].as_str()),
            (Some("A"), Some("D")),
            "{resp}"
        );
    }

    /// A request the user had completely answered before the restart is not a question any more: no
    /// `QuestionAsked`, the session is not `Blocked` for it, `answer` refuses it, and its item ends
    /// `Answered` with the real answer when the result arrives.
    #[test]
    fn a_fully_answered_question_is_not_asked_again_after_a_restart() {
        let mut s = S::ready();
        let u = s.send("ask", PromptHow::Queue).unwrap();
        s.start(&u);
        s.feed(can_use_tool(
            "rq",
            "tq",
            "AskUserQuestion",
            ask_input(&[("A or B?", &["A", "B"], false)]),
        ));
        s.d.answer("rq", &Answer::Option(1), &mut s.out).unwrap();
        let kept = s.d.pending_answers();
        assert_eq!(kept, [("rq".to_owned(), Answer::Option(1))]);
        // the daemon died before the echo of our response reached the spool
        let (mut d, ev, mut out) = restart(&s, kept);
        assert!(asked(&ev).is_empty(), "{ev:?}");
        assert!(!statuses(&ev).contains(&AgentStatus::Blocked), "{ev:?}");
        assert_ne!(d.status(), AgentStatus::Blocked);
        assert!(matches!(
            d.answer("rq", &Answer::Option(0), &mut out),
            Err(Error::UnknownRequest(_))
        ));
        assert!(out.stdin.is_empty());
        let ev = d.on_line(
            s.end(),
            &serde_json::to_vec(&tool_result("tq", "ok", false, Value::Null)).unwrap(),
            &mut out,
        );
        let (_, status, body) = completed(&ev, "tq").expect("the item ends");
        assert_eq!(*status, ItemStatus::Answered);
        assert_eq!(body["answered"], json!({"Option":1}));
        assert!(d.pending_answers().is_empty(), "nothing is open any more");
    }

    // ---- findings 7 and 8 ---------------------------------------------------------------------

    /// `attach` gives a fresh driver what the previous incarnation knew: the offset (reported until a
    /// replayed line moves it), the session id and the model; the repeated `system/init` of the
    /// next turn is not a new `Started`.
    #[test]
    fn attach_restores_offset_session_and_model() {
        let mut d = ClaudeDriver::new();
        assert_eq!(d.resume_offset(), 0);
        let mut out = Out::default();
        let a = Attach {
            resume_offset: 4096,
            agent_sid: Some("sid-1".into()),
            model: Some("claude-haiku-5-5".into()),
            answers: vec![],
            epoch: 1,
        };
        d.attach(&launch(), &a, &mut out);
        assert_eq!(
            d.resume_offset(),
            4096,
            "a restart before the first replayed line commits what it was given, not 0"
        );
        assert_eq!(d.agent_session_id(), Some("sid-1"));
        assert!(
            out.stdin.is_empty(),
            "Claude needs nothing from the process to catch up"
        );
        let tui = d.tui(&launch()).unwrap().cmd;
        assert_eq!(flag(&tui, "--resume="), Some("sid-1".into()), "{tui:?}");
        let init = serde_json::to_vec(&sys_init("sid-1")).unwrap();
        assert!(
            d.on_line(4096, &init, &mut out).is_empty(),
            "the same session and model again say nothing new"
        );
        assert_eq!(d.resume_offset(), 4096 + init.len() as u64 + 1);
        // another model is news
        let mut other = sys_init("sid-1");
        other["model"] = json!("claude-sonnet-5-5");
        let ev = d.on_line(9000, &serde_json::to_vec(&other).unwrap(), &mut out);
        assert_eq!(
            ev,
            vec![AgentEvent::ModelChanged("claude-sonnet-5-5".into())]
        );
    }

    /// `held_prompts` hands back plain data the daemon can store (and give to `prompt` again).
    #[test]
    fn held_prompts_round_trip_through_json() {
        let h = HeldPrompt {
            text: "again".into(),
            how: PromptHow::Queue,
            queued_at_ms: 12,
            id: None,
        };
        let back: HeldPrompt = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(back, h);
        // what a daemon saved before prompts had ids still reads
        let old: HeldPrompt =
            serde_json::from_str(r#"{"text":"again","how":"Queue","queued_at_ms":12}"#).unwrap();
        assert_eq!(old, h);
    }
}

// ---- cross-session messages (2.1.224+) ---------------------------------------------------------

/// A user line whose `origin.kind` is `peer` is a `PeerMessage` from the named session with the
/// `origin.body` as its text, and never a `UserMessage`; it opens a turn like a prompt does.
#[test]
fn a_peer_message_is_its_own_item_not_the_users() {
    let mut s = S::ready();
    let ev = s.feed(peer_line("platform-12", "hello", true));
    assert!(ev.iter().any(|e| matches!(e, AgentEvent::TurnStarted)));
    assert!(ev.iter().any(|e| matches!(e,
        AgentEvent::ItemCompleted { kind: ItemKind::PeerMessage, body, .. }
            if body["from"] == "platform-12" && body["text"] == "hello")));
    assert!(!ev.iter().any(|e| matches!(
        e,
        AgentEvent::ItemCompleted {
            kind: ItemKind::UserMessage,
            ..
        }
    )));
}

/// Without `origin.name`/`body` (a transcript's `queued_command`), the sender and text come from
/// the `<cross-session-message>` framing.
#[test]
fn a_peer_message_falls_back_to_its_tags() {
    let mut s = S::ready();
    let ev = s.feed(peer_line("femme-65", "two\nlines", false));
    assert!(ev.iter().any(|e| matches!(e,
        AgentEvent::ItemCompleted { kind: ItemKind::PeerMessage, body, .. }
            if body["from"] == "femme-65" && body["text"] == "two\nlines")));
}

/// The host's own prompt echo (`isReplay`, no `origin`) is still the `UserMessage`.
#[test]
fn a_prompt_echo_without_origin_stays_a_user_message() {
    let mut s = S::ready();
    let u = s.send("hi", PromptHow::Queue).unwrap();
    let ev = s.feed(
        json!({"type":"user","uuid":u,"isReplay":true,"message":{"role":"user","content":"hi"}}),
    );
    assert!(ev.iter().any(|e| matches!(e,
        AgentEvent::ItemCompleted { kind: ItemKind::UserMessage, body, .. } if body["text"] == "hi")));
}

// ---- mid-turn steer: written at once, ended by the turn's one result ----------------------------

/// A Steer mid-turn is written at once (the CLI folds it into the running turn at its next tool
/// boundary); the turn's one `result` lists both `user_message_uuids` and ends both commands, so
/// nothing is left in flight and nothing is reported dropped. A Queue sent at the same moment
/// stays held until then.
#[test]
fn a_mid_turn_steer_is_written_at_once_and_one_result_ends_both_messages() {
    let mut s = S::ready();
    let first = s.send("first", PromptHow::Queue).unwrap();
    s.start(&first);
    assert_eq!(s.d.status(), AgentStatus::Working);
    let steer = s
        .send("also say banana", PromptHow::Steer)
        .expect("a Steer goes out while the turn runs");
    assert_eq!(s.send("after", PromptHow::Queue), None, "a Queue waits");
    // the CLI takes the steer into the running turn: queued, echoed, started
    s.feed(lifecycle(&steer, "queued"));
    let ev = s.feed(json!({"type":"user","isReplay":true,"uuid":steer,
        "message":{"role":"user","content":[{"type":"text","text":"also say banana"}]}}));
    assert!(
        ev.iter().any(|e| matches!(e, AgentEvent::ItemCompleted { kind: ItemKind::UserMessage, body, .. } if body["text"] == "also say banana")),
        "the echo is the user's bubble, inside the turn: {ev:?}"
    );
    s.feed(lifecycle(&steer, "started"));
    assert!(s.wrote().is_empty(), "the queued one is still held");
    let mut ev = s.feed(result_ok(&[&first, &steer]));
    ev.extend(s.feed(lifecycle(&steer, "completed")));
    ev.extend(s.feed(lifecycle(&first, "completed")));
    assert_eq!(turn_events(&ev), ["TurnEnded"], "one turn: {ev:?}");
    assert!(
        !ev.iter()
            .any(|e| matches!(e, AgentEvent::PromptDropped { .. })),
        "both messages ran: {ev:?}"
    );
    // nothing of ours in flight any more: the held Queue goes out now
    let w = s.wrote();
    assert_eq!(w.len(), 1, "{w:?}");
    assert_eq!(w[0]["message"]["content"][0]["text"], "after");
    assert_eq!(s.d.held_prompts().len(), 0);
}

// ---- the daemon's own ids: the CLI echoes a message under the uuid it was written with -----------

/// `prompt_with_id` writes the message under the daemon's id, also when it was held back first, and
/// a message the CLI drops is announced with that id, so the daemon finds the message it saved.
#[test]
fn a_prompt_with_an_id_is_written_held_and_dropped_under_it() {
    let mut s = S::ready();
    let a = "0b6e7c1e-4f2a-4d1b-9c3e-5a7d2f8e1b40";
    let b = "3f9a2c71-8d4e-4b6a-a1c2-7e5f9d3b2a18";
    s.d.prompt_with_id(a, "first", PromptHow::Queue, &mut s.out)
        .unwrap();
    let w = s.wrote();
    assert_eq!(w[0]["uuid"], a, "written at once under its id: {w:?}");
    s.start(a);
    s.d.prompt_with_id(b, "second", PromptHow::Queue, &mut s.out)
        .unwrap();
    assert!(s.wrote().is_empty(), "held until the turn ends");
    assert_eq!(s.d.held_prompts()[0].id.as_deref(), Some(b));
    let ev = s.d.on_exit(Some(1));
    assert!(
        ev.iter().any(|e| matches!(e, AgentEvent::PromptDropped { text, id: Some(id), .. }
            if text == "second" && id == b)),
        "{ev:?}"
    );
}
