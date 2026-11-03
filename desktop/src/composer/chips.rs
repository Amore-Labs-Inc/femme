//! The attachment chips above the input, and the preview that opens while the pointer rests on one.
//! Plain `div`s in the FEMME skin; gpui-component's `Attachment` and `Spinner` are themed for other
//! looks and draw their icons from SVG assets this app does not register.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, img, prelude::FluentBuilder as _, px, relative, Anchor, Animation, AnimationExt as _,
    AnyElement, App, AppContext as _, Context, Div, Entity, Image, InteractiveElement as _,
    IntoElement, ObjectFit, ParentElement as _, StatefulInteractiveElement as _, Styled as _,
    StyledImage as _,
};
use gpui_component::hover_card::HoverCard;

use super::draft::{Chip, ChipId, Upload};
use super::sniff;
use super::Composer;
use crate::theme::{
    BAD, FONT_MONO, FONT_PIXEL, FONT_UI, HOVER, INK, META_SIZE, MUTED, PANEL, PAPER,
};
use crate::ui::{dialog, meta, ruled};
use crate::viewer::FileView;

/// The viewer entities behind the previews of non-image chips, made on the first hover and kept
/// until the chip goes, so hovering again does not decode the file again.
pub(super) type Previews = Rc<RefCell<HashMap<ChipId, Entity<FileView>>>>;

/// The widest a previewed image is drawn.
const PREVIEW_MAX: f32 = 480.;

/// The most the chips take of the screen before they scroll: three rows of them.
const ROWS_MAX: f32 = 196.;

/// The chips, left to right, wrapping onto more lines when they do not fit.
pub(super) fn row(
    chips: &[Chip],
    previews: &Previews,
    cx: &mut Context<Composer>,
) -> impl IntoElement {
    div()
        .id("composer-chips")
        .flex()
        .flex_wrap()
        .gap(px(6.))
        .max_h(px(ROWS_MAX))
        .overflow_y_scroll()
        .children(chips.iter().map(|c| chip(c, previews, cx)))
}

/// What the preview shows of a chip, cloned so the popover owns it.
struct Preview {
    id: ChipId,
    name: String,
    data: Arc<Vec<u8>>,
    image: Option<Arc<Image>>,
    dims: Option<(u32, u32)>,
    error: Option<String>,
}

fn chip(c: &Chip, previews: &Previews, cx: &mut Context<Composer>) -> impl IntoElement {
    let failed = matches!(c.upload, Upload::Failed(_));
    let body = ruled(
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .p(px(4.))
            .max_w(px(260.))
            .bg(PAPER),
    )
    .border_color(if failed { BAD } else { INK })
    .child(thumb(c))
    .child(info(c, cx))
    .child(button(("chip-x", c.id), "×").on_click(cx.listener({
        let id = c.id;
        move |this, _, _, cx| this.remove_chip(id, cx)
    })));

    let preview = Preview {
        id: c.id,
        name: c.name.clone(),
        data: c.data.clone(),
        image: c.image.clone(),
        dims: c.dims,
        error: match &c.upload {
            Upload::Failed(why) => Some(why.clone()),
            _ => None,
        },
    };
    let previews = previews.clone();
    HoverCard::new(("chip-hover", c.id))
        .anchor(Anchor::BottomLeft)
        .open_delay(Duration::from_millis(200))
        .close_delay(Duration::from_millis(120))
        .appearance(false)
        .trigger(body)
        .content(move |_, _, cx| preview_card(&preview, &previews, cx))
}

/// A small VT323 button inside a chip (`×`, `retry`).
fn button(id: impl Into<gpui::ElementId>, label: &'static str) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .px(px(4.))
        .font_family(FONT_PIXEL)
        .text_size(px(20.))
        .line_height(relative(1.))
        .text_color(INK)
        .cursor_pointer()
        .hover(|s| s.bg(HOVER))
        .child(label)
}

