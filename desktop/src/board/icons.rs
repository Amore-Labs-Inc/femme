//! The tool icons: a few polylines each, in a 20 by 20 box, drawn by the painter with the board's
//! own stroker. They are strokes like the ones on the canvas, so any colour and size is free and
//! nothing depends on an icon font.

use std::f32::consts::TAU;

use super::editor::Tool;
use super::geom::Point;

/// The side of the box an icon is drawn in.
pub const BOX: f32 = 20.0;

/// The width of an icon's strokes, in box units.
pub const STROKE: f32 = 1.6;

/// One polyline of an icon.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    /// The corners, in box units.
    pub points: Vec<Point>,
    /// The last corner joins the first.
    pub closed: bool,
    /// Filled with the colour (a convex shape) instead of stroked.
    pub filled: bool,
}

fn points(v: &[(f32, f32)]) -> Vec<Point> {
    v.iter().map(|&(x, y)| Point::new(x, y)).collect()
}

fn open(v: &[(f32, f32)]) -> Line {
    Line {
        points: points(v),
        closed: false,
        filled: false,
    }
}

fn closed(v: &[(f32, f32)]) -> Line {
    Line {
        points: points(v),
        closed: true,
        filled: false,
    }
}

/// The icon of `tool`.
pub fn icon(tool: Tool) -> Vec<Line> {
    match tool {
        // The pointer: tip at the top left, a tail to the bottom right.
        Tool::Select => vec![closed(&[
            (5.0, 3.0),
            (5.0, 15.5),
            (8.6, 12.2),
            (11.4, 17.2),
            (13.4, 16.2),
            (10.6, 11.3),
            (15.2, 10.8),
        ])],
        Tool::Rect => vec![closed(&[
            (3.5, 5.0),
            (16.5, 5.0),
            (16.5, 15.0),
            (3.5, 15.0),
        ])],
        Tool::Ellipse => {
            let n = 28;
            let ring = (0..n)
                .map(|i| {
                    let a = TAU * i as f32 / n as f32;
                    Point::new(10.0 + 6.8 * a.cos(), 10.0 + 5.2 * a.sin())
                })
                .collect();
            vec![Line {
                points: ring,
                closed: true,
                filled: false,
            }]
        }
        Tool::Arrow => arrow(),
        Tool::Line => vec![open(&[(4.0, 16.0), (16.0, 4.0)])],
        Tool::Freehand => {
            let n = 24;
            let squiggle = (0..=n)
                .map(|i| {
                    let t = i as f32 / n as f32;
                    Point::new(3.0 + 14.0 * t, 10.0 - 3.6 * (t * 1.5 * TAU).sin())
                })
                .collect();
            vec![Line {
                points: squiggle,
                closed: false,
                filled: false,
            }]
        }
        // A T: the bar, the stem and a short foot.
        Tool::Text => vec![
            open(&[(4.5, 5.0), (15.5, 5.0)]),
            open(&[(10.0, 5.0), (10.0, 16.0)]),
            open(&[(7.8, 16.0), (12.2, 16.0)]),
        ],
    }
}

/// A shaft running to a solid head at the top right.
fn arrow() -> Vec<Line> {
    let tail = Point::new(3.5, 16.5);
    let tip = Point::new(16.5, 3.5);
    let along = (tip - tail).unit().unwrap_or(Point::new(1.0, 0.0));
    let across = along.normal();
    let back = tip - along * 5.5;
    vec![
        Line {
            points: vec![tail, tip - along * 4.0],
            closed: false,
            filled: false,
        },
        Line {
            points: vec![tip, back + across * 2.8, back - across * 2.8],
            closed: true,
            filled: true,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::stroke::area2;

    #[test]
    fn every_tool_has_an_icon_of_its_own() {
        let all: Vec<_> = Tool::ALL.iter().map(|t| icon(*t)).collect();
        for (tool, lines) in Tool::ALL.iter().zip(&all) {
            assert!(!lines.is_empty(), "{tool:?}");
        }
        for i in 0..all.len() {
            for j in i + 1..all.len() {
                assert_ne!(all[i], all[j], "{:?} and {:?}", Tool::ALL[i], Tool::ALL[j]);
            }
        }
    }

    #[test]
    fn the_strokes_stay_inside_the_box_with_room_for_their_width() {
        let room = STROKE / 2.0;
        for tool in Tool::ALL {
            for line in icon(tool) {
                for p in &line.points {
                    assert!(p.is_finite(), "{tool:?}");
                    for v in [p.x, p.y] {
                        assert!(
                            v >= room && v <= BOX - room,
                            "{tool:?}: {p:?} leaves the {BOX} box"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_line_has_the_corners_it_needs() {
        for tool in Tool::ALL {
            for line in icon(tool) {
                if line.filled {
                    assert!(line.closed && line.points.len() == 3, "{tool:?}");
                    let t = [line.points[0], line.points[1], line.points[2]];
                    assert!(area2(&t).abs() > 1.0, "{tool:?}: a flat head");
                } else {
                    assert!(line.points.len() >= 2, "{tool:?}");
                }
            }
        }
    }

    #[test]
    fn the_arrow_head_points_at_the_tip_and_the_shaft_ends_inside_it() {
        let lines = icon(Tool::Arrow);
        let (shaft, head) = (&lines[0], &lines[1]);
        let tip = head.points[0];
        assert_eq!(tip, Point::new(16.5, 3.5));
        // The shaft stops short of the tip, so its round end cannot poke out of the point.
        let end = *shaft.points.last().unwrap();
        assert!(end.dist(tip) > STROKE, "{end:?}");
        assert!(end.dist(tip) < 5.5, "{end:?} is outside the head");
    }
}
