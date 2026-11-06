//! Claude driver against the fixtures captured from claude 2.1.302 (`scripts/capture-claude.py`).
//! No agent runs here: every test replays recorded stdout through a fresh `ClaudeDriver`.
//!
//! Golden files `fixtures/claude/<case>.events.json` are written by the first run (or with
//! `UPDATE_GOLDEN=1`); review the diff by eye before accepting it.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use common::{fixture_dir, golden, replay, spool_lines};
use femme_agents::claude::ClaudeDriver;
use femme_agents::{AgentEvent, AgentStatus, Driver, Error, Launch, Out, StopReason};
use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::{ItemKind, ItemStatus};
use proptest::prelude::*;
use serde_json::{json, Value};

fn dir() -> PathBuf {
    fixture_dir("claude")
}

fn read(case: &str) -> Vec<u8> {
    std::fs::read(dir().join(format!("{case}.jsonl")))
        .unwrap_or_else(|e| panic!("fixture {case}.jsonl: {e}"))
}

fn stdin_lines(case: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(dir().join(format!("{case}.stdin.jsonl")))
        .unwrap_or_else(|e| panic!("{case}.stdin.jsonl: {e}"));
    text.lines()
        .map(|l| serde_json::from_str(l).expect("stdin fixture line is JSON"))
        .collect()
}

fn events(case: &str) -> Vec<AgentEvent> {
    replay(&mut ClaudeDriver::new(), &read(case)).0
}

fn launch() -> Launch {
    Launch {
        cwd: "/work/repo".into(),
        model: Some("claude-haiku-5-5".into()),
        ..Launch::default()
    }
}

/// What a driver wrote, as JSON values.
fn written(out: &Out) -> Vec<Value> {
    out.lines()
        .iter()
        .map(|l| serde_json::from_str(l).expect("driver wrote JSON"))
        .collect()
}

/// A message with its per-run random parts (`uuid`) removed, for comparing with captures.
fn no_uuid(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.remove("uuid");
    }
    v
}

macro_rules! golden_cases {
    ($($case:ident),* $(,)?) => {
        const CASES: &[&str] = &[$(stringify!($case)),*];
        $(
            #[test]
            fn $case() {
                let name = stringify!($case);
                let mut d = ClaudeDriver::new();
                let (ev, stdin) = replay(&mut d, &read(name));
                assert!(stdin.is_empty(), "{name}: a replaying driver must not write to stdin, wrote {:?}", String::from_utf8_lossy(&stdin));
                golden(&dir().join(format!("{name}.events.json")), &ev);
            }
        )*
    };
}

golden_cases!(
    ok,
    tool,
    question,
    question_multi,
    question_two,
    question_deny,
    plan,
    interrupt,
    ask_interrupt,
    queue,
    queue_text,
    set_model,
    set_model_bad,
    resume_1,
    resume_2,
    session_id,
    kill9_1,
    kill9_2,
    reinit,
    error_model,
    error_resume,
    usage,
);

#[test]
fn every_fixture_has_a_golden_test() {
    let mut on_disk: Vec<String> = std::fs::read_dir(dir())
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".jsonl") && !n.ends_with(".stdin.jsonl"))
        .map(|n| n.trim_end_matches(".jsonl").to_owned())
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = CASES.iter().map(|s| (*s).to_owned()).collect();
    listed.sort();
    assert_eq!(on_disk, listed, "add new fixtures to golden_cases!");
}

// ---- what the golden files must say (the reviewed intent) ------------------------------------

fn kinds(ev: &[AgentEvent]) -> Vec<String> {
    ev.iter()
        .map(|e| match e {
            AgentEvent::ItemStarted { kind, .. } => format!("start:{kind:?}"),
            AgentEvent::ItemDelta { .. } => "delta".into(),
            AgentEvent::ItemUpdated { .. } => "update".into(),
            AgentEvent::ItemCompleted { kind, status, .. } => format!("done:{kind:?}:{status:?}"),
            AgentEvent::QuestionAsked { .. } => "asked".into(),
            AgentEvent::QuestionResolved { .. } => "resolved".into(),
            AgentEvent::PromptDropped { .. } => "dropped".into(),
            AgentEvent::Status(s) => format!("status:{s:?}"),
            AgentEvent::TurnStarted => "turn+".into(),
            AgentEvent::TurnEnded { stop, .. } => format!("turn-:{stop:?}"),
            AgentEvent::Started { .. } => "started".into(),
            AgentEvent::ModelChanged(m) => format!("model:{m}"),
            AgentEvent::Commands(_) => "commands".into(),
            AgentEvent::PermissionMode(m) => format!("permission:{m}"),
            AgentEvent::RateLimit { kind, .. } => format!("rl:{kind}"),
            AgentEvent::Exited { .. } => "exited".into(),
            AgentEvent::Raw(_) => "raw".into(),
        })
        .collect()
}

fn count(ev: &[AgentEvent], what: &str) -> usize {
    kinds(ev).iter().filter(|k| k.as_str() == what).count()
}

#[test]
fn plain_turn_has_one_turn_and_one_text_item() {
    let ev = events("ok");
    let k = kinds(&ev);

    assert_eq!(&k[..3], ["status:Working", "turn+", "started"], "{k:?}");
    assert_eq!(count(&ev, "start:AssistantMessage"), 1);
    assert_eq!(count(&ev, "done:AssistantMessage:Completed"), 1);
    assert_eq!(
        count(&ev, "done:UserMessage:Completed"),
        1,
        "the echo of our prompt is the UserMessage item"
    );
    let n = k.len();
    assert_eq!(&k[n - 2..], ["turn-:EndTurn", "status:Idle"], "{k:?}");
    let Some(AgentEvent::TurnEnded {
        usage: Some(u),
        duration_ms: Some(d),
        ..
    }) = ev
        .iter()
        .find(|e| matches!(e, AgentEvent::TurnEnded { .. }))
    else {
        panic!("TurnEnded without usage")
    };
    assert!(u.output > 0 && u.cache_write + u.cache_read > 0 && *d > 0);
    let Some(AgentEvent::RateLimit {
        kind,
        percent,
        resets_at,
    }) = ev
        .iter()
        .find(|e| matches!(e, AgentEvent::RateLimit { .. }))
    else {
        panic!("no rate limit event")
    };
    assert_eq!(kind, "session");
    assert!((0.0..=100.0).contains(percent) && resets_at.is_some());
}

