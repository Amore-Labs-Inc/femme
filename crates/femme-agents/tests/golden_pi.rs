//! `PiDriver` against the `pi --mode rpc` streams in `fixtures/pi/` (`scripts/capture-pi.py`, pi
//! 1.3.0 talking to the script's scripted model), plus synthetic cases for paths no capture
//! reaches and a never-panic fuzz. Golden events: `<case>.events.json` (`UPDATE_GOLDEN=1`
//! rewrites them; review the diff by eye).
mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::{fixture_dir, golden, replay, spool_lines};
use femme_agents::pi::PiDriver;
use femme_agents::{AgentEvent, AgentStatus, Driver, DropReason, Error, Launch, Out, StopReason};
use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::{ItemKind, ItemStatus, Usage};
use proptest::prelude::*;
use serde_json::{json, Value};

const CASES: &[&str] = &[
    "ok",
    "tool",
    "interrupt",
    "interrupt_send",
    "steer",
    "followup",
    "queue",
    "set_model",
    "state",
    "resume_setup",
    "resume",
    "kill9",
    "kill9_resume",
    "question",
    "error",
];

struct Case {
    calls: Value,
    spool: Vec<u8>,
    /// What the capture wrote to pi's stdin, with the stream position of each write.
    stdin: Vec<(u64, Value)>,
}

fn load(case: &str) -> Case {
    let dir = fixture_dir("pi");
    let read = |ext: &str| {
        std::fs::read(dir.join(format!("{case}.{ext}")))
            .unwrap_or_else(|e| panic!("{case}.{ext}: {e}"))
    };
    let calls: Value = serde_json::from_slice(&read("calls.json")).unwrap();
    let raw = read("stdin.jsonl");
    let lines = spool_lines(&raw);
    let at = calls["stdin_at"].as_array().unwrap();
    assert_eq!(
        lines.len(),
        at.len(),
        "{case}: stdin_at does not match stdin.jsonl"
    );
    let stdin = lines
        .iter()
        .zip(at)
        .map(|((_, l), a)| (a.as_u64().unwrap(), serde_json::from_slice(l).unwrap()))
        .collect();
    Case {
        spool: read("jsonl"),
        calls,
        stdin,
    }
}

fn launch(calls: &Value) -> Launch {
    let s = |k: &str| calls["launch"][k].as_str().map(str::to_owned);
    Launch {
        cwd: s("cwd").unwrap(),
        model: s("model"),
        resume: s("resume"),
        ..Launch::default()
    }
}

fn how(s: &str) -> PromptHow {
    match s {
        "Queue" => PromptHow::Queue,
        "Steer" => PromptHow::Steer,
        "InterruptThenSend" => PromptHow::InterruptThenSend,
        other => panic!("unknown PromptHow {other}"),
    }
}

/// Take what the driver wrote, as JSON values tagged with stream position `at`.
fn take(out: &mut Out, at: u64, into: &mut Vec<(u64, Value)>) {
    for l in out.lines() {
        into.push((at, serde_json::from_str(&l).expect("driver wrote JSON")));
    }
    out.take();
}

/// The captured conversation again: the stdout lines through a fresh driver, the driver calls at
/// the positions the capture made them (`at` = lines seen), then `on_exit`. Returns the events and
/// what the driver wrote.
fn run(case: &str) -> (Vec<AgentEvent>, Vec<(u64, Value)>) {
    let c = load(case);
    let l = launch(&c.calls);
    let calls = c.calls["calls"].as_array().unwrap();
    let lines = spool_lines(&c.spool);
    let mut d = PiDriver::new();
    let (mut out, mut wrote, mut events) = (Out::default(), Vec::new(), Vec::new());
    let mut next = calls.iter().peekable();
    for i in 0..=lines.len() {
        while let Some(call) = next.next_if(|c| c["at"].as_u64() == Some(i as u64)) {
            let s = |k: &str| call[k].as_str().unwrap_or_default().to_owned();
            match call["call"].as_str().unwrap() {
                "on_start" => d.on_start(&l, &mut out),
                "prompt" => d.prompt(&s("text"), how(&s("how")), &mut out).unwrap(),
                "interrupt" => d.interrupt(&mut out).unwrap(),
                "set_model" => d.set_model(&s("model"), &mut out).unwrap(),
                "answer" => d
                    .answer(
                        &s("request_id"),
                        &serde_json::from_value(call["answer"].clone()).unwrap(),
                        &mut out,
                    )
                    .unwrap(),
                "raw" => {} // a command PiDriver never sends; the capture script wrote it itself
                other => panic!("{case}: unknown call {other}"),
            }
            take(&mut out, i as u64, &mut wrote);
        }
        if let Some((off, line)) = lines.get(i) {
            events.extend(d.on_line(*off, line, &mut out));
            take(&mut out, i as u64 + 1, &mut wrote);
        }
    }
    assert!(next.next().is_none(), "{case}: calls after the last line");
    let code = c.calls["exit"].as_i64().map(|c| c as i32);
    events.extend(d.on_exit(code));
    (events, wrote)
}

