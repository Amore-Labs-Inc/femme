//! Regression tests for the audit of the Pi driver (numbering as in the audit; `NOTES.md` has the
//! story). Synthetic lines shaped like `fixtures/pi/` and pi's RPC docs.
//!
//! Everything above the `NEW API` marker uses only calls the driver already had before the audit,
//! so on the unfixed crate those tests fail on an assertion (or a debug-build overflow panic). The
//! module below the marker needs `Driver::attach`, `held_prompts`, `pending_answers` or
//! `PromptDropped`, which did not exist.

mod common;

use femme_agents::pi::PiDriver;
use femme_agents::{AgentEvent, AgentStatus, Driver, Error, Launch, Out, StopReason};
use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::ItemKind;
use serde_json::{json, Value};

fn launch() -> Launch {
    Launch {
        cwd: "/w".into(),
        ..Launch::default()
    }
}

// ---- synthetic lines --------------------------------------------------------------------------

fn state(id: &str, sid: &str) -> Value {
    json!({"id": id, "type": "response", "command": "get_state", "success": true,
        "data": {"sessionId": sid, "model": {"provider": "mock", "id": "mock-a"}}})
}
fn started(id: &str) -> Value {
    json!({"id": id, "type": "response", "command": "prompt", "success": true, "data": {"disposition": "started"}})
}
fn refused(id: &str, why: &str) -> Value {
    json!({"id": id, "type": "response", "command": "prompt", "success": false, "error": why})
}
fn agent_start() -> Value {
    json!({"type": "agent_start"})
}
fn settled(aborted: bool) -> Value {
    json!({"type": "agent_settled", "aborted": aborted})
}
fn dialog(id: &str, method: &str, title: &str) -> Value {
    json!({"type": "extension_ui_request", "id": id, "method": method, "title": title,
        "options": ["a", "b"], "message": "m"})
}
fn tool_start(id: &str, name: &str) -> Value {
    json!({"type": "tool_execution_start", "toolCallId": id, "toolName": name, "args": {"command": "x"}})
}
fn tool_end(id: &str, name: &str, text: &str) -> Value {
    json!({"type": "tool_execution_end", "toolCallId": id, "toolName": name, "isError": false,
        "result": {"content": [{"type": "text", "text": text}]}})
}
fn assistant_end(usage_in: u64) -> Value {
    json!({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}],
        "usage": {"input": usage_in, "output": 1, "cacheRead": 0, "cacheWrite": 0}, "stopReason": "stop", "timestamp": 1}})
}

/// A driver and every line it has been fed (at its real offset).
struct P {
    d: PiDriver,
    spool: Vec<u8>,
    out: Out,
}

