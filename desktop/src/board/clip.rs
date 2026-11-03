//! The picture on the system clipboard.
//!
//! gpui puts a picture on the clipboard on macOS and Windows only: its X11 backend writes an image
//! as empty text, and its Wayland one offers no image type to other programs. Elsewhere a helper
//! does it: `wl-copy` (wl-clipboard) under Wayland, `xclip` under X11, fed the PNG on stdin.

use std::io::{self, Write};
use std::process::{Command, Stdio};

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

#[cfg(all(test, unix))]
mod tests;