/// The stdin lines the capture wrote on the driver's behalf (raw calls left out).
fn driver_stdin(case: &str) -> Vec<(u64, Value)> {
    let c = load(case);
    let mut want = c.stdin;
    for call in c.calls["calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["call"] == "raw")
    {
        let at = call["at"].as_u64().unwrap();
        let i = want
            .iter()
            .position(|(a, v)| *a == at && *v == call["line"])
            .unwrap_or_else(|| panic!("{case}: raw line not in stdin"));
        want.remove(i);
    }
    want
}

#[test]
fn golden_events_and_stdin() {
    for case in CASES {
        let (events, wrote) = run(case);
        assert_eq!(
            wrote,
            driver_stdin(case),
            "{case}: driver stdin differs from the capture"
        );
        check_invariants(case, &events);
        golden(
            &fixture_dir("pi").join(format!("{case}.events.json")),
            &events,
        );
    }
}

/// Contract rules every event stream must keep.
fn check_invariants(case: &str, events: &[AgentEvent]) {
    assert!(
        matches!(events.last(), Some(AgentEvent::Exited { .. })),
        "{case}: last event is not Exited"
    );
    let (mut status, mut in_turn) = (AgentStatus::Idle, false);
    let mut started = BTreeSet::new();
    let mut completed = BTreeMap::new();
    let (mut asked, mut resolved) = (BTreeSet::new(), BTreeSet::new());
    for (i, e) in events.iter().enumerate() {
        match e {
            AgentEvent::Status(s) => {
                assert_ne!(*s, status, "{case}: event {i}: Status without a change");
                status = *s;
            }
            AgentEvent::TurnStarted => {
                assert!(!in_turn, "{case}: event {i}: TurnStarted twice");
                in_turn = true;
                assert_eq!(
                    events.get(i + 1),
                    Some(&AgentEvent::Status(AgentStatus::Working)),
                    "{case}: event {i}"
                );
            }
            AgentEvent::TurnEnded { .. } => {
                assert!(in_turn, "{case}: event {i}: TurnEnded outside a turn");
                in_turn = false;
                assert_eq!(
                    events.get(i + 1),
                    Some(&AgentEvent::Status(AgentStatus::Idle)),
                    "{case}: event {i}"
                );
            }
            AgentEvent::ItemStarted { id, .. } => {
                assert!(started.insert(id.clone()), "{case}: {id} started twice");
            }
            AgentEvent::ItemCompleted {
                id, kind, status, ..
            } => {
                assert!(
                    completed.insert(id.clone(), (*kind, *status)).is_none(),
                    "{case}: {id} completed twice"
                );
            }
            AgentEvent::ItemDelta { id, .. } | AgentEvent::ItemUpdated { id, .. } => {
                assert!(
                    started.contains(id) && !completed.contains_key(id),
                    "{case}: {id} changed while not open"
                );
            }
            AgentEvent::QuestionAsked { item_id, q } => {
                assert_eq!(item_id, &q.request_id);
                asked.insert(q.request_id.clone());
                assert_eq!(
                    events.get(i + 1),
                    Some(&AgentEvent::Status(AgentStatus::Blocked)),
                    "{case}: event {i}"
                );
            }
            AgentEvent::QuestionResolved { request_id } => {
                assert!(
                    asked.contains(request_id) && resolved.insert(request_id.clone()),
                    "{case}: odd QuestionResolved {request_id}"
                );
            }
            _ => {}
        }
    }
    assert_eq!(status, AgentStatus::Idle, "{case}: not idle at the end");
    assert_eq!(asked, resolved, "{case}: unresolved questions");
    for id in &started {
        assert!(completed.contains_key(id), "{case}: {id} never completed");
    }
    for (id, (kind, st)) in &completed {
        if *kind == ItemKind::ToolCall && *st == ItemStatus::Completed {
            assert!(
                completed.contains_key(&format!("{id}:result")),
                "{case}: {id} has no result item"
            );
        }
    }
}

/// A fresh driver replaying from `resume_offset()` (daemon restart at any line) produces the same
/// events afterwards as the driver that saw everything; replay never writes to stdin.
#[test]
fn restart_at_every_line_replays_the_same() {
    let not_started = |evs: Vec<AgentEvent>| -> Vec<AgentEvent> {
        evs.into_iter()
            .filter(|e| !matches!(e, AgentEvent::Started { .. }))
            .collect()
    };
    for case in CASES {
        let spool = load(case).spool;
        let lines = spool_lines(&spool);
        for k in 0..=lines.len() {
            let (mut a, mut out) = (PiDriver::new(), Out::default());
            for (off, l) in &lines[..k] {
                a.on_line(*off, l, &mut out);
            }
            let r = a.resume_offset();
            let next = lines.get(k).map_or(spool.len() as u64, |(o, _)| *o);
            assert!(
                r <= next,
                "{case}@{k}: resume_offset {r} past the next line {next}"
            );
            let mut b = PiDriver::new();
            for (off, l) in lines[..k].iter().filter(|(o, _)| *o >= r) {
                b.on_line(*off, l, &mut out);
            }
            let (mut ea, mut eb) = (Vec::new(), Vec::new());
            for (off, l) in &lines[k..] {
                ea.extend(a.on_line(*off, l, &mut out));
                eb.extend(b.on_line(*off, l, &mut out));
            }
            ea.extend(a.on_exit(None));
            eb.extend(b.on_exit(None));
            assert_eq!(
                not_started(ea),
                not_started(eb),
                "{case}: restart before line {k} (resume at {r})"
            );
            assert!(
                out.stdin.is_empty(),
                "{case}: replay wrote {:?}",
                out.lines()
            );
        }
    }
}

#[test]
fn killed_mid_turn() {
    let spool = load("kill9").spool;
    let lines = spool_lines(&spool);
    let mut d = PiDriver::new();
    let (ev, _) = replay(&mut d, &spool);
    assert_eq!(d.status(), AgentStatus::Working);
    // The unfinished turn starts at agent_start (line 2).
    assert_eq!(
        std::str::from_utf8(lines[2].1).unwrap(),
        r#"{"type":"agent_start"}"#
    );
    assert_eq!(d.resume_offset(), lines[2].0);
    assert_eq!(
        d.agent_session_id(),
        Some("01a19bac-7f26-747d-a504-ccfa15f7aa9a")
    );
    assert!(!ev.iter().any(|e| matches!(e, AgentEvent::TurnEnded { .. })));
    let exit = d.on_exit(None);
    let text_id = format!("o{}:0", lines[8].0);
    assert!(
        matches!(&exit[0], AgentEvent::ItemCompleted { id, kind: ItemKind::AssistantMessage, status: ItemStatus::Failed, body, .. } if *id == text_id && body["text"].as_str().unwrap().starts_with("1\n2\n"))
    );
    assert!(matches!(
        &exit[1],
        AgentEvent::TurnEnded {
            stop: StopReason::Interrupted,
            usage: None,
            ..
        }
    ));
    assert_eq!(
        exit[2..],
        [
            AgentEvent::Status(AgentStatus::Idle),
            AgentEvent::Exited { code: None }
        ]
    );
    assert_eq!(d.resume_offset(), spool.len() as u64);
}

#[test]
fn turn_usage_and_duration() {
    let (ev, _) = run("tool");
    let ends: Vec<_> = ev
        .iter()
        .filter_map(|e| {
            if let AgentEvent::TurnEnded {
                stop,
                usage,
                duration_ms,
            } = e
            {
                Some((stop, usage, duration_ms))
            } else {
                None
            }
        })
        .collect();
    // Two LLM calls in one run: usage summed, duration from the prompt to the end of the last answer.
    assert_eq!(
        ends,
        [(
            &StopReason::EndTurn,
            &Some(Usage {
                input: 1260,
                output: 9,
                cache_read: 800,
                cache_write: 0
            }),
            &Some(114)
        )]
    );
}

// -- synthetic: prompt modes ---------------------------------------------------------------------

/// A driver that has started (get_state answered); returns it and the next free offset.
fn ready() -> (PiDriver, u64) {
    let mut d = PiDriver::new();
    let mut out = Out::default();
    d.on_start(
        &Launch {
            cwd: "/w".into(),
            ..Launch::default()
        },
        &mut out,
    );
    let ev = feed(
        &mut d,
        0,
        &json!({"id":"f1","type":"response","command":"get_state","success":true,"data":{"sessionId":"s1","model":{"provider":"mock","id":"mock-a"}}}),
        &mut out,
    );
    assert_eq!(
        ev,
        [AgentEvent::Started {
            agent_sid: "s1".into(),
            model: Some("mock/mock-a".into())
        }]
    );
    (d, 1000)
}

fn feed(d: &mut PiDriver, off: u64, v: &Value, out: &mut Out) -> Vec<AgentEvent> {
    d.on_line(off, serde_json::to_string(v).unwrap().as_bytes(), out)
}

fn written(out: &mut Out) -> Vec<Value> {
    let v = out
        .lines()
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    out.take();
    v
}

fn started(id: &str) -> Value {
    json!({"id": id, "type":"response","command":"prompt","success":true,"data":{"disposition":"started"}})
}

const START: &str = r#"{"type":"agent_start"}"#;
const SETTLED: &str = r#"{"type":"agent_settled","aborted":false}"#;

#[test]
fn queue_holds_prompts_until_each_run_settles() {
    let (mut d, _) = ready();
    let mut out = Out::default();
    d.prompt("a", PromptHow::Queue, &mut out).unwrap();
    assert_eq!(
        written(&mut out),
        [json!({"id":"f2","type":"prompt","message":"a"})]
    );
    // Before pi even answered: held.
    d.prompt("b", PromptHow::Queue, &mut out).unwrap();
    d.prompt("c", PromptHow::Queue, &mut out).unwrap();
    assert!(written(&mut out).is_empty());
    feed(&mut d, 100, &started("f2"), &mut out);
    d.on_line(200, START.as_bytes(), &mut out);
    assert!(written(&mut out).is_empty());
    d.on_line(300, SETTLED.as_bytes(), &mut out);
    assert_eq!(
        written(&mut out),
        [json!({"id":"f3","type":"prompt","message":"b"})]
    );
    feed(&mut d, 400, &started("f3"), &mut out);
    d.on_line(500, START.as_bytes(), &mut out);
    d.on_line(600, SETTLED.as_bytes(), &mut out);
    assert_eq!(
        written(&mut out),
        [json!({"id":"f4","type":"prompt","message":"c"})]
    );
}

#[test]
fn steer_goes_into_the_running_turn() {
    let (mut d, _) = ready();
    let mut out = Out::default();
    d.prompt("a", PromptHow::Queue, &mut out).unwrap();
    // No agent_start yet: nothing to steer, it waits like Queue.
    d.prompt("early", PromptHow::Steer, &mut out).unwrap();
    assert_eq!(written(&mut out).len(), 1);
    feed(&mut d, 100, &started("f2"), &mut out);
    d.on_line(200, START.as_bytes(), &mut out);
    d.prompt("s", PromptHow::Steer, &mut out).unwrap();
    assert_eq!(
        written(&mut out),
        [json!({"id":"f3","type":"prompt","message":"s","streamingBehavior":"steer"})]
    );
    feed(
        &mut d,
        300,
        &json!({"id":"f3","type":"response","command":"prompt","success":true,"data":{"disposition":"queued"}}),
        &mut out,
    );
    d.on_line(400, SETTLED.as_bytes(), &mut out);
    assert_eq!(
        written(&mut out),
        [json!({"id":"f4","type":"prompt","message":"early"})]
    );
}

#[test]
fn steer_that_raced_the_end_of_a_run_starts_one() {
    let (mut d, _) = ready();
    let mut out = Out::default();
    d.prompt("a", PromptHow::Queue, &mut out).unwrap();
    feed(&mut d, 100, &started("f2"), &mut out);
    d.on_line(200, START.as_bytes(), &mut out);
    d.prompt("s", PromptHow::Steer, &mut out).unwrap();
    d.prompt("q", PromptHow::Queue, &mut out).unwrap();
    written(&mut out);
    // The run settles before pi reads the steer; pi starts a new run for it.
    d.on_line(300, SETTLED.as_bytes(), &mut out);
    assert!(
        written(&mut out).is_empty(),
        "the steer is still unanswered"
    );
    feed(&mut d, 400, &started("f3"), &mut out);
    assert!(written(&mut out).is_empty(), "a run was announced");
    d.on_line(500, START.as_bytes(), &mut out);
    d.on_line(600, SETTLED.as_bytes(), &mut out);
    assert_eq!(
        written(&mut out),
        [json!({"id":"f4","type":"prompt","message":"q"})]
    );
}

#[test]
fn interrupt_then_send_clears_aborts_and_sends_first() {
    let (mut d, _) = ready();
    let mut out = Out::default();
    // nothing runs: an interrupt is an Ok that writes nothing
    assert_eq!(d.interrupt(&mut out), Ok(()));
    assert!(out.stdin.is_empty());
    d.prompt("a", PromptHow::Queue, &mut out).unwrap();
    feed(&mut d, 100, &started("f2"), &mut out);
    d.on_line(200, START.as_bytes(), &mut out);
    d.prompt("later", PromptHow::Queue, &mut out).unwrap();
    written(&mut out);
    d.prompt("now", PromptHow::InterruptThenSend, &mut out)
        .unwrap();
    assert_eq!(
        written(&mut out),
        [
            json!({"id":"f3","type":"clear_queue"}),
            json!({"id":"f4","type":"abort"})
        ]
    );
    let ev = feed(
        &mut d,
        300,
        &json!({"id":"f3","type":"response","command":"clear_queue","success":true,"data":{"steering":["s1"],"followUp":[]}}),
        &mut out,
    );
    assert!(
        matches!(&ev[..], [AgentEvent::ItemCompleted { id, kind: ItemKind::Event, body, .. }, AgentEvent::PromptDropped { text, how: PromptHow::Steer, reason: DropReason::Refused(_) }] if id == "o300" && body["text"] == "Not delivered (interrupted): s1" && text == "s1")
    );
    let ev = d.on_line(400, br#"{"type":"agent_settled","aborted":true}"#, &mut out);
    assert!(matches!(
        ev[0],
        AgentEvent::TurnEnded {
            stop: StopReason::Interrupted,
            ..
        }
    ));
    assert_eq!(
        written(&mut out),
        [json!({"id":"f5","type":"prompt","message":"now"})]
    );
    feed(
        &mut d,
        500,
        &json!({"id":"f4","type":"response","command":"abort","success":true}),
        &mut out,
    );
    feed(&mut d, 600, &started("f5"), &mut out);
    d.on_line(700, START.as_bytes(), &mut out);
    d.on_line(800, SETTLED.as_bytes(), &mut out);
    assert_eq!(
        written(&mut out),
        [json!({"id":"f6","type":"prompt","message":"later"})]
    );
}

#[test]
fn refused_prompts() {
    let (mut d, _) = ready();
    let mut out = Out::default();
    // Refused while idle (no model, no key): reported, dropped.
    d.prompt("a", PromptHow::Queue, &mut out).unwrap();
    let ev = feed(
        &mut d,
        100,
        &json!({"id":"f2","type":"response","command":"prompt","success":false,"error":"No API key found"}),
        &mut out,
    );
    assert!(
        matches!(&ev[..], [AgentEvent::ItemCompleted { id, kind: ItemKind::Event, body, .. }, AgentEvent::PromptDropped { text, how: PromptHow::Queue, reason: DropReason::Refused(why) }] if id == "o100" && body["text"] == "pi did not take the prompt: No API key found" && text == "a" && why == "No API key found")
    );
    assert_eq!(
        written(&mut out),
        [json!({"id":"f2","type":"prompt","message":"a"})]
    );
    // Refused because a run pi had started on its own was going: sent again after it.
    d.prompt("b", PromptHow::Queue, &mut out).unwrap();
    assert_eq!(
        written(&mut out),
        [json!({"id":"f3","type":"prompt","message":"b"})]
    );
    d.on_line(200, START.as_bytes(), &mut out);
    let ev = feed(
        &mut d,
        300,
        &json!({"id":"f3","type":"response","command":"prompt","success":false,"error":"Agent is already processing."}),
        &mut out,
    );
    assert!(ev.is_empty());
    d.on_line(400, SETTLED.as_bytes(), &mut out);
    assert_eq!(
        written(&mut out),
        [json!({"id":"f4","type":"prompt","message":"b"})]
    );
}

#[test]
fn set_model_is_confirmed_by_pi() {
    let (mut d, _) = ready();
    let mut out = Out::default();
    d.set_model("mock-b", &mut out).unwrap();
    assert_eq!(
        written(&mut out),
        [json!({"id":"f2","type":"set_model","provider":"mock","modelId":"mock-b"})]
    );
    let ev = feed(
        &mut d,
        100,
        &json!({"id":"f2","type":"response","command":"set_model","success":true,"data":{"provider":"mock","id":"mock-b"}}),
        &mut out,
    );
    assert_eq!(ev, [AgentEvent::ModelChanged("mock/mock-b".into())]);
    let ev = feed(
        &mut d,
        200,
        &json!({"id":"f3","type":"response","command":"set_model","success":false,"error":"Model not found: mock/nope"}),
        &mut out,
    );
    assert!(
        matches!(&ev[..], [AgentEvent::ItemCompleted { kind: ItemKind::Event, body, .. }] if body["text"] == "pi set_model failed: Model not found: mock/nope")
    );
}

// -- synthetic: extension dialogs (shapes from pi's docs/rpc-extension-ui.md; only `select` was
// -- captured live, in fixtures/pi/question.jsonl) -------------------------------------------------

fn dialog(d: &mut PiDriver, off: u64, v: Value) -> Vec<AgentEvent> {
    let mut out = Out::default();
    let mut v = v;
    v["type"] = json!("extension_ui_request");
    feed(d, off, &v, &mut out)
}

fn answer(d: &mut PiDriver, id: &str, a: Answer) -> Result<Value, Error> {
    let mut out = Out::default();
    d.answer(id, &a, &mut out)?;
    Ok(written(&mut out).remove(0))
}

#[test]
fn dialogs_map_to_questions() {
    let (mut d, _) = ready();
    let ev = dialog(
        &mut d,
        100,
        json!({"id":"c1","method":"confirm","title":"Clear session?","message":"All messages will be lost.","timeout":5000}),
    );
    let AgentEvent::QuestionAsked { q, .. } = &ev[0] else {
        panic!("{ev:?}")
    };
    assert_eq!(
        (q.prompt.as_str(), q.options.clone(), q.free_text),
        (
            "Clear session?\n\nAll messages will be lost.",
            vec!["Yes".to_owned(), "No".to_owned()],
            false
        )
    );
    assert_eq!(ev[1], AgentEvent::Status(AgentStatus::Blocked));
    assert_eq!(d.resume_offset(), 100);
    assert!(matches!(
        answer(&mut d, "c1", Answer::Text("maybe".into())),
        Err(Error::Bad(_))
    ));
    assert_eq!(
        answer(&mut d, "c1", Answer::Option(1)).unwrap(),
        json!({"type":"extension_ui_response","id":"c1","confirmed":false})
    );
    assert_eq!(
        answer(&mut d, "c1", Answer::Option(0)),
        Err(Error::UnknownRequest("c1".into()))
    );
    // Resolved with the next line, whatever it is.
    let mut out = Out::default();
    let ev = d.on_line(
        200,
        br#"{"type":"queue_update","steering":[],"followUp":[]}"#,
        &mut out,
    );
    assert_eq!(
        ev,
        [
            AgentEvent::QuestionResolved {
                request_id: "c1".into()
            },
            AgentEvent::Status(AgentStatus::Idle)
        ]
    );

    dialog(
        &mut d,
        300,
        json!({"id":"i1","method":"input","title":"Enter a value","placeholder":"type something..."}),
    );
    assert!(matches!(
        answer(&mut d, "i1", Answer::Option(0)),
        Err(Error::Bad(_))
    ));
    assert_eq!(
        answer(&mut d, "i1", Answer::Text("42".into())).unwrap(),
        json!({"type":"extension_ui_response","id":"i1","value":"42"})
    );
    let ev = dialog(
        &mut d,
        400,
        json!({"id":"e1","method":"editor","title":"Edit some text","prefill":"Line 1"}),
    );
    // The answer to i1 is reported first, then the new dialog.
    assert!(
        matches!(&ev[..], [AgentEvent::QuestionResolved { request_id }, AgentEvent::Status(AgentStatus::Idle), AgentEvent::QuestionAsked { q, .. }, AgentEvent::Status(AgentStatus::Blocked)] if request_id == "i1" && q.prompt == "Edit some text\n\nLine 1" && q.free_text)
    );
    assert_eq!(
        answer(&mut d, "e1", Answer::Cancel).unwrap(),
        json!({"type":"extension_ui_response","id":"e1","cancelled":true})
    );

    dialog(
        &mut d,
        500,
        json!({"id":"s1","method":"select","title":"Allow?","options":["Allow","Block"]}),
    );
    assert!(matches!(
        answer(&mut d, "s1", Answer::Option(2)),
        Err(Error::Bad(_))
    ));
    assert!(matches!(
        answer(&mut d, "s1", Answer::Text("Maybe".into())),
        Err(Error::Bad(_))
    ));
    assert_eq!(
        answer(&mut d, "s1", Answer::Text("Block".into())).unwrap(),
        json!({"type":"extension_ui_response","id":"s1","value":"Block"})
    );
    assert_eq!(
        answer(&mut d, "nope", Answer::Cancel),
        Err(Error::UnknownRequest("nope".into()))
    );

    // Fire-and-forget calls.
    let ev = dialog(
        &mut d,
        600,
        json!({"id":"n1","method":"notify","message":"Command blocked","notifyType":"warning"}),
    );
    assert!(
        matches!(&ev[..], [AgentEvent::QuestionResolved { .. }, AgentEvent::Status(AgentStatus::Idle), AgentEvent::ItemCompleted { id, kind: ItemKind::Event, body, .. }] if id == "o600" && body["text"] == "warning: Command blocked")
    );
    assert!(dialog(
        &mut d,
        700,
        json!({"id":"w1","method":"setWidget","widgetKey":"k","widgetLines":["a"]})
    )
    .is_empty());
    assert!(matches!(
        &dialog(&mut d, 800, json!({"id":"x1","method":"custom"}))[..],
        [AgentEvent::Raw(_)]
    ));
}

#[test]
fn unanswered_dialog_ends_with_its_tool() {
    // A dialog with a timeout that pi resolved by itself: over once the asking tool returned.
    let (mut d, _) = ready();
    let mut out = Out::default();
    d.on_line(100, START.as_bytes(), &mut out);
    feed(
        &mut d,
        200,
        &json!({"type":"tool_execution_start","toolCallId":"t1","toolName":"ask","args":{}}),
        &mut out,
    );
    feed(
        &mut d,
        250,
        &json!({"type":"tool_execution_start","toolCallId":"t2","toolName":"bash","args":{"command":"sleep 1"}}),
        &mut out,
    );
    dialog(
        &mut d,
        300,
        json!({"id":"q1","method":"select","title":"Pick","options":["a","b"],"timeout":1000}),
    );
    assert_eq!(d.status(), AgentStatus::Blocked);
    let end = |id: &str| json!({"type":"tool_execution_end","toolCallId":id,"toolName":"x","result":{"content":[{"type":"text","text":"done"}]},"isError":false});
    let ev = feed(&mut d, 400, &end("t1"), &mut out);
    assert!(
        !ev.contains(&AgentEvent::QuestionResolved {
            request_id: "q1".into()
        }),
        "t2 was running too"
    );
    let ev = feed(&mut d, 500, &end("t2"), &mut out);
    assert_eq!(
        ev[ev.len() - 2..],
        [
            AgentEvent::QuestionResolved {
                request_id: "q1".into()
            },
            AgentEvent::Status(AgentStatus::Working)
        ]
    );
}

#[test]
fn on_exit_closes_everything() {
    let (mut d, _) = ready();
    let mut out = Out::default();
    d.prompt("a", PromptHow::Queue, &mut out).unwrap();
    feed(&mut d, 100, &started("f2"), &mut out);
    d.on_line(200, START.as_bytes(), &mut out);
    feed(
        &mut d,
        300,
        &json!({"type":"message_start","message":{"role":"assistant","content":[]}}),
        &mut out,
    );
    feed(
        &mut d,
        400,
        &json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"hm"}}),
        &mut out,
    );
    feed(
        &mut d,
        500,
        &json!({"type":"message_update","assistantMessageEvent":{"type":"toolcall_start","contentIndex":1,"id":"t1","toolName":"bash"}}),
        &mut out,
    );
    let q = json!({"type":"extension_ui_request","id":"q1","method":"input","title":"?"});
    feed(&mut d, 600, &q, &mut out);
    let end = 600 + serde_json::to_string(&q).unwrap().len() as u64 + 1;
    d.prompt("b", PromptHow::Queue, &mut out).unwrap();
    assert_eq!(d.resume_offset(), 200);
    let ev = d.on_exit(Some(1));
    assert_eq!(
        ev,
        [
            AgentEvent::QuestionResolved {
                request_id: "q1".into()
            },
            AgentEvent::ItemCompleted {
                id: "o300:0".into(),
                kind: ItemKind::Thinking,
                status: ItemStatus::Failed,
                body: json!({"text":"hm"}),
                parent: None,
            },
            AgentEvent::ItemCompleted {
                id: "t1".into(),
                kind: ItemKind::ToolCall,
                status: ItemStatus::Failed,
                body: json!({"name":"bash","input":null,"call_id":"t1"}),
                parent: None,
            },
            AgentEvent::TurnEnded {
                stop: StopReason::Interrupted,
                usage: None,
                duration_ms: None
            },
            AgentEvent::Status(AgentStatus::Idle),
            AgentEvent::PromptDropped {
                text: "b".into(),
                how: PromptHow::Queue,
                reason: DropReason::Exited
            },
            AgentEvent::ItemCompleted {
                id: format!("o{end}:unsent0"),
                kind: ItemKind::Event,
                status: ItemStatus::Completed,
                body: json!({"text":"Not sent, pi exited: b"}),
                parent: None,
            },
            AgentEvent::Exited { code: Some(1) },
        ]
    );
    assert_eq!(d.resume_offset(), end);
}

