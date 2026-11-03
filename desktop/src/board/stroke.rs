//! The outline of a stroked polyline as plain convex pieces: one quad per segment, a disc or a
//! wedge at the joints, a disc at round caps. gpui paints filled triangles, not strokes, so the
//! screen painter turns the pieces into triangles ([`Outline::triangles`]); opaque colours make the
//! overlaps between pieces invisible.

use super::geom::Point;
use super::geometry::Join;

/// What the ends of an open polyline look like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    /// Cut square at the end point.
    Butt,
    /// A half disc past the end point.
    Round,
}

/// The pieces a stroke is made of.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outline {
    /// One per segment: the segment's two end points pushed out to either side by half the width.
    /// Corners in order `a+n, b+n, b-n, a-n`.
    pub quads: Vec<[Point; 4]>,
    /// Convex polygons that fill the gap on the outside of a corner (miter or bevel joins).
    pub wedges: Vec<Vec<Point>>,
    /// Centres of discs of [`Outline::radius`]: round joins and round caps.
    pub discs: Vec<Point>,
    /// Half the stroke width.
    pub radius: f32,
}

/// A turn sharper than this many radians gets a round join; a gentler one is hidden by the quads.
const ROUND_JOIN_MIN_TURN: f32 = 0.09;

/// How far a miter may reach, in half widths, before it is cut to a bevel (tiny-skia's default).
const MITER_LIMIT: f32 = 4.0;

/// The outline of the polyline `points` drawn `width` wide. Repeated points are ignored; a single
/// point is a dot when the cap is round and nothing otherwise.
pub fn outline(points: &[Point], width: f32, closed: bool, join: Join, cap: Cap) -> Outline {
    let hw = width * 0.5;
    let mut out = Outline {
        radius: hw,
        ..Outline::default()
    };
    let mut pts: Vec<Point> = Vec::with_capacity(points.len());
    for p in points {
        if pts.last().is_none_or(|l| l.dist(*p) > 1e-4) {
            pts.push(*p);
        }
    }
    if closed && pts.len() > 1 && pts[0].dist(pts[pts.len() - 1]) <= 1e-4 {
        pts.pop();
    }
    let n = pts.len();
    if n == 0 || hw <= 0.0 {
        return out;
    }
    if n == 1 {
        if cap == Cap::Round {
            out.discs.push(pts[0]);
        }
        return out;
    }
    let closed = closed && n > 2;
    let segments = if closed { n } else { n - 1 };
    let dir = |i: usize| {
        (pts[(i + 1) % n] - pts[i])
            .unit()
            .unwrap_or(Point::new(1.0, 0.0))
    };
    for i in 0..segments {
        let (a, b) = (pts[i], pts[(i + 1) % n]);
        let nrm = dir(i).normal() * hw;
        out.quads.push([a + nrm, b + nrm, b - nrm, a - nrm]);
    }
    // Joints: every interior vertex, and all of them for a closed polyline.
    let joints = if closed { 0..n } else { 1..n - 1 };
    for i in joints {
        let d0 = dir((i + n - 1) % n);
        let d1 = dir(i);
        let cross = d0.cross(d1);
        let dot = d0.dot(d1);
        if cross.abs() < 1e-4 && dot > 0.0 {
            continue;
        }
        match join {
            Join::Round => {
                if cross.abs().atan2(dot) > ROUND_JOIN_MIN_TURN {
                    out.discs.push(pts[i]);
                }
            }
            Join::Miter => out.wedges.push(miter(pts[i], d0, d1, cross, dot, hw)),
        }
    }
    if !closed && cap == Cap::Round {
        out.discs.push(pts[0]);
        out.discs.push(pts[n - 1]);
    }
    out
}

/// The wedge on the outside of the corner at `p` between a segment going `d0` and one going `d1`.
fn miter(p: Point, d0: Point, d1: Point, cross: f32, dot: f32, hw: f32) -> Vec<Point> {
    // The gap opens on the side away from the turn.
    let side = if cross > 0.0 { -1.0 } else { 1.0 };
    let n0 = d0.normal() * side;
    let n1 = d1.normal() * side;
    let (o0, o1) = (p + n0 * hw, p + n1 * hw);
    // Miter length over half width is 1 / cos(half the turn): sqrt(2 / (1 + n0.n1)).
    let cos_between = n0.dot(n1);
    let ratio = (2.0 / (1.0 + cos_between).max(1e-6)).sqrt();
    if dot < -0.999 || ratio > MITER_LIMIT {
        return vec![p, o0, o1];
    }
    let tip = p + (n0 + n1) * (hw / (1.0 + cos_between));
    vec![p, o0, tip, o1]
}

