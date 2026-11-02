//! Regression tests for the audit findings that belong to no single driver: golden strictness,
//! complete lines and line-boundary resume offsets (19), the capture script's `~/.codex` guard
//! (20), the turn/status order and `ItemStarted.preview` docs (21) and fixture hygiene. Each test
//! says which finding it pins; `NOTES.md` has the story.
//!
//! Everything above the `NEW API` marker uses only what the crate had before the audit, so on the
//! unfixed crate those tests run and the ones that pin a fix fail on an assertion. The module
//! below needs `common::golden_with`, which did not exist.
mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::panic::catch_unwind;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{fixture_dir, replay, spool_lines};
use femme_agents::{driver_for, AgentEvent, AgentStatus, Out};
use femme_proto::state::{Agent, ItemKind};
use serde_json::{json, Value};

const AGENTS: [(Agent, &str); 3] = [
    (Agent::Claude, "claude"),
    (Agent::Codex, "codex"),
    (Agent::Pi, "pi"),
];

/// `(file name, bytes)` of every captured stdout spool of `dir`, sorted.
fn spools(dir: &str) -> Vec<(String, Vec<u8>)> {
    let mut names: Vec<String> = std::fs::read_dir(fixture_dir(dir))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".jsonl") && !n.ends_with(".stdin.jsonl"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|n| {
            let bytes = std::fs::read(fixture_dir(dir).join(&n)).unwrap();
            (n, bytes)
        })
        .collect()
}

fn tmp(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

// ---------------------------------------------------------------- golden strictness

/// A golden file that does not exist fails the test; it used to be written silently, so a new
/// case (or a renamed one) passed against itself. `UPDATE_GOLDEN=1` is the only way to write.
#[test]
fn a_missing_golden_fails_the_test_instead_of_being_created() {
    if std::env::var("UPDATE_GOLDEN").as_deref() == Ok("1") {
        return; // the run that is meant to write goldens
    }
    let path = tmp("regress-missing-golden.json");
    std::fs::remove_file(&path).ok();
    let r = catch_unwind(|| common::golden(&path, &json!({"a": 1})));
    let created = path.exists();
    std::fs::remove_file(&path).ok();
    assert!(
        r.is_err(),
        "a golden that does not exist must fail the test"
    );
    assert!(
        !created,
        "and must not be created behind the reviewer's back"
    );
}

// ---------------------------------------------------------------- complete lines (19)

/// `on_line` takes one complete line. A line with a `\n` inside is a caller bug (a split that
/// kept a partial line, or two lines glued together): debug builds assert on it, because the
/// resume offset would otherwise land in the middle of a line.
#[cfg(debug_assertions)]
#[test]
fn a_line_with_a_newline_in_it_trips_the_debug_assert() {
    for (agent, name) in AGENTS {
        let r = catch_unwind(move || {
            let mut d = driver_for(agent);
            d.on_line(0, b"{\"a\":1}\n{\"b\":2}", &mut Out::default());
        });
        assert!(
            r.is_err(),
            "{name}: no assertion on a line holding a newline"
        );
    }
}

/// After every line of every capture, `resume_offset()` is the start of a line (or the end of
/// the spool): the daemon replays from it, and a replay that starts mid-line reads garbage.
#[test]
fn resume_offset_is_always_the_start_of_a_line() {
    for (agent, dir) in AGENTS {
        for (name, bytes) in spools(dir) {
            assert!(bytes.ends_with(b"\n"), "{dir}/{name} ends with a newline");
            let lines = spool_lines(&bytes);
            let boundaries: BTreeSet<u64> = lines
                .iter()
                .map(|(o, _)| *o)
                .chain([bytes.len() as u64])
                .collect();
            let mut d = driver_for(agent);
            let mut out = Out::default();
            for (off, line) in lines {
                d.on_line(off, line, &mut out);
                out.take();
                let r = d.resume_offset();
                assert!(
                    boundaries.contains(&r),
                    "{dir}/{name}: resume_offset {r} after the line at {off} is inside a line"
                );
            }
        }
    }
}

// ---------------------------------------------------------------- docs pinned by the captures (21)

/// Which of `TurnStarted` and `Status(Working)` comes first in `events`.
fn opening(events: &[AgentEvent]) -> Option<&'static str> {
    let turn = events.iter().position(|e| *e == AgentEvent::TurnStarted)?;
    let work = events
        .iter()
        .position(|e| *e == AgentEvent::Status(AgentStatus::Working))?;
    Some(if work < turn {
        "Status(Working) first"
    } else {
        "TurnStarted first"
    })
}

