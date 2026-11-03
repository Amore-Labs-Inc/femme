//! The text being typed on the canvas: a buffer and a caret, with the keys of a one-box text
//! editor (insert, backspace, delete, arrows, home, end, new line). Pure; the caret is a byte
//! index that always sits on a character boundary.

use super::geom::Point;
use super::scene::{Kind, Shape, Style};

/// A text on its way into the scene (a new one, or an existing one being changed).
#[derive(Clone, Debug, PartialEq)]
pub struct TextEdit {
    /// The shape being changed; the board hides it while the edit is shown. None for a new text.
    pub target: Option<u32>,
    /// Top-left corner of the block.
    pub at: Point,
    /// What the text will look like.
    pub style: Style,
    text: String,
    caret: usize,
}

impl TextEdit {
    /// An empty text to type at `at`.
    pub fn new(at: Point, style: Style) -> TextEdit {
        TextEdit {
            target: None,
            at,
            style,
            text: String::new(),
            caret: 0,
        }
    }

    /// An edit of an existing text shape, the caret at its end. None for any other shape.
    pub fn of(shape: &Shape) -> Option<TextEdit> {
        let Kind::Text { at, text } = &shape.kind else {
            return None;
        };
        Some(TextEdit {
            target: Some(shape.id),
            at: *at,
            style: shape.style,
            text: text.clone(),
            caret: text.len(),
        })
    }

    /// The text so far.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The caret's byte index.
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// The caret's line (0 based) and its byte offset in that line.
    pub fn caret_line(&self) -> (usize, usize) {
        let before = &self.text[..self.caret];
        let line = before.matches('\n').count();
        let start = before.rfind('\n').map_or(0, |i| i + 1);
        (line, self.caret - start)
    }

    /// Types `s` at the caret. Control characters are dropped, except new lines (a pasted `\r\n`
    /// is one new line) and tabs, which become two spaces.
    pub fn insert(&mut self, s: &str) {
        let clean: String = s
            .replace("\r\n", "\n")
            .chars()
            .filter_map(|c| match c {
                '\n' => Some("\n".to_string()),
                '\r' => Some("\n".to_string()),
                '\t' => Some("  ".to_string()),
                c if c.is_control() => None,
                c => Some(c.to_string()),
            })
            .collect();
        self.text.insert_str(self.caret, &clean);
        self.caret += clean.len();
    }

    /// A new line at the caret.
    pub fn newline(&mut self) {
        self.insert("\n");
    }

