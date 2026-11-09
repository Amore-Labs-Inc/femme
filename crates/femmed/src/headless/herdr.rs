//! Claude chats running in herdr panes on this machine, mirrored as headless sessions: herdr's
//! `pane list` says which panes run Claude and in which conversation, the conversation's own
//! transcript (`~/.claude/projects/*/<uuid>.jsonl`) is tailed through the Claude driver into
//! transcript items, and a prompt from a client is typed into the pane by `herdr agent prompt`.
//!
//! Subagents (Claude's `Agent` tool) write their own transcripts beside it,
//! `<uuid>/subagents/agent-<id>.jsonl` with `agent-<id>.meta.json` naming the spawning tool call
//! (`toolUseId`); those are tailed too, their lines marked with that call as `parent_tool_use_id`
//! the way the stream marks a subagent's, so their items nest under the call.
//!
//! A mirrored session is `Mode::Headless` with owner `herdr:<herdr session>:<pane>` and the
//! conversation's uuid as `agent_sid`, so clients show it like any headless chat. Its status is
//! herdr's. Closing it in FEMME only hides it: the pane runs on, and the same conversation in the
//! same pane does not come back (a new conversation there does).

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Weak;
use std::time::Duration;

use femme_agents::{driver_for, Driver, Out};
use femme_proto::state::{Agent, Attention, Mode, Pane, PaneKind, Session, SessionStatus};
use femme_proto::{Id, PaneId, Sid};
use femme_store::sessions;
use serde_json::Value;
use tokio::sync::mpsc;

use super::{fold, put, Cmd, FoldState, Head, Put};
use crate::core::CoreError;
use crate::daemon::{now_ms, Daemon};
use crate::holder_client::SpoolLines;
use crate::session::{find_on_path, label, MAIN};

/// The owner prefix of a mirrored session.
pub(crate) const OWNER: &str = "herdr:";
/// A conversation seen for the first time shows this much of its end.
// ponytail: the last 4 MiB, transcripts grow past 30 MB; page further back when someone misses it.
const BACKFILL: u64 = 4 << 20;
/// The most read from one transcript per tick.
const READ_MAX: u64 = 8 << 20;

/// A herdr pane that runs Claude.
#[derive(Debug, Clone, PartialEq)]
struct HPane {
    hs: String,
    pane: String,
    uuid: String,
    status: String,
    cwd: String,
    title: String,
}

impl HPane {
    fn owner(&self) -> String {
        format!("{OWNER}{}:{}", self.hs, self.pane)
    }
}

/// Polls herdr every 2 s and the transcripts every 250 ms, until the daemon goes. No `herdr` on
/// the tool path: nothing to mirror.
pub(crate) async fn run(d: Weak<Daemon>) {
    let Some(bin) = d
        .upgrade()
        .and_then(|d| find_on_path("herdr", &d.sessions.path(&d.cfg)))
    else {
        tracing::info!("no herdr: its chats are not mirrored");
        return;
    };
    let mut tails: HashMap<Sid, Tail> = HashMap::new();
    // ponytail: a stat and a read per transcript per tick; inotify if the panes get many
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    for n in 0u64.. {
        tick.tick().await;
        let Some(d) = d.upgrade() else { return };
        if n % 8 == 0 {
            // a herdr that does not answer changes nothing: no pane is taken for gone
            if let Some(panes) = list(&bin, &d.cfg.home).await {
                sync(&d, &bin, &panes, &mut tails).await;
            }
        }
        for t in tails.values_mut() {
            t.tail(&d).await;
        }
    }
}

/// Every Claude pane of every running herdr session. The conversation is Claude's own word for it
/// (`~/.claude/sessions/<pid>.json`) when there is one: herdr takes it from Claude's hooks, so any
/// `claude -p` run inside the pane overwrites it.
async fn list(bin: &str, home: &Path) -> Option<Vec<HPane>> {
    let own = conversations(home);
    let v: Value = serde_json::from_str(
        &herdr(bin, None, &["session", "list", "--json"])
            .await
            .ok()?,
    )
    .ok()?;
    let mut panes = Vec::new();
    for s in v["sessions"].as_array()? {
        let (Some(name), true) = (s["name"].as_str(), s["running"] == true) else {
            continue;
        };
        let out = herdr(bin, Some(name), &["pane", "list"]).await.ok()?;
        let sock = s["socket_path"].as_str().unwrap_or("");
        panes.extend(parse_panes(name, &out).into_iter().map(|mut p| {
            if let Some(u) = own.get(&(sock.to_owned(), p.pane.clone())) {
                p.uuid = u.clone();
            }
            p
        }));
    }
    Some(panes)
}

/// `(herdr socket, pane) -> conversation` of every interactive Claude that runs in a herdr pane.
fn conversations(home: &Path) -> HashMap<(String, String), String> {
    let mut out = HashMap::new();
    let Ok(dir) = std::fs::read_dir(home.join(".claude/sessions")) else {
        return out;
    };
    for e in dir.flatten() {
        let Some(v) = std::fs::read(e.path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        else {
            continue;
        };
        let (Some(pid), Some(conv)) = (v["pid"].as_u64(), v["sessionId"].as_str()) else {
            continue;
        };
        if v["kind"] != "interactive" {
            continue;
        }
        let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) else {
            continue; // gone
        };
        let var = |k: &str| {
            env.split(|&b| b == 0)
                .find_map(|kv| kv.strip_prefix(k.as_bytes()))
                .map(|v| String::from_utf8_lossy(v).into_owned())
        };
        if let (Some(sock), Some(pane)) = (var("HERDR_SOCKET_PATH="), var("HERDR_PANE_ID=")) {
            out.insert((sock, pane), conv.to_owned());
        }
    }
    out
}

