//! FEMME's retro skin: colour/size tokens from `assets/index.html` (the web board), the embedded
//! fonts, and the mapping of both onto gpui-component's [`Theme`] so stock widgets (inputs, menus,
//! popovers, dialogs, scrollbars, resize handles) wear the same palette. Light only.
//!
//! The skin in one paragraph: the window background (the *desk*) is pink with an ink dot grid and
//! shows only as a thin gutter around the panels; every panel and the centre are *paper*, with the
//! *panel grey* for secondary surfaces (tab strips, the composer's attachment row, muted rows);
//! text and 1.2 px rules are *ink*; hover fills are the FEMME pink ([`HOVER`]) and *neon* is kept
//! for the pressed state, the focus ring and the dot of a working session. Never hard-code a colour
//! outside this file: add a token here and use it.
//!
//! Contrast, measured with [`contrast`] (the unit tests below pin every pair a screen relies on):
//! ink on paper 16.5, ink on [`HOVER`] 4.74, paper on [`ACCENT_DEEP`] 5.5. White on [`HOVER`] is only
//! 3.5, so a hover fill keeps the label's ink colour; it also keeps gpui from showing a stale label
//! colour (gpui takes a label's hover colour from the last mouse move but the fill from the live hit
//! test, so after a key press under a resting pointer a label that turns white sits on paper).

use std::borrow::Cow;

use gpui::{point, px, App, BoxShadow, Hsla, Pixels, Rgba};
use gpui_component::{theme::ThemeColor, Theme};

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

/// `a` blended towards `b` by `t` (`0.0` is `a`, `1.0` is `b`), opaque. Used for hover/active
/// variants that are a shade of a base colour rather than a token of their own.
pub const fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.0,
    }
}

