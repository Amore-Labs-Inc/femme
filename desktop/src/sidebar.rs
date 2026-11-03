//! The left sidebar: the daemon's live sessions only, newest first, the one the centre shows marked.
//! Owned by the sidebar agent (docs/desktop-ui.md §sidebar).
//!
//! What the shell does with it:
//!
//! - [`Sidebar::new`] takes the shared [`Net`]; the list follows `Net::model` by itself (an
//!   observer, no polling) and redraws only when a row, a dot or the link changed.
//! - The sidebar sizes itself: [`Sidebar::width`] is [`WIDTH`] px expanded and [`STRIP`] px collapsed,
//!   and it takes the height it is given. Mount it as a flex child (or give its column that width).
//! - Subscribe to [`OpenSession`] (a click on a row, Enter on the cursor) and to
//!   [`CollapsedChanged`] (the sidebar's own `‹`/`›` button and a click on a dot of the strip change
//!   it too, so the shell can follow with its column).
//! - [`Sidebar::set_selected`] marks the session the centre shows (call it when `Net::current`
//!   changes); [`Sidebar::set_collapsed`] / [`Sidebar::collapsed`] are what ctrl-b drives.
//! - The sidebar is [`Focusable`]: with the keyboard on it, Up/Down/Home/End move a cursor and Enter
//!   opens the session under it.
//!
//! `+ shell` (the empty state and the foot of the list) creates a shell session in the daemon's
//! default directory and makes it the current one on [`Net`].

use femme_proto::rpc::{Req, Resp};
use femme_proto::state::{Agent, Attention, Mode, Session, SessionStatus};
use femme_proto::Sid;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, relative, App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Pixels,
    Render, Rgba, ScrollHandle, SharedString, Stateful, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window,
};
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::tooltip::Tooltip;

use crate::net::{self, Link, Model, Net};
use crate::theme::{
    BAD, BODY_SIZE, DESK, DONE, FONT_MONO, FONT_PIXEL, FONT_UI, HOVER, INK, META_SIZE, MUTED,
    PAPER, PAUSE, QUEUE, RULE, TB_SIZE, WORK,
};
use crate::ui::{button, dot, meta, ruled, ruled_sides, tb};

/// Width of the expanded sidebar, in logical pixels.
pub const WIDTH: f32 = 240.;
/// Width of the collapsed sidebar (a strip of status dots), in logical pixels.
pub const STRIP: f32 = 28.;
// The strip must be narrower than the list, or collapsing would widen the sidebar.
const _: () = assert!(STRIP < WIDTH);
/// Height of a row of the expanded list.
const ROW_H: f32 = 46.;
/// Height of a dot cell of the collapsed strip.
const CELL_H: f32 = 28.;
/// The chord the shell binds to collapse and expand, for the hint at the foot of the list.
const TOGGLE_KEY: &str = if cfg!(target_os = "macos") {
    "cmd-b"
} else {
    "ctrl-b"
};

/// The user picked a session (a click on its row or dot, or Enter on the keyboard cursor).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenSession(pub Sid);

/// The sidebar was collapsed (`true`) or expanded (`false`): by [`Sidebar::set_collapsed`], by its
/// own `‹`/`›` button, or by a click on a dot of the strip. It is [`Sidebar::width`] that changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollapsedChanged(pub bool);

/// The status colour of a session's dot: what it is doing, or what it waits for.
pub fn status_color(s: &Session) -> Rgba {
    match (s.status, s.attention) {
        (SessionStatus::Working, _) => WORK,
        (SessionStatus::Blocked, _) | (_, Attention::Blocked) => PAUSE,
        (SessionStatus::Idle | SessionStatus::Starting, _) => QUEUE,
        // A crash is as much a failure as a lost holder (a deviation from the plain mapping: DONE
        // would paint a dead session as finished).
        (
            SessionStatus::Interrupted
            | SessionStatus::Paused
            | SessionStatus::Crashed
            | SessionStatus::Failed,
            _,
        ) => BAD,
        (SessionStatus::Closed, _) => DONE,
    }
}

