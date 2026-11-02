//! Live Claude Code tests: real `claude` process, real (haiku) turns. `#[ignore]`d; run with
//! `cargo test -p femme-agents --test live_claude -- --ignored --test-threads=1`.
//! Each test costs one to three cheap turns of `claude-haiku-5-5`.

mod common;

use std::time::Duration;

use common::live::Live;
use femme_agents::claude::ClaudeDriver;
use femme_agents::{AgentEvent, AgentStatus, Launch, StopReason};
use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::{ItemKind, ItemStatus};

const T: Duration = Duration::from_secs(90);

fn launch(cwd: &std::path::Path) -> Launch {
    Launch {
        cwd: cwd.display().to_string(),
        model: Some("claude-haiku-5-5".into()),
        ..Launch::default()
    }
}

/// A fresh git repo in a temp dir (the agent refuses to do much in a non-repo).
fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("femme-live-claude-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let _ = std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&d)
        .status();
    d
}

fn turn_ended(e: &AgentEvent) -> bool {
    matches!(e, AgentEvent::TurnEnded { .. })
}

fn ended_with(live: &Live) -> Vec<StopReason> {
    live.events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::TurnEnded { stop, .. } = e {
                Some(stop.clone())
            } else {
                None
            }
        })
        .collect()
}

#[test]
#[ignore = "runs the real claude CLI"]
fn one_turn() {
    let dir = scratch("one");
    let mut live = Live::start(Box::new(ClaudeDriver::new()), &launch(&dir));
    live.call(|d, o| d.prompt("Reply with the single word ok.", PromptHow::Queue, o))
        .unwrap();
    live.expect("turn end", T, turn_ended);
    assert_eq!(ended_with(&live), [StopReason::EndTurn]);
    assert!(live
        .events
        .iter()
        .any(|e| matches!(e, AgentEvent::Started { .. })));
    assert!(live.events.iter().any(|e| matches!(
        e,
        AgentEvent::ItemCompleted {
            kind: ItemKind::AssistantMessage,
            ..
        }
    )));
    // Idle comes with the same line as TurnEnded, so it is already in the list
    assert!(live
        .events
        .iter()
        .any(|e| matches!(e, AgentEvent::Status(AgentStatus::Idle))));
    assert_eq!(live.driver.status(), AgentStatus::Idle);
    live.finish(Duration::from_secs(10));
}