/// WCAG relative luminance of an opaque colour (alpha is ignored), `0.0` black to `1.0` white.
pub fn luminance(c: Rgba) -> f32 {
    let lin = |v: f32| {
        if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// WCAG contrast ratio of two opaque colours, `1.0` (identical) to `21.0` (black on white). Body text
/// needs 4.5, large text and UI edges 3.0.
pub fn contrast(a: Rgba, b: Rgba) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// The more readable of [`INK`] and [`PAPER`] as text on a `bg` fill (status pills, solid dots).
pub fn text_on(bg: Rgba) -> Rgba {
    if contrast(INK, bg) >= contrast(PAPER, bg) {
        INK
    } else {
        PAPER
    }
}

// ---------------------------------------------------------------------------------------------
// The palette: the web board's `:root` custom properties, same names, same values.
// ---------------------------------------------------------------------------------------------

/// The desk (`--desk`): pink behind everything. With the ink dot grid ([`crate::ui::DotGrid`]) it is
/// the window background and shows only in the gutters around panels ([`GUTTER`]) and on title-bar
/// lettering; a work surface is never desk-coloured.
pub const DESK: Rgba = rgb8(0xF386A1);
/// Paper (`--paper`): the work surface. Panels, the centre, inputs, cards and the terminal.
pub const PAPER: Rgba = rgb8(0xFEFEFE);
/// Panel grey (`--panel`): secondary surfaces on paper (tab strips, the composer's attachment row,
/// muted rows, dialogs, popovers).
pub const PANEL: Rgba = rgb8(0xE5E5E5);
/// Ink (`--ink`): text, borders, title bars, shadows.
pub const INK: Rgba = rgb8(0x1E1E1E);
/// Readable secondary text (`--muted`): meta lines, placeholders. 6.6:1 on paper, 5.3:1 on panel grey.
/// For disabled text use [`MUTED_TEXT`].
pub const MUTED: Rgba = rgb8(0x5C5C5C);
/// FEMME pink (`--hover`): hover fills, the selected row, text selection. Label colour on it stays
/// [`INK`] (4.7:1); see [`ON_ACCENT`].
pub const HOVER: Rgba = rgb8(0xD45BB6);
/// Neon pink (`--neon`): the pressed/active state of a control, the focus ring ([`FOCUS_RING`]), the
/// dot of a working session, the wordmark. Never a resting fill, and never text on paper (3.0:1).
pub const NEON: Rgba = rgb8(0xFF4FB4);
/// Lane: working (`--work`). Green; a fill or a dot, not text on paper (3.0:1).
pub const WORK: Rgba = rgb8(0x03AA5C);
/// Lane: paused / blocked (`--pause`). Yellow; a fill or a dot, never text on paper (1.8:1).
pub const PAUSE: Rgba = rgb8(0xF2B33D);
/// Lane: queued (`--queue`).
pub const QUEUE: Rgba = rgb8(0xDEDEDE);
/// Lane: pull request (`--pr`). Teal; also the `info` colour of gpui-component widgets.
pub const PR: Rgba = rgb8(0x09AEA1);
/// Lane: failed / error (`--bad`). 4.7:1 on paper, so usable as text.
pub const BAD: Rgba = rgb8(0xD93036);
/// Lane: done / idle (`--done`).
pub const DONE: Rgba = rgb8(0x858585);

// ---------------------------------------------------------------------------------------------
// Semantic tokens: what a colour is *for*. New code reaches for these; the palette names above stay
// because the existing screens use them.
// ---------------------------------------------------------------------------------------------

/// Work surface: panels, the centre, inputs, cards (paper).
pub const SURFACE: Rgba = PAPER;
/// Secondary surface: tab strips, the composer's attachment row, muted rows, dialogs (panel grey).
pub const SURFACE_MUTED: Rgba = PANEL;
/// Border colour of every box: ink, [`RULE`] wide.
pub const BORDER: Rgba = INK;
/// Colour of the hard (zero-blur) shadow under buttons and cards; see [`shadow`].
pub const SHADOW: Rgba = INK;
/// Hover pink: the fill of a hovered control or row, and of the selected row (same as [`HOVER`]).
pub const ACCENT: Rgba = HOVER;
/// Strong accent (neon): the pressed/active state and the focus ring (same as [`NEON`]).
pub const ACCENT_STRONG: Rgba = NEON;
/// Soft pink: a light wash for a hovered row inside a list whose selected row is [`ACCENT`]; ink on
/// it is 10:1. `#F6CDE6`.
pub const ACCENT_SOFT: Rgba = rgb8(0xF6CDE6);
/// Deep pink: the hover fill of a *dark* control (ink fill, paper label), where the label stays paper:
/// paper on it is 5.5:1. Also the colour of a hovered link.
pub const ACCENT_DEEP: Rgba = rgb8(0xB53094);
/// Label colour on an [`ACCENT`] fill: ink (4.7:1; white would be 3.5:1, below AA).
pub const ON_ACCENT: Rgba = INK;
/// Text that is allowed to be faint: disabled labels, disabled borders (3.7:1 on paper). Readable
/// secondary text is [`MUTED`].
pub const MUTED_TEXT: Rgba = rgb8(0x858585);
/// Success: done, connected, "working" lane green (same as [`WORK`]).
pub const SUCCESS: Rgba = WORK;
/// Warning: blocked, asks the user something (same as [`PAUSE`]).
pub const WARN: Rgba = PAUSE;
/// Danger: failed, destructive (same as [`BAD`]).
pub const DANGER: Rgba = BAD;
/// Keyboard focus ring colour (neon, drawn [`FOCUS_RING_WIDTH`] wide). gpui-component widgets show
/// focus by tinting their own border with it (see [`apply`]).
pub const FOCUS_RING: Rgba = NEON;
/// Focus ring thickness.
pub const FOCUS_RING_WIDTH: Pixels = px(2.);

/// Border width of every ruled box (`.win`, `.card`, `button`: 1.2 px ink).
pub const RULE: Pixels = px(1.2);
/// The pink gutter between panels and the window edge: thin, so the desk reads as a margin.
pub const GUTTER: Pixels = px(6.);
/// Height of the top bar (the `femme` button and the link status).
pub const TOP_BAR_H: Pixels = px(36.);
/// Width of the right-hand hover strip.
pub const STRIP_W: Pixels = px(28.);

// ---------------------------------------------------------------------------------------------
// Type
// ---------------------------------------------------------------------------------------------

/// Pixel font (title bars, buttons, status pills, icons). 22 px as a title ([`TITLE_SIZE`]), 20 px in
/// a button ([`BTN_SIZE`]).
pub const FONT_PIXEL: &str = "VT323";
/// Monospace font: transcript code, terminal grid. The `NL` build has no ligatures, which a
/// character-cell grid needs.
pub const FONT_MONO: &str = "JetBrains Mono NL";
/// Body font.
pub const FONT_UI: &str = "Schibsted Grotesk";
/// Wordmark font.
pub const FONT_BRAND: &str = "Kaushan Script";

/// Title bar text size (`.tb`), VT323.
pub const TB_SIZE: Pixels = px(22.);
/// Title text size: the same 22 px VT323 as [`TB_SIZE`], named for what it is.
pub const TITLE_SIZE: Pixels = TB_SIZE;
/// Button text size (`button`), VT323.
pub const BTN_SIZE: Pixels = px(20.);
/// Body text size, Schibsted Grotesk.
pub const BODY_SIZE: Pixels = px(14.);
/// Body line height as a multiple of the text size.
pub const BODY_LINE: f32 = 1.4;
/// Code and tool-output text size in the transcript and viewers, JetBrains Mono.
pub const MONO_SIZE: Pixels = px(13.);
/// Small mono text (`.mini`, `.meta`).
pub const META_SIZE: Pixels = px(12.);
/// Terminal grid font size (same as [`MONO_SIZE`]).
pub const TERM_SIZE: Pixels = MONO_SIZE;

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

/// The small hard shadow of buttons and cards: `2px 2px 0` in solid [`SHADOW`] ink. A pressed
/// button drops it and moves by the same 2 px, as if pushed into the page.
pub fn shadow() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: Hsla::from(SHADOW),
        offset: point(px(2.), px(2.)),
        blur_radius: px(0.),
        spread_radius: px(0.),
        inset: false,
    }]
}