impl Outline {
    /// Everything as triangles, discs drawn as fans of `disc_segments` (at least 6).
    pub fn triangles(&self, disc_segments: usize) -> Vec<[Point; 3]> {
        let mut t = Vec::with_capacity(self.quads.len() * 2 + self.discs.len() * disc_segments);
        for q in &self.quads {
            t.push([q[0], q[1], q[2]]);
            t.push([q[0], q[2], q[3]]);
        }
        for w in &self.wedges {
            fan(w, &mut t);
        }
        let n = disc_segments.max(6);
        for c in &self.discs {
            for k in 0..n {
                let a0 = std::f32::consts::TAU * k as f32 / n as f32;
                let a1 = std::f32::consts::TAU * (k + 1) as f32 / n as f32;
                t.push([
                    *c,
                    Point::new(c.x + self.radius * a0.cos(), c.y + self.radius * a0.sin()),
                    Point::new(c.x + self.radius * a1.cos(), c.y + self.radius * a1.sin()),
                ]);
            }
        }
        t
    }
}

/// Triangles fanning out of the first vertex of a convex polygon.
pub fn fan(poly: &[Point], out: &mut Vec<[Point; 3]>) {
    for i in 1..poly.len().saturating_sub(1) {
        out.push([poly[0], poly[i], poly[i + 1]]);
    }
}

/// Triangles fanning out of the centroid, for a polygon that is star-shaped around it (the fill of
/// a wobbly ellipse): unlike [`fan`] it does not need a convex polygon.
pub fn fan_from_centroid(poly: &[Point], out: &mut Vec<[Point; 3]>) {
    if poly.len() < 3 {
        return;
    }
    let sum = poly.iter().fold(Point::ZERO, |s, p| s + *p);
    let c = sum * (1.0 / poly.len() as f32);
    for i in 0..poly.len() {
        out.push([c, poly[i], poly[(i + 1) % poly.len()]]);
    }
}

