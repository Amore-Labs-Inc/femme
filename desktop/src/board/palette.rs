//! What each [`Tone`] of a drawing is, in the FEMME palette: the screen paints with the live
//! theme's tokens, the PNG export with the light palette's as 8-bit RGBA. Nothing here is a colour
//! of its own: a shape's colour is a token's name (ink, pink accent, neon, success, warn, danger,
//! soft pink, panel grey) and this is the one place that says which.

use gpui::Rgba;

use super::scene::Tone;
use crate::theme::{self, Palette, LIGHT};

/// What the PNG export draws with, whatever the theme on screen is: the light palette. A picture
/// goes to Claude or the clipboard, not to this window, so it looks the same from either theme (a
/// dark picture would also be the one that fails to read on a white page).
pub const EXPORT: &Palette = &LIGHT;

/// The colour of a tone in the palette `p`.
pub fn token_in(p: &Palette, tone: Tone) -> Rgba {
    match tone {
        Tone::Ink => p.ink,
        Tone::Pink => p.accent,
        Tone::Neon => p.neon,
        Tone::Success => p.success,
        Tone::Warn => p.warn,
        Tone::Danger => p.danger,
        Tone::Soft => p.accent_soft,
        Tone::Panel => p.panel,
    }
}

/// The colour of a tone on screen: the live theme's token.
pub fn token(tone: Tone) -> Rgba {
    token_in(theme::current(), tone)
}

/// The faint dots of the grid: ink a long way towards paper.
pub fn grid() -> Rgba {
    let p = theme::current();
    theme::mix(p.paper, p.ink, 0.22)
}

/// The selection's box and the marquee: deep pink, which neither paper nor any stroke colour but
/// the pink accent resembles, and that one only by its dashes.
pub fn selection() -> Rgba {
    theme::accent_deep()
}

/// A token as 8-bit `[r, g, b, a]`, for the export.
pub fn rgba8(c: Rgba) -> [u8; 4] {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [q(c.r), q(c.g), q(c.b), q(c.a)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::DARK;

    const ALL: [Tone; 8] = [
        Tone::Ink,
        Tone::Pink,
        Tone::Neon,
        Tone::Success,
        Tone::Warn,
        Tone::Danger,
        Tone::Soft,
        Tone::Panel,
    ];

    /// The tests read the palettes directly and never flip the process-wide theme.
    const BOTH: [(&str, &Palette); 2] = [("light", &LIGHT), ("dark", &DARK)];

    #[test]
    fn tones_are_the_theme_tokens() {
        assert_eq!(rgba8(token_in(&LIGHT, Tone::Ink)), [0x1E, 0x1E, 0x1E, 0xFF]);
        assert_eq!(
            rgba8(token_in(&LIGHT, Tone::Soft)),
            [0xF6, 0xCD, 0xE6, 0xFF]
        );
        assert_eq!(rgba8(LIGHT.paper), [0xFE, 0xFE, 0xFE, 0xFF]);
        // On screen they answer for the live palette.
        for tone in ALL {
            assert_eq!(token(tone), token_in(theme::current(), tone));
        }
    }

    #[test]
    fn the_export_is_light_whatever_the_theme() {
        assert_eq!(EXPORT.paper, LIGHT.paper);
        assert_eq!(token_in(EXPORT, Tone::Ink), LIGHT.ink);
    }

    #[test]
    fn every_tone_is_distinct_in_both_themes() {
        use std::collections::HashSet;
        for (name, p) in BOTH {
            let set: HashSet<[u8; 4]> = ALL.iter().map(|t| rgba8(token_in(p, *t))).collect();
            assert_eq!(set.len(), ALL.len(), "{name}");
        }
    }

    /// A stroke must not vanish on its own paper, and the ink must read on the fills. The dark
    /// palette holds every stroke to 3:1 (WCAG's floor for a graphic); the light one keeps what it
    /// always had (its amber is 1.84:1 on white).
    #[test]
    fn strokes_show_on_their_paper_and_ink_reads_on_the_fills() {
        for (name, p, floor) in [("light", &LIGHT, 1.8), ("dark", &DARK, 3.0)] {
            for tone in [
                Tone::Ink,
                Tone::Pink,
                Tone::Neon,
                Tone::Success,
                Tone::Warn,
                Tone::Danger,
            ] {
                let c = theme::contrast(token_in(p, tone), p.paper);
                assert!(c >= floor, "{name}: {tone:?} has contrast {c:.2} on paper");
            }
            for fill in [Tone::Soft, Tone::Panel] {
                let f = token_in(p, fill);
                assert!(
                    theme::contrast(f, p.paper) > 1.1,
                    "{name}: {fill:?} is paper"
                );
                let c = theme::contrast(p.ink, f);
                assert!(c >= 4.5, "{name}: ink on {fill:?} has contrast {c:.2}");
            }
        }
    }

    #[test]
    fn the_grid_is_faint_and_the_selection_is_pink() {
        for (name, p) in BOTH {
            let dots = theme::mix(p.paper, p.ink, 0.22);
            let c = theme::contrast(dots, p.paper);
            assert!((1.2..=3.0).contains(&c), "{name}: grid contrast {c:.2}");
            let sel = theme::contrast(p.accent_deep, p.paper);
            assert!(sel >= 3.0, "{name}: selection contrast {sel:.2}");
        }
    }
}
