//! Tests of the scene: hit tests, the edits, undo and redo, the JSON file.

use super::*;

fn p(x: f32, y: f32) -> Point {
    Point::new(x, y)
}

fn rect(a: Point, b: Point) -> Kind {
    Kind::Rect { a, b }
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-3
}

fn filled() -> Style {
    Style {
        fill: Fill::Soft,
        ..Style::default()
    }
}

#[test]
fn ids_are_unique_and_never_reused() {
    let mut s = Scene::new();
    let a = s.add(rect(p(0., 0.), p(10., 10.)), Style::default());
    let b = s.add(rect(p(0., 0.), p(10., 10.)), Style::default());
    assert_ne!(a, b);
    s.delete(&[b]);
    let c = s.add(rect(p(0., 0.), p(10., 10.)), Style::default());
    assert!(
        c != a && c != b,
        "a deleted id must not come back for another shape"
    );
}

#[test]
fn a_stroke_is_hit_within_six_pixels_and_not_beyond() {
    let mut s = Scene::new();
    let id = s.add(
        Kind::Line {
            a: p(0., 0.),
            b: p(100., 0.),
        },
        Style::default(),
    );
    // Thin is 2 px wide: the reach is 6 + 1.
    assert_eq!(s.hit_test(p(50., 6.5), 6.0), Some(id));
    assert_eq!(s.hit_test(p(50., 7.5), 6.0), None);
    // The end of the segment counts as a round cap.
    assert_eq!(s.hit_test(p(-5., 0.), 6.0), Some(id));
    assert_eq!(s.hit_test(p(-8., 0.), 6.0), None);
    // A thick stroke reaches further.
    s.restyle(&[id], |st| st.width = Width::Thick);
    assert_eq!(s.hit_test(p(50., 7.5), 6.0), Some(id));
}

#[test]
fn an_unfilled_rect_is_hit_on_its_edge_only_and_a_filled_one_inside_too() {
    let mut s = Scene::new();
    let id = s.add(rect(p(0., 0.), p(100., 60.)), Style::default());
    assert_eq!(s.hit_test(p(50., 0.), 6.0), Some(id), "top edge");
    assert_eq!(
        s.hit_test(p(103., 30.), 6.0),
        Some(id),
        "just outside the right edge"
    );
    assert_eq!(
        s.hit_test(p(50., 30.), 6.0),
        None,
        "the middle of an unfilled rect"
    );
    s.restyle(&[id], |st| st.fill = Fill::Panel);
    assert_eq!(
        s.hit_test(p(50., 30.), 6.0),
        Some(id),
        "the middle of a filled rect"
    );
    assert_eq!(s.hit_test(p(120., 30.), 6.0), None, "far outside");
}

#[test]
fn a_rect_dragged_backwards_is_hit_like_any_other() {
    let mut s = Scene::new();
    let id = s.add(rect(p(100., 60.), p(0., 0.)), Style::default());
    assert_eq!(s.hit_test(p(0., 30.), 6.0), Some(id));
}

#[test]
fn an_ellipse_is_hit_on_its_outline() {
    let mut s = Scene::new();
    let id = s.add(
        Kind::Ellipse {
            a: p(0., 0.),
            b: p(100., 60.),
        },
        Style::default(),
    );
    assert_eq!(s.hit_test(p(100., 30.), 6.0), Some(id), "right tip");
    assert_eq!(s.hit_test(p(50., 0.), 6.0), Some(id), "top");
    assert_eq!(
        s.hit_test(p(50., 30.), 6.0),
        None,
        "centre of an unfilled ellipse"
    );
    assert_eq!(
        s.hit_test(p(2., 2.), 6.0),
        None,
        "the box corner is outside the ellipse"
    );
    s.restyle(&[id], |st| st.fill = Fill::Soft);
    assert_eq!(
        s.hit_test(p(50., 30.), 6.0),
        Some(id),
        "centre of a filled ellipse"
    );
}

#[test]
fn a_freehand_stroke_and_a_text_are_hit() {
    let mut s = Scene::new();
    let f = s.add(
        Kind::Freehand {
            points: vec![p(0., 0.), p(50., 50.), p(100., 0.)],
        },
        Style::default(),
    );
    assert_eq!(s.hit_test(p(25., 25.), 6.0), Some(f));
    assert_eq!(s.hit_test(p(50., 10.), 6.0), None);
    let t = s.add(
        Kind::Text {
            at: p(200., 200.),
            text: "hello".into(),
        },
        Style::default(),
    );
    let b = s.get(t).unwrap().bounds();
    assert!(b.width() > 20.0 && b.height() > 10.0, "{b:?}");
    assert_eq!(s.hit_test(b.center(), 6.0), Some(t));
    assert_eq!(s.hit_test(p(b.max.x + 20., b.max.y + 20.), 6.0), None);
}

