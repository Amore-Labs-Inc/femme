//! Claude Code's own conversations on this machine, listed for picking one to go on with
//! (`Req::Conversations`): every `~/.claude/projects/*/<uuid>.jsonl`, whoever ran it. That is a
//! Claude in a plain terminal FEMME never started as much as a chat of FEMME's that was closed.
//! `SessionCreate.resume` takes the uuid.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use femme_proto::rpc::Conversation;
use serde_json::Value;

use super::backfill::title_line;

/// How much of a transcript's start is read for its folder and first message.
const HEAD: u64 = 256 << 10;
/// How much of its end is read for its title.
// ponytail: a title older than the last 512 KiB is missed and the first message stands in for it
const TAIL: u64 = 512 << 10;
/// A first message that stands in for a title is cut here.
const TITLE_CHARS: usize = 80;

/// The conversations no session holds (`held`: the conversations of the open sessions), newest
/// first, at most `limit`. One whose folder is gone is left out: nothing can go on with it.
pub(crate) fn list(home: &Path, held: &HashSet<String>, limit: usize) -> Vec<Conversation> {
    let mut files = transcripts(home);
    files.retain(|(_, uuid, _)| !held.contains(uuid));
    files.sort_by_key(|f| std::cmp::Reverse(f.2));
    let open = open_now(home);
    let mut out = Vec::new();
    for (path, uuid, modified) in files {
        if out.len() >= limit {
            break;
        }
        let Some((cwd, title)) = read(&path) else {
            continue;
        };
        if !Path::new(&cwd).is_dir() {
            continue;
        }
        out.push(Conversation {
            open: open.contains(&uuid),
            uuid,
            cwd,
            title,
            modified,
        });
    }
    out
}

/// Every conversation's transcript: its path, uuid and last write (ms since the epoch). A
/// subagent's transcript lies a folder deeper and is not one.
fn transcripts(home: &Path) -> Vec<(PathBuf, String, i64)> {
    let Ok(projects) = std::fs::read_dir(home.join(".claude/projects")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for project in projects.flatten() {
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Some(uuid) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
                continue;
            };
            let Ok(meta) = f.metadata() else {
                continue;
            };
            if !meta.is_file() || meta.len() == 0 {
                continue;
            }
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
            out.push((path, uuid, modified));
        }
    }
    out
}

/// The conversations a live Claude process has open: Claude Code notes each of its processes in
/// `~/.claude/sessions/<pid>.json` with its conversation, and a pid that is gone left its note
/// behind (as [`super::backfill::held`] reads it for one conversation).
fn open_now(home: &Path) -> HashSet<String> {
    let Ok(dir) = std::fs::read_dir(home.join(".claude/sessions")) else {
        return HashSet::new();
    };
    dir.flatten()
        .filter_map(|e| {
            let v: Value = serde_json::from_slice(&std::fs::read(e.path()).ok()?).ok()?;
            let pid = v["pid"].as_u64()?;
            Path::new(&format!("/proc/{pid}"))
                .exists()
                .then(|| v["sessionId"].as_str().map(str::to_owned))?
        })
        .collect()
}

/// A conversation's folder and title, from its transcript: the folder its first lines name, and
/// the title Claude Code (or `/rename`) last wrote, else the start of the first message typed.
/// `None` when no line names a folder: not a conversation to go on with.
fn read(path: &Path) -> Option<(String, String)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut head = Vec::new();
    f.by_ref().take(HEAD).read_to_end(&mut head).ok()?;
    let (mut cwd, mut first) = (None, None);
    for line in head.split(|b| *b == b'\n') {
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            continue; // the last line of the window may be cut
        };
        if cwd.is_none() {
            cwd = v["cwd"].as_str().map(str::to_owned);
        }
        if first.is_none() {
            first = typed(&v);
        }
        if cwd.is_some() && first.is_some() {
            break;
        }
    }
    let cwd = cwd?;
    let tail = if len > HEAD {
        let mut t = Vec::new();
        f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
        f.read_to_end(&mut t).ok()?;
        t
    } else {
        head
    };
    let title = tail
        .rsplit(|b| *b == b'\n')
        .find_map(title_line)
        .or(first)
        .unwrap_or_default();
    Some((cwd, title))
}

