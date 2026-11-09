//! `server/worker.sh`, the board's issue scheduler, ported: [`plan`] is one pass of its loop as a
//! pure function of a snapshot; [`prepare_clone`], [`find_pr`], [`read_usage`] and [`list_repos`]
//! are the commands it ran around that; [`meta_state`] and [`meta_json`] are what it wrote to
//! `meta.json`.
//!
//! The daemon does the IO. Each tick (every 60 s, and when a session's status changes) it takes
//! the issues and sessions, reads the 5-hour window ([`read_usage`]), looks for a PR for every
//! working or waiting issue ([`find_pr`]), calls [`plan`] and carries out the [`Action`]s in order,
//! then publishes [`meta_json`]. When `/usage` cannot be read, [`plan`] gets no reading, and the
//! meta shows the last one while the queue is empty and none otherwise, as worker.sh's `last_usage`
//! did.
//!
//! The daemon must not re-plan in reaction to the scheduler's own issue patches, only on session
//! changes and on the 60 s tick. [`plan`] is worker.sh's once-a-minute loop and leans on that gap:
//! - a failed `Start` patches its issue `failed`, which frees its slot. The slot is free on the
//!   next tick, not at once: worker.sh failed at most `PARALLEL` issues a minute, where a re-plan
//!   per patch would fail the whole queue in seconds when, say, claude is broken;
//! - right after a `Start`, `Resume` or `MarkWorking` the issue is `working` but its session may
//!   still read idle for a moment. Rule A6 gives the agent [`A6_GRACE_MS`] after the patch to
//!   report working, which covers the re-plans a session change brings in the meantime.
//!
//! Phone pushes are not the scheduler's: `crate::notify` (`Notifier::attention`) sends them when a
//! session's attention changes (blocked: "needs you", done: "finished"), for every session, issue
//! or not, so [`plan`] has no push action.
//!
//! All times in this module are unix milliseconds, like `femme-proto`'s.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::time::Duration;

use anyhow::{bail, Context};
use femme_proto::state::{Issue, IssueStatus, SessionStatus};
use serde_json::{json, Value};
use tokio::process::Command;

use crate::config::Config;
use crate::core::{agent_status, SessionInfo};

/// What a paused agent is told when it wakes (worker.sh's text).
pub const RESUME_PROMPT: &str = "The usage window has reset. Continue where you left off.";
/// A failed start's note when the session did not take [`issue_prompt`] (worker.sh's text).
pub const NOTE_PROMPT_REFUSED: &str =
    "Claude did not take the prompt; open the workspace to see why";
/// A failed start's note when no session could be opened (worker.sh: "herdr could not open a workspace").
pub const NOTE_NO_SESSION: &str = "could not open a session";

/// A failed start's note when the repo could not be cloned (worker.sh's text).
pub fn note_clone_failed(repo: &str) -> String {
    format!("could not clone {repo}")
}

/// How long after an issue's last patch rule A6 (see [`plan`]) leaves a `working` issue alone whose
/// agent reads idle or done, in ms. `Start`, `Resume` and `MarkWorking` patch the issue to `working`
/// a moment before the agent says so: its session still reads idle while it takes the prompt, and a
/// re-plan on a session change in that moment would mark the issue waiting ("Claude stopped
/// without a PR"). [`Issue::updated`] is set by every patch, so a fresh one gives the agent this
/// long to report working, as worker.sh's 60 s cadence did.
pub const A6_GRACE_MS: i64 = 60_000;

// Generous: worker.sh had none, so a hung tool could stall it forever.
const CLONE_TIMEOUT: Duration = Duration::from_secs(600);
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(120);
const PR_TIMEOUT: Duration = Duration::from_secs(30);
const USAGE_TIMEOUT: Duration = Duration::from_secs(60);
const LIST_TIMEOUT: Duration = Duration::from_secs(60);

/// Claude's 5-hour ("session") usage window, as `/usage` reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageSnapshot {
    /// Percent of the window used.
    pub percent: f64,
    /// When the window resets (unix ms), if `/usage` said so in a form [`parse_usage`] reads.
    pub resets_at: Option<i64>,
}

/// The snapshot [`plan`] decides from.
#[derive(Debug, Clone, Copy)]
pub struct SchedInput<'a> {
    /// Unix ms.
    pub now: i64,
    /// Every issue, any order.
    pub issues: &'a [Issue],
    /// The sessions the daemon knows; an issue's is the one [`Issue::session`] names.
    pub sessions: &'a [SessionInfo],
    /// The 5-hour window; `None` when `/usage` could not be read, which counts as 0% for `CAP` (as in
    /// worker.sh). Whether there is a reading also decides how paused issues wake:
    /// - with a reading, every paused issue is resumed (or requeued) as soon as the box is under
    ///   `CAP`, whatever its `resume_at`, as worker.sh did: the usage can fall before the reset (an
    ///   account rotation), and an agent left asleep while open issues start would wake on top of them;
    /// - with none, a paused issue whose `resume_at` is still ahead is held, where worker.sh woke it
    ///   into a window it could not see; it keeps its slot while its agent is live.
    pub usage: Option<UsageSnapshot>,
    /// `(issue id, PR url)` for the working and waiting issues [`find_pr`] found a PR for.
    pub prs: &'a [(u64, String)],
    /// `CAP` and `PARALLEL`.
    pub cfg: &'a Config,
}

/// One step for the daemon to carry out; [`plan`] returns them in order. "Patch" is an issue
/// update; a `note` of `""` clears the note. Phone pushes are not actions: `crate::notify` sends
/// them when a session's attention changes, not the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Work an open issue, as worker.sh's `start`: close the issue's old session if it has one (a
    /// retry replaces the old attempt), [`prepare_clone`], open a Claude session in the clone
    /// labelled [`session_label`], wait until it takes a prompt (worker.sh gave the shell 15 s and
    /// Claude 60 s), send it [`issue_prompt`], then link the session to the issue and patch
    /// `{status: working, note: ""}`. A step that fails patches `{status: failed}` with its note
    /// instead: [`note_clone_failed`] (what [`prepare_clone`]'s error displays), [`NOTE_NO_SESSION`]
    /// or [`NOTE_PROMPT_REFUSED`].
    Start { issue: u64 },
    /// The agent opened a PR: patch `{status: pr, pr_url: url, note: ""}`.
    MarkPr { issue: u64, url: String },
    /// The agent stopped or asks something: patch `{status: waiting, note}`.
    MarkWaiting { issue: u64, note: String },
    /// The agent works again (the user answered in Chat): patch `{status: working, note: ""}`.
    MarkWorking { issue: u64 },
    /// Over `CAP`: interrupt the agent (worker.sh sent Esc; a headless one gets `interrupt`), then
    /// patch `{status: paused, note}` and set `resume_at`, when the window resets. With a reading the
    /// issue wakes as soon as the box is under `CAP` again, `resume_at` or not; without one it
    /// sleeps until `resume_at` (see [`SchedInput::usage`]).
    Pause {
        issue: u64,
        resume_at: i64,
        note: String,
    },
    /// The box is under `CAP` again (the window reset, or the account changed): prompt the agent
    /// with [`RESUME_PROMPT`], then patch `{status: working, note: ""}`.
    Resume { issue: u64 },
    /// A paused issue whose session is gone goes back to the queue: patch `{status: open, note}`.
    Requeue { issue: u64, note: String },
    /// Patch `{status: failed, note}`.
    Fail { issue: u64, note: String },
    /// The issue was marked done on the board: close its session.
    Close { issue: u64 },
}

