//! FEMME's retro skin: colour/size tokens from `assets/index.html` (the web board), the embedded
//! fonts, and the mapping of both onto gpui-component's [`Theme`] so stock widgets (inputs, menus,
//! popovers, dialogs, scrollbars, resize handles) wear the same palette. Light and dark.
//!
//! The skin in one paragraph: the window background (the *desk*) is pink with an ink dot grid and
//! shows only as a thin gutter around the panels; every panel and the centre are *paper*, with the
//! *panel grey* for secondary surfaces (tab strips, the composer's attachment row, muted rows);
//! text and 1.2 px rules are *ink*; hover fills are the FEMME pink ([`hover`]) and *neon* is kept
//! for the pressed state, the focus ring and the dot of a working session. Never hard-code a colour
//! outside this file: add a token here and use it.
//!
//! Dark mode is a second [`Palette`] with the same token names ([`DARK`] beside [`LIGHT`]). It turns
//! the neutrals over (ink is the light colour and paper the dark one, panel grey a step lighter, the
//! desk near-black with a dim pink dot grid), keeps hover and neon pink and lifts the lane colours so
//! a dot or a pill still reads on a dark surface. A label's colour follows its fill, so the two
//! pinks swap roles: [`hover`] carries ink labels and is the deeper pink in dark, [`accent_deep`]
//! carries paper labels and is the brighter one. Title bars stay dark with pink lettering in both
//! ([`bar`], [`bar_text`]). Screens read the tokens through accessor functions ([`ink`], [`paper`],
//! ...) that look at the live palette ([`current`]); [`set_dark`] flips all of them at once,
//! re-installs the gpui-component theme and redraws every window. A colour baked into state (a
//! cache, a static, a texture) goes stale on a flip: derive it from the live palette when it is
//! drawn.
//!
//! Contrast, measured with [`contrast`] (the unit tests pin every pair a screen relies on, in both
//! palettes): ink on paper 16.5 (dark 14.1), ink on [`hover`] 4.74 (dark 4.66), paper on
//! [`accent_deep`] 5.5 (dark 4.74). White on the light [`hover`] is only 3.5, so a hover fill keeps
//! the label's ink colour; it also keeps gpui from showing a stale label colour (gpui takes a
//! label's hover colour from the last mouse move but the fill from the live hit test, so after a
//! key press under a resting pointer a label that turns white sits on paper).

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};

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

/// The more readable of [`ink`] and [`paper`] as text on a `bg` fill (status pills, solid dots).
pub fn text_on(bg: Rgba) -> Rgba {
    text_on_in(current(), bg)
}

/// [`text_on`] for the palette `p`: its ink or its paper, whichever reads better on `bg`.
pub fn text_on_in(p: &Palette, bg: Rgba) -> Rgba {
    if contrast(p.ink, bg) >= contrast(p.paper, bg) {
        p.ink
    } else {
        p.paper
    }
}

// ---------------------------------------------------------------------------------------------
// The palettes: every colour token, once per theme.
// ---------------------------------------------------------------------------------------------

