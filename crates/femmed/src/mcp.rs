//! `femmed mcp`: a stdio MCP server for Claude Code, so a Claude anywhere (the laptop's, through
//! `ssh -T box femmed mcp`; see `plugin/claude`) can see and drive the box's chats: list them,
//! read one, start one, send one a message. It is a client of the daemon over its socket, like
//! `femmed open`.
//!
//! The protocol is JSON-RPC 2.0, one message per line on stdin/stdout. Only what Claude Code needs
//! for tools is answered: `initialize`, `ping`, `tools/list`, `tools/call`; notifications get no
//! answer, any other request `-32601`.
//! ponytail: hand-rolled, four methods; take an MCP crate when resources or prompts are wanted.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use femme_client::{Client, Event};
use femme_proto::rpc::{PromptHow, Req, Resp, Scope};
use femme_proto::state::{Agent, ItemKind, Mode, Row, Session};
use femme_proto::Sid;
use femme_transport::DeviceIdentity;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;
use ulid::Ulid;

use crate::config;

/// How long a tool waits for the daemon.
const WAIT: Duration = Duration::from_secs(20);

const INSTRUCTIONS: &str = "FEMME runs Claude Code, Codex and Pi agents on the user's own server \
(the box), where they keep working after the laptop closes; the user follows them in the FEMME \
desktop and phone apps. Use these tools when the user asks about their agents or chats on the box, \
wants work started there, or wants to hand a running agent a message. Chats are named by id (or the \
short handle list_chats shows).";

/// The daemon's sessions, kept current from the `Global` scope.
#[derive(Default)]
struct Chats {
    by_id: HashMap<Sid, Session>,
    /// The first rows came.
    loaded: bool,
}

/// Runs the server until stdin closes.
pub async fn run() -> anyhow::Result<()> {
    let sock = config::sock_path();
    anyhow::ensure!(sock.exists(), "no FEMME daemon at {}", sock.display());
    let mut proxy = tokio::process::Command::new(std::env::current_exe()?);
    proxy.arg("proxy");
    let mut cfg =
        femme_client::Config::new(DeviceIdentity::generate("femmed mcp"), Ulid::generate());
    cfg.ssh_command = Some(proxy);
    let client = Client::start(cfg);

    let chats = Arc::new(Mutex::new(Chats::default()));
    let loaded = Arc::new(Notify::new());
    let mut events = client.events();
    {
        let (chats, loaded) = (chats.clone(), loaded.clone());
        tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                let mut c = chats.lock().unwrap_or_else(|e| e.into_inner());
                match ev {
                    Event::Reset {
                        scope: Scope::Global,
                    } => c.by_id.clear(),
                    Event::Changes {
                        scope: Scope::Global,
                        rows,
                        ..
                    } => {
                        for row in rows {
                            match row {
                                Row::Session(s) => {
                                    c.by_id.insert(s.id, s);
                                }
                                Row::Tombstone { id, .. } => {
                                    if let Ok(sid) = id.parse() {
                                        c.by_id.remove(&sid);
                                    }
                                }
                                _ => {}
                            }
                        }
                        c.loaded = true;
                        loaded.notify_waiters();
                    }
                    _ => {}
                }
            }
        });
    }
    client
        .subscribe(Scope::Global)
        .await
        .map_err(|e| anyhow::anyhow!("subscribe: {e}"))?;

    let srv = Server {
        client,
        chats,
        loaded,
    };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue; // a notification
        };
        let method = msg["method"].as_str().unwrap_or("");
        let reply = match srv.handle(method, &msg["params"]).await {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        };
        let mut text = reply.to_string();
        text.push('\n');
        out.write_all(text.as_bytes()).await?;
        out.flush().await?;
    }
    Ok(())
}

struct Server {
    client: Client,
    chats: Arc<Mutex<Chats>>,
    loaded: Arc<Notify>,
}

