//! Which Claude Code is installed: what a chat's process is compared with, so an idle chat moves to
//! a newer one by itself (the terminal's `/restart`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::session::find_on_path;

/// How long an answer of `claude --version` is believed.
pub(crate) const FRESH: Duration = Duration::from_secs(600);
/// A `claude --version` that takes longer says nothing.
const ASK: Duration = Duration::from_secs(10);

/// What each `claude` said, by the file it resolves to: an install that moves a symlink is asked
/// at once, one that rewrites the file in place when the answer is older than `max_age`.
static SEEN: Mutex<BTreeMap<PathBuf, (Instant, Option<String>)>> = Mutex::new(BTreeMap::new());

/// The version of the `claude` on `path` (the agents' `PATH`), run with `env` (theirs too), from an
/// answer at most `max_age` old. `None`: no `claude`, or it did not say.
pub(crate) async fn installed(
    path: &str,
    env: &[(String, String)],
    max_age: Duration,
) -> Option<String> {
    let prog = find_on_path("claude", path)?;
    let file = std::fs::canonicalize(&prog).ok()?;
    let seen = SEEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&file)
        .cloned();
    if let Some((at, v)) = seen {
        if at.elapsed() < max_age {
            return v;
        }
    }
    let v = ask(&prog, env).await;
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    seen.insert(file, (Instant::now(), v.clone()));
    v
}

/// `2.1.331` of `2.1.331 (Claude Code)`.
async fn ask(prog: &str, env: &[(String, String)]) -> Option<String> {
    let run = tokio::process::Command::new(prog)
        .arg("--version")
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(ASK, run).await.ok()?.ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v = text.split_whitespace().next()?;
    (out.status.success() && v.starts_with(|c: char| c.is_ascii_digit())).then(|| v.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_installed_version_is_the_first_word_and_a_moved_link_is_asked_again() {
        let dir = tempfile::tempdir().unwrap();
        let tool = |name: &str, body: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
            p
        };
        let (old, new) = (
            tool("old", "echo '2.1.330 (Claude Code)'"),
            tool("new", "echo '2.1.331 (Claude Code)'"),
        );
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let path = bin.display().to_string();
        let env = [("PATH".to_string(), path.clone())];
        assert_eq!(installed(&path, &env, FRESH).await, None, "no claude");
        std::os::unix::fs::symlink(&old, bin.join("claude")).unwrap();
        assert_eq!(
            installed(&path, &env, FRESH).await.as_deref(),
            Some("2.1.330")
        );
        std::fs::remove_file(bin.join("claude")).unwrap();
        std::os::unix::fs::symlink(&new, bin.join("claude")).unwrap();
        assert_eq!(
            installed(&path, &env, FRESH).await.as_deref(),
            Some("2.1.331")
        );
        // rewritten in place: believed while fresh, asked again when not
        tool("new", "echo 'what'");
        assert_eq!(
            installed(&path, &env, FRESH).await.as_deref(),
            Some("2.1.331")
        );
        assert_eq!(installed(&path, &env, Duration::ZERO).await, None);
    }
}
