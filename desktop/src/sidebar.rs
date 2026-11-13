//! The left sidebar: the daemon's live sessions only, newest first, the one the centre shows marked.
//! Owned by the sidebar agent (docs/desktop-ui.md §sidebar).
//!
//! What the shell does with it:
//!
//! - [`Sidebar::new`] takes the shared [`Net`]; the list follows `Net::model` by itself (an
//!   observer, no polling) and redraws only when a row, a dot or the link changed.
//! - The sidebar sizes itself: [`Sidebar::width`] is [`WIDTH`] px expanded and [`STRIP`] px collapsed,
//!   and it takes the height it is given. Mount it as a flex child (or give its column that width).
//!   A `.cached(style)` mount lays out as that style alone, so the style must say
//!   `.h_full().w(sidebar.width())`, and the shell must redraw on [`CollapsedChanged`].
//! - Subscribe to [`OpenSession`] (a click on a row, Enter on the cursor) and to
//!   [`CollapsedChanged`] (the sidebar's own `‹`/`›` button and a click on a dot of the strip change
//!   it too, so the shell can follow with its column).
//! - [`Sidebar::set_selected`] marks the session the centre shows (call it when `Net::current`
//!   changes); [`Sidebar::set_collapsed`] / [`Sidebar::collapsed`] are what ctrl-b drives.
//! - The sidebar is [`Focusable`]: with the keyboard on it, Up/Down/Home/End move a cursor and Enter
//!   opens the session under it.
//!
//! `+ claude` (the empty state) creates a headless Claude session in the
//! daemon's default directory and makes it the current one on [`Net`]; a folder header's `+` does
//! the same in that folder. Shells start from the terminal panel's own `+`.

use std::collections::HashSet;

use femme_proto::rpc::{Req, Resp};
use femme_proto::state::{Agent, Attention, Session, SessionStatus};
use femme_proto::Sid;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, relative, App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Pixels,
    Render, Rgba, ScrollHandle, SharedString, Stateful, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window,
};
use gpui_component::input::Escape;
use gpui_component::scroll::ScrollableElement as _;
use gpui_component::tooltip::Tooltip;

use crate::net::{self, BoxId, Link, Model, Net};
use crate::session;
use crate::theme::{
    accent_soft, bad, bar, bar_text, done, ink, muted, on_accent_strong, paper, pause, queue, work,
    BODY_SIZE, FONT_MONO, FONT_PIXEL, FONT_UI, META_SIZE, RULE, TB_SIZE,
};
use crate::ui::{
    button, dot, icon, icon_button, meta, row, ruled, ruled_sides, st, title_bar, title_button,
    Glyph,
};

/// Width of the expanded sidebar, in logical pixels.
pub const WIDTH: f32 = 240.;
/// Width of the collapsed sidebar (a strip of status dots), in logical pixels.
pub const STRIP: f32 = 28.;
// The strip must be narrower than the list, or collapsing would widen the sidebar.
const _: () = assert!(STRIP < WIDTH);
/// Height of a row of the expanded list.
mod ctx;

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

/// The title bar's add project button: the shell opens the folder picker.
pub struct AddProject;

/// What a session's dot says: what it is doing, or what it waits for. Rows keep the lane, not its
/// colour, so a row built before a theme change is drawn in the new palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lane {
    Working,
    Blocked,
    Idle,
    Failed,
    Closed,
}

impl Lane {
    fn of(s: &Session) -> Self {
        match (s.status, s.attention) {
            (SessionStatus::Working, _) => Lane::Working,
            (SessionStatus::Blocked, _) | (_, Attention::Blocked) => Lane::Blocked,
            (SessionStatus::Idle | SessionStatus::Starting, _) => Lane::Idle,
            // A crash is as much a failure as a lost holder (a deviation from the plain mapping:
            // `Closed` would paint a dead session as finished).
            (
                SessionStatus::Interrupted
                | SessionStatus::Paused
                | SessionStatus::Crashed
                | SessionStatus::Failed,
                _,
            ) => Lane::Failed,
            (SessionStatus::Closed, _) => Lane::Closed,
        }
    }

