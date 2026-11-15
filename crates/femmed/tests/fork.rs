//! Forks end to end, with the fake `claude -p` under a real holder: a session that starts as a
//! copy of another conversation (`CreateSession::fork`), `/fork` typed in a chat, and what a
//! fork's next process resumes.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{headless, pane_of, wait_for, Box_};
use femme_proto::state::{Item, ItemKind, Session, SessionStatus};
use femme_proto::Sid;
use femme_store::{items, sessions};
use femmed::core::{Core, CoreError, CreateSession, SessionInfo};
use femmed::daemon::Daemon;
use serde_json::Value;

/// Conversation `uuid` on disk: one turn (`list it` / `one file`) on `claude-test-1`.
fn transcript(home: &Path, uuid: &str) {
    let proj = home.join(".claude/projects/-w");
    std::fs::create_dir_all(&proj).unwrap();
    let lines = [
        r#"{"type":"user","uuid":"m1","cwd":"/w","message":{"role":"user","content":"list it"}}"#,
        r#"{"type":"assistant","message":{"id":"a1","model":"claude-test-1","content":[{"type":"text","text":"one file"}]}}"#,
        r#"{"type":"system","subtype":"turn_duration","durationMs":12}"#,
    ];
    std::fs::write(proj.join(format!("{uuid}.jsonl")), lines.join("\n") + "\n").unwrap();
}

fn row(d: &Daemon, sid: Sid) -> Session {
    let got = d.store.read(|c| sessions::get_session(c, sid));
    got.unwrap().unwrap()
}

/// The session's transcript, oldest first, as (kind, text).
fn said(d: &Daemon, sid: Sid) -> Vec<(ItemKind, String)> {
    let mut v: Vec<Item> = d
        .store
        .read(|c| items::page_items(c, sid, None, 200))
        .unwrap();
    v.sort_by_key(|i| i.seq);
    v.iter()
        .map(|i| {
            let body: Value = serde_json::from_str(i.body.as_deref().unwrap_or("{}")).unwrap();
            (i.kind, body["text"].as_str().unwrap_or("").to_owned())
        })
        .collect()
}

