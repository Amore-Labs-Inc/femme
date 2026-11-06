//! The session row's menu, on a right click: `rename` and `close`. It replaced the `×` every row
//! carried, which was too easy to hit by accident. Close ends the session for real (the daemon
//! stops its holder and the agent with it); rename sets the row's label.

use femme_proto::rpc::Req;
use femme_proto::Sid;
use gpui::{
    deferred, div, px, AnyElement, AppContext as _, Context, Entity, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Window,
};
use gpui_component::input::{Input, InputEvent, InputState};

use super::Sidebar;
use crate::theme::bad;
use crate::ui::{menu, menu_item};

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

    /// The menu under `sid`'s row, when it is open there: drawn after the list so it lies over
    /// the rows below. A press inside stays inside (the sidebar's own press closes the menu).
    pub(super) fn menu_for(&self, sid: Sid, cx: &Context<Self>) -> Option<AnyElement> {
        let m = self.menu.as_ref().filter(|m| m.sid == sid)?;
        let body = match &m.rename {
            Some((input, _)) => menu().w(px(200.)).child(
                div()
                    .id("sb-rename")
                    .px(px(4.))
                    .py(px(2.))
                    .child(Input::new(input)),
            ),
            None => menu()
                .w(px(140.))
                .child(
                    menu_item(SharedString::from(format!("sb-rename-{sid}")))
                        .child("rename")
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            this.start_rename(window, cx);
                        })),
                )
                .child(
                    menu_item(SharedString::from(format!("sb-close-{sid}")))
                        .text_color(bad())
                        .child("close")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_menu(cx);
                            this.close(sid, cx);
                        })),
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