impl Server {
    async fn handle(&self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => Ok(json!({
                // the client's own version, which every one of these four methods fits
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "femme", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let args = &params["arguments"];
                Ok(match self.call(name, args).await {
                    Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
                    Err(e) => {
                        json!({"content": [{"type": "text", "text": e.to_string()}], "isError": true})
                    }
                })
            }
            _ => Err((-32601, format!("method not found: {method}"))),
        }
    }

    async fn call(&self, name: &str, a: &Value) -> anyhow::Result<String> {
        let s = |k: &str| a[k].as_str().filter(|v| !v.is_empty()).map(str::to_owned);
        match name {
            "list_chats" => self.list().await,
            "read_chat" => {
                let sid = self.sid(&s("chat").unwrap_or_default()).await?;
                let limit = a["limit"].as_u64().unwrap_or(20).clamp(1, 200) as u16;
                self.read(sid, limit).await
            }
            "start_chat" => {
                let cwd = s("cwd").ok_or_else(|| anyhow::anyhow!("cwd is required"))?;
                let req = Req::SessionCreate {
                    cwd,
                    agent: Agent::Claude,
                    mode: Mode::Headless,
                    label: s("label"),
                    name: None,
                    prompt: s("prompt"),
                    model: s("model"),
                    resume: s("resume"),
                    fork: a["fork"].as_bool().unwrap_or(false),
                };
                match self.client.request_timeout(req, WAIT).await? {
                    Resp::Session(sid) => Ok(format!("started chat {sid}")),
                    other => anyhow::bail!("unexpected answer {other:?}"),
                }
            }
            "send_message" => {
                let sid = self.sid(&s("chat").unwrap_or_default()).await?;
                let text = s("text").ok_or_else(|| anyhow::anyhow!("text is required"))?;
                let req = Req::Prompt {
                    sid,
                    text,
                    attachments: Vec::new(),
                    how: PromptHow::Queue,
                };
                self.client.request_timeout(req, WAIT).await?;
                Ok(format!(
                    "sent to {sid}; it runs after the chat's current turn"
                ))
            }
            _ => anyhow::bail!("no tool {name}"),
        }
    }

    /// Waits for the first rows of the session list (a fresh connection), up to [`WAIT`].
    async fn ready(&self) {
        let notified = self.loaded.notified();
        if self.chats.lock().unwrap_or_else(|e| e.into_inner()).loaded {
            return;
        }
        let _ = tokio::time::timeout(WAIT, notified).await;
    }

    /// A chat by its id or its handle.
    async fn sid(&self, chat: &str) -> anyhow::Result<Sid> {
        if let Ok(sid) = chat.parse() {
            return Ok(sid);
        }
        self.ready().await;
        let c = self.chats.lock().unwrap_or_else(|e| e.into_inner());
        c.by_id
            .values()
            .find(|s| s.handle == chat)
            .map(|s| s.id)
            .ok_or_else(|| anyhow::anyhow!("no chat {chat:?}: use an id or handle from list_chats"))
    }

    async fn list(&self) -> anyhow::Result<String> {
        self.ready().await;
        let c = self.chats.lock().unwrap_or_else(|e| e.into_inner());
        let mut open: Vec<&Session> = c.by_id.values().filter(|s| s.closed.is_none()).collect();
        open.sort_by_key(|s| std::cmp::Reverse(s.created));
        if open.is_empty() {
            return Ok("no open chats".into());
        }
        Ok(open
            .iter()
            .map(|s| {
                format!(
                    "{} ({}) {:?} {:?}/{:?} {}{}{}",
                    s.id,
                    s.handle,
                    s.agent,
                    s.status,
                    s.attention,
                    s.cwd,
                    s.label
                        .as_deref()
                        .map(|l| format!(" \"{l}\""))
                        .unwrap_or_default(),
                    s.model
                        .as_deref()
                        .map(|m| format!(" [{m}]"))
                        .unwrap_or_default(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn read(&self, sid: Sid, limit: u16) -> anyhow::Result<String> {
        let req = Req::TranscriptPage {
            sid,
            before_seq: u64::MAX,
            limit,
        };
        let Resp::Page { mut items, .. } = self.client.request_timeout(req, WAIT).await? else {
            anyhow::bail!("unexpected answer");
        };
        items.sort_by_key(|i| i.seq);
        let lines: Vec<String> = items
            .iter()
            .filter(|i| i.parent.is_none())
            .filter_map(|i| {
                let who = match i.kind {
                    ItemKind::UserMessage => "user",
                    ItemKind::PeerMessage => "peer",
                    ItemKind::AssistantMessage => "assistant",
                    ItemKind::ToolCall => "tool",
                    ItemKind::Question => "question",
                    ItemKind::Plan => "plan",
                    ItemKind::Event => "event",
                    _ => return None,
                };
                Some(format!("[{who}] {}", i.preview.trim()))
            })
            .collect();
        Ok(if lines.is_empty() {
            "nothing yet".into()
        } else {
            lines.join("\n\n")
        })
    }
}

/// The tools' definitions.
fn tools() -> Value {
    let chat =
        json!({"type": "string", "description": "The chat's id or handle (from list_chats)"});
    json!([
        {
            "name": "list_chats",
            "description": "The open agent chats on the user's FEMME box: id, handle, agent, status/attention, folder, label, model.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "read_chat",
            "description": "The latest messages of one chat on the box (user, assistant, tool calls, questions), oldest first.",
            "inputSchema": {"type": "object", "properties": {
                "chat": chat,
                "limit": {"type": "integer", "description": "How many recent items (default 20, at most 200)"},
            }, "required": ["chat"]},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "start_chat",
            "description": "Start a Claude Code chat on the box in a folder there, optionally with a first message. It keeps running after this session ends. Returns its id.",
            "inputSchema": {"type": "object", "properties": {
                "cwd": {"type": "string", "description": "Absolute folder on the box"},
                "prompt": {"type": "string", "description": "The first message"},
                "label": {"type": "string", "description": "A name for the chat in the apps"},
                "model": {"type": "string", "description": "Model (default: Claude's own)"},
                "resume": {"type": "string", "description": "A Claude conversation uuid on the box to go on with"},
                "fork": {"type": "boolean", "description": "With resume: start as a copy, leaving the original untouched"},
            }, "required": ["cwd"]},
        },
        {
            "name": "send_message",
            "description": "Send a chat on the box a message, as the user would. It runs after the chat's current turn.",
            "inputSchema": {"type": "object", "properties": {
                "chat": chat,
                "text": {"type": "string", "description": "The message"},
            }, "required": ["chat", "text"]},
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_name_schema_and_description() {
        let t = tools();
        let names: Vec<&str> = t
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["list_chats", "read_chat", "start_chat", "send_message"]
        );
        for x in t.as_array().unwrap() {
            assert_eq!(x["inputSchema"]["type"], "object");
            assert!(!x["description"].as_str().unwrap().is_empty());
        }
    }
}