#[test]
fn tool_call_and_result_are_linked_items() {
    let ev = events("tool");
    let calls: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ItemCompleted {
                id,
                kind: ItemKind::ToolCall,
                status,
                body,
                ..
            } => Some((id, status, body)),
            _ => None,
        })
        .collect();
    let results: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ItemCompleted {
                id,
                kind: ItemKind::ToolResult,
                body,
                ..
            } => Some((id, body)),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(results.len(), 1);
    let (cid, status, cbody) = calls[0];
    assert_eq!(*status, ItemStatus::Completed);
    assert_eq!(cbody["name"], "Bash");
    assert_eq!(cbody["input"]["command"], "echo hi");
    assert_eq!(cbody["call_id"], json!(cid));
    assert_eq!(results[0].0, &format!("{cid}:result"));
    assert_eq!(results[0].1["call_id"], json!(cid));
    assert_eq!(results[0].1["output"], "hi");
}

#[test]
fn ask_user_question_becomes_a_question_not_a_tool_call() {
    let ev = events("question");
    let k = kinds(&ev);
    assert_eq!(count(&ev, "asked"), 1);
    assert_eq!(count(&ev, "resolved"), 1);
    assert!(
        !k.iter().any(|x| x.contains("ToolCall")),
        "AskUserQuestion must not show up as a ToolCall: {k:?}"
    );
    let AgentEvent::QuestionAsked { item_id, q } = ev
        .iter()
        .find(|e| matches!(e, AgentEvent::QuestionAsked { .. }))
        .unwrap()
    else {
        unreachable!()
    };
    assert!(item_id.starts_with("toolu_"));
    assert_eq!(q.options.len(), 2);
    assert!(
        q.options[0].starts_with("A \u{2014} "),
        "labels carry their description: {:?}",
        q.options
    );
    assert!(!q.multi && q.free_text);
    // Blocked while the question is open, working again once answered, in that order
    let st: Vec<&str> = k
        .iter()
        .filter(|x| x.starts_with("status:") || *x == "asked" || *x == "resolved")
        .map(String::as_str)
        .collect();
    assert_eq!(
        st,
        [
            "status:Working",
            "asked",
            "status:Blocked",
            "resolved",
            "status:Working",
            "status:Idle"
        ],
        "{k:?}"
    );
    // the Question item is completed with the answer the CLI recorded
    let done = ev
        .iter()
        .find_map(|e| match e {
            AgentEvent::ItemCompleted {
                id,
                kind: ItemKind::Question,
                status,
                body,
                ..
            } if id == item_id => Some((status, body)),
            _ => None,
        })
        .expect("question completed");
    assert_eq!(*done.0, ItemStatus::Answered);
    assert_eq!(done.1["answered"], json!({"Option": 1}));
}

#[test]
fn multi_select_and_two_questions() {
    let ev = events("question_multi");
    let AgentEvent::QuestionAsked { q, .. } = ev
        .iter()
        .find(|e| matches!(e, AgentEvent::QuestionAsked { .. }))
        .unwrap()
    else {
        unreachable!()
    };
    assert!(q.multi);
    let done = ev
        .iter()
        .find_map(|e| match e {
            AgentEvent::ItemCompleted {
                kind: ItemKind::Question,
                body,
                ..
            } => Some(body),
            _ => None,
        })
        .unwrap();
    assert_eq!(done["answered"], json!({"Options": [0, 1]}));

    let ev = events("question_two");
    assert_eq!(count(&ev, "asked"), 2, "one QuestionAsked per question");
    let ids: Vec<(&String, &String)> = ev
        .iter()
        .filter_map(|e| match e {
            AgentEvent::QuestionAsked { item_id, q } => Some((item_id, &q.request_id)),
            _ => None,
        })
        .collect();
    assert!(
        ids[0].0.ends_with("#0")
            && ids[1].0.ends_with("#1")
            && ids[0].1.ends_with("#0")
            && ids[1].1.ends_with("#1"),
        "{ids:?}"
    );
    assert_eq!(count(&ev, "done:Question:Answered"), 2);
}

#[test]
fn denied_question_is_answered_cancel() {
    let ev = events("question_deny");
    let done = ev
        .iter()
        .find_map(|e| match e {
            AgentEvent::ItemCompleted {
                kind: ItemKind::Question,
                status,
                body,
                ..
            } => Some((status, body)),
            _ => None,
        })
        .unwrap();
    assert_eq!(*done.0, ItemStatus::Answered);
    assert_eq!(done.1["answered"], json!("Cancel"));
}

#[test]
fn plan_mode_shows_a_plan_and_asks_for_approval() {
    let ev = events("plan");
    let plan = ev
        .iter()
        .find_map(|e| match e {
            AgentEvent::ItemCompleted {
                id,
                kind: ItemKind::Plan,
                body,
                ..
            } => Some((id, body)),
            _ => None,
        })
        .expect("Plan item");
    assert_eq!(plan.1["text"], "create hello.txt\n");
    let AgentEvent::QuestionAsked { item_id, q } = ev
        .iter()
        .find(|e| matches!(e, AgentEvent::QuestionAsked { .. }))
        .expect("approval question")
    else {
        unreachable!()
    };
    assert_eq!(item_id, &format!("{}:ask", plan.0));
    assert_eq!(q.options, ["Approve", "Keep planning"]);
}

