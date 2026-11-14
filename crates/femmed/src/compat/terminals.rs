//! `/api/terminals`: the box's sessions as the board's "workspaces", the browser terminal (screen
//! and keystrokes), whole messages to the agent in a pane, and file uploads for the chat's attachments.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::response::Response;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use femme_proto::state::{Agent, Mode, PaneKind};
use serde_json::{json, Value};

use super::util::{clean_name, home_dir, random_hex, text};
use super::{blocking, core_err, err, ok, App, BodyErr, Req, MAX_UPLOAD, UPLOAD_DAYS};
use crate::core::{agent_name, parse_agent, CreateSession, PaneRef, SessionInfo, WsRef};

/// One session as a board workspace: its panes with the agent in each and its status.
fn workspace(s: &SessionInfo) -> Value {
    let agent_pane = |kind: PaneKind| kind == PaneKind::Agent;
    let panes: Vec<Value> = s
        .panes
        .iter()
        .map(|p| {
            json!({
                "pane_id": s.pane_id(p.no),
                "agent": agent_pane(p.kind).then(|| agent_name(s.agent)).flatten(),
                "status": agent_pane(p.kind).then(|| s.agent_status()).flatten(),
                "cwd": p.cwd.as_deref().unwrap_or(&s.cwd),
                "title": p.title,
            })
        })
        .collect();
    let label = s
        .label
        .clone()
        .or_else(|| s.name.clone())
        .unwrap_or_else(|| s.cwd.rsplit('/').next().unwrap_or("terminal").to_string());
    json!({"workspace_id": s.workspace_id(), "machine": s.machine, "label": label, "status": s.agent_status(), "panes": panes})
}

/// Folders you can open a new terminal in: home, and what is under Documents and femme-work.
fn folders(home: &Path) -> Vec<String> {
    let mut folders = vec![home.display().to_string()];
    for base in ["Documents", "femme-work"] {
        for d in fs::read_dir(home.join(base))
            .into_iter()
            .flatten()
            .flatten()
        {
            if d.path().is_dir() {
                folders.push(d.path().display().to_string());
                if base == "Documents" {
                    for sub in fs::read_dir(d.path()).into_iter().flatten().flatten() {
                        if sub.path().is_dir()
                            && !sub.file_name().to_string_lossy().starts_with('.')
                        {
                            folders.push(sub.path().display().to_string());
                        }
                    }
                }
            }
        }
    }
    folders.sort();
    folders
}

pub(super) async fn list(app: &Arc<App>) -> Response {
    let sessions = match app.core.terminals().await {
        Ok(s) => s,
        Err(e) => return core_err(e),
    };
    let home = app.cfg().home.clone();
    match blocking(move || folders(&home)).await {
        Ok(folders) => ok(
            json!({"workspaces": sessions.iter().map(workspace).collect::<Vec<_>>(), "folders": folders}),
        ),
        Err(r) => r,
    }
}

pub(super) async fn create(app: &Arc<App>, req: &mut Req) -> Response {
    let input = match req.json_or_400().await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let (home, cwd) = (app.cfg().home.clone(), text(&input, "cwd").to_string());
    let cwd = match blocking(move || home_dir(&home, &cwd)).await {
        Ok(Some(cwd)) => cwd,
        Ok(None) => return err(400, "folder must exist inside your home directory"),
        Err(r) => return r,
    };
    let label = input["label"]
        .as_str()
        .unwrap_or("")
        .trim()
        .chars()
        .take(60)
        .collect::<String>();
    // Anything but claude, codex or pi is a plain shell.
    let agent = input["agent"]
        .as_str()
        .and_then(parse_agent)
        .unwrap_or(Agent::Shell);
    let open = CreateSession {
        cwd,
        agent,
        mode: Mode::Tui,
        label: (!label.is_empty()).then_some(label),
        name: None,
        owner: "board".into(),
        model: None,
        resume: None,
    };
    match app.core.create_session(open).await {
        Ok(s) => match s.main_pane_id() {
            Some(pane) => ok(json!({"pane_id": pane})),
            None => err(502, "the session has no terminal"),
        },
        Err(e) => core_err(e),
    }
}

