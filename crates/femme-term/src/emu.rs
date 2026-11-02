//! The server-side emulator: `alacritty_terminal` + `vte` wrapped so PTY output becomes
//! incremental [`Frame`]s.
//!
//! # How rows are numbered
//!
//! Every row ever produced on the primary screen has an *absolute number*, counted from the
//! first line the terminal showed and never reused. `top_abs` is the number of screen row 0;
//! scrollback rows are `[history_lo_abs, top_abs)`. `top_abs` grows by one each time a row is
//! pushed off the top of the primary screen (see `tap` for exactly which operations do that);
//! scrolling inside a region that starts below row 0 does not renumber anything, the rows just
//! change in place. Scrollback rows never change, except that clearing history (`ED 3`, `RIS`)
//! drops them and a *new layout generation* (resize, style-table rebuild) renumbers everything:
//! `top_abs` jumps above every number issued before, so a client's cached old rows can never be
//! mistaken for new ones, and every view's next frame is a snapshot.
//!
//! The alternate screen has no scrollback. While it is active `top_abs` keeps its primary value,
//! its rows reuse the numbers `top_abs..top_abs + rows`, and `history_lo_abs == top_abs`
//! (the primary's history cannot be read while the alt screen owns the grid). Entering and leaving
//! it sends every view a snapshot.
//!
//! # How changes are found
//!
//! Each visible row keeps a copy of its cells and the rev it last changed at (its *stamp*).
//! After every `feed` the live rows are compared with the copies (cost: one pass over the visible
//! cells, ~12 000 comparisons at 200x60, about 25 us) and changed rows get the new rev. This does
//! not use alacritty's damage tracking: that is a hint for what to redraw (any scroll damages the
//! whole screen, `Term::damage` adds the cursor lines itself), while the comparison is exact by
//! construction. A scroll that pushes `n` rows rotates the copies by `n` so rows that merely
//! moved up keep their stamps (they keep their numbers too).
//!
//! A delta frame for a view at rev `r` is the visible rows stamped after `r`, plus the scrollback
//! rows pushed after `r` *whose final content the view does not already hold* (a row that scrolls
//! off unchanged was sent when it was visible). Blank rows that a scroll brings in are never
//! sent: their numbers are new, and a client reads a row it was never sent as blank. When that
//! would be more than a screenful of rows, or a layout/screen change intervened, the view gets a
//! snapshot instead.

mod keys;
mod osc;
mod render;
mod tap;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::{Config, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape as ACursorShape, Processor, Rgb};
use femme_proto::term::{
    Cursor, CursorShape, Frame, Key, Modes, MouseEv, RowPatch, Screen, Style, Timing,
};

use osc::{Found, OscScan};
use render::{ansi_row, cell_style, is_blank, render_row, wire_size, StyleTable};
use tap::{Tap, TapState};

/// Default foreground the emulator reports to colour queries (OSC 10/12, OSC 4;256).
// ponytail: one fixed palette for every client; a client-reported theme needs a new `TermUp` message.
pub const DEFAULT_FG: (u8, u8, u8) = (0xd4, 0xd4, 0xd4);
/// Default background the emulator reports to colour queries (OSC 11). Programs choose their
/// light/dark theme from it.
pub const DEFAULT_BG: (u8, u8, u8) = (0x1e, 0x1e, 0x1e);
/// Widest terminal: [`Emulator::new`] and [`Emulator::resize`] clamp to `2..=MAX_COLS` columns
/// (two so a wide character fits). The holder clamps client sizes to the same limits.
pub const MAX_COLS: u16 = 1000;
/// Tallest terminal: rows are clamped to `1..=MAX_ROWS`.
pub const MAX_ROWS: u16 = 500;
/// Most bytes of row data one [`Emulator::rows`] answer holds (see there). Well under the
/// holder's 16 MiB message limit, which a frame must also fit in.
pub const MAX_ROWS_BYTES: usize = 8 << 20;
/// Longest window title kept, in bytes. A frame carries the title every time.
const MAX_TITLE: usize = 1024;
/// Style-table rebuilds happen at most this often (each one renumbers everything).
const REBUILD_GAP: Duration = Duration::from_secs(1);
/// A synchronized update is applied at the latest this long after it began, however often the
/// program re-sends the begin marker.
const SYNC_MAX: Duration = Duration::from_secs(1);
/// Cell size in pixels reported to `CSI 14 t`; the server cannot know the client's font.
// ponytail: fixed 8x16; the client would have to send its real cell size with `Resize`.
const CELL_W: usize = 8;
const CELL_H: usize = 16;

