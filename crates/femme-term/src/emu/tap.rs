//! The handler between alacritty's parser and its `Term`.
//!
//! `Tap` forwards every parser callback to the `Term` unchanged and watches the few that can push
//! a row off the top of the primary screen into scrollback, because `alacritty_terminal` exposes
//! no scroll counter and its history size stops changing once the history is full. Before each
//! such call it predicts, from the cursor and the scroll region, how many rows will leave the
//! screen (`pushed`). The emulator turns that into absolute row numbers.
//!
//! What pushes (verified against `term/mod.rs` and `grid/mod.rs` of alacritty_terminal 0.26):
//! `Grid::scroll_up(origin..end, n)` moves the top `n` rows of the *whole* screen into history
//! exactly when `origin == 0`, whatever `end` is. It is called by
//! - `linefeed` when the cursor is on the last row of the scroll region (also reached from
//!   `input` and `put_tab` when a pending wrap wraps, and for a wide character that does not fit
//!   in the last column): `origin` is the region start, `n = 1`;
//! - `scroll_up` (CSI S): `origin` is the region start, `n` clamped to the region height;
//! - `delete_lines` (CSI M) with the cursor on screen row 0, inside the region;
//! - `clear_screen(All)` on the primary screen (`Grid::clear_viewport`): scrolls the used part of
//!   the screen into history, ignoring the scroll region;
//! - `Term::resize` (not a handler call; the emulator treats a resize as a new layout).
//!
//! Scrolling inside a region that starts below row 0, `insert_blank_lines`, `scroll_down` and
//! `reverse_index` never push. The alternate screen has no history, so nothing pushes there.
//! `Term` keeps the scroll region private, so it is mirrored here from the calls that change it.

use std::sync::{Mutex, PoisonError};

use alacritty_terminal::grid::{Dimensions, GridCell};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::cursor_icon::CursorIcon;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, Color as AColor, CursorShape, CursorStyle, Handler, Hyperlink,
    KeyboardModes, KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, NamedColor,
    NamedPrivateMode, PrivateMode, Rgb, ScpCharPath, ScpUpdateMode, StandardCharset,
    TabulationClearMode,
};
use femme_proto::term::Style;
use unicode_width::UnicodeWidthChar;

use super::render::{cell_style, StyleTable, STYLE_FLAGS};
use super::{default_color, EmuEvent, Listener, Shared, CELL_H, CELL_W};

type TemplateKey = (AColor, AColor, Flags, Option<AColor>);

/// Zero-width characters one cell keeps; further ones are dropped. Real text needs a handful
/// (a base with stacked accents, a ZWJ sequence), while a repeated `CSI b` or a plain flood would
/// otherwise grow one cell, and every frame that carries it, without limit.
const MAX_MARKS: usize = 16;

/// State that outlives one `feed` call.
pub(super) struct TapState {
    /// Scroll region `[start, end)` in screen rows, mirrored from the calls that set it.
    pub region: (i32, i32),
    /// Rows pushed off the top of the primary screen since the emulator last looked.
    pub pushed: u64,
    /// Scrollback was cleared (`ED 3`, `RIS`) since the emulator last looked.
    pub history_cleared: bool,
    /// The program switched screens (`CSI ? 1049 h/l`, or `RIS` while on the alt screen) since the
    /// emulator last looked, even if it switched back: the rows it saw may not be the ones now.
    pub alt_switched: bool,
    /// The cursor template whose styles have been interned, so unchanged SGR is a compare.
    template: TemplateKey,
}

impl TapState {
    pub(super) fn new(rows: usize) -> Self {
        Self {
            region: (0, rows as i32),
            pushed: 0,
            history_cleared: false,
            alt_switched: false,
            template: (
                AColor::Named(NamedColor::Foreground),
                AColor::Named(NamedColor::Background),
                Flags::empty(),
                None,
            ),
        }
    }
}

pub(super) struct Tap<'a> {
    pub term: &'a mut Term<Listener>,
    pub st: &'a mut TapState,
    pub styles: &'a mut StyleTable,
    pub shared: &'a Mutex<Shared>,
}