/// The agent, as the second line of a row says it.
fn agent_name(a: Agent) -> &'static str {
    match a {
        Agent::Shell => "shell",
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Pi => "pi",
    }
}

/// A directory as a row says it: the home directory of the box (`/home/<user>`, `/Users/<user>`,
/// `/root`) is `~`, and a trailing slash goes.
fn short_dir(cwd: &str) -> String {
    let path = match cwd.trim_end_matches('/') {
        "" => cwd,
        trimmed => trimmed,
    };
    let under_home = ["/home/", "/Users/"]
        .iter()
        .find_map(|root| {
            let (user, tail) = path
                .strip_prefix(root)?
                .split_once('/')
                .unwrap_or((&path[root.len()..], ""));
            (!user.is_empty()).then_some(tail)
        })
        .or_else(|| match path {
            "/root" => Some(""),
            p => p.strip_prefix("/root/"),
        });
    match under_home {
        Some("") => "~".into(),
        Some(tail) => format!("~/{tail}"),
        None => path.to_owned(),
    }
}

/// What one row shows, worked out from the model so the order and the words are testable without
/// a window.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    sid: Sid,
    dot: Rgba,
    title: String,
    agent: &'static str,
    dir: String,
}

impl Row {
    fn of(s: &Session) -> Self {
        Self {
            sid: s.id,
            dot: status_color(s),
            title: net::title(s),
            agent: agent_name(s.agent),
            dir: short_dir(&s.cwd),
        }
    }
}

/// The rows of the list: the live sessions, newest first.
fn rows(model: &Model) -> Vec<Row> {
    model.live().into_iter().map(Row::of).collect()
}

/// Everything the list is drawn from. The sidebar redraws when this changes, not whenever `Net`
/// does (the round trip moves about once a second and is not shown here).
#[derive(Clone, Debug, Default, PartialEq)]
struct Shown {
    rows: Vec<Row>,
    link: Link,
}

impl Shown {
    fn of(model: &Model) -> Self {
        Self {
            rows: rows(model),
            link: model.link.clone(),
        }
    }

    /// The empty list's one line: nothing yet to show says what it waits for.
    fn note(&self) -> &'static str {
        match self.link {
            Link::Up(_) => "no sessions",
            Link::Down(_) => "offline",
            _ => "connecting…",
        }
    }

    /// Whether a session can be created now.
    fn can_create(&self) -> bool {
        matches!(self.link, Link::Up(_))
    }
}

/// Where the keyboard cursor goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Go {
    Up,
    Down,
    First,
    Last,
}

/// The session the cursor lands on when it moves from `from` over `ids` (newest first): one row,
/// stopping at the ends; with no cursor yet (or one on a session that is gone) down is the first
/// row and up the last.
fn step(ids: &[Sid], from: Option<Sid>, go: Go) -> Option<Sid> {
    let last = ids.len().checked_sub(1)?;
    let at = from.and_then(|f| ids.iter().position(|id| *id == f));
    let to = match (go, at) {
        (Go::First, _) | (Go::Down, None) => 0,
        (Go::Last, _) | (Go::Up, None) => last,
        (Go::Up, Some(i)) => i.saturating_sub(1),
        (Go::Down, Some(i)) => (i + 1).min(last),
    };
    Some(ids[to])
}

/// The width of the sidebar, in logical pixels.
fn extent(collapsed: bool) -> f32 {
    if collapsed {
        STRIP
    } else {
        WIDTH
    }
}

/// The pink of a row under the pointer: the selection's colour, lighter, so the selected row stays
/// the strongest.
fn hover_tint() -> Rgba {
    Rgba { a: 0.4, ..HOVER }
}

/// The 3px ink bar on the left edge of the selected row.
fn edge() -> Div {
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .w(px(3.))
        .bg(INK)
}

/// The ink outline of the keyboard cursor.
fn ring() -> Div {
    ruled(div().absolute().inset_0())
}

/// A black title bar without a label (the collapsed strip has no room for one).
fn bare_bar() -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .py(px(3.))
        .bg(INK)
        .text_color(DESK)
        .font_family(FONT_PIXEL)
        .text_size(TB_SIZE)
        .line_height(relative(1.))
}

