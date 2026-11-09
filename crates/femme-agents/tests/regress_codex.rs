//! Regression tests for the audit of the Codex driver (numbering as in the audit; `NOTES.md` has
//! the story). Synthetic JSON-RPC lines shaped like `fixtures/codex/` and `schema.json`.
//!
//! Everything above the `NEW API` marker uses only calls the driver already had before the audit,
//! so on the unfixed crate those tests fail on an assertion. The module below the marker needs
//! `Driver::attach`, `held_prompts`, `pending_answers` or `PromptDropped`, which did not exist.

mod common;

use femme_agents::codex::CodexDriver;
use femme_agents::{AgentEvent, AgentStatus, Driver, Error, Launch, Out};
use femme_proto::rpc::{Answer, PromptHow};
use serde_json::{json, Value};

fn launch() -> Launch {
    Launch {
        cwd: "/w".into(),
        ..Launch::default()
    }
}

// ---- synthetic lines --------------------------------------------------------------------------

fn init_result(id: u64) -> Value {
    json!({"id": id, "result": {"userAgent": "codex/0.163.0 (test)"}})
}
fn thread_result(id: u64, thread: &str) -> Value {
    json!({"id": id, "result": {"thread": {"id": thread, "turns": []}, "model": "m"}})
}
fn turn_reply(id: u64, turn: &str) -> Value {
    json!({"id": id, "result": {"turn": {"id": turn}}})
}
fn turn_started(turn: &str) -> Value {
    json!({"method": "turn/started", "params": {"threadId": "t1", "turn": {"id": turn, "status": "inProgress"}}})
}
fn turn_completed(turn: &str, status: &str) -> Value {
    json!({"method": "turn/completed", "params": {"threadId": "t1", "turn": {"id": turn, "status": status, "error": null}}})
}
/// `item/tool/requestUserInput` with `(question id, question, labels)` per question.
fn user_input(rpc_id: u64, item: &str, qs: &[(&str, &str, &[&str])]) -> Value {
    let qs: Vec<Value> = qs
        .iter()
        .map(|(id, q, labels)| {
            let opts: Vec<Value> = labels
                .iter()
                .map(|l| json!({"label": l, "description": ""}))
                .collect();
            json!({"id": id, "header": "h", "question": q, "isOther": false, "isSecret": false, "options": opts})
        })
        .collect();
    json!({"method": "item/tool/requestUserInput", "id": rpc_id, "params": {"threadId": "t1", "turnId": "u1",
        "itemId": item, "isBlocking": true, "questions": qs}})
}

/// A driver and every line it has been fed (at its real offset).
struct C {
    d: CodexDriver,
    spool: Vec<u8>,
    out: Out,
}

impl C {
    /// Past the handshake on thread `t1`; the ids the driver used are 1 (initialize) and 2 (thread).
    fn ready() -> Self {
        let mut c = C {
            d: CodexDriver::new(),
            spool: Vec::new(),
            out: Out::default(),
        };
        c.d.on_start(&launch(), &mut c.out);
        c.feed(init_result(1));
        c.feed(thread_result(2, "t1"));
        c.wrote();
        c
    }

    fn feed(&mut self, v: Value) -> Vec<AgentEvent> {
        let off = self.end();
        let line = serde_json::to_vec(&v).unwrap();
        self.spool.extend_from_slice(&line);
        self.spool.push(b'\n');
        self.d.on_line(off, &line, &mut self.out)
    }

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

    /// Send a prompt and return the `turn/start` it wrote now, if any.
    fn send(&mut self, text: &str, how: PromptHow) -> Option<Value> {
        self.d.prompt(text, how, &mut self.out).unwrap();
        self.wrote()
            .into_iter()
            .find(|m| m["method"] == "turn/start" && m["params"]["input"][0]["text"] == text)
    }

    /// A turn `turn` of the prompt written as `start` is running.
    fn running(&mut self, start: &Value, turn: &str) {
        self.feed(turn_reply(start["id"].as_u64().unwrap(), turn));
        self.feed(turn_started(turn));
        self.wrote();
    }

