//! The pointer side of the editor: presses, drags (pan, draw, move, marquee) and releases.

use super::{Button, Editor, Mods, Tool, DRAG_SLOP, HIT_SLACK};
use crate::board::geom::{Point, Rect};
use crate::board::scene::Kind;

/// A drag that is going on.
#[derive(Clone, Debug)]
pub(super) enum Drag {
    /// The view follows the pointer.
    Pan { from: Point, pan0: Point },
    /// A shape is being drawn.
    Draw {
        tool: Tool,
        /// Where it started, scene units.
        start: Point,
        /// Where the button went down, canvas pixels.
        down: Point,
        /// The shape, once the pointer has moved far enough to make one.
        shape: Option<u32>,
        /// The scribble so far.
        points: Vec<Point>,
    },
    /// The selection follows the pointer.
    Move {
        down: Point,
        last: Point,
        /// The pointer has moved far enough for this to be a drag; the transaction is open.
        live: bool,
        /// A press on a shape of a bigger selection that does not turn into a drag selects only
        /// that shape.
        collapse: Option<u32>,
    },
    /// A box is being dragged out; what it holds is selected.
    Marquee {
        start: Point,
        now: Point,
        /// What was selected before, kept by shift.
        base: Vec<u32>,
    },
}

impl Editor {
    /// A button went down at `pos` (canvas pixels); `clicks` counts the presses in a row.
    pub fn pointer_down(&mut self, pos: Point, mods: Mods, clicks: u32, button: Button) {
        self.end_nudge();
        self.pointer = pos;
        if self.drag.is_some() {
            return;
        }
        if button == Button::Middle || self.space {
            self.drag = Some(Drag::Pan {
                from: pos,
                pan0: self.view.pan,
            });
            return;
        }
        let p = self.view.to_scene(pos);
        if self.typing.is_some() {
            if self.typing_box().is_some_and(|b| b.contains(p)) {
                return;
            }
            self.commit_text();
        }
        let tol = HIT_SLACK / self.view.zoom;
        match self.tool {
            Tool::Select => self.press_select(pos, p, mods, clicks, tol),
            Tool::Text => match self.text_at(p, tol) {
                Some(id) => self.edit_text(id),
                None => self.start_text(p),
            },
            tool => {
                self.selection.clear();
                self.drag = Some(Drag::Draw {
                    tool,
                    start: p,
                    down: pos,
                    shape: None,
                    points: vec![p],
                });
            }
        }
    }

    /// The text shape under `p`, if the topmost shape there is one.
    fn text_at(&self, p: Point, tol: f32) -> Option<u32> {
        let id = self.scene.hit_test(p, tol)?;
        matches!(self.scene.get(id)?.kind, Kind::Text { .. }).then_some(id)
    }

    /// A press with the select tool: a double click makes or edits a text, a click picks a shape
    /// (and may start moving it), a press on nothing starts a marquee.
    fn press_select(&mut self, pos: Point, p: Point, mods: Mods, clicks: u32, tol: f32) {
        let hit = self.scene.hit_test(p, tol);
        if clicks >= 2 {
            if hit.is_none() {
                self.start_text(p);
                return;
            }
            if let Some(id) = self.text_at(p, tol) {
                self.edit_text(id);
                return;
            }
        }
        let Some(id) = hit else {
            let base = if mods.shift {
                self.selection.clone()
            } else {
                self.selection.clear();
                Vec::new()
            };
            self.drag = Some(Drag::Marquee {
                start: p,
                now: p,
                base,
            });
            return;
        };
        let mut collapse = None;
        if mods.shift {
            if let Some(i) = self.selection.iter().position(|s| *s == id) {
                self.selection.remove(i);
                return;
            }
            let mut ids = self.selection.clone();
            ids.push(id);
            self.select(ids);
        } else if self.selection.contains(&id) {
            collapse = (self.selection.len() > 1).then_some(id);
        } else {
            self.select(vec![id]);
        }
        self.drag = Some(Drag::Move {
            down: pos,
            last: p,
            live: false,
            collapse,
        });
    }

    /// The pointer moved to `pos` (the button may be down).
    pub fn pointer_move(&mut self, pos: Point, mods: Mods) {
        self.pointer = pos;
        if let Some(drag) = self.drag.take() {
            self.drag = Some(self.drag_step(drag, pos, mods));
        }
    }

