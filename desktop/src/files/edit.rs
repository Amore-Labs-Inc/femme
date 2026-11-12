//! Editing a text file on the viewer page: the bytes as read, in gpui-component's code editor,
//! saved with Ctrl-S through `Req::FsWrite` (replacing the file, with the hash of what was read,
//! so a file Claude or a terminal changed meanwhile is not written over unasked). The panel owns
//! one of these per open file, draws it with [`Editing::element`], and runs [`Editing::save`].

use std::path::Path;

use femme_proto::rpc::{Req, Resp, MAX_FS_WRITE};
use femme_proto::Sid;
use gpui::{AppContext as _, AsyncApp, Context, Entity, IntoElement, Subscription, Task, Window};
use gpui_component::input::{Editor, EditorState};

use super::source::{ask, Source};
use super::tree::clean;
use super::FilesPanel;

/// What a save ended in.
#[derive(Debug, PartialEq)]
pub enum Saved {
    /// Written whole; the file's hash now.
    Ok([u8; 32]),
    /// The file on disk is not the one read: nothing written.
    Conflict,
    /// Not written, and why.
    Failed(String),
}

/// A file open for editing.
pub struct Editing {
    /// Relative to the session's directory, as the tree names it.
    pub path: String,
    state: Entity<EditorState>,
    /// sha256 of the file as read, or as last saved.
    was: [u8; 32],
    /// The text as read, or as last saved: the editor differing from it is `modified`.
    clean: String,
    /// A save is on its way.
    pub saving: bool,
    /// A save was refused: the file changed on disk since it was read.
    pub conflict: bool,
    /// What the last save ran into, until the next one or a keystroke.
    pub error: Option<String>,
    _watch: Subscription,
}

impl Editing {
    /// `None` when the bytes are not UTF-8 text.
    pub fn new(
        path: String,
        data: &[u8],
        window: &mut Window,
        cx: &mut Context<FilesPanel>,
    ) -> Option<Editing> {
        let text = std::str::from_utf8(data).ok()?.to_owned();
        let lang = language(&path);
        let state = cx.new(|cx| {
            let mut s = EditorState::new(window, cx).language(lang);
            s.set_value(text.clone(), window, cx);
            s
        });
        // every keystroke redraws the panel: the modified mark and the save button follow
        let watch = cx.observe(&state, |_, _, cx| cx.notify());
        Some(Editing {
            path,
            state,
            was: hash(data),
            clean: text,
            saving: false,
            conflict: false,
            error: None,
            _watch: watch,
        })
    }

    /// The editor's text now.
    pub fn text(&self, cx: &gpui::App) -> String {
        self.state.read(cx).text().to_string()
    }

    /// Whether the editor holds something else than what was read or last saved.
    // ponytail: a whole-text compare per redraw; a dirty flag from the editor's events if a
    // 10 MiB file ever makes typing lag
    pub fn modified(&self, cx: &gpui::App) -> bool {
        let now = self.state.read(cx).text();
        now.len() != self.clean.len() || *now != self.clean
    }

    /// Gives the editor the keyboard.
    pub fn focus(&self, window: &mut Window, cx: &mut gpui::App) {
        self.state.update(cx, |s, cx| s.focus(window, cx));
    }

    /// The editor, to be placed in the viewer page's body (it scrolls by itself).
    pub fn element(&self) -> impl IntoElement {
        Editor::new(&self.state)
            .appearance(false)
            .bordered(false)
            .h(gpui::relative(1.))
    }

    /// Writes the editor's text over the file, in pieces of `MAX_FS_WRITE`. With `overwrite` the
    /// file as it is on disk is not checked against what was read (the answer to a conflict).
    /// The task's answer goes to [`Editing::finish`] with the text that was saved.
    pub fn save(
        &mut self,
        source: &Source,
        sid: Option<Sid>,
        overwrite: bool,
        cx: &mut Context<FilesPanel>,
    ) -> Task<(String, Saved)> {
        self.saving = true;
        self.error = None;
        let text = self.text(cx);
        let path = self.path.clone();
        let was = (!overwrite).then_some(self.was);
        let source = source.clone();
        cx.spawn(async move |_, cx| {
            let r = write(&source, sid, &path, text.as_bytes(), was, cx).await;
            (text, r)
        })
    }

