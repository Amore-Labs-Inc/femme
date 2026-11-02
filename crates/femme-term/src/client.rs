//! Client-side model of one terminal pane: a cache of rows keyed by absolute row number that
//! [`Frame`]s are applied to. No alacritty dependency, so the desktop and the phone can use it.
//!
//! Rules (they mirror what the server's `Emulator::frame` produces):
//! - a frame with `base_rev == 0` is a snapshot: the previous visible rows and the style table are
//!   dropped first, then the frame's rows and styles are applied;
//! - any other frame must have `base_rev == self.rev`, otherwise the caller re-opens the pane
//!   with `have_rev = self.rev` ([`ApplyError::NeedResync`]);
//! - on a primary-screen frame, rows older than `history_lo_abs` are dropped (the server no longer
//!   has them either); alt-screen frames report no scrollback and leave the cache alone;
//! - rows from a `TermDown::Rows` answer ([`RowCache::add_rows`]) only ever fill scrollback. The
//!   visible area belongs to frames.

use std::collections::BTreeMap;
use std::fmt;

use femme_proto::term::{Cursor, Frame, Modes, RowPatch, Screen, Style};

/// Why [`RowCache::apply`] refused a frame. The cache is left exactly as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// The cache holds a different rev than the frame's `base_rev` (a frame was lost or skipped):
    /// re-send `TermUp::Open` with `have_rev`.
    NeedResync {
        /// The rev the cache holds (0 for a cache that never applied anything).
        have_rev: u64,
    },
    /// The frame comes from another holder instance (the PTY was recreated). Drop this cache
    /// (`*cache = RowCache::new()`) and open the pane again with `have_rev = 0`.
    EpochChanged,
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyError::NeedResync { have_rev } => {
                write!(f, "frame does not follow rev {have_rev}: resync")
            }
            ApplyError::EpochChanged => f.write_str("frame is from another holder epoch: reset"),
        }
    }
}

impl std::error::Error for ApplyError {}

/// What the client knows about one pane. The public fields are the scalar state of the last
/// applied frame; rows and styles are behind accessors.
#[derive(Clone, Debug, Default)]
pub struct RowCache {
    /// Holder instance of the last applied frame (0 until the first one).
    pub epoch: u64,
    /// Rev reached by the last applied frame (0 until the first one).
    pub rev: u64,
    /// Terminal width in columns.
    pub cols: u16,
    /// Terminal height in rows (the number of rows [`RowCache::visible`] yields).
    pub rows: u16,
    /// Absolute row number of the first visible row.
    pub top_abs: u64,
    /// Oldest scrollback row the server still has (rows below it are dropped from the cache).
    pub history_lo_abs: u64,
    /// Cursor, relative to the visible screen.
    pub cursor: Cursor,
    /// Live terminal modes.
    pub modes: Modes,
    /// Window title, if the program set one.
    pub title: Option<String>,
    /// Which screen the visible rows belong to.
    pub screen: Screen,
    /// Highest input seq the holder had written to the PTY.
    pub applied_seq: u32,
    /// Highest input seq whose effect the last frame reflects.
    pub echo_seq: u32,
    /// `styles[id - 1]` is style `id`; id 0 is the default style and is not stored.
    styles: Vec<Style>,
    cache: BTreeMap<u64, RowPatch>,
}

