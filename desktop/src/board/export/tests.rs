//! Tests of the PNG export: the frame, the pixels it draws, and that no scene makes it panic.

use tiny_skia::Pixmap;

use super::*;
use crate::board::scene::{Fill, Kind, StrokeColor, Style, Width};

type Rgba8 = [u8; 4];

fn p(x: f32, y: f32) -> Point {
    Point::new(x, y)
}

fn rect(a: Point, b: Point) -> Kind {
    Kind::Rect { a, b }
}

fn ellipse(a: Point, b: Point) -> Kind {
    Kind::Ellipse { a, b }
}

fn line(a: Point, b: Point) -> Kind {
    Kind::Line { a, b }
}

fn arrow(a: Point, b: Point) -> Kind {
    Kind::Arrow { a, b }
}

fn scribble(points: &[Point]) -> Kind {
    Kind::Freehand {
        points: points.to_vec(),
    }
}

fn text(at: Point, t: &str) -> Kind {
    Kind::Text {
        at,
        text: t.to_string(),
    }
}

fn thick() -> Style {
    Style {
        width: Width::Thick,
        ..Style::default()
    }
}

fn coloured(color: StrokeColor) -> Style {
    Style {
        color,
        width: Width::Thick,
        ..Style::default()
    }
}

fn filled(fill: Fill) -> Style {
    Style {
        fill,
        ..Style::default()
    }
}

fn token(tone: Tone) -> Rgba8 {
    palette::rgba8(palette::token(tone))
}

fn paper() -> Rgba8 {
    palette::rgba8(palette::paper())
}

/// A scene of one shape.
fn one(kind: Kind, style: Style) -> Scene {
    let mut s = Scene::new();
    s.add(kind, style);
    s
}

/// Renders, checks that the bytes are a PNG, and decodes them again.
fn render(scene: &Scene, scale: f32) -> Pixmap {
    let bytes = scene.render_png(scale);
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "a PNG signature");
    Pixmap::decode_png(&bytes).expect("a PNG that decodes")
}

fn px(img: &Pixmap, x: u32, y: u32) -> Rgba8 {
    let c = img.pixel(x, y).expect("a pixel inside the picture");
    [c.red(), c.green(), c.blue(), c.alpha()]
}

/// The pixel a scene point falls in, worked out from the frame's origin and scale alone.
fn at(frame: &ExportFrame, scene_point: Point) -> (u32, u32) {
    let x = (scene_point.x - frame.origin.x) * frame.scale;
    let y = (scene_point.y - frame.origin.y) * frame.scale;
    (x.floor() as u32, y.floor() as u32)
}

/// The pixel at a scene point of a picture drawn at `scale`.
fn px_at(scene: &Scene, img: &Pixmap, scale: f32, scene_point: Point) -> Rgba8 {
    let (x, y) = at(&scene.export_frame(scale), scene_point);
    px(img, x, y)
}

fn is_dark(c: Rgba8) -> bool {
    c[0] < 0x80 && c[1] < 0x80 && c[2] < 0x80
}

/// The box of the pixels that are not paper (min x, min y, max x, max y); none for a blank picture.
fn ink_box(img: &Pixmap) -> Option<(u32, u32, u32, u32)> {
    let mut found: Option<(u32, u32, u32, u32)> = None;
    for (i, c) in img.pixels().iter().enumerate() {
        if [c.red(), c.green(), c.blue(), c.alpha()] == paper() {
            continue;
        }
        let (x, y) = (i as u32 % img.width(), i as u32 / img.width());
        found = Some(match found {
            None => (x, y, x, y),
            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
        });
    }
    found
}

/// How many pixels in the window `x0..x1`, `y0..y1` are not paper.
fn ink_in(img: &Pixmap, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
    (y0..y1)
        .flat_map(|y| (x0..x1).map(move |x| (x, y)))
        .filter(|&(x, y)| px(img, x, y) != paper())
        .count()
}

/// The runs of rows that hold ink, as (first row, last row).
fn ink_bands(img: &Pixmap) -> Vec<(u32, u32)> {
    let mut bands: Vec<(u32, u32)> = Vec::new();
    for y in 0..img.height() {
        if ink_in(img, 0, y, img.width(), y + 1) == 0 {
            continue;
        }
        match bands.last_mut() {
            Some(last) if last.1 + 1 == y => last.1 = y,
            _ => bands.push((y, y)),
        }
    }
    bands
}

