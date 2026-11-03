//! The keyboard side of the editor: shortcuts on the board, and typing into a text.

use super::{Command, Editor, Keyed, Mods, Tool, NUDGE, ZOOM_STEP};
use crate::board::geom::Point;
use crate::board::textedit::TextEdit;

impl Editor {
    /// A key went down. `key` is gpui's key name (`"a"`, `"enter"`, `"left"`), `ch` the text the key
    /// types (`"A"` with shift), `mods` what is held.
    pub fn key(&mut self, key: &str, ch: Option<&str>, mods: Mods) -> Keyed {
        if !matches!(key, "left" | "right" | "up" | "down") {
            self.end_nudge();
        }
        if self.typing.is_some() {
            return self.key_typing(key, ch, mods);
        }
        self.key_board(key, mods)
    }

    /// A key went up.
    pub fn key_up(&mut self, key: &str) {
        match key {
            "space" => self.space = false,
            "left" | "right" | "up" | "down" => self.end_nudge(),
            _ => {}
        }
    }

    /// Types `s` into the text being typed (a paste).
    pub fn insert_text(&mut self, s: &str) {
        self.with_text(|t| t.insert(s));
    }

    fn with_text(&mut self, f: impl FnOnce(&mut TextEdit)) {
        if let Some(t) = &mut self.typing {
            f(t);
        }
    }

    /// A key while a text is being typed: editing keys, and every character goes into the text
    /// (letters are not tool shortcuts here).
    fn key_typing(&mut self, key: &str, ch: Option<&str>, mods: Mods) -> Keyed {
        match key {
            "escape" => {
                self.commit_text();
                self.tool = Tool::Select;
            }
            "enter" if mods.ctrl => {
                self.commit_text();
                return Keyed::Do(Command::Send);
            }
            "enter" if mods.shift => self.with_text(TextEdit::newline),
            "enter" => self.commit_text(),
            "backspace" => self.with_text(TextEdit::backspace),
            "delete" => self.with_text(TextEdit::delete),
            "left" => self.with_text(TextEdit::left),
            "right" => self.with_text(TextEdit::right),
            "up" => self.with_text(TextEdit::up),
            "down" => self.with_text(TextEdit::down),
            "home" => self.with_text(TextEdit::home),
            "end" => self.with_text(TextEdit::end),
            "tab" => self.with_text(|t| t.insert("\t")),
            "v" if mods.ctrl => return Keyed::Do(Command::PasteText),
            _ if mods.ctrl => {}
            _ => match ch {
                Some(s) => self.with_text(|t| t.insert(s)),
                None => return Keyed::Ignored,
            },
        }
        Keyed::Done
    }

    fn nudge(&mut self, d: Point) {
        if self.selection.is_empty() || self.drag.is_some() {
            return;
        }
        if !self.nudging {
            self.scene.begin();
            self.nudging = true;
        }
        self.scene.move_by(&self.selection, d);
        self.changed();
    }

    /// A key on the board itself: the shortcuts.
    fn key_board(&mut self, key: &str, mods: Mods) -> Keyed {
        let big = if mods.shift { 10.0 } else { 1.0 } * NUDGE;
        match (key, mods.ctrl) {
            ("escape", _) => {
                if !self.cancel_drag() {
                    if self.tool != Tool::Select {
                        self.tool = Tool::Select;
                    } else if !self.selection.is_empty() {
                        self.selection.clear();
                    } else {
                        return Keyed::Do(Command::Close);
                    }
                }
            }
            ("enter", true) => return Keyed::Do(Command::Send),
            ("z", true) if mods.shift => self.redo(),
            ("y", true) => self.redo(),
            ("z", true) => self.undo(),
            ("a", true) => self.select_all(),
            ("d", true) => self.duplicate(),
            ("c", true) if mods.shift || self.selection.is_empty() => {
                return Keyed::Do(Command::CopyImage)
            }
            ("c", true) => self.copy_selection(),
            ("x", true) => self.cut_selection(),
            ("v", true) => self.paste(),
            ("0", true) => self.zoom_reset(),
            ("=" | "+", true) => self.zoom_by(ZOOM_STEP),
            ("-", true) => self.zoom_by(1.0 / ZOOM_STEP),
            ("delete" | "backspace", false) => self.delete_selection(),
            ("left", false) => self.nudge(Point::new(-big, 0.0)),
            ("right", false) => self.nudge(Point::new(big, 0.0)),
            ("up", false) => self.nudge(Point::new(0.0, -big)),
            ("down", false) => self.nudge(Point::new(0.0, big)),
            ("space", false) => self.space = true,
            ("1", false) if mods.shift => self.fit(),
            (k, false) => match Tool::from_key(k).filter(|_| !mods.alt) {
                Some(tool) => self.set_tool(tool),
                None => return Keyed::Ignored,
            },
            _ => return Keyed::Ignored,
        }
        Keyed::Done
    }
}
