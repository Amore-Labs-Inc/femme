//! Live `pi --mode rpc` runs through `PiDriver`:
//! `cargo test -p femme-agents --test live_pi -- --ignored --test-threads 1`.
//! Pi uses its own configuration (a real provider costs tokens; the prompts are tiny). Model:
//! `FEMME_PI_MODEL` (`provider/id`), else pi's default. Without credentials, run against the
//! capture script's scripted model: start `scripts/capture-pi.py --serve` and export the
//! environment it prints (`PI_CODING_AGENT_DIR=... FEMME_PI_MODEL=mock/mock-a ...`).
mod common;

use std::time::Duration;

use common::live::Live;
use femme_agents::pi::PiDriver;
use femme_agents::{
    AgentEvent, AgentStatus, Attach, Driver, Error, HeldPrompt, Launch, Out, Spawn, StopReason,
};
use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::{Agent, ItemKind, ItemStatus};

/// `PiDriver` with its command under `timeout 120`, so a stuck test cannot leave pi behind.
struct Timed(PiDriver);

impl Driver for Timed {
    fn agent(&self) -> Agent {
        self.0.agent()
    }
    fn spawn(&self, l: &Launch) -> Spawn {
        let mut s = self.0.spawn(l);
        s.cmd.splice(0..0, ["timeout".to_owned(), "120".to_owned()]);
        s
    }
    fn tui(&self, l: &Launch) -> Option<Spawn> {
        self.0.tui(l)
    }
    fn on_start(&mut self, l: &Launch, out: &mut Out) {
        self.0.on_start(l, out)
    }
    fn attach(&mut self, l: &Launch, a: &Attach, out: &mut Out) {
        self.0.attach(l, a, out)
    }
    fn on_line(&mut self, offset: u64, line: &[u8], out: &mut Out) -> Vec<AgentEvent> {
        self.0.on_line(offset, line, out)
    }
    fn prompt(&mut self, text: &str, how: PromptHow, out: &mut Out) -> Result<(), Error> {
        self.0.prompt(text, how, out)
    }
    fn interrupt(&mut self, out: &mut Out) -> Result<(), Error> {
        self.0.interrupt(out)
    }
    fn answer(&mut self, request_id: &str, answer: &Answer, out: &mut Out) -> Result<(), Error> {
        self.0.answer(request_id, answer, out)
    }
    fn set_model(&mut self, model: &str, out: &mut Out) -> Result<(), Error> {
        self.0.set_model(model, out)
    }
    fn resume_offset(&self) -> u64 {
        self.0.resume_offset()
    }
    fn held_prompts(&self) -> Vec<HeldPrompt> {
        self.0.held_prompts()
    }
    fn pending_answers(&self) -> Vec<(String, Answer)> {
        self.0.pending_answers()
    }
    fn agent_session_id(&self) -> Option<&str> {
        self.0.agent_session_id()
    }
    fn status(&self) -> AgentStatus {
        self.0.status()
    }
    fn on_exit(&mut self, code: Option<i32>) -> Vec<AgentEvent> {
        self.0.on_exit(code)
    }
}

/// A scratch git repo per test.
fn launch(name: &str) -> Launch {
    let dir = std::env::temp_dir().join(format!("femme-pi-live-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let _ = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .status();
    Launch {
        cwd: dir.to_string_lossy().into_owned(),
        model: std::env::var("FEMME_PI_MODEL").ok(),
        ..Launch::default()
    }
}

const WAIT: Duration = Duration::from_secs(90);

fn turn_ended(e: &AgentEvent) -> bool {
    matches!(e, AgentEvent::TurnEnded { .. })
}

#[test]
#[ignore = "runs the real pi"]
fn tool_turn_then_resume() {
    let l = launch("tool");
    let mut a = Live::start(Box::new(Timed(PiDriver::new())), &l);
    a.expect("Started", WAIT, |e| matches!(e, AgentEvent::Started { .. }));
    a.call(|d, out| {
        d.prompt(
            "Run `echo hi` with your shell tool and then say done.",
            PromptHow::Queue,
            out,
        )
    })
    .unwrap();
    a.expect("TurnEnded", WAIT, turn_ended);
    let result = a.events.iter().find_map(|e| match e {
        AgentEvent::ItemCompleted {
            kind: ItemKind::ToolResult,
            status: ItemStatus::Completed,
            body,
            ..
        } => Some(body["output"].as_str().unwrap_or("").to_owned()),
        _ => None,
    });
    assert_eq!(
        result.as_deref().map(str::trim),
        Some("hi"),
        "{:#?}",
        a.events
    );
    assert!(a.events.iter().any(|e| matches!(
        e,
        AgentEvent::TurnEnded {
            stop: StopReason::EndTurn,
            usage: Some(_),
            ..
        }
    )));
    let sid = a.driver.agent_session_id().expect("session id").to_owned();
    a.finish(Duration::from_secs(5));
    assert_eq!(a.events.last(), Some(&AgentEvent::Exited { code: Some(0) }));

    // A second process continues the same session.
    let mut b = Live::start(
        Box::new(Timed(PiDriver::new())),
        &Launch {
            resume: Some(sid.clone()),
            ..l
        },
    );
    b.expect(
        "Started",
        WAIT,
        |e| matches!(e, AgentEvent::Started { agent_sid, .. } if *agent_sid == sid),
    );
    b.call(|d, out| d.prompt("Reply with the single word ok.", PromptHow::Queue, out))
        .unwrap();
    b.expect("TurnEnded", WAIT, turn_ended);
    b.finish(Duration::from_secs(5));
}

#[test]
#[ignore = "runs the real pi"]
fn interrupt_then_send() {
    let l = launch("interrupt");
    let mut a = Live::start(Box::new(Timed(PiDriver::new())), &l);
    a.call(|d, out| {
        d.prompt(
            "Count slowly from 1 to 50, one number per line.",
            PromptHow::Queue,
            out,
        )
    })
    .unwrap();
    a.expect("streaming text", WAIT, |e| {
        matches!(e, AgentEvent::ItemDelta { .. })
    });
    a.call(|d, out| {
        d.prompt(
            "Reply with the single word ok.",
            PromptHow::InterruptThenSend,
            out,
        )
    })
    .unwrap();
    a.expect("first TurnEnded", WAIT, turn_ended);
    a.expect("second TurnEnded", WAIT, turn_ended);
    let stops: Vec<_> = a
        .events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::TurnEnded { stop, .. } = e {
                Some(stop.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(stops, [StopReason::Interrupted, StopReason::EndTurn]);
    assert_eq!(a.driver.status(), AgentStatus::Idle);
    a.finish(Duration::from_secs(5));
}
