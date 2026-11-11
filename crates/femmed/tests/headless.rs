//! Headless sessions end to end: the fake `claude -p` under a real `femme-holder`, its driver in
//! the daemon, the transcript in the store, questions, restarts mid-turn (M3's exit criterion),
//! the agent's death, and takeover round trips.

mod common;

use std::time::{Duration, Instant};

use common::{headless, pane_of, port_of, wait_for, wait_screen, Box_};
use femme_proto::rpc::{Answer, Attachment, PromptHow};
use femme_proto::state::{Attention, Context, Item, ItemKind, ItemStatus, Mode, SessionStatus};
use femme_proto::Sid;
use femmed::core::{Core, CoreError, PaneRef};
use femmed::daemon::Daemon;
use serde_json::Value;

/// The session's transcript, oldest first.
fn items(d: &Daemon, sid: Sid) -> Vec<Item> {
    let mut v = d
        .store
        .read(|c| femme_store::items::page_items(c, sid, None, 10_000))
        .unwrap();
    v.reverse();
    v
}

/// The context window the session row holds.
fn context(d: &Daemon, sid: Sid) -> Option<Context> {
    d.store
        .read(|c| femme_store::sessions::get_session(c, sid))
        .unwrap()
        .unwrap()
        .context
}

/// What a transcript says, without the ids and times that differ between runs.
fn shape(items: &[Item]) -> Vec<(ItemKind, ItemStatus, Value)> {
    items
        .iter()
        .map(|i| {
            let mut body: Value = i
                .body
                .as_deref()
                .map(|b| serde_json::from_str(b).unwrap())
                .unwrap_or(Value::Null);
            // a saved message's ids and times are its own each run
            if let Some(o) = body.as_object_mut() {
                for k in ["echo", "read_at", "read", "op"] {
                    if o.remove(k).is_some() {
                        o.insert(k.into(), Value::Bool(true));
                    }
                }
            }
            (i.kind, i.status, body)
        })
        .collect()
}

fn text_of(i: &Item) -> String {
    let body: Value = i
        .body
        .as_deref()
        .map(|b| serde_json::from_str(b).unwrap())
        .unwrap_or(Value::Null);
    body["text"].as_str().unwrap_or("").to_owned()
}

