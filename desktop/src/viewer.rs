//! Renders one file from the box by name and bytes: images, PDF pages, text and code, markdown.
//! Owned by the viewer agent (docs/desktop-ui.md §viewer). Used by the files panel and by the
//! composer's attachment previews.
#![allow(missing_docs)] // stub: the owning agent documents what it fills in

mod bitmap;
mod chrome;
mod hex;
mod highlight;
mod kind;
mod pdf;

pub use kind::{detect, Kind, Raster};

use std::panic::{catch_unwind, AssertUnwindSafe};

/// Runs `f`, turning a panic inside a decoder into an error message, so that one hostile file does
/// not take the window down. (The release profile has `panic = "abort"`, where a panic still ends
/// the process: the guard is for dev builds and tests.)
fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(format!("{what} crashed on this file")))
}

use gpui::{div, Context, IntoElement, ParentElement, Render, Window};

pub struct FileView {
    name: String,
    data: Vec<u8>,
}

impl FileView {
    pub fn new(name: String, data: Vec<u8>, _cx: &mut Context<Self>) -> Self {
        FileView { name, data }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

impl Render for FileView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().child(format!("{} ({} bytes, stub)", self.name, self.data.len()))
    }
}
