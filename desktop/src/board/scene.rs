//! The board's scene: the shapes of a diagram, their styles, hit tests, the marquee query and the
//! undo/redo history. Pure data and arithmetic (no gpui), so every operation is unit-tested.
//!
//! Coordinates are *scene units*, which are logical pixels at zoom 1; the view (zoom and pan, in
//! [`super::editor`]) maps them to the screen. Shapes are kept in z-order, the last one on top.
//!
//! History is snapshot based: [`Scene::begin`] remembers the shapes, [`Scene::commit`] pushes that
//! snapshot onto the undo stack when anything changed. Every edit below wraps itself in such a
//! transaction, unless one is already open (a drag draws, moves and resizes live between its own
//! `begin` and `commit`, so one drag is one undo step).

use serde::{Deserialize, Serialize};

use super::font;
use super::geom::{dist_to_segment, Point, Rect};

/// How many undo steps are kept.
pub const HISTORY_CAP: usize = 200;

/// Version of the JSON the scene is saved as.
pub const FILE_VERSION: u32 = 1;

/// The stroke colours of a shape: the FEMME palette's ink, pink accent, neon and the three status
/// colours (see [`super::palette`] for the tokens each stands for).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrokeColor {
    /// Ink: the default.
    #[default]
    Ink,
    /// The pink accent (hover pink).
    Pink,
    /// Neon pink.
    Neon,
    /// Success green.
    Success,
    /// Warning yellow.
    Warn,
    /// Danger red.
    Danger,
}

impl StrokeColor {
    /// Every colour, in the order the swatches show them.
    pub const ALL: [StrokeColor; 6] = [
        StrokeColor::Ink,
        StrokeColor::Pink,
        StrokeColor::Neon,
        StrokeColor::Success,
        StrokeColor::Warn,
        StrokeColor::Danger,
    ];

    /// The colour's name, for tooltips and tests.
    pub fn name(self) -> &'static str {
        match self {
            StrokeColor::Ink => "ink",
            StrokeColor::Pink => "pink",
            StrokeColor::Neon => "neon",
            StrokeColor::Success => "green",
            StrokeColor::Warn => "yellow",
            StrokeColor::Danger => "red",
        }
    }
}

/// Stroke width of a shape; for a text it is the size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Width {
    /// 2 px strokes, 20 px text: the default.
    #[default]
    Thin,
    /// 4 px strokes, 32 px text.
    Thick,
}

impl Width {
    /// Stroke width in scene units.
    pub fn px(self) -> f32 {
        match self {
            Width::Thin => 2.0,
            Width::Thick => 4.0,
        }
    }

    /// Text size in scene units.
    pub fn text_size(self) -> f32 {
        match self {
            Width::Thin => 20.0,
            Width::Thick => 32.0,
        }
    }

    /// The other width.
    pub fn toggled(self) -> Width {
        match self {
            Width::Thin => Width::Thick,
            Width::Thick => Width::Thin,
        }
    }
}

/// The fill of a closed shape (a rectangle or an ellipse).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fill {
    /// Nothing: the paper shows through. Only the outline selects the shape.
    #[default]
    None,
    /// The soft pink wash.
    Soft,
    /// Panel grey.
    Panel,
}

impl Fill {
    /// Every fill, in the order the buttons show them.
    pub const ALL: [Fill; 3] = [Fill::None, Fill::Soft, Fill::Panel];

    /// The fill's name, for tooltips and tests.
    pub fn name(self) -> &'static str {
        match self {
            Fill::None => "no fill",
            Fill::Soft => "pink fill",
            Fill::Panel => "grey fill",
        }
    }
}

/// Everything a shape is painted with: any colour of the palette, stroke or fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tone {
    /// See [`StrokeColor::Ink`].
    Ink,
    /// See [`StrokeColor::Pink`].
    Pink,
    /// See [`StrokeColor::Neon`].
    Neon,
    /// See [`StrokeColor::Success`].
    Success,
    /// See [`StrokeColor::Warn`].
    Warn,
    /// See [`StrokeColor::Danger`].
    Danger,
    /// See [`Fill::Soft`].
    Soft,
    /// See [`Fill::Panel`].
    Panel,
}