#[test]
fn aborted_tool_call_never_runs() {
    let (mut d, _) = ready();
    let mut out = Out::default();
    d.on_line(100, START.as_bytes(), &mut out);
    feed(
        &mut d,
        200,
        &json!({"type":"message_start","message":{"role":"assistant","content":[]}}),
        &mut out,
    );
    feed(
        &mut d,
        300,
        &json!({"type":"message_update","assistantMessageEvent":{"type":"toolcall_start","contentIndex":0,"id":"t1","toolName":"bash"}}),
        &mut out,
    );
    let ev = feed(
        &mut d,
        400,
        &json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"toolCall","id":"t1","name":"bash","arguments":{"command":"ls"}}],"stopReason":"aborted","errorMessage":"Request was aborted","timestamp":10,"durationMs":5}}),
        &mut out,
    );
    assert_eq!(
        ev,
        [
            AgentEvent::ItemUpdated {
                id: "t1".into(),
                body: json!({"name":"bash","input":{"command":"ls"},"call_id":"t1"})
            },
            AgentEvent::ItemCompleted {
                id: "t1".into(),
                kind: ItemKind::ToolCall,
                status: ItemStatus::Failed,
                body: json!({"name":"bash","input":{"command":"ls"},"call_id":"t1"}),
                parent: None,
            },
        ]
    );
    let ev = d.on_line(500, br#"{"type":"agent_settled","aborted":true}"#, &mut out);
    assert_eq!(
        ev[0],
        AgentEvent::TurnEnded {
            stop: StopReason::Interrupted,
            usage: None,
            duration_ms: Some(5)
        }
    );
}

