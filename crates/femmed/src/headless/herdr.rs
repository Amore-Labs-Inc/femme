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
//!
//! A pane blocked on a prompt the TUI draws (a permission: "Do you want to proceed? 1. Yes 2. No")
//! shows it as an open question, read off the pane's screen (`herdr agent read`); its answer is the
//! option's number, or Escape, typed into the pane. While it waits, a prompt from FEMME is refused
//! with a line that says so.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use femme_agents::items::{body, QuestionItem, UserMessage};
use femme_agents::{Driver, Out};
use femme_proto::rpc::Answer;
use femme_proto::state::Item;
use femme_proto::state::{
    Agent, Attention, Goal, GoalState, ItemKind, ItemStatus, Mode, Pane, PaneKind, Question,
    Session, SessionStatus,
};
use femme_proto::{Id, PaneId, Sid};
use femme_store::{items as store_items, sessions, Store};
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
/// The request id prefix of a question read off the pane's screen: its answer is keys typed into
/// the pane, where a transcript's question (AskUserQuestion) is the pane's own to answer.
const SCREEN_ASK: &str = "herdr:";
/// A blocked pane whose screen shows no question is looked at again after this long.
const SCREEN_RETRY: Duration = Duration::from_secs(10);
/// What a prompt gets while the pane waits on its question.
const WAITING: &str =
    "Claude is waiting for an answer in its terminal: answer the question above, or in the terminal tab";

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
            // the pane went idle a while ago and the transcript is read: what it did not echo
            // it did not get
            if t.idle_since.is_some_and(|i| i.elapsed() > IDLE_SETTLE) {
                t.idle_since = None;
                d.idle_unread(t.sid);
            }
        }
    }
}

/// How long after a pane goes idle its unechoed messages count as not got: the transcript's last
/// lines are read by then.
const IDLE_SETTLE: Duration = Duration::from_secs(5);

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
async fn sync(d: &Arc<Daemon>, bin: &str, panes: &[HPane], tails: &mut HashMap<Sid, Tail>) {
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
            d.sessions.set_head(
                s.id,
                head(
                    bin.into(),
                    p.hs.clone(),
                    p.pane.clone(),
                    s.id,
                    Arc::downgrade(d),
                ),
            );
            e.insert(Tail::new(s.id, p.uuid.clone(), from));
        }
        let on_plan = tails.get(&s.id).is_some_and(|t| t.plan_open.is_some());
        let state = status_of(if on_plan { "blocked" } else { &p.status });
        let title = (!p.title.is_empty()).then(|| p.title.clone());
        if let (Some((now, _)), Some(t)) = (state, tails.get_mut(&s.id)) {
            match (s.status, now) {
                (SessionStatus::Working, SessionStatus::Idle) => {
                    t.idle_since = Some(std::time::Instant::now());
                }
                (_, SessionStatus::Working) => t.idle_since = None,
                _ => {}
            }
        }
        if let Some(t) = tails.get_mut(&s.id) {
            screen_asks(d, bin, p, t).await;
        }
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

/// Keeps the pane's screen question in step with its status: a pane blocked on a prompt the TUI
/// draws gets it as an open question, and one no longer blocked closes what is still open (the user
/// answered in the pane; an answer from FEMME closed it as it went). A transcript's own question
/// (an AskUserQuestion) open at the time is left alone: the pane's prompt is that one.
async fn screen_asks(d: &Daemon, bin: &str, p: &HPane, t: &mut Tail) {
    if p.status != "blocked" && t.plan_open.is_none() {
        // the first look after a start closes what a run before left open
        if t.asked.take().is_some() || !t.asks_checked {
            close_screen_asks(d, t.sid, "answered in the pane");
        }
        t.asks_checked = true;
        t.screen_tried = None; // the next block is a new prompt
        return;
    }
    t.asks_checked = true;
    if t.asked.is_some() || t.screen_tried.is_some_and(|at| at.elapsed() < SCREEN_RETRY) {
        return;
    }
    match d.pending_questions(t.sid) {
        Ok(open) if !open.is_empty() => {
            t.asked = open
                .iter()
                .map(|(_, q)| q.q.request_id.clone())
                .find(|id| id.starts_with(SCREEN_ASK));
            return;
        }
        Ok(_) => {}
        Err(_) => return,
    }
    t.screen_tried = Some(Instant::now());
    let Ok(screen) = herdr(bin, Some(&p.hs), &["agent", "read", &p.pane]).await else {
        return;
    };
    let Some(ScreenPrompt {
        prompt, options, ..
    }) = screen_prompt(&screen)
    else {
        return;
    };
    let now = now_ms();
    let id = format!("{SCREEN_ASK}{}:{now}", p.pane);
    let q = QuestionItem {
        q: Question {
            request_id: id.clone(),
            prompt,
            options,
            multi: false,
            free_text: false,
        },
        answered: None,
        reason: None,
    };
    let item = super::new_item(
        t.sid,
        id.clone(),
        ItemKind::Question,
        ItemStatus::Pending,
        Some(&body(&q)),
        now,
    );
    let sid = t.sid;
    match d.write_items(|tx, _, published| put(tx, sid, &Put::Done(item), published)) {
        Ok(()) => t.asked = Some(id),
        Err(e) => tracing::warn!(%sid, %e, "the pane's question was not stored"),
    }
}

/// Ends the open screen questions of `sid` as `Failed` with `reason`: nobody answered them here.
fn close_screen_asks(d: &Daemon, sid: Sid, reason: &str) {
    let Ok(open) = d.pending_questions(sid) else {
        return;
    };
    let puts: Vec<Put> = open
        .into_iter()
        .filter(|(_, q)| q.q.request_id.starts_with(SCREEN_ASK))
        .map(|(item, mut q)| {
            q.reason = Some(reason.into());
            Put::Done(super::new_item(
                sid,
                item.item_id,
                ItemKind::Question,
                ItemStatus::Failed,
                Some(&body(&q)),
                item.ts,
            ))
        })
        .collect();
    if puts.is_empty() {
        return;
    }
    let r = d.write_items(|tx, _, published| {
        for p in &puts {
            put(tx, sid, p, published)?;
        }
        Ok(())
    });
    if let Err(e) = r {
        tracing::warn!(%sid, %e, "the pane's question was not closed");
    }
}

/// What the TUI draws at the bottom of the pane's screen and waits on: its box (the call, what it
/// wants, "Do you want to proceed?") and its options, numbered ("1. Yes", wrapped over lines when
/// long) or a list under a cursor (the folder trust dialog's "❯ No, exit").
#[derive(Debug, PartialEq)]
struct ScreenPrompt {
    prompt: String,
    options: Vec<String>,
    /// the options are numbered: a digit picks one
    numbered: bool,
    /// the option the cursor is on
    cursor: usize,
    /// the footer says "Enter to …": a pick is confirmed with Enter (an AskUserQuestion)
    confirm: bool,
}