/// Declares [`Palette`] (one `Rgba` field per token) and, for each token, a function of the same
/// name that reads it from the live palette ([`current`]). The doc comment is on both.
macro_rules! tokens {
    ($($(#[$doc:meta])* $name:ident,)*) => {
        /// A complete set of colour tokens. [`LIGHT`] is the web board's skin, [`DARK`] its night
        /// version; both define every token, so a screen written against the names never cares
        /// which one is on.
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub struct Palette {
            $($(#[$doc])* pub $name: Rgba,)*
        }

        impl Palette {
            /// Every token with its name, for the tests.
            #[cfg(test)]
            fn entries(&self) -> Vec<(&'static str, Rgba)> {
                vec![$((stringify!($name), self.$name),)*]
            }
        }

        $(
            $(#[$doc])*
            pub fn $name() -> Rgba {
                current().$name
            }
        )*
    };
}

tokens! {
    /// The desk (`--desk`): the window background behind everything. Pink in the light palette,
    /// near-black in the dark one, with a dot grid ([`crate::ui::DotGrid`], dots in `desk_dot`). It
    /// shows only in the gutters around panels ([`GUTTER`]) and on title-bar lettering; a work
    /// surface is never desk-coloured.
    desk,
    /// The dots of the desk's grid: ink on the pink desk, a dim pink on the dark one, where a bright
    /// grid would shout over the work.
    desk_dot,
    /// Paper (`--paper`): the work surface. Panels, the centre, inputs, cards and the terminal.
    paper,
    /// Panel grey (`--panel`): secondary surfaces on paper (tab strips, the composer's attachment
    /// row, muted rows, dialogs, popovers). A step lighter than paper in the dark palette.
    panel,
    /// Ink (`--ink`): text, borders, shadows, the fill of a dark button. The dark colour in the
    /// light palette and the light one in the dark palette.
    ink,
    /// Readable secondary text (`--muted`): meta lines, placeholders. Over 5:1 on paper and on panel
    /// grey in both palettes. For disabled text use `muted_text`.
    muted,
    /// FEMME pink (`--hover`): hover fills, the selected row, text selection. It carries `ink`
    /// labels (4.6:1 or better; see `on_accent`), so it is the mid pink in the light palette and
    /// the deeper pink in the dark one.
    hover,
    /// Neon pink (`--neon`): the pressed/active state of a control, the focus ring, the dot of a
    /// working session, the wordmark. Never a resting fill, and the same in both palettes.
    neon,
    /// Lane: working (`--work`). Green; a fill or a dot, not text on light paper (3.0:1).
    work,
    /// Lane: paused / blocked (`--pause`). Yellow; a fill or a dot, never text on light paper (1.8:1).
    pause,
    /// Lane: queued (`--queue`).
    queue,
    /// Lane: pull request (`--pr`). Teal; also the `info` colour of gpui-component widgets.
    pr,
    /// Lane: failed / error (`--bad`). Usable as text on paper in both palettes.
    bad,
    /// Lane: done / idle (`--done`).
    done,
    /// The goal strip's mark (`◎`) while a `/goal` runs: amber, readable as text on paper.
    goal,
    /// The goal strip's mark and verdict once the goal is achieved: green, readable on paper.
    goal_met,
    /// Work surface: panels, the centre, inputs, cards (same as `paper`).
    surface,
    /// Secondary surface: tab strips, the composer's attachment row, muted rows, dialogs (same as
    /// `panel`).
    surface_muted,
    /// Border colour of every box: ink, [`RULE`] wide.
    border,
    /// Colour of the hard (zero-blur) shadow under buttons and cards; see [`shadow`]. Ink in both
    /// palettes.
    shadow_color,
    /// Colour of the dimmer hard shadow under dialogs and the composer; see [`hard_shadow`]. Black
    /// at 40% on light paper, ink at 30% on dark (a black shadow would vanish there).
    shadow_dim,
    /// Hover pink: the fill of a hovered control or row, and of the selected row (same as `hover`).
    accent,
    /// Strong accent (neon): the pressed/active state and the focus ring (same as `neon`).
    accent_strong,
    /// Soft pink: a wash for a hovered row inside a list whose selected row is `accent`; ink on it is
    /// over 10:1. Pale in the light palette, a dark plum in the dark one.
    accent_soft,
    /// Deep pink: the hover fill of an ink-filled control (the dark button), whose label is paper:
    /// the pink that carries paper labels (4.7:1 or better). Also the colour of a hovered link. In
    /// the dark palette that is the brighter pink, because paper is the dark colour there.
    accent_deep,
    /// Pink for text and translucent washes on paper (`#D45BB6` in both palettes): links, the drop
    /// target. 3.5:1 on light paper, 4.7:1 on dark paper.
    accent_text,
    /// Label colour on an `accent` fill: ink (4.6:1 or better; white would be 3.5:1 on the light
    /// palette's pink, below AA).
    on_accent,
    /// Label colour on the neon fill of a pressed control: the dark colour in both palettes (ink in
    /// the light one, paper in the dark one), 5.5:1 or better.
    on_accent_strong,
    /// Text that is allowed to be faint: disabled labels, disabled borders (3.5:1 or better on
    /// paper). Readable secondary text is `muted`.
    muted_text,
    /// Success: done, connected, "working" lane green (same as `work`).
    success,
    /// Warning: blocked, asks the user something (same as `pause`).
    warn,
    /// Danger: failed, destructive (same as `bad`).
    danger,
    /// Keyboard focus ring colour (neon, drawn [`FOCUS_RING_WIDTH`] wide). gpui-component widgets
    /// show focus by tinting their own border with it (see [`apply`]).
    focus_ring,
    /// The permission square's colour for plan mode (`#B53094` in both palettes): a purple-leaning
    /// magenta that stays apart from `neon`, the colour of auto mode.
    plan,
    /// Title bars (and the other dark bars): a dark fill in both palettes, so a bar reads as a bar.
    bar,
    /// The pink lettering of a title bar: the light palette's desk pink in both.
    bar_text,
    /// Label colour on a `bar` fill (buttons and glyphs on a title bar).
    on_bar,
    /// Secondary lettering on a `bar` (the size of the open file): panel grey in the light palette,
    /// a light grey in the dark one.
    bar_dim,
    /// Hover fill of a button on a `bar`; `on_bar` stays readable on it.
    bar_hover,
    /// The veil behind a modal card (the accounts overlay, a project dialog, the board's dialogs).
    scrim,
    /// The veil of gpui-component's own dialogs and sheets.
    overlay,
}

/// The light palette: the web board's `:root` custom properties, same names, same values.
pub const LIGHT: Palette = light();

/// The dark palette: the neutrals turned over (ink light, paper dark, a near-black desk with a dim
/// pink dot grid), the pinks kept, the lane colours lifted to read on a dark surface.
pub const DARK: Palette = dark();

/// The live theme: set by [`set_dark`], read by every accessor. A relaxed atomic is enough: it is
/// written on the UI thread and a stale read for one frame is harmless.
static DARK_ON: AtomicBool = AtomicBool::new(false);

/// Whether the dark palette is the live one.
pub fn is_dark() -> bool {
    DARK_ON.load(Ordering::Relaxed)
}

/// The live palette: [`DARK`] after `set_dark(true, ..)`, else [`LIGHT`]. The accessor functions
/// ([`ink`], [`paper`], ...) read from it; a screen that needs many tokens can take it once.
pub fn current() -> &'static Palette {
    pick(is_dark())
}

/// The palette for a theme choice.
fn pick(dark: bool) -> &'static Palette {
    if dark {
        &DARK
    } else {
        &LIGHT
    }
}

const fn light() -> Palette {
    let desk = rgb8(0xF386A1);
    let paper = rgb8(0xFEFEFE);
    let panel = rgb8(0xE5E5E5);
    let ink = rgb8(0x1E1E1E);
    let hover = rgb8(0xD45BB6);
    let neon = rgb8(0xFF4FB4);
    let work = rgb8(0x03AA5C);
    let pause = rgb8(0xF2B33D);
    let bad = rgb8(0xD93036);
    let accent_deep = rgb8(0xB53094);
    Palette {
        desk,
        desk_dot: ink,
        paper,
        panel,
        ink,
        muted: rgb8(0x5C5C5C),
        hover,
        neon,
        work,
        pause,
        queue: rgb8(0xDEDEDE),
        pr: rgb8(0x09AEA1),
        bad,
        done: rgb8(0x858585),
        goal: rgb8(0x8A5200),
        goal_met: rgb8(0x1B6B33),
        surface: paper,
        surface_muted: panel,
        border: ink,
        shadow_color: ink,
        shadow_dim: rgba8(0x000000, 0.4),
        accent: hover,
        accent_strong: neon,
        accent_soft: rgb8(0xF6CDE6),
        accent_deep,
        accent_text: hover,
        on_accent: ink,
        on_accent_strong: ink,
        muted_text: rgb8(0x858585),
        success: work,
        warn: pause,
        danger: bad,
        focus_ring: neon,
        plan: accent_deep,
        bar: ink,
        bar_text: desk,
        on_bar: paper,
        bar_dim: panel,
        bar_hover: accent_deep,
        scrim: rgba8(0x000000, 0.25),
        overlay: rgba8(0x1E1E1E, 0.4),
    }
}

const fn dark() -> Palette {
    let desk = rgb8(0x120C10);
    let paper = rgb8(0x1E1E1E);
    let panel = rgb8(0x2B2B2B);
    let ink = rgb8(0xECECEC);
    // The pinks swap roles with the neutrals: `hover` carries ink (light) labels, so it is the deep
    // pink; `accent_deep` carries paper (dark) labels, so it is the bright one.
    let hover = rgb8(0xB53094);
    let neon = rgb8(0xFF4FB4);
    let work = rgb8(0x2FD07A);
    let pause = rgb8(0xF5C24B);
    let bad = rgb8(0xFF6B70);
    let accent_deep = rgb8(0xD45BB6);
    Palette {
        desk,
        desk_dot: rgb8(0x56313B),
        paper,
        panel,
        ink,
        muted: rgb8(0xA6A6A6),
        hover,
        neon,
        work,
        pause,
        queue: rgb8(0x4D4D4D),
        pr: rgb8(0x2CD3C4),
        bad,
        done: rgb8(0x8C8C8C),
        goal: rgb8(0xF2B544),
        goal_met: rgb8(0x5FD38D),
        surface: paper,
        surface_muted: panel,
        border: ink,
        shadow_color: ink,
        shadow_dim: rgba8(0xECECEC, 0.3),
        accent: hover,
        accent_strong: neon,
        accent_soft: rgb8(0x4A2640),
        accent_deep,
        accent_text: accent_deep,
        on_accent: ink,
        on_accent_strong: paper,
        muted_text: rgb8(0x747474),
        success: work,
        warn: pause,
        danger: bad,
        focus_ring: neon,
        plan: hover,
        bar: rgb8(0x0A080A),
        bar_text: rgb8(0xF386A1),
        on_bar: ink,
        bar_dim: rgb8(0xA6A6A6),
        bar_hover: hover,
        scrim: rgba8(0x000000, 0.5),
        overlay: rgba8(0x000000, 0.6),
    }
}

// ---------------------------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------------------------

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

/// The hard drop shadow of dialogs and the composer: `3px 3px 0` in [`shadow_dim`] (black at 40% on
/// light paper).
pub fn hard_shadow() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: Hsla::from(shadow_dim()),
        offset: point(px(3.), px(3.)),
        blur_radius: px(0.),
        spread_radius: px(0.),
        inset: false,
    }]
}

