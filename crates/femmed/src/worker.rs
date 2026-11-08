//! `server/worker.sh`'s loop: every 60 s, and whenever a session changes, a snapshot of the board
//! goes through [`scheduler::plan`] and its actions are carried out here, in order, one pass at a
//! time. What a pass itself changes does not start another (see [`crate::scheduler`]).

use std::sync::Weak;
use std::time::{Duration, Instant};

use femme_proto::state::{Agent, Issue, IssueStatus, Mode};
use femme_proto::Sid;

use crate::core::{Core, CoreError, CreateSession, PaneRef, SessionInfo};
use crate::daemon::{now_ms, Daemon};
use crate::scheduler::{
    self, Action, SchedInput, NOTE_NO_SESSION, NOTE_PROMPT_REFUSED, RESUME_PROMPT,
};
use crate::session::MAIN;

/// worker.sh's cadence.
const TICK: Duration = Duration::from_secs(60);
/// How long a fresh issue session gets to take its brief (worker.sh gave Claude 60 s).
const PROMPT_WAIT: Duration = Duration::from_secs(60);

/// Passes until the daemon is gone: one now, then one per [`TICK`] or session change.
pub async fn run(daemon: Weak<Daemon>) {
    let Some(wake) = daemon.upgrade().map(|d| d.sched_wake.clone()) else {
        return;
    };
    loop {
        let Some(d) = daemon.upgrade() else { return };
        pass(&d).await;
        drop(d);
        // The pass's own session changes are no reason for another: drop a wake that arrived meanwhile.
        let _ = tokio::time::timeout(Duration::ZERO, wake.notified()).await;
        tokio::select! { _ = wake.notified() => {}, _ = tokio::time::sleep(TICK) => {} }
    }
}

/// One pass: read the board, `/usage` and the PRs, plan, carry out.
pub async fn pass(d: &Daemon) {
    let (issues, sessions) = match (d.issues().await, d.terminals().await) {
        (Ok(i), Ok(s)) => (i, s),
        (Err(e), _) | (_, Err(e)) => return tracing::warn!(%e, "scheduler: cannot read the board"),
    };
    // One claude process a pass, only while there is work; the meta keeps the last reading meanwhile.
    if issues.iter().any(|i| scheduler::queued(i.status)) {
        let usage = scheduler::read_usage(&d.cfg).await.unwrap_or_else(|e| {
            tracing::warn!("scheduler: /usage: {e:#}");
            None
        });
        *d.usage.lock().unwrap_or_else(|e| e.into_inner()) = usage;
    }
    let usage = *d.usage.lock().unwrap_or_else(|e| e.into_inner());
    let mut prs = Vec::new();
    for i in issues
        .iter()
        .filter(|i| matches!(i.status, IssueStatus::Working | IssueStatus::Waiting))
    {
        match scheduler::find_pr(&i.repo, i.id, &d.cfg).await {
            Ok(Some(url)) => prs.push((i.id, url)),
            Ok(None) => {}
            Err(e) => tracing::warn!(issue = i.id, "scheduler: PR lookup: {e:#}"),
        }
    }
    let actions = scheduler::plan(&SchedInput {
        now: now_ms(),
        issues: &issues,
        sessions: &sessions,
        usage,
        prs: &prs,
        cfg: &d.cfg,
    });
    for action in actions {
        tracing::info!(?action, "scheduler");
        if let Err(e) = carry_out(d, action, &issues, &sessions).await {
            tracing::warn!(%e, "scheduler: action failed");
        }
    }
}

async fn carry_out(
    d: &Daemon,
    action: Action,
    issues: &[Issue],
    sessions: &[SessionInfo],
) -> Result<(), CoreError> {
    let issue_of = |id: u64| {
        issues
            .iter()
            .find(|i| i.id == id)
            .cloned()
            .ok_or(CoreError::NotFound)
    };
    let agent_of = |id: u64| {
        issue_of(id)
            .ok()
            .and_then(|i| i.session)
            .and_then(|sid| sessions.iter().find(|s| s.sid == sid))
    };
    match action {
        Action::Start { issue } => start(d, issue_of(issue)?).await,
        Action::MarkPr { issue, url } => patch(d, issue, |i| {
            i.status = IssueStatus::Pr;
            i.pr_url = Some(url);
            i.note = None;
        }),
        Action::MarkWaiting { issue, note } => patch(d, issue, |i| {
            i.status = IssueStatus::Waiting;
            i.note = Some(note);
        }),
        Action::MarkWorking { issue } => patch(d, issue, |i| {
            i.status = IssueStatus::Working;
            i.note = None;
        }),
        Action::Pause {
            issue,
            resume_at,
            note,
        } => {
            // worker.sh sent Esc; a headless agent is interrupted through its driver. One that
            // refuses (replaying, switching mode, starting) leaves the issue as it is: while the
            // agent works over the cap the next pass plans the pause again.
            if let Some(s) = agent_of(issue) {
                if s.mode == Mode::Headless {
                    d.interrupt(s.sid).await?;
                } else {
                    d.input(&pane_of(s), b"\x1b").await?;
                }
            }
            patch(d, issue, |i| {
                i.status = IssueStatus::Paused;
                i.note = Some(note);
                i.resume_at = Some(resume_at);
            })
        }
        Action::Resume { issue } => {
            // A refused prompt (the agent asks something) is the next pass's to see.
            if let Some(s) = agent_of(issue) {
                if let Err(e) = d.prompt(&pane_of(s), RESUME_PROMPT).await {
                    tracing::warn!(issue, %e, "scheduler: agent not resumed");
                }
            }
            patch(d, issue, |i| {
                i.status = IssueStatus::Working;
                i.note = None;
                i.resume_at = None;
            })
        }
        Action::Requeue { issue, note } => patch(d, issue, |i| {
            i.status = IssueStatus::Open;
            i.note = Some(note);
            i.session = None;
            i.resume_at = None;
        }),
        Action::Fail { issue, note } => patch(d, issue, |i| {
            i.status = IssueStatus::Failed;
            i.note = Some(note);
        }),
        Action::Close { issue } => match issue_of(issue)?.session {
            Some(sid) => d.close_session(sid).await,
            None => Ok(()),
        },
    }
}

