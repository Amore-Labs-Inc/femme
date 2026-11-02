//! Pure half of the terminal glyph-run renderer: turns one [`RowPatch`] into batched draw
//! primitives (one quad per [`BgRun`], one shaped line per [`TextRun`]) and resolves wire
//! [`Style`]s to colours through a [`Palette`]. No gpui here, so all of it is unit-tested.
//!
//! Hot path: [`batch_row`] runs once per visible row per frame (up to 4 panes at 120 Hz). It never
//! allocates (it only pushes into the caller's reused Vecs) and is O(chars in the row).

use femme_proto::term::{Attrs, Color, RowPatch, Style};
use unicode_width::UnicodeWidthChar;

/// An sRGB colour, `[r, g, b]`.
pub type Rgb = [u8; 3];

/// How a cell is underlined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnderlineKind {
    /// No underline.
    None,
    /// One straight line.
    Single,
    /// Two straight lines.
    Double,
    /// A wavy line.
    Curly,
    /// A dotted line.
    Dotted,
    /// A dashed line.
    Dashed,
}

/// A cell style after palette lookup with INVERSE / DIM / HIDDEN applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// Glyph colour.
    pub fg: Rgb,
    /// Cell background; `None` is the pane background (no quad is drawn).
    pub bg: Option<Rgb>,
    /// Use the bold face (colours are not brightened).
    pub bold: bool,
    /// Use the italic face.
    pub italic: bool,
    /// Underline style.
    pub underline: UnderlineKind,
    /// Underline colour; `None` means `fg`.
    pub underline_color: Option<Rgb>,
    /// Draw a strikethrough line.
    pub strike: bool,
    /// Concealed text: its background is drawn, its glyphs are not.
    pub hidden: bool,
}

/// Default colours plus ANSI 0-15. Indices 16-255 are the fixed xterm colour cube and grey ramp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Palette {
    /// Default foreground (ink).
    pub fg: Rgb,
    /// Default background (the pane's paper).
    pub bg: Rgb,
    /// ANSI colours: 0-7 normal, 8-15 bright.
    pub ansi: [Rgb; 16],
}