#[test]
fn the_topmost_shape_wins() {
    let mut s = Scene::new();
    let under = s.add(rect(p(0., 0.), p(100., 100.)), filled());
    let over = s.add(rect(p(50., 50.), p(150., 150.)), filled());
    assert_eq!(s.hit_test(p(75., 75.), 6.0), Some(over));
    assert_eq!(s.hit_test(p(10., 10.), 6.0), Some(under));
}

#[test]
fn moving_shifts_every_kind_and_undo_puts_it_back() {
    let mut s = Scene::new();
    let r = s.add(rect(p(0., 0.), p(10., 10.)), Style::default());
    let f = s.add(
        Kind::Freehand {
            points: vec![p(1., 1.), p(2., 3.)],
        },
        Style::default(),
    );
    let t = s.add(
        Kind::Text {
            at: p(5., 5.),
            text: "x".into(),
        },
        Style::default(),
    );
    let other = s.add(rect(p(100., 100.), p(110., 110.)), Style::default());
    s.move_by(&[r, f, t], p(10., -5.));
    assert_eq!(s.get(r).unwrap().kind, rect(p(10., -5.), p(20., 5.)));
    assert_eq!(
        s.get(f).unwrap().kind,
        Kind::Freehand {
            points: vec![p(11., -4.), p(12., -2.)]
        }
    );
    assert_eq!(
        s.get(t).unwrap().kind,
        Kind::Text {
            at: p(15., 0.),
            text: "x".into()
        }
    );
    assert_eq!(
        s.get(other).unwrap().kind,
        rect(p(100., 100.), p(110., 110.)),
        "unselected stays"
    );
    assert!(s.undo());
    assert_eq!(s.get(r).unwrap().kind, rect(p(0., 0.), p(10., 10.)));
    assert!(s.redo());
    assert_eq!(s.get(r).unwrap().kind, rect(p(10., -5.), p(20., 5.)));
}

#[test]
fn undo_and_redo_walk_the_history_and_a_new_edit_ends_the_redo() {
    let mut s = Scene::new();
    assert!(!s.undo() && !s.redo());
    let a = s.add(rect(p(0., 0.), p(1., 1.)), Style::default());
    let b = s.add(rect(p(0., 0.), p(2., 2.)), Style::default());
    assert_eq!(s.len(), 2);
    assert!(s.undo());
    assert_eq!(s.ids(), vec![a]);
    assert!(s.can_redo());
    assert!(s.redo());
    assert_eq!(s.ids(), vec![a, b]);
    assert!(s.undo() && s.undo());
    assert!(s.is_empty() && !s.can_undo());
    s.redo();
    s.add(rect(p(0., 0.), p(3., 3.)), Style::default());
    assert!(!s.can_redo(), "an edit after an undo drops the redo stack");
}

#[test]
fn history_keeps_two_hundred_steps() {
    let mut s = Scene::new();
    for i in 0..250 {
        s.add(rect(p(0., 0.), p(i as f32, 1.)), Style::default());
    }
    let mut undone = 0;
    while s.undo() {
        undone += 1;
    }
    assert_eq!(undone, HISTORY_CAP);
    assert_eq!(s.len(), 250 - HISTORY_CAP);
}

#[test]
fn a_transaction_is_one_step_and_a_no_op_leaves_no_step() {
    let mut s = Scene::new();
    s.begin();
    let id = s.add(rect(p(0., 0.), p(0., 0.)), Style::default());
    for i in 1..=20 {
        s.set_kind(id, rect(p(0., 0.), p(i as f32, i as f32)));
    }
    assert!(s.commit());
    assert!(s.undo());
    assert!(s.is_empty(), "the whole drag was one step");
    s.redo();
    s.begin();
    s.move_by(&[id], p(0., 0.));
    assert!(!s.commit(), "nothing changed");
    assert!(s.undo(), "the previous step is still the top of the stack");
    assert!(s.is_empty());
}