#[rustfmt::skip]
const ANSI16: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00), (0xcd, 0x00, 0x00), (0x00, 0xcd, 0x00), (0xcd, 0xcd, 0x00),
    (0x00, 0x00, 0xee), (0xcd, 0x00, 0xcd), (0x00, 0xcd, 0xcd), (0xe5, 0xe5, 0xe5),
    (0x7f, 0x7f, 0x7f), (0xff, 0x00, 0x00), (0x00, 0xff, 0x00), (0xff, 0xff, 0x00),
    (0x5c, 0x5c, 0xff), (0xff, 0x00, 0xff), (0x00, 0xff, 0xff), (0xff, 0xff, 0xff),
];

/// The colour for palette index `index` when the program has not overridden it: xterm's 256
/// colours, then foreground (256), background (257), cursor (258).
fn default_color(index: usize) -> Rgb {
    let (r, g, b) = match index {
        0..=15 => ANSI16[index],
        16..=231 => {
            let level = |n: usize| if n == 0 { 0 } else { 55 + 40 * n as u8 };
            let i = index - 16;
            (level(i / 36), level(i / 6 % 6), level(i % 6))
        }
        232..=255 => {
            let v = 8 + 10 * (index - 232) as u8;
            (v, v, v)
        }
        257 => DEFAULT_BG,
        _ => DEFAULT_FG,
    };
    Rgb { r, g, b }
}

/// Things the holder must act on after [`Emulator::feed`], from [`Emulator::take_events`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmuEvent {
    /// Bytes the terminal must write back to the PTY (DA, DSR, cursor position reports, colour
    /// and size queries...).
    PtyWrite(Vec<u8>),
    /// The program set the window title (OSC 0/2, or a title-stack pop).
    Title(String),
    /// The program reset the window title.
    ResetTitle,
    /// BEL. At most one per `take_events` call.
    Bell,
    /// OSC 9 (iTerm2: `title` is empty) or OSC 777 `notify` desktop notification.
    Notify {
        /// Notification title ("" for OSC 9).
        title: String,
        /// Notification body.
        body: String,
    },
}

/// What the listener hands to the emulator: queued events and the latest title change.
#[derive(Default)]
struct Shared {
    events: Vec<EmuEvent>,
    /// `Some(t)` when the title was set (`t`) or reset (`None`) since the last `feed` finished.
    title: Option<Option<String>>,
}

/// alacritty's event sink. Only the events the holder cares about are kept; clipboard (OSC 52 is
/// disabled in the config), colour and size requests never arrive here because `Tap` answers
/// them itself.
struct Listener(Arc<Mutex<Shared>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let mut sh = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match event {
            Event::PtyWrite(s) => sh.events.push(EmuEvent::PtyWrite(s.into_bytes())),
            Event::Title(mut t) => {
                // ponytail: capped here because every frame carries the title; sending it only
                // when it changed needs `Option<Option<String>>` in `Frame::title` (a wire change).
                t.truncate(t.floor_char_boundary(MAX_TITLE));
                sh.title = Some(Some(t.clone()));
                sh.events.push(EmuEvent::Title(t));
            }
            Event::ResetTitle => {
                sh.title = Some(None);
                sh.events.push(EmuEvent::ResetTitle);
            }
            Event::Bell => sh.events.push(EmuEvent::Bell),
            _ => {}
        }
    }
}

/// `Dimensions` for `Term::new` / `Term::resize`.
struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// One visible row: a copy of its cells as of the last pass, and the rev it last changed at.
struct VisRow {
    cells: Vec<Cell>,
    /// `cells` mirrors what clients were last told; false after a scroll brought in a new row or
    /// a snapshot-forcing event, so the next pass counts the row as changed.
    valid: bool,
    stamp: u64,
}

/// Everything a frame shows besides row contents; a change in any of it bumps the rev.
#[derive(PartialEq)]
struct Sig {
    cursor: Cursor,
    modes: Modes,
    cols: u16,
    rows: u16,
    top: u64,
    lo: u64,
}

/// Per-client state: the rev that client holds and how much of the style table it has been sent.
/// Create one per attached client with [`Emulator::new_view`] (or [`View::with_have_rev`]).
#[derive(Clone, Debug, Default)]
pub struct View {
    /// The rev this client holds (0: nothing yet, so the next frame is a full snapshot).
    pub rev: u64,
    /// Style ids `1..=styles_sent` are defined at the client.
    styles_sent: usize,
    /// `(applied_seq, echo_seq)` of the last frame sent, so a seq-only change still produces one.
    seqs: (u32, u32),
}

impl View {
    /// Resume a client that says it holds `have_rev`. If that rev predates the last resize or
    /// screen switch (or is not a rev this emulator ever had), the first `frame()` is a snapshot.
    /// The first frame also re-sends the whole style table: ids are stable, so for a client that
    /// already has them it is a no-op.
    pub fn with_have_rev(have_rev: u64) -> View {
        View {
            rev: have_rev,
            styles_sent: 0,
            seqs: (0, 0),
        }
    }
}