fn parse_panes(hs: &str, json: &str) -> Vec<HPane> {
    let v: Value = serde_json::from_str(json).unwrap_or_default();
    let s = |p: &Value, k: &str| p[k].as_str().unwrap_or("").to_owned();
    v["result"]["panes"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|p| p["agent"] == "claude")
        .filter_map(|p| {
            Some(HPane {
                hs: hs.to_owned(),
                pane: s(p, "pane_id"),
                uuid: p["agent_session"]["value"].as_str()?.to_owned(),
                status: s(p, "agent_status"),
                cwd: s(p, "cwd"),
                title: s(p, "terminal_title_stripped"),
            })
        })
        .filter(|p| !p.pane.is_empty() && !p.uuid.is_empty())
        .collect()
}

/// herdr's agent status as the session's.
fn status_of(s: &str) -> Option<(SessionStatus, Attention)> {
    Some(match s {
        "working" => (SessionStatus::Working, Attention::None),
        "blocked" => (SessionStatus::Blocked, Attention::Blocked),
        "done" => (SessionStatus::Idle, Attention::Done),
        "idle" => (SessionStatus::Idle, Attention::None),
        _ => return None,
    })
}

/// Makes the mirrored sessions match the panes: new conversations get a session, ones whose pane
/// is gone (or runs another conversation now) are closed, statuses and titles follow herdr.
async fn sync(d: &Daemon, bin: &str, panes: &[HPane], tails: &mut HashMap<Sid, Tail>) {
    let rows: Vec<Session> = d
        .store
        .read(|c| sessions::list_sessions(c, true))
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.owner.starts_with(OWNER))
        .collect();
    let row_of = |p: &HPane| {
        rows.iter()
            .find(|s| s.owner == p.owner() && s.agent_sid.as_deref() == Some(&p.uuid))
    };
    for s in rows.iter().filter(|s| s.closed.is_none()) {
        if !panes
            .iter()
            .any(|p| row_of(p).is_some_and(|r| r.id == s.id))
        {
            // gone, not closed: a closed row is one the user hid, and stays hidden
            tails.remove(&s.id);
            if let Some(h) = d.sessions.head(s.id) {
                h.close().await;
                d.sessions.unhead(s.id);
            }
            if let Err(e) = d.write(|tx, _| tx.delete_session(s.id)) {
                tracing::warn!(sid = %s.id, %e, "herdr session not dropped");
            }
        }
    }
    let mut open = HashSet::new();
    for p in panes {
        let s = match row_of(p) {
            Some(s) if s.closed.is_some() => continue, // closed in FEMME: hidden
            Some(s) => s.clone(),
            None => match insert(d, p) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(pane = %p.pane, %e, "herdr session not stored");
                    continue;
                }
            },
        };
        open.insert(s.id);
        if let std::collections::hash_map::Entry::Vacant(e) = tails.entry(s.id) {
            let from = d
                .store
                .read(|c| sessions::spool_committed(c, s.id))
                .unwrap_or(0);
            d.sessions
                .set_head(s.id, head(bin.into(), p.hs.clone(), p.pane.clone(), s.id));
            e.insert(Tail::new(s.id, p.uuid.clone(), from));
        }
        let state = status_of(&p.status);
        let title = (!p.title.is_empty()).then(|| p.title.clone());
        let stale = state.is_some_and(|st| st != (s.status, s.attention)) || title != s.label;
        if stale {
            match d.update(s.id, |s| {
                if let Some((status, attention)) = state {
                    s.status = status;
                    s.attention = attention;
                }
                s.label = title.clone();
            }) {
                Ok(after) if after.attention != s.attention => {
                    d.notifier.attention(s.id, &label(&after), after.attention)
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(sid = %s.id, %e, "herdr session row not updated"),
            }
        }
    }
    // closed in FEMME since the last round: nothing more is written to it
    tails.retain(|sid, _| open.contains(sid));
}

/// A new conversation's session and its main pane (no attach state: `reconcile` leaves it to us).
fn insert(d: &Daemon, p: &HPane) -> Result<Session, CoreError> {
    let (status, attention) =
        status_of(&p.status).unwrap_or((SessionStatus::Idle, Attention::None));
    d.write(|tx, now| {
        let s = tx.insert_session(&Session {
            id: Id(0),
            handle: String::new(),
            rev: 0,
            name: None,
            owner: p.owner(),
            agent: Agent::Claude,
            mode: Mode::Headless,
            cwd: p.cwd.clone(),
            label: (!p.title.is_empty()).then(|| p.title.clone()),
            agent_sid: Some(p.uuid.clone()),
            model: None,
            status,
            attention,
            created: now,
            closed: None,
            permission: None,
            context: None,
            effort: None,
        })?;
        tx.upsert_pane(&Pane {
            id: PaneId {
                sid: s.id,
                no: MAIN,
            },
            rev: 0,
            kind: PaneKind::Agent,
            title: String::new(),
        })?;
        Ok(s)
    })
}

// ---- the transcript -----------------------------------------------------------------------------

/// One conversation's transcript, read on from where the store has it.
struct Tail {
    sid: Sid,
    uuid: String,
    path: Option<PathBuf>,
    lines: SpoolLines,
    /// The file is read up to here.
    read: u64,
    /// Where the store has it (`spool_committed`).
    committed: u64,
    /// Started inside a line (the backfill cut): the first one is a piece.
    skip: bool,
    driver: Box<dyn Driver>,
    fold: FoldState,
    model: Option<String>,
    permission: Option<String>,
    /// What the lines read so far said of the context, for the row: what the last call read,
    /// the window, and the compactions since the last row write.
    ctx_used: Option<u64>,
    ctx_window: Option<u64>,
    compacted: u32,
    pending: Vec<Put>,
    /// The subagent transcripts beside it, by path.
    // ponytail: their offsets live only here, so a daemon restart reads them again from the start;
    // that writes nothing (the items are the same), persist them if the reading ever shows
    subs: HashMap<PathBuf, SubTail>,
}

