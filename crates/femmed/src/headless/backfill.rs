//! Claude Code's own transcript on disk (`~/.claude/projects/*/<uuid>.jsonl`), read into
//! transcript items through the Claude driver's disk mode: what a conversation did while no
//! stream of FEMME's was watching it. [`Reader`] tails one conversation from an offset on (the
//! herdr mirror keeps one per pane; [`run`] reads once, for a session back from its TUI).
//!
//! Subagents (Claude's `Agent` tool) write their own transcripts beside it,
//! `<uuid>/subagents/agent-<id>.jsonl` with `agent-<id>.meta.json` naming the spawning tool call
//! (`toolUseId`); those are read too, their lines marked with that call as `parent_tool_use_id`
//! the way the stream marks a subagent's, so their items nest under the call.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use femme_agents::items::{body, UserMessage};
use femme_agents::{Driver, Out};
use femme_proto::state::{Goal, GoalState, ItemKind, ItemStatus};
use femme_proto::Sid;
use serde_json::Value;

use super::{fold, forward_pushes, push_title, put, stamp_pushes, FoldState, Put};
use crate::daemon::{now_ms, Daemon};
use crate::holder_client::SpoolLines;

/// A conversation seen for the first time shows this much of its end.
// ponytail: the last 4 MiB, transcripts grow past 30 MB; page further back when someone misses it.
pub(super) const BACKFILL: u64 = 4 << 20;
/// The most read from one transcript per tick.
pub(super) const READ_MAX: u64 = 8 << 20;

// ---- the transcript -----------------------------------------------------------------------------

/// One conversation's transcript, read on from where it was read.
pub(super) struct Reader {
    pub(super) sid: Sid,
    pub(super) uuid: String,
    pub(super) path: Option<PathBuf>,
    pub(super) lines: SpoolLines,
    /// The file is read up to here.
    pub(super) read: u64,
    /// Where the store has it (`spool_committed`).
    pub(super) committed: u64,
    /// Started inside a line (the backfill cut): the first one is a piece.
    pub(super) skip: bool,
    pub(super) driver: Box<dyn Driver>,
    pub(super) fold: FoldState,
    pub(super) model: Option<String>,
    pub(super) permission: Option<String>,
    /// The `/goal` as the lines read so far last said it, for the row.
    pub(super) goal: Option<Goal>,
    /// A prompt was typed since the last row write: an achieved goal leaves the row.
    pub(super) prompted: bool,
    /// What the lines read so far said of the context, for the row: what the last call read,
    /// the window, and the compactions since the last row write.
    pub(super) ctx_used: Option<u64>,
    pub(super) ctx_window: Option<u64>,
    pub(super) compacted: u32,
    pub(super) pending: Vec<Put>,
    /// The subagent transcripts beside it, by path.
    // ponytail: their offsets live only here. Reading them all again at every start was 1.5 GB
    // and got the daemon watchdog-killed in a loop, so a session the store already holds reads
    // the ones there at start from their end: what a subagent wrote while the daemon was down is
    // not shown. Persist the offsets if that gap ever matters.
    pub(super) subs: HashMap<PathBuf, SubTail>,
    /// Whether the subagent transcripts there at start are read from their end (the store holds
    /// the session already) on the next look; false after the first.
    pub(super) subs_from_end: bool,
    /// Lines of kinds this reader does not know, by kind: each kind is one note, its count kept.
    // ponytail: counted from where this run started reading, so a restart counts again from there
    pub(super) unknown: HashMap<String, u32>,
    /// Lines from here on were written while this reader followed the file: what Claude asks to
    /// push in them goes to the phone. Before it is the backfill, or what a daemon that was down
    /// missed, and old news.
    pub(super) live_from: u64,
    /// Claude's `PushNotification` calls sent to the phone, by call id: the outcome.
    pub(super) pushes: HashMap<String, String>,
    /// Whether the offset read up to is the session's `spool_committed` (the mirror's is;
    /// a session with a stream of its own keeps that column for the stream).
    pub(super) store_offset: bool,
    /// Only subagent transcripts changed since then are read (a one-shot read after a TUI phase:
    /// the older ones were read from the stream).
    pub(super) subs_since: Option<SystemTime>,
}

