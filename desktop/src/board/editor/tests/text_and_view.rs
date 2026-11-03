//! Tests of the editor's text typing and of the view (wheel, pan, zoom, fit).

use super::*;

fn type_str(e: &mut Editor, s: &str) {
    for c in s.chars() {
        let c = c.to_string();
        e.key(&c, Some(&c), NO);
    }
}

// ---- text -----------------------------------------------------------------------------------

#[test]
fn the_text_tool_places_a_caret_and_enter_commits() {
    let mut e = ed();
    tap(&mut e, "t");
    click(&mut e, p(200., 300.));
    assert!(e.typing().is_some());
    assert!(
        e.scene().is_empty(),
        "nothing is added before the first commit"
    );
    type_str(&mut e, "hello world");
    assert_eq!(e.typing().unwrap().text(), "hello world");
    assert_eq!(tap(&mut e, "enter"), Keyed::Done);
    assert!(e.typing().is_none());
    let Kind::Text { at, text } = kinds(&e)[0].clone() else {
        panic!("not a text")
    };
    assert_eq!(text, "hello world");
    assert_eq!(at.x, 200.0);
    assert!(
        (at.y + 20.0 * font::LINE_HEIGHT * 0.5 - 300.0).abs() < 1e-3,
        "the first line is centred on the click: {at:?}"
    );
    assert_eq!(e.selection().len(), 1);
    assert_eq!(e.tool(), Tool::Select);
    e.key("z", Some("z"), CTRL);
    assert!(e.scene().is_empty());
}

#[test]
fn shift_enter_is_a_new_line_and_escape_commits() {
    let mut e = ed();
    tap(&mut e, "t");
    click(&mut e, p(100., 100.));
    type_str(&mut e, "one");
    e.key("enter", None, SHIFT);
    type_str(&mut e, "two");
    assert_eq!(e.typing().unwrap().text(), "one\ntwo");
    tap(&mut e, "escape");
    assert_eq!(
        kinds(&e).len(),
        1,
        "escape keeps the text (and typing is over)"
    );
    let Kind::Text { text, .. } = kinds(&e)[0].clone() else {
        panic!()
    };
    assert_eq!(text, "one\ntwo");
    assert_eq!(e.tool(), Tool::Select);
}

#[test]
fn letters_typed_into_a_text_are_not_shortcuts() {
    let mut e = ed();
    tap(&mut e, "t");
    click(&mut e, p(100., 100.));
    type_str(&mut e, "rect 1 a");
    tap(&mut e, "space");
    assert_eq!(e.tool(), Tool::Text, "no tool was picked");
    assert_eq!(e.typing().unwrap().text(), "rect 1 a");
    // Space is a character of the text: the editor gets key "space" with the text " ".
    e.key("space", Some(" "), NO);
    assert_eq!(e.typing().unwrap().text(), "rect 1 a ");
}

#[test]
fn text_keys_edit_the_buffer() {
    let mut e = ed();
    tap(&mut e, "t");
    click(&mut e, p(100., 100.));
    type_str(&mut e, "helo");
    tap(&mut e, "left");
    type_str(&mut e, "l");
    assert_eq!(e.typing().unwrap().text(), "hello");
    tap(&mut e, "home");
    tap(&mut e, "delete");
    assert_eq!(e.typing().unwrap().text(), "ello");
    tap(&mut e, "end");
    tap(&mut e, "backspace");
    assert_eq!(e.typing().unwrap().text(), "ell");
    e.insert_text("o!");
    assert_eq!(e.typing().unwrap().text(), "ello!");
    // An unknown key with no text passes through; a ctrl chord is swallowed, not typed.
    assert_eq!(tap(&mut e, "f5"), Keyed::Ignored);
    assert_eq!(e.key("a", Some("a"), CTRL), Keyed::Done);
    assert_eq!(e.typing().unwrap().text(), "ello!");
    assert_eq!(e.key("v", Some("v"), CTRL), Keyed::Do(Command::PasteText));
}

#[test]
fn ctrl_enter_while_typing_commits_then_sends() {
    let mut e = ed();
    tap(&mut e, "t");
    click(&mut e, p(100., 100.));
    type_str(&mut e, "hi");
    assert_eq!(e.key("enter", None, CTRL), Keyed::Do(Command::Send));
    assert_eq!(
        e.scene().len(),
        1,
        "the text is in the picture that is sent"
    );
}

