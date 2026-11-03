//! Tests of the editor: what the pointer and the keys do to the scene, the selection and the view.

use super::*;

fn p(x: f32, y: f32) -> Point {
    Point::new(x, y)
}

const NO: Mods = Mods {
    ctrl: false,
    shift: false,
    alt: false,
};
const SHIFT: Mods = Mods {
    ctrl: false,
    shift: true,
    alt: false,
};
const CTRL: Mods = Mods {
    ctrl: true,
    shift: false,
    alt: false,
};
const CTRL_SHIFT: Mods = Mods {
    ctrl: true,
    shift: true,
    alt: false,
};

/// An editor over an empty scene and a 1000 x 700 canvas.
fn ed() -> Editor {
    let mut e = Editor::new(Scene::new());
    e.set_viewport(p(1000.0, 700.0));
    e
}

/// Drags the left button from `a` to `b` through the middle.
fn drag(e: &mut Editor, a: Point, b: Point, mods: Mods) {
    e.pointer_down(a, mods, 1, Button::Primary);
    e.pointer_move(a.lerp(b, 0.5), mods);
    e.pointer_move(b, mods);
    e.pointer_up(b, mods);
}

fn click(e: &mut Editor, at: Point) {
    e.pointer_down(at, NO, 1, Button::Primary);
    e.pointer_up(at, NO);
}

fn tap(e: &mut Editor, key: &str) -> Keyed {
    let ch = (key.chars().count() == 1).then_some(key);
    e.key(key, ch, NO)
}

/// Draws a rectangle with the rectangle tool and returns its id.
fn rect(e: &mut Editor, a: Point, b: Point) -> u32 {
    e.set_tool(Tool::Rect);
    drag(e, a, b, NO);
    e.selection()[0]
}

fn kinds(e: &Editor) -> Vec<Kind> {
    e.scene().shapes().iter().map(|s| s.kind.clone()).collect()
}

// ---- tools and keys -------------------------------------------------------------------------

#[test]
fn every_tool_has_its_number_and_letter() {
    let keys = [
        ("1", Tool::Select),
        ("v", Tool::Select),
        ("2", Tool::Rect),
        ("r", Tool::Rect),
        ("3", Tool::Ellipse),
        ("o", Tool::Ellipse),
        ("4", Tool::Arrow),
        ("a", Tool::Arrow),
        ("5", Tool::Line),
        ("l", Tool::Line),
        ("6", Tool::Freehand),
        ("p", Tool::Freehand),
        ("7", Tool::Text),
        ("t", Tool::Text),
    ];
    for (key, tool) in keys {
        assert_eq!(Tool::from_key(key), Some(tool), "{key}");
        let mut e = ed();
        assert_eq!(tap(&mut e, key), Keyed::Done);
        assert_eq!(e.tool(), tool, "{key}");
    }
    assert_eq!(Tool::from_key("8"), None);
    assert_eq!(Tool::from_key("escape"), None);
    assert_eq!(Tool::from_key(""), None);
    assert_eq!(Tool::from_key("R"), Some(Tool::Rect));
    // Names, numbers and letters are all distinct.
    for (i, a) in Tool::ALL.iter().enumerate() {
        for b in &Tool::ALL[i + 1..] {
            assert_ne!(a.name(), b.name());
            assert_ne!(a.number(), b.number());
            assert_ne!(a.letter(), b.letter());
        }
    }
}

#[test]
fn escape_unwinds_one_thing_at_a_time_and_then_closes() {
    let mut e = ed();
    let id = rect(&mut e, p(10., 10.), p(110., 60.));
    assert_eq!(e.selection(), [id]);
    e.set_tool(Tool::Arrow);
    assert!(
        e.selection().is_empty(),
        "a drawing tool drops the selection"
    );
    assert_eq!(tap(&mut e, "escape"), Keyed::Done);
    assert_eq!(e.tool(), Tool::Select);
    e.select_all();
    assert_eq!(tap(&mut e, "escape"), Keyed::Done);
    assert!(e.selection().is_empty());
    assert_eq!(tap(&mut e, "escape"), Keyed::Do(Command::Close));
}

#[test]
fn keys_the_board_does_not_know_pass_through() {
    let mut e = ed();
    assert_eq!(tap(&mut e, "f5"), Keyed::Ignored);
    assert_eq!(tap(&mut e, "8"), Keyed::Ignored);
    assert_eq!(e.key("q", Some("q"), CTRL), Keyed::Ignored);
    assert_eq!(
        e.key("r", Some("r"), Mods { alt: true, ..NO }),
        Keyed::Ignored
    );
}

