//! `/api/agents/{name}`: one standing Claude per name, for another agent (Instinct on WhatsApp).
//! Opened on the first call, or again after it is gone (closed, box rebooted), with the standing
//! brief in front of the first message; tasks after that share the session. The changes stay
//! uncommitted until the owner's `push` or `discard` comes.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::response::Response;
use femme_proto::state::{Agent, Mode};
use serde_json::{json, Value};

use super::util::{
    clone_url, home_dir, lock, text, tool, valid_branch, valid_name, valid_repo, NAME_RULE,
};
use super::{blocking, core_err, err, json_response, ok, App, Req};
use crate::config::Config;
use crate::core::{ApiKey, CreateSession, PaneRef, SessionInfo};

/// The open session an agent name belongs to.
async fn named_agent(app: &App, name: &str) -> Result<Option<SessionInfo>, Response> {
    app.core.session_by_name(name).await.map_err(core_err)
}

/// The session's pane as the Terminals routes know it (same machine tag, so the id round-trips).
fn tagged(s: &SessionInfo) -> Result<PaneRef, Response> {
    let no = s
        .main_pane()
        .map(|p| p.no)
        .ok_or_else(|| err(502, "the session has no terminal"))?;
    Ok(PaneRef {
        machine: s.machine.clone(),
        handle: s.handle.clone(),
        no,
    })
}

fn report_path(cfg: &Config, name: &str) -> PathBuf {
    cfg.state_path(&format!("report-{name}.md"))
}

pub(super) async fn get(app: &Arc<App>, name: &str) -> Response {
    if !valid_name(name) {
        return err(400, NAME_RULE);
    }
    let session = match named_agent(app, name).await {
        Ok(Some(s)) => s,
        Ok(None) => return err(404, "not running"),
        Err(r) => return r,
    };
    let pane = match tagged(&session) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let status = session.agent_status().unwrap_or("unknown").to_string();
    let screen = app
        .core
        .screen(&pane, false)
        .await
        .map(|s| s.text)
        .unwrap_or_default();
    let before = lock(&app.heads).get(name).cloned();
    let (cfg, dir, n, st) = (
        app.cfg().clone(),
        app.cfg().work_root().join(name),
        name.to_string(),
        status.clone(),
    );
    let mut v = match blocking(move || outcome(&cfg, &dir, before.as_deref(), &st, &n)).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    v["pane_id"] = pane.to_string().into();
    v["status"] = status.into();
    v["screen"] = screen.into();
    v["report"] = fs::read_to_string(report_path(app.cfg(), name))
        .ok()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .into();
    ok(v)
}

pub(super) async fn post(
    app: &Arc<App>,
    req: &mut Req,
    name: &str,
    caller: Option<&ApiKey>,
) -> Response {
    if !valid_name(name) {
        return err(400, NAME_RULE);
    }
    let input = match req.json_or_400().await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let text = input["text"].as_str().unwrap_or("").replace('\0', "");
    if text.is_empty() || text.len() > 100_000 {
        return err(400, "text: 1 to 100000 bytes");
    }
    // One message per name at a time: two first messages at once would open two sessions (the
    // board took one request at a time, so it never could).
    let gate = lock(&app.openers)
        .entry(name.to_string())
        .or_default()
        .clone();
    let _opening = gate.lock().await;
    let (pane, text) = match named_agent(app, name).await {
        Ok(Some(s)) => match tagged(&s) {
            Ok(pane) => (pane, text),
            Err(r) => return r,
        },
        Ok(None) => match open_agent(app, name, &input, caller).await {
            Ok((pane, brief)) => (pane, format!("{brief}{text}")),
            Err(r) => return r,
        },
        Err(r) => return r,
    };
    send(app, name, pane, text).await
}

pub(super) async fn verdict(app: &Arc<App>, req: &mut Req, name: &str, verdict: &str) -> Response {
    if !valid_name(name) {
        return err(400, NAME_RULE);
    }
    let session = match named_agent(app, name).await {
        Ok(Some(s)) => s,
        Ok(None) => return err(404, "not running"),
        Err(r) => return r,
    };
    let pane = match tagged(&session) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let (cfg, dir) = (app.cfg().clone(), app.cfg().work_root().join(name));
    let branch =
        match blocking(move || git(&cfg, &dir, &["rev-parse", "--abbrev-ref", "HEAD"])).await {
            Ok(Some(b)) => b,
            Ok(None) => return err(400, "this session has no repo"),
            Err(r) => return r,
        };
    let note = req
        .json(super::MAX_BODY)
        .await
        .ok()
        .and_then(|v| v["text"].as_str().map(|s| s.replace('\0', "")))
        .unwrap_or_default();
    if note.len() > 10_000 {
        return err(400, "text: up to 10000 bytes");
    }
    let note = if note.trim().is_empty() {
        String::new()
    } else {
        format!("\n\nNote from the owner:\n{note}")
    };
    let text = match verdict {
        "push" => format!("Push request from the owner. Commit every change from the last task as one commit whose message says what changed and why, then git pull --rebase origin {branch} && git push origin {branch}. If there is nothing to commit, say so and push nothing.{note}"),
        _ => format!("Discard request from the owner. Throw away every uncommitted change from the last task (git checkout -- . && git clean -fd) and say so.{note}"),
    };
    send(app, name, pane, text).await
}