/// A rectangle with a label in it: the scene most tests start from.
fn labelled_rect() -> Scene {
    let mut s = one(rect(p(0., 0.), p(200., 100.)), Style::default());
    s.add(text(p(20., 30.), "hello"), Style::default());
    s
}

/// The pixel size a frame of `len` scene units has at `scale`, worked out from the spec.
fn side(len: f32, scale: f32) -> u32 {
    ((len + 2.0 * EXPORT_MARGIN) * scale).ceil() as u32
}

// ---- the frame ----------------------------------------------------------------------------------

#[test]
fn a_labelled_rect_exports_framed_on_its_ink_with_paper_around() {
    let s = labelled_rect();
    let b = s.bounds().expect("a scene with shapes has bounds");
    let img = render(&s, 2.0);
    let size = (img.width(), img.height());
    assert_eq!(size, (side(b.width(), 2.0), side(b.height(), 2.0)));
    let frame = s.export_frame(2.0);
    assert_eq!((frame.width, frame.height), size);

    assert_eq!(px(&img, 2, 2), paper(), "the margin is paper");
    assert_eq!(px(&img, size.0 - 3, size.1 - 3), paper());
    // The middle of the left edge: a 2 unit stroke is 4 pixels wide at this scale.
    let (ex, ey) = at(&frame, p(0., 50.));
    let edge = px(&img, ex, ey);
    assert!(is_dark(edge), "on the rect's edge: {edge:?}");
    assert!(is_dark(px(&img, ex - 1, ey)) && is_dark(px(&img, ex + 1, ey)));
    // Well inside the rect, away from the label.
    let inside = px_at(&s, &img, 2.0, p(150., 80.));
    assert_eq!(inside, paper(), "an unfilled rect shows the paper");
    // The glyphs are in the label's box.
    let (w, h) = font::measure("hello", 20.0);
    let (x0, y0) = at(&frame, p(20., 30.));
    let (x1, y1) = at(&frame, p(20. + w, 30. + h));
    assert!(ink_in(&img, x0, y0, x1, y1) > 200, "the label is drawn");
}

#[test]
fn an_empty_scene_is_a_square_of_paper_two_margins_wide() {
    let s = Scene::new();
    let want = ExportFrame {
        origin: p(-EXPORT_MARGIN, -EXPORT_MARGIN),
        scale: 2.0,
        width: 128,
        height: 128,
    };
    assert_eq!(s.export_frame(2.0), want);
    let img = render(&s, 2.0);
    assert_eq!((img.width(), img.height()), (128, 128));
    assert!(ink_box(&img).is_none(), "every pixel is paper");
    assert!(img.pixels().iter().all(|c| c.alpha() == 255), "and opaque");
    let one_to_one = render(&s, 1.0);
    assert_eq!((one_to_one.width(), one_to_one.height()), (64, 64));
}

#[test]
fn the_picture_follows_the_scale() {
    let s = labelled_rect();
    let b = s.bounds().unwrap();
    for scale in [0.5, 1.0, 2.0, 3.0] {
        let img = render(&s, scale);
        let size = (img.width(), img.height());
        assert_eq!(size, (side(b.width(), scale), side(b.height(), scale)));
        assert_eq!(s.export_frame(scale).scale, scale);
        let edge = px_at(&s, &img, scale, p(0., 50.));
        assert_ne!(edge, paper(), "the edge is drawn at scale {scale}");
        assert_eq!(px(&img, 1, 1), paper());
    }
    // At scale 1 the 2 unit stroke is 2 pixels wide: solid ink on one side of the line's middle.
    let img = render(&s, 1.0);
    let (ex, ey) = at(&s.export_frame(1.0), p(0., 50.));
    assert!(is_dark(px(&img, ex, ey)) || is_dark(px(&img, ex - 1, ey)));
    // At scale 0.5 it is a one pixel hairline: faint, but clearly there.
    let img = render(&s, 0.5);
    let (ex, ey) = at(&s.export_frame(0.5), p(0., 50.));
    let darkest = (ex - 1..=ex + 1).map(|x| px(&img, x, ey)[0]).min().unwrap();
    assert!(darkest < 0x90, "a hairline edge, darkest {darkest:#x}");
}

