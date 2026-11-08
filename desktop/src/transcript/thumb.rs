//! The files attached to a user message, and the files the agent shows (`femmed show`), in the
//! transcript: a chip with the file's name for any file, and for an image a thumbnail (a bigger
//! box for the agent's) that opens a larger preview while the pointer is on it. The
//! bytes come from the box (`Req::FsRead`); the thumbnail and the preview both draw the picture
//! with gpui's `img`, fitted whole into their box: it keeps its ratio, sits in the middle, and the
//! panel colour shows around it. A right click on a thumbnail opens its menu (`copy image`, `open`),
//! drawn by [`crate::ui::ctx`]: the picture to copy is the one the thumbnail already holds.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, img, px, relative, AnyElement, App, AppContext as _, Context, ElementId, Image, Img,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, ObjectFit,
    ParentElement as _, Pixels, Point, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, StyledImage as _, Window,
};

use gpui_component::WindowExt as _;

use super::attach::{self, Attached};
use super::live::Transcript;
use crate::theme::{self, bad, muted, panel, paper, FONT_MONO};
use crate::ui::ctx::Item;
use crate::ui::{clip, meta, ruled, tb};

/// Side of a thumbnail's box.
pub const THUMB: f32 = 88.;

/// Size of a picture the agent shows (`femmed show`): big enough to read a screenshot.
const SHOWN: (f32, f32) = (360., 240.);

/// Size of the preview.
const PREVIEW: (f32, f32) = (460., 380.);

/// How long the pointer rests on a thumbnail before the preview opens.
const PREVIEW_DELAY: Duration = Duration::from_millis(180);

/// What is known of an attached file.
#[derive(Clone)]
pub enum Thumb {
    /// The bytes are being fetched.
    Loading,
    /// A picture gpui can draw.
    Ready {
        /// The decoded-on-demand picture; the preview draws this same one, so gpui decodes it once.
        image: Arc<Image>,
        /// The file's name.
        name: String,
    },
    /// It could not be fetched, or it is not a picture.
    Failed,
}

impl Thumb {
    /// The state after `bytes` of the file called `name` arrived.
    pub fn from_bytes(name: &str, bytes: Vec<u8>) -> Self {
        match attach::image_format(name, &bytes) {
            Some(format) => Thumb::Ready {
                image: Arc::new(Image::from_bytes(format, bytes)),
                name: name.to_owned(),
            },
            None => Thumb::Failed,
        }
    }
}

/// The picture fitted whole into the box it is put in: it keeps its ratio and is centred, and the
/// box's own background shows around it. It is placed absolutely and takes the box's size, so the
/// box must have one: an image in the flow is never laid out shorter than its width over its
/// ratio, which leaves a tall one no room to be letterboxed.
fn picture(image: &Arc<Image>) -> Img {
    img(image.clone())
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .object_fit(ObjectFit::Contain)
        .with_loading(|| div().size_full().bg(panel()).into_any_element())
        .with_fallback(|| note("cannot draw", bad()).into_any_element())
}

/// The preview that opens over a thumbnail: the picture, fitted whole into a titled box.
struct Preview {
    name: SharedString,
    image: Arc<Image>,
}

impl Render for Preview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let (w, h) = PREVIEW;
        let mut shell = ruled(
            div()
                .w(px(w))
                .h(px(h))
                .flex()
                .flex_col()
                .bg(paper())
                .overflow_hidden(),
        );
        shell.style().box_shadow = Some(theme::hard_shadow());
        shell.child(tb(self.name.clone())).child(
            div()
                .relative()
                .flex_1()
                .min_h(px(0.))
                .w_full()
                .bg(panel())
                .overflow_hidden()
                .child(picture(&self.image)),
        )
    }
}

/// An attached file as a chip (a file) or a thumbnail (a picture), with `thumb` what is known of
/// it. `id` tells it apart from the other attachments on screen. `on_menu` hears a right click on
/// a thumbnail that shows its picture.
pub fn attachment(
    id: impl Into<ElementId>,
    file: &Attached,
    thumb: Option<&Thumb>,
    on_menu: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    framed(id, file, thumb, on_menu, THUMB, THUMB)
}

/// A file the agent shows the user: a picture as a readable-sized box, any other file as a chip.
pub fn shown(
    id: impl Into<ElementId>,
    file: &Attached,
    thumb: Option<&Thumb>,
    on_menu: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    framed(id, file, thumb, on_menu, SHOWN.0, SHOWN.1)
}

