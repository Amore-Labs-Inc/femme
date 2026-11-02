//! The static-ish panels of the shell: sidebar (lanes, sessions, accounts), the right panel with its
//! diff/files/term/issue tabs, the composer, and the command-palette placeholder. Each panel is its
//! own entity so the shell can cache it: a frame where only the terminal panes changed replays these
//! from the previous frame instead of rebuilding them.

use femme_proto::state::{Attention, Session as Wire, SessionStatus};
use femme_proto::Sid;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, relative, AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Render,
    Rgba, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window,
};

use crate::net::{self, Net};
use crate::theme::{
    self, BAD, DONE, FONT_MONO, FONT_PIXEL, HOVER, INK, MUTED, PANEL, PAPER, PAUSE, PR, QUEUE,
    RULE, WORK,
};
use crate::ui::{button, button_dark, card, dialog, dot, meta, ruled, ruled_sides, st, tb, win};

/// A `.win` with a title bar and a body that takes the remaining height.
fn section(title: &str) -> gpui::Div {
    win().child(tb(title))
}

/// A static session row: status colour, title, working directory.
struct Session {
    color: Rgba,
    title: &'static str,
    cwd: &'static str,
}

const SESSIONS: [Session; 12] = [
    Session {
        color: WORK,
        title: "refactor auth middleware",
        cwd: "~/Web_Apps/femme",
    },
    Session {
        color: WORK,
        title: "term emulator: wide chars",
        cwd: "~/Web_Apps/femme/crates/femme-term",
    },
    Session {
        color: PAUSE,
        title: "quic transport: 0-rtt resume",
        cwd: "~/Web_Apps/femme/crates/femme-transport",
    },
    Session {
        color: WORK,
        title: "holder snapshot format",
        cwd: "~/Web_Apps/femme/crates/femme-holder",
    },
    Session {
        color: DONE,
        title: "board: lane colours",
        cwd: "~/Web_Apps/femme/board",
    },
    Session {
        color: PAUSE,
        title: "pi driver fixtures",
        cwd: "~/Web_Apps/femme/crates/femme-agents",
    },
    Session {
        color: DONE,
        title: "install.sh: arrow-key menu",
        cwd: "~/Web_Apps/femme",
    },
    Session {
        color: BAD,
        title: "sqlite migration 0007",
        cwd: "~/Web_Apps/femme/crates/femme-store",
    },
    Session {
        color: DONE,
        title: "provision laptop",
        cwd: "~/Web_Apps/femme/laptop",
    },
    Session {
        color: WORK,
        title: "desktop: gpui terminal element",
        cwd: "~/Web_Apps/femme/desktop",
    },
    Session {
        color: DONE,
        title: "docs: plan-desktop",
        cwd: "~/Web_Apps/femme/docs",
    },
    Session {
        color: DONE,
        title: "hooks fixtures",
        cwd: "~/Web_Apps/femme/crates/femme-agents",
    },
];

/// The status colour of a session's dot: what it is doing, or what it waits for.
pub fn status_color(s: &Wire) -> Rgba {
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

/// What the sidebar asks of the shell when a session is clicked: show its first terminal.
pub struct OpenSession(pub Sid);

/// Left column: lane counts, sessions, accounts.
pub struct Sidebar {
    focus: FocusHandle,
    /// The daemon's sessions; without one the list is the mock one.
    net: Option<Entity<Net>>,
    _watch: Option<Subscription>,
}

impl EventEmitter<OpenSession> for Sidebar {}

impl Sidebar {
    /// A sidebar with its own focus stop, listing the sessions `net` knows of (or mock ones).
    pub fn new(net: Option<Entity<Net>>, cx: &mut Context<Self>) -> Self {
        let watch = net.as_ref().map(|n| cx.observe(n, |_, _, cx| cx.notify()));
        Self {
            focus: cx.focus_handle(),
            net,
            _watch: watch,
        }
    }

    /// One row per live session, newest first.
    fn rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(net) = &self.net else {
            return SESSIONS
                .iter()
                .map(|s| row(s.color, s.title.into(), s.cwd.into()).into_any_element())
                .collect();
        };
        let sessions: Vec<_> = net
            .read(cx)
            .model
            .live()
            .into_iter()
            .map(|s| {
                let line = format!("{} {}", format!("{:?}", s.agent).to_lowercase(), s.cwd);
                (s.id, row(status_color(s), net::title(s), line))
            })
            .collect();
        sessions
            .into_iter()
            .map(|(sid, row)| {
                row.id(SharedString::from(sid.to_string()))
                    .cursor_pointer()
                    .hover(|s| s.bg(HOVER))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(OpenSession(sid))))
                    .into_any_element()
            })
            .collect()
    }
}

