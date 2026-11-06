//! Text of the transcript that the mouse selects and ctrl-c copies.
//!
//! A plain `div().child("text")` is glyphs and nothing else: the window-level text selection of
//! gpui-base (the `TextSelectionLayer` that `gpui_component::Root` mounts) does not know it is
//! there. A [`Selectable`] is the same text plus the three things that selection asks of a
//! participant: it registers its box and its reading order every frame (prepaint), it reports its
//! laid-out glyphs and is told which bytes are selected (paint), and it paints the highlight
//! between the backgrounds of its spans and its glyphs. The drag, double and triple click, the
//! auto-scroll at the edge of the list and the order of the copied text are gpui-base's.
//!
//! The pattern is gpui-base's own `SelectableText`, which cannot be used as it is: it takes plain
//! text, and the transcript's text has bold, code and link spans.
//!
//! Reading order: [`Sel`] hands every text of a list row its [`Slot`], an element id and a document
//! order. The order is the row's index in the list and then the text's place in the row, so a drag
//! across rows copies them top to bottom whatever order they were built in.
//!
//! What a selection copies: the selected bytes of every text it touches, in order, one text per
//! line. A text that ends a block ([`Join::Block`]) is followed by a blank line, so paragraphs are
//! told apart; the items of a list and the cells of a table ([`Join::Line`]) are not. Code is
//! copied as it is.
//!
//! Limits: the list draws only the rows near the window, so a selection reaches as far as the rows
//! that are drawn (the window and 300 px beyond it); a text the row cuts short with an ellipsis
//! cannot be selected (gpui-base refuses a layout that is not the whole text), and its row's open
//! form shows it whole.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    fill, px, App, Bounds, ClickEvent, ClipboardItem, CursorStyle, DispatchPhase, Element,
    ElementId, EntityId, GlobalElementId, HighlightStyle, Hitbox, HitboxBehavior,
    InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseUpEvent, Pixels,
    Point, Rgba, SharedString, StyledText, Subscription, Window,
};
use gpui_base::{
    TextSelection, TextSelectionEvent, TextSelectionHandle, TextSelectionRegistration,
    TextSelectionRun,
};

use crate::theme::HOVER;

/// How many low bits of an element id and of a document order number the texts of one row.
const ROW_BITS: u32 = 24;

/// The `key` of the row after the last item of the list (prompts on their way, the working line).
pub const TAIL: u64 = (1 << (64 - ROW_BITS)) - 1;

/// The highlight of selected text: the theme's pink ([`HOVER`], the colour of the composer's
/// selection), thin enough to leave ink and the grey of a code span readable under it.
const SELECTION: Rgba = Rgba {
    r: HOVER.r,
    g: HOVER.g,
    b: HOVER.b,
    a: 0.55,
};

/// How far the pointer may move between press and release and still make a click.
const DRAG: Pixels = px(4.);

/// What follows a text in the copied text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Join {
    /// A blank line: the text ends a block (paragraph, heading, quote, code block).
    #[default]
    Block,
    /// A line break only: the next text goes with this one (list items, table cells, the words of
    /// a tool row).
    Line,
}

/// The place of one [`Selectable`]: its element id and its reading order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    id: u64,
    order: u64,
}

impl Slot {
    /// The id the element keeps its selection handle under.
    fn element_id(self) -> ElementId {
        ElementId::NamedInteger("selectable".into(), self.id)
    }
}

/// Hands out the [`Slot`]s of one row of the list, in reading order: take one for each text, in
/// the order the texts show on the screen.
pub struct Sel {
    row: u64,
    key: u64,
    next: Cell<u64>,
}

impl Sel {
    /// The slots of row `ix` of the list, which shows the item `key` ([`TAIL`] for the row after
    /// the last item). The id follows `key`, which stays with its item when rows are added above
    /// it; the order follows `ix`, which is where the row is.
    pub fn new(ix: usize, key: u64) -> Self {
        Self {
            row: ix as u64,
            key: key.min(TAIL),
            next: Cell::new(0),
        }
    }