/// One pass of worker.sh's loop over a snapshot: what to do, in order. Pure.
///
/// `pct` is the window's percent, floored (0 without a reading); the box is *over* when
/// `pct >= CAP`. An issue's agent is *gone* when the issue has no session, or its session is
/// unknown, closed, interrupted, crashed or failed; otherwise its state is herdr's word for it,
/// `working`, `blocked`, `done` or `idle` ([`agent_status`]).
///
/// - No issue open, working, waiting or paused: nothing at all.
/// - **A.** Working and waiting issues, lowest id first; the first rule that matches wins:
///   1. a PR and the agent not working: [`Action::MarkPr`];
///   2. the agent gone: [`Action::Fail`];
///   3. the agent working, over: [`Action::Pause`] until the reset;
///   4. the agent working, the issue waiting: [`Action::MarkWorking`];
///   5. the agent blocked, the issue working: [`Action::MarkWaiting`];
///   6. the agent done or idle, the issue working, unless the issue was patched less than
///      [`A6_GRACE_MS`] ago (a `Start`, `Resume` or `MarkWorking` the agent has not caught up with
///      yet): [`Action::MarkWaiting`].
/// - Over: stop here.
/// - **B.** Paused issues, lowest id first: [`Action::Resume`] when the agent is live, else
///   [`Action::Requeue`]. With a reading every one, whatever its `resume_at`, as in worker.sh: the
///   box is under `CAP`, whether the window reset or an account rotation dropped the usage early,
///   and an agent left asleep while open issues start would later wake on top of them. Without a
///   reading (`usage` is `None`; worker.sh read that as 0% and woke everyone blind) a paused issue
///   whose `resume_at` is still ahead is *held*: neither resumed nor requeued, as the window it
///   would wake into cannot be seen.
/// - **C.** Open issues (one requeued in B too), lowest id first: [`Action::Start`] while fewer
///   than `PARALLEL` issues are running: the working ones, counting what A and B just moved, and
///   the held ones of B whose agent is live. A held issue whose agent is gone counts as nothing.
/// - **D.** Done issues whose agent is live: [`Action::Close`].
pub fn plan(input: &SchedInput) -> Vec<Action> {
    let mut issues: Vec<&Issue> = input.issues.iter().collect();
    issues.sort_by_key(|i| i.id);
    if !issues.iter().any(|i| queued(i.status)) {
        return Vec::new();
    }
    let pct = percent(input.usage);
    let over = pct >= i64::from(input.cfg.cap);
    let resume_at = input.usage.and_then(|u| u.resets_at).unwrap_or(input.now);
    // Each issue's status once the actions so far are carried out: worker.sh re-read the board
    // between phases.
    let mut status: HashMap<u64, IssueStatus> = issues.iter().map(|i| (i.id, i.status)).collect();
    let mut out = Vec::new();

    // A: opened a PR, went over budget, stopped to ask you, or got your reply.
    for issue in issues
        .iter()
        .filter(|i| matches!(i.status, IssueStatus::Working | IssueStatus::Waiting))
    {
        let (id, working, waiting) = (
            issue.id,
            issue.status == IssueStatus::Working,
            issue.status == IssueStatus::Waiting,
        );
        let agent = agent_of(issue, input.sessions);
        let st = agent.map_or("", |(st, _)| st); // worker.sh's `$st`: empty when the agent is gone
        let pr = input.prs.iter().find(|(i, _)| *i == id).map(|(_, url)| url);
        // New in femmed: the agent's report can lag the patch that made the issue working.
        let fresh = input.now.saturating_sub(issue.updated) < A6_GRACE_MS;
        let action = if let Some(url) = pr.filter(|_| st != "working") {
            Action::MarkPr {
                issue: id,
                url: url.clone(),
            }
        } else if let Err(why) = agent {
            Action::Fail {
                issue: id,
                note: why.into(),
            }
        } else if st == "working" && over {
            let note = format!("paused at {pct}% of the 5-hour window, continues after it resets");
            Action::Pause {
                issue: id,
                resume_at,
                note,
            }
        } else if st == "working" && waiting {
            Action::MarkWorking { issue: id }
        } else if st == "blocked" && working {
            Action::MarkWaiting {
                issue: id,
                note: "Claude is asking for your approval: open Chat".into(),
            }
        } else if (st == "done" || st == "idle") && working && !fresh {
            Action::MarkWaiting {
                issue: id,
                note: "Claude stopped without a PR: read its last message in Chat".into(),
            }
        } else {
            continue;
        };
        record(&mut out, &mut status, action);
    }
    if over {
        return out;
    }

    // B: wake paused agents; an issue whose agent is gone goes back to the queue. Every one, as in
    // worker.sh, whatever its resume_at.
    let mut held_live = 0; // the held issues whose agents keep their slots in C
    for issue in &issues {
        if status[&issue.id] != IssueStatus::Paused {
            continue;
        }
        let agent = agent_of(issue, input.sessions);
        // New in femmed (worker.sh had no resume_at): with no reading, a paused issue waits for its time.
        if input.usage.is_none() && issue.resume_at.is_some_and(|t| input.now < t) {
            held_live += usize::from(agent.is_ok());
            continue;
        }
        let action = match agent {
            Ok(_) => Action::Resume { issue: issue.id },
            Err(_) => Action::Requeue {
                issue: issue.id,
                note: String::new(),
            },
        };
        record(&mut out, &mut status, action);
    }

    // C: start queued issues while there is room.
    let mut running = status
        .values()
        .filter(|s| **s == IssueStatus::Working)
        .count()
        + held_live;
    for issue in &issues {
        if status[&issue.id] != IssueStatus::Open {
            continue;
        }
        if running >= input.cfg.parallel {
            break;
        }
        record(&mut out, &mut status, Action::Start { issue: issue.id });
        running += 1; // even if the start fails, as in worker.sh
    }

    // D: issues marked done on the board: close their sessions.
    for issue in issues.iter().filter(|i| i.status == IssueStatus::Done) {
        if agent_of(issue, input.sessions).is_ok() {
            out.push(Action::Close { issue: issue.id });
        }
    }
    out
}

/// The statuses that keep the queue busy; an issue in any other one is history.
pub(crate) fn queued(s: IssueStatus) -> bool {
    matches!(
        s,
        IssueStatus::Open | IssueStatus::Working | IssueStatus::Waiting | IssueStatus::Paused
    )
}

/// worker.sh's `pct`: `.percent // 0 | floor`.
fn percent(usage: Option<UsageSnapshot>) -> i64 {
    usage.map_or(0, |u| u.percent.floor() as i64)
}

/// worker.sh's `agent_status` for an issue: `Ok((state, session))` while its agent is live, the
/// state in herdr's words; `Err(note)` when it is gone, the note saying why for the failed issue.
fn agent_of<'a>(
    issue: &Issue,
    sessions: &'a [SessionInfo],
) -> Result<(&'static str, &'a SessionInfo), &'static str> {
    let Some(s) = issue
        .session
        .and_then(|sid| sessions.iter().find(|s| s.sid == sid))
    else {
        return Err("its workspace was closed");
    };
    match s.status {
        SessionStatus::Starting
        | SessionStatus::Idle
        | SessionStatus::Working
        | SessionStatus::Blocked
        | SessionStatus::Paused => Ok((agent_status(s.status, s.attention), s)),
        SessionStatus::Closed => Err("its workspace was closed"),
        // Additions: a herdr agent either existed or did not.
        SessionStatus::Interrupted => Err("its session was interrupted: retry to start it again"),
        SessionStatus::Crashed => Err("its session crashed"),
        SessionStatus::Failed => Err("its session failed"),
    }
}

/// Appends `action`, keeping `status` at what the board says once it is carried out.
fn record(out: &mut Vec<Action>, status: &mut HashMap<u64, IssueStatus>, action: Action) {
    let next = match &action {
        Action::Start { issue } | Action::MarkWorking { issue } | Action::Resume { issue } => {
            Some((*issue, IssueStatus::Working))
        }
        Action::MarkPr { issue, .. } => Some((*issue, IssueStatus::Pr)),
        Action::MarkWaiting { issue, .. } => Some((*issue, IssueStatus::Waiting)),
        Action::Pause { issue, .. } => Some((*issue, IssueStatus::Paused)),
        Action::Requeue { issue, .. } => Some((*issue, IssueStatus::Open)),
        Action::Fail { issue, .. } => Some((*issue, IssueStatus::Failed)),
        Action::Close { .. } => None,
    };
    if let Some((id, s)) = next {
        status.insert(id, s);
    }
    out.push(action);
}

/// The `state` line worker.sh wrote to `meta.json` after a pass (the board shows it).
pub fn meta_state(issues: &[Issue], usage: Option<UsageSnapshot>, cfg: &Config) -> String {
    let pct = percent(usage);
    if !issues.iter().any(|i| queued(i.status)) {
        "idle: queue is empty".into()
    } else if pct >= i64::from(cfg.cap) {
        format!("paused until the window resets ({pct}% used)")
    } else {
        format!("working ({pct}% used, pause at {}%)", cfg.cap)
    }
}

/// `meta.json` as worker.sh wrote it: `{state, usage: {percent, resets_at} | null, cap, repos,
/// updated}`, with `resets_at` and `updated` in unix seconds (worker.sh copied `/usage`'s
/// `resets_at` text). The board reads `state`, `cap` and `repos`.
pub fn meta_json(
    state: &str,
    usage: Option<UsageSnapshot>,
    cfg: &Config,
    repos: &[String],
    now: i64,
) -> Value {
    let usage = usage.map(
        |u| json!({ "percent": u.percent, "resets_at": u.resets_at.map(|t| t.div_euclid(1000)) }),
    );
    json!({ "state": state, "usage": usage, "cap": cfg.cap, "repos": repos, "updated": now.div_euclid(1000) })
}

/// The first message of an issue's session, byte for byte what worker.sh sent. Like bash's
/// `$(...)` it drops trailing newlines from the title and the body; an empty body is `(none)`.
/// `_cfg` is unused: the prompt reads no setting.
pub fn issue_prompt(issue: &Issue, _cfg: &Config) -> String {
    let (id, repo, title) = (issue.id, &issue.repo, issue.title.trim_end_matches('\n'));
    let body = match issue.body.trim_end_matches('\n') {
        "" => "(none)",
        body => body,
    };
    format!(
        "You are working on FEMME board issue #{id} for {repo}, cloned here on branch femme/issue-{id}.\n\
         \n\
         Title: {title}\n\
         \n\
         Details:\n\
         {body}\n\
         \n\
         Do it end to end: implement, run the project's checks and tests, commit, push the branch \
         (git push -u origin femme/issue-{id}), then open a PR with gh pr create whose body starts with \
         \"FEMME issue #{id}\" and says what changed and how you verified it. If you are blocked on a \
         decision only the owner can make, say so plainly and stop."
    )
}