/// Twice the signed area of a triangle (positive when clockwise on a y-down screen).
pub fn area2(t: &[Point; 3]) -> f32 {
    (t[1] - t[0]).cross(t[2] - t[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f32, y: f32) -> Point {
        Point::new(x, y)
    }

    fn near(a: Point, b: Point) -> bool {
        a.dist(b) < 1e-4
    }

    #[test]
    fn a_two_point_line_is_one_quad_of_the_right_width() {
        let o = outline(&[p(0., 0.), p(10., 0.)], 4.0, false, Join::Round, Cap::Butt);
        assert_eq!(o.quads.len(), 1);
        assert!(o.wedges.is_empty() && o.discs.is_empty());
        let q = o.quads[0];
        assert!(near(q[0], p(0., 2.)) && near(q[1], p(10., 2.)));
        assert!(near(q[2], p(10., -2.)) && near(q[3], p(0., -2.)));
        // Width is the distance between the two long edges, whatever the direction.
        let o = outline(
            &[p(3., 3.), p(3. + 6., 3. + 8.)],
            5.0,
            false,
            Join::Round,
            Cap::Butt,
        );
        let q = o.quads[0];
        assert!((q[0].dist(q[3]) - 5.0).abs() < 1e-4);
        assert!((q[1].dist(q[2]) - 5.0).abs() < 1e-4);
        assert!(
            ((q[0] - q[1]).len() - 10.0).abs() < 1e-4,
            "its length is the segment's"
        );
    }

    #[test]
    fn round_caps_put_a_disc_at_each_end() {
        let o = outline(
            &[p(0., 0.), p(10., 0.)],
            4.0,
            false,
            Join::Round,
            Cap::Round,
        );
        assert_eq!(o.discs, vec![p(0., 0.), p(10., 0.)]);
        assert_eq!(o.radius, 2.0);
    }

    #[test]
    fn a_dot_is_a_disc_with_a_round_cap_and_nothing_without() {
        let o = outline(&[p(5., 5.), p(5., 5.)], 6.0, false, Join::Round, Cap::Round);
        assert_eq!(o.discs, vec![p(5., 5.)]);
        assert!(o.quads.is_empty());
        assert_eq!(
            outline(&[p(5., 5.)], 6.0, false, Join::Round, Cap::Butt),
            Outline {
                radius: 3.0,
                ..Outline::default()
            }
        );
        assert!(outline(&[], 6.0, false, Join::Round, Cap::Round)
            .quads
            .is_empty());
        assert!(
            outline(&[p(0., 0.), p(5., 0.)], 0.0, false, Join::Round, Cap::Round)
                .quads
                .is_empty()
        );
    }

    #[test]
    fn a_corner_gets_a_round_join_and_a_straight_run_does_not() {
        let corner = outline(
            &[p(0., 0.), p(10., 0.), p(10., 10.)],
            4.0,
            false,
            Join::Round,
            Cap::Butt,
        );
        assert_eq!(corner.quads.len(), 2);
        assert_eq!(corner.discs, vec![p(10., 0.)]);
        let straight = outline(
            &[p(0., 0.), p(10., 0.), p(20., 0.)],
            4.0,
            false,
            Join::Round,
            Cap::Butt,
        );
        assert!(straight.discs.is_empty());
        // A hair of a turn is hidden by the quads.
        let gentle = outline(
            &[p(0., 0.), p(10., 0.), p(20., 0.5)],
            4.0,
            false,
            Join::Round,
            Cap::Butt,
        );
        assert!(gentle.discs.is_empty());
    }

    #[test]
    fn a_miter_join_fills_the_outside_corner() {
        // Right, then down: the outside corner is at the top right, (12, -2).
        let o = outline(
            &[p(0., 0.), p(10., 0.), p(10., 10.)],
            4.0,
            false,
            Join::Miter,
            Cap::Butt,
        );
        assert_eq!(o.wedges.len(), 1);
        let w = &o.wedges[0];
        assert_eq!(w.len(), 4);
        assert!(w.iter().any(|v| near(*v, p(12., -2.))), "{w:?}");
        // Left, then up: the outside corner is at the bottom left of the turn.
        let o = outline(
            &[p(10., 10.), p(0., 10.), p(0., 0.)],
            4.0,
            false,
            Join::Miter,
            Cap::Butt,
        );
        assert!(
            o.wedges[0].iter().any(|v| near(*v, p(-2., 12.))),
            "{:?}",
            o.wedges[0]
        );
    }

    #[test]
    fn a_very_sharp_miter_is_cut_to_a_bevel() {
        let o = outline(
            &[p(0., 0.), p(100., 0.), p(0., 3.)],
            4.0,
            false,
            Join::Miter,
            Cap::Butt,
        );
        assert_eq!(o.wedges[0].len(), 3, "a bevel is a triangle");
    }

    #[test]
    fn a_closed_square_has_four_quads_and_four_corners() {
        let sq = [p(0., 0.), p(10., 0.), p(10., 10.), p(0., 10.)];
        let o = outline(&sq, 2.0, true, Join::Miter, Cap::Round);
        assert_eq!(o.quads.len(), 4);
        assert_eq!(o.wedges.len(), 4);
        assert!(o.discs.is_empty(), "a closed outline has no caps");
        // The same square with its first point repeated at the end is the same outline.
        let again = outline(
            &[sq[0], sq[1], sq[2], sq[3], sq[0]],
            2.0,
            true,
            Join::Miter,
            Cap::Round,
        );
        assert_eq!(again, o);
    }

    #[test]
    fn triangles_cover_the_quad_and_the_disc() {
        let o = outline(
            &[p(0., 0.), p(10., 0.)],
            4.0,
            false,
            Join::Round,
            Cap::Round,
        );
        let t = o.triangles(12);
        assert_eq!(t.len(), 2 + 2 * 12);
        let area: f32 = t.iter().take(2).map(|t| area2(t).abs() / 2.0).sum();
        assert!((area - 40.0).abs() < 1e-3, "the quad is 10 x 4");
        let disc: f32 = t
            .iter()
            .skip(2)
            .take(12)
            .map(|t| area2(t).abs() / 2.0)
            .sum();
        let ideal = std::f32::consts::PI * 4.0;
        assert!(disc < ideal && disc > ideal * 0.95, "{disc} vs {ideal}");
    }

    #[test]
    fn fans_triangulate_convex_and_star_shaped_polygons() {
        let sq = [p(0., 0.), p(10., 0.), p(10., 10.), p(0., 10.)];
        let mut t = Vec::new();
        fan(&sq, &mut t);
        assert_eq!(t.len(), 2);
        assert!((t.iter().map(|t| area2(t).abs() / 2.0).sum::<f32>() - 100.0).abs() < 1e-3);
        let mut u = Vec::new();
        fan_from_centroid(&sq, &mut u);
        assert_eq!(u.len(), 4);
        assert!((u.iter().map(|t| area2(t).abs() / 2.0).sum::<f32>() - 100.0).abs() < 1e-3);
        let mut none = Vec::new();
        fan_from_centroid(&sq[..2], &mut none);
        assert!(none.is_empty());
    }
}