    /// One step of a drag: applies the pointer's new position and hands the drag back.
    fn drag_step(&mut self, drag: Drag, pos: Point, mods: Mods) -> Drag {
        let p = self.view.to_scene(pos);
        match drag {
            Drag::Pan { from, pan0 } => {
                self.view.pan = pan0 + (pos - from);
                Drag::Pan { from, pan0 }
            }
            Drag::Move {
                down,
                mut last,
                mut live,
                collapse,
            } => {
                if !live && pos.dist(down) < DRAG_SLOP {
                    return Drag::Move {
                        down,
                        last,
                        live,
                        collapse,
                    };
                }
                if !live {
                    live = true;
                    self.scene.begin();
                }
                let d = p - last;
                if d != Point::ZERO {
                    self.scene.move_by(&self.selection, d);
                    last = p;
                    self.changed();
                }
                Drag::Move {
                    down,
                    last,
                    live,
                    collapse: None,
                }
            }
            Drag::Marquee { start, base, .. } => {
                let inside = self.scene.shapes_in(&Rect::new(start, p));
                self.selection = self
                    .scene
                    .ids()
                    .into_iter()
                    .filter(|id| base.contains(id) || inside.contains(id))
                    .collect();
                Drag::Marquee {
                    start,
                    now: p,
                    base,
                }
            }
            Drag::Draw {
                tool,
                start,
                down,
                mut shape,
                mut points,
            } => {
                if shape.is_none() && pos.dist(down) >= DRAG_SLOP {
                    self.scene.begin();
                    let kind = draw_kind(tool, start, start, &points);
                    shape = Some(self.scene.add(kind, self.style));
                }
                if let Some(id) = shape {
                    if tool == Tool::Freehand {
                        let spacing = 1.5 / self.view.zoom;
                        if points.last().is_none_or(|l| l.dist(p) >= spacing) {
                            points.push(p);
                        }
                    }
                    let end = constrain(tool, start, p, mods.shift);
                    self.scene
                        .set_kind(id, draw_kind(tool, start, end, &points));
                    self.changed();
                }
                Drag::Draw {
                    tool,
                    start,
                    down,
                    shape,
                    points,
                }
            }
        }
    }

    /// The button went up at `pos`: the drag ends.
    pub fn pointer_up(&mut self, pos: Point, mods: Mods) {
        if self.drag.is_none() {
            return;
        }
        self.pointer_move(pos, mods);
        match self.drag.take() {
            Some(Drag::Draw {
                tool, start, shape, ..
            }) => match shape {
                Some(id) => {
                    self.scene.commit();
                    if tool != Tool::Freehand {
                        self.select(vec![id]);
                        self.tool = Tool::Select;
                    }
                }
                None if tool == Tool::Freehand => {
                    // A click with the pen is a dot.
                    self.scene.add(
                        Kind::Freehand {
                            points: vec![start],
                        },
                        self.style,
                    );
                    self.changed();
                }
                None => {}
            },
            Some(Drag::Move { live, collapse, .. }) => {
                if live {
                    self.scene.commit();
                } else if let Some(id) = collapse {
                    self.select(vec![id]);
                }
            }
            Some(Drag::Pan { .. } | Drag::Marquee { .. }) | None => {}
        }
    }

    /// Esc on a drag: takes back what it did. Says whether there was a drag.
    pub(super) fn cancel_drag(&mut self) -> bool {
        match self.drag.take() {
            Some(Drag::Pan { pan0, .. }) => self.view.pan = pan0,
            Some(Drag::Draw { shape, .. }) => {
                if shape.is_some() {
                    self.scene.cancel();
                    self.changed();
                }
            }
            Some(Drag::Move { live, .. }) => {
                if live {
                    self.scene.cancel();
                    self.prune_selection();
                    self.changed();
                }
            }
            Some(Drag::Marquee { base, .. }) => self.selection = base,
            None => return false,
        }
        true
    }

    /// The board is closing or losing the keyboard: typed text is kept, a drag ends where the pointer
    /// is, held keys let go.
    pub fn blur(&mut self) {
        self.commit_text();
        self.end_nudge();
        self.space = false;
        if self.drag.is_some() {
            self.pointer_up(self.pointer, Mods::default());
        }
    }
}

/// The far corner of a shape being dragged out from `start` to the pointer at `p`: shift makes a
/// rectangle a square and an ellipse a circle, and snaps a line or an arrow to a multiple of 45
/// degrees (its length is the pointer's distance).
pub fn constrain(tool: Tool, start: Point, p: Point, shift: bool) -> Point {
    if !shift {
        return p;
    }
    let d = p - start;
    match tool {
        Tool::Rect | Tool::Ellipse => {
            let m = d.x.abs().max(d.y.abs());
            Point::new(
                start.x + m * if d.x < 0.0 { -1.0 } else { 1.0 },
                start.y + m * if d.y < 0.0 { -1.0 } else { 1.0 },
            )
        }
        Tool::Line | Tool::Arrow => {
            let step = std::f32::consts::FRAC_PI_4;
            let angle = (d.y.atan2(d.x) / step).round() * step;
            start + Point::new(angle.cos(), angle.sin()) * d.len()
        }
        _ => p,
    }
}

/// The geometry a tool makes of a drag from `a` to `b` (and, for a scribble, the points).
fn draw_kind(tool: Tool, a: Point, b: Point, points: &[Point]) -> Kind {
    match tool {
        Tool::Ellipse => Kind::Ellipse { a, b },
        Tool::Arrow => Kind::Arrow { a, b },
        Tool::Line => Kind::Line { a, b },
        Tool::Freehand => Kind::Freehand {
            points: points.to_vec(),
        },
        _ => Kind::Rect { a, b },
    }
}
