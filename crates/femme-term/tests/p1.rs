//! P1: the server-side emulator turns PTY bytes into frames, and a client rebuilding the screen
//! from those frames always ends up with exactly the server's screen.
//!
//! Every test funnels through [`Rig`]: one emulator, several clients (each a server-side `View`
//! plus a client-side `RowCache`) that sync at different paces, and [`check`] after every frame a
//! client receives. The fuzz and recording tests feed the same bytes in random chunk sizes.
//!
//! Measurements (`#[ignore]`d): `cargo test -p femme-term --release -- --ignored --nocapture`.
#![cfg(feature = "emulator")]

use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;
use femme_proto::term::{
    Attrs, Color, Cursor, CursorShape, Key, KeyCode, Modes, Mods, MouseEv, MouseKind, RowPatch,
    Screen, Style,
};
use femme_term::client::{row_text, ApplyError, RowCache};
use femme_term::{EmuEvent, Emulator, View};

const EPOCH: u64 = 9;

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

/// Seeded LCG: failures must reproduce from the seed alone.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// Inclusive range.
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

/// What a frame stream cost one client.
#[derive(Clone, Copy, Debug, Default)]
struct Stats {
    frames: u32,
    snapshots: u32,
    deltas: u32,
    /// Row patches received in total.
    patches: u64,
}

/// One attached client: the server-side view and the client-side cache.
struct Client {
    name: String,
    /// Syncs after every `every`-th feed.
    every: usize,
    view: View,
    cache: RowCache,
    st: Stats,
}

impl Client {
    fn new(name: &str, every: usize) -> Client {
        Client {
            name: name.to_string(),
            every,
            view: View::default(),
            cache: RowCache::new(),
            st: Stats::default(),
        }
    }

    /// Take the next frame (if any), apply it, and check the client agrees with the server.
    fn sync(&mut self, emu: &mut Emulator) {
        if let Some(f) = emu.frame(&mut self.view, EPOCH, 0, 0, None) {
            self.st.frames += 1;
            self.st.patches += f.row_patches.len() as u64;
            if f.base_rev == 0 {
                self.st.snapshots += 1;
                let ids: Vec<u16> = f.new_styles.iter().map(|s| s.0).collect();
                assert_eq!(
                    ids,
                    (1..=ids.len() as u16).collect::<Vec<_>>(),
                    "{}: a snapshot carries the whole style table",
                    self.name
                );
                for p in &f.row_patches {
                    assert!(
                        p.abs >= f.top_abs && p.abs < f.top_abs + u64::from(f.rows),
                        "{}: snapshot row {} outside the screen",
                        self.name,
                        p.abs
                    );
                }
            } else {
                self.st.deltas += 1;
                assert_eq!(
                    f.base_rev, self.cache.rev,
                    "{}: delta must continue the client's rev",
                    self.name
                );
            }
            assert!(
                f.row_patches.len() <= usize::from(f.rows),
                "{}: {} patches for {} rows",
                self.name,
                f.row_patches.len(),
                f.rows
            );
            self.cache
                .apply(&f)
                .unwrap_or_else(|e| panic!("{}: frame refused: {e}", self.name));
            for p in &f.row_patches {
                for (id, _) in &p.runs {
                    assert!(
                        *id == 0 || self.cache.style(*id) != Style::default(),
                        "{}: style {id} used but never defined",
                        self.name
                    );
                }
            }
        }
        check(emu, &self.cache, &self.name);
    }
}

/// Everything a client must agree with the server on, after every frame.
fn check(emu: &Emulator, c: &RowCache, who: &str) {
    assert_eq!(c.rev, emu.rev(), "{who}: rev");
    assert_eq!(
        c.text(),
        emu.screen_text(),
        "{who}: visible text differs (rev {}, top {})",
        emu.rev(),
        emu.top_abs()
    );
    assert_eq!(
        (c.cols, c.rows),
        (emu.cols(), emu.screen_rows()),
        "{who}: size"
    );
    assert_eq!(c.top_abs, emu.top_abs(), "{who}: top_abs");
    assert_eq!(
        c.history_lo_abs,
        emu.history_lo_abs(),
        "{who}: history_lo_abs"
    );
    assert_eq!(c.cursor, emu.cursor(), "{who}: cursor");
    assert_eq!(c.modes, emu.modes(), "{who}: modes");
    assert_eq!(c.title.as_deref(), emu.title(), "{who}: title");
    assert_eq!(
        c.screen == Screen::Alt,
        emu.is_alt_screen(),
        "{who}: screen"
    );
    let top = emu.top_abs();
    let server = emu.rows(top, top + u64::from(c.rows));
    assert_eq!(server.len(), usize::from(c.rows), "{who}: server rows");
    let styles = emu.styles();
    for (i, (srv, cl)) in server.iter().zip(c.visible()).enumerate() {
        let blank = RowPatch {
            abs: top + i as u64,
            ..RowPatch::default()
        };
        assert_eq!(
            srv,
            cl.unwrap_or(&blank),
            "{who}: visible row {i} (rev {})",
            emu.rev()
        );
        for (id, _) in &srv.runs {
            if *id != 0 {
                assert_eq!(
                    styles[usize::from(*id) - 1],
                    (*id, c.style(*id)),
                    "{who}: style {id}"
                );
            }
        }
    }
    if c.screen == Screen::Primary {
        for abs in emu.history_lo_abs().saturating_sub(4)..emu.history_lo_abs() {
            assert!(
                c.row(abs).is_none(),
                "{who}: row {abs} is below history_lo_abs but still cached"
            );
        }
    }
}

/// What the desktop does lazily: fetch scrollback rows the cache lacks (`TermUp::GetRows`).
fn fill_history(emu: &Emulator, c: &mut RowCache) {
    let (lo, top) = (emu.history_lo_abs(), emu.top_abs());
    let mut abs = lo;
    while abs < top {
        if c.row(abs).is_some() {
            abs += 1;
            continue;
        }
        let start = abs;
        while abs < top && c.row(abs).is_none() {
            abs += 1;
        }
        let rows = emu.rows(start, abs);
        assert_eq!(
            rows.len() as u64,
            abs - start,
            "server cannot supply rows {start}..{abs}"
        );
        c.add_rows(rows);
    }
}

/// Scrollback rows are immutable: whatever the client holds must be what the server has now.
fn check_history(emu: &Emulator, c: &RowCache, who: &str) {
    for abs in emu.history_lo_abs()..emu.top_abs() {
        let mine = c
            .row(abs)
            .unwrap_or_else(|| panic!("{who}: history row {abs} missing after fill"));
        assert_eq!(mine, &emu.rows(abs, abs + 1)[0], "{who}: history row {abs}");
    }
}

/// One emulator and its clients: `live` syncs after every feed, `lag` after every fourth.
struct Rig {
    emu: Emulator,
    clients: Vec<Client>,
    feeds: usize,
    /// Audit "every style already has an id" after this many feeds (it walks all scrollback).
    audit_every: usize,
}

impl Rig {
    fn new(cols: u16, rows: u16, scrollback: usize) -> Rig {
        let mut r = Rig {
            emu: Emulator::new(cols, rows, scrollback),
            clients: vec![Client::new("live", 1), Client::new("lag", 4)],
            feeds: 0,
            audit_every: 1,
        };
        r.sync(true);
        r
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.emu.feed(bytes);
        self.feeds += 1;
        self.sync(false);
        if self.feeds.is_multiple_of(self.audit_every) {
            self.emu.audit_styles().unwrap();
        }
    }

    fn feed_str(&mut self, s: &str) {
        self.feed(s.as_bytes());
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.emu.resize(cols, rows);
        self.feeds += 1;
        self.sync(false);
    }

    fn sync(&mut self, force: bool) {
        for c in &mut self.clients {
            if force || self.feeds.is_multiple_of(c.every) {
                c.sync(&mut self.emu);
            }
        }
    }

    /// Everyone catches up, fetches missing scrollback, and agrees on all of it.
    fn done(&mut self) {
        self.sync(true);
        for c in &mut self.clients {
            fill_history(&self.emu, &mut c.cache);
            check_history(&self.emu, &c.cache, &c.name);
        }
        self.emu.audit_styles().unwrap();
    }

    /// Text of rows `[from, to)` by absolute number.
    fn texts(&self, from: u64, to: u64) -> Vec<String> {
        self.emu.rows(from, to).iter().map(row_text).collect()
    }

    /// The visible rows as text.
    fn screen(&self) -> Vec<String> {
        self.emu
            .screen_text()
            .split('\n')
            .map(str::to_string)
            .collect()
    }
}

fn rig(cols: u16, rows: u16, scrollback: usize) -> Rig {
    Rig::new(cols, rows, scrollback)
}

fn lines(prefix: &str, range: std::ops::Range<usize>) -> Vec<String> {
    range.map(|i| format!("{prefix}{i}")).collect()
}

/// The resolved style of a cell: feed `s`, then `X` at the cursor, and read it back.
fn style_of(s: &str) -> Style {
    let mut e = Emulator::new(20, 3, 10);
    e.feed(format!("{s}X").as_bytes());
    let row = e.rows(0, 1).remove(0);
    let (id, text) = row.runs.last().expect("X was drawn").clone();
    assert!(text.ends_with('X'));
    if id == 0 {
        Style::default()
    } else {
        e.styles()[usize::from(id) - 1].1
    }
}

fn events(e: &mut Emulator) -> Vec<EmuEvent> {
    e.take_events()
}

