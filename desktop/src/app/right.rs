//! The right-hand side of the frame: a 28 px strip on the window's edge with two icons (terminal,
//! files), and the panel its hover opens. The pointer and focus rules are pure and live in
//! [`crate::frame::Right`]; this file draws, and carries them out (delays, focus, the width drag).
//!
//! The panel floats over the centre's right edge instead of taking a column, so opening it never
//! reflows the transcript or resizes a terminal.

use std::time::Duration;

use gpui::{
    div, px, relative, AnyElement, AppContext as _, Axis, Context, Div, DragMoveEvent, Empty,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Rgba,
    StatefulInteractiveElement as _, StyleRefinement, Styled as _, Window,
};
use gpui_component::resizable::HandleEdge;
use gpui_component::resize_handle;
use gpui_component::tooltip::Tooltip;

use super::{FemmeApp, ToggleTerminal};
use crate::frame::{self, Settle, Tab, MARGIN, STRIP_W, TOP_BAR};
use crate::theme::{self, FONT_MONO, HOVER, INK, PAPER};
use crate::ui::{self, ruled};

/// How long the pointer rests on the strip before the panel opens.
const OPEN_AFTER: Duration = Duration::from_millis(120);
/// How long the pointer is away before the panel closes: long enough to cross a seam or overshoot.
pub(super) const CLOSE_AFTER: Duration = Duration::from_millis(280);

/// The payload of a drag of the right-hand panel's left edge.
#[derive(Clone)]
struct RightGrip;

impl Render for RightGrip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl FemmeApp {
    /// Opens `tab` of the right-hand panel; `focus` puts the keyboard in it (a click, a key) so it
    /// stays open when the pointer leaves.
    pub(super) fn open_right(
        &mut self,
        tab: Tab,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.right.show(tab);
        if focus {
            self.want_focus = false;
            self.focus_right(window, cx);
        }
        cx.notify();
    }

    pub(super) fn close_right(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let had_focus = self.right_focus.contains_focused(window, cx);
        self.right.hide();
        if had_focus {
            self.land_focus(window, cx);
        }
        cx.notify();
    }

