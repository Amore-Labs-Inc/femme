//! The title bar's `+`: the one place a chat starts. Its menu asks for the model (`claude`,
//! `deepseek`, `inkling 2 mini (uncensored)`), then, for claude and deepseek while `femmed local`
//! is connected, the box: `femme` or `this laptop`. Inkling is served on the laptop only, so it goes
//! there without asking and is dimmed while that daemon is down. `existing claude code…` asks the
//! box the same way, then lists that machine's Claude Code conversations no FEMME chat holds (the
//! ones run in a plain terminal): picking one opens it as a chat, as a copy while a terminal still
//! has it open. The last row, `project…`, opens the projects picker as the `+` did before.
//!
//! While it is open Up/Down move the highlight, Enter picks and Esc closes, wherever the keyboard is
//! (the femme menu's way: the focus never moves, so the composer keeps its caret); a press outside
//! closes it.

use std::time::{SystemTime, UNIX_EPOCH};

use femme_proto::rpc::{Conversation, Req, Resp};
use femme_proto::state::{Agent, Mode};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    anchored, deferred, div, point, px, AnyElement, Context, Div, InteractiveElement as _,
    IntoElement, KeystrokeEvent, MouseDownEvent, ParentElement as _, ScrollHandle, SharedString,
    Stateful, StatefulInteractiveElement as _, Styled as _, Window,
};

use super::{AddProject, Sidebar};
use crate::net::BoxId;
use crate::session;
use crate::theme::accent;
use crate::ui::{menu, menu_item, menu_item_off, meta};

/// A model the menu starts a chat on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pick {
    Claude,
    Deepseek,
    Inkling,
    /// Not a model: a Claude Code conversation already on that box, picked from a list.
    Existing,
}

impl Pick {
    const ALL: [Pick; 4] = [Pick::Claude, Pick::Deepseek, Pick::Inkling, Pick::Existing];

    fn label(self) -> &'static str {
        match self {
            Pick::Claude => "claude",
            Pick::Deepseek => "deepseek",
            Pick::Inkling => "inkling 2 mini (uncensored)",
            Pick::Existing => "existing claude code…",
        }
    }

    /// The session-making request, in the daemon's home directory; `None` for `Existing`, whose
    /// request is made from the conversation picked ([`resume`]).
    fn req(self) -> Option<Req> {
        match self {
            Pick::Claude => Some(session::new_claude("")),
            Pick::Deepseek => Some(session::new_deepseek("")),
            Pick::Inkling => Some(session::new_inkling("")),
            Pick::Existing => None,
        }
    }

    /// What going on with `p` on box `on` does: start the chat, or list that box's conversations.
    fn on(self, on: BoxId) -> Act {
        match self {
            Pick::Existing => Act::List(on),
            p => Act::Start(p, on),
        }
    }
}

/// The chat that takes conversation `c` over: Claude, headless, in its folder, going on from it;
/// a copy (`fork`) while a terminal has it open, so the two never write one transcript.
fn resume(c: &Conversation) -> Req {
    Req::SessionCreate {
        cwd: c.cwd.clone(),
        agent: Agent::Claude,
        mode: Mode::Headless,
        label: None,
        name: None,
        prompt: None,
        model: None,
        resume: Some(c.uuid.clone()),
        fork: c.open,
    }
}

/// How long ago `then` was, from `now` (both ms since the epoch): `3 m`, `2 h`, `4 d`; `now` under
/// a minute (and for a clock that runs behind the other machine's).
fn age(then: i64, now: i64) -> String {
    let m = (now - then) / 60_000;
    match m {
        ..=0 => "now".to_owned(),
        1..60 => format!("{m} m"),
        60..1440 => format!("{} h", m / 60),
        _ => format!("{} d", m / 1440),
    }
}

/// Milliseconds since the epoch, now.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// `dir` with its home part as `~`, where the path shows it: `/home/<user>`, `/Users/<user>`,
/// `/root`. The box's home is not known here, so another layout stays whole.
fn short_dir(dir: &str) -> String {
    let rest = |p: &str| -> Option<usize> {
        let tail = dir.strip_prefix(p)?;
        let user = tail.split('/').next()?;
        (!user.is_empty()).then(|| p.len() + user.len())
    };
    let cut = rest("/home/")
        .or_else(|| rest("/Users/"))
        .or_else(|| (dir == "/root" || dir.starts_with("/root/")).then_some(5));
    match cut {
        Some(n) => format!("~{}", &dir[n..]),
        None => dir.to_owned(),
    }
}