impl From<StrokeColor> for Tone {
    fn from(c: StrokeColor) -> Tone {
        match c {
            StrokeColor::Ink => Tone::Ink,
            StrokeColor::Pink => Tone::Pink,
            StrokeColor::Neon => Tone::Neon,
            StrokeColor::Success => Tone::Success,
            StrokeColor::Warn => Tone::Warn,
            StrokeColor::Danger => Tone::Danger,
        }
    }
}

/// How a shape looks: stroke colour and width, and the fill of a closed shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Style {
    /// Stroke colour (the text colour of a text).
    pub color: StrokeColor,
    /// Stroke width (the size of a text).
    pub width: Width,
    /// Fill of a rectangle or an ellipse.
    #[serde(default)]
    pub fill: Fill,
}

impl Style {
    /// The tone of the fill, none when the shape is not filled.
    pub fn fill_tone(&self) -> Option<Tone> {
        match self.fill {
            Fill::None => None,
            Fill::Soft => Some(Tone::Soft),
            Fill::Panel => Some(Tone::Panel),
        }
    }
}

/// The geometry of a shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Kind {
    /// A rectangle between two opposite corners (the order is the order the user dragged in).
    Rect {
        /// Where the drag started.
        a: Point,
        /// Where it ended.
        b: Point,
    },
    /// An ellipse in the box between two opposite corners.
    Ellipse {
        /// Where the drag started.
        a: Point,
        /// Where it ended.
        b: Point,
    },
    /// A straight line.
    Line {
        /// Start.
        a: Point,
        /// End.
        b: Point,
    },
    /// A straight arrow, its head at `b`.
    Arrow {
        /// Start.
        a: Point,
        /// End, where the head is.
        b: Point,
    },
    /// A scribble through the pointer's positions.
    Freehand {
        /// The positions, in the order drawn.
        points: Vec<Point>,
    },
    /// A label; its size comes from the style's [`Width`].
    Text {
        /// Top-left corner of the text block.
        at: Point,
        /// The text, lines split by `\n`.
        text: String,
    },
}

/// One drawn thing: an id, its geometry and its style.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Shape {
    /// Unique in the scene for its whole life (undo brings a deleted shape back under the same id);
    /// also seeds the sketchy jitter, so a shape keeps its wobble.
    pub id: u32,
    /// The geometry.
    pub kind: Kind,
    /// The look.
    pub style: Style,
}

/// Half the opening angle of an arrow head and its relative size: see [`arrow_head`].
const HEAD_HALF_ANGLE: f32 = 0.42;

/// Length of the head of an arrow of stroke width `w`, for a shaft of length `len`.
pub fn arrow_head_len(w: f32, len: f32) -> f32 {
    (11.0 + 3.0 * w).min(len * 0.6)
}

/// The three corners of an arrow head for an arrow from `a` to `b`: the tip (at `b`) and the two
/// barbs. None for an arrow of no length.
pub fn arrow_head(a: Point, b: Point, w: f32) -> Option<[Point; 3]> {
    let d = (b - a).unit()?;
    let len = arrow_head_len(w, a.dist(b));
    let back = b - d * len;
    let half = len * HEAD_HALF_ANGLE.tan();
    let n = d.normal();
    Some([b, back + n * half, back - n * half])
}

impl Shape {
    /// Size of the text of a [`Kind::Text`].
    pub fn text_size(&self) -> f32 {
        self.style.width.text_size()
    }

    /// The box of the geometry itself (a stroke's own width not counted); what the selection box
    /// and the marquee use.
    pub fn bounds(&self) -> Rect {
        match &self.kind {
            Kind::Rect { a, b }
            | Kind::Ellipse { a, b }
            | Kind::Line { a, b }
            | Kind::Arrow { a, b } => Rect::new(*a, *b),
            Kind::Freehand { points } => Rect::around(points).unwrap_or_default(),
            Kind::Text { at, text } => {
                let (w, h) = font::measure(text, self.text_size());
                Rect::new(*at, *at + Point::new(w, h))
            }
        }
    }

