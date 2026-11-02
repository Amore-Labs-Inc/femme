//! `TerminalElement`: paints a [`RowCache`] as a character grid.
//!
//! Per frame and pane: each visible row is batched into background runs and same-style text runs
//! ([`batch_row`]), then ALL background quads are painted first and ALL text second so the scene
//! splits into two instanced draws instead of two per row. Text runs are shaped every frame and
//! painted with `paint_glyph` per glyph.
//!
//! Approach read from Zed's terminal element (batched rects + text runs); no code taken from it
//! (GPL), this one is built on the proto's run model.

use std::cell::RefCell;
use std::rc::Rc;

use femme_proto::term::CursorShape;
use gpui::{
    black, fill, font, outline, point, px, relative, size, App, BorderStyle, Bounds, Element,
    ElementId, FocusHandle, FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, LayoutId, Pixels, Rgba, SharedString, StrikethroughStyle, Style, TextAlign,
    TextRun as GpuiRun, UnderlineStyle, Window,
};
use unicode_width::UnicodeWidthChar;

use super::pane::{Metrics, PaneState};
use super::runs::{batch_row, Rgb, TextRun, UnderlineKind};
use crate::theme::{FONT_MONO, NEON, PAPER, TERM_SIZE};

/// Converts an 8-bit colour to gpui's [`Hsla`].
pub fn hsla(c: Rgb) -> Hsla {
    Hsla::from(Rgba {
        r: f32::from(c[0]) / 255.0,
        g: f32::from(c[1]) / 255.0,
        b: f32::from(c[2]) / 255.0,
        a: 1.0,
    })
}

/// The terminal font for a style.
fn grid_font(bold: bool, italic: bool) -> gpui::Font {
    let mut f = font(FONT_MONO);
    if bold {
        f.weight = FontWeight::BOLD;
    }
    if italic {
        f.style = FontStyle::Italic;
    }
    f
}

