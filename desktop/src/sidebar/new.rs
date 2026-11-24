//! The title bar's `+`: the one place a chat starts. Its menu asks for the model (`claude`,
//! `deepseek`, `inkling 2 mini (uncensored)`), then, for claude and deepseek while `femmed local`
//! is connected, the box: `femme` or `this laptop`. Inkling is served on the laptop only, so it goes
//! there without asking and is dimmed while that daemon is down. The last row, `project…`, opens
//! the projects picker as the `+` did before.
//!
//! While it is open Up/Down move the highlight, Enter picks and Esc closes, wherever the keyboard is
//! (the femme menu's way: the focus never moves, so the composer keeps its caret); a press outside
//! closes it.

use femme_proto::rpc::Req;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    anchored, deferred, div, point, px, AnyElement, Context, InteractiveElement as _, IntoElement,
    KeystrokeEvent, MouseDownEvent, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window,
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
}

impl Pick {
    const ALL: [Pick; 3] = [Pick::Claude, Pick::Deepseek, Pick::Inkling];

    fn label(self) -> &'static str {
        match self {
            Pick::Claude => "claude",
            Pick::Deepseek => "deepseek",
            Pick::Inkling => "inkling 2 mini (uncensored)",
        }
    }

    /// The session-making request, in the daemon's home directory.
    fn req(self) -> Req {
        match self {
            Pick::Claude => session::new_claude(""),
            Pick::Deepseek => session::new_deepseek(""),
            Pick::Inkling => session::new_inkling(""),
        }
    }
}

/// Which question the menu asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Model,
    Place(Pick),
}

/// What picking a row does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Act {
    /// Ask where `Pick` runs.
    Ask(Pick),
    /// Start the chat on that box.
    Start(Pick, BoxId),
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
        (Pick::Inkling, _, true) => (Act::Start(p, BoxId::Local), None),
        (Pick::Inkling, _, false) => (Act::Start(p, BoxId::Local), Some("needs this laptop")),
        (_, true, true) => (Act::Ask(p), None),
        (_, false, true) => (Act::Start(p, BoxId::Local), None),
        (_, true, false) => (Act::Start(p, BoxId::Remote), None),
        (_, false, false) => (Act::Start(p, BoxId::Remote), Some("offline")),
    }
}

/// The rows of `step`.
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
            act: Act::Start(p, on),
            off: (!up).then_some("offline"),
        })
        .collect(),
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
}

impl Sidebar {
    /// The `+` was clicked: the menu opens on its first question.
    pub(super) fn open_new(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.close_menu(cx);
        self.new_menu = Some(NewMenu {
            step: Step::Model,
            lit: None,
            window: window.window_handle(),
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

    /// Runs row `ix` of the open menu; a dimmed row does nothing.
    fn pick_new(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(step) = self.new_menu.as_ref().map(|m| m.step) else {
            return;
        };
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
                self.start(on, p.req(), cx);
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
        let Some((step, lit, at)) = self.new_menu.as_ref().map(|m| (m.step, m.lit, m.window))
        else {
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
                let on: Vec<bool> = self
                    .new_entries(step)
                    .iter()
                    .map(|e| e.off.is_none())
                    .collect();
                if let Some(m) = &mut self.new_menu {
                    m.lit = next_lit(lit, key == "down", &on).or(lit);
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

    /// The menu under the `+`, when it is open: a sheet over the window takes the presses outside
    /// it (they close it), so nothing behind reacts to them.
    pub(super) fn new_menu_view(&self, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let m = self.new_menu.as_ref()?;
        let rows = self
            .new_entries(m.step)
            .into_iter()
            .enumerate()
            .map(|(ix, e)| {
                let off = e.off.is_some();
                let id = SharedString::from(format!("sb-new-{ix}"));
                (if off {
                    menu_item_off(id)
                } else {
                    menu_item(id)
                })
                .when(m.lit == Some(ix) && !off, |r| r.bg(accent()))
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
                .child(e.label)
                .when_some(e.off, |r, why| r.child(div().flex_1()).child(meta(why)))
            });
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
                        .w(px(340.))
                        .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                            this.close_new(cx);
                        }))
                        .children(rows),
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
                ("project…", Project, None),
            ]
        );
        assert_eq!(
            say(Step::Model, true, true),
            [
                ("claude", Ask(Pick::Claude), None),
                ("deepseek", Ask(Pick::Deepseek), None),
                (inkling, Start(Pick::Inkling, Local), None),
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
        let of = |p: Pick| match p.req() {
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
}
