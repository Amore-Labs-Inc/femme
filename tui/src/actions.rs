//! What the user asks herdr to do. Each runs on its own thread and reports back as a toast, so the
//! screen never waits on a socket. The new-agent recipe is the board's (board/src/main.rs, POST
//! /api/terminals): workspace, wait for its shell, start the agent, drop the temporary name.
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::herdr::client::{Client, Error};
use crate::worker::Msg;

fn spawn(tx: Sender<Msg>, f: impl FnOnce() -> Result<String, Error> + Send + 'static) {
    thread::spawn(move || {
        let text = match f() {
            Ok(s) => s,
            Err(e) => format!("✗ {e}"),
        };
        if !text.is_empty() {
            let _ = tx.send(Msg::Toast(text));
        }
    });
}

/// A whole message to an agent: herdr pastes and submits it (`agent.prompt`). A blocked agent
/// refuses it (`agent_blocked`): it is asking something first.
pub fn prompt(sock: PathBuf, pane: String, text: String, tx: Sender<Msg>) {
    spawn(tx, move || {
        let mut c = Client::new(sock);
        match c.call_timeout("agent.prompt", json!({"target": pane, "text": text}), Duration::from_secs(15)) {
            Ok(_) => Ok(String::new()),
            Err(e) if e.code() == "agent_blocked" => Err(Error::Api {
                code: "agent_blocked".into(),
                message: "the agent is asking you something: answer it in its terminal first".into(),
            }),
            Err(e) => Err(e),
        }
    });
}

/// Raw text plus Enter into a plain shell pane.
pub fn send_text(sock: PathBuf, pane: String, text: String, tx: Sender<Msg>) {
    spawn(tx, move || {
        let mut c = Client::new(sock);
        c.call("pane.send_input", json!({"pane_id": pane, "text": text, "keys": ["enter"]}))?;
        Ok(String::new())
    });
}

pub fn close_workspace(sock: PathBuf, workspace_id: String, label: String, tx: Sender<Msg>) {
    spawn(tx, move || {
        let mut c = Client::new(sock);
        c.call("workspace.close", json!({"workspace_id": workspace_id}))?;
        Ok(format!("closed {label}"))
    });
}

/// Bring the agent's pane to the front in the herdr client (only meaningful inside one).
pub fn focus(sock: PathBuf, pane: String, workspace_id: String, shell: bool, tx: Sender<Msg>) {
    spawn(tx, move || {
        let mut c = Client::new(sock);
        if shell {
            c.call("workspace.focus", json!({"workspace_id": workspace_id}))?;
        } else {
            c.call("agent.focus", json!({"target": pane}))?;
        }
        Ok(String::new())
    });
}

/// A new workspace in `cwd` (None: herdr's default folder) running `kind` (claude, codex, pi, or
/// shell for a bare terminal), labelled `label`.
pub fn new_agent(sock: PathBuf, machine: String, cwd: Option<String>, kind: String, label: String, tx: Sender<Msg>) {
    spawn(tx, move || {
        let mut c = Client::new(sock);
        let mut params = json!({"focus": false});
        if let Some(cwd) = cwd {
            params["cwd"] = Value::String(cwd);
        }
        if !label.is_empty() {
            params["label"] = Value::String(label.clone());
        }
        let r = c.call("workspace.create", params)?;
        let pane = r["root_pane"]["pane_id"].as_str().ok_or_else(|| Error::Io("herdr opened no pane".into()))?.to_string();
        let shown = if label.is_empty() { pane.clone() } else { label.clone() };
        if kind == "shell" {
            return Ok(format!("opened {shown} on {machine}"));
        }
        wait_for_shell(&mut c, &pane);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let name = format!("t-{:x}", nanos & 0xffff_ffff);
        let start = json!({"name": name, "kind": kind, "pane_id": pane, "timeout_ms": 60000});
        if let Err(e) = c.call_timeout("agent.start", start, Duration::from_secs(70)) {
            // Claude's "do you trust this folder?" on a folder's first visit holds the start up.
            let screen = c
                .call("pane.read", json!({"pane_id": pane, "source": "visible", "format": "text"}))
                .map(|r| r["read"]["text"].as_str().unwrap_or("").to_string())
                .unwrap_or_default();
            if screen.contains("trust this folder") {
                let _ = c.call("pane.send_input", json!({"pane_id": pane, "keys": ["down", "enter"]}));
            } else {
                return Err(e);
            }
        }
        let _ = c.call("agent.rename", json!({"target": pane, "name": null}));
        Ok(format!("{kind} started in {shown} on {machine}"))
    });
}

/// The new pane's shell is up once exactly one foreground process (the shell) is there.
fn wait_for_shell(c: &mut Client, pane: &str) {
    for _ in 0..30 {
        let ready = c
            .call("pane.process_info", json!({"pane_id": pane}))
            .ok()
            .is_some_and(|r| r["process_info"]["foreground_processes"].as_array().is_some_and(|a| a.len() == 1));
        if ready {
            return;
        }
        thread::sleep(Duration::from_millis(500));
    }
}