/// `lib.rs` says: `Status(Working)` first when the agent had work queued before the turn began
/// (Claude's `queued` frame), `TurnStarted` first when the turn is the first sign of work (Claude
/// `system/status`, every Codex and Pi turn). The captures are the evidence.
#[test]
fn turns_open_in_the_order_lib_rs_documents() {
    let mut seen = BTreeMap::new();
    for (agent, dir) in AGENTS {
        for (name, bytes) in spools(dir) {
            let (events, _) = replay(&mut *driver_for(agent), &bytes);
            let Some(got) = opening(&events) else {
                continue;
            };
            let want = match (agent, name.as_str()) {
                (Agent::Claude, "usage.jsonl") => "TurnStarted first",
                (Agent::Claude, _) => "Status(Working) first",
                _ => "TurnStarted first",
            };
            assert_eq!(got, want, "{dir}/{name}");
            *seen.entry((dir, got)).or_insert(0) += 1;
        }
    }
    for key in [
        ("claude", "Status(Working) first"),
        ("claude", "TurnStarted first"),
        ("codex", "TurnStarted first"),
        ("pi", "TurnStarted first"),
    ] {
        assert!(seen.contains_key(&key), "no capture shows {key:?}");
    }
}

/// `ItemStarted.preview` is what the driver knows when the item opens: the tool's name (Claude,
/// Pi) or the command line (Codex command execution) for a call, the text so far for a Codex text
/// item, nothing for Claude and Pi text and thinking; never more than 512 bytes, and never "the
/// first bytes of the final text" for streamed text.
#[test]
fn item_started_preview_is_what_lib_rs_documents() {
    let mut checked = BTreeMap::new();
    for (agent, dir) in AGENTS {
        for (name, bytes) in spools(dir) {
            let (events, _) = replay(&mut *driver_for(agent), &bytes);
            let mut last_body: BTreeMap<&str, &Value> = BTreeMap::new();
            for e in &events {
                if let AgentEvent::ItemUpdated { id, body }
                | AgentEvent::ItemCompleted { id, body, .. } = e
                {
                    last_body.insert(id, body);
                }
            }
            for e in &events {
                let AgentEvent::ItemStarted { id, kind, preview } = e else {
                    continue;
                };
                let at = format!("{dir}/{name} {id}");
                assert!(preview.len() <= 512, "{at}");
                let body = last_body.get(id.as_str()).copied();
                match (agent, kind) {
                    (Agent::Codex, ItemKind::ToolCall) => {
                        if let Some(b) = body.filter(|b| b["name"] == "commandExecution") {
                            assert_eq!(b["input"]["command"], json!(preview), "{at}");
                            *checked.entry("codex command").or_insert(0) += 1;
                        }
                    }
                    (Agent::Codex, _) => {
                        if let Some(text) = body.and_then(|b| b["text"].as_str()) {
                            assert!(text.starts_with(preview.as_str()), "{at}: {preview:?}");
                            *checked.entry("codex text").or_insert(0) += 1;
                        }
                    }
                    (_, ItemKind::ToolCall) => {
                        if let Some(b) = body {
                            assert_eq!(b["name"], json!(preview), "{at}");
                            *checked.entry("tool name").or_insert(0) += 1;
                        }
                    }
                    (_, ItemKind::AssistantMessage | ItemKind::Thinking) => {
                        assert_eq!(preview, "", "{at}: text arrives as deltas");
                        *checked.entry("streamed text").or_insert(0) += 1;
                    }
                    _ => {}
                }
            }
        }
    }
    for what in ["codex command", "codex text", "tool name", "streamed text"] {
        assert!(
            checked.contains_key(what),
            "no capture checks {what}: {checked:?}"
        );
    }
}