    /// The box everything of the shape is painted inside: the geometry plus half the stroke, the
    /// arrow head and the room the sketchy jitter takes. What an export frames.
    pub fn ink_bounds(&self) -> Rect {
        let w = self.style.width.px();
        let room = match &self.kind {
            Kind::Arrow { .. } => w * 0.5 + arrow_head_len(w, f32::MAX).min(40.0) * 0.5 + 4.0,
            Kind::Text { .. } => 2.0,
            _ => w * 0.5 + 4.0,
        };
        self.bounds().expand(room)
    }

    /// Whether `p` hits the shape. `tol` is the slack in scene units (6 px at zoom 1): strokes are
    /// hit within `tol` plus half their width, a filled shape and a text anywhere inside.
    pub fn hit(&self, p: Point, tol: f32) -> bool {
        let reach = tol + self.style.width.px() * 0.5;
        let filled = self.style.fill != Fill::None;
        match &self.kind {
            Kind::Rect { a, b } => {
                let r = Rect::new(*a, *b);
                let outer = r.expand(reach);
                if !outer.contains(p) {
                    return false;
                }
                filled || !r.expand(-reach).contains(p)
            }
            Kind::Ellipse { a, b } => {
                let r = Rect::new(*a, *b);
                let c = r.center();
                let (rx, ry) = (r.width() * 0.5, r.height() * 0.5);
                let d = p - c;
                let q = Point::new(d.x / rx.max(1e-3), d.y / ry.max(1e-3));
                let qn = q.len();
                if filled && qn <= 1.0 {
                    return true;
                }
                // How far `p` is from the outline, measured along the ray from the centre.
                let off = if qn < 1e-6 {
                    rx.min(ry)
                } else {
                    let l = d.len();
                    (l - l / qn).abs()
                };
                off <= reach
            }
            Kind::Line { a, b } | Kind::Arrow { a, b } => dist_to_segment(p, *a, *b) <= reach,
            Kind::Freehand { points } => match points.as_slice() {
                [] => false,
                [one] => p.dist(*one) <= reach,
                many => many
                    .windows(2)
                    .any(|w| dist_to_segment(p, w[0], w[1]) <= reach),
            },
            Kind::Text { .. } => self.bounds().expand(tol).contains(p),
        }
    }

    /// Moves the shape by `d`.
    pub fn translate(&mut self, d: Point) {
        match &mut self.kind {
            Kind::Rect { a, b }
            | Kind::Ellipse { a, b }
            | Kind::Line { a, b }
            | Kind::Arrow { a, b } => {
                *a = *a + d;
                *b = *b + d;
            }
            Kind::Freehand { points } => points.iter_mut().for_each(|p| *p = *p + d),
            Kind::Text { at, .. } => *at = *at + d,
        }
    }

    /// Every coordinate is a finite number.
    fn is_finite(&self) -> bool {
        match &self.kind {
            Kind::Rect { a, b }
            | Kind::Ellipse { a, b }
            | Kind::Line { a, b }
            | Kind::Arrow { a, b } => a.is_finite() && b.is_finite(),
            Kind::Freehand { points } => points.iter().all(|p| p.is_finite()),
            Kind::Text { at, .. } => at.is_finite(),
        }
    }
}

/// The scene: shapes in z-order (the last one on top), the sketchy flag, and the undo and redo
/// stacks.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    shapes: Vec<Shape>,
    /// Draw lines and outlines the hand-drawn way (a seeded wobble per shape, see
    /// [`super::geometry`]) instead of ruler-straight. A view flag: not part of the undo history.
    pub sketchy: bool,
    next_id: u32,
    sent: u32,
    undo: Vec<Vec<Shape>>,
    redo: Vec<Vec<Shape>>,
    open: Option<Vec<Shape>>,
}

/// The scene as it is saved.
#[derive(Serialize)]
struct FileRef<'a> {
    version: u32,
    sketchy: bool,
    next_id: u32,
    sent: u32,
    shapes: Vec<&'a Shape>,
}

/// The scene as it is loaded: everything but the shapes may be missing.
#[derive(Deserialize)]
struct FileOwned {
    version: u32,
    #[serde(default)]
    sketchy: bool,
    #[serde(default)]
    next_id: u32,
    #[serde(default)]
    sent: u32,
    shapes: Vec<Shape>,
}

impl Scene {
    /// An empty scene.
    pub fn new() -> Scene {
        Scene::default()
    }