    /// The next text of the row.
    pub fn slot(&self) -> Slot {
        let n = self.next.get();
        // A row never holds 16 million texts; if it did, the last ones would share a slot.
        self.next.set((n + 1).min((1 << ROW_BITS) - 1));
        Slot {
            id: (self.key << ROW_BITS) | n,
            order: (self.row << ROW_BITS) | n,
        }
    }
}

/// What `text` contributes to the copied text when `range` of it is selected.
fn copied(text: &str, range: Option<&Range<usize>>, join: Join) -> String {
    let selected = range.and_then(|r| text.get(r.clone())).unwrap_or("");
    if selected.is_empty() {
        return String::new();
    }
    match join {
        // The window's copy joins the texts with one newline: one more makes a blank line.
        Join::Block => format!("{selected}\n"),
        Join::Line => selected.to_owned(),
    }
}

/// `selected` as it goes to the clipboard: without the blank line the last block leaves after it;
/// `None` when nothing but blanks is selected.
fn clipboard_text(selected: &str) -> Option<String> {
    let text = selected.trim_end();
    (!text.trim_start().is_empty()).then(|| text.to_owned())
}

/// Copies the selected text to the clipboard. False when nothing is selected.
pub fn copy(window: &mut Window, cx: &mut App) -> bool {
    let selected = TextSelection::selected_text(window, cx);
    let Some(text) = clipboard_text(&selected) else {
        return false;
    };
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    true
}

/// Clears the selection. False when there was none.
pub fn clear(window: &mut Window, cx: &mut App) -> bool {
    if !TextSelection::has_selection(window, cx) {
        return false;
    }
    TextSelection::clear(window, cx);
    true
}

/// Whether the pointer went from `down` to `up` far enough to be a drag.
fn dragged(down: Point<Pixels>, up: Point<Pixels>) -> bool {
    (up.x - down.x).abs() > DRAG || (up.y - down.y).abs() > DRAG
}

/// Whether a click on a row is meant to act on the row: not the end of a drag that selected text,
/// and not the second click of a double click that selected a word.
pub fn plain_click(click: &ClickEvent) -> bool {
    match click {
        ClickEvent::Mouse(c) => c.up.click_count < 2 && !dragged(c.down.position, c.up.position),
        _ => true,
    }
}

/// The index of the link of `links` that holds the byte `index`.
fn link_at(links: &[(Range<usize>, String)], index: usize) -> Option<usize> {
    links.iter().position(|(range, _)| range.contains(&index))
}

/// The rectangles a selection from `start` to `end` fills in text laid out in `bounds`: the first
/// row from `start`, the rows between across the whole width, the last row up to `end`.
fn selection_quads(
    start: Point<Pixels>,
    end: Point<Pixels>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
) -> Vec<Bounds<Pixels>> {
    if start.y == end.y {
        return vec![Bounds::from_corners(
            start,
            Point::new(end.x, end.y + line_height),
        )];
    }
    let mut quads = vec![Bounds::from_corners(
        start,
        Point::new(bounds.right(), start.y + line_height),
    )];
    if end.y > start.y + line_height {
        quads.push(Bounds::from_corners(
            Point::new(bounds.left(), start.y + line_height),
            Point::new(bounds.right(), end.y),
        ));
    }
    quads.push(Bounds::from_corners(
        Point::new(bounds.left(), end.y),
        Point::new(end.x, end.y + line_height),
    ));
    quads
}

/// What the copy callback needs of the last paint: the text, which bytes of it were selected, and
/// what follows it.
#[derive(Default)]
struct Latest {
    text: SharedString,
    range: Option<Range<usize>>,
    join: Join,
}

/// A press on a link that has not been released yet: the link, and where.
#[derive(Clone, Copy)]
struct Press {
    link: usize,
    at: Point<Pixels>,
}