pub(super) async fn delete(app: &Arc<App>, name: &str) -> Response {
    if !valid_name(name) {
        return err(400, NAME_RULE);
    }
    let session = match named_agent(app, name).await {
        Ok(Some(s)) => s,
        Ok(None) => return err(404, "not running"),
        Err(r) => return r,
    };
    match app.core.close_session(session.sid).await {
        Ok(()) => ok(json!({"ok": true})),
        Err(e) => core_err(e),
    }
}

/// Sends a message to a running session. The last report goes (GET shows null until the new one
/// is written) and a reminder about it comes with every message, since the brief only went with
/// the first; the clone's HEAD is noted so GET can tell what the message changed.
async fn send(app: &Arc<App>, name: &str, pane: PaneRef, text: String) -> Response {
    let cfg = app.cfg().clone();
    let dir = cfg.work_root().join(name);
    let report = report_path(&cfg, name);
    let _ = fs::remove_file(&report);
    let mut text = format!(
        "{text}\n\nWhen done, write the report to {} (overwrite it), as briefed.",
        report.display()
    );
    if dir.join(".git").is_dir() && !text.starts_with("Push request") {
        text += " Do not commit or push: that comes as a separate push request.";
    }
    let head = match blocking(move || git(&cfg, &dir, &["rev-parse", "HEAD"])).await {
        Ok(h) => h,
        Err(r) => return r,
    };
    if let Some(head) = head {
        lock(&app.heads).insert(name.to_string(), head);
    }
    match app.core.prompt(&pane, &text).await {
        Ok(()) => {
            // The status flips to `working` a moment later (a PTY agent's hook); until then a GET
            // would read the last task as finished. Up to five seconds.
            for _ in 0..10 {
                if matches!(app.core.session_by_name(name).await, Ok(Some(s)) if s.agent_status() == Some("working"))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            ok(json!({"pane_id": pane.to_string(), "status": "working"}))
        }
        // 409 whatever went wrong, as the board answered (openapi.json documents no other failure).
        Err(e) => json_response(
            409,
            json!({"error": e.to_string(), "pane_id": pane.to_string()}),
        ),
    }
}

/// A git query in `dir`, trimmed; None when it fails (no clone there).
pub(super) fn git(cfg: &Config, dir: &Path, args: &[&str]) -> Option<String> {
    let out = tool(cfg, "git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// What a task came to, for the caller: read off the clone's git state once Claude stopped.
/// Pushed = clean tree with HEAD on its remote branch (a push moves origin/<branch> too, so no
/// fetch) and HEAD moved since the task was sent (`before`; unknown after a daemon restart).
pub(super) fn outcome(
    cfg: &Config,
    dir: &Path,
    before: Option<&str>,
    status: &str,
    name: &str,
) -> Value {
    let no = |m: &str| json!({"message": m, "pushed": false});
    match status {
        "working" => return json!({"message": null, "pushed": false}),
        "blocked" => return no("waiting for an answer: read the screen"),
        _ => {}
    }
    if !dir.join(".git").is_dir() {
        return no("finished: read the screen");
    }
    let head = git(cfg, dir, &["rev-parse", "HEAD"]).unwrap_or_default();
    let branch = git(cfg, dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let remote = git(cfg, dir, &["rev-parse", &format!("origin/{branch}")]).unwrap_or_default();
    let clean = git(cfg, dir, &["status", "--porcelain"]).is_some_and(|s| s.is_empty());
    let repo = git(cfg, dir, &["remote", "get-url", "origin"]).unwrap_or_default();
    let repo = repo
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("")
        .to_string();
    if clean && !head.is_empty() && head == remote && before != Some(head.as_str()) {
        json!({"message": format!("changes created and pushed to {repo}"), "pushed": true, "commit": head})
    } else if clean && head == remote {
        no("finished, nothing new to push: read the screen")
    } else {
        no(&format!("changes ready, not pushed to {branch}: read the report, then POST /api/agents/{name}/push or /discard"))
    }
}

/// Opens a session and starts Claude in it under `name`. `repo` (owner/name) is cloned to
/// ~/femme-work/<name> on first use and becomes the folder; otherwise `cwd` (under home), else
/// home. With a GitHub token on the caller's key, the clone pulls and pushes as that account and
/// commits under its name (set again on every open, so a remade key takes over). Returns the pane
/// and the brief that goes in front of the first message: the session's standing orders, with
/// `push` (default main) the branch each finished task lands on.
async fn open_agent(
    app: &Arc<App>,
    name: &str,
    input: &Value,
    caller: Option<&ApiKey>,
) -> Result<(PaneRef, String), Response> {
    let repo = text(input, "repo").trim().to_string();
    let push = input["push"].as_str().unwrap_or("main");
    if !valid_branch(push) {
        return Err(err(400, "push must be a branch name, like main"));
    }
    let cfg = app.cfg().clone();
    let dir = if repo.is_empty() {
        let (home, cwd) = (cfg.home.clone(), text(input, "cwd").to_string());
        blocking(move || home_dir(&home, &cwd))
            .await?
            .ok_or_else(|| {
                err(
                    400,
                    "cwd must be an existing folder inside home (or give repo)",
                )
            })?
    } else {
        if !valid_repo(&repo) {
            return Err(err(400, "repo must look like owner/name"));
        }
        let dir = cfg.work_root().join(name);
        let (token, git_name, git_email) = match caller {
            Some(k) => (
                k.github_token.clone().unwrap_or_default(),
                k.git_name.clone().unwrap_or_default(),
                k.git_email.clone().unwrap_or_default(),
            ),
            None => Default::default(),
        };
        let (c, r, d) = (cfg.clone(), repo.clone(), dir.clone());
        blocking(move || clone_and_set_up(&c, &r, &d, &token, &git_name, &git_email))
            .await?
            .map_err(|e| err(502, &e))?;
        dir.display().to_string()
    };
    let owner = caller.map_or_else(|| "board".to_string(), |k| format!("key:{}", k.name));
    let req = CreateSession {
        cwd: dir,
        agent: Agent::Claude,
        mode: Mode::Headless,
        label: Some(name.into()),
        name: Some(name.into()),
        owner,
        model: None,
        resume: None,
    };
    let session = app.core.create_session(req).await.map_err(core_err)?;
    let pane = tagged(&session)?;
    let mut brief =
        format!("This terminal is {name}'s standing Claude Code session on the FEMME box: every message is a new task, and the session is kept between them. ");
    if !repo.is_empty() {
        brief += &format!("The repo {repo} is cloned here on branch {push}. For each task: do it end to end and run the project's checks and tests, but do not commit and do not push: leave the changes in the working tree, the owner reads the report first and then sends a push request or a discard request. Do not open PRs. ");
    }
    brief += "If a decision is only the owner's to make, say so plainly and stop. \
        Write everything for a reader with ADHD: the point first, short lines, bullets, no paragraphs. \
        When a task is finished, write a report to the file named at the end of the task (overwrite it) and print the same. \
        The reader is not a programmer: line 1, what the change does for them, under 15 words; then up to five bullets, each under 15 words, on what they will notice or can now do, what was checked, and what is left open or needs their decision. No file names, no code, no jargon. Nothing else.\n\nTask:\n";
    Ok((pane, brief))
}

/// The blocking half of [`open_agent`] for a repo: clone it on first use (with the key's token when it
/// has one, else through the box's `gh`), and point the clone's remote and committer at the right account.
fn clone_and_set_up(
    cfg: &Config,
    repo: &str,
    dir: &Path,
    token: &str,
    git_name: &str,
    git_email: &str,
) -> Result<(), String> {
    if !dir.join(".git").is_dir() {
        let _ = fs::create_dir_all(dir.parent().unwrap());
        let out = if token.is_empty() {
            tool(cfg, "gh")
                .args(["repo", "clone", repo])
                .arg(dir)
                .args(["--", "--quiet"])
                .output()
        } else {
            tool(cfg, "git")
                .args(["clone", "-q", &clone_url(repo, token)])
                .arg(dir)
                .output()
        };
        if !out.as_ref().is_ok_and(|o| o.status.success()) {
            return Err(format!("could not clone {repo}"));
        }
    }
    if !token.is_empty() {
        git(
            cfg,
            dir,
            &["remote", "set-url", "origin", &clone_url(repo, token)],
        );
        for (k, v) in [("user.name", git_name), ("user.email", git_email)] {
            if !v.is_empty() {
                git(cfg, dir, &["config", k, v]);
            }
        }
    } else if git(cfg, dir, &["remote", "get-url", "origin"])
        .is_some_and(|u| u.contains("x-access-token"))
    {
        git(
            cfg,
            dir,
            &["remote", "set-url", "origin", &clone_url(repo, "")],
        ); // an earlier key's token: the box's gh takes over
    }
    Ok(())
}