#[test]
fn interrupt_ends_the_turn_as_interrupted_and_the_session_keeps_working() {
    let ev = events("interrupt");
    let ends: Vec<&StopReason> = ev
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TurnEnded { stop, .. } => Some(stop),
            _ => None,
        })
        .collect();
    assert_eq!(ends, [&StopReason::Interrupted, &StopReason::EndTurn]);
    assert!(
        kinds(&ev).iter().any(|k| k == "done:Event:Completed"),
        "the interrupt marker is an Event item"
    );
}

#[test]
fn interrupt_while_a_question_is_pending_resolves_it() {
    let ev = events("ask_interrupt");
    let k = kinds(&ev);
    let asked = k.iter().position(|x| x == "asked").unwrap();
    let resolved = k
        .iter()
        .position(|x| x == "resolved")
        .expect("control_cancel_request resolves the question");
    assert!(asked < resolved);
    assert!(k.contains(&"turn-:Interrupted".to_owned()), "{k:?}");
    // nobody answered it (the user interrupted): the item fails, with the CLI's words as the reason
    assert_eq!(count(&ev, "done:Question:Answered"), 0, "{k:?}");
    let done = ev
        .iter()
        .find_map(|e| match e {
            AgentEvent::ItemCompleted {
                kind: ItemKind::Question,
                status,
                body,
                ..
            } => Some((status, body)),
            _ => None,
        })
        .unwrap();
    assert_eq!(*done.0, ItemStatus::Failed);
    assert!(done.1["answered"].is_null());
    assert!(
        done.1["reason"].as_str().unwrap().contains("rejected"),
        "{}",
        done.1
    );
}

#[test]
fn queue_folds_into_the_running_turn_but_text_turns_run_one_after_the_other() {
    // tool turn: the second message joins the running turn: one TurnEnded, two user messages
    let ev = events("queue");
    assert_eq!(count(&ev, "turn-:EndTurn"), 1, "{:?}", kinds(&ev));
    assert_eq!(count(&ev, "done:UserMessage:Completed"), 2);
    // text-only turn: two turns, and the status never drops to Idle in between
    let ev = events("queue_text");
    let k = kinds(&ev);
    assert_eq!(count(&ev, "turn-:EndTurn"), 2, "{k:?}");
    let first_end = k.iter().position(|x| x == "turn-:EndTurn").unwrap();
    let second_start = first_end + k[first_end..].iter().position(|x| x == "turn+").unwrap();
    assert!(
        !k[first_end..second_start].contains(&"status:Idle".to_owned()),
        "no Idle blip between queued turns: {k:?}"
    );
}

#[test]
fn set_model_is_reported_once() {
    let ev = events("set_model");
    let models: Vec<&String> = ev
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ModelChanged(m) => Some(m),
            _ => None,
        })
        .collect();
    assert_eq!(models, ["claude-haiku-4-5-20251001"], "{:?}", kinds(&ev));
    // a rejected control request shows up as an Event item, not a panic
    let ev = events("set_model_bad");
    assert_eq!(count(&ev, "done:Event:Completed"), 1, "{:?}", kinds(&ev));
}

#[test]
fn resume_keeps_the_session_id() {
    let sid = |case: &str| {
        events(case)
            .into_iter()
            .find_map(|e| match e {
                AgentEvent::Started { agent_sid, .. } => Some(agent_sid),
                _ => None,
            })
            .expect("Started")
    };
    assert_eq!(sid("resume_1"), sid("resume_2"));
}

#[test]
fn session_id_flag_is_honoured() {
    let cmd: Value =
        serde_json::from_slice(&std::fs::read(dir().join("session_id.cmd.json")).unwrap()).unwrap();
    let want = cmd["argv"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|a| a.as_str()?.strip_prefix("--session-id="))
        .expect("--session-id in argv")
        .to_owned();
    let got = events("session_id")
        .into_iter()
        .find_map(|e| match e {
            AgentEvent::Started { agent_sid, .. } => Some(agent_sid),
            _ => None,
        })
        .unwrap();
    assert_eq!(got, want);
}

#[test]
fn api_error_ends_the_turn_and_failed_resume_says_so_with_the_cli_message() {
    let ev = events("error_model");
    let Some(AgentEvent::TurnEnded {
        stop: StopReason::Error(m),
        ..
    }) = ev
        .iter()
        .find(|e| matches!(e, AgentEvent::TurnEnded { .. }))
    else {
        panic!("{:?}", kinds(&ev))
    };
    assert!(m.contains("claude-nonexistent-9"), "{m}");
    // resuming a session the CLI does not know fails before any turn begins: an Event item, and
    // no TurnStarted/TurnEnded pair made up to carry the message (finding 11)
    let ev = events("error_resume");
    assert_eq!(kinds(&ev), ["done:Event:Completed"], "{:?}", kinds(&ev));
    let AgentEvent::ItemCompleted { body, .. } = &ev[0] else {
        unreachable!()
    };
    assert!(
        body["text"]
            .as_str()
            .unwrap()
            .contains("No conversation found"),
        "{body}"
    );
}

#[test]
fn resumed_session_after_kill9_is_one_clean_turn() {
    // (the empty bookkeeping `result` a lost background task can leave is a unit test in claude.rs)
    let ev = events("kill9_2");
    assert_eq!(count(&ev, "turn+"), 1, "{:?}", kinds(&ev));
    assert_eq!(count(&ev, "turn-:EndTurn"), 1);
}

#[test]
fn usage_command_leaves_no_turn_behind() {
    // `/usage` is a local command: its fixture has an assistant message but no model turn
    let ev = events("usage");
    assert!(
        !kinds(&ev).iter().any(|k| k.starts_with("turn-:Error")),
        "{:?}",
        kinds(&ev)
    );
}

// ---- what we write to stdin matches what the captures sent -----------------------------------

