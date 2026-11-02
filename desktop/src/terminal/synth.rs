//! Deterministic synthetic terminals for the bench, and the holder that streams them. [`Fire`]
//! rewrites every cell every frame (the renderer's worst case: all rows dirty, many short style
//! runs); [`ShellScroll`] scrolls one new line per frame (the delta path: one row patch and a bigger
//! `top_abs`) and, as [`ShellScroll::cat`], a whole screen of new text per frame (the shaping worst
//! case). [`Holder`] stands in for `femme-holder`'s view of one client: it answers `Open` with a
//! snapshot, keeps at most [`WINDOW`] frames unacked and merges while the window is full (plan rule 6).

use std::collections::VecDeque;

use femme_proto::term::{
    Attrs, Color, Cursor, CursorShape, Frame, Modes, RowPatch, Screen, Style, TermUp,
};

use crate::rng::Rng;

/// Frames the holder keeps unacked per view (plan rule 6).
// ponytail: frames only; the holder also caps 128 KiB unacked, add a byte estimate in `Holder::send` if big panes need it.
pub const WINDOW: usize = 8;

/// A deterministic source of terminal output as frames, standing in for a holder's emulator. The
/// first frame after `new`, `resize`, `open` or `restart` is a full snapshot (`base_rev == 0`, all
/// styles in `new_styles`); later frames chain `base_rev` to the rev last sent and carry everything
/// stepped since, so several steps merge into one frame.
pub trait FrameSource: Send {
    /// Changes the screen size; the next frame is a full snapshot at the new size (same epoch, the
    /// rev keeps counting).
    fn resize(&mut self, cols: u16, rows: u16);
    /// The program wrote one more step of output: the state and the rev move on, no frame yet.
    fn step(&mut self);
    /// The frame for the client: a delta over everything stepped since the last frame, or a snapshot.
    fn frame(&mut self) -> Frame;
    /// The client (re)opened the stream: the next frame is a snapshot.
    fn open(&mut self);
    /// The PTY was recreated: a new epoch, the output starts over, the next frame is a snapshot.
    fn restart(&mut self);
    /// One step and its frame.
    fn tick(&mut self) -> Frame {
        self.step();
        self.frame()
    }
}

/// What a source remembers about its one client: the holder epoch, the rev reached, the rev last
/// sent and whether the next frame is a snapshot.
struct Wire {
    epoch: u64,
    rev: u64,
    sent: u64,
    snapshot: bool,
}

impl Wire {
    fn new(epoch: u64) -> Self {
        Wire {
            epoch,
            rev: 0,
            sent: 0,
            snapshot: true,
        }
    }

    /// `(base_rev, snapshot)` of the frame going out now; everything up to `rev` counts as sent.
    fn take(&mut self) -> (u64, bool) {
        let snapshot = std::mem::take(&mut self.snapshot);
        let base = if snapshot { 0 } else { self.sent };
        self.sent = self.rev;
        (base, snapshot)
    }

    fn restart(&mut self) {
        *self = Wire::new(self.epoch + 1);
    }
}

/// Heat levels of the fire.
const LEVELS: usize = 36;
/// The hottest level, held by the bottom row.
const HOT: u8 = LEVELS as u8 - 1;
/// Holder epoch reported by [`Fire`] (fixed, non-zero).
const FIRE_EPOCH: u64 = 0xF12E;
/// Holder epoch reported by [`ShellScroll`] (fixed, non-zero).
const SHELL_EPOCH: u64 = 0x5E11;

/// The DOOM fire palette (black, deep red, orange, yellow, white) minus one of its duplicate entries.
const FIRE_PALETTE: [(u8, u8, u8); LEVELS] = [
    (0x07, 0x07, 0x07),
    (0x1F, 0x07, 0x07),
    (0x2F, 0x0F, 0x07),
    (0x47, 0x0F, 0x07),
    (0x57, 0x17, 0x07),
    (0x67, 0x1F, 0x07),
    (0x77, 0x1F, 0x07),
    (0x8F, 0x27, 0x07),
    (0x9F, 0x2F, 0x07),
    (0xAF, 0x3F, 0x07),
    (0xBF, 0x47, 0x07),
    (0xC7, 0x47, 0x07),
    (0xDF, 0x4F, 0x07),
    (0xDF, 0x57, 0x07),
    (0xD7, 0x5F, 0x07),
    (0xD7, 0x5F, 0x07),
    (0xD7, 0x67, 0x0F),
    (0xCF, 0x6F, 0x0F),
    (0xCF, 0x77, 0x0F),
    (0xCF, 0x7F, 0x0F),
    (0xCF, 0x87, 0x17),
    (0xC7, 0x87, 0x17),
    (0xC7, 0x8F, 0x17),
    (0xC7, 0x97, 0x1F),
    (0xBF, 0x9F, 0x1F),
    (0xBF, 0x9F, 0x1F),
    (0xBF, 0xA7, 0x27),
    (0xBF, 0xA7, 0x27),
    (0xBF, 0xAF, 0x2F),
    (0xB7, 0xAF, 0x2F),
    (0xB7, 0xB7, 0x2F),
    (0xB7, 0xB7, 0x37),
    (0xCF, 0xCF, 0x6F),
    (0xDF, 0xDF, 0x9F),
    (0xEF, 0xEF, 0xC7),
    (0xFF, 0xFF, 0xFF),
];