impl ScreenPrompt {
    /// The keys that pick option `i`.
    fn keys(&self, i: usize) -> Vec<String> {
        let mut keys = Vec::new();
        if self.numbered {
            keys.push((i + 1).to_string());
        } else {
            let (key, n) = match i.cmp(&self.cursor) {
                std::cmp::Ordering::Less => ("up", self.cursor - i),
                _ => ("down", i - self.cursor),
            };
            keys.extend(std::iter::repeat_n(key.to_owned(), n));
        }
        if self.confirm || !self.numbered {
            keys.push("enter".into());
        }
        keys
    }
}

/// The prompt at the end of `screen`; `None` when the screen ends in something else (output, the
/// input box, a numbered list in prose).
fn screen_prompt(screen: &str) -> Option<ScreenPrompt> {
    let lines: Vec<&str> = screen.lines().collect();
    let blank = |l: &str| l.trim().is_empty();
    let col = |l: &str| l.chars().take_while(|c| c.is_whitespace()).count();
    // a solid rule opens the box; dashed ones inside it part the call from what the TUI says about it
    let rule = |l: &str, solid: bool| {
        let t = l.trim();
        t.chars().count() >= 8
            && t.chars()
                .all(|c| matches!(c, '─' | '━' | '═') || (!solid && c == '╌'))
    };
    let hint = |l: &str| {
        ["Esc", "Enter", "ctrl+", "shift+", "Tab", "↑"]
            .iter()
            .any(|k| l.contains(k))
    };
    // "1. Yes" / "❯ 2. No": the number, its column and the label
    fn numbered(l: &str) -> Option<(usize, usize, &str)> {
        let t = l.trim_start().trim_start_matches('❯').trim_start();
        let (n, rest) = t.split_once(". ")?;
        let at = l.chars().position(|c| c.is_ascii_digit())?;
        Some((n.parse().ok()?, at, rest.trim()))
    }
    let cursor_on = |l: &str| l.trim_start().starts_with('❯');
    // below the options: hints, blanks and the odd line (a status line); more is output after an
    // answered prompt
    let mut end = lines.len();
    let (mut odd, mut hinted) = (0, false);
    loop {
        let l = *lines.get(end.checked_sub(1)?)?;
        if numbered(l).is_some() || cursor_on(l) {
            break;
        }
        if hint(l) {
            hinted = true;
        } else if !blank(l) {
            odd += 1;
            if odd > 2 {
                return None;
            }
        }
        end -= 1;
    }
    let mut last = end - 1;
    let is_numbered = numbered(lines[last]).is_some();
    // the label column: a number's, or the text after "❯ "
    let at = match numbered(lines[last]) {
        Some((_, at, _)) => at,
        None => col(lines[last]) + 2,
    };
    // the cursor's list goes on below it: the options at the label column
    if !is_numbered {
        while lines
            .get(last + 1)
            .is_some_and(|l| !blank(l) && !hint(l) && col(l) >= at)
        {
            last += 1;
        }
        end = last + 1;
    }
    // up over the block: option lines, continuations indented past the label, rules between them
    // (number, the cursor is on it, label)
    let mut groups: Vec<(Option<usize>, bool, String)> = vec![];
    let mut cont: Vec<&str> = vec![];
    let mut first = last;
    for i in (0..=last).rev() {
        let l = lines[i];
        if blank(l) {
            break;
        }
        if rule(l, false) {
            continue;
        }
        let option = if is_numbered {
            numbered(l)
                .filter(|(_, c, _)| *c == at)
                .map(|(n, _, s)| (Some(n), s))
        } else if cursor_on(l) {
            Some((None, l.trim_start().trim_start_matches('❯').trim()))
        } else if col(l) == at {
            Some((None, l.trim()))
        } else {
            None
        };
        match option {
            Some((n, label)) => {
                let mut text = label.to_owned();
                for c in cont.drain(..).rev() {
                    // an AskUserQuestion's description under its label: the same again, often
                    if c != label {
                        text.push(' ');
                        text.push_str(c);
                    }
                }
                groups.push((n, cursor_on(l), text));
                first = i;
            }
            None if col(l) > at => cont.push(l.trim()),
            None => break,
        }
    }
    groups.reverse();
    if groups.is_empty() {
        return None;
    }
    if is_numbered
        && groups
            .iter()
            .enumerate()
            .any(|(i, (n, ..))| *n != Some(i + 1))
    {
        return None;
    }
    // what asks: a question line right above the options, or the footer's hints
    let asks = lines[first.saturating_sub(3)..first]
        .iter()
        .any(|l| l.trim().ends_with('?'));
    if !asks && !hinted {
        return None;
    }
    let top = lines[..first]
        .iter()
        .rposition(|l| rule(l, true))
        .map_or(0, |i| i + 1);
    let prompt: Vec<String> = lines[top.max(first.saturating_sub(40))..first]
        .iter()
        .filter(|l| !rule(l, false))
        .map(|l| l.trim().trim_start_matches('│').trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect();
    Some(ScreenPrompt {
        prompt: prompt.join("\n"),
        cursor: groups.iter().position(|(_, on, _)| *on).unwrap_or(0),
        options: groups.into_iter().map(|(_, _, s)| s).collect(),
        numbered: is_numbered,
        confirm: lines[end..].iter().any(|l| l.contains("Enter to")),
    })
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
    /// The `/goal` as the lines read so far last said it, for the row.
    goal: Option<Goal>,
    /// A prompt was typed since the last row write: an achieved goal leaves the row.
    prompted: bool,
    /// What the lines read so far said of the context, for the row: what the last call read,
    /// the window, and the compactions since the last row write.
    ctx_used: Option<u64>,
    ctx_window: Option<u64>,
    compacted: u32,
    pending: Vec<Put>,
    /// The subagent transcripts beside it, by path.
    // ponytail: their offsets live only here. Reading them all again at every start was 1.5 GB
    // and got the daemon watchdog-killed in a loop, so a session the store already holds reads
    // the ones there at start from their end: what a subagent wrote while the daemon was down is
    // not shown. Persist the offsets if that gap ever matters.
    subs: HashMap<PathBuf, SubTail>,
    /// Whether the subagent transcripts there at start are read from their end (the store holds
    /// the session already) on the next look; false after the first.
    subs_from_end: bool,
    /// When the pane last went from working to idle, until its unechoed messages are settled.
    idle_since: Option<std::time::Instant>,
    /// Lines of kinds this reader does not know, by kind: each kind is one note, its count kept.
    // ponytail: counted from where this run started reading, so a restart counts again from there
    unknown: HashMap<String, u32>,
    /// The open question read off the pane's screen (`screen_asks`), by request id.
    asked: Option<String>,
    /// Whether the store was looked at once for a screen question a run before left open.
    asks_checked: bool,
    /// When the blocked pane's screen was last read and showed no question.
    screen_tried: Option<Instant>,
    /// The ExitPlanMode call the transcript has no result for yet: the pane is on the plan
    /// dialog, which herdr's detection reads as idle (its question wraps on a wide pane).
    plan_open: Option<String>,
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
            idle_since: None,
            unknown: HashMap::new(),
            asked: None,
            asks_checked: false,
            screen_tried: None,
            plan_open: None,
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

    /// Keeps `plan_open` in step with the line: an ExitPlanMode call opens it, its result closes it.
    fn note_plan(&mut self, line: &[u8]) {
        let (call, result) = (b"\"ExitPlanMode\"", b"\"tool_result\"");
        let has = |needle: &[u8]| line.windows(needle.len()).any(|w| w == needle);
        if !has(call) && !(self.plan_open.is_some() && has(result)) {
            return;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        let Some(blocks) = v["message"]["content"].as_array() else {
            return;
        };
        for b in blocks {
            if b["type"] == "tool_use" && b["name"] == "ExitPlanMode" {
                self.plan_open = b["id"].as_str().map(str::to_owned);
            } else if b["type"] == "tool_result"
                && b["tool_use_id"].as_str() == self.plan_open.as_deref()
            {
                self.plan_open = None;
            }
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
        let from_end = std::mem::take(&mut self.subs_from_end);
        let subs = tokio::task::spawn_blocking(move || read_subs(&dir, &known, from_end))
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
            self.note_plan(&line);
            let events = self.driver.on_line(at, &line, &mut out);
            out.stdin.clear(); // nobody listens: the pane's Claude reads its own terminal
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

// ---- recovering what was lost -----------------------------------------------------------------

/// Messages the user sent while Claude worked, written before this reader read them
/// (`queued_command`) and so never stored: put back where they were. Runs on the store before it
/// is shared; returns how many. Every item after one moved, so the caller rotates the epoch when
/// any were put back.
pub(crate) fn recover(store: &Store, home: &Path, uploads: &Path) -> anyhow::Result<usize> {
    let mut n = 0;
    for s in store.read(|c| sessions::list_sessions(c, true))? {
        if !s.owner.starts_with(OWNER) {
            continue;
        }
        let Some(path) = s.agent_sid.as_deref().and_then(|u| find(home, u)) else {
            continue;
        };
        let mut held = HashMap::new();
        let mut before = None;
        loop {
            let page = store.read(|c| store_items::page_items(c, s.id, before, 2000))?;
            let Some(last) = page.last() else { break };
            before = Some(last.seq);
            held.extend(page.into_iter().map(|i| (i.item_id, i.seq)));
        }
        let end = held.values().max().map_or(1, |s| s + 1);
        let file = std::io::BufReader::new(std::fs::File::open(&path)?);
        let mut lost = lost_in(file, &held, end, &uploads.join(s.id.to_string()))?;
        if lost.is_empty() {
            continue;
        }
        tracing::info!(sid = %s.id, n = lost.len(), "messages sent mid-turn put back");
        n += lost.len();
        store.write(|tx| tx.insert_items_before(s.id, &mut lost))?;
    }
    Ok(n)
}

/// The queued messages of a transcript that the store does not hold (`held`: item id to seq), each
/// with the seq it goes before: that of the first later line the store holds, or `end`. Only
/// from the first line the store holds on: what is before it was never read at all.
fn lost_in(
    file: impl std::io::BufRead,
    held: &HashMap<String, u64>,
    end: u64,
    uploads: &Path,
) -> std::io::Result<Vec<(u64, Item)>> {
    let mut started = false;
    let mut waiting: Vec<Item> = Vec::new();
    let mut out = Vec::new();
    for line in file.split(b'\n') {
        let line = line?;
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if v["isSidechain"] == true {
            continue;
        }
        if let Some(&seq) = ids_of(&v).iter().filter_map(|id| held.get(id)).min() {
            started = true;
            out.extend(waiting.drain(..).map(|i| (seq, i)));
            continue;
        }
        if !started || v["attachment"]["type"] != "queued_command" {
            continue;
        }
        let Some(Disk::Feed {
            typed: Some((id, text)),
            ..
        }) = disk_line(&line, uploads)
        else {
            continue;
        };
        let ts = v["timestamp"]
            .as_str()
            .and_then(crate::scheduler::rfc3339_ms)
            .unwrap_or_else(now_ms);
        let b = body(&UserMessage { text });
        waiting.push(super::new_item(
            Id(0),
            id,
            ItemKind::UserMessage,
            ItemStatus::Completed,
            Some(&b),
            ts,
        ));
    }
    out.extend(waiting.into_iter().map(|i| (end, i)));
    Ok(out)
}

/// The ids of the items a transcript line is stored as: its own `uuid` (a message, an event), its
/// tool calls and their results, and its text blocks (`<message id>:<index>`).
fn ids_of(v: &Value) -> Vec<String> {
    let mut ids: Vec<String> = v["uuid"].as_str().map(str::to_owned).into_iter().collect();
    let msg = &v["message"];
    for (i, b) in msg["content"].as_array().into_iter().flatten().enumerate() {
        match b["type"].as_str() {
            Some("tool_use") => ids.extend(b["id"].as_str().map(str::to_owned)),
            Some("tool_result") => {
                ids.extend(b["tool_use_id"].as_str().map(|t| format!("{t}:result")));
            }
            _ => ids.extend(msg["id"].as_str().map(|m| format!("{m}:{i}"))),
        }
    }
    ids
}

/// A message the user sent (`typed`: its id and text) that the driver's writes `puts` do not
/// hold as their message: stored as one anyway, so a reader slip never loses it.
fn missed(sid: Sid, puts: &[Put], (id, text): (String, String)) -> Option<Put> {
    let held = puts.iter().any(|p| match p {
        Put::Start(i) | Put::Done(i) => i.item_id == id && i.kind == ItemKind::UserMessage,
        Put::Update(..) => false,
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
/// its end with `from_end` (the ones there when the daemon starts on a session it holds).
fn read_subs(dir: &Path, known: &[(PathBuf, u64)], from_end: bool) -> Vec<SubRead> {
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
enum Disk {
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
fn disk_line(line: &[u8], uploads: &Path) -> Option<Disk> {
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
fn queued(v: Value) -> Option<Value> {
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
    if v["isMeta"] == true && v["origin"]["kind"] != "peer" {
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
                v = serde_json::json!({
                    "type": "system",
                    "subtype": "task_notification",
                    "tool_use_id": tag("tool-use-id"),
                    "status": tag("status"),
                    "summary": tag("summary").unwrap_or_else(|| "A background task reported".into()),
                    "uuid": v["uuid"],
                });
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

// ---- the way in ---------------------------------------------------------------------------------

/// The session's head: a prompt is typed into the pane, an interrupt is its Escape, a screen
/// question's answer is its option's number (or Escape). Transcript questions, models and
/// takeovers are the pane's.
fn head(bin: String, hs: String, pane: String, sid: Sid, d: Weak<Daemon>) -> Head {
    let (tx, mut rx) = mpsc::channel(16);
    tokio::spawn(async move {
        let not_here = || Err(CoreError::Bad("a herdr pane: do that in the pane".into()));
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Cmd::Prompt { text, reply, .. } => {
                    if super::model_switch(&text).is_some_and(|m| super::is_gateway(Some(&m))) {
                        let why =
                            "a herdr pane's Claude cannot move to DeepSeek: start a FEMME chat";
                        let _ = reply.send(Err(CoreError::Bad(why.into())));
                        continue;
                    }
                    let r = herdr(&bin, Some(&hs), &["agent", "prompt", &pane, &text])
                        .await
                        .map_err(|e| match e {
                            // blocked on its prompt: say so, "agent_not_ready" says nothing
                            CoreError::Busy => CoreError::Conflict(WAITING.into()),
                            e => e,
                        });
                    if let Err(e) = &r {
                        tracing::debug!(%sid, %e, "prompt not typed into the herdr pane");
                    }
                    let _ = reply.send(r.map(drop));
                }
                // The pane's prompt takes the answer as keys, whether FEMME asked it from the
                // screen or the transcript did (an AskUserQuestion): the screen as it is now says
                // which keys pick the option.
                Cmd::Answer {
                    request_id,
                    answer,
                    reply,
                } => {
                    let keys = match &answer {
                        Answer::Cancel => Ok(vec!["esc".to_owned()]),
                        Answer::Option(i) => {
                            let i = usize::from(*i);
                            match herdr(&bin, Some(&hs), &["agent", "read", &pane]).await {
                                Err(e) => Err(e),
                                Ok(screen) => match screen_prompt(&screen) {
                                    Some(p) if i < p.options.len() => Ok(p.keys(i)),
                                    Some(p) => Err(CoreError::Bad(format!(
                                        "the pane's prompt has {} options",
                                        p.options.len()
                                    ))),
                                    None => Err(CoreError::Conflict(
                                        "the pane shows no prompt now: it was answered there"
                                            .into(),
                                    )),
                                },
                            }
                        }
                        _ => Err(CoreError::Bad(
                            "the pane's prompt takes one option, or Escape".into(),
                        )),
                    };
                    let r = match keys {
                        Ok(keys) => {
                            let mut args = vec!["pane", "send-keys", pane.as_str()];
                            args.extend(keys.iter().map(String::as_str));
                            herdr(&bin, Some(&hs), &args).await.map(drop)
                        }
                        Err(e) => Err(e),
                    };
                    if r.is_ok() {
                        if let Some(d) = d.upgrade() {
                            if let Some(p) = d.answered_part(sid, &request_id, &answer) {
                                let w =
                                    d.write_items(|tx, _, published| put(tx, sid, &p, published));
                                if let Err(e) = w {
                                    tracing::warn!(%sid, %e, "the pane's answer was not stored");
                                }
                            }
                        }
                    }
                    let _ = reply.send(r);
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
                Cmd::SetModel { reply, .. } | Cmd::Takeover { reply, .. } => {
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
    use femme_agents::driver_for;
    use femme_proto::rpc::PromptHow;
    use femme_proto::state::{ItemKind, ItemStatus};
    use femme_store::items as store_items;
    use std::os::unix::fs::PermissionsExt as _;

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
        match disk_line(v.to_string().as_bytes(), Path::new("/nonexistent"))? {
            Disk::Feed { line, .. } => Some(serde_json::from_slice(&line).unwrap()),
            Disk::Unknown(k) => panic!("unknown: {k}"),
        }
    }

    fn typed(v: Value) -> Option<(String, String)> {
        match disk_line(v.to_string().as_bytes(), Path::new("/nonexistent"))? {
            Disk::Feed { typed, .. } => typed,
            Disk::Unknown(_) => None,
        }
    }

    /// A message sent while Claude worked, as Claude Code 2.1 writes it (from a real transcript).
    fn queued_line(kind: &str, prompt: Value) -> Value {
        serde_json::json!({"type":"attachment","uuid":"ca3e","timestamp":"2026-11-09T13:55:06.152Z",
            "isSidechain":false,"attachment":{"type":"queued_command","commandMode":"prompt",
            "delivery_id":"d9d9","source_uuid":"s1","humanTurn":true,"origin":{"kind":kind},
            "prompt":prompt,"timestamp":"2026-11-09T13:53:24.095Z"}})
    }

    #[test]
    fn a_message_sent_while_claude_worked_is_the_users() {
        let v = line(queued_line("human", "stasge commit push these btw".into())).unwrap();
        assert_eq!(
            (
                &v["type"],
                &v["isReplay"],
                &v["uuid"],
                &v["message"]["content"]
            ),
            (
                &Value::from("user"),
                &Value::Bool(true),
                &Value::from("ca3e"),
                &Value::from("stasge commit push these btw")
            )
        );
        assert_eq!(
            typed(queued_line("human", "hi".into())),
            Some(("ca3e".into(), "hi".into()))
        );
        let peer = line(queued_line("peer", "<cross-session-message>x".into())).unwrap();
        assert_eq!(
            peer["isReplay"],
            Value::Null,
            "another session's: not typed"
        );
        assert_eq!(typed(queued_line("peer", "x".into())), None);
        let mut meta = queued_line("peer", "x".into());
        meta["attachment"]["isMeta"] = Value::Bool(true);
        assert_eq!(line(meta), None, "for the model only");
        let note = line(queued_line(
            "task",
            "<task-notification>\n<tool-use-id>toolu_1</tool-use-id>\n<status>completed</status>\n</task-notification>".into(),
        ))
        .unwrap();
        assert_eq!(note["subtype"], "task_notification");
        let blocks =
            serde_json::json!([{"type":"text","text":"look"},{"type":"text","text":"here"}]);
        assert_eq!(
            typed(queued_line("human", blocks.clone())),
            Some(("ca3e".into(), "look\nhere".into())),
            "the text, not the blocks' JSON"
        );
        assert_eq!(
            line(queued_line("human", blocks)).unwrap()["isReplay"],
            true
        );
    }

    #[test]
    fn a_kind_of_line_not_known_is_said_not_dropped() {
        let read = |v: Value| disk_line(v.to_string().as_bytes(), Path::new("/nonexistent"));
        assert!(matches!(
            read(serde_json::json!({"type":"brand-new"})),
            Some(Disk::Unknown(k)) if k == "brand-new"
        ));
        assert!(matches!(
            read(serde_json::json!({"type":"attachment","attachment":{"type":"brand_new"}})),
            Some(Disk::Unknown(k)) if k == "attachment brand_new"
        ));
        for known in [
            serde_json::json!({"type":"queue-operation","operation":"enqueue"}),
            serde_json::json!({"type":"attachment","attachment":{"type":"hook_success"}}),
            serde_json::json!({"type":"last-prompt"}),
        ] {
            assert!(read(known).is_none());
        }
    }

    #[test]
    fn a_lost_message_goes_back_before_the_next_line_held() {
        let file = [
            r#"{"type":"user","uuid":"old","message":{"content":"never read"}}"#,
            r#"{"type":"attachment","uuid":"q0","timestamp":"2026-11-09T10:00:00Z","attachment":{"type":"queued_command","origin":{"kind":"human"},"prompt":"before the start"}}"#,
            r#"{"type":"user","uuid":"m1","message":{"content":"go"}}"#,
            r#"{"type":"assistant","message":{"id":"a1","content":[{"type":"tool_use","id":"t1"}]}}"#,
            r#"{"type":"attachment","uuid":"q1","timestamp":"2026-11-09T10:00:01Z","attachment":{"type":"queued_command","origin":{"kind":"human"},"prompt":"one"}}"#,
            r#"{"type":"attachment","uuid":"q2","attachment":{"type":"queued_command","origin":{"kind":"peer"},"prompt":"not typed"}}"#,
            r#"{"type":"attachment","uuid":"q3","attachment":{"type":"queued_command","origin":{"kind":"human"},"prompt":"two"}}"#,
            r#"{"type":"user","uuid":"r1","message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#,
            r#"{"type":"attachment","uuid":"q4","attachment":{"type":"queued_command","origin":{"kind":"human"},"prompt":"held already"}}"#,
            r#"{"type":"assistant","message":{"id":"a2","content":[{"type":"text","text":"ok"}]}}"#,
            r#"{"type":"attachment","uuid":"q5","attachment":{"type":"queued_command","origin":{"kind":"human"},"prompt":"last"}}"#,
        ]
        .join("\n");
        let held: HashMap<String, u64> = [
            ("m1", 1),
            ("t1", 2),
            ("t1:result", 3),
            ("q4", 4),
            ("a2:0", 5),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        let lost = lost_in(file.as_bytes(), &held, 6, Path::new("/nonexistent")).unwrap();
        let got: Vec<(u64, &str)> = lost.iter().map(|(b, i)| (*b, i.item_id.as_str())).collect();
        assert_eq!(got, [(3, "q1"), (3, "q3"), (6, "q5")]);
        assert_eq!(lost[0].1.ts, 1_794_218_401_000, "when Claude took it in");
        assert!(lost[0].1.preview.contains("one"));
    }

    #[test]
    fn a_typed_message_the_driver_drops_is_stored_anyway() {
        let sid = Id(1);
        let kept = Put::Done(super::super::new_item(
            sid,
            "u1".into(),
            ItemKind::UserMessage,
            ItemStatus::Completed,
            None,
            0,
        ));
        assert!(missed(sid, &[kept], ("u1".into(), "hi".into())).is_none());
        let Some(Put::Done(i)) = missed(sid, &[], ("u1".into(), "hi".into())) else {
            panic!("not stored");
        };
        assert_eq!((i.item_id.as_str(), i.kind), ("u1", ItemKind::UserMessage));
        assert!(i.preview.contains("hi"));
    }

    /// The transcript's goal lines, read as a mirrored pane reads them (file, driver, fold), give
    /// the goal its states in order.
    #[test]
    fn the_files_goal_lines_become_the_sessions_goal() {
        let mut drv = driver_for(Agent::Claude);
        let mut st = FoldState::default();
        let mut state = |v: Value| {
            let Disk::Feed { line, .. } =
                disk_line(v.to_string().as_bytes(), Path::new("/nonexistent"))?
            else {
                return None;
            };
            let ev = drv.on_line(0, &line, &mut Out::default());
            fold(&mut st, Sid::new(), "d0", ev, 0)
                .goal
                .map(|g| (g.state, g.reason))
        };
        let att = |a: Value| serde_json::json!({"type":"attachment","attachment":a});
        assert_eq!(
            state(att(
                serde_json::json!({"type":"goal_status","met":false,"sentinel":true,"condition":"c"})
            )),
            Some((GoalState::Active, None))
        );
        assert_eq!(
            state(att(
                serde_json::json!({"type":"goal_status","met":false,"condition":"c","reason":"r1"})
            )),
            Some((GoalState::NotMet, Some("r1".into())))
        );
        assert_eq!(
            state(att(
                serde_json::json!({"type":"goal_status","met":true,"condition":"c","reason":"r2"})
            )),
            Some((GoalState::Met, Some("r2".into())))
        );
        assert_eq!(
            state(
                serde_json::json!({"type":"user","message":{"content":[{"type":"text",
                "text":"<local-command-stdout>Goal cleared: c</local-command-stdout>"}]}})
            ),
            Some((GoalState::Cleared, None))
        );
        assert_eq!(state(att(serde_json::json!({"type":"other"}))), None);
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
            serde_json::json!({"type":"file-history-snapshot"}),
        ] {
            assert_eq!(line(skipped), None);
        }
    }

    /// A peer's message (SendMessage from another session) passes the `isMeta` drop, whether it
    /// started a turn (a `user` entry) or landed mid-turn (a `queued_command` attachment), and the
    /// driver maps both to a `PeerMessage` from the sender, not a `UserMessage`.
    #[test]
    fn a_peer_message_reads_as_a_peer_message() {
        use femme_agents::Driver as _;
        let framed = "Another Claude session sent a message:\n<cross-session-message from=\"uds:/x.sock\" from-name=\"platform-12\" from-mode=\"bypass\">\nhello\n</cross-session-message>\n\nThis came from another Claude session";
        let origin = serde_json::json!({"kind":"peer","name":"platform-12","body":"hello"});
        let entry = serde_json::json!({"type":"user","uuid":"u1","isMeta":true,"promptSource":"system",
            "turnOrigin":"peer","origin":origin,"message":{"role":"user","content":framed}});
        let queued = serde_json::json!({"type":"attachment","uuid":"u2","attachment":{"type":"queued_command",
            "prompt":"<cross-session-message from=\"uds:/x.sock\" from-name=\"platform-12\" from-mode=\"bypass\">\nhello\n</cross-session-message>",
            "isMeta":true}});
        for v in [entry, queued] {
            let l = line(v).unwrap();
            assert_eq!(
                (&l["type"], &l["origin"]["kind"]),
                (&Value::from("user"), &Value::from("peer"))
            );
            let mut d = femme_agents::claude::ClaudeDriver::new();
            let mut out = femme_agents::Out::default();
            d.on_start(&femme_agents::Launch::default(), &mut out);
            let init = br#"{"type":"control_response","response":{"subtype":"success","request_id":"femme-init","response":{}}}"#;
            d.on_line(0, init, &mut out);
            let ev = d.on_line(1, l.to_string().as_bytes(), &mut out);
            let kinds: Vec<_> = ev
                .iter()
                .filter_map(|e| match e {
                    femme_agents::AgentEvent::ItemCompleted { kind, body, .. } => {
                        Some((*kind, body["from"].clone(), body["text"].clone()))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                kinds,
                [(
                    ItemKind::PeerMessage,
                    Value::from("platform-12"),
                    Value::from("hello")
                )]
            );
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
            let Some(Disk::Feed { line, .. }) = disk_line(l.as_bytes(), dir.path()) else {
                panic!("not fed");
            };
            serde_json::from_slice(&line).unwrap()
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
{"type":"attachment","attachment":{"type":"hook_success"}}
{"type":"attachment","uuid":"q1","attachment":{"type":"queued_command","commandMode":"prompt","origin":{"kind":"human"},"prompt":"and the tests"}}
{"type":"system","subtype":"compact_boundary","uuid":"c1","compactMetadata":{"trigger":"auto","preTokens":310172}}
{"type":"assistant","message":{"id":"a2","model":"claude-fable-5-1","content":[{"type":"text","text":"one file"}],"usage":{"input_tokens":2,"cache_read_input_tokens":40000,"cache_creation_input_tokens":500,"output_tokens":6}}}
{"type":"system","subtype":"turn_duration","durationMs":12}
"#;

    /// What `herdr agent read` showed for a pane blocked on a Bash permission (2026-11-10).
    const SCREEN: &str = r#"    progress.

  Version 2, the real map, is still being built.

✻ Waiting for 1 background agent to finish

───────────────────────────────────────────────────────────────
 Bash command · from the general-purpose agent
 Build contact sheets from recording
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 │ cd /tmp/claude-1000/-home-darkwarro-Documents-Web-Apps-Plat
 │ form/766de32b-275e-4bda-aa13-1aba430a617d/scratchpad/v2 &&
 │ mkdir -p fr && rm -f fr/* && ffmpeg -loglevel error -i
 │ r1.webm -vf "fps=2,scale=640:-1,tile=6x5" -frames:v 2
 │ fr/sheet%d.png && rtk ls fr; ffprobe -v error -show_entries
 │ format=duration -of csv=p=0 r1.webm
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 │ Compound command contains cd with write operation - manual
 │ approval required to prevent path resolution bypass

 Do you want to proceed?
 ❯ 1. Yes
   2. No

 Esc to cancel · Tab to amend · ctrl+x ctrl+k twice to stop b…
"#;

    #[test]
    fn the_permission_prompt_on_the_screen_is_a_question_with_its_options() {
        let p = screen_prompt(SCREEN).unwrap();
        assert_eq!(p.options, ["Yes", "No"]);
        assert!(p.numbered && !p.confirm);
        assert_eq!(p.keys(1), ["2"]);
        let lines: Vec<&str> = p.prompt.lines().collect();
        assert_eq!(lines[0], "Bash command · from the general-purpose agent");
        assert_eq!(lines[1], "Build contact sheets from recording");
        assert!(lines[2].starts_with("cd /tmp/claude-1000/"));
        assert_eq!(lines[lines.len() - 1], "Do you want to proceed?");
        assert!(p
            .prompt
            .contains("approval required to prevent path resolution bypass"));
        assert!(!p.prompt.contains("╌"), "{}", p.prompt);
    }

    /// The Write prompt: an option wrapped over three lines is one option.
    const EDIT: &str = r#"● Write(notes.txt)

─────────────────────────────────────────────────────────
 Create file
 notes.txt
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
  1 hello
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 Do you want to create notes.txt?
 ❯ 1. Yes
   2. Yes, and switch to accept edits (auto-approve file
      edits and common file commands) for this session
      (shift+tab)
   3. No

 Esc to cancel · Tab to amend"#;

    #[test]
    fn a_wrapped_option_is_one_option() {
        let p = screen_prompt(EDIT).unwrap();
        assert_eq!(p.options.len(), 3, "{:?}", p.options);
        assert_eq!(p.options[0], "Yes");
        assert_eq!(
            p.options[1],
            "Yes, and switch to accept edits (auto-approve file edits and common file commands) \
             for this session (shift+tab)"
        );
        assert_eq!(p.options[2], "No");
        assert_eq!(
            p.prompt,
            "Create file\nnotes.txt\n1 hello\nDo you want to create notes.txt?"
        );
    }

    /// An AskUserQuestion: a description under each label, "Type something." and, past a rule,
    /// "Chat about this"; a digit moves the cursor and Enter picks.
    const ASK: &str = r#"  which colour do you prefer, with the options red,
  green and blue. Then reply with my answer.
─────────────────────────────────────────────────────────
 ☐ Colour

Which colour do you prefer?

❯ 1. Red
     Red
  2. Green
     Green
  3. Blue
     Blue
  4. Type something.
─────────────────────────────────────────────────────────
  5. Chat about this

Enter to select · ↑/↓ to navigate · Esc to cancel"#;

    #[test]
    fn an_ask_user_question_on_the_screen_picks_with_a_digit_and_enter() {
        let p = screen_prompt(ASK).unwrap();
        assert_eq!(
            p.options,
            ["Red", "Green", "Blue", "Type something.", "Chat about this"]
        );
        assert!(p.numbered && p.confirm);
        assert_eq!(p.keys(1), ["2", "enter"]);
        assert_eq!(p.prompt, "☐ Colour\nWhich colour do you prefer?");
    }

    /// The plan dialog: the first option wraps, a hint sits under the last one and more below.
    const PLAN: &str = r#"
   Plan

   1. Write /tmp/femme-prompts/README.md with the
      Write tool. Content: a # femme-prompts heading
      and one short paragraph saying the folder holds
      small test files (hello.txt, notes.txt) created
      in a Claude Code session.

   Verification                                        ↓
  ─────────────────────────────────────────────────────
   Claude has written up a plan and is ready to
   execute. Would you like to proceed?

   ❯ 1. Yes, and switch to BYPASS PERMISSIONS (no
        further prompts) for this session
     2. Yes, manually approve edits
     3. Tell Claude what to change
        shift+tab to approve with this feedback

   ctrl+g to edit in Vim ·
   ~/.claude/plans/plan-how-to-add-polished-charm.md"#;

    #[test]
    fn the_plan_dialog_is_a_question_despite_its_footer() {
        let p = screen_prompt(PLAN).unwrap();
        assert_eq!(p.options.len(), 3, "{:?}", p.options);
        assert!(p.options[0].starts_with("Yes, and switch to BYPASS PERMISSIONS (no further"));
        assert_eq!(p.options[1], "Yes, manually approve edits");
        assert_eq!(p.options[2], "Tell Claude what to change");
        assert!(
            p.prompt.ends_with("Would you like to proceed?"),
            "{}",
            p.prompt
        );
        assert!(!p.prompt.contains("Verification"), "{}", p.prompt);
    }

    /// The folder trust dialog: no numbers, a cursor; the answer moves it and presses Enter.
    const TRUST: &str = r#"darkwarro@agentbox:/tmp/femme-prompts$ claude

─────────────────────────────────────────────────────────
 Accessing workspace:

 /tmp/femme-prompts

 Quick safety check: Is this a project you created or
 one you trust? (Like your own code, a well-known open
 source project, or work from your team). If not, take a
 moment to review what's in this folder first.

 Claude Code'll be able to read, edit, and execute files
 here.

 Security guide

 ❯ No, exit
   Yes, I trust this folder

 Enter to confirm · Esc to cancel"#;

    #[test]
    fn an_unnumbered_prompt_is_answered_by_moving_the_cursor() {
        let p = screen_prompt(TRUST).unwrap();
        assert_eq!(p.options, ["No, exit", "Yes, I trust this folder"]);
        assert!(!p.numbered && p.confirm && p.cursor == 0);
        assert_eq!(p.keys(1), ["down", "enter"]);
        assert_eq!(p.keys(0), ["enter"]);
        assert!(p
            .prompt
            .starts_with("Accessing workspace:\n/tmp/femme-prompts\nQuick safety"));
        assert!(p.prompt.ends_with("Security guide"));
        // the cursor further down: up to the first
        let moved = TRUST
            .replace(" ❯ No, exit", "   No, exit")
            .replace("   Yes, I trust", " ❯ Yes, I trust");
        let p = screen_prompt(&moved).unwrap();
        assert_eq!(
            (p.cursor, p.keys(0)),
            (1, vec!["up".to_owned(), "enter".to_owned()])
        );
    }

    #[test]
    fn a_screen_with_no_prompt_at_its_end_is_no_question() {
        // a numbered list in prose, no question above it
        let prose = "The plan:\n  1. The nut shakes\n  2. The nuts fly up\n\n> \n";
        assert_eq!(screen_prompt(prose), None);
        // an answered prompt: the options scrolled up, output follows
        let answered = format!("{SCREEN}\n fr/sheet1.png\n fr/sheet2.png\n 61.2\n\n> ");
        assert_eq!(screen_prompt(&answered), None);
        assert_eq!(screen_prompt(""), None);
        // options not numbered from 1 are prose
        assert_eq!(screen_prompt(" Ready?\n 2. No\n 3. Maybe\n"), None);
        // a user's message echoed with the cursor glyph, then output
        assert_eq!(screen_prompt("❯ what is 2+2?\n\n⏺ 4\n"), None);
    }

    /// A `herdr` stand-in: `agent read` prints `<dir>/screen`, `pane send-keys` logs to
    /// `<dir>/keys`, `agent prompt` is refused as blocked.
    fn fake_herdr(dir: &Path) -> String {
        let bin = dir.join("herdr");
        let script = format!(
            "#!/bin/sh\nwhile [ \"$1\" = --session ]; do shift 2; done\n\
             case \"$1 $2\" in\n\
             \"agent read\") cat {d}/screen ;;\n\
             \"pane send-keys\") echo \"$3 $4\" >> {d}/keys ;;\n\
             \"agent prompt\") echo agent_blocked; exit 1 ;;\n\
             *) exit 1 ;;\nesac\n",
            d = dir.display()
        );
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin.display().to_string()
    }

    const BLOCKED: &str = r#"{"id":"cli:pane:list","result":{"type":"pane_list","panes":[
        {"agent":"claude","agent_session":{"agent":"claude","kind":"id","value":"u-1"},
         "agent_status":"blocked","cwd":"/w","pane_id":"wA:p1","terminal_title_stripped":"fix it"}]}}"#;

    fn question_items(d: &Daemon, sid: Sid) -> Vec<(ItemStatus, QuestionItem)> {
        d.store
            .read(|c| store_items::page_items(c, sid, None, 100))
            .unwrap()
            .into_iter()
            .rev()
            .filter(|i| i.kind == ItemKind::Question)
            .map(|i| {
                let q = serde_json::from_str(i.body.as_deref().unwrap()).unwrap();
                (i.status, q)
            })
            .collect()
    }

    #[tokio::test]
    async fn a_blocked_pane_asks_its_screen_prompt_and_an_answer_is_keys_into_the_pane() {
        let (d, dir) = daemon();
        std::fs::write(dir.path().join("screen"), SCREEN).unwrap();
        let bin = fake_herdr(dir.path());
        let mut tails = HashMap::new();
        let blocked = parse_panes("default", BLOCKED);
        sync(&d, &bin, &blocked, &mut tails).await;
        let sid = d.store.read(|c| sessions::list_sessions(c, false)).unwrap()[0].id;
        let q = d
            .pending_question(sid)
            .unwrap()
            .expect("the prompt is a question");
        assert!(q.request_id.starts_with("herdr:wA:p1:"), "{}", q.request_id);
        assert_eq!(q.options, ["Yes", "No"]);
        assert!(q.prompt.starts_with("Bash command"));
        // asked once: another look adds nothing
        sync(&d, &bin, &blocked, &mut tails).await;
        assert_eq!(question_items(&d, sid).len(), 1);

        // a prompt meanwhile says what Claude waits on
        let r = d
            .sessions
            .head(sid)
            .unwrap()
            .prompt("go", PromptHow::Queue)
            .await;
        assert_eq!(r, Err(CoreError::Conflict(WAITING.into())));

        // the answer: its number into the pane (no Enter: a permission prompt), the item answered
        d.answer_question(sid, &q.request_id, Answer::Option(0))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("keys")).unwrap(),
            "wA:p1 1\n"
        );
        let items = question_items(&d, sid);
        assert_eq!(items[0].0, ItemStatus::Answered);
        assert_eq!(items[0].1.answered, Some(Answer::Option(0)));
        // the pane goes on: nothing to close, the answer stands
        let mut working = blocked.clone();
        working[0].status = "working".into();
        sync(&d, &bin, &working, &mut tails).await;
        assert_eq!(question_items(&d, sid)[0].0, ItemStatus::Answered);

        // escape cancels; text is not what the prompt takes
        sync(&d, &bin, &blocked, &mut tails).await;
        let q = d.pending_question(sid).unwrap().unwrap();
        let r = d
            .answer_question(sid, &q.request_id, Answer::Text("why".into()))
            .await;
        assert!(matches!(r, Err(CoreError::Bad(_))), "{r:?}");
        d.answer_question(sid, &q.request_id, Answer::Cancel)
            .await
            .unwrap();
        assert!(std::fs::read_to_string(dir.path().join("keys"))
            .unwrap()
            .ends_with("wA:p1 esc\n"));
    }

    #[tokio::test]
    async fn a_prompt_answered_in_the_pane_closes_the_question_here() {
        let (d, dir) = daemon();
        std::fs::write(dir.path().join("screen"), SCREEN).unwrap();
        let bin = fake_herdr(dir.path());
        let mut tails = HashMap::new();
        let blocked = parse_panes("default", BLOCKED);
        sync(&d, &bin, &blocked, &mut tails).await;
        let sid = d.store.read(|c| sessions::list_sessions(c, false)).unwrap()[0].id;
        assert!(d.pending_question(sid).unwrap().is_some());
        let mut idle = blocked.clone();
        idle[0].status = "idle".into();
        sync(&d, &bin, &idle, &mut tails).await;
        let items = question_items(&d, sid);
        assert_eq!(items[0].0, ItemStatus::Failed);
        assert_eq!(items[0].1.reason.as_deref(), Some("answered in the pane"));
        assert!(d.pending_question(sid).unwrap().is_none());

        // a daemon that starts while the pane is blocked adopts the open question, no second one
        sync(&d, &bin, &blocked, &mut tails).await;
        let mut fresh = HashMap::new();
        sync(&d, &bin, &blocked, &mut fresh).await;
        assert_eq!(
            question_items(&d, sid)
                .iter()
                .filter(|(s, _)| *s == ItemStatus::Pending)
                .count(),
            1
        );
        // and one that starts after the pane moved on closes what was left open
        let mut later = HashMap::new();
        sync(&d, &bin, &idle, &mut later).await;
        assert!(d.pending_question(sid).unwrap().is_none());
    }

    /// herdr reads the plan dialog as idle (its question wraps on a wide pane): the transcript's
    /// ExitPlanMode call with no result yet says the pane is on it.
    #[tokio::test]
    async fn an_idle_pane_on_the_plan_dialog_asks_it_until_the_call_has_its_result() {
        let (d, dir) = daemon();
        let proj = dir.path().join(".claude/projects/-w");
        std::fs::create_dir_all(&proj).unwrap();
        let call = r#"{"type":"user","uuid":"m1","message":{"role":"user","content":"plan it"}}
{"type":"assistant","message":{"id":"a1","content":[{"type":"tool_use","id":"tp","name":"ExitPlanMode","input":{"plan":"write README"}}]}}
"#;
        std::fs::write(proj.join("u-1.jsonl"), call).unwrap();
        std::fs::write(dir.path().join("screen"), PLAN).unwrap();
        let bin = fake_herdr(dir.path());
        let mut idle = parse_panes("default", BLOCKED);
        idle[0].status = "idle".into();
        let mut tails = HashMap::new();
        sync(&d, &bin, &idle, &mut tails).await;
        let sid = d.store.read(|c| sessions::list_sessions(c, false)).unwrap()[0].id;
        assert!(
            d.pending_question(sid).unwrap().is_none(),
            "nothing read yet"
        );
        for t in tails.values_mut() {
            t.tail(&d).await;
        }
        sync(&d, &bin, &idle, &mut tails).await;
        let q = d.pending_question(sid).unwrap().expect("the plan dialog");
        assert!(q.request_id.starts_with(SCREEN_ASK));
        assert_eq!(q.options[1], "Yes, manually approve edits");
        let s = d.store.read(|c| sessions::list_sessions(c, false)).unwrap();
        assert_eq!(s[0].status, SessionStatus::Blocked);
        // answered in the pane: the result line arrives, the question closes, the row goes idle
        let result = r#"{"type":"user","uuid":"r1","message":{"content":[{"type":"tool_result","tool_use_id":"tp","content":"approved"}]}}
"#;
        std::fs::write(proj.join("u-1.jsonl"), format!("{call}{result}")).unwrap();
        for t in tails.values_mut() {
            t.tail(&d).await;
        }
        sync(&d, &bin, &idle, &mut tails).await;
        assert!(d.pending_question(sid).unwrap().is_none());
        let s = d.store.read(|c| sessions::list_sessions(c, false)).unwrap();
        assert_eq!(s[0].status, SessionStatus::Idle);
    }

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
                (ItemKind::UserMessage, ItemStatus::Completed),
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
        let r = d
            .sessions
            .head(sid)
            .unwrap()
            .prompt("go", PromptHow::Queue)
            .await;
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

    #[test]
    fn subagent_transcripts_there_at_a_restart_are_read_from_their_end() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("agent-x1.jsonl");
        std::fs::write(&sub, SUB).unwrap();
        std::fs::write(
            dir.path().join("agent-x1.meta.json"),
            r#"{"toolUseId":"toolu_A"}"#,
        )
        .unwrap();
        let fresh = read_subs(dir.path(), &[], false);
        assert_eq!(fresh[0].2, 0, "a session new to the store: from the start");
        assert_eq!(fresh[0].3.as_ref().unwrap().len(), SUB.len());
        let restart = read_subs(dir.path(), &[], true);
        assert_eq!(restart[0].2, SUB.len() as u64, "held already: from the end");
        assert!(
            restart[0].3.as_ref().unwrap().is_empty(),
            "nothing read again"
        );
    }

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
