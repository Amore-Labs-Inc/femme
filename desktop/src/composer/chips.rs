//! The attachment chips above the input, and the preview that opens while the pointer rests on one.
//! A right click on a chip opens its menu (`copy image` for a picture, `open`, `remove`), drawn by
//! [`crate::ui::ctx`]. Plain `div`s in the FEMME skin; gpui-component's `Attachment` and `Spinner`
//! are themed for other looks and draw their icons from SVG assets this app does not register.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use femme_proto::Sid;
use gpui::{
    div, img, prelude::FluentBuilder as _, px, relative, Anchor, Animation, AnimationExt as _,
    AnyElement, App, AppContext as _, Context, Div, Entity, Image, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, ObjectFit, ParentElement as _, Pixels, Point,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _,
};
use gpui_component::hover_card::HoverCard;
use gpui_component::scroll::ScrollableElement as _;

use super::draft::{Chip, ChipId, Upload};
use super::sniff;
use super::Composer;
use crate::theme::{
    BAD, FONT_MONO, FONT_PIXEL, FONT_UI, HOVER, INK, META_SIZE, MUTED, PANEL, PAPER, RULE,
};
use crate::ui::ctx::Item;
use crate::ui::{clip, dialog, meta, ruled};
use crate::viewer::FileView;

/// The viewer entities behind the previews of non-image chips, made on the first hover and kept
/// until the chip goes, so hovering again does not decode the file again.
pub(super) type Previews = Rc<RefCell<HashMap<ChipId, Entity<FileView>>>>;

/// The widest a previewed image is drawn.
const PREVIEW_MAX: f32 = 480.;

/// The gap between chips, across and down.
const GAP: f32 = 6.;

/// How much of a third row shows under the two whole ones when there are more chips than that: the
/// cut-off row is what tells the user that the box scrolls.
const PEEK: f32 = 22.;

/// The most the chips take of the screen before they scroll: two rows and the top of a third. A chip
/// is its 48px thumbnail, 4px of padding on each side and the rule round it.
fn rows_max() -> Pixels {
    (px(48. + 2. * 4.) + RULE * 2.) * 2. + px(GAP) * 2. + px(PEEK)
}

/// The chips, left to right, wrapping onto more lines when they do not fit. Past [`rows_max`] the
/// box scrolls, with the scrollbar the theme shows (while the wheel moves, on this platform).
pub(super) fn row(
    chips: &[Chip],
    previews: &Previews,
    cx: &mut Context<Composer>,
) -> impl IntoElement {
    div()
        .id("composer-chips")
        .flex()
        .flex_wrap()
        .gap(px(GAP))
        .max_h(rows_max())
        .overflow_y_scrollbar()
        .children(chips.iter().map(|c| chip(c, previews, cx)))
}

/// What the preview shows of a chip, cloned so the popover owns it.
#[derive(Clone)]
struct Preview {
    id: ChipId,
    name: String,
    data: Arc<Vec<u8>>,
    image: Option<Arc<Image>>,
    dims: Option<(u32, u32)>,
    error: Option<String>,
}

impl Preview {
    fn of(c: &Chip) -> Self {
        Preview {
            id: c.id,
            name: c.name.clone(),
            data: c.data.clone(),
            image: c.image.clone(),
            dims: c.dims,
            error: match &c.upload {
                Upload::Failed(why) => Some(why.clone()),
                _ => None,
            },
        }
    }
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
    })))
    .on_mouse_down(
        MouseButton::Right,
        cx.listener({
            let id = c.id;
            move |this, ev: &MouseDownEvent, _, cx| this.chip_menu(id, ev.position, cx)
        }),
    );

    let preview = Preview::of(c);
    let previews = previews.clone();
    HoverCard::new(("chip-hover", c.id))
        .anchor(Anchor::BottomLeft)
        .open_delay(Duration::from_millis(200))
        .close_delay(Duration::from_millis(120))
        .appearance(false)
        .trigger(body)
        .content(move |_, _, cx| preview_card(&preview, &previews, cx))
}