/// A session row: status dot, title, agent and directory.
fn row(color: Rgba, title: String, meta_line: String) -> gpui::Div {
    card()
        .flex()
        .items_center()
        .gap(px(10.))
        .py(px(6.))
        .child(dot(color))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(div().text_size(px(14.)).truncate().child(title))
                .child(meta(meta_line)),
        )
}

impl Focusable for Sidebar {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn account(title: &'static str, plan: &'static str, used: f32, on: bool) -> gpui::Div {
    card()
        .relative()
        .flex()
        .flex_col()
        .gap(px(6.))
        .when(on, |d| {
            d.pl(px(16.)).child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(6.))
                    .bg(WORK),
            )
        })
        .child(div().text_size(px(17.)).child(title))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(meta(plan))
                .child(ruled(
                    div()
                        .flex_1()
                        .h(px(10.))
                        .bg(PAPER)
                        .child(div().h_full().w(relative(used)).bg(INK)),
                ))
                .child(meta(format!("{:.0}%", used * 100.))),
        )
}

impl Render for Sidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows(cx);
        let lanes = [
            ("work", WORK, 4),
            ("pause", PAUSE, 2),
            ("queue", QUEUE, 5),
            ("pr", PR, 2),
            ("bad", BAD, 1),
            ("done", DONE, 12),
        ];
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(6.))
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)),
            )
            .child(
                section("lanes").child(div().flex().flex_wrap().gap(px(3.)).children(
                    lanes.iter().map(|(name, color, n)| {
                        div()
                            .bg(PAPER)
                            .p(px(4.))
                            .child(st(format!("{name} {n}"), *color))
                    }),
                )),
            )
            .child(
                section("sessions").flex_1().min_h_0().child(
                    div()
                        .id("sessions")
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .overflow_y_scroll()
                        .children(rows),
                ),
            )
            .child(
                section("accounts")
                    .child(account("anthropic max", "5h", 0.62, true))
                    .child(account("openai pro", "wk", 0.18, false)),
            )
    }
}

/// Tabs of the right panel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RightTab {
    /// Working-tree diff.
    Diff,
    /// File tree.
    Files,
    /// Terminal output of the session.
    Term,
    /// The issue the session works on.
    Issue,
}

