//! Small pure helpers of the compat layer, moved from `board/src/main.rs` (with their tests, in
//! `tests.rs`): validation of everything that ends up on a command line, the key allowlist, the
//! constant-time compare, file helpers.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use femme_proto::rpc::IssueField;
use femme_proto::state::IssueStatus;
use serde_json::Value;

use crate::config::Config;

pub(crate) const STATUSES: &[&str] = &[
    "open", "working", "waiting", "paused", "pr", "done", "failed",
];
/// The fields `POST`/`PATCH /api/issues` take. `pane` and `resume_at` are still accepted from old
/// clients (the retired worker.sh patched them) but the daemon owns both now: they are checked and dropped.
pub(crate) const PATCHABLE: &[&str] = &[
    "repo",
    "title",
    "body",
    "status",
    "pr_url",
    "note",
    "pane",
    "resume_at",
];
/// 9999-12-31: `resume_at` past this is a typo, and `* 1000` must not overflow.
const MAX_UNIX_SECS: u64 = 253_402_300_799;

/// What the 400s for a bad agent name say.
pub(crate) const NAME_RULE: &str = "name: a-z, 0-9 and -, up to 32 characters";

/// A locked mutex even when a panicking thread poisoned it (the maps here stay consistent).
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A command for an external tool (`git`, `gh`, `claude`, `cswap`, `aws`), with `Config::tool_path`
/// as its `PATH` when set (tests point it at fake tools).
pub(crate) fn tool(cfg: &Config, program: &str) -> Command {
    let mut cmd = Command::new(program);
    if let Some(path) = &cfg.tool_path {
        cmd.env("PATH", path);
    }
    cmd
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

pub(crate) fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

pub(crate) fn random_hex() -> String {
    let mut b = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("no /dev/urandom");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Compares tokens in the same time wherever they first differ.
pub(crate) fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .fold(0u8, |acc, (x, y)| std::hint::black_box(acc | (x ^ y)))
            == 0
}

/// What an API key may do: its own standing agents, and keys into a pane (answering a prompt).
/// Not issues, accounts, uploads, other terminals or the page.
pub(crate) fn api_route(method: &str, parts: &[&str]) -> bool {
    matches!(
        (method, parts),
        ("GET", ["api", "ping" | "openapi.json" | "terminals"])
            | ("GET" | "POST" | "DELETE", ["api", "agents", _])
            | ("POST", ["api", "agents", _, "push" | "discard"])
            | ("GET", ["api", "terminals", _])
            | ("POST", ["api", "terminals", _, "prompt" | "input"])
    )
}

/// An agent name the caller picks: the session's name, and a folder name under femme-work.
pub(crate) fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
}

