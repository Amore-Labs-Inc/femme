//! What each [`Tone`] of a drawing is, in the FEMME palette: the screen paints with the theme
//! tokens directly, the PNG export with the same tokens as 8-bit RGBA. Nothing here is a colour of
//! its own: a shape's colour is a token's name (ink, pink accent, neon, success, warn, danger,
//! soft pink, panel grey) and this is the one place that says which.

use gpui::Rgba;

use super::scene::Tone;
use crate::theme::{
    self, ACCENT, ACCENT_DEEP, ACCENT_SOFT, DANGER, INK, NEON, PANEL, PAPER, SUCCESS, WARN,
};

/// The theme token of a tone.
pub fn token(tone: Tone) -> Rgba {
    match tone {
        Tone::Ink => INK,
        Tone::Pink => ACCENT,
        Tone::Neon => NEON,
        Tone::Success => SUCCESS,
        Tone::Warn => WARN,
        Tone::Danger => DANGER,
        Tone::Soft => ACCENT_SOFT,
        Tone::Panel => PANEL,
    }
}

/// The paper the board is drawn on, and the PNG's background.
pub fn paper() -> Rgba {
    PAPER
}

/// The faint dots of the grid: ink a long way towards paper.
pub fn grid() -> Rgba {
    theme::mix(PAPER, INK, 0.22)
}

/// The selection's box and the marquee: deep pink, which neither paper nor any stroke colour but
/// the pink accent resembles, and that one only by its dashes.
pub fn selection() -> Rgba {
    ACCENT_DEEP
}

/// A token as 8-bit `[r, g, b, a]`, for the export.
pub fn rgba8(c: Rgba) -> [u8; 4] {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [q(c.r), q(c.g), q(c.b), q(c.a)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tones_are_the_theme_tokens() {
        assert_eq!(rgba8(token(Tone::Ink)), [0x1E, 0x1E, 0x1E, 0xFF]);
        assert_eq!(rgba8(token(Tone::Soft)), [0xF6, 0xCD, 0xE6, 0xFF]);
        assert_eq!(rgba8(paper()), [0xFE, 0xFE, 0xFE, 0xFF]);
    }

    #[test]
    fn every_tone_is_distinct() {
        use std::collections::HashSet;
        let all = [
            Tone::Ink,
            Tone::Pink,
            Tone::Neon,
            Tone::Success,
            Tone::Warn,
            Tone::Danger,
            Tone::Soft,
            Tone::Panel,
        ];
        let set: HashSet<[u8; 4]> = all.iter().map(|t| rgba8(token(*t))).collect();
        assert_eq!(set.len(), all.len());
    }
}
