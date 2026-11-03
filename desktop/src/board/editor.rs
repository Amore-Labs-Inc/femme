//! The editor: everything the user does on the canvas as a state machine with no gpui in it. The
//! board entity feeds it pointer, wheel and key events (positions in canvas pixels, relative to the
//! canvas' top-left) and paints what it holds; the editor owns the [`Scene`], the [`View`], the
//! tool, the selection, the text being typed and the shapes on the board's own clipboard.
//!
//! One drag is one undo step: a drag opens a scene transaction when the pointer has travelled
//! [`DRAG_SLOP`] pixels and closes it when the button goes up (Esc takes it back instead). Holding an
//! arrow key is one step as well. [`Editor::edits`] counts every change to the scene, which is what
//! the autosave watches.

use super::font;
use super::geom::{Point, Rect};
use super::scene::{Fill, Kind, Scene, Shape, StrokeColor, Style, Width};
use super::textedit::TextEdit;
use super::view::View;

mod keys;
mod pointer;
mod tool;

pub use pointer::constrain;
use pointer::Drag;
pub use tool::{Button, Command, Keyed, Mods, Tool};

/// How far, in screen pixels, a click may miss a stroke and still select it.
pub const HIT_SLACK: f32 = 6.0;

/// How far, in screen pixels, the pointer must travel before a press becomes a drag: a move of the
/// selection, or a new shape. A click that stays inside draws nothing and moves nothing.
pub const DRAG_SLOP: f32 = 3.0;

/// The room, in screen pixels, [`Editor::fit`] leaves around the drawing.
pub const FIT_MARGIN: f32 = 48.0;

/// How far, in scene units, a duplicate or a paste lands from what it copies (each further paste of
/// the same copy goes one more step).
pub const PASTE_OFFSET: f32 = 16.0;

/// Scene units an arrow key moves the selection; with shift it is ten times that.
pub const NUDGE: f32 = 1.0;

/// What one zoom button press or ctrl-plus multiplies the zoom by.
pub const ZOOM_STEP: f32 = 1.25;

/// The zoom a scroll pixel makes with ctrl held (the factor is `exp(pixels * this)`).
const ZOOM_PER_PIXEL: f32 = 0.002;

/// The editor.
#[derive(Debug)]
pub struct Editor {
    scene: Scene,
    view: View,
    tool: Tool,
    style: Style,
    selection: Vec<u32>,
    typing: Option<TextEdit>,
    drag: Option<Drag>,
    space: bool,
    viewport: Point,
    pointer: Point,
    clipboard: Vec<Shape>,
    pastes: u32,
    nudging: bool,
    fit_pending: bool,
    edits: u64,
}

impl Editor {
    /// An editor over `scene`; the view is fitted to it when the canvas' size is first known.
    pub fn new(scene: Scene) -> Editor {
        Editor {
            scene,
            view: View::default(),
            tool: Tool::Select,
            style: Style::default(),
            selection: Vec::new(),
            typing: None,
            drag: None,
            space: false,
            viewport: Point::ZERO,
            pointer: Point::ZERO,
            clipboard: Vec::new(),
            pastes: 0,
            nudging: false,
            fit_pending: true,
            edits: 0,
        }
    }

    // ---- what the painter and the toolbar read -------------------------------------------------

    /// The scene.
    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    /// The view.
    pub fn view(&self) -> View {
        self.view
    }

    /// The tool.
    pub fn tool(&self) -> Tool {
        self.tool
    }

    /// The style new shapes get, and what the colour, width and fill buttons show.
    pub fn style(&self) -> Style {
        self.style
    }

    /// The selected ids, bottom to top.
    pub fn selection(&self) -> &[u32] {
        &self.selection
    }

    /// The box around the selected shapes.
    pub fn selection_bounds(&self) -> Option<Rect> {
        self.selection
            .iter()
            .filter_map(|id| self.scene.get(*id))
            .map(Shape::bounds)
            .reduce(|a, b| a.union(&b))
    }

    /// The text being typed.
    pub fn typing(&self) -> Option<&TextEdit> {
        self.typing.as_ref()
    }

    /// The box being dragged out (scene units).
    pub fn marquee(&self) -> Option<Rect> {
        match &self.drag {
            Some(Drag::Marquee { start, now, .. }) => Some(Rect::new(*start, *now)),
            _ => None,
        }
    }

    /// The view is being panned by hand: the cursor is a hand.
    pub fn is_panning(&self) -> bool {
        self.space || matches!(self.drag, Some(Drag::Pan { .. }))
    }