    /// Let Codex run every `turn/start` the driver writes, one after the other, until it writes no
    /// more; the texts in the order they were written.
    fn run_all(&mut self) -> Vec<String> {
        let (mut order, mut n) = (Vec::new(), 0);
        loop {
            let starts: Vec<Value> = self
                .wrote()
                .into_iter()
                .filter(|m| m["method"] == "turn/start")
                .collect();
            if starts.is_empty() {
                return order;
            }
            for m in starts {
                order.push(m["params"]["input"][0]["text"].as_str().unwrap().to_owned());
                n += 1;
                let turn = format!("run{n}");
                self.feed(turn_reply(m["id"].as_u64().unwrap(), &turn));
                self.feed(turn_started(&turn));
                self.feed(turn_completed(&turn, "completed"));
            }
        }
    }
}

fn asked(ev: &[AgentEvent]) -> Vec<String> {
    ev.iter()
        .filter_map(|e| match e {
            AgentEvent::QuestionAsked { q, .. } => Some(q.request_id.clone()),
            _ => None,
        })
        .collect()
}

// ---- finding 6: a server request nobody serves must be answered -------------------------------

/// Codex waits for the reply to every server request. One the driver does not serve gets a JSON-RPC
/// error (the agent carries on instead of hanging) and is still reported as `Raw`.
#[test]
fn an_unknown_server_request_is_refused_and_still_raw() {
    let mut c = C::ready();
    let req = json!({"id": 7, "method": "item/permissions/requestApproval", "params": {"x": 1}});
    let ev = c.feed(req.clone());
    assert_eq!(
        c.wrote(),
        [
            json!({"id": 7, "error": {"code": -32601, "message": "femme does not handle item/permissions/requestApproval"}})
        ]
    );
    assert_eq!(ev, [AgentEvent::Raw(req)]);
    assert_eq!(c.d.status(), AgentStatus::Idle, "it is not a question");
}

/// A request for user input with nothing to ask cannot be answered by anybody either.
#[test]
fn a_question_request_without_questions_is_refused() {
    let mut c = C::ready();
    let req = user_input(8, "call_x", &[]);
    let ev = c.feed(req.clone());
    let w = c.wrote();
    assert_eq!(w.len(), 1, "{w:?}");
    assert_eq!(
        (w[0]["id"].clone(), w[0]["error"]["code"].clone()),
        (json!(8), json!(-32602))
    );
    assert_eq!(ev, [AgentEvent::Raw(req)]);
}

/// ...and a request the driver does serve is not refused.
#[test]
fn a_served_request_is_not_refused() {
    let mut c = C::ready();
    c.feed(turn_started("u1"));
    let ev = c.feed(user_input(9, "call_y", &[("q", "Pick?", &["x", "y"])]));
    assert_eq!(asked(&ev), ["call_y"]);
    assert!(c.wrote().is_empty());
}

// ---- finding 11: no TurnEnded without a turn --------------------------------------------------

/// A `turn/completed` with no `turn/started` before it (a replay that began after it, a duplicate)
/// closes what is open but ends no turn: `TurnEnded` always pairs with a `TurnStarted`.
#[test]
fn turn_completed_without_an_open_turn_ends_nothing() {
    let mut c = C::ready();
    let ev = c.feed(turn_completed("u9", "completed"));
    assert!(
        !ev.iter()
            .any(|e| matches!(e, AgentEvent::TurnEnded { .. } | AgentEvent::TurnStarted)),
        "{ev:?}"
    );
    assert_eq!(c.d.status(), AgentStatus::Idle);

    // a duplicate after a real turn
    c.feed(turn_started("u1"));
    let ev = c.feed(turn_completed("u1", "completed"));
    assert_eq!(
        ev.iter()
            .filter(|e| matches!(e, AgentEvent::TurnEnded { .. }))
            .count(),
        1
    );
    let ev = c.feed(turn_completed("u1", "completed"));
    assert!(
        !ev.iter().any(|e| matches!(e, AgentEvent::TurnEnded { .. })),
        "{ev:?}"
    );
}

