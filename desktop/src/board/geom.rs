//! The plane the board is drawn on: points, boxes and the distance from a point to a segment, in
//! *scene units* (logical pixels at zoom 1). Pure arithmetic, no gpui.

use std::ops::{Add, Mul, Neg, Sub};

use serde::{Deserialize, Serialize};

/// A point (or a vector) in scene units. Saved as `[x, y]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "[f32; 2]", into = "[f32; 2]")]
pub struct Point {
    /// Horizontal position, to the right.
    pub x: f32,
    /// Vertical position, downwards.
    pub y: f32,
}

impl From<[f32; 2]> for Point {
    fn from(a: [f32; 2]) -> Point {
        Point { x: a[0], y: a[1] }
    }
}

impl From<Point> for [f32; 2] {
    fn from(p: Point) -> [f32; 2] {
        [p.x, p.y]
    }
}

impl Add for Point {
    type Output = Point;
    fn add(self, o: Point) -> Point {
        Point::new(self.x + o.x, self.y + o.y)
    }
}

impl Sub for Point {
    type Output = Point;
    fn sub(self, o: Point) -> Point {
        Point::new(self.x - o.x, self.y - o.y)
    }
}

impl Mul<f32> for Point {
    type Output = Point;
    fn mul(self, k: f32) -> Point {
        Point::new(self.x * k, self.y * k)
    }
}

impl Neg for Point {
    type Output = Point;
    fn neg(self) -> Point {
        Point::new(-self.x, -self.y)
    }
}

impl Point {
    /// The origin.
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    /// A point at `(x, y)`.
    pub const fn new(x: f32, y: f32) -> Point {
        Point { x, y }
    }

    /// Length of the vector from the origin.
    pub fn len(self) -> f32 {
        self.x.hypot(self.y)
    }

    /// Distance to `o`.
    pub fn dist(self, o: Point) -> f32 {
        (self - o).len()
    }

    /// `self` moved towards `o` by `t` (`0.0` is `self`, `1.0` is `o`).
    pub fn lerp(self, o: Point, t: f32) -> Point {
        self + (o - self) * t
    }

    /// Dot product.
    pub fn dot(self, o: Point) -> f32 {
        self.x * o.x + self.y * o.y
    }

    /// Z component of the cross product: positive when `o` lies clockwise of `self` on a y-down screen.
    pub fn cross(self, o: Point) -> f32 {
        self.x * o.y - self.y * o.x
    }

    /// The vector turned a quarter turn (`(-y, x)`).
    pub fn normal(self) -> Point {
        Point::new(-self.y, self.x)
    }

    /// The vector scaled to length 1; none for the zero vector.
    pub fn unit(self) -> Option<Point> {
        let l = self.len();
        (l > 1e-6).then(|| self * (1.0 / l))
    }

    /// Both coordinates are finite numbers.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// Distance from `p` to the segment `a`..`b`.
pub fn dist_to_segment(p: Point, a: Point, b: Point) -> f32 {
    let ab = b - a;
    let len2 = ab.dot(ab);
    if len2 < 1e-12 {
        return p.dist(a);
    }
    let t = ((p - a).dot(ab) / len2).clamp(0.0, 1.0);
    p.dist(a + ab * t)
}

/// An axis-aligned box in scene units (`min` is the top-left, `max` the bottom-right).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// Top-left corner.
    pub min: Point,
    /// Bottom-right corner.
    pub max: Point,
}

impl Rect {
    /// The box spanned by two opposite corners, in any order.
    pub fn new(a: Point, b: Point) -> Rect {
        Rect {
            min: Point::new(a.x.min(b.x), a.y.min(b.y)),
            max: Point::new(a.x.max(b.x), a.y.max(b.y)),
        }
    }

    /// The smallest box holding every point; none for no points.
    pub fn around(points: &[Point]) -> Option<Rect> {
        let first = *points.first()?;
        Some(
            points
                .iter()
                .fold(Rect::new(first, first), |r, p| r.union(&Rect::new(*p, *p))),
        )
    }

    /// Width.
    pub fn width(&self) -> f32 {
        self.max.x - self.min.x
    }

    /// Height.
    pub fn height(&self) -> f32 {
        self.max.y - self.min.y
    }

    /// Middle of the box.
    pub fn center(&self) -> Point {
        self.min.lerp(self.max, 0.5)
    }

    /// The box grown by `d` on every side (shrunk for a negative `d`; a box shrunk past nothing
    /// contains no point).
    pub fn expand(&self, d: f32) -> Rect {
        Rect {
            min: Point::new(self.min.x - d, self.min.y - d),
            max: Point::new(self.max.x + d, self.max.y + d),
        }
    }