fn fire_rgb(h: usize) -> Color {
    let (r, g, b) = FIRE_PALETTE[h];
    Color::Rgb(r, g, b)
}

/// Style ids 1..=36, one per heat level: background = the level's colour, glyphs a bit hotter.
fn fire_styles() -> Vec<(u16, Style)> {
    (0..LEVELS)
        .map(|h| {
            let style = Style {
                fg: fire_rgb((h + 6).min(LEVELS - 1)),
                bg: fire_rgb(h),
                ..Style::default()
            };
            (h as u16 + 1, style)
        })
        .collect()
}

/// Shade glyph for a heat level.
fn fire_glyph(h: u8) -> char {
    match h {
        0..=2 => ' ',
        3..=11 => '░',
        12..=19 => '▒',
        20..=27 => '▓',
        _ => '█',
    }
}

/// Classic DOOM fire on a `rows x cols` heat grid: the bottom row stays at the hottest level and
/// every other cell takes the heat of a random neighbour below (offset -1..=1) minus a random
/// decay of 0..=2. Every frame resends every row. It runs on the alternate screen like any
/// full-screen program: no scrollback, `history_lo_abs == top_abs`.
pub struct Fire {
    cols: u16,
    rows: u16,
    /// Row-major heat levels, row 0 at the top.
    heat: Vec<u8>,
    rng: Rng,
    wire: Wire,
}

impl Fire {
    /// A fire of the given size (each clamped to at least 1), already burning at full height.
    pub fn new(cols: u16, rows: u16, seed: u64) -> Self {
        let mut fire = Fire {
            cols: 0,
            rows: 0,
            heat: Vec::new(),
            rng: Rng::new(seed),
            wire: Wire::new(FIRE_EPOCH),
        };
        fire.resize(cols, rows);
        fire
    }

    /// One propagation step. Rows are updated top-down, so each reads the previous step's row
    /// below it (as if double-buffered).
    fn burn(&mut self) {
        let w = usize::from(self.cols);
        for y in 0..usize::from(self.rows) - 1 {
            for x in 0..w {
                let r = self.rng.below(9); // one draw for both: offset r % 3, decay r / 3
                let src = (x + (r % 3) as usize).saturating_sub(1).min(w - 1);
                self.heat[y * w + x] = self.heat[(y + 1) * w + src].saturating_sub((r / 3) as u8);
            }
        }
    }
}

impl FrameSource for Fire {
    /// Restarts the fire at the new size (same epoch); the next frame is a full snapshot.
    fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        let (w, h) = (usize::from(self.cols), usize::from(self.rows));
        self.heat.clear();
        self.heat.resize(w * h, 0);
        self.heat[(h - 1) * w..].fill(HOT);
        // Burn in, so the first frame already shows a full-height fire.
        for _ in 0..h {
            self.burn();
        }
        self.wire.snapshot = true;
    }

    fn step(&mut self) {
        self.burn();
        self.wire.rev += 1;
    }

    fn frame(&mut self) -> Frame {
        let (base_rev, snapshot) = self.wire.take();
        let w = usize::from(self.cols);
        let row_patches = self
            .heat
            .chunks_exact(w)
            .enumerate()
            .map(|(y, cells)| {
                // Merge equal neighbours into one run: one style id per heat level.
                let mut runs = Vec::new();
                let mut x = 0;
                while x < w {
                    let h = cells[x];
                    let n = cells[x..].iter().take_while(|&&c| c == h).count();
                    let glyph = fire_glyph(h);
                    let mut text = String::with_capacity(n * glyph.len_utf8());
                    text.extend(std::iter::repeat_n(glyph, n));
                    runs.push((u16::from(h) + 1, text));
                    x += n;
                }
                RowPatch {
                    abs: y as u64,
                    wrapped: false,
                    runs,
                    wide_cols: Vec::new(),
                }
            })
            .collect();
        Frame {
            epoch: self.wire.epoch,
            rev: self.wire.rev,
            base_rev,
            screen: Screen::Alt,
            cols: self.cols,
            rows: self.rows,
            top_abs: 0,
            history_lo_abs: 0,
            cursor: Cursor {
                visible: false,
                ..Cursor::default()
            },
            modes: Modes::ALT_SCREEN,
            // Every server frame carries the title: a frame without one clears it.
            title: Some("fire".to_string()),
            new_styles: if snapshot { fire_styles() } else { Vec::new() },
            row_patches,
            ..Frame::default()
        }
    }

    fn open(&mut self) {
        self.wire.snapshot = true;
    }

    fn restart(&mut self) {
        self.wire.restart();
    }
}

