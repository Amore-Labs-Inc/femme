//! `Prs`, `PrVideo`, `PrMerge` and `PrClose`: the box repo's open pull requests, through `gh` in
//! the box's checkout (`<home>/femme`). A merge lands on main and `server/deploy.sh` ships it.
//!
//! `PrFix` starts a Claude chat (`fix #<n>`, named `pr-fix-<n>`) in a worktree of the branch under
//! `<home>/femme-work/pr-fix/`, which merges main in, resolves the conflicts, tests and pushes. With
//! auto-fix on (`<state>/pr-autofix`), a merge from FEMME starts one for every `claude-update/*` pull
//! request it leaves conflicting.
//!
//! A pull request's demo video is `<state>/pr-videos/<branch, '/' as '-'>.<mp4|webm|gif>`, which
//! `server/claude-update.sh`'s implementer records.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use femme_proto::rpc::{Err as WireErr, ErrCode, Pr, Resp, MAX_BLOB_PUT};
use femme_proto::state::{Agent, Mode};
use femme_proto::Sid;
use serde::Deserialize;
use tokio::process::Command;

use super::req::{err, wire};
use crate::config::Config;
use crate::core::{Core, CreateSession, PaneRef};
use crate::daemon::Daemon;
use crate::session::MAIN;

const VIDEO_EXTS: [&str; 3] = ["mp4", "webm", "gif"];

/// `gh pr list --json`'s shape for the fields asked.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPr {
    number: u64,
    title: String,
    head_ref_name: String,
    author: GhAuthor,
    url: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    /// `MERGEABLE`, `CONFLICTING` or `UNKNOWN`.
    #[serde(default)]
    mergeable: String,
}

#[derive(Deserialize)]
struct GhAuthor {
    login: String,
}

/// Runs `gh` in the box's checkout and answers its stdout.
async fn gh(cfg: &Config, args: &[&str]) -> Result<Vec<u8>, WireErr> {
    run(&cfg.home.join("femme"), "gh", args).await
}

/// Runs `prog args` in `dir` and answers its stdout; a failure carries its stderr.
async fn run(dir: &Path, prog: &str, args: &[&str]) -> Result<Vec<u8>, WireErr> {
    let out = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .map_err(|e| err(ErrCode::Internal, format!("gh: {e}")))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(err(
            ErrCode::Internal,
            format!("gh {}: {why}", args[..2].join(" ")),
        ));
    }
    Ok(out.stdout)
}

fn videos(cfg: &Config) -> PathBuf {
    cfg.state_path("pr-videos")
}

/// The demo video's file name for `branch`, when the folder has one.
fn video_for(dir: &Path, branch: &str) -> Option<String> {
    let stem = branch.replace('/', "-");
    VIDEO_EXTS
        .iter()
        .map(|ext| format!("{stem}.{ext}"))
        .find(|name| dir.join(name).is_file())
}

async fn open_prs(cfg: &Config) -> Result<Vec<GhPr>, WireErr> {
    let fields = "number,title,headRefName,author,url,body,additions,deletions,mergeable";
    let out = gh(
        cfg,
        &[
            "pr", "list", "--state", "open", "--limit", "100", "--json", fields,
        ],
    )
    .await?;
    serde_json::from_slice(&out).map_err(|e| err(ErrCode::Internal, format!("gh pr list: {e}")))
}

fn mergeable(state: &str) -> Option<bool> {
    match state {
        "MERGEABLE" => Some(true),
        "CONFLICTING" => Some(false),
        _ => None,
    }
}

/// The live fix chat of pull request `number`.
async fn fixing(d: &Daemon, number: u64) -> Option<Sid> {
    let name = format!("pr-fix-{number}");
    d.session_by_name(&name).await.ok().flatten().map(|s| s.sid)
}

pub(crate) async fn list(d: &Daemon) -> Result<Resp, WireErr> {
    let dir = videos(&d.cfg);
    let mut out = Vec::new();
    for p in open_prs(&d.cfg).await? {
        if p.mergeable == "UNKNOWN" {
            // GitHub works mergeability out when one pull request is asked for, not for a list:
            // ask, and the desktop's next refresh has it.
            let (cfg, n) = (d.cfg.clone(), p.number.to_string());
            tokio::spawn(async move {
                let _ = gh(&cfg, &["pr", "view", &n, "--json", "mergeable"]).await;
            });
        }
        out.push(Pr {
            fixing: fixing(d, p.number).await,
            merging: merging(p.number).await,
            mergeable: mergeable(&p.mergeable),
            video: video_for(&dir, &p.head_ref_name),
            number: p.number,
            title: p.title,
            branch: p.head_ref_name,
            author: p.author.login,
            url: p.url,
            body: p.body,
            additions: p.additions,
            deletions: p.deletions,
        });
    }
    Ok(Resp::Prs(out))
}

