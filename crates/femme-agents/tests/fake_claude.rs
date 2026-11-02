//! The fake `claude` femmed's tests run (`fixtures/claude/fake-claude.py`) speaks what
//! `ClaudeDriver` expects: every path the daemon uses, with nothing the driver has to drop.
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use femme_agents::{driver_for, AgentEvent, AgentStatus, Launch, Out, StopReason};
use femme_proto::rpc::{Answer, PromptHow};
use femme_proto::state::{Agent, ItemKind, ItemStatus};

struct Rig {
    driver: Box<dyn femme_agents::Driver>,
    out: Out,
    stdin: std::process::ChildStdin,
    lines: mpsc::Receiver<Vec<u8>>,
    offset: u64,
    events: Vec<AgentEvent>,
    child: std::process::Child,
}

impl Rig {
    fn flush(&mut self) {
        let b = self.out.take();
        if !b.is_empty() {
            self.stdin.write_all(&b).unwrap();
            self.stdin.flush().unwrap();
        }
    }
    /// Feed lines until `pred` holds on the events so far (or 10 s pass).
    fn until(&mut self, what: &str, pred: impl Fn(&[AgentEvent]) -> bool) -> Vec<AgentEvent> {
        let t0 = Instant::now();
        let start = self.events.len();
        while !pred(&self.events[start..]) {
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "{what}: not seen; events so far: {:#?}",
                &self.events[start..]
            );
            match self.lines.recv_timeout(Duration::from_millis(200)) {
                Ok(line) => {
                    let ev = self.driver.on_line(self.offset, &line, &mut self.out);
                    self.offset += line.len() as u64 + 1;
                    self.flush();
                    self.events.extend(ev);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let code = self.child.wait().unwrap().code();
                    let ev = self.driver.on_exit(code);
                    self.events.extend(ev);
                    assert!(
                        pred(&self.events[start..]),
                        "{what}: process exited {code:?}; events: {:#?}",
                        &self.events[start..]
                    );
                }
            }
        }
        self.events[start..].to_vec()
    }
}

fn has(ev: &[AgentEvent], f: impl Fn(&AgentEvent) -> bool) -> bool {
    ev.iter().any(f)
}

fn completed(ev: &[AgentEvent], kind: ItemKind) -> Vec<&AgentEvent> {
    ev.iter()
        .filter(|e| matches!(e, AgentEvent::ItemCompleted { kind: k, .. } if *k == kind))
        .collect()
}

