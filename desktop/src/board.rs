//! The drawing board: an Excalidraw-style canvas over the centre for quick diagrams that go straight
//! to Claude. Owned by the board agent (docs/desktop-ui.md §board).
//!
//! Draw rectangles, ellipses, lines, arrows, freehand strokes and text, move and restyle them,
//! zoom and pan, and send the picture to Claude as a PNG attachment (`ctrl-enter`) or copy it to
//! the clipboard (on Linux through `wl-copy` or `xclip`: gpui 0.3 has no picture clipboard there).
//! The drawing stays in memory when the board closes and is saved as JSON under
//! `$XDG_STATE_HOME/femme/board.json` (half a second after the last change), so it is there on the
//! next start.
//!
//! # Mounting
//!
//! The shell makes one entity, `cx.new(|cx| Board::new(cx))`, and renders it in a layer as big as
//! the window (an absolute, `inset_0` child of the root: the board's own root is `absolute` too,
//! and empty while it is closed). It calls [`Board::open`] from its menu and subscribes to
//! [`BoardEvent`]:
//!
//! ```ignore
//! cx.subscribe(&board, |this, _, ev: &BoardEvent, cx| match ev {
//!     // The picture is a PNG: the composer attaches it to the selected session's draft.
//!     BoardEvent::Send { name, png } => this
//!         .composer
//!         .update(cx, |c, cx| c.add_attachment(name.clone(), png.clone(), cx)),
//!     // The board closed by itself (Esc, `‹`, a click outside, after a send): focus goes back.
//!     BoardEvent::Closed => this.focus_composer(cx),
//! })
//! ```
//!
//! The board takes the keyboard when it opens and gives it back to whatever had it when it closes
//! by itself. Its keys (all handled while it is open, none leak to the shell): `1`-`7` or
//! `v r o a l p t` pick a tool, `esc` goes back (a drag, then the tool, then the selection, then
//! closes), `ctrl-z` / `ctrl-shift-z` undo and redo, `ctrl-a` `ctrl-d` `ctrl-x` `ctrl-c` `ctrl-v`
//! select all, duplicate, cut, copy and paste shapes (`ctrl-c` with nothing selected copies the
//! picture), `delete`, the arrows nudge (`shift` by ten), `ctrl-enter` sends, `ctrl-0` `ctrl-+`
//! `ctrl--` `shift-1` zoom and fit, `space`-drag or the wheel pans, `ctrl`-wheel zooms.
//!
//! # How it is built
//!
//! Everything that needs thinking is plain Rust with no window, tested on its own: the [`scene`]
//! and its undo, the [`editor`] state machine (tools, selection, drags, keys), the [`geometry`] and
//! [`stroke`] of the look (sketchy or clean), the [`drawlist`] of one frame and the PNG [`export`],
//! which draws the same geometry the screen shows. This file and its private modules are the thin
//! gpui layer: events in, the draw list out.

pub mod drawlist;
pub mod editor;
pub mod export;
pub mod font;
pub mod geom;
pub mod geometry;
pub mod icons;
pub mod palette;
pub mod scene;
pub mod store;
pub mod stroke;
pub mod textedit;
pub mod view;

mod chrome;
mod clip;
mod input;
mod paint;
mod persist;
mod skin;
mod toolbar;

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    div, App, Bounds, Context, EventEmitter, FocusHandle, Focusable, IntoElement, Pixels, Render,
    SharedString, Task, Window,
};

use editor::Editor;
use store::Store;

/// What the board tells the shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoardEvent {
    /// The user sent the drawing to Claude: a PNG (2x, at most 4096 px on a side) named `name`
    /// (`diagram-1.png`, `diagram-2.png`, ...; the count survives restarts). The shell hands both
    /// to the composer, which attaches them to the selected session's draft:
    /// `Composer::add_attachment(name: String, data: Vec<u8>, cx)`. The board closes right after,
    /// so a [`BoardEvent::Closed`] follows.
    Send {
        /// The file name for the attachment.
        name: String,
        /// The picture, PNG bytes.
        png: Vec<u8>,
    },
    /// The board closed: by itself (Esc, `‹`, a click outside, after a send) or through
    /// [`Board::close`]. The shell hides its layer.
    Closed,
}