fn framed(
    id: impl Into<ElementId>,
    file: &Attached,
    thumb: Option<&Thumb>,
    on_menu: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    w: f32,
    h: f32,
) -> AnyElement {
    if !file.image {
        return chip(&file.name).into_any_element();
    }
    let frame = ruled(
        div()
            .id(id.into())
            .relative()
            .flex_none()
            .w(px(w))
            .h(px(h))
            .bg(panel())
            .overflow_hidden(),
    );
    match thumb {
        Some(Thumb::Ready { image, name }) => {
            let (image, name) = (image.clone(), SharedString::from(name.clone()));
            let thumb = picture(&image);
            frame
                .cursor_pointer()
                .on_mouse_down(MouseButton::Right, on_menu)
                .tooltip_show_delay(PREVIEW_DELAY)
                .tooltip(move |_, cx: &mut App| {
                    let (name, image) = (name.clone(), image.clone());
                    cx.new(|_| Preview { name, image }).into()
                })
                .child(thumb)
                .into_any_element()
        }
        Some(Thumb::Failed) => frame.child(note("unavailable", bad())).into_any_element(),
        Some(Thumb::Loading) | None => frame.child(note("loading", muted())).into_any_element(),
    }
}

/// One row of a thumbnail's menu.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Entry {
    /// The picture, to the clipboard.
    Copy,
    /// The preview, kept on screen.
    Open,
}

impl Entry {
    fn label(self) -> &'static str {
        match self {
            Entry::Copy => "copy image",
            Entry::Open => "open",
        }
    }
}

/// The rows of a thumbnail's menu: `copy image` when the picture can be copied, then `open`.
fn entries(image: &Image) -> &'static [Entry] {
    if clip::copyable(image) {
        &[Entry::Copy, Entry::Open]
    } else {
        &[Entry::Open]
    }
}

impl Transcript {
    /// A right click at `at` on the thumbnail of the file at `path`: its menu opens. A file whose
    /// picture is not there (yet) has none. The click leaves the keyboard where it is.
    pub(super) fn thumb_menu(
        &mut self,
        path: &str,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Thumb::Ready { image, name }) = self.thumbs.get(path) else {
            return;
        };
        if window.has_focused_input(cx) {
            window.prevent_default();
        }
        let preview = cx.new(|_| Preview {
            name: name.clone().into(),
            image: image.clone(),
        });
        let items = entries(image)
            .iter()
            .map(|entry| match entry {
                Entry::Copy => {
                    let (name, image) = (name.clone(), image.clone());
                    Item::new(
                        entry.label(),
                        cx.listener(move |this, _, _, cx| this.copy_thumb(&name, &image, cx)),
                    )
                }
                Entry::Open => {
                    let preview = preview.clone();
                    Item::new(
                        entry.label(),
                        cx.listener(move |this, _, _, cx| {
                            let preview = preview.clone();
                            this.ctx.update(cx, |menu, cx| {
                                menu.pin(at, move |_, _| preview.clone().into_any_element(), cx)
                            });
                        }),
                    )
                }
            })
            .collect();
        self.ctx.update(cx, |menu, cx| menu.open(at, items, cx));
    }

    /// `copy image`: the picture goes to the clipboard; a helper that fails is said in the note.
    fn copy_thumb(&mut self, name: &str, image: &Arc<Image>, cx: &mut Context<Self>) {
        let copied = clip::copy_image(image, cx);
        let name = name.to_owned();
        cx.spawn(async move |this, cx| {
            if let Err(why) = copied.await {
                this.update(cx, |this, cx| {
                    this.note = Some(format!("could not copy {name}: {why}"));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }
}

/// A file that is not shown as a picture: its name in a small box.
fn chip(name: &str) -> impl IntoElement {
    ruled(
        div()
            .flex_none()
            .max_w(relative(1.))
            .px(px(8.))
            .py(px(3.))
            .bg(paper())
            .font_family(FONT_MONO)
            .text_size(theme::META_SIZE)
            .whitespace_nowrap()
            .overflow_hidden()
            .text_ellipsis(),
    )
    .child(SharedString::from(name.to_owned()))
}

/// A word in the middle of a thumbnail's box.
fn note(text: &'static str, color: gpui::Rgba) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p(px(4.))
        .child(meta(text).text_color(color))
}

#[cfg(test)]
mod tests {
    use gpui::ImageFormat;

    use super::*;

    fn picture(format: ImageFormat) -> Image {
        Image::from_bytes(format, Vec::new())
    }

    #[test]
    fn a_picture_is_copied_or_opened() {
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Webp] {
            assert_eq!(entries(&picture(format)), [Entry::Copy, Entry::Open]);
        }
    }

    #[test]
    fn a_vector_picture_is_only_opened() {
        assert_eq!(entries(&picture(ImageFormat::Svg)), [Entry::Open]);
    }

    #[test]
    fn the_rows_are_worded_for_the_user() {
        assert_eq!(Entry::Copy.label(), "copy image");
        assert_eq!(Entry::Open.label(), "open");
    }
}