/// What the list step has: the request is out, the answer, or why there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Convs {
    Looking,
    Got(Vec<Conversation>),
    Failed(String),
}

/// A row of the list step: the title, the dimmer line under it, and whether it can be picked (a
/// conversation) or only says something (`looking…`, empty, the error).
#[derive(Clone, Debug, PartialEq, Eq)]
struct ConvRow {
    title: String,
    meta: String,
    pick: bool,
}

/// The rows of the list step for `convs`, newest first, ages counted from `now`.
fn conv_rows(convs: &Convs, now: i64) -> Vec<ConvRow> {
    let say = |t: &str| {
        vec![ConvRow {
            title: t.to_owned(),
            meta: String::new(),
            pick: false,
        }]
    };
    match convs {
        Convs::Looking => say("looking…"),
        Convs::Failed(e) => say(e),
        Convs::Got(v) if v.is_empty() => say("no claude code chats here"),
        Convs::Got(v) => sorted(v)
            .into_iter()
            .map(|c| {
                let mut meta = format!("{} · {}", short_dir(&c.cwd), age(c.modified, now));
                if c.open {
                    meta.push_str(" · in a terminal · opens a copy");
                }
                ConvRow {
                    title: if c.title.trim().is_empty() {
                        "untitled".to_owned()
                    } else {
                        c.title.clone()
                    },
                    meta,
                    pick: true,
                }
            })
            .collect(),
    }
}

/// The conversations of the list in the order its rows show them.
fn sorted(v: &[Conversation]) -> Vec<&Conversation> {
    let mut v: Vec<&Conversation> = v.iter().collect();
    v.sort_by_key(|c| std::cmp::Reverse(c.modified));
    v
}

/// Which question the menu asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Model,
    Place(Pick),
    /// The conversations of that box.
    List(BoxId),
}

/// What picking a row does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Act {
    /// Ask where `Pick` runs.
    Ask(Pick),
    /// Start the chat on that box.
    Start(Pick, BoxId),
    /// List that box's Claude Code conversations.
    List(BoxId),
    /// The projects picker.
    Project,
}

/// One row: its words, what it does, and why it cannot (`Some`: dimmed, inert, the reason after).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    label: &'static str,
    act: Act,
    off: Option<&'static str>,
}

/// What picking `p` leads to with the box (`remote`) and this laptop (`local`) connected or not:
/// the second question only when both could run it, else straight to the one that can.
fn after(p: Pick, remote: bool, local: bool) -> (Act, Option<&'static str>) {
    match (p, remote, local) {
        (Pick::Inkling, _, true) => (p.on(BoxId::Local), None),
        (Pick::Inkling, _, false) => (p.on(BoxId::Local), Some("needs this laptop")),
        (_, true, true) => (Act::Ask(p), None),
        (_, false, true) => (p.on(BoxId::Local), None),
        (_, true, false) => (p.on(BoxId::Remote), None),
        (_, false, false) => (p.on(BoxId::Remote), Some("offline")),
    }
}

/// The rows of `step` (none for the list, whose rows are [`conv_rows`]).
pub(super) fn entries(step: Step, remote: bool, local: bool) -> Vec<Entry> {
    match step {
        Step::Model => Pick::ALL
            .into_iter()
            .map(|p| {
                let (act, off) = after(p, remote, local);
                Entry {
                    label: p.label(),
                    act,
                    off,
                }
            })
            .chain([Entry {
                label: "project…",
                act: Act::Project,
                off: None,
            }])
            .collect(),
        Step::Place(p) => [
            ("femme", BoxId::Remote, remote),
            ("this laptop", BoxId::Local, local),
        ]
        .into_iter()
        .map(|(label, on, up)| Entry {
            label,
            act: p.on(on),
            off: (!up).then_some("offline"),
        })
        .collect(),
        Step::List(_) => Vec::new(),
    }
}