    /// mod+j: the terminal tab opens with the keyboard in it, or closes when it is the one on show.
    pub(super) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.right.open() == Some(Tab::Terminal) {
            self.close_right(window, cx);
        } else {
            self.open_right(Tab::Terminal, true, window, cx);
        }
    }

    /// The keyboard goes into the panel: the tab's own handle when its module has one.
    fn focus_right(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let own = match self.right.open() {
            Some(Tab::Terminal) => focus_of!(self.terminal.read(cx), cx),
            Some(Tab::Files) => focus_of!(self.files.read(cx), cx),
            None => None,
        };
        window.focus(&own.unwrap_or_else(|| self.right_focus.clone()), cx);
    }

    /// Decides what the pointer and the focus ask of the right-hand panel in a moment: the delay
    /// lets the pointer cross a seam or overshoot without the panel flickering.
    pub(super) fn poke(&mut self, after: Duration, window: &mut Window, cx: &mut Context<Self>) {
        self.settle = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(after).await;
            this.update_in(cx, |t, window, cx| t.settle_right(window, cx))
                .ok();
        }));
    }

    fn settle_right(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inside = self.right_focus.contains_focused(window, cx);
        let dragging = cx.has_active_drag();
        match self.right.settle(inside, dragging) {
            Settle::Nothing => {
                // No leave event comes when a drag ends outside the panel: look again.
                if dragging && self.right.open().is_some() {
                    self.poke(CLOSE_AFTER, window, cx);
                }
            }
            // The tab changed under the keyboard: the old tab's module is gone, so it follows.
            Settle::Open(_) => {
                if inside {
                    self.focus_right(window, cx);
                }
                cx.notify();
            }
            Settle::Close => cx.notify(),
        }
    }

    /// One of the strip's icons: it opens its tab when the pointer rests on it, or on a click.
    fn strip_icon(&self, tab: Tab, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.right.open() == Some(tab);
        let (bg, fg) = if on { (INK, PAPER) } else { (PAPER, INK) };
        let id = tab.label();
        ruled(
            div()
                .id(id)
                .flex_none()
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .bg(bg)
                .cursor_pointer()
                // Hover recolours the background only (see `ui::button_dark`).
                .hover(|s| s.bg(HOVER)),
        )
        .child(match tab {
            Tab::Terminal => icon_terminal(fg),
            Tab::Files => icon_files(fg),
        })
        .tooltip(move |window, cx| {
            let tip = Tooltip::new(tab.label());
            match tab {
                Tab::Terminal => tip.action(&ToggleTerminal, Some("Femme")),
                Tab::Files => tip,
            }
            .build(window, cx)
        })
        .on_hover(cx.listener(move |t, on: &bool, window, cx| {
            t.right.hover_icon(tab, *on);
            t.poke(if *on { OPEN_AFTER } else { CLOSE_AFTER }, window, cx);
        }))
        .on_click(cx.listener(move |t, _, window, cx| t.open_right(tab, true, window, cx)))
    }

    /// The 28 px strip on the right edge: hovering it opens the panel.
    pub(super) fn strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        ruled(
            div()
                .id("strip")
                .flex_none()
                .w(px(STRIP_W))
                .h_full()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(4.))
                .py(px(4.))
                .bg(PAPER),
        )
        .on_hover(cx.listener(|t, on: &bool, window, cx| {
            t.right.hover_strip(*on);
            t.poke(if *on { OPEN_AFTER } else { CLOSE_AFTER }, window, cx);
        }))
        .children(Tab::ALL.map(|tab| self.strip_icon(tab, cx)))
    }

    /// The right-hand panel, floating over the centre's right edge so that opening it never
    /// reflows the transcript or resizes a terminal.
    pub(super) fn right_panel(&self, tab: Tab, cx: &mut Context<Self>) -> impl IntoElement {
        let full = StyleRefinement::default().size_full();
        let body: AnyElement = match tab {
            Tab::Terminal => self.terminal.clone().cached(full).into_any_element(),
            Tab::Files => self.files.clone().cached(full).into_any_element(),
        };
        let grip = resize_handle::<RightGrip, RightGrip>("right-grip", Axis::Horizontal)
            .inside(HandleEdge::Leading)
            // The grip occludes the panel under it: the panel does not read as hovered there.
            .on_hover(cx.listener(|t, on: &bool, window, cx| {
                t.right.hover_grip(*on);
                t.poke(CLOSE_AFTER, window, cx);
            }))
            .on_drag(RightGrip, |grip, _, _, cx| cx.new(|_| grip.clone()))
            .on_drag_move::<RightGrip>(cx.listener(
                |t, ev: &DragMoveEvent<RightGrip>, window, cx| {
                    let viewport = f32::from(window.viewport_size().width);
                    let width = frame::width_for(f32::from(ev.event.position.x), viewport);
                    t.right.set_width(width, viewport);
                    cx.notify();
                },
            ));
        let mut panel = div()
            .id("right-panel")
            .absolute()
            .top(px(MARGIN + TOP_BAR + MARGIN))
            .bottom(px(MARGIN))
            .right(px(MARGIN + STRIP_W))
            .w(px(self.right.width()))
            .occlude()
            .track_focus(&self.right_focus)
            .on_hover(cx.listener(|t, on: &bool, window, cx| {
                t.right.hover_panel(*on);
                t.poke(CLOSE_AFTER, window, cx);
            }));
        panel.style().box_shadow = Some(theme::hard_shadow());
        panel
            .child(
                ui::win()
                    .size_full()
                    .child(
                        ui::tb(tab.label()).child(div().flex_1()).child(
                            div()
                                .id("right-close")
                                .px(px(6.))
                                .cursor_pointer()
                                .hover(|s| s.bg(HOVER))
                                .child("x")
                                .on_click(
                                    cx.listener(|t, _, window, cx| t.close_right(window, cx)),
                                ),
                        ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .bg(PAPER)
                            .overflow_hidden()
                            .child(body),
                    ),
            )
            .child(grip)
    }
}

/// The terminal icon: a framed `>_`.
fn icon_terminal(color: Rgba) -> Div {
    ruled_in(
        div()
            .w(px(18.))
            .h(px(14.))
            .flex()
            .items_center()
            .justify_center()
            .font_family(FONT_MONO)
            .text_size(px(9.))
            .line_height(relative(1.))
            .text_color(color)
            .child(">_"),
        color,
    )
}

/// The files icon: a folder, a tab on top of a framed body.
fn icon_files(color: Rgba) -> Div {
    let b = theme::RULE;
    div()
        .flex()
        .flex_col()
        .child(ui::ruled_sides(div().w(px(8.)).h(px(3.)), b, b, px(0.), b).border_color(color))
        .child(ruled_in(div().w(px(18.)).h(px(11.)), color))
}

/// `ui::ruled` in a colour other than ink.
fn ruled_in(el: Div, color: Rgba) -> Div {
    ruled(el).border_color(color)
}