// ---- drawing --------------------------------------------------------------------------------

#[test]
fn dragging_a_rectangle_adds_it_selects_it_and_returns_to_select() {
    let mut e = ed();
    let id = rect(&mut e, p(100., 100.), p(300., 200.));
    assert_eq!(
        kinds(&e),
        vec![Kind::Rect {
            a: p(100., 100.),
            b: p(300., 200.)
        }]
    );
    assert_eq!(e.selection(), [id]);
    assert_eq!(e.tool(), Tool::Select);
    assert!(e.can_undo());
    e.key("z", Some("z"), CTRL);
    assert!(e.scene().is_empty(), "the whole drag was one undo step");
    assert!(e.selection().is_empty(), "the undone shape is not selected");
}

#[test]
fn a_click_with_a_shape_tool_draws_nothing() {
    let mut e = ed();
    e.set_tool(Tool::Rect);
    click(&mut e, p(50., 50.));
    // Inside the slop: no shape either.
    e.pointer_down(p(50., 50.), NO, 1, Button::Primary);
    e.pointer_move(p(51., 52.), NO);
    e.pointer_up(p(51., 52.), NO);
    assert!(e.scene().is_empty());
    assert!(!e.can_undo());
    assert_eq!(e.edits(), 0);
    assert_eq!(e.tool(), Tool::Rect, "the tool stays armed");
}

#[test]
fn every_shape_tool_makes_its_kind() {
    let (a, b) = (p(20., 30.), p(220., 130.));
    for (tool, want) in [
        (Tool::Ellipse, Kind::Ellipse { a, b }),
        (Tool::Arrow, Kind::Arrow { a, b }),
        (Tool::Line, Kind::Line { a, b }),
    ] {
        let mut e = ed();
        e.set_tool(tool);
        drag(&mut e, a, b, NO);
        assert_eq!(kinds(&e), vec![want], "{tool:?}");
    }
}

#[test]
fn shift_makes_squares_and_circles() {
    assert_eq!(
        constrain(Tool::Rect, p(10., 10.), p(110., 40.), true),
        p(110., 110.)
    );
    assert_eq!(
        constrain(Tool::Ellipse, p(10., 10.), p(-30., 50.), true),
        p(-30., 50.),
        "the longer side wins, signs kept"
    );
    assert_eq!(
        constrain(Tool::Ellipse, p(10., 10.), p(40., -100.), true),
        p(120., -100.)
    );
    assert_eq!(
        constrain(Tool::Rect, p(10., 10.), p(110., 40.), false),
        p(110., 40.)
    );
    let mut e = ed();
    e.set_tool(Tool::Rect);
    drag(&mut e, p(100., 100.), p(300., 150.), SHIFT);
    assert_eq!(
        kinds(&e),
        vec![Kind::Rect {
            a: p(100., 100.),
            b: p(300., 300.)
        }]
    );
}

#[test]
fn shift_snaps_lines_to_45_degrees_and_keeps_their_length() {
    let start = p(0., 0.);
    // Nearly horizontal: horizontal.
    let end = constrain(Tool::Line, start, p(100., 10.), true);
    assert!(end.y.abs() < 1e-3, "{end:?}");
    assert!((end.x - p(100., 10.).len()).abs() < 1e-3);
    // Nearly diagonal: diagonal.
    let end = constrain(Tool::Arrow, start, p(80., 90.), true);
    assert!((end.x - end.y).abs() < 1e-3, "{end:?}");
    assert!((end.len() - p(80., 90.).len()).abs() < 1e-3);
    // Nearly vertical, upwards.
    let end = constrain(Tool::Line, start, p(5., -100.), true);
    assert!(end.x.abs() < 1e-3 && end.y < 0.0, "{end:?}");
    // Other tools are left alone.
    assert_eq!(constrain(Tool::Freehand, start, p(3., 4.), true), p(3., 4.));
}

#[test]
fn shapes_are_drawn_in_scene_coordinates_whatever_the_view() {
    let mut e = ed();
    // Zoom in around the canvas centre, then pan.
    e.zoom_by(2.0);
    e.wheel(p(500., 350.), p(30., -20.), NO);
    let v = e.view();
    assert!((v.zoom - 2.0).abs() < 1e-6);
    e.set_tool(Tool::Rect);
    drag(&mut e, p(200., 200.), p(400., 300.), NO);
    let Kind::Rect { a, b } = kinds(&e)[0].clone() else {
        panic!("not a rectangle")
    };
    assert!(a.dist(v.to_scene(p(200., 200.))) < 1e-3);
    assert!(b.dist(v.to_scene(p(400., 300.))) < 1e-3);
    assert!(
        (b.x - a.x - 100.0).abs() < 1e-3,
        "200 px at 2x is 100 units"
    );
}