impl RowCache {
    /// An empty cache: the first frame it accepts must be a snapshot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a frame, atomically: on `Err` nothing has changed.
    pub fn apply(&mut self, f: &Frame) -> Result<(), ApplyError> {
        if self.rev != 0 && f.epoch != self.epoch {
            return Err(ApplyError::EpochChanged);
        }
        let snapshot = f.base_rev == 0;
        if !snapshot && f.base_rev != self.rev {
            return Err(ApplyError::NeedResync { have_rev: self.rev });
        }

        if snapshot {
            let lo = self.top_abs;
            let hi = lo.saturating_add(u64::from(self.rows));
            let doomed: Vec<u64> = self.cache.range(lo..hi).map(|(k, _)| *k).collect();
            for k in doomed {
                self.cache.remove(&k);
            }
            self.styles.clear();
        }
        for &(id, style) in &f.new_styles {
            if id != 0 {
                let i = usize::from(id) - 1;
                if self.styles.len() <= i {
                    self.styles.resize(i + 1, Style::default());
                }
                self.styles[i] = style;
            }
        }
        for p in &f.row_patches {
            self.cache.insert(p.abs, p.clone());
        }
        // Rows that scrolled off since `base_rev` and are still unknown were blank (the server
        // sends a pushed row only if the client does not already hold its final content, and
        // never sends blank rows in a snapshot): history has no holes behind a delta.
        if !snapshot && f.screen == Screen::Primary && self.screen == Screen::Primary {
            for abs in self.top_abs..f.top_abs.min(self.top_abs + u64::from(self.rows)) {
                self.cache.entry(abs).or_insert_with(|| RowPatch {
                    abs,
                    ..RowPatch::default()
                });
            }
        }

        self.epoch = f.epoch;
        self.rev = f.rev;
        self.cols = f.cols;
        self.rows = f.rows;
        self.top_abs = f.top_abs;
        self.history_lo_abs = f.history_lo_abs;
        self.cursor = f.cursor;
        self.modes = f.modes;
        self.title.clone_from(&f.title);
        self.screen = f.screen;
        self.applied_seq = f.applied_seq;
        self.echo_seq = f.echo_seq;

        if f.screen == Screen::Primary {
            while let Some(first) = self.cache.first_entry() {
                if *first.key() < self.history_lo_abs {
                    first.remove();
                } else {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Merge scrollback rows from a `TermDown::Rows` answer. Rows older than `history_lo_abs` or at
    /// or after `top_abs` are ignored: the visible area is owned by frames, so a late answer can
    /// never overwrite a newer frame's rows.
    pub fn add_rows(&mut self, rows: Vec<RowPatch>) {
        for r in rows {
            if r.abs >= self.history_lo_abs && r.abs < self.top_abs {
                self.cache.insert(r.abs, r);
            }
        }
    }

    /// The first row number in `[from, to)` the cache does not hold, or `None` when it holds them
    /// all. An answer to `TermUp::GetRows` can stop short (see `Emulator::rows`): this is where to
    /// ask again.
    pub fn missing_after(&self, from: u64, to: u64) -> Option<u64> {
        (from..to).find(|abs| !self.cache.contains_key(abs))
    }

    /// Style for `id` (id 0, and any id the server never defined, is the default style).
    pub fn style(&self, id: u16) -> Style {
        match usize::from(id).checked_sub(1) {
            Some(i) => self.styles.get(i).copied().unwrap_or_default(),
            None => Style::default(),
        }
    }

    /// The cached row with absolute number `abs`, if the client has it.
    pub fn row(&self, abs: u64) -> Option<&RowPatch> {
        self.cache.get(&abs)
    }

    /// Exactly `rows` entries, from `top_abs` down. `None` is a row the server never sent: a blank
    /// row (snapshots leave blank rows out).
    pub fn visible(&self) -> impl Iterator<Item = Option<&RowPatch>> + '_ {
        (0..u64::from(self.rows)).map(move |i| self.cache.get(&(self.top_abs + i)))
    }

    /// The visible screen as plain text: byte-identical to `Emulator::screen_text()` for the same
    /// screen (each row via [`row_text`], blank rows empty, joined by `\n`, no trailing newline).
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (i, row) in self.visible().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            if let Some(r) = row {
                out.push_str(&row_text(r));
            }
        }
        out
    }
}

/// Plain text of one row: the runs concatenated, trailing blanks trimmed. Wide characters appear
/// once (their spacer column is in `wide_cols`), combining marks follow their base character.
pub fn row_text(p: &RowPatch) -> String {
    let mut s = String::with_capacity(p.runs.iter().map(|r| r.1.len()).sum());
    for (_, t) in &p.runs {
        s.push_str(t);
    }
    s.truncate(s.trim_end_matches(' ').len());
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use femme_proto::term::{Attrs, Color};

    fn row(abs: u64, text: &str) -> RowPatch {
        RowPatch {
            abs,
            wrapped: false,
            runs: vec![(0, text.to_string())],
            wide_cols: vec![],
        }
    }

    fn snap(rev: u64, top: u64, rows: u16, patches: Vec<RowPatch>) -> Frame {
        Frame {
            epoch: 7,
            rev,
            base_rev: 0,
            cols: 10,
            rows,
            top_abs: top,
            history_lo_abs: 0,
            row_patches: patches,
            ..Frame::default()
        }
    }

    fn delta(base: u64, rev: u64, top: u64, rows: u16, patches: Vec<RowPatch>) -> Frame {
        Frame {
            epoch: 7,
            rev,
            base_rev: base,
            cols: 10,
            rows,
            top_abs: top,
            history_lo_abs: 0,
            row_patches: patches,
            ..Frame::default()
        }
    }

    #[test]
    fn missing_after_names_where_to_ask_again() {
        let mut c = RowCache::new();
        c.apply(&snap(1, 10, 2, vec![row(10, "v")])).unwrap();
        c.add_rows(vec![row(3, "x"), row(4, "y"), row(6, "z")]);
        assert_eq!(c.missing_after(3, 8), Some(5));
        assert_eq!(c.missing_after(3, 5), None);
        assert_eq!(c.missing_after(6, 7), None);
        assert_eq!(c.missing_after(0, 4), Some(0));
        assert_eq!(c.missing_after(8, 8), None, "an empty range misses nothing");
    }

    #[test]
    fn wrong_base_rev_needs_resync_and_changes_nothing() {
        let mut c = RowCache::new();
        c.apply(&snap(3, 0, 2, vec![row(0, "a"), row(1, "b")]))
            .unwrap();
        let err = c.apply(&delta(5, 6, 0, 2, vec![row(0, "x")])).unwrap_err();
        assert_eq!(err, ApplyError::NeedResync { have_rev: 3 });
        assert_eq!(c.rev, 3);
        assert_eq!(c.text(), "a\nb");
        // a fresh cache cannot take a delta either
        let err = RowCache::new()
            .apply(&delta(1, 2, 0, 2, vec![]))
            .unwrap_err();
        assert_eq!(err, ApplyError::NeedResync { have_rev: 0 });
    }

    #[test]
    fn epoch_change_is_refused_and_a_fresh_cache_adopts_any_epoch() {
        let mut c = RowCache::new();
        c.apply(&snap(3, 0, 1, vec![row(0, "old")])).unwrap();
        let mut f = snap(1, 0, 1, vec![row(0, "new")]);
        f.epoch = 8;
        assert_eq!(c.apply(&f), Err(ApplyError::EpochChanged));
        assert_eq!((c.epoch, c.rev, c.text().as_str()), (7, 3, "old"));
        let mut fresh = RowCache::new();
        fresh.apply(&f).unwrap();
        assert_eq!((fresh.epoch, fresh.text().as_str()), (8, "new"));
    }

    #[test]
    fn snapshot_resets_styles_and_the_old_visible_rows() {
        let bold = Style {
            attrs: Attrs::BOLD,
            ..Style::default()
        };
        let red = Style {
            fg: Color::Indexed(1),
            ..Style::default()
        };
        let mut c = RowCache::new();
        let mut f = snap(2, 0, 2, vec![row(0, "a"), row(1, "b")]);
        f.new_styles = vec![(1, bold), (2, red)];
        c.apply(&f).unwrap();
        assert_eq!(
            (c.style(1), c.style(2), c.style(9)),
            (bold, red, Style::default())
        );

        // Snapshot at a new position with only one style: old visible rows and old styles are gone.
        let mut f = snap(5, 10, 2, vec![row(11, "z")]);
        f.new_styles = vec![(1, red)];
        c.apply(&f).unwrap();
        assert_eq!(c.style(1), red);
        assert_eq!(
            c.style(2),
            Style::default(),
            "style 2 died with the snapshot"
        );
        assert!(c.row(0).is_none() && c.row(1).is_none());
        assert_eq!(c.text(), "\nz");
    }

    #[test]
    fn snapshot_keeps_scrollback_above_the_old_visible_range() {
        let mut c = RowCache::new();
        c.apply(&snap(1, 5, 2, vec![row(5, "v5"), row(6, "v6")]))
            .unwrap();
        // the screen scrolled by 3: rows 5 and 6 are scrollback now; a delta brings 5 as history
        c.apply(&delta(1, 2, 8, 2, vec![row(5, "v5"), row(8, "v8")]))
            .unwrap();
        assert_eq!(c.row(5).map(row_text).as_deref(), Some("v5"));
        // snapshot at the same top: drops only the old visible rows [8, 10)
        c.apply(&snap(3, 8, 2, vec![row(9, "n9")])).unwrap();
        assert!(c.row(8).is_none());
        assert_eq!(c.row(5).map(row_text).as_deref(), Some("v5"));
        assert_eq!(c.text(), "\nn9");
    }

    #[test]
    fn delta_merges_rows_and_styles() {
        let ul = Style {
            attrs: Attrs::UNDERLINE,
            ..Style::default()
        };
        let mut c = RowCache::new();
        c.apply(&snap(1, 0, 3, vec![row(0, "one"), row(1, "two")]))
            .unwrap();
        let mut f = delta(1, 2, 0, 3, vec![row(1, "TWO"), row(2, "three")]);
        f.new_styles = vec![(1, ul)];
        f.title = Some("t".into());
        f.echo_seq = 4;
        f.applied_seq = 5;
        c.apply(&f).unwrap();
        assert_eq!(c.text(), "one\nTWO\nthree");
        assert_eq!(
            (
                c.rev,
                c.style(1),
                c.title.as_deref(),
                c.echo_seq,
                c.applied_seq
            ),
            (2, ul, Some("t"), 4, 5)
        );
    }

    #[test]
    fn seq_only_frame_keeps_rev() {
        let mut c = RowCache::new();
        c.apply(&snap(4, 0, 1, vec![row(0, "x")])).unwrap();
        let mut f = delta(4, 4, 0, 1, vec![]);
        f.echo_seq = 9;
        c.apply(&f).unwrap();
        assert_eq!((c.rev, c.echo_seq, c.text().as_str()), (4, 9, "x"));
    }

    #[test]
    fn add_rows_only_fills_scrollback() {
        let mut c = RowCache::new();
        let mut f = snap(1, 10, 2, vec![row(10, "v10"), row(11, "v11")]);
        f.history_lo_abs = 4;
        c.apply(&f).unwrap();
        c.add_rows(vec![
            row(3, "too old"),
            row(4, "h4"),
            row(9, "h9"),
            row(10, "stale visible"),
            row(40, "future"),
        ]);
        assert!(c.row(3).is_none());
        assert_eq!(c.row(4).map(row_text).as_deref(), Some("h4"));
        assert_eq!(c.row(9).map(row_text).as_deref(), Some("h9"));
        assert_eq!(
            c.row(10).map(row_text).as_deref(),
            Some("v10"),
            "frames own the visible area"
        );
        assert!(c.row(40).is_none());
    }

    #[test]
    fn primary_frames_drop_old_history_alt_frames_do_not() {
        let mut c = RowCache::new();
        let mut f = snap(1, 10, 1, vec![row(10, "v")]);
        f.history_lo_abs = 2;
        c.apply(&f).unwrap();
        c.add_rows(vec![row(2, "a"), row(3, "b"), row(4, "c")]);
        let mut f = delta(1, 2, 10, 1, vec![]);
        f.history_lo_abs = 4;
        c.apply(&f).unwrap();
        assert!(c.row(2).is_none() && c.row(3).is_none());
        assert!(c.row(4).is_some());
        // alt screen: the server reports no scrollback, the cache keeps what it has
        let mut f = snap(0, 10, 1, vec![]);
        f.rev = 3;
        f.screen = Screen::Alt;
        f.history_lo_abs = 10;
        c.apply(&f).unwrap();
        assert!(c.row(4).is_some());
        // back on primary with the old history: still there
        let mut f = snap(0, 10, 1, vec![row(10, "v")]);
        f.rev = 4;
        f.history_lo_abs = 4;
        c.apply(&f).unwrap();
        assert!(c.row(4).is_some());
    }

    #[test]
    fn visible_yields_exactly_rows_entries() {
        let mut c = RowCache::new();
        assert_eq!(c.visible().count(), 0);
        c.apply(&snap(1, 100, 5, vec![row(101, "b"), row(104, "e")]))
            .unwrap();
        let v: Vec<Option<String>> = c.visible().map(|r| r.map(row_text)).collect();
        assert_eq!(
            v,
            vec![None, Some("b".into()), None, None, Some("e".into())]
        );
        assert_eq!(c.text(), "\nb\n\n\ne");
    }

    #[test]
    fn text_trims_blanks_and_keeps_wide_chars_once() {
        let p = RowPatch {
            abs: 0,
            wrapped: false,
            runs: vec![(0, "a ".into()), (3, "界  ".into())],
            wide_cols: vec![3],
        };
        assert_eq!(row_text(&p), "a 界");
        let blank = RowPatch {
            abs: 0,
            wrapped: false,
            runs: vec![(2, "   ".into())],
            wide_cols: vec![],
        };
        assert_eq!(row_text(&blank), "");
    }
}