fn pty_writes(evs: &[EmuEvent]) -> Vec<String> {
    evs.iter()
        .filter_map(|e| match e {
            EmuEvent::PtyWrite(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// deterministic scenarios
// ---------------------------------------------------------------------------------------------

#[test]
fn emulator_is_send_and_sync() {
    fn both<T: Send + Sync>() {}
    both::<Emulator>();
    both::<View>();
    both::<RowCache>();
}

#[test]
fn scrolling_numbers_rows_absolutely() {
    let mut r = rig(20, 5, 100);
    for i in 0..10 {
        r.feed_str(&format!("l{i}\r\n"));
    }
    // 10 lines + the empty one under the cursor = 11 lines produced; the screen shows the last 5.
    assert_eq!(r.emu.top_abs(), 6);
    assert_eq!(r.emu.history_lo_abs(), 0);
    assert_eq!(r.emu.history_len(), 6);
    let mut want = lines("l", 0..10);
    want.push(String::new());
    assert_eq!(r.texts(0, 11), want);
    assert_eq!(r.screen(), want[6..]);
    assert_eq!(
        r.emu.cursor(),
        Cursor {
            row: 4,
            col: 0,
            shape: CursorShape::Block,
            visible: true
        }
    );
    r.done();
    // The client numbers them the same way.
    for abs in 0..10 {
        assert_eq!(
            row_text(r.clients[0].cache.row(abs).expect("row cached")),
            format!("l{abs}")
        );
    }
}

#[test]
fn rows_clamps_to_what_exists() {
    let mut r = rig(10, 3, 10);
    r.feed_str("a\r\nb\r\nc\r\nd\r\n");
    assert_eq!((r.emu.top_abs(), r.emu.history_lo_abs()), (2, 0));
    assert_eq!(r.texts(0, 100), ["a", "b", "c", "d", ""]);
    assert_eq!(r.texts(1, 3), ["b", "c"]);
    assert!(r.emu.rows(3, 3).is_empty());
    assert!(r.emu.rows(4, 2).is_empty());
    assert!(r.emu.rows(100, 200).is_empty());
    assert_eq!(r.emu.rows(4, 100).len(), 1);
    r.done();
}

#[test]
fn soft_wrap_marks_the_row_that_continues() {
    let mut r = rig(10, 4, 10);
    r.feed_str("abcdefghijklmnopqrstuvwxyz");
    let rows = r.emu.rows(0, 3);
    assert_eq!(
        rows.iter().map(row_text).collect::<Vec<_>>(),
        ["abcdefghij", "klmnopqrst", "uvwxyz"]
    );
    assert_eq!(
        rows.iter().map(|p| p.wrapped).collect::<Vec<_>>(),
        [true, true, false]
    );

    // Exactly full: the wrap only happens when the next character arrives.
    let mut r = rig(10, 4, 10);
    r.feed_str("abcdefghij");
    assert_eq!((r.emu.cursor().row, r.emu.cursor().col), (0, 9));
    assert!(!r.emu.rows(0, 1)[0].wrapped);
    r.feed_str("\r\nX");
    assert!(
        !r.emu.rows(0, 1)[0].wrapped,
        "a hard newline is not a soft wrap"
    );
    let mut r2 = rig(10, 4, 10);
    r2.feed_str("abcdefghijk");
    assert!(r2.emu.rows(0, 1)[0].wrapped);
    assert_eq!(r2.screen()[1], "k");
    r.done();
    r2.done();
}

#[test]
fn wide_characters_including_the_last_column() {
    // 5 columns: "ab" + wide + wide. The second wide character does not fit in column 4.
    let mut r = rig(5, 3, 10);
    r.feed_str("ab\u{3042}\u{3044}");
    let rows = r.emu.rows(0, 2);
    assert_eq!(
        rows.iter().map(row_text).collect::<Vec<_>>(),
        ["ab\u{3042}", "\u{3044}"]
    );
    assert_eq!(
        (rows[0].wide_cols.clone(), rows[1].wide_cols.clone()),
        (vec![3], vec![1])
    );
    assert!(
        rows[0].wrapped,
        "the blank left in column 4 belongs to the wrapped row"
    );

    // A wide character that fits exactly in the last two columns does not wrap.
    let mut r2 = rig(5, 3, 10);
    r2.feed_str("abc\u{3042}");
    let rows = r2.emu.rows(0, 2);
    assert_eq!(rows[0].wide_cols, [4]);
    assert_eq!(row_text(&rows[0]), "abc\u{3042}");
    assert!(!rows[0].wrapped);
    assert_eq!(row_text(&rows[1]), "");
    // Narrow characters filling the row, then a wide one: the wide one starts the next row.
    r2.feed_str("\r\n12345\u{3042}");
    assert_eq!(r2.screen()[1], "12345");
    assert_eq!(r2.screen()[2], "\u{3042}");
    // Emoji are wide too.
    r2.feed_str("\r\n\u{1F600}x");
    assert_eq!(r2.screen(), ["12345", "\u{3042}", "\u{1F600}x"]);
    r.done();
    r2.done();
}

#[test]
fn overwriting_half_of_a_wide_character_blanks_the_other_half() {
    let mut r = rig(10, 3, 10);
    r.feed_str("\u{3042}\x1b[1;1Hx");
    assert_eq!(r.screen()[0], "x");
    assert!(r.emu.rows(0, 1)[0].wide_cols.is_empty());
    r.feed_str("\x1b[1;1H\u{3042}\x1b[1;2Hy");
    assert_eq!(r.screen()[0], " y");
    assert!(r.emu.rows(0, 1)[0].wide_cols.is_empty());
    r.done();
}

#[test]
fn combining_marks_stay_with_their_base() {
    let mut r = rig(10, 3, 10);
    r.feed_str("e\u{301}x\u{200d}y");
    assert_eq!(r.screen()[0], "e\u{301}x\u{200d}y");
    assert_eq!(
        r.emu.cursor().col,
        3,
        "e+accent is one column, x+zwj one, y one"
    );
    r.feed_str("\r\n\u{1F468}\u{200d}\u{1F469}z");
    r.done();
}

#[test]
fn sgr_styles_are_resolved() {
    let st = |fg, bg, attrs| Style {
        fg,
        bg,
        underline: Color::Default,
        attrs,
    };
    let attr = |a| st(Color::Default, Color::Default, a);
    let cases: Vec<(&str, Style)> = vec![
        ("", Style::default()),
        ("\x1b[1m", attr(Attrs::BOLD)),
        ("\x1b[2m", attr(Attrs::DIM)),
        ("\x1b[3m", attr(Attrs::ITALIC)),
        ("\x1b[4m", attr(Attrs::UNDERLINE)),
        ("\x1b[4:2m", attr(Attrs::DOUBLE_UNDERLINE)),
        ("\x1b[21m", Style::default()), // alacritty reads 21 as "bold off", not double underline
        ("\x1b[4:3m", attr(Attrs::CURLY_UNDERLINE)),
        ("\x1b[4:4m", attr(Attrs::DOTTED_UNDERLINE)),
        ("\x1b[4:5m", attr(Attrs::DASHED_UNDERLINE)),
        ("\x1b[7m", attr(Attrs::INVERSE)),
        ("\x1b[8m", attr(Attrs::HIDDEN)),
        ("\x1b[9m", attr(Attrs::STRIKETHROUGH)),
        ("\x1b[5m", Style::default()), // blink is not supported by alacritty_terminal
        (
            "\x1b[31m",
            st(Color::Indexed(1), Color::Default, Attrs::empty()),
        ),
        (
            "\x1b[37m",
            st(Color::Indexed(7), Color::Default, Attrs::empty()),
        ),
        (
            "\x1b[91m",
            st(Color::Indexed(9), Color::Default, Attrs::empty()),
        ),
        (
            "\x1b[97m",
            st(Color::Indexed(15), Color::Default, Attrs::empty()),
        ),
        (
            "\x1b[41m",
            st(Color::Default, Color::Indexed(1), Attrs::empty()),
        ),
        (
            "\x1b[107m",
            st(Color::Default, Color::Indexed(15), Attrs::empty()),
        ),
        (
            "\x1b[38;5;200m",
            st(Color::Indexed(200), Color::Default, Attrs::empty()),
        ),
        (
            "\x1b[48;5;17m",
            st(Color::Default, Color::Indexed(17), Attrs::empty()),
        ),
        (
            "\x1b[38;2;1;2;3m",
            st(Color::Rgb(1, 2, 3), Color::Default, Attrs::empty()),
        ),
        (
            "\x1b[48;2;9;8;7m",
            st(Color::Default, Color::Rgb(9, 8, 7), Attrs::empty()),
        ),
        (
            "\x1b[38:2::10:20:30m",
            st(Color::Rgb(10, 20, 30), Color::Default, Attrs::empty()),
        ),
        (
            "\x1b[1;31;44m",
            st(Color::Indexed(1), Color::Indexed(4), Attrs::BOLD),
        ),
        (
            "\x1b[1;3;4;9;7m",
            attr(
                Attrs::BOLD
                    | Attrs::ITALIC
                    | Attrs::UNDERLINE
                    | Attrs::STRIKETHROUGH
                    | Attrs::INVERSE,
            ),
        ),
        (
            "\x1b[58;2;4;5;6m\x1b[4m",
            Style {
                underline: Color::Rgb(4, 5, 6),
                attrs: Attrs::UNDERLINE,
                ..Style::default()
            },
        ),
        (
            "\x1b[58;5;99m\x1b[4:3m",
            Style {
                underline: Color::Indexed(99),
                attrs: Attrs::CURLY_UNDERLINE,
                ..Style::default()
            },
        ),
        // resets
        ("\x1b[1m\x1b[22m", Style::default()),
        ("\x1b[1;2m\x1b[22m", Style::default()),
        ("\x1b[3m\x1b[23m", Style::default()),
        ("\x1b[4m\x1b[24m", Style::default()),
        ("\x1b[7m\x1b[27m", Style::default()),
        ("\x1b[8m\x1b[28m", Style::default()),
        ("\x1b[9m\x1b[29m", Style::default()),
        ("\x1b[31m\x1b[39m", Style::default()),
        ("\x1b[41m\x1b[49m", Style::default()),
        ("\x1b[1;31;44m\x1b[0m", Style::default()),
        ("\x1b[1;31;44m\x1b[m", Style::default()),
        ("\x1b[1;31;44m\x1bc", Style::default()),
    ];
    for (input, want) in cases {
        assert_eq!(style_of(input), want, "input {input:?}");
    }
}

#[test]
fn erased_cells_keep_the_background_colour() {
    // BCE: the colour of the erased cells is a style of its own, and travels like any other.
    let mut r = rig(10, 3, 10);
    r.feed_str("\x1b[44m\x1b[2K\x1b[0mabc");
    let row = &r.emu.rows(0, 1)[0];
    assert_eq!(row_text(row), "abc", "text trims blanks");
    assert_eq!(
        row.runs.len(),
        2,
        "but the blue blanks are a run of their own: {:?}",
        row.runs
    );
    assert_eq!(row.runs[1].1, " ".repeat(7));
    assert_ne!(row.runs[1].0, 0);
    r.feed_str("\x1b[2;1H\x1b[41m\x1b[J\x1b[0m");
    r.done();
}

#[test]
fn scroll_region_below_the_top_does_not_renumber() {
    let mut r = rig(10, 6, 10);
    r.feed_str("\x1b[1;1Hr0\x1b[2;1Hr1\x1b[3;1Hr2\x1b[4;1Hr3\x1b[5;1Hr4\x1b[6;1Hr5");
    assert_eq!(r.screen(), ["r0", "r1", "r2", "r3", "r4", "r5"]);
    r.feed_str("\x1b[2;4r"); // rows 1..=3 (0-based)
    r.feed_str("\x1b[4;1H\n");
    assert_eq!(r.screen(), ["r0", "r2", "r3", "", "r4", "r5"]);
    r.feed_str("\x1b[4;1H\n\n");
    assert_eq!(r.screen(), ["r0", "", "", "", "r4", "r5"]);
    assert_eq!(r.emu.top_abs(), 0, "nothing left the screen");
    assert_eq!(r.emu.history_len(), 0);
    // reverse index at the top of the region scrolls the region down, still nothing pushed
    r.feed_str("\x1b[2;1Hxx\x1b[2;1H\x1bM");
    assert_eq!(r.screen(), ["r0", "", "xx", "", "r4", "r5"]);
    assert_eq!(r.emu.top_abs(), 0);
    r.feed_str("\x1b[r");
    r.done();
}

#[test]
fn scroll_region_starting_at_row_zero_pushes_rows_into_history() {
    let mut r = rig(10, 6, 10);
    r.feed_str("\x1b[1;1Hr0\x1b[2;1Hr1\x1b[3;1Hr2\x1b[4;1Hr3\x1b[5;1Hr4\x1b[6;1Hr5");
    r.feed_str("\x1b[1;4r\x1b[4;1H\n");
    // alacritty (unlike xterm) puts the row that leaves a top-anchored region into scrollback
    // whatever the region's bottom; rows below the region stay where they are.
    assert_eq!(r.screen(), ["r1", "r2", "r3", "", "r4", "r5"]);
    assert_eq!(r.emu.top_abs(), 1);
    assert_eq!(r.emu.history_len(), 1);
    assert_eq!(r.texts(0, 1), ["r0"]);
    r.feed_str("\x1b[4;1H\n\n\n");
    assert_eq!(r.emu.top_abs(), 4);
    assert_eq!(r.texts(0, 4), ["r0", "r1", "r2", "r3"]);
    assert_eq!(r.screen(), ["", "", "", "", "r4", "r5"]);
    r.done();
}

#[test]
fn scroll_up_and_down_csi() {
    let mut r = rig(10, 3, 10);
    r.feed_str("\x1b[1;1Ha\x1b[2;1Hb\x1b[3;1Hc");
    r.feed_str("\x1b[S");
    assert_eq!(r.screen(), ["b", "c", ""]);
    assert_eq!(r.texts(0, 1), ["a"]);
    assert_eq!(r.emu.top_abs(), 1);
    r.feed_str("\x1b[T");
    assert_eq!(r.screen(), ["", "b", "c"]);
    assert_eq!(
        r.emu.top_abs(),
        1,
        "scrolling down pulls nothing back out of history"
    );
    r.feed_str("\x1b[5S");
    assert_eq!(
        r.emu.top_abs(),
        4,
        "no more than a screenful leaves per call"
    );
    r.done();
}

#[test]
fn alt_screen_both_ways() {
    let mut r = rig(12, 4, 10);
    r.feed_str("one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n");
    let (top, lo) = (r.emu.top_abs(), r.emu.history_lo_abs());
    assert_eq!((top, lo), (2, 0));
    let snaps = r.clients[0].st.snapshots;
    r.feed_str("\x1b[?1049h\x1b[H\x1b[2Jalt!");
    assert!(r.emu.is_alt_screen());
    assert!(r.emu.modes().contains(Modes::ALT_SCREEN));
    assert_eq!(r.screen(), ["alt!", "", "", ""]);
    assert_eq!(
        (r.emu.top_abs(), r.emu.history_lo_abs()),
        (top, top),
        "alt rows reuse the numbers; no scrollback is readable"
    );
    assert!(r.emu.rows(0, top).is_empty());
    assert_eq!(
        r.clients[0].st.snapshots,
        snaps + 1,
        "entering the alt screen sends a snapshot"
    );
    // scrolling on the alt screen pushes nothing
    for i in 0..20 {
        r.feed_str(&format!("\x1b[4;1H\nalt{i}"));
    }
    assert_eq!((r.emu.top_abs(), r.emu.history_len()), (top, 0));
    r.feed_str("\x1b[?1049l");
    assert!(!r.emu.is_alt_screen());
    assert_eq!(r.screen(), ["three", "four", "five", ""]);
    assert_eq!((r.emu.top_abs(), r.emu.history_lo_abs()), (top, lo));
    assert_eq!(r.texts(0, 2), ["one", "two"]);
    assert_eq!(
        r.clients[0].st.snapshots,
        snaps + 2,
        "and so does leaving it"
    );
    // primary keeps working afterwards
    r.feed_str("six\r\n");
    assert_eq!(r.emu.top_abs(), top + 1);
    r.done();
}

#[test]
fn alt_screen_entered_and_left_within_one_feed_changes_nothing() {
    let mut r = rig(12, 4, 10);
    r.feed_str("keep\r\nme");
    let rev = r.emu.rev();
    r.feed_str("\x1b[?1049hdraw something\x1b[?1049l");
    assert_eq!(r.screen(), ["keep", "me", "", ""]);
    assert_eq!(
        r.emu.rev(),
        rev,
        "the cursor is restored too, so there is nothing to report"
    );
    r.done();
}

#[test]
fn clearing_scrollback_and_screen() {
    let mut r = rig(10, 4, 50);
    for i in 0..10 {
        r.feed_str(&format!("n{i}\r\n"));
    }
    assert_eq!((r.emu.top_abs(), r.emu.history_len()), (7, 7));
    r.feed_str("\x1b[3J");
    assert_eq!(
        (r.emu.top_abs(), r.emu.history_lo_abs(), r.emu.history_len()),
        (7, 7, 0)
    );
    assert_eq!(
        r.screen(),
        ["n7", "n8", "n9", ""],
        "ED 3 leaves the screen alone"
    );
    assert!(r.emu.rows(0, 7).is_empty());
    // ED 2 on the primary screen scrolls what was on it into scrollback (alacritty's choice)
    r.feed_str("\x1b[2J");
    assert_eq!(r.screen(), ["", "", "", ""]);
    assert_eq!(r.emu.top_abs(), 10);
    assert_eq!(r.texts(7, 10), ["n7", "n8", "n9"]);
    // ... but only down to the last used row, and never when there is nothing on it
    r.feed_str("\x1b[2J\x1b[2J");
    assert_eq!(r.emu.top_abs(), 10);
    r.done();

    // The one quirk: an empty screen with no scrollback at all still gets pushed once.
    let mut q = rig(10, 4, 50);
    q.feed_str("\x1b[2J");
    assert_eq!((q.emu.top_abs(), q.emu.history_len()), (1, 1));
    q.feed_str("\x1b[2J");
    assert_eq!((q.emu.top_abs(), q.emu.history_len()), (1, 1));
    q.done();
}

#[test]
fn full_reset_clears_everything_but_keeps_numbers_moving_forward() {
    let mut r = rig(10, 4, 50);
    for i in 0..10 {
        r.feed_str(&format!("n{i}\r\n"));
    }
    r.feed_str("\x1b]0;title\x07\x1b[1;31m");
    let top = r.emu.top_abs();
    r.feed_str("\x1bc");
    assert_eq!(r.screen(), ["", "", "", ""]);
    assert!(r.emu.top_abs() >= top, "numbers never go backwards");
    assert_eq!(r.emu.history_lo_abs(), r.emu.top_abs());
    r.feed_str("after\r\n");
    assert_eq!(style_of("\x1b[1;31m\x1bc"), Style::default());
    r.done();
}

#[test]
fn cursor_movement_and_overwrite() {
    let mut r = rig(20, 5, 10);
    r.feed_str("hello world");
    assert_eq!((r.emu.cursor().row, r.emu.cursor().col), (0, 11));
    r.feed_str("\x1b[6DX");
    assert_eq!(r.screen()[0], "helloXworld");
    r.feed_str("\x1b[2;1H\x1b[3Cabc");
    assert_eq!(r.screen()[1], "   abc");
    r.feed_str("\x1b[3;5H");
    assert_eq!((r.emu.cursor().row, r.emu.cursor().col), (2, 4));
    r.feed_str("\x1b[2A");
    assert_eq!(r.emu.cursor().row, 0);
    r.feed_str("\x1b[B\x1b[10G");
    assert_eq!((r.emu.cursor().row, r.emu.cursor().col), (1, 9));
    r.feed_str("\x1b[4d");
    assert_eq!(r.emu.cursor().row, 3);
    r.feed_str("\r\x08");
    assert_eq!(r.emu.cursor().col, 0);
    r.feed_str("\x1b[99;99H");
    assert_eq!(
        (r.emu.cursor().row, r.emu.cursor().col),
        (4, 19),
        "positions clamp to the screen"
    );
    r.feed_str("\x1b[1;1H\x1b[2E");
    assert_eq!((r.emu.cursor().row, r.emu.cursor().col), (2, 0));
    r.feed_str("\x1b[1F");
    assert_eq!((r.emu.cursor().row, r.emu.cursor().col), (1, 0));
    // save and restore
    r.feed_str("\x1b[3;3H\x1b7\x1b[1;1H\x1b8");
    assert_eq!((r.emu.cursor().row, r.emu.cursor().col), (2, 2));
    r.done();
}

#[test]
fn cursor_shape_and_visibility() {
    let mut r = rig(10, 3, 10);
    assert_eq!(
        r.emu.cursor(),
        Cursor {
            row: 0,
            col: 0,
            shape: CursorShape::Block,
            visible: true
        }
    );
    r.feed_str("\x1b[?25l");
    assert!(!r.emu.cursor().visible);
    r.feed_str("\x1b[?25h\x1b[4 q");
    assert_eq!(
        (r.emu.cursor().shape, r.emu.cursor().visible),
        (CursorShape::Underline, true)
    );
    r.feed_str("\x1b[6 q");
    assert_eq!(r.emu.cursor().shape, CursorShape::Bar);
    r.feed_str("\x1b[2 q");
    assert_eq!(r.emu.cursor().shape, CursorShape::Block);
    r.done();
}

#[test]
fn insert_delete_lines_and_characters() {
    let mut r = rig(10, 4, 10);
    r.feed_str("aaaa\r\nbbbb\r\ncccc\r\ndddd");
    r.feed_str("\x1b[2;1H\x1b[L");
    assert_eq!(r.screen(), ["aaaa", "", "bbbb", "cccc"]);
    r.feed_str("\x1b[M");
    assert_eq!(r.screen(), ["aaaa", "bbbb", "cccc", ""]);
    assert_eq!(
        r.emu.top_abs(),
        0,
        "inserting and deleting below row 0 pushes nothing"
    );
    // deleting lines at the very top is a scroll: they go to scrollback
    r.feed_str("\x1b[1;1H\x1b[2M");
    assert_eq!(r.screen(), ["cccc", "", "", ""]);
    assert_eq!(r.emu.top_abs(), 2);
    assert_eq!(r.texts(0, 2), ["aaaa", "bbbb"]);
    r.done();

    let mut r = rig(10, 3, 10);
    r.feed_str("abcdef");
    r.feed_str("\x1b[1;2H\x1b[2@");
    assert_eq!(r.screen()[0], "a  bcdef");
    r.feed_str("\x1b[1;2H\x1b[2P");
    assert_eq!(r.screen()[0], "abcdef");
    r.feed_str("\x1b[1;2H\x1b[2X");
    assert_eq!(r.screen()[0], "a  def");
    r.feed_str("\x1b[1;4H\x1b[K");
    assert_eq!(r.screen()[0], "a");
    r.feed_str("\x1b[1;1Hxyz\x1b[1;2H\x1b[1K");
    assert_eq!(r.screen()[0], "  z");
    r.feed_str("\x1b[2K");
    assert_eq!(r.screen()[0], "");
    r.done();
}

#[test]
fn tabs_charsets_and_insert_mode() {
    let mut r = rig(20, 3, 10);
    r.feed_str("a\tb\tc");
    assert_eq!(r.screen()[0], "a       b       c");
    r.feed_str("\r\n\x1b(0lqqk\x1b(B");
    assert_eq!(r.screen()[1], "\u{250c}\u{2500}\u{2500}\u{2510}");
    r.feed_str("\x1b[3;1Hworld\x1b[3;1H\x1b[4hhello \x1b[4l");
    assert_eq!(r.screen()[2], "hello world");
    assert!(!r.emu.modes().contains(Modes::INSERT));
    r.done();
}

#[test]
fn resize_reflows_and_renumbers() {
    let mut r = rig(10, 3, 100);
    r.feed_str("abcdefghijklmnop");
    assert_eq!(r.screen(), ["abcdefghij", "klmnop", ""]);
    let (top, rev) = (r.emu.top_abs(), r.emu.rev());
    let snaps = r.clients[0].st.snapshots;
    r.resize(20, 5);
    assert_eq!(r.emu.rev(), rev + 1, "a resize is one rev");
    assert_eq!((r.emu.cols(), r.emu.screen_rows()), (20, 5));
    assert_eq!(r.screen(), ["abcdefghijklmnop", "", "", "", ""]);
    assert!(
        r.emu.top_abs() > top + 3,
        "a new layout generation numbers above every old row"
    );
    assert_eq!(
        r.clients[0].st.snapshots,
        snaps + 1,
        "and every client takes a snapshot"
    );
    // Where alacritty puts the reflowed rows (screen or scrollback) is its business; the content
    // and the client agreeing with the server are ours.
    let all = |r: &Rig| {
        let mut rows = r.texts(
            r.emu.history_lo_abs(),
            r.emu.top_abs() + u64::from(r.emu.screen_rows()),
        );
        while rows.last().is_some_and(String::is_empty) {
            rows.pop();
        }
        rows
    };
    assert_eq!(all(&r), ["abcdefghijklmnop"]);
    r.resize(5, 5);
    assert_eq!(all(&r), ["abcde", "fghij", "klmno", "p"]);
    r.resize(5, 2);
    assert_eq!(all(&r), ["abcde", "fghij", "klmno", "p"]);
    r.feed_str("\r\nmore\r\nlines\r\n");
    r.done();
    // same size: nothing happens
    let rev = r.emu.rev();
    r.emu.resize(5, 2);
    assert_eq!(r.emu.rev(), rev);
    // nonsense sizes are raised to the minimum instead of panicking
    r.emu.resize(0, 0);
    assert_eq!((r.emu.cols(), r.emu.screen_rows()), (2, 1));
    let tiny = Emulator::new(0, 0, 0);
    assert_eq!((tiny.cols(), tiny.screen_rows()), (2, 1));
}

#[test]
fn resize_while_on_the_alt_screen() {
    let mut r = rig(10, 4, 20);
    r.feed_str("p1\r\np2\r\np3\r\np4\r\np5\r\n");
    r.feed_str("\x1b[?1049h\x1b[Halt");
    r.resize(7, 3);
    assert_eq!((r.emu.cols(), r.emu.screen_rows()), (7, 3));
    r.feed_str("\x1b[?1049l");
    assert!(!r.emu.is_alt_screen());
    r.feed_str("tail\r\n");
    r.done();
}

#[test]
fn title_and_events() {
    let mut e = Emulator::new(20, 3, 10);
    e.feed(b"\x1b]0;hello\x07");
    assert_eq!(events(&mut e), [EmuEvent::Title("hello".into())]);
    assert_eq!(e.title(), Some("hello"));
    e.feed(b"\x1b]2;split\x1b\\");
    assert_eq!(events(&mut e), [EmuEvent::Title("split".into())]);
    // an OSC cut across feeds
    e.feed(b"\x1b]0;ab");
    assert!(events(&mut e).is_empty());
    e.feed(b"cd\x07");
    assert_eq!(events(&mut e), [EmuEvent::Title("abcd".into())]);
    assert_eq!(e.title(), Some("abcd"));
    // push, change, pop
    e.feed(b"\x1b[22;0t\x1b]0;temp\x07");
    assert_eq!(e.title(), Some("temp"));
    e.feed(b"\x1b[23;0t");
    assert_eq!(e.title(), Some("abcd"));
    let _ = events(&mut e);
    // and it shows in frames
    let mut v = e.new_view();
    let f = e.frame(&mut v, EPOCH, 0, 0, None).unwrap();
    assert_eq!(f.title.as_deref(), Some("abcd"));
}

#[test]
fn title_reset_event() {
    let mut e = Emulator::new(20, 3, 10);
    e.feed(b"\x1b]0;x\x07");
    let _ = events(&mut e);
    e.feed(b"\x1b[22;0t\x1b[23;0t");
    // popping restores the pushed title; a pop with nothing pushed resets it
    let evs = events(&mut e);
    assert!(
        evs.iter()
            .all(|ev| matches!(ev, EmuEvent::Title(_) | EmuEvent::ResetTitle)),
        "{evs:?}"
    );
    e.feed(b"\x1b[23;0t");
    assert!(e.title().is_none() || e.title() == Some("x"));
}

#[test]
fn device_reports_are_answered() {
    let mut e = Emulator::new(20, 3, 10);
    let mut ask = |bytes: &[u8]| {
        e.feed(bytes);
        pty_writes(&e.take_events())
    };
    assert_eq!(ask(b"\x1b[c"), ["\x1b[?6c"]);
    assert_eq!(ask(b"\x1b[5n"), ["\x1b[0n"]);
    assert_eq!(ask(b"\x1b[2;4H\x1b[6n"), ["\x1b[2;4R"]);
    assert_eq!(ask(b"\x1b[18t"), ["\x1b[8;3;20t"]);
    assert_eq!(ask(b"\x1b[14t"), ["\x1b[4;48;160t"]);
    assert_eq!(ask(b"\x1b]11;?\x07"), ["\x1b]11;rgb:1e1e/1e1e/1e1e\x07"]);
    assert_eq!(
        ask(b"\x1b]10;?\x1b\\"),
        ["\x1b]10;rgb:d4d4/d4d4/d4d4\x1b\\"]
    );
    assert_eq!(ask(b"\x1b]4;1;?\x07"), ["\x1b]4;1;rgb:cdcd/0000/0000\x07"]);
    assert_eq!(
        ask(b"\x1b]4;16;?\x07"),
        ["\x1b]4;16;rgb:0000/0000/0000\x07"]
    );
    // an override by the program is what a later query reports
    assert_eq!(
        ask(b"\x1b]4;1;rgb:ff/80/00\x07\x1b]4;1;?\x07"),
        ["\x1b]4;1;rgb:ffff/8080/0000\x07"]
    );
    // kitty keyboard flags
    assert_eq!(ask(b"\x1b[?u"), ["\x1b[?0u"]);
    assert_eq!(ask(b"\x1b[>1u\x1b[?u"), ["\x1b[?1u"]);
    // clipboard and window requests are ignored, not answered, and do not break parsing
    assert!(ask(b"\x1b]52;c;?\x07\x1b]52;c;aGk=\x07\x1b[22;2t\x1b[21t").len() <= 1);
    assert!(ask(b"\x1b[>c")[0].starts_with("\x1b[>0;"));
    assert_eq!(ask(b"plain text"), Vec::<String>::new());
}

#[test]
fn osc_52_cannot_reach_the_clipboard() {
    let mut e = Emulator::new(20, 3, 10);
    e.feed(b"\x1b]52;c;aGVsbG8=\x07ok");
    assert!(
        e.take_events()
            .iter()
            .all(|ev| matches!(ev, EmuEvent::PtyWrite(_))),
        "no clipboard event exists, and none must leak out"
    );
    assert_eq!(e.screen_text(), "ok\n\n");
}

#[test]
fn scrollback_overflow_keeps_numbering() {
    let mut r = rig(20, 5, 50);
    for i in 0..500 {
        r.feed_str(&format!("n{i}\r\n"));
    }
    assert_eq!(r.emu.history_len(), 50);
    assert_eq!(r.emu.top_abs(), 496);
    assert_eq!(r.emu.history_lo_abs(), 446);
    assert!(r.emu.rows(0, 10).is_empty());
    assert_eq!(r.texts(440, 450), lines("n", 446..450));
    assert!(
        r.clients[0].cache.row(445).is_none(),
        "rows the server dropped are dropped from the client too"
    );
    assert!(r.clients[0].cache.row(446).is_some());
    r.done();
}

#[test]
fn scrollback_zero_still_numbers_rows() {
    let mut r = rig(10, 3, 0);
    for i in 0..20 {
        r.feed_str(&format!("n{i}\r\n"));
    }
    assert_eq!(r.emu.history_len(), 0);
    assert_eq!(r.emu.top_abs(), 18);
    assert_eq!(r.emu.history_lo_abs(), 18);
    assert_eq!(r.screen(), ["n18", "n19", ""]);
    r.done();
}

#[test]
fn ten_thousand_lines() {
    let mut r = rig(80, 24, 10_000);
    r.audit_every = 2_000;
    for i in 1..=10_000 {
        r.feed_str(&format!("{i}\r\n"));
    }
    assert_eq!(r.emu.top_abs(), 10_001 - 24);
    assert_eq!(r.emu.history_lo_abs(), 0);
    assert_eq!(r.emu.history_len(), 9_977);
    let all = r.texts(0, 10_000);
    assert!(all
        .iter()
        .enumerate()
        .all(|(i, t)| *t == (i + 1).to_string()));
    for i in 10_001..=10_100 {
        r.feed_str(&format!("{i}\r\n"));
    }
    assert_eq!(r.emu.history_len(), 10_000);
    assert_eq!(r.emu.history_lo_abs(), 10_101 - 24 - 10_000);
    assert_eq!(r.texts(77, 78), ["78"]);
    r.done();
}

#[test]
fn notifications_and_bells() {
    let mut e = Emulator::new(20, 3, 10);
    e.feed(b"\x1b]9;hello there\x07");
    assert_eq!(
        events(&mut e),
        [EmuEvent::Notify {
            title: String::new(),
            body: "hello there".into()
        }]
    );
    e.feed(b"\x1b]777;notify;Build;done ok\x1b\\");
    assert_eq!(
        events(&mut e),
        [EmuEvent::Notify {
            title: "Build".into(),
            body: "done ok".into()
        }]
    );
    e.feed(b"\x1b]777;noti");
    e.feed(b"fy;T;B\x07");
    assert_eq!(
        events(&mut e),
        [EmuEvent::Notify {
            title: "T".into(),
            body: "B".into()
        }]
    );
    e.feed(b"\x1b]9;4;1;50\x07");
    assert!(
        events(&mut e).is_empty(),
        "ConEmu progress is not a notification"
    );
    e.feed(b"\x07\x07\x07");
    assert_eq!(events(&mut e), [EmuEvent::Bell]);
    e.feed(b"\x07");
    e.feed(b"\x07");
    assert_eq!(events(&mut e), [EmuEvent::Bell], "one per take_events call");
    assert!(events(&mut e).is_empty());
    // a notification does not touch the screen
    assert_eq!(e.screen_text(), "\n\n");
}

#[test]
fn rev_only_moves_on_visible_change() {
    let mut e = Emulator::new(10, 3, 10);
    let r0 = e.rev();
    assert_eq!(r0, 1);
    e.feed(b"");
    assert_eq!(e.rev(), r0, "an empty feed changes nothing");
    e.feed(b"a");
    assert_eq!(e.rev(), r0 + 1);
    e.feed(b"\x1b[2;2H\x1b[2;2H\x1b[2;2H");
    assert_eq!(e.rev(), r0 + 2, "one bump per feed, however many changes");
    e.feed(b"\x1b[2;2H");
    assert_eq!(e.rev(), r0 + 2, "cursor already there");
    e.feed(b"\x1b[1;1Ha\x1b[2;2H");
    assert_eq!(
        e.rev(),
        r0 + 2,
        "rewriting the same character leaves nothing to report"
    );
    e.feed(b"\x1b]0;t\x07");
    assert_eq!(e.rev(), r0 + 3, "title");
    e.feed(b"\x1b]0;t\x07");
    assert_eq!(e.rev(), r0 + 3, "same title again");
    e.feed(b"\x1b[?1h");
    assert_eq!(e.rev(), r0 + 4, "mode");
    e.feed(b"\x1b[?1h");
    assert_eq!(e.rev(), r0 + 4);
    e.feed(b"\x1b[?25l");
    assert_eq!(e.rev(), r0 + 5, "cursor visibility");
    e.feed(&b"lots of text\r\n".repeat(100));
    assert_eq!(e.rev(), r0 + 6);
    e.feed(b"\x07\x1b]9;x\x07\x1b[5n");
    assert_eq!(e.rev(), r0 + 6, "events are not screen changes");
    e.resize(8, 3);
    assert_eq!(e.rev(), r0 + 7);
}

#[test]
fn seq_only_frames() {
    let mut e = Emulator::new(10, 3, 10);
    let mut v = e.new_view();
    let mut c = RowCache::new();
    let f1 = e.frame(&mut v, EPOCH, 1, 1, None).unwrap();
    assert_eq!(f1.base_rev, 0);
    c.apply(&f1).unwrap();
    assert!(e.frame(&mut v, EPOCH, 1, 1, None).is_none(), "nothing new");
    let f2 = e.frame(&mut v, EPOCH, 2, 1, None).unwrap();
    assert_eq!(
        (f2.base_rev, f2.rev, f2.applied_seq, f2.echo_seq),
        (f1.rev, f1.rev, 2, 1)
    );
    assert!(f2.row_patches.is_empty() && f2.new_styles.is_empty());
    c.apply(&f2).unwrap();
    assert_eq!((c.applied_seq, c.echo_seq, c.rev), (2, 1, f1.rev));
    let f3 = e.frame(&mut v, EPOCH, 2, 2, None).unwrap();
    c.apply(&f3).unwrap();
    assert_eq!((c.applied_seq, c.echo_seq), (2, 2));
    assert!(e.frame(&mut v, EPOCH, 2, 2, None).is_none());
    // timing is passed through untouched
    e.feed(b"x");
    let t = femme_proto::term::Timing {
        seq: 3,
        input_recv_ns: 1,
        pty_write_ns: 2,
        pty_read_ns: 3,
        frame_sent_ns: 4,
    };
    assert_eq!(
        e.frame(&mut v, EPOCH, 3, 3, Some(t)).unwrap().timing,
        Some(t)
    );
}

#[test]
fn clients_refuse_gaps_and_foreign_epochs() {
    let mut e = Emulator::new(10, 3, 10);
    let mut v = e.new_view();
    let mut c = RowCache::new();
    c.apply(&e.frame(&mut v, EPOCH, 0, 0, None).unwrap())
        .unwrap();
    e.feed(b"a");
    let f2 = e.frame(&mut v, EPOCH, 0, 0, None).unwrap();
    e.feed(b"b");
    let f3 = e.frame(&mut v, EPOCH, 0, 0, None).unwrap();
    let before = (c.rev, c.text());
    assert_eq!(
        c.apply(&f3),
        Err(ApplyError::NeedResync { have_rev: before.0 })
    );
    assert_eq!((c.rev, c.text()), before, "a refused frame changes nothing");
    // the recovery path: reopen with the rev the cache has
    let mut v2 = View::with_have_rev(c.rev);
    let resync = e.frame(&mut v2, EPOCH, 0, 0, None).unwrap();
    assert_eq!(resync.base_rev, c.rev);
    c.apply(&resync).unwrap();
    assert_eq!(c.text(), e.screen_text());
    // in-order application would have worked too
    let mut c2 = RowCache::new();
    let mut v3 = e.new_view();
    let _ = (f2, f3);
    c2.apply(&e.frame(&mut v3, EPOCH, 0, 0, None).unwrap())
        .unwrap();
    let mut other = e.frame(&mut e.new_view(), EPOCH + 1, 0, 0, None).unwrap();
    assert_eq!(c2.apply(&other), Err(ApplyError::EpochChanged));
    other.base_rev = 0;
    assert_eq!(
        c2.apply(&other),
        Err(ApplyError::EpochChanged),
        "even a snapshot, once the cache holds something"
    );
    let mut fresh = RowCache::new();
    fresh.apply(&other).unwrap();
}

#[test]
fn late_attach_resumes_with_a_delta_or_a_snapshot() {
    let mut r = rig(20, 5, 100);
    for i in 0..12 {
        r.feed_str(&format!("line {i}\r\n"));
    }
    let held = r.clients[0].cache.clone();
    let held_rev = held.rev;
    r.feed_str("one more\r\nand another\r\n");
    // resume: the client says what it has; the server answers with exactly what changed
    let mut cache = held.clone();
    let mut view = View::with_have_rev(held_rev);
    let f = r.emu.frame(&mut view, EPOCH, 0, 0, None).unwrap();
    assert_eq!(
        f.base_rev, held_rev,
        "a delta from the rev the client holds"
    );
    assert!(f.row_patches.len() <= 5);
    assert_eq!(f.new_styles.len(), 0);
    cache.apply(&f).unwrap();
    check(&r.emu, &cache, "resumed");
    // already current: nothing to send
    assert!(r
        .emu
        .frame(&mut View::with_have_rev(r.emu.rev()), EPOCH, 0, 0, None)
        .is_none());
    // rev 0, or a rev from the future, means a snapshot
    for have in [0, r.emu.rev() + 10, u64::MAX] {
        let mut c = RowCache::new();
        let mut v = View::with_have_rev(have);
        let f = r.emu.frame(&mut v, EPOCH, 0, 0, None).unwrap();
        assert_eq!(f.base_rev, 0, "have_rev {have}");
        c.apply(&f).unwrap();
        check(&r.emu, &c, "attached from scratch");
    }
    // a rev from before a resize or a screen switch cannot be continued
    let old = r.emu.rev();
    r.feed_str("x");
    r.resize(16, 6);
    let f = r
        .emu
        .frame(&mut View::with_have_rev(old), EPOCH, 0, 0, None)
        .unwrap();
    assert_eq!(f.base_rev, 0);
    let old = r.emu.rev();
    r.feed_str("\x1b[?1049hy");
    assert_eq!(
        r.emu
            .frame(&mut View::with_have_rev(old), EPOCH, 0, 0, None)
            .unwrap()
            .base_rev,
        0
    );
    let old = r.emu.rev();
    r.feed_str("z");
    assert_eq!(
        r.emu
            .frame(&mut View::with_have_rev(old), EPOCH, 0, 0, None)
            .unwrap()
            .base_rev,
        old,
        "after the switch, deltas are fine again"
    );
    r.done();
}

#[test]
fn deltas_are_capped_at_a_screenful() {
    let mut r = rig(10, 4, 100);
    r.feed_str("\x1b[1;1Haaaa\x1b[2;1Hbbbb\x1b[3;1Hcccc\x1b[4;1Hdddd");
    let mut view = View::default();
    let mut c = RowCache::new();
    c.apply(&r.emu.frame(&mut view, EPOCH, 0, 0, None).unwrap())
        .unwrap();
    let base = c.rev;
    // every row changes: still a delta, 4 rows
    r.feed_str("\x1b[1;1HAAAA\x1b[2;1HBBBB\x1b[3;1HCCCC\x1b[4;1HDDDD");
    let f = r.emu.frame(&mut view, EPOCH, 0, 0, None).unwrap();
    assert_eq!((f.base_rev, f.row_patches.len()), (base, 4));
    c.apply(&f).unwrap();
    // Three rows scroll away and one new row of text appears. The three rows the client already
    // holds are not resent and the blank rows that come in are implied: one patch.
    r.feed_str("\x1b[4;1H\n\n\nxy");
    let f = r.emu.frame(&mut view, EPOCH, 0, 0, None).unwrap();
    assert_ne!(f.base_rev, 0);
    assert_eq!(f.row_patches.len(), 1, "{:?}", f.row_patches);
    c.apply(&f).unwrap();
    check(&r.emu, &c, "scrolled");
    assert_eq!(
        row_text(
            c.row(r.emu.top_abs() - 1)
                .expect("pushed rows are never holes")
        ),
        "CCCC"
    );
    // More rows scroll away than the screen has: cheaper (and simpler) as a snapshot
    r.feed_str("\n\n\n\n\nzz");
    let f = r.emu.frame(&mut view, EPOCH, 0, 0, None).unwrap();
    assert_eq!(f.base_rev, 0);
    assert!(f.row_patches.len() <= 4);
    c.apply(&f).unwrap();
    check(&r.emu, &c, "snapshot");
    // the scrollback rows a snapshot does not carry are fetched on demand
    assert!(c.row(r.emu.top_abs() - 1).is_none());
    assert!(r.emu.top_abs() > 5);
    fill_history(&r.emu, &mut c);
    check_history(&r.emu, &c, "after GetRows");
    r.done();
}

#[test]
fn a_view_that_skips_frames_gets_the_merged_changes() {
    let mut r = rig(12, 4, 100);
    let mut view = View::default();
    let mut c = RowCache::new();
    c.apply(&r.emu.frame(&mut view, EPOCH, 0, 0, None).unwrap())
        .unwrap();
    // three feeds the view never sees: a row changes, scrolls away, another changes
    r.feed_str("first");
    r.feed_str("\r\nsecond");
    r.feed_str("\x1b[4;1H\nthird\x1b[1;1Hzero");
    let f = r.emu.frame(&mut view, EPOCH, 0, 0, None).unwrap();
    assert!(f.base_rev != 0, "small enough for a delta");
    c.apply(&f).unwrap();
    check(&r.emu, &c, "merged");
    assert_eq!(
        row_text(c.row(0).unwrap()),
        "first",
        "the row that scrolled off arrives with its final content"
    );
    r.done();
}

#[test]
fn sync_update_holds_the_screen_back_until_it_ends() {
    let mut e = Emulator::new(20, 3, 10);
    assert!(e.sync_deadline().is_none());
    e.feed(b"\x1b[?2026h");
    let rev = e.rev();
    e.feed(b"abc");
    assert_eq!(e.screen_text(), "\n\n", "held back");
    assert_eq!(e.rev(), rev, "and so is the rev");
    assert!(e.sync_deadline().is_some());
    e.feed(b"def\x1b[?2026l");
    assert_eq!(e.screen_text(), "abcdef\n\n");
    assert_eq!(e.rev(), rev + 1);
    assert!(e.sync_deadline().is_none());

    // the end marker split across feeds, through the harness
    let mut r = rig(20, 3, 10);
    for chunk in ["\x1b[?20", "26hxyz", "\x1b[?2", "026l", "!"] {
        r.feed_str(chunk);
    }
    assert_eq!(r.screen()[0], "xyz!");
    r.done();

    // a program that never ends the update: the next feed after the deadline flushes it
    let mut e = Emulator::new(20, 3, 10);
    e.feed(b"\x1b[?2026hstuck");
    let deadline = e.sync_deadline().expect("pending");
    std::thread::sleep(
        deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(20),
    );
    e.feed(b"");
    assert_eq!(e.screen_text(), "stuck\n\n");
    assert!(e.sync_deadline().is_none());
    // and the holder can flush at the deadline without waiting for output
    e.feed(b"\x1b[?2026hmore");
    e.flush_sync();
    assert_eq!(e.screen_text(), "stuckmore\n\n");
    e.flush_sync();
}

#[test]
fn modes_map_to_the_wire_flags() {
    let mut e = Emulator::new(20, 3, 10);
    assert_eq!(
        e.modes(),
        Modes::LINE_WRAP | Modes::ALTERNATE_SCROLL,
        "defaults"
    );
    let pairs: &[(&str, &str, Modes)] = &[
        ("\x1b[?1h", "\x1b[?1l", Modes::APP_CURSOR),
        ("\x1b=", "\x1b>", Modes::APP_KEYPAD),
        ("\x1b[?2004h", "\x1b[?2004l", Modes::BRACKETED_PASTE),
        ("\x1b[?1000h", "\x1b[?1000l", Modes::MOUSE_CLICK),
        ("\x1b[?1002h", "\x1b[?1002l", Modes::MOUSE_DRAG),
        ("\x1b[?1003h", "\x1b[?1003l", Modes::MOUSE_MOTION),
        ("\x1b[?1006h", "\x1b[?1006l", Modes::SGR_MOUSE),
        ("\x1b[?1004h", "\x1b[?1004l", Modes::FOCUS_IN_OUT),
        ("\x1b[?1049h", "\x1b[?1049l", Modes::ALT_SCREEN),
        ("\x1b[?7l", "\x1b[?7h", Modes::empty()), // LINE_WRAP is on by default: checked below
        ("\x1b[?1007l", "\x1b[?1007h", Modes::empty()),
        ("\x1b[>1u", "\x1b[<u", Modes::KITTY_KEYBOARD),
        ("\x1b[4h", "\x1b[4l", Modes::INSERT),
        ("\x1b[?6h", "\x1b[?6l", Modes::ORIGIN),
    ];
    for (on, off, flag) in pairs {
        let before = e.modes();
        e.feed(on.as_bytes());
        assert_eq!(e.modes() & *flag, *flag, "{on:?}");
        if flag.is_empty() {
            assert_ne!(e.modes(), before, "{on:?} must change something");
        }
        e.feed(off.as_bytes());
        assert_eq!(e.modes(), before, "{off:?} undoes {on:?}");
    }
    e.feed(b"\x1b[?7l");
    assert!(!e.modes().contains(Modes::LINE_WRAP));
    e.feed(b"\x1b[?1007l");
    assert!(!e.modes().contains(Modes::ALTERNATE_SCROLL));
    // and with the wrap off, text at the right edge overwrites instead of wrapping
    e.feed(b"\x1b[?7l");
    e.feed(b"0123456789012345678901234");
    assert_eq!(
        e.screen_text().split('\n').next().unwrap(),
        "01234567890123456784"
    );
}

#[test]
fn keys_and_mouse_follow_the_live_modes() {
    let mut e = Emulator::new(80, 24, 0);
    let key = |code, mods| Key { code, mods };
    let plain = |code| Key {
        code,
        mods: Mods::empty(),
    };
    assert_eq!(e.encode_key(plain(KeyCode::Up)), b"\x1b[A");
    assert_eq!(e.encode_key(plain(KeyCode::Char('x'))), b"x");
    assert_eq!(e.encode_key(plain(KeyCode::Enter)), b"\r");
    assert_eq!(e.encode_key(plain(KeyCode::Backspace)), b"\x7f");
    assert_eq!(e.encode_key(plain(KeyCode::F(5))), b"\x1b[15~");
    assert_eq!(e.encode_key(key(KeyCode::Char('c'), Mods::CTRL)), [3]);
    assert_eq!(e.encode_key(key(KeyCode::Char('a'), Mods::ALT)), b"\x1ba");
    assert_eq!(e.encode_key(key(KeyCode::Tab, Mods::SHIFT)), b"\x1b[Z");
    assert_eq!(e.encode_key(key(KeyCode::Up, Mods::CTRL)), b"\x1b[1;5A");
    e.feed(b"\x1b[?1h");
    assert_eq!(
        e.encode_key(plain(KeyCode::Up)),
        b"\x1bOA",
        "application cursor keys"
    );
    assert_eq!(
        e.encode_key(key(KeyCode::Up, Mods::CTRL)),
        b"\x1b[1;5A",
        "modified arrows stay CSI"
    );
    e.feed(b"\x1b[>1u");
    assert_eq!(
        e.encode_key(plain(KeyCode::Escape)),
        b"\x1b[27u",
        "kitty: disambiguated escape"
    );
    assert_eq!(
        e.encode_key(key(KeyCode::Enter, Mods::SHIFT)),
        b"\x1b[13;2u"
    );
    e.feed(b"\x1b[<u");
    assert_eq!(e.encode_key(plain(KeyCode::Escape)), b"\x1b");
    e.feed(b"\x1b[?1l");

    assert_eq!(e.encode_paste("a\x1bb"), b"a\x1bb");
    e.feed(b"\x1b[?2004h");
    assert_eq!(
        e.encode_paste("a\x1bb"),
        b"\x1b[200~ab\x1b[201~",
        "bracketed, and the paste cannot carry the end marker"
    );

    let press = MouseEv {
        kind: MouseKind::Press,
        button: 0,
        col: 4,
        row: 2,
        mods: Mods::empty(),
    };
    assert_eq!(e.encode_mouse(press), None, "reporting is off");
    e.feed(b"\x1b[?1000h\x1b[?1006h");
    assert_eq!(e.encode_mouse(press).unwrap(), b"\x1b[<0;5;3M");
    assert_eq!(
        e.encode_mouse(MouseEv {
            kind: MouseKind::Release,
            ..press
        })
        .unwrap(),
        b"\x1b[<0;5;3m"
    );
    assert_eq!(
        e.encode_mouse(MouseEv {
            kind: MouseKind::ScrollUp,
            ..press
        })
        .unwrap(),
        b"\x1b[<64;5;3M"
    );
    assert_eq!(
        e.encode_mouse(MouseEv {
            kind: MouseKind::Move,
            button: 9,
            ..press
        }),
        None,
        "plain click mode has no motion"
    );
    e.feed(b"\x1b[?1000l\x1b[?1006l");
    // alternate scroll: on the alt screen a wheel notch is three arrow presses
    let wheel = MouseEv {
        kind: MouseKind::ScrollUp,
        ..press
    };
    assert_eq!(
        e.encode_mouse(wheel),
        None,
        "primary screen: the client scrolls its own history"
    );
    e.feed(b"\x1b[?1049h");
    assert_eq!(e.encode_mouse(wheel).unwrap(), b"\x1b[A\x1b[A\x1b[A");
    assert_eq!(
        e.encode_mouse(MouseEv {
            kind: MouseKind::ScrollDown,
            ..press
        })
        .unwrap(),
        b"\x1b[B\x1b[B\x1b[B"
    );
    e.feed(b"\x1b[?1h");
    assert_eq!(e.encode_mouse(wheel).unwrap(), b"\x1bOA\x1bOA\x1bOA");
    assert_eq!(e.encode_focus(true), None);
    e.feed(b"\x1b[?1004h");
    assert_eq!(e.encode_focus(true).unwrap(), b"\x1b[I");
    assert_eq!(e.encode_focus(false).unwrap(), b"\x1b[O");
}

#[test]
fn screen_ansi_dumps_colours_and_resets() {
    let mut e = Emulator::new(20, 3, 10);
    e.feed(b"plain \x1b[1;31mred bold\x1b[0m after\r\n\x1b[48;2;1;2;3mbg\x1b[0m");
    let ansi = e.screen_ansi();
    let rows: Vec<&str> = ansi.split('\n').collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], "plain \x1b[0;1;31mred bold\x1b[0m after");
    assert_eq!(rows[1], "\x1b[0;48;2;1;2;3mbg\x1b[0m");
    assert_eq!(rows[2], "");
}

#[test]
fn styles_are_stable_and_every_cell_has_one() {
    let mut r = rig(30, 3, 20);
    for i in 0..60 {
        r.feed_str(&format!(
            "\x1b[38;5;{}m\x1b[{}mcolour {i}\x1b[0m\r\n",
            i + 20,
            1 + i % 4
        ));
    }
    // ids never change once handed out
    let before = r.emu.styles();
    for i in 0..30 {
        r.feed_str(&format!("\x1b[48;5;{}mmore {i}\x1b[0m\r\n", i + 100));
    }
    let after = r.emu.styles();
    assert_eq!(&after[..before.len()], &before[..]);
    assert!(after.len() > before.len());
    assert!(after
        .iter()
        .enumerate()
        .all(|(i, (id, _))| usize::from(*id) == i + 1));
    // a style in history, never rendered while visible, is defined when asked for later
    r.done();
}

#[test]
fn style_table_overflow_rebuilds_without_corrupting_the_screen() {
    let mut e = Emulator::new(100, 10, 50);
    let mut client = Client::new("live", 1);
    let mut max_len = 0;
    let mut text = Vec::new();
    for chunk in 0..70 {
        text.clear();
        for i in 0..1_000u32 {
            let n = chunk * 1_000 + i;
            write!(
                text,
                "\x1b[38;2;{};{};{}mX",
                (n >> 16) & 255,
                (n >> 8) & 255,
                n & 255
            )
            .unwrap();
            if n % 90 == 89 {
                text.extend_from_slice(b"\r\n");
            }
        }
        e.feed(&text);
        client.sync(&mut e);
        e.audit_styles().unwrap();
        max_len = max_len.max(e.styles().len());
    }
    assert!(max_len >= 60_000, "the table did fill up ({max_len})");
    assert!(
        e.styles().len() < 20_000,
        "and was rebuilt from what is on screen ({})",
        e.styles().len()
    );
    assert!(
        client.st.snapshots >= 2,
        "a rebuild renumbers styles, so clients resync"
    );
    fill_history(&e, &mut client.cache);
    check_history(&e, &client.cache, "after overflow");
}

#[test]
fn style_table_overflow_twice_in_a_second_degrades_instead_of_failing() {
    // ponytail: rebuilds are rate-limited to one per second; between two, new styles beyond the
    // 65 535th render as the default style. Text and layout stay right.
    let mut e = Emulator::new(100, 10, 5_000);
    let mut client = Client::new("live", 1);
    let mut text = Vec::new();
    for chunk in 0..140 {
        text.clear();
        for i in 0..1_000u32 {
            let n = chunk * 1_000 + i;
            write!(
                text,
                "\x1b[38;2;{};{};{}mX",
                (n >> 16) & 255,
                (n >> 8) & 255,
                n & 255
            )
            .unwrap();
            if n % 90 == 89 {
                text.extend_from_slice(b"\r\n");
            }
        }
        e.feed(&text);
        client.sync(&mut e);
    }
    fill_history(&e, &mut client.cache);
    assert_eq!(client.cache.text(), e.screen_text());
}

// ---------------------------------------------------------------------------------------------
// limits: what a program on the PTY cannot make the emulator hold or send
// ---------------------------------------------------------------------------------------------

/// Characters in a row's text (the base characters and their zero-width followers).
fn chars_in(p: &RowPatch) -> usize {
    p.runs.iter().map(|(_, t)| t.chars().count()).sum()
}

#[test]
fn a_cell_keeps_at_most_sixteen_zero_width_characters() {
    let mut r = rig(40, 8, 10);
    let flood = "\u{301}".repeat(1000);
    // an ordinary base character
    r.feed_str(&format!("a{flood}"));
    // a pending wrap: the marks go on the last column's own cell, not on the one before it
    r.feed_str(&format!("\r\n{}x{flood}", "b".repeat(39)));
    // a wide base: the cursor is past its spacer cell, the marks still belong to the base
    r.feed_str(&format!("\r\n\u{3042}{flood}"));
    // REP repeats the last character 65 535 times per sequence
    r.feed_str("\r\na\u{1D167}\x1b[65535b\x1b[65535b\x1b[65535b");
    let rows = r.emu.rows(0, 4);
    let seen: Vec<usize> = rows.iter().map(chars_in).collect();
    assert_eq!(seen, [1 + 16, 40 + 16, 1 + 16, 1 + 16]);
    r.done();
}

#[test]
fn a_row_shows_at_most_one_zero_width_character_per_cell_in_total() {
    let mut r = rig(20, 3, 10);
    // 20 cells with 16 marks each: every cell is within its own limit, the row is not
    r.feed_str(&format!("a{}", "\u{301}".repeat(16)).repeat(20));
    let p = &r.emu.rows(0, 1)[0];
    assert_eq!(
        chars_in(p),
        20 + 20,
        "20 base characters and a budget of 20 marks"
    );
    assert_eq!(row_text(p).chars().take(2).collect::<String>(), "a\u{301}");
    r.done();
}

#[test]
fn a_rows_answer_stops_at_the_byte_budget_with_whole_rows() {
    // 1 000 cells per row, two styles alternating, four bytes per character: about 10 KB a row
    let mut e = Emulator::new(1000, 10, 3000);
    let line = "\x1b[1m\u{10348}\x1b[22m\u{10348}".repeat(500);
    for _ in 0..1200 {
        e.feed(format!("{line}\r\n").as_bytes());
    }
    // a client that attaches now gets the screen in a snapshot and has to fetch the scrollback
    let mut c = Client::new("late", 1);
    c.sync(&mut e);
    let (lo, top) = (e.history_lo_abs(), e.top_abs());
    assert!(top - lo > 1000, "setup: {} history rows", top - lo);
    let first = e.rows(lo, top);
    assert!(
        first.len() > 1 && (first.len() as u64) < top - lo,
        "{} of {} rows",
        first.len(),
        top - lo
    );
    for (i, p) in first.iter().enumerate() {
        assert_eq!(p.abs, lo + i as u64, "whole rows from the start, no gaps");
    }
    // the caller asks again from the first row it lacks, and ends up with all of them
    c.cache.add_rows(first);
    let mut asks = 1;
    while let Some(from) = c.cache.missing_after(lo, top) {
        let rows = e.rows(from, top);
        assert_eq!(rows.first().map(|p| p.abs), Some(from));
        c.cache.add_rows(rows);
        asks += 1;
        assert!(asks < 10, "no progress");
    }
    assert!(asks >= 2);
    check_history(&e, &c.cache, "after the budgeted fetches");
}

#[test]
fn a_long_title_is_cut_to_a_kilobyte_on_a_character_boundary() {
    let mut e = Emulator::new(20, 3, 10);
    let mut v = e.new_view();
    let _ = e.frame(&mut v, EPOCH, 0, 0, None);
    // 3 000 bytes of 3-byte characters: 1 024 is inside one, so the cut has to back up
    let long = "\u{20ac}".repeat(1000);
    e.feed(format!("\x1b]0;{long}\x07").as_bytes());
    let title = e.title().expect("a title was set").to_string();
    assert!(
        title.len() <= 1024 && title.len() > 1000 && title.chars().all(|c| c == '\u{20ac}'),
        "{} bytes",
        title.len()
    );
    assert_eq!(events(&mut e), [EmuEvent::Title(title.clone())]);
    // frames carry the capped title, and a short one is left alone
    let f = e.frame(&mut v, EPOCH, 0, 0, None).unwrap();
    assert_eq!(f.title.as_deref(), Some(title.as_str()));
    e.feed(b"\x1b]0;short\x07");
    assert_eq!(e.title(), Some("short"));
}

/// A line of 100 cells that all have different background colours, `n..n + 100`. The line break
/// comes first so the line ends on the last row.
fn colour_line(n: u32) -> String {
    let mut s = String::from("\r\n");
    for i in n..n + 100 {
        s.push_str(&format!(
            "\x1b[48;2;{};{};{}mX",
            i & 255,
            (i >> 8) & 255,
            i >> 16
        ));
    }
    s
}

/// Feed colour lines until the 65 535 style ids run out and the emulator rebuilds the table
/// (which renumbers everything). Returns the next unused colour.
fn feed_colours_until_rebuild(e: &mut Emulator, c: &mut Client) -> u32 {
    let mut n = 0;
    loop {
        let top = e.top_abs();
        e.feed(colour_line(n).as_bytes());
        n += 100;
        c.sync(e);
        if e.top_abs() > top + 5 {
            return n;
        }
        assert!(n < 200_000, "the style table never filled up");
    }
}

/// How many of the visible rows show all 100 of their colours (one run each, none in the default style).
fn coloured_rows(e: &Emulator) -> usize {
    let top = e.top_abs();
    e.rows(top, top + u64::from(e.screen_rows()))
        .iter()
        .filter(|p| p.runs.len() == 100 && p.runs.iter().all(|r| r.0 != 0))
        .count()
}

#[test]
fn style_rebuild_keeps_the_screen_ahead_of_scrollback() {
    let mut e = Emulator::new(100, 10, 1000);
    let mut c = Client::new("live", 1);
    feed_colours_until_rebuild(&mut e, &mut c);
    // About 64 600 styles are in scrollback and 1 000 on the screen: more than the table holds.
    // What is left out must be the oldest scrollback, not the end of the screen.
    assert_eq!(coloured_rows(&e), 10);
}

#[test]
fn style_rebuild_is_retried_from_frame_once_the_rate_limit_is_over() {
    let mut e = Emulator::new(100, 10, 1000);
    let mut c = Client::new("live", 1);
    let n = feed_colours_until_rebuild(&mut e, &mut c);
    // New colours straight away: the table is full again, but rebuilds are limited to one a second
    e.feed(colour_line(n).as_bytes());
    c.sync(&mut e);
    assert!(
        coloured_rows(&e) < 10,
        "setup: the new line has lost its colours"
    );
    // and nothing more is fed: a frame has to do it
    std::thread::sleep(Duration::from_millis(1100));
    c.sync(&mut e);
    assert_eq!(coloured_rows(&e), 10);
}

#[test]
fn history_rows_after_an_alt_screen_style_rebuild_use_styles_the_client_has() {
    let mut e = Emulator::new(40, 5, 1000);
    let mut c = Client::new("live", 1);
    e.feed(b"\x1b[31mred history\x1b[0m");
    for i in 0..10 {
        e.feed(format!("\r\nrow {i}").as_bytes());
    }
    c.sync(&mut e);
    e.feed(b"\x1b[?1049h");
    c.sync(&mut e);
    // 66 000 colours on the alt screen overflow the table, and the rebuild there sees the alt screen only
    let mut s = String::new();
    for i in 0..66_000u32 {
        s.push_str(&format!(
            "\x1b[38;2;{};{};{}mx",
            i & 255,
            (i >> 8) & 255,
            i >> 16
        ));
        if i % 4000 == 3999 {
            s.push_str("\x1b[H");
        }
    }
    e.feed(s.as_bytes());
    c.sync(&mut e);
    e.feed(b"\x1b[0m\x1b[2J");
    c.sync(&mut e);
    e.feed(b"\x1b[?1049l");
    c.sync(&mut e);
    let (lo, top) = (e.history_lo_abs(), e.top_abs());
    let rows = e.rows(lo, top);
    let red = rows.iter().find(|p| row_text(p).starts_with("red history"));
    let id = red.expect("the first line is still in scrollback").runs[0].0;
    assert_ne!(id, 0, "scrollback kept its colour");
    // a GetRows answer carries no style definitions, so every id in it must be one the client was sent
    c.cache.add_rows(rows.clone());
    let table = e.styles();
    for p in &rows {
        for (id, _) in &p.runs {
            if *id != 0 {
                assert_eq!(
                    table[usize::from(*id) - 1],
                    (*id, c.cache.style(*id)),
                    "style {id}"
                );
            }
        }
    }
}

#[test]
fn an_oversized_osc_is_cut_whatever_the_chunking() {
    let mut payload = b"\x1b]0;".to_vec();
    payload.extend(std::iter::repeat_n(b'a', 20_000));
    payload.extend_from_slice(b"\x1b\\ok\x1b]9;note\x07");
    // the parser's escape state also takes control bytes, which must not hide an OSC from the scanner
    payload.extend_from_slice(b"\x1b\x01]0;viaC0\x07");
    for chunk in [1, 7, 4095, 4096, 4097, 9000, payload.len()] {
        let mut e = Emulator::new(20, 3, 10);
        for part in payload.chunks(chunk) {
            e.feed(part);
        }
        assert_eq!(e.title(), Some("viaC0"), "chunks of {chunk}");
        assert_eq!(e.screen_text(), "ok\n\n", "chunks of {chunk}");
        let evs = events(&mut e);
        assert!(
            evs.contains(&EmuEvent::Notify {
                title: String::new(),
                body: "note".into()
            }),
            "chunks of {chunk}: {evs:?}"
        );
    }
}

#[test]
fn sizes_are_clamped_to_what_the_holder_serves() {
    let e = Emulator::new(0, 0, 10);
    assert_eq!((e.cols(), e.screen_rows()), (2, 1));
    let mut e = Emulator::new(1200, 700, 0);
    assert_eq!((e.cols(), e.screen_rows()), (1000, 500));
    e.resize(3, 4);
    assert_eq!((e.cols(), e.screen_rows()), (3, 4));
    e.resize(1500, 600);
    assert_eq!((e.cols(), e.screen_rows()), (1000, 500));
    e.feed(b"still works");
    assert!(e.screen_text().starts_with("still works"));
}

#[test]
fn re_armed_synchronized_updates_cannot_hold_the_screen_for_more_than_a_second() {
    let mut e = Emulator::new(20, 3, 10);
    let start = Instant::now();
    let mut visible_at = None;
    // each begin marker re-arms the parser's 150 ms timeout, and the program never ends the update
    while visible_at.is_none() && start.elapsed() < Duration::from_secs(3) {
        e.feed(b"\x1b[?2026hx");
        if e.screen_text().starts_with('x') {
            visible_at = Some(start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let at = visible_at.expect("the update was held back for 3 s");
    assert!(at < Duration::from_millis(1300), "applied after {at:?}");
}

// ---------------------------------------------------------------------------------------------
// random streams
// ---------------------------------------------------------------------------------------------

const WORDS: &[&str] = &[
    "foo",
    "bar",
    "hello",
    "world",
    "x",
    "Lorem",
    "ipsum",
    "0123456789",
    "~/src $ ",
    "error:",
    "ok",
    "  ",
    "tab\there",
];
const UNICODE: &[&str] = &[
    "\u{3042}",
    "\u{3044}",
    "\u{6f22}",
    "\u{5b57}",
    "\u{1F600}",
    "\u{1F44D}",
    "e\u{301}",
    "\u{200d}",
    "\u{fe0f}",
    "\u{ff71}",
    "\u{ff10}",
    "\u{ff21}",
    "\u{e9}",
    "\u{df}",
    "\u{301}",
    "\u{1F468}\u{200d}\u{1F469}\u{200d}\u{1F467}",
    "\u{1F1E9}\u{1F1EA}",
    "\u{2764}\u{fe0f}",
    "\u{200b}",
    "\u{a0}",
    "\u{feff}",
    "\u{2500}",
    "\u{2588}",
    "\u{20dd}",
    "\u{0928}\u{094d}",
];
const MODES: &[u32] = &[
    1, 3, 6, 7, 12, 25, 47, 66, 1000, 1002, 1003, 1004, 1006, 1007, 1047, 1048, 2004,
];
const SGR_SIMPLE: &[&str] = &[
    "0", "1", "2", "3", "4", "7", "8", "9", "21", "22", "23", "24", "27", "28", "29", "39", "49",
    "59", "4:2", "4:3", "4:4", "4:5", "4:0", "53", "55", "",
];

/// Knobs of one generated stream.
#[derive(Clone, Copy)]
struct Gen {
    cols: u64,
    rows: u64,
    /// Leave out `ED 3` and `RIS` (the oracle test counts pushed rows, and these forget them).
    keep_history: bool,
}

fn csi(out: &mut Vec<u8>, params: &str, fin: char) {
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(params.as_bytes());
    out.push(fin as u8);
}

/// A numeric parameter: usually small, sometimes 0 (default) or huge.
fn param(rng: &mut Lcg, max: u64) -> u64 {
    match rng.below(12) {
        0 => 0,
        1 => 9999,
        _ => rng.range(1, max.max(1) + 1),
    }
}

fn sgr(rng: &mut Lcg, out: &mut Vec<u8>) {
    let mut parts: Vec<String> = Vec::new();
    for _ in 0..rng.range(1, 3) {
        parts.push(match rng.below(14) {
            0..=3 => (*rng.pick(SGR_SIMPLE)).to_string(),
            4 => (30 + rng.below(8)).to_string(),
            5 => (90 + rng.below(8)).to_string(),
            6 => (40 + rng.below(8)).to_string(),
            7 => (100 + rng.below(8)).to_string(),
            8 => format!("38;5;{}", rng.below(256)),
            9 => format!("48;5;{}", rng.below(256)),
            10 => format!(
                "38;2;{};{};{}",
                rng.below(3) * 100,
                rng.below(256),
                rng.below(3) * 100
            ),
            11 => format!(
                "48;2;{};{};{}",
                rng.below(256),
                rng.below(3) * 100,
                rng.below(256)
            ),
            12 => format!("58;5;{}", rng.below(256)),
            _ => format!(
                "58;2;{};{};{}",
                rng.below(256),
                rng.below(256),
                rng.below(256)
            ),
        });
    }
    csi(out, &parts.join(";"), 'm');
}

fn fragment(rng: &mut Lcg, out: &mut Vec<u8>, g: Gen) {
    let (cols, rows) = (g.cols, g.rows);
    match rng.below(100) {
        0..=27 => {
            out.extend_from_slice(rng.pick(WORDS).as_bytes());
            if rng.below(3) == 0 {
                out.push(b' ');
            }
        }
        28..=29 => {
            for _ in 0..rng.range(1, cols * 2 + 3) {
                out.push(b'a' + rng.below(26) as u8);
            }
        }
        30..=37 => out.extend_from_slice(b"\r\n"),
        38..=41 => out.push(b'\n'),
        42..=43 => out.push(b'\r'),
        44 => out.push(8),
        45..=46 => out.push(b'\t'),
        47..=52 => sgr(rng, out),
        53..=60 => {
            let fin = *rng.pick(&['A', 'B', 'C', 'D', 'E', 'F', 'G', 'd', 'H', 'f', 'H']);
            let params = if matches!(fin, 'H' | 'f') {
                format!("{};{}", param(rng, rows), param(rng, cols))
            } else {
                param(rng, rows.max(cols)).to_string()
            };
            csi(out, &params, fin);
        }
        61..=63 => {
            if rng.below(2) == 0 {
                let n = if g.keep_history {
                    rng.below(3)
                } else {
                    rng.below(4)
                };
                csi(out, &n.to_string(), 'J');
            } else {
                csi(out, &rng.below(3).to_string(), 'K');
            }
        }
        64..=67 => {
            let fin = *rng.pick(&['L', 'M', '@', 'P', 'X', 'S', 'T']);
            csi(out, &param(rng, 5).to_string(), fin);
        }
        68..=69 => {
            if rng.below(5) == 0 {
                csi(out, "", 'r');
            } else {
                let t = rng.below(rows + 1);
                let b = t + rng.below(rows + 1);
                csi(out, &format!("{t};{b}"), 'r');
            }
        }
        70..=72 => out.extend_from_slice(rng.pick(UNICODE).as_bytes()),
        73..=74 => csi(out, "?1049", if rng.below(2) == 0 { 'h' } else { 'l' }),
        75..=77 => csi(
            out,
            &format!("?{}", rng.pick(MODES)),
            if rng.below(2) == 0 { 'h' } else { 'l' },
        ),
        78..=79 => {
            let mut esc: Vec<&[u8]> = vec![
                b"\x1b7",
                b"\x1b8",
                b"\x1bM",
                b"\x1bD",
                b"\x1bE",
                b"\x1bH",
                b"\x1b[g",
                b"\x1b[3g",
                b"\x1b#8",
                b"\x1b(0",
                b"\x1b(B",
                b"\x0e",
                b"\x0f",
                b"\x1b=",
                b"\x1b>",
                b"\x1b[!p",
                b"\x1b[4h",
                b"\x1b[4l",
                b"\x1b[20h",
                b"\x1b[20l",
            ];
            if g.keep_history {
                esc.retain(|e| *e != b"\x1bc");
            } else if rng.below(8) == 0 {
                esc.push(b"\x1bc");
            }
            out.extend_from_slice(rng.pick(&esc));
        }
        80..=81 => {
            let w = rng.pick(WORDS);
            let s = match rng.below(12) {
                0 => format!("\x1b]0;{w}\x07"),
                1 => format!("\x1b]2;{w}\x1b\\"),
                2 => format!("\x1b]9;{w}\x07"),
                3 => format!("\x1b]777;notify;{w};{w}\x07"),
                4 => "\x1b]52;c;aGk=\x07".to_string(),
                5 => format!("\x1b]8;;http://x/{w}\x07{w}\x1b]8;;\x07"),
                6 => "\x1b]104\x07".to_string(),
                7 => format!("\x1b]4;{};rgb:ff/00/80\x07", rng.below(256)),
                8 => "\x1b]10;?\x07".to_string(),
                9 => "\x1b]11;?\x1b\\".to_string(),
                10 => "\x1b]112\x07".to_string(),
                _ => format!("\x1b]4;{};?\x07", rng.below(256)),
            };
            out.extend_from_slice(s.as_bytes());
        }
        82 => {
            let q: &[&[u8]] = &[
                b"\x1b[c",
                b"\x1b[5n",
                b"\x1b[6n",
                b"\x1b[>c",
                b"\x1b[14t",
                b"\x1b[18t",
                b"\x1b[?u",
                b"\x1b[>1u",
                b"\x1b[<u",
                b"\x1b[=1;1u",
                b"\x1b[22;0t",
                b"\x1b[23;0t",
                b"\x1b[?2026$p",
                b"\x1b[4$p",
                b"\x1b[0 q",
                b"\x1b[3 q",
                b"\x1b[5 q",
            ];
            out.extend_from_slice(rng.pick(q));
        }
        83 => {
            for _ in 0..rng.range(1, 6) {
                let b = rng.below(256) as u8;
                if b != 0x1b {
                    out.push(b);
                }
            }
        }
        84 => out.push(0x18),
        _ => {
            out.extend_from_slice(rng.pick(WORDS).as_bytes());
            out.extend_from_slice(b"\r\n");
        }
    }
}

fn gen_stream(rng: &mut Lcg, frags: u64, g: Gen) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..frags {
        fragment(rng, &mut out, g);
    }
    out
}

/// Seeds per fuzz test; `P1_SEEDS=2000 cargo test ...` for a long soak.
fn seeds() -> u64 {
    std::env::var("P1_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24)
}

/// Random chunking: sizes drawn from a seed-dependent ceiling (1 byte at a time to a few KiB).
fn chunk_len(rng: &mut Lcg, max_chunk: u64) -> usize {
    rng.range(1, max_chunk) as usize
}

fn fuzz_one(seed: u64) {
    let mut rng = Lcg::new(seed);
    let (cols, rows) = (rng.range(2, 60) as u16, rng.range(1, 20) as u16);
    let scrollback = *rng.pick(&[0usize, 1, 5, 50, 300]);
    let g = Gen {
        cols: u64::from(cols),
        rows: u64::from(rows),
        keep_history: false,
    };
    let frags = rng.range(400, 2500);
    let stream = gen_stream(&mut rng, frags, g);
    let max_chunk = *rng.pick(&[1u64, 2, 7, 64, 500, 4096]);
    let mut r = rig(cols, rows, scrollback);
    r.audit_every = 5;
    r.clients.push(Client::new("rare", 37));
    r.clients.push(Client::new("reconnecting", 3));
    let ctx = format!(
        "seed {seed}: {cols}x{rows} scrollback {scrollback} chunks<= {max_chunk}, {} bytes",
        stream.len()
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut pos = 0;
        while pos < stream.len() {
            let n = chunk_len(&mut rng, max_chunk).min(stream.len() - pos);
            r.feed(&stream[pos..pos + n]);
            pos += n;
            match rng.below(100) {
                0..=1 => {
                    let (c, rr) = (rng.range(2, 60) as u16, rng.range(1, 20) as u16);
                    r.resize(c, rr);
                }
                2..=4 => {
                    // a client reconnects: resumes from what it holds, or starts from scratch
                    let i = rng.below(r.clients.len() as u64) as usize;
                    let c = &mut r.clients[i];
                    if rng.below(2) == 0 {
                        c.view = View::with_have_rev(c.cache.rev);
                    } else {
                        c.view = View::default();
                        c.cache = RowCache::new();
                    }
                }
                5 if r.emu.history_lo_abs() < r.emu.top_abs() => {
                    // lazy scrollback fetch, as the desktop does while the user scrolls
                    r.sync(true);
                    fill_history(&r.emu, &mut r.clients[0].cache);
                    check_history(&r.emu, &r.clients[0].cache, "live");
                }
                _ => {}
            }
        }
        r.done();
    }));
    if let Err(e) = result {
        eprintln!("FUZZ FAILURE {ctx}");
        std::panic::resume_unwind(e);
    }
}

#[test]
fn fuzz_clients_always_match_the_server() {
    for seed in 0..seeds() {
        fuzz_one(seed);
    }
}

/// Same bytes through a wrapper with scrollback 5/50 and one that never forgets: numbering must
/// agree, and `top_abs` must equal the number of rows alacritty itself holds in history.
fn oracle_one(seed: u64) {
    let mut rng = Lcg::new(seed ^ 0xABCD);
    let (cols, rows) = (rng.range(2, 50) as u16, rng.range(1, 16) as u16);
    let sb = *rng.pick(&[1usize, 5, 50]);
    let g = Gen {
        cols: u64::from(cols),
        rows: u64::from(rows),
        keep_history: true,
    };
    let frags = rng.range(400, 2500);
    let stream = gen_stream(&mut rng, frags, g);
    let max_chunk = *rng.pick(&[1u64, 7, 200, 4096]);
    let mut e = Emulator::new(cols, rows, sb);
    let mut huge = Emulator::new(cols, rows, 100_000);
    let mut single = Emulator::new(cols, rows, sb);
    let mut evs = Vec::new();
    let ctx = format!("oracle seed {seed}: {cols}x{rows} sb {sb}");
    let mut pos = 0;
    while pos < stream.len() {
        let n = chunk_len(&mut rng, max_chunk).min(stream.len() - pos);
        e.feed(&stream[pos..pos + n]);
        huge.feed(&stream[pos..pos + n]);
        evs.extend(e.take_events());
        pos += n;
        assert_eq!(
            e.top_abs(),
            huge.top_abs(),
            "{ctx}: top_abs after {pos} bytes"
        );
        assert_eq!(e.is_alt_screen(), huge.is_alt_screen(), "{ctx}");
        if !huge.is_alt_screen() {
            assert_eq!(
                huge.top_abs(),
                huge.history_len() as u64,
                "{ctx}: predicted pushes vs alacritty's history after {pos} bytes"
            );
            assert_eq!(
                e.history_len(),
                huge.history_len().min(sb),
                "{ctx}: capped history"
            );
        }
        assert_eq!(
            e.screen_text(),
            huge.screen_text(),
            "{ctx}: screens after {pos} bytes"
        );
        assert_eq!(e.cursor(), huge.cursor(), "{ctx}: cursor");
    }
    // the same rows, by number, in the part both still have
    let resolve = |em: &Emulator, p: &RowPatch| -> Vec<(Style, String)> {
        let t = em.styles();
        p.runs
            .iter()
            .map(|(id, s)| {
                (
                    if *id == 0 {
                        Style::default()
                    } else {
                        t[usize::from(*id) - 1].1
                    },
                    s.clone(),
                )
            })
            .collect()
    };
    let end = e.top_abs() + u64::from(rows);
    let (a, b) = (
        e.rows(e.history_lo_abs(), end),
        huge.rows(e.history_lo_abs(), end),
    );
    assert_eq!(a.len(), b.len(), "{ctx}");
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(
            (x.abs, x.wrapped, &x.wide_cols, resolve(&e, x)),
            (y.abs, y.wrapped, &y.wide_cols, resolve(&huge, y)),
            "{ctx}: row {}",
            x.abs
        );
    }
    // chunk boundaries do not matter: one big feed ends in the same state, with the same answers
    single.feed(&stream);
    let mut evs_single = single.take_events();
    evs_single.retain(|x| *x != EmuEvent::Bell);
    evs.retain(|x| *x != EmuEvent::Bell);
    assert_eq!(evs, evs_single, "{ctx}: events");
    assert_eq!(single.screen_text(), e.screen_text(), "{ctx}: single feed");
    assert_eq!(
        (
            single.top_abs(),
            single.cursor(),
            single.modes(),
            single.title()
        ),
        (e.top_abs(), e.cursor(), e.modes(), e.title()),
        "{ctx}"
    );
}

#[test]
fn fuzz_numbering_matches_an_emulator_that_never_forgets() {
    for seed in 0..seeds() {
        oracle_one(seed);
    }
}

// ---------------------------------------------------------------------------------------------
// the wrapper against a bare alacritty Term
// ---------------------------------------------------------------------------------------------

struct Size(usize, usize);

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.1
    }
    fn screen_lines(&self) -> usize {
        self.1
    }
    fn columns(&self) -> usize {
        self.0
    }
}