/// What a [`Selectable`] keeps from one frame to the next, under its element id: the state type
/// of its element, not meant for anything else.
#[doc(hidden)]
#[derive(Clone)]
pub struct Retained {
    handle: TextSelectionHandle,
    latest: Rc<RefCell<Latest>>,
    press: Rc<Cell<Option<Press>>>,
    /// Redraws `view` (the transcript, which is drawn from a cache and would not notice) when
    /// the selection of this text changes.
    _watch: Rc<Subscription>,
}

impl Retained {
    fn new(view: EntityId, cx: &mut App) -> Self {
        let handle = TextSelectionHandle::new("", cx);
        let latest = Rc::new(RefCell::new(Latest::default()));
        let copy = latest.clone();
        handle.copy_with(
            move |_| {
                let l = copy.borrow();
                copied(&l.text, l.range.as_ref(), l.join)
            },
            cx,
        );
        let watch = handle.subscribe(
            move |event, cx| {
                if matches!(event, TextSelectionEvent::SelectionChanged(_)) {
                    cx.notify(view);
                }
            },
            cx,
        );
        Self {
            handle,
            latest,
            press: Rc::new(Cell::new(None)),
            _watch: Rc::new(watch),
        }
    }
}

/// Text that takes part in the window's text selection. It lays out and looks like the
/// [`StyledText`] it wraps; `links` are the byte ranges that open an address when clicked.
pub struct Selectable {
    slot: Slot,
    text: SharedString,
    styled: StyledText,
    links: Vec<(Range<usize>, String)>,
    join: Join,
}

impl Selectable {
    /// `text` in the place `slot`, in the text style it inherits.
    pub fn new(slot: Slot, text: impl Into<SharedString>) -> Self {
        let text = text.into();
        Self {
            slot,
            styled: StyledText::new(text.clone()),
            text,
            links: Vec::new(),
            join: Join::Block,
        }
    }

    /// Styles stretches of the text (bold, a background, an underline).
    pub fn highlights(
        mut self,
        highlights: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    ) -> Self {
        self.styled = self.styled.with_highlights(highlights);
        self
    }

    /// Sets the font family of stretches of the text, sorted and not overlapping.
    pub fn font_families(
        mut self,
        overrides: impl IntoIterator<Item = (Range<usize>, SharedString)>,
    ) -> Self {
        self.styled = self.styled.with_font_family_overrides(overrides);
        self
    }

    /// The byte ranges of the text that are links, with their addresses: a click on one (press and
    /// release on the same link, the pointer not dragged) opens its address.
    pub fn links(mut self, links: Vec<(Range<usize>, String)>) -> Self {
        self.links = links;
        self
    }

    /// What follows the text in the copied text (a blank line unless this says otherwise).
    pub fn join(mut self, join: Join) -> Self {
        self.join = join;
        self
    }
}

