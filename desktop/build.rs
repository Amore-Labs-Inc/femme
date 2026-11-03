//! Stamps the binary with the commit it was built from: `env!("FEMME_COMMIT")`, a short sha.
//!
//! The sources, in order: the `FEMME_COMMIT` environment variable (the PC's updater builds from
//! `git archive`, so there is no `.git` to ask, and passes the commit it extracted), else
//! `git rev-parse --short HEAD`, else `dev`. The menu compares it with the commit the box runs.

use std::path::Path;
use std::process::Command;

/// A short sha is seven characters; a full one from the environment is cut to that.
const SHORT: usize = 7;

fn main() {
    println!("cargo:rerun-if-env-changed=FEMME_COMMIT");
    let commit = from_env().or_else(from_git).unwrap_or_else(|| "dev".into());
    println!("cargo:rustc-env=FEMME_COMMIT={commit}");
}

/// The environment's commit, when it is set and looks like one.
fn from_env() -> Option<String> {
    let v = std::env::var("FEMME_COMMIT").ok()?;
    let v = v.trim();
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(v[..v.len().min(SHORT)].to_ascii_lowercase());
    }
    // Not a sha (a tag, `local`): taken as it is when it is plain text.
    is_word(v).then(|| v.to_owned())
}

/// Letters, digits and `._-` only: it ends up in a `cargo:` line and in the UI.
fn is_word(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 32
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `git rev-parse --short=7 HEAD` in the crate's directory, and the files that move HEAD watched.
fn from_git() -> Option<String> {
    let sha = git(&["rev-parse", &format!("--short={SHORT}"), "HEAD"])?;
    watch_head();
    is_word(&sha).then_some(sha)
}

/// Rerun when HEAD moves: a branch switch changes `HEAD`, a commit on the branch changes the ref
/// file it names (or `packed-refs` after a gc). `--git-path` also finds them in a linked worktree,
/// where `.git` is a file and HEAD lives under the main repository's `worktrees/<name>`.
fn watch_head() {
    let mut paths = Vec::new();
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        if let Ok(text) = std::fs::read_to_string(&head) {
            if let Some(name) = text.trim().strip_prefix("ref: ") {
                paths.extend(git(&["rev-parse", "--git-path", name]));
            }
        }
        paths.push(head);
    }
    paths.extend(git(&["rev-parse", "--git-path", "packed-refs"]));
    for p in paths.iter().filter(|p| Path::new(p).exists()) {
        println!("cargo:rerun-if-changed={p}");
    }
}

/// One line of `git <args>`'s output, `None` when git is missing, fails or says nothing.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let line = text.trim();
    (out.status.success() && !line.is_empty()).then(|| line.to_owned())
}