/// A line of feedback under the canvas: "copied the picture", "could not save the board: ...".
struct Notice {
    /// What to say.
    text: SharedString,
    /// It is bad news: shown in the danger colour.
    bad: bool,
}

/// The drawing board.
pub struct Board {
    /// The drawing and everything the user does to it.
    editor: Editor,
    /// Showing (the layer is drawn and has the keyboard).
    open: bool,
    /// The keyboard focus of the board.
    focus: FocusHandle,
    /// What had the keyboard when the board opened, to give it back.
    restore: Option<FocusHandle>,
    /// Take the keyboard in the next frame.
    want_focus: bool,
    /// Where the canvas was painted in the last frame, in window pixels: pointer positions are
    /// made relative to it, and the editor is told when its size changes.
    canvas: Rc<Cell<Bounds<Pixels>>>,
    /// Where the drawing is kept.
    store: Store,
    /// The editor's change count when the board last reacted to it.
    seen_edits: u64,
    /// The drawing changed and is not on disk yet.
    dirty: bool,
    /// The half second before a save: dropping it cancels the save.
    save: Option<Task<()>>,
    /// A file is being written.
    writing: bool,
    /// The drawing changed again while a file was being written: write once more.
    rewrite: bool,
    /// Why the saved drawing was not used at start, to say when the board opens.
    warning: Option<String>,
    /// `clear` was clicked once: the title bar asks "clear the board?".
    confirm_clear: bool,
    /// The line of feedback, and the timer that takes it away.
    notice: Option<Notice>,
    /// Removes the notice after a few seconds.
    notice_timer: Option<Task<()>>,
}

impl EventEmitter<BoardEvent> for Board {}

impl Focusable for Board {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Board {
    /// A closed board holding the drawing saved by the last run (an empty one if there is none).
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::with_store(Store::standard(), cx)
    }

    /// A closed board that keeps its drawing in `store`: for tries and screenshots that must not
    /// touch the user's own file.
    pub fn with_store(store: Store, cx: &mut Context<Self>) -> Self {
        // A board that was never saved starts hand-drawn; after that the scene remembers the choice.
        let fresh = store.path().is_none_or(|p| !p.exists());
        let loaded = store.load();
        let mut scene = loaded.scene;
        if fresh {
            scene.sketchy = true;
        }
        let editor = Editor::new(scene);
        Board {
            seen_edits: editor.edits(),
            editor,
            open: false,
            focus: cx.focus_handle(),
            restore: None,
            want_focus: false,
            canvas: Rc::new(Cell::new(Bounds::default())),
            store,
            dirty: false,
            save: None,
            writing: false,
            rewrite: false,
            warning: loaded.warning,
            confirm_clear: false,
            notice: None,
            notice_timer: None,
        }
    }

    /// Shows the board and gives it the keyboard.
    pub fn open(&mut self, cx: &mut Context<Self>) {
        if self.open {
            return;
        }
        self.open = true;
        self.want_focus = true;
        self.restore = None;
        self.confirm_clear = false;
        self.notice = None;
        if let Some(why) = self.warning.take() {
            self.say(why, true, cx);
        }
        cx.notify();
    }

    /// Hides the board. A text being typed is kept, a drag in progress ends, and a drawing that is
    /// not on disk yet is saved. The drawing stays in memory. Emits [`BoardEvent::Closed`].
    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.editor.blur();
        self.changed(cx);
        self.save_now(cx);
        self.open = false;
        self.confirm_clear = false;
        cx.emit(BoardEvent::Closed);
        cx.notify();
    }

    /// Whether the board is showing.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Closes the board on the user's say-so and gives the keyboard back.
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let back = self.restore.take();
        self.close(cx);
        if let Some(handle) = back {
            window.focus(&handle, cx);
        }
    }

    /// The board keeps the keyboard: it takes it when it opens, and takes it back when whatever
    /// had it goes away.
    fn sync_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.want_focus || !self.focus.contains_focused(window, cx) {
            self.want_focus = false;
            cx.defer_in(window, |t, window, cx| {
                if t.restore.is_none() {
                    t.restore = window.focused(cx).filter(|h| !t.focus.contains(h, window));
                }
                window.focus(&t.focus, cx);
            });
        }
    }
}

impl Render for Board {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        self.sync_focus(window, cx);
        self.layer(cx)
    }
}