#[test]
fn the_frame_maps_scene_points_to_pixels() {
    let f = labelled_rect().export_frame(2.0);
    assert_eq!(f.to_pixels(f.origin), p(0., 0.));
    assert_eq!(f.to_pixels(f.origin + p(10., 5.)), p(20., 10.));
    // The rect's corner is 5 units inside the ink box and 32 more inside the margin.
    assert_eq!(f.to_pixels(p(0., 0.)), p(74., 74.));
}

#[test]
fn a_drawing_far_from_the_origin_exports_like_one_at_the_origin() {
    let rect_at = |d: f32| one(rect(p(d, d), p(d + 200., d + 100.)), Style::default());
    let near = render(&rect_at(0.0), 2.0);
    for d in [1e6, -1e6, 1e7] {
        let far = rect_at(d);
        let img = render(&far, 2.0);
        let size = (img.width(), img.height());
        assert_eq!(size, (near.width(), near.height()), "at {d}");
        let edge = px_at(&far, &img, 2.0, p(d, d + 50.));
        assert!(is_dark(edge), "the edge is drawn at {d}");
    }
}

#[test]
fn a_huge_drawing_is_drawn_smaller_so_its_longer_side_is_the_cap() {
    let wide = one(line(p(0., 0.), p(10_000., 0.)), Style::default());
    let b = wide.bounds().unwrap();
    let frame = wide.export_frame(2.0);
    assert_eq!(frame.width, MAX_SIDE, "the longer side is exactly the cap");
    assert!(frame.height < MAX_SIDE && frame.scale < 2.0);
    // Both axes shrank by the same factor.
    let want_h = (b.height() + 2.0 * EXPORT_MARGIN) * frame.scale;
    assert!(
        (frame.height as f32 - want_h).abs() <= 1.0,
        "{} vs {want_h}",
        frame.height
    );
    let want_scale = MAX_SIDE as f32 / (b.width() + 2.0 * EXPORT_MARGIN);
    assert!((frame.scale - want_scale).abs() < 1e-5);
    let img = render(&wide, 2.0);
    assert_eq!((img.width(), img.height()), (MAX_SIDE, frame.height));
    let middle = px_at(&wide, &img, 2.0, p(5_000., 0.));
    assert_ne!(middle, paper(), "the line is still drawn, thinner");

    let tall = one(line(p(0., 0.), p(0., 9_000.)), Style::default());
    let f = tall.export_frame(2.0);
    assert_eq!(f.height, MAX_SIDE);
    assert!(f.width < MAX_SIDE);
    // Square and enormous: both sides at the cap.
    let big = one(rect(p(0., 0.), p(50_000., 50_000.)), Style::default());
    let f = big.export_frame(2.0);
    assert_eq!((f.width, f.height), (MAX_SIDE, MAX_SIDE));
    // A scale that already fits is left alone.
    let f = wide.export_frame(0.1);
    assert_eq!(f.scale, 0.1);
    assert!(f.width < MAX_SIDE);
}

#[test]
fn a_scale_that_is_not_a_positive_number_means_the_default() {
    let s = labelled_rect();
    let want = s.export_frame(DEFAULT_SCALE);
    assert_eq!(want.scale, DEFAULT_SCALE);
    for bad in [0.0, -0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(s.export_frame(bad), want, "scale {bad}");
    }
    assert_eq!(s.render_png(f32::NAN), s.render_png(DEFAULT_SCALE));
    assert_eq!(s.render_png(0.0), s.render_png(DEFAULT_SCALE));
    // A huge scale is capped like a huge drawing, a tiny one is a picture of one pixel.
    let f = s.export_frame(1e9);
    assert!(f.width <= MAX_SIDE && f.height <= MAX_SIDE && f.width.max(f.height) == MAX_SIDE);
    let tiny = render(&s, 1e-9);
    assert_eq!((tiny.width(), tiny.height()), (1, 1));
}

// ---- colours and shapes -------------------------------------------------------------------------

