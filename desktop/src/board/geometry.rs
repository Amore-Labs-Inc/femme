//! From shapes to drawing primitives: what the screen painter and the PNG export both draw, so the
//! two always agree. A [`Prim`] is a filled polygon, a stroked polyline or a text, in scene units.
//!
//! The *sketchy* look (the Excalidraw hand-drawn feel) is a displacement of the points by a smooth
//! noise that depends on nothing but the shape's id and the position along the outline: no random
//! number generator, no frame counter. So a shape never wobbles between frames, keeps its wobble
//! when it is moved, and looks the same at every zoom (the zoom only changes how many samples are
//! taken of the same curve).

use std::f32::consts::{PI, TAU};

use super::geom::{Point, Rect};
use super::scene::{arrow_head, arrow_head_len, Kind, Shape, Tone};

/// How the corners of a stroked polyline are joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Join {
    /// Rounded: freehand, lines, ellipses.
    Round,
    /// Sharp (up to a limit): the corners of a rectangle.
    Miter,
}

/// One thing to draw, in scene units.
#[derive(Clone, Debug, PartialEq)]
pub enum Prim {
    /// A filled polygon (convex, or star-shaped around its centre, which is all a rectangle, an
    /// ellipse and an arrow head ever are).
    Fill {
        /// The corners, in order.
        points: Vec<Point>,
        /// The colour.
        tone: Tone,
    },
    /// A stroked polyline with round caps.
    Stroke {
        /// The vertices, in order. One vertex is a dot.
        points: Vec<Point>,
        /// The last vertex joins back to the first.
        closed: bool,
        /// Stroke width.
        width: f32,
        /// The colour.
        tone: Tone,
        /// The corner style.
        join: Join,
    },
    /// A text block, lines split by `\n`, drawn in the body font.
    Text {
        /// Top-left corner of the block.
        at: Point,
        /// The text.
        text: String,
        /// Font size.
        size: f32,
        /// The colour.
        tone: Tone,
    },
}

impl Prim {
    /// The same primitive in another space: every point mapped through `map`, every length (stroke
    /// width, text size) multiplied by `k`. The view maps scene units to the screen, the export maps
    /// them to its pixmap.
    pub fn mapped(&self, map: impl Fn(Point) -> Point, k: f32) -> Prim {
        match self {
            Prim::Fill { points, tone } => Prim::Fill {
                points: points.iter().map(|p| map(*p)).collect(),
                tone: *tone,
            },
            Prim::Stroke {
                points,
                closed,
                width,
                tone,
                join,
            } => Prim::Stroke {
                points: points.iter().map(|p| map(*p)).collect(),
                closed: *closed,
                width: width * k,
                tone: *tone,
                join: *join,
            },
            Prim::Text {
                at,
                text,
                size,
                tone,
            } => Prim::Text {
                at: map(*at),
                text: text.clone(),
                size: size * k,
                tone: *tone,
            },
        }
    }
}

// ---- the deterministic noise --------------------------------------------------------------------

/// A 32-bit hash of two numbers (a multiply-xorshift mix): no state, so the same inputs give the same
/// output on every run and every machine. The jitter is made of nothing else.
pub fn hash2(a: u32, b: u32) -> u32 {
    let mut h = a.wrapping_mul(0x9E37_79B1) ^ b.wrapping_add(0x7F4A_7C15).wrapping_mul(0x85EB_CA6B);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^= h >> 15;
    h
}

/// A hash as a number in `-1.0..=1.0`.
fn signed(h: u32) -> f32 {
    h as f32 / u32::MAX as f32 * 2.0 - 1.0
}

/// A hash as a number in `0.0..=1.0`.
fn unit(h: u32) -> f32 {
    h as f32 / u32::MAX as f32
}

/// The smoothstep blend of two lattice values.
fn blend(a: f32, b: f32, f: f32) -> f32 {
    a + (b - a) * (f * f * (3.0 - 2.0 * f))
}

/// Smooth one-dimensional value noise in `-1.0..=1.0`: a hashed value at every integer, blended
/// between them.
pub fn noise(seed: u32, x: f32) -> f32 {
    let i = x.floor();
    let at = |k: f32| signed(hash2(seed, k as i32 as u32));
    blend(at(i), at(i + 1.0), x - i)
}

