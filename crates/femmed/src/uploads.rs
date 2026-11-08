//! The files behind a prompt's attachments. A client uploads bytes with `BlobPut`; a `Prompt`
//! that names them gets them as files the agent can open, and its text says where.
//!
//! Each lands in `<state>/uploads/<sid>/<name>`, mode 600 in a folder of 700. The text the agent
//! gets (typed into a TUI, sent to a headless one) is the user's, then a blank line and one
//! `Attached: <absolute path>` line per file. A headless transcript's user message is the CLI's
//! echo of that text, so it shows the lines too. The files go when the session is purged
//! ([`Daemon::forget_uploads`]).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use femme_proto::rpc::Attachment;
use femme_proto::state::{Item, ItemKind, ItemStatus};
use femme_proto::Sid;
use femme_store::Store;
use ulid::Ulid;

use crate::core::{CoreError, PaneRef};
use crate::daemon::{core_err, Daemon};

/// Most attachments one prompt may carry.
pub const ATTACHMENTS_MAX: usize = 16;
/// Longest file name kept, in bytes: a filesystem takes 255, and `-2` may be added to it.
const NAME_MAX: usize = 200;
/// What an attachment is called when its own name is nothing once cleaned.
const NAME_FALLBACK: &str = "attachment";

impl Daemon {
    /// [`Core::prompt`](crate::core::Core::prompt) for a prompt with attachments: each blob is
    /// written to `<state>/uploads/<sid>/<name>` and the paths are appended to `text` as
    /// `Attached:` lines. `Bad` for more than [`ATTACHMENTS_MAX`] or for a blob that is not (or no
    /// longer) in the store, in which case nothing stays on disk from this call. Without
    /// attachments it is `prompt`.
    pub async fn prompt_attached(
        &self,
        pane: &PaneRef,
        text: &str,
        attachments: &[Attachment],
    ) -> Result<(), CoreError> {
        if attachments.is_empty() {
            return self.pane_prompt(pane, text).await;
        }
        if attachments.len() > ATTACHMENTS_MAX {
            let why = format!("at most {ATTACHMENTS_MAX} attachments to a prompt");
            return Err(CoreError::Bad(why));
        }
        let (session, _) = self.resolve(pane)?;
        let dir = self.cfg.state_path("uploads").join(session.id.to_string());
        let (store, list) = (self.store.clone(), attachments.to_vec());
        let paths = tokio::task::spawn_blocking(move || save_all(&store, &dir, &list))
            .await
            .map_err(|e| CoreError::Internal(format!("saving attachments: {e}")))??;
        // ponytail: the files stay when the prompt then fails (`Busy`, say): a retry makes
        // `name-2` beside them, and the session's purge sweeps them all. Deleting them would break
        // a prompt that was half delivered.
        self.pane_prompt(pane, &with_lines(text, &paths)).await
    }

