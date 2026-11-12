//! The right sidebar's "files" tab: the session's directory on the box as a tree (`Req::FsList`,
//! one request per directory, asked for when it is opened), and a click on a file reads it
//! (`Req::FsRead`) and shows it with [`crate::viewer::FileView`] in a page of its own with a
//! `‹ back` button. Owned by the files agent (docs/desktop-ui.md §files).
//!
//! * Tree: `▸`/`▾` and indentation, sizes on the right, dot-files muted. A click opens or closes a
//!   directory, a double click (or Enter) makes it the view's root, and a `..` line leads back up,
//!   never above the session's directory. Keys: Up/Down (PageUp/PageDown, Home/End), Right opens
//!   and enters, Left closes and climbs, Enter opens a file or enters a directory, Backspace goes
//!   up, F5 refreshes.
//! * Files over 8 MiB ask first, files over what the daemon reads (just under 16 MiB, see
//!   [`source::MAX_READ`]) are not offered. A failed request shows its reason in the panel, never
//!   more than that.
//! * Cache: listings per path and the last 8 files, for the session on show, so going back from
//!   the viewer is instant. `refresh` drops it (what is on show is asked for again and stays on
//!   screen until the new listing arrives), and a listing on show is asked for again by itself
//!   once it is [`TTL`] old. Another session starts afresh.
//! * Debug aid: with `FEMME_FILES_FIXTURE=<dir>` set, the panel lists and reads that local
//!   directory instead of asking the daemon, whatever session is set (see [`source::Source::new`]).

use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use femme_proto::Sid;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, relative, transparent_black, uniform_list, AnyElement, App, AppContext as _,
    ClickEvent, ClipboardEntry, ClipboardItem, Context, Div, Entity, ExternalPaths, FocusHandle,
    Focusable, FontWeight, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    ParentElement as _, Render, Rgba, ScrollHandle, ScrollStrategy, Stateful,
    StatefulInteractiveElement as _, Styled as _, Task, UniformListScrollHandle, Window,
};
use gpui_component::scroll::Scrollbar;

use crate::lightbox::Lightbox;
use crate::net::Net;
use crate::theme::{
    bad, bar, bar_dim, bar_text, ink, muted, neon, on_accent_strong, panel, paper, FONT_MONO,
    FONT_PIXEL, FONT_UI, META_SIZE, RULE, TB_SIZE,
};
use crate::ui::{button, ruled_sides, st};
use crate::video::{self, Clip};
use crate::viewer::FileView;

pub mod cache;
pub mod edit;
pub mod plan;
pub mod size;
pub mod source;
pub mod transfer;
pub mod tree;
pub mod zip;

use cache::FileCache;
use edit::{Editing, Saved};
use plan::{plan, Plan};
use size::fmt_size;
use source::{Piece, Source, MAX_READ};
use transfer::{
    clip_uri, downloads_dir, paths_in_text, piece_len, progress, read_piece, save_new, tilde,
    Origin, UpState, Uploads,
};
use tree::{Kind, Row, Tree};

/// A listing on show is asked for again once it is this old (the next time the panel is drawn).
pub const TTL: Duration = Duration::from_secs(10);

/// Height of one line of the tree, the same for all (the list is virtual).
const ROW_H: f32 = 24.;
/// Indentation per level of the tree.
const INDENT: f32 = 14.;
/// The frames of the spinner (VT323 draws all four).
const SPIN: [&str; 4] = ["|", "/", "-", "\\"];

/// A message between the header and the tree.
#[derive(Debug, PartialEq, Eq)]
enum Notice {
    /// A file is being read.
    Reading { name: String, len: u64 },
    /// A big file waits for a yes.
    Confirm {
        path: String,
        name: String,
        len: u64,
    },
    /// A file the daemon would refuse.
    TooBig { name: String, len: u64 },
    /// A directory could not be listed; gone when it is listed after all.
    ListFailed { path: String, text: String },
    /// Another request failed.
    Error(String),
    /// Something asked for was not done, and why (not a failure of the box).
    Note(String),
    /// A download landed at `path`; `clip`: it went on the clipboard as a file (`true`) or, with
    /// no `wl-copy` or `xclip` to do that, as its path in text (`false`).
    Saved { path: PathBuf, clip: Option<bool> },
}

/// Where a download goes.
enum Dest {
    /// The download folder under a free name; `clip`: then onto the clipboard (Ctrl-C).
    Downloads { clip: bool },
    /// The path the save dialog gave.
    At(PathBuf),
}

/// A file on show in the viewer page.
struct Opened {
    name: String,
    /// Relative to the session's directory: what a save writes.
    path: String,
    size: usize,
    view: Entity<FileView>,
    /// The file in the editor instead of the view (`edit` in the bar).
    edit: Option<Editing>,
}

/// What the panel shows.
enum Page {
    /// The tree.
    Tree,
    /// One file (boxed: the tree variant is small).
    Viewer(Box<Opened>),
}

/// The files tab: the directory of one session as a tree, and a viewer page for the file picked.
pub struct FilesPanel {
    source: Source,
    focus: FocusHandle,
    session: Option<Sid>,
    cwd: String,
    tree: Tree,
    files: FileCache,
    page: Page,
    notice: Option<Notice>,
    /// The file being read, and which request that is (a newer click supersedes an older one).
    reading: Option<String>,
    opening: u64,
    /// Bumped whenever what is on screen is thrown away: answers to older requests are dropped.
    epoch: u64,
    scroll: UniformListScrollHandle,
    view_scroll: ScrollHandle,
    born: Instant,
    ticker: Option<Task<()>>,
    /// Files going up, one at a time; `sending` is the one whose pieces are out.
    uploads: Uploads,
    sending: Option<u64>,
    /// The files and folders on their way to this machine.
    downloads: HashSet<String>,
    /// The line under the pointer (it shows the `↓` button).
    hovered: Option<usize>,
    /// Pictures pasted so far, for their names (`paste-3.png`).
    pastes: u32,
}