/// One row of a chip's menu.
enum Entry {
    /// The picture, to the clipboard.
    Copy(Arc<Image>),
    /// The chip's preview, kept on screen.
    Open,
    /// The chip's `×`.
    Remove,
}

impl Entry {
    fn label(&self) -> &'static str {
        match self {
            Entry::Copy(_) => "copy image",
            Entry::Open => "open",
            Entry::Remove => "remove",
        }
    }
}

/// The rows of a chip's menu: `copy image` when the chip is a picture that can be copied, then
/// `open` and `remove`.
fn entries(image: Option<&Arc<Image>>) -> Vec<Entry> {
    let copy = image
        .filter(|i| clip::copyable(i))
        .map(|i| Entry::Copy(i.clone()));
    copy.into_iter()
        .chain([Entry::Open, Entry::Remove])
        .collect()
}

impl Composer {
    /// The menu of the chip `id`, opened at `at` by a right click on it.
    pub(super) fn chip_menu(&mut self, id: ChipId, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(sid) = self.session else { return };
        let Some(chip) = self.drafts.get(sid).and_then(|d| d.chip(id)) else {
            return;
        };
        let (preview, previews) = (Preview::of(chip), self.previews.clone());
        let items = entries(chip.image.as_ref())
            .into_iter()
            .map(|entry| {
                let label = entry.label();
                match entry {
                    Entry::Copy(image) => {
                        let name = chip.name.clone();
                        Item::new(
                            label,
                            cx.listener(move |this, _, _, cx| {
                                this.copy_chip(sid, &name, &image, cx)
                            }),
                        )
                    }
                    Entry::Open => {
                        let (preview, previews) = (preview.clone(), previews.clone());
                        Item::new(
                            label,
                            cx.listener(move |this, _, _, cx| {
                                let (preview, previews) = (preview.clone(), previews.clone());
                                this.ctx.update(cx, |menu, cx| {
                                    let card = move |_: &mut _, cx: &mut App| {
                                        preview_card(&preview, &previews, cx).into_any_element()
                                    };
                                    menu.pin(at, card, cx)
                                });
                            }),
                        )
                    }
                    Entry::Remove => Item::new(
                        label,
                        cx.listener(move |this, _, _, cx| this.remove_chip(id, cx)),
                    )
                    .danger(),
                }
            })
            .collect();
        self.ctx.update(cx, |menu, cx| menu.open(at, items, cx));
    }

    /// `copy image`: the picture goes to the clipboard; a helper that fails is said under the input.
    fn copy_chip(&mut self, sid: Sid, name: &str, image: &Arc<Image>, cx: &mut Context<Self>) {
        let copied = clip::copy_image(image, cx);
        let name = name.to_owned();
        cx.spawn(async move |this, cx| {
            if let Err(why) = copied.await {
                this.update(cx, |this, cx| {
                    this.notice(sid, format!("could not copy {name}: {why}"), cx)
                })
                .ok();
            }
        })
        .detach();
    }
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

/// The 48px square: the picture cropped to fill it, or the file's extension. The tile does not clip
/// (`overflow_hidden`): the image crops itself to its box (`Cover`), and a tile that also clips
/// makes gpui paint the top pixel row of every thumbnail the scroll box has pushed out of sight.
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

#[cfg(test)]
mod tests {
    use gpui::ImageFormat;

    use super::*;

    fn labels(image: Option<&Arc<Image>>) -> Vec<&'static str> {
        entries(image).iter().map(Entry::label).collect()
    }

    fn picture(format: ImageFormat) -> Arc<Image> {
        Arc::new(Image::from_bytes(format, Vec::new()))
    }

    #[test]
    fn a_picture_is_copied_opened_or_removed() {
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Gif] {
            let image = picture(format);
            assert_eq!(labels(Some(&image)), ["copy image", "open", "remove"]);
        }
    }

    #[test]
    fn a_file_or_a_vector_picture_is_only_opened_or_removed() {
        assert_eq!(labels(None), ["open", "remove"]);
        assert_eq!(labels(Some(&picture(ImageFormat::Svg))), ["open", "remove"]);
    }
}