#[test]
fn other_events() {
    let (mut d, _) = ready();
    let mut out = Out::default();
    let text = |ev: Vec<AgentEvent>| match &ev[..] {
        [AgentEvent::ItemCompleted {
            kind: ItemKind::Event,
            body,
            ..
        }] => body["text"].as_str().unwrap().to_owned(),
        other => panic!("{other:?}"),
    };
    let t = text(feed(
        &mut d,
        1,
        &json!({"type":"auto_retry_start","attempt":1,"maxAttempts":3,"delayMs":2000,"errorMessage":"529 overloaded"}),
        &mut out,
    ));
    assert_eq!(t, "Retrying (1/3) in 2000 ms: 529 overloaded");
    assert!(feed(
        &mut d,
        2,
        &json!({"type":"auto_retry_end","success":true,"attempt":2}),
        &mut out
    )
    .is_empty());
    let t = text(feed(
        &mut d,
        3,
        &json!({"type":"compaction_end","reason":"threshold","result":{"tokensBefore":150000},"aborted":false}),
        &mut out,
    ));
    assert_eq!(t, "Context compacted (threshold), 150000 tokens before");
    let t = text(feed(
        &mut d,
        4,
        &json!({"type":"extension_error","extensionPath":"/x.ts","event":"tool_call","error":"boom"}),
        &mut out,
    ));
    assert_eq!(t, "Extension error (/x.ts, tool_call): boom");
    let t = text(feed(
        &mut d,
        5,
        &json!({"type":"message_end","message":{"role":"custom","customType":"x","content":"shown","display":true}}),
        &mut out,
    ));
    assert_eq!(t, "shown");
    let t = text(feed(
        &mut d,
        6,
        &json!({"type":"response","command":"parse","success":false,"error":"Failed to parse command"}),
        &mut out,
    ));
    assert_eq!(t, "pi parse failed: Failed to parse command");
    assert!(out.stdin.is_empty());
}

