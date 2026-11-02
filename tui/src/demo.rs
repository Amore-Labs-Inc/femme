//! `--demo`: canned machines and screens, no herdr. For a look at the UI, screenshots, and a
//! laptop without herdr. One agent flips between working and blocked every ten seconds.
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use ansi_to_tui::IntoText;
use ratatui::text::Text;
use serde_json::{json, Value};

use crate::herdr::machines::Machine;
use crate::herdr::types::{parse_snapshot, Status};
use crate::worker::{Msg, Shared};

const AGENTBOX: &str = include_str!("../tests/fixtures/snapshot.json");

fn laptop() -> Value {
    json!({"workspaces": [{"workspace_id": "w1", "label": "refactor the pricing module", "agent_status": "working"}],
           "panes": [{"pane_id": "w1:p1", "workspace_id": "w1", "cwd": "/Users/dora/code/pricing", "agent": "claude",
                      "terminal_title_stripped": "pricing", "agent_status": "working"}]})
}

fn instinct() -> Value {
    json!({"workspaces": [{"workspace_id": "w2", "label": "whatsapp · dora", "agent_status": "idle"}],
           "panes": [{"pane_id": "w2:p1", "workspace_id": "w2", "cwd": "/home/dora/instinct", "agent": "claude",
                      "terminal_title_stripped": "instinct", "agent_status": "idle"}]})
}

fn screen_for(pane: &str, status: Status) -> Text<'static> {
    let claude = |task: &str, tail: &str| {
        format!(
            "\x1b[1m\x1b[38;5;205m✻\x1b[0m \x1b[1mClaude Code\x1b[0m \x1b[2mv2.1\x1b[0m\n\n\
             \x1b[1m>\x1b[0m {task}\n\n\
             \x1b[2m●\x1b[0m Read \x1b[36msrc/orders/page.tsx\x1b[0m\n\
             \x1b[2m●\x1b[0m Read \x1b[36msrc/lib/export.ts\x1b[0m\n\
             \x1b[32m●\x1b[0m Update \x1b[36msrc/orders/page.tsx\x1b[0m\n\
             \x1b[2m  ⎿  Added 42 lines, removed 3\x1b[0m\n\
             \x1b[32m●\x1b[0m Bash \x1b[2m(npm test -- orders)\x1b[0m\n\
             \x1b[2m  ⎿  12 passed\x1b[0m\n\n{tail}\n"
        )
    };
    let text = match (pane, status) {
        ("w1:p1", Status::Blocked) => claude(
            "add csv export to the orders page",
            "\x1b[33m?\x1b[0m Add a \"download all\" button as well, or only the filtered rows?\n  \x1b[1m❯ 1.\x1b[0m All rows\n    2. Filtered rows only\n    3. Ask me later",
        ),
        ("w1:p1", _) => claude("add csv export to the orders page", "\x1b[2m✻ Writing the export helper… (esc to interrupt)\x1b[0m"),
        ("w2:p1", _) => "\x1b[1mOpenAI Codex\x1b[0m \x1b[2m(v0.42)\x1b[0m\n\n\x1b[1m›\x1b[0m fix the login redirect loop\n\n\x1b[2mThinking…\x1b[0m\n\n\x1b[33m⚠\x1b[0m Codex wants to run \x1b[36mgit push origin fix/login-redirect\x1b[0m\n\n  \x1b[1m[y]\x1b[0m allow   \x1b[1m[n]\x1b[0m deny   \x1b[1m[a]\x1b[0m always\n".to_string(),
        ("w3:p1", _) => claude("upgrade to node 22", "\x1b[32m✓\x1b[0m Opened pull request \x1b[4mhttps://github.com/exposure/web/pull/12\x1b[0m\n\n\x1b[2mDone. 18 files changed.\x1b[0m"),
        _ => "\x1b[1mdora@agentbox\x1b[0m:\x1b[34m~\x1b[0m$ \n".to_string(),
    };
    text.as_str().into_text().unwrap_or_else(|_| Text::raw(text))
}

pub fn spawn(tx: Sender<Msg>, sel: Shared) {
    thread::spawn(move || {
        let machines = vec![
            Machine { name: "agentbox".into(), sock: "/tmp/demo-agentbox.sock".into(), local: true },
            Machine { name: "laptop".into(), sock: "/tmp/demo-laptop.sock".into(), local: false },
            Machine { name: "instinct".into(), sock: "/tmp/demo-instinct.sock".into(), local: false },
        ];
        let _ = tx.send(Msg::Machines(machines));
        let base: Value = serde_json::from_str(AGENTBOX).expect("fixture");
        let mut tick = 0u64;
        let mut last_sent: Option<(String, Status)> = None;
        loop {
            let flipped = (tick / 20) % 2 == 1; // every ten seconds at 500 ms ticks
            let mut snap = base["result"].clone();
            let status = if flipped { "blocked" } else { "working" };
            snap["snapshot"]["panes"][0]["agent_status"] = json!(status);
            snap["snapshot"]["workspaces"][0]["agent_status"] = json!(status);
            if tick % 20 == 0 {
                let _ = tx.send(Msg::Snapshot("agentbox".into(), parse_snapshot(&snap).unwrap()));
                let _ = tx.send(Msg::Snapshot("laptop".into(), parse_snapshot(&laptop()).unwrap()));
                let _ = tx.send(Msg::Snapshot("instinct".into(), parse_snapshot(&instinct()).unwrap()));
            }
            if let Some(t) = sel.lock().unwrap().clone() {
                let status = if t.target.pane == "w1:p1" { if flipped { Status::Blocked } else { Status::Working } } else { t.status };
                let key = (t.target.pane.clone(), status);
                if last_sent.as_ref() != Some(&key) {
                    let _ = tx.send(Msg::Screen(t.target.clone(), screen_for(&t.target.pane, status)));
                    last_sent = Some(key);
                }
            }
            tick += 1;
            thread::sleep(Duration::from_millis(500));
        }
    });
}