/// A subagent's transcript, read on from where it was read.
struct SubTail {
    /// The `Agent` tool call that spawned it.
    parent: String,
    lines: SpoolLines,
    /// The file is read up to here.
    read: u64,
}

/// What a tick read from a subagent's transcript: its path, its parent when it is new, where the
/// data starts and the data (`None`: shorter than read, rewritten).
type SubRead = (PathBuf, Option<String>, u64, Option<Vec<u8>>);

impl Tail {
    fn new(sid: Sid, uuid: String, committed: u64) -> Tail {
        Tail {
            sid,
            uuid,
            path: None,
            lines: SpoolLines::new(committed),
            read: committed,
            committed,
            skip: false,
            driver: driver_for(Agent::Claude),
            fold: FoldState::default(),
            model: None,
            permission: None,
            ctx_used: None,
            ctx_window: None,
            compacted: 0,
            pending: Vec::new(),
            subs: HashMap::new(),
        }
    }

    async fn tail(&mut self, d: &Daemon) {
        if self.path.is_none() {
            let Some(path) = find(&d.cfg.home, &self.uuid) else {
                return; // no message yet
            };
            let len = std::fs::metadata(&path).map_or(0, |m| m.len());
            if self.committed == 0 && len > BACKFILL {
                self.read = len - BACKFILL;
                self.lines = SpoolLines::new(self.read);
                self.skip = true;
            }
            self.path = Some(path);
        }
        let Some(path) = self.path.clone() else {
            return;
        };
        let at = self.read;
        let dir = path.with_extension("").join("subagents");
        let data = match tokio::task::spawn_blocking(move || read_from(&path, at)).await {
            Ok(Ok(Some(data))) => data,
            Ok(Ok(None)) => {
                // shorter than what was read: rewritten, start over
                *self = Tail::new(self.sid, self.uuid.clone(), 0);
                return;
            }
            _ => return,
        };
        let known: Vec<(PathBuf, u64)> =
            self.subs.iter().map(|(p, t)| (p.clone(), t.read)).collect();
        let subs = tokio::task::spawn_blocking(move || read_subs(&dir, &known))
            .await
            .unwrap_or_default();
        let sub_data = subs
            .iter()
            .any(|s| s.3.as_ref().is_none_or(|d| !d.is_empty()));
        if data.is_empty() && !sub_data && self.pending.is_empty() {
            return;
        }
        self.lines.push(at, &data);
        self.read += data.len() as u64;
        let mut puts = std::mem::take(&mut self.pending);
        let mut out = Out::default();
        let uploads = d.cfg.state_path("uploads").join(self.sid.to_string());
        while let Some((at, line)) = self.lines.next_line() {
            if std::mem::take(&mut self.skip) {
                continue;
            }
            let Some(line) = disk_line(&line, &uploads) else {
                continue;
            };
            let events = self.driver.on_line(at, &line, &mut out);
            out.stdin.clear(); // nobody listens: the pane's Claude reads its own terminal
            let f = fold(
                &mut self.fold,
                self.sid,
                &format!("d{at}"),
                events,
                now_ms(),
            );
            puts.extend(f.puts);
            if f.model.is_some() {
                self.model = f.model;
            }
            if f.permission.is_some() {
                self.permission = f.permission;
            }
            if f.ctx_used.is_some() {
                self.ctx_used = f.ctx_used;
            }
            if f.ctx_window.is_some() {
                self.ctx_window = f.ctx_window;
            }
            self.compacted += f.compacted;
        }
        for (path, parent, at, data) in subs {
            self.tail_sub(path, parent, at, data, &mut puts, &mut out);
        }
        let end = self.lines.offset();
        if puts.is_empty() && end == self.committed {
            return;
        }
        let sid = self.sid;
        let r = d.write_items(|tx, _, published| {
            for p in &puts {
                put(tx, sid, p, published)?;
            }
            tx.set_spool_committed(sid, end)
        });
        match r {
            Ok(()) => self.committed = end,
            Err(e) => {
                tracing::warn!(%sid, %e, "herdr transcript not stored, kept for later");
                self.pending = puts;
            }
        }
        if let Some(m) = self.model.take() {
            let _ = d.update(sid, |s| s.model = Some(m));
        }
        if let Some(p) = self.permission.take() {
            let _ = d.update(sid, |s| s.permission = Some(p));
        }
        let (used, window, compacted) = (
            self.ctx_used.take(),
            self.ctx_window.take(),
            std::mem::take(&mut self.compacted),
        );
        if used.is_some() || window.is_some() || compacted > 0 {
            // ponytail: a file rewritten under us is read again from its start and its
            // compactions counted again (the level overstates); keep the count in the row's
            // store if that ever shows
            let _ = d.update(sid, |s| {
                s.context = super::with_context(s.context, used, window, compacted);
            });
        }
    }
}

