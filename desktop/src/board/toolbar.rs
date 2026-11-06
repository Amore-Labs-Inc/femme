//! The row under the title bar: the tools, the colours, widths and fills of what is drawn next,
//! the sketchy switch, undo and redo, and the zoom.

use gpui::{
    div, px, relative, Context, Div, InteractiveElement as _, IntoElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _,
};

use super::editor::Tool;
use super::palette;
use super::scene::{Fill, StrokeColor, Tone, Width};
use super::skin::{cell, clear, group, run, tool_icon, word_cell, word_off, CELL};
use super::Board;
use crate::theme::{accent_soft, ink, paper, BTN_SIZE, FONT_MONO, FONT_PIXEL, RULE};
use crate::ui::{ruled, ruled_sides};

impl Board {
    /// The row under the title bar: tools, colours, widths, fills, the sketchy switch, and at the
    /// right undo, redo and the zoom.
    pub(super) fn tool_row(&self, cx: &mut Context<Self>) -> Div {
        ruled_sides(
            div()
                .flex()
                .flex_none()
                .flex_wrap()
                .items_center()
                .gap_x(px(12.))
                .gap_y(px(4.))
                .px(px(8.))
                .py(px(5.))
                .bg(paper()),
            px(0.),
            px(0.),
            RULE,
            px(0.),
        )
        .child(self.tools(cx))
        .child(self.colors(cx))
        .child(self.widths(cx))
        .child(self.fills(cx))
        .child(self.sketchy(cx))
        .child(div().flex_1())
        .child(self.history(cx))
        .child(self.zoom(cx))
    }

    /// The seven tools, each with its icon and the key that picks it.
    fn tools(&self, cx: &mut Context<Self>) -> Div {
        let current = self.editor.tool();
        group().children(Tool::ALL.into_iter().enumerate().map(|(i, tool)| {
            cell(("board-tool", i), tool == current)
                .w(px(CELL))
                .child(tool_icon(tool))
                .child(
                    div()
                        .absolute()
                        .right(px(3.))
                        .bottom(px(1.))
                        .font_family(FONT_MONO)
                        .text_size(px(9.))
                        .opacity(0.7)
                        .child(tool.number().to_string()),
                )
                .on_click(cx.listener(move |t, _, _, cx| {
                    t.editor.set_tool(tool);
                    t.changed(cx);
                }))
        }))
    }

    /// The six stroke colours.
    fn colors(&self, cx: &mut Context<Self>) -> Div {
        let current = self.editor.style().color;
        run().children(StrokeColor::ALL.into_iter().enumerate().map(|(i, color)| {
            let ring = if color == current { ink() } else { clear() };
            ruled(
                div()
                    .id(("board-color", i))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(26.))
                    .cursor_pointer()
                    .hover(|s| s.bg(accent_soft())),
            )
            .border_color(ring)
            .child(ruled(
                div().size(px(16.)).bg(palette::token(Tone::from(color))),
            ))
            .on_click(cx.listener(move |t, _, _, cx| {
                t.editor.set_color(color);
                t.changed(cx);
            }))
        }))
    }

    /// Thin and thick.
    fn widths(&self, cx: &mut Context<Self>) -> Div {
        let current = self.editor.style().width;
        group().children(
            [(Width::Thin, 2.0), (Width::Thick, 4.0)]
                .into_iter()
                .enumerate()
                .map(|(i, (width, height))| {
                    let on = width == current;
                    cell(("board-width", i), on)
                        .w(px(CELL + 4.))
                        .child(
                            div()
                                .w(px(16.))
                                .h(px(height))
                                .bg(if on { paper() } else { ink() }),
                        )
                        .on_click(cx.listener(move |t, _, _, cx| {
                            t.editor.set_width(width);
                            t.changed(cx);
                        }))
                }),
        )
    }

    /// No fill, soft pink, panel grey.
    fn fills(&self, cx: &mut Context<Self>) -> Div {
        let current = self.editor.style().fill;
        group().children(
            [
                (Fill::None, paper()),
                (Fill::Soft, palette::token(Tone::Soft)),
                (Fill::Panel, palette::token(Tone::Panel)),
            ]
            .into_iter()
            .enumerate()
            .map(|(i, (fill, sample))| {
                cell(("board-fill", i), fill == current)
                    .w(px(CELL))
                    .child(ruled(div().size(px(14.)).bg(sample)))
                    .on_click(cx.listener(move |t, _, _, cx| {
                        t.editor.set_fill(fill);
                        t.changed(cx);
                    }))
            }),
        )
    }

    /// The switch between the hand-drawn look and clean lines.
    fn sketchy(&self, cx: &mut Context<Self>) -> Div {
        group().child(
            word_cell("board-sketchy", "sketchy", self.editor.scene().sketchy).on_click(
                cx.listener(|t, _, _, cx| {
                    t.editor.toggle_sketchy();
                    t.changed(cx);
                }),
            ),
        )
    }

    /// Undo and redo.
    fn history(&self, cx: &mut Context<Self>) -> Div {
        let undo = if self.editor.can_undo() {
            word_cell("board-undo", "undo", false)
                .on_click(cx.listener(|t, _, _, cx| {
                    t.editor.undo();
                    t.changed(cx);
                }))
                .into_any_element()
        } else {
            word_off("board-undo", "undo").into_any_element()
        };
        let redo = if self.editor.can_redo() {
            word_cell("board-redo", "redo", false)
                .on_click(cx.listener(|t, _, _, cx| {
                    t.editor.redo();
                    t.changed(cx);
                }))
                .into_any_element()
        } else {
            word_off("board-redo", "redo").into_any_element()
        };
        run().child(undo).child(redo)
    }

    /// Zoom out, the zoom (a click makes it 100 %), zoom in, fit.
    fn zoom(&self, cx: &mut Context<Self>) -> Div {
        let percent = format!("{}%", self.editor.view().percent());
        group()
            .child(
                word_cell("board-zoom-out", "-", false).on_click(cx.listener(|t, _, _, cx| {
                    t.editor.zoom_by(1.0 / super::editor::ZOOM_STEP);
                    t.changed(cx);
                })),
            )
            .child(
                cell("board-zoom-reset", false)
                    .w(px(52.))
                    .font_family(FONT_PIXEL)
                    .text_size(BTN_SIZE)
                    .line_height(relative(1.))
                    .child(percent)
                    .on_click(cx.listener(|t, _, _, cx| {
                        t.editor.zoom_reset();
                        t.changed(cx);
                    })),
            )
            .child(
                word_cell("board-zoom-in", "+", false).on_click(cx.listener(|t, _, _, cx| {
                    t.editor.zoom_by(super::editor::ZOOM_STEP);
                    t.changed(cx);
                })),
            )
            .child(
                word_cell("board-fit", "fit", false).on_click(cx.listener(|t, _, _, cx| {
                    t.editor.fit();
                    t.changed(cx);
                })),
            )
    }
}
