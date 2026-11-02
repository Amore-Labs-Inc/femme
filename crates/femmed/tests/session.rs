//! A PTY-mode session end to end: a fake `claude` in a real `femme-holder`, its status through
//! `femmed hook`, prompt/input/screen through the daemon, and a daemon restart that finds the
//! holder again. Needs the `femme-holder` binary next to `femmed`'s (built here when missing).

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{claude, wait_for, wait_screen, Box_};
use femme_proto::rpc::IssueField;
use femme_proto::state::{Agent, Attention, IssueStatus, Mode, SessionStatus};
use femmed::core::{Core, CoreError, CreateSession, PaneRef};
use femmed::daemon::Daemon;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claude_session_reports_through_hooks_and_takes_prompts() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    let s = d.create_session(claude(&b.home())).await.unwrap();
    assert_eq!((s.status, s.handle.len()), (SessionStatus::Starting, 6));
    let pane = PaneRef {
        machine: None,
        handle: s.handle.clone(),
        no: 1,
    };
    let sid = s.sid;
    // the first hook (SessionStart) makes it idle
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;

    d.prompt(&pane, "hello").await.unwrap();
    let s = wait_for(&d, sid, "done", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle && s.attention == Attention::Done)
    })
    .await
    .unwrap();
    assert_eq!(s.agent_status(), Some("done"));
    let screen = d.screen(&pane, false).await.unwrap();
    assert!(screen.text.contains("echo: hello"), "{}", screen.text);
    assert_eq!(screen.status.as_deref(), Some("done"), "done shows once");
    assert_eq!(
        d.screen(&pane, false).await.unwrap().status.as_deref(),
        Some("idle")
    );
    assert!(d
        .screen(&pane, true)
        .await
        .unwrap()
        .text
        .contains("echo: hello"));

    // a question blocks it: prompts bounce, raw input answers
    d.prompt(&pane, "ask").await.unwrap();
    wait_for(&d, sid, "blocked", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Blocked && s.attention == Attention::Blocked)
    })
    .await;
    assert_eq!(d.prompt(&pane, "more").await, Err(CoreError::Busy));
    d.input(&pane, b"b\r").await.unwrap();
    wait_for(&d, sid, "answered", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    wait_screen(&d, &pane, "answered: b").await;

    // the agent's own exit closes the session and stops its holder
    d.prompt(&pane, "quit").await.unwrap();
    wait_for(&d, sid, "closed", |s| s.is_none()).await;
    let t0 = Instant::now();
    while b.socket(sid).exists() {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "holder socket still there"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(d.screen(&pane, false).await, Err(CoreError::NotFound));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_daemon_finds_its_sessions_and_closes_them() {
    let b = Box_::new();
    let (d1, stop1) = b.start(None).await;
    let port = d1
        .cfg
        .http_bind
        .rsplit(':')
        .next()
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let s = d1.create_session(claude(&b.home())).await.unwrap();
    let (sid, pane) = (
        s.sid,
        PaneRef {
            machine: None,
            handle: s.handle.clone(),
            no: 1,
        },
    );
    wait_for(&d1, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    d1.prompt(&pane, "before").await.unwrap();
    wait_screen(&d1, &pane, "echo: before").await;

    // the daemon goes away; the holder and the agent stay
    drop(stop1);
    drop(d1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(b.socket(sid).exists());

    let (d2, _stop2) = b.start(Some(port)).await;
    assert_eq!(
        d2.prompt(&pane, "x").await,
        Err(CoreError::Internal("the session's terminal is gone".into())),
        "not reattached yet"
    );
    d2.reconcile().await;
    let s = d2.terminals().await.unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].sid, sid);
    assert!(
        d2.screen(&pane, false)
            .await
            .unwrap()
            .text
            .contains("echo: before"),
        "same screen"
    );
    d2.prompt(&pane, "after").await.unwrap();
    // `done` first: reading the screen is what clears it
    wait_for(&d2, sid, "done", |s| {
        s.is_some_and(|s| s.attention == Attention::Done)
    })
    .await;
    assert!(d2
        .screen(&pane, false)
        .await
        .unwrap()
        .text
        .contains("echo: after"));

    d2.close_session(sid).await.unwrap();
    assert!(d2.terminals().await.unwrap().is_empty());
    assert_eq!(d2.close_session(sid).await, Err(CoreError::NotFound));
    let t0 = Instant::now();
    while b.socket(sid).exists() {
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "holder socket still there"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // a second start with nothing left: nothing to do, nothing listed
    d2.reconcile().await;
    assert!(d2.terminals().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shell_session_takes_input_and_an_agent_that_is_missing_never_leaves_a_row() {
    let b = Box_::new();
    let (d, _stop) = b.start(None).await;
    let sh = CreateSession {
        agent: Agent::Shell,
        ..claude(&b.home())
    };
    let s = d.create_session(sh).await.unwrap();
    assert_eq!((s.status, s.agent_status()), (SessionStatus::Idle, None));
    let pane = PaneRef {
        machine: None,
        handle: s.handle.clone(),
        no: 1,
    };
    assert_eq!(
        d.prompt(&pane, "hi").await,
        Err(CoreError::Busy),
        "a shell has no agent to prompt"
    );
    d.input(&pane, b"echo marker-$((40+2))\r").await.unwrap();
    let screen = wait_screen(&d, &pane, "marker-42").await;
    assert_eq!(d.screen(&pane, false).await.unwrap().status, None);
    assert!(!screen.contains("\u{1b}["), "plain text");
    d.close_session(s.sid).await.unwrap();

    // no `codex` on the fake PATH: a readable error and no session
    let r = d
        .create_session(CreateSession {
            agent: Agent::Codex,
            ..claude(&b.home())
        })
        .await;
    assert_eq!(
        r,
        Err(CoreError::Internal(
            "Codex did not start: codex: not found on PATH".into()
        ))
    );
    assert!(d.terminals().await.unwrap().is_empty());
    // a bad pane id, a pane of a closed session
    assert_eq!(d.input(&pane, b"x").await, Err(CoreError::NotFound));
    assert_eq!(
        d.screen(
            &PaneRef {
                machine: Some("instinct".into()),
                handle: s.handle,
                no: 1
            },
            false
        )
        .await,
        Err(CoreError::NotFound)
    );
}

/// The scheduler's loop over a real session: an open issue is cloned, briefed and `working`; a PR
/// moves it to `pr`; marked done, its session closes while the next issue starts.
#[tokio::test]
async fn an_open_issue_is_started_marked_pr_and_closed_when_done() {
    let bx = Box_::new();
    let (d, _stop) = bx.start(None).await;
    let issue = |d: &Arc<Daemon>, id: u64| {
        let d = d.clone();
        async move {
            d.issues()
                .await
                .unwrap()
                .into_iter()
                .find(|i| i.id == id)
                .unwrap()
        }
    };
    d.issue_create("acme/widgets", "Add the thing\n", "Do it.")
        .await
        .unwrap();
    femmed::worker::pass(&d).await;
    let i = issue(&d, 1).await;
    assert_eq!(
        (i.status, i.note.as_deref()),
        (IssueStatus::Working, None),
        "{i:?}"
    );
    let sid = i.session.expect("linked to its session");
    let s = d
        .terminals()
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.sid == sid)
        .unwrap();
    let dir = bx.home().join("femme-work/widgets-1");
    assert_eq!(
        (s.owner.as_str(), s.label.as_deref(), s.cwd.as_str()),
        ("issue:1", Some("#1 Add the thing"), dir.to_str().unwrap())
    );
    assert!(dir.join(".git").is_dir(), "cloned");
    let pane = PaneRef {
        machine: None,
        handle: s.handle.clone(),
        no: 1,
    };
    // issue sessions run headless: the screen is the transcript, and the whole brief is on it
    assert_eq!(s.mode, Mode::Headless);
    wait_screen(&d, &pane, "echo: You are working on FEMME").await;
    wait_screen(&d, &pane, "say so plainly and stop.").await;
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;

    // The agent opened a PR (gh says so): the next pass marks it.
    std::fs::write(
        bx.dir.path().join("pr-url"),
        "https://github.com/acme/widgets/pull/7\n",
    )
    .unwrap();
    femmed::worker::pass(&d).await;
    let i = issue(&d, 1).await;
    assert_eq!(
        (i.status, i.pr_url.as_deref()),
        (
            IssueStatus::Pr,
            Some("https://github.com/acme/widgets/pull/7")
        ),
        "{i:?}"
    );

    // Marked done on the board, with another issue queued: its session closes, the next starts.
    d.issue_patch(1, vec![IssueField::Status(IssueStatus::Done)])
        .await
        .unwrap();
    d.issue_create("acme/widgets", "Second", "").await.unwrap();
    femmed::worker::pass(&d).await;
    assert!(
        d.terminals().await.unwrap().iter().all(|s| s.sid != sid),
        "issue 1's session closed"
    );
    let i = issue(&d, 2).await;
    assert_eq!(i.status, IssueStatus::Working, "{i:?}");
    assert!(bx.home().join("femme-work/widgets-2/.git").is_dir());
}

/// M1 exit criterion: fifty daemon restarts in the middle of a turn restart no agent and lose no
/// output; the hooks that fire in the gaps reach the next daemon (`femmed hook` tries again).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fifty_daemon_restarts_mid_turn_restart_no_agent_and_lose_no_output() {
    let b = Box_::new();
    let (mut d, mut stop) = b.start(None).await;
    let port = d
        .cfg
        .http_bind
        .rsplit(':')
        .next()
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let s = d.create_session(claude(&b.home())).await.unwrap();
    let (sid, pane) = (
        s.sid,
        PaneRef {
            machine: None,
            handle: s.handle.clone(),
            no: 1,
        },
    );
    wait_for(&d, sid, "idle", |s| {
        s.is_some_and(|s| s.status == SessionStatus::Idle)
    })
    .await;
    let pid = std::fs::read_to_string(b.home().join("claude.pid")).unwrap();
    d.prompt(&pane, "slow").await.unwrap();
    wait_screen(&d, &pane, "tick 1").await;
    let t0 = Instant::now();
    for n in 1..=50 {
        drop(stop);
        drop(d);
        let next = b.start(Some(port)).await;
        (d, stop) = next;
        d.reconcile().await;
        let listed = d.terminals().await.unwrap();
        assert!(
            listed
                .iter()
                .any(|s| s.sid == sid && s.status == SessionStatus::Working),
            "restart {n}: {listed:?}"
        );
    }
    assert!(
        t0.elapsed() < Duration::from_secs(10),
        "fifty restarts took {:?}; the turn is 10 s",
        t0.elapsed()
    );
    wait_screen(&d, &pane, "tick 50").await;
    // the Stop hook after the last tick reaches the last daemon
    wait_for(&d, sid, "done", |s| {
        s.is_some_and(|s| s.attention == Attention::Done)
    })
    .await;
    let screen = d.screen(&pane, false).await.unwrap().text;
    for i in 20..=50 {
        assert!(
            screen.contains(&format!("tick {i}")),
            "tick {i} on the screen:\n{screen}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(b.home().join("claude.pid")).unwrap(),
        pid,
        "the same agent process"
    );
    d.close_session(sid).await.unwrap();
}
