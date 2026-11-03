//! The PNG export: [`Scene::render_png`] draws the whole scene to a picture with no window and no
//! GPU, so a drawing can go to Claude as an image attachment from anywhere.
//!
//! It draws the very primitives the screen painter draws ([`geometry::prims`]), mapped from scene
//! units to pixels and rasterised by tiny-skia, with the palette's tokens for colours and the
//! outlines of the bundled body font for labels ([`font`]). A PNG therefore looks like the board,
//! framed on its drawing (the ink plus a margin of paper) and sharper (two pixels to a scene unit
//! by default). Nothing here panics on a strange scene: shapes with coordinates that are not
//! numbers are left out, and absurd sizes are capped to [`MAX_SIDE`].

use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Stroke, Transform,
};

use super::font;
use super::geom::Point;
use super::geometry::{self, Join, Prim};
use super::palette;
use super::scene::{Scene, Shape, Tone};

/// Blank paper around the drawing, in scene units.
pub const EXPORT_MARGIN: f32 = 32.0;

/// Longest side of an exported PNG, in pixels: a bigger picture is drawn at a lower scale.
pub const MAX_SIDE: u32 = 4096;

/// The scale the board sends to Claude with: two pixels to a scene unit, crisp enough for small
/// labels to be read.
pub const DEFAULT_SCALE: f32 = 2.0;

/// How far a miter may reach, in stroke widths, before it is cut to a bevel (what the screen
/// painter uses too).
const MITER_LIMIT: f32 = 4.0;

/// Where an export sits on the scene and how big it is: what a caller needs to know about a PNG
/// without drawing one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExportFrame {
    /// The picture's top-left corner, margin included, in scene units.
    pub origin: Point,
    /// Pixels per scene unit actually used: the scale asked for, or less when the picture would
    /// pass [`MAX_SIDE`].
    pub scale: f32,
    /// Width of the picture in pixels, at least 1 and at most [`MAX_SIDE`].
    pub width: u32,
    /// Height of the picture in pixels, at least 1 and at most [`MAX_SIDE`].
    pub height: u32,
}

impl ExportFrame {
    /// Where the scene point `p` lands in the picture, in (fractional) pixels. Worked out in double
    /// precision, so a drawing far from the origin does not lose its details to rounding.
    pub fn to_pixels(&self, p: Point) -> Point {
        let s = f64::from(self.scale);
        let at = |v: f32, origin: f32| ((f64::from(v) - f64::from(origin)) * s) as f32;
        Point::new(at(p.x, self.origin.x), at(p.y, self.origin.y))
    }
}

impl Scene {
    /// The frame an export at `scale` (pixels per scene unit) has.
    ///
    /// The picture shows the ink of every shape (the same box as [`Scene::bounds`], except that a
    /// shape whose box is not made of finite numbers is left out) with [`EXPORT_MARGIN`] of paper
    /// on every side; an empty scene is the margin alone, `2 * EXPORT_MARGIN` square. Each side is
    /// `ceil(scene units * scale)` pixels, at least 1. When the longer side would pass
    /// [`MAX_SIDE`] the scale is lowered, for both axes alike, until it is exactly [`MAX_SIDE`]. A
    /// `scale` that is not a positive number means [`DEFAULT_SCALE`].
    pub fn export_frame(&self, scale: f32) -> ExportFrame {
        let (min, max) = self
            .shapes()
            .iter()
            .map(Shape::ink_bounds)
            .filter(|b| b.min.is_finite() && b.max.is_finite())
            .reduce(|a, b| a.union(&b))
            .map_or((Point::ZERO, Point::ZERO), |b| (b.min, b.max));
        // In double precision: the f32 span of a drawing out at 1e38 would overflow.
        let margin = f64::from(EXPORT_MARGIN);
        let (x0, y0) = (f64::from(min.x) - margin, f64::from(min.y) - margin);
        let (w, h) = (
            f64::from(max.x) + margin - x0,
            f64::from(max.y) + margin - y0,
        );
        let mut scale = if scale.is_finite() && scale > 0.0 {
            f64::from(scale)
        } else {
            f64::from(DEFAULT_SCALE)
        };
        let longest = w.max(h);
        if longest * scale > f64::from(MAX_SIDE) {
            scale = f64::from(MAX_SIDE) / longest;
        }
        // The clamp also absorbs the last bit of rounding when the scale was lowered to fit.
        let side = |len: f64| (len * scale).ceil().clamp(1.0, f64::from(MAX_SIDE)) as u32;
        ExportFrame {
            origin: Point::new(x0 as f32, y0 as f32),
            scale: scale as f32,
            width: side(w),
            height: side(h),
        }
    }

