//! A picture (or a video the agent showed, which is an animated picture) opened over the whole
//! window, the way the drawing board opens: a dim backdrop and a card with `‹ name` over the
//! picture, fitted whole. A left click on a transcript thumbnail opens it; Esc, `‹` or a click on
//! the backdrop closes it, and the keyboard goes back to whoever had it.
//!
//! The shell makes the one entity ([`Lightbox::install`]) and renders it last, like the board; any
//! view opens it with [`Lightbox::show`], without a handle passed down.

use std::sync::Arc;

use gpui::{
    div, img, px, relative, App, AppContext as _, Context, Entity, FocusHandle, Global, Image,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ObjectFit, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window,
};

use crate::theme::{self, bar, bar_text, ink, panel, paper, FONT_PIXEL, FONT_UI, TB_SIZE};
use crate::ui::{ruled, title_button, Glyph};

/// The card's size as a share of the window, as the board's.
const CARD_W: f32 = 0.94;
const CARD_H: f32 = 0.92;

/// What is open: its name and its picture.
struct Open {
    name: SharedString,
    image: Arc<Image>,
}

/// The layer over the window: empty while nothing is open.
pub struct Lightbox {
    open: Option<Open>,
    focus: FocusHandle,
    /// Who had the keyboard before it opened.
    restore: Option<FocusHandle>,
    /// Just opened: the next frame takes the keyboard.
    want_focus: bool,
}

struct Installed(Entity<Lightbox>);

impl Global for Installed {}

impl Lightbox {
    /// Makes the one lightbox and lets [`Lightbox::show`] find it. The shell renders what this
    /// returns as the last child of its root.
    pub fn install(cx: &mut App) -> Entity<Self> {
        let this = cx.new(|cx| Self {
            open: None,
            focus: cx.focus_handle(),
            restore: None,
            want_focus: false,
        });
        cx.set_global(Installed(this.clone()));
        this
    }

    /// Opens `image`, called `name`, over the window and gives it the keyboard.
    pub fn show(name: &str, image: Arc<Image>, cx: &mut App) {
        let Some(this) = cx.try_global::<Installed>().map(|g| g.0.clone()) else {
            return;
        };
        this.update(cx, |t, cx| {
            t.want_focus |= t.open.is_none();
            t.open = Some(Open {
                name: name.to_owned().into(),
                image,
            });
            cx.notify();
        });
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open.take().is_none() {
            return;
        }
        if let Some(back) = self.restore.take() {
            window.focus(&back, cx);
        }
        cx.notify();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key == "escape" {
            self.close(window, cx);
        }
        // its keys stay here: the composer behind it must not type
        cx.stop_propagation();
    }
}

impl Render for Lightbox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.open.is_some() && std::mem::take(&mut self.want_focus) {
            self.restore = window.focused(cx);
            window.focus(&self.focus, cx);
        }
        let Some(open) = &self.open else {
            return div().into_any_element();
        };
        let title = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .py(px(3.))
            .bg(bar())
            .text_color(bar_text())
            .font_family(FONT_PIXEL)
            .text_size(TB_SIZE)
            .line_height(relative(1.))
            .child(
                title_button("lightbox-close", Glyph::Back)
                    .on_click(cx.listener(|t, _, window, cx| t.close(window, cx))),
            )
            .child(div().flex_1().truncate().child(open.name.clone()));
        let picture = div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .bg(panel())
            .child(
                img(open.image.clone())
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            );
        let card = ruled(div().flex().flex_col().bg(paper()))
            .shadow(theme::hard_shadow())
            .w(relative(CARD_W))
            .h(relative(CARD_H))
            .overflow_hidden()
            // a click on the card is not a click on the backdrop
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(title)
            .child(picture);
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::scrim())
            .occlude()
            .track_focus(&self.focus)
            .font_family(FONT_UI)
            .text_color(ink())
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|t, _, window, cx| t.close(window, cx)),
            )
            .child(card)
            .into_any_element()
    }
}

/// Whether a file is a video the lightbox plays (as an animated WebP), by its name.
pub fn is_video(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| {
        matches!(
            ext.to_ascii_lowercase().as_str(),
            "mp4" | "mov" | "webm" | "mkv" | "avi" | "m4v"
        )
    })
}

/// The most frames a video keeps: every one is held decoded (640x360 RGBA is ~0.9 MB).
const FRAMES: f64 = 200.;

/// A video's `bytes` as a silent, looping animated WebP, by `ffmpeg` on this machine: 640 px wide,
/// the first minute, at most [`FRAMES`] frames (a longer clip at a lower rate). Blocking: run it
/// off the main thread. The same recipe as `femmed show`.
pub fn video_webp(name: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
    use std::process::{Command, Stdio};
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("femme-video-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let made = (|| {
        let ext = name.rsplit_once('.').map_or("mp4", |(_, e)| e);
        let (input, out) = (dir.join(format!("in.{ext}")), dir.join("out.webp"));
        std::fs::write(&input, bytes).map_err(|e| e.to_string())?;
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ])
            .arg(&input)
            .output()
            .map_err(|e| format!("playing a video needs ffmpeg: {e}"))?;
        let seconds: f64 = String::from_utf8_lossy(&probe.stdout)
            .trim()
            .parse()
            .unwrap_or(60.);
        let fps = (FRAMES / seconds.clamp(1., 60.)).clamp(2., 15.);
        let ran = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-t", "60", "-i"])
            .arg(&input)
            .args([
                "-vf",
                &format!("fps={fps:.2},scale='min(640,iw)':-2:flags=lanczos"),
                "-an",
                "-c:v",
                "libwebp_anim",
                "-loop",
                "0",
                "-q:v",
                "70",
            ])
            .arg(&out)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("playing a video needs ffmpeg: {e}"))?;
        if !ran.status.success() {
            return Err(String::from_utf8_lossy(&ran.stderr).trim().to_owned());
        }
        std::fs::read(&out).map_err(|e| e.to_string())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    made
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_video_becomes_an_animated_webp() {
        assert!(is_video("clip.MP4") && is_video("a.b.webm") && !is_video("shot.png"));
        let dir = std::env::temp_dir().join(format!("femme-video-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip.mp4");
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .args([
                "testsrc2=size=320x180:rate=10",
                "-t",
                "2",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&clip)
            .status();
        // ponytail: no ffmpeg here, nothing to check
        if !made.is_ok_and(|s| s.success()) {
            return;
        }
        let webp = video_webp("clip.mp4", &std::fs::read(&clip).unwrap()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(&webp[..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");
        assert!(video_webp("x.mp4", b"not a video").is_err());
    }
}