/// The live sessions of the daemon as a list, or as a strip of dots when collapsed.
pub struct Sidebar {
    /// The daemon's sessions; without one the list is empty.
    net: Option<Entity<Net>>,
    /// What the list shows now.
    shown: Shown,
    /// The session the centre shows.
    selected: Option<Sid>,
    /// The keyboard cursor.
    cursor: Option<Sid>,
    collapsed: bool,
    /// A `+ shell` request is out: a second click would queue a second session.
    creating: bool,
    focus: FocusHandle,
    scroll: ScrollHandle,
    _net: Option<Subscription>,
    /// Redraws on focus in and out, so the cursor shows only while the keyboard is here.
    _focus: Vec<Subscription>,
}

impl EventEmitter<OpenSession> for Sidebar {}
impl EventEmitter<CollapsedChanged> for Sidebar {}

impl Sidebar {
    /// A sidebar with its own focus stop, listing the live sessions `net` knows of (none without
    /// one). It starts expanded with nothing selected.
    pub fn new(net: Option<Entity<Net>>, cx: &mut Context<Self>) -> Self {
        let watch = net
            .as_ref()
            .map(|n| cx.observe(n, |this, _, cx| this.refresh(cx)));
        let shown = net
            .as_ref()
            .map(|n| Shown::of(&n.read(cx).model))
            .unwrap_or_default();
        Self {
            net,
            shown,
            selected: None,
            cursor: None,
            collapsed: false,
            creating: false,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            _net: watch,
            _focus: Vec::new(),
        }
    }

    /// Marks the session the centre shows (`None`: none) and brings it into view. The keyboard
    /// cursor goes to it too.
    pub fn set_selected(&mut self, sid: Option<Sid>, cx: &mut Context<Self>) {
        if self.selected == sid {
            return;
        }
        self.selected = sid;
        self.cursor = sid;
        self.reveal(sid);
        cx.notify();
    }

    /// Whether the sidebar is the strip of dots.
    pub fn collapsed(&self) -> bool {
        self.collapsed
    }