#[test]
fn freehand_collects_the_pointer_path_and_a_click_is_a_dot() {
    let mut e = ed();
    e.set_tool(Tool::Freehand);
    e.pointer_down(p(10., 10.), NO, 1, Button::Primary);
    for i in 1..=20 {
        e.pointer_move(
            p(10. + i as f32 * 5., 10. + (i as f32 * 0.5).sin() * 8.),
            NO,
        );
    }
    e.pointer_up(p(110., 10.), NO);
    let Kind::Freehand { points } = kinds(&e)[0].clone() else {
        panic!("not a scribble")
    };
    assert!(points.len() >= 20, "{}", points.len());
    assert_eq!(points[0], p(10., 10.));
    assert_eq!(e.tool(), Tool::Freehand, "the pen stays armed");
    assert!(e.selection().is_empty(), "a scribble is not selected");
    // A click is a dot.
    click(&mut e, p(300., 300.));
    assert_eq!(e.scene().len(), 2);
    assert_eq!(
        kinds(&e)[1],
        Kind::Freehand {
            points: vec![p(300., 300.)]
        }
    );
}

#[test]
fn points_closer_than_a_pixel_and_a_half_are_not_kept() {
    let mut e = ed();
    e.set_tool(Tool::Freehand);
    e.pointer_down(p(0., 0.), NO, 1, Button::Primary);
    e.pointer_move(p(10., 0.), NO);
    for _ in 0..50 {
        e.pointer_move(p(10.5, 0.), NO);
    }
    e.pointer_up(p(10.5, 0.), NO);
    let Kind::Freehand { points } = kinds(&e)[0].clone() else {
        panic!()
    };
    assert_eq!(points.len(), 2, "{points:?}");
}

#[test]
fn new_shapes_take_the_current_style() {
    let mut e = ed();
    e.set_color(StrokeColor::Danger);
    e.set_width(Width::Thick);
    e.set_fill(Fill::Soft);
    let id = rect(&mut e, p(0., 0.), p(100., 100.));
    let s = e.scene().get(id).unwrap().style;
    assert_eq!(
        s,
        Style {
            color: StrokeColor::Danger,
            width: Width::Thick,
            fill: Fill::Soft
        }
    );
}

// ---- selecting and moving -------------------------------------------------------------------

#[test]
fn a_click_selects_the_shape_under_it_and_a_click_on_nothing_clears() {
    let mut e = ed();
    let a = rect(&mut e, p(100., 100.), p(200., 200.));
    let b = rect(&mut e, p(400., 100.), p(500., 200.));
    e.select_all();
    click(&mut e, p(100., 150.));
    assert_eq!(
        e.selection(),
        [a],
        "a click on an edge, from a bigger selection"
    );
    click(&mut e, p(502., 150.));
    assert_eq!(e.selection(), [b], "the slop reaches the edge from outside");
    click(&mut e, p(700., 600.));
    assert!(e.selection().is_empty());
    // The inside of an unfilled rectangle is not a hit.
    click(&mut e, p(150., 150.));
    assert!(e.selection().is_empty());
}

#[test]
fn the_hit_slack_is_in_screen_pixels() {
    let mut e = ed();
    rect(&mut e, p(100., 100.), p(200., 200.));
    // Zoomed out to 0.25: 6 screen pixels are 24 scene units, but the shape's edge is also
    // 4x closer to the pointer. A click 5 px (screen) off the edge hits at any zoom.
    e.zoom_reset();
    e.zoom_by(0.25);
    let v = e.view();
    let edge = v.to_screen(p(100., 150.));
    click(&mut e, edge + p(-5., 0.));
    assert_eq!(e.selection().len(), 1);
    click(&mut e, edge + p(-40., 0.));
    assert!(e.selection().is_empty());
}

#[test]
fn shift_click_adds_and_removes() {
    let mut e = ed();
    let a = rect(&mut e, p(100., 100.), p(200., 200.));
    let b = rect(&mut e, p(400., 100.), p(500., 200.));
    click(&mut e, p(100., 150.));
    e.pointer_down(p(400., 150.), SHIFT, 1, Button::Primary);
    e.pointer_up(p(400., 150.), SHIFT);
    assert_eq!(e.selection(), [a, b]);
    e.pointer_down(p(100., 150.), SHIFT, 1, Button::Primary);
    e.pointer_up(p(100., 150.), SHIFT);
    assert_eq!(e.selection(), [b]);
}