impl IntoElement for Selectable {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Selectable {
    type RequestLayoutState = Retained;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.slot.element_id())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Retained) {
        let view = window.current_view();
        let retained = window.with_element_state(
            id.expect("a Selectable has an element id"),
            |kept: Option<Retained>, _| {
                let kept = kept.unwrap_or_else(|| Retained::new(view, cx));
                (kept.clone(), kept)
            },
        );
        let (layout, ()) = self.styled.request_layout(id, inspector_id, window, cx);
        (layout, retained)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        retained: &mut Retained,
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        self.styled
            .prepaint(id, inspector_id, bounds, &mut (), window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let registration = TextSelectionRegistration::new(hitbox.clone(), bounds)
            .with_document_order(self.slot.order)
            .with_text_bounds(vec![bounds])
            .with_rendered_element(&retained.handle, window, cx);
        retained.handle.register(registration, window, cx);
        hitbox
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        retained: &mut Retained,
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = self.styled.layout().clone();
        let run = TextSelectionRun::new(self.text.clone(), layout.clone(), bounds)
            .with_document_order(self.slot.order);
        let range = retained
            .handle
            .update_runs(&[run], cx)
            .ranges()
            .first()
            .cloned()
            .flatten();
        *retained.latest.borrow_mut() = Latest {
            text: self.text.clone(),
            range: range.clone(),
            join: self.join,
        };

        // The highlight goes over the backgrounds of the spans (a code span's grey) and under the
        // glyphs.
        if layout.paint_background(window, cx).is_err() {
            return;
        }
        if let Some(range) = range {
            if let (Some(start), Some(end)) = (
                layout.position_for_index(range.start),
                layout.position_for_index(range.end),
            ) {
                for quad in selection_quads(start, end, layout.bounds(), layout.line_height()) {
                    window.paint_quad(fill(quad, SELECTION));
                }
            }
        }
        layout.paint_foreground(window, cx).ok();

        let over = layout.index_for_position(window.mouse_position()).ok();
        let cursor = if over.is_some_and(|ix| link_at(&self.links, ix).is_some()) {
            CursorStyle::PointingHand
        } else {
            CursorStyle::IBeam
        };
        window.set_cursor_style(cursor, hitbox);

        if self.links.is_empty() {
            return;
        }
        let links = Rc::new(std::mem::take(&mut self.links));
        window.on_mouse_event({
            let (links, layout, hitbox, press) = (
                links.clone(),
                layout.clone(),
                hitbox.clone(),
                retained.press.clone(),
            );
            move |event: &MouseDownEvent, phase, window, _| {
                if phase == DispatchPhase::Bubble
                    && event.button == MouseButton::Left
                    && hitbox.is_hovered(window)
                {
                    let link = layout
                        .index_for_position(event.position)
                        .ok()
                        .and_then(|ix| link_at(&links, ix));
                    press.set(link.map(|link| Press {
                        link,
                        at: event.position,
                    }));
                }
            }
        });
        let press = retained.press.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            let Some(down) = press.take() else {
                return;
            };
            let up = layout
                .index_for_position(event.position)
                .ok()
                .and_then(|ix| link_at(&links, ix));
            if up == Some(down.link) && event.click_count == 1 && !dragged(down.at, event.position)
            {
                cx.open_url(&links[down.link].1);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    #[test]
    fn slots_of_a_row_follow_each_other_and_rows_follow_the_list() {
        let a = Sel::new(2, 40);
        let (a0, a1) = (a.slot(), a.slot());
        let b = Sel::new(3, 41);
        let b0 = b.slot();
        assert!(a0.order < a1.order, "reading order inside a row");
        assert!(
            a1.order < b0.order,
            "a later row reads after every text of an earlier one"
        );
        let ids = [a0.id, a1.id, b0.id];
        assert!(ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2]);
    }

    #[test]
    fn the_id_follows_the_item_and_the_order_follows_the_row() {
        // History loaded above shifts every row index by 10: the ids stay, the order moves.
        let before = Sel::new(5, 77).slot();
        let after = Sel::new(15, 77).slot();
        assert_eq!(before.id, after.id);
        assert_eq!(after.order - before.order, 10 << ROW_BITS);
        // The row after the last item reads last and has ids of its own.
        let tail = Sel::new(16, TAIL).slot();
        assert!(tail.order > after.order);
        assert_ne!(tail.id, after.id);
    }

    #[test]
    fn a_full_row_does_not_run_into_the_next_one() {
        let row = Sel::new(0, 1);
        row.next.set((1 << ROW_BITS) - 3);
        let last = (0..6).map(|_| row.slot()).last().unwrap();
        assert!(last.order < Sel::new(1, 2).slot().order);
    }

    #[test]
    fn a_block_is_followed_by_a_blank_line_and_a_line_by_a_line_break() {
        let text = "hello world";
        assert_eq!(copied(text, Some(&(0..5)), Join::Block), "hello\n");
        assert_eq!(copied(text, Some(&(6..11)), Join::Line), "world");
        assert_eq!(copied(text, Some(&(0..0)), Join::Block), "");
        assert_eq!(copied(text, None, Join::Block), "");
        assert_eq!(
            copied(text, Some(&(0..99)), Join::Line),
            "",
            "a range that is not in the text copies nothing"
        );
    }

    #[test]
    fn selected_texts_join_as_the_window_joins_them() {
        // The window sorts by order and joins with one newline; the callbacks bring the rest.
        let items = [
            copied("first paragraph", Some(&(0..15)), Join::Block),
            copied("item one", Some(&(0..8)), Join::Line),
            copied("item two", Some(&(0..8)), Join::Line),
            copied("last paragraph", Some(&(0..4)), Join::Block),
        ];
        let joined = items.join("\n");
        assert_eq!(
            clipboard_text(&joined).as_deref(),
            Some("first paragraph\n\nitem one\nitem two\nlast")
        );
    }

    #[test]
    fn code_is_copied_verbatim() {
        let code = "fn main() {\n    println!(\"hi\");\n}";
        let copied = copied(code, Some(&(0..code.len())), Join::Block);
        assert_eq!(clipboard_text(&copied).as_deref(), Some(code));
        // Indentation at the start of a selection is kept; only the end is trimmed.
        assert_eq!(clipboard_text("    x\n\n").as_deref(), Some("    x"));
    }

    #[test]
    fn a_selection_of_blanks_copies_nothing() {
        assert_eq!(clipboard_text(""), None);
        assert_eq!(clipboard_text(" \n\n"), None);
    }

    #[test]
    fn a_click_is_a_press_and_a_release_that_stay_put() {
        let at = point(px(10.), px(10.));
        assert!(!dragged(at, at));
        assert!(!dragged(at, point(px(13.), px(7.))));
        assert!(dragged(at, point(px(15.), px(10.))));
        assert!(dragged(at, point(px(10.), px(4.))));
    }

    #[test]
    fn links_are_found_by_the_byte_under_the_pointer() {
        let links = vec![
            (4..9, "https://a.example".to_owned()),
            (12..14, "b".to_owned()),
        ];
        assert_eq!(link_at(&links, 3), None);
        assert_eq!(link_at(&links, 4), Some(0));
        assert_eq!(link_at(&links, 8), Some(0));
        assert_eq!(link_at(&links, 9), None, "the end of a range is outside it");
        assert_eq!(link_at(&links, 13), Some(1));
        assert_eq!(link_at(&[], 0), None);
    }

    #[test]
    fn a_selection_on_one_row_is_one_rectangle() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(100.)));
        let quads = selection_quads(
            point(px(30.), px(20.)),
            point(px(60.), px(20.)),
            bounds,
            px(20.),
        );
        assert_eq!(
            quads,
            vec![Bounds::from_corners(
                point(px(30.), px(20.)),
                point(px(60.), px(40.))
            )]
        );
    }

    #[test]
    fn a_selection_over_several_rows_fills_the_rows_between() {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(100.)));
        let quads = selection_quads(
            point(px(40.), px(20.)),
            point(px(30.), px(80.)),
            bounds,
            px(20.),
        );
        assert_eq!(
            quads,
            vec![
                Bounds::from_corners(point(px(40.), px(20.)), point(px(110.), px(40.))),
                Bounds::from_corners(point(px(10.), px(40.)), point(px(110.), px(80.))),
                Bounds::from_corners(point(px(10.), px(80.)), point(px(30.), px(100.))),
            ]
        );
        // Two neighbouring rows: nothing between them.
        let two = selection_quads(
            point(px(40.), px(20.)),
            point(px(30.), px(40.)),
            bounds,
            px(20.),
        );
        assert_eq!(two.len(), 2);
    }
}