    /// Collapses the sidebar to a strip of status dots, or expands it again. A change is announced
    /// as [`CollapsedChanged`].
    pub fn set_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if self.collapsed == collapsed {
            return;
        }
        self.collapsed = collapsed;
        // The rows of the two forms differ in height: start over at the top, on the session on show.
        self.scroll.set_offset(gpui::Point::default());
        self.reveal(self.selected);
        cx.emit(CollapsedChanged(collapsed));
        cx.notify();
    }

    /// How wide the sidebar is now: [`WIDTH`], or [`STRIP`] when collapsed.
    pub fn width(&self) -> Pixels {
        px(extent(self.collapsed))
    }

    /// The empty list's one line. With no daemon at all there is nothing to wait for.
    fn note(&self) -> &'static str {
        if self.net.is_none() {
            "no sessions"
        } else {
            self.shown.note()
        }
    }

    /// Whether `+ shell` is offered: there is a daemon and it is connected.
    fn can_create(&self) -> bool {
        self.net.is_some() && self.shown.can_create()
    }

    /// Redraws when what the list shows changed.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(net) = &self.net else { return };
        let shown = Shown::of(&net.read(cx).model);
        if shown != self.shown {
            // A session made current before its row arrived (a `+ shell` answer beats the row) is
            // scrolled to once the row is here.
            let was_row = self
                .selected
                .is_some_and(|s| self.shown.rows.iter().any(|r| r.sid == s));
            self.shown = shown;
            if !was_row {
                self.reveal(self.selected);
            }
            cx.notify();
        }
    }

    /// Scrolls the list so `sid`, if it is a row, is in view (at the next frame).
    fn reveal(&self, sid: Option<Sid>) {
        let at = sid.and_then(|s| self.shown.rows.iter().position(|r| r.sid == s));
        if let Some(ix) = at {
            self.scroll.scroll_to_item(ix);
        }
    }

    /// Opens `sid`: the cursor goes to it and the shell is told.
    fn open(&mut self, sid: Sid, cx: &mut Context<Self>) {
        self.cursor = Some(sid);
        cx.emit(OpenSession(sid));
        cx.notify();
    }

    /// Moves the cursor, from where it is, else from the selected session.
    fn go(&mut self, go: Go, cx: &mut Context<Self>) {
        let ids: Vec<Sid> = self.shown.rows.iter().map(|r| r.sid).collect();
        if let Some(to) = step(&ids, self.cursor.or(self.selected), go) {
            self.cursor = Some(to);
            self.reveal(Some(to));
            cx.notify();
        }
    }

    /// The keyboard arrives: the cursor shows on the session on show, else on the first row.
    fn land(&mut self, cx: &mut Context<Self>) {
        let row = |s: Option<Sid>| s.filter(|s| self.shown.rows.iter().any(|r| r.sid == *s));
        let at = row(self.cursor)
            .or(row(self.selected))
            .or(self.shown.rows.first().map(|r| r.sid));
        self.cursor = at;
        self.reveal(at);
        cx.notify();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        if k.modifiers.modified() {
            return;
        }
        match k.key.as_str() {
            "up" => self.go(Go::Up, cx),
            "down" => self.go(Go::Down, cx),
            "home" => self.go(Go::First, cx),
            "end" => self.go(Go::Last, cx),
            "enter" => match self.cursor {
                Some(sid) => self.open(sid, cx),
                None => return,
            },
            _ => return,
        }
        cx.stop_propagation();
    }

    /// `+ shell`: a shell session in the daemon's default directory, which becomes the current one.
    fn create_shell(&mut self, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        if self.creating {
            return;
        }
        self.creating = true;
        cx.notify();
        let sent = net.update(cx, |n, cx| {
            n.run(
                Req::SessionCreate {
                    cwd: String::new(),
                    agent: Agent::Shell,
                    mode: Mode::Tui,
                    label: None,
                    name: None,
                    prompt: None,
                },
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            let answer = sent.await;
            this.update(cx, |this, cx| {
                this.creating = false;
                if let Some(Resp::Session(sid)) = answer {
                    net.update(cx, |n, cx| n.set_current(Some(sid), cx));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Subscribes to focus in and out, once the window is known: the cursor shows while the
    /// keyboard is on the sidebar.
    fn watch_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._focus.is_empty() {
            let handle = self.focus.clone();
            self._focus = vec![
                cx.on_focus_in(&handle, window, |this, _, cx| this.land(cx)),
                cx.on_focus_out(&handle, window, |_, _, _, cx| cx.notify()),
            ];
        }
    }

    /// The black bar: "sessions" and the button that collapses, or just the button that expands.
    fn title_bar(&self, cx: &Context<Self>) -> Div {
        let collapsed = self.collapsed;
        let toggle = div()
            .id("sb-toggle")
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(22.))
            .cursor_pointer()
            .text_color(PAPER)
            .hover(|s| s.bg(HOVER))
            .tooltip(move |window, cx| {
                let text = if collapsed {
                    "show sessions"
                } else {
                    "hide sessions"
                };
                Tooltip::new(text).build(window, cx)
            })
            .on_click(cx.listener(|this, _, _, cx| this.set_collapsed(!this.collapsed, cx)))
            .child(if collapsed { "›" } else { "‹" });
        if collapsed {
            bare_bar().justify_center().child(toggle)
        } else {
            tb("sessions").child(div().flex_1()).child(toggle)
        }
    }

    /// The `+ shell` button.
    fn new_button(&self, cx: &Context<Self>) -> Stateful<Div> {
        let b = button("sb-new", "+ shell")
            .h(px(28.))
            .on_click(cx.listener(|this, _, _, cx| this.create_shell(cx)));
        if self.creating {
            b.opacity(0.5)
        } else {
            b
        }
    }

    /// A row of the expanded list: status dot, title, and agent and directory below it.
    fn row(&self, r: &Row, focused: bool, cx: &Context<Self>) -> Stateful<Div> {
        let sid = r.sid;
        let on = self.selected == Some(sid);
        let cursor = focused && self.cursor == Some(sid);
        let second = div()
            .flex()
            .gap(px(6.))
            .font_family(FONT_MONO)
            .text_size(META_SIZE)
            .line_height(px(16.))
            .text_color(if on { INK } else { MUTED })
            .child(div().flex_none().child(r.agent))
            // The end of a path says more than its start: when it does not fit, the start goes.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_start()
                    .child(r.dir.clone()),
            );
        div()
            .id(SharedString::from(format!("sb-{sid}")))
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(9.))
            .w_full()
            .h(px(ROW_H))
            .pl(px(13.))
            .pr(px(8.))
            .cursor_pointer()
            .when(on, |d| d.bg(HOVER))
            .when(!on, |d| d.hover(|s| s.bg(hover_tint())))
            .on_click(cx.listener(move |this, _, _, cx| this.open(sid, cx)))
            .when(on, |d| d.child(edge()))
            .child(dot(r.dot))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_size(BODY_SIZE)
                            .line_height(px(20.))
                            .text_color(INK)
                            .child(r.title.clone()),
                    )
                    .child(second),
            )
            .when(cursor, |d| d.child(ring()))
    }

    /// A dot of the collapsed strip. A click opens the session and expands the sidebar.
    fn cell(&self, r: &Row, focused: bool, cx: &Context<Self>) -> Stateful<Div> {
        let sid = r.sid;
        let on = self.selected == Some(sid);
        let cursor = focused && self.cursor == Some(sid);
        let tip = SharedString::from(format!("{} ({} {})", r.title, r.agent, r.dir));
        div()
            .id(SharedString::from(format!("sbd-{sid}")))
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .w_full()
            .h(px(CELL_H))
            .cursor_pointer()
            .when(on, |d| d.bg(HOVER))
            .when(!on, |d| d.hover(|s| s.bg(hover_tint())))
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_collapsed(false, cx);
                this.open(sid, cx);
            }))
            .when(on, |d| d.child(edge()))
            .child(dot(r.dot))
            .when(cursor, |d| d.child(ring()))
    }

    /// The area under the title bar that scrolls when the rows outgrow it, with a scrollbar.
    fn scroller(&self, id: &'static str, rows: Vec<Stateful<Div>>) -> Div {
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id(id)
                    .size_full()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(rows),
            )
            .vertical_scrollbar(&self.scroll)
    }

    /// The expanded body: the rows and the foot with `+ shell`, or the empty state.
    fn list(&self, focused: bool, cx: &Context<Self>) -> Div {
        let shown = &self.shown;
        if shown.rows.is_empty() {
            return div()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(8.))
                .p(px(12.))
                .child(div().text_color(MUTED).child(self.note()))
                .when(self.can_create(), |d| d.child(self.new_button(cx)));
        }
        let rows = shown
            .rows
            .iter()
            .map(|r| self.row(r, focused, cx))
            .collect();
        let foot = ruled_sides(div(), RULE, px(0.), px(0.), px(0.))
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .p(px(6.))
            .when(self.can_create(), |d| d.child(self.new_button(cx)))
            .child(meta(TOGGLE_KEY));
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(self.scroller("sb-rows", rows))
            .child(foot)
    }

    /// The collapsed body: one dot per live session.
    fn strip(&self, focused: bool, cx: &Context<Self>) -> Div {
        let cells = self
            .shown
            .rows
            .iter()
            .map(|r| self.cell(r, focused, cx))
            .collect();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(self.scroller("sb-dots", cells))
    }
}

impl Focusable for Sidebar {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.watch_focus(window, cx);
        let focused = self.focus.is_focused(window);
        let body = if self.collapsed {
            self.strip(focused, cx)
        } else {
            self.list(focused, cx)
        };
        ruled(div())
            .id("sidebar")
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus, cx);
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(Self::on_key))
            .flex()
            .flex_col()
            .flex_none()
            .w(self.width())
            .h_full()
            .overflow_hidden()
            .bg(PAPER)
            .font_family(FONT_UI)
            .text_size(BODY_SIZE)
            .line_height(relative(1.3))
            .text_color(INK)
            .child(self.title_bar(cx))
            .child(body)
    }
}