#[test]
fn dragging_a_shape_moves_it_in_one_undo_step() {
    let mut e = ed();
    let id = rect(&mut e, p(100., 100.), p(200., 200.));
    let before = e.edits();
    drag(&mut e, p(100., 150.), p(160., 190.), NO);
    assert_eq!(
        e.scene().get(id).unwrap().kind,
        Kind::Rect {
            a: p(160., 140.),
            b: p(260., 240.)
        }
    );
    assert!(e.edits() > before);
    e.key("z", Some("z"), CTRL);
    assert_eq!(
        e.scene().get(id).unwrap().kind,
        Kind::Rect {
            a: p(100., 100.),
            b: p(200., 200.)
        },
        "one undo puts it back"
    );
    e.key("z", Some("z"), CTRL_SHIFT);
    assert_eq!(
        e.scene().get(id).unwrap().kind,
        Kind::Rect {
            a: p(160., 140.),
            b: p(260., 240.)
        }
    );
}

#[test]
fn a_press_that_hardly_moves_is_a_click_not_a_move() {
    let mut e = ed();
    let id = rect(&mut e, p(100., 100.), p(200., 200.));
    let before = e.edits();
    e.pointer_down(p(100., 150.), NO, 1, Button::Primary);
    e.pointer_move(p(101., 151.), NO);
    e.pointer_up(p(101., 151.), NO);
    assert_eq!(e.edits(), before);
    assert_eq!(
        e.scene().get(id).unwrap().kind,
        Kind::Rect {
            a: p(100., 100.),
            b: p(200., 200.)
        }
    );
}

#[test]
fn moving_takes_the_whole_selection_and_a_plain_click_narrows_it() {
    let mut e = ed();
    let a = rect(&mut e, p(100., 100.), p(200., 200.));
    let b = rect(&mut e, p(400., 100.), p(500., 200.));
    e.select_all();
    drag(&mut e, p(100., 150.), p(100., 250.), NO);
    for id in [a, b] {
        assert_eq!(e.scene().get(id).unwrap().bounds().min.y, 200.0);
    }
    assert_eq!(e.selection(), [a, b], "a drag keeps the selection");
    click(&mut e, p(100., 250.));
    assert_eq!(
        e.selection(),
        [a],
        "a click without a drag keeps only that shape"
    );
}

#[test]
fn a_marquee_selects_what_it_holds_whole() {
    let mut e = ed();
    let a = rect(&mut e, p(100., 100.), p(200., 200.));
    let b = rect(&mut e, p(400., 100.), p(500., 200.));
    let c = rect(&mut e, p(250., 400.), p(350., 500.));
    drag(&mut e, p(50., 50.), p(520., 250.), NO);
    assert_eq!(e.selection(), [a, b]);
    // Half a shape is not enough.
    drag(&mut e, p(50., 50.), p(150., 250.), NO);
    assert!(e.selection().is_empty());
    // Shift keeps what was selected.
    drag(&mut e, p(50., 50.), p(520., 250.), NO);
    e.pointer_down(p(200., 380.), SHIFT, 1, Button::Primary);
    e.pointer_move(p(400., 520.), SHIFT);
    assert!(e.marquee().is_some());
    e.pointer_up(p(400., 520.), SHIFT);
    assert!(e.marquee().is_none());
    assert_eq!(e.selection(), [a, b, c]);
}

#[test]
fn the_marquee_is_in_scene_units() {
    let mut e = ed();
    e.zoom_by(2.0);
    let v = e.view();
    e.pointer_down(p(100., 100.), NO, 1, Button::Primary);
    e.pointer_move(p(300., 200.), NO);
    let m = e.marquee().unwrap();
    assert!(m.min.dist(v.to_scene(p(100., 100.))) < 1e-3);
    assert!(m.max.dist(v.to_scene(p(300., 200.))) < 1e-3);
}