/// `0xRRGGBB` as [`Rgb`], so the palette reads like the skin's hex tokens.
const fn hex(v: u32) -> Rgb {
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

/// Channel levels of the xterm 6x6x6 colour cube (indices 16-231).
const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Underline attributes in priority order: the first one set wins.
const UNDERLINES: [(Attrs, UnderlineKind); 5] = [
    (Attrs::UNDERLINE, UnderlineKind::Single),
    (Attrs::DOUBLE_UNDERLINE, UnderlineKind::Double),
    (Attrs::CURLY_UNDERLINE, UnderlineKind::Curly),
    (Attrs::DOTTED_UNDERLINE, UnderlineKind::Dotted),
    (Attrs::DASHED_UNDERLINE, UnderlineKind::Dashed),
];

impl Palette {
    /// FEMME's light skin: ink `#1E1E1E` on paper `#FEFEFE`. Colours 1-6 are skin tokens darkened
    /// to WCAG contrast >= 4.5 on paper, 9-14 to >= 3.0; 0 and 8 are dark, and 7 and 15 ("white",
    /// which dark-theme programs use for plain and emphasised text) are greys that still read on
    /// paper (>= 2.4), 15 the lighter one as usual.
    pub fn femme_light() -> Self {
        Self {
            fg: hex(0x1E1E1E),
            bg: hex(0xFEFEFE),
            ansi: [
                hex(0x1E1E1E), // 0 black: ink
                hex(0xD93036), // 1 red: skin red as is
                hex(0x027A42), // 2 green: skin #03AA5C darkened
                hex(0x9C6A00), // 3 yellow: skin amber #F2B33D darkened to ochre
                hex(0x2D5BD0), // 4 blue
                hex(0xB8379A), // 5 magenta: skin pink #D45BB6 darkened
                hex(0x00827A), // 6 cyan: skin teal #09AEA1 darkened
                hex(0x858585), // 7 white: skin grey (--done)
                hex(0x5A5A5A), // 8 bright black: dark grey
                hex(0xE8474D), // 9 bright red
                hex(0x029E55), // 10 bright green
                hex(0xB98000), // 11 bright yellow
                hex(0x4A7DF0), // 12 bright blue
                hex(0xD45BB6), // 13 bright magenta: skin pink as is
                hex(0x08968B), // 14 bright cyan
                hex(0xA3A3A3), // 15 bright white: light grey
            ],
        }
    }

    /// Looks up a wire colour: `Default` gives `default`, 0-15 come from [`Palette::ansi`], 16-231
    /// are the xterm 6x6x6 cube, 232-255 the grey ramp `8 + 10 * n`, and `Rgb` is used as is.
    pub fn color(&self, c: Color, default: Rgb) -> Rgb {
        match c {
            Color::Default => default,
            Color::Indexed(i @ 0..=15) => self.ansi[usize::from(i)],
            Color::Indexed(i @ 16..=231) => {
                let i = usize::from(i - 16);
                [CUBE[i / 36], CUBE[i / 6 % 6], CUBE[i % 6]]
            }
            Color::Indexed(i) => [8 + 10 * (i - 232); 3],
            Color::Rgb(r, g, b) => [r, g, b],
        }
    }

    /// Resolves a wire style. INVERSE swaps fg and bg (inverse default text is paper on ink), then
    /// DIM blends fg halfway toward the bg in effect, then a bg equal to the paper becomes `None`.
    /// BOLD only picks the bold face (no bright-colour promotion); BLINK is ignored.
    pub fn resolve(&self, s: &Style) -> Resolved {
        let a = s.attrs;
        let mut fg = self.color(s.fg, self.fg);
        let mut bg = self.color(s.bg, self.bg);
        if a.contains(Attrs::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if a.contains(Attrs::DIM) {
            fg = [half(fg[0], bg[0]), half(fg[1], bg[1]), half(fg[2], bg[2])];
        }
        Resolved {
            fg,
            bg: (bg != self.bg).then_some(bg),
            bold: a.contains(Attrs::BOLD),
            italic: a.contains(Attrs::ITALIC),
            underline: UNDERLINES
                .iter()
                .find_map(|&(flag, kind)| a.contains(flag).then_some(kind))
                .unwrap_or(UnderlineKind::None),
            underline_color: (s.underline != Color::Default).then(|| self.color(s.underline, fg)),
            strike: a.contains(Attrs::STRIKETHROUGH),
            hidden: a.contains(Attrs::HIDDEN),
        }
    }
}

/// Per-channel midpoint, `(a + b) / 2`.
fn half(a: u8, b: u8) -> u8 {
    ((u16::from(a) + u16::from(b)) / 2) as u8
}

/// A background quad: `cols` cells from column `col`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BgRun {
    /// First column.
    pub col: u16,
    /// Width in columns.
    pub cols: u16,
    /// Fill colour.
    pub color: Rgb,
}

/// Text of one style to shape as a single line, starting at column `col`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextRun<'a> {
    /// First column.
    pub col: u16,
    /// Columns covered, never 0. A wide char always comes alone, with `cols == 2`.
    pub cols: u16,
    /// The chars, borrowed from the [`RowPatch`]; zero-width chars stay with their base char.
    pub text: &'a str,
    /// The style of the RowPatch run it comes from.
    pub style: Resolved,
}