    /// The lane's colour in the live palette.
    fn color(self) -> Rgba {
        match self {
            Lane::Working => work(),
            Lane::Blocked => pause(),
            Lane::Idle => queue(),
            Lane::Failed => bad(),
            Lane::Closed => done(),
        }
    }
}

/// The status colour of a session's dot, in the live palette: what it is doing, or what it waits for.
pub fn status_color(s: &Session) -> Rgba {
    Lane::of(s).color()
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

/// The name a project goes by: its folder's last part (`~` for home).
fn project_name(dir: &str) -> String {
    dir.rsplit('/')
        .find(|p| !p.is_empty())
        .unwrap_or(dir)
        .to_string()
}

/// One project: a folder and the live sessions in it, newest first.
#[derive(Clone, Debug, PartialEq)]
struct Group {
    cwd: String,
    name: String,
    dir: String,
    rows: Vec<Row>,
}

/// The rows by folder, the project of the newest session first (t3code's sidebar).
fn groups(rows: &[Row]) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for r in rows {
        match groups.iter_mut().find(|g| g.cwd == r.cwd) {
            Some(g) => g.rows.push(r.clone()),
            None => groups.push(Group {
                cwd: r.cwd.clone(),
                name: project_name(&r.dir),
                dir: r.dir.clone(),
                rows: vec![r.clone()],
            }),
        }
    }
    groups
}

/// What one row shows, worked out from the model so the order and the words are testable without
/// a window.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    sid: Sid,
    lane: Lane,
    title: String,
    /// The agent, or the provider of a model run through OpenGateway (`deepseek`).
    agent: String,
    dir: String,
    /// The folder as the daemon has it: what the project groups by.
    cwd: String,
    /// The session runs on this machine (`femmed local`): a `local` pill after the title.
    local: bool,
}

impl Row {
    fn of(s: &Session) -> Self {
        Self {
            sid: s.id,
            lane: Lane::of(s),
            title: net::title(s),
            agent: match s.model.as_deref().and_then(|m| m.split_once('/')) {
                Some((provider, _)) => provider.to_owned(),
                None => agent_name(s.agent).to_owned(),
            },
            dir: short_dir(&s.cwd),
            cwd: s.cwd.trim_end_matches('/').to_string(),
            local: false,
        }
    }
}

/// The rows of the list: the live sessions, newest first.
fn rows(model: &Model) -> Vec<Row> {
    model
        .live()
        .into_iter()
        .map(|s| Row {
            local: model.is_local(s.id),
            ..Row::of(s)
        })
        .collect()
}

/// Everything the list is drawn from. The sidebar redraws when this changes, not whenever `Net`
/// does (the round trip moves about once a second and is not shown here).
#[derive(Clone, Debug, Default, PartialEq)]
struct Shown {
    rows: Vec<Row>,
    link: Link,
    /// `femmed local` is connected: `+ this laptop` is offered.
    local_up: bool,
}