// ---- finding 14: ordering, interrupt, after exit -----------------------------------------------

/// Two `InterruptThenSend` prompts run in the order they were given, ahead of the `Queue` prompt
/// that was already waiting.
#[test]
fn interrupt_then_send_prompts_run_in_the_order_given() {
    let mut c = C::ready();
    let start = c.send("running", PromptHow::Queue).unwrap();
    c.running(&start, "u1");
    assert!(c.send("q", PromptHow::Queue).is_none());
    assert!(c.send("i1", PromptHow::InterruptThenSend).is_none());
    assert!(c.send("i2", PromptHow::InterruptThenSend).is_none());
    c.feed(turn_completed("u1", "interrupted"));
    assert_eq!(c.run_all(), ["i1", "i2", "q"]);
}

/// A prompt still held when the turn is interrupted is kept and runs after it.
#[test]
fn interrupt_keeps_the_held_prompts() {
    let mut c = C::ready();
    let start = c.send("running", PromptHow::Queue).unwrap();
    c.running(&start, "u1");
    assert!(c.send("later", PromptHow::Queue).is_none());
    c.d.interrupt(&mut c.out).unwrap();
    let w = c.wrote();
    assert_eq!(w.len(), 1, "{w:?}");
    assert_eq!(w[0]["method"], "turn/interrupt");
    c.feed(turn_completed("u1", "interrupted"));
    assert_eq!(c.run_all(), ["later"]);
}

/// Steers Codex turns down (`turn/steer` is refused once the turn is over) run as turns of their
/// own, in the order they were given and ahead of the `Queue` prompt that was waiting: a
/// `push_front` per refusal would have reversed them.
#[test]
fn steers_codex_turns_down_run_in_the_order_given() {
    let mut c = C::ready();
    let start = c.send("running", PromptHow::Queue).unwrap();
    c.running(&start, "u1");
    assert!(c.send("q", PromptHow::Queue).is_none());
    c.d.prompt("s1", PromptHow::Steer, &mut c.out).unwrap();
    c.d.prompt("s2", PromptHow::Steer, &mut c.out).unwrap();
    let steers: Vec<Value> = c
        .wrote()
        .into_iter()
        .filter(|m| m["method"] == "turn/steer")
        .collect();
    assert_eq!(steers.len(), 2);
    for s in &steers {
        c.feed(json!({"id": s["id"], "error": {"code": -32600, "message": "turn is not active"}}));
    }
    c.feed(turn_completed("u1", "completed"));
    assert_eq!(c.run_all(), ["s1", "s2", "q"]);
}

/// Interrupting while no turn runs is an `Ok` that writes nothing.
#[test]
fn interrupt_is_a_no_op_unless_a_turn_runs() {
    let mut c = C::ready();
    c.d.interrupt(&mut c.out)
        .expect("idle interrupt is not an error");
    assert!(c.out.stdin.is_empty());
    let start = c.send("a", PromptHow::Queue).unwrap();
    c.running(&start, "u1");
    c.d.interrupt(&mut c.out).unwrap();
    assert_eq!(c.wrote().len(), 1);
    c.feed(turn_completed("u1", "interrupted"));
    c.d.interrupt(&mut c.out).unwrap();
    assert!(c.out.stdin.is_empty(), "idle again");
}

/// After the process exited every call that would write says so and writes nothing.
#[test]
fn calls_after_exit_are_refused_and_write_nothing() {
    let mut c = C::ready();
    c.d.on_exit(Some(0));
    for how in [
        PromptHow::Queue,
        PromptHow::Steer,
        PromptHow::InterruptThenSend,
    ] {
        assert!(
            matches!(c.d.prompt("x", how, &mut c.out), Err(Error::State(_))),
            "{how:?}"
        );
    }
    assert!(matches!(c.d.interrupt(&mut c.out), Err(Error::State(_))));
    assert!(matches!(
        c.d.answer("r", &Answer::Cancel, &mut c.out),
        Err(Error::State(_))
    ));
    assert!(matches!(
        c.d.set_model("m", &mut c.out),
        Err(Error::State(_))
    ));
    c.d.read_rate_limits(&mut c.out);
    assert!(
        c.out.stdin.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&c.out.stdin)
    );
}

