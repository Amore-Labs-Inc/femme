//! The picture on the system clipboard.
//!
//! gpui puts a picture on the clipboard on macOS and Windows only: its X11 backend writes an image
//! as empty text, and its Wayland one offers no image type to other programs. Elsewhere a helper
//! does it: `wl-copy` (wl-clipboard) under Wayland, `xclip` under X11, fed the PNG on stdin.
//!
//! [`copy_image`] is the call for a picture the app shows (a chip of the composer, a thumbnail of
//! the transcript). Every program that takes a pasted picture reads a PNG, so a PNG goes as it is
//! and any other raster is turned into one first ([`png_of`]). The board has its own PNG and calls
//! [`copy_png`].

use std::borrow::Cow;
use std::io::{self, Cursor, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

use gpui::{App, AppContext as _, ClipboardItem, Image, ImageFormat, Task};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{DynamicImage, ImageDecoder as _, ImageReader};

/// Whether this platform needs a helper for a picture on the clipboard: all but macOS and Windows.
pub const NEEDS_HELPER: bool = cfg!(not(any(target_os = "macos", target_os = "windows")));

/// A program that puts the PNG it reads on stdin on the clipboard.
#[derive(Clone, Copy, Debug)]
pub struct Helper {
    /// The program, looked up on `PATH`.
    pub program: &'static str,
    /// Its arguments.
    pub args: &'static [&'static str],
}

/// wl-clipboard, the Wayland one.
const WL_COPY: Helper = Helper {
    program: "wl-copy",
    args: &["--type", "image/png"],
};

/// xclip, for X11 (and XWayland).
const XCLIP: Helper = Helper {
    program: "xclip",
    args: &["-selection", "clipboard", "-t", "image/png"],
};

/// The helpers worth trying, best first: `wl-copy` when there is a Wayland display, then `xclip`
/// when there is an X11 one.
pub fn helpers(wayland: bool, x11: bool) -> Vec<Helper> {
    let mut list = Vec::new();
    if wayland {
        list.push(WL_COPY);
    }
    if x11 {
        list.push(XCLIP);
    }
    list
}

/// The helpers for this session, by the display variables in the environment.
pub fn session_helpers() -> Vec<Helper> {
    let set = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    helpers(set("WAYLAND_DISPLAY"), set("DISPLAY"))
}

/// Runs `program` with `args`, writes `bytes` to its stdin and waits for it to exit. The clipboard
/// helpers hand the data to a copy of themselves that stays behind to serve it, and return.
pub fn pipe_to(program: &str, args: &[&str], bytes: &[u8]) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // A program that quits early closes the pipe: its exit status says what went wrong.
        let _ = stdin.write_all(bytes);
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program} failed ({status})")))
    }
}

/// Puts `png` on the clipboard with the first of `helpers` that works. The error says which helper
/// failed, or what to install when there was none to try.
pub fn copy_png(helpers: &[Helper], png: &[u8]) -> Result<(), String> {
    let mut failed = None;
    for helper in helpers {
        match pipe_to(helper.program, helper.args, png) {
            Ok(()) => return Ok(()),
            // Not installed: try the next one.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => failed = Some(e.to_string()),
        }
    }
    Err(failed.unwrap_or_else(|| "install wl-clipboard (wl-copy) or xclip".to_owned()))
}

/// Whether `image` can go on the clipboard: every raster can. An SVG cannot: the pictures that are
/// pasted are bitmaps, and nothing here draws a vector one into a bitmap.
pub fn copyable(image: &Image) -> bool {
    image.format != ImageFormat::Svg
}

/// The PNG that goes on the clipboard for the picture in `bytes`: a PNG as it is, any other raster
/// decoded and encoded again, with its EXIF orientation applied as the viewer shows it (an
/// animation gives its first frame). The error says why the bytes are no picture.
pub fn png_of(bytes: &[u8]) -> Result<Cow<'_, [u8]>, String> {
    let fail = |e: image::ImageError| format!("could not read the picture: {e}");
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    if reader.format() == Some(image::ImageFormat::Png) {
        return Ok(Cow::Borrowed(bytes));
    }
    let mut decoder = reader.into_decoder().map_err(fail)?;
    let orientation = decoder.orientation().map_err(fail)?;
    let mut picture = DynamicImage::from_decoder(decoder).map_err(fail)?;
    picture.apply_orientation(orientation);
    let mut png = Vec::new();
    // The default compression is slow in a debug build, and the clipboard is no archive.
    let encoder =
        PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive);
    picture.write_with_encoder(encoder).map_err(fail)?;
    Ok(Cow::Owned(png))
}

/// Puts `image` on the clipboard. Where a helper has to do it, that runs off the UI thread: the
/// task ends with why the picture could not be copied, in words for the user.
pub fn copy_image(image: &Arc<Image>, cx: &App) -> Task<Result<(), String>> {
    if !NEEDS_HELPER {
        cx.write_to_clipboard(ClipboardItem::new_image(image));
        return Task::ready(Ok(()));
    }
    let image = image.clone();
    cx.background_spawn(async move { copy_png(&session_helpers(), &png_of(&image.bytes)?) })
}

#[cfg(all(test, unix))]
mod tests;