impl FilesPanel {
    /// A panel with no session. `net` is the daemon to ask; without one (and without the
    /// `FEMME_FILES_FIXTURE` debug aid) every request fails with a message.
    pub fn new(net: Option<Entity<Net>>, cx: &mut Context<Self>) -> Self {
        FilesPanel {
            source: Source::new(net),
            focus: cx.focus_handle(),
            session: None,
            cwd: String::new(),
            tree: Tree::new(),
            files: FileCache::default(),
            page: Page::Tree,
            notice: None,
            reading: None,
            opening: 0,
            epoch: 0,
            scroll: UniformListScrollHandle::new(),
            view_scroll: ScrollHandle::new(),
            born: Instant::now(),
            ticker: None,
            uploads: Uploads::default(),
            sending: None,
            downloads: HashSet::new(),
            hovered: None,
            pastes: 0,
        }
    }

    /// The session whose directory is listed (`cwd` as the session row says it, for the header:
    /// requests name the session and paths below its directory). The same session and directory
    /// again changes nothing; another one starts the view afresh.
    pub fn set_session(&mut self, sid: Option<Sid>, cwd: &str, cx: &mut Context<Self>) {
        if self.session == sid && self.cwd == cwd {
            return;
        }
        self.session = sid;
        self.cwd = cwd.to_owned();
        self.reset(cx);
    }

    /// The session on show.
    pub fn session(&self) -> Option<Sid> {
        self.session
    }