/// Turns one row into background quads and text lines, appending to `bgs` and `texts` (caller
/// clears them). `resolve(style_id)` is called once per RowPatch run; runs starting at or after
/// `max_cols` are skipped without resolving.
///
/// - Columns: a char takes one column, two when the next column is listed in `wide_cols` (the
///   server decides wideness, not unicode-width). Zero-width chars (combining marks, ZWJ,
///   variation selectors) take none and stay with the char before them.
/// - Backgrounds: touching cells with the same `Some` bg merge into one [`BgRun`], across style
///   runs; a `None` bg breaks it. Runs pushed before this call are never extended.
/// - Text: one [`TextRun`] per RowPatch run, split around each wide char, which gets its own
///   piece. Hidden pieces, and pieces of only `' '` without underline or strike, are left out
///   (their background is still drawn).
/// - Clipping at `max_cols`: a text piece keeps only the chars that fit whole (plus the zero-width
///   chars after the last one), and a wide char straddling the edge keeps only its background.
pub fn batch_row<'a>(
    row: &'a RowPatch,
    max_cols: u16,
    resolve: &mut impl FnMut(u16) -> Resolved,
    bgs: &mut Vec<BgRun>,
    texts: &mut Vec<TextRun<'a>>,
) {
    // Assumption (open question with the femme-term author): a wide char's spacer column has no
    // char in the text and only shows up in `wide_cols`. A placeholder char sent there would be
    // counted as a cell of its own and shift the rest of the row one column right.
    debug_assert!(
        row.wide_cols.windows(2).all(|w| w[0] < w[1]),
        "wide_cols must be ascending"
    );
    let first_bg = bgs.len();
    // `wide_cols` is ascending, so one forward-only cursor serves the row: no per-char search.
    let mut spacers = row.wide_cols.iter().copied().peekable();
    let mut col = 0u16;
    for (id, text) in &row.runs {
        if col >= max_cols {
            break;
        }
        let style = resolve(*id);
        let run_col = col;
        let mut piece: Option<Piece> = None;
        let mut end = text.len();
        for (i, c) in text.char_indices() {
            if c.width() == Some(0) {
                // Part of the cell before it, so it rides along in the open piece.
                // ponytail: at the very start of a run its base char is in the previous run
                // (another String) and the mark is dropped; alacritty keeps marks in their base
                // cell's style, so only a server bug gets here. Upgrade: an owned cluster string.
                if let Some(p) = &mut piece {
                    p.blank = false;
                }
                continue;
            }
            if col >= max_cols {
                end = i;
                break;
            }
            // Entries behind us only survive in malformed rows; drop them so they can't block.
            while spacers.next_if(|&s| s <= col).is_some() {}
            let wide = spacers.next_if_eq(&(col + 1)).is_some();
            let w = 1 + u16::from(wide);
            if w > max_cols - col {
                // A wide char straddling the edge: no glyph, its bg still covers the last column.
                end = i;
                col = max_cols;
                break;
            }
            if wide || piece.as_ref().is_some_and(|p| p.wide) {
                emit(texts, text, piece.take(), i, style);
            }
            let p = piece.get_or_insert(Piece {
                start: i,
                col,
                cols: 0,
                blank: true,
                wide,
            });
            p.cols += w;
            p.blank &= c == ' ';
            col += w;
        }
        emit(texts, text, piece, end, style);
        if let Some(color) = style.bg.filter(|_| col > run_col) {
            match bgs[first_bg..].last_mut() {
                Some(last) if last.color == color && last.col + last.cols == run_col => {
                    last.cols = col - last.col;
                }
                _ => bgs.push(BgRun {
                    col: run_col,
                    cols: col - run_col,
                    color,
                }),
            }
        }
    }
}

/// The [`TextRun`] being collected: run text from byte `start`, `cols` columns from `col`.
struct Piece {
    start: usize,
    col: u16,
    cols: u16,
    /// Only `' '` so far.
    blank: bool,
    /// Holds one wide char, which never shares its piece.
    wide: bool,
}