    /// The smallest box holding both.
    pub fn union(&self, o: &Rect) -> Rect {
        Rect {
            min: Point::new(self.min.x.min(o.min.x), self.min.y.min(o.min.y)),
            max: Point::new(self.max.x.max(o.max.x), self.max.y.max(o.max.y)),
        }
    }

    /// The point is inside (edges included).
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }

    /// `o` lies wholly inside this box.
    pub fn contains_rect(&self, o: &Rect) -> bool {
        self.contains(o.min) && self.contains(o.max)
    }

    /// The two boxes share at least a point.
    pub fn intersects(&self, o: &Rect) -> bool {
        self.min.x <= o.max.x
            && o.min.x <= self.max.x
            && self.min.y <= o.max.y
            && o.min.y <= self.max.y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f32, y: f32) -> Point {
        Point::new(x, y)
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn segment_distance_handles_ends_and_points() {
        assert!(near(dist_to_segment(p(5., 3.), p(0., 0.), p(10., 0.)), 3.0));
        assert!(near(
            dist_to_segment(p(-3., 4.), p(0., 0.), p(10., 0.)),
            5.0
        ));
        assert!(near(dist_to_segment(p(3., 4.), p(0., 0.), p(0., 0.)), 5.0));
    }

    #[test]
    fn a_rect_is_normalised_whatever_order_the_corners_come_in() {
        let r = Rect::new(p(10., 20.), p(-5., 0.));
        assert_eq!(r.min, p(-5., 0.));
        assert_eq!(r.max, p(10., 20.));
        assert!(near(r.width(), 15.0) && near(r.height(), 20.0));
        assert_eq!(r.center(), p(2.5, 10.));
    }

    #[test]
    fn rects_contain_intersect_and_union() {
        let a = Rect::new(p(0., 0.), p(10., 10.));
        let b = Rect::new(p(5., 5.), p(20., 20.));
        let c = Rect::new(p(30., 30.), p(40., 40.));
        assert!(a.contains(p(10., 10.)) && !a.contains(p(10.1, 5.)));
        assert!(a.intersects(&b) && !a.intersects(&c));
        assert!(!a.contains_rect(&b));
        assert!(a.union(&b).contains_rect(&a) && a.union(&b).contains_rect(&b));
        assert!(Rect::new(p(0., 0.), p(50., 50.)).contains_rect(&c));
    }

    #[test]
    fn a_rect_shrunk_past_nothing_holds_no_point() {
        let a = Rect::new(p(0., 0.), p(10., 4.));
        assert!(a.expand(-1.0).contains(p(5., 2.)));
        assert!(!a.expand(-3.0).contains(p(5., 2.)));
        assert_eq!(a.expand(2.0).min, p(-2., -2.));
    }

    #[test]
    fn around_takes_the_extremes_of_a_set_of_points() {
        assert_eq!(Rect::around(&[]), None);
        let r = Rect::around(&[p(3., 9.), p(-1., 4.), p(7., 0.)]).unwrap();
        assert_eq!((r.min, r.max), (p(-1., 0.), p(7., 9.)));
    }

    #[test]
    fn vectors_do_their_arithmetic() {
        assert_eq!(p(1., 2.) + p(3., 4.), p(4., 6.));
        assert_eq!(p(1., 2.) - p(3., 4.), p(-2., -2.));
        assert_eq!(p(1., 2.) * 2.0, p(2., 4.));
        assert_eq!(-p(1., -2.), p(-1., 2.));
        assert!(near(p(3., 4.).len(), 5.0));
        assert_eq!(p(0., 0.).lerp(p(10., 20.), 0.5), p(5., 10.));
        assert!(p(0., 0.).unit().is_none());
        assert!(near(p(0., 7.).unit().unwrap().y, 1.0));
        assert!(near(p(1., 0.).cross(p(0., 1.)), 1.0));
        assert_eq!(p(1., 0.).normal(), p(0., 1.));
        assert!(!p(f32::NAN, 0.).is_finite() && p(1., 2.).is_finite());
    }

    #[test]
    fn a_point_is_saved_as_a_pair() {
        assert_eq!(serde_json::to_string(&p(1.5, -2.0)).unwrap(), "[1.5,-2.0]");
        assert_eq!(serde_json::from_str::<Point>("[3,4]").unwrap(), p(3., 4.));
        assert!(serde_json::from_str::<Point>("[3]").is_err());
    }
}