    /// Deletes the character before the caret.
    pub fn backspace(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.text.replace_range(prev..self.caret, "");
            self.caret = prev;
        }
    }

    /// Deletes the character after the caret.
    pub fn delete(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.text.replace_range(self.caret..next, "");
        }
    }

    /// Moves the caret one character left.
    pub fn left(&mut self) {
        if let Some(prev) = self.prev_boundary() {
            self.caret = prev;
        }
    }

    /// Moves the caret one character right.
    pub fn right(&mut self) {
        if let Some(next) = self.next_boundary() {
            self.caret = next;
        }
    }

    /// Moves the caret to the start of its line.
    pub fn home(&mut self) {
        self.caret = self.text[..self.caret].rfind('\n').map_or(0, |i| i + 1);
    }

    /// Moves the caret to the end of its line.
    pub fn end(&mut self) {
        self.caret = self.text[self.caret..]
            .find('\n')
            .map_or(self.text.len(), |i| self.caret + i);
    }

    /// Moves the caret to the same column of the line above (the start of the text on the first line).
    pub fn up(&mut self) {
        let (line, _) = self.caret_line();
        if line == 0 {
            self.caret = 0;
        } else {
            self.go_to_line(line - 1);
        }
    }

    /// Moves the caret to the same column of the line below (the end of the text on the last line).
    pub fn down(&mut self) {
        let (line, _) = self.caret_line();
        if line + 1 >= self.text.split('\n').count() {
            self.caret = self.text.len();
        } else {
            self.go_to_line(line + 1);
        }
    }

    /// Puts the caret on `line`, as many characters in as it is now (or at the line's end).
    fn go_to_line(&mut self, line: usize) {
        let col = self.text[..self.caret]
            .rsplit('\n')
            .next()
            .map_or(0, |l| l.chars().count());
        let start: usize = self.text.split('\n').take(line).map(|l| l.len() + 1).sum();
        let target = self.text[start..].split('\n').next().unwrap_or("");
        let offset = target
            .char_indices()
            .nth(col)
            .map_or(target.len(), |(i, _)| i);
        self.caret = start + offset;
    }

    fn prev_boundary(&self) -> Option<usize> {
        self.text[..self.caret]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
    }

    fn next_boundary(&self) -> Option<usize> {
        self.text[self.caret..]
            .chars()
            .next()
            .map(|c| self.caret + c.len_utf8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(text: &str) -> TextEdit {
        let mut e = TextEdit::new(Point::ZERO, Style::default());
        e.insert(text);
        e
    }

    #[test]
    fn typing_inserts_at_the_caret() {
        let mut e = edit("helo");
        e.left();
        e.insert("l");
        assert_eq!(e.text(), "hello");
        assert_eq!(e.caret(), 4);
        e.end();
        e.insert(" world");
        assert_eq!(e.text(), "hello world");
        assert_eq!(e.caret(), e.text().len());
    }

    #[test]
    fn backspace_and_delete_take_whole_characters() {
        let mut e = edit("aéz");
        e.backspace();
        assert_eq!(e.text(), "aé");
        e.backspace();
        assert_eq!(e.text(), "a");
        e.backspace();
        e.backspace();
        assert_eq!(e.text(), "");
        let mut e = edit("héllo");
        e.home();
        e.right();
        e.delete();
        assert_eq!(e.text(), "hllo");
        e.end();
        e.delete();
        assert_eq!(e.text(), "hllo");
    }

    #[test]
    fn the_caret_walks_over_multibyte_characters() {
        let mut e = edit("日本語");
        assert_eq!(e.caret(), 9);
        e.left();
        assert_eq!(e.caret(), 6);
        e.home();
        assert_eq!(e.caret(), 0);
        e.left();
        assert_eq!(e.caret(), 0);
        e.right();
        e.right();
        assert_eq!(e.caret(), 6);
    }

    #[test]
    fn new_lines_split_the_text_and_home_end_work_per_line() {
        let mut e = edit("one");
        e.newline();
        e.insert("two");
        assert_eq!(e.text(), "one\ntwo");
        assert_eq!(e.caret_line(), (1, 3));
        e.home();
        assert_eq!(e.caret_line(), (1, 0));
        e.up();
        assert_eq!(e.caret_line(), (0, 0));
        e.end();
        assert_eq!(e.caret_line(), (0, 3));
        e.right();
        assert_eq!(e.caret_line(), (1, 0), "right steps over the new line");
    }

    #[test]
    fn up_and_down_keep_the_column() {
        let mut e = edit("abcdef\nxy\nabcdef");
        // The caret is at the end of the last line, column 6.
        e.up();
        assert_eq!(
            e.caret_line(),
            (1, 2),
            "a shorter line takes the caret to its end"
        );
        e.up();
        assert_eq!(e.caret_line(), (0, 2));
        e.up();
        assert_eq!(e.caret(), 0, "up on the first line goes to the start");
        e.down();
        assert_eq!(e.caret_line(), (1, 0));
        e.end();
        e.down();
        assert_eq!(e.caret_line(), (2, 2));
        e.down();
        assert_eq!(
            e.caret(),
            e.text().len(),
            "down on the last line goes to the end"
        );
    }

    #[test]
    fn up_and_down_count_characters_not_bytes() {
        let mut e = edit("éé\nabcd");
        e.up();
        // Column 4 on a two-character line: the end of that line.
        assert_eq!(e.caret_line(), (0, 4));
        e.down();
        assert_eq!(e.caret_line(), (1, 2), "two characters in");
    }

    #[test]
    fn pasted_text_is_cleaned() {
        let mut e = edit("");
        e.insert("a\r\nb\tc\u{7}d\re");
        assert_eq!(e.text(), "a\nb  cd\ne");
    }

    #[test]
    fn editing_an_existing_text_starts_at_its_end_and_only_for_texts() {
        let t = Shape {
            id: 4,
            kind: Kind::Text {
                at: Point::new(3., 4.),
                text: "hi there".into(),
            },
            style: Style::default(),
        };
        let e = TextEdit::of(&t).unwrap();
        assert_eq!(e.target, Some(4));
        assert_eq!(e.at, Point::new(3., 4.));
        assert_eq!(e.caret(), 8);
        let r = Shape {
            id: 5,
            kind: Kind::Rect {
                a: Point::ZERO,
                b: Point::ZERO,
            },
            style: Style::default(),
        };
        assert!(TextEdit::of(&r).is_none());
    }
}