impl RightTab {
    const ALL: [(RightTab, &'static str); 4] = [
        (Self::Diff, "diff"),
        (Self::Files, "files"),
        (Self::Term, "term"),
        (Self::Issue, "issue"),
    ];
}

const DIFF: &str = "\
diff --git a/src/auth.rs b/src/auth.rs
@@ -12,7 +12,9 @@ pub fn verify(token: &str) -> bool {
     let now = unix_now();
-    if claims.exp < now {
+    if claims.exp <= now {
         return false;
     }
+    // reject tokens issued in the future
+    if claims.iat > now + SKEW { return false; }
     check_signature(token)
 }
@@ -40,6 +42,8 @@ fn check_signature(token: &str) -> bool {
-    let key = KEYS.get(kid)?;
+    let Some(key) = KEYS.get(kid) else { return false };
     hmac_eq(key, token)";

const FILES: [&str; 12] = [
    "desktop/",
    "  Cargo.toml",
    "  assets/fonts/",
    "  src/",
    "    main.rs",
    "    app.rs",
    "    theme.rs",
    "    terminal/",
    "      element.rs",
    "      pane.rs",
    "    transcript/",
    "      view.rs",
];

/// Right column with the diff/files/term/issue tabs.
pub struct RightPanel {
    tab: RightTab,
    focus: FocusHandle,
}

impl RightPanel {
    /// Starts on the diff tab.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            tab: RightTab::Diff,
            focus: cx.focus_handle(),
        }
    }
}

impl Focusable for RightPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for RightPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = div()
            .flex()
            .gap(px(6.))
            .children(RightTab::ALL.iter().enumerate().map(|(i, (tab, name))| {
                let tab = *tab;
                let b = if tab == self.tab {
                    button_dark(("rtab", i), *name)
                } else {
                    button(("rtab", i), *name)
                };
                b.flex_1().on_click(cx.listener(move |this, _, _, cx| {
                    this.tab = tab;
                    cx.notify();
                }))
            }));
        let mono = |t: SharedString| {
            div()
                .font_family(FONT_MONO)
                .text_size(px(12.))
                .line_height(relative(1.5))
                .child(t)
        };
        let body: AnyElement = match self.tab {
            RightTab::Diff => div()
                .flex()
                .flex_col()
                .children(DIFF.lines().map(|l| {
                    let (bg, fg) = match l.as_bytes().first() {
                        Some(b'+') => (Some(theme::rgba8(0x03AA5C, 0.16)), INK),
                        Some(b'-') => (Some(theme::rgba8(0xD93036, 0.14)), INK),
                        Some(b'@') | Some(b'd') => (None, MUTED),
                        _ => (None, INK),
                    };
                    mono(l.to_string().into()).text_color(fg).when_some(bg, |d, c| d.bg(c)).whitespace_nowrap()
                }))
                .into_any_element(),
            RightTab::Files => div().children(FILES.iter().map(|f| mono((*f).to_string().into()).whitespace_nowrap())).into_any_element(),
            RightTab::Term => div().child(meta("terminal output lives in the drawer (mod+j)")).into_any_element(),
            RightTab::Issue => card()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(div().flex().gap(px(8.)).child(st("open", PR)).child(meta("#142")))
                .child(div().text_size(px(17.)).child("tokens issued in the future are accepted"))
                .child(div().text_size(px(14.)).text_color(MUTED).child(
                    "verify() only checks exp. A token with iat far in the future passes if the key is right.",
                ))
                .into_any_element(),
        };
        div()
            .size_full()
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)),
            )
            .child(
                section("session")
                    .size_full()
                    .child(div().bg(PANEL).p(px(6.)).child(tabs))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .bg(PAPER)
                            .p(px(8.))
                            .overflow_hidden()
                            .child(body),
                    ),
            )
    }
}

/// The composer: a text box with a send button. Typing, backspace and enter work; there is no IME,
/// selection or multi-line editing.
// ponytail: toy input (append/backspace only); gpui-component's `Input` replaces it in M3 (adds tree-sitter and IME).
pub struct Composer {
    text: String,
    focus: FocusHandle,
}