/// A `Term` fed straight from the parser: what the wrapper must be indistinguishable from.
struct Plain {
    term: Term<VoidListener>,
    proc: Processor,
}

impl Plain {
    fn new(cols: u16, rows: u16, scrollback: usize) -> Plain {
        let config = Config {
            scrolling_history: scrollback,
            kitty_keyboard: true,
            osc52: alacritty_terminal::term::Osc52::Disabled,
            ..Config::default()
        };
        Plain {
            term: Term::new(config, &Size(cols.into(), rows.into()), VoidListener),
            proc: Processor::new(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.proc.advance(&mut self.term, bytes);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(Size(cols.into(), rows.into()));
    }

    fn text(&self) -> String {
        let grid = self.term.grid();
        let mut out = Vec::new();
        for l in 0..self.term.screen_lines() {
            let mut s = String::new();
            for cell in &grid[Line(l as i32)][..] {
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                s.push(if cell.c.is_control() { ' ' } else { cell.c });
                if let Some(z) = cell.zerowidth() {
                    s.extend(z.iter());
                }
            }
            out.push(s.trim_end_matches(' ').to_string());
        }
        out.join("\n")
    }
}

fn same_as_plain(emu: &Emulator, plain: &Plain, ctx: &str) {
    assert_eq!(emu.screen_text(), plain.text(), "{ctx}: screen");
    let p = plain.term.grid().cursor.point;
    assert_eq!(
        (emu.cursor().row, emu.cursor().col),
        (p.line.0 as u16, p.column.0 as u16),
        "{ctx}: cursor"
    );
    assert_eq!(
        emu.is_alt_screen(),
        plain
            .term
            .mode()
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN),
        "{ctx}: alt"
    );
    if !emu.is_alt_screen() {
        assert_eq!(
            emu.history_len(),
            plain.term.grid().history_size(),
            "{ctx}: history"
        );
    }
}

#[test]
fn the_wrapper_forwards_everything_to_the_term() {
    for seed in 0..seeds() {
        let mut rng = Lcg::new(seed ^ 0x5151);
        let (cols, rows) = (rng.range(2, 50) as u16, rng.range(1, 16) as u16);
        let sb = *rng.pick(&[0usize, 5, 50]);
        let g = Gen {
            cols: u64::from(cols),
            rows: u64::from(rows),
            keep_history: false,
        };
        let stream = gen_stream(&mut rng, 1500, g);
        let (mut e, mut plain) = (Emulator::new(cols, rows, sb), Plain::new(cols, rows, sb));
        let mut pos = 0;
        while pos < stream.len() {
            let n = chunk_len(&mut rng, 200).min(stream.len() - pos);
            e.feed(&stream[pos..pos + n]);
            plain.feed(&stream[pos..pos + n]);
            pos += n;
            same_as_plain(&e, &plain, &format!("seed {seed} after {pos} bytes"));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// recorded sessions
// ---------------------------------------------------------------------------------------------

/// `(name, cols, rows, bytes)` for every `tests/recordings/NAME-COLSxROWS.bin`.
fn recordings() -> Vec<(String, u16, u16, Vec<u8>)> {
    let dir: PathBuf = [env!("CARGO_MANIFEST_DIR"), "tests", "recordings"]
        .iter()
        .collect();
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("tests/recordings exists") {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "bin") {
            let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
            let (name, size) = stem.rsplit_once('-').expect("NAME-COLSxROWS.bin");
            let (c, r) = size.split_once('x').expect("COLSxROWS");
            out.push((
                name.to_string(),
                c.parse().unwrap(),
                r.parse().unwrap(),
                std::fs::read(&path).unwrap(),
            ));
        }
    }
    out.sort();
    out
}

#[test]
fn there_are_at_least_ten_recordings() {
    let names: Vec<String> = recordings().into_iter().map(|r| r.0).collect();
    assert!(names.len() >= 10, "{names:?}");
}

/// Feed `bytes` in random chunks through a rig; also keep an unbounded emulator as oracle.
fn replay(
    name: &str,
    cols: u16,
    rows: u16,
    bytes: &[u8],
    max_chunk: u64,
    seed: u64,
) -> (Rig, bool, Stats) {
    let mut rng = Lcg::new(seed);
    let mut r = rig(cols, rows, 10_000);
    r.audit_every = 40;
    r.clients.push(Client::new("rare", 29));
    let mut huge = Emulator::new(cols, rows, 100_000);
    let (mut pos, mut saw_alt) = (0, false);
    let (mut last_history, mut cleared) = (0, false);
    while pos < bytes.len() {
        let n = chunk_len(&mut rng, max_chunk).min(bytes.len() - pos);
        r.feed(&bytes[pos..pos + n]);
        huge.feed(&bytes[pos..pos + n]);
        pos += n;
        saw_alt |= r.emu.is_alt_screen();
        assert_eq!(
            r.emu.top_abs(),
            huge.top_abs(),
            "{name}: numbering after {pos} bytes"
        );
        if !huge.is_alt_screen() {
            // The unbounded emulator never drops history, so a drop means the program cleared it
            // (ED 3): from then on its history size no longer equals the rows ever pushed.
            cleared |= huge.history_len() < last_history;
            last_history = huge.history_len();
            if !cleared {
                assert_eq!(
                    huge.top_abs(),
                    huge.history_len() as u64,
                    "{name}: predicted pushes vs alacritty's history"
                );
            }
        }
    }
    r.done();
    assert_eq!(r.emu.screen_text(), huge.screen_text(), "{name}");
    let st = r.clients[0].st;
    (r, saw_alt, st)
}

#[test]
fn recordings_rebuild_the_screen_at_every_frame() {
    for (name, cols, rows, bytes) in recordings() {
        for (max_chunk, seed) in [(16, 1), (700, 2), (8192, 3)] {
            let (r, saw_alt, st) = replay(&name, cols, rows, &bytes, max_chunk, seed);
            let text = r.emu.screen_text();
            match name.as_str() {
                "seq_5000" => {
                    assert_eq!(
                        r.texts(0, 5000),
                        (1..=5000).map(|i| i.to_string()).collect::<Vec<_>>(),
                        "every one of the 5000 lines, by number"
                    );
                    assert_eq!(r.emu.history_lo_abs(), 0);
                }
                "ls_usr" => assert!(text.contains("share") && text.contains("bin"), "{text}"),
                "cargo_help" => assert!(r.texts(0, 40).iter().any(|l| l.contains("Usage:"))),
                "git_log" => assert!(r.texts(0, 60).iter().any(|l| l.starts_with("commit "))),
                "stress" => assert!(text.contains("end"), "{text}"),
                "claude_tui" => {
                    let (lo, hi) = (r.emu.history_lo_abs(), r.emu.top_abs() + u64::from(rows));
                    assert!(r.texts(lo, hi).iter().any(|l| l.contains("Claude")));
                }
                "vim_services" | "less_nav" | "htop" | "nano" | "tmux" | "man_ls" => {
                    assert!(
                        bytes.windows(8).any(|w| w == b"\x1b[?1049h"),
                        "{name} should use the alternate screen"
                    );
                    assert!(!r.emu.is_alt_screen(), "{name} should have left it");
                    let _ = saw_alt;
                }
                _ => {}
            }
            if max_chunk == 700 {
                println!(
                    "recording {name:<14} {cols:>3}x{rows:<3} {:>6} bytes: frames {:>5} (snapshots {:>3}, deltas {:>5}), patches {:>6}, final top_abs {:>5}, history {:>5}",
                    bytes.len(),
                    st.frames,
                    st.snapshots,
                    st.deltas,
                    st.patches,
                    r.emu.top_abs(),
                    r.emu.history_len()
                );
            }
        }
    }
}

#[test]
fn recordings_through_the_wrapper_equal_a_bare_term() {
    for (name, cols, rows, bytes) in recordings() {
        let (mut e, mut plain) = (
            Emulator::new(cols, rows, 10_000),
            Plain::new(cols, rows, 10_000),
        );
        let mut rng = Lcg::new(7);
        let mut pos = 0;
        while pos < bytes.len() {
            let n = chunk_len(&mut rng, 1000).min(bytes.len() - pos);
            e.feed(&bytes[pos..pos + n]);
            plain.feed(&bytes[pos..pos + n]);
            pos += n;
            same_as_plain(&e, &plain, &format!("{name} after {pos} bytes"));
        }
    }
}

#[test]
fn a_recording_replayed_one_byte_at_a_time() {
    // The harshest chunking: every escape sequence is cut at every possible point.
    for (name, cols, rows, bytes) in recordings().into_iter().filter(|r| r.3.len() < 12_000) {
        let (r, _, st) = replay(&name, cols, rows, &bytes, 1, 11);
        assert!(st.frames as usize <= bytes.len() + 1);
        drop(r);
    }
}

#[test]
fn late_attach_to_a_recording_at_every_point() {
    // A client appears after k bytes with nothing, then follows the rest: it must converge.
    let (name, cols, rows, bytes) = recordings()
        .into_iter()
        .find(|r| r.0 == "stress")
        .expect("stress recording");
    let mut emu = Emulator::new(cols, rows, 1_000);
    let mut clients: Vec<Client> = Vec::new();
    let mut rng = Lcg::new(99);
    let mut pos = 0;
    while pos < bytes.len() {
        let n = chunk_len(&mut rng, 300).min(bytes.len() - pos);
        emu.feed(&bytes[pos..pos + n]);
        pos += n;
        if rng.below(6) == 0 {
            clients.push(Client::new(
                &format!("{name}@{pos}"),
                1 + rng.below(3) as usize,
            ));
        }
        for c in &mut clients {
            c.sync(&mut emu);
        }
    }
    for c in &mut clients {
        c.sync(&mut emu);
        fill_history(&emu, &mut c.cache);
        check_history(&emu, &c.cache, &c.name);
    }
    assert!(clients.len() > 10);
}

#[test]
fn recordings_survive_resize_storms() {
    // A window being dragged while the program runs: resizes between bursts of output, to sizes
    // from tiny to wider than the recording. The clients, the wrapper and a bare Term all agree.
    for (name, cols, rows, bytes) in recordings() {
        let mut rng = Lcg::new(0x5702 + bytes.len() as u64);
        let mut r = rig(cols, rows, 500);
        r.audit_every = 40;
        r.clients.push(Client::new("rare", 13));
        let mut plain = Plain::new(cols, rows, 500);
        let (mut pos, mut resizes) = (0, 0);
        while pos < bytes.len() {
            let n = chunk_len(&mut rng, 400).min(bytes.len() - pos);
            r.feed(&bytes[pos..pos + n]);
            plain.feed(&bytes[pos..pos + n]);
            pos += n;
            if rng.below(4) == 0 {
                let (c, rr) = (rng.range(2, 140) as u16, rng.range(1, 50) as u16);
                r.resize(c, rr);
                plain.resize(c, rr);
                resizes += 1;
            }
            let ctx = format!("{name} after {pos} bytes and {resizes} resizes");
            same_as_plain(&r.emu, &plain, &ctx);
        }
        r.done();
    }
}

/// Output in the style of Ink TUIs such as Claude Code: finished lines are printed once and
/// scroll away, while the live area under them (spinner, rule, prompt) is erased with
/// `eraseLines` (cursor up + erase line) and drawn again on every update. With `sync`, half of the
/// updates sit inside a synchronized update (DEC 2026).
fn ink_like(rng: &mut Lcg, cols: usize, renders: usize, sync: bool) -> Vec<u8> {
    fn text(rng: &mut Lcg, width: usize) -> String {
        let mut s = String::new();
        while s.chars().count() < width {
            s.push_str(rng.pick(WORDS));
            s.push(' ');
        }
        s
    }
    let mut out: Vec<u8> = Vec::new();
    // cursor hidden, bracketed paste, kitty keyboard, focus events: what such programs switch on
    out.extend_from_slice(b"\x1b[?25l\x1b[?2004h\x1b[>1u\x1b[?1004h");
    let mut live = 0;
    for i in 0..renders {
        let held = sync && rng.below(2) == 0;
        if held {
            out.extend_from_slice(b"\x1b[?2026h");
        }
        for _ in 1..live {
            out.extend_from_slice(b"\x1b[2K\x1b[1A");
        }
        if live > 0 {
            out.extend_from_slice(b"\x1b[2K\x1b[G");
        }
        for _ in 0..rng.below(4) {
            let width = rng.range(5, 2 * cols as u64) as usize; // sometimes wider than the screen
            let (r, g, b) = (rng.below(256), rng.below(256), rng.below(256));
            let cjk = if rng.below(5) == 0 {
                "日本語の文字 "
            } else {
                ""
            };
            let body = text(rng, width);
            write!(out, "\x1b[38;2;{r};{g};{b}m●\x1b[0m {cjk}{body}\r\n").unwrap();
        }
        live = rng.range(2, 6) as usize;
        for l in 0..live {
            if l > 0 {
                out.extend_from_slice(b"\r\n");
            }
            match l {
                0 => write!(
                    out,
                    "\x1b[38;5;208m{}\x1b[0m \x1b[2mThinking… ({i}s · esc to interrupt)\x1b[0m",
                    ["✻", "✽", "✶", "✳", "✢", "·"][i % 6]
                )
                .unwrap(),
                // a rule as wide as the screen leaves a wrap pending at the last column
                1 => write!(out, "\x1b[2m{}\x1b[0m", "─".repeat(cols)).unwrap(),
                _ => {
                    let width = rng.range(0, cols as u64 / 2) as usize;
                    let body = text(rng, width);
                    write!(out, "\x1b[1m>\x1b[0m {body}").unwrap();
                }
            }
        }
        if held {
            out.extend_from_slice(b"\x1b[?2026l");
        }
    }
    out
}

#[test]
fn ink_style_redraws_keep_clients_in_sync() {
    for seed in 0..4u64 {
        let mut rng = Lcg::new(0x1c0 + seed);
        let (cols, rows) = (rng.range(24, 100) as u16, rng.range(8, 30) as u16);
        for (max_chunk, renders) in [(1u64, 6usize), (60, 24), (4096, 24)] {
            // Without synchronized updates the stream can also be checked against an emulator
            // that never forgets (its numbering must match).
            let bytes = ink_like(&mut rng, usize::from(cols), renders, false);
            assert!(
                bytes.windows(4).any(|w| w == b"\x1b[1A"),
                "the live area is erased upward"
            );
            replay(
                &format!("ink {seed}"),
                cols,
                rows,
                &bytes,
                max_chunk,
                seed + 1,
            );
            // With them, the screen holds still until the update ends, whatever the clock says
            // about the time limit: the clients still follow the server exactly.
            let bytes = ink_like(&mut rng, usize::from(cols), renders, true);
            assert!(bytes.windows(8).any(|w| w == b"\x1b[?2026h") || renders < 10);
            let mut r = rig(cols, rows, 200);
            r.audit_every = 20;
            let mut pos = 0;
            while pos < bytes.len() {
                let n = chunk_len(&mut rng, max_chunk).min(bytes.len() - pos);
                r.feed(&bytes[pos..pos + n]);
                pos += n;
            }
            r.done();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// measurements: cargo test -p femme-term --release -- --ignored --nocapture
// ---------------------------------------------------------------------------------------------

/// The measurements time things, so they take turns instead of sharing the machine.
static MEASURE: Mutex<()> = Mutex::new(());

fn turn() -> std::sync::MutexGuard<'static, ()> {
    MEASURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `(p50, p99, max)`.
fn pct(mut v: Vec<Duration>) -> (Duration, Duration, Duration) {
    v.sort();
    (v[v.len() / 2], v[v.len() * 99 / 100], *v.last().unwrap())
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// 1/5/15 minute load average, so a noisy machine shows in the numbers.
fn load() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

/// About 1 MiB of ordinary output: lines of words; with `sgr`, many coloured and some bold.
fn block(sgr: bool) -> Vec<u8> {
    let mut rng = Lcg::new(42);
    let mut out = Vec::with_capacity((1 << 20) + 512);
    while out.len() < (1 << 20) {
        for _ in 0..rng.range(3, 14) {
            let coloured = sgr && rng.below(3) == 0;
            if coloured {
                write!(out, "\x1b[{}m", 31 + rng.below(7)).unwrap();
            }
            if sgr && rng.below(8) == 0 {
                out.extend_from_slice(b"\x1b[1m");
            }
            out.extend_from_slice(rng.pick(WORDS).as_bytes());
            for _ in 0..rng.below(6) {
                out.push(b'a' + rng.below(26) as u8);
            }
            if coloured {
                out.extend_from_slice(b"\x1b[0m");
            }
            out.push(b' ');
        }
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Every cell of a `cols` x `rows` screen rewritten (different on every `it`), without scrolling.
fn rewrite(it: usize, rows: usize, cols: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(rows * (cols + 40));
    for r in 0..rows {
        write!(b, "\x1b[{};1H", r + 1).unwrap();
        for c in 0..cols {
            if c % 25 == 0 {
                write!(b, "\x1b[{}m", 31 + (r + c / 25 + it) % 7).unwrap();
            }
            b.push(b'a' + ((r * 7 + c + it) % 26) as u8);
        }
        b.extend_from_slice(b"\x1b[0m");
    }
    b
}

#[test]
#[ignore = "measurement"]
fn throughput_100_mb() {
    let _turn = turn();
    let rounds = 100;
    let mbps = |bytes: usize, d: Duration| bytes as f64 / (1 << 20) as f64 / d.as_secs_f64();
    println!(
        "throughput of 100 MiB in 64 KiB feeds, 200x60, scrollback 10000 (load: {})",
        load()
    );

    let mut mixed_rate = 0.0;
    for (name, sgr) in [("text + SGR", true), ("plain text", false)] {
        let block = block(sgr);
        let total = block.len() * rounds;

        let mut e = Emulator::new(200, 60, 10_000);
        let t = Instant::now();
        for _ in 0..rounds {
            for chunk in block.chunks(64 << 10) {
                e.feed(chunk);
            }
        }
        let wrapped = mbps(total, t.elapsed());

        let mut plain = Plain::new(200, 60, 10_000);
        let t = Instant::now();
        for _ in 0..rounds {
            for chunk in block.chunks(64 << 10) {
                plain.feed(chunk);
            }
        }
        let bare = mbps(total, t.elapsed());
        println!(
            "  {name:<11} Emulator::feed {wrapped:>6.1} MiB/s, bare alacritty Term {bare:>6.1} MiB/s (wrapper at {:.0} %)",
            wrapped / bare * 100.0
        );
        assert!(
            wrapped >= 0.5 * bare,
            "the wrapper must stay within 2x of the bare Term"
        );
        if sgr {
            mixed_rate = wrapped;
        }
    }
    println!(
        "  target 100 MiB/s on text + SGR: {}",
        if mixed_rate >= 100.0 {
            "met"
        } else {
            "NOT met (bare alacritty is the limit)"
        }
    );

    // Every feed has a fixed cost (the pass over the visible cells), so small PTY reads hurt.
    println!("  text + SGR by feed size, 20 MiB each:");
    let block = block(true);
    for kib in [1usize, 4, 16, 64, 256] {
        let (mut e, mut plain) = (Emulator::new(200, 60, 10_000), Plain::new(200, 60, 10_000));
        let t = Instant::now();
        for _ in 0..20 {
            for chunk in block.chunks(kib << 10) {
                e.feed(chunk);
            }
        }
        let wrapped = mbps(block.len() * 20, t.elapsed());
        let t = Instant::now();
        for _ in 0..20 {
            for chunk in block.chunks(kib << 10) {
                plain.feed(chunk);
            }
        }
        let bare = mbps(block.len() * 20, t.elapsed());
        println!(
            "    {kib:>3} KiB feeds: Emulator::feed {wrapped:>6.1} MiB/s, bare {bare:>6.1} MiB/s ({:.0} %)",
            wrapped / bare * 100.0
        );
    }

    // What a holder does: one frame for one attached view after every PTY read.
    let mut e = Emulator::new(200, 60, 10_000);
    let mut view = e.new_view();
    let (mut t_feed, mut t_frame, mut frames) = (Duration::ZERO, Duration::ZERO, 0u32);
    for round in 0..20 {
        for chunk in block.chunks(64 << 10) {
            let t = Instant::now();
            e.feed(chunk);
            let fed = t.elapsed();
            let t = Instant::now();
            let f = e.frame(&mut view, EPOCH, 0, 0, None);
            // the first rounds fill the scrollback, which is allocation, not steady state
            if round >= 5 {
                t_feed += fed;
                t_frame += t.elapsed();
                frames += u32::from(f.is_some());
            }
        }
    }
    println!(
        "  feed + a frame after every 64 KiB (rounds 5..20): {:.1} MiB/s; per 64 KiB: feed {:.0} us, frame {:.0} us ({frames} frames)",
        mbps(block.len() * 15, t_feed + t_frame),
        us(t_feed) / f64::from(frames),
        us(t_frame) / f64::from(frames),
    );
}

#[test]
#[ignore = "measurement"]
fn frame_cost_200x60_with_10000_lines_of_scrollback() {
    let _turn = turn();
    let (cols, rows) = (200usize, 60usize);
    let mut e = Emulator::new(cols as u16, rows as u16, 10_000);
    let mut plain = Plain::new(cols as u16, rows as u16, 10_000);
    let mut fill = Vec::new();
    for i in 0..10_100 {
        write!(
            fill,
            "\x1b[3{}mline {i} {}\x1b[0m\r\n",
            i % 8,
            "x".repeat(150)
        )
        .unwrap();
    }
    e.feed(&fill);
    plain.feed(&fill);
    assert_eq!(e.history_len(), 10_000);

    let mut view = e.new_view();
    let mut cache = RowCache::new();
    cache
        .apply(&e.frame(&mut view, EPOCH, 0, 0, None).unwrap())
        .unwrap();

    // 1. every row changes on every iteration
    let (mut t_feed, mut t_plain, mut t_frame, mut t_apply, mut patches) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), 0);
    for it in 0..1000 {
        let bytes = rewrite(it, rows, cols);
        let t = Instant::now();
        plain.feed(&bytes);
        t_plain.push(t.elapsed());
        let t = Instant::now();
        e.feed(&bytes);
        t_feed.push(t.elapsed());
        let t = Instant::now();
        let f = e.frame(&mut view, EPOCH, 0, 0, None).unwrap();
        t_frame.push(t.elapsed());
        patches += f.row_patches.len();
        let t = Instant::now();
        cache.apply(&f).unwrap();
        t_apply.push(t.elapsed());
        assert_eq!(cache.text(), e.screen_text());
    }
    let (feed, bare, frame, apply) = (pct(t_feed), pct(t_plain), pct(t_frame), pct(t_apply));
    println!("200x60, 10000 lines of scrollback, a feed that rewrites all {rows} rows, 1000 iterations (microseconds, p50 / p99 / max):");
    println!(
        "  Emulator::feed (parse + change detection)  {:>8.1} {:>8.1} {:>8.1}",
        us(feed.0),
        us(feed.1),
        us(feed.2)
    );
    println!(
        "  bare alacritty parse of the same bytes     {:>8.1} {:>8.1} {:>8.1}",
        us(bare.0),
        us(bare.1),
        us(bare.2)
    );
    println!(
        "  => change detection (feed - bare)          {:>8.1}",
        us(feed.0) - us(bare.0)
    );
    println!(
        "  Emulator::frame ({} rows rendered)          {:>8.1} {:>8.1} {:>8.1}",
        patches / 1000,
        us(frame.0),
        us(frame.1),
        us(frame.2)
    );
    println!(
        "  RowCache::apply                            {:>8.1} {:>8.1} {:>8.1}",
        us(apply.0),
        us(apply.1),
        us(apply.2)
    );

    // 2. typing: one character, one changed row (a fresh line every 100 characters: no wrapping)
    let (mut t_feed, mut t_frame) = (Vec::new(), Vec::new());
    for i in 0..1000 {
        if i % 100 == 0 {
            e.feed(b"\x1b[30;1H\x1b[2K");
            cache
                .apply(&e.frame(&mut view, EPOCH, 0, 0, None).unwrap())
                .unwrap();
        }
        let t = Instant::now();
        e.feed(&[b'a' + (i % 26) as u8]);
        t_feed.push(t.elapsed());
        let t = Instant::now();
        let f = e.frame(&mut view, EPOCH, 0, 0, None).unwrap();
        t_frame.push(t.elapsed());
        assert_eq!(f.row_patches.len(), 1);
        cache.apply(&f).unwrap();
    }
    let (feed, frame) = (pct(t_feed), pct(t_frame));
    println!("one typed character (1 row changes):");
    println!(
        "  Emulator::feed                             {:>8.1} {:>8.1} {:>8.1}",
        us(feed.0),
        us(feed.1),
        us(feed.2)
    );
    println!(
        "  Emulator::frame                            {:>8.1} {:>8.1} {:>8.1}",
        us(frame.0),
        us(frame.1),
        us(frame.2)
    );

    // 3. a line of output at the bottom: one row scrolls into history, one row appears
    let (mut t_feed, mut t_frame, mut n_patches) = (Vec::new(), Vec::new(), 0);
    for i in 0..1000 {
        let t = Instant::now();
        e.feed(format!("\x1b[32moutput line {i}\x1b[0m\r\n").as_bytes());
        t_feed.push(t.elapsed());
        let t = Instant::now();
        let f = e.frame(&mut view, EPOCH, 0, 0, None).unwrap();
        t_frame.push(t.elapsed());
        n_patches += f.row_patches.len();
        cache.apply(&f).unwrap();
    }
    let (feed, frame) = (pct(t_feed), pct(t_frame));
    println!(
        "one line of output scrolling the screen ({:.1} row patches per frame):",
        n_patches as f64 / 1000.0
    );
    println!(
        "  Emulator::feed                             {:>8.1} {:>8.1} {:>8.1}",
        us(feed.0),
        us(feed.1),
        us(feed.2)
    );
    println!(
        "  Emulator::frame                            {:>8.1} {:>8.1} {:>8.1}",
        us(frame.0),
        us(frame.1),
        us(frame.2)
    );

    // 4. a client attaching: full snapshot
    let mut t_snap = Vec::new();
    for _ in 0..200 {
        let mut v = e.new_view();
        let t = Instant::now();
        let f = e.frame(&mut v, EPOCH, 0, 0, None).unwrap();
        t_snap.push(t.elapsed());
        assert!(f.row_patches.len() >= rows - 1);
    }
    let snap = pct(t_snap);
    println!(
        "snapshot for a new client (whole screen + style table): {:>8.1} {:>8.1} {:>8.1}",
        us(snap.0),
        us(snap.1),
        us(snap.2)
    );

    // 5. GetRows for 100 scrollback rows
    let mut t_rows = Vec::new();
    for i in 0..200 {
        let from = e.history_lo_abs() + (i * 37) as u64 % 9_000;
        let t = Instant::now();
        let r = e.rows(from, from + 100);
        t_rows.push(t.elapsed());
        assert_eq!(r.len(), 100);
    }
    let rows100 = pct(t_rows);
    println!(
        "rows(from, from + 100) for scrollback:                  {:>8.1} {:>8.1} {:>8.1}",
        us(rows100.0),
        us(rows100.1),
        us(rows100.2)
    );
}

#[test]
#[ignore = "measurement"]
fn resize_cost_with_full_scrollback() {
    let _turn = turn();
    let mut e = Emulator::new(200, 60, 10_000);
    let mut fill = Vec::new();
    for i in 0..10_100 {
        write!(fill, "line {i} {}\r\n", "y".repeat(40 + i % 120)).unwrap();
    }
    e.feed(&fill);
    for (cols, rows) in [
        (199, 60),
        (200, 50),
        (120, 40),
        (200, 60),
        (80, 24),
        (200, 60),
    ] {
        let t = Instant::now();
        e.resize(cols, rows);
        let d = t.elapsed();
        let mut v = e.new_view();
        let t = Instant::now();
        let f = e.frame(&mut v, EPOCH, 0, 0, None).unwrap();
        println!(
            "resize to {cols}x{rows}: {:>7.2} ms, then snapshot {:>6.1} us ({} rows)",
            d.as_secs_f64() * 1e3,
            us(t.elapsed()),
            f.row_patches.len()
        );
    }
}

fn vm_rss_kib() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").expect("linux");
    let line = s.lines().find(|l| l.starts_with("VmRSS:")).expect("VmRSS");
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
#[ignore = "measurement"]
fn memory_per_pane() {
    const CASES: [(usize, usize, usize); 3] =
        [(80, 24, 10_000), (200, 50, 10_000), (200, 50, 2_000)];
    let Ok(case) = std::env::var("P1_RSS_CASE") else {
        // One process per case: the allocator keeps what was freed, so a second case in the same
        // process would read as nothing.
        let _turn = turn();
        println!(
            "size_of::<alacritty Cell>() = {} bytes",
            std::mem::size_of::<alacritty_terminal::term::cell::Cell>()
        );
        for case in 0..CASES.len() {
            let out = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "memory_per_pane",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("P1_RSS_CASE", case.to_string())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some((_, report)) = line.split_once("RSS|") {
                    println!("{report}");
                }
            }
        }
        return;
    };
    let (cols, rows, sb) = CASES[case.parse::<usize>().unwrap()];
    let mut bytes = Vec::new();
    for i in 0..sb + 100 {
        write!(
            bytes,
            "\x1b[3{}m{i} {}\x1b[0m\r\n",
            i % 8,
            "z".repeat(cols - 8)
        )
        .unwrap();
    }
    let base = vm_rss_kib();
    let mut e = Emulator::new(cols as u16, rows as u16, sb);
    let fresh = vm_rss_kib();
    e.feed(&bytes);
    let full = vm_rss_kib();
    let mut cache = RowCache::new();
    let mut view = e.new_view();
    cache
        .apply(&e.frame(&mut view, EPOCH, 0, 0, None).unwrap())
        .unwrap();
    fill_history(&e, &mut cache);
    let client = vm_rss_kib();
    println!(
        "RSS|{cols}x{rows}, scrollback {sb}: empty emulator +{} KiB; full scrollback +{:.1} MiB ({:.0} B per history row); a client holding all {} rows +{:.1} MiB",
        fresh.saturating_sub(base),
        full.saturating_sub(base) as f64 / 1024.0,
        full.saturating_sub(base) as f64 * 1024.0 / sb as f64,
        e.history_len(),
        client.saturating_sub(full) as f64 / 1024.0
    );
}
