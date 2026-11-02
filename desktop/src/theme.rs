//! FEMME's retro skin: colour/size tokens from `board/src/index.html`, the embedded fonts, and the
//! mapping of both onto gpui-component's [`Theme`] so restyled stock widgets (resize handles,
//! scrollbars, buttons) wear the same palette. Light only.

use std::borrow::Cow;

use gpui::{point, px, App, BoxShadow, Hsla, Pixels, Rgba};
use gpui_component::Theme;

/// A colour from a `0xRRGGBB` literal (const so the tokens can be `const`).
pub const fn rgb8(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

/// A colour from `0xRRGGBB` plus an alpha in `0.0..=1.0`.
pub const fn rgba8(hex: u32, a: f32) -> Rgba {
    let c = rgb8(hex);
    Rgba { a, ..c }
}

/// The desk (page background, shows through the gutters between windows).
pub const DESK: Rgba = rgb8(0xF386A1);
/// Paper: card, input and terminal background.
pub const PAPER: Rgba = rgb8(0xFEFEFE);
/// Panel grey: dialogs, tab bars.
pub const PANEL: Rgba = rgb8(0xE5E5E5);
/// Ink: text, borders, title bars.
pub const INK: Rgba = rgb8(0x1E1E1E);
/// Muted text.
pub const MUTED: Rgba = rgb8(0x5C5C5C);
/// Hover/selection pink.
pub const HOVER: Rgba = rgb8(0xD45BB6);
/// Neon pink (wordmark, cursor).
pub const NEON: Rgba = rgb8(0xFF4FB4);
/// Lane: working.
pub const WORK: Rgba = rgb8(0x03AA5C);
/// Lane: paused / blocked.
pub const PAUSE: Rgba = rgb8(0xF2B33D);
/// Lane: queued.
pub const QUEUE: Rgba = rgb8(0xDEDEDE);
/// Lane: pull request.
pub const PR: Rgba = rgb8(0x09AEA1);
/// Lane: failed / error.
pub const BAD: Rgba = rgb8(0xD93036);
/// Lane: done / idle.
pub const DONE: Rgba = rgb8(0x858585);

/// `.win`, `.card`, `button` border width.
pub const RULE: Pixels = px(1.2);

/// Pixel font (title bars, buttons, status pills).
pub const FONT_PIXEL: &str = "VT323";
/// Monospace font: transcript code, terminal grid. The `NL` build has no ligatures, which a
/// character-cell grid needs.
pub const FONT_MONO: &str = "JetBrains Mono NL";
/// Body font.
pub const FONT_UI: &str = "Schibsted Grotesk";
/// Wordmark font.
pub const FONT_BRAND: &str = "Kaushan Script";

/// Title bar text size (`.tb`).
pub const TB_SIZE: Pixels = px(22.);
/// Button text size (`button`).
pub const BTN_SIZE: Pixels = px(20.);
/// Body text size.
pub const BODY_SIZE: Pixels = px(15.);
/// Small mono text (`.mini`, `.meta`).
pub const META_SIZE: Pixels = px(12.);
/// Terminal grid font size.
pub const TERM_SIZE: Pixels = px(13.);

/// The embedded font files, in the order `text_system().add_fonts` registers them.
fn font_data() -> Vec<Cow<'static, [u8]>> {
    macro_rules! font {
        ($p:literal) => {
            Cow::Borrowed(&include_bytes!(concat!("../assets/fonts/", $p))[..])
        };
    }
    vec![
        font!("vt323/VT323-Regular.ttf"),
        font!("schibsted-grotesk/SchibstedGrotesk-Variable.ttf"),
        font!("kaushan-script/KaushanScript-Regular.ttf"),
        font!("jetbrains-mono/JetBrainsMonoNL-Regular.ttf"),
        font!("jetbrains-mono/JetBrainsMonoNL-Bold.ttf"),
        font!("jetbrains-mono/JetBrainsMonoNL-Italic.ttf"),
        font!("jetbrains-mono/JetBrainsMonoNL-BoldItalic.ttf"),
    ]
}

/// The hard drop shadow of dialogs and the composer: `3px 3px 0 rgba(0,0,0,.4)`.
pub fn hard_shadow() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: Hsla::from(rgba8(0x000000, 0.4)),
        offset: point(px(3.), px(3.)),
        blur_radius: px(0.),
        spread_radius: px(0.),
        inset: false,
    }]
}

/// Registers the embedded fonts and installs the FEMME palette into gpui-component's global
/// [`Theme`]. Call once after `gpui_component::init`.
pub fn apply(cx: &mut App) {
    cx.text_system()
        .add_fonts(font_data())
        .expect("embedded fonts are valid TTFs");

    let h = |c: Rgba| Hsla::from(c);
    Theme::update(cx, |t| {
        t.font_family = FONT_UI.into();
        t.mono_font_family = FONT_MONO.into();
        t.font_size = BODY_SIZE;
        t.mono_font_size = TERM_SIZE;
        // Square corners everywhere, no soft shadows, no focus halo (the skin uses a 2px ink outline).
        t.radius = px(0.);
        t.radius_lg = px(0.);
        t.shadow = false;
        t.focus_ring = false;

        let c = &mut t.colors;
        c.background = h(PAPER);
        c.foreground = h(INK);
        c.border = h(INK);
        c.input = h(INK);
        c.ring = h(INK);
        c.caret = h(INK);
        c.selection = h(HOVER);
        c.muted = h(PANEL);
        c.muted_foreground = h(MUTED);
        c.accent = h(HOVER);
        c.accent_foreground = h(INK);
        c.primary = h(INK);
        c.primary_foreground = h(PAPER);
        c.primary_hover = h(HOVER);
        c.primary_active = h(NEON);
        c.secondary = h(PAPER);
        c.secondary_foreground = h(INK);
        c.secondary_hover = h(HOVER);
        c.secondary_active = h(NEON);
        c.danger = h(BAD);
        c.success = h(WORK);
        c.warning = h(PAUSE);
        c.info = h(PR);
        c.link = h(INK);
        c.popover = h(PANEL);
        c.popover_foreground = h(INK);
        c.sidebar = h(PANEL);
        c.sidebar_foreground = h(INK);
        c.sidebar_border = h(INK);
        c.title_bar = h(INK);
        c.title_bar_border = h(INK);
        c.tab = h(PAPER);
        c.tab_active = h(INK);
        c.tab_active_foreground = h(PAPER);
        c.tab_foreground = h(INK);
        c.tab_bar = h(PANEL);
        c.list_hover = h(HOVER);
        c.list_active = h(NEON);
        c.drag_border = h(NEON);
        c.scrollbar = Hsla::from(rgba8(0x000000, 0.0));
        c.scrollbar_thumb = Hsla::from(rgba8(0x1E1E1E, 0.55));
        c.scrollbar_thumb_hover = h(INK);
        c.overlay = Hsla::from(rgba8(0x000000, 0.25));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fonts_are_embedded() {
        let fonts = font_data();
        assert_eq!(fonts.len(), 7);
        // TrueType / OpenType magic.
        for f in &fonts {
            assert!(matches!(&f[..4], [0, 1, 0, 0] | b"OTTO" | b"true"));
        }
    }
}