async fn wait_items(d: &Daemon, sid: Sid, what: &str, ok: impl Fn(&[Item]) -> bool) -> Vec<Item> {
    let t0 = Instant::now();
    loop {
        let v = items(d, sid);
        if ok(&v) {
            return v;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "timeout waiting for {what}; transcript: {:#?}",
            shape(&v)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn has_text(v: &[Item], kind: ItemKind, text: &str) -> bool {
    v.iter().any(|i| i.kind == kind && text_of(i) == text)
}

/// Live `claude` processes of this box (the fake, headless or TUI), by their command line.
fn claude_processes(b: &Box_) -> usize {
    let needle = format!("{}/bin/claude", b.dir.path().display());
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .chars()
                .all(|c| c.is_ascii_digit())
        })
        .filter(|e| {
            let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
            let stat = std::fs::read_to_string(e.path().join("stat")).unwrap_or_default();
            let zombie = stat.rsplit(") ").next().is_some_and(|s| s.starts_with('Z'));
            String::from_utf8_lossy(&cmd).contains(&needle) && !zombie
        })
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_headless_claude_session_streams_its_transcript_and_reports_status() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    let s = d.create_session(headless(&b.home())).await.unwrap();
    assert_eq!(s.mode, Mode::Headless);
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle after the handshake", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    assert_eq!(claude_processes(&b), 1, "one headless agent");

    d.prompt(&pane, "hi").await.unwrap();
    wait_for(&d, sid, "working", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Working)
    })
    .await;
    let s = wait_for(&d, sid, "done", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle && s.attention == Attention::Done)
    })
    .await
    .unwrap();
    assert_eq!(s.agent_status(), Some("done"));
    let v = wait_items(&d, sid, "the echo", |v| {
        has_text(v, ItemKind::AssistantMessage, "echo: hi")
    })
    .await;
    assert!(
        has_text(&v, ItemKind::UserMessage, "hi"),
        "{:#?}",
        shape(&v)
    );
    let a = v
        .iter()
        .find(|i| i.kind == ItemKind::AssistantMessage)
        .unwrap();
    assert_eq!(
        (a.status, a.preview.as_str()),
        (ItemStatus::Completed, "echo: hi")
    );
    assert!(
        v.iter().all(|i| i.status != ItemStatus::InProgress),
        "{:#?}",
        shape(&v)
    );

    // the screen is the transcript; `done` shows once, as in a terminal
    let screen = d.screen(&pane, false).await.unwrap();
    assert!(screen.text.contains("echo: hi"), "{}", screen.text);
    assert!(screen.text.contains("hi"), "{}", screen.text);
    assert_eq!(screen.status.as_deref(), Some("done"));
    assert_eq!(
        d.screen(&pane, false).await.unwrap().status.as_deref(),
        Some("idle")
    );

    // a second turn appends, nothing is lost or repeated
    d.prompt(&pane, "again").await.unwrap();
    let v = wait_items(&d, sid, "the second echo", |v| {
        has_text(v, ItemKind::AssistantMessage, "echo: again")
    })
    .await;
    assert_eq!(
        v.iter()
            .filter(|i| i.kind == ItemKind::AssistantMessage)
            .count(),
        2,
        "{:#?}",
        shape(&v)
    );
    assert!(v.windows(2).all(|w| w[0].seq < w[1].seq), "seq ascends");

    d.close_session(sid).await.unwrap();
    wait_for(&d, sid, "closed", |s| s.is_none()).await;
    let t0 = Instant::now();
    while claude_processes(&b) > 0 {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "the agent is still running"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_question_blocks_the_session_until_an_answer_and_the_first_answer_wins() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;

    d.prompt(&pane, "ask").await.unwrap();
    let s = wait_for(&d, sid, "blocked", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Blocked && s.attention == Attention::Blocked)
    })
    .await
    .unwrap();
    assert_eq!(s.agent_status(), Some("blocked"));
    let v = wait_items(&d, sid, "a pending question", |v| {
        v.iter()
            .any(|i| i.kind == ItemKind::Question && i.status == ItemStatus::Pending)
    })
    .await;
    let q = v.iter().find(|i| i.kind == ItemKind::Question).unwrap();
    let body: Value = serde_json::from_str(q.body.as_deref().unwrap()).unwrap();
    let request_id = body["request_id"].as_str().unwrap().to_owned();
    assert_eq!(body["prompt"], "Which?", "{body}");
    assert_eq!(body["options"].as_array().unwrap().len(), 2, "{body}");
    // the screen shows it with numbered options, and a prompt bounces as in a terminal
    let screen = d.screen(&pane, false).await.unwrap();
    assert!(screen.text.contains("Which?"), "{}", screen.text);
    assert!(
        screen.text.contains('1') && screen.text.contains('2'),
        "{}",
        screen.text
    );
    assert_eq!(screen.status.as_deref(), Some("blocked"));
    assert_eq!(d.prompt(&pane, "more").await, Err(CoreError::Busy));

    // raw input answers it the way the phone board does: the digit picks the option
    d.input(&pane, b"1").await.unwrap();
    wait_for(&d, sid, "answered and done", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle && s.attention == Attention::Done)
    })
    .await;
    let v = wait_items(&d, sid, "the choice", |v| {
        has_text(v, ItemKind::AssistantMessage, "you chose: a")
    })
    .await;
    let q = v.iter().find(|i| i.kind == ItemKind::Question).unwrap();
    assert_eq!(q.status, ItemStatus::Answered, "{:#?}", shape(&v));
    let body: Value = serde_json::from_str(q.body.as_deref().unwrap()).unwrap();
    assert_eq!(body["answered"], serde_json::json!({"Option": 0}), "{body}");

    // first answer wins: the same question answered again is a conflict, and so is a stray one
    assert!(matches!(
        d.answer(sid, &request_id, Answer::Option(1)).await,
        Err(CoreError::Conflict(_))
    ));
    assert!(d.input(&pane, b"2").await.is_err(), "no question to answer");

    // the driver's interrupt is `interrupt`, not Esc: a running turn ends `Interrupted`
    d.prompt(&pane, "slow").await.unwrap();
    wait_for(&d, sid, "working", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Working)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    d.interrupt(sid).await.unwrap();
    wait_for(&d, sid, "idle after the interrupt", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    let v = items(&d, sid);
    let slow = v
        .iter()
        .rfind(|i| i.kind == ItemKind::AssistantMessage)
        .unwrap();
    assert!(
        !text_of(slow).contains("tick 50"),
        "the stream stopped early: {:#?}",
        shape(&v)
    );
    d.close_session(sid).await.unwrap();
}