#[test]
fn spawn_line_matches_the_capture() {
    for (case, resume) in [("ok", None), ("resume_2", Some(()))] {
        let cmd: Value =
            serde_json::from_slice(&std::fs::read(dir().join(format!("{case}.cmd.json"))).unwrap())
                .unwrap();
        let want: Vec<String> = cmd["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_owned())
            .collect();
        let mut l = launch();
        if resume.is_some() {
            let id = want
                .iter()
                .find_map(|a| a.strip_prefix("--resume="))
                .expect("--resume= in the capture")
                .to_owned();
            l.resume = Some(id);
        }
        let sp = ClaudeDriver::new().spawn(&l);
        let strip = |v: &[String]| {
            v.iter()
                .filter(|a| !a.starts_with("--session-id="))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            strip(&sp.cmd),
            strip(&want),
            "{case}: driver and capture script disagree about the command line"
        );
        assert_eq!(sp.cwd, "/work/repo");
        assert!(sp.env.contains(&(
            "CLAUDE_CODE_SDK_READS_SESSION_STATE".to_owned(),
            "1".to_owned()
        )));
        if resume.is_none() {
            assert!(
                sp.cmd.iter().any(|a| a.starts_with("--session-id=")),
                "new sessions get a session id"
            );
        }
    }
    let tui = ClaudeDriver::new()
        .tui(&Launch {
            resume: Some("abc".into()),
            hooks_path: Some("/h.json".into()),
            ..launch()
        })
        .unwrap();
    assert_eq!(
        tui.cmd,
        [
            "claude",
            "--dangerously-skip-permissions",
            "--resume=abc",
            "--model=claude-haiku-5-5",
            "--settings=/h.json"
        ]
    );
}

#[test]
fn handshake_interrupt_and_set_model_match_the_capture() {
    let mut d = ClaudeDriver::new();
    let mut out = Out::default();
    d.on_start(&launch(), &mut out);
    assert_eq!(written(&out), [stdin_lines("ok")[0].clone()], "initialize");
    assert!(
        d.agent_session_id().is_some(),
        "known from on_start with --session-id"
    );
    // the prompt: same shape as the capture, only the uuid is ours
    let mut out = Out::default();
    let mut ev = Vec::new();
    for (off, line) in spool_lines(&read("ok")).into_iter().take(11) {
        ev.extend(d.on_line(off, line, &mut out));
    }
    out.take();
    d.prompt("Reply with the single word ok.", PromptHow::Queue, &mut out)
        .unwrap();
    let sent = written(&out);
    assert!(
        sent[0]["uuid"].as_str().is_some_and(|u| u.len() == 36),
        "{:?}",
        sent[0]
    );
    assert_eq!(
        no_uuid(sent[0].clone()),
        no_uuid(stdin_lines("ok")[1].clone())
    );

    let mut d = ClaudeDriver::new();
    let (mut out, mut ev) = (Out::default(), Vec::new());
    for (off, line) in spool_lines(&read("set_model")) {
        ev.extend(d.on_line(off, line, &mut out));
    }
    let mut out = Out::default();
    d.set_model("claude-haiku-4-5-20251001", &mut out).unwrap();
    let sent = stdin_lines("set_model");
    assert_eq!(
        written(&out),
        [sent
            .iter()
            .find(|m| m["request"]["subtype"] == "set_model")
            .unwrap()
            .clone()]
    );

    // an interrupt is only written while a turn runs (an idle one is a no-op, see
    // `interrupt_is_a_no_op_unless_a_turn_runs`), so take the capture to a running turn first
    let mut d = ClaudeDriver::new();
    let mut out = Out::default();
    feed_until(&mut d, &read("interrupt"), &mut out, |v| {
        is_lifecycle(v, "started")
    });
    d.interrupt(&mut out).unwrap();
    let int = stdin_lines("interrupt");
    let want = int
        .iter()
        .find(|m| m["request"]["subtype"] == "interrupt")
        .unwrap();
    assert_eq!(written(&out)[0]["request"], want["request"]);
}

/// Feed lines until `stop` returns true for a line (inclusive); returns the offset after it.
fn feed_until(
    d: &mut ClaudeDriver,
    bytes: &[u8],
    out: &mut Out,
    stop: impl Fn(&Value) -> bool,
) -> u64 {
    for (off, line) in spool_lines(bytes) {
        d.on_line(off, line, out);
        if serde_json::from_slice::<Value>(line)
            .map(|v| stop(&v))
            .unwrap_or(false)
        {
            return off + line.len() as u64 + 1;
        }
    }
    panic!("stop line not found");
}

fn is_init_response(v: &Value) -> bool {
    v["type"] == "control_response" && v["response"]["request_id"] == "femme-init"
}

fn is_lifecycle(v: &Value, state: &str) -> bool {
    v["type"] == "command_lifecycle" && v["state"] == state
}

fn texts(out: &Out) -> Vec<String> {
    written(out)
        .iter()
        .filter(|m| m["type"] == "user")
        .map(|m| {
            m["message"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[test]
fn prompt_before_the_handshake_is_buffered_then_sent() {
    let bytes = read("ok");
    let mut d = ClaudeDriver::new();
    let mut out = Out::default();
    d.on_start(&launch(), &mut out);
    out.take();
    d.prompt("first", PromptHow::Queue, &mut out).unwrap();
    d.prompt("second", PromptHow::Queue, &mut out).unwrap();
    d.prompt("steer", PromptHow::Steer, &mut out).unwrap();
    assert!(
        out.stdin.is_empty(),
        "nothing is written before initialize is answered"
    );
    feed_until(&mut d, &bytes, &mut out, is_init_response);
    assert_eq!(
        texts(&out),
        ["first"],
        "one at a time: the rest wait for the turn to end"
    );
    assert!(
        written(&out)[0]["uuid"].is_string(),
        "every message is stamped so the CLI's lifecycle frames can be matched"
    );
}

#[test]
fn queue_waits_for_the_turn_to_end_and_goes_out_one_by_one() {
    let bytes = read("ok");
    let mut d = ClaudeDriver::new();
    let mut out = Out::default();
    feed_until(&mut d, &bytes, &mut out, |v| is_lifecycle(v, "started"));
    assert_eq!(d.status(), AgentStatus::Working);
    d.prompt("queued one", PromptHow::Queue, &mut out).unwrap();
    d.prompt("queued two", PromptHow::Queue, &mut out).unwrap();
    assert!(
        out.stdin.is_empty(),
        "Queue mid-turn is held back (the CLI would fold it into the running turn)"
    );
    // finish the turn: the first queued prompt goes out, the second waits for the next turn
    let mut ev = Vec::new();
    for (off, line) in spool_lines(&bytes) {
        ev.extend(d.on_line(off, line, &mut out));
    }
    assert_eq!(texts(&out), ["queued one"]);
    assert_eq!(
        d.status(),
        AgentStatus::Working,
        "no Idle blip while a queued prompt is on its way"
    );
    assert!(
        !kinds(&ev).iter().any(|k| k == "status:Idle"),
        "{:?}",
        kinds(&ev)
    );
}

#[test]
fn steer_writes_through_and_interrupt_then_send_interrupts_first() {
    let bytes = read("ok");
    let mut d = ClaudeDriver::new();
    let mut out = Out::default();
    feed_until(&mut d, &bytes, &mut out, |v| is_lifecycle(v, "started"));
    d.prompt("steer now", PromptHow::Steer, &mut out).unwrap();
    assert_eq!(
        texts(&out),
        ["steer now"],
        "the CLI folds it in at the next tool boundary"
    );
    out.take();
    d.prompt("instead do this", PromptHow::InterruptThenSend, &mut out)
        .unwrap();
    let w = written(&out);
    assert_eq!(w.len(), 1);
    assert_eq!(
        w[0]["request"]["subtype"], "interrupt",
        "interrupt now, the message after the interrupted result"
    );
    out.take();
    // the interrupted turn's result releases it
    let int = read("interrupt");
    let mut d2 = ClaudeDriver::new();
    feed_until(&mut d2, &int, &mut out, |v| is_lifecycle(v, "started"));
    d2.prompt("instead do this", PromptHow::InterruptThenSend, &mut out)
        .unwrap();
    out.take();
    feed_until(&mut d2, &int, &mut out, |v| v["type"] == "result");
    assert_eq!(texts(&out), ["instead do this"]);
}

#[test]
fn idle_prompt_is_written_immediately() {
    for how in [
        PromptHow::Queue,
        PromptHow::Steer,
        PromptHow::InterruptThenSend,
    ] {
        let mut d = ClaudeDriver::new();
        let mut out = Out::default();
        d.prompt("go", how, &mut out).unwrap();
        assert_eq!(texts(&out), ["go"], "{how:?}");
    }
}

#[test]
fn a_second_queued_prompt_waits_even_before_the_first_one_has_started() {
    // Between our write and the CLI's `started` frame there is no turn yet; a Queue prompt must
    // still not overtake or join the first one.
    let mut d = ClaudeDriver::new();
    let mut out = Out::default();
    d.prompt("a", PromptHow::Queue, &mut out).unwrap();
    let a_uuid = written(&out)[0]["uuid"].as_str().unwrap().to_owned();
    d.prompt("b", PromptHow::Queue, &mut out).unwrap();
    assert_eq!(texts(&out), ["a"], "b is held");
    // the CLI's turn for `a`: the `ok` capture with our uuid in place of the capture script's
    let bytes = String::from_utf8(read("ok"))
        .unwrap()
        .replace("00000001-0000-4000-8000-000000000001", &a_uuid)
        .into_bytes();
    let before_result = spool_lines(&bytes)
        .into_iter()
        .take_while(|(_, l)| {
            serde_json::from_slice::<Value>(l).map_or(true, |v| v["type"] != "result")
        })
        .count();
    for (off, line) in spool_lines(&bytes).into_iter().take(before_result) {
        d.on_line(off, line, &mut out);
    }
    assert_eq!(texts(&out), ["a"], "still held while the turn runs");
    for (off, line) in spool_lines(&bytes).into_iter().skip(before_result) {
        d.on_line(off, line, &mut out);
    }
    assert_eq!(texts(&out), ["a", "b"], "released by the result of a");
}

// ---- answering -------------------------------------------------------------------------------

fn up_to_question(case: &str) -> (ClaudeDriver, Vec<femme_proto::state::Question>) {
    let mut d = ClaudeDriver::new();
    let (mut out, mut qs) = (Out::default(), Vec::new());
    for (off, line) in spool_lines(&read(case)) {
        for e in d.on_line(off, line, &mut out) {
            if let AgentEvent::QuestionAsked { q, .. } = e {
                qs.push(q);
            }
        }
        if !qs.is_empty()
            && serde_json::from_slice::<Value>(line)
                .map(|v| v["type"] == "control_request")
                .unwrap_or(false)
        {
            break;
        }
    }
    (d, qs)
}

fn captured_response(case: &str) -> Value {
    stdin_lines(case)
        .into_iter()
        .find(|m| m["type"] == "control_response")
        .expect("a control_response in the capture")
}

#[test]
fn answer_is_what_the_capture_sent() {
    // question: option 1 -> answers {question: "B"}
    let (mut d, qs) = up_to_question("question");
    let mut out = Out::default();
    d.answer(&qs[0].request_id, &Answer::Option(1), &mut out)
        .unwrap();
    assert_eq!(written(&out), [captured_response("question")]);
    assert_eq!(
        d.answer(&qs[0].request_id, &Answer::Option(1), &mut out),
        Err(Error::UnknownRequest(qs[0].request_id.clone())),
        "answered once"
    );

    // multi-select: options 0 and 1 -> "A, B"
    let (mut d, qs) = up_to_question("question_multi");
    let mut out = Out::default();
    d.answer(&qs[0].request_id, &Answer::Options(vec![0, 1]), &mut out)
        .unwrap();
    assert_eq!(written(&out), [captured_response("question_multi")]);

    // two questions: nothing is written until both are answered
    let (mut d, qs) = up_to_question("question_two");
    assert_eq!(qs.len(), 2);
    let mut out = Out::default();
    d.answer(&qs[1].request_id, &Answer::Option(1), &mut out)
        .unwrap();
    assert!(out.stdin.is_empty());
    d.answer(&qs[0].request_id, &Answer::Option(1), &mut out)
        .unwrap();
    assert_eq!(written(&out), [captured_response("question_two")]);

    // declining: deny, with the message the capture used
    let (mut d, qs) = up_to_question("question_deny");
    let mut out = Out::default();
    d.answer(&qs[0].request_id, &Answer::Cancel, &mut out)
        .unwrap();
    assert_eq!(written(&out), [captured_response("question_deny")]);

    // plan approval: allow with the plan input unchanged
    let (mut d, qs) = up_to_question("plan");
    let mut out = Out::default();
    d.answer(&qs[0].request_id, &Answer::Option(0), &mut out)
        .unwrap();
    assert_eq!(written(&out), [captured_response("plan")]);
}

#[test]
fn bad_answers_are_refused_without_writing() {
    let (mut d, qs) = up_to_question("question");
    let mut out = Out::default();
    let id = &qs[0].request_id;
    assert!(matches!(
        d.answer(id, &Answer::Option(9), &mut out),
        Err(Error::Bad(_))
    ));
    assert!(
        matches!(
            d.answer(id, &Answer::Options(vec![0, 1]), &mut out),
            Err(Error::Bad(_))
        ),
        "single choice"
    );
    assert!(matches!(
        d.answer("nope", &Answer::Option(0), &mut out),
        Err(Error::UnknownRequest(_))
    ));
    assert!(out.stdin.is_empty());
    // free text is allowed ("Other")
    d.answer(id, &Answer::Text("neither".into()), &mut out)
        .unwrap();
    assert_eq!(
        written(&out)[0]["response"]["response"]["updatedInput"]["answers"]
            ["Do you prefer A or B?"],
        "neither"
    );
}

// ---- resume_offset and restart ---------------------------------------------------------------

#[test]
fn resume_offset_is_the_start_of_the_unfinished_turn() {
    let bytes = read("kill9_1");
    let mut d = ClaudeDriver::new();
    let mut out = Out::default();
    let started = feed_until(&mut d, &bytes, &mut out, |v| is_lifecycle(v, "started"));
    let lines = spool_lines(&bytes);
    let started_line = lines
        .iter()
        .map(|(o, _)| *o)
        .find(|o| *o + 1 > 0 && *o < started)
        .map(|_| ())
        .and(lines.iter().rev().map(|(o, _)| *o).find(|o| *o < started))
        .unwrap();
    assert_eq!(
        d.resume_offset(),
        started_line,
        "mid-turn: the line that opened the turn"
    );
    let mut d = ClaudeDriver::new();
    for (off, line) in &lines {
        d.on_line(*off, line, &mut out);
    }
    assert_eq!(d.status(), AgentStatus::Working, "killed mid-turn");
    assert_eq!(
        d.resume_offset(),
        started_line,
        "still the turn start: the file ends inside the turn"
    );
    // after on_exit everything is closed and the next offset is the end of the spool
    let ev = d.on_exit(None);
    let k = kinds(&ev);
    assert!(
        k.contains(&"turn-:Interrupted".to_owned())
            && k.contains(&"status:Idle".to_owned())
            && k.last().unwrap() == "exited",
        "{k:?}"
    );
    assert!(
        ev.iter().any(|e| matches!(
            e,
            AgentEvent::ItemCompleted {
                kind: ItemKind::ToolCall,
                status: ItemStatus::Failed,
                ..
            }
        )),
        "the tool that was running failed"
    );
    assert_eq!(d.resume_offset(), bytes.len() as u64);

    let bytes = read("ok");
    let mut d = ClaudeDriver::new();
    replay(&mut d, &bytes);
    assert_eq!(
        d.resume_offset(),
        bytes.len() as u64,
        "complete turn: nothing needs replaying"
    );
    assert_eq!(d.status(), AgentStatus::Idle);
}

/// Final state of the items an event list produced, like the daemon's upsert by item id.
fn fold(ev: &[AgentEvent]) -> BTreeMap<String, (ItemKind, ItemStatus, Value)> {
    let mut m = BTreeMap::new();
    for e in ev {
        if let AgentEvent::ItemCompleted {
            id,
            kind,
            status,
            body,
            ..
        } = e
        {
            m.insert(id.clone(), (*kind, *status, body.clone()));
        }
    }
    m
}

#[test]
fn restart_from_resume_offset_loses_and_duplicates_nothing() {
    // Cut the spool at every line: a first driver sees the head, the daemon "restarts", a fresh
    // driver replays from the first one's resume_offset to the end. The items must come out the
    // same as with one uninterrupted run (daemon upserts by item id).
    for case in [
        "tool",
        "question",
        "question_two",
        "plan",
        "interrupt",
        "queue_text",
        "ask_interrupt",
    ] {
        let bytes = read(case);
        let lines = spool_lines(&bytes);
        let want = fold(&replay(&mut ClaudeDriver::new(), &bytes).0);
        for cut in 1..lines.len() {
            let mut first = ClaudeDriver::new();
            let mut out = Out::default();
            let mut ev = Vec::new();
            for (off, line) in &lines[..cut] {
                ev.extend(first.on_line(*off, line, &mut out));
            }
            let from = first.resume_offset();
            assert!(
                from <= lines[cut].0,
                "{case}@{cut}: resume_offset {from} is past the unread data"
            );
            let mut second = ClaudeDriver::new();
            for (off, line) in lines.iter().filter(|(o, _)| *o >= from) {
                ev.extend(second.on_line(*off, line, &mut out));
            }
            assert_eq!(
                fold(&ev),
                want,
                "{case}: restart after line {cut} (resume from {from}) changed the transcript"
            );
            assert!(out.stdin.is_empty(), "{case}@{cut}: replay wrote to stdin");
        }
    }
}

// ---- never panic -----------------------------------------------------------------------------

fn arb_json() -> impl Strategy<Value = Value> {
    let key = prop::sample::select(vec![
        "type",
        "subtype",
        "event",
        "message",
        "content",
        "index",
        "delta",
        "content_block",
        "request",
        "request_id",
        "tool_use_id",
        "tool_name",
        "input",
        "questions",
        "options",
        "label",
        "id",
        "name",
        "text",
        "state",
        "command_uuid",
        "user_message_uuids",
        "usage",
        "is_error",
        "rate_limit_info",
        "unifiedWindows",
        "utilization",
        "resetsAt",
        "parent_tool_use_id",
        "isReplay",
        "uuid",
        "answers",
        "model",
        "session_id",
    ]);
    let ty = prop::sample::select(vec![
        "system",
        "stream_event",
        "assistant",
        "user",
        "result",
        "rate_limit_event",
        "command_lifecycle",
        "control_request",
        "control_cancel_request",
        "control_response",
        "init",
        "status",
        "can_use_tool",
        "text",
        "tool_use",
        "tool_result",
        "thinking",
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "message_start",
        "AskUserQuestion",
        "ExitPlanMode",
        "started",
        "queued",
        "completed",
    ]);
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        any::<f64>()
            .prop_filter("finite", |f| f.is_finite())
            .prop_map(|f| json!(f)),
        "[a-z0-9_ -]{0,8}".prop_map(Value::String),
        ty.prop_map(|t| Value::String(t.to_owned())),
    ];
    leaf.prop_recursive(4, 48, 6, move |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::vec((key.clone(), inner), 0..6).prop_map(|kv| Value::Object(
                kv.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
            )),
        ]
    })
}

/// Every line of every Claude fixture, for mutation fuzzing.
fn all_fixture_lines() -> Vec<Value> {
    let mut v = Vec::new();
    for case in CASES {
        for line in String::from_utf8(read(case)).unwrap().lines() {
            v.push(serde_json::from_str(line).unwrap());
        }
    }
    v
}

/// Replace one node of `v`, chosen by walking down with `seed`, by `with`.
fn mutate(v: &mut Value, seed: u64, with: Value) {
    let seed = seed.rotate_left(7) ^ 0x9e37_79b9;
    let pick = (seed >> 8) as usize;
    let child = match v {
        Value::Object(o) if !o.is_empty() && !seed.is_multiple_of(3) => {
            let n = pick % o.len();
            o.iter_mut().nth(n).map(|(_, x)| x)
        }
        Value::Array(a) if !a.is_empty() && !seed.is_multiple_of(3) => {
            let n = pick % a.len();
            a.get_mut(n)
        }
        _ => None,
    };
    match child {
        Some(c) => mutate(c, seed, with),
        None => *v = with,
    }
}

fn feed(d: &mut ClaudeDriver, lines: &[Vec<u8>]) {
    let mut out = Out::default();
    let mut off = 0u64;
    for l in lines {
        let _ = d.on_line(off, l, &mut out);
        off += l.len() as u64 + 1;
        assert!(d.resume_offset() <= off);
        let _ = d.status();
        let _ = d.prompt("p", PromptHow::Queue, &mut out);
        let _ = d.interrupt(&mut out);
        let _ = d.answer("x", &Answer::Option(0), &mut out);
    }
    let _ = d.on_exit(Some(1));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn random_json_never_panics(msgs in prop::collection::vec(arb_json(), 0..24)) {
        let lines: Vec<Vec<u8>> = msgs.iter().map(|m| serde_json::to_vec(m).unwrap()).collect();
        feed(&mut ClaudeDriver::new(), &lines);
        let mut started = ClaudeDriver::new();
        started.on_start(&launch(), &mut Out::default());
        feed(&mut started, &lines);
    }

    #[test]
    fn random_bytes_never_panic(lines in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 0..16)) {
        // a line is complete: the newline that ended it is not part of it
        let lines: Vec<Vec<u8>> = lines.into_iter().map(|l| l.into_iter().filter(|b| *b != b'\n').collect()).collect();
        feed(&mut ClaudeDriver::new(), &lines);
    }

    #[test]
    fn mutated_real_lines_never_panic(picks in prop::collection::vec((any::<prop::sample::Index>(), any::<u64>(), arb_json()), 1..40)) {
        let pool = all_fixture_lines();
        let lines: Vec<Vec<u8>> = picks.into_iter().map(|(i, seed, with)| {
            let mut v = pool[i.index(pool.len())].clone();
            if !seed.is_multiple_of(4) { mutate(&mut v, seed, with); }
            serde_json::to_vec(&v).unwrap()
        }).collect();
        feed(&mut ClaudeDriver::new(), &lines);
    }
}

// ---- subagents -------------------------------------------------------------------------------

/// A turn whose `Agent` call (`toolu_A`) runs a subagent; `sub` adds the subagent's lines.
fn agent_turn(sub: bool) -> Vec<u8> {
    let p = |mut v: Value| {
        v["parent_tool_use_id"] = json!("toolu_A");
        v
    };
    let se = |e: Value| p(json!({"type":"stream_event","event":e}));
    let mut lines = vec![
        json!({"type":"user","uuid":"u1","isReplay":true,"message":{"role":"user","content":[{"type":"text","text":"look around"}]}}),
        json!({"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"toolu_A","name":"Agent","input":{"description":"look","prompt":"ls"}}]}}),
    ];
    if sub {
        lines.extend([
            p(json!({"type":"user","uuid":"s1","message":{"role":"user","content":[{"type":"text","text":"ls"}]}})),
            se(json!({"type":"message_start","message":{"id":"sm1"}})),
            se(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
            se(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"on it"}})),
            se(json!({"type":"content_block_stop","index":0})),
            p(json!({"type":"assistant","message":{"id":"sm1","content":[{"type":"text","text":"on it"}]}})),
            p(json!({"type":"assistant","message":{"id":"sm2","content":[
                {"type":"thinking","thinking":"hm"},
                {"type":"tool_use","id":"toolu_B","name":"Bash","input":{"command":"ls"}},
                {"type":"tool_use","id":"toolu_Q","name":"AskUserQuestion","input":{"questions":[{"question":"which?","options":[{"label":"a"},{"label":"b"}]}]}}]}})),
            p(json!({"type":"user","uuid":"s2","message":{"content":[
                {"type":"tool_result","tool_use_id":"toolu_B","content":"x.rs"},
                {"type":"tool_result","tool_use_id":"toolu_Q","content":"no user here","is_error":true}]}})),
            p(json!({"type":"assistant","message":{"content":[{"type":"text","text":"x.rs"}]}})),
        ]);
    }
    lines.extend([
        json!({"type":"user","uuid":"r1","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_A","content":"x.rs"}]}}),
        json!({"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"one file"}]}}),
        json!({"type":"result","subtype":"success","is_error":false,"num_turns":2,"duration_ms":9}),
    ]);
    let mut bytes = Vec::new();
    for l in lines {
        bytes.extend(l.to_string().into_bytes());
        bytes.push(b'\n');
    }
    bytes
}