impl Tail {
    /// Runs what was read from one subagent transcript through the driver as that subagent's.
    fn tail_sub(
        &mut self,
        path: PathBuf,
        parent: Option<String>,
        at: u64,
        data: Option<Vec<u8>>,
        puts: &mut Vec<Put>,
        out: &mut Out,
    ) {
        let t = match (self.subs.entry(path), parent) {
            (std::collections::hash_map::Entry::Occupied(e), _) => e.into_mut(),
            (std::collections::hash_map::Entry::Vacant(e), Some(parent)) => e.insert(SubTail {
                parent,
                lines: SpoolLines::new(0),
                read: 0,
            }),
            (std::collections::hash_map::Entry::Vacant(_), None) => return,
        };
        let Some(data) = data else {
            // rewritten: read it again (the items it gives again are the same ones)
            t.lines = SpoolLines::new(0);
            t.read = 0;
            return;
        };
        t.lines.push(at, &data);
        t.read = at + data.len() as u64;
        while let Some((at, line)) = t.lines.next_line() {
            let Some(line) = sub_line(&line, &t.parent) else {
                continue;
            };
            let events = self.driver.on_line(at, &line, out);
            out.stdin.clear();
            let f = fold(
                &mut self.fold,
                self.sid,
                &format!("d{}:{at}", t.parent),
                events,
                now_ms(),
            );
            puts.extend(f.puts);
        }
    }
}

/// Reads on in every subagent transcript of `dir`: the `known` ones from where they were read,
/// a new one from the start once its `.meta.json` names the tool call that spawned it.
fn read_subs(dir: &Path, known: &[(PathBuf, u64)]) -> Vec<SubRead> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let path = e.path();
        let Some(stem) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".jsonl"))
            .filter(|n| n.starts_with("agent-"))
        else {
            continue;
        };
        let (parent, at) = match known.iter().find(|(p, _)| *p == path) {
            Some((_, at)) => (None, *at),
            None => {
                let meta = std::fs::read(dir.join(format!("{stem}.meta.json")))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
                let Some(id) = meta.as_ref().and_then(|m| m["toolUseId"].as_str()) else {
                    continue; // not said yet: next tick
                };
                (Some(id.to_owned()), 0)
            }
        };
        if let Ok(data) = read_from(&path, at) {
            out.push((path, parent, at, data));
        }
    }
    out
}

/// `<home>/.claude/projects/*/<uuid>.jsonl`. The folder is the cwd with every character that is
/// not alphanumeric made '-', so it is looked up, not rebuilt.
fn find(home: &Path, uuid: &str) -> Option<PathBuf> {
    let name = format!("{uuid}.jsonl");
    std::fs::read_dir(home.join(".claude/projects"))
        .ok()?
        .flatten()
        .map(|e| e.path().join(&name))
        .find(|p| p.is_file())
}

/// What the file holds from `at` on (at most [`READ_MAX`]); `None` when it is shorter than `at`.
fn read_from(path: &Path, at: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if len < at {
        return Ok(None);
    }
    f.seek(SeekFrom::Start(at))?;
    let mut data = Vec::new();
    f.take(READ_MAX).read_to_end(&mut data)?;
    Ok(Some(data))
}

/// A line of Claude's transcript file as the driver reads it from `--output-format stream-json`,
/// or `None` for the file's own bookkeeping. A typed prompt becomes the replay the stream would
/// echo (`isReplay`), and a turn's `turn_duration` the `result` that ends it. A pasted picture is
/// saved in `uploads` and named by an `Attached:` line, as a desktop attachment is.
fn disk_line(line: &[u8], uploads: &Path) -> Option<Vec<u8>> {
    let mut v: Value = serde_json::from_slice(line).ok()?;
    if v["isSidechain"] == true {
        return None;
    }
    if v["type"] == "user" {
        attach_images(&mut v, uploads);
    }
    serde_json::to_vec(&disk_value(v)?).ok()
}

/// A typed message's pictures (base64 `image` blocks) as files in `dir`, and its content as text
/// with an `Attached:` line per file in place of Claude's `[Image #N]` markers. A file is named by
/// its bytes' hash, so reading the transcript again finds it instead of writing another.
fn attach_images(v: &mut Value, dir: &Path) {
    use base64::Engine as _;
    use sha2::Digest as _;
    let Some(blocks) = v["message"]["content"].as_array() else {
        return;
    };
    if blocks.iter().any(|b| b["type"] == "tool_result") {
        return;
    }
    let mut text = Vec::new();
    let mut paths = Vec::new();
    for b in blocks {
        match b["type"].as_str() {
            Some("text") => text.push(b["text"].as_str().unwrap_or_default().to_owned()),
            Some("image") => {
                let Some(bytes) = b["source"]["data"]
                    .as_str()
                    .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok())
                else {
                    continue;
                };
                let ext = match b["source"]["media_type"].as_str() {
                    Some("image/jpeg") => "jpg",
                    Some("image/gif") => "gif",
                    Some("image/webp") => "webp",
                    _ => "png",
                };
                let hash = sha2::Sha256::digest(&bytes);
                let hex: String = hash[..6].iter().map(|b| format!("{b:02x}")).collect();
                let path = dir.join(format!("paste-{hex}.{ext}"));
                if path.exists() || save_new(dir, &path, &bytes).is_ok() {
                    paths.push(path);
                }
            }
            _ => {}
        }
    }
    if paths.is_empty() {
        return;
    }
    let text = text.join("\n");
    let text = strip_image_marks(&text);
    v["message"]["content"] = Value::String(crate::uploads::with_lines(text.trim(), &paths));
}

/// `path` with `bytes`, its folder and the file private (700, 600), as uploads are.
fn save_new(dir: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)
}