/// A colour for gpui-component's [`Hsla`] fields.
fn h(c: Rgba) -> Hsla {
    Hsla::from(c)
}

/// Every colour of gpui-component's theme, from the FEMME tokens. A struct literal without
/// `..Default::default()` on purpose: a field gpui-component adds becomes a compile error here
/// instead of a stray shadcn blue on screen, and the test below checks the result holds no blue.
fn palette() -> ThemeColor {
    let clear = h(rgba8(0xFEFEFE, 0.0));
    // A shade of `c` towards ink: the hover and active variants of a coloured button.
    let hover = |c: Rgba| h(mix(c, INK, 0.12));
    let active = |c: Rgba| h(mix(c, INK, 0.26));
    ThemeColor {
        // Surfaces and text.
        background: h(SURFACE),
        foreground: h(INK),
        border: h(BORDER),
        input: h(BORDER),
        // gpui-component shows focus by tinting the border with `ring` (the ring itself is off, see
        // `apply`), so a focused input turns neon.
        ring: h(FOCUS_RING),
        caret: h(INK),
        selection: h(HOVER),
        muted: h(SURFACE_MUTED),
        muted_foreground: h(MUTED),
        accent: h(ACCENT),
        accent_foreground: h(ON_ACCENT),
        overlay: h(rgba8(0x1E1E1E, 0.4)),
        window_border: h(INK),
        // Menus, popovers, tooltips, select lists.
        popover: h(SURFACE_MUTED),
        popover_foreground: h(INK),
        // Plain Button.
        button: h(SURFACE),
        button_hover: h(ACCENT),
        button_active: h(ACCENT_STRONG),
        button_foreground: h(INK),
        // Primary: the dark, ink-filled button. Its hover keeps the paper label, so the fill goes deep.
        primary: h(INK),
        primary_hover: h(ACCENT_DEEP),
        primary_active: h(ACCENT_STRONG),
        primary_foreground: h(PAPER),
        button_primary: h(INK),
        button_primary_hover: h(ACCENT_DEEP),
        button_primary_active: h(ACCENT_STRONG),
        button_primary_foreground: h(PAPER),
        // Secondary: panel grey.
        secondary: h(SURFACE_MUTED),
        secondary_hover: h(ACCENT),
        secondary_active: h(ACCENT_STRONG),
        secondary_foreground: h(INK),
        button_secondary: h(SURFACE_MUTED),
        button_secondary_hover: h(ACCENT),
        button_secondary_active: h(ACCENT_STRONG),
        button_secondary_foreground: h(INK),
        // Status colours: a solid lane colour, a shade darker under the pointer and pressed. The label
        // is whichever of ink and paper reads better on the fill.
        danger: h(DANGER),
        danger_hover: hover(DANGER),
        danger_active: active(DANGER),
        danger_foreground: h(text_on(DANGER)),
        button_danger: h(DANGER),
        button_danger_hover: hover(DANGER),
        button_danger_active: active(DANGER),
        button_danger_foreground: h(text_on(DANGER)),
        success: h(SUCCESS),
        success_hover: hover(SUCCESS),
        success_active: active(SUCCESS),
        success_foreground: h(text_on(SUCCESS)),
        button_success: h(SUCCESS),
        button_success_hover: hover(SUCCESS),
        button_success_active: active(SUCCESS),
        button_success_foreground: h(text_on(SUCCESS)),
        warning: h(WARN),
        warning_hover: hover(WARN),
        warning_active: active(WARN),
        warning_foreground: h(text_on(WARN)),
        button_warning: h(WARN),
        button_warning_hover: hover(WARN),
        button_warning_active: active(WARN),
        button_warning_foreground: h(text_on(WARN)),
        info: h(PR),
        info_hover: hover(PR),
        info_active: active(PR),
        info_foreground: h(text_on(PR)),
        button_info: h(PR),
        button_info_hover: hover(PR),
        button_info_active: active(PR),
        button_info_foreground: h(text_on(PR)),
        // Links: ink, deep pink under the pointer.
        link: h(INK),
        link_hover: h(ACCENT_DEEP),
        link_active: h(INK),
        // Lists and tables: a soft wash on hover, the full pink and an ink edge on the selected row.
        list: h(SURFACE),
        list_even: h(SURFACE),
        list_head: h(SURFACE_MUTED),
        list_hover: h(ACCENT_SOFT),
        list_active: h(ACCENT),
        list_active_border: h(INK),
        table: h(SURFACE),
        table_even: h(SURFACE),
        table_head: h(SURFACE_MUTED),
        table_head_foreground: h(INK),
        table_foot: h(SURFACE_MUTED),
        table_foot_foreground: h(INK),
        table_hover: h(ACCENT_SOFT),
        table_active: h(ACCENT),
        table_active_border: h(INK),
        table_row_border: h(SURFACE_MUTED),
        // Tabs: the selected tab is the dark button.
        tab: clear,
        tab_foreground: h(INK),
        tab_active: h(INK),
        tab_active_foreground: h(PAPER),
        tab_bar: h(SURFACE_MUTED),
        tab_bar_segmented: h(SURFACE_MUTED),
        // Sidebar, title and status bars.
        sidebar: h(SURFACE_MUTED),
        sidebar_foreground: h(INK),
        sidebar_border: h(INK),
        sidebar_accent: h(ACCENT),
        sidebar_accent_foreground: h(ON_ACCENT),
        sidebar_primary: h(INK),
        sidebar_primary_foreground: h(PAPER),
        title_bar: h(INK),
        title_bar_border: h(INK),
        status_bar: h(SURFACE_MUTED),
        status_bar_border: h(INK),
        // Scrollbars: no track, an ink thumb (the hover/drag colour is the full ink).
        scrollbar: clear,
        scrollbar_thumb: h(rgba8(0x1E1E1E, 0.55)),
        scrollbar_thumb_hover: h(INK),
        // Dragging a resize handle or a file over a drop target.
        drag_border: h(ACCENT_STRONG),
        drop_target: h(rgba8(0xD45BB6, 0.25)),
        // Small controls.
        accordion: h(SURFACE),
        group_box: h(SURFACE_MUTED),
        group_box_foreground: h(INK),
        description_list_label: h(SURFACE_MUTED),
        description_list_label_foreground: h(INK),
        progress_bar: h(INK),
        slider_bar: h(INK),
        slider_thumb: h(PAPER),
        switch: h(SURFACE_MUTED),
        switch_thumb: h(PAPER),
        skeleton: h(SURFACE_MUTED),
        // Charts: pink, ink, teal and yellow; never the stock blue ramp.
        chart_1: h(ACCENT_STRONG),
        chart_2: h(ACCENT),
        chart_3: h(INK),
        chart_4: h(PR),
        chart_5: h(PAUSE),
        chart_bullish: h(SUCCESS),
        chart_bearish: h(DANGER),
        chart_grid: h(SURFACE_MUTED),
        // The base hues (syntax and chart helpers): blue and magenta are taken from the FEMME pinks and
        // teal so that "blue" never shows up anywhere.
        red: h(DANGER),
        red_light: h(mix(DANGER, PAPER, 0.35)),
        green: h(SUCCESS),
        green_light: h(mix(SUCCESS, PAPER, 0.35)),
        blue: h(PR),
        blue_light: h(mix(PR, PAPER, 0.35)),
        yellow: h(WARN),
        yellow_light: h(mix(WARN, PAPER, 0.35)),
        magenta: h(ACCENT),
        magenta_light: h(mix(ACCENT, PAPER, 0.35)),
        cyan: h(PR),
        cyan_light: h(mix(PR, PAPER, 0.35)),
    }
}

