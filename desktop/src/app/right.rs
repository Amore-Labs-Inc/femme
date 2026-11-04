//! The right-hand side of the frame: a 28 px strip on the window's edge with two icons (terminal,
//! files) and the board's button, and the panel the first two open on hover. The pointer and focus
//! rules are pure and live in [`crate::frame::Right`]; this file draws, and carries them out (delays,
//! focus, the width drag).
//!
//! The panel floats over the centre's right edge instead of taking a column, so opening it never
//! reflows the transcript or resizes a terminal.

use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, AnyElement, AppContext as _, Axis, Context, DragMoveEvent, Empty, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, StyleRefinement, Styled as _, Window,
};
use gpui_component::resizable::HandleEdge;
use gpui_component::resize_handle;
use gpui_component::tooltip::Tooltip;

use super::{FemmeApp, ToggleBoard, ToggleTerminal};
use crate::frame::{self, Settle, Tab};
use crate::motion::Appear;
use crate::theme::{self, GUTTER, STRIP_W, SURFACE, TOP_BAR_H};
use crate::ui::{self, Glyph};

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
        self.revealed(tab, cx);
        if focus {
            self.want_focus = false;
            self.focus_right(window, cx);
        }
        cx.notify();
    }

    /// `tab` has come into view. The files tab is a cached view, which is not drawn again by
    /// itself, so it is told to look at the age of its listings (they are asked for again past
    /// their TTL).
    fn revealed(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if tab == Tab::Files {
            self.files.update(cx, |files, cx| files.shown(cx));
        }
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

    /// The keyboard goes into the panel: the tab's own handle.
    fn focus_right(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let own = match self.right.open() {
            Some(Tab::Terminal) => self.terminal.focus_handle(cx),
            Some(Tab::Files) => self.files.focus_handle(cx),
            None => self.right_focus.clone(),
        };
        window.focus(&own, cx);
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
            Settle::Open(tab) => {
                self.revealed(tab, cx);
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
        let glyph = match tab {
            Tab::Terminal => Glyph::Terminal,
            Tab::Files => Glyph::Files,
        };
        let button = if self.right.open() == Some(tab) {
            ui::icon_button_on(tab.label(), glyph)
        } else {
            ui::icon_button(tab.label(), glyph)
        };
        button
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

    /// The strip's last icon opens the drawing board. It occludes the strip under it: the panel is not
    /// for this button, so the pointer passing over it must not open the panel.
    fn board_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        ui::icon_button("board", Glyph::Board)
            .occlude()
            .tooltip(|window, cx| {
                Tooltip::new("board")
                    .action(&ToggleBoard, None)
                    .build(window, cx)
            })
            .on_click(cx.listener(|t, _, window, cx| t.toggle_board(window, cx)))
    }

    /// The strip on the right edge: hovering it opens the panel.
    pub(super) fn strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        ui::strip("strip")
            .h_full()
            .on_hover(cx.listener(|t, on: &bool, window, cx| {
                t.right.hover_strip(*on);
                t.poke(if *on { OPEN_AFTER } else { CLOSE_AFTER }, window, cx);
            }))
            .children(Tab::ALL.map(|tab| self.strip_icon(tab, cx)))
            .child(self.board_button(cx))
    }

    /// The right-hand panel, floating over the centre's right edge so that opening it never
    /// reflows the transcript or resizes a terminal.
    pub(super) fn right_panel(
        &self,
        tab: Tab,
        a: Appear,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // A leaving panel is past hovering: the pointer on it must not bring it back.
        let leaving = self.right.open().is_none();
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
            .top(GUTTER + TOP_BAR_H + GUTTER)
            .bottom(GUTTER)
            // It slides in from the strip, fading up, and goes the same way.
            .right(GUTTER + STRIP_W - a.offset(16.))
            .opacity(a.opacity())
            .w(px(self.right.width()))
            .occlude()
            .track_focus(&self.right_focus)
            .when(!leaving, |p| {
                p.on_hover(cx.listener(|t, on: &bool, window, cx| {
                    t.right.hover_panel(*on);
                    t.poke(CLOSE_AFTER, window, cx);
                }))
            });
        panel.style().box_shadow = Some(theme::hard_shadow());
        panel
            .child(
                ui::win()
                    .size_full()
                    .child(
                        ui::title_bar(tab.label()).child(
                            ui::title_button("right-close", Glyph::Close).on_click(
                                cx.listener(|t, _, window, cx| t.close_right(window, cx)),
                            ),
                        ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .bg(SURFACE)
                            .overflow_hidden()
                            .child(body),
                    ),
            )
            .child(grip)
    }
}
