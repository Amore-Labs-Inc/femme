//! Keys, mouse and wheel: gpui's events turned into the editor's, and what the editor leaves to
//! the board (send, copy the picture, paste text, close).

use gpui::{
    px, ClipboardItem, Context, Image, ImageFormat, KeyDownEvent, KeyUpEvent, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PinchEvent, Pixels, ScrollEvent,
    Window,
};

use super::clip;
use super::editor::{Button, Command, Keyed, Mods};
use super::export::DEFAULT_SCALE;
use super::geom::Point;
use super::{Board, BoardEvent};

/// What one "line" of a mouse wheel is worth in pixels (a notch is three lines; a trackpad reports
/// pixels itself): a notch pans 90 px and, with ctrl held, zooms by about a fifth.
const WHEEL_LINE: f32 = 30.0;

/// The modifiers held, as the editor knows them (the command key is ctrl, or cmd on a Mac).
fn mods(m: &Modifiers, cx: &gpui::App) -> Mods {
    Mods {
        ctrl: m.secondary(cx),
        shift: m.shift,
        alt: m.alt,
    }
}

impl Board {
    /// A window position as a canvas position.
    fn local(&self, at: gpui::Point<Pixels>) -> Point {
        let origin = self.canvas.get().origin;
        Point::new(f32::from(at.x - origin.x), f32::from(at.y - origin.y))
    }

    /// A key went down while the board has the keyboard.
    pub(super) fn key_down(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ks = &ev.keystroke;
        if self.confirm_clear && ks.key == "escape" {
            self.confirm_clear = false;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        match self
            .editor
            .key(&ks.key, ks.key_char.as_deref(), mods(&ks.modifiers, cx))
        {
            Keyed::Ignored => {}
            Keyed::Done => {
                cx.stop_propagation();
                self.changed(cx);
            }
            Keyed::Do(command) => {
                cx.stop_propagation();
                self.run(command, window, cx);
            }
        }
    }

    /// A key went up (space ends a pan, an arrow ends a nudge).
    pub(super) fn key_up(&mut self, ev: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.editor.key_up(&ev.keystroke.key);
        self.changed(cx);
    }

    /// What a key asks for that the editor cannot do by itself.
    fn run(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::Send => self.send(cx),
            Command::CopyImage => self.copy_image(cx),
            Command::Close => self.dismiss(window, cx),
            Command::PasteText => {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    self.editor.insert_text(&text);
                }
                self.changed(cx);
            }
        }
    }

    /// A button went down on the canvas.
    pub(super) fn mouse_down(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let button = match ev.button {
            MouseButton::Left => Button::Primary,
            MouseButton::Middle => Button::Middle,
            _ => return,
        };
        cx.stop_propagation();
        window.focus(&self.focus, cx);
        self.confirm_clear = false;
        let at = self.local(ev.position);
        let clicks = u32::try_from(ev.click_count).unwrap_or(1);
        self.editor
            .pointer_down(at, mods(&ev.modifiers, cx), clicks, button);
        self.changed(cx);
    }

    /// The pointer moved anywhere in the window: it matters only while a drag is on, and then the
    /// drag follows it out of the canvas too.
    pub(super) fn mouse_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.open || !self.editor.is_dragging() {
            return;
        }
        let at = self.local(ev.position);
        let held = mods(&ev.modifiers, cx);
        if ev.pressed_button.is_none() {
            // The button went up where this window did not hear it: the drag is over.
            self.editor.pointer_up(at, held);
        } else {
            self.editor.pointer_move(at, held);
        }
        self.changed(cx);
    }

    /// A button went up anywhere in the window.
    pub(super) fn mouse_up(&mut self, ev: &MouseUpEvent, cx: &mut Context<Self>) {
        if !self.open || !self.editor.is_dragging() {
            return;
        }
        let at = self.local(ev.position);
        self.editor.pointer_up(at, mods(&ev.modifiers, cx));
        self.changed(cx);
    }

    /// The wheel (or two fingers on a trackpad): pans, zooms with ctrl held.
    pub(super) fn scroll(&mut self, ev: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let d = ev.delta.pixel_delta(px(WHEEL_LINE));
        let at = self.local(ev.position);
        self.editor.wheel(
            at,
            Point::new(f32::from(d.x), f32::from(d.y)),
            mods(&ev.modifiers, cx),
        );
        cx.stop_propagation();
        cx.notify();
    }

    /// A pinch on a trackpad: zooms around the fingers.
    pub(super) fn pinch(&mut self, ev: &PinchEvent, _: &mut Window, cx: &mut Context<Self>) {
        let at = self.local(ev.position);
        self.editor.zoom_at(at, (1.0 + ev.delta).max(0.05));
        cx.stop_propagation();
        cx.notify();
    }

    /// The picture of the drawing as PNG bytes, if there is a drawing to picture: finishes a text
    /// being typed first, and says so when the board is empty.
    fn picture(&mut self, cx: &mut Context<Self>) -> Option<Vec<u8>> {
        self.editor.blur();
        self.changed(cx);
        if !self.editor.has_content() {
            self.say("nothing drawn yet", false, cx);
            return None;
        }
        Some(self.editor.scene().render_png(DEFAULT_SCALE))
    }

    /// Sends the drawing to Claude: the shell attaches the PNG to the prompt, and the board
    /// closes.
    pub(super) fn send(&mut self, cx: &mut Context<Self>) {
        let Some(png) = self.picture(cx) else {
            return;
        };
        let name = self.editor.next_export_name();
        self.changed(cx);
        cx.emit(BoardEvent::Send { name, png });
        self.close(cx);
    }

    /// Puts the picture of the drawing on the clipboard: gpui does it on macOS and Windows, a helper
    /// program (wl-copy or xclip) on Linux, where gpui cannot.
    pub(super) fn copy_image(&mut self, cx: &mut Context<Self>) {
        let Some(png) = self.picture(cx) else {
            return;
        };
        if !clip::NEEDS_HELPER {
            cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
                ImageFormat::Png,
                png,
            )));
            self.say("copied the picture", false, cx);
            return;
        }
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move { clip::copy_png(&clip::session_helpers(), &png) })
                .await;
            this.update(cx, |b, cx| match done {
                Ok(()) => b.say("copied the picture", false, cx),
                Err(e) => b.say(format!("could not copy the picture: {e}"), true, cx),
            })
            .ok();
        })
        .detach();
    }

    /// `clear` was clicked: asks once.
    pub(super) fn ask_clear(&mut self, cx: &mut Context<Self>) {
        self.confirm_clear = self.editor.has_content();
        cx.notify();
    }

    /// The answer to "clear the board?".
    pub(super) fn answer_clear(&mut self, yes: bool, cx: &mut Context<Self>) {
        self.confirm_clear = false;
        if yes {
            self.editor.clear();
        }
        self.changed(cx);
    }
}