/// Style ids of the shell output.
const GREEN_BOLD: u16 = 1;
const CYAN: u16 = 2;
const YELLOW_BOLD: u16 = 3;
const RED_BOLD: u16 = 4;
const GREY: u16 = 5;
const BOLD: u16 = 6;
const DEFAULT: u16 = 0;

/// The 64 characters of base64, what `cat` of random data looks like.
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn shell_styles() -> Vec<(u16, Style)> {
    let style = |fg, attrs| Style {
        fg,
        attrs,
        ..Style::default()
    };
    vec![
        (GREEN_BOLD, style(Color::Indexed(2), Attrs::BOLD)),
        (CYAN, style(Color::Indexed(6), Attrs::BOLD)),
        (YELLOW_BOLD, style(Color::Indexed(3), Attrs::BOLD)),
        (RED_BOLD, style(Color::Indexed(1), Attrs::BOLD)),
        (GREY, style(Color::Indexed(8), Attrs::empty())),
        (BOLD, style(Color::Default, Attrs::BOLD)),
    ]
}

const CRATES: &[&str] = &[
    "serde",
    "tokio",
    "gpui",
    "femme-proto",
    "postcard",
    "bytes",
    "ulid",
    "bitflags",
    "thiserror",
    "quinn",
    "rustls",
    "zstd",
    "unicode-width",
    "taffy",
    "cosmic-text",
    "rustybuzz",
    "swash",
    "hdrhistogram",
    "clap",
    "anyhow",
    "mimalloc",
    "futures",
    "tracing",
    "axum",
    "femme-term",
];
const FILES: &[&str] = &[
    "src/terminal/pane.rs",
    "src/terminal/runs.rs",
    "src/transcript/pacer.rs",
    "src/net/quic.rs",
    "src/state/store.rs",
    "src/app.rs",
    "src/main.rs",
];
const WARNINGS: &[&str] = &[
    "unused variable: `frame`",
    "unused import: `std::fmt`",
    "field `rev` is never read",
    "variable does not need to be mutable",
    "unreachable pattern",
];
const ERRORS: &[(&str, &str)] = &[
    ("E0308", "mismatched types"),
    ("E0597", "`rows` does not live long enough"),
    ("E0425", "cannot find value `top_abs` in this scope"),
    ("E0277", "the trait bound `Pane: Send` is not satisfied"),
];
const CODE: &[&str] = &[
    "let frame = source.tick();",
    "cache.apply(&frame)?;",
    "for row in cache.visible() {",
    "    batch_row(row, cols, &mut resolve, &mut bgs, &mut texts);",
    "let mut x = 5;",
    "match frame.screen {",
];

fn pick<'a, T>(rng: &mut Rng, items: &'a [T]) -> &'a T {
    &items[rng.below(items.len() as u32) as usize]
}

/// "cargo build"-style output scrolling one line per frame. Line `abs` is a pure function of
/// the seed and `abs`, so a snapshot after `resize` re-sends the same lines.
pub struct ShellScroll {
    cols: u16,
    rows: u16,
    seed: u64,
    wire: Wire,
    /// One past the last line written; the cursor sits on line `end - 1`, the bottom screen row.
    end: u64,
    /// `end` when the last frame went out: a delta carries the lines from here on.
    sent_end: u64,
    /// `cat`-like output: a whole screen of fresh text per step instead of one build line.
    cat: bool,
}