/// The 48px square: the picture cropped to fill it, or the file's extension.
fn thumb(c: &Chip) -> impl IntoElement {
    let label = sniff::ext_label(&c.name);
    let tile = ruled(
        div()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .size(px(48.))
            .bg(PANEL)
            .overflow_hidden()
            .font_family(FONT_PIXEL)
            .text_size(px(20.))
            .line_height(relative(1.))
            .text_color(INK),
    )
    .when(matches!(c.upload, Upload::Uploading), |t| t.opacity(0.5));
    match &c.image {
        Some(image) => tile.child(
            img(image.clone())
                .size_full()
                .object_fit(ObjectFit::Cover)
                .with_fallback(move || div().child(label.clone()).into_any_element()),
        ),
        None => tile.child(label),
    }
}

/// Name over a status line: the size, `uploading` with a spinner, or `failed` with a retry button.
fn info(c: &Chip, cx: &mut Context<Composer>) -> impl IntoElement {
    let failed = matches!(c.upload, Upload::Failed(_));
    let status = div()
        .flex()
        .items_center()
        .gap(px(6.))
        .font_family(FONT_MONO)
        .text_size(META_SIZE)
        .text_color(MUTED);
    let status = match &c.upload {
        Upload::Uploading => status.child(spinner(c.id)).child("uploading"),
        Upload::Ready(_) => status.child(size_line(c)),
        Upload::Failed(_) => status.child(div().text_color(BAD).child("failed")).child(
            button(("chip-retry", c.id), "retry").on_click(cx.listener({
                let id = c.id;
                move |this, _, _, cx| this.retry_chip(id, cx)
            })),
        ),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .min_w_0()
        .child(
            div()
                .max_w(px(160.))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .font_family(FONT_UI)
                .text_size(px(13.))
                .text_color(if failed { BAD } else { INK })
                .child(c.name.clone()),
        )
        .child(status)
}

/// `1.5 KB`, or `1.5 KB · 640x480` for a picture whose header says.
fn size_line(c: &Chip) -> String {
    let size = sniff::human_size(c.data.len());
    match c.dims {
        Some((w, h)) => format!("{size} · {w}x{h}"),
        None => size,
    }
}

/// A `|/-\` that turns, in the pixel font. gpui stops it when the user asks for reduced motion.
fn spinner(id: ChipId) -> impl IntoElement {
    let frame = |glyph: &'static str| {
        div()
            .w(px(8.))
            .font_family(FONT_PIXEL)
            .text_size(px(16.))
            .line_height(relative(1.))
            .child(glyph)
    };
    frame("|").with_animation(
        ("chip-spin", id),
        Animation::new(Duration::from_millis(600))
            .repeat()
            .with_max_fps(10.),
        move |_, t| frame(["|", "/", "-", "\\"][((t * 4.) as usize).min(3)]),
    )
}

/// The popover: name and size, the failure if there was one, then the picture at up to 480px or
/// the viewer's rendering of the file.
fn preview_card(p: &Preview, previews: &Previews, cx: &mut App) -> impl IntoElement {
    let body: AnyElement = match &p.image {
        Some(image) => {
            let (w, h) = p
                .dims
                .map_or((320., 240.), |(w, h)| sniff::fit_within(w, h, PREVIEW_MAX));
            img(image.clone())
                .w(px(w))
                .h(px(h))
                .object_fit(ObjectFit::Contain)
                .into_any_element()
        }
        None => {
            let view = previews
                .borrow_mut()
                .entry(p.id)
                .or_insert_with(|| cx.new(|cx| FileView::new(p.name.clone(), p.data.to_vec(), cx)))
                .clone();
            div()
                .w(px(420.))
                .h(px(260.))
                .overflow_hidden()
                .child(view)
                .into_any_element()
        }
    };
    let mut size = sniff::human_size(p.data.len());
    if let Some((w, h)) = p.dims {
        size = format!("{size} · {w}x{h}");
    }
    // The card clips what overflows it, so the hard shadow gets room inside a wrapper.
    div().pr(px(3.)).pb(px(3.)).child(
        dialog()
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(8.))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(8.))
                    .child(
                        div()
                            .font_family(FONT_UI)
                            .text_size(px(13.))
                            .text_color(INK)
                            .child(p.name.clone()),
                    )
                    .child(meta(size)),
            )
            .when_some(p.error.clone(), |d, why| {
                d.child(
                    div()
                        .max_w(px(420.))
                        .font_family(FONT_MONO)
                        .text_size(META_SIZE)
                        .text_color(BAD)
                        .child(format!("upload failed: {why}")),
                )
            })
            .child(body),
    )
}
