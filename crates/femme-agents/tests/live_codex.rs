//! Live `codex app-server` runs through `CodexDriver` (real model, costs tokens):
//! `cargo test -p femme-agents --test live_codex -- --ignored --test-threads 1`.
//! Model: `FEMME_CODEX_MODEL` (default `gpt-6-luna`, the cheapest on the capture host).
mod common;

use std::time::Duration;

use common::live::Live;
use femme_agents::codex::CodexDriver;
use femme_agents::{
    AgentEvent, AgentStatus, Attach, Driver, Error, HeldPrompt, Launch, Out, Spawn, StopReason,
};
use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::{Agent, ItemKind};

/// `CodexDriver` with its command under `timeout 120`, so a stuck test cannot leave codex behind.
struct Timed(CodexDriver);

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

/// A scratch git repo per test and the cheap model.
fn launch(name: &str) -> Launch {
    let dir = std::env::temp_dir().join(format!("femme-codex-live-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let _ = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&dir)
        .status();
    let model = std::env::var("FEMME_CODEX_MODEL").unwrap_or_else(|_| "gpt-6-luna".into());
    Launch {
        cwd: dir.to_string_lossy().into_owned(),
        model: Some(model),
        ..Launch::default()
    }
}

const WAIT: Duration = Duration::from_secs(90);

#[test]
#[ignore = "runs the real codex app-server"]
fn live_ok() {
    let l = launch("ok");
    let mut live = Live::start(Box::new(Timed(CodexDriver::new())), &l);
    live.call(|d, out| d.prompt("Reply with the single word ok.", PromptHow::Queue, out))
        .unwrap();
    live.expect("turn end", WAIT, |e| {
        matches!(e, AgentEvent::TurnEnded { .. })
    });
    let said = live.events.iter().any(|e| {
        matches!(e, AgentEvent::ItemCompleted { kind: ItemKind::AssistantMessage, body, .. }
            if body["text"].as_str().is_some_and(|t| t.to_lowercase().contains("ok")))
    });
    assert!(said, "{:#?}", live.events);
    assert!(live.events.iter().any(|e| matches!(
        e,
        AgentEvent::TurnEnded {
            stop: StopReason::EndTurn,
            usage: Some(_),
            ..
        }
    )));
    assert_eq!(live.driver.resume_offset(), live.spool.len() as u64);
    let sid = live.driver.agent_session_id().map(str::to_owned);
    assert!(sid.is_some());
    live.finish(Duration::from_secs(5));
    // a restarted daemon replays the spool without writing anything
    let mut fresh = CodexDriver::new();
    let (_, wrote) = common::replay(&mut fresh, &live.spool);
    assert!(wrote.is_empty());
    assert_eq!(fresh.agent_session_id().map(str::to_owned), sid);
    let _ = std::fs::remove_dir_all(&l.cwd);
}

#[test]
#[ignore = "runs the real codex app-server"]
fn live_question_then_interrupt() {
    let l = launch("question");
    let mut live = Live::start(Box::new(Timed(CodexDriver::new())), &l);
    let text = "Before doing anything else, use your request_user_input tool to ask me one multiple-choice \
                question: which color do I prefer, with the options red and blue. After I answer, reply with only the color.";
    live.call(|d, out| d.prompt(text, PromptHow::Queue, out))
        .unwrap();
    live.expect("question", WAIT, |e| {
        matches!(e, AgentEvent::QuestionAsked { .. })
    });
    assert_eq!(live.driver.status(), AgentStatus::Blocked);
    live.call(|d, out| d.interrupt(out)).unwrap();
    live.expect("interrupted turn", WAIT, |e| {
        matches!(
            e,
            AgentEvent::TurnEnded {
                stop: StopReason::Interrupted,
                ..
            }
        )
    });
    assert!(live
        .events
        .iter()
        .any(|e| matches!(e, AgentEvent::QuestionResolved { .. })));
    assert_eq!(live.driver.status(), AgentStatus::Idle);
    live.finish(Duration::from_secs(5));
    let _ = std::fs::remove_dir_all(&l.cwd);
}
