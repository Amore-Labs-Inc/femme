//! The gpui painter: draws a [`draw_list`](super::drawlist::draw_list) and the tool icons. All the
//! thinking (culling, strokes as triangles, selection boxes) happened when the list was made; this
//! only turns its operations into quads, paths and shaped text.

use gpui::{
    fill, font, point, px, quad, size, transparent_black, App, BorderStyle, Bounds, Corners, Edges,
    Hsla, Path, Pixels, ShapedLine, SharedString, TextAlign, TextRun, Window,
};

use super::drawlist::Op;
use super::font::LINE_HEIGHT;
use super::geom::Point;
use super::geometry::Join;
use super::icons::{self, Line};
use super::stroke::{fan_from_centroid, outline, Cap};
use crate::theme::FONT_UI;

/// The width of the caret of a text being typed, in pixels.
const CARET_WIDTH: f32 = 1.5;

/// A point of the draw list (canvas pixels) as a point of the window.
fn at(origin: gpui::Point<Pixels>, p: Point) -> gpui::Point<Pixels> {
    point(origin.x + px(p.x), origin.y + px(p.y))
}

/// Paints `ops` with the canvas' top-left corner at `origin`, in the order they come.
pub fn paint_ops(ops: &[Op], origin: gpui::Point<Pixels>, window: &mut Window, cx: &mut App) {
    for op in ops {
        match op {
            Op::Tris { tris, color } => paint_tris(tris, origin, Hsla::from(*color), window),
            Op::Dots {
                at: corners,
                size: side,
                color,
            } => {
                let color = Hsla::from(*color);
                for p in corners {
                    let bounds = Bounds::new(at(origin, *p), size(px(*side), px(*side)));
                    window.paint_quad(fill(bounds, color));
                }
            }
            Op::Text {
                at: top_left,
                text,
                size: em,
                color,
            } => paint_text(
                at(origin, *top_left),
                text,
                *em,
                Hsla::from(*color),
                window,
                cx,
            ),
            Op::Frame {
                rect,
                fill: wash,
                edge,
            } => {
                let bounds = Bounds::new(
                    at(origin, rect.min),
                    size(px(rect.width()), px(rect.height())),
                );
                let background = wash.map_or_else(transparent_black, Hsla::from);
                let (width, color, style) = match edge {
                    Some(e) => (
                        e.width,
                        Hsla::from(e.color),
                        if e.dashed {
                            BorderStyle::Dashed
                        } else {
                            BorderStyle::Solid
                        },
                    ),
                    None => (0.0, transparent_black(), BorderStyle::Solid),
                };
                window.paint_quad(quad(
                    bounds,
                    Corners::default(),
                    background,
                    Edges::all(px(width)),
                    color,
                    style,
                ));
            }
            Op::Caret {
                at: top_left,
                text,
                size: em,
                byte,
                color,
            } => paint_caret(
                at(origin, *top_left),
                text,
                *em,
                *byte,
                Hsla::from(*color),
                window,
            ),
        }
    }
}

/// Paints triangles (points in canvas pixels, `origin` the canvas' corner) as one path.
fn paint_tris(tris: &[[Point; 3]], origin: gpui::Point<Pixels>, color: Hsla, window: &mut Window) {
    let Some(first) = tris.first() else {
        return;
    };
    // gpui's path triangles with st = (0, 1) are plain filled triangles.
    let st = (point(0.0, 1.0), point(0.0, 1.0), point(0.0, 1.0));
    let mut path = Path::new(at(origin, first[0]));
    for t in tris {
        path.push_triangle((at(origin, t[0]), at(origin, t[1]), at(origin, t[2])), st);
    }
    window.paint_path(path, color);
}

/// One line of text shaped in the body font.
fn shape(line: &str, em: f32, color: Hsla, window: &Window) -> ShapedLine {
    let run = TextRun {
        len: line.len(),
        font: font(FONT_UI),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(SharedString::from(line.to_owned()), px(em), &[run], None)
}

/// Paints a block of text, one shaped line per `\n`, a line box of `em * LINE_HEIGHT` each.
fn paint_text(
    top_left: gpui::Point<Pixels>,
    text: &str,
    em: f32,
    color: Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    let pitch = em * LINE_HEIGHT;
    for (i, line) in text.split('\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let origin = point(top_left.x, top_left.y + px(pitch * i as f32));
        // A line that cannot be drawn is left out: there is nothing better to do with it.
        let _ = shape(line, em, color, window).paint(
            origin,
            px(pitch),
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

/// Paints the caret at byte `byte` of `text`: a bar the height of the line, where the shaped line
/// puts that byte.
fn paint_caret(
    top_left: gpui::Point<Pixels>,
    text: &str,
    em: f32,
    byte: usize,
    color: Hsla,
    window: &mut Window,
) {
    let byte = byte.min(text.len());
    let before = &text[..byte];
    let row = before.matches('\n').count();
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    let line = text[start..].split('\n').next().unwrap_or("");
    let x = if line.is_empty() {
        px(0.0)
    } else {
        shape(line, em, color, window).x_for_index(byte - start)
    };
    let pitch = em * LINE_HEIGHT;
    let bar = Bounds::new(
        point(top_left.x + x, top_left.y + px(pitch * row as f32)),
        size(px(CARET_WIDTH), px(pitch)),
    );
    window.paint_quad(fill(bar, color));
}

/// Paints the icon `lines` in `bounds` (the icon's 20 unit box is scaled to fit, and centred).
pub fn paint_icon(lines: &[Line], bounds: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let k = w.min(h) / icons::BOX;
    let (x0, y0) = (
        f32::from(bounds.origin.x) + (w - icons::BOX * k) / 2.0,
        f32::from(bounds.origin.y) + (h - icons::BOX * k) / 2.0,
    );
    let zero = point(px(0.0), px(0.0));
    for line in lines {
        let corners: Vec<Point> = line
            .points
            .iter()
            .map(|p| Point::new(x0 + p.x * k, y0 + p.y * k))
            .collect();
        let tris = if line.filled {
            let mut t = Vec::new();
            fan_from_centroid(&corners, &mut t);
            t
        } else {
            outline(
                &corners,
                icons::STROKE * k,
                line.closed,
                Join::Round,
                Cap::Round,
            )
            .triangles(12)
        };
        paint_tris(&tris, zero, color, window);
    }
}