// -- fuzz ----------------------------------------------------------------------------------------

fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<f64>()
            .prop_map(|f| serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)),
        "[a-z_]{0,8}".prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map("[a-zA-Z_]{1,12}", inner, 0..5)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

const TYPES: &[&str] = &[
    "response",
    "agent_start",
    "agent_end",
    "agent_settled",
    "turn_start",
    "turn_end",
    "message_start",
    "message_update",
    "message_end",
    "tool_execution_start",
    "tool_execution_update",
    "tool_execution_end",
    "extension_ui_request",
    "extension_error",
    "auto_retry_start",
    "auto_retry_end",
    "compaction_end",
    "queue_update",
];

/// A message of a known type with fields drawn from a small vocabulary, so the fuzz reaches the
/// mapping code and not just the fallbacks.
fn arb_message() -> impl Strategy<Value = Value> {
    let word = prop::sample::select(vec![
        "",
        "t1",
        "t2",
        "q1",
        "f2",
        "user",
        "assistant",
        "toolResult",
        "custom",
        "select",
        "confirm",
        "input",
        "notify",
        "prompt",
        "get_state",
        "set_model",
        "clear_queue",
        "started",
        "text_delta",
        "thinking_start",
        "toolcall_start",
        "toolcall_end",
        "text_end",
        "aborted",
        "error",
        "stop",
    ]);
    (
        prop::sample::select(TYPES.to_vec()),
        prop::collection::btree_map(
            prop::sample::select(vec![
                "id",
                "command",
                "success",
                "data",
                "message",
                "role",
                "assistantMessageEvent",
                "toolCallId",
                "method",
                "options",
                "aborted",
                "content",
                "contentIndex",
                "delta",
                "stopReason",
                "usage",
                "timestamp",
            ]),
            prop_oneof![
                word.clone().prop_map(Value::from),
                arb_json(),
                (0u64..4).prop_map(Value::from)
            ],
            0..8,
        ),
        word.clone(),
        word,
    )
        .prop_map(|(t, fields, role, sub)| {
            let mut v = Value::Object(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect());
            v["type"] = json!(t);
            if v["message"].is_object() {
                v["message"]["role"] = json!(role);
            }
            if v["assistantMessageEvent"].is_object() {
                v["assistantMessageEvent"]["type"] = json!(sub);
            }
            v
        })
}