/// The text without Claude's `[Image #N]` markers (the pictures are attached instead).
fn strip_image_marks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("[Image #") {
        out.push_str(&rest[..i]);
        let after = &rest[i + "[Image #".len()..];
        let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        match after[digits..].strip_prefix(']') {
            Some(tail) if digits > 0 => rest = tail,
            _ => {
                out.push_str("[Image #");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A line of a subagent's transcript (every one a sidechain) as the stream shows a subagent's:
/// its messages only, marked with the spawning tool call `parent`.
fn sub_line(line: &[u8], parent: &str) -> Option<Vec<u8>> {
    let v: Value = serde_json::from_slice(line).ok()?;
    if !matches!(v["type"].as_str(), Some("assistant" | "user")) {
        return None;
    }
    let mut v = disk_value(v)?;
    v["parent_tool_use_id"] = Value::String(parent.to_owned());
    serde_json::to_vec(&v).ok()
}

/// [`disk_line`] on a parsed line, sidechain or not.
fn disk_value(mut v: Value) -> Option<Value> {
    if v["isMeta"] == true {
        return None;
    }
    match v["type"].as_str()? {
        "assistant" => {}
        "user" => {
            let content = &v["message"]["content"];
            let result = content
                .as_array()
                .is_some_and(|a| a.iter().any(|b| b["type"] == "tool_result"));
            let text = match content {
                Value::String(s) => s.as_str(),
                Value::Array(a) => a.first().and_then(|b| b["text"].as_str()).unwrap_or(""),
                _ => "",
            };
            if let Some(n) = text.trim_start().strip_prefix("<task-notification>") {
                // a background task's report, not something typed: the stream's
                // `task_notification`, so the call it names (a background agent) is let go
                let tag = |t: &str| {
                    n.split_once(&format!("<{t}>"))
                        .and_then(|(_, r)| r.split_once(&format!("</{t}>")))
                        .map(|(s, _)| s.trim().to_owned())
                };
                v = serde_json::json!({
                    "type": "system",
                    "subtype": "task_notification",
                    "tool_use_id": tag("tool-use-id"),
                    "status": tag("status"),
                    "summary": tag("summary").unwrap_or_else(|| "A background task reported".into()),
                    "uuid": v["uuid"],
                });
            } else if !result && !text.starts_with("[Request interrupted") {
                v["isReplay"] = Value::Bool(true);
            }
        }
        "system" if v["subtype"] == "turn_duration" => {
            v = serde_json::json!({
                "type": "result",
                "subtype": "success",
                "is_error": false,
                "num_turns": 1,
                "duration_ms": v["durationMs"],
            });
        }
        "system" => {}
        // the TUI notes every mode change (Shift+Tab): the stream says it as a status
        "permission-mode" => {
            v = serde_json::json!({
                "type": "system",
                "subtype": "status",
                "permissionMode": v["permissionMode"],
            });
        }
        _ => return None,
    }
    Some(v)
}

// ---- the way in ---------------------------------------------------------------------------------

/// The session's head: a prompt is typed into the pane, an interrupt is its Escape. Answering,
/// models and takeovers are the pane's.
fn head(bin: String, hs: String, pane: String, sid: Sid) -> Head {
    let (tx, mut rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let not_here = || Err(CoreError::Bad("a herdr pane: do that in the pane".into()));
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Cmd::Prompt { text, reply } => {
                    if super::model_switch(&text).is_some_and(|m| super::is_gateway(Some(&m))) {
                        let why =
                            "a herdr pane's Claude cannot move to DeepSeek: start a FEMME chat";
                        let _ = reply.send(Err(CoreError::Bad(why.into())));
                        continue;
                    }
                    let r = herdr(&bin, Some(&hs), &["agent", "prompt", &pane, &text]).await;
                    if let Err(e) = &r {
                        tracing::debug!(%sid, %e, "prompt not typed into the herdr pane");
                    }
                    let _ = reply.send(r.map(drop));
                }
                Cmd::Interrupt { reply } => {
                    let r = herdr(&bin, Some(&hs), &["pane", "send-keys", &pane, "esc"]).await;
                    let _ = reply.send(r.map(drop));
                }
                // the TUI backgrounds its foreground task on Ctrl+B (every one: it has no per-call key)
                Cmd::Background { reply, .. } => {
                    let r = herdr(&bin, Some(&hs), &["pane", "send-keys", &pane, "ctrl+b"]).await;
                    let _ = reply.send(r.map(drop));
                }
                // the TUI steps through its modes on Shift+Tab; the file says where it landed
                Cmd::SetPermission { reply, .. } => {
                    let r =
                        herdr(&bin, Some(&hs), &["pane", "send-keys", &pane, "shift+tab"]).await;
                    let _ = reply.send(r.map(drop));
                }
                Cmd::Answer { reply, .. }
                | Cmd::SetModel { reply, .. }
                | Cmd::Takeover { reply, .. } => {
                    let _ = reply.send(not_here());
                }
                Cmd::Hook(_) => {}
                Cmd::Streams { reply } => {
                    let _ = reply.send(HashMap::new());
                }
                // Closing from FEMME ends the pane too: a close is a kill, not a hide. A pane
                // that is already gone (the sync noticed it) makes this a no-op error.
                Cmd::Close { done } => {
                    let _ = herdr(&bin, Some(&hs), &["pane", "close", &pane]).await;
                    let _ = done.send(());
                    return;
                }
            }
        }
    });
    Head { tx }
}