#[test]
fn a_fill_is_exactly_its_palette_token() {
    for (fill, tone) in [(Fill::Soft, Tone::Soft), (Fill::Panel, Tone::Panel)] {
        let r = one(rect(p(0., 0.), p(100., 60.)), filled(fill));
        let img = render(&r, 2.0);
        assert_eq!(px_at(&r, &img, 2.0, p(50., 30.)), token(tone), "{fill:?}");
        assert_eq!(token(tone)[3], 255);
        // The outline is drawn over the fill, in the stroke colour.
        let edge = px_at(&r, &img, 2.0, p(0., 30.));
        assert_eq!(edge, token(Tone::Ink), "the outline of a {fill:?} rect");
        // An ellipse fills too.
        let e = one(ellipse(p(0., 0.), p(100., 60.)), filled(fill));
        let img = render(&e, 2.0);
        assert_eq!(
            px_at(&e, &img, 2.0, p(50., 30.)),
            token(tone),
            "{fill:?} ellipse"
        );
    }
}

#[test]
fn every_stroke_colour_is_exactly_its_token() {
    for colour in StrokeColor::ALL {
        let s = one(line(p(0., 50.), p(100., 50.)), coloured(colour));
        let img = render(&s, 2.0);
        let want = token(Tone::from(colour));
        assert_eq!(px_at(&s, &img, 2.0, p(50., 50.)), want, "{}", colour.name());
    }
    assert_eq!(
        token(Tone::Danger),
        [0xD9, 0x30, 0x36, 0xFF],
        "the red of the Danger token"
    );
}

#[test]
fn a_stroke_is_as_wide_as_its_style_says() {
    for (style, units) in [(Style::default(), 2.0), (thick(), 4.0)] {
        let s = one(line(p(0., 50.), p(100., 50.)), style);
        let img = render(&s, 2.0);
        let (x, y) = at(&s.export_frame(2.0), p(50., 50.));
        // How much ink there is in a column across the line, in pixels' worth of coverage.
        let darkness = |v: u8| 1.0 - f32::from(v) / f32::from(paper()[0]);
        let ink: f32 = (y - 12..y + 12)
            .map(|row| darkness(px(&img, x, row)[0]))
            .sum();
        let pixels = ink / darkness(token(Tone::Ink)[0]);
        assert!(
            (pixels - units * 2.0).abs() < 0.2,
            "{units} units is {} px: {pixels}",
            units * 2.0
        );
    }
}

#[test]
fn a_rect_has_square_mitred_corners() {
    let s = one(rect(p(0., 0.), p(100., 60.)), Style::default());
    let img = render(&s, 2.0);
    // The outer corner of the outline is a square of side 2 around (0, 0): its outermost pixel is
    // solid ink under a miter join, and only partly covered under a round one.
    assert_eq!(px_at(&s, &img, 2.0, p(-0.9, -0.9)), token(Tone::Ink));
    assert_eq!(px_at(&s, &img, 2.0, p(100.9, 60.9)), token(Tone::Ink));
}

#[test]
fn later_shapes_are_painted_over_earlier_ones() {
    let panel = (rect(p(0., 0.), p(100., 100.)), filled(Fill::Panel));
    let red = (
        line(p(-20., 50.), p(120., 50.)),
        coloured(StrokeColor::Danger),
    );
    for rect_on_top in [false, true] {
        let (under, over) = if rect_on_top {
            (&red, &panel)
        } else {
            (&panel, &red)
        };
        let mut s = one(under.0.clone(), under.1);
        s.add(over.0.clone(), over.1);
        let img = render(&s, 2.0);
        let want = token(if rect_on_top {
            Tone::Panel
        } else {
            Tone::Danger
        });
        assert_eq!(
            px_at(&s, &img, 2.0, p(50., 50.)),
            want,
            "rect on top: {rect_on_top}"
        );
    }
}

