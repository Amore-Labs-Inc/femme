//! The pieces every kind of view shares: the spinner shown while a file is being read, the caption
//! strip under the picture, and the plain notice for what cannot be shown.

use std::time::Duration;

use gpui::{
    div, px, Animation, AnimationExt as _, Div, ElementId, InteractiveElement as _, IntoElement,
    ParentElement as _, Rgba, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _,
};

use crate::theme::{self, FONT_MONO, FONT_PIXEL, INK, MUTED, PAPER, RULE};
use crate::ui;

/// Frames of the spinner, in the pixel font's own ASCII.
const SPIN: [&str; 4] = ["|", "/", "-", "\\"];

/// One frame of the spinner; `t` runs 0..1 around the cycle.
fn spin_glyph(t: f32) -> Div {
    let frame = SPIN[((t * SPIN.len() as f32) as usize).min(SPIN.len() - 1)];
    div()
        .font_family(FONT_PIXEL)
        .text_size(theme::TB_SIZE)
        .text_color(INK)
        .child(frame)
}

/// A spinner with a muted label under it, centred in whatever it is placed in.
pub(crate) fn spinner(label: impl Into<SharedString>) -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(2.))
        .child(
            spin_glyph(0.).with_animation(
                "viewer-spin",
                Animation::new(Duration::from_millis(640))
                    .repeat()
                    .with_max_fps(12.),
                |_, t| spin_glyph(t),
            ),
        )
        .child(ui::meta(label))
}

/// The caption strip: one muted line over a thin ink rule, as tall as its text.
pub(crate) fn caption(text: impl Into<SharedString>) -> Div {
    ui::ruled_sides(
        div().flex_none().w_full().px(px(8.)).py(px(3.)).bg(PAPER),
        RULE,
        px(0.),
        px(0.),
        px(0.),
    )
    .child(ui::meta(text))
}

/// `body` filling the space over a caption strip, on `backdrop`.
pub(crate) fn framed(
    body: impl IntoElement,
    caption_text: impl Into<SharedString>,
    backdrop: Rgba,
) -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(backdrop)
        .child(
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_hidden()
                .child(div().absolute().top_0().left_0().size_full().child(body)),
        )
        .child(caption(caption_text))
}

/// A headline and monospace lines (a failure reason, a hex dump) on paper, from the top left;
/// scrolls when they do not fit. `id` keeps the scroll position apart from other notices.
pub(crate) fn notice(
    id: impl Into<ElementId>,
    headline: impl Into<SharedString>,
    lines: Vec<String>,
) -> Stateful<Div> {
    div()
        .id(id)
        .size_full()
        .overflow_scroll()
        .bg(PAPER)
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .flex_none()
                .font_family(theme::FONT_UI)
                .text_size(theme::BODY_SIZE)
                .text_color(INK)
                .child(headline.into()),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .font_family(FONT_MONO)
                .text_size(theme::META_SIZE)
                .text_color(MUTED)
                .whitespace_nowrap()
                .children(
                    lines
                        .into_iter()
                        .map(|l| div().child(SharedString::from(l))),
                ),
        )
}
