//! Renders one file from the box by name and bytes: images (raster and SVG), PDF pages, text and
//! source code with syntax highlighting, markdown as highlighted text, and a hex dump for anything
//! else. Owned by the viewer agent (docs/desktop-ui.md §viewer). Used by the files panel and by the
//! composer's attachment previews, so it fits whatever bounds it is given and scrolls inside them.
//!
//! What a file is comes from its magic bytes first and its extension second ([`detect`]). Decoding
//! and rendering never run on the UI thread: each kind has a view of its own that shows a spinner
//! until the background work is done.

mod bitmap;
mod chrome;
mod hex;
mod highlight;
mod kind;
mod pdf;
mod text;

use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;

use gpui::{
    div, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
    Window,
};

pub use kind::{detect, Kind, Raster};

/// How much of a binary file the hex dump shows.
const HEX_BYTES: usize = 256;

/// Runs `f`, turning a panic inside a decoder into an error message, so that one hostile file does
/// not take the window down. (The release profile has `panic = "abort"`, where a panic still ends
/// the process: the guard is for dev builds and tests.)
fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(format!("{what} crashed on this file")))
}

/// What the view shows.
enum Body {
    Image(Entity<bitmap::ImageView>),
    Pdf(Entity<pdf::PdfView>),
    Text(Entity<text::TextView>),
    /// A file that cannot be shown: a headline and the lines under it (the hex dump).
    Notice {
        headline: String,
        lines: Vec<String>,
    },
}

/// One file, rendered by what it is: a PNG, JPEG, GIF, WebP, BMP, TIFF, ICO or SVG as a picture
/// fitted into the view (a click toggles 1:1), a PDF as lazily rendered pages, UTF-8 text with line
/// numbers and syntax highlighting, anything else as a notice with a hex dump of its first bytes.
///
/// It fills the space it is placed in and scrolls inside it, so it works as a full panel and as a
/// small hover preview alike.
pub struct FileView {
    name: String,
    len: usize,
    kind: Kind,
    body: Body,
}

impl FileView {
    /// Views the file `name` whose content is `data`. The kind is worked out here, at once; decoding
    /// starts in the background and the view shows a spinner until it is done.
    pub fn new(name: String, data: Vec<u8>, cx: &mut Context<Self>) -> Self {
        let kind = detect(&name, &data);
        let len = data.len();
        let body = match &kind {
            Kind::Raster(_) | Kind::Svg => {
                let kind = kind.clone();
                Body::Image(cx.new(|cx| bitmap::ImageView::new(kind, Arc::new(data), cx)))
            }
            Kind::Pdf => Body::Pdf(cx.new(|cx| pdf::PdfView::new(Arc::new(data), cx))),
            Kind::Text => {
                let name = name.clone();
                Body::Text(cx.new(|cx| text::TextView::new(name, Arc::new(data), cx)))
            }
            Kind::Binary(what) => {
                let mut lines = hex::hex_dump(&data, HEX_BYTES);
                if len > HEX_BYTES {
                    lines.push(format!("... {} more bytes", len - HEX_BYTES));
                }
                Body::Notice {
                    headline: format!("binary file · {len} bytes · {what}"),
                    lines,
                }
            }
        };
        FileView {
            name,
            len,
            kind,
            body,
        }
    }

    /// Reads `path` and views it under its file name. For tools and proofs that have a file rather
    /// than bytes.
    pub fn from_path(path: impl AsRef<Path>, cx: &mut Context<Self>) -> io::Result<Self> {
        let path = path.as_ref();
        let data = std::fs::read(path)?;
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        Ok(Self::new(name, data, cx))
    }

    /// The file's name, as given.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The file's size in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the file has no bytes.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// What the file was detected to be.
    pub fn kind(&self) -> &Kind {
        &self.kind
    }
}

impl Render for FileView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.body {
            Body::Image(view) => view.clone().into_any_element(),
            Body::Pdf(view) => view.clone().into_any_element(),
            Body::Text(view) => view.clone().into_any_element(),
            Body::Notice { headline, lines } => chrome::notice(
                ("viewer-notice", cx.entity_id()),
                headline.clone(),
                lines.clone(),
            )
            .into_any_element(),
        };
        div().size_full().overflow_hidden().child(body)
    }
}
