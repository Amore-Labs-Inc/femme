//! The board as a list of plain draw operations in canvas pixels: what [`super::paint`] hands to
//! gpui. Everything that needs thinking (culling, the grid, strokes as triangles, the selection and
//! marquee, the text being typed) happens here, with no window, so it is tested like the rest.
//!
//! Shapes come from the same [`geometry::prims`] the PNG export draws, so the screen and the
//! picture that is sent cannot disagree.

use gpui::Rgba;

use super::editor::Editor;
use super::font;
use super::geom::{Point, Rect};
use super::geometry::{prims, Prim};
use super::palette;
use super::scene::{Shape, Tone};
use super::stroke::{fan_from_centroid, outline, Cap};
use super::view::View;

/// The spacing of the grid dots at zoom 1, in scene units.
const GRID_STEP: f32 = 24.0;

/// The grid doubles its spacing until the dots are at least this far apart on the screen.
const GRID_MIN_PX: f32 = 14.0;

/// Side of a grid dot, in pixels.
const DOT_PX: f32 = 2.0;

/// Room around a selected shape's box, in pixels.
const SELECTION_PAD: f32 = 6.0;

/// The border of a [`Op::Frame`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edge {
    /// The colour.
    pub color: Rgba,
    /// The width in pixels.
    pub width: f32,
    /// Drawn as dashes.
    pub dashed: bool,
}

/// One thing to draw, in canvas pixels (the canvas' top-left is the origin).
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// Filled triangles of one colour: the strokes and fills of the shapes.
    Tris {
        /// The triangles.
        tris: Vec<[Point; 3]>,
        /// The colour.
        color: Rgba,
    },
    /// Small squares, the grid.
    Dots {
        /// Top-left corners.
        at: Vec<Point>,
        /// Side length.
        size: f32,
        /// The colour.
        color: Rgba,
    },
    /// A text block in the body font, lines split by `\n`.
    Text {
        /// Top-left corner of the block.
        at: Point,
        /// The text.
        text: String,
        /// Font size.
        size: f32,
        /// The colour.
        color: Rgba,
    },
    /// A box: a selection, the marquee, the frame of the text being typed.
    Frame {
        /// The box.
        rect: Rect,
        /// What fills it.
        fill: Option<Rgba>,
        /// Its border.
        edge: Option<Edge>,
    },
    /// The caret of the text being typed: a bar the height of the line, placed from the shaped line.
    Caret {
        /// Top-left corner of the text block it is in.
        at: Point,
        /// The whole text.
        text: String,
        /// Font size.
        size: f32,
        /// The caret's byte index in `text`.
        byte: usize,
        /// The colour.
        color: Rgba,
    },
}

/// Everything to paint for `ed` on a canvas of `viewport` pixels, bottom to top: the grid, the
/// shapes, the selection, the marquee, the text being typed.
pub fn draw_list(ed: &Editor, viewport: Point) -> Vec<Op> {
    let view = ed.view();
    let scene = ed.scene();
    let mut ops = Vec::new();
    grid(&view, viewport, &mut ops);
    let hidden = ed.typing().and_then(|t| t.target);
    let screen = Rect::new(Point::ZERO, viewport).expand(32.0);
    for shape in scene.shapes() {
        if Some(shape.id) == hidden || !on_screen(shape, &view, &screen) {
            continue;
        }
        shape_ops(shape, scene.sketchy, &view, &mut ops);
    }
    selection(ed, &mut ops);
    if let Some(m) = ed.marquee() {
        let c = palette::selection();
        ops.push(Op::Frame {
            rect: to_screen(&view, m),
            fill: Some(Rgba { a: 0.10, ..c }),
            edge: Some(Edge {
                color: c,
                width: 1.0,
                dashed: true,
            }),
        });
    }
    typing(ed, &mut ops);
    ops
}

fn to_screen(view: &View, r: Rect) -> Rect {
    Rect::new(view.to_screen(r.min), view.to_screen(r.max))
}

/// The shape's ink touches the visible canvas.
fn on_screen(shape: &Shape, view: &View, screen: &Rect) -> bool {
    to_screen(view, shape.ink_bounds()).intersects(screen)
}