/// What Claude Code keeps free below its auto-compact window (2.1.321: a window of 534,643
/// compacts at 501,643).
const COMPACT_BUFFER: u64 = 33_000;
/// The smallest auto-compact window Claude Code takes (2.1.321): a smaller one in a settings file
/// is ignored, a smaller `CLAUDE_CODE_AUTO_COMPACT_WINDOW` counts as this.
const COMPACT_WINDOW_MIN: u64 = 100_000;

/// The settings files Claude Code reads in `cwd`, the one that wins first; a missing or broken
/// file is skipped.
pub(super) fn claude_settings(home: &Path, cwd: &Path) -> Vec<Value> {
    [
        cwd.join(".claude/settings.local.json"),
        cwd.join(".claude/settings.json"),
        home.join(".claude/settings.json"),
    ]
    .iter()
    .filter_map(|p| serde_json::from_slice(&std::fs::read(p).ok()?).ok())
    .collect()
}

/// Where a Claude pane compacts by itself, in tokens, as its settings say (`files`, the one that
/// wins first): `CLAUDE_CODE_AUTO_COMPACT_WINDOW` (`env`), then the model's
/// `modelSettings.<model>.autoCompactWindow`, then `autoCompactWindow`, capped at the model's
/// `window` (0: unknown), minus the buffer. `None` when nothing sets it or auto-compaction is off.
// ponytail: with nothing set the CLI compacts at the window minus the buffer; the bar then fills
// against the window as before, 3% off on a 1M model
pub(super) fn compact_at(
    env: Option<&str>,
    files: &[Value],
    model: Option<&str>,
    window: u64,
) -> Option<u64> {
    if files.iter().find_map(|f| f["autoCompactEnabled"].as_bool()) == Some(false) {
        return None;
    }
    // the row's model may carry the size suffix (`opus[1m]`), the settings key may not
    let names = model.map(|m| [m, m.split('[').next().unwrap_or(m)]);
    let valid = |v: &Value| v.as_u64().filter(|w| *w >= COMPACT_WINDOW_MIN);
    let of_model = |f: &Value| {
        names?
            .iter()
            .find_map(|m| valid(&f["modelSettings"][m]["autoCompactWindow"]))
    };
    let set = env
        .and_then(|e| e.trim().parse::<u64>().ok())
        .map(|w| w.max(COMPACT_WINDOW_MIN))
        .or_else(|| files.iter().find_map(of_model))
        .or_else(|| files.iter().find_map(|f| valid(&f["autoCompactWindow"])))?;
    let capped = if window > 0 { set.min(window) } else { set };
    Some(capped.saturating_sub(COMPACT_BUFFER)).filter(|n| *n > 0)
}

/// A subagent's transcript, read on from where it was read.
pub(super) struct SubTail {
    /// The `Agent` tool call that spawned it.
    pub(super) parent: String,
    pub(super) lines: SpoolLines,
    /// The file is read up to here.
    pub(super) read: u64,
}

/// What a tick read from a subagent's transcript: its path, its parent when it is new, where the
/// data starts and the data (`None`: shorter than read, rewritten).
type SubRead = (PathBuf, Option<String>, u64, Option<Vec<u8>>);

impl Reader {
    pub(super) fn new(sid: Sid, uuid: String, committed: u64) -> Reader {
        Reader {
            sid,
            uuid,
            path: None,
            lines: SpoolLines::new(committed),
            read: committed,
            committed,
            skip: false,
            driver: Box::new(femme_agents::claude::ClaudeDriver::mirror()),
            fold: FoldState::default(),
            model: None,
            permission: None,
            goal: None,
            prompted: false,
            ctx_used: None,
            ctx_window: None,
            compacted: 0,
            pending: Vec::new(),
            subs: HashMap::new(),
            subs_from_end: committed > 0,
            unknown: HashMap::new(),
            live_from: u64::MAX,
            pushes: HashMap::new(),
            store_offset: true,
            subs_since: None,
        }
    }

