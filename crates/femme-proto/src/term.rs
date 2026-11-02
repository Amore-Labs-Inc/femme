//! Terminal state on the wire. The emulator lives on the server (in the holder); clients keep a
//! cache of rows keyed by absolute row number and apply [`Frame`]s to it. Nothing here is a byte
//! stream: reconnecting means asking for frames since the last `rev`, never replaying output.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

use crate::PaneId;

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    pub struct Mods: u8 {
        const SHIFT = 1;
        const ALT = 2;
        const CTRL = 4;
        const SUPER = 8;
    }
}

/// A key as the user pressed it. The holder turns it into bytes using the terminal's *live* modes
/// (application cursor keys, kitty keyboard, bracketed paste), so the client never races a mode change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub code: KeyCode,
    pub mods: Mods,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseKind {
    Press,
    Release,
    Move,
    ScrollUp,
    ScrollDown,
}

/// Mouse event in cell coordinates of the visible screen. Encoded by the holder per the active mouse mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseEv {
    pub kind: MouseKind,
    /// 0 left, 1 middle, 2 right; ignored for scroll.
    pub button: u8,
    pub col: u16,
    pub row: u16,
    pub mods: Mods,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    pub struct Attrs: u16 {
        const BOLD = 1 << 0;
        const DIM = 1 << 1;
        const ITALIC = 1 << 2;
        const UNDERLINE = 1 << 3;
        const DOUBLE_UNDERLINE = 1 << 4;
        const CURLY_UNDERLINE = 1 << 5;
        const DOTTED_UNDERLINE = 1 << 6;
        const DASHED_UNDERLINE = 1 << 7;
        const STRIKETHROUGH = 1 << 8;
        const INVERSE = 1 << 9;
        const HIDDEN = 1 << 10;
        const BLINK = 1 << 11;
    }
}

/// Cell style. Sent once per distinct style per view, then referenced by id (see [`Frame::new_styles`]).
/// Style id 0 is always the default style and is never sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub underline: Color,
    pub attrs: Attrs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

/// Cursor position relative to the visible screen (`row` 0 = `Frame::top_abs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub shape: CursorShape,
    pub visible: bool,
}

bitflags! {
    /// Terminal modes the client cares about (for prediction and input encoding decisions).
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    pub struct Modes: u32 {
        const APP_CURSOR = 1 << 0;
        const APP_KEYPAD = 1 << 1;
        const BRACKETED_PASTE = 1 << 2;
        const MOUSE_CLICK = 1 << 3;
        const MOUSE_DRAG = 1 << 4;
        const MOUSE_MOTION = 1 << 5;
        const SGR_MOUSE = 1 << 6;
        const FOCUS_IN_OUT = 1 << 7;
        const ALT_SCREEN = 1 << 8;
        const LINE_WRAP = 1 << 9;
        const ALTERNATE_SCROLL = 1 << 10;
        const KITTY_KEYBOARD = 1 << 11;
        const INSERT = 1 << 12;
        const ORIGIN = 1 << 13;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Screen {
    #[default]
    Primary,
    Alt,
}

/// One row of cells, identified by its absolute row number (`abs`): rows are numbered from the
/// first line the terminal ever produced and never renumbered. Scrollback rows are immutable;
/// visible rows change and get resent with a newer rev.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RowPatch {
    pub abs: u64,
    /// This row continues on the next one (soft wrap), for copy and reflow.
    pub wrapped: bool,
    /// `(style id, text)` runs, left to right. Trailing default-style blanks are omitted.
    pub runs: Vec<(u16, String)>,
    /// Columns that are the second half of a wide character (the client draws nothing there).
    pub wide_cols: Vec<u16>,
}

/// Sampled on ~1 % of frames: holder-monotonic nanoseconds so the client can build per-hop histograms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Timing {
    /// The input seq this frame is the first response to, if any.
    pub seq: u32,
    pub input_recv_ns: u64,
    pub pty_write_ns: u64,
    pub pty_read_ns: u64,
    pub frame_sent_ns: u64,
}

/// A screen update. Apply rule: the client must currently hold `base_rev`; after applying it
/// holds `rev`. `base_rev == 0` is a full snapshot (all visible rows, all styles used; the client
/// drops its style table and visible rows first). If the client holds anything else it re-sends
/// [`TermUp::Open`] with the rev it has.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Frame {
    /// Holder instance. A new epoch means the PTY was recreated: the client resets everything.
    pub epoch: u64,
    pub rev: u64,
    pub base_rev: u64,
    /// Highest input `seq` the holder has written to the PTY (dedups resends after reconnect).
    pub applied_seq: u32,
    /// Highest input `seq` whose effect is reflected in this frame (confirms predictions).
    pub echo_seq: u32,
    pub screen: Screen,
    pub cols: u16,
    pub rows: u16,
    /// Absolute row number of the first visible row.
    pub top_abs: u64,
    /// Oldest scrollback row still available for [`TermUp::GetRows`].
    pub history_lo_abs: u64,
    pub cursor: Cursor,
    pub modes: Modes,
    pub title: Option<String>,
    /// Styles first used by this frame, to add to the view's style table.
    pub new_styles: Vec<(u16, Style)>,
    pub row_patches: Vec<RowPatch>,
    pub timing: Option<Timing>,
}

/// Client → holder, on a `term:<pane>` stream (one per attached pane).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TermUp {
    /// First message on the stream. `have_rev == 0` or an unknown epoch → a full frame follows.
    Open {
        pane: PaneId,
        epoch: u64,
        have_rev: u64,
        /// Identifies this desktop process across reconnects, so its last-known input seq carries over.
        client_instance: u128,
    },
    Key { seq: u32, key: Key },
    Paste { seq: u32, text: String },
    /// Raw bytes (used by the compat layer's `/input`; desktop clients prefer `Key`).
    Bytes { seq: u32, data: Vec<u8> },
    Mouse { seq: u32, ev: MouseEv },
    Resize { cols: u16, rows: u16 },
    Focus(bool),
    /// Flow control: the client applied everything up to `rev`. The holder keeps ≤ 8 frames /
    /// 128 KiB unacked per view and merges while blocked.
    Ack { rev: u64 },
    /// Lazy scrollback fetch, `[from_abs, to_abs)`.
    GetRows { from_abs: u64, to_abs: u64 },
}

/// Holder → client on the same stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TermDown {
    Frame(Frame),
    /// Answer to `GetRows`.
    Rows(Vec<RowPatch>),
    Exited { status: i32 },
    Bell,
}