/// Claude 2.1.319: a Bash command the turn waits for can be moved to the background (`Ctrl+B`):
/// the call's result says so, the turn ends at once, and the task's end comes as its own event
/// and turn later. With nothing in the foreground the request is `Busy`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreground_task_moves_to_the_background_and_the_turn_goes_on() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    assert_eq!(d.background(sid, None).await, Err(CoreError::Busy));

    let body = |i: &Item| -> Value {
        i.body
            .as_deref()
            .map(|b| serde_json::from_str(b).unwrap())
            .unwrap_or(Value::Null)
    };
    d.prompt(&pane, "bgtask").await.unwrap();
    let v = wait_items(&d, sid, "a foreground task", |v| {
        v.iter()
            .any(|i| i.kind == ItemKind::ToolCall && body(i)["task"] == "foreground")
    })
    .await;
    let call = v
        .iter()
        .find(|i| i.kind == ItemKind::ToolCall)
        .unwrap()
        .item_id
        .clone();
    assert_eq!(
        d.background(sid, Some("toolu_nope")).await,
        Err(CoreError::Busy),
        "not that call"
    );
    let t0 = Instant::now();
    d.background(sid, Some(&call)).await.unwrap();
    wait_for(&d, sid, "idle while the task runs", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    assert!(
        t0.elapsed() < Duration::from_secs(4),
        "the turn did not wait for the task: {:?}",
        t0.elapsed()
    );
    let v = items(&d, sid);
    let result = v.iter().find(|i| i.kind == ItemKind::ToolResult).unwrap();
    assert!(
        body(result)["output"]
            .as_str()
            .unwrap()
            .contains("manually backgrounded"),
        "{:#?}",
        shape(&v)
    );
    let c = v.iter().find(|i| i.item_id == call).unwrap();
    assert_eq!(c.status, ItemStatus::Completed);
    assert_eq!(body(c)["task"], "background", "{:#?}", shape(&v));
    assert_eq!(d.background(sid, None).await, Err(CoreError::Busy));

    // the task's end: an event naming the call, then the turn that reads its output
    let v = wait_items(&d, sid, "the task's end", |v| {
        has_text(v, ItemKind::AssistantMessage, "the task printed 42")
    })
    .await;
    let end = v
        .iter()
        .find(|i| i.kind == ItemKind::Event && body(i)["task"]["call"] == call)
        .unwrap();
    assert_eq!(body(end)["task"]["status"], "completed", "{:#?}", shape(&v));
    d.close_session(sid).await.unwrap();
}

/// M3 exit criterion: a daemon restart in the middle of a streamed turn gives the same transcript
/// as an uninterrupted run, and restarts no agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_restart_mid_turn_resumes_from_the_committed_offset_with_an_identical_transcript()
{
    let b = Box_::new();
    let (mut d, mut stop) = b.start(None).await;
    let port = port_of(&d);

    // the reference: one `slow` turn, nothing in between
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    d.prompt(&pane, "slow").await.unwrap();
    wait_for(&d, sid, "done", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle && s.attention == Attention::Done)
    })
    .await;
    let reference = shape(&items(&d, sid));
    assert!(
        reference
            .iter()
            .any(|(k, _, body)| *k == ItemKind::AssistantMessage
                && body["text"].as_str().unwrap().contains("tick 50")),
        "{reference:#?}"
    );
    d.close_session(sid).await.unwrap();

    // the same turn with the daemon restarted three times while it streams
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    let pid = b.claude_pid();
    d.prompt(&pane, "slow").await.unwrap();
    wait_for(&d, sid, "working", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Working)
    })
    .await;
    for n in 1..=3 {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        drop(stop);
        drop(d);
        (d, stop) = b.start(Some(port)).await;
        d.reconcile().await;
        let listed = d.terminals().await.unwrap();
        assert!(
            listed
                .iter()
                .any(|s| s.sid == sid && s.status == SessionStatus::Working),
            "restart {n}: {listed:?}"
        );
    }
    wait_for(&d, sid, "done", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle && s.attention == Attention::Done)
    })
    .await;
    assert_eq!(b.claude_pid(), pid, "the same agent process");
    let got = shape(&items(&d, sid));
    assert_eq!(got, reference);
    assert!(
        d.screen(&pane, false)
            .await
            .unwrap()
            .text
            .contains("tick 50"),
        "the screen is the transcript"
    );
    d.close_session(sid).await.unwrap();
}