    /// The note for one more line of a `kind` this reader does not know: the kind only, never
    /// what the line says (it may be context meant for the model).
    fn unknown(&mut self, kind: String) -> Put {
        let n = self.unknown.entry(kind.clone()).or_default();
        *n += 1;
        if *n == 1 {
            tracing::warn!(sid = %self.sid, %kind, "transcript line of a kind FEMME does not read");
        }
        let text = if *n == 1 {
            format!("Claude Code wrote an entry FEMME does not read yet: {kind}")
        } else {
            format!("Claude Code wrote {n} entries FEMME does not read yet: {kind}")
        };
        Put::Done(super::event_item(
            self.sid,
            format!("unknown:{kind}"),
            &text,
            now_ms(),
        ))
    }

    /// Reads what the file (and its subagents') gained and stores it; `on_line` sees every line
    /// fed to the driver.
    pub(super) async fn tail(&mut self, d: &Daemon, on_line: &mut (dyn FnMut(&[u8]) + Send)) {
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
            self.live_from = len;
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
                *self = Reader::new(self.sid, self.uuid.clone(), 0);
                return;
            }
            _ => return,
        };
        let known: Vec<(PathBuf, u64)> =
            self.subs.iter().map(|(p, t)| (p.clone(), t.read)).collect();
        let from_end = std::mem::take(&mut self.subs_from_end);
        let since = self.subs_since;
        let subs = tokio::task::spawn_blocking(move || read_subs(&dir, &known, from_end, since))
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
            let (line, typed) = match disk_line(&line, &uploads) {
                None => continue,
                Some(Disk::Unknown(kind)) => {
                    puts.push(self.unknown(kind));
                    continue;
                }
                Some(Disk::Feed { line, typed }) => (line, typed),
            };
            on_line(&line);
            let mut events = self.driver.on_line(at, &line, &mut out);
            out.stdin.clear(); // nobody listens: the pane's Claude reads its own terminal
            if at >= self.live_from {
                let sid = self.sid;
                let title = || {
                    d.session(sid)
                        .map_or_else(|_| String::new(), |s| push_title(&s))
                };
                forward_pushes(&d.notifier, sid, title, &events, &mut self.pushes, true).await;
            }
            stamp_pushes(&mut events, &self.pushes);
            let f = fold(
                &mut self.fold,
                self.sid,
                &format!("d{at}"),
                events,
                now_ms(),
            );
            if let Some(missed) = typed.and_then(|t| missed(self.sid, &f.puts, t)) {
                puts.push(missed);
            }
            puts.extend(f.puts);
            if f.model.is_some() {
                self.model = f.model;
            }
            if f.permission.is_some() {
                self.permission = f.permission;
            }
            if f.goal.is_some() {
                self.goal = f.goal;
            }
            self.prompted |= f.prompted;
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
        let removed = super::removes(d, &puts);
        let store_offset = self.store_offset;
        let r = d.write_items(|tx, _, published| {
            for p in &puts {
                put(tx, sid, p, published)?;
            }
            if store_offset {
                tx.set_spool_committed(sid, end)?;
            }
            Ok(())
        });
        if let Some(before) = removed {
            d.say_removed(sid, before);
        }
        match r {
            Ok(()) => self.committed = end,
            Err(e) => {
                tracing::warn!(%sid, %e, "transcript not stored, kept for later");
                self.pending = puts;
            }
        }
        if let Some(m) = self.model.take() {
            let _ = d.update(sid, |s| s.model = Some(m));
        }
        if let Some(p) = self.permission.take() {
            let _ = d.update(sid, |s| s.permission = Some(p));
        }
        let (goal, prompted) = (self.goal.take(), std::mem::take(&mut self.prompted));
        if goal.is_some() || prompted {
            let _ = d.update(sid, |s| {
                if let Some(g) = goal {
                    s.goal = Some(g);
                }
                // an achieved goal stays on show until the next prompt
                if prompted && s.goal.as_ref().is_some_and(|g| g.state == GoalState::Met) {
                    s.goal = None;
                }
            });
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
            // a pane has no control channel to ask: the point is what its settings say, read
            // again at every write (`/autocompact` rewrites them)
            let files = d
                .session(sid)
                .map(|s| claude_settings(&d.cfg.home, Path::new(&s.cwd)))
                .unwrap_or_default();
            let env = std::env::var("CLAUDE_CODE_AUTO_COMPACT_WINDOW").ok();
            let _ = d.update(sid, |s| {
                s.context = super::with_context(s.context, used, window, None, compacted);
                if let Some(c) = &mut s.context {
                    c.compact_at = compact_at(env.as_deref(), &files, s.model.as_deref(), c.window);
                }
            });
        }
    }
}

