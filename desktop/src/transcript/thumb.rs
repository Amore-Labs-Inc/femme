//! The files attached to a user message, in the transcript: a chip with the file's name for any
//! file, and for an image a thumbnail that opens a larger preview while the pointer is on it. The
//! bytes come from the box (`Req::FsRead`); the thumbnail and the preview both draw the picture
//! with gpui's `img`, fitted whole into their box: it keeps its ratio, sits in the middle, and the
//! panel colour shows around it.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, img, px, relative, AnyElement, App, AppContext as _, Context, ElementId, Image, Img,
    InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window,
};

use super::attach::{self, Attached};
use crate::theme::{self, BAD, FONT_MONO, MUTED, PANEL, PAPER};
use crate::ui::{meta, ruled, tb};

/// Side of a thumbnail's box.
pub const THUMB: f32 = 88.;

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
        .with_loading(|| div().size_full().bg(PANEL).into_any_element())
        .with_fallback(|| note("cannot draw", BAD).into_any_element())
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
                .bg(PAPER)
                .overflow_hidden(),
        );
        shell.style().box_shadow = Some(theme::hard_shadow());
        shell.child(tb(self.name.clone())).child(
            div()
                .relative()
                .flex_1()
                .min_h(px(0.))
                .w_full()
                .bg(PANEL)
                .overflow_hidden()
                .child(picture(&self.image)),
        )
    }
}

/// An attached file as a chip (a file) or a thumbnail (a picture), with `thumb` what is known of
/// it. `id` tells it apart from the other attachments on screen.
pub fn attachment(id: impl Into<ElementId>, file: &Attached, thumb: Option<&Thumb>) -> AnyElement {
    if !file.image {
        return chip(&file.name).into_any_element();
    }
    let side = px(THUMB);
    let frame = ruled(
        div()
            .id(id.into())
            .relative()
            .flex_none()
            .size(side)
            .bg(PANEL)
            .overflow_hidden(),
    );
    match thumb {
        Some(Thumb::Ready { image, name }) => {
            let (image, name) = (image.clone(), SharedString::from(name.clone()));
            let thumb = picture(&image);
            frame
                .cursor_pointer()
                .tooltip_show_delay(PREVIEW_DELAY)
                .tooltip(move |_, cx: &mut App| {
                    let (name, image) = (name.clone(), image.clone());
                    cx.new(|_| Preview { name, image }).into()
                })
                .child(thumb)
                .into_any_element()
        }
        Some(Thumb::Failed) => frame.child(note("unavailable", BAD)).into_any_element(),
        Some(Thumb::Loading) | None => frame.child(note("loading", MUTED)).into_any_element(),
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
            .bg(PAPER)
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