/// The row the arrows light next from `lit`, going down (or up) and wrapping round, over the rows
/// that can be picked (`on`); with nothing lit the first (last) of them.
fn next_lit(lit: Option<usize>, down: bool, on: &[bool]) -> Option<usize> {
    let n = on.len();
    let from = lit.unwrap_or(if down { n - 1 } else { 0 });
    (1..=n)
        .map(|d| {
            if down {
                (from + d) % n
            } else {
                (from + n * 2 - d) % n
            }
        })
        .find(|&i| on[i])
}

/// The open menu.
pub(super) struct NewMenu {
    step: Step,
    /// The highlighted row, by the pointer or the arrows.
    lit: Option<usize>,
    /// The window it opened in: another window's keys are not its business.
    window: gpui::AnyWindowHandle,
    /// The list step's conversations.
    convs: Convs,
    /// The list step's scroller, so the arrows keep the lit row in view.
    scroll: ScrollHandle,
}

impl Sidebar {
    /// The `+` was clicked: the menu opens on its first question.
    pub(super) fn open_new(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.close_menu(cx);
        self.new_menu = Some(NewMenu {
            step: Step::Model,
            lit: None,
            window: window.window_handle(),
            convs: Convs::Looking,
            scroll: ScrollHandle::new(),
        });
        cx.notify();
    }