#[test]
fn an_empty_text_is_not_added() {
    let mut e = ed();
    tap(&mut e, "t");
    click(&mut e, p(100., 100.));
    type_str(&mut e, "   ");
    tap(&mut e, "enter");
    assert!(e.scene().is_empty());
    assert!(!e.can_undo());
}

#[test]
fn a_click_elsewhere_commits_the_text_and_goes_on_as_a_click() {
    let mut e = ed();
    let id = rect(&mut e, p(400., 400.), p(500., 500.));
    tap(&mut e, "t");
    click(&mut e, p(100., 100.));
    type_str(&mut e, "note");
    click(&mut e, p(400., 450.));
    assert!(e.typing().is_none());
    assert_eq!(e.scene().len(), 2);
    assert_eq!(
        e.selection(),
        [id],
        "the click went on to select the rectangle"
    );
}

#[test]
fn a_click_inside_the_text_being_typed_keeps_typing() {
    let mut e = ed();
    tap(&mut e, "t");
    click(&mut e, p(100., 100.));
    type_str(&mut e, "note");
    click(&mut e, p(110., 100.));
    assert_eq!(e.typing().unwrap().text(), "note");
}

#[test]
fn double_clicking_a_text_edits_it_and_emptying_it_deletes_it() {
    let mut e = ed();
    tap(&mut e, "t");
    click(&mut e, p(100., 100.));
    type_str(&mut e, "abc");
    tap(&mut e, "enter");
    let id = e.selection()[0];
    tap(&mut e, "escape");
    // The text starts at x 100, y about 87.5; a click inside its box hits it.
    e.pointer_down(p(105., 100.), NO, 2, Button::Primary);
    e.pointer_up(p(105., 100.), NO);
    assert_eq!(e.typing().and_then(|t| t.target), Some(id));
    assert_eq!(e.typing().unwrap().text(), "abc");
    type_str(&mut e, "d");
    tap(&mut e, "enter");
    let Kind::Text { text, .. } = e.scene().get(id).unwrap().kind.clone() else {
        panic!()
    };
    assert_eq!(text, "abcd");
    assert_eq!(e.scene().len(), 1, "edited in place");
    // Empty it.
    e.pointer_down(p(105., 100.), NO, 2, Button::Primary);
    e.pointer_up(p(105., 100.), NO);
    for _ in 0..4 {
        tap(&mut e, "backspace");
    }
    tap(&mut e, "enter");
    assert!(e.scene().is_empty());
    // And the deletion is one undo step.
    e.key("z", Some("z"), CTRL);
    assert_eq!(e.scene().len(), 1);
}

#[test]
fn double_clicking_empty_canvas_starts_a_text() {
    let mut e = ed();
    e.pointer_down(p(300., 300.), NO, 2, Button::Primary);
    e.pointer_up(p(300., 300.), NO);
    assert!(e.typing().is_some());
    type_str(&mut e, "x");
    tap(&mut e, "enter");
    assert_eq!(e.scene().len(), 1);
}

#[test]
fn the_colour_buttons_recolour_the_text_being_typed() {
    let mut e = ed();
    tap(&mut e, "t");
    click(&mut e, p(100., 100.));
    e.set_color(StrokeColor::Warn);
    type_str(&mut e, "x");
    tap(&mut e, "enter");
    assert_eq!(e.scene().shapes()[0].style.color, StrokeColor::Warn);
}

#[test]
fn blur_keeps_typed_text_and_finishes_a_drag() {
    let mut e = ed();
    tap(&mut e, "t");
    click(&mut e, p(100., 100.));
    type_str(&mut e, "kept");
    e.blur();
    assert!(e.typing().is_none());
    assert_eq!(e.scene().len(), 1);
    e.set_tool(Tool::Rect);
    e.pointer_down(p(300., 300.), NO, 1, Button::Primary);
    e.pointer_move(p(400., 400.), NO);
    e.blur();
    assert_eq!(e.scene().len(), 2);
    assert!(!e.scene().in_transaction());
}