/// Pushes `piece`, ending at byte `end` of `text`, unless it would draw nothing.
fn emit<'a>(
    texts: &mut Vec<TextRun<'a>>,
    text: &'a str,
    piece: Option<Piece>,
    end: usize,
    style: Resolved,
) {
    let Some(p) = piece else { return };
    let invisible =
        style.hidden || (p.blank && style.underline == UnderlineKind::None && !style.strike);
    if !invisible {
        texts.push(TextRun {
            col: p.col,
            cols: p.cols,
            text: &text[p.start..end],
            style,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Rgb = [200, 0, 0];
    const GREEN: Rgb = [0, 150, 0];
    const BLUE: Rgb = [0, 0, 200];

    /// A style with only colours set.
    fn st(fg: Rgb, bg: Option<Rgb>) -> Resolved {
        Resolved {
            fg,
            bg,
            bold: false,
            italic: false,
            underline: UnderlineKind::None,
            underline_color: None,
            strike: false,
            hidden: false,
        }
    }

    fn row(runs: &[(u16, &str)], wide_cols: &[u16]) -> RowPatch {
        RowPatch {
            runs: runs.iter().map(|&(id, t)| (id, t.to_owned())).collect(),
            wide_cols: wide_cols.to_vec(),
            ..RowPatch::default()
        }
    }

    fn bg(col: u16, cols: u16, color: Rgb) -> BgRun {
        BgRun { col, cols, color }
    }

    /// Batches `row` with style id `n` resolving to `styles[n]`.
    fn batch<'a>(
        row: &'a RowPatch,
        max_cols: u16,
        styles: &[Resolved],
    ) -> (Vec<BgRun>, Vec<TextRun<'a>>) {
        let (mut bgs, mut texts) = (Vec::new(), Vec::new());
        batch_row(
            row,
            max_cols,
            &mut |id| styles[usize::from(id)],
            &mut bgs,
            &mut texts,
        );
        (bgs, texts)
    }

    /// `(col, cols, text)` of each text run.
    fn pieces<'a>(texts: &[TextRun<'a>]) -> Vec<(u16, u16, &'a str)> {
        texts.iter().map(|t| (t.col, t.cols, t.text)).collect()
    }

    /// WCAG 2 contrast ratio.
    fn contrast(a: Rgb, b: Rgb) -> f64 {
        let lin = |v: u8| {
            let v = f64::from(v) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        let lum = |c: Rgb| 0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2]) + 0.05;
        let (x, y) = (lum(a), lum(b));
        x.max(y) / x.min(y)
    }

    #[test]
    fn bg_merges_across_fg_changes() {
        let styles = [st(RED, Some(BLUE)), st(GREEN, Some(BLUE))];
        let r = row(&[(0, "ab"), (1, "cd")], &[]);
        let (bgs, texts) = batch(&r, 80, &styles);
        assert_eq!(bgs, [bg(0, 4, BLUE)]);
        assert_eq!(pieces(&texts), [(0, 2, "ab"), (2, 2, "cd")]);
        assert_eq!((texts[0].style.fg, texts[1].style.fg), (RED, GREEN));
    }

    #[test]
    fn default_bg_and_colour_changes_break_bg_runs() {
        let styles = [st(RED, Some(BLUE)), st(RED, None), st(RED, Some(GREEN))];
        let r = row(&[(0, "ab"), (1, "c"), (0, "de"), (2, "f"), (0, "g")], &[]);
        let (bgs, _) = batch(&r, 80, &styles);
        assert_eq!(
            bgs,
            [
                bg(0, 2, BLUE),
                bg(3, 2, BLUE),
                bg(5, 1, GREEN),
                bg(6, 1, BLUE)
            ]
        );
    }

    #[test]
    fn appends_without_extending_runs_from_earlier_calls() {
        let styles = [st(RED, Some(BLUE)), st(RED, None)];
        let (a, b) = (row(&[(0, "ab")], &[]), row(&[(1, "xy"), (0, "cd")], &[]));
        let (mut bgs, mut texts) = (Vec::new(), Vec::new());
        for r in [&a, &b] {
            batch_row(
                r,
                80,
                &mut |id| styles[usize::from(id)],
                &mut bgs,
                &mut texts,
            );
        }
        assert_eq!(bgs, [bg(0, 2, BLUE), bg(2, 2, BLUE)]);
        assert_eq!(pieces(&texts), [(0, 2, "ab"), (0, 2, "xy"), (2, 2, "cd")]);
    }

    #[test]
    fn space_only_pieces_skip_text_but_keep_bg_and_columns() {
        let plain = st(RED, None);
        let under = Resolved {
            underline: UnderlineKind::Single,
            ..plain
        };
        let struck = Resolved {
            strike: true,
            ..plain
        };
        let styles = [plain, st(RED, Some(BLUE)), under, struck];
        let r = row(
            &[
                (0, "  "),
                (1, "   "),
                (0, "x"),
                (2, "  "),
                (3, " "),
                (0, "a b"),
                (0, " \u{301}"),
            ],
            &[],
        );
        let (bgs, texts) = batch(&r, 80, &styles);
        assert_eq!(bgs, [bg(2, 3, BLUE)]);
        assert_eq!(
            pieces(&texts),
            [
                (5, 1, "x"),
                (6, 2, "  "),
                (8, 1, " "),
                (9, 3, "a b"),
                (12, 1, " \u{301}")
            ]
        );
    }

    #[test]
    fn wide_chars_get_their_own_two_column_piece() {
        let styles = [st(RED, Some(BLUE)), st(GREEN, None)];
        // a(0) 你(1-2) 好(3-4) b(5) | c(6): the spacer columns 2 and 4 hold no char.
        let r = row(&[(0, "a你好b"), (1, "c")], &[2, 4]);
        let (bgs, texts) = batch(&r, 80, &styles);
        assert_eq!(
            pieces(&texts),
            [
                (0, 1, "a"),
                (1, 2, "你"),
                (3, 2, "好"),
                (5, 1, "b"),
                (6, 1, "c")
            ]
        );
        assert_eq!(bgs, [bg(0, 6, BLUE)]);
        // Wideness comes from wide_cols only: without a spacer, 你 is one column.
        let r = row(&[(0, "你x")], &[]);
        assert_eq!(pieces(&batch(&r, 80, &styles).1), [(0, 2, "你x")]);
    }

    #[test]
    fn zero_width_chars_stay_with_their_base_char() {
        let styles = [st(RED, None), st(GREEN, None)];
        let r = row(
            &[
                (0, "e\u{301}x"),
                (1, "你\u{301}a"),
                (0, "\u{200D}"),
                (1, "\u{301}b"),
            ],
            &[3],
        );
        let (_, texts) = batch(&r, 80, &styles);
        assert_eq!(
            pieces(&texts),
            [
                (0, 2, "e\u{301}x"),
                (2, 2, "你\u{301}"),
                (4, 1, "a"),
                (5, 1, "b")
            ]
        );
    }

    #[test]
    fn hidden_text_draws_only_its_background() {
        let hidden = Resolved {
            hidden: true,
            ..st(RED, Some(BLUE))
        };
        let r = row(&[(0, "ab你"), (1, "c")], &[3]);
        let (bgs, texts) = batch(&r, 80, &[hidden, st(GREEN, None)]);
        assert_eq!(bgs, [bg(0, 4, BLUE)]);
        assert_eq!(pieces(&texts), [(4, 1, "c")]);
    }

    #[test]
    fn clipping_cuts_bg_and_text_at_max_cols() {
        let styles = [st(RED, Some(BLUE)), st(GREEN, Some(GREEN))];

        let r = row(&[(0, "abcdef"), (1, "gh")], &[]);
        let (bgs, texts) = batch(&r, 4, &styles);
        assert_eq!(bgs, [bg(0, 4, BLUE)]);
        assert_eq!(pieces(&texts), [(0, 4, "abcd")]);

        // Cut at a char boundary (é is two bytes), keeping the mark after the last kept char.
        let r = row(&[(0, "aé\u{301}xyz")], &[]);
        assert_eq!(pieces(&batch(&r, 2, &styles).1), [(0, 2, "aé\u{301}")]);

        // A wide char straddling the edge is dropped; its bg still covers the visible column.
        let r = row(&[(0, "ab你"), (1, "c")], &[3]);
        let (bgs, texts) = batch(&r, 3, &styles);
        assert_eq!(bgs, [bg(0, 3, BLUE)]);
        assert_eq!(pieces(&texts), [(0, 2, "ab")]);
        // One that ends exactly at the edge stays.
        assert_eq!(
            pieces(&batch(&r, 4, &styles).1),
            [(0, 2, "ab"), (2, 2, "你")]
        );

        // Nothing at or after the edge is resolved or emitted.
        let r = row(&[(0, "abc"), (1, "def")], &[]);
        let mut calls = Vec::new();
        let (mut bgs, mut texts) = (Vec::new(), Vec::new());
        let mut resolve = |id: u16| {
            calls.push(id);
            styles[usize::from(id)]
        };
        batch_row(&r, 3, &mut resolve, &mut bgs, &mut texts);
        assert_eq!(calls, [0]);
        assert_eq!(bgs, [bg(0, 3, BLUE)]);
        assert_eq!(pieces(&texts), [(0, 3, "abc")]);
        let (bgs, texts) = batch(&r, 0, &styles);
        assert!(bgs.is_empty() && texts.is_empty());
    }

    #[test]
    fn empty_row_pushes_nothing() {
        let empty = RowPatch::default();
        let mut calls = 0;
        let (mut bgs, mut texts) = (Vec::new(), Vec::new());
        let mut resolve = |_| {
            calls += 1;
            st(RED, Some(BLUE))
        };
        batch_row(&empty, 80, &mut resolve, &mut bgs, &mut texts);
        assert_eq!(calls, 0);
        assert!(bgs.is_empty() && texts.is_empty());
    }

    #[test]
    fn resolve_is_called_once_per_run() {
        let styles = [st(RED, None), st(GREEN, Some(BLUE)), st(BLUE, None)];
        let r = row(
            &[(1, "ab"), (0, ""), (2, "你"), (1, "\u{301}"), (1, "cd")],
            &[3],
        );
        let mut calls = Vec::new();
        let (mut bgs, mut texts) = (Vec::new(), Vec::new());
        let mut resolve = |id: u16| {
            calls.push(id);
            styles[usize::from(id)]
        };
        batch_row(&r, 80, &mut resolve, &mut bgs, &mut texts);
        assert_eq!(calls, [1, 0, 2, 1, 1]);
        assert_eq!(pieces(&texts), [(0, 2, "ab"), (2, 2, "你"), (4, 2, "cd")]);
    }

    #[test]
    fn resolve_applies_inverse_then_dim() {
        let p = Palette::femme_light();
        let (ink, paper) = (p.fg, p.bg);
        assert_eq!(p.resolve(&Style::default()), st(ink, None));

        let inverse = Style {
            attrs: Attrs::INVERSE,
            ..Style::default()
        };
        assert_eq!(p.resolve(&inverse), st(paper, Some(ink)));

        // DIM blends toward the bg in effect, after the swap; the bg itself is not dimmed.
        let dim = Style {
            attrs: Attrs::DIM,
            ..Style::default()
        };
        assert_eq!(p.resolve(&dim), st([142; 3], None));
        let red_on_blue = Style {
            fg: Color::Indexed(1),
            bg: Color::Indexed(4),
            attrs: Attrs::INVERSE | Attrs::DIM,
            ..Style::default()
        };
        let [r, g, b] = [0, 1, 2].map(|i| half(p.ansi[1][i], p.ansi[4][i]));
        assert_eq!(p.resolve(&red_on_blue), st([r, g, b], Some(p.ansi[1])));

        // A bg that happens to equal the paper draws no quad.
        let paper_bg = Style {
            bg: Color::Rgb(0xFE, 0xFE, 0xFE),
            ..Style::default()
        };
        assert_eq!(p.resolve(&paper_bg).bg, None);
    }

    #[test]
    fn resolve_maps_flags() {
        let p = Palette::femme_light();
        let red = Style {
            fg: Color::Indexed(1),
            ..Style::default()
        };
        let with = |attrs| p.resolve(&Style { attrs, ..red });
        let plain = st(p.ansi[1], None);
        // Bold keeps the normal colour: no bright promotion.
        assert_eq!(
            with(Attrs::BOLD),
            Resolved {
                bold: true,
                ..plain
            }
        );
        assert_eq!(
            with(Attrs::ITALIC),
            Resolved {
                italic: true,
                ..plain
            }
        );
        assert_eq!(
            with(Attrs::STRIKETHROUGH),
            Resolved {
                strike: true,
                ..plain
            }
        );
        assert_eq!(
            with(Attrs::HIDDEN),
            Resolved {
                hidden: true,
                ..plain
            }
        );
        assert_eq!(with(Attrs::BLINK), plain);
    }

    #[test]
    fn underline_kind_priority_and_color() {
        let p = Palette::femme_light();
        let kind = |attrs| {
            p.resolve(&Style {
                attrs,
                ..Style::default()
            })
            .underline
        };
        assert_eq!(kind(Attrs::empty()), UnderlineKind::None);
        assert_eq!(
            kind(Attrs::UNDERLINE | Attrs::DASHED_UNDERLINE),
            UnderlineKind::Single
        );
        assert_eq!(
            kind(Attrs::DOUBLE_UNDERLINE | Attrs::CURLY_UNDERLINE),
            UnderlineKind::Double
        );
        assert_eq!(
            kind(Attrs::CURLY_UNDERLINE | Attrs::DOTTED_UNDERLINE),
            UnderlineKind::Curly
        );
        assert_eq!(
            kind(Attrs::DOTTED_UNDERLINE | Attrs::DASHED_UNDERLINE),
            UnderlineKind::Dotted
        );
        assert_eq!(kind(Attrs::DASHED_UNDERLINE), UnderlineKind::Dashed);

        let under = Style {
            attrs: Attrs::UNDERLINE,
            ..Style::default()
        };
        assert_eq!(p.resolve(&under).underline_color, None);
        let blue = Style {
            underline: Color::Indexed(4),
            ..under
        };
        assert_eq!(p.resolve(&blue).underline_color, Some(p.ansi[4]));
        let rgb = Style {
            underline: Color::Rgb(1, 2, 3),
            ..under
        };
        assert_eq!(p.resolve(&rgb).underline_color, Some([1, 2, 3]));
    }

    #[test]
    fn color_maps_ansi_cube_grey_ramp_and_rgb() {
        let p = Palette::femme_light();
        let d = [1, 2, 3];
        assert_eq!(p.color(Color::Default, d), d);
        assert_eq!(p.color(Color::Indexed(1), d), p.ansi[1]);
        assert_eq!(p.color(Color::Indexed(15), d), p.ansi[15]);
        assert_eq!(p.color(Color::Indexed(16), d), [0, 0, 0]);
        assert_eq!(p.color(Color::Indexed(67), d), [95, 135, 175]);
        assert_eq!(p.color(Color::Indexed(196), d), [255, 0, 0]);
        assert_eq!(p.color(Color::Indexed(231), d), [255, 255, 255]);
        assert_eq!(p.color(Color::Indexed(232), d), [8, 8, 8]);
        assert_eq!(p.color(Color::Indexed(255), d), [238, 238, 238]);
        assert_eq!(p.color(Color::Rgb(9, 8, 7), d), [9, 8, 7]);
        for i in 0..=255 {
            p.color(Color::Indexed(i), d); // no overflow anywhere in the table
        }
    }

    #[test]
    fn femme_light_colours_read_on_paper() {
        let p = Palette::femme_light();
        assert_eq!((p.fg, p.bg), ([0x1E; 3], [0xFE; 3]));
        for i in (1..=6).chain(9..=14) {
            let c = contrast(p.ansi[i], p.bg);
            assert!(c >= 3.0, "ansi {i}: contrast {c:.2} on paper");
        }
        for i in 1..=6 {
            assert!(contrast(p.ansi[i], p.bg) >= 4.5, "ansi {i} below AA");
        }
        assert!(contrast(p.ansi[0], p.bg) >= 12.0, "black is not dark");
        assert!(contrast(p.ansi[8], p.bg) >= 4.5, "bright black is not dark");
        for i in [7, 15] {
            assert!(contrast(p.ansi[i], p.bg) >= 2.4, "white {i} vanishes");
        }
        assert!(contrast(p.ansi[15], p.bg) < contrast(p.ansi[7], p.bg));
    }
}