    /// How many changes the scene has had. The autosave compares it with the count it last saved.
    pub fn edits(&self) -> u64 {
        self.edits
    }

    /// The scene's undo stack has a step.
    pub fn can_undo(&self) -> bool {
        self.scene.can_undo()
    }

    /// The scene's redo stack has a step.
    pub fn can_redo(&self) -> bool {
        self.scene.can_redo()
    }

    /// The board has shapes to copy or send.
    pub fn has_content(&self) -> bool {
        !self.scene.is_empty()
    }

    // ---- view ----------------------------------------------------------------------------------

    /// Tells the editor how big the canvas is, in pixels. The first real size fits a loaded drawing.
    pub fn set_viewport(&mut self, size: Point) {
        self.viewport = size;
        if self.fit_pending && size.x > 1.0 && size.y > 1.0 {
            self.fit_pending = false;
            if !self.scene.is_empty() {
                self.fit();
            }
        }
    }

    /// Shows the whole drawing in the middle of the canvas (no drawing: the origin at the corner,
    /// at 100 %).
    pub fn fit(&mut self) {
        self.view = match self.scene.bounds() {
            Some(b) => View::fit(b, self.viewport, FIT_MARGIN),
            None => View::default(),
        };
    }

    /// Zooms by `factor` around the canvas' middle (the zoom buttons, ctrl-plus and minus).
    pub fn zoom_by(&mut self, factor: f32) {
        self.view.zoom_at(self.viewport * 0.5, factor);
    }

    /// Zooms to 100 % around the canvas' middle (ctrl-0).
    pub fn zoom_reset(&mut self) {
        self.zoom_by(1.0 / self.view.zoom);
    }

    /// Zooms by `factor` around the canvas position `at` (a pinch).
    pub fn zoom_at(&mut self, at: Point, factor: f32) {
        self.view.zoom_at(at, factor);
    }

    /// The wheel: it pans, or with ctrl held it zooms around the pointer; shift turns a vertical
    /// wheel into a horizontal pan. `delta` is in pixels, signed as gpui hands it out (it is added
    /// to the pan).
    pub fn wheel(&mut self, at: Point, delta: Point, mods: Mods) {
        if mods.ctrl {
            self.view.zoom_at(at, (delta.y * ZOOM_PER_PIXEL).exp());
        } else if mods.shift && delta.x == 0.0 {
            self.view.pan_by(Point::new(delta.y, 0.0));
        } else {
            self.view.pan_by(delta);
        }
    }

    // ---- tools and style -----------------------------------------------------------------------

    /// Picks a tool. Typed text is finished, a drag in progress ends, and a drawing tool drops the
    /// selection.
    pub fn set_tool(&mut self, tool: Tool) {
        self.blur();
        self.tool = tool;
        if tool != Tool::Select {
            self.selection.clear();
        }
    }

    /// Sets the stroke colour: of what is selected, of the text being typed, and of what is drawn next.
    pub fn set_color(&mut self, color: StrokeColor) {
        self.style.color = color;
        self.restyle(|s| s.color = color);
    }

    /// Sets the width (the size of a text), the same way.
    pub fn set_width(&mut self, width: Width) {
        self.style.width = width;
        self.restyle(|s| s.width = width);
    }

    /// Sets the fill of rectangles and ellipses, the same way.
    pub fn set_fill(&mut self, fill: Fill) {
        self.style.fill = fill;
        self.restyle(|s| s.fill = fill);
    }

    fn restyle(&mut self, f: impl Fn(&mut Style)) {
        if let Some(t) = &mut self.typing {
            f(&mut t.style);
        } else if !self.selection.is_empty() {
            self.scene.restyle(&self.selection, f);
            self.changed();
        }
    }

    /// Flips between the hand-drawn look and ruler-straight lines.
    pub fn toggle_sketchy(&mut self) {
        self.scene.sketchy = !self.scene.sketchy;
        self.changed();
    }

    // ---- edits ---------------------------------------------------------------------------------

    fn changed(&mut self) {
        self.edits += 1;
    }

    /// Selects `ids` and takes the style of the first for the buttons.
    fn select(&mut self, ids: Vec<u32>) {
        if let Some(sh) = ids.first().and_then(|id| self.scene.get(*id)) {
            self.style = sh.style;
        }
        self.selection = ids;
    }

