use super::*;

/// Both palettes with their names, for the failure messages. The tests read the palettes
/// directly and never flip the process-wide theme, so they can run in parallel.
const BOTH: [(&str, &Palette); 2] = [("light", &LIGHT), ("dark", &DARK)];

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
    assert_eq!(
        contrast(LIGHT.ink, LIGHT.paper),
        contrast(LIGHT.paper, LIGHT.ink)
    );
    // Known values: ink on paper is 16.5:1 in the light palette.
    assert!((contrast(LIGHT.ink, LIGHT.paper) - 16.53).abs() < 0.05);
}

/// Every text/fill pair a screen relies on is AA (4.5:1) readable, in both palettes.
#[test]
fn text_is_readable_on_its_fills() {
    for (theme, p) in BOTH {
        // The label a pill or a dot's text takes on a lane fill.
        let label = |bg| text_on_in(p, bg);
        let pairs: Vec<(&str, Rgba, Rgba)> = vec![
            ("ink on paper", p.ink, p.paper),
            ("ink on panel grey", p.ink, p.panel),
            ("label on hover pink", p.on_accent, p.accent),
            ("ink on soft pink", p.ink, p.accent_soft),
            ("paper on ink (dark button)", p.paper, p.ink),
            (
                "paper on deep pink (dark button hover)",
                p.paper,
                p.accent_deep,
            ),
            (
                "label on neon (pressed)",
                p.on_accent_strong,
                p.accent_strong,
            ),
            ("lettering on the bar", p.bar_text, p.bar),
            ("label on the bar", p.on_bar, p.bar),
            ("label on the bar's hover fill", p.on_bar, p.bar_hover),
            ("muted on paper", p.muted, p.paper),
            ("muted on panel grey", p.muted, p.panel),
            ("danger red on paper", p.danger, p.paper),
            ("paper on danger red", p.paper, p.danger),
            ("label on success green", label(p.success), p.success),
            ("label on warning yellow", label(p.warn), p.warn),
            ("label on teal", label(p.pr), p.pr),
            ("label on queued grey", label(p.queue), p.queue),
            ("label on done grey", label(p.done), p.done),
        ];
        for (name, fg, bg) in pairs {
            let c = contrast(fg, bg);
            assert!(c >= 4.5, "{theme}: {name}: {c:.2}:1 is below 4.5:1");
        }
    }
}

/// `text_on_in` always picks a readable label, for every opaque fill the skin paints.
#[test]
fn text_on_picks_a_readable_label() {
    for (theme, p) in BOTH {
        for (name, bg) in p.entries() {
            if bg.a < 1.0 {
                continue;
            }
            let c = contrast(text_on_in(p, bg), bg);
            assert!(c >= 3.0, "{theme}: a label on {name} is {c:.2}:1");
        }
        assert_eq!(text_on_in(p, p.ink), p.paper, "{theme}");
        assert_eq!(text_on_in(p, p.paper), p.ink, "{theme}");
        assert_eq!(text_on_in(p, p.bad), p.paper, "{theme}");
    }
}

/// The documented caveat: white on the light palette's hover pink is below AA, which is why a
/// hover fill keeps an ink label. If the pink is ever retuned this test says the doc comments
/// need to follow.
#[test]
fn white_on_hover_pink_is_not_aa() {
    let white = rgb8(0xFFFFFF);
    assert!(contrast(white, LIGHT.hover) < 4.5);
    assert!(contrast(LIGHT.ink, LIGHT.hover) >= 4.5);
}

/// The dark palette is the light one turned over: ink the light colour, paper the dark one with
/// the panel grey a step above it and the desk below it. Ink still reads on paper at 7:1 (AAA)
/// and a label on the hover pink at 3.5:1 or better, as in the light palette.
#[test]
fn dark_turns_the_neutrals_over() {
    assert!(luminance(DARK.desk) < luminance(DARK.paper));
    assert!(luminance(DARK.paper) < luminance(DARK.panel));
    assert!(luminance(DARK.panel) < luminance(DARK.ink));
    for (theme, p) in BOTH {
        let c = contrast(p.ink, p.paper);
        assert!(c >= 7.0, "{theme}: ink on paper {c:.2}:1");
        let c = contrast(p.on_accent, p.accent);
        assert!(c >= 3.5, "{theme}: label on hover pink {c:.2}:1");
    }
}

