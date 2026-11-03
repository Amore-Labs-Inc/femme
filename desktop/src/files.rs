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

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use femme_proto::Sid;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, relative, uniform_list, AnyElement, App, AppContext as _, ClickEvent, Context, Div,
    Entity, FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement, KeyDownEvent,
    MouseButton, ParentElement as _, Render, Rgba, ScrollHandle, ScrollStrategy, Stateful,
    StatefulInteractiveElement as _, Styled as _, Task, UniformListScrollHandle, Window,
};
use gpui_component::scroll::Scrollbar;

use crate::net::Net;
use crate::theme::{
    BAD, DESK, FONT_MONO, FONT_PIXEL, FONT_UI, INK, META_SIZE, MUTED, NEON, PANEL, PAPER, RULE,
    TB_SIZE,
};
use crate::ui::{button, ruled_sides, st};
use crate::viewer::FileView;

pub mod cache;
pub mod plan;
pub mod size;
pub mod source;
pub mod tree;

use cache::FileCache;
use plan::{plan, Plan};
use size::fmt_size;
use source::{Source, MAX_READ};
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
}

/// A file on show in the viewer page.
struct Opened {
    name: String,
    size: usize,
    view: Entity<FileView>,
}

/// What the panel shows.
enum Page {
    /// The tree.
    Tree,
    /// One file.
    Viewer(Opened),
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
        if matches!(self.page, Page::Viewer(_)) {
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
                        let text = format!("can't list {}: {why}", tree::under(&t.cwd, &path));
                        t.notice = Some(Notice::ListFailed {
                            path: path.clone(),
                            text,
                        });
                    }
                    Ok(_) => {
                        if matches!(&t.notice, Some(Notice::ListFailed { path: p, .. }) if *p == path)
                        {
                            t.notice = None;
                        }
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
                    self.show_file(name, data, cx);
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
                        t.show_file(name, data, cx);
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

    /// Switches to the viewer page with `data`.
    fn show_file(&mut self, name: String, data: Arc<Vec<u8>>, cx: &mut Context<Self>) {
        let size = data.len();
        let view = cx.new(|cx| FileView::new(name.clone(), data.to_vec(), cx));
        self.view_scroll = ScrollHandle::new();
        self.page = Page::Viewer(Opened { name, size, view });
        cx.notify();
    }

    /// Whether something is on its way (the spinner turns).
    fn busy(&self) -> bool {
        self.tree.busy() || matches!(self.notice, Some(Notice::Reading { .. }))
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
            Kind::Loading | Kind::Empty | Kind::Failed => {}
        }
    }

    fn on_key(&mut self, e: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &e.keystroke;
        // Chords belong to the shell.
        if k.modifiers.control || k.modifiers.alt || k.modifiers.platform {
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
            Notice::Reading { name, len } => strip(PANEL)
                .child(spinner(self.spin()))
                .child(text(format!("reading {name} ({})", fmt_size(*len))))
                .into_any_element(),
            Notice::Confirm { path, len, .. } => {
                let (path, len) = (path.clone(), *len);
                strip(PANEL)
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
            Notice::TooBig { name, len } => strip(PANEL)
                .child(st("too big", BAD))
                .child(text(format!(
                    "{name} is {}; the daemon reads at most {}",
                    fmt_size(*len),
                    fmt_size(MAX_READ)
                )))
                .child(dismiss(cx))
                .into_any_element(),
            Notice::ListFailed { text: why, .. } | Notice::Error(why) => {
                strip(Rgba { a: 0.12, ..BAD })
                    .child(st("error", BAD))
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
                .bg(PANEL),
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
                .text_color(MUTED)
                .child(tree::under(&self.cwd, self.tree.root())),
        )
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
                    .text_color(MUTED)
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
        let clickable = !matches!(row.kind, Kind::Loading | Kind::Empty);
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
                d.bg(if focused { NEON } else { cursor_idle() })
            })
            .when(!selected && clickable, |d| d.hover(|s| s.bg(PANEL)));
        let marker = |glyph: &'static str| {
            div()
                .w(px(12.))
                .flex_none()
                .font_family(FONT_MONO)
                .text_size(px(13.))
                .text_color(MUTED)
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
        let color = if row.hidden() { MUTED } else { INK };
        let line = match row.kind {
            Kind::Up => base
                .child(marker(""))
                .child(name("..".to_owned(), MUTED).font_family(FONT_MONO)),
            Kind::Dir { open } => base
                .child(marker(if open { "▾" } else { "▸" }))
                .child(name(row.name().to_owned(), color).font_weight(FontWeight::SEMIBOLD)),
            Kind::File { len } => base
                .child(marker(""))
                .child(name(row.name().to_owned(), color))
                .child(
                    div()
                        .flex_none()
                        .pl(px(6.))
                        .font_family(FONT_MONO)
                        .text_size(META_SIZE)
                        .text_color(MUTED)
                        .child(fmt_size(len)),
                ),
            Kind::Loading => base
                .child(spinner(spin))
                .child(note("loading…".to_owned(), MUTED)),
            Kind::Empty => base
                .child(marker(""))
                .child(note("(empty)".to_owned(), MUTED)),
            Kind::Failed => base
                .child(marker("!"))
                .child(note("could not list: click to try again".to_owned(), BAD)),
        };
        if !clickable {
            return line.cursor_default();
        }
        line
            // gpui paints the hover style from where the pointer is, but only repaints this view
            // when the pointer moves: a line that slid under a still pointer (a notice appeared,
            // the page changed) would stay grey after the pointer left. This listener also
            // fires for such moves of the layout, and the repaint it asks for is what clears it.
            .on_hover(cx.listener(|_, _, _, cx| cx.notify()))
            .on_click(
                cx.listener(move |t, e: &ClickEvent, _, cx| {
                    t.row_clicked(&row, e.click_count(), cx)
                }),
            )
    }

    /// The viewer page: a black title bar (`‹ back`, name, size) over the file.
    fn viewer_page(&self, open: &Opened, cx: &mut Context<Self>) -> Div {
        let bar = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .py(px(3.))
            .bg(INK)
            .text_color(DESK)
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
                    .text_color(PANEL)
                    .child(fmt_size(open.size as u64)),
            );
        // The scrollbar is a sibling of what scrolls: inside it, it would scroll away.
        let body = div()
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
            .child(Scrollbar::vertical(&self.view_scroll));
        div().size_full().flex().flex_col().child(bar).child(body)
    }
}

/// The cursor's line while the panel does not have the keyboard: a pale [`NEON`], apart from the
/// grey of the line under the pointer.
fn cursor_idle() -> Rgba {
    Rgba { a: 0.3, ..NEON }
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
            .bg(PAPER)
            .text_color(INK)
            .font_family(FONT_UI)
            .text_size(px(14.))
            .line_height(relative(1.3))
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)),
            )
            .on_key_down(cx.listener(Self::on_key))
            .child(page)
    }
}
