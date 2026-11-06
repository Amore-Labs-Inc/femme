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
    let colors = theme_colors(&LIGHT);
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
