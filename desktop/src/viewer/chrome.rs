//! The pieces every kind of view shares: the spinner shown while a file is being read, the caption
//! over the content, the plain notice for what cannot be shown, and the probe that tells a view which
//! part of it is on screen.
//!
//! A view is as wide as the space it is given and as tall as its content: whoever hosts it owns the
//! vertical scroll. Only what is wider than the space (code lines, a picture at 1:1) scrolls inside.

use std::time::Duration;

use gpui::{
    canvas, div, px, Animation, AnimationExt as _, App, Bounds, Canvas, Context, Div, ElementId,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Rgba, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _,
};

use crate::theme::{self, FONT_MONO, FONT_PIXEL, INK, MUTED, PAPER, RULE};
use crate::ui;

/// Frames of the spinner, in the pixel font's own ASCII.
const SPIN: [&str; 4] = ["|", "/", "-", "\\"];

/// How tall the box of a spinner is when it stands for a whole view.
const WAITING_HEIGHT: f32 = 120.;

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

/// A spinner standing for a whole view that is not ready: it fills what the caption leaves, and is
/// [`WAITING_HEIGHT`] tall at least.
pub(crate) fn waiting(label: impl Into<SharedString>) -> Div {
    div()
        .flex_1()
        .w_full()
        .min_h(px(WAITING_HEIGHT))
        .child(spinner(label))
}

/// The caption strip: one muted line over a thin ink rule, as tall as its text.
pub(crate) fn caption(text: impl Into<SharedString>) -> Div {
    ui::ruled_sides(
        div().flex_none().w_full().px(px(8.)).py(px(3.)).bg(PAPER),
        px(0.),
        px(0.),
        RULE,
        px(0.),
    )
    .child(ui::meta(text))
}

/// A caption strip over `body`, on `backdrop`. It fills the height its host gives it (the view's
/// root grows to it) and is as tall as the body when that is taller.
pub(crate) fn framed(
    body: impl IntoElement,
    caption_text: impl Into<SharedString>,
    backdrop: Rgba,
) -> Div {
    div()
        .flex_1()
        .w_full()
        .flex()
        .flex_col()
        .bg(backdrop)
        .child(caption(caption_text))
        .child(body)
}

/// A headline and monospace lines (a failure reason, a hex dump) on paper, from the top left; lines
/// wider than the view scroll sideways. `id` keeps the scroll position apart from other notices.
pub(crate) fn notice(
    id: impl Into<ElementId>,
    headline: impl Into<SharedString>,
    lines: Vec<String>,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex_1()
        .flex()
        .w_full()
        .overflow_x_scroll()
        .restrict_scroll_to_axis()
        .bg(PAPER)
        .child(
            div()
                .flex_none()
                .min_w_full()
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
                ),
        )
}

/// What a [`probe`] saw of the element it fills, in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Seen {
    /// The element's width.
    pub(crate) width: f32,
    /// The part of the element that is on screen (inside the window and inside every scroll area
    /// around it), as distances from the element's top edge: `top..bottom`. `0..0` when none is.
    pub(crate) top: f32,
    pub(crate) bottom: f32,
}

impl Seen {
    /// What of an element at `bounds` lies inside `clip` (both in window coordinates).
    pub(crate) fn of(bounds: Bounds<Pixels>, clip: Bounds<Pixels>) -> Seen {
        let top = f32::from(clip.top() - bounds.top()).max(0.);
        let bottom = f32::from(clip.bottom() - bounds.top()).min(f32::from(bounds.size.height));
        let (top, bottom) = if bottom > top {
            (top, bottom)
        } else {
            (0., 0.)
        };
        Seen {
            width: f32::from(bounds.size.width),
            top,
            bottom,
        }
    }

    /// Nothing of the element is on screen.
    pub(crate) fn is_empty(&self) -> bool {
        self.bottom <= self.top
    }
}

/// An invisible element as big as its parent (which has to be positioned) that finds out, while the
/// frame is drawn, what part of it is on screen. It calls `report` after the frame if that differs
/// from `last`. A view that sits in somebody else's scroll area has no other way to know what the
/// reader is looking at.
pub(crate) fn probe(last: Seen, report: impl FnOnce(Seen, &mut App) + 'static) -> Canvas<Seen> {
    canvas(
        |bounds, window, _| Seen::of(bounds, window.content_mask().bounds),
        move |_, seen, _, cx| {
            if seen != last {
                report(seen, cx);
            }
        },
    )
    .absolute()
    .size_full()
}

/// A [`probe`] that hands what it saw to `see` on the view `V` that is being rendered, once the
/// frame is done.
pub(crate) fn watch<V: 'static>(
    last: Seen,
    cx: &mut Context<V>,
    see: fn(&mut V, Seen, &mut Context<V>),
) -> Canvas<Seen> {
    let this = cx.entity().downgrade();
    probe(last, move |seen, cx| {
        cx.defer(move |cx| {
            this.update(cx, |view, cx| see(view, seen, cx)).ok();
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    fn bounds(y: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(10.), px(y)), size(px(300.), px(height)))
    }

    /// The span seen of an element at height `y`, `h` tall, in a clip that runs from 100 to 500.
    fn span(y: f32, h: f32) -> (f32, f32) {
        let s = Seen::of(bounds(y, h), bounds(100., 400.));
        assert_eq!(s.width, 300.);
        (s.top, s.bottom)
    }

    #[test]
    fn what_is_seen_is_the_part_inside_the_clip() {
        assert_eq!(span(150., 100.), (0., 100.), "entirely inside");
        assert_eq!(
            span(-200., 700.),
            (300., 700.),
            "scrolled up, ends at the clip"
        );
        assert_eq!(
            span(-200., 800.),
            (300., 700.),
            "scrolled up, runs past the clip"
        );
        assert_eq!(
            span(400., 1000.),
            (0., 100.),
            "starts inside, runs past the clip"
        );
    }

    #[test]
    fn an_element_outside_the_clip_sees_nothing() {
        let clip = bounds(100., 400.);
        for (y, h) in [(-500., 300.), (600., 300.), (500., 50.), (0., 100.)] {
            let s = Seen::of(bounds(y, h), clip);
            assert!(s.is_empty(), "{y} {h} {s:?}");
            assert_eq!((s.top, s.bottom, s.width), (0., 0., 300.));
        }
    }
}