    /// Takes a save's outcome: the saved text is the clean one on success.
    pub fn finish(&mut self, text: String, r: Saved) {
        self.saving = false;
        match r {
            Saved::Ok(hash) => {
                self.was = hash;
                self.clean = text;
                self.conflict = false;
            }
            Saved::Conflict => self.conflict = true,
            Saved::Failed(why) => self.error = Some(why),
        }
    }
}

/// `data` over `path`, piece by piece; `was` makes the daemon refuse a file changed since.
async fn write(
    source: &Source,
    sid: Option<Sid>,
    path: &str,
    data: &[u8],
    was: Option<[u8; 32]>,
    cx: &mut AsyncApp,
) -> Saved {
    match source {
        Source::Offline => Saved::Failed("not connected to a daemon".into()),
        Source::Fixture(root) => {
            let (root, path, data) = (root.clone(), path.to_owned(), data.to_vec());
            cx.background_executor()
                .spawn(async move { fixture_write(&root, &path, &data, was) })
                .await
        }
        Source::Daemon(net) => {
            let Some(sid) = sid else {
                return Saved::Failed("no session".into());
            };
            let total = data.len() as u64;
            let mut offset = 0u64;
            // an empty file is one empty piece
            loop {
                let end = (offset as usize + MAX_FS_WRITE).min(data.len());
                let req = Req::FsWrite {
                    sid,
                    path: path.to_owned(),
                    offset,
                    total,
                    data: data[offset as usize..end].to_vec(),
                    replace: true,
                    was,
                };
                match ask(net, req, cx).await {
                    Ok(Resp::FsWritten { done: true, .. }) => return Saved::Ok(hash(data)),
                    Ok(Resp::FsWritten { written, .. }) => offset = written,
                    Ok(_) => return Saved::Failed("the daemon answered something else".into()),
                    Err(why) if why.contains("changed on disk") => return Saved::Conflict,
                    Err(why) => return Saved::Failed(why),
                }
                if end >= data.len() && offset >= total {
                    return Saved::Ok(hash(data));
                }
            }
        }
    }
}

/// The fixture's save: the local file, with the same conflict rule as the daemon's.
fn fixture_write(root: &Path, rel: &str, data: &[u8], was: Option<[u8; 32]>) -> Saved {
    if !clean(rel) {
        return Saved::Failed(format!("{rel:?} is not a path under the directory"));
    }
    let file = root.join(rel);
    if let Some(was) = was {
        match std::fs::read(&file) {
            Ok(now) if hash(&now) == was => {}
            _ => return Saved::Conflict,
        }
    }
    let part = file.with_extension(match file.extension() {
        Some(e) => format!("{}.part", e.to_string_lossy()),
        None => "part".to_owned(),
    });
    let r = std::fs::write(&part, data).and_then(|_| std::fs::rename(&part, &file));
    match r {
        Ok(()) => Saved::Ok(hash(data)),
        Err(e) => Saved::Failed(e.to_string()),
    }
}

fn hash(data: &[u8]) -> [u8; 32] {
    <sha2::Sha256 as sha2::Digest>::digest(data).into()
}

/// The highlighter's name for the file, by extension; plain text for the rest.
pub fn language(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" | "mts" => "typescript",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "markdown" => "markdown",
        "html" | "htm" => "html",
        "css" => "css",
        "sh" | "bash" | "zsh" => "bash",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" => "cpp",
        "go" => "go",
        "sql" => "sql",
        _ => "plaintext",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_language_follows_the_extension() {
        assert_eq!(language("src/main.rs"), "rust");
        assert_eq!(language("notes.md"), "markdown");
        assert_eq!(language("Makefile"), "plaintext");
        assert_eq!(language("a.tar.gz"), "plaintext");
    }

    #[test]
    fn the_fixture_save_keeps_the_conflict_rule() {
        let dir = std::env::temp_dir().join(format!("femme-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"one").unwrap();
        let was = hash(b"one");
        // as read: saved
        assert_eq!(
            fixture_write(&dir, "a.txt", b"two", Some(was)),
            Saved::Ok(hash(b"two"))
        );
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"two");
        // changed meanwhile: refused, the file kept
        assert_eq!(
            fixture_write(&dir, "a.txt", b"three", Some(was)),
            Saved::Conflict
        );
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"two");
        // overwrite: written
        assert_eq!(
            fixture_write(&dir, "a.txt", b"three", None),
            Saved::Ok(hash(b"three"))
        );
        assert!(!dir.join("a.txt.part").exists());
        assert!(matches!(
            fixture_write(&dir, "../a.txt", b"x", None),
            Saved::Failed(_)
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