/// Hover and neon stay pink in both palettes (hue 300 to 350 degrees), neon is the very same
/// colour, and the pink that carries paper labels is pink too.
#[test]
fn the_pinks_stay_pink() {
    for (theme, p) in BOTH {
        for (name, c) in [
            ("hover", p.hover),
            ("neon", p.neon),
            ("accent_deep", p.accent_deep),
            ("accent_text", p.accent_text),
        ] {
            let (h, s) = hue_sat(c);
            assert!(
                (300.0..=350.0).contains(&h) && s > 0.4,
                "{theme}: {name} is not pink (hue {h:.0}, saturation {s:.2})"
            );
        }
    }
    assert_eq!(LIGHT.neon, DARK.neon);
    assert_eq!(LIGHT.accent_text, DARK.accent_text);
}

/// A lane dot on dark paper or panel grey is a visible mark (3:1, the bar for UI graphics).
#[test]
fn dark_lanes_show_on_dark_surfaces() {
    for (name, c) in [
        ("work", DARK.work),
        ("pause", DARK.pause),
        ("pr", DARK.pr),
        ("bad", DARK.bad),
        ("done", DARK.done),
    ] {
        for (on, bg) in [("paper", DARK.paper), ("panel grey", DARK.panel)] {
            let r = contrast(c, bg);
            assert!(r >= 3.0, "{name} on {on}: {r:.2}:1");
        }
    }
}

/// Pink text (links) is legible on its paper: 3:1 at least, the bar for large text.
#[test]
fn pink_text_is_legible() {
    for (theme, p) in BOTH {
        let c = contrast(p.accent_text, p.paper);
        assert!(c >= 3.0, "{theme}: pink text on paper {c:.2}:1");
    }
    assert!(contrast(DARK.accent_text, DARK.paper) >= 4.5);
}

/// The dark desk's dots are a dim grid: visible, and nowhere near the contrast of the work.
#[test]
fn the_dark_dot_grid_is_dim() {
    let c = contrast(DARK.desk_dot, DARK.desk);
    assert!((1.3..=3.0).contains(&c), "{c:.2}:1");
}

/// A bar is a dark fill with pink lettering in both palettes.
#[test]
fn bars_are_dark_with_pink_lettering() {
    for (theme, p) in BOTH {
        assert!(luminance(p.bar) < 0.03, "{theme}: the bar is not dark");
        let (h, s) = hue_sat(p.bar_text);
        assert!(
            (330.0..=355.0).contains(&h) && s > 0.5,
            "{theme}: the bar's lettering is not pink (hue {h:.0}, saturation {s:.2})"
        );
    }
}

/// The alias tokens are what their doc comments say.
#[test]
fn aliases_follow_their_base_tokens() {
    for (theme, p) in BOTH {
        for (name, alias, base) in [
            ("surface", p.surface, p.paper),
            ("surface_muted", p.surface_muted, p.panel),
            ("border", p.border, p.ink),
            ("shadow_color", p.shadow_color, p.ink),
            ("accent", p.accent, p.hover),
            ("accent_strong", p.accent_strong, p.neon),
            ("on_accent", p.on_accent, p.ink),
            ("success", p.success, p.work),
            ("warn", p.warn, p.pause),
            ("danger", p.danger, p.bad),
            ("focus_ring", p.focus_ring, p.neon),
        ] {
            assert_eq!(alias, base, "{theme}: {name}");
        }
    }
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
    for (theme, p) in BOTH {
        for (name, c) in p.entries() {
            let (h, s) = hue_sat(c);
            assert!(
                !is_blue(h, s),
                "{theme}: {name} is blue (hue {h:.0}, saturation {s:.2})"
            );
        }
    }
    assert!(is_blue(220.0, 0.9), "the check itself spots blue");
    assert!(!is_blue(175.0, 0.9), "teal is not blue");
}