impl P {
    /// Past `get_state` (id `f1`): session `s1`, model `mock/mock-a`.
    fn ready() -> Self {
        let mut p = P {
            d: PiDriver::new(),
            spool: Vec::new(),
            out: Out::default(),
        };
        p.d.on_start(&launch(), &mut p.out);
        let ev = p.feed(state("f1", "s1"));
        assert!(
            matches!(ev.as_slice(), [AgentEvent::Started { .. }]),
            "{ev:?}"
        );
        p.wrote();
        p
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

    /// Send a prompt and return the `prompt` command it wrote now, if any.
    fn send(&mut self, text: &str, how: PromptHow) -> Option<Value> {
        self.d.prompt(text, how, &mut self.out).unwrap();
        self.wrote()
            .into_iter()
            .find(|m| m["type"] == "prompt" && m["message"] == text)
    }

    /// Pi accepted prompt `cmd` and a run is going.
    fn running(&mut self, cmd: &Value) {
        self.feed(started(cmd["id"].as_str().unwrap()));
        self.feed(agent_start());
        self.wrote();
    }

    /// Let pi run every `prompt` the driver writes, one after the other, until it writes no more;
    /// the messages in the order they were written.
    fn run_all(&mut self) -> Vec<String> {
        let mut order = Vec::new();
        loop {
            let prompts: Vec<Value> = self
                .wrote()
                .into_iter()
                .filter(|m| m["type"] == "prompt")
                .collect();
            if prompts.is_empty() {
                return order;
            }
            for m in prompts {
                order.push(m["message"].as_str().unwrap().to_owned());
                self.feed(started(m["id"].as_str().unwrap()));
                self.feed(agent_start());
                self.feed(settled(false));
            }
        }
    }
}

fn types(w: &[Value]) -> Vec<&str> {
    w.iter().map(|m| m["type"].as_str().unwrap()).collect()
}
fn asked(ev: &[AgentEvent]) -> Vec<String> {
    ev.iter()
        .filter_map(|e| match e {
            AgentEvent::QuestionAsked { q, .. } => Some(q.request_id.clone()),
            _ => None,
        })
        .collect()
}
fn resolved(ev: &[AgentEvent]) -> Vec<String> {
    ev.iter()
        .filter_map(|e| match e {
            AgentEvent::QuestionResolved { request_id } => Some(request_id.clone()),
            _ => None,
        })
        .collect()
}

// ---- finding 3: interrupt cancels an open dialog first -----------------------------------------

/// Pi keeps the extension blocked on an open dialog until it gets an `extension_ui_response`, even
/// after an abort. Interrupting (and `InterruptThenSend`) answers it with `cancelled: true` first.
#[test]
fn interrupt_cancels_an_open_dialog_before_it_aborts() {
    for via_prompt in [false, true] {
        let mut p = P::ready();
        let run = p.send("run", PromptHow::Queue).unwrap();
        p.running(&run);
        p.feed(tool_start("t1", "ask"));
        let ev = p.feed(dialog("q1", "editor", "Edit"));
        assert_eq!(asked(&ev), ["q1"]);
        assert_eq!(p.d.status(), AgentStatus::Blocked);
        if via_prompt {
            p.d.prompt("instead", PromptHow::InterruptThenSend, &mut p.out)
                .unwrap();
        } else {
            p.d.interrupt(&mut p.out).unwrap();
        }
        let w = p.wrote();
        assert_eq!(
            types(&w),
            ["extension_ui_response", "clear_queue", "abort"],
            "via_prompt {via_prompt}"
        );
        assert_eq!(
            w[0],
            json!({"type": "extension_ui_response", "id": "q1", "cancelled": true})
        );
        // pi never echoes an answer: the question resolves with the next line
        let ev = p.feed(json!({"type": "queue_update", "steering": [], "followUp": []}));
        assert_eq!(resolved(&ev), ["q1"]);
        // and it is not left open to be answered again
        assert!(matches!(
            p.d.answer("q1", &Answer::Cancel, &mut p.out),
            Err(Error::UnknownRequest(_))
        ));
    }
}

/// With nothing running there is nothing to abort; a dialog an extension opened on its own is still
/// cancelled, and with no dialog either an interrupt writes nothing.
#[test]
fn interrupt_is_a_no_op_unless_something_runs() {
    let mut p = P::ready();
    p.d.interrupt(&mut p.out)
        .expect("idle interrupt is not an error");
    assert!(p.out.stdin.is_empty());
    p.feed(dialog("c1", "confirm", "Continue?"));
    p.d.interrupt(&mut p.out).unwrap();
    let w = p.wrote();
    assert_eq!(
        w,
        [json!({"type": "extension_ui_response", "id": "c1", "cancelled": true})]
    );
}

// ---- finding 12: usage sums must not overflow --------------------------------------------------

/// A run's usage is the sum over its messages; a huge (or hostile) count saturates instead of
/// panicking in debug builds and wrapping in release.
#[test]
fn usage_sums_saturate() {
    let mut p = P::ready();
    let run = p.send("go", PromptHow::Queue).unwrap();
    p.running(&run);
    p.feed(assistant_end(u64::MAX));
    p.feed(assistant_end(u64::MAX));
    let ev = p.feed(settled(false));
    let Some(AgentEvent::TurnEnded {
        usage: Some(u),
        stop,
        ..
    }) = ev
        .iter()
        .find(|e| matches!(e, AgentEvent::TurnEnded { .. }))
    else {
        panic!("{ev:?}")
    };
    assert_eq!((u.input, u.output), (u64::MAX, 2));
    assert_eq!(*stop, StopReason::EndTurn);
}

// ---- finding 13: a tool result without a call ---------------------------------------------------

/// A result that arrives after its run settled (the call was already failed and forgotten) or for a
/// call never seen adds only its result item: the call keeps the name and input it has.
#[test]
fn a_late_or_unknown_tool_end_only_adds_its_result() {
    let mut p = P::ready();
    let run = p.send("go", PromptHow::Queue).unwrap();
    p.running(&run);
    p.feed(tool_start("t1", "bash"));
    let ev = p.feed(settled(true));
    assert!(
        ev.iter().any(|e| matches!(e, AgentEvent::ItemCompleted { id, kind: ItemKind::ToolCall, .. } if id == "t1")),
        "the run ended with the call open: {ev:?}"
    );
    assert_eq!(p.d.status(), AgentStatus::Idle);
    for id in ["t1", "never-seen"] {
        let ev = p.feed(tool_end(id, "bash", "late output"));
        let done: Vec<&str> = ev
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ItemCompleted { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(done, [format!("{id}:result")], "{id}: {ev:?}");
        assert!(
            !ev.iter()
                .any(|e| matches!(e, AgentEvent::TurnStarted | AgentEvent::Status(_))),
            "{id}: {ev:?}"
        );
        assert_eq!(p.d.status(), AgentStatus::Idle, "{id}");
        assert_eq!(p.d.resume_offset(), p.end(), "{id}: nothing left open");
    }
}

// ---- finding 14: ordering, interrupt, after exit -----------------------------------------------

/// Two `InterruptThenSend` prompts run in the order they were given, ahead of the `Queue` prompt
/// already waiting.
#[test]
fn interrupt_then_send_prompts_run_in_the_order_given() {
    let mut p = P::ready();
    let run = p.send("running", PromptHow::Queue).unwrap();
    p.running(&run);
    assert!(p.send("q", PromptHow::Queue).is_none());
    assert!(p.send("i1", PromptHow::InterruptThenSend).is_none());
    assert!(p.send("i2", PromptHow::InterruptThenSend).is_none());
    p.feed(settled(true));
    assert_eq!(p.run_all(), ["i1", "i2", "q"]);
}

/// Steers pi turns down while a run it had not reported yet goes on are held again, ahead of the
/// `Queue` prompt that was waiting and in the order they were written: a `push_front` per refusal
/// would have reversed them.
#[test]
fn steers_pi_turns_down_run_in_the_order_given() {
    let mut p = P::ready();
    let run = p.send("running", PromptHow::Queue).unwrap();
    p.running(&run);
    assert!(p.send("q", PromptHow::Queue).is_none());
    p.d.prompt("s1", PromptHow::Steer, &mut p.out).unwrap();
    p.d.prompt("s2", PromptHow::Steer, &mut p.out).unwrap();
    let steers: Vec<Value> = p
        .wrote()
        .into_iter()
        .filter(|m| m["type"] == "prompt")
        .collect();
    assert_eq!(steers.len(), 2);
    for s in &steers {
        p.feed(refused(s["id"].as_str().unwrap(), "agent is busy"));
    }
    p.feed(settled(false));
    assert_eq!(p.run_all(), ["s1", "s2", "q"]);
}

/// A prompt the driver holds when the run is interrupted is kept and runs after it; only pi's own
/// queue is cleared.
#[test]
fn interrupt_keeps_the_held_prompts() {
    let mut p = P::ready();
    let run = p.send("running", PromptHow::Queue).unwrap();
    p.running(&run);
    assert!(p.send("later", PromptHow::Queue).is_none());
    p.d.interrupt(&mut p.out).unwrap();
    assert_eq!(types(&p.wrote()), ["clear_queue", "abort"]);
    p.feed(settled(true));
    assert_eq!(p.run_all(), ["later"]);
}

/// After the process exited every call that would write says so and writes nothing.
#[test]
fn calls_after_exit_are_refused_and_write_nothing() {
    let mut p = P::ready();
    p.d.on_exit(Some(0));
    for how in [
        PromptHow::Queue,
        PromptHow::Steer,
        PromptHow::InterruptThenSend,
    ] {
        assert!(
            matches!(p.d.prompt("x", how, &mut p.out), Err(Error::State(_))),
            "{how:?}"
        );
    }
    assert!(matches!(p.d.interrupt(&mut p.out), Err(Error::State(_))));
    assert!(matches!(
        p.d.answer("r", &Answer::Cancel, &mut p.out),
        Err(Error::State(_))
    ));
    assert!(matches!(
        p.d.set_model("mock/m", &mut p.out),
        Err(Error::State(_))
    ));
    assert!(
        p.out.stdin.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&p.out.stdin)
    );
}

// ==== NEW API: the pre-fix crate has no `attach`, `held_prompts`, `pending_answers`, `PromptDropped` ====

mod new_api {
    use super::*;
    use femme_agents::{Attach, DropReason};

