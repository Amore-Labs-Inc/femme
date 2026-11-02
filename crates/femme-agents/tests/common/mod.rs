//! Shared test helpers (golden files, spool replay). Included with `mod common;` from each test file.
#![allow(dead_code)]

pub mod live;

use std::path::{Path, PathBuf};

use femme_agents::{AgentEvent, Driver, Out};

/// `crates/femme-agents/fixtures/<agent>/`.
pub fn fixture_dir(agent: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(agent)
}

/// Split a spool file into `(offset, line)` pairs, offsets absolute, lines without `\n`.
pub fn spool_lines(bytes: &[u8]) -> Vec<(u64, &[u8])> {
    let mut v = Vec::new();
    let mut start = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            v.push((start as u64, &bytes[start..i]));
            start = i + 1;
        }
    }
    if start < bytes.len() {
        v.push((start as u64, &bytes[start..]));
    }
    v
}

/// Feed every line of `bytes` through `d`; returns all events and everything the driver wrote.
pub fn replay(d: &mut dyn Driver, bytes: &[u8]) -> (Vec<AgentEvent>, Vec<u8>) {
    let mut out = Out::default();
    let mut events = Vec::new();
    for (off, line) in spool_lines(bytes) {
        events.extend(d.on_line(off, line, &mut out));
    }
    (events, out.stdin)
}

/// Compare `got` against the golden JSON file `path` (pretty, trailing newline). The file is
/// (re)written only when `UPDATE_GOLDEN=1`; review the diff by eye. A missing golden file fails
/// the test: a silently created golden would make any new case pass against itself.
pub fn golden(path: &Path, got: &impl serde::Serialize) {
    golden_with(
        path,
        got,
        std::env::var("UPDATE_GOLDEN").as_deref() == Ok("1"),
    );
}

/// [`golden`] with the `UPDATE_GOLDEN=1` switch passed in, so it can be tested.
pub fn golden_with(path: &Path, got: &impl serde::Serialize, update: bool) {
    let mut text = serde_json::to_string_pretty(got).expect("serialize golden");
    text.push('\n');
    if update {
        std::fs::write(path, &text).expect("write golden");
        eprintln!("wrote golden {}", path.display());
        return;
    }
    let want = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "golden file {} is missing or unreadable ({e}); run with UPDATE_GOLDEN=1 to create it, then review it",
            path.display()
        )
    });
    if want != text {
        let tmp = path.with_extension("actual.json");
        std::fs::write(&tmp, &text).ok();
        panic!(
            "golden mismatch for {}: actual written to {} (UPDATE_GOLDEN=1 to accept)",
            path.display(),
            tmp.display()
        );
    }
}