// ---- finding 18: responses are matched by id -------------------------------------------------------

/// A response to a request this driver did not send (the replay of an old incarnation's) is read by
/// nobody: a stale `turn/start` result must not make the driver think a turn runs, a stale
/// rate-limit read must not show old numbers as new, a stale thread result must not change the
/// thread.
#[test]
fn a_reply_to_a_request_this_driver_did_not_send_is_ignored() {
    let mut c = C::ready();
    assert!(c.feed(turn_reply(77, "old-turn")).is_empty());
    let limits = json!({"id": 78, "result": {"rateLimits": {"primary": {"usedPercent": 12.0, "windowDurationMins": 300, "resetsAt": 5}}}});
    assert!(c.feed(limits).is_empty());
    assert!(c.feed(thread_result(79, "other-thread")).is_empty());
    assert_eq!(c.d.agent_session_id(), Some("t1"));
    // not busy: the prompt goes out at once instead of waiting for a turn that is not there
    let start = c.send("x", PromptHow::Queue).expect("written at once");
    assert_eq!(start["params"]["threadId"], "t1");
    // and the real reply is still taken
    assert!(c
        .feed(turn_reply(start["id"].as_u64().unwrap(), "real-turn"))
        .is_empty());
    c.d.interrupt(&mut c.out).unwrap();
    assert_eq!(c.wrote()[0]["params"]["turnId"], "real-turn");
}

// ==== NEW API: the pre-fix crate has no `attach`, `held_prompts`, `pending_answers`, `PromptDropped` ====

mod new_api {
    use super::*;
    use femme_agents::{Attach, DropReason};