#[test]
#[ignore = "runs the real claude CLI"]
fn question_then_answer() {
    let dir = scratch("question");
    let mut live = Live::start(Box::new(ClaudeDriver::new()), &launch(&dir));
    live.call(|d, o| {
        d.prompt("Use the AskUserQuestion tool to ask me whether I prefer A or B (two options, labels A and B), then wait for my answer and reply with just the letter I picked.", PromptHow::Queue, o)
    })
    .unwrap();
    let mut asked = None;
    live.expect("question", T, |e| {
        if let AgentEvent::QuestionAsked { q, .. } = e {
            asked = Some(q.clone());
            true
        } else {
            false
        }
    });
    let q = asked.unwrap();
    assert_eq!(live.driver.status(), AgentStatus::Blocked);
    live.call(|d, o| d.answer(&q.request_id, &Answer::Option(1), o))
        .unwrap();
    live.expect("resolved", T, |e| {
        matches!(e, AgentEvent::QuestionResolved { .. })
    });
    live.expect("turn end", T, turn_ended);
    let text: String = live
        .events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::ItemCompleted {
                kind: ItemKind::AssistantMessage,
                body,
                ..
            } = e
            {
                body["text"].as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .collect();
    assert!(
        text.contains('B'),
        "the agent should have seen our answer: {text:?}"
    );
    live.finish(Duration::from_secs(10));
}

#[test]
#[ignore = "runs the real claude CLI"]
fn interrupt_mid_turn() {
    let dir = scratch("interrupt");
    let mut live = Live::start(Box::new(ClaudeDriver::new()), &launch(&dir));
    live.call(|d, o| {
        d.prompt(
            "Write the numbers from 1 to 400, one per line, nothing else.",
            PromptHow::Queue,
            o,
        )
    })
    .unwrap();
    live.expect("streaming text", T, |e| {
        matches!(e, AgentEvent::ItemDelta { .. })
    });
    live.call(|d, o| d.interrupt(o)).unwrap();
    live.expect("turn end", T, turn_ended);
    assert_eq!(ended_with(&live), [StopReason::Interrupted]);
    // still usable afterwards
    live.call(|d, o| d.prompt("Reply with the single word ok.", PromptHow::Queue, o))
        .unwrap();
    live.expect("second turn end", T, |e| {
        matches!(
            e,
            AgentEvent::TurnEnded {
                stop: StopReason::EndTurn,
                ..
            }
        )
    });
    live.finish(Duration::from_secs(10));
}

#[test]
#[ignore = "runs the real claude CLI"]
fn queued_prompts_run_in_order() {
    let dir = scratch("queue");
    let mut live = Live::start(Box::new(ClaudeDriver::new()), &launch(&dir));
    // sent before the handshake is answered: the driver holds all of them
    live.call(|d, o| d.prompt("Reply with the single word one.", PromptHow::Queue, o))
        .unwrap();
    live.call(|d, o| d.prompt("Reply with the single word two.", PromptHow::Queue, o))
        .unwrap();
    live.expect("first turn end", T, turn_ended);
    live.expect("second turn end", T, |e| {
        matches!(e, AgentEvent::TurnEnded { .. })
    });
    assert_eq!(
        ended_with(&live),
        [StopReason::EndTurn, StopReason::EndTurn]
    );
    let users: Vec<String> = live
        .events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::ItemCompleted {
                kind: ItemKind::UserMessage,
                body,
                ..
            } = e
            {
                body["text"].as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        users,
        [
            "Reply with the single word one.",
            "Reply with the single word two."
        ]
    );
    live.finish(Duration::from_secs(10));
}

#[test]
#[ignore = "runs the real claude CLI"]
fn kill_then_resume_replays_from_resume_offset() {
    let dir = scratch("resume");
    let mut live = Live::start(Box::new(ClaudeDriver::new()), &launch(&dir));
    live.call(|d, o| {
        d.prompt(
            "Remember the word tangerine. Reply with the single word ok.",
            PromptHow::Queue,
            o,
        )
    })
    .unwrap();
    live.expect("turn end", T, turn_ended);
    let sid = live
        .driver
        .agent_session_id()
        .expect("session id after the first turn")
        .to_owned();
    live.call(|d, o| d.prompt("Run the shell command `python3 -c 'import time; time.sleep(60)'` and tell me when it is done.", PromptHow::Queue, o)).unwrap();
    live.expect("tool call", T, |e| {
        matches!(
            e,
            AgentEvent::ItemStarted {
                kind: ItemKind::ToolCall,
                ..
            }
        )
    });
    let from = live.driver.resume_offset();
    assert!(
        from > 0 && from < live.spool.len() as u64,
        "mid-turn: resume_offset {from} is inside the spool"
    );
    let spool = live.spool.clone();
    live.child.kill().unwrap(); // kill -9: no goodbye, no result
    live.finish(Duration::from_secs(5));
    assert!(live
        .events
        .iter()
        .any(|e| matches!(e, AgentEvent::Exited { .. })));
    assert!(
        live.events.iter().any(|e| matches!(
            e,
            AgentEvent::ItemCompleted {
                kind: ItemKind::ToolCall,
                status: ItemStatus::Failed,
                ..
            }
        )),
        "the running tool call is failed on exit"
    );

    // "daemon restart": a fresh driver replays the spool from resume_offset, then the agent is resumed
    let mut fresh = ClaudeDriver::new();
    let mut out = femme_agents::Out::default();
    for (off, line) in common::spool_lines(&spool)
        .into_iter()
        .filter(|(o, _)| *o >= from)
    {
        use femme_agents::Driver;
        fresh.on_line(off, line, &mut out);
    }
    assert!(out.stdin.is_empty(), "replay must not write");
    let l = Launch {
        resume: Some(sid.clone()),
        ..launch(&dir)
    };
    let mut live = Live::start(Box::new(ClaudeDriver::new()), &l);
    live.call(|d, o| {
        d.prompt(
            "What word did I ask you to remember? Reply with that word only.",
            PromptHow::Queue,
            o,
        )
    })
    .unwrap();
    live.expect("turn end", T, |e| {
        matches!(
            e,
            AgentEvent::TurnEnded {
                stop: StopReason::EndTurn,
                ..
            }
        )
    });
    let text: String = live
        .events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::ItemCompleted {
                kind: ItemKind::AssistantMessage,
                body,
                ..
            } = e
            {
                body["text"].as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .collect();
    assert!(
        text.to_lowercase().contains("tangerine"),
        "resumed session remembers: {text:?}"
    );
    assert_eq!(live.driver.agent_session_id(), Some(sid.as_str()));
    live.finish(Duration::from_secs(10));
}
