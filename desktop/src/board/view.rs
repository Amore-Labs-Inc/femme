//! The view onto the scene: zoom and pan, and the maths between screen pixels (relative to the
//! canvas' top-left) and scene units. Pure.

use super::geom::{Point, Rect};

/// The smallest zoom (a quarter size).
pub const MIN_ZOOM: f32 = 0.25;

/// The largest zoom (four times).
pub const MAX_ZOOM: f32 = 4.0;

/// `fit` zooms in no further than this: a small diagram is not blown up to the whole canvas.
pub const FIT_MAX_ZOOM: f32 = 2.0;

/// Zoom and pan: a scene point `p` is at `p * zoom + pan` on the canvas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    /// Screen pixels per scene unit, between [`MIN_ZOOM`] and [`MAX_ZOOM`].
    pub zoom: f32,
    /// Where the scene origin is on the canvas, in pixels.
    pub pan: Point,
}

impl Default for View {
    fn default() -> View {
        View {
            zoom: 1.0,
            pan: Point::ZERO,
        }
    }
}

impl View {
    /// The canvas position of a scene point.
    pub fn to_screen(&self, p: Point) -> Point {
        p * self.zoom + self.pan
    }

    /// The scene point under a canvas position.
    pub fn to_scene(&self, p: Point) -> Point {
        (p - self.pan) * (1.0 / self.zoom)
    }

    /// Zooms by `factor` (clamped to the limits) keeping the scene point under `at` where it is.
    pub fn zoom_at(&mut self, at: Point, factor: f32) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let k = zoom / self.zoom;
        self.pan = at - (at - self.pan) * k;
        self.zoom = zoom;
    }

    /// Slides the scene by `d` pixels.
    pub fn pan_by(&mut self, d: Point) {
        self.pan = self.pan + d;
    }

    /// The view that shows `bounds` in the middle of a canvas of `viewport` pixels with `margin`
    /// pixels to spare, as large as [`FIT_MAX_ZOOM`] allows.
    pub fn fit(bounds: Rect, viewport: Point, margin: f32) -> View {
        let w = bounds.width().max(1.0);
        let h = bounds.height().max(1.0);
        let room = Point::new(
            (viewport.x - 2.0 * margin).max(1.0),
            (viewport.y - 2.0 * margin).max(1.0),
        );
        let zoom = (room.x / w).min(room.y / h).clamp(MIN_ZOOM, FIT_MAX_ZOOM);
        View {
            zoom,
            pan: viewport * 0.5 - bounds.center() * zoom,
        }
    }

    /// The zoom as a whole percentage, for the label.
    pub fn percent(&self) -> u32 {
        (self.zoom * 100.0).round() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f32, y: f32) -> Point {
        Point::new(x, y)
    }

    fn near(a: Point, b: Point) -> bool {
        a.dist(b) < 1e-3
    }

    #[test]
    fn screen_and_scene_are_inverse() {
        let v = View {
            zoom: 2.5,
            pan: p(40., -30.),
        };
        for q in [p(0., 0.), p(100., 50.), p(-7., 13.)] {
            assert!(near(v.to_scene(v.to_screen(q)), q));
        }
        assert_eq!(v.to_screen(p(0., 0.)), p(40., -30.));
    }

    #[test]
    fn zooming_keeps_the_point_under_the_pointer() {
        let mut v = View {
            zoom: 1.0,
            pan: p(10., 20.),
        };
        let at = p(300., 200.);
        let before = v.to_scene(at);
        v.zoom_at(at, 1.5);
        assert!((v.zoom - 1.5).abs() < 1e-6);
        assert!(near(v.to_scene(at), before));
        v.zoom_at(at, 1.0 / 3.0);
        assert!(near(v.to_scene(at), before));
    }

    #[test]
    fn zoom_stays_within_its_limits() {
        let mut v = View::default();
        for _ in 0..40 {
            v.zoom_at(p(5., 5.), 1.3);
        }
        assert_eq!(v.zoom, MAX_ZOOM);
        for _ in 0..80 {
            v.zoom_at(p(5., 5.), 0.7);
        }
        assert_eq!(v.zoom, MIN_ZOOM);
        let before = v;
        v.zoom_at(p(5., 5.), f32::NAN);
        v.zoom_at(p(5., 5.), -2.0);
        assert_eq!(v, before);
    }

    #[test]
    fn a_pan_slides_the_scene() {
        let mut v = View::default();
        v.pan_by(p(5., -3.));
        v.pan_by(p(1., 1.));
        assert_eq!(v.pan, p(6., -2.));
    }

    #[test]
    fn fit_centres_the_drawing_with_room_to_spare() {
        let b = Rect::new(p(100., 100.), p(500., 300.));
        let vp = p(1000., 600.);
        let v = View::fit(b, vp, 40.0);
        // 920 / 400 = 2.3 and 520 / 200 = 2.6 both exceed the cap: the cap holds.
        assert_eq!(v.zoom, FIT_MAX_ZOOM);
        assert!(near(v.to_screen(b.center()), p(500., 300.)));
        // A drawing wider than the canvas is shrunk to fit.
        let wide = Rect::new(p(0., 0.), p(3000., 1000.));
        let v = View::fit(wide, vp, 40.0);
        assert!((v.zoom - 920.0 / 3000.0).abs() < 1e-5);
        let (tl, br) = (v.to_screen(wide.min), v.to_screen(wide.max));
        assert!(tl.x >= 39.9 && br.x <= 960.1 && tl.y >= 0.0 && br.y <= 600.0);
        // So wide that even the smallest zoom cannot show it all: the limit holds.
        let huge = Rect::new(p(0., 0.), p(40_000., 100.));
        assert_eq!(View::fit(huge, vp, 40.0).zoom, MIN_ZOOM);
        // A degenerate box (a single point) does not divide by zero.
        let dot = View::fit(Rect::new(p(5., 5.), p(5., 5.)), vp, 40.0);
        assert_eq!(dot.zoom, FIT_MAX_ZOOM);
    }

    #[test]
    fn percent_rounds() {
        assert_eq!(View::default().percent(), 100);
        let mut v = View::default();
        v.zoom_at(Point::ZERO, 1.1);
        assert_eq!(v.percent(), 110);
    }
}