/// A name `video_for` could have given: one file in the folder, nothing that climbs out.
fn plain_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\'])
}

pub(crate) async fn video(cfg: &Config, name: &str) -> Result<Resp, WireErr> {
    if !plain_name(name) {
        return Err(err(ErrCode::Bad, "a video is a file name in pr-videos"));
    }
    let data = tokio::fs::read(videos(cfg).join(name))
        .await
        .map_err(|e| err(ErrCode::NotFound, format!("{name}: {e}")))?;
    if data.len() > MAX_BLOB_PUT {
        return Err(err(
            ErrCode::Bad,
            format!("{name} is over {MAX_BLOB_PUT} bytes"),
        ));
    }
    Ok(Resp::Blob(data))
}

pub(crate) async fn merge(d: &Arc<Daemon>, number: u64) -> Result<Resp, WireErr> {
    let n = number.to_string();
    if let Err(e) = gh(&d.cfg, &["pr", "merge", &n, "--squash", "--delete-branch"]).await {
        if !e.msg.contains("merge conflicts") && !e.msg.contains("not mergeable") {
            return Err(e);
        }
        // It conflicts with main: `server/pr-merge.sh` has Claude resolve it, then merges it. A
        // unit of its own, not a task of ours: a deploy restarts femmed while it works.
        queue_merge(&d.cfg, number).await?;
        return Ok(Resp::Ok);
    }
    if auto_fix(&d.cfg) {
        tokio::spawn(fix_left_conflicting(d.clone()));
    }
    Ok(Resp::Ok)
}

fn merge_unit(number: u64) -> String {
    format!("femme-pr-merge-{number}")
}

async fn queue_merge(cfg: &Config, number: u64) -> Result<(), WireErr> {
    if merging(number).await {
        return Ok(());
    }
    let script = cfg.home.join("femme/server/pr-merge.sh");
    let unit = format!("--unit={}", merge_unit(number));
    let path = format!(
        "--setenv=PATH={}",
        std::env::var("PATH").unwrap_or_default()
    );
    let (script, n) = (script.display().to_string(), number.to_string());
    let args = ["--user", "--collect", "--quiet", &unit, &path, &script, &n];
    run(&cfg.home, "systemd-run", &args).await.map(|_| ())
}

/// Whether a queued merge (`server/pr-merge.sh`) is still at work on pull request `number`.
async fn merging(number: u64) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", &merge_unit(number)])
        .status()
        .await
        .is_ok_and(|s| s.success())
}

fn autofix_file(cfg: &Config) -> PathBuf {
    cfg.state_path("pr-autofix")
}

fn auto_fix(cfg: &Config) -> bool {
    autofix_file(cfg).exists()
}

pub(crate) async fn set_auto_fix(cfg: &Config, on: Option<bool>) -> Result<Resp, WireErr> {
    let file = autofix_file(cfg);
    let io = |e: std::io::Error| err(ErrCode::Internal, format!("pr-autofix: {e}"));
    match on {
        Some(true) => tokio::fs::write(&file, b"on\n").await.map_err(io)?,
        Some(false) if file.exists() => tokio::fs::remove_file(&file).await.map_err(io)?,
        _ => {}
    }
    Ok(Resp::PrAutoFix(auto_fix(cfg)))
}

/// After a merge: GitHub works out the other pull requests' mergeability lazily, so ask a few times;
/// every `claude-update/*` one that conflicts and has no fix running gets one.
async fn fix_left_conflicting(d: Arc<Daemon>) {
    let mut done = std::collections::HashSet::new();
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_secs(20)).await;
        let Ok(prs) = open_prs(&d.cfg).await else {
            continue;
        };
        let mut unknown = false;
        for p in prs
            .iter()
            .filter(|p| p.head_ref_name.starts_with("claude-update/"))
        {
            match mergeable(&p.mergeable) {
                None => unknown = true,
                Some(false) if done.insert(p.number) => {
                    if let Err(e) = start_fix(&d, "device:ssh".into(), p.number).await {
                        tracing::warn!(pr = p.number, "auto-fix did not start: {}", e.msg);
                    }
                }
                _ => {}
            }
        }
        if !unknown {
            return;
        }
    }
}

pub(crate) async fn fix(d: &Daemon, owner: String, number: u64) -> Result<Resp, WireErr> {
    start_fix(d, owner, number).await.map(Resp::Session)
}