#[test]
fn cancel_puts_back_what_the_transaction_saw() {
    let mut s = Scene::new();
    let id = s.add(rect(p(0., 0.), p(10., 10.)), Style::default());
    s.begin();
    s.move_by(&[id], p(50., 50.));
    s.delete(&[id]);
    s.cancel();
    assert_eq!(s.get(id).unwrap().kind, rect(p(0., 0.), p(10., 10.)));
    assert!(!s.in_transaction());
}

#[test]
fn nothing_is_undone_in_the_middle_of_a_drag() {
    let mut s = Scene::new();
    s.add(rect(p(0., 0.), p(10., 10.)), Style::default());
    s.begin();
    assert!(!s.undo());
    s.cancel();
    assert!(s.undo());
}

#[test]
fn duplicate_copies_on_top_with_new_ids_and_an_offset() {
    let mut s = Scene::new();
    let a = s.add(
        rect(p(0., 0.), p(10., 10.)),
        Style {
            color: StrokeColor::Danger,
            ..Style::default()
        },
    );
    let b = s.add(
        Kind::Line {
            a: p(0., 0.),
            b: p(5., 5.),
        },
        Style::default(),
    );
    let copies = s.duplicate(&[a, b], p(16., 16.));
    assert_eq!(copies.len(), 2);
    assert_eq!(s.len(), 4);
    assert!(copies.iter().all(|c| *c != a && *c != b));
    assert_eq!(
        s.get(copies[0]).unwrap().kind,
        rect(p(16., 16.), p(26., 26.))
    );
    assert_eq!(s.get(copies[0]).unwrap().style.color, StrokeColor::Danger);
    assert_eq!(
        s.get(a).unwrap().kind,
        rect(p(0., 0.), p(10., 10.)),
        "the original stays"
    );
    assert_eq!(s.ids()[2..], copies[..], "the copies are on top, in order");
    assert!(s.undo());
    assert_eq!(s.len(), 2, "a duplicate is one step");
}

#[test]
fn delete_and_clear_are_undoable() {
    let mut s = Scene::new();
    let a = s.add(rect(p(0., 0.), p(10., 10.)), Style::default());
    let b = s.add(rect(p(0., 0.), p(20., 20.)), Style::default());
    s.delete(&[a]);
    assert_eq!(s.ids(), vec![b]);
    s.clear();
    assert!(s.is_empty());
    assert!(s.undo());
    assert_eq!(s.ids(), vec![b]);
    assert!(s.undo());
    assert_eq!(s.ids(), vec![a, b]);
}

#[test]
fn bounds_cover_every_shape_with_its_stroke() {
    let mut s = Scene::new();
    assert_eq!(s.bounds(), None);
    s.add(rect(p(0., 0.), p(10., 10.)), Style::default());
    s.add(
        Kind::Line {
            a: p(-20., 5.),
            b: p(30., 50.),
        },
        Style::default(),
    );
    let b = s.bounds().unwrap();
    assert!(
        b.min.x <= -20.0 && b.min.y <= 0.0 && b.max.x >= 30.0 && b.max.y >= 50.0,
        "{b:?}"
    );
    assert!(b.min.x > -30.0 && b.max.y < 60.0, "{b:?}");
}

#[test]
fn a_marquee_takes_only_the_shapes_wholly_inside() {
    let mut s = Scene::new();
    let inside = s.add(rect(p(10., 10.), p(20., 20.)), Style::default());
    let _across = s.add(rect(p(10., 10.), p(200., 20.)), Style::default());
    let also = s.add(
        Kind::Freehand {
            points: vec![p(30., 30.), p(40., 35.)],
        },
        Style::default(),
    );
    let hit = s.shapes_in(&Rect::new(p(0., 0.), p(100., 100.)));
    assert_eq!(hit, vec![inside, also]);
}

#[test]
fn restyle_and_set_text_change_only_what_they_name() {
    let mut s = Scene::new();
    let a = s.add(rect(p(0., 0.), p(10., 10.)), Style::default());
    let b = s.add(
        Kind::Text {
            at: p(0., 0.),
            text: "a".into(),
        },
        Style::default(),
    );
    s.restyle(&[a], |st| st.color = StrokeColor::Neon);
    assert_eq!(s.get(a).unwrap().style.color, StrokeColor::Neon);
    assert_eq!(s.get(b).unwrap().style.color, StrokeColor::Ink);
    s.set_text(b, "hello\nworld".into());
    assert_eq!(
        s.get(b).unwrap().kind,
        Kind::Text {
            at: p(0., 0.),
            text: "hello\nworld".into()
        }
    );
    s.set_text(a, "nope".into());
    assert_eq!(
        s.get(a).unwrap().kind,
        rect(p(0., 0.), p(10., 10.)),
        "a rect has no text"
    );
}