/// The composer's XP bar: the session row follows what the agent says of its context window, and a
/// daemon restarted while a turn that compacted still streams does not count the compaction again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_context_window_follows_the_agent_and_a_restart_does_not_count_a_compaction_twice() {
    let b = Box_::new();
    let (mut d, mut stop) = b.start(None).await;
    let port = port_of(&d);
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    assert_eq!(context(&d, sid), None, "the agent has said nothing yet");

    // an ordinary turn: what its call read, and the window its end names
    d.prompt(&pane, "hi").await.unwrap();
    wait_for(&d, sid, "done", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle && s.attention == Attention::Done)
    })
    .await;
    assert_eq!(
        context(&d, sid),
        Some(Context {
            used: 2,
            window: 200_000,
            compactions: 0,
            compact_at: None
        })
    );

    // a big call, the compaction, a small call; the daemon restarted twice while it streams
    d.prompt(&pane, "compact").await.unwrap();
    let t0 = Instant::now();
    while !matches!(context(&d, sid), Some(c) if c.compactions > 0) {
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "the compaction was not counted: {:?}",
            context(&d, sid)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for n in 1..=2 {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        drop(stop);
        drop(d);
        (d, stop) = b.start(Some(port)).await;
        d.reconcile().await;
        let listed = d.terminals().await.unwrap();
        assert!(
            listed
                .iter()
                .any(|s| s.sid == sid && s.status == SessionStatus::Working),
            "restart {n}: {listed:?}"
        );
    }
    wait_for(&d, sid, "done", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle && s.attention == Attention::Done)
    })
    .await;
    assert_eq!(
        context(&d, sid),
        Some(Context {
            used: 2,
            window: 200_000,
            compactions: 1,
            compact_at: None
        }),
        "one compaction, however often its line was read"
    );
    d.close_session(sid).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_prompt_survives_a_restart_and_runs_after_attach() {
    let b = Box_::new();
    let (mut d, mut stop) = b.start(None).await;
    let port = port_of(&d);
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    d.prompt(&pane, "slow").await.unwrap();
    wait_for(&d, sid, "working", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Working)
    })
    .await;
    // queued behind the running turn: the driver holds it, the daemon persists it
    d.prompt(&pane, "second").await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    drop(stop);
    drop(d);
    (d, stop) = b.start(Some(port)).await;
    d.reconcile().await;
    let _stop = stop;
    let v = wait_items(&d, sid, "the held prompt's turn", |v| {
        has_text(v, ItemKind::AssistantMessage, "echo: second")
    })
    .await;
    assert_eq!(
        v.iter()
            .filter(|i| i.kind == ItemKind::UserMessage)
            .map(text_of)
            .collect::<Vec<_>>(),
        ["slow", "second"],
        "{:#?}",
        shape(&v)
    );
    assert!(
        v.iter()
            .any(|i| i.kind == ItemKind::AssistantMessage && text_of(i).contains("tick 50")),
        "the first turn finished first: {:#?}",
        shape(&v)
    );
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    d.close_session(sid).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_that_dies_is_restarted_once_with_resume_and_fails_on_the_second_death() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    let pid1 = b.claude_pid();

    // first death: restarted with `--resume` after 2 s, the session goes on
    d.prompt(&pane, "crash").await.unwrap();
    let t0 = Instant::now();
    loop {
        let pid = b.claude_pid();
        if pid != pid1 && claude_processes(&b) == 1 {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "no restart");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        t0.elapsed() >= Duration::from_millis(1500),
        "restarted too soon: {:?}",
        t0.elapsed()
    );
    wait_for(&d, sid, "idle again", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    d.prompt(&pane, "hi").await.unwrap();
    wait_items(&d, sid, "a turn after the restart", |v| {
        has_text(v, ItemKind::AssistantMessage, "echo: hi")
    })
    .await;

    // second death within 30 s: the session fails, the stderr tail is on the transcript
    d.prompt(&pane, "crash").await.unwrap();
    let s = wait_for(&d, sid, "failed", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Failed)
    })
    .await
    .unwrap();
    assert_eq!(s.attention, Attention::None);
    let v = wait_items(&d, sid, "the stderr tail", |v| {
        v.iter()
            .any(|i| i.kind == ItemKind::Event && text_of(i).contains("fake claude: crash"))
    })
    .await;
    assert!(
        v.iter().all(|i| i.status != ItemStatus::InProgress),
        "{:#?}",
        shape(&v)
    );
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(claude_processes(&b), 0, "no third attempt");
    assert!(
        d.prompt(&pane, "hi").await.is_err(),
        "a failed session takes no prompt"
    );
    d.close_session(sid).await.unwrap();
}