impl ShellScroll {
    /// A terminal of the given size (each clamped to at least 1) whose first frame shows `rows` lines of output.
    pub fn new(cols: u16, rows: u16, seed: u64) -> Self {
        ShellScroll {
            cols: cols.max(1),
            rows: rows.max(1),
            seed,
            wire: Wire::new(SHELL_EPOCH),
            end: 0,
            sent_end: 0,
            cat: false,
        }
    }

    /// Like `new`, but every step scrolls a whole screen of new text, as `cat /dev/urandom | base64`
    /// would: no row on screen was on screen before, so every text run is shaped afresh.
    pub fn cat(cols: u16, rows: u16, seed: u64) -> Self {
        ShellScroll {
            cat: true,
            ..Self::new(cols, rows, seed)
        }
    }

    /// A `cat` line: an address, then random base64, cut to the screen width. Unique per row.
    fn cat_line(&self, abs: u64) -> RowPatch {
        let mut rng = Rng::new(self.seed ^ abs.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let cols = usize::from(self.cols);
        let tail = (0..cols)
            .map(|_| char::from(BASE64[rng.below(64) as usize]))
            .collect();
        RowPatch {
            abs,
            wrapped: false,
            runs: cut(vec![(GREY, format!("{abs:08x} ")), (DEFAULT, tail)], cols),
            wide_cols: Vec::new(),
        }
    }

    /// Line `abs` of the output, cut to the screen width. All ASCII, so one byte is one column.
    fn line(&self, abs: u64) -> RowPatch {
        if self.cat {
            return self.cat_line(abs);
        }
        let mut rng = Rng::new(self.seed ^ abs.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let roll = rng.below(100);
        let version =
            |rng: &mut Rng| format!("v{}.{}.{}", rng.below(3), rng.below(40), rng.below(300));
        let runs: Vec<(u16, String)> = match roll {
            0..=54 => vec![
                (GREEN_BOLD, "   Compiling".into()),
                (
                    DEFAULT,
                    format!(" {} {}", pick(&mut rng, CRATES), version(&mut rng)),
                ),
            ],
            55..=64 => vec![
                (YELLOW_BOLD, "warning".into()),
                (BOLD, format!(": {}", pick(&mut rng, WARNINGS))),
            ],
            65..=74 => vec![
                (CYAN, "  -->".into()),
                (
                    GREY,
                    format!(
                        " {}:{}:{}",
                        pick(&mut rng, FILES),
                        rng.range(1, 900),
                        rng.range(1, 80)
                    ),
                ),
            ],
            75..=84 => vec![
                (CYAN, format!("{:>4} |", rng.range(1, 900))),
                (DEFAULT, format!("     {}", pick(&mut rng, CODE))),
            ],
            85..=89 => {
                let (code, msg) = pick(&mut rng, ERRORS);
                vec![
                    (RED_BOLD, format!("error[{code}]")),
                    (BOLD, format!(": {msg}")),
                ]
            }
            90..=94 => {
                // A long progress line, usually wider than the screen.
                let total = rng.range(200, 600);
                let done = rng.range(1, total);
                let bar = (done * 30 / total) as usize;
                let mut tail = format!(
                    " [{}>{}] {done}/{total}:",
                    "=".repeat(bar),
                    " ".repeat(30 - bar)
                );
                for _ in 0..rng.range(4, 20) {
                    tail.push(' ');
                    tail.push_str(pick(&mut rng, CRATES));
                    tail.push(',');
                }
                tail.pop();
                vec![(CYAN, "    Building".into()), (DEFAULT, tail)]
            }
            _ => vec![
                (GREEN_BOLD, "    Finished".into()),
                (
                    DEFAULT,
                    format!(
                        " `dev` profile [unoptimized + debuginfo] target(s) in {}.{:02}s",
                        rng.below(60),
                        rng.below(100)
                    ),
                ),
            ],
        };
        RowPatch {
            abs,
            wrapped: false,
            runs: cut(runs, usize::from(self.cols)),
            wide_cols: Vec::new(),
        }
    }
}

/// Cuts ASCII runs to `cols` columns (no wrapping).
fn cut(mut runs: Vec<(u16, String)>, cols: usize) -> Vec<(u16, String)> {
    let mut left = cols;
    let mut keep = 0;
    for (_, text) in runs.iter_mut() {
        debug_assert!(text.is_ascii());
        if left == 0 {
            break;
        }
        text.truncate(left);
        left -= text.len();
        keep += 1;
    }
    runs.truncate(keep);
    runs
}

/// Lines of scrollback the synthetic shell's holder keeps.
const HISTORY: u64 = 10_000;

/// Columns the text of `p` takes (all ASCII).
fn text_len(p: &RowPatch) -> usize {
    p.runs.iter().map(|(_, t)| t.len()).sum()
}

impl FrameSource for ShellScroll {
    /// Keeps the output and the cursor line at the bottom; the next frame is a full snapshot.
    fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        self.wire.snapshot = true;
    }

    fn step(&mut self) {
        self.end += if self.cat { u64::from(self.rows) } else { 1 };
        self.wire.rev += 1;
    }

    fn frame(&mut self) -> Frame {
        let (base_rev, snapshot) = self.wire.take();
        let rows = u64::from(self.rows);
        // Prefill the screen when there is less output than rows.
        self.end = self.end.max(rows);
        let top_abs = self.end - rows;
        let history_lo_abs = top_abs.saturating_sub(HISTORY);
        // A snapshot shows the screen; a delta carries every line written since the last frame that
        // the history still holds (the lines of merged steps).
        let from = if snapshot {
            top_abs
        } else {
            self.sent_end.max(history_lo_abs)
        };
        let row_patches: Vec<RowPatch> = (from..self.end).map(|abs| self.line(abs)).collect();
        self.sent_end = self.end;
        let last_len = row_patches
            .last()
            .map_or_else(|| text_len(&self.line(self.end - 1)), text_len);
        Frame {
            epoch: self.wire.epoch,
            rev: self.wire.rev,
            base_rev,
            screen: Screen::Primary,
            cols: self.cols,
            rows: self.rows,
            top_abs,
            history_lo_abs,
            cursor: Cursor {
                row: self.rows - 1,
                col: (last_len as u16).min(self.cols - 1),
                shape: CursorShape::Block,
                visible: true,
            },
            modes: Modes::LINE_WRAP,
            title: Some(if self.cat { "cat" } else { "cargo build" }.to_string()),
            new_styles: if snapshot { shell_styles() } else { Vec::new() },
            row_patches,
            ..Frame::default()
        }
    }

    fn open(&mut self) {
        self.wire.snapshot = true;
    }

    fn restart(&mut self) {
        self.wire.restart();
        self.end = 0;
        self.sent_end = 0;
    }
}

/// The holder's view of one client (plan rule 6). Nothing goes out before the client's `Open`, which
/// is answered with a snapshot at once. At most [`WINDOW`] frames are unacked; while the window is
/// full the program keeps running and the next frame merges everything that happened meanwhile.
pub struct Holder {
    src: Box<dyn FrameSource>,
    /// Revs of the frames sent and not acked yet.
    unacked: VecDeque<u64>,
    open: bool,
}

impl Holder {
    /// A holder whose program has already written its first output (revs on the wire start at 1).
    pub fn new(mut src: Box<dyn FrameSource>) -> Self {
        src.step();
        Holder {
            src,
            unacked: VecDeque::new(),
            open: false,
        }
    }