#[test]
fn an_ellipse_is_drawn_on_its_outline_and_hollow_inside() {
    let s = one(ellipse(p(0., 0.), p(200., 100.)), thick());
    let img = render(&s, 2.0);
    let frame = s.export_frame(2.0);
    for on_outline in [p(0., 50.), p(200., 50.), p(100., 0.), p(100., 100.)] {
        let (x, y) = at(&frame, on_outline);
        // The outline is 8 pixels wide and the point is on its middle: one of these is ink.
        let near = [px(&img, x, y), px(&img, x - 1, y), px(&img, x, y - 1)];
        assert!(near.iter().any(|c| is_dark(*c)), "{on_outline:?}");
    }
    for off in [p(100., 50.), p(5., 5.), p(195., 95.)] {
        let (x, y) = at(&frame, off);
        assert_eq!(px(&img, x, y), paper(), "{off:?} is not on the outline");
    }
}

#[test]
fn a_dot_is_a_round_blob_as_wide_as_the_stroke() {
    // A click with the pencil, a pencil held still, and an ellipse dragged to nothing.
    let click = scribble(&[p(50., 50.)]);
    let held_still = scribble(&[p(50., 50.); 5]);
    for kind in [click, held_still, ellipse(p(50., 50.), p(50., 50.))] {
        let s = one(kind.clone(), thick());
        let img = render(&s, 2.0);
        let (cx, cy) = at(&s.export_frame(2.0), p(50., 50.));
        // The dot is 4 units, so 8 pixels, wide: solid in the middle, paper five pixels out.
        for (dx, dy) in [(0, 0), (-2, 1), (1, -2), (1, 1), (-2, -2)] {
            let c = px(&img, (cx as i32 + dx) as u32, (cy as i32 + dy) as u32);
            assert_eq!(c, token(Tone::Ink), "{kind:?} at {dx},{dy}");
        }
        assert_eq!(px(&img, cx + 5, cy), paper(), "{kind:?}");
        assert_eq!(px(&img, cx, cy + 5), paper(), "{kind:?}");
        let dark = ink_in(&img, cx - 8, cy - 8, cx + 8, cy + 8);
        assert!(
            (44..=66).contains(&dark),
            "a disc of radius 4 px, {dark} pixels, for {kind:?}"
        );
    }
}

#[test]
fn an_arrow_has_its_head_at_its_tip_and_nothing_beyond() {
    for sketchy in [false, true] {
        let mut s = one(arrow(p(0., 0.), p(100., 0.)), Style::default());
        s.sketchy = sketchy;
        let img = render(&s, 2.0);
        let (tx, ty) = at(&s.export_frame(2.0), p(100., 0.));
        // The head is a solid triangle whose point is the tip.
        let behind = ink_in(&img, tx - 6, ty - 3, tx, ty + 3);
        assert!(behind >= 20, "sketchy {sketchy}: the head, {behind} pixels");
        assert_eq!(px(&img, tx - 6, ty), token(Tone::Ink), "sketchy {sketchy}");
        // Past the tip there is nothing: the shaft's round cap does not poke through it.
        let beyond = ink_in(&img, tx + 2, ty - 6, tx + 14, ty + 6);
        assert_eq!(beyond, 0, "sketchy {sketchy}");
        // And the shaft runs back from the head.
        let (sx, sy) = at(&s.export_frame(2.0), p(30., 0.));
        assert!(
            ink_in(&img, sx, sy - 2, sx + 1, sy + 2) > 0,
            "sketchy {sketchy}"
        );
    }
}

#[test]
fn the_ink_stays_inside_the_margins_of_the_frame() {
    let mut s = one(rect(p(-40., 10.), p(120., 90.)), thick());
    s.add(ellipse(p(150., -30.), p(260., 40.)), Style::default());
    s.add(arrow(p(120., 50.), p(150., 10.)), thick());
    s.add(
        scribble(&[p(-60., 120.), p(0., 150.), p(60., 110.), p(300., 160.)]),
        thick(),
    );
    s.add(text(p(-30., 20.), "Typography gjpqy\nsecond line"), thick());
    s.add(text(p(140., 100.), "right side MMMM"), Style::default());
    for sketchy in [false, true] {
        s.sketchy = sketchy;
        for scale in [1.0, 2.0] {
            let img = render(&s, scale);
            let (x0, y0, x1, y1) = ink_box(&img).expect("something is drawn");
            let room = EXPORT_MARGIN * scale - 3.0;
            let (right, bottom) = (img.width() - 1 - x1, img.height() - 1 - y1);
            let ok = x0 as f32 >= room && y0 as f32 >= room;
            assert!(ok, "sketchy {sketchy} at {scale}: ink from {x0},{y0}");
            let ok = right as f32 >= room && bottom as f32 >= room;
            assert!(
                ok,
                "sketchy {sketchy} at {scale}: {right},{bottom} left right and below"
            );
        }
    }
}