    fn dropped(ev: &[AgentEvent]) -> Vec<(String, PromptHow, DropReason)> {
        ev.iter()
            .filter_map(|e| match e {
                AgentEvent::PromptDropped { text, how, reason, .. } => {
                    Some((text.clone(), *how, reason.clone()))
                }
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

    fn attached(
        resume_offset: u64,
        sid: Option<&str>,
        answers: Vec<(String, Answer)>,
        epoch: u32,
    ) -> (PiDriver, Out) {
        let mut d = PiDriver::new();
        let mut out = Out::default();
        let a = Attach {
            resume_offset,
            agent_sid: sid.map(str::to_owned),
            model: Some("mock/mock-a".into()),
            answers,
            epoch,
        };
        d.attach(&launch(), &a, &mut out);
        (d, out)
    }

    fn written(out: &mut Out) -> Vec<Value> {
        let b = out.take();
        String::from_utf8_lossy(&b)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    // ---- finding 1 ----------------------------------------------------------------------------

    /// What the driver holds is visible, in the order it will go out, with mode and time.
    #[test]
    fn held_prompts_are_listed_in_send_order_with_mode_and_time() {
        let mut p = P::ready();
        let run = p.send("running", PromptHow::Queue).unwrap();
        p.running(&run);
        let before = now_ms();
        assert!(p.send("q1", PromptHow::Queue).is_none());
        assert!(p.send("i1", PromptHow::InterruptThenSend).is_none());
        let after = now_ms();
        let held = p.d.held_prompts();
        let got: Vec<(&str, PromptHow)> = held.iter().map(|h| (h.text.as_str(), h.how)).collect();
        assert_eq!(
            got,
            [
                ("i1", PromptHow::InterruptThenSend),
                ("q1", PromptHow::Queue)
            ]
        );
        assert!(held
            .iter()
            .all(|h| (before..=after).contains(&h.queued_at_ms)));
    }

    /// The steers pi throws away at an abort are announced one by one, with the note that was
    /// already there.
    #[test]
    fn clear_queue_announces_the_steers_pi_drops() {
        let mut p = P::ready();
        let run = p.send("running", PromptHow::Queue).unwrap();
        p.running(&run);
        p.d.interrupt(&mut p.out).unwrap();
        let clear = p.wrote().remove(0);
        assert_eq!(clear["type"], "clear_queue");
        let ev = p.feed(
            json!({"id": clear["id"], "type": "response", "command": "clear_queue", "success": true,
            "data": {"steering": ["s1", "s2"], "followUp": ["f1"]}}),
        );
        assert_eq!(
            dropped(&ev),
            [
                (
                    "s1".into(),
                    PromptHow::Steer,
                    DropReason::Refused("interrupted before pi delivered it".into())
                ),
                (
                    "s2".into(),
                    PromptHow::Steer,
                    DropReason::Refused("interrupted before pi delivered it".into())
                ),
                (
                    "f1".into(),
                    PromptHow::Queue,
                    DropReason::Refused("interrupted before pi delivered it".into())
                ),
            ]
        );
        assert!(
            ev.iter().any(|e| matches!(e, AgentEvent::ItemCompleted { kind: ItemKind::Event, body, .. } if body["text"].as_str().is_some_and(|t| t.contains("s1 | s2 | f1")))),
            "{ev:?}"
        );
    }

    /// A prompt pi refuses (no API key, ...) is announced with pi's words; one refused only because
    /// a run had not been reported yet is not dropped, it goes again.
    #[test]
    fn a_prompt_pi_refuses_is_announced() {
        let mut p = P::ready();
        let a = p.send("a", PromptHow::Queue).unwrap();
        let ev = p.feed(refused(a["id"].as_str().unwrap(), "No API key found"));
        assert_eq!(
            dropped(&ev),
            [(
                "a".into(),
                PromptHow::Queue,
                DropReason::Refused("No API key found".into())
            )]
        );
        assert!(dropped(&p.d.on_exit(None)).is_empty(), "announced once");

        // pi was already running something we had not seen start: the prompt waits and goes again
        let mut p = P::ready();
        let a = p.send("a", PromptHow::Queue).unwrap();
        p.feed(agent_start());
        let ev = p.feed(refused(
            a["id"].as_str().unwrap(),
            "Agent is already processing",
        ));
        assert!(dropped(&ev).is_empty(), "{ev:?}");
        assert_eq!(p.d.held_prompts().len(), 1);
        p.feed(settled(false));
        assert_eq!(p.run_all(), ["a"]);
    }

    /// Steers pi refused wait ahead of the queue: listed first, and announced first at exit.
    #[test]
    fn refused_steers_are_listed_and_announced_ahead_of_the_queue() {
        let mut p = P::ready();
        let run = p.send("running", PromptHow::Queue).unwrap();
        p.running(&run);
        assert!(p.send("q", PromptHow::Queue).is_none());
        p.d.prompt("s1", PromptHow::Steer, &mut p.out).unwrap();
        p.d.prompt("s2", PromptHow::Steer, &mut p.out).unwrap();
        for s in p.wrote().iter().filter(|m| m["type"] == "prompt") {
            p.feed(refused(s["id"].as_str().unwrap(), "agent is busy"));
        }
        let texts: Vec<String> = p.d.held_prompts().into_iter().map(|h| h.text).collect();
        assert_eq!(texts, ["s1", "s2", "q"]);
        let ev = p.d.on_exit(None);
        let got: Vec<String> = dropped(&ev).into_iter().map(|d| d.0).collect();
        assert_eq!(got, ["s1", "s2", "q"]);
    }

    /// Exit announces what did not run: the writes pi never acknowledged, oldest first (by order of
    /// sending, `f10` after `f9`), then the held prompts in their order, each also noted in the
    /// transcript; `Exited` stays last.
    #[test]
    fn exit_announces_every_prompt_that_did_not_run_in_order() {
        let mut p = P::ready();
        let run = p.send("running", PromptHow::Queue).unwrap();
        p.running(&run); // f2 is answered; steers below are f3..f12
        let steers: Vec<String> = (1..=10).map(|i| format!("s{i}")).collect();
        for s in &steers {
            assert!(p.send(s, PromptHow::Steer).is_some());
        }
        let ev = p.d.on_exit(Some(9));
        let want: Vec<(String, PromptHow, DropReason)> = steers
            .iter()
            .map(|s| (s.clone(), PromptHow::Steer, DropReason::Exited))
            .collect();
        assert_eq!(dropped(&ev), want);
        assert!(matches!(
            ev.last(),
            Some(AgentEvent::Exited { code: Some(9) })
        ));

        // held ones: announced after the written ones, and noted
        let mut p = P::ready();
        let run = p.send("running", PromptHow::Queue).unwrap();
        p.running(&run);
        assert!(p.send("s", PromptHow::Steer).is_some()); // written, no reply
        assert!(p.send("b", PromptHow::Queue).is_none());
        assert!(p.send("c", PromptHow::InterruptThenSend).is_none());
        p.wrote();
        let ev = p.d.on_exit(None);
        assert_eq!(
            dropped(&ev),
            [
                ("s".into(), PromptHow::Steer, DropReason::Exited),
                ("c".into(), PromptHow::InterruptThenSend, DropReason::Exited),
                ("b".into(), PromptHow::Queue, DropReason::Exited),
            ]
        );
        let notes: Vec<&str> = ev
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ItemCompleted {
                    kind: ItemKind::Event,
                    body,
                    ..
                } => body["text"].as_str(),
                _ => None,
            })
            .collect();
        assert_eq!(notes, ["Not sent, pi exited: c", "Not sent, pi exited: b"]);
        assert!(p.d.held_prompts().is_empty());
    }

    // ---- finding 4 ----------------------------------------------------------------------------

    /// Pi never echoes an answer, so a replay from before it meets the dialog again: a dialog the
    /// daemon says was answered is not asked again, the session is not `Blocked`, and it cannot be
    /// answered twice. `pending_answers()` lists only what a replay from the resume offset can still
    /// reach.
    #[test]
    fn an_answered_dialog_is_not_asked_again_after_a_restart() {
        let mut p = P::ready();
        let run = p.send("go", PromptHow::Queue).unwrap();
        p.running(&run);
        let turn_at = p.d.resume_offset();
        p.feed(tool_start("t1", "ask"));
        p.feed(dialog("q1", "confirm", "Deploy?"));
        p.d.answer("q1", &Answer::Option(0), &mut p.out).unwrap();
        p.wrote();
        let kept = p.d.pending_answers();
        assert_eq!(kept, [("q1".to_owned(), Answer::Option(0))]);

        // restart in the middle of the long tool: the replay starts at the turn and meets the dialog
        let (mut d, mut out) = attached(turn_at, Some("s1"), kept, 1);
        let mut ev = Vec::new();
        for (off, line) in common::spool_lines(&p.spool)
            .into_iter()
            .filter(|(o, _)| *o >= turn_at)
        {
            ev.extend(d.on_line(off, line, &mut out));
        }
        assert!(asked(&ev).is_empty(), "{ev:?}");
        assert!(
            !ev.contains(&AgentEvent::Status(AgentStatus::Blocked)),
            "{ev:?}"
        );
        assert_eq!(d.status(), AgentStatus::Working);
        assert!(matches!(
            d.answer("q1", &Answer::Option(1), &mut out),
            Err(Error::UnknownRequest(_))
        ));
        assert!(out.stdin.is_empty());
        assert_eq!(
            d.pending_answers().len(),
            1,
            "a replay from here still meets the dialog"
        );

        // once the turn is over a replay starts after the dialog: nothing left to remember
        p.feed(tool_end("t1", "ask", "ok"));
        p.feed(settled(false));
        assert_eq!(p.d.pending_answers(), []);
    }

    // ---- finding 5 ----------------------------------------------------------------------------

    /// The first process counts `f1`, `f2`, ...; after a re-attach the epoch is part of the id, so a
    /// replayed response of an earlier incarnation never matches a command of this one.
    #[test]
    fn ids_after_a_re_attach_carry_the_epoch() {
        let id_of = |epoch: u32| {
            let (mut d, mut out) = attached(0, Some("s1"), vec![], epoch);
            d.prompt("x", PromptHow::Queue, &mut out).unwrap();
            written(&mut out)[0]["id"].as_str().unwrap().to_owned()
        };
        let mut p = P::ready();
        assert_eq!(
            p.send("x", PromptHow::Queue).unwrap()["id"],
            "f2",
            "the first process keeps f<n>"
        );
        assert_eq!((id_of(1).as_str(), id_of(2).as_str()), ("f1-1", "f2-1"));
        assert_eq!(id_of(0), "f1-1", "an attach is at least the first one");

        // the old incarnation's refusal of its own `f2` shows up in the replay of the new one
        let (mut d, mut out) = attached(0, Some("s1"), vec![], 1);
        d.prompt("mine", PromptHow::Queue, &mut out).unwrap();
        assert_eq!(written(&mut out)[0]["id"], "f1-1");
        let ev = d.on_line(
            0,
            &serde_json::to_vec(&refused("f2", "Agent is already processing")).unwrap(),
            &mut out,
        );
        assert!(dropped(&ev).is_empty(), "not our command: {ev:?}");
        assert_eq!(d.held_prompts().len(), 0);
        let ev = d.on_exit(None);
        assert_eq!(
            dropped(&ev),
            [("mine".into(), PromptHow::Queue, DropReason::Exited)],
            "still ours, still unanswered"
        );
    }

    // ---- findings 7 and 8 ---------------------------------------------------------------------

    /// `attach` hands over the offset, the session id and the model. Pi says neither again on its
    /// own, so without a session id the driver asks `get_state` (its answer is `Started`), with one
    /// it asks nothing and a bare model id still completes into `provider/id`.
    #[test]
    fn attach_carries_the_session_and_the_model() {
        let (mut d, mut out) = attached(4096, Some("s1"), vec![], 1);
        assert_eq!(d.resume_offset(), 4096);
        assert_eq!(d.agent_session_id(), Some("s1"));
        assert!(out.stdin.is_empty());
        d.set_model("mock-b", &mut out).unwrap();
        let w = written(&mut out);
        assert_eq!(
            (w[0]["provider"].as_str(), w[0]["modelId"].as_str()),
            (Some("mock"), Some("mock-b"))
        );

        let (mut d, mut out) = attached(100, None, vec![], 3);
        let w = written(&mut out);
        assert_eq!(
            (w[0]["type"].as_str(), w[0]["id"].as_str()),
            (Some("get_state"), Some("f3-1"))
        );
        assert_eq!(d.agent_session_id(), None);
        let ev = d.on_line(
            100,
            &serde_json::to_vec(&state("f3-1", "s7")).unwrap(),
            &mut out,
        );
        assert_eq!(
            ev,
            [AgentEvent::Started {
                agent_sid: "s7".into(),
                model: Some("mock/mock-a".into())
            }]
        );
        assert_eq!(d.agent_session_id(), Some("s7"));
    }
}