/// The small hard shadow of buttons and cards: `2px 2px 0` in solid [`shadow_color`] (ink). A
/// pressed button drops it and moves by the same 2 px, as if pushed into the page.
pub fn shadow() -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: Hsla::from(shadow_color()),
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

/// Every colour of gpui-component's theme, from the FEMME tokens of `p`. A struct literal without
/// `..Default::default()` on purpose: a field gpui-component adds becomes a compile error here
/// instead of a stray shadcn blue on screen, and the test below checks the result holds no blue.
///
/// The widgets' own mode stays "light" in both palettes: gpui-component derives a few fills from
/// `is_dark()` on the assumption that its stock dark colours are in use (an input's background then
/// becomes its border colour mixed with transparent, which here is ink), so its dark branches would
/// paint light inputs on the dark palette.
fn theme_colors(p: &Palette) -> ThemeColor {
    let clear = h(rgba8(0xFEFEFE, 0.0));
    // A shade of `c` towards ink: the hover and active variants of a coloured button (darker on
    // paper, lighter on dark paper).
    let hovered = |c: Rgba| h(mix(c, p.ink, 0.12));
    let pressed = |c: Rgba| h(mix(c, p.ink, 0.26));
    ThemeColor {
        // Surfaces and text.
        background: h(p.surface),
        foreground: h(p.ink),
        border: h(p.border),
        input: h(p.border),
        // gpui-component shows focus by tinting the border with `ring` (the ring itself is off, see
        // `apply`), so a focused input turns neon.
        ring: h(p.focus_ring),
        caret: h(p.ink),
        selection: h(p.hover),
        muted: h(p.surface_muted),
        muted_foreground: h(p.muted),
        accent: h(p.accent),
        accent_foreground: h(p.on_accent),
        overlay: h(p.overlay),
        window_border: h(p.ink),
        // Menus, popovers, tooltips, select lists.
        popover: h(p.surface_muted),
        popover_foreground: h(p.ink),
        // Plain Button.
        button: h(p.surface),
        button_hover: h(p.accent),
        button_active: h(p.accent_strong),
        button_foreground: h(p.ink),
        // Primary: the dark, ink-filled button. Its hover keeps the paper label, so the fill goes deep.
        primary: h(p.ink),
        primary_hover: h(p.accent_deep),
        primary_active: h(p.accent_strong),
        primary_foreground: h(p.paper),
        button_primary: h(p.ink),
        button_primary_hover: h(p.accent_deep),
        button_primary_active: h(p.accent_strong),
        button_primary_foreground: h(p.paper),
        // Secondary: panel grey.
        secondary: h(p.surface_muted),
        secondary_hover: h(p.accent),
        secondary_active: h(p.accent_strong),
        secondary_foreground: h(p.ink),
        button_secondary: h(p.surface_muted),
        button_secondary_hover: h(p.accent),
        button_secondary_active: h(p.accent_strong),
        button_secondary_foreground: h(p.ink),
        // Status colours: a solid lane colour, a shade darker under the pointer and pressed. The label
        // is whichever of ink and paper reads better on the fill.
        danger: h(p.danger),
        danger_hover: hovered(p.danger),
        danger_active: pressed(p.danger),
        danger_foreground: h(text_on_in(p, p.danger)),
        button_danger: h(p.danger),
        button_danger_hover: hovered(p.danger),
        button_danger_active: pressed(p.danger),
        button_danger_foreground: h(text_on_in(p, p.danger)),
        success: h(p.success),
        success_hover: hovered(p.success),
        success_active: pressed(p.success),
        success_foreground: h(text_on_in(p, p.success)),
        button_success: h(p.success),
        button_success_hover: hovered(p.success),
        button_success_active: pressed(p.success),
        button_success_foreground: h(text_on_in(p, p.success)),
        warning: h(p.warn),
        warning_hover: hovered(p.warn),
        warning_active: pressed(p.warn),
        warning_foreground: h(text_on_in(p, p.warn)),
        button_warning: h(p.warn),
        button_warning_hover: hovered(p.warn),
        button_warning_active: pressed(p.warn),
        button_warning_foreground: h(text_on_in(p, p.warn)),
        info: h(p.pr),
        info_hover: hovered(p.pr),
        info_active: pressed(p.pr),
        info_foreground: h(text_on_in(p, p.pr)),
        button_info: h(p.pr),
        button_info_hover: hovered(p.pr),
        button_info_active: pressed(p.pr),
        button_info_foreground: h(text_on_in(p, p.pr)),
        // Links: ink, deep pink under the pointer.
        link: h(p.ink),
        link_hover: h(p.accent_deep),
        link_active: h(p.ink),
        // Lists and tables: a soft wash on hover, the full pink and an ink edge on the selected row.
        list: h(p.surface),
        list_even: h(p.surface),
        list_head: h(p.surface_muted),
        list_hover: h(p.accent_soft),
        list_active: h(p.accent),
        list_active_border: h(p.ink),
        table: h(p.surface),
        table_even: h(p.surface),
        table_head: h(p.surface_muted),
        table_head_foreground: h(p.ink),
        table_foot: h(p.surface_muted),
        table_foot_foreground: h(p.ink),
        table_hover: h(p.accent_soft),
        table_active: h(p.accent),
        table_active_border: h(p.ink),
        table_row_border: h(p.surface_muted),
        // Tabs: the selected tab is the dark button.
        tab: clear,
        tab_foreground: h(p.ink),
        tab_active: h(p.ink),
        tab_active_foreground: h(p.paper),
        tab_bar: h(p.surface_muted),
        tab_bar_segmented: h(p.surface_muted),
        // Sidebar, title and status bars.
        sidebar: h(p.surface_muted),
        sidebar_foreground: h(p.ink),
        sidebar_border: h(p.ink),
        sidebar_accent: h(p.accent),
        sidebar_accent_foreground: h(p.on_accent),
        sidebar_primary: h(p.ink),
        sidebar_primary_foreground: h(p.paper),
        title_bar: h(p.bar),
        title_bar_border: h(p.ink),
        status_bar: h(p.surface_muted),
        status_bar_border: h(p.ink),
        // Scrollbars: no track, an ink thumb (the hover/drag colour is the full ink).
        scrollbar: clear,
        scrollbar_thumb: h(Rgba { a: 0.55, ..p.ink }),
        scrollbar_thumb_hover: h(p.ink),
        // Dragging a resize handle or a file over a drop target.
        drag_border: h(p.accent_strong),
        drop_target: h(Rgba {
            a: 0.25,
            ..p.accent_text
        }),
        // Small controls.
        accordion: h(p.surface),
        group_box: h(p.surface_muted),
        group_box_foreground: h(p.ink),
        description_list_label: h(p.surface_muted),
        description_list_label_foreground: h(p.ink),
        progress_bar: h(p.ink),
        slider_bar: h(p.ink),
        slider_thumb: h(p.paper),
        switch: h(p.surface_muted),
        switch_thumb: h(p.paper),
        skeleton: h(p.surface_muted),
        // Charts: pink, ink, teal and yellow; never the stock blue ramp.
        chart_1: h(p.accent_strong),
        chart_2: h(p.accent),
        chart_3: h(p.ink),
        chart_4: h(p.pr),
        chart_5: h(p.pause),
        chart_bullish: h(p.success),
        chart_bearish: h(p.danger),
        chart_grid: h(p.surface_muted),
        // The base hues (syntax and chart helpers): blue and magenta are taken from the FEMME pinks and
        // teal so that "blue" never shows up anywhere.
        red: h(p.danger),
        red_light: h(mix(p.danger, p.paper, 0.35)),
        green: h(p.success),
        green_light: h(mix(p.success, p.paper, 0.35)),
        blue: h(p.pr),
        blue_light: h(mix(p.pr, p.paper, 0.35)),
        yellow: h(p.warn),
        yellow_light: h(mix(p.warn, p.paper, 0.35)),
        magenta: h(p.accent),
        magenta_light: h(mix(p.accent, p.paper, 0.35)),
        cyan: h(p.pr),
        cyan_light: h(mix(p.pr, p.paper, 0.35)),
    }
}

/// Registers the embedded fonts, reads the saved theme choice (see [`state`]: light when there is
/// none) and installs the FEMME palette into gpui-component's global [`Theme`]. Call once after
/// `gpui_component::init` and before the first window opens.
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
    DARK_ON.store(state::load(), Ordering::Relaxed);
    install(cx);
}

/// Switches the whole app to the dark palette (`true`) or the light one: flips every token, installs
/// the matching gpui-component colours and redraws every window, cached views included. A colour
/// that was copied into state instead of read when drawn does not follow; see the module docs. The
/// choice is kept in `~/.local/state/femme/theme` for the next run ([`apply`] reads it back).
pub fn set_dark(dark: bool, cx: &mut App) {
    DARK_ON.store(dark, Ordering::Relaxed);
    state::save(dark);
    install(cx);
}

/// Writes the live palette (and the fixed fonts, radius and shadow flags) into gpui-component's
/// [`Theme`]. `Theme::update` rebuilds the base layer and refreshes every window.
///
/// The theme's own mode stays "light": see [`theme_colors`].
fn install(cx: &mut App) {
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
        t.colors = theme_colors(current());
    });
}

mod state;

#[cfg(test)]
mod tests;