    /// Forgets selected ids the scene no longer has (after an undo).
    fn prune_selection(&mut self) {
        let scene = &self.scene;
        self.selection.retain(|id| scene.get(*id).is_some());
    }

    /// Undoes the last step.
    pub fn undo(&mut self) {
        self.end_nudge();
        if self.scene.undo() {
            self.prune_selection();
            self.changed();
        }
    }

    /// Redoes the last undone step.
    pub fn redo(&mut self) {
        self.end_nudge();
        if self.scene.redo() {
            self.prune_selection();
            self.changed();
        }
    }

    /// Selects every shape.
    pub fn select_all(&mut self) {
        self.selection = self.scene.ids();
    }

    /// Deletes the selection.
    pub fn delete_selection(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        self.scene.delete(&self.selection);
        self.selection.clear();
        self.changed();
    }

    /// Puts a copy of the selection beside it and selects the copy.
    pub fn duplicate(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        let ids = self
            .scene
            .duplicate(&self.selection, Point::new(PASTE_OFFSET, PASTE_OFFSET));
        self.select(ids);
        self.changed();
    }

    /// Copies the selection onto the board's own clipboard.
    pub fn copy_selection(&mut self) {
        let shapes: Vec<Shape> = self
            .scene
            .shapes()
            .iter()
            .filter(|s| self.selection.contains(&s.id))
            .cloned()
            .collect();
        if !shapes.is_empty() {
            self.clipboard = shapes;
            self.pastes = 0;
        }
    }

    /// Copies the selection and deletes it.
    pub fn cut_selection(&mut self) {
        self.copy_selection();
        self.delete_selection();
    }

    /// Pastes the board's clipboard, a step further from the original each time.
    pub fn paste(&mut self) {
        if self.clipboard.is_empty() {
            return;
        }
        self.pastes += 1;
        let off = PASTE_OFFSET * self.pastes as f32;
        let ids = self.scene.paste(&self.clipboard, Point::new(off, off));
        self.select(ids);
        self.changed();
    }

    /// Clears the board: one undo step brings it back.
    pub fn clear(&mut self) {
        self.typing = None;
        self.cancel_drag();
        self.end_nudge();
        self.scene.clear();
        self.selection.clear();
        self.changed();
    }

    /// Closes the step a run of arrow keys made.
    fn end_nudge(&mut self) {
        if std::mem::take(&mut self.nudging) {
            self.scene.commit();
        }
    }

    // ---- text ----------------------------------------------------------------------------------

    /// Starts a new text with its first line centred on `p`.
    fn start_text(&mut self, p: Point) {
        let style = Style {
            fill: Fill::None,
            ..self.style
        };
        let lh = style.width.text_size() * font::LINE_HEIGHT;
        self.selection.clear();
        self.typing = Some(TextEdit::new(Point::new(p.x, p.y - lh * 0.5), style));
    }

    /// Starts editing the text shape `id`.
    fn edit_text(&mut self, id: u32) {
        if let Some(edit) = self.scene.get(id).and_then(TextEdit::of) {
            self.style = edit.style;
            self.typing = Some(edit);
            self.selection = vec![id];
        }
    }

    /// The box of the text being typed, for deciding whether a click is in it.
    fn typing_box(&self) -> Option<Rect> {
        let t = self.typing.as_ref()?;
        let (w, h) = font::measure(t.text(), t.style.width.text_size());
        Some(Rect::new(t.at, t.at + Point::new(w, h)).expand(4.0))
    }

    /// Ends the typing: a new text with something in it joins the scene; an existing one takes the
    /// new text (and is deleted when it was emptied). One undo step.
    pub fn commit_text(&mut self) {
        let Some(edit) = self.typing.take() else {
            return;
        };
        let text = edit.text().trim_end().to_owned();
        let blank = text.trim().is_empty();
        match edit.target {
            Some(id) if blank => {
                self.scene.delete(&[id]);
                self.selection.clear();
            }
            Some(id) => {
                self.scene.begin();
                self.scene.set_text(id, text);
                self.scene.restyle(&[id], |s| {
                    s.color = edit.style.color;
                    s.width = edit.style.width;
                });
                self.scene.commit();
                self.selection = vec![id];
            }
            None if blank => {}
            None => {
                let id = self.scene.add(Kind::Text { at: edit.at, text }, edit.style);
                self.selection = vec![id];
            }
        }
        self.changed();
        if self.tool == Tool::Text {
            self.tool = Tool::Select;
        }
    }
}
