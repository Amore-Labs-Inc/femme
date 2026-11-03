//! Metrics of the body font (Schibsted Grotesk, the font the labels are drawn in on the screen and
//! in the exported PNG), so the scene can size a text without a window: the selection box of a
//! label, the room an export frames.

/// Line height of a text, as a multiple of its size.
pub const LINE_HEIGHT: f32 = 1.25;

/// Width of one line of text at `size`.
pub fn line_width(line: &str, size: f32) -> f32 {
    line.chars().count() as f32 * size * 0.52
}

/// Width of the widest line and the height of all the lines of `text` (lines split by `\n`) at
/// `size`. An empty text is as wide as a caret.
pub fn measure(text: &str, size: f32) -> (f32, f32) {
    let lines = text.split('\n');
    let height = lines.clone().count() as f32 * size * LINE_HEIGHT;
    let width = lines.map(|l| line_width(l, size)).fold(0.0, f32::max);
    (width.max(2.0), height)
}