/// The fix chat for pull request `number`: the running one, else a new one in a fresh worktree of
/// its branch.
async fn start_fix(d: &Daemon, owner: String, number: u64) -> Result<Sid, WireErr> {
    if let Some(sid) = fixing(d, number).await {
        return Ok(sid);
    }
    let n = number.to_string();
    let out = gh(
        &d.cfg,
        &["pr", "view", &n, "--json", "headRefName,title,url"],
    )
    .await?;
    let v: serde_json::Value = serde_json::from_slice(&out)
        .map_err(|e| err(ErrCode::Internal, format!("gh pr view: {e}")))?;
    let branch = v["headRefName"].as_str().unwrap_or_default().to_owned();
    if branch.is_empty() || branch.starts_with('-') {
        return Err(err(ErrCode::Internal, format!("#{number} has no branch")));
    }
    let wt = worktree(&d.cfg, &branch, number).await?;
    let prompt = fix_prompt(number, &branch, v["title"].as_str().unwrap_or(""), &wt);
    let info = d
        .create_session(CreateSession {
            cwd: wt.display().to_string(),
            agent: Agent::Claude,
            mode: Mode::Headless,
            label: Some(format!("fix #{number}")),
            name: Some(format!("pr-fix-{number}")),
            owner,
            model: Some("claude-opus-5-5".into()),
            resume: None,
        })
        .await
        .map_err(wire)?;
    let pane = PaneRef {
        machine: info.machine.clone(),
        handle: info.handle.clone(),
        no: MAIN,
    };
    d.prompt(&pane, &prompt).await.map_err(wire)?;
    Ok(info.sid)
}

/// `<home>/femme-work/pr-fix/<number>`, a worktree of the shared clone `pr-fix/repo` checked out at
/// the branch's tip on GitHub (a leftover from an earlier fix is reset to it).
async fn worktree(cfg: &Config, branch: &str, number: u64) -> Result<PathBuf, WireErr> {
    let base = cfg.work_root().join("pr-fix");
    let repo = base.join("repo");
    let wt = base.join(number.to_string());
    tokio::fs::create_dir_all(&base)
        .await
        .map_err(|e| err(ErrCode::Internal, format!("{}: {e}", base.display())))?;
    if !repo.join(".git").exists() {
        let box_repo = cfg.home.join("femme");
        let url = String::from_utf8_lossy(
            &run(&box_repo, "git", &["remote", "get-url", "origin"]).await?,
        )
        .trim()
        .to_owned();
        let (r, b) = (repo.display().to_string(), box_repo.display().to_string());
        run(
            &base,
            "git",
            &[
                "clone",
                "-q",
                "--reference-if-able",
                &b,
                "--dissociate",
                &url,
                &r,
            ],
        )
        .await?;
    }
    run(&repo, "git", &["fetch", "-q", "origin"]).await?;
    let remote = format!("origin/{branch}");
    if wt.join(".git").exists() {
        run(&wt, "git", &["checkout", "-q", "-B", branch, &remote]).await?;
        run(&wt, "git", &["reset", "-q", "--hard", &remote]).await?;
    } else {
        let w = wt.display().to_string();
        run(
            &repo,
            "git",
            &["worktree", "add", "-q", "-B", branch, &w, &remote],
        )
        .await?;
    }
    Ok(wt)
}

fn fix_prompt(number: u64, branch: &str, title: &str, wt: &Path) -> String {
    format!(
        "Pull request #{number} (\"{title}\", branch {branch}) no longer merges into main: \
         other pull requests merged first and touched the same lines. This folder ({wt}) is a \
         worktree of the branch at its tip on GitHub.\n\n\
         1. `git fetch origin && git merge origin/main`.\n\
         2. Resolve every conflict so both sides keep working: main's change and this pull \
         request's intent. Read both sides and the commits that brought them; don't drop either.\n\
         3. Build and test what the merge touched (`cargo build`/`cargo test -p <crate>` at the \
         root, `cargo build`/`cargo test` in desktop/ for desktop files). Fix what breaks.\n\
         4. Commit the merge (message: what each conflict was and how you resolved it), then \
         `git push origin HEAD:{branch}`. Never push to main, never force-push, never merge the \
         pull request yourself: the user merges it from FEMME.\n\
         5. Comment on the pull request with `gh pr comment {number}`: one line per resolved \
         conflict.\n\n\
         Reply at the end in two or three sentences: what conflicted, how you resolved it, and \
         that it is ready to merge (or what still blocks it).",
        wt = wt.display()
    )
}

pub(crate) async fn close(cfg: &Config, number: u64) -> Result<Resp, WireErr> {
    let n = number.to_string();
    gh(cfg, &["pr", "close", &n, "--delete-branch"]).await?;
    Ok(Resp::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_branch_finds_its_video_by_its_dashed_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("claude-update-2.1.319-recap.mp4"), b"x").unwrap();
        assert_eq!(
            video_for(dir.path(), "claude-update/2.1.319/recap").as_deref(),
            Some("claude-update-2.1.319-recap.mp4")
        );
        assert_eq!(video_for(dir.path(), "other"), None);
    }

    #[test]
    fn a_video_name_cannot_leave_the_folder() {
        assert!(plain_name("a-b.mp4"));
        for bad in ["", "../x.mp4", "a/b.mp4", ".hidden", "a\\b"] {
            assert!(!plain_name(bad), "{bad}");
        }
    }
}