    /// `Req::Show`: the agent in `sid` shows the user a file. The blob is saved in the session's
    /// uploads folder (so the desktop reads it like an attachment, and it goes with the session)
    /// and an assistant item carrying `caption` and the `Attached:` line is appended. `Bad` for an
    /// unknown session or a blob that is not in the store.
    pub async fn show(
        &self,
        sid: Sid,
        name: &str,
        blob: [u8; 32],
        caption: &str,
    ) -> Result<(), CoreError> {
        let known = self
            .store
            .read(|c| femme_store::sessions::get_session(c, sid))
            .map_err(core_err)?
            .is_some();
        if !known {
            return Err(CoreError::Bad(format!("no session {sid}")));
        }
        let dir = self.cfg.state_path("uploads").join(sid.to_string());
        let (store, name) = (self.store.clone(), file_name(name));
        let path = tokio::task::spawn_blocking(move || {
            let dir = std::path::absolute(&dir)
                .map_err(|e| CoreError::Internal(format!("uploads folder: {e}")))?;
            match store.get_blob(&blob) {
                Ok(Some(bytes)) => save(&dir, &name, &bytes)
                    .map_err(|e| CoreError::Internal(format!("cannot save {name}: {e}"))),
                Ok(None) => Err(CoreError::Bad("the upload is gone, send it again".into())),
                Err(e) => Err(core_err(e)),
            }
        })
        .await
        .map_err(|e| CoreError::Internal(format!("saving the file: {e}")))??;
        let text = with_lines(caption.trim(), std::slice::from_ref(&path));
        let preview = match caption.trim().lines().next() {
            Some(line) => line.to_owned(),
            None => format!(
                "shows {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
        };
        self.write_items(|tx, now, published| {
            let item = Item {
                sid,
                item_id: Ulid::generate().to_string(),
                seq: 0,
                rev: 0,
                kind: ItemKind::AssistantMessage,
                status: ItemStatus::Completed,
                preview,
                body: Some(serde_json::json!({ "text": text }).to_string()),
                blob_hash: None,
                ts: now,
                parent: None,
            };
            published.push(tx.put_item(&item)?);
            Ok(())
        })
    }

    /// Drops a purged session's uploads.
    pub(crate) fn forget_uploads(&self, sid: Sid) {
        let _ = fs::remove_dir_all(self.cfg.state_path("uploads").join(sid.to_string()));
    }
}

/// Writes each attachment's blob into `dir`, one at a time, and returns the paths in order. A
/// failure removes what this call wrote.
fn save_all(store: &Store, dir: &Path, list: &[Attachment]) -> Result<Vec<PathBuf>, CoreError> {
    let dir = std::path::absolute(dir)
        .map_err(|e| CoreError::Internal(format!("uploads folder: {e}")))?;
    let mut paths = Vec::with_capacity(list.len());
    for a in list {
        let name = file_name(&a.name);
        let saved = match store.get_blob(&a.blob) {
            Ok(Some(bytes)) => save(&dir, &name, &bytes)
                .map_err(|e| CoreError::Internal(format!("cannot save {name}: {e}"))),
            Ok(None) => {
                let why = format!("attachment {name}: its upload is gone, send it again");
                Err(CoreError::Bad(why))
            }
            Err(e) => Err(core_err(e)),
        };
        match saved {
            Ok(path) => paths.push(path),
            Err(e) => {
                for path in &paths {
                    let _ = fs::remove_file(path);
                }
                return Err(e);
            }
        }
    }
    Ok(paths)
}

/// Creates `<dir>/<name>` (the first of `name`, `stem-2.ext`, `stem-3.ext`.. that is free) with
/// `bytes`, mode 600, and returns its path. `name` must already be a [`file_name`].
fn save(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let mut n = 1;
    loop {
        let path = dir.join(numbered(name, n));
        // `create_new` is O_EXCL: it never follows a link someone left at that name.
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                if let Err(e) = file.write_all(bytes) {
                    let _ = fs::remove_file(&path);
                    return Err(e);
                }
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e),
        }
    }
}

/// `name` for the `n`th try: itself, then `stem-2.ext`, `stem-3.ext` (the extension stays last:
/// it is what tells an agent an attachment is an image).
fn numbered(name: &str, n: u32) -> String {
    if n == 1 {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}-{n}.{ext}"),
        None => format!("{name}-{n}"),
    }
}

/// The text with a blank line and an `Attached:` line per path after it (just the lines when the
/// text is empty).
fn with_lines(text: &str, paths: &[PathBuf]) -> String {
    let lines: Vec<String> = paths
        .iter()
        .map(|p| format!("Attached: {}", p.display()))
        .collect();
    match text.trim_end() {
        "" => lines.join("\n"),
        text => format!("{text}\n\n{}", lines.join("\n")),
    }
}

/// An attachment's name as a file name: the last path component (a client may send a whole path,
/// with either separator), control and invisible characters turned into `_`, leading and trailing
/// dots and spaces dropped (no hidden file, no `.` or `..`), the end kept when it is too long (that
/// is the extension), and `attachment` when nothing is left.
pub(crate) fn file_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let plain: String = last
        .chars()
        .map(|c| if unprintable(c) { '_' } else { c })
        .collect();
    let kept = tail(&plain, NAME_MAX).trim_matches(|c: char| c == '.' || c.is_whitespace());
    if kept.is_empty() {
        NAME_FALLBACK.into()
    } else {
        kept.into()
    }
}

/// Control characters and the invisible ones that reorder or hide text (a right-to-left override
/// can make `gpj.exe` read as `exe.jpg`).
fn unprintable(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}'
        )
}