impl Shown {
    fn of(model: &Model) -> Self {
        Self {
            rows: rows(model),
            link: model.link.clone(),
            local_up: matches!(model.local_link, Some(Link::Up(_))),
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

    /// Whether a session can be created on this laptop now.
    fn can_create_local(&self) -> bool {
        self.local_up
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
        .bg(bar())
        .text_color(bar_text())
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
    /// The projects folded shut, by folder.
    folded: HashSet<String>,
    /// A `+ claude` request is out: a second click would queue a second session.
    creating: bool,
    /// The right-click menu open on a row.
    menu: Option<ctx::Menu>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    _net: Option<Subscription>,
    /// Redraws on focus in and out, so the cursor shows only while the keyboard is here.
    _focus: Vec<Subscription>,
}

impl EventEmitter<OpenSession> for Sidebar {}
impl EventEmitter<CollapsedChanged> for Sidebar {}
impl EventEmitter<AddProject> for Sidebar {}

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
            folded: HashSet::new(),
            creating: false,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            _net: watch,
            menu: None,
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

    /// Whether `+ claude` is offered: there is a daemon and it is connected.
    fn can_create(&self) -> bool {
        self.net.is_some() && self.shown.can_create()
    }

    /// Redraws when what the list shows changed.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(net) = &self.net else { return };
        let shown = Shown::of(&net.read(cx).model);
        if shown != self.shown {
            // A session made current before its row arrived (a `+ claude` answer beats the row) is
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
        let at = sid.and_then(|s| self.place(s));
        if let Some(ix) = at {
            self.scroll.scroll_to_item(ix);
        }
    }

    /// Where `sid` is among the children of the scroller: the strip has one dot per session, the
    /// list a header per project before its rows (none for a folded one's).
    fn place(&self, sid: Sid) -> Option<usize> {
        if self.collapsed {
            return self.shown.rows.iter().position(|r| r.sid == sid);
        }
        let mut ix = 0;
        for g in groups(&self.shown.rows) {
            ix += 1;
            if self.folded.contains(&g.cwd) {
                continue;
            }
            if let Some(i) = g.rows.iter().position(|r| r.sid == sid) {
                return Some(ix + i);
            }
            ix += g.rows.len();
        }
        None
    }

    /// The pointer entered or left a row or a dot. gpui paints the tint from the hit test but only
    /// redraws a cached view when its own bookkeeping flips, which a row that appears under a
    /// resting pointer (a click on a dot expands the list) never does: it would keep the tint after
    /// the pointer left. Redrawing here, once per change, clears it.
    fn hover_changed(&mut self, _: &bool, _: &mut Window, cx: &mut Context<Self>) {
        cx.notify();
    }

    /// Opens `sid`: the cursor goes to it and the shell is told.
    fn open(&mut self, sid: Sid, cx: &mut Context<Self>) {
        self.cursor = Some(sid);
        cx.emit(OpenSession(sid));
        cx.notify();
    }

    /// The sessions on the list, in its order: the folded projects' are not.
    fn visible(&self) -> Vec<Sid> {
        groups(&self.shown.rows)
            .into_iter()
            .filter(|g| self.collapsed || !self.folded.contains(&g.cwd))
            .flat_map(|g| g.rows.into_iter().map(|r| r.sid))
            .collect()
    }

    /// Moves the cursor, from where it is, else from the selected session.
    fn go(&mut self, go: Go, cx: &mut Context<Self>) {
        let ids = self.visible();
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
            "escape" => {
                if !self.close_menu(cx) {
                    return;
                }
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// `+ claude`, `+ this laptop` (`cwd` empty: the daemon's home directory) or a folder header's
    /// `+`: a headless Claude session there, on box `on`, which becomes the current one.
    fn create(&mut self, cwd: &str, on: BoxId, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        if self.creating {
            return;
        }
        self.creating = true;
        cx.notify();
        let sent = net.update(cx, |n, cx| n.run_on(on, session::new_claude(cwd), cx));
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

    /// The menu's `close`: closes the session (any agent; a crashed one too, whose holder is gone). The
    /// daemon's row change takes it off the list, and the centre moves on if it was on show.
    fn close(&mut self, sid: Sid, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        net.update(cx, |n, cx| n.run(Req::SessionClose { sid }, cx))
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
    fn header(&self, cx: &Context<Self>) -> Div {
        let collapsed = self.collapsed;
        let toggle = title_button(
            // One id per form: gpui keeps a tooltip by id, and "hide sessions" must not outlive the
            // click that turned the button into "show sessions".
            if collapsed { "sb-show" } else { "sb-hide" },
            if collapsed {
                Glyph::Forward
            } else {
                Glyph::Back
            },
        )
        .tooltip(move |window, cx| {
            let text = if collapsed {
                "show sessions"
            } else {
                "hide sessions"
            };
            Tooltip::new(text).build(window, cx)
        })
        .on_click(cx.listener(|this, _, _, cx| this.set_collapsed(!this.collapsed, cx)));
        if collapsed {
            bare_bar().justify_center().child(toggle)
        } else {
            let add = title_button("sb-add-project", Glyph::Plus)
                .tooltip(|window, cx| Tooltip::new("Add project").build(window, cx))
                .on_click(cx.listener(|_, _, _, cx| cx.emit(AddProject)));
            title_bar("sessions").child(add).child(toggle)
        }
    }

    /// The `+ claude` button: a session on the remote box.
    fn new_button(&self, cx: &Context<Self>) -> Stateful<Div> {
        let b = button("sb-new-claude", "+ claude")
            .on_click(cx.listener(|this, _, _, cx| this.create("", BoxId::Remote, cx)));
        if self.creating {
            b.opacity(0.5)
        } else {
            b
        }
    }

    /// The `+ this laptop` button: a Claude session on `femmed local`, in the home directory.
    fn new_local_button(&self, cx: &Context<Self>) -> Stateful<Div> {
        let b = button("sb-new-local", "+ this laptop")
            .on_click(cx.listener(|this, _, _, cx| this.create("", BoxId::Local, cx)));
        if self.creating {
            b.opacity(0.5)
        } else {
            b
        }
    }

    /// Whether `+ this laptop` is offered: `femmed local` is connected.
    fn can_create_local(&self) -> bool {
        self.net.is_some() && self.shown.can_create_local()
    }

    /// A row of the expanded list: status dot, title, and agent and directory below it.
    fn entry(&self, r: &Row, focused: bool, cx: &Context<Self>) -> Stateful<Div> {
        let sid = r.sid;
        let on = self.selected == Some(sid);
        let cursor = focused && self.cursor == Some(sid);
        // While the row is pressed it is neon and its labels take the colour that reads on neon:
        // the title inherits the row's, the second line has its own colour and follows the press
        // through the row's group.
        let press = SharedString::from(format!("sb-press-{sid}"));
        let second = div()
            .id(SharedString::from(format!("sb-2-{sid}")))
            .flex()
            .gap(px(6.))
            .font_family(FONT_MONO)
            .text_size(META_SIZE)
            .line_height(px(16.))
            .text_color(if on { ink() } else { muted() })
            .group_active(press.clone(), |s| s.text_color(on_accent_strong()))
            // The folder is the project's header above.
            .child(div().flex_none().child(r.agent.clone()));
        row(SharedString::from(format!("sb-{sid}")), on)
            .group(press)
            .flex_none()
            .gap(px(9.))
            .w_full()
            .h(px(ROW_H))
            .pl(px(13.))
            .pr(px(8.))
            .on_hover(cx.listener(Self::hover_changed))
            .on_click(cx.listener(move |this, _, _, cx| this.open(sid, cx)))
            // The right button opens the row's menu (rename, close) and gives the sidebar the keys.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.focus, cx);
                    this.open_menu(sid, cx);
                }),
            )
            .child(dot(r.lane.color()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(BODY_SIZE)
                                    .line_height(px(20.))
                                    .child(r.title.clone()),
                            )
                            .when(r.local, |d| d.child(st("local", muted()).flex_none())),
                    )
                    .child(second),
            )
            .when(cursor, |d| d.child(ring()))
            .children(self.menu_for(sid, cx))
    }

    /// A project's header: the chevron, its folder's name, and on hover `+` for a new Claude
    /// session there. A click folds it; a folded one says how many sessions it hides.
    fn project(&self, g: &Group, folded: bool, cx: &Context<Self>) -> Stateful<Div> {
        let cwd = g.cwd.clone();
        let fold = cwd.clone();
        let hover = SharedString::from(format!("sbp-g-{}", g.cwd));
        let tip = SharedString::from(g.dir.clone());
        let new_tip = SharedString::from(format!("New session in {}", g.name));
        // The folder is on the box of its newest session.
        let on = if g.rows.first().is_some_and(|r| r.local) {
            BoxId::Local
        } else {
            BoxId::Remote
        };
        div()
            .id(SharedString::from(format!("sbp-{}", g.cwd)))
            .group(hover.clone())
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .w_full()
            .h(px(32.))
            .mt(px(6.))
            .pl(px(8.))
            .pr(px(8.))
            .cursor_pointer()
            .hover(|s| s.bg(accent_soft()))
            .on_hover(cx.listener(Self::hover_changed))
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.folded.remove(&fold) {
                    this.folded.insert(fold.clone());
                }
                cx.notify();
            }))
            .child(icon(if folded { Glyph::Right } else { Glyph::Down }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(FONT_MONO)
                    .text_size(META_SIZE)
                    .text_color(ink())
                    .child(g.name.clone()),
            )
            .when(folded, |d| d.child(meta(g.rows.len().to_string())))
            .child(
                icon_button(
                    SharedString::from(format!("sbp-new-{}", g.cwd)),
                    Glyph::Plus,
                )
                .opacity(0.)
                .group_hover(hover, |s| s.opacity(1.))
                .tooltip(move |window, cx| Tooltip::new(new_tip.clone()).build(window, cx))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.create(&cwd, on, cx);
                })),
            )
    }

    /// A dot of the collapsed strip. A click opens the session and expands the sidebar.
    fn cell(&self, r: &Row, focused: bool, cx: &Context<Self>) -> Stateful<Div> {
        let sid = r.sid;
        let on = self.selected == Some(sid);
        let cursor = focused && self.cursor == Some(sid);
        let tip = SharedString::from(format!("{} ({} {})", r.title, r.agent, r.dir));
        row(SharedString::from(format!("sbd-{sid}")), on)
            .flex_none()
            .justify_center()
            .w_full()
            .h(px(CELL_H))
            .min_h(px(CELL_H))
            .px_0()
            .py_0()
            .on_hover(cx.listener(Self::hover_changed))
            .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_collapsed(false, cx);
                this.open(sid, cx);
            }))
            .child(dot(r.lane.color()))
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

    /// The expanded body: the rows and the foot with the ctrl-b hint, or the empty state.
    fn list(&self, focused: bool, cx: &Context<Self>) -> Div {
        let shown = &self.shown;
        if shown.rows.is_empty() {
            return div()
                .flex()
                .flex_col()
                .items_start()
                .gap(px(8.))
                .p(px(12.))
                .child(div().text_color(muted()).child(self.note()))
                .when(self.can_create(), |d| d.child(self.new_button(cx)))
                .when(self.can_create_local(), |d| {
                    d.child(self.new_local_button(cx))
                });
        }
        let mut rows = Vec::new();
        for g in groups(&shown.rows) {
            let folded = self.folded.contains(&g.cwd);
            rows.push(self.project(&g, folded, cx));
            if !folded {
                rows.extend(g.rows.iter().map(|r| self.entry(r, focused, cx)));
            }
        }
        let foot = ruled_sides(div(), RULE, px(0.), px(0.), px(0.))
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .p(px(6.))
            .child(meta(TOGGLE_KEY))
            .when(self.can_create_local(), |d| {
                d.child(self.new_local_button(cx))
            });
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
                    this.close_menu(cx);
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, _, cx| {
                    this.close_menu(cx);
                }),
            )
            // Esc in the rename box reaches here as the input's action.
            .on_action(cx.listener(|this, _: &Escape, _, cx| {
                if this.close_menu(cx) {
                    cx.stop_propagation();
                }
            }))
            .on_key_down(cx.listener(Self::on_key))
            .flex()
            .flex_col()
            .flex_none()
            // The shell's column says how wide (it slides when the sidebar collapses or expands);
            // the box fills it, so nothing shows between it and the centre on the way.
            .w_full()
            .h_full()
            .overflow_hidden()
            .bg(paper())
            .font_family(FONT_UI)
            .text_size(BODY_SIZE)
            .line_height(relative(1.3))
            .text_color(ink())
            .child(self.header(cx))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_client::Link as Transport;
    use femme_proto::state::Mode;
    use femme_proto::Id;
    use gpui::AppContext as _;

    fn session(n: u128, created: i64) -> Session {
        Session {
            id: Id(n),
            handle: format!("h{n}"),
            rev: 1,
            name: None,
            owner: "board".into(),
            agent: Agent::Shell,
            mode: Mode::Tui,
            cwd: "/home/u/work".into(),
            label: None,
            agent_sid: None,
            model: None,
            status: SessionStatus::Idle,
            attention: Attention::None,
            created,
            closed: None,
            permission: None,
            context: None,
            effort: None,
            goal: None,
        }
    }

    fn model(sessions: Vec<Session>) -> Model {
        let mut m = Model::default();
        for s in sessions {
            m.sessions.insert(s.id, s);
        }
        m
    }

    fn ids(rows: &[Row]) -> Vec<u128> {
        rows.iter().map(|r| r.sid.0).collect()
    }

    fn dot_of(status: SessionStatus, attention: Attention) -> Rgba {
        let mut s = session(1, 1);
        s.status = status;
        s.attention = attention;
        status_color(&s)
    }

    #[test]
    fn only_live_sessions_are_listed_newest_first() {
        let mut closed = session(3, 30);
        closed.closed = Some(31);
        let mut ended = session(4, 40);
        ended.status = SessionStatus::Closed;
        let m = model(vec![
            session(1, 10),
            session(2, 20),
            closed,
            ended,
            // Created in the same millisecond as 2: the later id is the newer.
            session(5, 20),
        ]);
        assert_eq!(ids(&rows(&m)), [5, 2, 1]);
        assert!(rows(&Model::default()).is_empty());
    }

    #[test]
    fn a_session_of_this_laptop_is_marked_local() {
        let mut m = model(vec![session(1, 10), session(2, 20)]);
        m.boxes.insert(Id(2), net::BoxId::Local);
        let rows = rows(&m);
        assert_eq!(
            rows.iter().map(|r| (r.sid.0, r.local)).collect::<Vec<_>>(),
            [(2, true), (1, false)]
        );
        // `+ this laptop` is offered while the local daemon is connected.
        assert!(!Shown::of(&m).can_create_local());
        m.local_link = Some(Link::Up(Transport::Local));
        assert!(Shown::of(&m).can_create_local());
        m.local_link = Some(Link::Down("refused".into()));
        assert!(!Shown::of(&m).can_create_local());
        // The mark is part of what the list is drawn from: placing a session redraws it.
        let before = Shown::of(&m);
        m.boxes.insert(Id(1), net::BoxId::Local);
        assert_ne!(Shown::of(&m), before);
    }

    #[test]
    fn a_row_has_the_title_the_agent_and_a_short_directory() {
        let mut named = session(1, 1);
        named.name = Some("nightly".into());
        named.agent = Agent::Claude;
        let mut labelled = session(2, 2);
        labelled.label = Some("fix the build".into());
        labelled.agent = Agent::Codex;
        labelled.cwd = "/srv/app/".into();
        let bare = session(3, 3);
        let m = model(vec![named, labelled, bare]);
        let rows = rows(&m);
        let say = |r: &Row| (r.title.clone(), r.agent.clone(), r.dir.clone());
        assert_eq!(
            say(&rows[0]),
            ("work".into(), "shell".into(), "~/work".into())
        );
        assert_eq!(
            say(&rows[1]),
            ("fix the build".into(), "codex".into(), "/srv/app".into())
        );
        assert_eq!(
            say(&rows[2]),
            ("nightly".into(), "claude".into(), "~/work".into())
        );
        // A model run through OpenGateway is named by its provider; a Claude model is not.
        let mut deep = session(4, 4);
        deep.agent = Agent::Claude;
        deep.model = Some("deepseek/deepseek-v4.1-flash-ultrafast".into());
        assert_eq!(Row::of(&deep).agent, "deepseek");
        deep.model = Some("opus".into());
        assert_eq!(Row::of(&deep).agent, "claude");
    }

    #[test]
    fn sessions_group_by_folder_newest_project_first() {
        let mut a = session(1, 10);
        a.cwd = "/home/u/femme".into();
        let mut b = session(2, 20);
        b.cwd = "/home/u/work/".into();
        let mut c = session(3, 30);
        c.cwd = "/home/u/femme/".into();
        let mut home = session(4, 5);
        home.cwd = "/home/u".into();
        let g = groups(&rows(&model(vec![a, b, c, home])));
        let say: Vec<(&str, Vec<u128>)> =
            g.iter().map(|g| (g.name.as_str(), ids(&g.rows))).collect();
        assert_eq!(
            say,
            [("femme", vec![3, 1]), ("work", vec![2]), ("~", vec![4])]
        );
    }

    #[test]
    fn the_dot_says_what_the_session_is_doing_or_waits_for() {
        use SessionStatus::*;
        assert_eq!(dot_of(Working, Attention::None), work());
        assert_eq!(dot_of(Blocked, Attention::None), pause());
        assert_eq!(dot_of(Idle, Attention::Blocked), pause());
        assert_eq!(dot_of(Idle, Attention::None), queue());
        assert_eq!(dot_of(Starting, Attention::None), queue());
        for s in [Interrupted, Paused, Crashed, Failed] {
            assert_eq!(dot_of(s, Attention::None), bad(), "{s:?}");
        }
        assert_eq!(dot_of(Closed, Attention::None), done());
        // The row carries the lane of its session and takes the colour when it is drawn.
        let mut s = session(1, 1);
        s.status = Working;
        assert_eq!(Row::of(&s).lane, Lane::Working);
        assert_eq!(Row::of(&s).lane.color(), work());
    }

    #[test]
    fn the_home_directory_is_a_tilde() {
        for (cwd, said) in [
            ("/home/u/work", "~/work"),
            ("/home/u/work/", "~/work"),
            ("/home/u", "~"),
            ("/home/u/", "~"),
            ("/Users/me/x/y", "~/x/y"),
            ("/root", "~"),
            ("/root/a", "~/a"),
            ("/srv/app/", "/srv/app"),
            ("/rooted", "/rooted"),
            ("/home", "/home"),
            ("/home//x", "/home//x"),
            ("/", "/"),
            ("", ""),
        ] {
            assert_eq!(short_dir(cwd), said, "{cwd:?}");
        }
    }

    #[test]
    fn the_cursor_steps_one_row_and_stops_at_the_ends() {
        let ids = [Id(5), Id(4), Id(3)];
        assert_eq!(step(&[], None, Go::Down), None);
        assert_eq!(step(&[], Some(Id(1)), Go::Last), None);
        // No cursor: down starts at the top, up at the bottom.
        assert_eq!(step(&ids, None, Go::Down), Some(Id(5)));
        assert_eq!(step(&ids, None, Go::Up), Some(Id(3)));
        // A cursor on a session that is gone counts as none.
        assert_eq!(step(&ids, Some(Id(9)), Go::Down), Some(Id(5)));
        assert_eq!(step(&ids, Some(Id(5)), Go::Down), Some(Id(4)));
        assert_eq!(step(&ids, Some(Id(4)), Go::Down), Some(Id(3)));
        assert_eq!(step(&ids, Some(Id(3)), Go::Down), Some(Id(3)));
        assert_eq!(step(&ids, Some(Id(4)), Go::Up), Some(Id(5)));
        assert_eq!(step(&ids, Some(Id(5)), Go::Up), Some(Id(5)));
        assert_eq!(step(&ids, Some(Id(4)), Go::First), Some(Id(5)));
        assert_eq!(step(&ids, None, Go::First), Some(Id(5)));
        assert_eq!(step(&ids, Some(Id(4)), Go::Last), Some(Id(3)));
    }

    #[test]
    fn the_list_redraws_for_what_it_shows_and_not_for_the_round_trip() {
        let mut m = model(vec![session(1, 1), session(2, 2)]);
        let before = Shown::of(&m);
        // The nav bar's round trip moves every second; the sidebar does not show it.
        m.rtt_ms = Some(23);
        m.current = Some(Id(1));
        assert_eq!(Shown::of(&m), before);
        // A rev bump alone shows nothing.
        m.sessions.get_mut(&Id(1)).unwrap().rev = 2;
        assert_eq!(Shown::of(&m), before);
        // A status, a title and the link are.
        m.sessions.get_mut(&Id(1)).unwrap().status = SessionStatus::Working;
        assert_ne!(Shown::of(&m), before);
        let before = Shown::of(&m);
        m.sessions.get_mut(&Id(1)).unwrap().label = Some("x".into());
        assert_ne!(Shown::of(&m), before);
        let before = Shown::of(&m);
        m.link = Link::Up(Transport::Quic);
        assert_ne!(Shown::of(&m), before);
        m.sessions.remove(&Id(2));
        assert_eq!(ids(&Shown::of(&m).rows), [1]);
    }

    #[test]
    fn the_empty_list_says_what_it_waits_for() {
        let mut m = Model::default();
        let say = |m: &Model| {
            let s = Shown::of(m);
            (s.note(), s.can_create())
        };
        assert_eq!(say(&m), ("connecting…", false));
        m.link = Link::Up(Transport::Quic);
        assert_eq!(say(&m), ("no sessions", true));
        m.link = Link::Down("closed".into());
        assert_eq!(say(&m), ("offline", false));
    }

    #[test]
    fn the_sidebar_is_as_wide_as_the_list_or_the_strip() {
        assert_eq!(extent(false), WIDTH);
        assert_eq!(extent(true), STRIP);
    }

    /// The collapsed API on a real entity, in a headless gpui app (no display needed).
    #[test]
    fn collapsing_is_announced_once_per_change() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let seen = Rc::new(RefCell::new(Vec::new()));
        let log = seen.clone();
        gpui_platform::headless().run(move |cx| {
            let sidebar = cx.new(|cx| Sidebar::new(None, cx));
            let events = log.clone();
            cx.subscribe(&sidebar, move |_, ev: &CollapsedChanged, _| {
                events.borrow_mut().push(ev.0)
            })
            .detach();
            assert!(!sidebar.read(cx).collapsed());
            assert_eq!(sidebar.read(cx).width(), px(WIDTH));
            sidebar.update(cx, |s, cx| s.set_collapsed(true, cx));
            // Already collapsed: nothing changes and nothing is announced.
            sidebar.update(cx, |s, cx| s.set_collapsed(true, cx));
            assert!(sidebar.read(cx).collapsed());
            assert_eq!(sidebar.read(cx).width(), px(STRIP));
            sidebar.update(cx, |s, cx| s.set_collapsed(false, cx));
            assert!(!sidebar.read(cx).collapsed());
            assert_eq!(sidebar.read(cx).width(), px(WIDTH));
            // Selecting is idempotent and has no side effect on the form.
            sidebar.update(cx, |s, cx| s.set_selected(Some(Id(7)), cx));
            assert_eq!(sidebar.read(cx).selected, Some(Id(7)));
            assert_eq!(sidebar.read(cx).cursor, Some(Id(7)));
            sidebar.update(cx, |s, cx| s.set_selected(None, cx));
            assert_eq!(sidebar.read(cx).selected, None);
            // The platform loop clears a stop request made before it starts, so quit from a task.
            cx.spawn(async move |cx| cx.update(|cx| cx.quit())).detach();
        });
        assert_eq!(*seen.borrow(), [true, false]);
    }
}