/// (id, kind, status, parent) of every completed item.
fn done_items(ev: &[AgentEvent]) -> Vec<(String, ItemKind, ItemStatus, Option<String>)> {
    ev.iter()
        .filter_map(|e| match e {
            AgentEvent::ItemCompleted {
                id,
                kind,
                status,
                parent,
                ..
            } => Some((id.clone(), *kind, *status, parent.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn subagent_lines_are_items_of_their_agent_call_and_leave_the_turn_alone() {
    let (with, _) = replay(&mut ClaudeDriver::new(), &agent_turn(true));
    let (without, _) = replay(&mut ClaudeDriver::new(), &agent_turn(false));

    // the turn and status run exactly as without the subagent
    let turn = |ev: &[AgentEvent]| -> Vec<String> {
        kinds(ev)
            .into_iter()
            .filter(|k| k.starts_with("turn") || k.starts_with("status"))
            .collect()
    };
    assert_eq!(turn(&with), turn(&without));
    assert_eq!(count(&with, "asked"), 0);

    let a = Some("toolu_A".to_owned());
    let all = done_items(&with);
    let main: Vec<_> = all.iter().filter(|i| i.3.is_none()).cloned().collect();
    assert_eq!(main, done_items(&without));
    // the Agent call's item id is its tool_use_id: children find it by that
    assert!(main
        .iter()
        .any(|i| i.0 == "toolu_A" && i.1 == ItemKind::ToolCall));

    let sub: Vec<_> = all
        .iter()
        .filter(|i| i.3.is_some())
        .map(|(id, k, s, p)| {
            assert_eq!(*p, a);
            (id.as_str(), *k, *s)
        })
        .collect();
    let off = agent_turn(true)
        .split(|&b| b == b'\n')
        .take(10)
        .map(|l| l.len() + 1)
        .sum::<usize>();
    let scoped = format!("toolu_A/o{off}:0");
    assert_eq!(
        sub,
        [
            ("s1", ItemKind::UserMessage, ItemStatus::Completed),
            ("sm1:0", ItemKind::AssistantMessage, ItemStatus::Completed),
            ("sm2:0", ItemKind::Thinking, ItemStatus::Completed),
            ("toolu_B", ItemKind::ToolCall, ItemStatus::Completed),
            (
                "toolu_B:result",
                ItemKind::ToolResult,
                ItemStatus::Completed
            ),
            ("toolu_Q", ItemKind::ToolCall, ItemStatus::Failed),
            ("toolu_Q:result", ItemKind::ToolResult, ItemStatus::Failed),
            (
                scoped.as_str(),
                ItemKind::AssistantMessage,
                ItemStatus::Completed
            ),
        ]
    );
    // a subagent's AskUserQuestion is a plain tool call, never a Question or Plan
    assert!(!all
        .iter()
        .any(|i| matches!(i.1, ItemKind::Question | ItemKind::Plan)));
    // what streamed opened with the parent too
    assert!(with.iter().any(|e| matches!(e,
        AgentEvent::ItemStarted { id, parent, .. } if id == "sm1:0" && *parent == a)));
}

/// A subagent still running when the turn ends (interrupted) fails what it left open; outside a
/// turn its lines open none.
#[test]
fn a_subagent_left_open_fails_with_the_turn_and_never_opens_one() {
    let p = |mut v: Value| {
        v["parent_tool_use_id"] = json!("toolu_A");
        v
    };
    let lines = [
        json!({"type":"user","uuid":"u1","isReplay":true,"message":{"role":"user","content":[{"type":"text","text":"go"}]}}),
        json!({"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"toolu_A","name":"Agent","input":{}}]}}),
        p(
            json!({"type":"assistant","message":{"id":"sm1","content":[{"type":"tool_use","id":"toolu_B","name":"Bash","input":{}}]}}),
        ),
        json!({"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":1}),
        p(
            json!({"type":"assistant","message":{"id":"sm2","content":[{"type":"text","text":"late"}]}}),
        ),
    ];
    let mut bytes = Vec::new();
    for l in lines {
        bytes.extend(l.to_string().into_bytes());
        bytes.push(b'\n');
    }
    let mut d = ClaudeDriver::new();
    let (ev, _) = replay(&mut d, &bytes);
    let items = done_items(&ev);
    assert!(items.contains(&(
        "toolu_B".into(),
        ItemKind::ToolCall,
        ItemStatus::Failed,
        Some("toolu_A".into())
    )));
    assert!(items.contains(&(
        "toolu_A".into(),
        ItemKind::ToolCall,
        ItemStatus::Failed,
        None
    )));
    assert_eq!(count(&ev, "turn+"), 1);
    assert_eq!(
        kinds(&ev).iter().filter(|k| k.starts_with("turn-")).count(),
        1
    );
    assert_eq!(d.status(), AgentStatus::Idle);
    assert_eq!(
        items.last().unwrap().0,
        "sm2:0",
        "a late subagent line is still an item"
    );
}
