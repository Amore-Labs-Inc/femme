//! The files attached to a user message, in the transcript: a chip with the file's name for any
//! file, and for an image a thumbnail that opens a larger preview while the pointer is on it. The
//! bytes come from the box (`Req::FsRead`), the picture is drawn by gpui's `img`, the preview is
//! the viewer's `FileView`.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, img, px, relative, AnyElement, App, AppContext as _, Context, ElementId, Entity, Image,
    InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window,
};

use super::attach::{self, Attached};
use crate::theme::{self, BAD, FONT_MONO, MUTED, PANEL, PAPER};
use crate::ui::{meta, ruled, tb};
use crate::viewer::FileView;

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
        /// The decoded-on-demand picture; its bytes are also what the preview shows.
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

/// The preview that opens over a thumbnail: the file in the viewer, in a titled box.
struct Preview {
    name: SharedString,
    view: Entity<FileView>,
}

impl Preview {
    fn new(name: String, data: Vec<u8>, cx: &mut Context<Self>) -> Self {
        let shown = name.clone();
        Self {
            view: cx.new(|cx| FileView::new(shown, data, cx)),
            name: name.into(),
        }
    }
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
                .flex_1()
                .min_h(px(0.))
                .w_full()
                .overflow_hidden()
                .child(self.view.clone()),
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
            .flex_none()
            .size(side)
            .bg(PANEL)
            .overflow_hidden(),
    );
    match thumb {
        Some(Thumb::Ready { image, name }) => {
            let (image, name) = (image.clone(), name.clone());
            let picture = img(image.clone())
                .size_full()
                .object_fit(ObjectFit::Cover)
                .with_loading(|| div().size_full().bg(PANEL).into_any_element())
                .with_fallback(|| note("cannot draw", BAD).into_any_element());
            frame
                .cursor_pointer()
                .tooltip_show_delay(PREVIEW_DELAY)
                .tooltip(move |_, cx: &mut App| {
                    let (name, data) = (name.clone(), image.bytes.clone());
                    cx.new(|cx| Preview::new(name, data, cx)).into()
                })
                .child(picture)
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