/// worker.sh's `start`: a retry replaces the old attempt's session; clone; a Claude session in the
/// clone; the brief; then the issue is `working` with its session. A step that fails leaves the
/// issue `failed` with its note, and the session, when one opened, for the user to look at.
async fn start(d: &Daemon, issue: Issue) -> Result<(), CoreError> {
    let id = issue.id;
    if let Some(sid) = issue.session {
        let _ = d.close_session(sid).await;
    }
    let fail = |note: String, sid: Option<Sid>| {
        patch(d, id, move |i| {
            i.status = IssueStatus::Failed;
            i.note = Some(note);
            i.session = sid.or(i.session);
        })
    };
    let dir = match scheduler::prepare_clone(&issue, &d.cfg).await {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(issue = id, "scheduler: {e:#}");
            return fail(e.to_string(), None);
        }
    };
    let req = CreateSession {
        cwd: dir.display().to_string(),
        agent: Agent::Claude,
        mode: Mode::Headless,
        label: Some(scheduler::session_label(&issue)),
        name: None,
        owner: format!("issue:{id}"),
        model: None,
    };
    let session = match d.create_session(req).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(issue = id, %e, "scheduler: no session");
            return fail(NOTE_NO_SESSION.into(), None);
        }
    };
    let (pane, brief, t0) = (
        pane_of(&session),
        scheduler::issue_prompt(&issue, &d.cfg),
        Instant::now(),
    );
    loop {
        match d.prompt(&pane, &brief).await {
            Ok(()) => break,
            Err(CoreError::Busy) if t0.elapsed() < PROMPT_WAIT => {
                tokio::time::sleep(Duration::from_secs(1)).await
            }
            Err(e) => {
                tracing::warn!(issue = id, %e, "scheduler: brief refused");
                return fail(NOTE_PROMPT_REFUSED.into(), Some(session.sid));
            }
        }
    }
    patch(d, id, |i| {
        i.status = IssueStatus::Working;
        i.note = None;
        i.session = Some(session.sid);
        i.resume_at = None;
    })
}

/// The scheduler's issue edit: no base rev, never conflicts (`note: ""` clears).
fn patch(d: &Daemon, id: u64, f: impl FnOnce(&mut Issue)) -> Result<(), CoreError> {
    d.write(|tx, now| tx.update_issue(id, now, f)).map(drop)
}

/// The agent's pane of an issue session.
fn pane_of(s: &SessionInfo) -> PaneRef {
    PaneRef {
        machine: s.machine.clone(),
        handle: s.handle.clone(),
        no: MAIN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use femme_proto::state::{Attention, Session, SessionStatus};
    use femme_proto::Id;

    #[tokio::test]
    async fn a_pause_whose_interrupt_is_refused_leaves_the_issue_working_for_the_next_pass() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::from_vars([("HOME".to_string(), dir.path().display().to_string())]);
        cfg.state_dir = dir.path().join("state");
        let d = Daemon::open(cfg).unwrap();
        // A headless session nothing drives (no actor, as while a reattach fails): `Busy`.
        let sid = d
            .write(|tx, _| {
                Ok(tx
                    .insert_session(&Session {
                        id: Id(0),
                        handle: String::new(),
                        rev: 0,
                        name: None,
                        owner: "issue:1".into(),
                        agent: Agent::Claude,
                        mode: Mode::Headless,
                        cwd: "/".into(),
                        label: None,
                        agent_sid: None,
                        model: None,
                        status: SessionStatus::Working,
                        attention: Attention::None,
                        created: 0,
                        closed: None,
                        permission: None,
                        context: None,
                        effort: None,
                    })?
                    .id)
            })
            .unwrap();
        let issue = d.issue_create("o/r", "t", "b").await.unwrap();
        patch(&d, issue.id, |i| {
            i.status = IssueStatus::Working;
            i.session = Some(sid);
        })
        .unwrap();
        let (issues, sessions) = (d.issues().await.unwrap(), d.terminals().await.unwrap());
        let pause = || Action::Pause {
            issue: issue.id,
            resume_at: 5,
            note: "over".into(),
        };

        assert_eq!(
            carry_out(&d, pause(), &issues, &sessions).await,
            Err(CoreError::Busy)
        );
        assert_eq!(d.issues().await.unwrap()[0].status, IssueStatus::Working);
        // an issue with no agent to stop is paused all the same
        carry_out(&d, pause(), &issues, &[]).await.unwrap();
        let paused = &d.issues().await.unwrap()[0];
        assert_eq!(
            (paused.status, paused.resume_at),
            (IssueStatus::Paused, Some(5))
        );
    }
}