impl Tap<'_> {
    fn alt(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    fn push_event(&self, e: EmuEvent) {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .events
            .push(e);
    }

    /// `Grid::scroll_up(origin..region.end, lines)` after `Term::scroll_up_relative`'s clamp.
    fn pushed_by(&mut self, origin: i32, lines: usize) {
        if origin == 0 && !self.alt() {
            let height = (self.st.region.1 - self.st.region.0).max(0) as usize;
            self.st.pushed += lines.min(height) as u64;
        }
    }

    /// `Term::linefeed` from screen row `line`.
    fn linefeed_from(&mut self, line: i32) {
        if line + 1 == self.st.region.1 {
            self.pushed_by(self.st.region.0, 1);
        }
    }

    /// `Term::wrapline` (only reached when `LINE_WRAP` is set) from screen row `*line`: updates
    /// the row the way the cursor moves, counts a push.
    fn wrap(&mut self, line: &mut i32) {
        if *line + 1 >= self.st.region.1 {
            let next = *line + 1;
            self.linefeed_from(*line);
            if next != self.st.region.1 && (next as usize) < self.term.screen_lines() {
                *line += 1;
            }
        } else {
            *line += 1;
        }
    }

    /// Mirror of `Term::set_scrolling_region`.
    fn mirror_region(&mut self, top: usize, bottom: Option<usize>) {
        let lines = self.term.screen_lines();
        let bottom = bottom.unwrap_or(lines);
        if top >= bottom {
            return;
        }
        let lines = lines as i32;
        self.st.region = ((top as i32 - 1).min(lines), (bottom as i32).min(lines));
    }

    /// Rows `Grid::clear_viewport` scrolls into history: down to the last used row; with an empty
    /// screen, 0 if there is history to stop at, else 1 (an alacritty quirk, pinned by a test).
    fn clear_viewport_rows(&self) -> u64 {
        let grid = self.term.grid();
        for line in (0..self.term.screen_lines() as i32).rev() {
            if grid[Line(line)][..].iter().any(|c| !c.is_empty()) {
                return line as u64 + 1;
            }
        }
        u64::from(grid.history_size() == 0)
    }

    /// The two private modes that matter here: DECCOLM resets the scroll region, 1049 swaps screens.
    fn watch_private_mode(&mut self, mode: PrivateMode) {
        match mode {
            PrivateMode::Named(NamedPrivateMode::ColumnMode) => self.mirror_region(1, None),
            PrivateMode::Named(NamedPrivateMode::SwapScreenAndSetRestoreCursor) => {
                self.st.alt_switched = true
            }
            _ => {}
        }
    }

    /// True when the cell a zero-width character would join already has `MAX_MARKS` of them.
    /// Mirrors `Term::input`: the cell left of the cursor (the cursor's own with a wrap pending),
    /// and one more to the left from a wide character's spacer to the character.
    fn marks_full(&self) -> bool {
        let cur = &self.term.grid().cursor;
        let mut col = cur.point.column.0;
        if !cur.input_needs_wrap {
            col = col.saturating_sub(1);
        }
        let row = &self.term.grid()[cur.point.line];
        if row[Column(col)].flags.contains(Flags::WIDE_CHAR_SPACER) {
            col = col.saturating_sub(1);
        }
        row[Column(col)]
            .zerowidth()
            .is_some_and(|z| z.len() >= MAX_MARKS)
    }

    /// Intern the styles the cursor template can paint (its own, and the background-only one
    /// that erases and scrolling leave behind) so every style in any cell has an id already.
    fn sync_template(&mut self) {
        let t = &self.term.grid().cursor.template;
        let key = (t.fg, t.bg, t.flags & STYLE_FLAGS, t.underline_color());
        if key == self.st.template {
            return;
        }
        self.st.template = key;
        let full = cell_style(t);
        self.styles.intern(full);
        self.styles.intern(Style {
            bg: full.bg,
            ..Style::default()
        });
    }
}

/// Forward `name(args)` to the `Term`.
macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*);)*) => {
        $(
            #[inline]
            fn $name(&mut self, $($arg: $ty),*) {
                self.term.$name($($arg),*)
            }
        )*
    };
}