/// A line is complete: the newline that ended it is not part of it (`on_line` asserts that).
fn one_line(mut l: Vec<u8>) -> Vec<u8> {
    l.retain(|b| *b != b'\n');
    l
}

#[derive(Debug, Clone)]
enum Step {
    Line(Vec<u8>),
    Prompt(u8),
    Interrupt,
    Answer(u16),
    Model,
}

fn arb_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => arb_message().prop_map(|v| Step::Line(serde_json::to_vec(&v).unwrap())),
        2 => arb_json().prop_map(|v| Step::Line(serde_json::to_vec(&v).unwrap())),
        1 => prop::collection::vec(any::<u8>(), 0..64).prop_map(|l| Step::Line(one_line(l))),
        1 => (0u8..3).prop_map(Step::Prompt),
        1 => Just(Step::Interrupt),
        1 => (0u16..3).prop_map(Step::Answer),
        1 => Just(Step::Model),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn never_panics(start in any::<bool>(), steps in prop::collection::vec(arb_step(), 0..40)) {
        let mut d = PiDriver::new();
        let mut out = Out::default();
        if start {
            d.on_start(&Launch { cwd: "/w".into(), ..Launch::default() }, &mut out);
        }
        let (mut off, mut asked) = (0u64, Vec::new());
        for s in steps {
            match s {
                Step::Line(l) => {
                    for e in d.on_line(off, &l, &mut out) {
                        if let AgentEvent::QuestionAsked { q, .. } = e {
                            asked.push(q.request_id);
                        }
                    }
                    off += l.len() as u64 + 1;
                    prop_assert!(d.resume_offset() <= off);
                }
                Step::Prompt(h) => {
                    let how = [PromptHow::Queue, PromptHow::Steer, PromptHow::InterruptThenSend][usize::from(h)];
                    prop_assert!(d.prompt("x", how, &mut out).is_ok());
                }
                Step::Interrupt => { let _ = d.interrupt(&mut out); }
                Step::Answer(i) => {
                    let id = asked.last().cloned().unwrap_or_default();
                    let _ = d.answer(&id, &Answer::Option(i), &mut out);
                    let _ = d.answer(&id, &Answer::Text("t".into()), &mut out);
                }
                Step::Model => { let _ = d.set_model("mock/m", &mut out); }
            }
            for l in written(&mut out) {
                prop_assert!(l.is_object());
            }
        }
        let ev = d.on_exit(None);
        prop_assert_eq!(ev.last(), Some(&AgentEvent::Exited { code: None }));
        prop_assert_eq!(d.status(), AgentStatus::Idle);
        prop_assert_eq!(d.resume_offset(), off);
    }

    #[test]
    fn unknown_types_are_raw(t in "[a-z]{1,8}_zz[a-z_]{0,8}", fields in arb_json()) {
        let (mut d, _) = ready();
        let mut v = if fields.is_object() { fields } else { json!({"x": fields}) };
        v["type"] = json!(t);
        let mut out = Out::default();
        let line = serde_json::to_vec(&v).unwrap();
        let ev = d.on_line(10_000, &line, &mut out);
        // compare with the parsed line: float text round trips are not exact
        prop_assert_eq!(ev, vec![AgentEvent::Raw(serde_json::from_slice(&line).unwrap())]);
        prop_assert!(out.stdin.is_empty());
    }

    #[test]
    fn non_json_lines_are_raw_text(line in prop::collection::vec(any::<u8>(), 1..80).prop_map(one_line)) {
        let mut d = PiDriver::new();
        let mut out = Out::default();
        let ev = d.on_line(0, &line, &mut out);
        let parsed = serde_json::from_slice::<Value>(line.strip_suffix(b"\r").unwrap_or(&line));
        let blank = line.iter().all(u8::is_ascii_whitespace);
        match parsed {
            _ if blank => prop_assert!(ev.is_empty()),
            Ok(v) if v.is_object() => {}
            Ok(v) => prop_assert_eq!(ev, vec![AgentEvent::Raw(v)]),
            Err(_) => prop_assert_eq!(ev, vec![AgentEvent::Raw(Value::String(String::from_utf8_lossy(&line).into_owned()))]),
        }
        prop_assert!(out.stdin.is_empty());
    }
}
