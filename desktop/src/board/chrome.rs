//! The board's look: the layer over the window, and the card in it with its title bar, tool row,
//! canvas and status line. Plain `div`s in the FEMME skin (every colour is a [`crate::theme`]
//! token); the canvas is the one element that paints by hand.

use gpui::{
    canvas, div, px, relative, AnyElement, Context, DispatchPhase, Div, InteractiveElement as _,
    IntoElement as _, MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _,
};

use super::drawlist::draw_list;
use super::editor::Tool;
use super::geom::Point;
use super::paint::paint_ops;
use super::Board;
use crate::theme::{
    self, BODY_SIZE, DANGER, DESK, FONT_MONO, FONT_PIXEL, FONT_UI, INK, MUTED, PANEL, PAPER, RULE,
    TB_SIZE,
};
use crate::ui::{button, button_disabled, ruled, ruled_sides, title_button, Glyph};

/// The card's size as a share of the window, and its widest.
const CARD_W: f32 = 0.94;
const CARD_H: f32 = 0.92;
const CARD_MAX_W: f32 = 1500.0;

impl Board {
    /// The layer over the window: a dim backdrop with the card in the middle. It holds the
    /// keyboard focus, so the board's keys work wherever the pointer is.
    pub(super) fn layer(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::rgba8(0x000000, 0.25))
            .occlude()
            .key_context("Board")
            .track_focus(&self.focus)
            .font_family(FONT_UI)
            .text_size(BODY_SIZE)
            .line_height(relative(1.4))
            .text_color(INK)
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(Self::key_up))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|t, _, window, cx| t.dismiss(window, cx)),
            )
            .child(self.card(cx))
            .into_any_element()
    }

    /// The card: paper with the ink rule and the large hard shadow.
    fn card(&self, cx: &mut Context<Self>) -> Div {
        ruled(div().flex().flex_col().bg(PAPER))
            .shadow(theme::hard_shadow())
            .w(relative(CARD_W))
            .max_w(px(CARD_MAX_W))
            .h(relative(CARD_H))
            // A click on the card is not a click on the backdrop.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(self.title_bar(cx))
            .child(self.tool_row(cx))
            .child(self.canvas_el(cx))
            .child(self.status_row())
    }

    /// `‹  board` and, at the right, `send to claude`, `copy` and `clear`.
    fn title_bar(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .py(px(3.))
            .bg(INK)
            .text_color(DESK)
            .font_family(FONT_PIXEL)
            .text_size(TB_SIZE)
            .line_height(relative(1.))
            .child(
                title_button("board-close", Glyph::Back)
                    .on_click(cx.listener(|t, _, window, cx| t.dismiss(window, cx))),
            )
            .child(div().flex_1().child("board"))
            .child(self.actions(cx))
    }

    /// The buttons of the title bar: the three actions, or the question after `clear`.
    fn actions(&self, cx: &mut Context<Self>) -> Div {
        let row = div().flex().flex_none().items_center().gap(px(8.));
        if self.confirm_clear {
            return row
                .child("clear the board?")
                .child(
                    button("board-clear-yes", "yes")
                        .on_click(cx.listener(|t, _, _, cx| t.answer_clear(true, cx))),
                )
                .child(
                    button("board-clear-no", "no")
                        .on_click(cx.listener(|t, _, _, cx| t.answer_clear(false, cx))),
                );
        }
        if !self.editor.has_content() {
            return row
                .child(button_disabled("board-send", "send to claude"))
                .child(button_disabled("board-copy", "copy"))
                .child(button_disabled("board-clear", "clear"));
        }
        row.child(
            button("board-send", "send to claude").on_click(cx.listener(|t, _, _, cx| t.send(cx))),
        )
        .child(button("board-copy", "copy").on_click(cx.listener(|t, _, _, cx| t.copy_image(cx))))
        .child(button("board-clear", "clear").on_click(cx.listener(|t, _, _, cx| t.ask_clear(cx))))
    }

    /// Called when the canvas has a new size: the editor needs it to fit the first drawing and to
    /// zoom around the middle.
    pub(super) fn canvas_resized(&mut self, cx: &mut Context<Self>) {
        let size = self.canvas.get().size;
        self.editor
            .set_viewport(Point::new(f32::from(size.width), f32::from(size.height)));
        cx.notify();
    }

    /// The canvas: the draw list painted by hand, and the mouse, wheel and pinch handlers.
    fn canvas_el(&self, cx: &mut Context<Self>) -> Div {
        // The list is made for the size the canvas had in the last frame; if it changed, the
        // measure below asks for another frame.
        let seen = self.canvas.get().size;
        let ops = draw_list(
            &self.editor,
            Point::new(f32::from(seen.width), f32::from(seen.height)),
        );
        let place = self.canvas.clone();
        let measure = cx.entity();
        let drive = cx.entity();
        let surface = canvas(
            move |bounds, window, cx| {
                let before = place.replace(bounds);
                if before.size != bounds.size {
                    window.defer(cx, move |_, cx| {
                        measure.update(cx, |b, cx| b.canvas_resized(cx));
                    });
                }
            },
            move |bounds, (), window, cx| {
                paint_ops(&ops, bounds.origin, window, cx);
                // A drag follows the pointer out of the canvas, so these listen to the whole window.
                let this = drive.clone();
                window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                    if phase == DispatchPhase::Capture {
                        this.update(cx, |b, cx| b.mouse_move(ev, cx));
                    }
                });
                window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Capture {
                        drive.update(cx, |b, cx| b.mouse_up(ev, cx));
                    }
                });
            },
        )
        .absolute()
        .size_full();
        let el = div()
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(PAPER)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::mouse_down))
            .on_scroll(cx.listener(Self::scroll))
            .on_pinch(cx.listener(Self::pinch))
            .child(surface);
        match (self.editor.is_panning(), self.editor.is_dragging()) {
            (true, true) => el.cursor_grabbing(),
            (true, false) => el.cursor_grab(),
            _ => match self.editor.tool() {
                Tool::Select => el.cursor_default(),
                Tool::Text => el.cursor_text(),
                _ => el.cursor_crosshair(),
            },
        }
    }

    /// What the pointer would do now, in a few words.
    fn hint(&self) -> &'static str {
        if self.editor.typing().is_some() {
            return "type; enter finishes, shift-enter adds a line, esc keeps the text";
        }
        match self.editor.tool() {
            Tool::Select if self.editor.selection().is_empty() => {
                "click or drag a box to select; the wheel pans, ctrl-wheel zooms"
            }
            Tool::Select => "drag to move; arrows nudge; ctrl-d duplicates; delete removes",
            Tool::Rect | Tool::Ellipse => "drag to draw; shift keeps it square",
            Tool::Arrow | Tool::Line => "drag to draw; shift snaps to 45 degrees",
            Tool::Freehand => "drag to scribble",
            Tool::Text => "click to place text, then type",
        }
    }

    /// The line under the canvas: feedback or a hint at the left, the size of the drawing and the
    /// zoom at the right.
    fn status_row(&self) -> Div {
        let left = match &self.notice {
            Some(n) => div()
                .text_color(if n.bad { DANGER } else { INK })
                .child(n.text.clone()),
            None => div().text_color(MUTED).child(self.hint()),
        };
        let shapes = self.editor.scene().len();
        ruled_sides(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(12.))
                .h(px(24.))
                .px(px(8.))
                .bg(PANEL)
                .font_family(FONT_MONO)
                .text_size(px(12.))
                .whitespace_nowrap()
                .overflow_hidden(),
            RULE,
            px(0.),
            px(0.),
            px(0.),
        )
        .child(div().flex_1().min_w_0().truncate().child(left))
        .child(div().flex_none().text_color(MUTED).child(format!(
            "{shapes} {}   {}%",
            if shapes == 1 { "shape" } else { "shapes" },
            self.editor.view().percent()
        )))
    }
}
