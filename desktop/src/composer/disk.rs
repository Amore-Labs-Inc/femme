//! Drafts between runs: what was typed for a session, in `$XDG_STATE_HOME/femme/drafts/<sid>.txt`
//! (else `~/.local/state/femme/drafts/`), written whole on every change and removed once the text
//! is gone, so a quit or a crash loses nothing and a sent prompt leaves no file behind. The text
//! only: attachments are already uploaded blobs and are made again by pasting.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use femme_proto::Sid;

/// The directory, from the two environment variables that decide it: an absolute
/// `XDG_STATE_HOME`, else `HOME`/.local/state, then `femme/drafts`. None when neither is usable.
pub fn dir_from(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let usable = |v: Option<OsString>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    let state =
        usable(xdg_state_home).or_else(|| usable(home).map(|h| h.join(".local").join("state")))?;
    Some(state.join("femme").join("drafts"))
}

/// Where this process keeps its drafts (see [`dir_from`], with the real environment).
pub fn default_dir() -> Option<PathBuf> {
    dir_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

/// The session's file under `dir`.
pub fn file(dir: &Path, sid: Sid) -> PathBuf {
    dir.join(format!("{sid}.txt"))
}

/// The text saved for `sid`, when there is some.
pub fn load(dir: &Path, sid: Sid) -> Option<String> {
    std::fs::read_to_string(file(dir, sid))
        .ok()
        .filter(|t| !t.is_empty())
}

/// Keeps `text` for `sid`: written whole through a temporary file renamed into place, so a crash
/// mid-write leaves the previous text; an empty `text` removes the file instead.
pub fn save(dir: &Path, sid: Sid, text: &str) -> io::Result<()> {
    let path = file(dir, sid);
    if text.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        };
    }
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("femme-drafts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_directory_follows_xdg_then_home() {
        let s = |v: &str| Some(OsString::from(v));
        assert_eq!(
            dir_from(s("/st"), s("/home/u")),
            Some(PathBuf::from("/st/femme/drafts"))
        );
        assert_eq!(
            dir_from(None, s("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/femme/drafts"))
        );
        // A relative XDG_STATE_HOME is ignored, as the spec says.
        assert_eq!(
            dir_from(s("rel"), s("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/femme/drafts"))
        );
        assert_eq!(dir_from(None, None), None);
    }

    #[test]
    fn text_comes_back_after_a_save_and_an_empty_save_removes_it() {
        let dir = scratch("roundtrip");
        let sid = femme_proto::Id(7);
        assert_eq!(load(&dir, sid), None);
        save(&dir, sid, "half a thought").unwrap();
        assert_eq!(load(&dir, sid).as_deref(), Some("half a thought"));
        save(&dir, sid, "half a thought, then more").unwrap();
        assert_eq!(
            load(&dir, sid).as_deref(),
            Some("half a thought, then more")
        );
        assert!(!file(&dir, sid).with_extension("tmp").exists());
        // Another session's draft is its own file.
        assert_eq!(load(&dir, femme_proto::Id(8)), None);
        save(&dir, sid, "").unwrap();
        assert_eq!(load(&dir, sid), None);
        assert!(!file(&dir, sid).exists());
        // Removing what is not there is fine (a session that never typed).
        save(&dir, sid, "").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