    /// Renders the whole scene to a PNG.
    ///
    /// The picture is [`Scene::export_frame`]: the drawing's ink plus [`EXPORT_MARGIN`], at `scale`
    /// pixels per scene unit (the board sends [`DEFAULT_SCALE`]; a bad scale falls back to it and
    /// a huge picture is drawn smaller, see the frame). It is paper-coloured and opaque, with every
    /// shape drawn bottom to top, anti-aliased, as the primitives of [`geometry::prims`] (sketchy
    /// or not as the scene says). The same scene at the same scale always gives the same bytes.
    ///
    /// A shape with a coordinate that is not a number is not drawn. The result is empty only if
    /// the picture could not be allocated or encoded, which the size cap rules out.
    pub fn render_png(&self, scale: f32) -> Vec<u8> {
        let frame = self.export_frame(scale);
        let Some(mut pixmap) = Pixmap::new(frame.width, frame.height) else {
            return Vec::new();
        };
        let [r, g, b, a] = palette::rgba8(palette::paper());
        pixmap.fill(Color::from_rgba8(r, g, b, a));
        for shape in self.shapes() {
            for prim in geometry::prims(shape, self.sketchy, frame.scale) {
                draw(
                    &mut pixmap,
                    &prim.mapped(|p| frame.to_pixels(p), frame.scale),
                );
            }
        }
        // Encoding into a Vec has no way to fail; an empty result is better than a panic in a
        // build that aborts on one.
        pixmap.encode_png().unwrap_or_default()
    }
}

/// The paint of a tone: the palette's token, opaque, anti-aliased.
fn paint(tone: Tone) -> Paint<'static> {
    let [r, g, b, a] = palette::rgba8(palette::token(tone));
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint
}

/// A path through `points`, closed or not. None when tiny-skia refuses it: no points, or one that
/// is not a finite number. A single point gets a second one on top of it: a segment of no length
/// is what a round cap turns into a dot.
fn polyline(points: &[Point], closed: bool) -> Option<Path> {
    let (first, rest) = points.split_first()?;
    let mut path = PathBuilder::new();
    path.move_to(first.x, first.y);
    for p in rest {
        path.line_to(p.x, p.y);
    }
    if rest.is_empty() {
        path.line_to(first.x, first.y);
    }
    if closed {
        path.close();
    }
    path.finish()
}

/// Draws one primitive, already in pixels.
fn draw(pixmap: &mut Pixmap, prim: &Prim) {
    match prim {
        Prim::Fill { points, tone } => {
            // Fewer than three corners enclose nothing.
            if points.len() < 3 {
                return;
            }
            if let Some(path) = polyline(points, true) {
                pixmap.fill_path(
                    &path,
                    &paint(*tone),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }
        Prim::Stroke {
            points,
            closed,
            width,
            tone,
            join,
        } => {
            let stroke = Stroke {
                width: *width,
                miter_limit: MITER_LIMIT,
                line_cap: LineCap::Round,
                line_join: match join {
                    Join::Round => LineJoin::Round,
                    Join::Miter => LineJoin::Miter,
                },
                dash: None,
            };
            if let Some(path) = polyline(points, *closed) {
                pixmap.stroke_path(&path, &paint(*tone), &stroke, Transform::identity(), None);
            }
        }
        Prim::Text {
            at,
            text,
            size,
            tone,
        } => draw_text(pixmap, *at, text, *size, *tone),
    }
}

/// Draws a text block with its top-left corner at `at`: every line from `at.x`, one line pitch
/// below the one before, each on the baseline the screen puts it on. All the lines are one path,
/// filled once.
fn draw_text(pixmap: &mut Pixmap, at: Point, text: &str, size: f32, tone: Tone) {
    let pitch = size * font::LINE_HEIGHT;
    let to_baseline = font::baseline(size);
    let mut path = PathBuilder::new();
    for (i, line) in text.split('\n').enumerate() {
        font::push_line(
            &mut path,
            line,
            at.x,
            at.y + i as f32 * pitch + to_baseline,
            size,
        );
    }
    if let Some(path) = path.finish() {
        pixmap.fill_path(
            &path,
            &paint(tone),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

#[cfg(test)]
mod tests;