#[test]
fn escape_takes_back_a_drag() {
    let mut e = ed();
    let id = rect(&mut e, p(100., 100.), p(200., 200.));
    // A move.
    e.pointer_down(p(100., 150.), NO, 1, Button::Primary);
    e.pointer_move(p(150., 250.), NO);
    assert_ne!(
        e.scene().get(id).unwrap().bounds().min,
        p(100., 100.),
        "it follows the pointer"
    );
    assert_eq!(tap(&mut e, "escape"), Keyed::Done);
    assert_eq!(e.scene().get(id).unwrap().bounds().min, p(100., 100.));
    e.pointer_up(p(150., 250.), NO);
    assert_eq!(e.scene().get(id).unwrap().bounds().min, p(100., 100.));
    // A new shape.
    e.set_tool(Tool::Ellipse);
    e.pointer_down(p(300., 300.), NO, 1, Button::Primary);
    e.pointer_move(p(400., 400.), NO);
    assert_eq!(e.scene().len(), 2);
    tap(&mut e, "escape");
    assert_eq!(e.scene().len(), 1);
    assert!(!e.scene().in_transaction());
}

// ---- keys on the selection --------------------------------------------------------------------

#[test]
fn delete_and_backspace_delete_the_selection() {
    for key in ["delete", "backspace"] {
        let mut e = ed();
        rect(&mut e, p(100., 100.), p(200., 200.));
        rect(&mut e, p(400., 100.), p(500., 200.));
        assert_eq!(e.selection().len(), 1);
        tap(&mut e, key);
        assert_eq!(e.scene().len(), 1, "{key}");
        assert!(e.selection().is_empty());
        e.key("z", Some("z"), CTRL);
        assert_eq!(e.scene().len(), 2, "undo brings it back");
    }
}

#[test]
fn ctrl_a_selects_everything() {
    let mut e = ed();
    rect(&mut e, p(100., 100.), p(200., 200.));
    rect(&mut e, p(400., 100.), p(500., 200.));
    assert_eq!(e.key("a", Some("a"), CTRL), Keyed::Done);
    assert_eq!(e.selection().len(), 2);
    assert_eq!(e.tool(), Tool::Select, "ctrl-a is not the arrow tool");
}

#[test]
fn ctrl_d_duplicates_beside_and_selects_the_copy() {
    let mut e = ed();
    let a = rect(&mut e, p(100., 100.), p(200., 200.));
    e.key("d", Some("d"), CTRL);
    assert_eq!(e.scene().len(), 2);
    let copy = e.selection()[0];
    assert_ne!(copy, a);
    assert_eq!(
        e.scene().get(copy).unwrap().kind,
        Kind::Rect {
            a: p(116., 116.),
            b: p(216., 216.)
        }
    );
    e.key("z", Some("z"), CTRL);
    assert_eq!(e.scene().len(), 1);
}

#[test]
fn copy_and_paste_step_away_from_the_original() {
    let mut e = ed();
    let a = rect(&mut e, p(100., 100.), p(200., 200.));
    assert_eq!(e.key("c", Some("c"), CTRL), Keyed::Done);
    assert_eq!(e.scene().len(), 1, "copying changes nothing");
    e.key("v", Some("v"), CTRL);
    e.key("v", Some("v"), CTRL);
    assert_eq!(e.scene().len(), 3);
    let mins: Vec<Point> = e.scene().shapes().iter().map(|s| s.bounds().min).collect();
    assert_eq!(mins, vec![p(100., 100.), p(116., 116.), p(132., 132.)]);
    assert_eq!(e.selection().len(), 1, "the last paste is selected");
    assert_ne!(e.selection()[0], a);
    // A new copy starts over.
    e.key("c", Some("c"), CTRL);
    e.key("v", Some("v"), CTRL);
    assert_eq!(
        e.scene().shapes().last().unwrap().bounds().min,
        p(148., 148.)
    );
}

#[test]
fn cut_copies_and_deletes() {
    let mut e = ed();
    rect(&mut e, p(100., 100.), p(200., 200.));
    e.key("x", Some("x"), CTRL);
    assert!(e.scene().is_empty());
    e.key("v", Some("v"), CTRL);
    assert_eq!(e.scene().len(), 1);
}

#[test]
fn ctrl_c_with_nothing_selected_copies_the_picture() {
    let mut e = ed();
    rect(&mut e, p(100., 100.), p(200., 200.));
    e.select_all();
    assert_eq!(
        e.key("c", Some("c"), CTRL),
        Keyed::Done,
        "shapes go to the board's clipboard"
    );
    assert_eq!(
        e.key("c", Some("c"), CTRL_SHIFT),
        Keyed::Do(Command::CopyImage),
        "ctrl-shift-c is always the picture"
    );
    tap(&mut e, "escape");
    assert_eq!(e.key("c", Some("c"), CTRL), Keyed::Do(Command::CopyImage));
}

