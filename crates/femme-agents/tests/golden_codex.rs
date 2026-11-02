//! `CodexDriver` against the captured `codex app-server` streams in `fixtures/codex/`
//! (`scripts/capture-codex.py`), plus synthetic cases for paths no cheap capture reaches and a
//! never-panic fuzz. Golden events: `<case>.events.json` (`UPDATE_GOLDEN=1` rewrites them).
mod common;

use std::collections::BTreeSet;

use common::{fixture_dir, golden, replay, spool_lines};
use femme_agents::codex::{self, CodexDriver};
use femme_agents::{AgentEvent, AgentStatus, Driver, Error, Launch, Out, StopReason};
use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::{ItemKind, ItemStatus};
use proptest::prelude::*;
use serde_json::{json, Value};

const CASES: &[&str] = &[
    "ok",
    "tool",
    "question",
    "interrupt",
    "steer",
    "queue",
    "resume",
    "rate_limits",
];

struct Case {
    calls: Value,
    spool: Vec<u8>,
    stdin: Vec<Value>,
}

fn load(case: &str) -> Case {
    let dir = fixture_dir("codex");
    let read = |ext: &str| {
        std::fs::read(dir.join(format!("{case}.{ext}")))
            .unwrap_or_else(|e| panic!("{case}.{ext}: {e}"))
    };
    let stdin = read("stdin.jsonl");
    let stdin = spool_lines(&stdin)
        .into_iter()
        .map(|(_, l)| norm(serde_json::from_slice(l).unwrap()))
        .collect();
    Case {
        calls: serde_json::from_slice(&read("calls.json")).unwrap(),
        spool: read("jsonl"),
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

/// The client version changes with the crate; everything else must match the capture.
fn norm(mut v: Value) -> Value {
    if v["params"]["clientInfo"]["version"].is_string() {
        v["params"]["clientInfo"]["version"] = json!("*");
    }
    v
}

/// What the driver wrote since the last call, one JSON value per line.
fn written(out: &mut Out) -> Vec<Value> {
    let bytes = out.take();
    spool_lines(&bytes)
        .into_iter()
        .map(|(_, l)| norm(serde_json::from_slice(l).expect("driver wrote JSON")))
        .collect()
}

fn method(line: &[u8]) -> String {
    serde_json::from_slice::<Value>(line)
        .ok()
        .and_then(|v| v["method"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn call(d: &mut CodexDriver, c: &Value, l: &Launch, out: &mut Out) {
    match c["call"].as_str().unwrap() {
        "on_start" => d.on_start(l, out),
        "prompt" => {
            let how: PromptHow = serde_json::from_value(c["how"].clone()).unwrap();
            d.prompt(c["text"].as_str().unwrap(), how, out).unwrap()
        }
        "interrupt" => d.interrupt(out).unwrap(),
        "answer" => {
            let a: Answer = serde_json::from_value(c["answer"].clone()).unwrap();
            d.answer(c["request_id"].as_str().unwrap(), &a, out)
                .unwrap()
        }
        "read_rate_limits" => d.read_rate_limits(out),
        other => panic!("unknown call {other}"),
    }
}

/// The first `n` lines of a case through a fresh driver, with the recorded calls (those made
/// after at most `n` lines) at their stream positions. Returns the driver, its events and what it wrote (each line with the number of
/// stdout lines seen before it was written).
fn run(case: &Case, n: usize) -> (CodexDriver, Vec<AgentEvent>, Vec<(u64, Value)>) {
    let l = launch(&case.calls);
    let calls = case.calls["calls"].as_array().unwrap();
    let lines = spool_lines(&case.spool);
    let (mut d, mut events, mut wrote, mut out) =
        (CodexDriver::new(), Vec::new(), Vec::new(), Out::default());
    let mut next = 0;
    for i in 0..=n {
        while next < calls.len() && calls[next]["at"].as_u64().unwrap() <= i as u64 {
            call(&mut d, &calls[next], &l, &mut out);
            wrote.extend(written(&mut out).into_iter().map(|v| (i as u64, v)));
            next += 1;
        }
        if let Some((off, line)) = lines.get(i).filter(|_| i < n) {
            events.extend(d.on_line(*off, line, &mut out));
            wrote.extend(written(&mut out).into_iter().map(|v| (i as u64 + 1, v)));
        }
    }
    (d, events, wrote)
}

fn check(name: &str) {
    let case = load(name);
    let n = spool_lines(&case.spool).len();
    let (mut d, mut events, wrote) = run(&case, n);
    let calls = case.calls["calls"].as_array().unwrap();
    assert!(
        calls.iter().all(|c| c["at"].as_u64().unwrap() <= n as u64),
        "{name}: call after the last line"
    );
    let want: Vec<(u64, Value)> = case.calls["stdin_at"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_u64().unwrap())
        .zip(case.stdin.iter().cloned())
        .collect();
    assert_eq!(
        wrote, want,
        "{name}: stdin (line, position) differs from the capture"
    );
    assert_eq!(
        d.resume_offset(),
        case.spool.len() as u64,
        "{name}: nothing is left open at the end"
    );
    events.extend(d.on_exit(case.calls["exit"].as_i64().map(|c| c as i32)));
    assert!(matches!(events.last(), Some(AgentEvent::Exited { .. })));
    golden(
        &fixture_dir("codex").join(format!("{name}.events.json")),
        &events,
    );
    // after a daemon restart a fresh driver replays the spool: same thread, never a write
    let mut fresh = CodexDriver::new();
    let (_, w) = replay(&mut fresh, &case.spool);
    assert!(
        w.is_empty(),
        "{name}: replay wrote {}",
        String::from_utf8_lossy(&w)
    );
    assert_eq!(
        fresh.agent_session_id(),
        d.agent_session_id(),
        "{name}: thread id after replay"
    );
}

macro_rules! golden_cases {
    ($($name:ident),*) => {$(
        #[test]
        fn $name() {
            check(stringify!($name));
        }
    )*};
}
golden_cases!(
    ok,
    tool,
    question,
    interrupt,
    steer,
    queue,
    resume,
    rate_limits
);

#[test]
fn prompt_before_the_thread_is_ready_waits() {
    let case = load("ok");
    let lines = spool_lines(&case.spool);
    let (mut d, mut out) = (CodexDriver::new(), Out::default());
    d.on_start(&launch(&case.calls), &mut out);
    d.prompt("hi", PromptHow::Queue, &mut out).unwrap();
    let methods = |v: Vec<Value>| {
        v.iter()
            .map(|v| v["method"].as_str().unwrap_or("").to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(methods(written(&mut out)), ["initialize"]);
    d.on_line(lines[0].0, lines[0].1, &mut out);
    assert_eq!(methods(written(&mut out)), ["initialized", "thread/start"]);
    d.on_line(lines[1].0, lines[1].1, &mut out);
    let w = written(&mut out);
    assert_eq!(methods(w.clone()), ["turn/start"]);
    assert_eq!(w[0]["params"]["input"][0]["text"], "hi");
    assert_eq!(w[0]["params"]["threadId"], d.agent_session_id().unwrap());
}

#[test]
fn queued_prompt_waits_for_turn_completed() {
    let case = load("queue");
    let lines = spool_lines(&case.spool);
    let (_, events, wrote) = run(&case, lines.len());
    let first_done = lines
        .iter()
        .position(|(_, l)| method(l) == "turn/completed")
        .unwrap();
    let starts: Vec<u64> = wrote
        .iter()
        .filter(|(_, v)| v["method"] == "turn/start")
        .map(|(at, _)| *at)
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(
        starts[1],
        first_done as u64 + 1,
        "second turn/start right after the first turn/completed"
    );
    let ends = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                AgentEvent::TurnEnded {
                    stop: StopReason::EndTurn,
                    ..
                }
            )
        })
        .count();
    assert_eq!(ends, 2);
}

#[test]
fn restart_mid_turn_resumes_at_turn_start() {
    let case = load("ok");
    let lines = spool_lines(&case.spool);
    let started = lines
        .iter()
        .position(|(_, l)| method(l) == "turn/started")
        .unwrap();
    let delta = lines
        .iter()
        .position(|(_, l)| method(l) == "item/agentMessage/delta")
        .unwrap();
    let (live, _, _) = run(&case, delta + 1);
    assert_eq!(live.resume_offset(), lines[started].0);
    // a fresh driver replays the full spool from there
    let mut d = CodexDriver::new();
    let (events, w) = replay_from(&mut d, &case.spool, live.resume_offset());
    assert!(w.is_empty());
    assert_eq!(d.agent_session_id(), live.agent_session_id());
    assert!(matches!(events[0], AgentEvent::TurnStarted));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ItemCompleted { kind: ItemKind::AssistantMessage, body, .. } if body["text"] == "ok")));
    assert!(matches!(
        events.iter().rev().nth(1),
        Some(AgentEvent::TurnEnded {
            stop: StopReason::EndTurn,
            usage: Some(_),
            ..
        })
    ));
    assert_eq!(d.resume_offset(), case.spool.len() as u64);
}

/// Feed lines starting at `from` (absolute offsets kept).
fn replay_from(d: &mut CodexDriver, spool: &[u8], from: u64) -> (Vec<AgentEvent>, Vec<u8>) {
    let mut out = Out::default();
    let events = spool_lines(spool)
        .into_iter()
        .filter(|(o, _)| *o >= from)
        .flat_map(|(o, l)| d.on_line(o, l, &mut out))
        .collect();
    (events, out.stdin)
}

#[test]
fn restart_with_a_pending_question_can_still_answer_it() {
    let case = load("question");
    let lines = spool_lines(&case.spool);
    let req = lines
        .iter()
        .position(|(_, l)| method(l) == "item/tool/requestUserInput")
        .unwrap();
    let cut = (lines[req].0 as usize) + lines[req].1.len() + 1;
    let mut before = CodexDriver::new();
    replay(&mut before, &case.spool[..cut]);
    let started = lines
        .iter()
        .position(|(_, l)| method(l) == "turn/started")
        .unwrap();
    assert_eq!(before.resume_offset(), lines[started].0);
    let mut d = CodexDriver::new();
    let (events, w) = replay_from(&mut d, &case.spool[..cut], before.resume_offset());
    assert!(w.is_empty());
    let q = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::QuestionAsked { q, .. } => Some(q.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(d.status(), AgentStatus::Blocked);
    let mut out = Out::default();
    d.answer(&q.request_id, &Answer::Option(0), &mut out)
        .unwrap();
    let answer = case
        .stdin
        .iter()
        .find(|v| v.get("result").is_some())
        .unwrap();
    assert_eq!(
        written(&mut out),
        std::slice::from_ref(answer),
        "the original JSON-RPC id is answered"
    );
}

#[test]
fn exit_mid_turn_closes_everything() {
    let case = load("ok");
    let lines = spool_lines(&case.spool);
    let delta = lines
        .iter()
        .position(|(_, l)| method(l) == "item/agentMessage/delta")
        .unwrap();
    let (mut d, _, _) = run(&case, delta + 1);
    let ev = d.on_exit(Some(1));
    assert!(
        matches!(&ev[0], AgentEvent::ItemCompleted { kind: ItemKind::AssistantMessage, status: ItemStatus::Failed, body, .. } if body["text"] == "ok")
    );
    assert!(matches!(
        ev[1],
        AgentEvent::TurnEnded {
            stop: StopReason::Interrupted,
            ..
        }
    ));
    assert_eq!(ev[2], AgentEvent::Status(AgentStatus::Idle));
    assert_eq!(ev[3], AgentEvent::Exited { code: Some(1) });
    assert_eq!(ev.len(), 4);
    let mut out = Out::default();
    assert!(d.interrupt(&mut out).is_err());
}

#[test]
fn methods_exist_in_the_schema() {
    let schema: Value =
        serde_json::from_slice(&std::fs::read(fixture_dir("codex").join("schema.json")).unwrap())
            .unwrap();
    let mut known = BTreeSet::new();
    collect_methods(&schema, &mut known);
    assert!(known.len() > 100, "schema.json has the method enums");
    for m in codex::SENT
        .iter()
        .chain(codex::HANDLED)
        .chain(codex::IGNORED)
        .chain(codex::OPT_OUT)
    {
        assert!(known.contains(*m), "{m} is not in schema.json");
    }
    for name in CASES {
        let case = load(name);
        let out: Vec<Value> = spool_lines(&case.spool)
            .iter()
            .filter_map(|(_, l)| serde_json::from_slice(l).ok())
            .collect();
        for v in case.stdin.iter().chain(&out) {
            if let Some(m) = v["method"].as_str() {
                assert!(known.contains(m), "{name}: {m} is not in schema.json");
            }
        }
    }
}

fn collect_methods(v: &Value, acc: &mut BTreeSet<String>) {
    match v {
        Value::Object(m) => {
            if let Some(e) = m
                .get("properties")
                .and_then(|p| p["method"]["enum"].as_array())
            {
                acc.extend(e.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            m.values().for_each(|x| collect_methods(x, acc));
        }
        Value::Array(a) => a.iter().for_each(|x| collect_methods(x, acc)),
        _ => {}
    }
}

// ---- synthetic: paths the captures do not reach (shapes from schema.json) ----

/// A driver past the handshake on thread `t1`, plus the next free offset.
fn ready() -> (CodexDriver, u64) {
    let (mut d, mut out) = (CodexDriver::new(), Out::default());
    d.on_start(
        &Launch {
            cwd: "/w".into(),
            ..Launch::default()
        },
        &mut out,
    );
    let mut off = 0;
    feed(&mut d, &mut off, json!({"id": 1, "result": {}}));
    feed(
        &mut d,
        &mut off,
        json!({"id": 2, "result": {"thread": {"id": "t1", "turns": []}, "model": "m"}}),
    );
    (d, off)
}

fn feed(d: &mut CodexDriver, off: &mut u64, v: Value) -> (Vec<AgentEvent>, Vec<Value>) {
    let line = serde_json::to_vec(&v).unwrap();
    let mut out = Out::default();
    let ev = d.on_line(*off, &line, &mut out);
    *off += line.len() as u64 + 1;
    (ev, written(&mut out))
}

fn turn_started(turn: &str) -> Value {
    json!({"method": "turn/started", "params": {"threadId": "t1", "turn": {"id": turn, "status": "inProgress"}}})
}

fn turn_completed(turn: &str, status: &str) -> Value {
    json!({"method": "turn/completed", "params": {"threadId": "t1", "turn": {"id": turn, "status": status, "error": null}}})
}

#[test]
fn steer_queue_and_interrupt() {
    let (mut d, mut off) = ready();
    let mut out = Out::default();
    // idle: Steer starts a turn; a second Steer before its id is known goes in as turn/start too
    d.prompt("a", PromptHow::Steer, &mut out).unwrap();
    d.prompt("b", PromptHow::Steer, &mut out).unwrap();
    let w = written(&mut out);
    assert_eq!(
        (w[0]["method"].as_str(), w[1]["method"].as_str()),
        (Some("turn/start"), Some("turn/start"))
    );
    let id = w[0]["id"].clone();
    // interrupt before the turn id is known waits for it
    d.interrupt(&mut out).unwrap();
    assert!(written(&mut out).is_empty());
    let (_, w) = feed(
        &mut d,
        &mut off,
        json!({"id": id, "result": {"turn": {"id": "u1"}}}),
    );
    assert_eq!(
        w,
        [
            json!({"id": w[0]["id"], "method": "turn/interrupt", "params": {"threadId": "t1", "turnId": "u1"}})
        ]
    );
    feed(&mut d, &mut off, turn_started("u1"));
    // running: Steer is turn/steer; if the turn ends first, the text runs as the next turn
    d.prompt("c", PromptHow::Steer, &mut out).unwrap();
    let w = written(&mut out);
    assert_eq!(w[0]["method"], "turn/steer");
    assert_eq!(w[0]["params"]["expectedTurnId"], "u1");
    let (ev, _) = feed(&mut d, &mut off, turn_completed("u1", "interrupted"));
    assert!(matches!(
        ev[0],
        AgentEvent::TurnEnded {
            stop: StopReason::Interrupted,
            ..
        }
    ));
    let (_, w) = feed(
        &mut d,
        &mut off,
        json!({"id": w[0]["id"], "error": {"code": -32600, "message": "no active turn"}}),
    );
    assert_eq!(
        (
            w[0]["method"].as_str(),
            w[0]["params"]["input"][0]["text"].as_str()
        ),
        (Some("turn/start"), Some("c"))
    );
    // InterruptThenSend: turn/interrupt now, the text after turn/completed
    feed(
        &mut d,
        &mut off,
        json!({"id": w[0]["id"], "result": {"turn": {"id": "u2"}}}),
    );
    feed(&mut d, &mut off, turn_started("u2"));
    d.prompt("d", PromptHow::InterruptThenSend, &mut out)
        .unwrap();
    assert_eq!(
        written(&mut out)[0]["params"],
        json!({"threadId": "t1", "turnId": "u2"})
    );
    let (_, w) = feed(&mut d, &mut off, turn_completed("u2", "interrupted"));
    assert_eq!(w[0]["params"]["input"][0]["text"], "d");
    // idle again (after u3): nothing to stop, so interrupt is an Ok that writes nothing
    feed(
        &mut d,
        &mut off,
        json!({"id": w[0]["id"], "result": {"turn": {"id": "u3"}}}),
    );
    feed(&mut d, &mut off, turn_started("u3"));
    feed(&mut d, &mut off, turn_completed("u3", "completed"));
    assert_eq!(d.interrupt(&mut out), Ok(()));
    assert!(out.stdin.is_empty());
}

#[test]
fn set_model_rides_on_the_next_turn() {
    let (mut d, mut off) = ready();
    let mut out = Out::default();
    d.set_model("gpt-x", &mut out).unwrap();
    assert!(written(&mut out).is_empty());
    d.prompt("a", PromptHow::Queue, &mut out).unwrap();
    let w = written(&mut out);
    assert_eq!(w[0]["params"]["model"], "gpt-x");
    let (ev, _) = feed(
        &mut d,
        &mut off,
        json!({"id": w[0]["id"], "result": {"turn": {"id": "u1"}}}),
    );
    assert_eq!(ev, [AgentEvent::ModelChanged("gpt-x".into())]);
}

#[test]
fn synthetic_multi_question_request() {
    let (mut d, mut off) = ready();
    feed(&mut d, &mut off, turn_started("u1"));
    let req = json!({"method": "item/tool/requestUserInput", "id": 7, "params": {"threadId": "t1", "turnId": "u1",
        "itemId": "call_1", "isBlocking": true, "questions": [
        {"id": "a", "header": "A", "question": "Pick?", "isOther": false, "isSecret": false,
         "options": [{"label": "x", "description": ""}, {"label": "y", "description": ""}]},
        {"id": "b", "header": "B", "question": "Name?", "isOther": true, "isSecret": false, "options": null}]}});
    let (ev, w) = feed(&mut d, &mut off, req);
    assert!(w.is_empty());
    let qs: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            AgentEvent::QuestionAsked { q, .. } => Some(q.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        qs.iter().map(|q| q.request_id.as_str()).collect::<Vec<_>>(),
        ["call_1#0", "call_1#1"]
    );
    assert_eq!(
        (qs[0].options.clone(), qs[0].free_text, qs[1].free_text),
        (vec!["x".to_owned(), "y".to_owned()], false, true)
    );
    assert_eq!(ev.last(), Some(&AgentEvent::Status(AgentStatus::Blocked)));
    let mut out = Out::default();
    assert!(matches!(
        d.answer("call_1#0", &Answer::Text("z".into()), &mut out),
        Err(Error::Bad(_))
    ));
    d.answer("call_1#0", &Answer::Option(1), &mut out).unwrap();
    assert!(
        written(&mut out).is_empty(),
        "waits for every question of the request"
    );
    d.answer("call_1#1", &Answer::Text("bob".into()), &mut out)
        .unwrap();
    assert_eq!(
        written(&mut out),
        [
            json!({"id": 7, "result": {"answers": {"a": {"answers": ["y"]}, "b": {"answers": ["bob"]}}}})
        ]
    );
    assert!(matches!(
        d.answer("call_1#1", &Answer::Cancel, &mut out),
        Err(Error::UnknownRequest(_))
    ));
    let (ev, _) = feed(
        &mut d,
        &mut off,
        json!({"method": "serverRequest/resolved", "params": {"threadId": "t1", "requestId": 7}}),
    );
    assert_eq!(
        ev,
        [
            AgentEvent::QuestionResolved {
                request_id: "call_1#0".into()
            },
            AgentEvent::QuestionResolved {
                request_id: "call_1#1".into()
            },
            AgentEvent::Status(AgentStatus::Working),
        ]
    );
}

#[test]
fn synthetic_approval_request_is_a_question() {
    let (mut d, mut off) = ready();
    feed(&mut d, &mut off, turn_started("u1"));
    let req = json!({"method": "item/commandExecution/requestApproval", "id": 3, "params": {"threadId": "t1", "turnId": "u1",
        "itemId": "exec-1", "approvalId": null, "command": "rm -rf build", "reason": "cleanup", "startedAtMs": 1}});
    let (ev, _) = feed(&mut d, &mut off, req);
    let AgentEvent::QuestionAsked { item_id, q } = &ev[0] else {
        panic!("{ev:?}")
    };
    assert_eq!(
        (item_id.as_str(), q.options.clone()),
        (
            "exec-1:approval",
            vec!["Accept".to_owned(), "Decline".to_owned()]
        )
    );
    assert_eq!(q.prompt, "Allow Codex to run `rm -rf build`?\ncleanup");
    let mut out = Out::default();
    d.answer("exec-1:approval", &Answer::Option(1), &mut out)
        .unwrap();
    assert_eq!(
        written(&mut out),
        [json!({"id": 3, "result": {"decision": "decline"}})]
    );
}

#[test]
fn synthetic_failed_resume_starts_a_new_thread() {
    let (mut d, mut out) = (CodexDriver::new(), Out::default());
    let l = Launch {
        cwd: "/w".into(),
        resume: Some("gone".into()),
        ..Launch::default()
    };
    d.on_start(&l, &mut out);
    d.prompt("hi", PromptHow::Queue, &mut out).unwrap();
    written(&mut out);
    let mut off = 0;
    let (_, w) = feed(&mut d, &mut off, json!({"id": 1, "result": {}}));
    assert_eq!(
        w[1]["params"],
        json!({"threadId": "gone", "cwd": "/w", "approvalPolicy": "never", "sandbox": "danger-full-access"})
    );
    let (ev, w) = feed(
        &mut d,
        &mut off,
        json!({"id": 2, "error": {"code": -32600, "message": "no rollout found"}}),
    );
    assert!(matches!(
        &ev[0],
        AgentEvent::ItemCompleted {
            kind: ItemKind::Event,
            status: ItemStatus::Failed,
            ..
        }
    ));
    assert_eq!(
        (w[0]["method"].as_str(), w[0]["params"].get("threadId")),
        (Some("thread/start"), None)
    );
    let (ev, w) = feed(
        &mut d,
        &mut off,
        json!({"id": w[0]["id"], "result": {"thread": {"id": "t2"}, "model": "m"}}),
    );
    assert_eq!(
        ev[0],
        AgentEvent::Started {
            agent_sid: "t2".into(),
            model: Some("m".into())
        }
    );
    assert_eq!(
        w[0]["params"]["threadId"], "t2",
        "the buffered prompt goes to the new thread"
    );
}

#[test]
fn synthetic_rate_limit_windows() {
    let (mut d, mut off) = ready();
    let snap = |limit: &str| {
        json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {"limitId": limit,
        "primary": {"usedPercent": 12.5, "windowDurationMins": 300, "resetsAt": 100},
        "secondary": {"usedPercent": 40, "windowDurationMins": 10080, "resetsAt": null}}}})
    };
    let (ev, _) = feed(&mut d, &mut off, snap("codex"));
    assert_eq!(
        ev,
        [
            AgentEvent::RateLimit {
                kind: "session".into(),
                percent: 12.5,
                resets_at: Some(100)
            },
            AgentEvent::RateLimit {
                kind: "weekly".into(),
                percent: 40.0,
                resets_at: None
            },
        ]
    );
    let (ev, _) = feed(&mut d, &mut off, snap("codex_other"));
    assert!(matches!(&ev[0], AgentEvent::RateLimit { kind, .. } if kind == "codex_other:session"));
}

#[test]
fn reattached_driver_finds_its_thread() {
    // replay from the end of an idle spool: nothing seen, so ask which thread is loaded
    let mut d = CodexDriver::new();
    let mut out = Out::default();
    let mut off = 5000;
    d.prompt("hi", PromptHow::Queue, &mut out).unwrap();
    let w = written(&mut out);
    assert_eq!(w[0]["method"], "thread/loaded/list");
    let (_, w2) = feed(
        &mut d,
        &mut off,
        json!({"id": w[0]["id"], "result": {"data": ["t9", "t2"], "nextCursor": null}}),
    );
    assert_eq!(w2[0]["method"], "turn/start");
    assert_eq!(
        w2[0]["params"]["threadId"], "t2",
        "the oldest (UUIDv7) thread is the session's own"
    );
    // ids of a re-attached driver start past the spool offset seen, away from the old driver's
    assert!(w2[0]["id"].as_u64().unwrap() > 5000);
}

#[test]
fn subagent_threads_are_ignored() {
    let (mut d, mut off) = ready();
    let item = json!({"method": "item/completed", "params": {"threadId": "other", "turnId": "x",
        "item": {"type": "agentMessage", "id": "m1", "text": "hi"}}});
    assert!(feed(&mut d, &mut off, item).0.is_empty());
    assert!(feed(&mut d, &mut off, turn_started("u1"))
        .0
        .contains(&AgentEvent::TurnStarted));
}

// ---- never panic ----

const KEYS: &[&str] = &[
    "id",
    "method",
    "params",
    "result",
    "error",
    "item",
    "type",
    "threadId",
    "turnId",
    "itemId",
    "turn",
    "thread",
    "status",
    "delta",
    "questions",
    "options",
    "label",
    "requestId",
    "tokenUsage",
    "last",
    "rateLimits",
    "primary",
    "secondary",
    "usedPercent",
    "windowDurationMins",
    "data",
    "message",
    "willRetry",
    "summaryIndex",
    "exitCode",
    "aggregatedOutput",
    "changes",
    "kind",
    "path",
    "content",
    "text",
    "summary",
    "model",
    "toModel",
    "plan",
    "step",
    "isOther",
    "turns",
    "items",
];
const WORDS: &[&str] = &[
    "userMessage",
    "agentMessage",
    "reasoning",
    "commandExecution",
    "fileChange",
    "mcpToolCall",
    "dynamicToolCall",
    "plan",
    "completed",
    "interrupted",
    "failed",
    "inProgress",
    "t1",
    "u1",
    "call_1",
    "add",
    "text",
];

fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        any::<u64>().prop_map(|n| json!(n)),
        any::<f64>().prop_map(|f| json!(f)),
        "\\PC{0,12}".prop_map(Value::String),
        prop::sample::select(WORDS).prop_map(|s| json!(s)),
    ];
    leaf.prop_recursive(5, 64, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map(
                prop::sample::select(KEYS).prop_map(str::to_owned),
                inner,
                0..7
            )
            .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

fn arb_message() -> impl Strategy<Value = Value> {
    let methods: Vec<&str> = codex::HANDLED
        .iter()
        .chain(codex::IGNORED)
        .copied()
        .chain(["zz/unknown"])
        .collect();
    (
        prop::sample::select(methods),
        arb_json(),
        prop::option::of(0u64..4),
        arb_json(),
    )
        .prop_map(|(m, params, id, extra)| {
            let mut v = json!({"method": m, "params": params});
            if let Some(id) = id {
                v["id"] = json!(id);
            }
            if let Value::Object(x) = extra {
                for (k, val) in x {
                    v.as_object_mut().unwrap().entry(k).or_insert(val);
                }
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
    Limits,
}

fn arb_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => arb_message().prop_map(|v| Step::Line(serde_json::to_vec(&v).unwrap())),
        3 => arb_json().prop_map(|v| Step::Line(serde_json::to_vec(&v).unwrap())),
        1 => prop::collection::vec(any::<u8>(), 0..64).prop_map(|l| Step::Line(one_line(l))),
        1 => (0u8..3).prop_map(Step::Prompt),
        1 => Just(Step::Interrupt),
        1 => (0u16..3).prop_map(Step::Answer),
        1 => Just(Step::Model),
        1 => Just(Step::Limits),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn never_panics(start in any::<bool>(), steps in prop::collection::vec(arb_step(), 0..40)) {
        let mut d = CodexDriver::new();
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
                Step::Model => { let _ = d.set_model("m", &mut out); }
                Step::Limits => d.read_rate_limits(&mut out),
            }
            for l in written(&mut out) {
                prop_assert!(l.is_object());
            }
        }
        let ev = d.on_exit(None);
        prop_assert_eq!(ev.last(), Some(&AgentEvent::Exited { code: None }));
        prop_assert_eq!(d.status(), AgentStatus::Idle);
    }

    #[test]
    fn unknown_messages_are_raw(m in "[a-z]{1,8}/zz[a-zA-Z/]{0,8}", params in arb_json(), id in prop::option::of(0u64..9)) {
        let (mut d, _) = ready();
        let mut v = json!({"method": m, "params": params});
        if let Some(id) = id {
            v["id"] = json!(id);
        }
        let mut out = Out::default();
        let line = serde_json::to_vec(&v).unwrap();
        let ev = d.on_line(10_000, &line, &mut out);
        // compare with the parsed line: float text round trips are not exact
        prop_assert_eq!(ev, vec![AgentEvent::Raw(serde_json::from_slice(&line).unwrap())]);
        // a request nobody serves is refused (method not found) so the agent stops waiting for it;
        // a notification gets nothing
        match id {
            Some(id) => prop_assert_eq!(
                written(&mut out),
                vec![json!({"id": id, "error": {"code": -32601, "message": format!("femme does not handle {m}")}})]
            ),
            None => prop_assert!(out.stdin.is_empty()),
        }
    }

    #[test]
    fn non_json_lines_are_raw_text(line in prop::collection::vec(any::<u8>(), 1..80).prop_map(one_line)) {
        let mut d = CodexDriver::new();
        let mut out = Out::default();
        let ev = d.on_line(0, &line, &mut out);
        let json = serde_json::from_slice::<Value>(line.strip_suffix(b"\r").unwrap_or(&line)).is_ok();
        let blank = line.iter().all(u8::is_ascii_whitespace);
        if !json && !blank {
            prop_assert!(matches!(ev.as_slice(), [AgentEvent::Raw(Value::String(_))]));
        }
        prop_assert!(out.stdin.is_empty());
    }
}