/// [`noise`] that repeats every `period` (an integer, at least 2): the value at `period` is the
/// value at 0, so a closed outline has no seam.
pub fn noise_ring(seed: u32, x: f32, period: u32) -> f32 {
    let i = x.floor();
    let at = |k: i64| signed(hash2(seed, k.rem_euclid(period as i64) as u32));
    blend(at(i as i64), at(i as i64 + 1), x - i)
}

/// Distance along an outline between two lattice values of the noise, in scene units: the wobble
/// has about this wavelength.
const WAVE: f32 = 36.0;

/// How many device pixels one sample of a line covers at most.
const STEP_PX: f32 = 6.0;

/// A seed for the `k`-th thing drawn for a shape.
fn seed_of(shape: &Shape, k: u32) -> u32 {
    hash2(shape.id, k.wrapping_add(0x51ED))
}

// ---- line pieces ----------------------------------------------------------------------------------

/// A line from `a` to `b` drawn by hand: bowed a little, wobbling along its length, running
/// `overshoot` scene units past either end at most. `scale` is device pixels per scene unit; it
/// picks how many samples are taken, not what the curve is.
pub fn wobble(a: Point, b: Point, seed: u32, scale: f32, overshoot: f32) -> Vec<Point> {
    let d = b - a;
    let len = d.len();
    let Some(dir) = d.unit() else {
        return vec![a, b];
    };
    let nrm = dir.normal();
    let amp = (len * 0.025).clamp(0.4, 1.7);
    let bow = signed(hash2(seed, 7)) * (len * 0.01).min(2.5);
    let before = overshoot * (0.4 + 0.6 * unit(hash2(seed, 8)));
    let after = overshoot * (0.4 + 0.6 * unit(hash2(seed, 9)));
    let total = len + before + after;
    let n = ((total * scale / STEP_PX).ceil() as usize).clamp(2, 400);
    (0..=n)
        .map(|i| {
            let s = total * i as f32 / n as f32;
            let t = s / total;
            let off = bow * 4.0 * t * (1.0 - t) + amp * noise(seed, s / WAVE);
            a + dir * (s - before) + nrm * off
        })
        .collect()
}

/// The perimeter of an ellipse with these radii (Ramanujan's approximation).
pub fn ellipse_perimeter(rx: f32, ry: f32) -> f32 {
    PI * (3.0 * (rx + ry) - ((3.0 * rx + ry) * (rx + 3.0 * ry)).sqrt())
}

/// An ellipse to sample: its centre and radii.
struct Oval {
    c: Point,
    rx: f32,
    ry: f32,
}

/// How far round an ellipse to go: from angle `start`, `turns` revolutions, the radius wobbling
/// by up to `amp` scene units (none when `amp` is zero).
struct Sweep {
    start: f32,
    turns: f32,
    amp: f32,
}

/// Points around an ellipse (see [`Sweep`]).
fn ring(o: &Oval, seed: u32, scale: f32, sweep: Sweep) -> Vec<Point> {
    let Oval { c, rx, ry } = *o;
    let per = ellipse_perimeter(rx, ry);
    let n = ((per * sweep.turns * scale / (STEP_PX * 0.8)).ceil() as usize).clamp(24, 600);
    let k = ((per / (WAVE * 0.9)).round() as u32).clamp(4, 64);
    (0..=n)
        .map(|i| {
            let u = sweep.turns * i as f32 / n as f32;
            let th = sweep.start + TAU * u;
            let wob = if sweep.amp > 0.0 {
                sweep.amp * noise_ring(seed, u.rem_euclid(1.0) * k as f32, k)
            } else {
                0.0
            };
            Point::new(c.x + (rx + wob) * th.cos(), c.y + (ry + wob) * th.sin())
        })
        .collect()
}

/// A scribble with its corners cut (one round of Chaikin's scheme), so the straight chords between
/// the pointer's samples read as a curve. The ends stay where they are.
pub fn smooth(points: &[Point]) -> Vec<Point> {
    if points.len() < 3 {
        return points.to_vec();
    }
    let mut out = Vec::with_capacity(points.len() * 2);
    out.push(points[0]);
    for w in points.windows(2) {
        out.push(w[0].lerp(w[1], 0.25));
        out.push(w[0].lerp(w[1], 0.75));
    }
    out.push(points[points.len() - 1]);
    out
}