/// The issue session's label, as worker.sh named the workspace: `#<id> <first 40 characters of
/// the title>`.
pub fn session_label(issue: &Issue) -> String {
    format!(
        "#{} {}",
        issue.id,
        issue
            .title
            .trim_end_matches('\n')
            .chars()
            .take(40)
            .collect::<String>()
    )
}

/// The branch an issue is worked on: `femme/issue-<id>`.
pub fn branch(issue: u64) -> String {
    format!("femme/issue-{issue}")
}

/// An issue's clone: `<home>/femme-work/<repo name>-<id>`, the name being what follows the
/// owner's slash (worker.sh's `${repo#*/}`).
pub fn work_dir(cfg: &Config, repo: &str, issue: u64) -> PathBuf {
    let name = repo.split_once('/').map_or(repo, |(_, name)| name);
    cfg.work_root().join(format!("{name}-{issue}"))
}

/// worker.sh's `start` up to the session: clones the issue's repo into [`work_dir`] unless a clone
/// is there (`gh repo clone <repo> <dir> -- --quiet`), then puts it on [`branch`]
/// (`git -C <dir> checkout -q -B femme/issue-<id>`, whose failure is ignored, as in worker.sh).
/// The error displays as [`note_clone_failed`], the failed issue's note; `{:#}` adds the cause.
pub async fn prepare_clone(issue: &Issue, cfg: &Config) -> anyhow::Result<PathBuf> {
    let failed = || note_clone_failed(&issue.repo);
    std::fs::create_dir_all(cfg.work_root()).with_context(failed)?;
    let dir = work_dir(cfg, &issue.repo, issue.id);
    if !dir.join(".git").is_dir() {
        let mut clone = cmd(cfg, "gh");
        clone
            .args(["repo", "clone", &issue.repo])
            .arg(&dir)
            .args(["--", "--quiet"])
            .stdout(Stdio::null());
        run_ok(&mut clone, CLONE_TIMEOUT)
            .await
            .with_context(failed)?;
    }
    let mut checkout = cmd(cfg, "git");
    checkout
        .arg("-C")
        .arg(&dir)
        .args(["checkout", "-q", "-B", &branch(issue.id)])
        .stdout(Stdio::null());
    let _ = run(&mut checkout, CHECKOUT_TIMEOUT).await;
    Ok(dir)
}

/// worker.sh's `pr_for`: the URL of a PR in any state from the issue's [`branch`]
/// (`gh pr list --repo <repo> --head femme/issue-<id> --state all --json url -q '.[0].url'`).
/// `Err` when gh cannot run, fails or takes over 30 s. worker.sh read any failure as "no PR",
/// so callers log it and go on without a PR for that issue.
pub async fn find_pr(repo: &str, issue: u64, cfg: &Config) -> anyhow::Result<Option<String>> {
    let mut c = cmd(cfg, "gh");
    c.args([
        "pr",
        "list",
        "--repo",
        repo,
        "--head",
        &branch(issue),
        "--state",
        "all",
        "--json",
        "url",
        "-q",
        ".[0].url",
    ]);
    let out = run_ok(&mut c, PR_TIMEOUT).await?;
    let url = out.trim();
    Ok((!url.is_empty() && url != "null").then(|| url.to_string()))
}

/// worker.sh's `usage`: `claude -p /usage --output-format stream-json --verbose
/// --no-session-persistence`, run in `/tmp` and read with [`parse_usage`]. `/usage` is a local
/// command, not a model turn. Its exit status is not looked at (worker.sh piped it into jq):
/// `Ok(None)` when it printed no window; `Err` when claude cannot run or takes over 60 s.
// ponytail: one claude process per tick; switch to the agents' RateLimit events once they prove sufficient (plan §5).
pub async fn read_usage(cfg: &Config) -> anyhow::Result<Option<UsageSnapshot>> {
    let mut c = cmd(cfg, "claude");
    c.args([
        "-p",
        "/usage",
        "--output-format",
        "stream-json",
        "--verbose",
        "--no-session-persistence",
    ])
    .current_dir("/tmp");
    let out = run(&mut c, USAGE_TIMEOUT).await?;
    Ok(parse_usage(&String::from_utf8_lossy(&out.stdout)))
}

/// The window in `claude -p /usage --output-format stream-json` output, as worker.sh's jq read
/// it: the first limit of kind `session` in an `assistant` line's
/// `usage_report.rate_limits.limits`. `None` without one, or when its `percent` is not a number
/// (worker.sh read that as 0%, which is what [`plan`] does with `None`). A line that is not JSON
/// is skipped, where jq would have stopped reading.
pub fn parse_usage(stdout: &str) -> Option<UsageSnapshot> {
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["type"] != "assistant" {
            continue;
        }
        let limits = v["usage_report"]["rate_limits"]["limits"].as_array();
        if let Some(l) = limits
            .into_iter()
            .flatten()
            .find(|l| l["kind"] == "session")
        {
            return Some(UsageSnapshot {
                percent: l["percent"].as_f64()?,
                resets_at: time_ms(&l["resets_at"]),
            });
        }
    }
    None
}

/// The repos the board offers for new issues, as worker.sh's `write_meta` listed them: yours, then
/// those of every org you are in (`gh api user/orgs --jq '.[].login'`, then
/// `gh repo list [<owner>] --limit 300 --json nameWithOwner -q '.[].nameWithOwner'`). A call that
/// fails adds nothing. The caller caches the list (worker.sh kept it an hour).
// ponytail: 300 repos per owner, as worker.sh; page through `gh api` if someone has more.
pub async fn list_repos(cfg: &Config) -> Vec<String> {
    let orgs = gh_out(cfg, &["api", "user/orgs", "--jq", ".[].login"]).await;
    let mut repos = Vec::new();
    for owner in std::iter::once("").chain(orgs.split_whitespace()) {
        let mut args = vec!["repo", "list"];
        if !owner.is_empty() {
            args.push(owner);
        }
        args.extend([
            "--limit",
            "300",
            "--json",
            "nameWithOwner",
            "-q",
            ".[].nameWithOwner",
        ]);
        repos.extend(
            gh_out(cfg, &args)
                .await
                .split('\n')
                .filter(|l| !l.is_empty())
                .map(String::from),
        );
    }
    repos
}

/// What `gh <args>` prints, or nothing when it fails.
async fn gh_out(cfg: &Config, args: &[&str]) -> String {
    let mut c = cmd(cfg, "gh");
    c.args(args);
    run_ok(&mut c, LIST_TIMEOUT).await.unwrap_or_else(|e| {
        tracing::debug!("gh {}: {e:#}", args.join(" "));
        String::new()
    })
}

/// A tool run as worker.sh ran it, minus the shell: `PATH` from [`Config::tool_path`] (which also
/// finds the program), no stdin, stdout captured, stderr discarded, killed when dropped.
fn cmd(cfg: &Config, program: &str) -> Command {
    let mut c = Command::new(program);
    if let Some(path) = &cfg.tool_path {
        c.env("PATH", path);
    }
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    c
}

/// Runs `c` to its end, killing it after `limit`. `Err` when it cannot start or times out; its
/// exit status is the caller's to judge.
async fn run(c: &mut Command, limit: Duration) -> anyhow::Result<Output> {
    let name = c.as_std().get_program().to_string_lossy().into_owned();
    let child = c.spawn().with_context(|| format!("could not run {name}"))?;
    match tokio::time::timeout(limit, child.wait_with_output()).await {
        Ok(out) => out.with_context(|| format!("{name} failed")),
        Err(_) => bail!("{name} timed out after {limit:?}"),
    }
}

