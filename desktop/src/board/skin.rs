//! The small controls of the board's tool row in the FEMME skin: a cell that is bare at rest, pink
//! under the pointer and ink while it is on; a word in the pixel font; the greyed word; bordered
//! and bare runs of cells; the icon of a tool.

use gpui::{
    canvas, div, px, relative, Div, ElementId, InteractiveElement as _, ParentElement as _, Rgba,
    Stateful, StatefulInteractiveElement as _, Styled as _,
};

use super::editor::Tool;
use super::icons;
use super::paint::paint_icon;
use crate::theme::{
    self, ACCENT, ACCENT_DEEP, ACCENT_STRONG, BTN_SIZE, FONT_PIXEL, INK, MUTED_TEXT, PAPER,
};
use crate::ui::ruled;

/// Side of a square control of the tool row, in pixels.
pub const CELL: f32 = 28.0;

/// No colour at all (a control at rest has no fill).
pub fn clear() -> Rgba {
    theme::rgba8(0xFEFEFE, 0.0)
}

/// A control of the tool row: no fill at rest, pink under the pointer, neon pressed; ink with a
/// paper label while `on`. Icons and text inside follow its label colour.
pub fn cell(id: impl Into<ElementId>, on: bool) -> Stateful<Div> {
    let (bg, fg, hover) = if on {
        (INK, PAPER, ACCENT_DEEP)
    } else {
        (clear(), INK, ACCENT)
    };
    div()
        .id(id)
        .relative()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(CELL))
        .min_w(px(CELL))
        .bg(bg)
        .text_color(fg)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .active(|s| s.bg(ACCENT_STRONG).text_color(INK))
}

/// A [`cell`] holding a word in the pixel font.
pub fn word_cell(id: impl Into<ElementId>, label: &'static str, on: bool) -> Stateful<Div> {
    cell(id, on)
        .px(px(8.))
        .font_family(FONT_PIXEL)
        .text_size(BTN_SIZE)
        .line_height(relative(1.))
        .child(label)
}

/// A word of the tool row that cannot be used now.
pub fn word_off(id: impl Into<ElementId>, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .h(px(CELL))
        .px(px(8.))
        .font_family(FONT_PIXEL)
        .text_size(BTN_SIZE)
        .line_height(relative(1.))
        .text_color(MUTED_TEXT)
        .cursor_not_allowed()
        .child(label)
}

/// A bordered run of controls.
pub fn group() -> Div {
    ruled(div().flex().flex_none().items_center().bg(PAPER))
}

/// An unbordered run of controls.
pub fn run() -> Div {
    div().flex().flex_none().items_center().gap(px(2.))
}

/// The icon of `tool`, drawn in the colour of the label it sits in.
pub fn tool_icon(tool: Tool) -> impl gpui::IntoElement {
    let lines = icons::icon(tool);
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            let color = window.text_style().color;
            paint_icon(&lines, bounds, color, window);
        },
    )
    .size(px(20.))
}