/// What the user typed, when `v` is such a line: a `user` line of the main thread that is not
/// the CLI's own (a command's echo, a caveat, a compaction's summary, a tool's result).
fn typed(v: &Value) -> Option<String> {
    let own = v["isSidechain"] == true || v["isMeta"] == true || v["isCompactSummary"] == true;
    if v["type"] != "user" || own {
        return None;
    }
    let text = match &v["message"]["content"] {
        Value::String(s) => s.clone(),
        c => c
            .as_array()?
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() || text.starts_with('<') {
        return None;
    }
    Some(match text.char_indices().nth(TITLE_CHARS) {
        Some((at, _)) => format!("{}…", text[..at].trim_end()),
        None => text,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    const A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
    const C: &str = "cccccccc-0000-4000-8000-000000000003";

    /// Writes a transcript of `lines` for `uuid`, last written `age` seconds ago.
    fn transcript(home: &Path, uuid: &str, age: u64, lines: &[Value]) -> PathBuf {
        let dir = home.join(".claude/projects/-work");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{uuid}.jsonl"));
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let at = SystemTime::now() - Duration::from_secs(age);
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(at).unwrap();
        path
    }

    fn user(cwd: &Path, text: &str) -> Value {
        serde_json::json!({"type": "user", "cwd": cwd, "message": {"role": "user", "content": text}})
    }

    #[test]
    fn lists_what_no_session_holds_newest_first_with_folder_title_and_whether_it_is_open() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let work = home.join("work");
        std::fs::create_dir_all(&work).unwrap();
        // titled by Claude Code, twice: the last one counts
        transcript(
            home,
            A,
            300,
            &[
                serde_json::json!({"type": "permission-mode", "permissionMode": "default"}),
                user(&work, "fix the login bug"),
                serde_json::json!({"type": "ai-title", "aiTitle": "Login"}),
                serde_json::json!({"type": "ai-title", "aiTitle": "Fix the login bug"}),
            ],
        );
        // no title: the first thing typed, not the command echo or the caveat before it
        let long = "word ".repeat(40);
        transcript(
            home,
            B,
            10,
            &[
                serde_json::json!({"type": "user", "cwd": work, "isMeta": true,
                    "message": {"role": "user", "content": "Caveat: the messages below"}}),
                user(&work, "<command-name>/model</command-name>"),
                serde_json::json!({"type": "user", "cwd": work, "message": {"role": "user",
                    "content": [{"type": "text", "text": long}]}}),
            ],
        );
        // held by an open session: not listed
        transcript(home, C, 1, &[user(&work, "mine already")]);
        // a subagent's transcript, an empty file, one with no folder, one whose folder is gone
        let sub = home.join(format!(".claude/projects/-work/{A}/subagents"));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("agent-1.jsonl"),
            format!("{}\n", user(&work, "sub")),
        )
        .unwrap();
        std::fs::write(home.join(".claude/projects/-work/empty.jsonl"), "").unwrap();
        transcript(
            home,
            "dddddddd-0000-4000-8000-000000000004",
            2,
            &[serde_json::json!({"type": "summary", "summary": "x"})],
        );
        transcript(
            home,
            "eeeeeeee-0000-4000-8000-000000000005",
            3,
            &[user(&home.join("gone"), "the folder was deleted")],
        );
        // this very process has A open; a note of a pid long gone says B
        let notes = home.join(".claude/sessions");
        std::fs::create_dir_all(&notes).unwrap();
        let note = |pid: u32, conv: &str| {
            let v = serde_json::json!({"pid": pid, "sessionId": conv});
            std::fs::write(notes.join(format!("{pid}.json")), v.to_string()).unwrap();
        };
        note(std::process::id(), A);
        note(u32::MAX - 7, B);

        let held = HashSet::from([C.to_owned()]);
        let got = list(home, &held, 10);
        let rows: Vec<_> = got
            .iter()
            .map(|c| (c.uuid.as_str(), c.title.as_str(), c.open))
            .collect();
        let cut = format!("{}…", "word ".repeat(16).trim_end());
        assert_eq!(
            rows,
            [(B, cut.as_str(), false), (A, "Fix the login bug", true)]
        );
        assert!(got.iter().all(|c| c.cwd == work.display().to_string()));
        assert!(got[0].modified > got[1].modified && got[1].modified > 0);
        // the newest one alone
        assert_eq!(list(home, &held, 1).len(), 1);
        assert_eq!(list(home, &held, 1)[0].uuid, B);
        // no Claude Code on the machine at all
        assert!(list(&home.join("nobody"), &held, 10).is_empty());
    }

    #[test]
    fn a_title_past_the_head_window_is_found_at_the_end() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let work = home.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let filler =
            serde_json::json!({"type": "assistant", "message": {"content": "x".repeat(4096)}});
        let mut lines = vec![user(&work, "start")];
        lines.extend(std::iter::repeat_n(filler, 100)); // ~400 KiB: past HEAD
        lines.push(serde_json::json!({"type": "custom-title", "customTitle": "Renamed by hand"}));
        transcript(home, A, 0, &lines);
        let got = list(home, &HashSet::new(), 5);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "Renamed by hand");
    }
}