    /// The program writes one more step; the frame to send if the client has room for it.
    pub fn tick(&mut self) -> Option<Frame> {
        self.src.step();
        self.send()
    }

    /// Handles what the client said; the frame to send in answer, if any.
    pub fn up(&mut self, msg: TermUp) -> Option<Frame> {
        match msg {
            TermUp::Open { .. } => {
                self.open = true;
                self.unacked.clear();
                self.src.open();
                self.send()
            }
            TermUp::Ack { rev } => {
                while self.unacked.front().is_some_and(|&r| r <= rev) {
                    self.unacked.pop_front();
                }
                None
            }
            TermUp::Resize { cols, rows } => {
                self.src.resize(cols, rows);
                None
            }
            // Keys, mouse, paste and scrollback requests: the synthetic programs ignore them.
            _ => None,
        }
    }

    /// The PTY behind the pane was recreated (a new epoch); the client finds out from the next frame.
    pub fn restart(&mut self) {
        self.src.restart();
    }

    fn send(&mut self) -> Option<Frame> {
        if !self.open || self.unacked.len() >= WINDOW {
            return None;
        }
        let f = self.src.frame();
        self.unacked.push_back(f.rev);
        Some(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_term::client::RowCache;
    use unicode_width::UnicodeWidthStr;

    fn width(p: &RowPatch) -> usize {
        p.runs.iter().map(|(_, t)| t.width()).sum()
    }

    fn text(p: &RowPatch) -> String {
        let s: String = p.runs.iter().map(|(_, t)| t.as_str()).collect();
        s.trim_end_matches(' ').to_string()
    }

    fn style_ids(f: &Frame) -> Vec<u16> {
        f.new_styles.iter().map(|&(id, _)| id).collect()
    }

    /// Mean heat (style id - 1) of a fire row.
    fn mean_heat(p: &RowPatch) -> f64 {
        let (mut sum, mut n) = (0.0, 0.0);
        for (id, t) in &p.runs {
            let c = t.chars().count() as f64;
            sum += f64::from(id - 1) * c;
            n += c;
        }
        sum / n
    }

    #[test]
    fn fire_first_frame_is_full_snapshot() {
        let f = Fire::new(80, 24, 1).tick();
        assert_eq!((f.base_rev, f.rev), (0, 1));
        assert_ne!(f.epoch, 0);
        assert_eq!(style_ids(&f), (1..=36).collect::<Vec<u16>>());
        assert_eq!(f.new_styles[0].1.bg, Color::Rgb(7, 7, 7));
        assert_eq!(f.new_styles[35].1.bg, Color::Rgb(255, 255, 255));
        assert_eq!(f.new_styles[0].1.fg, fire_rgb(6));
        assert_eq!(f.new_styles[35].1.fg, fire_rgb(35));
        assert!(!f.cursor.visible);
        assert_eq!((f.top_abs, f.history_lo_abs), (0, 0));
    }

    #[test]
    fn fire_runs_on_the_alt_screen_without_scrollback() {
        let mut fire = Fire::new(40, 10, 1);
        for _ in 0..3 {
            let f = fire.tick();
            assert_eq!(f.screen, Screen::Alt);
            assert!(f.modes.contains(Modes::ALT_SCREEN));
            assert_eq!(f.history_lo_abs, f.top_abs);
        }
    }

    #[test]
    fn every_frame_carries_the_title() {
        // The real cache clears the title on a frame that has none (the server always sends it).
        let sources: Vec<(Box<dyn FrameSource>, &str)> = vec![
            (Box::new(Fire::new(40, 10, 1)), "fire"),
            (Box::new(ShellScroll::new(40, 10, 1)), "cargo build"),
            (Box::new(ShellScroll::cat(40, 10, 1)), "cat"),
        ];
        for (mut src, title) in sources {
            let mut cache = RowCache::new();
            for _ in 0..3 {
                cache.apply(&src.tick()).unwrap();
                assert_eq!(cache.title.as_deref(), Some(title));
            }
        }
    }

    #[test]
    fn fire_frames_resend_every_row_at_full_width() {
        let mut fire = Fire::new(97, 31, 5);
        for n in 1..=20u64 {
            let f = fire.tick();
            assert_eq!(f.rev, n);
            if n > 1 {
                assert_eq!(f.base_rev, n - 1);
                assert!(f.new_styles.is_empty());
            }
            assert_eq!(f.row_patches.len(), 31);
            for (y, p) in f.row_patches.iter().enumerate() {
                assert_eq!(p.abs, y as u64);
                assert_eq!(width(p), 97);
                // Neighbouring runs always differ in style (equal neighbours are merged).
                assert!(p.runs.windows(2).all(|w| w[0].0 != w[1].0));
                assert!(p.runs.iter().all(|(id, _)| (1..=36).contains(id)));
            }
        }
    }

    #[test]
    fn fire_heat_is_plausible() {
        let mut fire = Fire::new(120, 60, 9);
        let f = fire.tick();
        let rows = &f.row_patches;
        // Bottom row: one run at the hottest level.
        assert_eq!(rows[59].runs, vec![(36, "█".repeat(120))]);
        // Heat falls with height: decay averages one level per row over 36 levels.
        assert!(mean_heat(&rows[58]) > 30.0, "{}", mean_heat(&rows[58]));
        assert!(mean_heat(&rows[0]) < 1.0, "{}", mean_heat(&rows[0]));
        let upper: f64 = rows[..30].iter().map(mean_heat).sum();
        let lower: f64 = rows[30..].iter().map(mean_heat).sum();
        assert!(upper < lower);
        // Glyphs follow the heat.
        for p in rows {
            for (id, t) in &p.runs {
                assert!(t.chars().all(|c| c == fire_glyph((id - 1) as u8)));
            }
        }
    }

    #[test]
    fn fire_is_deterministic_per_seed() {
        let (mut a, mut b, mut c) = (
            Fire::new(40, 20, 3),
            Fire::new(40, 20, 3),
            Fire::new(40, 20, 4),
        );
        let mut differs = false;
        for _ in 0..10 {
            let (fa, fb, fc) = (a.tick(), b.tick(), c.tick());
            assert_eq!(fa, fb);
            differs |= fa.row_patches != fc.row_patches;
        }
        assert!(differs);
    }

    #[test]
    fn shell_first_frame_is_full_snapshot() {
        let f = ShellScroll::new(80, 24, 1).tick();
        assert_eq!((f.base_rev, f.rev, f.top_abs), (0, 1, 0));
        assert_ne!(f.epoch, 0);
        assert_eq!(style_ids(&f), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(f.row_patches.len(), 24);
        assert!(f
            .row_patches
            .iter()
            .enumerate()
            .all(|(i, p)| p.abs == i as u64));
        assert_eq!(f.cursor.row, 23);
        assert!(f.cursor.visible);
    }

    #[test]
    fn shell_ticks_append_one_line_each() {
        let mut sh = ShellScroll::new(60, 10, 2);
        let first = sh.tick();
        let mut prev_rev = first.rev;
        let mut prev_top = first.top_abs;
        for _ in 0..100 {
            let f = sh.tick();
            assert_eq!(f.base_rev, prev_rev);
            assert_eq!(f.rev, prev_rev + 1);
            assert_eq!(f.top_abs, prev_top + 1);
            assert_eq!(f.row_patches.len(), 1);
            assert_eq!(f.row_patches[0].abs, f.top_abs + 9);
            assert!(f.new_styles.is_empty());
            assert!(width(&f.row_patches[0]) <= 60);
            assert_eq!(f.cursor.row, 9);
            assert_eq!(f.history_lo_abs, f.top_abs.saturating_sub(10_000));
            prev_rev = f.rev;
            prev_top = f.top_abs;
        }
    }

    #[test]
    fn shell_lines_are_cut_to_the_width() {
        let mut sh = ShellScroll::new(25, 5, 11);
        let mut cut_seen = false;
        for _ in 0..300 {
            for p in sh.tick().row_patches {
                assert!(width(&p) <= 25);
                assert!(p.runs.iter().all(|(_, t)| !t.is_empty()));
                cut_seen |= width(&p) == 25;
            }
        }
        assert!(cut_seen);
    }

    #[test]
    fn cache_applies_500_ticks_of_each_source() {
        let sources: Vec<Box<dyn FrameSource>> = vec![
            Box::new(Fire::new(80, 24, 7)),
            Box::new(ShellScroll::new(80, 24, 7)),
            Box::new(ShellScroll::cat(80, 24, 7)),
        ];
        for mut src in sources {
            let mut cache = RowCache::new();
            for _ in 0..500 {
                cache.apply(&src.tick()).expect("no resync");
            }
            assert_eq!(cache.rev, 500);
            assert_eq!((cache.cols, cache.rows), (80, 24));
            assert!(cache.visible().all(|r| r.is_some()));
        }
    }

    #[test]
    fn scroll_cache_text_ends_with_the_last_line_emitted() {
        let mut sh = ShellScroll::new(72, 20, 13);
        let mut cache = RowCache::new();
        let mut last = String::new();
        for _ in 0..500 {
            let f = sh.tick();
            cache.apply(&f).unwrap();
            last = text(f.row_patches.last().unwrap());
        }
        assert_eq!(cache.top_abs, 499);
        let screen = cache.text();
        let shown = screen.lines().rev().find(|l| !l.is_empty()).unwrap();
        assert_eq!(shown, last);
        // Scrolled-off lines stay addressable (within the cache's history).
        assert!(cache.row(400).is_some());
    }

    #[test]
    fn resize_sends_a_full_snapshot_again() {
        let sources: Vec<Box<dyn FrameSource>> = vec![
            Box::new(Fire::new(80, 24, 1)),
            Box::new(ShellScroll::new(80, 24, 1)),
        ];
        for mut src in sources {
            let mut cache = RowCache::new();
            for _ in 0..5 {
                cache.apply(&src.tick()).unwrap();
            }
            let epoch = cache.epoch;
            src.resize(50, 12);
            let f = src.tick();
            assert_eq!((f.base_rev, f.rev, f.epoch), (0, 6, epoch));
            assert_eq!((f.cols, f.rows), (50, 12));
            assert!(!f.new_styles.is_empty());
            assert_eq!(f.row_patches.len(), 12);
            assert!(f.row_patches.iter().all(|p| width(p) <= 50));
            cache.apply(&f).unwrap();
            let g = src.tick();
            assert_eq!(g.base_rev, 6);
            cache.apply(&g).unwrap();
            assert_eq!((cache.cols, cache.rows), (50, 12));
            assert!(cache.visible().all(|r| r.is_some()));
        }
    }

    #[test]
    fn shell_resize_keeps_the_bottom_lines() {
        let mut sh = ShellScroll::new(80, 10, 4);
        let mut cache = RowCache::new();
        for _ in 0..30 {
            cache.apply(&sh.tick()).unwrap();
        }
        let before: Vec<String> = cache.text().lines().map(str::to_string).collect();
        sh.resize(80, 4);
        let f = sh.frame(); // the snapshot at the new size; no new output
        cache.apply(&f).unwrap();
        let after: Vec<String> = cache.text().lines().map(str::to_string).collect();
        assert_eq!(after, before[6..]);
        // Growing past the output so far prefills fresh lines below.
        let mut small = ShellScroll::new(80, 3, 4);
        small.tick();
        small.resize(80, 8);
        let f = small.tick();
        assert_eq!((f.top_abs, f.row_patches.len()), (0, 8));
    }

    #[test]
    fn tiny_sizes_do_not_panic() {
        let mut fire = Fire::new(0, 0, 1);
        let mut sh = ShellScroll::new(0, 0, 1);
        for _ in 0..3 {
            assert_eq!(fire.tick().row_patches.len(), 1);
            assert_eq!(sh.tick().row_patches.len(), 1);
        }
        fire.resize(1, 1);
        sh.resize(1, 1);
        assert_eq!(width(&fire.tick().row_patches[0]), 1);
        assert!(width(&sh.tick().row_patches[0]) <= 1);
    }

    #[test]
    fn cat_scrolls_a_screen_of_unique_text_every_frame() {
        let mut cat = ShellScroll::cat(60, 8, 1);
        cat.tick(); // the snapshot
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10 {
            let f = cat.tick();
            assert_eq!(
                f.row_patches.len(),
                8,
                "a whole screen of new rows per frame"
            );
            for p in &f.row_patches {
                assert_eq!(width(p), 60);
                assert!(seen.insert(text(p)), "row text repeated: {}", text(p));
            }
        }
    }

    fn open() -> TermUp {
        TermUp::Open {
            pane: femme_proto::PaneId {
                sid: femme_proto::Id(1),
                no: 0,
            },
            epoch: 0,
            have_rev: 0,
            client_instance: 1,
        }
    }

    #[test]
    fn holder_is_silent_until_open_and_answers_it_with_a_snapshot() {
        let mut h = Holder::new(Box::new(ShellScroll::new(40, 6, 1)));
        assert!(h.tick().is_none(), "nothing goes out before Open");
        let snap = h.up(open()).expect("Open is answered at once");
        assert_eq!((snap.base_rev, snap.row_patches.len()), (0, 6));
        let delta = h.tick().expect("the window is open");
        assert_ne!(delta.base_rev, 0);
        // Open in the middle of a stream asks for a snapshot again.
        assert_eq!(h.up(open()).unwrap().base_rev, 0);
    }

    #[test]
    fn holder_merges_what_happens_while_the_window_is_full() {
        let mut h = Holder::new(Box::new(ShellScroll::new(40, 6, 1)));
        let mut cache = RowCache::new();
        let mut frames = vec![h.up(open()).unwrap()];
        for _ in 0..20 {
            frames.extend(h.tick());
        }
        assert_eq!(frames.len(), WINDOW, "no frame past the unacked window");
        for f in &frames {
            cache.apply(f).unwrap();
        }
        let held = cache.rev;
        assert!(h.up(TermUp::Ack { rev: held }).is_none());
        let merged = h.tick().expect("the ack reopens the window");
        assert_eq!(merged.base_rev, held, "chains to the last frame sent");
        assert!(
            merged.rev > held + 1,
            "and covers the steps taken while blocked"
        );
        assert_eq!(
            merged.row_patches.len() as u64,
            merged.rev - held,
            "no line is lost"
        );
        cache.apply(&merged).unwrap();
        assert_eq!(cache.rev, merged.rev);
        assert!(cache.visible().all(|r| r.is_some()));
    }
}
