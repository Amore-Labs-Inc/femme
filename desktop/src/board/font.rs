//! The body font of the board (Schibsted Grotesk, the face labels are drawn in on the screen) as
//! numbers and outlines. The numbers size a label (its selection box, the room an export frames);
//! the outlines let the PNG export draw the same text with no window and no GPU.
//!
//! The font file is embedded and parsed once. A line is laid out glyph after glyph by the advances
//! the font gives, so widths add up (the text editor can measure a prefix of a line to place its
//! caret). Kerning and ligatures are not applied: the screen's shaper does apply them, so a label
//! can differ from what is drawn on the screen by a pixel or two. The default instance of the
//! variable font is used, which is the Regular weight.

use std::sync::OnceLock;

use tiny_skia::PathBuilder;
use ttf_parser::{Face, GlyphId, OutlineBuilder};

/// Line height of a text, as a multiple of its size.
pub const LINE_HEIGHT: f32 = 1.25;

/// The font file, so the PNG never depends on what is installed on the machine.
const FONT: &[u8] =
    include_bytes!("../../assets/fonts/schibsted-grotesk/SchibstedGrotesk-Variable.ttf");

/// How many spaces a tab stands for.
const TAB_SPACES: u32 = 4;

/// Width of a char, in ems, when the font cannot be read: the estimate the board started with.
const FALLBACK_ADVANCE: f32 = 0.52;

/// Ascent in ems when the font cannot be read: this font's own, rounded.
const FALLBACK_ASCENT: f32 = 0.98;

/// Descent in ems when the font cannot be read: this font's own, rounded.
const FALLBACK_DESCENT: f32 = 0.26;

/// The font, parsed on first use. None if the embedded file were unreadable, in which case widths
/// are estimated and nothing is outlined, rather than the board panicking over a label.
fn face() -> Option<&'static Face<'static>> {
    static FACE: OnceLock<Option<Face<'static>>> = OnceLock::new();
    FACE.get_or_init(|| Face::parse(FONT, 0).ok()).as_ref()
}

/// Chars that take no room and show nothing: control characters (the `\r` of a Windows line
/// break, say) and the zero-width format characters (soft hyphen, joiners, line and paragraph
/// separators, bidi marks, variation selectors, the byte order mark). The font has no glyph for
/// them, and a box in their place would be wrong.
fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
        )
}

/// Walks the chars of one line. `ink` is called for every glyph that has something to draw, with
/// the face, the glyph and the pen's position along the line in ems; the result is the width of
/// the whole line in ems. Widths and outlines both come from here, so they cannot disagree.
fn walk(line: &str, ink: impl FnMut(&Face<'static>, GlyphId, f32)) -> f32 {
    walk_in(face(), line, ink)
}

/// [`walk`] in a given face: with none, every char is [`FALLBACK_ADVANCE`] wide and nothing is
/// drawn.
///
/// A tab is [`TAB_SPACES`] spaces and other invisible chars are skipped. A char the font has no
/// glyph for is drawn as the font's `.notdef` glyph (a box in this font, half an em wide): the
/// reader of the picture sees that something stood there that could not be drawn.
fn walk_in(
    face: Option<&Face<'static>>,
    line: &str,
    mut ink: impl FnMut(&Face<'static>, GlyphId, f32),
) -> f32 {
    let mut pen = 0.0;
    for c in line.chars() {
        let (c, times) = if c == '\t' { (' ', TAB_SPACES) } else { (c, 1) };
        if is_invisible(c) {
            continue;
        }
        let Some(face) = face else {
            pen += times as f32 * FALLBACK_ADVANCE;
            continue;
        };
        let glyph = face.glyph_index(c).unwrap_or(GlyphId(0));
        let advance = face
            .glyph_hor_advance(glyph)
            .map_or(0.5, |a| f32::from(a) / f32::from(face.units_per_em()));
        if !c.is_whitespace() {
            ink(face, glyph, pen);
        }
        pen += times as f32 * advance;
    }
    pen
}

/// Width of one line of text at `size`.
pub fn line_width(line: &str, size: f32) -> f32 {
    walk(line, |_, _, _| {}) * size
}

/// Width of the widest line and the height of all the lines of `text` (lines split by `\n`) at
/// `size`. An empty text is as wide as a caret.
pub fn measure(text: &str, size: f32) -> (f32, f32) {
    let lines = text.split('\n');
    let height = lines.clone().count() as f32 * size * LINE_HEIGHT;
    let width = lines.map(|l| line_width(l, size)).fold(0.0, f32::max);
    (width.max(2.0), height)
}

/// How far the tallest glyphs reach above the baseline at `size`: the font's ascender (the
/// typographic one when the font asks for it, as the screen's font stack does).
pub fn ascent(size: f32) -> f32 {
    face().map_or(FALLBACK_ASCENT, |f| {
        f32::from(f.ascender()) / f32::from(f.units_per_em())
    }) * size
}

/// How far the lowest glyphs reach below the baseline at `size`, as a positive number.
pub fn descent(size: f32) -> f32 {
    face().map_or(FALLBACK_DESCENT, |f| {
        -f32::from(f.descender()) / f32::from(f.units_per_em())
    }) * size
}

/// Distance from the top of a line to its baseline, for a line of pitch `size * LINE_HEIGHT`: the
/// leading the font leaves over is split evenly above and below the glyphs, which is how gpui
/// places the text it paints into a line box.
pub fn baseline(size: f32) -> f32 {
    (size * LINE_HEIGHT - (ascent(size) + descent(size))) / 2.0 + ascent(size)
}

/// Feeds a glyph's outline into a path: scaled from font units to pixels, moved to the pen and
/// flipped, since the font's y axis points up and a pixmap's points down.
struct Placer<'a> {
    path: &'a mut PathBuilder,
    /// Pen position (the glyph's origin on the baseline).
    x: f32,
    y: f32,
    /// Pixels per font unit.
    k: f32,
}

impl Placer<'_> {
    fn map(&self, x: f32, y: f32) -> (f32, f32) {
        (self.x + x * self.k, self.y - y * self.k)
    }
}