/// Measures the cell grid for the terminal font: advance of `M` and the line box height.
pub fn measure(window: &Window) -> Metrics {
    let line = window.text_system().shape_line(
        SharedString::new_static("M"),
        TERM_SIZE,
        &[GpuiRun {
            len: 1,
            font: grid_font(false, false),
            color: black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    // Snap the cell to whole device pixels: fractional row/column edges make adjacent background
    // quads anti-alias into visible seams.
    let scale = window.scale_factor();
    let cell_w = px((f32::from(line.width()) * scale).round().max(1.) / scale);
    let line_h = px((f32::from(line.ascent + line.descent) * scale)
        .ceil()
        .max(1.)
        / scale);
    Metrics {
        cell_w,
        line_h,
        scale,
    }
}

/// A piece holding one double-width char (the batcher always emits those alone with `cols == 2`).
fn is_wide(t: &TextRun<'_>) -> bool {
    t.cols == 2 && t.text.chars().filter(|c| c.width() != Some(0)).count() == 1
}

fn shape(window: &Window, t: &TextRun<'_>, cell_w: Pixels, wide: bool) -> gpui::ShapedLine {
    let s = &t.style;
    let color = hsla(s.fg);
    let underline = match s.underline {
        UnderlineKind::None => None,
        // ponytail: double/dotted/dashed draw as a single line; curly is gpui's wavy. Upgrade: custom quads.
        k => Some(UnderlineStyle {
            thickness: px(1.),
            color: Some(s.underline_color.map_or(color, hsla)),
            wavy: k == UnderlineKind::Curly,
        }),
    };
    let strikethrough = s.strike.then(|| StrikethroughStyle {
        thickness: px(1.),
        color: Some(color),
    });
    let run = GpuiRun {
        len: t.text.len(),
        font: grid_font(s.bold, s.italic),
        color,
        background_color: None,
        underline,
        strikethrough,
    };
    window.text_system().shape_line(
        SharedString::from(t.text.to_string()),
        TERM_SIZE,
        &[run],
        // ponytail: force_width is per glyph, so a base char with combining marks drifts; clusters need their own run.
        Some(if wide { cell_w * 2.0 } else { cell_w }),
    )
}

/// Paints the glyphs of an undecorated, unwrapped shaped line the way `ShapedLine::paint` places
/// them (baseline centred in `line_h`), but without that call's per-line layer.
fn paint_glyphs(
    line: &gpui::ShapedLine,
    origin: gpui::Point<Pixels>,
    line_h: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    let baseline = point(
        px(0.),
        (line_h - line.ascent - line.descent) / 2. + line.ascent,
    );
    for run in &line.runs {
        for g in &run.glyphs {
            let at = origin + baseline + g.position;
            let _ = if g.is_emoji {
                window.paint_emoji(at, run.font_id, g.id, line.font_size)
            } else {
                window.paint_glyph(at, run.font_id, g.id, line.font_size, color)
            };
        }
    }
}

/// A pane shorter than the grid in the cache (the source has not answered the resize yet) shows the
/// bottom rows, where a shell's prompt and the cursor are. Returns how many top rows to skip, how
/// many rows to draw and the cursor's row among them, if it is still on screen.
fn bottom(cache_rows: u16, pane_rows: u16, cursor_row: u16) -> (u16, u16, Option<u16>) {
    let rows = pane_rows.min(cache_rows);
    let skip = cache_rows - rows;
    (
        skip,
        rows,
        cursor_row.checked_sub(skip).filter(|r| *r < rows),
    )
}

/// The grid element. Cheap to build every frame: it only holds handles.
pub struct TerminalElement {
    state: Rc<RefCell<PaneState>>,
    focus: FocusHandle,
}

impl TerminalElement {
    /// A grid painting `state`; `focus` only decides filled vs hollow cursor.
    pub fn new(state: Rc<RefCell<PaneState>>, focus: FocusHandle) -> Self {
        Self { state, focus }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

/// What prepaint hands to paint.
pub struct Prepaint {
    cols: u16,
    rows: u16,
    m: Metrics,
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        style.flex_grow = 1.0;
        style.flex_shrink = 1.0;
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) -> Prepaint {
        let mut st = self.state.borrow_mut();
        let scale = window.scale_factor();
        let m = match st.metrics {
            Some(m) if (m.scale - scale).abs() < f32::EPSILON => m,
            _ => {
                let m = measure(window);
                st.metrics = Some(m);
                m
            }
        };
        let cols =
            ((bounds.size.width / m.cell_w).floor() as i64).clamp(1, i64::from(u16::MAX)) as u16;
        let rows =
            ((bounds.size.height / m.line_h).floor() as i64).clamp(1, i64::from(u16::MAX)) as u16;
        st.want_size(cols, rows);
        Prepaint { cols, rows, m }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        pre: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let probe = crate::bench::probe::enabled().then(std::time::Instant::now);
        let Metrics {
            cell_w,
            line_h,
            scale,
        } = pre.m;
        window.paint_quad(fill(bounds, PAPER));
        // Rows and columns hang off a device-pixel-aligned origin (the 1.2px rules leave it fractional).
        let snap = |v: Pixels| px((f32::from(v) * scale).round() / scale);
        let origin = point(snap(bounds.origin.x), snap(bounds.origin.y));

        let mut guard = self.state.borrow_mut();
        guard.origin = origin;
        let scroll = guard.scroll;
        let PaneState {
            cache,
            palette,
            bgs,
            ..
        } = &mut *guard;
        let cols = pre.cols.min(cache.cols);
        let (skip, rows, cursor_row) = bottom(cache.rows, pre.rows, cache.cursor.row);
        let cache = &*cache;
        // Scrolled back, the rows on show start `scroll` rows above the live screen's.
        let first = (cache.top_abs + u64::from(skip)).saturating_sub(scroll);

        // Batch every visible row first so backgrounds and text can be painted as two groups.
        bgs.clear();
        let mut texts: Vec<(u16, TextRun<'_>)> = Vec::with_capacity(512);
        let (mut row_bgs, mut row_texts) = (Vec::with_capacity(64), Vec::with_capacity(64));
        for r in 0..rows {
            let Some(row) = cache.row(first + u64::from(r)) else {
                continue;
            };
            row_bgs.clear();
            row_texts.clear();
            batch_row(
                row,
                cols,
                &mut |id| palette.resolve(&cache.style(id)),
                &mut row_bgs,
                &mut row_texts,
            );
            bgs.extend(row_bgs.iter().map(|b| (r, *b)));
            texts.extend(row_texts.iter().map(|t| (r, *t)));
        }

        // gpui orders primitives by inserting each one's bounds into a tree (about a microsecond
        // apiece once thousands overlap: half of a fire frame in a profile). Primitives inside one
        // layer share the layer's order and skip the tree, so all backgrounds go in one layer and all
        // glyphs in a second one (two inserts per pane instead of one per run; `ShapedLine::paint`
        // alone opens a layer per run). Glyphs are painted directly for the same reason.
        window.paint_layer(bounds, |window| {
            for (r, b) in bgs.iter() {
                let origin = origin + point(cell_w * f32::from(b.col), line_h * f32::from(*r));
                window.paint_quad(fill(
                    Bounds::new(origin, size(cell_w * f32::from(b.cols), line_h)),
                    hsla(b.color),
                ));
            }
        });

        // ponytail: block and box-drawing glyphs come from the font, so they can leave 1px seams at cell
        // edges (visible in the fire); upgrade: draw U+2500..=U+259F as quads like other terminal UIs.
        window.paint_layer(bounds, |window| {
            for (r, t) in &texts {
                let origin = origin + point(cell_w * f32::from(t.col), line_h * f32::from(*r));
                let wide = is_wide(t);
                // ponytail: every run is shaped every frame, no cache: a 4096-entry LRU of shaped lines measured
                // no faster on the hidden bench (README, Results). Upgrade: a cache keyed by (text, style) if a
                // much bigger grid shows `shape` in the profile.
                let line = shape(window, t, cell_w, wide);
                if t.style.underline == UnderlineKind::None && !t.style.strike {
                    paint_glyphs(&line, origin, line_h, hsla(t.style.fg), window);
                } else {
                    // Decorated runs take gpui's own path (it draws the lines); they are rare.
                    let _ = line.paint(origin, line_h, TextAlign::Left, None, window, cx);
                }
            }
        });

        // ponytail: the cursor is hidden while scrolled back; upgrade: draw it `scroll` rows lower.
        let c = cache.cursor;
        if let Some(row) = cursor_row.filter(|_| c.visible && c.col < cols && scroll == 0) {
            let origin = origin + point(cell_w * f32::from(c.col), line_h * f32::from(row));
            let cell = Bounds::new(origin, size(cell_w, line_h));
            let color = Hsla::from(NEON);
            let q = match c.shape {
                CursorShape::Block if self.focus.is_focused(window) => {
                    fill(cell, Hsla { a: 0.6, ..color })
                }
                CursorShape::Block => outline(cell, color, BorderStyle::Solid),
                CursorShape::Underline => fill(
                    Bounds::new(
                        origin + point(px(0.), line_h - px(2.)),
                        size(cell_w, px(2.)),
                    ),
                    color,
                ),
                CursorShape::Bar => fill(Bounds::new(origin, size(px(2.), line_h)), color),
            };
            window.paint_quad(q);
        }
        if let Some(t0) = probe {
            crate::bench::probe::paint_since(t0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shrunk_pane_shows_the_bottom_rows_and_keeps_the_cursor() {
        // 10 rows in the cache, room for 4: rows 6..10 are drawn, not 0..4.
        assert_eq!(
            bottom(10, 4, 9),
            (6, 4, Some(3)),
            "the prompt row stays on screen"
        );
        assert_eq!(bottom(10, 4, 6), (6, 4, Some(0)));
        assert_eq!(
            bottom(10, 4, 5),
            (6, 4, None),
            "a cursor scrolled out of view is not drawn"
        );
        // A pane as tall as the grid, or taller, shows all of it from the top.
        assert_eq!(bottom(10, 10, 9), (0, 10, Some(9)));
        assert_eq!(bottom(10, 30, 9), (0, 10, Some(9)));
        assert_eq!(bottom(0, 5, 0), (0, 0, None));
    }
}