    /// Closes the `+` menu; false when it was not open.
    pub(super) fn close_new(&mut self, cx: &mut Context<Self>) -> bool {
        if self.new_menu.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    fn new_entries(&self, step: Step) -> Vec<Entry> {
        entries(step, self.can_create(), self.can_create_local())
    }

    /// Which rows of the open menu can be picked, in order.
    fn new_pickable(&self) -> Vec<bool> {
        let Some(m) = &self.new_menu else {
            return Vec::new();
        };
        match m.step {
            Step::List(_) => conv_rows(&m.convs, 0).iter().map(|r| r.pick).collect(),
            step => self
                .new_entries(step)
                .iter()
                .map(|e| e.off.is_none())
                .collect(),
        }
    }

    /// Asks box `on` for its conversations; the answer fills the list step if it is still open.
    fn list_convs(&mut self, on: BoxId, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        let sent = net.update(cx, |n, cx| {
            n.try_run_on(on, Req::Conversations { limit: 30 }, cx)
        });
        cx.spawn(async move |this, cx| {
            let answer = sent.await;
            this.update(cx, |this, cx| {
                // ponytail: an answer from a list closed and opened again on the same box while
                // it was out fills the new one; it is the same box's list, so harmless.
                let Some(m) = this.new_menu.as_mut().filter(|m| m.step == Step::List(on)) else {
                    return;
                };
                m.convs = match answer {
                    Ok(Resp::Conversations(v)) => Convs::Got(v),
                    Ok(other) => Convs::Failed(format!("unexpected answer {other:?}")),
                    Err(e) => Convs::Failed(e),
                };
                m.lit = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Runs row `ix` of the open menu; a dimmed row does nothing.
    fn pick_new(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(step) = self.new_menu.as_ref().map(|m| m.step) else {
            return;
        };
        if let Step::List(on) = step {
            let req = self.new_menu.as_ref().and_then(|m| match &m.convs {
                Convs::Got(v) => sorted(v).get(ix).map(|c| resume(c)),
                _ => None,
            });
            if let Some(req) = req {
                self.close_new(cx);
                self.start(on, req, cx);
            }
            return;
        }
        let Some(e) = self
            .new_entries(step)
            .into_iter()
            .nth(ix)
            .filter(|e| e.off.is_none())
        else {
            return;
        };
        match e.act {
            Act::Ask(p) => {
                if let Some(m) = &mut self.new_menu {
                    m.step = Step::Place(p);
                    m.lit = None;
                }
                cx.notify();
            }
            Act::Start(p, on) => {
                self.close_new(cx);
                if let Some(req) = p.req() {
                    self.start(on, req, cx);
                }
            }
            Act::List(on) => {
                if let Some(m) = &mut self.new_menu {
                    m.step = Step::List(on);
                    m.lit = None;
                    m.convs = Convs::Looking;
                }
                self.list_convs(on, cx);
                cx.notify();
            }
            Act::Project => {
                self.close_new(cx);
                cx.emit(AddProject);
            }
        }
    }

    /// Every key down in the app while the menu is open: Up/Down move the highlight, Enter picks it,
    /// Esc closes. Anything else goes where it was going.
    pub(super) fn new_key(&mut self, ev: &KeystrokeEvent, window: &Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;
        let Some((lit, at)) = self.new_menu.as_ref().map(|m| (m.lit, m.window)) else {
            return;
        };
        if at != window.window_handle() || k.modifiers.modified() {
            return;
        }
        match k.key.as_str() {
            "escape" => {
                self.close_new(cx);
            }
            key @ ("down" | "up") => {
                let on = self.new_pickable();
                if let Some(m) = &mut self.new_menu {
                    m.lit = next_lit(lit, key == "down", &on).or(lit);
                    if let (Step::List(_), Some(ix)) = (m.step, m.lit) {
                        m.scroll.scroll_to_item(ix);
                    }
                }
                cx.notify();
            }
            "enter" => match lit {
                Some(ix) => self.pick_new(ix, cx),
                None => return,
            },
            _ => return,
        }
        cx.stop_propagation();
    }

    /// Row `ix` of the open menu, empty: lit by the pointer or the arrows, picked by a click;
    /// muted and inert when `off`.
    fn new_row(&self, ix: usize, off: bool, lit: bool, cx: &Context<Self>) -> Stateful<Div> {
        let id = SharedString::from(format!("sb-new-{ix}"));
        (if off {
            menu_item_off(id)
        } else {
            menu_item(id)
        })
        .when(lit && !off, |r| r.bg(accent()))
        .on_mouse_move(cx.listener(move |this, _, _, cx| {
            if let Some(m) = this.new_menu.as_mut().filter(|_| !off) {
                if m.lit != Some(ix) {
                    m.lit = Some(ix);
                    cx.notify();
                }
            }
        }))
        .on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            this.pick_new(ix, cx);
        }))
    }

    /// The menu under the `+`, when it is open: a sheet over the window takes the presses outside
    /// it (they close it), so nothing behind reacts to them.
    pub(super) fn new_menu_view(&self, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let m = self.new_menu.as_ref()?;
        let (body, width) = match m.step {
            Step::List(_) => {
                let rows = conv_rows(&m.convs, now_ms())
                    .into_iter()
                    .enumerate()
                    .map(|(ix, r)| {
                        let row = self.new_row(ix, !r.pick, m.lit == Some(ix), cx);
                        if !r.pick {
                            // A note (`looking…`, the error): wraps rather than cut a reason off.
                            return row.h_auto().min_h(px(28.)).py(px(4.)).child(r.title);
                        }
                        row.h(px(40.))
                            .flex_col()
                            .items_start()
                            .justify_center()
                            .gap(px(0.))
                            .child(
                                div()
                                    .w_full()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(r.title),
                            )
                            .child(meta(r.meta).w_full())
                    });
                // ponytail: the daemon sends at most 30; no search or paging over more.
                let list = div()
                    .id("sb-new-list")
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .track_scroll(&m.scroll)
                    .children(rows)
                    .into_any_element();
                (vec![list], px(440.))
            }
            step => {
                let rows = self
                    .new_entries(step)
                    .into_iter()
                    .enumerate()
                    .map(|(ix, e)| {
                        self.new_row(ix, e.off.is_some(), m.lit == Some(ix), cx)
                            .child(e.label)
                            .when_some(e.off, |r, why| r.child(div().flex_1()).child(meta(why)))
                            .into_any_element()
                    })
                    .collect();
                (rows, px(340.))
            }
        };
        let screen = window.viewport_size();
        let sheet = div()
            .w(screen.width)
            .h(screen.height)
            .block_mouse_except_scroll();
        Some(
            div()
                .absolute()
                .top(px(26.))
                .left(px(4.))
                .child(deferred(
                    anchored().position(point(px(0.), px(0.))).child(sheet),
                ))
                .child(deferred(
                    menu()
                        .id("sb-new-menu")
                        .occlude()
                        .w(width)
                        .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            this.close_new(cx);
                        }))
                        .children(body),
                ))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::state::Agent;

    fn say(
        step: Step,
        remote: bool,
        local: bool,
    ) -> Vec<(&'static str, Act, Option<&'static str>)> {
        entries(step, remote, local)
            .into_iter()
            .map(|e| (e.label, e.act, e.off))
            .collect()
    }

    #[test]
    fn the_box_is_asked_only_when_this_laptop_is_connected() {
        use Act::*;
        use BoxId::*;
        let inkling = "inkling 2 mini (uncensored)";
        assert_eq!(
            say(Step::Model, true, false),
            [
                ("claude", Start(Pick::Claude, Remote), None),
                ("deepseek", Start(Pick::Deepseek, Remote), None),
                (
                    inkling,
                    Start(Pick::Inkling, Local),
                    Some("needs this laptop")
                ),
                ("existing claude code…", List(Remote), None),
                ("project…", Project, None),
            ]
        );
        assert_eq!(
            say(Step::Model, true, true),
            [
                ("claude", Ask(Pick::Claude), None),
                ("deepseek", Ask(Pick::Deepseek), None),
                (inkling, Start(Pick::Inkling, Local), None),
                ("existing claude code…", Ask(Pick::Existing), None),
                ("project…", Project, None),
            ]
        );
        // The box down: the laptop runs it; both down: nothing can.
        assert_eq!(
            say(Step::Model, false, true)[0].1,
            Start(Pick::Claude, Local)
        );
        assert_eq!(say(Step::Model, false, false)[1].2, Some("offline"));
        assert_eq!(
            say(Step::Place(Pick::Deepseek), true, true),
            [
                ("femme", Start(Pick::Deepseek, Remote), None),
                ("this laptop", Start(Pick::Deepseek, Local), None),
            ]
        );
    }

    #[test]
    fn the_arrows_step_over_the_rows_that_cannot_be_picked() {
        let on = [true, true, false, true];
        assert_eq!(next_lit(None, true, &on), Some(0));
        assert_eq!(next_lit(None, false, &on), Some(3));
        assert_eq!(next_lit(Some(1), true, &on), Some(3));
        assert_eq!(next_lit(Some(3), true, &on), Some(0));
        assert_eq!(next_lit(Some(3), false, &on), Some(1));
        assert_eq!(next_lit(Some(0), false, &on), Some(3));
        assert_eq!(next_lit(None, true, &[false, false]), None);
    }

    #[test]
    fn each_model_starts_a_headless_chat_on_its_own_model() {
        assert_eq!(Pick::Existing.req(), None);
        let of = |p: Pick| match p.req().unwrap() {
            Req::SessionCreate {
                agent, model, cwd, ..
            } => (agent, model.unwrap(), cwd),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            of(Pick::Claude),
            (Agent::Claude, session::CLAUDE_MODEL.into(), String::new())
        );
        assert_eq!(
            of(Pick::Deepseek),
            (Agent::Pi, session::DEEPSEEK_MODEL.into(), String::new())
        );
        assert_eq!(
            of(Pick::Inkling),
            (
                Agent::Pi,
                "inkling/inkling-2-mini-sq-10".into(),
                String::new()
            )
        );
    }

    #[test]
    fn existing_claude_code_asks_the_box_like_claude_then_lists_it() {
        use Act::*;
        use BoxId::*;
        let row = |remote, local| say(Step::Model, remote, local)[3];
        let name = "existing claude code…";
        assert_eq!(row(true, true), (name, Ask(Pick::Existing), None));
        assert_eq!(row(true, false), (name, List(Remote), None));
        assert_eq!(row(false, true), (name, List(Local), None));
        assert_eq!(row(false, false), (name, List(Remote), Some("offline")));
        assert_eq!(
            say(Step::Place(Pick::Existing), true, true),
            [
                ("femme", List(Remote), None),
                ("this laptop", List(Local), None),
            ]
        );
        assert_eq!(
            say(Step::Place(Pick::Existing), true, false)[1],
            ("this laptop", List(Local), Some("offline"))
        );
    }

    fn conv(uuid: &str, cwd: &str, title: &str, modified: i64, open: bool) -> Conversation {
        Conversation {
            uuid: uuid.into(),
            cwd: cwd.into(),
            title: title.into(),
            modified,
            open,
        }
    }

    const MIN: i64 = 60_000;

    #[test]
    fn the_list_says_each_conversation_newest_first_and_its_states() {
        let now = 1_000 * 24 * 60 * MIN;
        let v = vec![
            conv("a", "/srv/app", "", now - 4 * 24 * 60 * MIN, false),
            conv(
                "b",
                "/home/cem/femme",
                "Fix the login bug",
                now - 3 * MIN,
                true,
            ),
            conv("c", "/root/x", "Tidy", now - 2 * 60 * MIN, false),
        ];
        let rows = conv_rows(&Convs::Got(v.clone()), now);
        let said: Vec<(&str, &str, bool)> = rows
            .iter()
            .map(|r| (r.title.as_str(), r.meta.as_str(), r.pick))
            .collect();
        assert_eq!(
            said,
            [
                (
                    "Fix the login bug",
                    "~/femme · 3 m · in a terminal · opens a copy",
                    true
                ),
                ("Tidy", "~/x · 2 h", true),
                ("untitled", "/srv/app · 4 d", true),
            ]
        );
        // Row order and the conversation a pick takes agree.
        let order: Vec<&str> = sorted(&v).iter().map(|c| c.uuid.as_str()).collect();
        assert_eq!(order, ["b", "c", "a"]);
        let note = |c: &Convs| {
            let r = conv_rows(c, now);
            assert_eq!(r.len(), 1);
            assert!(!r[0].pick);
            r[0].title.clone()
        };
        assert_eq!(note(&Convs::Looking), "looking…");
        assert_eq!(note(&Convs::Got(Vec::new())), "no claude code chats here");
        assert_eq!(
            note(&Convs::Failed("unknown request".into())),
            "unknown request"
        );
        // The arrows have nothing to light while it only says something.
        let on: Vec<bool> = conv_rows(&Convs::Looking, now)
            .iter()
            .map(|r| r.pick)
            .collect();
        assert_eq!(next_lit(None, true, &on), None);
    }

    #[test]
    fn a_picked_conversation_goes_on_in_its_folder_as_a_copy_only_while_open() {
        let of = |c: &Conversation| match resume(c) {
            Req::SessionCreate {
                cwd,
                agent,
                mode,
                label,
                name,
                prompt,
                model,
                resume,
                fork,
            } => {
                assert_eq!((agent, mode), (Agent::Claude, Mode::Headless));
                assert_eq!((label, name, prompt, model), (None, None, None, None));
                (cwd, resume.unwrap(), fork)
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(
            of(&conv("u1", "/tmp/demo", "t", 0, false)),
            ("/tmp/demo".into(), "u1".into(), false)
        );
        assert_eq!(
            of(&conv("u2", "/home/cem", "", 0, true)),
            ("/home/cem".into(), "u2".into(), true)
        );
    }

    #[test]
    fn ages_and_folders_read_short() {
        assert_eq!(age(0, 59_999), "now");
        assert_eq!(age(10 * MIN, 0), "now");
        assert_eq!(age(0, 3 * MIN), "3 m");
        assert_eq!(age(0, 59 * MIN), "59 m");
        assert_eq!(age(0, 60 * MIN), "1 h");
        assert_eq!(age(0, 23 * 60 * MIN + 59 * MIN), "23 h");
        assert_eq!(age(0, 4 * 24 * 60 * MIN), "4 d");
        assert_eq!(short_dir("/home/cem/femme"), "~/femme");
        assert_eq!(short_dir("/home/cem"), "~");
        assert_eq!(short_dir("/Users/cem/code/x"), "~/code/x");
        assert_eq!(short_dir("/root"), "~");
        assert_eq!(short_dir("/root/a"), "~/a");
        assert_eq!(short_dir("/rootless/a"), "/rootless/a");
        assert_eq!(short_dir("/home/"), "/home/");
        assert_eq!(short_dir("/tmp/demo"), "/tmp/demo");
    }
}