// ---- text ----------------------------------------------------------------------------------------

#[test]
fn a_labels_pixels_lie_in_its_box() {
    let cases = [
        ("Hello world", Style::default()),
        ("Typography gjpqy", Style::default()),
        ("WIDE MMMM wwww", thick()),
        ("two\nlines of text\nand three", Style::default()),
        ("tab\tstop", Style::default()),
    ];
    for (label, style) in cases {
        let s = one(text(p(10., 10.), label), style);
        for scale in [1.0, 2.0, 3.0] {
            let img = render(&s, scale);
            let frame = s.export_frame(scale);
            let (w, h) = font::measure(label, style.width.text_size());
            let x0 = (10.0 - frame.origin.x) * scale;
            let y0 = (10.0 - frame.origin.y) * scale;
            let (x1, y1) = (x0 + w * scale, y0 + h * scale);
            let (ix0, iy0, ix1, iy1) = ink_box(&img).expect("the label is drawn");
            let slack = 2.0;
            let ok = ix0 as f32 >= x0 - slack && ix1 as f32 <= x1 + slack;
            assert!(
                ok,
                "{label:?} at {scale}: ink x {ix0}..{ix1} not in {x0}..{x1}"
            );
            let ok = iy0 as f32 >= y0 - slack && iy1 as f32 <= y1 + slack;
            assert!(
                ok,
                "{label:?} at {scale}: ink y {iy0}..{iy1} not in {y0}..{y1}"
            );
            // The advances are really applied: the ink spans most of the measured width.
            let span = (ix1 - ix0) as f32;
            assert!(
                span >= 0.8 * (x1 - x0),
                "{label:?} at {scale}: ink {span} of {}",
                x1 - x0
            );
        }
    }
}

#[test]
fn text_stands_on_its_baseline() {
    // Capital H has no overshoot: it fills exactly from the baseline up to the cap height.
    let s = one(text(p(10., 10.), "HHH"), Style::default());
    let img = render(&s, 2.0);
    let frame = s.export_frame(2.0);
    let (_, top, _, bottom) = ink_box(&img).unwrap();
    // 20 units: the line's top, plus 19.6875 down to the baseline, plus 14.0625 (1440/2048 em)
    // for the capitals' height.
    let baseline = (10.0 + 19.6875 - frame.origin.y) * 2.0;
    let cap_top = (10.0 + 19.6875 - 14.0625 - frame.origin.y) * 2.0;
    assert!(
        (bottom as f32 + 1.0 - baseline).abs() <= 1.0,
        "bottom {bottom}, baseline {baseline}"
    );
    assert!(
        (top as f32 - cap_top).abs() <= 1.0,
        "top {top}, cap top {cap_top}"
    );
}

#[test]
fn text_takes_its_colour_from_its_tone() {
    for colour in [StrokeColor::Danger, StrokeColor::Success, StrokeColor::Ink] {
        let s = one(text(p(0., 0.), "ILl"), coloured(colour));
        let img = render(&s, 2.0);
        let want = token(Tone::from(colour));
        let solid = img
            .pixels()
            .iter()
            .filter(|c| [c.red(), c.green(), c.blue(), c.alpha()] == want);
        assert!(
            solid.count() > 50,
            "{} text has solid pixels",
            colour.name()
        );
    }
}