/// The dots of the grid: every [`GRID_STEP`] scene units, every doubling of that when zoomed out
/// far enough that they would crowd.
fn grid(view: &View, viewport: Point, ops: &mut Vec<Op>) {
    let mut step = GRID_STEP;
    while step * view.zoom < GRID_MIN_PX {
        step *= 2.0;
    }
    // The dots inside the canvas: the first one at or after its top-left, the last at or before
    // its bottom-right.
    let top_left = view.to_scene(Point::ZERO);
    let bottom_right = view.to_scene(viewport);
    let (x0, y0) = ((top_left.x / step).ceil(), (top_left.y / step).ceil());
    let (x1, y1) = (
        (bottom_right.x / step).floor(),
        (bottom_right.y / step).floor(),
    );
    let mut at = Vec::new();
    let mut gy = y0;
    while gy <= y1 {
        let mut gx = x0;
        while gx <= x1 {
            let p = view.to_screen(Point::new(gx * step, gy * step));
            at.push(Point::new(p.x - DOT_PX * 0.5, p.y - DOT_PX * 0.5));
            gx += 1.0;
        }
        gy += 1.0;
    }
    ops.push(Op::Dots {
        at,
        size: DOT_PX,
        color: palette::grid(),
    });
}

/// How many segments a disc of `radius` pixels is cut into.
fn disc_segments(radius: f32) -> usize {
    ((radius * 4.0).ceil() as usize).clamp(8, 36)
}

/// The draw operations of one shape.
fn shape_ops(shape: &Shape, sketchy: bool, view: &View, ops: &mut Vec<Op>) {
    for prim in prims(shape, sketchy, view.zoom) {
        match prim.mapped(|p| view.to_screen(p), view.zoom) {
            Prim::Fill { points, tone } => {
                let mut tris = Vec::new();
                fan_from_centroid(&points, &mut tris);
                ops.push(Op::Tris {
                    tris,
                    color: palette::token(tone),
                });
            }
            Prim::Stroke {
                points,
                closed,
                width,
                tone,
                join,
            } => {
                let o = outline(&points, width, closed, join, Cap::Round);
                ops.push(Op::Tris {
                    tris: o.triangles(disc_segments(o.radius)),
                    color: palette::token(tone),
                });
            }
            Prim::Text {
                at,
                text,
                size,
                tone,
            } => ops.push(Op::Text {
                at,
                text,
                size,
                color: palette::token(tone),
            }),
        }
    }
}

/// A dashed box around every selected shape, and one around the lot when there are several.
fn selection(ed: &Editor, ops: &mut Vec<Op>) {
    let view = ed.view();
    let edge = Edge {
        color: palette::selection(),
        width: 1.2,
        dashed: true,
    };
    // Room all round, so even a flat line gets a box with some height.
    let padded = |r: Rect| to_screen(&view, r).expand(SELECTION_PAD);
    // A text being typed has its own frame, around what is typed so far; the box of the old text
    // under it would only be a second, stale frame.
    let typed = ed.typing().and_then(|t| t.target);
    for id in ed.selection() {
        if Some(*id) == typed {
            continue;
        }
        if let Some(shape) = ed.scene().get(*id) {
            ops.push(Op::Frame {
                rect: padded(shape.bounds()),
                fill: None,
                edge: Some(edge),
            });
        }
    }
    if ed.selection().len() > 1 {
        if let Some(b) = ed.selection_bounds() {
            ops.push(Op::Frame {
                rect: padded(b).expand(SELECTION_PAD),
                fill: None,
                edge: Some(Edge {
                    dashed: false,
                    ..edge
                }),
            });
        }
    }
}

/// The text being typed: the text, a dashed frame around it, and the caret.
fn typing(ed: &Editor, ops: &mut Vec<Op>) {
    let Some(t) = ed.typing() else {
        return;
    };
    let view = ed.view();
    let size = t.style.width.text_size();
    let (w, h) = font::measure(t.text(), size);
    let box_ = to_screen(&view, Rect::new(t.at, t.at + Point::new(w.max(size), h))).expand(4.0);
    let color = palette::token(Tone::from(t.style.color));
    let at = view.to_screen(t.at);
    ops.push(Op::Frame {
        rect: box_,
        fill: None,
        edge: Some(Edge {
            color: palette::selection(),
            width: 1.0,
            dashed: true,
        }),
    });
    ops.push(Op::Text {
        at,
        text: t.text().to_owned(),
        size: size * view.zoom,
        color,
    });
    ops.push(Op::Caret {
        at,
        text: t.text().to_owned(),
        size: size * view.zoom,
        byte: t.caret(),
        color,
    });
}

#[cfg(test)]
mod tests;