    /// Drops what was cached and asks again for everything on show, which stays on screen until
    /// the new listings arrive.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.epoch += 1;
        self.files.clear();
        self.notice = None;
        self.reading = None;
        self.tree.refresh();
        self.fetch_wanted(cx);
        cx.notify();
    }

    /// The tab was opened: the listings on show that are older than [`TTL`] are asked for again
    /// (they stay on screen until the new ones arrive). A view that is cached by its parent is
    /// not drawn again by itself, so the shell calls this when the panel comes into view.
    pub fn shown(&mut self, cx: &mut Context<Self>) {
        if self.active() && self.tree.expire(TTL) {
            self.fetch_wanted(cx);
            cx.notify();
        }
    }

    /// Leaves the viewer page for the tree (where it was left).
    pub fn back(&mut self, cx: &mut Context<Self>) {
        if let Page::Viewer(open) = &mut self.page {
            // unsaved edits stay on the page: save or discard first
            if let Some(e) = &mut open.edit {
                if e.modified(cx) {
                    e.error = Some("unsaved edits: save or discard".into());
                    cx.notify();
                    return;
                }
            }
            self.page = Page::Tree;
            cx.notify();
        }
    }

    /// Whether there is anything to ask: a session, or the fixture.
    fn active(&self) -> bool {
        self.session.is_some() || self.source.is_fixture()
    }

    /// Throws the view away and lists the session's directory.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.epoch += 1;
        self.tree = Tree::new();
        self.files.clear();
        self.page = Page::Tree;
        self.notice = None;
        self.reading = None;
        self.uploads.clear();
        self.sending = None;
        self.downloads.clear();
        self.hovered = None;
        self.scroll = UniformListScrollHandle::new();
        self.fetch_wanted(cx);
        cx.notify();
    }

    /// Asks for every directory on show that was not asked for yet.
    fn fetch_wanted(&mut self, cx: &mut Context<Self>) {
        if !self.active() {
            return;
        }
        for path in self.tree.wanted() {
            self.spawn_list(path, cx);
        }
        self.ensure_ticker(cx);
    }

    fn spawn_list(&mut self, path: String, cx: &mut Context<Self>) {
        let (source, sid, epoch) = (self.source.clone(), self.session, self.epoch);
        cx.spawn(async move |this, cx| {
            let answer = source.list(sid, &path, cx).await;
            this.update(cx, |t, cx| {
                if t.epoch != epoch {
                    return;
                }
                match &answer {
                    Err(why) => {
                        // The header has the session's directory; a line is short in a narrow panel.
                        let what = if path.is_empty() { &t.cwd } else { &path };
                        let text = format!("can't list {what}: {why}");
                        t.notice = Some(Notice::ListFailed {
                            path: path.clone(),
                            text,
                        });
                    }
                    Ok(entries) => {
                        if matches!(&t.notice, Some(Notice::ListFailed { path: p, .. }) if *p == path)
                        {
                            t.notice = None;
                        }
                        // The uploads that finished here are in the listing now.
                        let names: Vec<&str> = entries
                            .iter()
                            .filter(|e| !e.dir)
                            .map(|e| e.name.as_str())
                            .collect();
                        t.uploads.listed(&path, &names);
                        t.tree.set_pending(t.uploads.pending());
                    }
                }
                t.tree.loaded(&path, answer);
                // A directory that was open below this one is on show now.
                t.fetch_wanted(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Opens the file `path` of `len` bytes (as listed): from the cache when it is there, else
    /// after the questions a big file asks, by reading it.
    fn open_file(&mut self, path: String, len: u64, confirmed: bool, cx: &mut Context<Self>) {
        let name = tree::base(&path).to_owned();
        let cached = self.files.get(&path);
        let reading = self.reading.as_deref() == Some(path.as_str());
        match plan(len, cached.is_some(), reading, confirmed) {
            Plan::Show => {
                if let Some(data) = cached {
                    self.notice = None;
                    self.reading = None;
                    self.show_file(name, path, data, cx);
                }
                return;
            }
            Plan::Wait => return,
            Plan::Refuse => {
                self.notice = Some(Notice::TooBig { name, len });
                cx.notify();
                return;
            }
            Plan::Ask => {
                self.notice = Some(Notice::Confirm { path, name, len });
                cx.notify();
                return;
            }
            Plan::Read => {}
        }
        self.notice = Some(Notice::Reading {
            name: name.clone(),
            len,
        });
        self.reading = Some(path.clone());
        self.opening += 1;
        let (source, sid, epoch, seq) =
            (self.source.clone(), self.session, self.epoch, self.opening);
        cx.spawn(async move |this, cx| {
            let answer = source.read(sid, &path, cx).await;
            this.update(cx, |t, cx| {
                if t.epoch != epoch || t.opening != seq {
                    return;
                }
                t.reading = None;
                match answer {
                    Ok(data) => {
                        let data = Arc::new(data);
                        t.files.put(&path, data.clone());
                        t.notice = None;
                        t.show_file(name, path, data, cx);
                    }
                    Err(why) => {
                        t.notice = Some(Notice::Error(format!("can't read {name}: {why}")));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
        self.ensure_ticker(cx);
        cx.notify();
    }

    /// Switches to the viewer page with `data`. A picture or a video opens in the lightbox
    /// instead.
    fn show_file(
        &mut self,
        name: String,
        path: String,
        data: Arc<Vec<u8>>,
        cx: &mut Context<Self>,
    ) {
        if let Some(format) = crate::transcript::attach::image_format(&name, &data) {
            let image = Arc::new(gpui::Image::from_bytes(format, data.to_vec()));
            Lightbox::show(&name, image, cx);
            return;
        }
        if video::is_video(&name) {
            match Clip::from_bytes(&name, &data) {
                Ok(clip) => Lightbox::play(&name, clip, cx),
                Err(why) => self.notice = Some(Notice::Error(format!("can't play {name}: {why}"))),
            }
            cx.notify();
            return;
        }
        let size = data.len();
        let view = cx.new(|cx| FileView::new(name.clone(), data.to_vec(), cx));
        self.view_scroll = ScrollHandle::new();
        self.page = Page::Viewer(Box::new(Opened {
            name,
            path,
            size,
            view,
            edit: None,
        }));
        cx.notify();
    }

    /// Puts the open text file in the editor (`edit` in the bar).
    fn start_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Page::Viewer(open) = &self.page else {
            return;
        };
        let (path, name) = (open.path.clone(), open.name.clone());
        let Some(data) = self.files.get(&path) else {
            return;
        };
        let Some(e) = Editing::new(path, &data, window, cx) else {
            self.notice = Some(Notice::Note(format!("{name} is not text")));
            cx.notify();
            return;
        };
        e.focus(window, cx);
        if let Page::Viewer(open) = &mut self.page {
            open.edit = Some(e);
        }
        cx.notify();
    }

    /// Leaves the editor for the view, dropping what was not saved.
    fn stop_edit(&mut self, cx: &mut Context<Self>) {
        if let Page::Viewer(open) = &mut self.page {
            open.edit = None;
            cx.notify();
        }
    }

    /// Saves the editor's text over the file (Ctrl-S); `overwrite` after a conflict.
    fn save_edit(&mut self, overwrite: bool, cx: &mut Context<Self>) {
        let (source, sid) = (self.source.clone(), self.session);
        let Page::Viewer(open) = &mut self.page else {
            return;
        };
        let Some(e) = &mut open.edit else {
            return;
        };
        if e.saving {
            return;
        }
        let task = e.save(&source, sid, overwrite, cx);
        self.ensure_ticker(cx);
        cx.spawn(async move |this, cx| {
            let (text, r) = task.await;
            this.update(cx, |t, cx| t.saved_edit(text, r, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    /// A save ended: the view, the cache and the listing follow a saved file.
    fn saved_edit(&mut self, text: String, r: Saved, cx: &mut Context<Self>) {
        let Page::Viewer(open) = &mut self.page else {
            return;
        };
        let Some(e) = &mut open.edit else {
            return;
        };
        let ok = matches!(r, Saved::Ok(_));
        e.finish(text.clone(), r);
        if ok {
            let data = Arc::new(text.into_bytes());
            open.size = data.len();
            let name = open.name.clone();
            open.view = cx.new(|cx| FileView::new(name, data.to_vec(), cx));
            self.files.put(&open.path.clone(), data);
            self.relist(cx);
        }
        cx.notify();
    }

    /// Reads the file again after a conflict: the editor shows what is on disk now.
    fn reload_edit(&mut self, cx: &mut Context<Self>) {
        let Page::Viewer(open) = &self.page else {
            return;
        };
        let (path, len) = (open.path.clone(), open.size as u64);
        self.files.forget(&path);
        self.page = Page::Tree;
        self.open_file(path, len, true, cx);
    }

    /// Whether something is on its way (the spinner turns).
    fn busy(&self) -> bool {
        self.tree.busy()
            || matches!(self.notice, Some(Notice::Reading { .. }))
            || self.uploads.active()
            || !self.downloads.is_empty()
    }

    /// Asks again for every listing on show (after uploads), keeping the notice and the cache.
    fn relist(&mut self, cx: &mut Context<Self>) {
        self.tree.refresh();
        self.fetch_wanted(cx);
        cx.notify();
    }

    /// Uploads the files `paths` (of this machine) into the directory `dir`, one after the other;
    /// a folder is refused with a note. Measured off the UI thread.
    fn upload_paths(&mut self, dir: String, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if !self.active() {
            self.notice = Some(Notice::Note("no session to upload to".into()));
            cx.notify();
            return;
        }
        cx.spawn(async move |this, cx| {
            let measured = cx
                .background_spawn(async move {
                    paths
                        .into_iter()
                        .map(|p| {
                            let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
                            let meta = std::fs::metadata(&p);
                            (p, name, meta)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |t, cx| {
                let (mut folders, mut refused) = (Vec::new(), Vec::new());
                for (path, name, meta) in measured {
                    let name = name.unwrap_or_else(|| path.display().to_string());
                    match meta {
                        Ok(m) if m.is_dir() => folders.push(name),
                        Ok(m) if m.is_file() => {
                            let origin = Origin::Local(path);
                            if t.uploads.add(origin, &dir, &name, m.len()).is_none() {
                                refused.push(format!("{name}: not a name the box takes"));
                            }
                        }
                        Ok(_) => refused.push(format!("{name}: not a regular file")),
                        Err(e) => refused.push(format!("{name}: {e}")),
                    }
                }
                if !folders.is_empty() {
                    let names = folders.join(", ");
                    t.notice = Some(Notice::Note(format!(
                        "folders are not uploaded (files only): {names}"
                    )));
                }
                if !refused.is_empty() {
                    t.notice = Some(Notice::Error(format!(
                        "can't upload {}",
                        refused.join("; ")
                    )));
                }
                t.tree.set_pending(t.uploads.pending());
                t.pump(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Ctrl-V: copied files go into the folder on show (as gpui's paths, or as the text of
    /// `file://` URIs or absolute paths a file manager copies), a picture as `paste-N.<ext>`.
    /// Whether the clipboard held anything of that.
    fn paste(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        let dir = self.tree.root().to_owned();
        let mut taken = false;
        for entry in item.entries {
            match entry {
                ClipboardEntry::ExternalPaths(paths) => {
                    self.upload_paths(dir.clone(), paths.paths().to_vec(), cx);
                    taken = true;
                }
                ClipboardEntry::String(text) => {
                    if let Some(paths) = paths_in_text(text.text()) {
                        self.upload_paths(dir.clone(), paths, cx);
                        taken = true;
                    }
                }
                ClipboardEntry::Image(image) if self.active() => {
                    self.pastes += 1;
                    let name = format!("paste-{}.{}", self.pastes, image.format.extension());
                    let total = image.bytes.len() as u64;
                    let origin = Origin::Bytes(Arc::new(image.bytes));
                    self.uploads.add(origin, &dir, &name, total);
                    taken = true;
                }
                _ => {}
            }
        }
        if taken {
            self.tree.set_pending(self.uploads.pending());
            self.pump(cx);
            cx.notify();
        }
        taken
    }

    /// Starts the next queued upload unless one is going: its pieces go out one after the
    /// other, each read off the UI thread just before it is sent. After the last upload the
    /// listings on show are asked for again.
    fn pump(&mut self, cx: &mut Context<Self>) {
        if self.sending.is_some() {
            return;
        }
        let Some(id) = self.uploads.start_next() else {
            return;
        };
        self.sending = Some(id);
        let (source, sid) = (self.source.clone(), self.session);
        cx.spawn(async move |this, cx| {
            loop {
                let next = this
                    .update(cx, |t, _| {
                        (t.sending == Some(id))
                            .then(|| t.uploads.next(id))
                            .flatten()
                    })
                    .ok()
                    .flatten();
                let Some((origin, path, offset, total)) = next else {
                    break;
                };
                let len = piece_len(offset, total);
                let data = match origin {
                    Origin::Local(file) => {
                        cx.background_spawn(async move { read_piece(&file, offset, len) })
                            .await
                    }
                    Origin::Bytes(bytes) => bytes
                        .get(offset as usize..offset as usize + len)
                        .map(<[u8]>::to_vec)
                        .ok_or_else(|| "the bytes ran out".to_owned()),
                };
                let answer = match data {
                    Ok(data) => {
                        let piece = Piece {
                            path,
                            offset,
                            total,
                            data,
                            ..Piece::default()
                        };
                        source.write(sid, piece, cx).await
                    }
                    Err(why) => Err(why),
                };
                let more = this
                    .update(cx, |t, cx| {
                        if t.sending != Some(id) {
                            return false;
                        }
                        let more = match answer {
                            // An answer that takes nothing would go round forever.
                            Ok(w) if !w.done && w.written <= offset => {
                                t.uploads.failed(id, "the box took none of it".into());
                                false
                            }
                            Ok(w) => {
                                t.uploads.landed(id, &w.path, w.written, w.done);
                                !w.done
                            }
                            Err(why) => {
                                t.uploads.failed(id, why);
                                false
                            }
                        };
                        t.tree.set_pending(t.uploads.pending());
                        cx.notify();
                        more
                    })
                    .unwrap_or(false);
                if !more {
                    break;
                }
            }
            this.update(cx, |t, cx| {
                if t.sending == Some(id) {
                    t.sending = None;
                    if t.uploads.active() {
                        t.pump(cx);
                    } else {
                        t.relist(cx);
                    }
                }
            })
            .ok();
        })
        .detach();
        self.ensure_ticker(cx);
    }

    /// The `↓` button (with `ask`, Shift: a save dialog) or Ctrl-C (`clip`): the file on `row`,
    /// or its folder as `<name>.zip`, onto this machine.
    fn download(&mut self, row: &Row, ask: bool, clip: bool, cx: &mut Context<Self>) {
        let (len, name) = match row.kind {
            Kind::File { len } if len > MAX_READ => {
                let name = row.name().to_owned();
                self.notice = Some(Notice::TooBig { name, len });
                cx.notify();
                return;
            }
            Kind::File { len } => (Some(len), row.name().to_owned()),
            Kind::Dir { .. } => (None, format!("{}.zip", row.name())),
            _ => return,
        };
        if self.downloads.contains(&row.path) {
            return;
        }
        let path = row.path.clone();
        if !ask {
            self.fetch_out(path, name, len, Dest::Downloads { clip }, cx);
            return;
        }
        let picked = cx.prompt_for_new_path(&downloads_dir(), Some(&name));
        cx.spawn(async move |this, cx| {
            let answer = picked.await;
            this.update(cx, |t, cx| match answer {
                Ok(Ok(Some(to))) => t.fetch_out(path, name, len, Dest::At(to), cx),
                Ok(Err(why)) => {
                    t.notice = Some(Notice::Error(format!("no save dialog: {why}")));
                    cx.notify();
                }
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    /// Fetches `path` (a file of `len` bytes, or with `None` a folder, zipped) and writes it as
    /// `name` where `dest` says, then says where it went.
    fn fetch_out(
        &mut self,
        path: String,
        name: String,
        len: Option<u64>,
        dest: Dest,
        cx: &mut Context<Self>,
    ) {
        self.downloads.insert(path.clone());
        let cached = len.and_then(|_| self.files.get(&path));
        let (source, sid) = (self.source.clone(), self.session);
        let clip = matches!(dest, Dest::Downloads { clip: true });
        cx.spawn(async move |this, cx| {
            let data = match cached {
                Some(data) => Ok(data),
                None if len.is_some() => source.read(sid, &path, cx).await.map(Arc::new),
                None => source.zip(sid, &path, cx).await.map(Arc::new),
            };
            let saved = match data {
                Ok(data) => {
                    cx.background_spawn(async move {
                        match dest {
                            Dest::Downloads { .. } => save_new(&downloads_dir(), &name, &data),
                            Dest::At(to) => std::fs::write(&to, &*data)
                                .map(|()| to.clone())
                                .map_err(|e| format!("{}: {e}", tilde(&to))),
                        }
                    })
                    .await
                }
                Err(why) => Err(format!("can't download {name}: {why}")),
            };
            let clipped = match &saved {
                Ok(at) if clip => {
                    let at = at.clone();
                    Some(cx.background_spawn(async move { clip_uri(&at) }).await)
                }
                _ => None,
            };
            this.update(cx, |t, cx| {
                t.downloads.remove(&path);
                t.notice = Some(match saved {
                    Ok(at) => {
                        if clipped == Some(false) {
                            let text = at.display().to_string();
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                        Notice::Saved {
                            path: at,
                            clip: clipped,
                        }
                    }
                    Err(why) => Notice::Error(why),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
        self.ensure_ticker(cx);
        cx.notify();
    }

    /// Files dropped on the line of `dir` (a directory, opened so the uploads show) or into
    /// the panel (the directory on show).
    fn dropped(&mut self, dir: String, paths: &ExternalPaths, cx: &mut Context<Self>) {
        if !dir.is_empty() && self.tree.set_open(&dir, true) {
            self.fetch_wanted(cx);
        }
        self.upload_paths(dir, paths.paths().to_vec(), cx);
    }

    /// Redraws a few times a second while something is on its way, so the spinner turns even in a
    /// parent that caches this view; it stops when nothing is.
    fn ensure_ticker(&mut self, cx: &mut Context<Self>) {
        if self.ticker.is_some() || !self.busy() {
            return;
        }
        self.ticker = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let more = this
                .update(cx, |t, cx| {
                    if t.busy() {
                        cx.notify();
                        true
                    } else {
                        t.ticker = None;
                        false
                    }
                })
                .unwrap_or(false);
            if !more {
                break;
            }
        }));
    }

    /// The frame of the spinner now.
    fn spin(&self) -> &'static str {
        SPIN[(self.born.elapsed().as_millis() / 120 % 4) as usize]
    }

    /// What a click on `row` does; `clicks` is 2 on the second click of a double click.
    fn row_clicked(&mut self, row: &Row, clicks: usize, cx: &mut Context<Self>) {
        match row.kind {
            Kind::Up => {
                self.tree.up();
            }
            Kind::Dir { .. } => {
                self.tree.select(row.key());
                if clicks >= 2 {
                    self.tree.enter(&row.path);
                } else {
                    self.tree.toggle(&row.path);
                }
            }
            Kind::File { len } => {
                self.tree.select(row.key());
                self.open_file(row.path.clone(), len, false, cx);
            }
            Kind::Failed => self.tree.retry(&row.path),
            Kind::Transfer { id } => {
                if self.uploads.retry(id) {
                    self.tree.set_pending(self.uploads.pending());
                    self.pump(cx);
                }
            }
            Kind::Loading | Kind::Empty => {}
        }
        self.fetch_wanted(cx);
        cx.notify();
    }

    /// Enter on the line under the cursor: a file opens, a directory becomes the view's root,
    /// `..` goes up.
    fn activate(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.tree.selected_row().cloned() else {
            return;
        };
        match row.kind {
            Kind::Up => {
                self.tree.up();
            }
            Kind::Dir { .. } => {
                self.tree.enter(&row.path);
            }
            Kind::File { len } => self.open_file(row.path, len, false, cx),
            Kind::Loading | Kind::Empty | Kind::Failed | Kind::Transfer { .. } => {}
        }
    }

    fn on_key(&mut self, e: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &e.keystroke;
        let m = &k.modifiers;
        // Ctrl-S saves the editor
        if matches!(&self.page, Page::Viewer(open) if open.edit.is_some()) {
            if (m.control || m.platform) && !m.alt && !m.shift && k.key == "s" {
                self.save_edit(false, cx);
                cx.stop_propagation();
            }
            return;
        }
        // Ctrl-V uploads what was copied, Ctrl-C copies the line's file out; other chords belong
        // to the shell.
        if (m.control || m.platform) && !m.alt && !m.shift && matches!(self.page, Page::Tree) {
            match k.key.as_str() {
                "v" => {
                    if self.paste(cx) {
                        cx.stop_propagation();
                    }
                }
                "c" => {
                    if let Some(row) = self.tree.selected_row().cloned() {
                        self.download(&row, false, true, cx);
                        cx.stop_propagation();
                    }
                }
                _ => {}
            }
            return;
        }
        if m.control || m.alt || m.platform {
            return;
        }
        let key = k.key.as_str();
        if matches!(self.page, Page::Viewer(_)) {
            if matches!(key, "escape" | "backspace") {
                self.back(cx);
                cx.stop_propagation();
            }
            return;
        }
        match key {
            "down" => {
                self.tree.step(1);
            }
            "up" => {
                self.tree.step(-1);
            }
            "pagedown" => {
                self.tree.step(10);
            }
            "pageup" => {
                self.tree.step(-10);
            }
            "home" => {
                self.tree.edge(false);
            }
            "end" => {
                self.tree.edge(true);
            }
            "right" => {
                self.tree.key_right();
            }
            "left" => {
                self.tree.key_left();
            }
            "enter" => self.activate(cx),
            "space" => match self.tree.selected_row().cloned() {
                Some(row) => self.row_clicked(&row, 1, cx),
                None => return,
            },
            "backspace" => {
                self.tree.up();
            }
            "f5" => self.refresh(cx),
            "escape" if self.notice.is_some() => {
                self.notice = None;
                self.reading = None;
                self.opening += 1;
            }
            _ => return,
        }
        if let Some(ix) = self.tree.selected_index() {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        self.fetch_wanted(cx);
        cx.stop_propagation();
        cx.notify();
    }

    /// `Notice` as a strip under the header.
    fn notice_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let small = |b: Stateful<Div>| b.h(px(24.));
        let text = |t: String| div().flex_1().min_w_0().text_size(px(13.)).child(t);
        Some(match self.notice.as_ref()? {
            Notice::Reading { name, len } => strip(panel())
                .child(spinner(self.spin()))
                .child(text(format!("reading {name} ({})", fmt_size(*len))))
                .into_any_element(),
            Notice::Confirm { path, len, .. } => {
                let (path, len) = (path.clone(), *len);
                strip(panel())
                    .child(text(format!("big file ({}): open?", fmt_size(len))))
                    .child(small(button("files-yes", "open")).on_click(
                        cx.listener(move |t, _, _, cx| t.open_file(path.clone(), len, true, cx)),
                    ))
                    .child(small(button("files-no", "cancel")).on_click(cx.listener(
                        |t, _, _, cx| {
                            t.notice = None;
                            cx.notify();
                        },
                    )))
                    .into_any_element()
            }
            Notice::TooBig { name, len } => strip(panel())
                .child(st("too big", bad()))
                .child(text(format!(
                    "{name} is {}; the daemon reads at most {}",
                    fmt_size(*len),
                    fmt_size(MAX_READ)
                )))
                .child(dismiss(cx))
                .into_any_element(),
            Notice::Note(why) => strip(panel())
                .child(text(why.clone()))
                .child(dismiss(cx))
                .into_any_element(),
            Notice::Saved { path, clip } => {
                let at = path.clone();
                let tail = match clip {
                    Some(true) => " · on the clipboard",
                    Some(false) => " · no wl-copy or xclip: its path is on the clipboard as text",
                    None => "",
                };
                strip(panel())
                    .child(text(format!("saved to {}{tail}", tilde(path))))
                    .child(
                        small(button("files-reveal", "open folder"))
                            .on_click(cx.listener(move |_, _, _, cx| cx.reveal_path(&at))),
                    )
                    .child(dismiss(cx))
                    .into_any_element()
            }
            Notice::ListFailed { text: why, .. } | Notice::Error(why) => {
                strip(Rgba { a: 0.12, ..bad() })
                    .child(st("error", bad()))
                    .child(text(why.clone()))
                    .child(dismiss(cx))
                    .into_any_element()
            }
        })
    }

    /// The tree page: header (directory, refresh), notice, the tree.
    fn tree_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let focused = self.focus.is_focused(window);
        if self.active() && self.tree.expire(TTL) {
            self.fetch_wanted(cx);
        }
        let header = ruled_sides(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(8.))
                .py(px(4.))
                .bg(panel()),
            px(0.),
            px(0.),
            RULE,
            px(0.),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis_start()
                .font_family(FONT_MONO)
                .text_size(META_SIZE)
                .text_color(muted())
                .child(tree::under(&self.cwd, self.tree.root())),
        )
        .when_some(self.uploads.counter(), |h, (k, n)| {
            h.child(spinner(self.spin())).child(
                div()
                    .flex_none()
                    .font_family(FONT_MONO)
                    .text_size(META_SIZE)
                    .text_color(muted())
                    .child(format!("{k} / {n}")),
            )
        })
        .child(
            button("files-refresh", "refresh")
                .h(px(26.))
                .on_click(cx.listener(|t, _, _, cx| t.refresh(cx))),
        );
        let body = if self.active() {
            let rows = self.tree.rows().len();
            div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(
                    uniform_list(
                        "files-rows",
                        rows,
                        cx.processor(move |this, range: Range<usize>, _, cx| {
                            this.rows_el(range, focused, cx)
                        }),
                    )
                    .track_scroll(&self.scroll)
                    .size_full(),
                )
                .child(Scrollbar::vertical(&self.scroll))
        } else {
            div().flex_1().min_h_0().p(px(12.)).child(
                div()
                    .font_family(FONT_MONO)
                    .text_size(META_SIZE)
                    .text_color(muted())
                    .child("no session selected"),
            )
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .children(self.notice_strip(cx))
            .child(body)
    }

    /// The lines `range` of the tree.
    fn rows_el(
        &mut self,
        range: Range<usize>,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> Vec<Stateful<Div>> {
        let spin = self.spin();
        range
            .filter_map(|ix| {
                let row = self.tree.rows().get(ix)?.clone();
                let selected = row.key().is_some() && self.tree.selected() == row.key();
                Some(self.row_el(ix, row, selected, focused, spin, cx))
            })
            .collect()
    }

    /// One line of the tree.
    fn row_el(
        &self,
        ix: usize,
        row: Row,
        selected: bool,
        focused: bool,
        spin: &'static str,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        // The lines that do something when clicked; a spinner or "(empty)" is only text.
        let clickable = match row.kind {
            Kind::Loading | Kind::Empty => false,
            Kind::Transfer { id } => self
                .uploads
                .get(id)
                .is_some_and(|u| matches!(u.state, UpState::Failed(_))),
            _ => true,
        };
        // On the neon row of the keyboard every label takes the colour that reads on neon (ink in
        // the light palette, paper in the dark one); elsewhere ink for a name, muted for the rest.
        let on_neon = selected && focused;
        let dim = if on_neon { on_accent_strong() } else { muted() };
        let base = div()
            .id(("files-row", ix))
            .w_full()
            .h(px(ROW_H))
            .flex()
            .items_center()
            .gap(px(4.))
            .pl(px(6. + INDENT * row.depth as f32))
            .pr(px(14.))
            .whitespace_nowrap()
            .cursor_pointer()
            .when(selected, |d| {
                d.bg(if focused { neon() } else { cursor_idle() })
            })
            .when(!selected && clickable, |d| d.hover(|s| s.bg(panel())));
        let marker = |glyph: &'static str| {
            div()
                .w(px(12.))
                .flex_none()
                .font_family(FONT_MONO)
                .text_size(px(13.))
                .text_color(dim)
                .child(glyph)
        };
        let name = |text: String, color: Rgba| {
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis_middle()
                .text_color(color)
                .child(text)
        };
        let note = |text: String, color: Rgba| {
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .font_family(FONT_MONO)
                .text_size(META_SIZE)
                .text_color(color)
                .child(text)
        };
        let color = if on_neon {
            on_accent_strong()
        } else if row.hidden() {
            muted()
        } else {
            ink()
        };
        let size_col = |text: String, color: Rgba| {
            div()
                .flex_none()
                .pl(px(6.))
                .font_family(FONT_MONO)
                .text_size(META_SIZE)
                .text_color(color)
                .child(text)
        };
        let down = self.downloads.contains(&row.path);
        let line = match row.kind {
            Kind::Up => base
                .child(marker(""))
                .child(name("..".to_owned(), dim).font_family(FONT_MONO)),
            Kind::Dir { open } => base
                .when(down, |d| d.opacity(0.5).child(spinner(spin)))
                .when(!down, |d| d.child(marker(if open { "▾" } else { "▸" })))
                .child(name(row.name().to_owned(), color).font_weight(FontWeight::SEMIBOLD))
                .when(down, |d| d.child(size_col("zipping…".into(), dim))),
            Kind::File { len } => match down {
                true => base
                    .opacity(0.5)
                    .child(spinner(spin))
                    .child(name(row.name().to_owned(), color))
                    .child(size_col(format!("downloading {}", fmt_size(len)), dim)),
                false => base
                    .child(marker(""))
                    .child(name(row.name().to_owned(), color))
                    .child(size_col(fmt_size(len), dim)),
            },
            Kind::Loading => base
                .child(spinner(spin))
                .child(note("loading…".to_owned(), dim)),
            Kind::Empty => base
                .child(marker(""))
                .child(note("(empty)".to_owned(), dim)),
            Kind::Transfer { id } => {
                let label = name(row.name().to_owned(), color);
                match self.uploads.get(id).map(|u| (&u.state, u.sent, u.total)) {
                    Some((UpState::Queued, _, _)) => base
                        .opacity(0.5)
                        .child(marker(""))
                        .child(label)
                        .child(size_col("waiting".into(), dim)),
                    Some((UpState::Sending, sent, total)) => base
                        .opacity(0.5)
                        .child(spinner(spin))
                        .child(label)
                        .child(size_col(
                            format!("uploading {}", progress(sent, total)),
                            dim,
                        )),
                    Some((UpState::Failed(_), _, _)) => {
                        base.child(marker("!")).child(label).child(size_col(
                            "failed · retry".into(),
                            if on_neon { on_accent_strong() } else { bad() },
                        ))
                    }
                    Some((UpState::Done, _, total)) => base
                        .child(marker(""))
                        .child(label)
                        .child(size_col(fmt_size(total), dim)),
                    None => base.child(marker("")).child(label),
                }
            }
            Kind::Failed => base.child(marker("!")).child(note(
                "could not list: click to try again".to_owned(),
                if on_neon { on_accent_strong() } else { bad() },
            )),
        };
        // Files dropped on a directory go into it, on a file into the file's directory.
        let drop_into = match row.kind {
            Kind::Dir { .. } => Some(row.path.clone()),
            Kind::File { .. } => Some(tree::parent(&row.path).to_owned()),
            _ => None,
        };
        let line = match drop_into {
            Some(dir) => line
                .drag_over::<ExternalPaths>(|s, _, _, _| s.bg(drop_fill()))
                .on_drop(cx.listener(move |t, paths: &ExternalPaths, _, cx| {
                    t.dropped(dir.clone(), paths, cx)
                })),
            None => line,
        };
        // The `↓` on the line under the pointer: Shift asks where to save.
        let offer = self.hovered == Some(ix)
            && !down
            && matches!(row.kind, Kind::Dir { .. } | Kind::File { .. });
        let line = line.when(offer, |d| {
            let row = row.clone();
            d.child(
                button(("files-down", ix), "↓")
                    .h(px(20.))
                    .px(px(6.))
                    .font_family(FONT_MONO)
                    .text_size(px(12.))
                    .on_click(cx.listener(move |t, e: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        t.download(&row, e.modifiers().shift, false, cx);
                    })),
            )
        });
        if !clickable {
            return line.cursor_default();
        }
        line
            // gpui paints the hover style from where the pointer is, but only repaints this view
            // when the pointer moves: a line that slid under a still pointer (a notice appeared,
            // the page changed) would stay grey after the pointer left. This listener also
            // fires for such moves of the layout, and the repaint it asks for is what clears it.
            .on_hover(cx.listener(move |t, hovered: &bool, _, cx| {
                if *hovered {
                    t.hovered = Some(ix);
                } else if t.hovered == Some(ix) {
                    t.hovered = None;
                }
                cx.notify();
            }))
            .on_click(
                cx.listener(move |t, e: &ClickEvent, _, cx| {
                    t.row_clicked(&row, e.click_count(), cx)
                }),
            )
    }

    /// The viewer page: a black title bar (`‹ back`, name, size) over the file.
    fn viewer_page(&self, open: &Opened, cx: &mut Context<Self>) -> Div {
        let small = |b: Stateful<Div>| b.h(px(26.));
        let text = matches!(open.view.read(cx).kind(), crate::viewer::Kind::Text);
        let (modified, saving) = open
            .edit
            .as_ref()
            .map_or((false, false), |e| (e.modified(cx), e.saving));
        let bar = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .py(px(3.))
            .bg(bar())
            .text_color(bar_text())
            .child(
                button("files-back", "‹ back")
                    .h(px(26.))
                    .on_click(cx.listener(|t, _, _, cx| t.back(cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_middle()
                    .font_family(FONT_PIXEL)
                    .text_size(TB_SIZE)
                    .line_height(relative(1.))
                    .child(open.name.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .font_family(FONT_MONO)
                    .text_size(META_SIZE)
                    .text_color(bar_dim())
                    .child(fmt_size(open.size as u64)),
            )
            .when(text && open.edit.is_none(), |bar| {
                bar.child(
                    small(button("files-edit", "edit"))
                        .on_click(cx.listener(|t, _, window, cx| t.start_edit(window, cx))),
                )
            })
            .when(open.edit.is_some(), |bar| {
                bar.when(modified, |bar| {
                    bar.child(div().flex_none().text_color(neon()).child("●"))
                })
                .when(saving, |bar| {
                    bar.child(spinner(self.spin()))
                        .child(st("saving", bar_dim()))
                })
                .when(!saving, |bar| {
                    bar.child(
                        small(button("files-save", "save"))
                            .on_click(cx.listener(|t, _, _, cx| t.save_edit(false, cx))),
                    )
                    .child(
                        small(button(
                            "files-done",
                            if modified { "discard" } else { "done" },
                        ))
                        .on_click(cx.listener(|t, _, _, cx| t.stop_edit(cx))),
                    )
                })
            });
        // what a save ran into, under the bar
        let trouble = open.edit.as_ref().and_then(|e| {
            if e.conflict {
                Some(
                    strip(panel())
                        .child(st("changed on disk", bad()))
                        .child(div().flex_1().child("the file is not the one you opened"))
                        .child(
                            small(button("files-reload", "reload"))
                                .on_click(cx.listener(|t, _, _, cx| t.reload_edit(cx))),
                        )
                        .child(
                            small(button("files-overwrite", "overwrite"))
                                .on_click(cx.listener(|t, _, _, cx| t.save_edit(true, cx))),
                        ),
                )
            } else {
                e.error.as_ref().map(|why| {
                    strip(panel())
                        .child(st("not saved", bad()))
                        .child(div().flex_1().min_w_0().child(why.clone()))
                })
            }
        });
        // The scrollbar is a sibling of what scrolls: inside it, it would scroll away.
        let body = match &open.edit {
            // the editor scrolls by itself
            Some(e) => div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(e.element()),
            None => div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(
                    div()
                        .id("files-view")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.view_scroll)
                        .child(open.view.clone()),
                )
                .child(Scrollbar::vertical(&self.view_scroll)),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar)
            .children(trouble)
            .child(body)
    }
}

/// The cursor's line while the panel does not have the keyboard: a pale [`neon`], apart from the
/// grey of the line under the pointer.
fn cursor_idle() -> Rgba {
    Rgba { a: 0.3, ..neon() }
}

/// The panel's colour while files are dragged over it (the composer's): paper with a fifth of
/// neon in it, opaque.
fn drop_fill() -> Rgba {
    let mix = |paper: f32, neon: f32| paper * 0.8 + neon * 0.2;
    Rgba {
        r: mix(paper().r, neon().r),
        g: mix(paper().g, neon().g),
        b: mix(paper().b, neon().b),
        a: 1.,
    }
}

/// A row between the header and the tree: ruled below, `bg` behind.
fn strip(bg: Rgba) -> Div {
    ruled_sides(
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .py(px(5.))
            .bg(bg),
        px(0.),
        px(0.),
        RULE,
        px(0.),
    )
}

/// The turning bar: one frame of [`SPIN`] in the pixel font.
fn spinner(frame: &'static str) -> Div {
    div()
        .w(px(12.))
        .flex_none()
        .font_family(FONT_PIXEL)
        .text_size(px(20.))
        .line_height(relative(1.))
        .child(frame)
}

/// The `×` that clears the notice.
fn dismiss(cx: &mut Context<FilesPanel>) -> Stateful<Div> {
    button("files-dismiss", "×")
        .h(px(24.))
        .w(px(28.))
        .on_click(cx.listener(|t, _, _, cx| {
            t.notice = None;
            cx.notify();
        }))
}

impl Focusable for FilesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FilesPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page = match &self.page {
            Page::Tree => self.tree_page(window, cx),
            Page::Viewer(open) => self.viewer_page(open, cx),
        };
        div()
            .size_full()
            .overflow_hidden()
            .bg(paper())
            .text_color(ink())
            .font_family(FONT_UI)
            .text_size(px(14.))
            .line_height(relative(1.3))
            .border_2()
            .border_color(transparent_black())
            .drag_over::<ExternalPaths>(|s, _, _, _| s.border_color(neon()).bg(drop_fill()))
            .on_drop(cx.listener(|t, paths: &ExternalPaths, _, cx| {
                let root = t.tree.root().to_owned();
                t.dropped(root, paths, cx)
            }))
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)),
            )
            .on_key_down(cx.listener(Self::on_key))
            .child(page)
    }
}