#[test]
fn a_newline_starts_a_second_line_one_pitch_lower() {
    let s = one(text(p(0., 0.), "HH\nHH"), Style::default());
    let img = render(&s, 2.0);
    let bands = ink_bands(&img);
    assert_eq!(bands.len(), 2, "two lines, two bands of ink: {bands:?}");
    let pitch = 20.0 * font::LINE_HEIGHT * 2.0;
    let (a, b) = (bands[0], bands[1]);
    assert!(
        ((b.0 - a.0) as f32 - pitch).abs() <= 1.0,
        "{bands:?}, pitch {pitch}"
    );
    assert!(
        ((b.1 - a.1) as f32 - pitch).abs() <= 1.0,
        "{bands:?}, pitch {pitch}"
    );
    assert_eq!(a.1 - a.0, b.1 - b.0, "both lines are as tall");
    // Both lines start at the same x.
    let first_col = |rows: (u32, u32)| {
        (0..img.width()).find(|&x| ink_in(&img, x, rows.0, x + 1, rows.1 + 1) > 0)
    };
    assert_eq!(first_col(a), first_col(b));
    // An empty line in the middle leaves a gap of its own.
    let gap = one(text(p(0., 0.), "H\n\nH"), Style::default());
    let bands = ink_bands(&render(&gap, 2.0));
    assert_eq!(bands.len(), 2);
    assert!(
        ((bands[1].0 - bands[0].0) as f32 - 2.0 * pitch).abs() <= 1.0,
        "{bands:?}"
    );
}

#[test]
fn blank_text_draws_nothing_and_unknown_chars_draw_a_box() {
    for label in ["", "   ", "\n\n", "\t", "a\u{200D}\u{FE0F}", "\u{1}\r"] {
        let img = render(&one(text(p(0., 0.), label), Style::default()), 2.0);
        let boxes = label.contains('a');
        assert_eq!(ink_box(&img).is_some(), boxes, "{label:?}");
    }
    // The font has no CJK and no emoji: the reader sees that something stood there.
    for label in ["\u{4e2d}\u{6587}", "\u{1F600}"] {
        let img = render(&one(text(p(0., 0.), label), Style::default()), 2.0);
        assert!(ink_box(&img).is_some(), "{label:?}");
    }
}

// ---- determinism and robustness -----------------------------------------------------------------

#[test]
fn the_same_scene_gives_the_same_bytes_and_sketchy_gives_different_ones() {
    let mut s = labelled_rect();
    s.add(ellipse(p(220., 0.), p(320., 80.)), Style::default());
    s.add(
        scribble(&[p(0., 140.), p(30., 160.), p(60., 130.), p(90., 150.)]),
        thick(),
    );
    s.add(arrow(p(200., 50.), p(220., 40.)), Style::default());
    s.sketchy = true;
    let first = s.render_png(2.0);
    assert_eq!(first, s.render_png(2.0), "sketchy export is deterministic");
    assert_eq!(
        first,
        s.clone().render_png(2.0),
        "and so is a copy of the scene"
    );
    s.sketchy = false;
    let plain = s.render_png(2.0);
    assert_eq!(plain, s.render_png(2.0));
    assert_ne!(first, plain, "sketchy changes the picture");
    let (a, b) = (
        Pixmap::decode_png(&first).unwrap(),
        Pixmap::decode_png(&plain).unwrap(),
    );
    assert_eq!(
        (a.width(), a.height()),
        (b.width(), b.height()),
        "but not its frame"
    );
}

#[test]
fn shapes_that_are_not_numbers_are_left_out_and_the_rest_is_drawn() {
    let mut s = labelled_rect();
    for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        s.add(rect(p(v, 0.), p(10., v)), Style::default());
        s.add(line(p(0., v), p(v, v)), thick());
        s.add(text(p(v, v), "lost"), Style::default());
        s.add(
            scribble(&[p(0., 0.), p(v, 5.), p(9., 9.)]),
            Style::default(),
        );
        s.add(ellipse(p(v, v), p(v, v)), Style::default());
        s.add(arrow(p(v, 1.), p(2., v)), Style::default());
    }
    let frame = s.export_frame(2.0);
    assert!(frame.width < 1000 && frame.height < 1000, "{frame:?}");
    let img = render(&s, 2.0);
    // Not a pixel of the others shows: all the ink is in the labelled rect (its outline reaches a
    // unit outside the corners).
    let (x0, y0) = at(&frame, p(-1., -1.));
    let (x1, y1) = at(&frame, p(201., 101.));
    let (ink_x0, ink_y0, ink_x1, ink_y1) = ink_box(&img).expect("the rect is drawn");
    assert!(
        ink_x0 + 1 >= x0 && ink_y0 + 1 >= y0,
        "{ink_x0},{ink_y0} before {x0},{y0}"
    );
    assert!(
        ink_x1 <= x1 + 1 && ink_y1 <= y1 + 1,
        "{ink_x1},{ink_y1} past {x1},{y1}"
    );
    assert!(is_dark(px_at(&s, &img, 2.0, p(0., 50.))), "the rect's edge");
    assert_eq!(px_at(&s, &img, 2.0, p(150., 80.)), paper());
    // Sketchy draws them through the jitter code too.
    s.sketchy = true;
    assert!(render(&s, 2.0).width() < 1000);
}

