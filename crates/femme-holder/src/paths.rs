//! File locations. The holder and `femmed` must agree on these, so both use this module.
//!
//! ```text
//! <runtime_dir>/s/<sid>.sock        the holder's unix socket
//! <state_dir>/spool/<sid>.jsonl     the headless agent's stdout, append-only
//! <state_dir>/snap/<sid>.snap       last screens, see [`crate::snap`]
//! ```

use std::path::{Path, PathBuf};

use femme_proto::Sid;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// `$XDG_RUNTIME_DIR/femme`, or `None` when `XDG_RUNTIME_DIR` is unset.
pub fn default_runtime_dir() -> Option<PathBuf> {
    env_path("XDG_RUNTIME_DIR").map(|d| d.join("femme"))
}

/// `$XDG_STATE_HOME/femme`, else `~/.local/state/femme`, or `None` when neither base is known.
pub fn default_state_dir() -> Option<PathBuf> {
    env_path("XDG_STATE_HOME")
        .or_else(|| env_path("HOME").map(|h| h.join(".local/state")))
        .map(|d| d.join("femme"))
}

/// The holder's socket for session `sid`.
pub fn socket_path(runtime_dir: &Path, sid: Sid) -> PathBuf {
    runtime_dir.join("s").join(format!("{sid}.sock"))
}

/// The agent's stdout spool for session `sid`.
pub fn spool_path(state_dir: &Path, sid: Sid) -> PathBuf {
    state_dir.join("spool").join(format!("{sid}.jsonl"))
}

/// The last-screens snapshot for session `sid`.
pub fn snap_path(state_dir: &Path, sid: Sid) -> PathBuf {
    state_dir.join("snap").join(format!("{sid}.snap"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let sid: Sid = "01J00000000000000000000000".parse().unwrap();
        assert_eq!(
            socket_path(Path::new("/run/user/1000/femme"), sid),
            Path::new("/run/user/1000/femme/s/01J00000000000000000000000.sock")
        );
        assert_eq!(
            spool_path(Path::new("/st"), sid),
            Path::new("/st/spool/01J00000000000000000000000.jsonl")
        );
        assert_eq!(snap_path(Path::new("/st"), sid), Path::new("/st/snap/01J00000000000000000000000.snap"));
    }
}
