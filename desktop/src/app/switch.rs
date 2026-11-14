//! The centre's chat ↔ terminal toggle of a Claude session ([`crate::takeover`] has the words and
//! the rules): a 24 px row over the centre with one button, `open in terminal` on a chat,
//! `back to chat` on Claude Code's own TUI, and mod+shift+t for the same. The click sends
//! `Req::Takeover`; the centre flips when the session's row brings the new `mode`.

use std::time::Instant;

use femme_proto::rpc::Req;
use femme_proto::Sid;
use gpui::{
    div, px, AnyElement, Context, IntoElement, ParentElement as _, StatefulInteractiveElement as _,
    Styled as _,
};
use gpui_component::tooltip::Tooltip;

use super::FemmeApp;
use crate::takeover::{self, Switch};
use crate::ui;

impl FemmeApp {
    /// The session on show, when it gets the toggle.
    fn switchable(&self, cx: &Context<Self>) -> Option<femme_proto::state::Session> {
        let sid = self.shown.sid?;
        let s = self.net.as_ref()?.read(cx).model.sessions.get(&sid)?;
        takeover::offered(s).then(|| s.clone())
    }

    /// Whether the switch asked for session `sid` still holds its button.
    fn switching(&self, sid: Sid, cx: &Context<Self>) -> bool {
        let Some(net) = &self.net else { return false };
        let model = &net.read(cx).model;
        self.switch
            .zip(model.sessions.get(&sid))
            .is_some_and(|(sw, s)| sw.holds(s, Instant::now()))
    }

    /// The button's click and mod+shift+t: asks the daemon to hand the session on show to the other
    /// mode. Nothing while a switch is already waiting. A refusal other than `Busy` frees the button
    /// and is said in the top bar; `Busy` (a switch already running) keeps it waiting like a yes.
    pub(super) fn take_over(&mut self, cx: &mut Context<Self>) {
        let (Some(net), Some(s)) = (self.net.clone(), self.switchable(cx)) else {
            return;
        };
        if self.switching(s.id, cx) {
            return;
        }
        let (sid, from) = (s.id, s.mode);
        self.switch = Some(Switch {
            sid,
            from,
            since: Instant::now(),
        });
        // The terminal (or the composer) takes the keyboard as soon as the new centre has it.
        self.want_focus = true;
        let label = takeover::label(from);
        let sent = net.update(cx, |n, cx| {
            n.try_run(
                Req::Takeover {
                    sid,
                    to: takeover::target(from),
                },
                cx,
            )
        });
        cx.spawn(async move |this, cx| {
            let answer = sent.await;
            this.update(cx, |t, cx| {
                match answer {
                    Err(why) if takeover::is_busy(&why) => {
                        t.say(&format!("{label}: busy, a switch is running"), cx)
                    }
                    Err(why) => {
                        if t.switch.is_some_and(|sw| sw.sid == sid) {
                            t.switch = None;
                        }
                        t.say(&format!("{label}: {why}"), cx);
                    }
                    Ok(_) => {}
                }
                cx.notify();
            })
            .ok();
            // Past the wait the button is live again even if the row never came: redraw then.
            cx.background_executor().timer(takeover::WAIT).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
        cx.notify();
    }

    /// The row over the centre with the toggle, for a live Claude session; `None` otherwise.
    pub(super) fn switch_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let s = self.switchable(cx)?;
        let waiting = self.switching(s.id, cx);
        let label = takeover::label(s.mode);
        let tip = takeover::tip(s.mode, waiting);
        let button = if waiting {
            ui::button_disabled("takeover", format!("{label}…"))
        } else {
            ui::button("takeover", label).on_click(cx.listener(|t, _, _, cx| t.take_over(cx)))
        };
        Some(
            div()
                .flex_none()
                .flex()
                .justify_end()
                .child(
                    button
                        .h(px(24.))
                        .px(px(8.))
                        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx)),
                )
                .into_any_element(),
        )
    }
}