/// The daemon comes back to a headless session whose agent died while it was away: the restart
/// policy applies from `reconcile`, with `--resume`, and the session goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_agent_found_at_reconcile_is_restarted_with_resume() {
    let b = Box_::new();
    let (d, stop) = b.start(None).await;
    let port = port_of(&d);
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    let pid1 = b.claude_pid();

    drop(stop);
    drop(d);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(std::process::Command::new("kill")
        .args(["-9", &pid1])
        .status()
        .unwrap()
        .success());
    let t0 = Instant::now();
    while claude_processes(&b) > 0 {
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "the killed agent lingers"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let (d, _stop) = b.start(Some(port)).await;
    d.reconcile().await;
    let t0 = Instant::now();
    loop {
        let pid = b.claude_pid();
        if pid != pid1 && claude_processes(&b) == 1 {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "no restart after reconcile"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    wait_for(&d, sid, "idle again", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    d.prompt(&pane, "hi").await.unwrap();
    wait_items(&d, sid, "a turn after the restart", |v| {
        has_text(v, ItemKind::AssistantMessage, "echo: hi")
    })
    .await;
    d.close_session(sid).await.unwrap();
}

/// P3's unrun criterion: twenty headless → TUI → headless round trips, never two agent processes,
/// each switch under three seconds, and the conversation goes on afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn twenty_headless_tui_takeover_cycles_never_run_two_processes_and_each_takes_under_three_seconds(
) {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    d.prompt(&pane, "hi").await.unwrap();
    wait_for(&d, sid, "done", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle && s.attention == Attention::Done)
    })
    .await;

    let mut took = Vec::new();
    for cycle in 1..=20 {
        for to in [Mode::Tui, Mode::Headless] {
            let t0 = Instant::now();
            d.takeover(sid, to).await.unwrap();
            loop {
                let n = claude_processes(&b);
                assert!(n <= 1, "cycle {cycle} → {to:?}: {n} agent processes");
                let s = d
                    .terminals()
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|s| s.sid == sid)
                    .unwrap_or_else(|| panic!("cycle {cycle}: session gone"));
                if s.mode == to && s.status == SessionStatus::Idle {
                    break;
                }
                assert!(
                    t0.elapsed() < Duration::from_secs(10),
                    "cycle {cycle} → {to:?}: stuck at {s:?}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            took.push(t0.elapsed());
            assert_eq!(claude_processes(&b), 1, "cycle {cycle} → {to:?}");
        }
        // the conversation is live in both modes
        if cycle % 5 == 0 {
            d.prompt(&pane, &format!("round {cycle}")).await.unwrap();
            wait_items(&d, sid, "the headless echo", |v| {
                has_text(
                    v,
                    ItemKind::AssistantMessage,
                    &format!("echo: round {cycle}"),
                )
            })
            .await;
            // the way to the terminal is refused while a turn runs: let this one end first
            wait_for(&d, sid, "the round's turn over", |s| {
                s.is_some_and(|s| s.status == SessionStatus::Idle && s.attention == Attention::Done)
            })
            .await;
        }
    }
    let max = took.iter().max().unwrap();
    let avg = took.iter().sum::<Duration>() / took.len() as u32;
    eprintln!("takeover: 40 switches, max {max:?}, avg {avg:?}");
    assert!(*max < Duration::from_secs(3), "slowest switch {max:?}");

    // and in the terminal: a prompt is a paste, the echo is on the screen
    d.takeover(sid, Mode::Tui).await.unwrap();
    wait_for(&d, sid, "tui idle", |s| {
        s.is_some_and(|s| s.mode == Mode::Tui && s.status == SessionStatus::Idle)
    })
    .await;
    d.prompt(&pane, "in the terminal").await.unwrap();
    wait_screen(&d, &pane, "echo: in the terminal").await;
    assert_eq!(claude_processes(&b), 1);
    d.close_session(sid).await.unwrap();
    wait_for(&d, sid, "closed", |s| s.is_none()).await;
}

/// The statuses of a transcript's question items, oldest first.
fn questions(v: &[Item]) -> Vec<ItemStatus> {
    v.iter()
        .filter(|i| i.kind == ItemKind::Question)
        .map(|i| i.status)
        .collect()
}

/// One `/input`, tried again while the actor is still replaying after a restart (`Busy`).
async fn input_when_ready(d: &Daemon, pane: &PaneRef, bytes: &[u8]) {
    let t0 = Instant::now();
    loop {
        match d.input(pane, bytes).await {
            Ok(()) => return,
            Err(CoreError::Busy) if t0.elapsed() < Duration::from_secs(10) => {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            Err(e) => panic!("input {bytes:?}: {e:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_question_with_two_parts_is_answered_part_by_part_through_input() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;

    d.prompt(&pane, "ask2").await.unwrap();
    wait_for(&d, sid, "blocked", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Blocked)
    })
    .await;
    let v = wait_items(&d, sid, "both parts", |v| questions(v).len() == 2).await;
    assert_eq!(questions(&v), [ItemStatus::Pending; 2]);
    let screen = d.screen(&pane, false).await.unwrap().text;
    assert!(
        screen.contains("? Which?") && screen.contains("? Second?"),
        "{screen}"
    );

    // the first `/input` answers the first part, and only that: the session still waits
    d.input(&pane, b"1").await.unwrap();
    let v = items(&d, sid);
    assert_eq!(
        questions(&v),
        [ItemStatus::Answered, ItemStatus::Pending],
        "{:#?}",
        shape(&v)
    );
    let s = d
        .terminals()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.sid == sid)
        .unwrap();
    assert_eq!(
        (s.status, s.attention),
        (SessionStatus::Blocked, Attention::Blocked)
    );
    let screen = d.screen(&pane, false).await.unwrap().text;
    assert!(screen.contains("? Which? -> a"), "{screen}");
    assert!(
        screen.contains("? Second?\n  1. x\n  2. y"),
        "the second part is asked still: {screen}"
    );

    // the second reaches the second part, and the agent goes on with both answers
    d.input(&pane, b"2").await.unwrap();
    wait_for(&d, sid, "done", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle && s.attention == Attention::Done)
    })
    .await;
    let v = wait_items(&d, sid, "the choices", |v| {
        has_text(v, ItemKind::AssistantMessage, "you chose: a, y")
    })
    .await;
    assert_eq!(questions(&v), [ItemStatus::Answered; 2], "{:#?}", shape(&v));
    let screen = d.screen(&pane, false).await.unwrap().text;
    assert!(
        screen.contains("? Which? -> a") && screen.contains("? Second? -> y"),
        "{screen}"
    );
    d.close_session(sid).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_part_answered_before_a_daemon_restart_stays_answered_and_the_next_one_completes_the_question(
) {
    let b = Box_::new();
    let (mut d, mut stop) = b.start(None).await;
    let port = port_of(&d);
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    d.prompt(&pane, "ask2").await.unwrap();
    wait_items(&d, sid, "both parts", |v| questions(v).len() == 2).await;
    d.input(&pane, b"1").await.unwrap();

    drop(stop);
    drop(d);
    (d, stop) = b.start(Some(port)).await;
    d.reconcile().await;
    let _stop = stop;
    let v = items(&d, sid);
    assert_eq!(
        questions(&v),
        [ItemStatus::Answered, ItemStatus::Pending],
        "{:#?}",
        shape(&v)
    );
    input_when_ready(&d, &pane, b"2").await;
    wait_items(&d, sid, "the choices", |v| {
        has_text(v, ItemKind::AssistantMessage, "you chose: a, y")
    })
    .await;
    d.close_session(sid).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_takeover_is_refused_while_a_turn_runs_and_goes_through_after_an_interrupt() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    let pid = b.claude_pid();

    d.prompt(&pane, "slow").await.unwrap();
    wait_for(&d, sid, "working", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Working)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(d.takeover(sid, Mode::Tui).await, Err(CoreError::Busy));
    // the refusal left the turn alone: the same process streams on, still headless
    tokio::time::sleep(Duration::from_secs(1)).await;
    let s = d
        .terminals()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.sid == sid)
        .unwrap();
    assert_eq!((s.mode, s.status), (Mode::Headless, SessionStatus::Working));
    assert_eq!((b.claude_pid(), claude_processes(&b)), (pid, 1));

    d.interrupt(sid).await.unwrap();
    wait_for(&d, sid, "idle after the interrupt", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    d.takeover(sid, Mode::Tui).await.unwrap();
    wait_for(&d, sid, "the terminal", |s| {
        s.is_some_and(|s| s.mode == Mode::Tui && s.status == SessionStatus::Idle)
    })
    .await;
    assert_eq!(claude_processes(&b), 1);
    d.close_session(sid).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_screen_shows_an_answer_while_it_is_still_streaming() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;

    d.prompt(&pane, "slow").await.unwrap();
    wait_screen(&d, &pane, "tick 3").await;
    let screen = d.screen(&pane, false).await.unwrap();
    assert_eq!(screen.status.as_deref(), Some("working"), "mid-turn");
    assert!(
        screen.text.contains("tick 1") && !screen.text.contains("tick 50"),
        "{}",
        screen.text
    );
    d.interrupt(sid).await.unwrap();
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    // the turn is over: the screen reads the item the store has
    assert!(d
        .screen(&pane, false)
        .await
        .unwrap()
        .text
        .contains("tick 3"));
    d.close_session(sid).await.unwrap();
}

/// What the held prompt of an agent that died was kept for: its next process. A terminal that
/// takes the session over first leaves no such process, and the prompt must not come back from
/// the next restart as a turn nobody asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_the_agent_dropped_when_it_died_does_not_run_after_a_takeover_and_a_restart() {
    let b = Box_::new();
    let (mut d, mut stop) = b.start(None).await;
    let port = port_of(&d);
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    let pid = b.claude_pid();
    d.prompt(&pane, "slow").await.unwrap();
    wait_for(&d, sid, "working", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Working)
    })
    .await;
    // held behind the turn; then the agent is killed, taking the turn and the held prompt with it
    d.prompt(&pane, "second").await.unwrap();
    assert!(std::process::Command::new("kill")
        .args(["-9", &pid])
        .status()
        .unwrap()
        .success());
    wait_items(&d, sid, "the dropped prompt's note", |v| {
        v.iter()
            .any(|i| i.kind == ItemKind::Event && text_of(i).contains("was not run"))
    })
    .await;

    // the restart is two seconds away: the terminal takes the session first, and gives it back
    d.takeover(sid, Mode::Tui).await.unwrap();
    d.takeover(sid, Mode::Headless).await.unwrap();
    wait_for(&d, sid, "headless again", |s| {
        s.is_some_and(|s| s.mode == Mode::Headless && s.status == SessionStatus::Idle)
    })
    .await;
    let held = |d: &Daemon| {
        let state = d
            .store
            .read(|c| femme_store::sessions::agent_state(c, sid))
            .unwrap();
        let saved: Value = serde_json::from_str(&state.unwrap().1).unwrap();
        saved["held"].as_array().unwrap().len()
    };
    assert_eq!(held(&d), 0, "nothing is saved for a restart");

    drop(stop);
    drop(d);
    (d, stop) = b.start(Some(port)).await;
    d.reconcile().await;
    let _stop = stop;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let v = items(&d, sid);
    // the saved message stays, and says the agent did not get it
    assert!(
        v.iter().any(|i| i.kind == ItemKind::UserMessage
            && text_of(i) == "second"
            && i.status == ItemStatus::Failed),
        "a turn nobody asked for: {:#?}",
        shape(&v)
    );

    assert_eq!(held(&d), 0);
    d.close_session(sid).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_with_attachments_saves_them_and_the_transcript_says_where() {
    use std::os::unix::fs::PermissionsExt;

    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(&d, sid, "idle after the handshake", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;

    let attach = |name: &str, data: &[u8]| Attachment {
        name: name.into(),
        blob: d.store.write(|tx| tx.put_blob_bytes(data)).unwrap(),
    };
    // two with the same name (one given as a path), and one that is not UTF-8
    let list = [
        attach("shot.png", b"\x89PNG one"),
        attach("../shot.png", b"\x89PNG two"),
        attach("my notes.bin", b"\xff\xfe\x00"),
    ];
    d.prompt_attached(&pane, "look", &list, PromptHow::Queue, None)
        .await
        .unwrap();

    let dir = b.dir.path().join("state/uploads").join(sid.to_string());
    let paths = ["shot.png", "shot-2.png", "my notes.bin"].map(|n| dir.join(n));
    for (path, data) in paths
        .iter()
        .zip([&b"\x89PNG one"[..], b"\x89PNG two", b"\xff\xfe\x00"])
    {
        assert_eq!(std::fs::read(path).unwrap(), data, "{}", path.display());
        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", path.display());
    }
    let lines: Vec<String> = paths
        .iter()
        .map(|p| format!("Attached: {}", p.display()))
        .collect();
    let want = format!("look\n\n{}", lines.join("\n"));
    // The user's message in the transcript is the text the agent got, lines included.
    let v = wait_items(&d, sid, "the echo", |v| {
        has_text(v, ItemKind::AssistantMessage, &format!("echo: {want}"))
    })
    .await;
    assert!(
        has_text(&v, ItemKind::UserMessage, &want),
        "{:#?}",
        shape(&v)
    );

    // A blob that is not there fails the prompt: Bad, no turn, nothing left of it on disk.
    wait_for(&d, sid, "idle again", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    let gone = Attachment {
        name: "gone.png".into(),
        blob: [9; 32],
    };
    let err = d
        .prompt_attached(
            &pane,
            "never",
            &[list[0].clone(), gone],
            PromptHow::Queue,
            None,
        )
        .await;
    assert!(
        matches!(&err, Err(CoreError::Bad(m)) if m.contains("gone.png")),
        "{err:?}"
    );
    assert!(!dir.join("shot-3.png").exists());
    assert!(!has_text(&items(&d, sid), ItemKind::UserMessage, "never"));
    d.close_session(sid).await.unwrap();
}

/// The texts of `kind`, in transcript order.
fn texts_of(v: &[Item], kind: ItemKind) -> Vec<String> {
    v.iter().filter(|i| i.kind == kind).map(text_of).collect()
}

/// A `slow` turn is running, a few ticks in.
async fn slow_turn(d: &Daemon, b: &Box_) -> (Sid, PaneRef) {
    let s = d.create_session(headless(&b.home())).await.unwrap();
    let (sid, pane) = (s.sid, pane_of(&s));
    wait_for(d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    d.prompt(&pane, "slow").await.unwrap();
    wait_for(d, sid, "working", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Working)
    })
    .await;
    // the store holds the streamed text only when the item is done: wait by the clock instead
    tokio::time::sleep(Duration::from_millis(700)).await;
    (sid, pane)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steer_mid_turn_joins_the_running_turn() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    let (sid, pane) = slow_turn(&d, &b).await;
    // the composer's Enter while working: written to the agent at once, folded into this turn
    d.prompt_attached(&pane, "steer", &[], PromptHow::Steer, None)
        .await
        .unwrap();
    let v = wait_items(&d, sid, "the steer's echo", |v| {
        has_text(v, ItemKind::UserMessage, "steer")
    })
    .await;
    assert!(
        !v.iter()
            .any(|i| i.kind == ItemKind::AssistantMessage && text_of(i).contains("tick 50")),
        "the echo came while the turn still ran: {:#?}",
        shape(&v)
    );
    let v = wait_items(&d, sid, "the turn's end", |v| {
        v.iter()
            .any(|i| i.kind == ItemKind::AssistantMessage && text_of(i).contains("echo: steer"))
    })
    .await;
    assert_eq!(texts_of(&v, ItemKind::UserMessage), ["slow", "steer"]);
    let answers = texts_of(&v, ItemKind::AssistantMessage);
    assert_eq!(answers.len(), 1, "one turn answered both: {:#?}", shape(&v));
    assert!(
        answers[0].contains("tick 50"),
        "never interrupted: {}",
        answers[0]
    );
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    d.close_session(sid).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_then_send_mid_turn_stops_the_turn_and_runs_the_prompt() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    let (sid, pane) = slow_turn(&d, &b).await;
    d.prompt_attached(&pane, "instead", &[], PromptHow::InterruptThenSend, None)
        .await
        .unwrap();
    let v = wait_items(&d, sid, "the new turn's answer", |v| {
        has_text(v, ItemKind::AssistantMessage, "echo: instead")
    })
    .await;
    assert_eq!(texts_of(&v, ItemKind::UserMessage), ["slow", "instead"]);
    let answers = texts_of(&v, ItemKind::AssistantMessage);
    assert_eq!(answers.len(), 2, "{:#?}", shape(&v));
    assert!(
        !answers[0].contains("tick 50"),
        "the slow turn was cut short: {}",
        answers[0]
    );
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    d.close_session(sid).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_core_prompt_path_still_waits_for_the_turn_to_end() {
    // board, API, worker: `Core::prompt` is a Queue, held until the running turn is over
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    let (sid, pane) = slow_turn(&d, &b).await;
    d.prompt(&pane, "second").await.unwrap();
    let v = wait_items(&d, sid, "the second turn", |v| {
        has_text(v, ItemKind::AssistantMessage, "echo: second")
    })
    .await;
    assert_eq!(texts_of(&v, ItemKind::UserMessage), ["slow", "second"]);
    let answers = texts_of(&v, ItemKind::AssistantMessage);
    assert_eq!(answers.len(), 2, "its own turn: {:#?}", shape(&v));
    assert!(answers[0].contains("tick 50"), "after the whole first turn");
    assert!(!answers[0].contains("echo: second"), "not folded in");
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    d.close_session(sid).await.unwrap();
}