#[test]
fn the_scene_round_trips_through_json() {
    let mut s = Scene::new();
    s.sketchy = true;
    s.add(
        rect(p(1.5, 2.25), p(100., 60.)),
        Style {
            color: StrokeColor::Pink,
            width: Width::Thick,
            fill: Fill::Soft,
        },
    );
    s.add(
        Kind::Ellipse {
            a: p(0., 0.),
            b: p(30., 40.),
        },
        Style::default(),
    );
    s.add(
        Kind::Arrow {
            a: p(1., 1.),
            b: p(9., 9.),
        },
        Style {
            color: StrokeColor::Success,
            ..Style::default()
        },
    );
    s.add(
        Kind::Freehand {
            points: vec![p(0., 0.), p(1., 2.), p(3., 4.)],
        },
        Style::default(),
    );
    s.add(
        Kind::Text {
            at: p(7., 8.),
            text: "two\nlines \"quoted\"".into(),
        },
        Style::default(),
    );
    s.next_export_name();
    let json = s.to_json();
    assert!(
        json.contains("\"type\":\"rect\"") && json.contains("[1.5,2.25]"),
        "{json}"
    );
    let mut back = Scene::from_json(&json).unwrap();
    assert_eq!(back.shapes(), s.shapes());
    assert!(back.sketchy);
    assert_eq!(
        back.next_export_name(),
        "diagram-2.png",
        "the export count is saved"
    );
    // New shapes keep getting fresh ids.
    let used = back.ids();
    let id = back.add(rect(p(0., 0.), p(1., 1.)), Style::default());
    assert!(!used.contains(&id));
    // The history is not saved.
    assert!(!Scene::from_json(&json).unwrap().can_undo());
}

#[test]
fn bad_json_is_refused_or_repaired() {
    assert!(Scene::from_json("not json").is_err());
    assert!(Scene::from_json("{}").is_err());
    let err = Scene::from_json(r#"{"version":99,"shapes":[]}"#).unwrap_err();
    assert!(err.contains("newer"), "{err}");
    // Repeated and zero ids are repaired, not refused.
    let s = Scene::from_json(
        r#"{"version":1,"shapes":[
            {"id":1,"kind":{"type":"rect","a":[0,0],"b":[5,5]},"style":{"color":"ink","width":"thin"}},
            {"id":1,"kind":{"type":"line","a":[0,0],"b":[5,5]},"style":{"color":"neon","width":"thick","fill":"panel"}},
            {"id":0,"kind":{"type":"text","at":[0,0],"text":"t"},"style":{"color":"ink","width":"thin"}}
        ]}"#,
    )
    .unwrap();
    let ids = s.ids();
    assert_eq!(ids.len(), 3);
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        3,
        "repeated and zero ids are renumbered: {ids:?}"
    );
    // `null` is what JSON makes of a NaN: the file is not a scene any more (the store keeps it aside).
    assert!(Scene::from_json(
        r#"{"version":1,"shapes":[{"id":1,"kind":{"type":"rect","a":[0,null],"b":[5,5]},"style":{"color":"ink","width":"thin"}}]}"#
    )
    .is_err());
}

#[test]
fn a_shape_that_is_not_a_number_is_not_saved() {
    let mut s = Scene::new();
    s.add(rect(p(0., 0.), p(5., 5.)), Style::default());
    s.add(rect(p(f32::NAN, 0.), p(5., 5.)), Style::default());
    s.add(
        Kind::Freehand {
            points: vec![p(0., 0.), p(f32::INFINITY, 1.)],
        },
        Style::default(),
    );
    let back = Scene::from_json(&s.to_json()).unwrap();
    assert_eq!(back.len(), 1);
}

#[test]
fn arrow_heads_point_at_the_end() {
    let [tip, l, r] = arrow_head(p(0., 0.), p(100., 0.), 2.0).unwrap();
    assert_eq!(tip, p(100., 0.));
    assert!(l.x < 100.0 && r.x < 100.0 && near(l.x, r.x));
    assert!(near(l.y, -r.y) && l.y.abs() > 2.0);
    assert!(arrow_head(p(5., 5.), p(5., 5.), 2.0).is_none());
    // A short arrow gets a head that fits.
    let [_, l, _] = arrow_head(p(0., 0.), p(10., 0.), 2.0).unwrap();
    assert!(l.x >= 3.9, "{l:?}");
}
