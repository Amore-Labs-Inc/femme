//! Cells to wire rows: style mapping, the emulator-wide style table, `RowPatch` rendering and the
//! ANSI dump used by the phone board.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::Color as AColor;
use femme_proto::term::{Attrs, Color, RowPatch, Style};

/// Cell flags that are part of a style; the rest (`WRAPLINE`, wide-char markers) are layout.
pub(super) const STYLE_FLAGS: Flags = Flags::INVERSE
    .union(Flags::BOLD)
    .union(Flags::ITALIC)
    .union(Flags::UNDERLINE)
    .union(Flags::DIM)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT)
    .union(Flags::DOUBLE_UNDERLINE)
    .union(Flags::UNDERCURL)
    .union(Flags::DOTTED_UNDERLINE)
    .union(Flags::DASHED_UNDERLINE);

/// Style ids are `u16` and 0 is the default style.
const MAX_STYLES: usize = u16::MAX as usize;

/// Multiply-rotate hash for the style table. Its keys are a few small integers, and std's SipHash
/// costs more than the SGR sequence (or the run boundary) the lookup is for.
#[derive(Default)]
struct StyleHasher(u64);

impl StyleHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl Hasher for StyleHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.add(u64::from(b));
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(u64::from(i));
    }
    #[inline]
    fn write_isize(&mut self, i: isize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0.rotate_left(26)
    }
}

/// Every style the emulator has put on screen, with a stable id. Ids are global (not per view) so
/// `Emulator::rows` can answer without knowing who asks; views track how much of the table they
/// have been sent. Styles are only defined while feeding (`Tap`) and by a rebuild, never while
/// rendering, so a frame or a rows answer cannot name an id no client was sent. Entries are never
/// removed, except by a rebuild (see `Emulator`).
#[derive(Default)]
pub(super) struct StyleTable {
    ids: HashMap<Style, u16, BuildHasherDefault<StyleHasher>>,
    /// `list[id - 1]`.
    list: Vec<Style>,
    /// An intern failed because the table was full: the emulator rebuilds it at the end of the feed.
    pub(super) full: bool,
}

impl StyleTable {
    /// Id for `s`, adding it if new. Returns 0 (default) when the table is full, and flags `full`.
    pub(super) fn intern(&mut self, s: Style) -> u16 {
        if s == Style::default() {
            return 0;
        }
        if let Some(&id) = self.ids.get(&s) {
            return id;
        }
        if self.list.len() >= MAX_STYLES {
            self.full = true;
            return 0;
        }
        self.list.push(s);
        let id = self.list.len() as u16;
        self.ids.insert(s, id);
        id
    }

    /// Id of a style that is already defined: 0 for the default style, and for one the table never
    /// had room for (see `intern`), which shows as the default style.
    pub(super) fn id(&self, s: Style) -> u16 {
        self.ids.get(&s).copied().unwrap_or(0)
    }

    /// Number of styles defined (the highest id).
    pub(super) fn len(&self) -> usize {
        self.list.len()
    }

    /// Definitions of the styles with id > `n`, in id order.
    pub(super) fn since(&self, n: usize) -> Vec<(u16, Style)> {
        self.list
            .iter()
            .enumerate()
            .skip(n)
            .map(|(i, s)| ((i + 1) as u16, *s))
            .collect()
    }

    pub(super) fn clear(&mut self) {
        self.ids.clear();
        self.list.clear();
        self.full = false;
    }
}