#[test]
fn ctrl_enter_asks_to_send() {
    let mut e = ed();
    assert_eq!(e.key("enter", None, CTRL), Keyed::Do(Command::Send));
}

#[test]
fn arrow_keys_nudge_and_a_run_of_them_is_one_undo_step() {
    let mut e = ed();
    let id = rect(&mut e, p(100., 100.), p(200., 200.));
    for _ in 0..5 {
        tap(&mut e, "right");
    }
    e.key("down", None, SHIFT);
    e.key_up("down");
    assert_eq!(
        e.scene().get(id).unwrap().kind,
        Kind::Rect {
            a: p(105., 110.),
            b: p(205., 210.)
        }
    );
    // Another key closes the run: undo takes the whole run back.
    e.key("z", Some("z"), CTRL);
    assert_eq!(
        e.scene().get(id).unwrap().kind,
        Kind::Rect {
            a: p(100., 100.),
            b: p(200., 200.)
        }
    );
    // With nothing selected the arrows do nothing.
    tap(&mut e, "escape");
    let edits = e.edits();
    tap(&mut e, "left");
    assert_eq!(e.edits(), edits);
}

#[test]
fn undo_and_redo_keys() {
    let mut e = ed();
    rect(&mut e, p(100., 100.), p(200., 200.));
    rect(&mut e, p(300., 100.), p(400., 200.));
    e.key("z", Some("z"), CTRL);
    assert_eq!(e.scene().len(), 1);
    e.key("y", Some("y"), CTRL);
    assert_eq!(e.scene().len(), 2);
    e.key("z", Some("z"), CTRL);
    e.key("z", Some("z"), CTRL_SHIFT);
    assert_eq!(e.scene().len(), 2);
    assert!(e.can_undo() && !e.can_redo());
}

#[test]
fn clear_empties_the_board_in_one_undoable_step() {
    let mut e = ed();
    rect(&mut e, p(100., 100.), p(200., 200.));
    rect(&mut e, p(300., 100.), p(400., 200.));
    e.clear();
    assert!(e.scene().is_empty());
    assert!(e.selection().is_empty());
    e.key("z", Some("z"), CTRL);
    assert_eq!(e.scene().len(), 2);
}

// ---- style ----------------------------------------------------------------------------------

#[test]
fn style_buttons_restyle_the_selection_and_the_next_shape() {
    let mut e = ed();
    let a = rect(&mut e, p(100., 100.), p(200., 200.));
    e.set_color(StrokeColor::Pink);
    e.set_fill(Fill::Panel);
    assert_eq!(e.scene().get(a).unwrap().style.color, StrokeColor::Pink);
    assert_eq!(e.scene().get(a).unwrap().style.fill, Fill::Panel);
    tap(&mut e, "escape");
    // Nothing selected: only the next shape is affected.
    e.set_color(StrokeColor::Success);
    assert_eq!(e.scene().get(a).unwrap().style.color, StrokeColor::Pink);
    let b = rect(&mut e, p(300., 100.), p(400., 200.));
    assert_eq!(e.scene().get(b).unwrap().style.color, StrokeColor::Success);
    // Selecting a shape shows its style in the buttons.
    tap(&mut e, "escape");
    click(&mut e, p(100., 150.));
    assert_eq!(e.style().color, StrokeColor::Pink);
    assert_eq!(e.style().fill, Fill::Panel);
}

#[test]
fn sketchy_is_a_scene_flag_that_counts_as_an_edit() {
    let mut e = ed();
    assert!(!e.scene().sketchy);
    let before = e.edits();
    e.toggle_sketchy();
    assert!(e.scene().sketchy);
    assert!(e.edits() > before);
    e.toggle_sketchy();
    assert!(!e.scene().sketchy);
}

#[test]
fn the_edit_count_moves_with_the_scene_and_not_with_the_view_or_the_selection() {
    let mut e = ed();
    let id = rect(&mut e, p(100., 100.), p(200., 200.));
    let n = e.edits();
    assert!(n > 0);
    e.zoom_by(1.5);
    e.wheel(p(10., 10.), p(5., 5.), NO);
    e.fit();
    e.select_all();
    e.set_tool(Tool::Arrow);
    e.set_tool(Tool::Select);
    // Click the rectangle's left edge wherever the view put it.
    let edge = e.view().to_screen(p(100., 150.));
    click(&mut e, edge);
    assert_eq!(e.selection(), [id]);
    assert_eq!(e.edits(), n);
}

mod text_and_view;