async fn idle(d: &Daemon, sid: Sid) {
    wait_for(d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
}

/// The arguments of the `claude` the box started last.
fn argv(b: &Box_) -> Vec<String> {
    let cmd = std::fs::read(format!("/proc/{}/cmdline", b.claude_pid())).unwrap_or_default();
    String::from_utf8_lossy(&cmd)
        .split('\0')
        .map(str::to_owned)
        .collect()
}

fn fork_of(b: &Box_, parent: &str) -> CreateSession {
    CreateSession {
        label: None,
        resume: Some(parent.into()),
        fork: true,
        ..headless(&b.home())
    }
}

async fn fork(d: &Daemon, b: &Box_, parent: &str) -> SessionInfo {
    let s = d.create_session(fork_of(b, parent)).await.unwrap();
    idle(d, s.sid).await;
    s
}

/// A fork is a session of its own on a copy of the conversation: made while another Claude has
/// the original open, under a new id, named after the original, on its model, showing its
/// history. Its process forks while the copy has no transcript, and resumes the copy once it has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fork_is_a_copy_under_its_own_id_and_resumes_itself_once_it_has_a_transcript() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    transcript(&b.home(), "p-1");
    // a desktop's chat thinks at medium; a board session (as the fork is asked for here) at the
    // agent's default
    let parent = CreateSession {
        resume: Some("p-1".into()),
        owner: "device:ssh".into(),
        ..headless(&b.home())
    };
    let parent = d.create_session(parent).await.unwrap();
    idle(&d, parent.sid).await;
    // the original is open in a Claude (the note names our own, live, pid): resuming it again
    // is refused, copying it is not
    let notes = b.home().join(".claude/sessions");
    std::fs::create_dir_all(&notes).unwrap();
    let note = format!(r#"{{"pid":{},"sessionId":"p-1"}}"#, std::process::id());
    std::fs::write(notes.join("1.json"), note).unwrap();
    let again = CreateSession {
        fork: false,
        ..fork_of(&b, "p-1")
    };
    assert!(matches!(
        d.create_session(again).await,
        Err(CoreError::Conflict(_))
    ));

    let one = fork(&d, &b, "p-1").await;
    let r = row(&d, one.sid);
    let copy = r.agent_sid.clone().expect("the copy's id");
    assert_ne!(copy, "p-1");
    assert_eq!(r.label.as_deref(), Some("t · fork"));
    assert_eq!(r.cwd, parent.cwd);
    assert_eq!(row(&d, parent.sid).agent_sid.as_deref(), Some("p-1"));
    let args = argv(&b);
    for want in [
        "--resume=p-1".to_owned(),
        "--fork-session".to_owned(),
        format!("--session-id={copy}"),
        "--effort=medium".to_owned(),
        format!("--model={}", row(&d, parent.sid).model.unwrap()),
    ] {
        assert!(args.contains(&want), "{want} not in {args:?}");
    }
    let history = said(&d, one.sid);
    assert!(
        history.contains(&(ItemKind::UserMessage, "list it".into()))
            && history.contains(&(ItemKind::AssistantMessage, "one file".into())),
        "{history:?}"
    );

    // a second one gets the next name
    let two = fork(&d, &b, "p-1").await;
    assert_eq!(row(&d, two.sid).label.as_deref(), Some("t · fork 2"));
    assert_ne!(row(&d, two.sid).agent_sid, Some(copy.clone()));
    d.close_session(two.sid).await.unwrap();

    // The copy had its first turn (Claude Code wrote its transcript): the process that follows
    // the next death resumes the copy, and forks nothing.
    transcript(&b.home(), &copy);
    let pid = b.claude_pid();
    d.prompt(&pane_of(&one), "crash").await.unwrap();
    let t0 = Instant::now();
    while b.claude_pid() == pid {
        assert!(t0.elapsed() < Duration::from_secs(10), "no restart");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    idle(&d, one.sid).await;
    let args = argv(&b);
    assert!(args.contains(&format!("--resume={copy}")), "{args:?}");
    assert!(!args.iter().any(|a| a == "--fork-session"), "{args:?}");
    assert_eq!(row(&d, one.sid).agent_sid, Some(copy));
    d.close_session(one.sid).await.unwrap();
    d.close_session(parent.sid).await.unwrap();
}

/// What cannot be forked says so, and makes no session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fork_needs_a_claude_conversation_with_a_transcript() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    for bad in [
        fork_of(&b, "no-such"),
        CreateSession {
            mode: femme_proto::state::Mode::Tui,
            ..fork_of(&b, "no-such")
        },
        CreateSession {
            resume: None,
            ..fork_of(&b, "no-such")
        },
    ] {
        let got = d.create_session(bad).await;
        assert!(matches!(got, Err(CoreError::Bad(_))), "{got:?}");
    }
    let open = d.store.read(|c| sessions::list_sessions(c, false));
    assert!(open.unwrap().is_empty());
}

/// `/fork hello` typed in a chat: the chat's agent never sees it, a copy exists whose first
/// prompt is `hello`, and the chat says where it went.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fork_command_makes_the_copy_prompts_it_and_leaves_a_note() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    transcript(&b.home(), "p-2");
    let parent = CreateSession {
        resume: Some("p-2".into()),
        ..headless(&b.home())
    };
    let parent = d.create_session(parent).await.unwrap();
    idle(&d, parent.sid).await;
    let before = said(&d, parent.sid);

    d.prompt(&pane_of(&parent), "/fork hello").await.unwrap();
    let open = d.store.read(|c| sessions::list_sessions(c, false)).unwrap();
    let copy = open
        .iter()
        .find(|s| s.id != parent.sid)
        .expect("the copy's session");
    assert_eq!(copy.label.as_deref(), Some("t · fork"));
    assert_eq!(copy.owner, parent.owner);
    let t0 = Instant::now();
    let turn = (ItemKind::AssistantMessage, "echo: hello".to_owned());
    while !said(&d, copy.id).contains(&turn) {
        let got = said(&d, copy.id);
        assert!(t0.elapsed() < Duration::from_secs(20), "no turn: {got:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(said(&d, copy.id).contains(&(ItemKind::UserMessage, "hello".into())));

    let mut want = before;
    want.push((ItemKind::Event, "forked into t · fork".into()));
    assert_eq!(said(&d, parent.sid), want, "the note, and nothing else");
    assert_eq!(row(&d, parent.sid).status, SessionStatus::Idle);
    d.close_session(copy.id).await.unwrap();
    d.close_session(parent.sid).await.unwrap();
}