    /// The shapes, bottom to top.
    pub fn shapes(&self) -> &[Shape] {
        &self.shapes
    }

    /// How many shapes there are.
    pub fn len(&self) -> usize {
        self.shapes.len()
    }

    /// Nothing is drawn.
    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }

    /// The shape with this id.
    pub fn get(&self, id: u32) -> Option<&Shape> {
        self.shapes.iter().find(|s| s.id == id)
    }

    /// Every id, bottom to top (select all).
    pub fn ids(&self) -> Vec<u32> {
        self.shapes.iter().map(|s| s.id).collect()
    }

    /// The box of everything painted (see [`Shape::ink_bounds`]); none for an empty scene.
    pub fn bounds(&self) -> Option<Rect> {
        self.shapes
            .iter()
            .map(Shape::ink_bounds)
            .reduce(|a, b| a.union(&b))
    }

    /// The topmost shape `p` hits, with `tol` scene units of slack.
    pub fn hit_test(&self, p: Point, tol: f32) -> Option<u32> {
        self.shapes
            .iter()
            .rev()
            .find(|s| s.hit(p, tol))
            .map(|s| s.id)
    }

    /// The shapes lying wholly inside `rect` (a marquee), bottom to top.
    pub fn shapes_in(&self, rect: &Rect) -> Vec<u32> {
        self.shapes
            .iter()
            .filter(|s| rect.contains_rect(&s.bounds()))
            .map(|s| s.id)
            .collect()
    }

    // ---- history ------------------------------------------------------------------------------

    /// Opens a transaction: the shapes as they are now are what one undo brings back. A second
    /// `begin` while one is open does nothing.
    pub fn begin(&mut self) {
        if self.open.is_none() {
            self.open = Some(self.shapes.clone());
        }
    }

    /// Closes the transaction. When the shapes changed, the snapshot goes onto the undo stack (the
    /// oldest step falls off past [`HISTORY_CAP`]) and the redo stack is dropped. Says whether
    /// anything changed.
    pub fn commit(&mut self) -> bool {
        let Some(before) = self.open.take() else {
            return false;
        };
        if before == self.shapes {
            return false;
        }
        self.undo.push(before);
        if self.undo.len() > HISTORY_CAP {
            self.undo.remove(0);
        }
        self.redo.clear();
        true
    }

    /// Closes the transaction and puts back what `begin` saw.
    pub fn cancel(&mut self) {
        if let Some(before) = self.open.take() {
            self.shapes = before;
        }
    }

    /// A transaction is open.
    pub fn in_transaction(&self) -> bool {
        self.open.is_some()
    }

    /// Runs `f` as one history step, or inside the transaction that is already open.
    fn step<R>(&mut self, f: impl FnOnce(&mut Scene) -> R) -> R {
        let own = self.open.is_none();
        if own {
            self.begin();
        }
        let r = f(self);
        if own {
            self.commit();
        }
        r
    }

    /// Undoes the last step. Says whether there was one (nothing is undone mid-drag).
    pub fn undo(&mut self) -> bool {
        if self.open.is_some() {
            return false;
        }
        let Some(before) = self.undo.pop() else {
            return false;
        };
        self.redo.push(std::mem::replace(&mut self.shapes, before));
        true
    }

    /// Redoes the last undone step.
    pub fn redo(&mut self) -> bool {
        if self.open.is_some() {
            return false;
        }
        let Some(after) = self.redo.pop() else {
            return false;
        };
        self.undo.push(std::mem::replace(&mut self.shapes, after));
        true
    }

    /// There is a step to undo.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// There is a step to redo.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    // ---- edits --------------------------------------------------------------------------------

    fn fresh_id(&mut self) -> u32 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        id
    }

    /// Adds a shape on top and returns its id.
    pub fn add(&mut self, kind: Kind, style: Style) -> u32 {
        self.step(|s| {
            let id = s.fresh_id();
            s.shapes.push(Shape { id, kind, style });
            id
        })
    }

    /// Replaces the geometry of a shape (the second corner of the rectangle being dragged).
    pub fn set_kind(&mut self, id: u32, kind: Kind) {
        self.step(|s| {
            if let Some(sh) = s.shapes.iter_mut().find(|sh| sh.id == id) {
                sh.kind = kind;
            }
        })
    }

    /// Sets the text of a text shape.
    pub fn set_text(&mut self, id: u32, text: String) {
        self.step(|s| {
            if let Some(Shape {
                kind: Kind::Text { text: t, .. },
                ..
            }) = s.shapes.iter_mut().find(|sh| sh.id == id)
            {
                *t = text;
            }
        })
    }

    /// Changes the style of the given shapes with `f`.
    pub fn restyle(&mut self, ids: &[u32], f: impl Fn(&mut Style)) {
        self.step(|s| {
            for sh in s.shapes.iter_mut().filter(|sh| ids.contains(&sh.id)) {
                f(&mut sh.style);
            }
        })
    }

    /// Moves the given shapes by `d`.
    pub fn move_by(&mut self, ids: &[u32], d: Point) {
        self.step(|s| {
            for sh in s.shapes.iter_mut().filter(|sh| ids.contains(&sh.id)) {
                sh.translate(d);
            }
        })
    }

    /// Deletes the given shapes.
    pub fn delete(&mut self, ids: &[u32]) {
        self.step(|s| s.shapes.retain(|sh| !ids.contains(&sh.id)))
    }

    /// Puts copies of the given shapes on top, moved by `d`, and returns the new ids (in z-order).
    pub fn duplicate(&mut self, ids: &[u32], d: Point) -> Vec<u32> {
        let copies: Vec<Shape> = self
            .shapes
            .iter()
            .filter(|sh| ids.contains(&sh.id))
            .cloned()
            .collect();
        self.paste(&copies, d)
    }

    /// Puts copies of `shapes` (from the board's own clipboard) on top under fresh ids, moved by
    /// `d`, and returns the new ids.
    pub fn paste(&mut self, shapes: &[Shape], d: Point) -> Vec<u32> {
        self.step(|s| {
            shapes
                .iter()
                .map(|sh| {
                    let mut copy = sh.clone();
                    copy.id = s.fresh_id();
                    copy.translate(d);
                    s.shapes.push(copy.clone());
                    copy.id
                })
                .collect()
        })
    }

    /// Clears the board (one undo step brings it back).
    pub fn clear(&mut self) {
        self.step(|s| s.shapes.clear())
    }

    /// The name of the next PNG sent to Claude: `diagram-1.png`, `diagram-2.png`, ... (the count is
    /// saved with the scene, so a restart does not reuse a name).
    pub fn next_export_name(&mut self) -> String {
        self.sent += 1;
        format!("diagram-{}.png", self.sent)
    }

    // ---- persistence --------------------------------------------------------------------------

    /// The scene as JSON (the undo history is not saved). A shape with a coordinate that is not a
    /// number is left out: JSON would write it as `null`, which no load could read back.
    pub fn to_json(&self) -> String {
        let file = FileRef {
            version: FILE_VERSION,
            sketchy: self.sketchy,
            next_id: self.next_id,
            sent: self.sent,
            shapes: self.shapes.iter().filter(|s| s.is_finite()).collect(),
        };
        serde_json::to_string(&file).expect("a scene is always serialisable")
    }

    /// Reads a scene back. A file from a newer version, or one that is not a scene, is refused;
    /// shapes with a coordinate that is not a number are dropped, and repeated ids are renumbered.
    pub fn from_json(json: &str) -> Result<Scene, String> {
        let file: FileOwned = serde_json::from_str(json).map_err(|e| e.to_string())?;
        if file.version > FILE_VERSION {
            return Err(format!(
                "board file version {} is newer than {FILE_VERSION}",
                file.version
            ));
        }
        let mut scene = Scene {
            sketchy: file.sketchy,
            next_id: file.next_id,
            sent: file.sent,
            ..Scene::default()
        };
        let mut seen = std::collections::HashSet::new();
        scene.next_id = scene.next_id.max(
            file.shapes
                .iter()
                .map(|s| s.id)
                .max()
                .map_or(1, |m| m.saturating_add(1)),
        );
        for mut sh in file.shapes.into_iter().filter(Shape::is_finite) {
            if sh.id == 0 || !seen.insert(sh.id) {
                sh.id = scene.fresh_id();
                seen.insert(sh.id);
            }
            scene.shapes.push(sh);
        }
        Ok(scene)
    }
}