/// `(field name, hue in degrees, saturation, lightness, alpha)` of every colour field of
/// `ThemeColor`, read from its `Debug` output (the struct has no iterator and this crate has no
/// serde).
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
    for (theme, p) in BOTH {
        let colors = theme_colors(p);
        let f = fields(&colors);
        // The parser found the whole struct (gpui-component 0.8.1 has 137 colour fields).
        assert!(f.len() >= 130, "{theme}: parsed only {} fields", f.len());
        for (name, hue, sat, lightness, alpha) in f {
            // A field nobody set is the all-zero default: transparent black.
            assert!(
                alpha != 0.0 || lightness != 0.0 || sat != 0.0 || hue != 0.0,
                "{theme}: {name} was left at the default"
            );
            // Transparent colours have no colour to speak of.
            if alpha > 0.0 {
                assert!(
                    !is_blue(hue, sat),
                    "{theme}: {name} is blue (hue {hue:.0}, saturation {sat:.2})"
                );
            }
        }
    }
}

/// The stock widgets' labels are readable on their fills in both palettes: 4.5:1 at rest and
/// under the pointer. (A pressed stock button is neon under its resting label; FEMME's own
/// controls take the pressed label from `on_accent_strong` instead, see `ui::look`.)
#[test]
fn stock_widget_labels_are_readable() {
    for (theme, p) in BOTH {
        let c = theme_colors(p);
        let pairs = [
            ("foreground on background", c.foreground, c.background),
            ("accent_foreground on accent", c.accent_foreground, c.accent),
            ("button_foreground on button", c.button_foreground, c.button),
            (
                "button_foreground on button_hover",
                c.button_foreground,
                c.button_hover,
            ),
            (
                "primary_foreground on primary",
                c.primary_foreground,
                c.primary,
            ),
            (
                "primary_foreground on primary_hover",
                c.primary_foreground,
                c.primary_hover,
            ),
            (
                "secondary_foreground on secondary",
                c.secondary_foreground,
                c.secondary,
            ),
            (
                "secondary_foreground on secondary_hover",
                c.secondary_foreground,
                c.secondary_hover,
            ),
            ("muted_foreground on muted", c.muted_foreground, c.muted),
            (
                "popover_foreground on popover",
                c.popover_foreground,
                c.popover,
            ),
            (
                "tab_active_foreground on tab_active",
                c.tab_active_foreground,
                c.tab_active,
            ),
            (
                "sidebar_foreground on sidebar",
                c.sidebar_foreground,
                c.sidebar,
            ),
            (
                "table_head_foreground on table_head",
                c.table_head_foreground,
                c.table_head,
            ),
            ("danger_foreground on danger", c.danger_foreground, c.danger),
            (
                "danger_foreground on danger_hover",
                c.danger_foreground,
                c.danger_hover,
            ),
            (
                "success_foreground on success",
                c.success_foreground,
                c.success,
            ),
            (
                "success_foreground on success_hover",
                c.success_foreground,
                c.success_hover,
            ),
            (
                "warning_foreground on warning",
                c.warning_foreground,
                c.warning,
            ),
            (
                "warning_foreground on warning_hover",
                c.warning_foreground,
                c.warning_hover,
            ),
            ("info_foreground on info", c.info_foreground, c.info),
            (
                "info_foreground on info_hover",
                c.info_foreground,
                c.info_hover,
            ),
        ];
        for (name, fg, bg) in pairs {
            let r = contrast(Rgba::from(fg), Rgba::from(bg));
            assert!(r >= 4.5, "{theme}: {name}: {r:.2}:1 is below 4.5:1");
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