/// A Codex command longer than 512 bytes is previewed cut on a char boundary, a prefix of the
/// command (`€` is 3 bytes; 512 falls inside one).
#[test]
fn a_long_codex_command_is_previewed_cut_on_a_char_boundary() {
    let command = "€".repeat(300);
    let line = json!({"method": "item/started", "params": {"threadId": "t1", "turnId": "u1",
        "item": {"type": "commandExecution", "id": "c1", "command": command, "cwd": "/w", "status": "inProgress"}}});
    let events =
        driver_for(Agent::Codex).on_line(0, line.to_string().as_bytes(), &mut Out::default());
    let preview = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ItemStarted { preview, .. } => Some(preview.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no ItemStarted in {events:?}"));
    assert_eq!(preview.len(), 510);
    assert!(command.starts_with(&preview));
}

// ---------------------------------------------------------------- fixture hygiene

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// The captures are scrubbed: no name of the machine they were taken on, the tools wrapped around
/// the agents there, or the account that paid for them. The tests do not depend on any of these
/// (they are what `scripts/capture-*.py` scrub).
#[test]
fn fixtures_name_no_machine_tool_or_account() {
    const BANNED: &[(&str, &str)] = &[
        (
            "herdr",
            "the terminal multiplexer the Codex capture ran under",
        ),
        (
            "rtk ",
            "the command rewriter in front of the hook fixtures' shell",
        ),
        (
            "/tmp/claude-",
            "the scratch directory of the capturing agent",
        ),
        ("Claude Team", "the account plan name"),
        ("darkwarro", "the developer's login"),
    ];
    let mut files = Vec::new();
    files_under(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures"),
        &mut files,
    );
    assert!(
        files.len() > 200,
        "found only {} fixture files",
        files.len()
    );
    let mut hits = Vec::new();
    for f in files {
        let text = String::from_utf8_lossy(&std::fs::read(&f).unwrap()).into_owned();
        for (needle, what) in BANNED {
            let n = text.matches(needle).count();
            if n > 0 {
                let rel = f.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap();
                hits.push(format!("{}: {n}x {needle:?} ({what})", rel.display()));
            }
        }
    }
    assert!(hits.is_empty(), "unscrubbed fixtures:\n{}", hits.join("\n"));
}

// ---------------------------------------------------------------- capture script guard (20)

/// Runs `CodexHomeGuard` of `scripts/capture-hooks.py` in scratch HOMEs; prints one line per
/// failed expectation and exits 1 if there is one. The script's contract: the user's
/// `~/.codex/config.toml` and `hooks.json` are copied to `<file>.femme-capture-bak` before the run;
/// afterwards the file is put back only when the run added nothing but lines the script owns (and
/// removed none), and the backup is deleted then; in every other case the file stays as it is and
/// the backup stays next to it; a backup left by an earlier crashed run stops the capture.
const GUARD_HARNESS: &str = r####"
import importlib.util, os, stat, sys, tempfile
from pathlib import Path

sys.dont_write_bytecode = True  # no scripts/__pycache__ from a test run
spec = importlib.util.spec_from_file_location("capture_hooks", sys.argv[1])
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

BAK = ".femme-capture-bak"
CWD = "/work/repo"
ORIG = 'model = "gpt-6-luna"\n\n[projects."/home/someone/else"]\ntrust_level = "trusted"\n'
OURS = '\n[projects."/work/repo"]\ntrust_level = "trusted"\n\n[tui]\nscreen_reader_detection_done = true\n'
HJ = '{\n  "hooks": {}\n}\n'
HJ2 = '{\n  "hooks": {"Stop": []}\n}\n'
fails = []

def check(name, ok, detail=""):
    if not ok:
        fails.append(f"{name}: {detail}")

def text(p):
    return p.read_text() if p.exists() else None

def run(config, during, hooks=None, stale=None):
    """One guarded run in a fresh HOME; what the files look like during and after it."""
    home = Path(tempfile.mkdtemp(prefix="femme-guard-"))
    os.environ["HOME"] = str(home)
    d = home / ".codex"
    d.mkdir()
    cfg, hj = d / "config.toml", d / "hooks.json"
    cbak, hbak = d / ("config.toml" + BAK), d / ("hooks.json" + BAK)
    if config is not None:
        cfg.write_text(config)
        os.chmod(cfg, 0o600)
    if hooks is not None:
        hj.write_text(hooks)
    if stale is not None:
        cbak.write_text(stale)
    r = {"raised": None}
    try:
        with mod.CodexHomeGuard(CWD):
            r["bak_during"] = text(cbak)
            r["hbak_during"] = text(hbak)
            during(cfg, hj)
    except RuntimeError as e:
        r["raised"] = str(e)
    r.update(config=text(cfg), hooks=text(hj), bak=text(cbak), hbak=text(hbak))
    r["mode"] = stat.S_IMODE(cfg.stat().st_mode) if cfg.exists() else None
    return r

r = run(ORIG, lambda c, h: c.write_text(ORIG + OURS), hooks=HJ)
check("ours added: backup of the original while the run goes", r["bak_during"] == ORIG, repr(r["bak_during"]))
check("ours added: hooks.json backed up too", r["hbak_during"] == HJ, repr(r["hbak_during"]))
check("ours added: file put back", r["config"] == ORIG, repr(r["config"]))
check("ours added: backup deleted", r["bak"] is None, repr(r["bak"]))
check("ours added: mode kept", r["mode"] == 0o600, oct(r["mode"] or 0))
check("ours added: untouched hooks.json leaves no backup", r["hbak"] is None and r["hooks"] == HJ, repr(r["hbak"]))

other = '\n[mcp_servers.x]\ncommand = "y"\n'
r = run(ORIG, lambda c, h: c.write_text(ORIG + other))
check("foreign table added: left as is", r["config"] == ORIG + other, repr(r["config"]))
check("foreign table added: original kept in the backup", r["bak"] == ORIG, repr(r["bak"]))

more = '\n[projects."/somewhere/else"]\ntrust_level = "trusted"\n'
r = run(ORIG, lambda c, h: c.write_text(ORIG + more))
check("other project added: left as is", r["config"] == ORIG + more, repr(r["config"]))
check("other project added: backup kept", r["bak"] == ORIG, repr(r["bak"]))

gone = ORIG.replace('model = "gpt-6-luna"\n', "")
r = run(ORIG, lambda c, h: c.write_text(gone))
check("line removed by someone: not rolled back", r["config"] == gone, repr(r["config"]))
check("line removed by someone: backup kept", r["bak"] == ORIG, repr(r["bak"]))

r = run(ORIG, lambda c, h: None)
check("untouched: same file", r["config"] == ORIG, repr(r["config"]))
check("untouched: no backup left", r["bak"] is None, repr(r["bak"]))

r = run(None, lambda c, h: c.write_text(OURS))
check("no file before: no backup invented", r["bak_during"] is None and r["bak"] is None, repr(r["bak"]))
check("no file before: what the run wrote stays", r["config"] == OURS, repr(r["config"]))

r = run(ORIG + OURS, lambda c, h: c.write_text("x"), stale=ORIG)
check("stale backup: the capture is refused", r["raised"] is not None, "no error")
check("stale backup: its content is intact", r["bak"] == ORIG, repr(r["bak"]))
check("stale backup: the config was not touched", r["config"] == ORIG + OURS, repr(r["config"]))

r = run(ORIG, lambda c, h: h.write_text(HJ2), hooks=HJ)
check("hooks.json changed: left as is", r["hooks"] == HJ2, repr(r["hooks"]))
check("hooks.json changed: original kept", r["hbak"] == HJ, repr(r["hbak"]))

if fails:
    print("\n".join(fails))
    sys.exit(1)
print("guard ok")
"####;

#[test]
fn the_capture_script_backs_up_and_only_restores_what_it_added() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/capture-hooks.py");
    let out = match Command::new("python3")
        .args(["-I", "-c", GUARD_HARNESS])
        .arg(&script)
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("skipped: no python3 ({e})");
            return;
        }
    };
    assert!(
        out.status.success(),
        "CodexHomeGuard of capture-hooks.py:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

// ==== NEW API: everything below needs `common::golden_with`, which did not exist ================

mod new_api {
    use super::*;

    /// `golden_with(.., true)` writes (pretty, trailing newline); `false` compares, and a mismatch
    /// leaves the golden alone, writes `*.actual.json` and fails.
    #[test]
    fn golden_with_writes_only_when_told_to_and_compares_otherwise() {
        let path = tmp("regress-golden-with.json");
        let actual = path.with_extension("actual.json");
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&actual).ok();
        common::golden_with(&path, &json!({"a": [1, 2]}), true);
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, "{\n  \"a\": [\n    1,\n    2\n  ]\n}\n");
        common::golden_with(&path, &json!({"a": [1, 2]}), false);
        let r = catch_unwind(|| common::golden_with(&path, &json!({"a": [1]}), false));
        assert!(r.is_err(), "a different value must fail");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            written,
            "golden untouched"
        );
        assert_eq!(
            std::fs::read_to_string(&actual).unwrap(),
            "{\n  \"a\": [\n    1\n  ]\n}\n"
        );
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&actual).ok();
    }

    #[test]
    fn golden_with_fails_on_a_missing_file_unless_updating() {
        let path = tmp("regress-golden-missing.json");
        std::fs::remove_file(&path).ok();
        let r = catch_unwind(|| common::golden_with(&path, &json!(1), false));
        assert!(r.is_err());
        assert!(!path.exists());
    }
}
