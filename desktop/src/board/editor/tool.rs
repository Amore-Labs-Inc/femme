//! The vocabulary the editor and the board share: the tools, the modifier keys, the mouse buttons
//! and what a key can ask of the board.

/// A tool of the strip. The numbers and letters are Excalidraw's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tool {
    /// Pick, move and delete shapes (`1`, `v`).
    #[default]
    Select,
    /// Drag out a rectangle (`2`, `r`).
    Rect,
    /// Drag out an ellipse (`3`, `o`).
    Ellipse,
    /// Drag out an arrow (`4`, `a`).
    Arrow,
    /// Drag out a straight line (`5`, `l`).
    Line,
    /// Draw with the pointer (`6`, `p`).
    Freehand,
    /// Click and type a label (`7`, `t`).
    Text,
}

impl Tool {
    /// Every tool, in the order of the strip.
    pub const ALL: [Tool; 7] = [
        Tool::Select,
        Tool::Rect,
        Tool::Ellipse,
        Tool::Arrow,
        Tool::Line,
        Tool::Freehand,
        Tool::Text,
    ];

    /// The name, for tooltips.
    pub fn name(self) -> &'static str {
        match self {
            Tool::Select => "select",
            Tool::Rect => "rectangle",
            Tool::Ellipse => "ellipse",
            Tool::Arrow => "arrow",
            Tool::Line => "line",
            Tool::Freehand => "draw",
            Tool::Text => "text",
        }
    }

    /// The number key.
    pub fn number(self) -> char {
        match self {
            Tool::Select => '1',
            Tool::Rect => '2',
            Tool::Ellipse => '3',
            Tool::Arrow => '4',
            Tool::Line => '5',
            Tool::Freehand => '6',
            Tool::Text => '7',
        }
    }

    /// The letter key.
    pub fn letter(self) -> char {
        match self {
            Tool::Select => 'v',
            Tool::Rect => 'r',
            Tool::Ellipse => 'o',
            Tool::Arrow => 'a',
            Tool::Line => 'l',
            Tool::Freehand => 'p',
            Tool::Text => 't',
        }
    }

    /// The tool a key (a gpui key name: `"1"`, `"r"`) picks.
    pub fn from_key(key: &str) -> Option<Tool> {
        let mut chars = key.chars();
        let c = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        let c = c.to_ascii_lowercase();
        Tool::ALL
            .into_iter()
            .find(|t| t.number() == c || t.letter() == c)
    }
}

/// The modifier keys held.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    /// The command key: ctrl, or cmd on a Mac.
    pub ctrl: bool,
    /// Shift: constrains a shape, adds to a selection, takes bigger steps.
    pub shift: bool,
    /// Alt.
    pub alt: bool,
}

/// The mouse button of a press. Only the two that do something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    /// The left one: draw, select, move.
    Primary,
    /// The middle one: pan.
    Middle,
}

/// Something a key asks for that the editor cannot do by itself, because it needs the window, the
/// clipboard or the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Send the drawing to Claude (ctrl-enter).
    Send,
    /// Put the drawing on the clipboard as a PNG (ctrl-c with nothing selected, ctrl-shift-c).
    CopyImage,
    /// Close the board (Esc with nothing left to cancel).
    Close,
    /// Insert the clipboard's text where the caret is (ctrl-v while typing).
    PasteText,
}

/// What the editor made of a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Keyed {
    /// Not a key of the board's: let it through.
    Ignored,
    /// Dealt with.
    Done,
    /// Dealt with as far as the editor can: the board does the rest.
    Do(Command),
}