/// Runs `herdr [--session <hs>] <args>`; its stdout, or why it failed.
async fn herdr(bin: &str, hs: Option<&str>, args: &[&str]) -> Result<String, CoreError> {
    let mut cmd = tokio::process::Command::new(bin);
    if let Some(hs) = hs {
        cmd.args(["--session", hs]);
    }
    let o = cmd
        .args(args)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| CoreError::Internal(format!("herdr: {e}")))?;
    if o.status.success() {
        return Ok(String::from_utf8_lossy(&o.stdout).into_owned());
    }
    let why = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    Err(if why.contains("agent_blocked") {
        CoreError::Busy
    } else {
        CoreError::Internal(format!("herdr: {}", why.trim()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::state::{ItemKind, ItemStatus};
    use femme_store::items as store_items;

    use crate::config::Config;

    const PANES: &str = r#"{"id":"cli:pane:list","result":{"type":"pane_list","panes":[
        {"agent":"claude","agent_session":{"agent":"claude","kind":"id","value":"u-1"},
         "agent_status":"working","cwd":"/w","pane_id":"wA:p1","terminal_title_stripped":"fix it"},
        {"agent":null,"agent_status":"unknown","cwd":"/w","pane_id":"wA:p2"},
        {"agent":"claude","agent_status":"idle","cwd":"/w","pane_id":"wA:p3"}]}}"#;

    #[test]
    fn only_claude_panes_with_a_conversation_are_mirrored() {
        let panes = parse_panes("default", PANES);
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].owner(), "herdr:default:wA:p1");
        assert_eq!(panes[0].uuid, "u-1");
        assert_eq!(panes[0].title, "fix it");
        assert!(parse_panes("x", "not json").is_empty());
    }

    #[test]
    fn herdrs_status_is_the_sessions() {
        assert_eq!(
            status_of("working"),
            Some((SessionStatus::Working, Attention::None))
        );
        assert_eq!(
            status_of("blocked"),
            Some((SessionStatus::Blocked, Attention::Blocked))
        );
        assert_eq!(
            status_of("done"),
            Some((SessionStatus::Idle, Attention::Done))
        );
        assert_eq!(
            status_of("idle"),
            Some((SessionStatus::Idle, Attention::None))
        );
        assert_eq!(status_of("unknown"), None);
    }

    fn line(v: Value) -> Option<Value> {
        disk_line(v.to_string().as_bytes(), Path::new("/nonexistent"))
            .map(|b| serde_json::from_slice(&b).unwrap())
    }

    #[test]
    fn the_file_reads_as_the_stream() {
        let typed = line(serde_json::json!({"type":"user","message":{"content":"hi"}})).unwrap();
        assert_eq!(typed["isReplay"], true);
        let result = serde_json::json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"t"}]}});
        assert_eq!(line(result).unwrap()["isReplay"], Value::Null);
        let stop = serde_json::json!({"type":"user","message":{"content":[
            {"type":"text","text":"[Request interrupted by user]"}]}});
        assert_eq!(line(stop).unwrap()["isReplay"], Value::Null);
        let end =
            line(serde_json::json!({"type":"system","subtype":"turn_duration","durationMs":5}));
        assert_eq!(end.unwrap()["type"], "result");
        let note = line(serde_json::json!({"type":"user","uuid":"u1","message":{"content":
            "<task-notification>\n<tool-use-id>toolu_1</tool-use-id>\n<status>completed</status>\n<summary>Build done</summary>\n</task-notification>"}}))
        .unwrap();
        assert_eq!(
            (
                &note["subtype"],
                &note["tool_use_id"],
                &note["status"],
                &note["summary"],
                &note["uuid"]
            ),
            (
                &Value::from("task_notification"),
                &Value::from("toolu_1"),
                &Value::from("completed"),
                &Value::from("Build done"),
                &Value::from("u1")
            ),
            "the stream's shape, so the driver ends the call's task"
        );
        let mode =
            line(serde_json::json!({"type":"permission-mode","permissionMode":"plan"})).unwrap();
        assert_eq!(
            (&mode["subtype"], &mode["permissionMode"]),
            (&Value::from("status"), &Value::from("plan"))
        );
        for skipped in [
            serde_json::json!({"type":"user","isSidechain":true,"message":{"content":"x"}}),
            serde_json::json!({"type":"user","isMeta":true,"message":{"content":"x"}}),
            serde_json::json!({"type":"attachment"}),
            serde_json::json!({"type":"file-history-snapshot"}),
        ] {
            assert_eq!(line(skipped), None);
        }
    }

    #[test]
    fn a_pasted_picture_is_saved_and_attached() {
        let dir = tempfile::tempdir().unwrap();
        let png = b"\x89PNG\r\n\x1a\nfake";
        let data = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(png)
        };
        let line = serde_json::json!({"type":"user","message":{"content":[
            {"type":"text","text":"[Image #177] look at this"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":data}},
        ]}})
        .to_string();
        let read = |l: &str| -> Value {
            serde_json::from_slice(&disk_line(l.as_bytes(), dir.path()).unwrap()).unwrap()
        };
        let text = read(&line)["message"]["content"]
            .as_str()
            .unwrap()
            .to_owned();
        let files: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(files.len(), 1);
        let path = files[0].as_ref().unwrap().path();
        assert_eq!(std::fs::read(&path).unwrap(), png);
        assert_eq!(
            text,
            format!("look at this\n\nAttached: {}", path.display())
        );
        assert_eq!(
            read(&line)["message"]["content"].as_str().unwrap(),
            text,
            "read again: the same file"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        assert_eq!(
            strip_image_marks("a [Image #] b [Image #2]c"),
            "a [Image #] b c"
        );
    }

    #[test]
    fn a_panes_conversation_is_claudes_own_word() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .env("HERDR_SOCKET_PATH", "/s.sock")
            .env("HERDR_PANE_ID", "wA:p1")
            .spawn()
            .unwrap();
        let sessions = dir.path().join(".claude/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let file = |pid: u32, kind: &str, conv: &str| {
            let v = serde_json::json!({"pid": pid, "sessionId": conv, "kind": kind});
            std::fs::write(sessions.join(format!("{pid}-{kind}.json")), v.to_string()).unwrap();
        };
        file(child.id(), "interactive", "c-1");
        file(child.id(), "print", "c-2"); // a `claude -p` in the pane is not its conversation
        file(u32::MAX, "interactive", "c-3"); // no such process
        let got = conversations(dir.path());
        child.kill().unwrap();
        let _ = child.wait();
        assert_eq!(got.len(), 1);
        assert_eq!(got[&("/s.sock".to_owned(), "wA:p1".to_owned())], "c-1");
    }

    #[test]
    fn a_subagents_file_reads_as_its_stream() {
        let sub = |v: Value| {
            sub_line(v.to_string().as_bytes(), "toolu_A")
                .map(|b| serde_json::from_slice::<Value>(&b).unwrap())
        };
        let prompt =
            sub(serde_json::json!({"type":"user","isSidechain":true,"message":{"content":"ls"}}))
                .unwrap();
        assert_eq!(prompt["parent_tool_use_id"], "toolu_A");
        assert_eq!(prompt["isReplay"], true);
        let said = serde_json::json!({"type":"assistant","isSidechain":true,
            "message":{"id":"s1","content":[{"type":"text","text":"x"}]}});
        assert_eq!(sub(said).unwrap()["parent_tool_use_id"], "toolu_A");
        for skipped in [
            serde_json::json!({"type":"attachment","isSidechain":true}),
            serde_json::json!({"type":"api-request","agentId":"a"}),
            serde_json::json!({"type":"system","subtype":"turn_duration","durationMs":5}),
            serde_json::json!({"type":"user","isSidechain":true,"isMeta":true,"message":{"content":"x"}}),
        ] {
            assert_eq!(sub(skipped), None);
        }
    }

    /// A temp home with a store and one transcript file.
    fn daemon() -> (std::sync::Arc<Daemon>, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix("femmed-h-")
            .tempdir_in("/tmp")
            .unwrap();
        let mut cfg = Config::from_vars([("HOME".to_string(), dir.path().display().to_string())]);
        cfg.state_dir = dir.path().join("state");
        cfg.runtime_dir = dir.path().join("rt");
        (Daemon::open(cfg).unwrap(), dir)
    }

    const TURN: &str = r#"{"type":"user","uuid":"m1","message":{"role":"user","content":"list it"}}
{"type":"assistant","message":{"id":"a1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}
{"type":"user","uuid":"r1","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"x.rs"}]}}
{"type":"attachment","attachment":{}}
{"type":"system","subtype":"compact_boundary","uuid":"c1","compactMetadata":{"trigger":"auto","preTokens":310172}}
{"type":"assistant","message":{"id":"a2","model":"claude-fable-5-1","content":[{"type":"text","text":"one file"}],"usage":{"input_tokens":2,"cache_read_input_tokens":40000,"cache_creation_input_tokens":500,"output_tokens":6}}}
{"type":"system","subtype":"turn_duration","durationMs":12}
"#;

    #[tokio::test]
    async fn a_pane_becomes_a_session_with_its_transcript_and_goes_when_the_pane_does() {
        let (d, dir) = daemon();
        let proj = dir.path().join(".claude/projects/-w");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("u-1.jsonl"), TURN).unwrap();
        let panes = parse_panes("default", PANES);
        let mut tails = HashMap::new();
        sync(&d, "/bin/false", &panes, &mut tails).await;
        let s = d.store.read(|c| sessions::list_sessions(c, false)).unwrap();
        assert_eq!(s.len(), 1);
        let sid = s[0].id;
        assert_eq!(
            (s[0].mode, s[0].status),
            (Mode::Headless, SessionStatus::Working)
        );
        assert!(d.sessions.head(sid).is_some());
        for t in tails.values_mut() {
            t.tail(&d).await;
        }
        let items = d
            .store
            .read(|c| store_items::page_items(c, sid, None, 100))
            .unwrap();
        let items: Vec<_> = items.into_iter().rev().collect();
        let kinds: Vec<_> = items.iter().map(|i| (i.kind, i.status)).collect();
        assert_eq!(
            kinds,
            [
                (ItemKind::UserMessage, ItemStatus::Completed),
                (ItemKind::ToolCall, ItemStatus::Completed),
                (ItemKind::ToolResult, ItemStatus::Completed),
                (ItemKind::Event, ItemStatus::Completed),
                (ItemKind::AssistantMessage, ItemStatus::Completed),
                (ItemKind::TurnEnd, ItemStatus::Completed),
            ]
        );
        // the row says what the last call read, the model's window, and the compaction
        let s = d.store.read(|c| sessions::list_sessions(c, false)).unwrap();
        assert_eq!(
            s[0].context,
            Some(femme_proto::state::Context {
                used: 40_502,
                window: 1_000_000,
                compactions: 1
            })
        );
        // read again: nothing new, nothing written twice
        for t in tails.values_mut() {
            t.tail(&d).await;
        }
        let again = d
            .store
            .read(|c| store_items::page_items(c, sid, None, 100))
            .unwrap();
        assert_eq!(again.len(), items.len());

        // a prompt through the head runs herdr, and its failure is said
        let r = d.sessions.head(sid).unwrap().prompt("go").await;
        assert!(matches!(r, Err(CoreError::Internal(_))), "{r:?}");

        // closed in FEMME: not brought back
        d.end_session(sid).await.unwrap();
        sync(&d, "/bin/false", &panes, &mut tails).await;
        assert!(d
            .store
            .read(|c| sessions::list_sessions(c, false))
            .unwrap()
            .is_empty());
        assert!(tails.is_empty());

        // a new conversation in that pane is a new session; the pane gone, it closes
        let mut next = panes.clone();
        next[0].uuid = "u-2".into();
        sync(&d, "/bin/false", &next, &mut tails).await;
        assert_eq!(
            d.store
                .read(|c| sessions::list_sessions(c, false))
                .unwrap()
                .len(),
            1
        );
        sync(&d, "/bin/false", &[], &mut tails).await;
        assert!(d
            .store
            .read(|c| sessions::list_sessions(c, false))
            .unwrap()
            .is_empty());
    }

    const AGENT_TURN: &str = r#"{"type":"user","uuid":"m1","message":{"role":"user","content":"look"}}
{"type":"assistant","message":{"id":"a1","content":[{"type":"tool_use","id":"toolu_A","name":"Agent","input":{"prompt":"ls"}}]}}
"#;

    const SUB: &str = r#"{"type":"user","isSidechain":true,"uuid":"s0","message":{"role":"user","content":"ls"}}
{"type":"attachment","isSidechain":true,"attachment":{}}
{"type":"api-request","agentId":"x"}
{"type":"assistant","isSidechain":true,"message":{"id":"sm1","content":[{"type":"tool_use","id":"toolu_B","name":"Bash","input":{"command":"ls"}}]}}
"#;

    #[tokio::test]
    async fn a_subagents_transcript_is_tailed_under_its_agent_call() {
        let (d, dir) = daemon();
        let proj = dir.path().join(".claude/projects/-w");
        let subs = proj.join("u-1/subagents");
        std::fs::create_dir_all(&subs).unwrap();
        std::fs::write(proj.join("u-1.jsonl"), AGENT_TURN).unwrap();
        // no meta yet: not read
        std::fs::write(subs.join("agent-x1.jsonl"), SUB).unwrap();
        let panes = parse_panes("default", PANES);
        let mut tails = HashMap::new();
        sync(&d, "/bin/false", &panes, &mut tails).await;
        let sid = *tails.keys().next().unwrap();
        let tick = |tails: &mut HashMap<Sid, Tail>| {
            let d = d.clone();
            let tails = std::mem::take(tails);
            async move {
                let mut tails = tails;
                for t in tails.values_mut() {
                    t.tail(&d).await;
                }
                tails
            }
        };
        let items = |d: &Daemon| {
            let mut v = d
                .store
                .read(|c| store_items::page_items(c, sid, None, 100))
                .unwrap();
            v.reverse();
            v.into_iter()
                .map(|i| (i.item_id, i.kind, i.status, i.parent))
                .collect::<Vec<_>>()
        };
        tails = tick(&mut tails).await;
        assert!(items(&d).iter().all(|i| i.3.is_none()));
        assert_eq!(items(&d).len(), 2);

        // the meta names the call: the file is read, its items nest under it
        std::fs::write(
            subs.join("agent-x1.meta.json"),
            r#"{"agentType":"Explore","description":"look","toolUseId":"toolu_A","finished":false}"#,
        )
        .unwrap();
        tails = tick(&mut tails).await;
        let a = Some("toolu_A".to_owned());
        assert_eq!(
            items(&d),
            [
                (
                    "m1".into(),
                    ItemKind::UserMessage,
                    ItemStatus::Completed,
                    None
                ),
                (
                    "toolu_A".into(),
                    ItemKind::ToolCall,
                    ItemStatus::InProgress,
                    None
                ),
                (
                    "s0".into(),
                    ItemKind::UserMessage,
                    ItemStatus::Completed,
                    a.clone()
                ),
                (
                    "toolu_B".into(),
                    ItemKind::ToolCall,
                    ItemStatus::InProgress,
                    a.clone()
                ),
            ]
        );

        // it goes on mid-turn, then the call returns and the turn ends; nothing is read twice
        let append = |p: PathBuf, s: &str| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .append(true)
                .open(p)
                .unwrap()
                .write_all(s.as_bytes())
                .unwrap();
        };
        append(
            subs.join("agent-x1.jsonl"),
            concat!(
                r#"{"type":"user","isSidechain":true,"uuid":"s2","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_B","content":"x.rs"}]}}"#,
                "\n",
                r#"{"type":"assistant","isSidechain":true,"message":{"id":"sm2","content":[{"type":"text","text":"x.rs"}]}}"#,
                "\n"
            ),
        );
        append(
            proj.join("u-1.jsonl"),
            concat!(
                r#"{"type":"user","uuid":"r1","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_A","content":"x.rs"}]}}"#,
                "\n",
                r#"{"type":"system","subtype":"turn_duration","durationMs":12}"#,
                "\n"
            ),
        );
        tails = tick(&mut tails).await;
        tails = tick(&mut tails).await;
        let got = items(&d);
        let sub: Vec<_> = got
            .iter()
            .filter(|i| i.3.is_some())
            .map(|i| (i.0.as_str(), i.1, i.2))
            .collect();
        assert_eq!(
            sub,
            [
                ("s0", ItemKind::UserMessage, ItemStatus::Completed),
                ("toolu_B", ItemKind::ToolCall, ItemStatus::Completed),
                (
                    "toolu_B:result",
                    ItemKind::ToolResult,
                    ItemStatus::Completed
                ),
                ("sm2:0", ItemKind::AssistantMessage, ItemStatus::Completed),
            ]
        );
        let main: Vec<_> = got
            .iter()
            .filter(|i| i.3.is_none())
            .map(|i| (i.0.as_str(), i.1, i.2))
            .collect();
        assert_eq!(main.len(), 4, "{main:?}");
        assert_eq!(
            main[1],
            ("toolu_A", ItemKind::ToolCall, ItemStatus::Completed)
        );
        assert_eq!(main[3].1, ItemKind::TurnEnd);
        assert_eq!(
            tails[&sid].subs.values().next().unwrap().read,
            std::fs::metadata(subs.join("agent-x1.jsonl"))
                .unwrap()
                .len()
        );
    }
}