impl OutlineBuilder for Placer<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.map(x, y);
        self.path.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.map(x, y);
        self.path.line_to(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let ((x1, y1), (x, y)) = (self.map(x1, y1), self.map(x, y));
        self.path.quad_to(x1, y1, x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let ((x1, y1), (x2, y2), (x, y)) = (self.map(x1, y1), self.map(x2, y2), self.map(x, y));
        self.path.cubic_to(x1, y1, x2, y2, x, y);
    }

    fn close(&mut self) {
        self.path.close();
    }
}

/// Appends the outlines of every glyph of one line to `path`, the line starting at pen position
/// `x` with its baseline at `baseline_y` (pixmap coordinates, y down), at `size`. Fill the result
/// with the nonzero winding rule: the contours of one glyph wind in opposite directions where
/// they make a hole, and overlapping contours must merge. White space adds nothing to the path.
pub fn push_line(path: &mut PathBuilder, line: &str, x: f32, baseline_y: f32, size: f32) {
    walk(line, |face, glyph, pen| {
        let mut sink = Placer {
            path: &mut *path,
            x: x + pen * size,
            y: baseline_y,
            k: size / f32::from(face.units_per_em()),
        };
        face.outline_glyph(glyph, &mut sink);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn widths_are_the_advances_of_the_font_file() {
        let upem = f32::from(face().expect("the embedded font parses").units_per_em());
        // At a size of one em in font units a width is the advance itself.
        assert_eq!(line_width("i", upem), 496.0);
        assert_eq!(line_width("M", upem), 1778.0);
        assert_eq!(line_width("iM", upem), 496.0 + 1778.0);
        assert_eq!(line_width(" ", upem), 520.0);
    }

    #[test]
    fn a_proportional_font_has_narrow_and_wide_letters() {
        assert!(line_width("iiii", 20.0) < line_width("MMMM", 20.0));
        assert!(line_width("iiii", 20.0) < line_width("abcd", 20.0));
        assert!(line_width("hello", 20.0) < line_width("hello!", 20.0));
        assert!(line_width("hello", 20.0) > line_width("hell", 20.0));
    }

    #[test]
    fn width_is_linear_in_the_size() {
        for text in ["hello", "MMMM", "a b\tc", ""] {
            let at20 = line_width(text, 20.0);
            let at40 = line_width(text, 40.0);
            assert!(near(at40, 2.0 * at20), "{text:?}: {at20} vs {at40}");
            assert!(near(line_width(text, 0.0), 0.0));
        }
        // Real sizes: 20 and 32 scene units.
        assert!(near(
            line_width("hello", 32.0) / line_width("hello", 20.0),
            1.6
        ));
    }

    #[test]
    fn a_text_measures_as_its_widest_line_times_its_lines() {
        let (w1, h1) = measure("hello", 20.0);
        let (w2, h2) = measure("hello\nhi", 20.0);
        assert!(near(w1, line_width("hello", 20.0)));
        assert!(near(h1, 20.0 * LINE_HEIGHT));
        assert!(near(w2, w1), "the wider line sets the width");
        assert!(near(h2, 2.0 * h1));
        let (w3, _) = measure("hi\nhello world", 20.0);
        assert!(near(w3, line_width("hello world", 20.0)));
        // A trailing newline starts a new, empty line.
        assert!(near(measure("a\n", 20.0).1, 2.0 * h1));
    }

    #[test]
    fn an_empty_text_is_a_caret_wide_and_one_line_high() {
        assert_eq!(measure("", 20.0).0, 2.0);
        assert!(near(measure("", 20.0).1, 25.0));
        assert_eq!(
            measure("\u{1}", 20.0).0,
            2.0,
            "nothing visible is a caret too"
        );
    }

    #[test]
    fn a_tab_is_four_spaces_and_control_chars_take_no_room() {
        assert!(near(line_width("\t", 20.0), 4.0 * line_width(" ", 20.0)));
        assert!(near(line_width("a\tb", 20.0), line_width("a    b", 20.0)));
        assert!(near(line_width("a\u{1}b\r", 20.0), line_width("ab", 20.0)));
        // Zero-width format characters too: a joiner, a variation selector, the BOM.
        assert!(near(
            line_width("a\u{200D}b\u{FE0F}\u{FEFF}", 20.0),
            line_width("ab", 20.0)
        ));
        let mut path = PathBuilder::new();
        push_line(&mut path, "\u{200D}\r\t", 0.0, 10.0, 20.0);
        assert!(path.finish().is_none(), "invisible chars have no outline");
    }

    #[test]
    fn without_a_font_widths_fall_back_to_the_old_estimate_and_nothing_is_drawn() {
        // Four letters, a tab (four spaces) and a control char (nothing): 4 + 4 chars.
        let em = walk_in(None, "a\tbc\u{1}d", |_, _, _| panic!("nothing to draw"));
        assert!(near(em, 8.0 * FALLBACK_ADVANCE), "{em}");
        assert_eq!(walk_in(None, "", |_, _, _| {}), 0.0);
    }

    #[test]
    fn a_char_without_a_glyph_is_half_an_em_wide_and_drawn_as_a_box() {
        // The font has no CJK and no emoji.
        for c in ['\u{4e2d}', '\u{1F600}'] {
            assert!(face().unwrap().glyph_index(c).is_none());
            assert!(near(line_width(&c.to_string(), 20.0), 10.0), "{c:?}");
            let mut path = PathBuilder::new();
            push_line(&mut path, &c.to_string(), 0.0, 20.0, 20.0);
            let b = path.finish().expect("a .notdef box").bounds();
            assert!(b.right() <= 10.0 + 1e-3 && b.bottom() <= 20.0 + 1e-3);
        }
    }

    #[test]
    fn the_baseline_sits_between_the_ascent_and_the_bottom_of_the_line_box() {
        let size = 20.0;
        // Schibsted Grotesk: ascender 2000, descender -528 on 2048 units per em.
        assert!(near(ascent(size), 2000.0 / 2048.0 * size));
        assert!(near(descent(size), 528.0 / 2048.0 * size));
        let b = baseline(size);
        assert!(near(b, 19.6875), "the leading is split evenly: {b}");
        assert!(
            b >= ascent(size),
            "no glyph pokes out of the top of the line"
        );
        assert!(
            b + descent(size) <= size * LINE_HEIGHT,
            "nor out of the bottom"
        );
        assert!(near(baseline(32.0) / baseline(20.0), 1.6));
    }

    #[test]
    fn an_outline_is_scaled_flipped_and_placed_on_the_baseline() {
        // `H` stands on the baseline and is as tall as the cap height (1440 of 2048 units).
        let (x, baseline_y, size) = (10.0, 100.0, 100.0);
        let mut path = PathBuilder::new();
        push_line(&mut path, "H", x, baseline_y, size);
        let b = path.finish().expect("H has an outline").bounds();
        assert!(near(b.bottom(), baseline_y), "bottom {}", b.bottom());
        let cap = 1440.0 / 2048.0 * size;
        assert!(
            near(b.top(), baseline_y - cap),
            "top {} wanted {}",
            b.top(),
            baseline_y - cap
        );
        assert!(b.left() > x && b.right() < x + line_width("H", size));
        // The next glyph starts one advance further along.
        let mut two = PathBuilder::new();
        push_line(&mut two, "HH", x, baseline_y, size);
        let b2 = two.finish().unwrap().bounds();
        assert!(near(b2.left(), b.left()));
        assert!(near(b2.right(), b.right() + line_width("H", size)));
    }

    #[test]
    fn white_space_and_empty_lines_draw_nothing() {
        for line in ["", " ", "   \t ", "\u{00A0}"] {
            let mut path = PathBuilder::new();
            push_line(&mut path, line, 0.0, 10.0, 20.0);
            assert!(path.finish().is_none(), "{line:?}");
        }
    }
}