/// [`run`], where a non-zero exit is an error too; returns stdout.
async fn run_ok(c: &mut Command, limit: Duration) -> anyhow::Result<String> {
    let out = run(c, limit).await?;
    if !out.status.success() {
        bail!(
            "{} exited with {}",
            c.as_std().get_program().to_string_lossy(),
            out.status
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A `resets_at` as unix ms: RFC 3339 text, or a number (from 1e12 up already ms, else seconds).
fn time_ms(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => rfc3339_ms(s),
        Value::Number(n) => n.as_f64().map(|n| {
            if n >= 1e12 {
                n as i64
            } else {
                (n * 1000.0) as i64
            }
        }),
        _ => None,
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.fraction](Z|+HH:MM|-HH:MM)` as unix ms, the fraction cut to ms.
// ponytail: the RFC 3339 that /usage prints, ranges checked loosely (Feb 31 rolls into March); a date crate if more shows up.
pub(crate) fn rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |from: usize, to: usize| s.get(from..to).and_then(digits);
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, min, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut ms = 0;
    if let Some(frac) = rest.strip_prefix('.') {
        let n = frac.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        ms = frac
            .bytes()
            .take(n)
            .chain(std::iter::repeat(b'0'))
            .take(3)
            .fold(0, |ms, d| ms * 10 + i64::from(d - b'0'));
        rest = &frac[n..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first() {
                Some(b'+') => 1,
                Some(b'-') => -1,
                _ => return None,
            };
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            let (h, m) = (
                rest.get(1..3).and_then(digits)?,
                rest.get(4..6).and_then(digits)?,
            );
            if h > 23 || m > 59 {
                return None;
            }
            sign * (h * 60 + m)
        }
    };
    let minutes = (days_from_civil(year, month, day) * 24 + hour) * 60 + min - offset;
    Some(minutes * 60_000 + sec * 1000 + ms)
}

/// An all-digits string's value.
fn digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Days from 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1; // [0, 365], from March 1
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use femme_proto::state::{
        Agent, Attention, IssueStatus as I, Mode, PaneKind, SessionStatus as S,
    };
    use femme_proto::Id;

    use super::*;
    use crate::core::PaneInfo;
    use crate::testutil::install_tool;

    const NOW: i64 = 1_793_610_000_000; // 2026-11-02T09:00:00Z
    const RESET: i64 = 1_793_617_200_000; // 2026-11-02T11:00:00Z, when the window resets
    /// How long before NOW the fixture issues were last patched: past `A6_GRACE_MS`, so the tests of
    /// the other rules need not think about the grace. The tests of the grace set `updated`.
    const PATCHED_AGO: i64 = 120_000;
    const PR: &str = "https://github.com/example-org/example-repo/pull/7";
    const CLOSED: &str = "its workspace was closed";
    const ASKING: &str = "Claude is asking for your approval: open Chat";
    const STOPPED: &str = "Claude stopped without a PR: read its last message in Chat";

    fn cfg(vars: &[(&str, &str)]) -> Config {
        Config::from_vars(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())))
    }

    /// Issue `id` of example-org/example-repo in `status`, worked in session `sid` (0: none), last
    /// patched `PATCHED_AGO` before NOW.
    fn issue(id: u64, status: IssueStatus, sid: u128) -> Issue {
        Issue {
            id,
            rev: 1,
            repo: "example-org/example-repo".into(),
            title: format!("issue {id}"),
            body: String::new(),
            status,
            pr_url: None,
            note: None,
            session: (sid != 0).then_some(Id(sid)),
            resume_at: None,
            created: 0,
            updated: NOW - PATCHED_AGO,
        }
    }

    /// Session `sid`: handle `<sid>`, label `#<sid> label`, one agent pane `w<sid>:p1`.
    fn session(sid: u128, status: SessionStatus, attention: Attention) -> SessionInfo {
        SessionInfo {
            sid: Id(sid),
            handle: sid.to_string(),
            machine: None,
            name: None,
            owner: format!("issue:{sid}"),
            agent: Agent::Claude,
            mode: Mode::Tui,
            cwd: format!("/h/femme-work/example-repo-{sid}"),
            label: Some(format!("#{sid} label")),
            status,
            attention,
            panes: vec![PaneInfo {
                no: 1,
                kind: PaneKind::Agent,
                title: "claude".into(),
                cwd: None,
            }],
        }
    }

    // The four states herdr reported for a live agent.
    fn working(sid: u128) -> SessionInfo {
        session(sid, S::Working, Attention::None)
    }
    fn blocked(sid: u128) -> SessionInfo {
        session(sid, S::Blocked, Attention::Blocked)
    }
    fn done(sid: u128) -> SessionInfo {
        session(sid, S::Idle, Attention::Done)
    }
    fn idle(sid: u128) -> SessionInfo {
        session(sid, S::Idle, Attention::None)
    }

    /// [`plan`] at NOW with CAP 60 and PARALLEL 2 unless `vars` say otherwise, the window at `pct`
    /// percent (None: /usage unreadable) resetting at RESET.
    fn tick_with(
        vars: &[(&str, &str)],
        issues: &[Issue],
        sessions: &[SessionInfo],
        pct: Option<f64>,
        prs: &[(u64, &str)],
    ) -> Vec<Action> {
        let prs: Vec<(u64, String)> = prs.iter().map(|(id, url)| (*id, url.to_string())).collect();
        let usage = pct.map(|percent| UsageSnapshot {
            percent,
            resets_at: Some(RESET),
        });
        plan(&SchedInput {
            now: NOW,
            issues,
            sessions,
            usage,
            prs: &prs,
            cfg: &cfg(vars),
        })
    }

    fn tick(
        issues: &[Issue],
        sessions: &[SessionInfo],
        pct: Option<f64>,
        prs: &[(u64, &str)],
    ) -> Vec<Action> {
        tick_with(&[], issues, sessions, pct, prs)
    }

    fn mark_pr(issue: u64) -> Action {
        Action::MarkPr {
            issue,
            url: PR.into(),
        }
    }
    fn waiting(issue: u64, note: &str) -> Action {
        Action::MarkWaiting {
            issue,
            note: note.into(),
        }
    }
    fn fail(issue: u64, note: &str) -> Action {
        Action::Fail {
            issue,
            note: note.into(),
        }
    }
    fn requeue(issue: u64) -> Action {
        Action::Requeue {
            issue,
            note: String::new(),
        }
    }

    // ---- phase A, one rule at a time ------------------------------------------------------------

    #[test]
    fn a1_a_pr_moves_the_issue_to_pr() {
        // Whatever the issue says and whatever the agent is doing, unless it works.
        for status in [I::Working, I::Waiting] {
            for agent in [idle(1), done(1), blocked(1)] {
                assert_eq!(
                    tick(&[issue(1, status, 1)], &[agent], Some(10.0), &[(1, PR)]),
                    [mark_pr(1)],
                    "{status:?}"
                );
            }
        }
    }

    #[test]
    fn a1_a_pr_beats_a_gone_session() {
        let issues = [
            issue(1, I::Working, 0),
            issue(2, I::Waiting, 2),
            issue(3, I::Working, 3),
        ];
        let sessions = [
            session(2, S::Closed, Attention::None),
            session(3, S::Crashed, Attention::None),
        ];
        assert_eq!(
            tick(&issues, &sessions, Some(10.0), &[(1, PR), (2, PR), (3, PR)]),
            [mark_pr(1), mark_pr(2), mark_pr(3)]
        );
    }

    #[test]
    fn a1_a_pr_is_ignored_while_the_agent_works() {
        // worker.sh fell through to the rules below: nothing, MarkWorking, or Pause over the cap.
        assert_eq!(
            tick(
                &[issue(1, I::Working, 1)],
                &[working(1)],
                Some(10.0),
                &[(1, PR)]
            ),
            []
        );
        assert_eq!(
            tick(
                &[issue(1, I::Waiting, 1)],
                &[working(1)],
                Some(10.0),
                &[(1, PR)]
            ),
            [Action::MarkWorking { issue: 1 }]
        );
        let note = "paused at 75% of the 5-hour window, continues after it resets".to_string();
        assert_eq!(
            tick(
                &[issue(1, I::Working, 1)],
                &[working(1)],
                Some(75.0),
                &[(1, PR)]
            ),
            [Action::Pause {
                issue: 1,
                resume_at: RESET,
                note
            }]
        );
    }

    #[test]
    fn a2_a_gone_session_fails_the_issue() {
        // No session, one the daemon does not know, a closed one: worker.sh's text. Lowest id first.
        let issues = [
            issue(3, I::Working, 3),
            issue(1, I::Working, 0),
            issue(2, I::Waiting, 9),
        ];
        let sessions = [session(3, S::Closed, Attention::None)];
        assert_eq!(
            tick(&issues, &sessions, Some(10.0), &[]),
            [fail(1, CLOSED), fail(2, CLOSED), fail(3, CLOSED)]
        );
    }

    #[test]
    fn a2_interrupted_crashed_and_failed_sessions_say_so() {
        // Additions: herdr had no such states.
        let issues = [
            issue(1, I::Working, 1),
            issue(2, I::Working, 2),
            issue(3, I::Waiting, 3),
        ];
        let sessions = [
            session(1, S::Interrupted, Attention::None),
            session(2, S::Crashed, Attention::Done),
            session(3, S::Failed, Attention::Blocked),
        ];
        assert_eq!(
            tick(&issues, &sessions, Some(10.0), &[]),
            [
                fail(1, "its session was interrupted: retry to start it again"),
                fail(2, "its session crashed"),
                fail(3, "its session failed")
            ]
        );
    }

    #[test]
    fn a3_over_the_cap_a_working_agent_is_paused_until_the_reset() {
        // 72.9 floors to 72; a waiting issue whose agent works again (Starting counts) is paused too.
        let note = "paused at 72% of the 5-hour window, continues after it resets";
        let issues = [issue(1, I::Working, 1), issue(2, I::Waiting, 2)];
        let sessions = [working(1), session(2, S::Starting, Attention::None)];
        assert_eq!(
            tick(&issues, &sessions, Some(72.9), &[]),
            [
                Action::Pause {
                    issue: 1,
                    resume_at: RESET,
                    note: note.into()
                },
                Action::Pause {
                    issue: 2,
                    resume_at: RESET,
                    note: note.into()
                }
            ]
        );
        // At the cap is over, just under is not.
        let note = "paused at 60% of the 5-hour window, continues after it resets".to_string();
        assert_eq!(
            tick(&issues[..1], &[working(1)], Some(60.0), &[]),
            [Action::Pause {
                issue: 1,
                resume_at: RESET,
                note
            }]
        );
        assert_eq!(tick(&issues[..1], &[working(1)], Some(59.99), &[]), []);
        // No reset time: resume_at is now.
        let usage = Some(UsageSnapshot {
            percent: 90.0,
            resets_at: None,
        });
        let got = plan(&SchedInput {
            now: NOW,
            issues: &issues[..1],
            sessions: &[working(1)],
            usage,
            prs: &[],
            cfg: &cfg(&[]),
        });
        let note = "paused at 90% of the 5-hour window, continues after it resets".to_string();
        assert_eq!(
            got,
            [Action::Pause {
                issue: 1,
                resume_at: NOW,
                note
            }]
        );
    }

    #[test]
    fn a4_a_waiting_issue_whose_agent_works_again_is_working() {
        // The user answered in Chat; a Starting session reads as working.
        let issues = [issue(1, I::Waiting, 1), issue(2, I::Waiting, 2)];
        let sessions = [working(1), session(2, S::Starting, Attention::None)];
        assert_eq!(
            tick(&issues, &sessions, Some(10.0), &[]),
            [
                Action::MarkWorking { issue: 1 },
                Action::MarkWorking { issue: 2 }
            ]
        );
    }

    #[test]
    fn a5_a_blocked_agent_waits_for_you() {
        // Blocked by status or by attention.
        let issues = [issue(1, I::Working, 1), issue(2, I::Working, 2)];
        let sessions = [blocked(1), session(2, S::Working, Attention::Blocked)];
        assert_eq!(
            tick(&issues, &sessions, Some(10.0), &[]),
            [waiting(1, ASKING), waiting(2, ASKING)]
        );
    }

    #[test]
    fn a6_an_agent_that_stopped_without_a_pr_waits_for_you() {
        // Done, idle, and a paused session (which reads as idle) alike.
        let issues = [
            issue(1, I::Working, 1),
            issue(2, I::Working, 2),
            issue(3, I::Working, 3),
        ];
        let sessions = [done(1), idle(2), session(3, S::Paused, Attention::None)];
        assert_eq!(
            tick(&issues, &sessions, Some(10.0), &[]),
            [
                waiting(1, STOPPED),
                waiting(2, STOPPED),
                waiting(3, STOPPED)
            ]
        );
    }

    #[test]
    fn fresh_working_issue_with_idle_session_is_left_alone() {
        // A Start, Resume or MarkWorking patched it 10 s ago and the session has not reported
        // working yet: a re-plan on a session change must not call that "stopped without a PR".
        let fresh = [Issue {
            updated: NOW - 10_000,
            ..issue(1, I::Working, 1)
        }];
        assert_eq!(tick(&fresh, &[idle(1)], Some(10.0), &[]), []);
        assert_eq!(tick(&fresh, &[done(1)], Some(10.0), &[]), []);
    }

    #[test]
    fn stale_working_issue_with_idle_session_is_marked_waiting() {
        // Still idle a minute after the patch: the agent did stop.
        let stale = [Issue {
            updated: NOW - 61_000,
            ..issue(1, I::Working, 1)
        }];
        assert_eq!(
            tick(&stale, &[idle(1)], Some(10.0), &[]),
            [waiting(1, STOPPED)]
        );
        assert_eq!(
            tick(&stale, &[done(1)], Some(10.0), &[]),
            [waiting(1, STOPPED)]
        );
        // The grace is 60 s: a patch that old is stale, one a millisecond younger is not.
        let patched = |ago| Issue {
            updated: NOW - ago,
            ..issue(1, I::Working, 1)
        };
        assert_eq!(
            tick(&[patched(60_000)], &[idle(1)], Some(10.0), &[]),
            [waiting(1, STOPPED)]
        );
        assert_eq!(tick(&[patched(59_999)], &[idle(1)], Some(10.0), &[]), []);
    }

    #[test]
    fn the_grace_holds_back_rule_a6_only() {
        // Patched a second ago: a PR, a gone agent and a blocked one are still acted on at once.
        let fresh = |id: u64| Issue {
            updated: NOW - 1_000,
            ..issue(id, I::Working, u128::from(id))
        };
        let closed = session(2, S::Closed, Attention::None);
        assert_eq!(
            tick(&[fresh(1)], &[idle(1)], Some(10.0), &[(1, PR)]),
            [mark_pr(1)]
        );
        assert_eq!(
            tick(&[fresh(2)], &[closed], Some(10.0), &[]),
            [fail(2, CLOSED)]
        );
        assert_eq!(
            tick(&[fresh(3)], &[blocked(3)], Some(10.0), &[]),
            [waiting(3, ASKING)]
        );
    }

    #[test]
    fn a_replan_right_after_a_resume_leaves_the_agent_alone() {
        // Tick 1 resumes the paused agent. The daemon patches the issue working at NOW; a session
        // change re-plans 5 s later with the agent still idle, then a minute on it really is idle.
        let paused = Issue {
            resume_at: Some(RESET),
            updated: NOW - 3_600_000,
            ..issue(1, I::Paused, 1)
        };
        assert_eq!(
            tick(&[paused], &[idle(1)], Some(10.0), &[]),
            [Action::Resume { issue: 1 }]
        );
        let resumed = [Issue {
            updated: NOW,
            ..issue(1, I::Working, 1)
        }];
        let at = |now| {
            let usage = Some(UsageSnapshot {
                percent: 10.0,
                resets_at: Some(RESET),
            });
            plan(&SchedInput {
                now,
                issues: &resumed,
                sessions: &[idle(1)],
                usage,
                prs: &[],
                cfg: &cfg(&[]),
            })
        };
        assert_eq!(at(NOW + 5_000), []);
        assert_eq!(at(NOW + 60_000), [waiting(1, STOPPED)]);
    }

    #[test]
    fn a7_otherwise_nothing() {
        // Already waiting on you (blocked, idle or done), or working under the cap.
        let issues = [
            issue(1, I::Waiting, 1),
            issue(2, I::Waiting, 2),
            issue(3, I::Waiting, 3),
            issue(4, I::Working, 4),
        ];
        assert_eq!(
            tick(
                &issues,
                &[blocked(1), idle(2), done(3), working(4)],
                Some(10.0),
                &[]
            ),
            []
        );
    }

    // ---- the early exits ------------------------------------------------------------------------

    #[test]
    fn an_empty_queue_does_nothing_at_all() {
        // Not even closing a done issue's session: worker.sh `continue`d before anything else.
        let issues = [
            issue(1, I::Done, 1),
            issue(2, I::Pr, 0),
            issue(3, I::Failed, 0),
        ];
        assert_eq!(tick(&issues, &[idle(1)], Some(10.0), &[]), []);
        assert_eq!(tick(&[], &[], None, &[]), []);
    }

    #[test]
    fn over_the_cap_nothing_resumes_starts_or_closes() {
        // worker.sh wrote the "paused" meta and slept after phase A.
        let issues = [
            issue(1, I::Working, 1),
            issue(2, I::Paused, 2),
            issue(3, I::Open, 0),
            issue(4, I::Done, 4),
            issue(5, I::Working, 5),
        ];
        let note = "paused at 61% of the 5-hour window, continues after it resets".to_string();
        assert_eq!(
            tick(
                &issues,
                &[working(1), idle(2), idle(4), blocked(5)],
                Some(61.0),
                &[]
            ),
            [
                Action::Pause {
                    issue: 1,
                    resume_at: RESET,
                    note
                },
                waiting(5, ASKING)
            ]
        );
    }

    #[test]
    fn unreadable_usage_reads_as_zero_percent() {
        // jq's `.percent // 0`: nothing pauses and queued issues start...
        let issues = [issue(1, I::Working, 1), issue(2, I::Open, 0)];
        assert_eq!(
            tick(&issues, &[working(1)], None, &[]),
            [Action::Start { issue: 2 }]
        );
        // ...unless CAP is 0, and then there is no reset time either.
        let note = "paused at 0% of the 5-hour window, continues after it resets".to_string();
        assert_eq!(
            tick_with(&[("CAP", "0")], &issues, &[working(1)], None, &[]),
            [Action::Pause {
                issue: 1,
                resume_at: NOW,
                note
            }]
        );
    }

    // ---- phase B --------------------------------------------------------------------------------

    #[test]
    fn b_paused_issues_resume_or_go_back_to_the_queue() {
        // A live agent is prompted to go on; without one the issue is requeued. Issue 1 is working
        // again, so one slot is left and requeued issue 2 starts.
        let issues = [
            issue(1, I::Paused, 1),
            issue(2, I::Paused, 0),
            issue(3, I::Paused, 3),
        ];
        let sessions = [idle(1), session(3, S::Interrupted, Attention::None)];
        assert_eq!(
            tick(&issues, &sessions, Some(10.0), &[]),
            [
                Action::Resume { issue: 1 },
                requeue(2),
                requeue(3),
                Action::Start { issue: 2 }
            ]
        );
    }

    #[test]
    fn b_a_requeued_issue_starts_in_the_same_tick() {
        assert_eq!(
            tick(&[issue(1, I::Paused, 0)], &[], Some(10.0), &[]),
            [requeue(1), Action::Start { issue: 1 }]
        );
    }

    #[test]
    fn b_no_reading_holds_paused_issues_until_their_resume_at() {
        // New in femmed (worker.sh had no resume_at), live agent or not; PARALLEL 0 keeps the starts out of it.
        let at = |id, sid, t| Issue {
            resume_at: Some(t),
            ..issue(id, I::Paused, sid)
        };
        let issues = [
            at(1, 1, NOW + 1),
            at(2, 0, NOW + 60_000),
            at(3, 3, NOW),
            at(4, 0, NOW - 1),
        ];
        let sessions = [idle(1), idle(3)];
        assert_eq!(
            tick_with(&[("PARALLEL", "0")], &issues, &sessions, None, &[]),
            [Action::Resume { issue: 3 }, requeue(4)]
        );
        // With a reading, as in worker.sh, every one wakes.
        assert_eq!(
            tick_with(&[("PARALLEL", "0")], &issues, &sessions, Some(10.0), &[]),
            [
                Action::Resume { issue: 1 },
                requeue(2),
                Action::Resume { issue: 3 },
                requeue(4)
            ]
        );
    }

    #[test]
    fn paused_issue_resumes_under_cap_even_before_resume_at() {
        // Paused at 61% until RESET, then an account rotation put the box at 10%: worker.sh woke it
        // at once (it knew no resume_at), whether its agent is live or gone.
        let at_reset = |sid| Issue {
            resume_at: Some(RESET),
            ..issue(1, I::Paused, sid)
        };
        assert_eq!(
            tick(&[at_reset(1)], &[idle(1)], Some(10.0), &[]),
            [Action::Resume { issue: 1 }]
        );
        assert_eq!(
            tick(&[at_reset(0)], &[], Some(10.0), &[]),
            [requeue(1), Action::Start { issue: 1 }]
        );
    }

    #[test]
    fn resume_takes_the_slot_before_a_start() {
        // PARALLEL=1: the woken agent holds the only slot, so the open issue, though its id is
        // lower, waits; with or without a resume_at ahead.
        for resume_at in [None, Some(RESET)] {
            let issues = [
                issue(1, I::Open, 0),
                Issue {
                    resume_at,
                    ..issue(2, I::Paused, 2)
                },
            ];
            assert_eq!(
                tick_with(&[("PARALLEL", "1")], &issues, &[idle(2)], Some(10.0), &[]),
                [Action::Resume { issue: 2 }],
                "{resume_at:?}"
            );
            // A second slot is for the open issue.
            let got = tick_with(&[("PARALLEL", "2")], &issues, &[idle(2)], Some(10.0), &[]);
            assert_eq!(
                got,
                [Action::Resume { issue: 2 }, Action::Start { issue: 1 }],
                "{resume_at:?}"
            );
        }
    }

    #[test]
    fn no_reading_holds_a_paused_issue_and_its_slot() {
        // /usage unreadable reads as 0% (not over), resume_at ahead: the agent is not woken into a
        // window we cannot see, and its slot stays taken, so the open issue waits too.
        let issues = [
            issue(1, I::Open, 0),
            Issue {
                resume_at: Some(RESET),
                ..issue(2, I::Paused, 2)
            },
        ];
        assert_eq!(
            tick_with(&[("PARALLEL", "1")], &issues, &[idle(2)], None, &[]),
            []
        );
        // With a second slot the open issue starts beside it.
        assert_eq!(
            tick_with(&[("PARALLEL", "2")], &issues, &[idle(2)], None, &[]),
            [Action::Start { issue: 1 }]
        );
        // The held slot counts with the working ones.
        let issues = [
            issue(1, I::Open, 0),
            issue(2, I::Working, 2),
            Issue {
                resume_at: Some(RESET),
                ..issue(3, I::Paused, 3)
            },
        ];
        assert_eq!(tick(&issues, &[working(2), idle(3)], None, &[]), []);
    }

    #[test]
    fn no_reading_held_issue_with_dead_agent_frees_the_slot() {
        // Its session is closed: the held issue counts as nothing, and it is not requeued while held.
        let issues = [
            issue(1, I::Open, 0),
            Issue {
                resume_at: Some(RESET),
                ..issue(2, I::Paused, 2)
            },
        ];
        let closed = [session(2, S::Closed, Attention::None)];
        assert_eq!(
            tick_with(&[("PARALLEL", "1")], &issues, &closed, None, &[]),
            [Action::Start { issue: 1 }]
        );
        // No session at all, or one the daemon does not know: the same.
        for sid in [0, 9] {
            let issues = [
                issue(1, I::Open, 0),
                Issue {
                    resume_at: Some(RESET),
                    ..issue(2, I::Paused, sid)
                },
            ];
            assert_eq!(
                tick_with(&[("PARALLEL", "1")], &issues, &[], None, &[]),
                [Action::Start { issue: 1 }],
                "session {sid}"
            );
        }
    }

    #[test]
    fn no_reading_resumes_once_resume_at_passed() {
        let paused = |resume_at| Issue {
            resume_at: Some(resume_at),
            ..issue(1, I::Paused, 1)
        };
        // Held up to the last millisecond, awake from resume_at on.
        assert_eq!(tick(&[paused(NOW + 1)], &[idle(1)], None, &[]), []);
        assert_eq!(
            tick(&[paused(NOW)], &[idle(1)], None, &[]),
            [Action::Resume { issue: 1 }]
        );
        assert_eq!(
            tick(&[paused(NOW - 60_000)], &[idle(1)], None, &[]),
            [Action::Resume { issue: 1 }]
        );
        // One whose agent is gone goes back to the queue, and starts at once.
        let gone = Issue {
            resume_at: Some(NOW),
            ..issue(1, I::Paused, 0)
        };
        assert_eq!(
            tick(&[gone], &[], None, &[]),
            [requeue(1), Action::Start { issue: 1 }]
        );
    }

    // ---- phase C --------------------------------------------------------------------------------

    #[test]
    fn c_open_issues_start_lowest_id_first_while_fewer_than_parallel_work() {
        // Working issues hold slots; waiting ones do not (a paused one wakes first: phase B).
        let issues = [
            issue(7, I::Open, 0),
            issue(5, I::Open, 0),
            issue(6, I::Open, 0),
            issue(1, I::Working, 1),
            issue(2, I::Waiting, 2),
        ];
        let sessions = [working(1), blocked(2)];
        assert_eq!(
            tick(&issues, &sessions, Some(10.0), &[]),
            [Action::Start { issue: 5 }]
        );
        assert_eq!(
            tick_with(&[("PARALLEL", "3")], &issues, &sessions, Some(10.0), &[]),
            [Action::Start { issue: 5 }, Action::Start { issue: 6 }]
        );
        assert_eq!(
            tick_with(&[("PARALLEL", "1")], &issues, &sessions, Some(10.0), &[]),
            []
        );
    }

    #[test]
    fn c_parallel_counts_what_this_tick_moved() {
        // Back to working (A) or resumed (B) takes a slot...
        let issues = [
            issue(1, I::Waiting, 1),
            issue(2, I::Paused, 2),
            issue(3, I::Open, 0),
        ];
        assert_eq!(
            tick(&issues, &[working(1), idle(2)], Some(10.0), &[]),
            [
                Action::MarkWorking { issue: 1 },
                Action::Resume { issue: 2 }
            ]
        );
        // ...and an agent that stopped frees its slot.
        let issues = [issue(1, I::Working, 1), issue(2, I::Open, 0)];
        assert_eq!(
            tick_with(&[("PARALLEL", "1")], &issues, &[done(1)], Some(10.0), &[]),
            [waiting(1, STOPPED), Action::Start { issue: 2 }]
        );
    }

    // ---- phase D --------------------------------------------------------------------------------

    #[test]
    fn d_done_issues_close_their_live_sessions() {
        let issues = [
            issue(1, I::Done, 1),
            issue(2, I::Done, 2),
            issue(3, I::Done, 0),
            issue(4, I::Done, 4),
            issue(5, I::Working, 5),
        ];
        let sessions = [
            idle(1),
            session(2, S::Closed, Attention::None),
            session(4, S::Crashed, Attention::None),
            working(5),
        ];
        assert_eq!(
            tick(&issues, &sessions, Some(10.0), &[]),
            [Action::Close { issue: 1 }]
        );
    }

    // ---- texts ----------------------------------------------------------------------------------

    #[test]
    fn meta_state_is_worker_sh_s_line() {
        let c = cfg(&[]);
        let usage = |percent| {
            Some(UsageSnapshot {
                percent,
                resets_at: None,
            })
        };
        assert_eq!(
            meta_state(&[issue(1, I::Done, 0), issue(2, I::Pr, 0)], usage(99.0), &c),
            "idle: queue is empty"
        );
        assert_eq!(meta_state(&[], None, &c), "idle: queue is empty");
        assert_eq!(
            meta_state(&[issue(1, I::Paused, 0)], usage(72.6), &c),
            "paused until the window resets (72% used)"
        );
        assert_eq!(
            meta_state(&[issue(1, I::Open, 0)], usage(12.9), &c),
            "working (12% used, pause at 60%)"
        );
        assert_eq!(
            meta_state(&[issue(1, I::Waiting, 0)], None, &cfg(&[("CAP", "80")])),
            "working (0% used, pause at 80%)"
        );
    }

    #[test]
    fn meta_json_has_the_fields_the_board_reads() {
        let c = cfg(&[("CAP", "70")]);
        let usage = Some(UsageSnapshot {
            percent: 11.0,
            resets_at: Some(RESET + 893),
        });
        assert_eq!(
            meta_json(
                "working (11% used, pause at 70%)",
                usage,
                &c,
                &["me/a".to_string()],
                NOW + 999
            ),
            json!({"state": "working (11% used, pause at 70%)", "usage": {"percent": 11.0, "resets_at": 1_793_617_200}, "cap": 70, "repos": ["me/a"], "updated": 1_793_610_000})
        );
        let unknown = meta_json(
            "idle: queue is empty",
            Some(UsageSnapshot {
                percent: 5.0,
                resets_at: None,
            }),
            &c,
            &[],
            NOW,
        );
        assert_eq!(unknown["usage"], json!({"percent": 5.0, "resets_at": null}));
        assert_eq!(
            meta_json("idle: queue is empty", None, &c, &[], NOW)["usage"],
            Value::Null
        );
    }

    #[test]
    fn issue_prompt_is_worker_sh_s_byte_for_byte() {
        let c = cfg(&[]);
        let mut i = Issue {
            title: "Fix the signup form".into(),
            body: "line one\nline two".into(),
            ..issue(12, I::Open, 0)
        };
        assert_eq!(
            issue_prompt(&i, &c),
            "You are working on FEMME board issue #12 for example-org/example-repo, cloned here on branch femme/issue-12.\n\nTitle: Fix the signup form\n\nDetails:\nline one\nline two\n\nDo it end to end: implement, run the project's checks and tests, commit, push the branch (git push -u origin femme/issue-12), then open a PR with gh pr create whose body starts with \"FEMME issue #12\" and says what changed and how you verified it. If you are blocked on a decision only the owner can make, say so plainly and stop."
        );
        // ${4:-(none)}
        i.body = String::new();
        assert_eq!(
            issue_prompt(&i, &c),
            "You are working on FEMME board issue #12 for example-org/example-repo, cloned here on branch femme/issue-12.\n\nTitle: Fix the signup form\n\nDetails:\n(none)\n\nDo it end to end: implement, run the project's checks and tests, commit, push the branch (git push -u origin femme/issue-12), then open a PR with gh pr create whose body starts with \"FEMME issue #12\" and says what changed and how you verified it. If you are blocked on a decision only the owner can make, say so plainly and stop."
        );
        // bash's $(...) dropped trailing newlines, so a body of newlines was empty too.
        i.body = "\n\n".into();
        assert!(issue_prompt(&i, &c).contains("\n\nDetails:\n(none)\n\nDo it end to end"));
        i.title = "Fix it\n".into();
        i.body = "line one\n\n".into();
        assert!(issue_prompt(&i, &c)
            .contains("\n\nTitle: Fix it\n\nDetails:\nline one\n\nDo it end to end"));
    }

    #[test]
    fn session_label_is_the_id_and_40_characters_of_the_title() {
        let i = Issue {
            title: "Fix the signup form".into(),
            ..issue(12, I::Open, 0)
        };
        assert_eq!(session_label(&i), "#12 Fix the signup form");
        let i = Issue {
            title: "é".repeat(41),
            ..issue(12, I::Open, 0)
        }; // characters, not bytes
        assert_eq!(session_label(&i), format!("#12 {}", "é".repeat(40)));
        let i = Issue {
            title: String::new(),
            ..issue(3, I::Open, 0)
        };
        assert_eq!(session_label(&i), "#3 ");
    }

    #[test]
    fn clones_live_in_femme_work_by_repo_name_and_issue() {
        let c = cfg(&[("HOME", "/home/me")]);
        assert_eq!(
            work_dir(&c, "example-org/example-repo", 12),
            PathBuf::from("/home/me/femme-work/example-repo-12")
        );
        assert_eq!(
            work_dir(&c, "solo", 3),
            PathBuf::from("/home/me/femme-work/solo-3")
        ); // ${repo#*/} keeps a name without owner
        assert_eq!(branch(12), "femme/issue-12");
    }

    // ---- /usage ---------------------------------------------------------------------------------

    /// `claude -p /usage --output-format stream-json --verbose --no-session-persistence` as claude
    /// 2.1.302 printed it: hook noise, the assistant line, the result. The limits are verbatim; the
    /// other fields are filled in.
    const USAGE_OUT: &str = r#"{"type":"system","subtype":"hook_started","hook_id":"h1","hook_name":"SessionStart:startup","session_id":"7c1d"}
{"type":"system","subtype":"hook_response","hook_id":"h1","hook_name":"SessionStart:startup","output":"","exit_code":0,"session_id":"7c1d"}
{"type":"system","subtype":"init","cwd":"/tmp","session_id":"7c1d","tools":["Bash","Read"],"slash_commands":["usage"]}
{"type":"assistant","message":{"id":"m1","type":"message","role":"assistant","content":[{"type":"text","text":"Current session: 11% used"}]},"parent_tool_use_id":null,"session_id":"7c1d","usage_report":{"session":{"cost_usd":0},"rate_limits":{"limits":[{"kind":"session","group":"session","percent":11,"resets_at":"2026-11-02T11:00:00.893897+00:00","scope":null,"severity":"normal","is_active":true},{"kind":"weekly_scoped","group":"weekly","percent":0,"resets_at":"2026-11-08T06:59:59.894046+00:00","scope":{"model":{"display_name":"Fable"},"surface":null},"severity":"normal","is_active":false}],"extra_usage":{"is_enabled":false}}}}
{"type":"result","subtype":"success","is_error":false,"num_turns":0,"result":"Current session: 11% used","session_id":"7c1d"}"#;

    #[test]
    fn parse_usage_reads_the_session_window() {
        assert_eq!(
            parse_usage(USAGE_OUT),
            Some(UsageSnapshot {
                percent: 11.0,
                resets_at: Some(RESET + 893)
            })
        );
        let line = |limit: &str| {
            format!(
                r#"{{"type":"assistant","usage_report":{{"rate_limits":{{"limits":[{limit}]}}}}}}"#
            )
        };
        let read = |limit: &str| parse_usage(&line(limit));
        // resets_at as a number, in seconds or in ms; unreadable or missing: the percent still counts.
        assert_eq!(
            read(r#"{"kind":"session","percent":61.5,"resets_at":1793617200}"#),
            Some(UsageSnapshot {
                percent: 61.5,
                resets_at: Some(RESET)
            })
        );
        assert_eq!(
            read(r#"{"kind":"session","percent":61,"resets_at":1793617200893}"#),
            Some(UsageSnapshot {
                percent: 61.0,
                resets_at: Some(RESET + 893)
            })
        );
        assert_eq!(
            read(r#"{"kind":"session","percent":61,"resets_at":"soon"}"#),
            Some(UsageSnapshot {
                percent: 61.0,
                resets_at: None
            })
        );
        assert_eq!(
            read(r#"{"kind":"session","percent":61}"#),
            Some(UsageSnapshot {
                percent: 61.0,
                resets_at: None
            })
        );
        // The first assistant line with a session window wins.
        let lines = format!(
            "{}\n{}\n{}",
            r#"{"type":"assistant","message":{}}"#,
            line(r#"{"kind":"session","percent":5}"#),
            line(r#"{"kind":"session","percent":6}"#)
        );
        assert_eq!(parse_usage(&lines).map(|u| u.percent), Some(5.0));
        // A line that is not JSON does not hide a later reading (jq would have stopped there).
        assert_eq!(
            parse_usage(&format!("oops\n{USAGE_OUT}")).map(|u| u.percent),
            Some(11.0)
        );
    }

    #[test]
    fn parse_usage_without_a_session_window_is_none() {
        let weekly = r#"{"type":"assistant","usage_report":{"rate_limits":{"limits":[{"kind":"weekly_scoped","percent":40,"resets_at":"2026-11-08T06:59:59Z"}]}}}"#;
        let not_assistant = r#"{"type":"result","usage_report":{"rate_limits":{"limits":[{"kind":"session","percent":40}]}}}"#;
        let no_percent = r#"{"type":"assistant","usage_report":{"rate_limits":{"limits":[{"kind":"session","percent":"40%"}]}}}"#;
        let odd = "not json\n{\"type\":\"assistant\"}\n[1,2]\n42\n{\"type\":\"assistant\",\"usage_report\":{\"rate_limits\":{\"limits\":7}}}\n";
        for out in ["", "garbage", odd, weekly, not_assistant, no_percent] {
            assert_eq!(parse_usage(out), None, "{out}");
        }
    }

    #[test]
    fn rfc3339_to_unix_ms() {
        assert_eq!(rfc3339_ms("2026-11-02T11:00:00+00:00"), Some(RESET));
        assert_eq!(rfc3339_ms("2026-11-02T11:00:00Z"), Some(RESET));
        assert_eq!(rfc3339_ms("2026-11-02T13:00:00+02:00"), Some(RESET));
        assert_eq!(rfc3339_ms("2026-11-02T06:30:00-04:30"), Some(RESET));
        assert_eq!(
            rfc3339_ms("2026-11-02T11:00:00.893897+00:00"),
            Some(RESET + 893)
        );
        assert_eq!(rfc3339_ms("2026-11-02T11:00:00.5Z"), Some(RESET + 500));
        assert_eq!(
            rfc3339_ms("2026-11-08T06:59:59.894046+00:00"),
            Some(1_794_121_199_894)
        );
        assert_eq!(rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_ms("2000-03-01T00:00:00Z"), Some(951_868_800_000));
        assert_eq!(rfc3339_ms("1999-12-31T23:59:59Z"), Some(946_684_799_000));
        assert_eq!(
            rfc3339_ms("2024-02-29T12:34:56-05:30"),
            Some(1_709_229_896_000)
        );
        for bad in [
            "",
            "soon",
            "2026-11-02",
            "2026-11-02T11:00:00",
            "2026-13-02T11:00:00Z",
            "2026-11-02T24:00:00Z",
            "2026-11-02T11:00:00+0000",
            "2026-11-02T11:00:00.Z",
            "+026-11-02T11:00:00Z",
            "2026-11-02T11:00:00+00:00x",
            "2026-11-02T11:00:00é",
        ] {
            assert_eq!(rfc3339_ms(bad), None, "{bad}");
        }
    }

    // ---- the commands, against fake tools -------------------------------------------------------

    /// Installs `<dir>/bin/<name>`: a script that appends its arguments to `<dir>/<name>.log` (a
    /// line per run, each argument in brackets), then runs `body`.
    fn fake(dir: &Path, name: &str, body: &str) {
        let log = dir.join(format!("{name}.log"));
        let log = log.display();
        let script = format!(
            "for a in \"$@\"; do printf '[%s]' \"$a\"; done >> '{log}'\necho >> '{log}'\n{body}\n"
        );
        install_tool(&dir.join("bin"), name, &script);
    }

    /// A config whose tools are the fakes in `<dir>/bin`, then the system's (git), home `<dir>/home`.
    fn fake_cfg(dir: &Path) -> Config {
        let mut c = cfg(&[("HOME", dir.join("home").to_str().unwrap())]);
        c.tool_path = Some(format!("{}:/usr/bin:/bin", dir.join("bin").display()));
        c
    }

    fn log(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(format!("{name}.log"))).unwrap_or_default()
    }

    #[tokio::test]
    async fn prepare_clone_clones_once_and_checks_out_the_issue_branch() {
        let tmp = tempfile::tempdir().unwrap();
        fake(
            tmp.path(),
            "gh",
            r#"[ "$1 $2" = "repo clone" ] && exec git init -q -b main "$4""#,
        );
        let cfg = fake_cfg(tmp.path());
        let i = issue(12, I::Open, 0);
        let dir = prepare_clone(&i, &cfg).await.unwrap();
        assert_eq!(dir, tmp.path().join("home/femme-work/example-repo-12"));
        assert_eq!(
            log(tmp.path(), "gh"),
            format!(
                "[repo][clone][example-org/example-repo][{}][--][--quiet]\n",
                dir.display()
            )
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(".git/HEAD")).unwrap(),
            "ref: refs/heads/femme/issue-12\n"
        );
        // A clone is there: no second clone, and it is put back on the branch.
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(prepare_clone(&i, &cfg).await.unwrap(), dir);
        assert_eq!(log(tmp.path(), "gh").lines().count(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.join(".git/HEAD")).unwrap(),
            "ref: refs/heads/femme/issue-12\n"
        );
    }

    #[tokio::test]
    async fn prepare_clone_fails_with_the_issue_note() {
        let tmp = tempfile::tempdir().unwrap();
        fake(
            tmp.path(),
            "gh",
            "echo 'GraphQL: Could not resolve to a Repository' >&2; exit 1",
        );
        let err = prepare_clone(&issue(12, I::Open, 0), &fake_cfg(tmp.path()))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "could not clone example-org/example-repo");
        assert!(
            format!("{err:#}").contains("gh exited with exit status: 1"),
            "{err:#}"
        );
        assert!(tmp.path().join("home/femme-work").is_dir());
    }

    #[tokio::test]
    async fn find_pr_asks_gh_about_the_issue_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let script = format!("case \"$6\" in\n  femme/issue-1) echo '{PR}' ;;\n  femme/issue-2) echo ;;\n  femme/issue-3) echo null ;;\n  *) echo 'HTTP 401' >&2; exit 1 ;;\nesac");
        fake(tmp.path(), "gh", &script);
        let cfg = fake_cfg(tmp.path());
        assert_eq!(
            find_pr("example-org/example-repo", 1, &cfg)
                .await
                .unwrap()
                .as_deref(),
            Some(PR)
        );
        assert_eq!(
            find_pr("example-org/example-repo", 2, &cfg).await.unwrap(),
            None
        );
        assert_eq!(
            find_pr("example-org/example-repo", 3, &cfg).await.unwrap(),
            None
        );
        assert!(find_pr("example-org/example-repo", 4, &cfg).await.is_err());
        let first = log(tmp.path(), "gh").lines().next().map(String::from);
        assert_eq!(first.as_deref(), Some("[pr][list][--repo][example-org/example-repo][--head][femme/issue-1][--state][all][--json][url][-q][.[0].url]"));
    }

    #[tokio::test]
    async fn read_usage_runs_claude_usage_in_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("cwd");
        // A non-zero exit does not matter: worker.sh piped the output into jq.
        fake(
            tmp.path(),
            "claude",
            &format!(
                "pwd > '{}'\ncat <<'EOF'\n{USAGE_OUT}\nEOF\necho 'hook noise' >&2\nexit 3",
                cwd.display()
            ),
        );
        let mut cfg = fake_cfg(tmp.path());
        assert_eq!(
            read_usage(&cfg).await.unwrap(),
            Some(UsageSnapshot {
                percent: 11.0,
                resets_at: Some(RESET + 893)
            })
        );
        assert_eq!(
            log(tmp.path(), "claude"),
            "[-p][/usage][--output-format][stream-json][--verbose][--no-session-persistence]\n"
        );
        assert_eq!(std::fs::read_to_string(&cwd).unwrap(), "/tmp\n");
        // No claude on PATH: an error, not a reading.
        cfg.tool_path = Some(tmp.path().join("nothing").display().to_string());
        assert!(read_usage(&cfg).await.is_err());
    }

    #[tokio::test]
    async fn list_repos_lists_yours_then_each_orgs() {
        let tmp = tempfile::tempdir().unwrap();
        let script = r#"case "$1 $2 $3" in
  "api user/orgs "*) printf 'org-one\norg-two\n' ;;
  "repo list --limit") printf 'me/a\n\nme/b\n' ;;
  "repo list org-one") echo org-one/c ;;
  *) exit 1 ;;
esac"#;
        fake(tmp.path(), "gh", script);
        assert_eq!(
            list_repos(&fake_cfg(tmp.path())).await,
            ["me/a", "me/b", "org-one/c"]
        );
        let q = "[--limit][300][--json][nameWithOwner][-q][.[].nameWithOwner]";
        assert_eq!(log(tmp.path(), "gh"), format!("[api][user/orgs][--jq][.[].login]\n[repo][list]{q}\n[repo][list][org-one]{q}\n[repo][list][org-two]{q}\n"));
        // Orgs unreadable: your own repos still come.
        let tmp = tempfile::tempdir().unwrap();
        fake(
            tmp.path(),
            "gh",
            r#"[ "$1" = repo ] && echo me/a || exit 1"#,
        );
        assert_eq!(list_repos(&fake_cfg(tmp.path())).await, ["me/a"]);
    }

    #[tokio::test]
    async fn a_command_that_hangs_is_killed_at_its_timeout() {
        let started = std::time::Instant::now();
        let mut c = cmd(&cfg(&[]), "sleep");
        c.arg("30");
        let err = run(&mut c, Duration::from_millis(200)).await.unwrap_err();
        assert_eq!(err.to_string(), "sleep timed out after 200ms");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