// ---- view -----------------------------------------------------------------------------------

#[test]
fn the_wheel_pans_and_ctrl_wheel_zooms_around_the_pointer() {
    let mut e = ed();
    e.wheel(p(500., 300.), p(0., -40.), NO);
    assert_eq!(e.view().pan, p(0., -40.));
    e.wheel(p(500., 300.), p(0., 25.), SHIFT);
    assert_eq!(e.view().pan, p(25., -40.), "shift turns it sideways");
    e.wheel(p(500., 300.), p(7., 3.), NO);
    assert_eq!(e.view().pan, p(32., -37.));
    let at = p(300., 200.);
    let under = e.view().to_scene(at);
    e.wheel(at, p(0., 120.), CTRL);
    assert!(e.view().zoom > 1.0, "wheel up zooms in");
    assert!(e.view().to_scene(at).dist(under) < 1e-2);
    e.wheel(at, p(0., -10_000.), CTRL);
    assert_eq!(e.view().zoom, 0.25, "the limit holds");
}

#[test]
fn space_drag_and_middle_drag_pan() {
    let mut e = ed();
    e.set_tool(Tool::Rect);
    tap(&mut e, "space");
    assert!(e.is_panning());
    drag(&mut e, p(100., 100.), p(160., 130.), NO);
    assert_eq!(e.view().pan, p(60., 30.));
    assert!(e.scene().is_empty(), "no shape was drawn");
    e.key_up("space");
    assert!(!e.is_panning());
    e.pointer_down(p(10., 10.), NO, 1, Button::Middle);
    assert!(e.is_panning());
    e.pointer_move(p(0., 0.), NO);
    e.pointer_up(p(0., 0.), NO);
    assert_eq!(e.view().pan, p(50., 20.));
    assert!(!e.is_panning());
}

#[test]
fn escape_during_a_pan_puts_the_view_back() {
    let mut e = ed();
    e.pointer_down(p(10., 10.), NO, 1, Button::Middle);
    e.pointer_move(p(90., 50.), NO);
    assert_eq!(e.view().pan, p(80., 40.));
    tap(&mut e, "escape");
    assert_eq!(e.view().pan, Point::ZERO);
}

#[test]
fn fit_shows_the_whole_drawing_and_the_first_size_fits_a_loaded_scene() {
    let mut scene = Scene::new();
    scene.add(
        Kind::Rect {
            a: p(1000., 1000.),
            b: p(1400., 1200.),
        },
        Style::default(),
    );
    let mut e = Editor::new(scene);
    assert_eq!(e.view(), View::default());
    e.set_viewport(p(1000., 700.));
    let v = e.view();
    assert!(v.zoom > 1.0 && v.zoom <= 2.0, "{}", v.zoom);
    let c = v.to_screen(e.scene().bounds().unwrap().center());
    assert!(c.dist(p(500., 350.)) < 1e-2, "centred: {c:?}");
    // A later resize does not move the view.
    e.wheel(p(0., 0.), p(50., 0.), NO);
    e.set_viewport(p(1100., 700.));
    assert_eq!(e.view().pan.x, v.pan.x + 50.0);
    // Shift-1 fits again.
    e.key("1", Some("!"), SHIFT);
    assert_eq!(e.tool(), Tool::Select);
    assert!(
        e.view()
            .to_screen(e.scene().bounds().unwrap().center())
            .dist(p(550., 350.))
            < 1e-2
    );
}

#[test]
fn fitting_an_empty_board_resets_the_view() {
    let mut e = ed();
    e.zoom_by(2.0);
    e.wheel(p(0., 0.), p(30., 30.), NO);
    e.fit();
    assert_eq!(e.view(), View::default());
}

#[test]
fn zoom_keys_and_reset() {
    let mut e = ed();
    e.key("=", Some("+"), CTRL_SHIFT);
    assert!((e.view().zoom - ZOOM_STEP).abs() < 1e-6);
    e.key("-", Some("-"), CTRL);
    assert!((e.view().zoom - 1.0).abs() < 1e-5);
    e.zoom_by(3.0);
    e.key("0", Some("0"), CTRL);
    assert!((e.view().zoom - 1.0).abs() < 1e-5);
}