/// The last `max` bytes of `s`, from a character boundary.
fn tail(s: &str, max: usize) -> &str {
    let mut start = s.len().saturating_sub(max);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn a_name_becomes_one_plain_file_name() {
        for (given, want) in [
            ("photo.png", "photo.png"),
            ("my photo (1).png", "my photo (1).png"),
            ("naïve été.txt", "naïve été.txt"),
            ("/etc/passwd", "passwd"),
            ("../../x.sh", "x.sh"),
            ("C:\\Users\\me\\pic.png", "pic.png"),
            ("dir/", NAME_FALLBACK),
            (".env", "env"),
            ("...hidden.txt", "hidden.txt"),
            (".", NAME_FALLBACK),
            ("..", NAME_FALLBACK),
            (". .", NAME_FALLBACK),
            ("", NAME_FALLBACK),
            ("   ", NAME_FALLBACK),
            ("a\nb\0c\u{7f}d.txt", "a_b_c_d.txt"),
            ("invoice\u{202e}gpj.exe", "invoice_gpj.exe"),
            ("report.pdf.", "report.pdf"),
        ] {
            assert_eq!(file_name(given), want, "{given:?}");
        }
    }

    #[test]
    fn a_long_name_keeps_its_end_on_a_character_boundary() {
        let long = format!("{}.png", "é".repeat(300));
        let name = file_name(&long);
        assert!(name.len() <= NAME_MAX, "{}", name.len());
        assert!(name.ends_with("é.png"), "{name}");
        let dotted = format!("{}.png", "x".repeat(NAME_MAX));
        let name = file_name(&dotted);
        assert_eq!(name.len(), NAME_MAX, "{name}");
        assert!(name.ends_with("x.png"), "{name}");
    }

    #[test]
    fn a_taken_name_gets_a_number_before_its_extension() {
        assert_eq!(numbered("a.png", 1), "a.png");
        assert_eq!(numbered("a.png", 2), "a-2.png");
        assert_eq!(numbered("a.tar.gz", 3), "a.tar-3.gz");
        assert_eq!(numbered("notes", 2), "notes-2");
    }

    #[test]
    fn files_are_private_and_never_overwrite_one_another() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("uploads").join("sid");
        let first = save(&dir, "a.png", b"one").unwrap();
        let second = save(&dir, "a.png", b"two").unwrap();
        let third = save(&dir, "a.png", b"three").unwrap();
        let other = save(&dir, "b", b"").unwrap();
        assert_eq!(first, dir.join("a.png"));
        assert_eq!(second, dir.join("a-2.png"));
        assert_eq!(third, dir.join("a-3.png"));
        assert_eq!(other, dir.join("b"));
        assert_eq!(fs::read(&first).unwrap(), b"one");
        assert_eq!(fs::read(&second).unwrap(), b"two");
        assert_eq!(fs::read(&other).unwrap(), b"");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&first), mode(&dir)), (0o600, 0o700));
        assert_eq!(mode(dir.parent().unwrap()), 0o700);
        // a link left at the name is not followed
        std::os::unix::fs::symlink("/nonexistent/target", dir.join("c.txt")).unwrap();
        let fourth = save(&dir, "c.txt", b"x").unwrap();
        assert_eq!(fourth, dir.join("c-2.txt"));
        assert!(!Path::new("/nonexistent/target").exists());
    }

    #[test]
    fn the_prompt_says_where_each_file_is() {
        let paths = [PathBuf::from("/s/u/a.png"), PathBuf::from("/s/u/b c.txt")];
        assert_eq!(
            with_lines("look at these\n", &paths),
            "look at these\n\nAttached: /s/u/a.png\nAttached: /s/u/b c.txt"
        );
        assert_eq!(with_lines("", &paths[..1]), "Attached: /s/u/a.png");
        assert_eq!(with_lines("  \n", &paths[..1]), "Attached: /s/u/a.png");
    }

    #[test]
    fn a_missing_blob_fails_the_call_and_leaves_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_memory().unwrap();
        let there = store.write(|tx| tx.put_blob_bytes(b"data")).unwrap();
        let list = |blob| Attachment {
            name: "x.txt".into(),
            blob,
        };
        let dir = tmp.path().join("u");
        let err = save_all(&store, &dir, &[list(there), list([9; 32])]).unwrap_err();
        assert!(
            matches!(&err, CoreError::Bad(m) if m.contains("x.txt")),
            "{err:?}"
        );
        assert!(
            !dir.join("x.txt").exists(),
            "the first one was removed again"
        );
        let ok = save_all(&store, &dir, &[list(there), list(there)]).unwrap();
        assert_eq!(ok, [dir.join("x.txt"), dir.join("x-2.txt")]);
    }
}