impl Handler for Tap<'_> {
    forward! {
        set_title(a: Option<String>);
        set_cursor_style(a: Option<CursorStyle>);
        set_cursor_shape(a: CursorShape);
        goto(a: i32, b: usize);
        goto_line(a: i32);
        goto_col(a: usize);
        insert_blank(a: usize);
        move_up(a: usize);
        move_down(a: usize);
        identify_terminal(a: Option<char>);
        device_status(a: usize);
        move_forward(a: usize);
        move_backward(a: usize);
        move_down_and_cr(a: usize);
        move_up_and_cr(a: usize);
        backspace();
        carriage_return();
        bell();
        substitute();
        set_horizontal_tabstop();
        scroll_down(a: usize);
        insert_blank_lines(a: usize);
        erase_chars(a: usize);
        delete_chars(a: usize);
        move_backward_tabs(a: u16);
        move_forward_tabs(a: u16);
        save_cursor_position();
        clear_line(a: LineClearMode);
        clear_tabs(a: TabulationClearMode);
        set_tabs(a: u16);
        reverse_index();
        set_mode(a: Mode);
        unset_mode(a: Mode);
        report_mode(a: Mode);
        report_private_mode(a: PrivateMode);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(a: CharsetIndex);
        configure_charset(a: CharsetIndex, b: StandardCharset);
        set_color(a: usize, b: Rgb);
        reset_color(a: usize);
        clipboard_store(a: u8, b: &[u8]);
        clipboard_load(a: u8, b: &str);
        decaln();
        push_title();
        pop_title();
        text_area_size_chars();
        set_hyperlink(a: Option<Hyperlink>);
        set_mouse_cursor_icon(a: CursorIcon);
        report_keyboard_mode();
        push_keyboard_mode(a: KeyboardModes);
        pop_keyboard_modes(a: u16);
        set_keyboard_mode(a: KeyboardModes, b: KeyboardModesApplyBehavior);
        set_modify_other_keys(a: ModifyOtherKeys);
        report_modify_other_keys();
        set_scp(a: ScpCharPath, b: ScpUpdateMode);
    }

    #[inline]
    fn input(&mut self, c: char) {
        let cur = &self.term.grid().cursor;
        // A narrow character with no wrap pending cannot move a row: the common case, one test.
        if cur.input_needs_wrap || !c.is_ascii() {
            let (mut line, mut col, mut need_wrap) =
                (cur.point.line.0, cur.point.column.0, cur.input_needs_wrap);
            let width = c.width();
            if width == Some(0) && self.marks_full() {
                return;
            }
            if let Some(w @ 1..) = width {
                if !self.alt() && self.term.mode().contains(TermMode::LINE_WRAP) {
                    if need_wrap {
                        self.wrap(&mut line);
                        col = 0;
                        need_wrap = false;
                    }
                    if w == 2 && !need_wrap && col + 1 >= self.term.columns() {
                        self.wrap(&mut line);
                    }
                }
            }
        }
        self.term.input(c)
    }

    fn put_tab(&mut self, count: u16) {
        let cur = &self.term.grid().cursor;
        if cur.input_needs_wrap && !self.alt() && self.term.mode().contains(TermMode::LINE_WRAP) {
            let mut line = cur.point.line.0;
            self.wrap(&mut line);
        }
        self.term.put_tab(count)
    }

    #[inline]
    fn linefeed(&mut self) {
        if !self.alt() {
            self.linefeed_from(self.term.grid().cursor.point.line.0);
        }
        self.term.linefeed()
    }

    fn newline(&mut self) {
        if !self.alt() {
            self.linefeed_from(self.term.grid().cursor.point.line.0);
        }
        self.term.newline()
    }

    fn scroll_up(&mut self, lines: usize) {
        self.pushed_by(self.st.region.0, lines);
        self.term.scroll_up(lines)
    }

    fn delete_lines(&mut self, lines: usize) {
        let origin = self.term.grid().cursor.point.line.0;
        let lines_left = self.term.screen_lines() - origin as usize;
        if lines_left.min(lines) > 0 && (self.st.region.0..self.st.region.1).contains(&origin) {
            self.pushed_by(origin, lines_left.min(lines));
        }
        self.term.delete_lines(lines)
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        match mode {
            ClearMode::All if !self.alt() => self.st.pushed += self.clear_viewport_rows(),
            ClearMode::Saved => self.st.history_cleared = true,
            _ => {}
        }
        self.term.clear_screen(mode)
    }

    fn reset_state(&mut self) {
        self.st.history_cleared = true;
        self.st.alt_switched |= self.alt();
        self.st.region = (0, self.term.screen_lines() as i32);
        self.term.reset_state();
        self.sync_template();
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        self.mirror_region(top, bottom);
        self.term.set_scrolling_region(top, bottom)
    }

    fn set_private_mode(&mut self, mode: PrivateMode) {
        self.watch_private_mode(mode);
        self.term.set_private_mode(mode)
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        self.watch_private_mode(mode);
        self.term.unset_private_mode(mode)
    }

    #[inline]
    fn terminal_attribute(&mut self, attr: Attr) {
        self.term.terminal_attribute(attr);
        self.sync_template();
    }

    fn restore_cursor_position(&mut self) {
        self.term.restore_cursor_position();
        self.sync_template();
    }

    /// OSC 4/10/11/12 `?`: answered here (not through `Term`'s event) so the reply reflects the
    /// colours as they are at this point of the stream: the app's own overrides, else the defaults.
    fn dynamic_color_sequence(&mut self, prefix: String, index: usize, terminator: &str) {
        let rgb = self.term.colors()[index].unwrap_or_else(|| default_color(index));
        let reply = format!(
            "\x1b]{prefix};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}{terminator}",
            r = rgb.r,
            g = rgb.g,
            b = rgb.b
        );
        self.push_event(EmuEvent::PtyWrite(reply.into_bytes()));
    }

    /// CSI 14 t: the server cannot know the client's font, so it reports a fixed cell size.
    fn text_area_size_pixels(&mut self) {
        let (h, w) = (
            self.term.screen_lines() * CELL_H,
            self.term.columns() * CELL_W,
        );
        self.push_event(EmuEvent::PtyWrite(format!("\x1b[4;{h};{w}t").into_bytes()));
    }
}