/// Registers the embedded fonts and installs the FEMME palette into gpui-component's global
/// [`Theme`]. Call once after `gpui_component::init`.
///
/// Stock widgets then render as the skin: paper fills, ink 1 px borders, pink hover and selection,
/// neon on focus (a focused input turns its border neon), square corners (`radius` 0), no soft
/// shadows, ink scrollbar thumbs and resize handles that turn neon while dragged. Popups
/// (`PopupMenu`, `Popover`, `Select`) keep gpui-component's hairline edge and soft shadow, which the
/// theme cannot change: build a dropdown from [`crate::ui::menu`] when the FEMME box is wanted.
pub fn apply(cx: &mut App) {
    cx.text_system()
        .add_fonts(font_data())
        .expect("embedded fonts are valid TTFs");

    Theme::update(cx, |t| {
        t.font_family = FONT_UI.into();
        t.mono_font_family = FONT_MONO.into();
        t.font_size = BODY_SIZE;
        t.mono_font_size = MONO_SIZE;
        // Square corners everywhere, no soft shadows. The focus ring stays off: gpui-component then
        // tints a focused control's own border with `ring` (neon) instead of painting a halo that
        // every `overflow_hidden` ancestor would cut off.
        t.radius = px(0.);
        t.radius_lg = px(0.);
        t.shadow = false;
        t.focus_ring = false;
        t.colors = palette();
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

    #[test]
    fn contrast_follows_wcag() {
        let black = rgb8(0x000000);
        let white = rgb8(0xFFFFFF);
        assert!((contrast(black, white) - 21.0).abs() < 0.01);
        assert!((contrast(white, white) - 1.0).abs() < 1e-6);
        // Order does not matter.
        assert_eq!(contrast(INK, PAPER), contrast(PAPER, INK));
        // Known values: ink on paper is 16.5:1.
        assert!((contrast(INK, PAPER) - 16.53).abs() < 0.05);
    }

    /// Every text/fill pair a screen relies on is AA (4.5:1) readable.
    #[test]
    fn text_is_readable_on_its_fills() {
        let pairs: &[(&str, Rgba, Rgba)] = &[
            ("ink on paper", INK, PAPER),
            ("ink on panel grey", INK, PANEL),
            ("ink on hover pink", ON_ACCENT, ACCENT),
            ("ink on soft pink", INK, ACCENT_SOFT),
            ("paper on ink (dark button)", PAPER, INK),
            ("paper on deep pink (dark button hover)", PAPER, ACCENT_DEEP),
            ("ink on neon (pressed)", INK, NEON),
            ("desk pink on ink (title bar)", DESK, INK),
            ("muted on paper", MUTED, PAPER),
            ("muted on panel grey", MUTED, PANEL),
            ("danger red on paper", DANGER, PAPER),
            ("paper on danger red", PAPER, DANGER),
            ("ink on success green", INK, SUCCESS),
            ("ink on warning yellow", INK, WARN),
            ("ink on teal", INK, PR),
            ("ink on queued grey", INK, QUEUE),
            ("ink on done grey", INK, DONE),
        ];
        for (name, fg, bg) in pairs {
            let c = contrast(*fg, *bg);
            assert!(c >= 4.5, "{name}: {c:.2}:1 is below 4.5:1");
        }
    }

    /// `text_on` always picks a readable label, for every fill the skin paints.
    #[test]
    fn text_on_picks_a_readable_label() {
        for bg in [
            DESK,
            PAPER,
            PANEL,
            INK,
            HOVER,
            NEON,
            ACCENT_SOFT,
            ACCENT_DEEP,
            WORK,
            PAUSE,
            QUEUE,
            PR,
            BAD,
            DONE,
        ] {
            let fg = text_on(bg);
            assert!(contrast(fg, bg) >= 3.0, "{bg:?}");
        }
        assert_eq!(text_on(INK), PAPER);
        assert_eq!(text_on(PAPER), INK);
        assert_eq!(text_on(BAD), PAPER);
    }

    /// The documented caveat: white on the hover pink is below AA, which is why a hover fill keeps an
    /// ink label. If the pink is ever retuned this test says the doc comments need to follow.
    #[test]
    fn white_on_hover_pink_is_not_aa() {
        let white = rgb8(0xFFFFFF);
        assert!(contrast(white, HOVER) < 4.5);
        assert!(contrast(INK, HOVER) >= 4.5);
    }

    /// Hue in degrees of an opaque colour and its HSL saturation.
    fn hue_sat(c: Rgba) -> (f32, f32) {
        let hsla = Hsla::from(c);
        (hsla.h * 360.0, hsla.s)
    }

    /// Blue is `190..=270` degrees (cyan-blue through violet) at any visible saturation. Teal (175
    /// degrees) and the pinks (315-345) are fine.
    fn is_blue(h: f32, s: f32) -> bool {
        (190.0..=270.0).contains(&h) && s > 0.15
    }

    #[test]
    fn no_token_is_blue() {
        for (name, c) in [
            ("DESK", DESK),
            ("PAPER", PAPER),
            ("PANEL", PANEL),
            ("INK", INK),
            ("MUTED", MUTED),
            ("HOVER", HOVER),
            ("NEON", NEON),
            ("WORK", WORK),
            ("PAUSE", PAUSE),
            ("QUEUE", QUEUE),
            ("PR", PR),
            ("BAD", BAD),
            ("DONE", DONE),
            ("ACCENT_SOFT", ACCENT_SOFT),
            ("ACCENT_DEEP", ACCENT_DEEP),
            ("MUTED_TEXT", MUTED_TEXT),
        ] {
            let (h, s) = hue_sat(c);
            assert!(
                !is_blue(h, s),
                "{name} is blue (hue {h:.0}, saturation {s:.2})"
            );
        }
        assert!(is_blue(220.0, 0.9), "the check itself spots blue");
        assert!(!is_blue(175.0, 0.9), "teal is not blue");
    }

    /// `(field name, hue in degrees, saturation, alpha)` of every colour field of `ThemeColor`, read from
    /// its `Debug` output (the struct has no iterator and this crate has no serde).
    fn fields(c: &ThemeColor) -> Vec<(String, f32, f32, f32, f32)> {
        let dbg = format!("{c:?}");
        let mut out = Vec::new();
        for (end, _) in dbg.match_indices(": Hsla { h: ") {
            let start = dbg[..end]
                .rfind(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .map_or(0, |i| i + 1);
            let name = dbg[start..end].to_string();
            let body = &dbg[end + ": Hsla { ".len()..];
            let body = &body[..body.find('}').expect("Hsla closes")];
            let mut vals = [0f32; 4];
            for (i, part) in body.split(',').enumerate() {
                let (_, v) = part.split_once(':').expect("name: value");
                vals[i] = v.trim().parse().expect("a float");
            }
            out.push((name, vals[0] * 360.0, vals[1], vals[2], vals[3]));
        }
        out
    }

    #[test]
    fn palette_sets_every_field_and_none_is_blue() {
        let colors = palette();
        let f = fields(&colors);
        // The parser found the whole struct (gpui-component 0.8.1 has 137 colour fields).
        assert!(f.len() >= 130, "parsed only {} fields", f.len());
        for (name, hue, sat, lightness, alpha) in f {
            // A field nobody set is the all-zero default: transparent black.
            assert!(
                alpha != 0.0 || lightness != 0.0 || sat != 0.0 || hue != 0.0,
                "{name} was left at the default"
            );
            // Transparent colours have no colour to speak of.
            if alpha > 0.0 {
                assert!(
                    !is_blue(hue, sat),
                    "{name} is blue (hue {hue:.0}, saturation {sat:.2})"
                );
            }
        }
    }

    #[test]
    fn shadows_are_hard() {
        for s in [shadow(), hard_shadow()] {
            assert_eq!(s.len(), 1);
            assert_eq!(s[0].blur_radius, px(0.));
            assert_eq!(s[0].spread_radius, px(0.));
            assert_eq!(s[0].offset.x, s[0].offset.y);
        }
    }
}
