//! Tests of the draw list: culling, the grid, fills and strokes as triangles, the selection, the
//! marquee and the text being typed.

use super::*;
use crate::board::editor::{Button, Mods, Tool};
use crate::board::scene::{Fill, Kind, Scene, Style};
use crate::board::stroke::area2;

fn p(x: f32, y: f32) -> Point {
    Point::new(x, y)
}

const NO: Mods = Mods {
    ctrl: false,
    shift: false,
    alt: false,
};
const VIEWPORT: Point = Point::new(1000.0, 700.0);

/// An editor with these shapes and the view at the origin, zoom 1 (a loaded scene would be fitted).
fn ed_with(shapes: &[(Kind, Style)]) -> Editor {
    let mut scene = Scene::new();
    for (k, s) in shapes {
        scene.add(k.clone(), *s);
    }
    let mut e = Editor::new(Scene::new());
    e.set_viewport(VIEWPORT);
    e.replace_scene(scene);
    e
}

fn rect_kind(a: Point, b: Point) -> Kind {
    Kind::Rect { a, b }
}

fn tris_of(ops: &[Op], color: Rgba) -> Vec<[Point; 3]> {
    ops.iter()
        .filter_map(|o| match o {
            Op::Tris { tris, color: c } if *c == color => Some(tris.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn area(tris: &[[Point; 3]]) -> f32 {
    tris.iter().map(|t| area2(t).abs() * 0.5).sum()
}

fn frames(ops: &[Op]) -> Vec<(Rect, Option<Rgba>, Option<Edge>)> {
    ops.iter()
        .filter_map(|o| match o {
            Op::Frame { rect, fill, edge } => Some((*rect, *fill, *edge)),
            _ => None,
        })
        .collect()
}

#[test]
fn an_empty_board_is_just_the_grid() {
    let e = ed_with(&[]);
    let ops = draw_list(&e, VIEWPORT);
    assert_eq!(ops.len(), 1);
    let Op::Dots { at, size, color } = &ops[0] else {
        panic!("{ops:?}")
    };
    assert_eq!(*size, DOT_PX);
    assert_eq!(*color, palette::grid());
    // About 42 x 30 dots, one every 24 px.
    assert!(at.len() > 1000 && at.len() < 1500, "{}", at.len());
    for d in at {
        assert!(
            d.x > -3.0 && d.x < 1003.0 && d.y > -3.0 && d.y < 703.0,
            "{d:?}"
        );
    }
}

#[test]
fn the_grid_thins_out_when_zoomed_out_and_stays_bounded() {
    let mut e = ed_with(&[]);
    e.zoom_by(0.25);
    let ops = draw_list(&e, VIEWPORT);
    let Op::Dots { at, .. } = &ops[0] else {
        panic!()
    };
    let spacing = at[1].x - at[0].x;
    assert!(spacing >= GRID_MIN_PX - 1e-3, "{spacing}");
    assert!(at.len() < 4000, "{}", at.len());
    // Zoomed in the dots are a screen-pixel step per scene step.
    let mut e = ed_with(&[]);
    e.zoom_by(2.0);
    let ops = draw_list(&e, VIEWPORT);
    let Op::Dots { at, .. } = &ops[0] else {
        panic!()
    };
    assert!((at[1].x - at[0].x - 48.0).abs() < 1e-3);
}

#[test]
fn a_filled_rectangle_is_covered_by_triangles_of_its_area() {
    let style = Style {
        fill: Fill::Soft,
        ..Style::default()
    };
    let e = ed_with(&[(rect_kind(p(100., 100.), p(300., 200.)), style)]);
    let ops = draw_list(&e, VIEWPORT);
    let soft = tris_of(&ops, palette::token(Tone::Soft));
    assert!(!soft.is_empty());
    assert!((area(&soft) - 200.0 * 100.0).abs() < 1.0, "{}", area(&soft));
    // The stroke is ink and sits on the outline: its triangles stay near the box.
    let ink = tris_of(&ops, palette::token(Tone::Ink));
    assert!(!ink.is_empty());
    for t in &ink {
        for v in t {
            assert!(
                v.x > 95.0 && v.x < 305.0 && v.y > 95.0 && v.y < 205.0,
                "{v:?}"
            );
        }
    }
    // Fill under the stroke: it comes first.
    let first_fill = ops
        .iter()
        .position(|o| matches!(o, Op::Tris { color, .. } if *color == palette::token(Tone::Soft)));
    let first_ink = ops
        .iter()
        .position(|o| matches!(o, Op::Tris { color, .. } if *color == palette::token(Tone::Ink)));
    assert!(first_fill < first_ink);
}

#[test]
fn a_stroke_is_as_wide_as_the_style_says_at_any_zoom() {
    let line = Kind::Line {
        a: p(100., 300.),
        b: p(400., 300.),
    };
    for zoom in [1.0, 2.0] {
        let mut e = ed_with(&[(line.clone(), Style::default())]);
        e.zoom_by(zoom);
        e.wheel(p(0., 0.), p(0., 0.), NO);
        let ops = draw_list(&e, VIEWPORT);
        let ink = tris_of(&ops, palette::token(Tone::Ink));
        let ys: Vec<f32> = ink.iter().flatten().map(|v| v.y).collect();
        let (lo, hi) = ys
            .iter()
            .fold((f32::MAX, f32::MIN), |(l, h), y| (l.min(*y), h.max(*y)));
        // Thin is 2 scene units: 2 px at zoom 1, 4 px at zoom 2.
        assert!(
            (hi - lo - 2.0 * zoom).abs() < 0.05,
            "zoom {zoom}: {}",
            hi - lo
        );
    }
}

#[test]
fn a_shape_off_the_canvas_is_not_drawn() {
    let e = ed_with(&[
        (
            rect_kind(p(5000., 5000.), p(5100., 5100.)),
            Style::default(),
        ),
        (rect_kind(p(-900., 10.), p(-800., 90.)), Style::default()),
    ]);
    let ops = draw_list(&e, VIEWPORT);
    assert_eq!(ops.len(), 1, "only the grid");
    // One that reaches in is drawn.
    let e = ed_with(&[(rect_kind(p(-50., 100.), p(50., 200.)), Style::default())]);
    assert!(draw_list(&e, VIEWPORT).len() > 1);
}

#[test]
fn a_text_is_drawn_where_and_as_big_as_the_view_says() {
    let kind = Kind::Text {
        at: p(480., 330.),
        text: "hello\nworld".into(),
    };
    let mut e = ed_with(&[(kind, Style::default())]);
    e.zoom_by(2.0);
    let v = e.view();
    let ops = draw_list(&e, VIEWPORT);
    let text: Vec<&Op> = ops
        .iter()
        .filter(|o| matches!(o, Op::Text { .. }))
        .collect();
    assert_eq!(text.len(), 1);
    let Op::Text {
        at,
        text,
        size,
        color,
    } = text[0]
    else {
        unreachable!()
    };
    assert_eq!(text, "hello\nworld");
    assert!(at.dist(v.to_screen(p(480., 330.))) < 1e-3);
    assert!((size - 40.0).abs() < 1e-3, "20 units at 2x");
    assert_eq!(*color, palette::token(Tone::Ink));
}

#[test]
fn sketchy_changes_the_outline_and_the_same_scene_draws_the_same_twice() {
    let kind = rect_kind(p(100., 100.), p(300., 200.));
    let mut e = ed_with(&[(kind, Style::default())]);
    let plain = draw_list(&e, VIEWPORT);
    assert_eq!(plain, draw_list(&e, VIEWPORT));
    e.toggle_sketchy();
    let sketchy = draw_list(&e, VIEWPORT);
    assert_eq!(
        sketchy,
        draw_list(&e, VIEWPORT),
        "no frame counter in the wobble"
    );
    assert_ne!(plain, sketchy);
}

#[test]
fn selected_shapes_get_dashed_boxes_and_several_get_a_group_box() {
    let mut e = ed_with(&[
        (rect_kind(p(100., 100.), p(200., 200.)), Style::default()),
        (rect_kind(p(300., 100.), p(400., 200.)), Style::default()),
    ]);
    assert!(frames(&draw_list(&e, VIEWPORT)).is_empty());
    e.key("a", Some("a"), Mods { ctrl: true, ..NO });
    let f = frames(&draw_list(&e, VIEWPORT));
    assert_eq!(f.len(), 3, "two shapes and the group");
    let (first, _, edge) = f[0];
    assert!(edge.unwrap().dashed);
    assert_eq!(edge.unwrap().color, palette::selection());
    assert_eq!(first.min, p(94., 94.));
    assert_eq!(first.max, p(206., 206.));
    let (group, _, gedge) = f[2];
    assert!(!gedge.unwrap().dashed);
    assert!(group.contains_rect(&f[0].0) && group.contains_rect(&f[1].0));
}

#[test]
fn a_flat_line_still_gets_a_box_with_height() {
    let line = Kind::Line {
        a: p(100., 300.),
        b: p(300., 300.),
    };
    let mut e = ed_with(&[(line, Style::default())]);
    e.select_all();
    let f = frames(&draw_list(&e, VIEWPORT));
    assert!(f[0].0.height() >= 2.0 * SELECTION_PAD - 1e-3);
}

#[test]
fn the_marquee_is_a_dashed_frame_with_a_wash_while_it_is_dragged() {
    let mut e = ed_with(&[]);
    e.pointer_down(p(100., 100.), NO, 1, Button::Primary);
    e.pointer_move(p(300., 250.), NO);
    let f = frames(&draw_list(&e, VIEWPORT));
    assert_eq!(f.len(), 1);
    let (rect, fill, edge) = f[0];
    assert_eq!(rect.min, p(100., 100.));
    assert_eq!(rect.max, p(300., 250.));
    assert!(fill.unwrap().a > 0.0 && fill.unwrap().a < 0.5);
    assert!(edge.unwrap().dashed);
    e.pointer_up(p(300., 250.), NO);
    assert!(frames(&draw_list(&e, VIEWPORT)).is_empty());
}

#[test]
fn the_text_being_typed_has_a_frame_the_text_and_a_caret_and_hides_the_shape_it_edits() {
    let kind = Kind::Text {
        at: p(100., 100.),
        text: "abc".into(),
    };
    let mut e = ed_with(&[(kind, Style::default())]);
    let before = draw_list(&e, VIEWPORT);
    assert_eq!(
        before
            .iter()
            .filter(|o| matches!(o, Op::Text { .. }))
            .count(),
        1
    );
    // Double click on it to edit.
    let on_text = p(105., 112.);
    e.pointer_down(on_text, NO, 2, Button::Primary);
    e.pointer_up(on_text, NO);
    assert!(e.typing().is_some());
    e.key("x", Some("x"), NO);
    let ops = draw_list(&e, VIEWPORT);
    let texts: Vec<&Op> = ops
        .iter()
        .filter(|o| matches!(o, Op::Text { .. }))
        .collect();
    assert_eq!(texts.len(), 1, "the shape is hidden, the buffer is drawn");
    let Op::Text { text, .. } = texts[0] else {
        unreachable!()
    };
    assert_eq!(text, "abcx");
    let carets: Vec<&Op> = ops
        .iter()
        .filter(|o| matches!(o, Op::Caret { .. }))
        .collect();
    assert_eq!(carets.len(), 1);
    let Op::Caret { byte, .. } = carets[0] else {
        unreachable!()
    };
    assert_eq!(*byte, 4);
    assert!(frames(&ops).iter().any(|(_, _, e)| e.unwrap().dashed));
}

#[test]
fn a_new_text_is_drawn_in_the_colour_it_will_have() {
    let mut e = ed_with(&[]);
    e.set_tool(Tool::Text);
    e.pointer_down(p(200., 200.), NO, 1, Button::Primary);
    e.pointer_up(p(200., 200.), NO);
    e.set_color(crate::board::scene::StrokeColor::Danger);
    e.key("h", Some("h"), NO);
    let ops = draw_list(&e, VIEWPORT);
    let Some(Op::Text { color, size, .. }) = ops.iter().find(|o| matches!(o, Op::Text { .. }))
    else {
        panic!("{ops:?}")
    };
    assert_eq!(*color, palette::token(Tone::Danger));
    assert_eq!(*size, 20.0);
}