pub(super) async fn close(app: &Arc<App>, ws: &str) -> Response {
    let Some(ws) = WsRef::parse(ws) else {
        return err(404, "not found");
    };
    let sessions = match app.core.terminals().await {
        Ok(s) => s,
        Err(e) => return core_err(e),
    };
    let Some(s) = sessions
        .iter()
        .find(|s| s.handle == ws.handle && s.machine == ws.machine)
    else {
        return err(404, "not found");
    };
    match app.core.close_session(s.sid).await {
        Ok(()) => ok(json!({"ok": true})),
        Err(e) => core_err(e),
    }
}

/// A pane's screen and agent status: with its colours for the browser terminal (`ansi`), else as
/// plain text for another agent's key.
pub(super) async fn screen(app: &Arc<App>, pane: &str, ansi: bool) -> Response {
    let Some(pane) = PaneRef::parse(pane) else {
        return err(404, "not found");
    };
    match app.core.screen(&pane, ansi).await {
        Ok(s) => ok(json!({"screen": s.text, "status": s.status})),
        Err(e) => core_err(e),
    }
}

/// A whole message to the agent in the pane, pasted and submitted.
pub(super) async fn prompt(app: &Arc<App>, req: &mut Req, pane: &str) -> Response {
    let Some(pane) = PaneRef::parse(pane) else {
        return err(404, "not found");
    };
    let input = match req.json_or_400().await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let text = input["text"].as_str().unwrap_or("").replace('\0', "");
    if text.is_empty() || text.len() > 100_000 {
        return err(400, "text: 1 to 100000 bytes");
    }
    match app.core.prompt(&pane, &text).await {
        Ok(()) => ok(json!({"ok": true})),
        Err(e) => core_err(e),
    }
}

/// Raw keystrokes into the pane (answering Claude's own prompts, pasted paths).
pub(super) async fn input(app: &Arc<App>, req: &mut Req, pane: &str) -> Response {
    let Some(pane) = PaneRef::parse(pane) else {
        return err(404, "not found");
    };
    let input = match req.json_or_400().await {
        Ok(v) => v,
        Err(r) => return r,
    };
    // NUL never went to the terminal (it could not travel as a command-line argument), and still does not.
    let data = input["data"].as_str().unwrap_or("").replace('\0', "");
    if data.is_empty() || data.len() > 100_000 {
        return err(400, "send 1 to 100000 bytes");
    }
    match app.core.input(&pane, data.as_bytes()).await {
        Ok(()) => ok(json!({"ok": true})),
        Err(e) => core_err(e),
    }
}

/// A pasted or dropped file lands in `<state>/uploads`; the page then types its path into the
/// terminal, which Claude Code turns into an attached image.
pub(super) async fn upload(app: &Arc<App>, req: &mut Req) -> Response {
    let input = match req.json(MAX_UPLOAD / 3 * 4 + 4096).await {
        Ok(v) => v,
        Err(BodyErr::TooLarge) => return err(413, "files up to 20 MB"),
        Err(BodyErr::Invalid) => return err(400, "invalid JSON"),
    };
    let Ok(bytes) = BASE64.decode(text(&input, "data")) else {
        return err(400, "data must be base64");
    };
    let (dir, name) = (
        app.cfg().state_path("uploads"),
        text(&input, "name").to_string(),
    );
    match blocking(move || save_upload(&dir, &name, &bytes)).await {
        Ok(Ok(path)) => ok(json!({"path": path})),
        Ok(Err(e)) => err(500, &format!("save failed: {e}")),
        Err(r) => r,
    }
}

/// Saves an upload under a random prefix and a cleaned name (the extension tells Claude it is an
/// image), and drops uploads older than UPLOAD_DAYS on the way.
pub(super) fn save_upload(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let old = SystemTime::now() - Duration::from_secs(UPLOAD_DAYS * 86400);
    for f in fs::read_dir(dir)?.flatten() {
        if f.metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < old)
        {
            let _ = fs::remove_file(f.path());
        }
    }
    let path: PathBuf = dir.join(format!("{}-{}", &random_hex()[..8], clean_name(name)));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?
        .write_all(bytes)?;
    Ok(path.display().to_string())
}