impl Reader {
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
                lines: SpoolLines::new(at),
                read: at,
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

/// Reads a conversation once, from `from` on (`None`: the last [`BACKFILL`] of it) and its
/// subagents changed since `since`, into the session's items: what Claude wrote while the session
/// was in its TUI. Nothing of the offset is stored: the next read is from where the next TUI
/// phase starts.
pub(super) async fn run(
    d: &Daemon,
    sid: Sid,
    uuid: &str,
    from: Option<u64>,
    since: Option<SystemTime>,
) {
    let Some(path) = find(&d.cfg.home, uuid) else {
        return;
    };
    let len = std::fs::metadata(&path).map_or(0, |m| m.len());
    let (from, cut) = match from {
        Some(at) if at <= len => (at, false),
        _ => (len.saturating_sub(BACKFILL), len > BACKFILL),
    };
    let mut r = Reader::new(sid, uuid.to_owned(), from);
    r.store_offset = false;
    r.subs_from_end = false;
    r.subs_since = since;
    r.skip = cut;
    r.path = Some(path);
    r.live_from = len;
    loop {
        let before = r.read;
        r.tail(d, &mut |_| {}).await;
        if r.read == before {
            break;
        }
    }
    if !r.pending.is_empty() {
        tracing::warn!(%sid, n = r.pending.len(), "the TUI's turns were not all stored");
    }
}

/// A message the user sent (`typed`: its id and text) that the driver's writes `puts` do not
/// hold as their message: stored as one anyway, so a reader slip never loses it.
pub(super) fn missed(sid: Sid, puts: &[Put], (id, text): (String, String)) -> Option<Put> {
    let held = puts.iter().any(|p| match p {
        Put::Start(i) | Put::Done(i) => i.item_id == id && i.kind == ItemKind::UserMessage,
        Put::Update(..) | Put::Remove(_) => false,
    });
    if held {
        return None;
    }
    tracing::warn!(%sid, item = %id, "a typed message the driver did not keep: stored as it is");
    let b = body(&UserMessage { text });
    Some(Put::Done(super::new_item(
        sid,
        id,
        ItemKind::UserMessage,
        ItemStatus::Completed,
        Some(&b),
        now_ms(),
    )))
}

/// Reads on in every subagent transcript of `dir`: the `known` ones from where they were read,
/// a new one from the start once its `.meta.json` names the tool call that spawned it, or from
/// its end with `from_end` (the ones there when the daemon starts on a session it holds), and
/// not at all when it was last written before `since`.
pub(super) fn read_subs(
    dir: &Path,
    known: &[(PathBuf, u64)],
    from_end: bool,
    since: Option<SystemTime>,
) -> Vec<SubRead> {
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
                let old = since.is_some_and(|t| {
                    std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .is_ok_and(|m| m < t)
                });
                if old {
                    continue;
                }
                let meta = std::fs::read(dir.join(format!("{stem}.meta.json")))
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
                let Some(id) = meta.as_ref().and_then(|m| m["toolUseId"].as_str()) else {
                    continue; // not said yet: next tick
                };
                let at = if from_end {
                    std::fs::metadata(&path).map_or(0, |m| m.len())
                } else {
                    0
                };
                (Some(id.to_owned()), at)
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
pub(super) fn find(home: &Path, uuid: &str) -> Option<PathBuf> {
    let name = format!("{uuid}.jsonl");
    std::fs::read_dir(home.join(".claude/projects"))
        .ok()?
        .flatten()
        .map(|e| e.path().join(&name))
        .find(|p| p.is_file())
}

/// What the file holds from `at` on (at most [`READ_MAX`]); `None` when it is shorter than `at`.
pub(super) fn read_from(path: &Path, at: u64) -> std::io::Result<Option<Vec<u8>>> {
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

/// What a line of Claude's transcript file is to the reader.
pub(super) enum Disk {
    /// A line for the driver, in the stream's shape. `typed` is the id and text of a message the
    /// user sent, which has to come out of the driver as their message.
    Feed {
        line: Vec<u8>,
        typed: Option<(String, String)>,
    },
    /// A kind of line this reader does not know yet: said, never silently dropped.
    Unknown(String),
}

/// The file's own bookkeeping: nothing in it is the conversation.
const BOOKKEEPING: &[&str] = &[
    "agent-name",
    "ai-title",
    "api-request",
    "api-request-blob",
    "api-request-shape",
    "artifact-autoreact-ledger",
    "artifact-comment-monitor",
    "atis-latch",
    "cost-state",
    "custom-title",
    "file-history-delta",
    "file-history-snapshot",
    "frame-link",
    "last-prompt",
    "mode",
    "pr-link",
    "progress",
    // the queue's own log: a message taken in mid-turn is its `queued_command` attachment, one
    // run between turns its `user` line
    "queue-operation",
    "session-owner",
    "summary",
    "tools-used",
];

/// What Claude Code attaches to the model's context (reminders, listings, hook output): not the
/// conversation. A `queued_command` is, and is read.
const CONTEXT: &[&str] = &[
    "agent_listing_delta",
    "auto_mode",
    "auto_mode_exit",
    "bash_output_audience_note",
    "batching_reminder_sent",
    "command_permissions",
    "compact_file_reference",
    "credential_org",
    "date",
    "deferred_tools_delta",
    "deferred_tools_record",
    "edited_text_file",
    "environment",
    "file",
    "hook_cancelled",
    "hook_non_blocking_error",
    "hook_success",
    "instructions",
    "invoked_skills",
    "mcp_instructions_delta",
    "model",
    "plan_file_reference",
    "plan_mode",
    "plan_mode_exit",
    "plan_mode_reentry",
    "prompt_snapshot",
    "read_truncation_notice",
    "remote_session_change",
    "session_context",
    "silent_turn_reminder",
    "skill_listing",
    "task_status",
    "thinking_drop",
    "thinking_stripped",
    "total_tokens_reminder",
];

/// A line of Claude's transcript file as the driver reads it from `--output-format stream-json`,
/// or `None` for the file's own bookkeeping. A typed prompt becomes the replay the stream would
/// echo (`isReplay`), and a turn's `turn_duration` the `result` that ends it. A pasted picture is
/// saved in `uploads` and named by an `Attached:` line, as a desktop attachment is.
pub(super) fn disk_line(line: &[u8], uploads: &Path) -> Option<Disk> {
    let mut v: Value = serde_json::from_slice(line).ok()?;
    if v["isSidechain"] == true {
        return None;
    }
    let ty = v["type"].as_str().unwrap_or_default().to_owned();
    if ty == "attachment" {
        let sub = v["attachment"]["type"].as_str().unwrap_or_default();
        // the goal checker's verdicts go to the driver as they are; a queued command becomes
        // the user line it stands for; every other attachment is context, not shown
        if sub == "queued_command" {
            v = queued(v)?;
        } else if sub != "goal_status" {
            return (!CONTEXT.contains(&sub)).then(|| Disk::Unknown(format!("attachment {sub}")));
        }
    } else if !matches!(
        ty.as_str(),
        "user" | "assistant" | "system" | "permission-mode"
    ) {
        return (!BOOKKEEPING.contains(&ty.as_str())).then_some(Disk::Unknown(ty));
    }
    if v["type"] == "user" {
        attach_images(&mut v, uploads);
    }
    let v = disk_value(v)?;
    let typed = (v["isReplay"] == true)
        .then(|| {
            let id = v["uuid"].as_str()?.to_owned();
            let text = match &v["message"]["content"] {
                Value::String(s) => s.clone(),
                c => c
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            Some((id, text))
        })
        .flatten();
    Some(Disk::Feed {
        line: serde_json::to_vec(&v).ok()?,
        typed,
    })
}

/// A message Claude took in while it worked (a `queued_command` attachment) as the `user` line
/// the TUI writes for one sent between turns. One the user did not type (another session's, an
/// auto-continuation) is marked so it is not their message, and a peer's keeps its `origin`; one
/// for the model only (`isMeta`, unless it is a peer's `<cross-session-message>`) is skipped.
pub(super) fn queued(v: Value) -> Option<Value> {
    let a = &v["attachment"];
    let peer = a["prompt"]
        .as_str()
        .is_some_and(|p| p.contains("<cross-session-message"));
    if a["isMeta"] == true && !peer {
        return None;
    }
    let mut u = serde_json::json!({
        "type": "user",
        "uuid": v["uuid"],
        "timestamp": v["timestamp"],
        "message": {"role": "user", "content": a["prompt"]},
    });
    if a["origin"]["kind"].as_str().is_some_and(|k| k != "human") {
        u["notTyped"] = Value::Bool(true);
    }
    // a peer's message keeps its origin, so the driver reads it as a `PeerMessage` from the sender
    if a["origin"]["kind"] == "peer" {
        u["origin"] = a["origin"].clone();
    } else if a["origin"].is_null() && peer {
        u["origin"] = serde_json::json!({"kind": "peer"});
        u["notTyped"] = Value::Bool(true);
    }
    Some(u)
}

/// A typed message's pictures (base64 `image` blocks) as files in `dir`, and its content as text
/// with an `Attached:` line per file in place of Claude's `[Image #N]` markers. A file is named by
/// its bytes' hash, so reading the transcript again finds it instead of writing another.
pub(super) fn attach_images(v: &mut Value, dir: &Path) {
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
pub(super) fn strip_image_marks(text: &str) -> String {
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
pub(super) fn sub_line(line: &[u8], parent: &str) -> Option<Vec<u8>> {
    let v: Value = serde_json::from_slice(line).ok()?;
    if !matches!(v["type"].as_str(), Some("assistant" | "user")) {
        return None;
    }
    let mut v = disk_value(v)?;
    v["parent_tool_use_id"] = Value::String(parent.to_owned());
    serde_json::to_vec(&v).ok()
}

/// [`disk_line`] on a parsed line, sidechain or not.
pub(super) fn disk_value(mut v: Value) -> Option<Value> {
    if v["isMeta"] == true && v["origin"]["kind"] != "peer" {
        return None;
    }
    // the summary a compaction restarts the model with: a `user` line the terminal never shows
    // (the `compact_boundary` before it is the "Conversation compacted" row), so not a message
    if v["isCompactSummary"] == true || v["isVisibleInTranscriptOnly"] == true {
        return None;
    }
    match v["type"].as_str()? {
        "assistant" => {}
        "user" => {
            // the file's record of a tool's outcome (an AskUserQuestion's answers), as the
            // stream names it
            if let Some(r) = v.get("toolUseResult").cloned() {
                v["tool_use_result"] = r;
            }
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
                // without a status it is a Monitor event, not an end: the driver draws its row
                if let Some(status) = tag("status") {
                    v = serde_json::json!({
                        "type": "system",
                        "subtype": "task_notification",
                        "tool_use_id": tag("tool-use-id"),
                        "status": status,
                        "summary": tag("summary").unwrap_or_else(|| "A background task reported".into()),
                        "uuid": v["uuid"],
                    });
                }
            } else if !result && !text.starts_with("[Request interrupted") && v["notTyped"] != true
            {
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
        // the goal checker's verdicts; the driver reads them, no other attachment is shown
        "attachment" if v["attachment"]["type"] == "goal_status" => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::session::MAIN;
    use femme_proto::state::{Agent, Attention, Mode, Pane, PaneKind, Session, SessionStatus};
    use femme_proto::{Id, PaneId};
    use femme_store::{items as store_items, sessions};

    const TURN: &str = r#"{"type":"user","uuid":"m1","message":{"role":"user","content":"list it"}}
{"type":"assistant","message":{"id":"a1","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}
{"type":"user","uuid":"r1","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"x.rs"}]}}
{"type":"system","subtype":"turn_duration","durationMs":12}
{"type":"user","uuid":"m2","message":{"role":"user","content":"and the tests"}}
{"type":"assistant","message":{"id":"a2","model":"claude-fable-5-1","content":[{"type":"text","text":"one file"}],"usage":{"input_tokens":2,"output_tokens":6}}}
{"type":"system","subtype":"turn_duration","durationMs":12}
"#;

    fn daemon() -> (std::sync::Arc<Daemon>, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix("femmed-b-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = ("HOME".to_string(), dir.path().display().to_string());
        let mut cfg = Config::from_vars([home]);
        cfg.state_dir = dir.path().join("state");
        cfg.runtime_dir = dir.path().join("rt");
        (Daemon::open(cfg).unwrap(), dir)
    }

    fn session(d: &Daemon, uuid: &str) -> Sid {
        d.write(|tx, now| {
            let s = tx.insert_session(&Session {
                id: Id(0),
                handle: String::new(),
                rev: 0,
                name: None,
                owner: "device:test".into(),
                agent: Agent::Claude,
                mode: Mode::Headless,
                cwd: "/w".into(),
                label: None,
                agent_sid: Some(uuid.into()),
                model: None,
                status: SessionStatus::Idle,
                attention: Attention::None,
                created: now,
                closed: None,
                permission: None,
                context: None,
                effort: None,
                goal: None,
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
            Ok(s.id)
        })
        .unwrap()
    }

    /// A session back from its TUI gets the turns the TUI ran, from where the file ended when the
    /// TUI started, and its stream offset is left alone.
    #[tokio::test]
    async fn the_tui_phase_is_read_from_where_it_started() {
        let (d, dir) = daemon();
        let proj = dir.path().join(".claude/projects/-w");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("u-1.jsonl"), TURN).unwrap();
        let sid = session(&d, "u-1");
        d.write(|tx, _| tx.set_spool_committed(sid, 77)).unwrap();
        // the first turn came over the stream; the TUI ran the second
        let at = TURN.match_indices('\n').nth(3).unwrap().0 as u64 + 1;
        run(&d, sid, "u-1", Some(at), None).await;
        let mut items = d
            .store
            .read(|c| store_items::page_items(c, sid, None, 100))
            .unwrap();
        items.reverse();
        let ids: Vec<&str> = items.iter().map(|i| i.item_id.as_str()).collect();
        assert!(ids.contains(&"m2") && ids.contains(&"a2:0"), "{ids:?}");
        assert!(!ids.contains(&"m1") && !ids.contains(&"t1"), "{ids:?}");
        assert_eq!(
            d.store.read(|c| sessions::spool_committed(c, sid)).unwrap(),
            77,
            "the stream's offset is not the file's"
        );
        // without the offset (the daemon restarted meanwhile): the end of the file, nothing twice
        run(&d, sid, "u-1", None, None).await;
        let again = d
            .store
            .read(|c| store_items::page_items(c, sid, None, 100))
            .unwrap();
        let ids: Vec<&str> = again.iter().map(|i| i.item_id.as_str()).collect();
        assert!(ids.contains(&"m1") && ids.contains(&"t1"), "{ids:?}");
        for id in ["m2", "a2:0"] {
            assert_eq!(
                ids.iter().filter(|i| **i == id).count(),
                1,
                "{id} once: {ids:?}"
            );
        }
    }
}