    fn dropped(ev: &[AgentEvent]) -> Vec<(String, PromptHow, DropReason)> {
        ev.iter()
            .filter_map(|e| match e {
                AgentEvent::PromptDropped {
                    text, how, reason, ..
                } => Some((text.clone(), *how, reason.clone())),
                _ => None,
            })
            .collect()
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// A driver re-attached the way the daemon does it (no `on_start`).
    fn attached(
        resume_offset: u64,
        sid: Option<&str>,
        answers: Vec<(String, Answer)>,
        epoch: u32,
    ) -> (CodexDriver, Out) {
        let mut d = CodexDriver::new();
        let mut out = Out::default();
        let a = Attach {
            resume_offset,
            agent_sid: sid.map(str::to_owned),
            model: None,
            answers,
            epoch,
        };
        d.attach(&launch(), &a, &mut out);
        (d, out)
    }

    fn line(d: &mut CodexDriver, off: &mut u64, out: &mut Out, v: Value) -> Vec<AgentEvent> {
        let b = serde_json::to_vec(&v).unwrap();
        let ev = d.on_line(*off, &b, out);
        *off += b.len() as u64 + 1;
        ev
    }

    fn written(out: &mut Out) -> Vec<Value> {
        let b = out.take();
        String::from_utf8_lossy(&b)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    // ---- finding 1 ----------------------------------------------------------------------------

    /// What waits for the handshake is visible, in the order it will go out, with the mode and the
    /// time it was accepted.
    #[test]
    fn held_prompts_wait_for_the_handshake_and_are_listed() {
        let mut d = CodexDriver::new();
        let mut out = Out::default();
        d.on_start(&launch(), &mut out);
        out.take();
        let before = now_ms();
        d.prompt("a", PromptHow::Queue, &mut out).unwrap();
        d.prompt("b", PromptHow::InterruptThenSend, &mut out)
            .unwrap();
        let after = now_ms();
        assert!(out.stdin.is_empty());
        let held = d.held_prompts();
        let got: Vec<(&str, PromptHow)> = held.iter().map(|h| (h.text.as_str(), h.how)).collect();
        assert_eq!(
            got,
            [("b", PromptHow::InterruptThenSend), ("a", PromptHow::Queue)]
        );
        assert!(held
            .iter()
            .all(|h| (before..=after).contains(&h.queued_at_ms)));
    }

    /// Exit announces what did not run: the writes whose reply never came, oldest first, then the
    /// held ones in their order; `Exited` stays last.
    #[test]
    fn exit_announces_every_prompt_that_did_not_run() {
        let mut c = C::ready();
        assert!(c.send("a", PromptHow::Queue).is_some()); // turn/start in flight
        assert!(c.send("b", PromptHow::Queue).is_none());
        assert!(c.send("c", PromptHow::InterruptThenSend).is_none());
        assert_eq!(
            c.d.held_prompts()
                .iter()
                .map(|h| h.text.as_str())
                .collect::<Vec<_>>(),
            ["c", "b"]
        );
        let ev = c.d.on_exit(Some(1));
        assert_eq!(
            dropped(&ev),
            [
                ("a".into(), PromptHow::Queue, DropReason::Exited),
                ("c".into(), PromptHow::InterruptThenSend, DropReason::Exited),
                ("b".into(), PromptHow::Queue, DropReason::Exited),
            ]
        );
        assert!(
            matches!(ev.last(), Some(AgentEvent::Exited { code: Some(1) })),
            "{ev:?}"
        );
        assert!(c.d.held_prompts().is_empty());

        // a steer whose reply never came is lost with the process too; one that was answered is not
        let mut c = C::ready();
        let start = c.send("a", PromptHow::Queue).unwrap();
        c.running(&start, "u1");
        c.d.prompt("s", PromptHow::Steer, &mut c.out).unwrap();
        let steer = c.wrote().remove(0);
        assert_eq!(steer["method"], "turn/steer");
        let ev = c.d.on_exit(None);
        assert_eq!(
            dropped(&ev),
            [("s".into(), PromptHow::Steer, DropReason::Exited)]
        );
        let mut c = C::ready();
        let start = c.send("a", PromptHow::Queue).unwrap();
        c.running(&start, "u1");
        c.d.prompt("s", PromptHow::Steer, &mut c.out).unwrap();
        let steer = c.wrote().remove(0);
        c.feed(json!({"id": steer["id"], "result": {"turnId": "u1"}}));
        assert!(dropped(&c.d.on_exit(None)).is_empty());
    }

    /// A `turn/start` Codex refuses is announced with Codex's own words, and the next held prompt
    /// goes out.
    #[test]
    fn a_refused_turn_start_is_announced_with_its_error() {
        let mut c = C::ready();
        let a = c.send("a", PromptHow::Queue).unwrap();
        assert!(c.send("b", PromptHow::Queue).is_none());
        let ev =
            c.feed(json!({"id": a["id"], "error": {"code": -32600, "message": "model not found"}}));
        assert_eq!(
            dropped(&ev),
            [(
                "a".into(),
                PromptHow::Queue,
                DropReason::Refused("model not found".into())
            )]
        );
        let w = c.wrote();
        assert_eq!(w.len(), 1, "{w:?}");
        assert_eq!(w[0]["params"]["input"][0]["text"], "b");
        assert!(
            dropped(&c.d.on_exit(None)).iter().all(|d| d.0 != "a"),
            "announced once"
        );
    }

    // ---- finding 2 ----------------------------------------------------------------------------

    /// The previous daemon died between `initialize` and the thread: the spool holds only the
    /// `initialize` result and the agent waits for `initialized`. An attached driver finishes the
    /// handshake itself; a bare replay never writes.
    #[test]
    fn a_handshake_the_daemon_died_in_is_finished_after_attach() {
        let epoch1 = 1u64 << 32;
        let (mut d, mut out) = attached(0, None, vec![], 1);
        let mut off = 0;
        let ev = line(&mut d, &mut off, &mut out, init_result(1));
        assert!(ev.is_empty());
        let w = written(&mut out);
        assert_eq!(w[0], json!({"method": "initialized"}));
        assert_eq!(
            (w[1]["method"].as_str(), w[1]["id"].as_u64()),
            (Some("thread/start"), Some(epoch1 + 1))
        );
        assert_eq!(w[1]["params"]["cwd"], "/w");
        let ev = line(&mut d, &mut off, &mut out, thread_result(epoch1 + 1, "t9"));
        assert_eq!(
            ev,
            [AgentEvent::Started {
                agent_sid: "t9".into(),
                model: Some("m".into())
            }]
        );
        assert_eq!(d.agent_session_id(), Some("t9"));
        // the prompt that was waiting for the thread goes out now
        d.prompt("hi", PromptHow::Queue, &mut out).unwrap();
        assert_eq!(written(&mut out)[0]["params"]["threadId"], "t9");

        // a session that asked to resume a thread continues with `thread/resume`
        let mut d = CodexDriver::new();
        let l = Launch {
            resume: Some("t5".into()),
            ..launch()
        };
        d.attach(
            &l,
            &Attach {
                epoch: 1,
                ..Attach::default()
            },
            &mut out,
        );
        let mut off = 0;
        line(&mut d, &mut off, &mut out, init_result(1));
        let w = written(&mut out);
        assert_eq!(w[1]["method"], "thread/resume");
        assert_eq!(w[1]["params"]["threadId"], "t5");

        // a replay that is not a re-attach (no `attach`) only reads
        let mut d = CodexDriver::new();
        let mut off = 0;
        assert!(line(&mut d, &mut off, &mut out, init_result(1)).is_empty());
        assert!(
            out.stdin.is_empty(),
            "{:?}",
            String::from_utf8_lossy(&out.stdin)
        );

        // an attached driver that already knows its thread had no handshake to finish
        let (mut d, mut out) = attached(0, Some("t1"), vec![], 1);
        let mut off = 0;
        line(&mut d, &mut off, &mut out, init_result(1));
        assert!(out.stdin.is_empty());
    }

    // ---- finding 4 ----------------------------------------------------------------------------

    /// Codex writes each answer at once but forgets it on restart. The daemon hands the answers back;
    /// the replayed request asks only what is still open, and the answered question is refused.
    #[test]
    fn an_answered_question_is_not_asked_again_after_a_restart() {
        let req = user_input(
            7,
            "call_1",
            &[("a", "First?", &["x", "y"]), ("b", "Second?", &["p", "q"])],
        );
        let (mut d, mut out) = attached(
            0,
            Some("t1"),
            vec![("call_1#1".into(), Answer::Option(1))],
            1,
        );
        let mut off = 0;
        let ev = line(&mut d, &mut off, &mut out, req.clone());
        assert_eq!(asked(&ev), ["call_1#0"], "{ev:?}");
        assert_eq!(ev.last(), Some(&AgentEvent::Status(AgentStatus::Blocked)));
        assert_eq!(
            d.pending_answers(),
            [("call_1#1".to_owned(), Answer::Option(1))]
        );
        assert!(matches!(
            d.answer("call_1#1", &Answer::Option(0), &mut out),
            Err(Error::UnknownRequest(_))
        ));
        d.answer("call_1#0", &Answer::Option(0), &mut out).unwrap();
        assert_eq!(
            written(&mut out),
            [
                json!({"id": 7, "result": {"answers": {"a": {"answers": ["x"]}, "b": {"answers": ["q"]}}}})
            ]
        );

        // completely answered before the restart: nothing to ask, nothing to write, never Blocked
        let both = vec![
            ("call_1#0".into(), Answer::Option(1)),
            ("call_1#1".into(), Answer::Option(0)),
        ];
        let (mut d, mut out) = attached(0, Some("t1"), both, 1);
        let mut off = 0;
        let ev = line(&mut d, &mut off, &mut out, req);
        assert!(asked(&ev).is_empty(), "{ev:?}");
        assert!(!ev.contains(&AgentEvent::Status(AgentStatus::Blocked)));
        assert_ne!(d.status(), AgentStatus::Blocked);
        assert!(
            out.stdin.is_empty(),
            "the response went out before the restart"
        );
        assert!(matches!(
            d.answer("call_1#0", &Answer::Option(0), &mut out),
            Err(Error::UnknownRequest(_))
        ));
        let ev = line(
            &mut d,
            &mut off,
            &mut out,
            json!({"method": "serverRequest/resolved", "params": {"threadId": "t1", "requestId": 7}}),
        );
        assert!(
            !ev.iter()
                .any(|e| matches!(e, AgentEvent::QuestionResolved { .. })),
            "the daemon has these answered already: {ev:?}"
        );
        assert!(d.pending_answers().is_empty());
    }

    // ---- findings 5 and 18 --------------------------------------------------------------------

    /// The ids of two incarnations never meet: the first process counts from 1, an attached driver
    /// from its epoch, so the old driver's replayed reply cannot be taken for the answer to the
    /// new driver's request.
    #[test]
    fn ids_of_two_incarnations_never_meet() {
        let id_of = |epoch: u32| {
            let (mut d, mut out) = attached(0, Some("t1"), vec![], epoch);
            d.prompt("x", PromptHow::Queue, &mut out).unwrap();
            let w = written(&mut out);
            assert_eq!(
                w[0]["method"], "turn/start",
                "the thread is known, no lookup: {w:?}"
            );
            w[0]["id"].as_u64().unwrap()
        };
        let mut c = C::ready();
        let first = c.send("x", PromptHow::Queue).unwrap()["id"]
            .as_u64()
            .unwrap();
        assert_eq!(first, 3, "the first process keeps counting from 1");
        let (e1, e2, e0) = (id_of(1), id_of(2), id_of(0));
        assert_eq!(e1, (1 << 32) + 1);
        assert_eq!(e2, (2 << 32) + 1);
        assert_eq!(e0, e1, "an attach is at least the first one");
        assert!(e1 != first && e2 != e1);

        // the previous incarnation's reply (id 3) shows up in the replay of the new one
        let (mut d, mut out) = attached(0, Some("t1"), vec![], 1);
        d.prompt("x", PromptHow::Queue, &mut out).unwrap();
        let mine = written(&mut out)[0]["id"].as_u64().unwrap();
        let mut off = 0;
        assert!(line(&mut d, &mut off, &mut out, turn_reply(3, "old-turn")).is_empty());
        d.interrupt(&mut out).unwrap();
        assert!(
            out.stdin.is_empty(),
            "no turn of ours is known yet, so there is nothing to interrupt"
        );
        line(&mut d, &mut off, &mut out, turn_reply(mine, "new-turn"));
        assert_eq!(written(&mut out)[0]["params"]["turnId"], "new-turn");
    }

    // ---- findings 7 and 8 ---------------------------------------------------------------------

    /// `attach` gives the driver the offset the replay starts at (reported until a line moves it)
    /// and the thread id (no `thread/loaded/list` round trip for the first prompt).
    #[test]
    fn attach_gives_the_offset_and_the_thread() {
        let (mut d, mut out) = attached(4096, Some("t1"), vec![], 1);
        assert_eq!(d.resume_offset(), 4096);
        assert_eq!(d.agent_session_id(), Some("t1"));
        let mut off = 4096;
        line(
            &mut d,
            &mut off,
            &mut out,
            json!({"method": "thread/status/changed", "params": {}}),
        );
        assert_eq!(d.resume_offset(), off);
        // without the thread id, the first prompt asks which thread is loaded
        let (mut d, mut out) = attached(100, None, vec![], 1);
        d.prompt("x", PromptHow::Queue, &mut out).unwrap();
        assert_eq!(written(&mut out)[0]["method"], "thread/loaded/list");
    }
}