/// A terminal: parses PTY output, keeps the screen, scrollback and modes, and produces frames.
pub struct Emulator {
    term: Term<Listener>,
    proc: Processor,
    shared: Arc<Mutex<Shared>>,
    osc: OscScan,
    tap: TapState,
    styles: StyleTable,
    scrollback: usize,
    /// Current rev. Starts at 1: a view at rev 0 holds nothing.
    rev: u64,
    /// Views older than this must take a snapshot (last resize / screen switch / style rebuild).
    floor_rev: u64,
    top_abs: u64,
    alt: bool,
    vis: Vec<VisRow>,
    /// Scrollback rows by the rev they were pushed at, oldest first: `(rev, rows)`. The row
    /// counts sum to the history size (trimmed after each pass), newest segment last.
    segments: VecDeque<(u64, u64)>,
    seg_total: u64,
    /// For the newest `min(rows, history)` scrollback rows, oldest first: the rev a client that
    /// was current at the last pass already holds that row at (see `stamp_pushed`).
    hist: VecDeque<u64>,
    title: Option<String>,
    sig: Sig,
    /// A resize is waiting for the next `finish`.
    relayout: bool,
    /// Last style-table rebuild, to rate-limit them.
    rebuilt_at: Option<Instant>,
    /// The last rebuild ran on the alternate screen, where the primary screen's scrollback is out
    /// of reach: its styles are missing, so `finish` rebuilds again when the primary returns.
    primary_unlisted: bool,
    /// When the pending run of synchronized updates began (each begin marker re-arms the
    /// parser's own timeout, so that alone never ends a run that keeps re-sending it).
    first_bsu: Option<Instant>,
}