/// Alacritty colour to wire colour: the 16 ANSI colours become `Indexed(0..16)` (dim variants map
/// to their base colour, the `DIM` attribute travels separately), default fg/bg/cursor `Default`.
pub(super) fn conv_color(c: AColor) -> Color {
    match c {
        AColor::Named(n) => match n as usize {
            i @ 0..=15 => Color::Indexed(i as u8),
            i @ 259..=266 => Color::Indexed((i - 259) as u8),
            _ => Color::Default,
        },
        AColor::Indexed(i) => Color::Indexed(i),
        AColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

/// The style of one cell.
pub(super) fn cell_style(c: &Cell) -> Style {
    let f = c.flags;
    let mut attrs = Attrs::empty();
    for (flag, attr) in [
        (Flags::BOLD, Attrs::BOLD),
        (Flags::DIM, Attrs::DIM),
        (Flags::ITALIC, Attrs::ITALIC),
        (Flags::UNDERLINE, Attrs::UNDERLINE),
        (Flags::DOUBLE_UNDERLINE, Attrs::DOUBLE_UNDERLINE),
        (Flags::UNDERCURL, Attrs::CURLY_UNDERLINE),
        (Flags::DOTTED_UNDERLINE, Attrs::DOTTED_UNDERLINE),
        (Flags::DASHED_UNDERLINE, Attrs::DASHED_UNDERLINE),
        (Flags::STRIKEOUT, Attrs::STRIKETHROUGH),
        (Flags::INVERSE, Attrs::INVERSE),
        (Flags::HIDDEN, Attrs::HIDDEN),
    ] {
        if f.contains(flag) {
            attrs |= attr;
        }
    }
    Style {
        fg: conv_color(c.fg),
        bg: conv_color(c.bg),
        underline: c.underline_color().map_or(Color::Default, conv_color),
        attrs,
    }
}

/// Cheap "same style as the previous cell" test on the raw cell fields.
#[inline]
fn same_style(a: &Cell, b: &Cell) -> bool {
    a.fg == b.fg
        && a.bg == b.bg
        && (a.flags & STYLE_FLAGS) == (b.flags & STYLE_FLAGS)
        && a.underline_color() == b.underline_color()
}

/// What a cell shows. Control characters (a tab stored in a blank cell) show as blanks.
#[inline]
fn shown(c: char) -> char {
    if c.is_control() {
        ' '
    } else {
        c
    }
}

/// Render one row of cells into a `RowPatch`. `style_id` maps a style to its wire id.
///
/// Runs are maximal stretches of one style id; wide-character spacer cells are skipped (their
/// columns go to `wide_cols`); zero-width characters follow their base character, at most one per
/// cell in the whole row; trailing default-style blanks are dropped; `wrapped` is the last cell's
/// `WRAPLINE` flag.
pub(super) fn render_row(
    cells: &[Cell],
    abs: u64,
    style_id: &mut impl FnMut(Style) -> u16,
) -> RowPatch {
    let mut runs: Vec<(u16, String)> = Vec::new();
    let mut wide_cols: Vec<u16> = Vec::new();
    let mut cur = String::new();
    let mut cur_id = 0u16;
    let mut prev: Option<&Cell> = None;
    // ponytail: a row of n cells shows at most n marks in total (`Tap` already stops 16 a cell),
    // so a row is at most a few KB; real text needs a handful per row.
    let mut marks_left = cells.len();
    for (col, cell) in cells.iter().enumerate() {
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            wide_cols.push(col as u16);
            continue;
        }
        if !prev.is_some_and(|p| same_style(p, cell)) {
            let id = style_id(cell_style(cell));
            if id != cur_id {
                if !cur.is_empty() {
                    runs.push((cur_id, std::mem::take(&mut cur)));
                }
                cur_id = id;
            }
        }
        prev = Some(cell);
        cur.push(shown(cell.c));
        if let Some(z) = cell.zerowidth() {
            let take = z.len().min(marks_left);
            cur.extend(z[..take].iter().copied());
            marks_left -= take;
        }
    }
    if !cur.is_empty() {
        runs.push((cur_id, cur));
    }
    while let Some((id, s)) = runs.last_mut() {
        if *id != 0 {
            break;
        }
        s.truncate(s.trim_end_matches(' ').len());
        if s.is_empty() {
            runs.pop();
        } else {
            break;
        }
    }
    let wrapped = cells
        .last()
        .is_some_and(|c| c.flags.contains(Flags::WRAPLINE));
    RowPatch {
        abs,
        wrapped,
        runs,
        wide_cols,
    }
}

/// Upper estimate of a patch's size on the wire (postcard: a few bytes per field, the text itself).
pub(super) fn wire_size(p: &RowPatch) -> usize {
    20 + p.runs.iter().map(|(_, t)| t.len() + 6).sum::<usize>() + 3 * p.wide_cols.len()
}

/// True when a patch is exactly what a client assumes for a row it was never sent: nothing drawn,
/// not a soft-wrap continuation, no wide-character halves (reflow can strand a lone spacer cell).
pub(super) fn is_blank(p: &RowPatch) -> bool {
    p.runs.is_empty() && !p.wrapped && p.wide_cols.is_empty()
}

fn push_color(out: &mut String, c: Color, base: u8, bright: u8, ext: u8) {
    use std::fmt::Write;
    let _ = match c {
        Color::Default => return,
        Color::Indexed(n) if n < 8 => write!(out, ";{}", base + n),
        Color::Indexed(n) if n < 16 => write!(out, ";{}", bright + n - 8),
        Color::Indexed(n) => write!(out, ";{ext};5;{n}"),
        Color::Rgb(r, g, b) => write!(out, ";{ext};2;{r};{g};{b}"),
    };
}

/// `ESC [ 0 ; ... m` for a style: reset, then every attribute and colour it has.
fn push_sgr(out: &mut String, s: &Style) {
    out.push_str("\x1b[0");
    for (attr, code) in [
        (Attrs::BOLD, ";1"),
        (Attrs::DIM, ";2"),
        (Attrs::ITALIC, ";3"),
        (Attrs::UNDERLINE, ";4"),
        (Attrs::DOUBLE_UNDERLINE, ";4:2"),
        (Attrs::CURLY_UNDERLINE, ";4:3"),
        (Attrs::DOTTED_UNDERLINE, ";4:4"),
        (Attrs::DASHED_UNDERLINE, ";4:5"),
        (Attrs::BLINK, ";5"),
        (Attrs::INVERSE, ";7"),
        (Attrs::HIDDEN, ";8"),
        (Attrs::STRIKETHROUGH, ";9"),
    ] {
        if s.attrs.contains(attr) {
            out.push_str(code);
        }
    }
    push_color(out, s.fg, 30, 90, 38);
    push_color(out, s.bg, 40, 100, 48);
    // 58 has no 8-colour shortcuts: indexed colours always use the 256-colour form.
    match s.underline {
        Color::Default => {}
        Color::Indexed(n) => out.push_str(&format!(";58;5;{n}")),
        Color::Rgb(r, g, b) => out.push_str(&format!(";58;2;{r};{g};{b}")),
    }
    out.push('m');
}

/// One row as text with SGR sequences, `ESC [ 0 m` at the end when a style was active.
pub(super) fn ansi_row(cells: &[Cell]) -> String {
    let mut styles: Vec<Style> = Vec::new();
    let patch = render_row(
        cells,
        0,
        &mut |s| match styles.iter().position(|x| *x == s) {
            Some(i) => (i + 1) as u16,
            None if s == Style::default() => 0,
            None => {
                styles.push(s);
                styles.len() as u16
            }
        },
    );
    let mut out = String::new();
    let mut active = 0u16;
    for (id, text) in &patch.runs {
        if *id != active {
            if *id == 0 {
                out.push_str("\x1b[0m");
            } else {
                push_sgr(&mut out, &styles[usize::from(*id) - 1]);
            }
            active = *id;
        }
        out.push_str(text);
    }
    if active != 0 {
        out.push_str("\x1b[0m");
    }
    out
}