#[test]
fn absurd_coordinates_neither_panic_nor_allocate_without_bound() {
    let values = [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        1e30,
        -1e30,
        f32::MAX,
        f32::MIN,
        1e-30,
        -0.0,
    ];
    let scene_of = |a: f32, b: f32, sketchy: bool| {
        let (q, r) = (p(a, b), p(b, a));
        let mut s = one(rect(q, r), thick());
        s.sketchy = sketchy;
        s.add(ellipse(q, r), thick());
        s.add(line(q, r), Style::default());
        s.add(arrow(q, r), thick());
        s.add(scribble(&[q, r, q]), Style::default());
        s.add(text(q, "hi\nthere"), thick());
        s
    };
    // Every pair of coordinates as every kind of shape, sketchy or not: the frame stays in range.
    for sketchy in [false, true] {
        for a in values {
            for b in values {
                let s = scene_of(a, b, sketchy);
                for scale in [2.0, 1e9] {
                    let f = s.export_frame(scale);
                    assert!((1..=MAX_SIDE).contains(&f.width), "{a} {b}: {f:?}");
                    assert!((1..=MAX_SIDE).contains(&f.height), "{a} {b}: {f:?}");
                    assert!(f.scale.is_finite() && f.scale > 0.0, "{a} {b}: {f:?}");
                }
            }
        }
    }
    // A few of them drawn, at a scale so small that the picture is a pixel however far the shapes
    // reach: every shape is still turned into primitives, mapped and handed to the rasteriser.
    for (a, b) in [
        (f32::NAN, f32::NAN),
        (1e30, -1e30),
        (f32::MAX, f32::MIN),
        (f32::INFINITY, 0.0),
    ] {
        for sketchy in [false, true] {
            let mut s = scene_of(a, b, sketchy);
            s.add(rect(p(0., 0.), p(30., 30.)), Style::default());
            let img = render(&s, 1e-39);
            assert_eq!(
                (img.width(), img.height()),
                (1, 1),
                "{a} {b} sketchy {sketchy}"
            );
        }
    }
}

#[test]
fn a_drawing_at_the_edge_of_the_number_range_exports_at_the_cap() {
    // The one picture of the largest size in these tests: 4096 square, slow to encode in a debug
    // build. Everything but the fill is far off the paper or thinner than a pixel.
    let mut s = one(rect(p(-1e30, -1e30), p(1e30, 1e30)), thick());
    s.sketchy = true;
    s.add(arrow(p(-1e30, 1e30), p(1e30, -1e30)), thick());
    s.add(ellipse(p(-1e30, -1e30), p(1e30, 1e30)), filled(Fill::Soft));
    s.add(text(p(0., 0.), "far away"), thick());
    let frame = s.export_frame(2.0);
    assert_eq!((frame.width, frame.height), (MAX_SIDE, MAX_SIDE));
    let img = render(&s, 2.0);
    assert_eq!((img.width(), img.height()), (MAX_SIDE, MAX_SIDE));
    // The ellipse fills the middle of the picture, whatever sizes the numbers had.
    assert_eq!(px(&img, MAX_SIDE / 2, MAX_SIDE / 4), token(Tone::Soft));
}

#[test]
fn a_drawing_as_wide_as_the_number_range_is_a_one_pixel_sliver() {
    let s = one(line(p(f32::MIN, 0.), p(f32::MAX, 0.)), thick());
    assert_eq!(
        (s.export_frame(2.0).width, s.export_frame(2.0).height),
        (MAX_SIDE, 1)
    );
    let img = render(&s, 2.0);
    assert_eq!((img.width(), img.height()), (MAX_SIDE, 1));
}