/// A branch name passed on to git in the brief: `main`, `release/2.1`; never an option.
pub(crate) fn valid_branch(s: &str) -> bool {
    !s.is_empty()
        && s.len() < 100
        && !s.starts_with('-')
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

pub(crate) fn valid_repo(s: &str) -> bool {
    let ok = |p: &str| {
        !p.is_empty()
            && !p.starts_with(['.', '-'])
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    matches!(s.split_once('/'), Some((o, n)) if ok(o) && ok(n))
}

/// A GitHub token as GitHub issues them (ghp_…, github_pat_…, gho_…): letters, digits and underscores.
pub(crate) fn valid_token(s: &str) -> bool {
    (20..=255).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// gh's device code in its output: `! First copy your one-time code: 24E9-601B`.
pub(crate) fn device_code(out: &str) -> Option<String> {
    let code: String = out[out.find("one-time code: ")? + 15..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    (code.len() >= 8).then_some(code)
}

/// The clone URL for a key's own token: GitHub takes any username with a token as the password.
pub(crate) fn clone_url(repo: &str, token: &str) -> String {
    if token.is_empty() {
        format!("https://github.com/{repo}.git")
    } else {
        format!("https://x-access-token:{token}@github.com/{repo}.git")
    }
}

pub(crate) fn noreply(id: u64, login: &str) -> String {
    format!("{id}+{login}@users.noreply.github.com")
}

/// The sign-in link in a login command's output, which may wrap it in a terminal hyperlink.
pub(crate) fn login_url(out: &str) -> Option<String> {
    let url: String = out[out.find("https://")?..]
        .chars()
        .take_while(|c| !c.is_whitespace() && !c.is_control())
        .collect();
    url.contains("/authorize?").then_some(url)
}

/// The code a sign-in page shows. Copying it can pick up line breaks, so whitespace goes; it is
/// written to stdin, so no control bytes.
pub(crate) fn sign_in_code(s: &str) -> Option<String> {
    let code: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (!code.is_empty() && code.len() <= 16_000 && !code.chars().any(char::is_control))
        .then_some(code)
}

pub(crate) fn last_line(s: &str) -> Option<&str> {
    s.lines().map(str::trim).rfind(|l| !l.is_empty())
}

/// An existing directory inside home, resolved (no `..` escapes, no symlinks out).
pub(crate) fn home_dir(home: &Path, dir: &str) -> Option<String> {
    let dir = if dir.trim().is_empty() {
        home.to_path_buf()
    } else {
        PathBuf::from(dir.trim())
    };
    let real = fs::canonicalize(dir).ok()?;
    (real.is_dir() && real.starts_with(fs::canonicalize(home).ok()?))
        .then(|| real.display().to_string())
}

pub(crate) fn clean_name(name: &str) -> String {
    let name: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
        .collect();
    let name = name.trim_start_matches('.');
    // ASCII only by now, so slicing bytes is safe; the end is kept for the extension.
    if name.is_empty() {
        "paste".into()
    } else {
        name[name.len().saturating_sub(60)..].to_string()
    }
}

pub(crate) fn status_str(s: IssueStatus) -> &'static str {
    match s {
        IssueStatus::Open => "open",
        IssueStatus::Working => "working",
        IssueStatus::Waiting => "waiting",
        IssueStatus::Paused => "paused",
        IssueStatus::Pr => "pr",
        IssueStatus::Done => "done",
        IssueStatus::Failed => "failed",
    }
}

fn parse_status(s: &str) -> Option<IssueStatus> {
    Some(match s {
        "open" => IssueStatus::Open,
        "working" => IssueStatus::Working,
        "waiting" => IssueStatus::Waiting,
        "paused" => IssueStatus::Paused,
        "pr" => IssueStatus::Pr,
        "done" => IssueStatus::Done,
        "failed" => IssueStatus::Failed,
        _ => return None,
    })
}

/// Checks the fields of a `POST`/`PATCH /api/issues` body and turns them into the store's per-field
/// patch, validating the ones the scheduler later passes to git/gh, since anything typed on the
/// board ends up on a command line. Strings are trimmed; an empty `note`/`pr_url` clears it.
pub(crate) fn apply(input: &Value) -> Result<Vec<IssueField>, String> {
    let Some(fields) = input.as_object() else {
        return Err("expected a JSON object".into());
    };
    let mut out = Vec::new();
    for (k, v) in fields {
        if !PATCHABLE.contains(&k.as_str()) {
            return Err(format!("unknown field {k}"));
        }
        match k.as_str() {
            "repo" if !valid_repo(v.as_str().unwrap_or("")) => {
                return Err("repo must look like owner/name".into())
            }
            "status" if !STATUSES.contains(&v.as_str().unwrap_or("")) => {
                return Err(format!("status must be one of {}", STATUSES.join(", ")))
            }
            "resume_at" if !v.is_null() && v.as_u64().is_none_or(|s| s > MAX_UNIX_SECS) => {
                return Err("resume_at must be unix seconds".into())
            }
            "resume_at" => {}
            _ if !v.is_string() => return Err(format!("{k} must be a string")),
            _ => {}
        }
        let s = v.as_str().map(str::trim).unwrap_or("");
        let optional = || (!s.is_empty()).then(|| s.to_string());
        match k.as_str() {
            "repo" => out.push(IssueField::Repo(s.into())),
            "title" => out.push(IssueField::Title(s.into())),
            "body" => out.push(IssueField::Body(s.into())),
            "status" => out.extend(parse_status(s).map(IssueField::Status)),
            "pr_url" => out.push(IssueField::PrUrl(optional())),
            "note" => out.push(IssueField::Note(optional())),
            "resume_at" => out.push(IssueField::ResumeAt(v.as_i64().map(|s| s * 1000))),
            // ponytail: `pane` was worker.sh's session link; the daemon links sessions itself, so a
            // client that still sends it is told yes and ignored.
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn apply_turns_fields_into_a_patch() {
        let got = apply(&json!({"status": "pr", "pr_url": " https://x ", "note": "", "title": " t ", "pane": "w1:p1", "resume_at": null})).unwrap();
        assert!(got.contains(&IssueField::Status(IssueStatus::Pr)));
        assert!(got.contains(&IssueField::PrUrl(Some("https://x".into()))));
        assert!(got.contains(&IssueField::Note(None)));
        assert!(got.contains(&IssueField::Title("t".into())));
        assert_eq!(
            apply(&json!({"resume_at": 1700000000})).unwrap(),
            vec![IssueField::ResumeAt(Some(1_700_000_000_000))]
        );
        assert_eq!(
            apply(&json!({"resume_at": null})).unwrap(),
            vec![IssueField::ResumeAt(None)]
        );
        assert!(apply(&json!({"resume_at": 253_402_300_800u64})).is_err());
        assert_eq!(got.len(), 5, "pane is dropped, resume_at kept");
    }

    #[test]
    fn statuses_round_trip() {
        for s in STATUSES {
            assert_eq!(status_str(parse_status(s).unwrap()), *s);
        }
        assert_eq!(parse_status("hacked"), None);
    }
}