// ---- shapes ---------------------------------------------------------------------------------------

/// The primitives of one shape, bottom to top. `sketchy` is the scene's flag; `scale` is device
/// pixels per scene unit (the zoom on the screen, the export scale in a PNG), which only sets how
/// finely curves are sampled.
pub fn prims(shape: &Shape, sketchy: bool, scale: f32) -> Vec<Prim> {
    let width = shape.style.width.px();
    let tone = Tone::from(shape.style.color);
    let fill = shape.style.fill_tone();
    let scale = scale.max(0.05);
    let stroke = |points: Vec<Point>, closed: bool, join: Join| Prim::Stroke {
        points,
        closed,
        width,
        tone,
        join,
    };
    let mut out = Vec::new();
    match &shape.kind {
        Kind::Rect { a, b } => {
            let r = Rect::new(*a, *b);
            let corners = [
                r.min,
                Point::new(r.max.x, r.min.y),
                r.max,
                Point::new(r.min.x, r.max.y),
            ];
            if !sketchy || r.width().min(r.height()) < 4.0 {
                if let Some(t) = fill {
                    out.push(Prim::Fill {
                        points: corners.to_vec(),
                        tone: t,
                    });
                }
                out.push(stroke(corners.to_vec(), true, Join::Miter));
            } else {
                // Each corner is off by a hair, and each side is drawn twice, as a pencil would.
                let j = (r.width().min(r.height()) * 0.03).clamp(0.2, 1.4);
                let jittered: Vec<Point> = corners
                    .iter()
                    .enumerate()
                    .map(|(k, c)| {
                        let s = seed_of(shape, 100 + k as u32);
                        Point::new(c.x + signed(hash2(s, 1)) * j, c.y + signed(hash2(s, 2)) * j)
                    })
                    .collect();
                if let Some(t) = fill {
                    out.push(Prim::Fill {
                        points: jittered.clone(),
                        tone: t,
                    });
                }
                for pass in 0..2u32 {
                    let from = if pass == 0 {
                        &jittered[..]
                    } else {
                        &corners[..]
                    };
                    for side in 0..4usize {
                        let seed = seed_of(shape, pass * 10 + side as u32);
                        out.push(stroke(
                            wobble(from[side], from[(side + 1) % 4], seed, scale, 2.5),
                            false,
                            Join::Round,
                        ));
                    }
                }
            }
        }
        Kind::Ellipse { a, b } => {
            let r = Rect::new(*a, *b);
            let oval = Oval {
                c: r.center(),
                rx: r.width() * 0.5,
                ry: r.height() * 0.5,
            };
            let (c, rx, ry) = (oval.c, oval.rx, oval.ry);
            if rx < 0.5 && ry < 0.5 {
                out.push(stroke(vec![c, c], false, Join::Round));
            } else if !sketchy || rx.min(ry) < 3.0 {
                let pts = ring(
                    &oval,
                    0,
                    scale,
                    Sweep {
                        start: 0.0,
                        turns: 1.0,
                        amp: 0.0,
                    },
                );
                if let Some(t) = fill {
                    out.push(Prim::Fill {
                        points: pts.clone(),
                        tone: t,
                    });
                }
                out.push(stroke(pts, true, Join::Round));
            } else {
                let amp = (rx.min(ry) * 0.04).clamp(0.4, 1.7);
                let start = unit(hash2(shape.id, 77)) * TAU;
                let first = ring(
                    &oval,
                    seed_of(shape, 1),
                    scale,
                    Sweep {
                        start,
                        turns: 1.0,
                        amp,
                    },
                );
                if let Some(t) = fill {
                    out.push(Prim::Fill {
                        points: first.clone(),
                        tone: t,
                    });
                }
                // Two loops from different starting angles, each running a little past its start.
                out.push(stroke(
                    ring(
                        &oval,
                        seed_of(shape, 1),
                        scale,
                        Sweep {
                            start,
                            turns: 1.04,
                            amp,
                        },
                    ),
                    false,
                    Join::Round,
                ));
                let start2 = start + TAU * (0.2 + 0.5 * unit(hash2(shape.id, 78)));
                out.push(stroke(
                    ring(
                        &oval,
                        seed_of(shape, 2),
                        scale,
                        Sweep {
                            start: start2,
                            turns: 1.05,
                            amp,
                        },
                    ),
                    false,
                    Join::Round,
                ));
            }
        }
        Kind::Line { a, b } => {
            if sketchy {
                for pass in 0..2u32 {
                    out.push(stroke(
                        wobble(*a, *b, seed_of(shape, pass), scale, 0.0),
                        false,
                        Join::Round,
                    ));
                }
            } else {
                out.push(stroke(vec![*a, *b], false, Join::Round));
            }
        }
        Kind::Arrow { a, b } => {
            // The shaft stops inside the head, so its round cap never pokes through the tip.
            let head = arrow_head(*a, *b, width);
            let end = match (head, (*b - *a).unit()) {
                (Some(_), Some(dir)) => *b - dir * (arrow_head_len(width, a.dist(*b)) * 0.6),
                _ => *b,
            };
            if sketchy {
                for pass in 0..2u32 {
                    out.push(stroke(
                        wobble(*a, end, seed_of(shape, pass), scale, 0.0),
                        false,
                        Join::Round,
                    ));
                }
            } else {
                out.push(stroke(vec![*a, end], false, Join::Round));
            }
            if let Some(h) = head {
                out.push(Prim::Fill {
                    points: h.to_vec(),
                    tone,
                });
            }
        }
        Kind::Freehand { points } => match points.as_slice() {
            [] => {}
            [one] => out.push(stroke(vec![*one, *one], false, Join::Round)),
            many => out.push(stroke(smooth(many), false, Join::Round)),
        },
        Kind::Text { at, text } => out.push(Prim::Text {
            at: *at,
            text: text.clone(),
            size: shape.text_size(),
            tone,
        }),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::scene::{Fill, Style, Width};

    fn p(x: f32, y: f32) -> Point {
        Point::new(x, y)
    }

    fn shape(id: u32, kind: Kind, style: Style) -> Shape {
        Shape { id, kind, style }
    }

    fn rect_shape(id: u32) -> Shape {
        shape(
            id,
            Kind::Rect {
                a: p(10., 10.),
                b: p(210., 110.),
            },
            Style::default(),
        )
    }

    #[test]
    fn the_hash_is_deterministic_and_spreads() {
        assert_eq!(hash2(1, 2), hash2(1, 2));
        assert_ne!(hash2(1, 2), hash2(2, 1));
        assert_ne!(hash2(1, 2), hash2(1, 3));
        // Fixed values pin the function: a change would move every saved drawing's wobble.
        assert_eq!(hash2(0, 0), 3_006_001_144);
        assert_eq!(hash2(1, 2), 4_235_845_158);
        assert_eq!(hash2(7, 99), 3_514_142_647);
        let mean: f32 = (0..2000).map(|i| unit(hash2(5, i))).sum::<f32>() / 2000.0;
        assert!((mean - 0.5).abs() < 0.05, "mean {mean}");
    }

    #[test]
    fn noise_is_smooth_bounded_and_ring_noise_closes() {
        for i in 0..2000 {
            let x = i as f32 * 0.037;
            let v = noise(3, x);
            assert!((-1.0..=1.0).contains(&v));
            assert!((noise(3, x + 0.01) - v).abs() < 0.1, "a jump at {x}");
        }
        for seed in 0..20 {
            assert!((noise_ring(seed, 0.0, 7) - noise_ring(seed, 7.0, 7)).abs() < 1e-5);
            assert!((noise_ring(seed, 3.5, 7) - noise_ring(seed, 10.5, 7)).abs() < 1e-5);
        }
    }

    #[test]
    fn the_same_shape_gives_the_same_primitives_every_time() {
        let s = rect_shape(7);
        assert_eq!(prims(&s, true, 1.0), prims(&s, true, 1.0));
        let e = shape(
            9,
            Kind::Ellipse {
                a: p(0., 0.),
                b: p(120., 80.),
            },
            Style::default(),
        );
        assert_eq!(prims(&e, true, 2.0), prims(&e, true, 2.0));
    }

    #[test]
    fn a_shape_keeps_its_wobble_when_it_moves_and_its_neighbour_wobbles_otherwise() {
        let a = prims(&rect_shape(7), true, 1.0);
        let mut moved = rect_shape(7);
        moved.translate(p(100., 50.));
        let b = prims(&moved, true, 1.0);
        let shifted: Vec<Prim> = a
            .iter()
            .map(|q| q.mapped(|v| v + p(100., 50.), 1.0))
            .collect();
        assert_eq!(b.len(), shifted.len());
        for (x, y) in b.iter().zip(&shifted) {
            let (Prim::Stroke { points: px, .. }, Prim::Stroke { points: py, .. }) = (x, y) else {
                continue;
            };
            assert_eq!(px.len(), py.len());
            for (u, v) in px.iter().zip(py) {
                assert!(u.dist(*v) < 1e-3, "{u:?} vs {v:?}");
            }
        }
        // Another id wobbles differently.
        assert_ne!(prims(&rect_shape(8), true, 1.0), a);
    }

    #[test]
    fn the_zoom_changes_the_sampling_but_not_the_curve() {
        let s = shape(
            3,
            Kind::Line {
                a: p(0., 0.),
                b: p(300., 40.),
            },
            Style::default(),
        );
        let coarse = prims(&s, true, 0.5);
        let fine = prims(&s, true, 4.0);
        let (Prim::Stroke { points: c, .. }, Prim::Stroke { points: f, .. }) =
            (&coarse[0], &fine[0])
        else {
            panic!("a line is a stroke")
        };
        assert!(f.len() > c.len() * 2, "{} vs {}", f.len(), c.len());
        // Every coarse sample lies on the fine curve (within what linear interpolation loses).
        for q in c {
            let near = f
                .windows(2)
                .map(|w| crate::board::geom::dist_to_segment(*q, w[0], w[1]))
                .fold(f32::MAX, f32::min);
            assert!(near < 0.5, "{q:?} is {near} off the fine curve");
        }
    }

    #[test]
    fn sketchy_off_is_ruler_straight() {
        let r = prims(&rect_shape(1), false, 1.0);
        assert_eq!(r.len(), 1);
        let Prim::Stroke {
            points,
            closed,
            join,
            ..
        } = &r[0]
        else {
            panic!()
        };
        assert_eq!(
            points,
            &vec![p(10., 10.), p(210., 10.), p(210., 110.), p(10., 110.)]
        );
        assert!(*closed && *join == Join::Miter);
        let l = prims(
            &shape(
                2,
                Kind::Line {
                    a: p(0., 0.),
                    b: p(5., 5.),
                },
                Style::default(),
            ),
            false,
            1.0,
        );
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn sketchy_wobble_stays_within_a_couple_of_units() {
        let s = shape(
            4,
            Kind::Line {
                a: p(0., 0.),
                b: p(400., 0.),
            },
            Style::default(),
        );
        for q in prims(&s, true, 1.0) {
            let Prim::Stroke { points, .. } = q else {
                panic!()
            };
            let worst = points.iter().map(|v| v.y.abs()).fold(0.0, f32::max);
            assert!(worst > 0.05, "a sketchy line is not straight");
            assert!(worst < 5.0, "but it does not stray: {worst}");
            assert!(
                points.first().unwrap().x.abs() < 1e-3
                    && (points.last().unwrap().x - 400.0).abs() < 1e-3
            );
        }
    }

    #[test]
    fn a_filled_rect_has_its_fill_below_its_outline() {
        let mut s = rect_shape(1);
        s.style.fill = Fill::Soft;
        for sketchy in [false, true] {
            let ps = prims(&s, sketchy, 1.0);
            assert!(matches!(
                ps[0],
                Prim::Fill {
                    tone: Tone::Soft,
                    ..
                }
            ));
            assert!(ps[1..].iter().all(|p| matches!(p, Prim::Stroke { .. })));
        }
        s.style.fill = Fill::None;
        assert!(prims(&s, false, 1.0)
            .iter()
            .all(|p| !matches!(p, Prim::Fill { .. })));
    }

    #[test]
    fn an_ellipse_is_round_and_closed_when_ruler_straight() {
        let s = shape(
            1,
            Kind::Ellipse {
                a: p(0., 0.),
                b: p(100., 60.),
            },
            Style::default(),
        );
        let ps = prims(&s, false, 1.0);
        let Prim::Stroke { points, closed, .. } = &ps[0] else {
            panic!()
        };
        assert!(*closed);
        for q in points {
            let v = ((q.x - 50.0) / 50.0).powi(2) + ((q.y - 30.0) / 30.0).powi(2);
            assert!((v - 1.0).abs() < 1e-3, "{q:?} is off the ellipse");
        }
        assert!(points.len() >= 24);
    }

    #[test]
    fn an_arrow_has_a_shaft_and_a_filled_head_at_its_end() {
        let s = shape(
            1,
            Kind::Arrow {
                a: p(0., 0.),
                b: p(100., 0.),
            },
            Style::default(),
        );
        let ps = prims(&s, false, 1.0);
        assert_eq!(ps.len(), 2);
        let Prim::Stroke { points, .. } = &ps[0] else {
            panic!()
        };
        assert!(
            points[1].x < 100.0 && points[1].x > 80.0,
            "the shaft stops inside the head"
        );
        let Prim::Fill { points: head, .. } = &ps[1] else {
            panic!()
        };
        assert_eq!(head.len(), 3);
        assert_eq!(head[0], p(100., 0.));
        // A zero-length arrow has no head.
        let z = shape(
            2,
            Kind::Arrow {
                a: p(5., 5.),
                b: p(5., 5.),
            },
            Style::default(),
        );
        assert_eq!(prims(&z, false, 1.0).len(), 1);
    }

    #[test]
    fn freehand_and_text_become_a_stroke_and_a_text() {
        let f = shape(
            1,
            Kind::Freehand {
                points: vec![p(0., 0.), p(10., 5.), p(20., 0.), p(30., 5.)],
            },
            Style::default(),
        );
        let ps = prims(&f, true, 1.0);
        let Prim::Stroke { points, .. } = &ps[0] else {
            panic!()
        };
        assert_eq!(points.first(), Some(&p(0., 0.)));
        assert_eq!(points.last(), Some(&p(30., 5.)));
        assert!(points.len() > 4, "smoothed");
        // A click is a dot.
        let dot = shape(
            2,
            Kind::Freehand {
                points: vec![p(3., 3.)],
            },
            Style::default(),
        );
        let Prim::Stroke { points, .. } = &prims(&dot, false, 1.0)[0] else {
            panic!()
        };
        assert_eq!(points, &vec![p(3., 3.), p(3., 3.)]);
        let t = shape(
            3,
            Kind::Text {
                at: p(1., 2.),
                text: "hi".into(),
            },
            Style {
                width: Width::Thick,
                ..Style::default()
            },
        );
        assert_eq!(
            prims(&t, true, 1.0),
            vec![Prim::Text {
                at: p(1., 2.),
                text: "hi".into(),
                size: 32.0,
                tone: Tone::Ink
            }]
        );
    }

    #[test]
    fn mapping_scales_lengths_with_the_points() {
        let q = Prim::Stroke {
            points: vec![p(1., 1.), p(2., 2.)],
            closed: false,
            width: 2.0,
            tone: Tone::Ink,
            join: Join::Round,
        };
        let m = q.mapped(|v| v * 3.0 + p(10., 0.), 3.0);
        assert_eq!(
            m,
            Prim::Stroke {
                points: vec![p(13., 3.), p(16., 6.)],
                closed: false,
                width: 6.0,
                tone: Tone::Ink,
                join: Join::Round
            }
        );
    }

    #[test]
    fn smoothing_keeps_the_ends() {
        let pts = vec![p(0., 0.), p(10., 10.), p(20., 0.)];
        let s = smooth(&pts);
        assert_eq!(s.first(), pts.first());
        assert_eq!(s.last(), pts.last());
        assert_eq!(s.len(), 6);
        assert_eq!(smooth(&pts[..2]), pts[..2].to_vec());
    }
}