impl Emulator {
    /// A blank `cols` x `rows` terminal keeping at most `scrollback` rows of history.
    /// Sizes outside 2..=[`MAX_COLS`] columns and 1..=[`MAX_ROWS`] rows are clamped into them.
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let (cols, rows) = (cols.clamp(2, MAX_COLS), rows.clamp(1, MAX_ROWS));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let config = Config {
            scrolling_history: scrollback,
            // Programs (neovim, Claude Code) negotiate the kitty keyboard protocol; `encode_key` honours it.
            kitty_keyboard: true,
            // A program on the server must not be able to write the user's clipboard.
            osc52: Osc52::Disabled,
            ..Config::default()
        };
        let term = Term::new(
            config,
            &Size {
                cols: cols.into(),
                rows: rows.into(),
            },
            Listener(shared.clone()),
        );
        let mut e = Emulator {
            term,
            proc: Processor::new(),
            shared,
            osc: OscScan::new(),
            tap: TapState::new(rows.into()),
            styles: StyleTable::default(),
            scrollback,
            rev: 1,
            floor_rev: 1,
            top_abs: 0,
            alt: false,
            vis: (0..rows)
                .map(|_| VisRow {
                    cells: Vec::new(),
                    valid: false,
                    stamp: 1,
                })
                .collect(),
            segments: VecDeque::new(),
            seg_total: 0,
            hist: VecDeque::new(),
            title: None,
            sig: Sig {
                cursor: Cursor::default(),
                modes: Modes::default(),
                cols: 0,
                rows: 0,
                top: 0,
                lo: 0,
            },
            relayout: false,
            rebuilt_at: None,
            primary_unlisted: false,
            first_bsu: None,
        };
        // The blank screen is what a client starts from: its rows are valid, not "new".
        let Emulator { term, vis, .. } = &mut e;
        for (l, v) in vis.iter_mut().enumerate() {
            v.cells = term.grid()[Line(l as i32)][..].to_vec();
            v.valid = true;
        }
        e.sig = e.compute_sig();
        e
    }

    /// Parse PTY output. Bumps the rev (once) when anything a frame shows changed.
    pub fn feed(&mut self, bytes: &[u8]) {
        let audit = self.audit_start();
        let mut found = Vec::new();
        self.osc.scan(bytes, &mut found);
        // A synchronized update (DEC 2026) whose program went quiet, or kept it going for too
        // long, is flushed by the next feed.
        if self.sync_end().is_some_and(|d| Instant::now() >= d) {
            self.run(|proc, tap| proc.stop_sync(tap));
            self.first_bsu = None;
        }
        // Notifications are queued where they sit in the stream, between the parser's own events.
        // The excess of an oversized OSC is left out: the parser would keep all of it.
        let mut from = 0;
        for (at, found) in found {
            match found {
                Found::Notify(event) => {
                    self.run(|proc, tap| proc.advance(tap, &bytes[from..at]));
                    from = at;
                    self.shared
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .events
                        .push(event);
                }
                Found::Cut(start) => {
                    self.run(|proc, tap| proc.advance(tap, &bytes[from..start]));
                    from = at;
                }
            }
        }
        self.run(|proc, tap| proc.advance(tap, &bytes[from..]));
        self.finish(audit);
    }

    /// When the emulator needs [`Emulator::flush_sync`] called if no output has arrived by then:
    /// the program's pending synchronized update (`CSI ? 2026 h` ... `l`) times out (while one is
    /// pending the emulator holds the bytes back and the screen does not change), or, with none
    /// pending, a style-table rebuild that the rate limit put off becomes due.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.sync_end().or_else(|| {
            if self.styles.full {
                self.rebuilt_at.map(|t| t + REBUILD_GAP)
            } else {
                None
            }
        })
    }

    /// When the pending synchronized update ends by itself: the parser's timeout, which every begin
    /// marker re-arms, but at the latest `SYNC_MAX` after the first of the run, so a program that
    /// keeps sending it cannot freeze the screen.
    fn sync_end(&self) -> Option<Instant> {
        let timeout = self.proc.sync_timeout().sync_timeout()?;
        Some(
            self.first_bsu
                .map_or(timeout, |t| timeout.min(t + SYNC_MAX)),
        )
    }

    /// End a pending synchronized update now and apply what it buffered, and do a style-table
    /// rebuild that is due. No-op if neither.
    pub fn flush_sync(&mut self) {
        let pending = self.proc.sync_timeout().sync_timeout().is_some();
        if pending || self.rebuild_due() {
            let audit = self.audit_start();
            if pending {
                self.run(|proc, tap| proc.stop_sync(tap));
            }
            self.finish(audit);
        }
    }

    /// The style table ran out of ids and the last rebuild is at least `REBUILD_GAP` ago.
    fn rebuild_due(&self) -> bool {
        self.styles.full && self.rebuilt_at.is_none_or(|t| t.elapsed() >= REBUILD_GAP)
    }

    /// Reflow to a new size, clamped like [`Emulator::new`]. Starts a new layout generation:
    /// numbering jumps, scrollback is renumbered, and every view's next frame is a full snapshot.
    /// Resizing reflows the whole scrollback, so the holder should debounce drag-resizes.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.clamp(2, MAX_COLS), rows.clamp(1, MAX_ROWS));
        if cols == self.cols() && rows == self.screen_rows() {
            return;
        }
        self.term.resize(Size {
            cols: cols.into(),
            rows: rows.into(),
        });
        self.tap.region = (0, i32::from(rows));
        self.jump_numbering();
        self.vis = (0..rows)
            .map(|_| VisRow {
                cells: Vec::new(),
                valid: false,
                stamp: 0,
            })
            .collect();
        self.relayout = true;
        self.finish(None);
    }

    /// Events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<EmuEvent> {
        let events = std::mem::take(
            &mut self
                .shared
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .events,
        );
        let mut seen_bell = false;
        events
            .into_iter()
            .filter(|e| !matches!(e, EmuEvent::Bell) || !std::mem::replace(&mut seen_bell, true))
            .collect()
    }

    /// Current rev: starts at 1, monotonic, bumps at most once per `feed`/`resize`, and only when
    /// something a frame shows changed (rows, cursor, modes, title, size, numbering).
    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// Live terminal modes in wire form.
    pub fn modes(&self) -> Modes {
        let m = *self.term.mode();
        let mut out = Modes::empty();
        for (from, to) in [
            (TermMode::APP_CURSOR, Modes::APP_CURSOR),
            (TermMode::APP_KEYPAD, Modes::APP_KEYPAD),
            (TermMode::BRACKETED_PASTE, Modes::BRACKETED_PASTE),
            (TermMode::MOUSE_REPORT_CLICK, Modes::MOUSE_CLICK),
            (TermMode::MOUSE_DRAG, Modes::MOUSE_DRAG),
            (TermMode::MOUSE_MOTION, Modes::MOUSE_MOTION),
            (TermMode::SGR_MOUSE, Modes::SGR_MOUSE),
            (TermMode::FOCUS_IN_OUT, Modes::FOCUS_IN_OUT),
            (TermMode::ALT_SCREEN, Modes::ALT_SCREEN),
            (TermMode::LINE_WRAP, Modes::LINE_WRAP),
            (TermMode::ALTERNATE_SCROLL, Modes::ALTERNATE_SCROLL),
            (TermMode::KITTY_KEYBOARD_PROTOCOL, Modes::KITTY_KEYBOARD),
            (TermMode::INSERT, Modes::INSERT),
            (TermMode::ORIGIN, Modes::ORIGIN),
        ] {
            if m.intersects(from) {
                out |= to;
            }
        }
        out
    }

    /// The window title as frames report it (`None` until the program sets one, or after a reset).
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// True while the alternate screen is active.
    pub fn is_alt_screen(&self) -> bool {
        self.alt
    }

    /// Width in columns.
    pub fn cols(&self) -> u16 {
        self.term.columns() as u16
    }

    /// Height in rows. (Not called `rows`: that name is the scrollback accessor below.)
    pub fn screen_rows(&self) -> u16 {
        self.term.screen_lines() as u16
    }

    /// Encode a key using the live modes (application cursor keys, kitty keyboard flags).
    /// `Mods::SUPER` only exists in the kitty encoding. Application keypad mode changes keypad
    /// keys, which `KeyCode` cannot express.
    pub fn encode_key(&self, key: Key) -> Vec<u8> {
        keys::encode_key(*self.term.mode(), key)
    }

    /// Paste text, wrapped in bracketed-paste markers when the program enabled them (then ESC and
    /// ETX characters are dropped from `text` so it cannot contain the end marker). Otherwise the
    /// bytes with each line break (`\r\n` or `\n`) turned into the `\r` the Enter key sends.
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        keys::encode_paste(*self.term.mode(), text)
    }

    /// Encode a mouse event for the active mouse mode and encoding (SGR / UTF-8 / X10). `None`
    /// when reporting is off or the event is not reported in this mode. For `Move`, `button`
    /// 0..=2 is the held button and larger means none. A wheel notch on the alt screen with
    /// `ALTERNATE_SCROLL` and no mouse mode becomes three arrow keys.
    pub fn encode_mouse(&self, ev: MouseEv) -> Option<Vec<u8>> {
        keys::encode_mouse(*self.term.mode(), ev)
    }

    /// The bytes to write for a focus change, if the program asked for focus reports.
    pub fn encode_focus(&self, focused: bool) -> Option<Vec<u8>> {
        keys::encode_focus(*self.term.mode(), focused)
    }

    /// The visible rows as plain text: trailing blanks trimmed per row, rows joined by `\n`, no
    /// trailing newline. Every row is present, empty ones as empty lines.
    /// [`crate::client::RowCache::text`] produces the identical string from frames.
    pub fn screen_text(&self) -> String {
        let grid = self.term.grid();
        let mut out = String::new();
        for l in 0..self.vis.len() {
            if l > 0 {
                out.push('\n');
            }
            let patch = render_row(&grid[Line(l as i32)][..], 0, &mut |_| 0);
            out.push_str(&crate::client::row_text(&patch));
        }
        out
    }

    /// The visible rows like [`Emulator::screen_text`] but with SGR sequences for colours and
    /// attributes, reset at the end of each row that used any. Blank cells keep their
    /// background colour; default-style trailing blanks are dropped.
    pub fn screen_ansi(&self) -> String {
        let grid = self.term.grid();
        (0..self.vis.len())
            .map(|l| ansi_row(&grid[Line(l as i32)][..]))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Rows with absolute numbers in `[from_abs, to_abs)`, clamped to what exists (scrollback
    /// from [`Emulator::history_lo_abs`], then the visible rows). Answers `TermUp::GetRows`; the
    /// holder should cap how many it asks for at once. Style ids are the emulator's own table
    /// (see [`Emulator::styles`]), the same ids frames use; a style the table has no id for
    /// shows as the default style.
    ///
    /// The answer stops before the row that would take it past [`MAX_ROWS_BYTES`] (always whole
    /// rows, at least one): a caller that gets fewer rows than it asked for re-requests from the
    /// last returned row + 1 (`RowCache::missing_after` finds that row).
    pub fn rows(&self, from_abs: u64, to_abs: u64) -> Vec<RowPatch> {
        let from = from_abs.max(self.history_lo_abs());
        let to = to_abs.min(self.top_abs + self.vis.len() as u64);
        let grid = self.term.grid();
        let mut out = Vec::new();
        let mut bytes = 0;
        for abs in from..to {
            let line = Line((abs as i64 - self.top_abs as i64) as i32);
            let patch = render_row(&grid[line][..], abs, &mut |s| self.styles.id(s));
            bytes += wire_size(&patch);
            if bytes > MAX_ROWS_BYTES && !out.is_empty() {
                break;
            }
            out.push(patch);
        }
        out
    }

    /// Oldest scrollback row still available. Equal to [`Emulator::top_abs`] on the alt screen.
    pub fn history_lo_abs(&self) -> u64 {
        if self.alt {
            self.top_abs
        } else {
            self.top_abs.saturating_sub(self.term.history_size() as u64)
        }
    }

    /// Absolute number of the first visible row.
    pub fn top_abs(&self) -> u64 {
        self.top_abs
    }

    /// Every style defined so far, `(id, style)` in id order. Row patches from [`Emulator::rows`]
    /// and frames refer to these ids (0 is the default style and is not listed). A holder that
    /// persists rows should persist this with them.
    pub fn styles(&self) -> Vec<(u16, Style)> {
        self.styles.since(0)
    }

    /// A fresh per-client state: its first frame is a full snapshot.
    pub fn new_view(&self) -> View {
        View::default()
    }

    /// The next frame for `view`, or `None` when it already holds the current rev (and the two
    /// seqs are what it was last sent). Sets `view.rev` to the frame's rev.
    ///
    /// A full snapshot (`base_rev == 0`: all non-blank visible rows and the whole style table)
    /// when the view holds nothing (`rev == 0`), predates the last resize / screen switch, or
    /// when a delta would carry more rows than the screen has. Otherwise a delta: the visible
    /// rows that changed after `view.rev`, plus the scrollback rows pushed after it that the view
    /// does not already hold (see the module docs). Cursor, modes and title are in every frame.
    /// Scrollback a snapshot does not carry is fetched with [`Emulator::rows`].
    pub fn frame(
        &mut self,
        view: &mut View,
        epoch: u64,
        applied_seq: u32,
        echo_seq: u32,
        timing: Option<Timing>,
    ) -> Option<Frame> {
        // A rebuild that the rate limit put off at the end of a feed happens now. It renumbers
        // everything, so this frame is a snapshot.
        if self.rebuild_due() {
            self.finish(None);
        }
        let seqs = (applied_seq, echo_seq);
        if view.rev == self.rev && view.seqs == seqs {
            return None;
        }
        let rows = self.vis.len();
        let mut delta = view.rev != 0 && view.rev >= self.floor_rev && view.rev <= self.rev;
        // Scrollback rows pushed since the view's rev that it does not already hold, as indexes
        // into `hist`.
        let mut send_hist: Vec<usize> = Vec::new();
        if delta {
            let pushed: u64 = self
                .segments
                .iter()
                .rev()
                .take_while(|s| s.0 > view.rev)
                .map(|s| s.1)
                .sum();
            if pushed > rows as u64 || pushed > self.hist.len() as u64 {
                delta = false;
            } else {
                send_hist.extend(
                    (self.hist.len() - pushed as usize..self.hist.len())
                        .filter(|&i| self.hist[i] > view.rev),
                );
                let changed = self.vis.iter().filter(|v| v.stamp > view.rev).count();
                delta = send_hist.len() + changed <= rows;
            }
        }

        let styles = &self.styles;
        let grid = self.term.grid();
        let mut row_patches = Vec::new();
        if delta {
            for i in send_hist {
                let back = self.hist.len() - i;
                row_patches.push(render_row(
                    &grid[Line(-(back as i32))][..],
                    self.top_abs - back as u64,
                    &mut |s| styles.id(s),
                ));
            }
            for (l, v) in self.vis.iter().enumerate() {
                if v.stamp > view.rev {
                    row_patches.push(render_row(
                        &grid[Line(l as i32)][..],
                        self.top_abs + l as u64,
                        &mut |s| styles.id(s),
                    ));
                }
            }
        } else {
            for l in 0..rows {
                let p = render_row(
                    &grid[Line(l as i32)][..],
                    self.top_abs + l as u64,
                    &mut |s| styles.id(s),
                );
                if !is_blank(&p) {
                    row_patches.push(p);
                }
            }
        }
        let new_styles = styles.since(if delta { view.styles_sent } else { 0 });
        view.styles_sent = styles.len();

        let frame = Frame {
            epoch,
            rev: self.rev,
            base_rev: if delta { view.rev } else { 0 },
            applied_seq,
            echo_seq,
            screen: if self.alt {
                Screen::Alt
            } else {
                Screen::Primary
            },
            cols: self.cols(),
            rows: self.screen_rows(),
            top_abs: self.top_abs,
            history_lo_abs: self.history_lo_abs(),
            cursor: self.cursor(),
            modes: self.modes(),
            title: self.title.clone(),
            new_styles,
            row_patches,
            timing,
        };
        view.rev = self.rev;
        view.seqs = seqs;
        Some(frame)
    }

    /// Run `f` with the parser and a `Tap` over this emulator's `Term`.
    fn run<R>(&mut self, f: impl FnOnce(&mut Processor, &mut Tap<'_>) -> R) -> R {
        let mut tap = Tap {
            term: &mut self.term,
            st: &mut self.tap,
            styles: &mut self.styles,
            shared: &self.shared,
        };
        f(&mut self.proc, &mut tap)
    }

    /// Cursor as frames report it: screen row/column, shape, visibility.
    pub fn cursor(&self) -> Cursor {
        let p = self.term.grid().cursor.point;
        let style = self.term.cursor_style();
        Cursor {
            row: p.line.0.max(0) as u16,
            col: p.column.0 as u16,
            shape: match style.shape {
                ACursorShape::Underline => CursorShape::Underline,
                ACursorShape::Beam => CursorShape::Bar,
                _ => CursorShape::Block,
            },
            visible: self.term.mode().contains(TermMode::SHOW_CURSOR)
                && style.shape != ACursorShape::Hidden,
        }
    }

    fn compute_sig(&self) -> Sig {
        Sig {
            cursor: self.cursor(),
            modes: self.modes(),
            cols: self.cols(),
            rows: self.screen_rows(),
            top: self.top_abs,
            lo: self.history_lo_abs(),
        }
    }

    /// A new layout generation numbers everything above every number issued so far: the rows
    /// currently visible, plus room for a full scrollback below the new `top_abs`.
    fn jump_numbering(&mut self) {
        self.top_abs += self.vis.len() as u64 + self.scrollback as u64 + 1;
    }

    /// Drop scrollback segments (oldest first) until they cover `history` rows.
    fn trim_segments(&mut self, history: u64) {
        while self.seg_total > history {
            let excess = self.seg_total - history;
            let Some(front) = self.segments.front_mut() else {
                break;
            };
            if front.1 > excess {
                front.1 -= excess;
                self.seg_total -= excess;
            } else {
                self.seg_total -= front.1;
                self.segments.pop_front();
            }
        }
    }

    /// Called before the shadow rows rotate, when `pushed` rows have just left the top of the
    /// primary screen. Pushed row `j` carries the number the visible row `j` had, and a client
    /// that was current holds shadow row `j` under it. If the row's final content is what the
    /// shadow says, such a client needs nothing and the row keeps the shadow's stamp; otherwise it
    /// is new as of `new_rev`. Only the newest `rows` pushed rows can ever go in a delta.
    fn stamp_pushed(&mut self, pushed: u64, new_rev: u64) {
        let rows = self.vis.len() as u64;
        let grid = self.term.grid();
        let history = grid.history_size() as u64;
        for j in pushed.saturating_sub(rows)..pushed {
            let back = pushed - j;
            let stamp = match self.vis.get(j as usize) {
                Some(old)
                    if old.valid
                        && back <= history
                        && grid[Line(-(back as i32))][..] == old.cells[..] =>
                {
                    old.stamp
                }
                _ => new_rev,
            };
            self.hist.push_back(stamp);
        }
        while self.hist.len() as u64 > rows {
            self.hist.pop_front();
        }
    }

    /// History size before a `feed`, for the push-accounting audit (primary screen only).
    fn audit_start(&self) -> Option<usize> {
        (cfg!(debug_assertions) && !self.alt).then(|| self.term.history_size())
    }

    /// Bookkeeping after anything that may have changed the screen: row numbering, shadow rows,
    /// stamps, the rev. `audit` is the history size before the feed (`None` skips the check).
    fn finish(&mut self, audit: Option<usize>) {
        let new_rev = self.rev + 1;
        let pushed = std::mem::take(&mut self.tap.pushed);
        let cleared = std::mem::take(&mut self.tap.history_cleared);
        let alt_now = self.term.mode().contains(TermMode::ALT_SCREEN);
        let switched = std::mem::take(&mut self.tap.alt_switched);
        let mut structural = std::mem::take(&mut self.relayout);
        let mut reseed = structural;

        // Cross-check the predicted push count against alacritty's own history size. Exact below
        // the cap and saturating at it; skipped when history was cleared or the feed ended on alt.
        if let (Some(before), false, false) = (audit, cleared, alt_now) {
            let expect = (before as u64 + pushed).min(self.scrollback as u64);
            debug_assert_eq!(
                self.term.history_size() as u64,
                expect,
                "scroll accounting drifted: history was {before}, predicted {pushed} pushed rows, cap {}",
                self.scrollback
            );
        }

        // Ending on the other screen is a switch. So is a visit to the primary screen while the
        // shadow rows were the alt screen's (it may have scrolled, and the rows are not the same).
        if alt_now != self.alt || (switched && self.alt) {
            self.alt = alt_now;
            structural = true;
        }
        if pushed > 0 {
            self.stamp_pushed(pushed, new_rev);
            self.top_abs += pushed;
            let k = (pushed as usize).min(self.vis.len());
            self.vis.rotate_left(k);
            for v in self.vis.iter_mut().rev().take(k) {
                v.valid = false;
            }
            self.segments.push_back((new_rev, pushed));
            self.seg_total += pushed;
        }
        // The start of an unbroken run of synchronized updates, for `sync_end`.
        self.first_bsu = self
            .proc
            .sync_timeout()
            .sync_timeout()
            .map(|_| self.first_bsu.unwrap_or_else(Instant::now));
        if self.rebuild_due() || (self.primary_unlisted && !self.alt) {
            self.rebuild_styles();
            self.jump_numbering();
            structural = true;
            reseed = true;
        }
        if reseed {
            // New numbers: nothing recorded under the old ones means anything now, and no view
            // older than this rev gets a delta.
            self.segments.clear();
            self.seg_total = 0;
            self.hist.clear();
        }
        if !self.alt {
            let history = self.term.history_size();
            self.trim_segments(history as u64);
            while self.hist.len() > history {
                self.hist.pop_front();
            }
        }

        if structural {
            self.floor_rev = new_rev;
            for v in &mut self.vis {
                v.valid = false;
            }
        }
        let grid = self.term.grid();
        let blank = Cell::default();
        let mut changed = structural;
        // ponytail: compares every visible cell on every feed (~25 us at 200x60 even for one
        // typed character); compare only the lines `Term::damage` reports if that ever shows up.
        for (l, v) in self.vis.iter_mut().enumerate() {
            let live = &grid[Line(l as i32)][..];
            if v.valid && v.cells.as_slice() == live {
                continue;
            }
            // A blank row that a scroll just brought in has a number no client has ever been
            // given (visible numbers only grow), so it needs no patch: stamp 0 is never newer
            // than any view. Stamp 0 is only right when that is true, so not after a new layout.
            let unseen_blank = !v.valid && !structural && live.iter().all(|c| *c == blank);
            v.cells.clear();
            v.cells.extend_from_slice(live);
            v.valid = true;
            if unseen_blank {
                v.stamp = 0;
            } else {
                v.stamp = new_rev;
                changed = true;
            }
        }

        if let Some(t) = self
            .shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .title
            .take()
        {
            if t != self.title {
                self.title = t;
                changed = true;
            }
        }
        let sig = self.compute_sig();
        if changed || pushed > 0 || sig != self.sig {
            self.rev = new_rev;
            self.sig = sig;
        }
    }

    /// Start the style table over from the styles actually in use, because it ran out of ids
    /// (65 535 distinct styles, e.g. a long truecolour gradient session). Ids change, so the
    /// caller starts a new layout generation. The cursor templates go in first (`Tap` interns a
    /// template once, when it changes), then the visible rows, then scrollback newest first until
    /// the table is full again: what does not fit is the oldest scrollback, which then shows in
    /// the default style. On the alternate screen the primary screen's scrollback cannot be
    /// read, so `finish` rebuilds once more when the primary screen returns.
    /// ponytail: one global table rebuilt at most once a second; a table per view, or one keyed
    /// by recency, would lift both limits.
    fn rebuild_styles(&mut self) {
        self.rebuilt_at = Some(Instant::now());
        self.primary_unlisted = self.alt;
        let t = &mut self.styles;
        t.clear();
        let grid = self.term.grid();
        for template in [&grid.cursor.template, &grid.saved_cursor.template] {
            let full = cell_style(template);
            t.intern(full);
            t.intern(Style {
                bg: full.bg,
                ..Style::default()
            });
        }
        let screen = 0..grid.screen_lines() as i32;
        let history = (grid.topmost_line().0..0).rev();
        for line in screen.chain(history) {
            if t.full {
                break;
            }
            let mut prev: Option<Style> = None;
            for cell in &grid[Line(line)][..] {
                let s = cell_style(cell);
                if prev != Some(s) {
                    t.intern(s);
                    prev = Some(s);
                }
            }
        }
        t.full = false;
    }

    /// Test hook: scrollback rows alacritty itself holds (the primary screen's, or 0 on alt).
    #[doc(hidden)]
    pub fn history_len(&self) -> usize {
        self.term.history_size()
    }

    /// Test hook: every style on screen or in scrollback already has an id, i.e. no row ever
    /// needs a style defined at render time.
    #[doc(hidden)]
    pub fn audit_styles(&self) -> Result<(), String> {
        let known: std::collections::HashSet<Style> =
            self.styles.since(0).into_iter().map(|(_, s)| s).collect();
        let grid = self.term.grid();
        for line in grid.topmost_line().0..=grid.bottommost_line().0 {
            for (col, cell) in grid[Line(line)][..].iter().enumerate() {
                let s = cell_style(cell);
                if s != Style::default() && !known.contains(&s) {
                    return Err(format!("line {line} col {col}: style {s:?} has no id"));
                }
            }
        }
        Ok(())
    }
}