#[test]
fn the_fake_claude_drives_every_path_the_daemon_uses() {
    let dir = std::env::temp_dir().join(format!("femme-fake-claude-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script =
        include_str!("../fixtures/claude/fake-claude.py").replace("__FEMMED__", "/bin/true");
    std::fs::write(dir.join("claude"), script).unwrap();
    std::fs::set_permissions(
        dir.join("claude"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let bin = dir.display().to_string();
    let driver = driver_for(Agent::Claude);
    let launch = Launch {
        cwd: std::env::temp_dir().display().to_string(),
        ..Default::default()
    };
    let spawn = driver.spawn(&launch);
    let planned = spawn
        .cmd
        .iter()
        .find_map(|a| a.strip_prefix("--session-id="))
        .unwrap()
        .to_owned();
    let mut child = Command::new(format!("{bin}/{}", spawn.cmd[0]))
        .args(&spawn.cmd[1..])
        .envs(spawn.env.iter().cloned())
        .env("HOME", std::env::temp_dir())
        .current_dir(&spawn.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).split(b'\n') {
            if tx.send(l.unwrap()).is_err() {
                break;
            }
        }
    });
    let mut rig = Rig {
        driver,
        out: Out::default(),
        stdin,
        lines: rx,
        offset: 0,
        events: Vec::new(),
        child,
    };
    rig.driver.on_start(&launch, &mut rig.out);
    rig.flush();
    let ev = rig.until("Started", |e| {
        has(e, |x| matches!(x, AgentEvent::Started { .. }))
    });
    assert!(
        has(&ev, |x| *x
            == AgentEvent::Started {
                agent_sid: planned.clone(),
                model: Some("claude-haiku-5-5".into())
            }),
        "{ev:#?}"
    );

    // 1. an echo turn: user + assistant items, a clean end, idle
    rig.driver
        .prompt("hi", PromptHow::Queue, &mut rig.out)
        .unwrap();
    rig.flush();
    let ev = rig.until("echo turn", |e| {
        has(e, |x| matches!(x, AgentEvent::TurnEnded { .. }))
    });
    assert!(
        has(
            &ev,
            |x| matches!(x, AgentEvent::ItemCompleted { kind: ItemKind::UserMessage, body, .. } if body["text"] == "hi")
        ),
        "{ev:#?}"
    );
    let a = completed(&ev, ItemKind::AssistantMessage);
    assert_eq!(a.len(), 1, "{ev:#?}");
    assert!(
        matches!(a[0], AgentEvent::ItemCompleted { status: ItemStatus::Completed, body, .. } if body["text"] == "echo: hi"),
        "{a:#?}"
    );
    assert!(
        has(
            &ev,
            |x| matches!(x, AgentEvent::ItemDelta { text, .. } if text == "echo: ")
        ),
        "{ev:#?}"
    );
    assert!(
        has(&ev, |x| matches!(
            x,
            AgentEvent::TurnEnded {
                stop: StopReason::EndTurn,
                ..
            }
        )),
        "{ev:#?}"
    );
    assert!(
        has(
            &ev,
            |x| matches!(x, AgentEvent::RateLimit { kind, percent, .. } if kind == "session" && (*percent - 31.0).abs() < 0.01)
        ),
        "{ev:#?}"
    );
    assert_eq!(
        ev.last(),
        Some(&AgentEvent::Status(AgentStatus::Idle)),
        "{ev:#?}"
    );

    // 2. a question: blocked until answered, first option is "a"
    rig.driver
        .prompt("ask", PromptHow::Queue, &mut rig.out)
        .unwrap();
    rig.flush();
    let ev = rig.until("question", |e| {
        has(e, |x| matches!(x, AgentEvent::Status(AgentStatus::Blocked)))
    });
    let q = ev
        .iter()
        .find_map(|x| match x {
            AgentEvent::QuestionAsked { q, .. } => Some(q.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        q.options
            .iter()
            .map(|o| o.chars().next().unwrap())
            .collect::<Vec<_>>(),
        vec!['a', 'b'],
        "{:?}",
        q.options
    );
    rig.driver
        .answer(&q.request_id, &Answer::Option(0), &mut rig.out)
        .unwrap();
    rig.flush();
    let ev = rig.until("answered turn", |e| {
        has(e, |x| matches!(x, AgentEvent::TurnEnded { .. }))
    });
    assert!(
        has(
            &ev,
            |x| matches!(x, AgentEvent::QuestionResolved { request_id } if *request_id == q.request_id)
        ),
        "{ev:#?}"
    );
    assert!(
        has(
            &ev,
            |x| matches!(x, AgentEvent::ItemCompleted { kind: ItemKind::AssistantMessage, body, .. } if body["text"] == "you chose: a")
        ),
        "{ev:#?}"
    );
    assert!(
        rig.driver
            .answer(&q.request_id, &Answer::Option(1), &mut rig.out)
            .is_err(),
        "second answer must be refused"
    );

    // 2b. two questions in one request: the first answer alone writes nothing, the second sends
    // the one response, with both answers in the order asked
    rig.driver
        .prompt("ask2", PromptHow::Queue, &mut rig.out)
        .unwrap();
    rig.flush();
    let ev = rig.until("two questions", |e| {
        e.iter()
            .filter(|x| matches!(x, AgentEvent::QuestionAsked { .. }))
            .count()
            == 2
    });
    let qs: Vec<_> = ev
        .iter()
        .filter_map(|x| match x {
            AgentEvent::QuestionAsked { q, .. } => Some(q.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        qs.iter().map(|q| q.prompt.as_str()).collect::<Vec<_>>(),
        ["Which?", "Second?"]
    );
    rig.driver
        .answer(&qs[0].request_id, &Answer::Option(0), &mut rig.out)
        .unwrap();
    assert!(rig.out.stdin.is_empty(), "one part of two is not sent");
    rig.driver
        .answer(&qs[1].request_id, &Answer::Option(1), &mut rig.out)
        .unwrap();
    rig.flush();
    let ev = rig.until("both answered", |e| {
        has(e, |x| matches!(x, AgentEvent::TurnEnded { .. }))
    });
    assert!(
        has(
            &ev,
            |x| matches!(x, AgentEvent::ItemCompleted { kind: ItemKind::AssistantMessage, body, .. } if body["text"] == "you chose: a, y")
        ),
        "{ev:#?}"
    );
    assert_eq!(
        completed(&ev, ItemKind::Question).len(),
        2,
        "each part ends Answered: {ev:#?}"
    );

    // 3. slow + interrupt mid-stream
    rig.driver
        .prompt("slow", PromptHow::Queue, &mut rig.out)
        .unwrap();
    rig.flush();
    rig.until("three ticks", |e| {
        e.iter()
            .filter(|x| matches!(x, AgentEvent::ItemDelta { .. }))
            .count()
            >= 3
    });
    rig.driver.interrupt(&mut rig.out).unwrap();
    rig.flush();
    let ev = rig.until("interrupted", |e| {
        has(e, |x| matches!(x, AgentEvent::TurnEnded { .. }))
    });
    assert!(
        has(&ev, |x| matches!(
            x,
            AgentEvent::TurnEnded {
                stop: StopReason::Interrupted,
                ..
            }
        )),
        "{ev:#?}"
    );
    let n = rig
        .events
        .iter()
        .filter(|x| matches!(x, AgentEvent::ItemDelta { .. }))
        .count();
    assert!(n < 50, "the stream stopped early: {n} deltas");

    // 4. set_model is learned from the CLI's echo
    rig.driver
        .set_model("claude-opus-5-5", &mut rig.out)
        .unwrap();
    rig.flush();
    rig.until("model changed", |e| {
        has(e, |x| {
            *x == AgentEvent::ModelChanged("claude-opus-5-5".into())
        })
    });

    // 5. a queued prompt while a turn runs is held and runs after it
    rig.driver
        .prompt("slow", PromptHow::Queue, &mut rig.out)
        .unwrap();
    rig.flush();
    rig.until("turn started", |e| {
        has(e, |x| matches!(x, AgentEvent::TurnStarted))
    });
    rig.driver
        .prompt("second", PromptHow::Queue, &mut rig.out)
        .unwrap();
    rig.flush();
    assert_eq!(rig.driver.held_prompts().len(), 1);
    rig.driver.interrupt(&mut rig.out).unwrap();
    rig.flush();
    let ev = rig.until("second ran", |e| {
        e.iter()
            .filter(|x| matches!(x, AgentEvent::TurnEnded { .. }))
            .count()
            >= 2
    });
    assert!(
        has(
            &ev,
            |x| matches!(x, AgentEvent::ItemCompleted { kind: ItemKind::AssistantMessage, body, .. } if body["text"] == "echo: second")
        ),
        "{ev:#?}"
    );
    assert!(rig.driver.held_prompts().is_empty());

    // 6. crash: stderr text, exit 1, Exited event
    rig.driver
        .prompt("crash", PromptHow::Queue, &mut rig.out)
        .unwrap();
    rig.flush();
    let ev = rig.until("exited", |e| {
        has(e, |x| matches!(x, AgentEvent::Exited { .. }))
    });
    assert!(
        has(&ev, |x| *x == AgentEvent::Exited { code: Some(1) }),
        "{ev:#?}"
    );
    assert!(
        !has(&rig.events, |x| matches!(x, AgentEvent::Raw(_))),
        "nothing the driver had to drop as Raw: {:#?}",
        rig.events
            .iter()
            .filter(|x| matches!(x, AgentEvent::Raw(_)))
            .collect::<Vec<_>>()
    );
}
