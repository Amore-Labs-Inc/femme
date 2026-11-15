//! The session row's menu, on a right click: `rename`, `fork` (Claude chats only) and `close`. It
//! replaced the `×` every row carried, which was too easy to hit by accident. Close ends the
//! session for real (the daemon stops its holder and the agent with it); rename sets the row's
//! label; fork makes a new headless chat that starts as a copy of this one's conversation, dimmed
//! until there is a conversation to copy.

use femme_proto::rpc::Req;
use femme_proto::Sid;
use gpui::{
    deferred, div, px, AnyElement, AppContext as _, Context, Entity, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, Styled as _, Subscription, Window,
};
use gpui_component::input::{Input, InputEvent, InputState};

use super::Sidebar;
use crate::net::BoxId;
use crate::session;
use crate::ui::ctx::{card, Item, WIDTH};
use crate::ui::menu;

/// The menu open on one row.
pub(super) struct Menu {
    pub sid: Sid,
    /// The name box, once `rename` was picked; Enter sends, Esc leaves.
    rename: Option<(Entity<InputState>, Subscription)>,
}

impl Sidebar {
    /// Opens the menu on `sid`'s row (any other closes).
    pub(super) fn open_menu(&mut self, sid: Sid, cx: &mut Context<Self>) {
        self.menu = Some(Menu { sid, rename: None });
        cx.notify();
    }

    /// Closes the menu; false when none was open.
    pub(super) fn close_menu(&mut self, cx: &mut Context<Self>) -> bool {
        if self.menu.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    /// `rename`: the menu becomes a name box holding the row's title, selected, with the keyboard.
    fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sid) = self.menu.as_ref().map(|m| m.sid) else {
            return;
        };
        let title = self
            .shown
            .rows
            .iter()
            .find(|r| r.sid == sid)
            .map(|r| r.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("name"));
        input.update(cx, |input, cx| {
            input.set_value(title.clone(), window, cx);
            input.set_selected_range(0..title.len(), cx);
            input.focus(window, cx);
        });
        let enter = cx.subscribe_in(
            &input,
            window,
            move |this, input, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    let name = input.read(cx).value().trim().to_owned();
                    this.rename(sid, name, cx);
                    this.close_menu(cx);
                }
            },
        );
        if let Some(m) = &mut self.menu {
            m.rename = Some((input, enter));
        }
        cx.notify();
    }

    /// Sets the session's label (an empty name clears it: the row falls back to its folder).
    fn rename(&mut self, sid: Sid, name: String, cx: &mut Context<Self>) {
        let Some(net) = self.net.clone() else { return };
        let label = Some(name).filter(|n| !n.is_empty());
        net.update(cx, |n, cx| n.run(Req::SessionLabel { sid, label }, cx))
            .detach();
    }

    /// `fork`: a copy of the row's conversation, made on the row's box in the row's folder; the
    /// copy becomes the current session. A row with no conversation yet does nothing.
    fn fork(&mut self, sid: Sid, cx: &mut Context<Self>) {
        let Some(r) = self.shown.rows.iter().find(|r| r.sid == sid) else {
            return;
        };
        let Some(agent_sid) = &r.agent_sid else {
            return;
        };
        let on = if r.local { BoxId::Local } else { BoxId::Remote };
        let req = session::fork_of(&r.cwd, agent_sid);
        self.start(on, req, cx);
    }

    /// The menu under `sid`'s row, when it is open there: drawn after the list so it lies over
    /// the rows below. A press inside stays inside (the sidebar's own press closes the menu).
    pub(super) fn menu_for(&self, sid: Sid, cx: &Context<Self>) -> Option<AnyElement> {
        let m = self.menu.as_ref().filter(|m| m.sid == sid)?;
        let row = self.shown.rows.iter().find(|r| r.sid == sid);
        let fork = row.filter(|r| r.claude).map(|r| {
            Item::new(
                "fork",
                cx.listener(move |this, _, _, cx| {
                    this.close_menu(cx);
                    this.fork(sid, cx);
                }),
            )
            .disabled(r.agent_sid.is_none())
        });
        let body = match &m.rename {
            Some((input, _)) => menu().w(px(200.)).child(
                div()
                    .id("sb-rename")
                    .px(px(4.))
                    .py(px(2.))
                    .child(Input::new(input)),
            ),
            None => card(
                &format!("sb-ctx-{sid}"),
                WIDTH,
                [Item::new(
                    "rename",
                    cx.listener(|this, _, window, cx| this.start_rename(window, cx)),
                )]
                .into_iter()
                .chain(fork)
                .chain([Item::new(
                    "close",
                    cx.listener(move |this, _, _, cx| {
                        this.close_menu(cx);
                        this.close(sid, cx);
                    }),
                )
                .danger()]),
            ),
        };
        Some(
            deferred(
                div()
                    .absolute()
                    .top(px(super::ROW_H - 6.))
                    .left(px(13.))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .child(body),
            )
            .into_any_element(),
        )
    }
}