impl Composer {
    /// An empty composer.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            text: String::new(),
            focus: cx.focus_handle(),
        }
    }

    fn on_key(&mut self, e: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &e.keystroke;
        if k.modifiers.control || k.modifiers.platform || k.modifiers.alt {
            return;
        }
        match (k.key.as_str(), &k.key_char) {
            ("backspace", _) => {
                self.text.pop();
            }
            ("enter", _) => self.text.clear(),
            (_, Some(c)) => self.text.push_str(c),
            _ => return,
        }
        cx.notify();
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus.is_focused(window);
        let mut boxed = ruled(div().bg(PAPER).p(px(10.)).flex().flex_col().gap(px(6.)));
        boxed.style().box_shadow = Some(theme::hard_shadow());
        div()
            .size_full()
            .p(px(3.))
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)),
            )
            .on_key_down(cx.listener(Self::on_key))
            .child(
                boxed
                    .size_full()
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .flex()
                            .flex_wrap()
                            .items_start()
                            .text_size(px(17.))
                            .line_height(relative(1.35))
                            .when(self.text.is_empty(), |d| {
                                d.text_color(gpui::rgb(0x767676)).child("message the agent")
                            })
                            .when(!self.text.is_empty(), |d| d.child(self.text.clone()))
                            .when(focused, |d| d.child(div().w(px(2.)).h(px(17.)).bg(INK))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(button("c-attach", "+"))
                            .child(ruled(
                                div()
                                    .px(px(12.))
                                    .h(px(30.))
                                    .flex()
                                    .items_center()
                                    .bg(PANEL)
                                    .text_size(px(15.))
                                    .child("claude code"),
                            ))
                            .child(div().flex_1())
                            .child(button_dark("c-send", "send")),
                    ),
            )
    }
}

/// The command palette placeholder: a dialog with a fake input and a few commands.
pub fn palette(on_close: impl Fn(&mut Window, &mut App) + Clone + 'static) -> impl IntoElement {
    const COMMANDS: [(&str, &str); 6] = [
        ("new session", "mod+n"),
        ("toggle terminal drawer", "mod+j"),
        ("toggle sidebar", "mod+b"),
        ("toggle right panel", "mod+alt+b"),
        ("focus next pane", "f6"),
        ("accounts", ""),
    ];
    let close = on_close.clone();
    div()
        .absolute()
        .inset_0()
        .flex()
        .justify_center()
        .items_start()
        .pt(px(120.))
        .bg(theme::rgba8(0x000000, 0.25))
        .on_mouse_down(MouseButton::Left, move |_, window, cx| close(window, cx))
        .child(
            dialog()
                .w(px(520.))
                .flex()
                .flex_col()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(tb("command palette"))
                .child(
                    div()
                        .p(px(8.))
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .child(ruled(
                            div()
                                .bg(PAPER)
                                .px(px(10.))
                                .py(px(8.))
                                .text_color(gpui::rgb(0x767676))
                                .child("type a command"),
                        ))
                        .children(COMMANDS.iter().map(|(name, key)| {
                            ruled_sides(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .px(px(10.))
                                    .py(px(6.))
                                    .bg(PAPER)
                                    .font_family(FONT_PIXEL)
                                    .text_size(px(20.))
                                    .line_height(relative(1.))
                                    .hover(|s| s.bg(HOVER)),
                                RULE,
                                RULE,
                                RULE,
                                RULE,
                            )
                            .child(*name)
                            .child(div().text_color(MUTED).child(*key))
                        })),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::state::{Agent, Mode};
    use femme_proto::Id;

    fn dot_of(status: SessionStatus, attention: Attention) -> Rgba {
        status_color(&Wire {
            id: Id(1),
            handle: "h".into(),
            rev: 1,
            name: None,
            owner: "board".into(),
            agent: Agent::Shell,
            mode: Mode::Tui,
            cwd: "/".into(),
            label: None,
            agent_sid: None,
            model: None,
            status,
            attention,
            created: 1,
            closed: None,
        })
    }

    #[test]
    fn the_dot_says_what_the_session_is_doing_or_waits_for() {
        use SessionStatus::*;
        assert_eq!(dot_of(Working, Attention::None), WORK);
        assert_eq!(dot_of(Blocked, Attention::None), PAUSE);
        assert_eq!(dot_of(Idle, Attention::Blocked), PAUSE);
        assert_eq!(dot_of(Idle, Attention::None), QUEUE);
        assert_eq!(dot_of(Starting, Attention::None), QUEUE);
        for s in [Interrupted, Paused, Crashed, Failed] {
            assert_eq!(dot_of(s, Attention::None), BAD, "{s:?}");
        }
        assert_eq!(dot_of(Closed, Attention::None), DONE);
    }
}
