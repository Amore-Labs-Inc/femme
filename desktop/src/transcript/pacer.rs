//! Token pacer: reveals streamed assistant text at a steady 12-20 words per
//! second, whether it arrives one word at a time or in a 200-word burst.
//!
//! Arrival and reveal are decoupled: the stream calls [`Pacer::push`] with
//! every chunk as it lands, and the UI calls [`Pacer::tick`] once per frame
//! with the frame delta, then draws [`Pacer::revealed`].
//!
//! - **Words.** A word unit is optional leading whitespace followed by a run
//!   of non-whitespace chars ([`char::is_whitespace`]). Whitespace is revealed
//!   with the word after it, so `revealed()` never ends in whitespace and
//!   newlines survive verbatim. The last unit stays hidden until whitespace
//!   follows it, because a chunk may end mid-word (`"hel"`, then
//!   `"lo world"`); [`Pacer::finish`] flushes it when the turn ends.
//! - **Rate.** Words per second rise linearly with the backlog of complete,
//!   unrevealed words: `min_wps` at 0-1 words, `max_wps` at 40 or more. A
//!   burst drains a bit faster, never faster than `max_wps`.
//! - **Credit.** Sub-word progress carries across frames as a fractional
//!   credit, dropped whenever nothing is pending so idle time cannot bank a
//!   burst.
//! - **Cost.** `push` is amortised O(pushed bytes). `tick` never allocates
//!   and is amortised O(revealed bytes): the backlog is counted on push, and
//!   a tick only scans the words it reveals.

use std::time::Duration;

/// Backlog (complete, unrevealed words) up to which the pacer runs at `min_wps`.
const MIN_RATE_BACKLOG: usize = 1;
/// Backlog from which the pacer runs at `max_wps`.
const MAX_RATE_BACKLOG: usize = 40;

/// Paces the reveal of one streamed message (see the module docs).
#[derive(Debug, Clone)]
pub struct Pacer {
    /// Everything pushed so far.
    buf: String,
    /// Byte offset right after the last revealed word: `revealed()` is `buf[..end]`.
    end: usize,
    /// Complete words in `buf[end..]`: the backlog.
    pending: usize,
    /// Words earned but not revealed yet; within `[0, 1)` between ticks.
    credit: f32,
    /// Words per second at a backlog of `MIN_RATE_BACKLOG` or less.
    min_wps: f32,
    /// Words per second at a backlog of `MAX_RATE_BACKLOG` or more.
    max_wps: f32,
}

impl Pacer {
    /// Creates an empty pacer that reveals `min_wps` to `max_wps` words per
    /// second; the UI uses `new(12.0, 20.0)`.
    ///
    /// # Panics
    ///
    /// In debug builds, unless `0 < min_wps <= max_wps` and both are finite.
    pub fn new(min_wps: f32, max_wps: f32) -> Self {
        debug_assert!(
            min_wps > 0.0 && min_wps <= max_wps && max_wps.is_finite(),
            "Pacer::new({min_wps}, {max_wps}): need 0 < min_wps <= max_wps < inf"
        );
        Self {
            buf: String::new(),
            end: 0,
            pending: 0,
            credit: 0.0,
            min_wps,
            max_wps,
        }
    }

    /// Appends streamed text as it arrives: any chunk size, and a chunk may
    /// start or end in the middle of a word. See [`finish`](Self::finish) for
    /// text pushed after the turn ended.
    pub fn push(&mut self, text: &str) {
        // Does the buffer end inside an unrevealed word, which the next
        // whitespace completes? Not when `finish` already revealed that tail.
        let mut in_word = self.end < self.buf.len()
            && self
                .buf
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace());
        for c in text.chars() {
            if !c.is_whitespace() {
                in_word = true;
            } else if in_word {
                self.pending += 1;
                in_word = false;
            }
        }
        self.buf.push_str(text);
    }

    /// Advances the reveal by one frame of `dt` and returns how many words it
    /// revealed: usually 0 or 1 at 120 Hz, and none for `Duration::ZERO`.
    pub fn tick(&mut self, dt: Duration) -> usize {
        if self.pending == 0 {
            self.credit = 0.0;
            return 0;
        }
        self.credit += self.rate() * dt.as_secs_f32();
        // Truncating the non-negative credit is `floor`, and `as` saturates,
        // so an absurd `dt` just reveals the whole backlog.
        let n = (self.credit as usize).min(self.pending);
        for _ in 0..n {
            self.end = self.unit_end(self.end);
        }
        self.pending -= n;
        self.credit = if self.pending == 0 {
            0.0
        } else {
            self.credit - n as f32
        };
        n
    }

    /// The text revealed so far: a prefix of everything pushed that ends
    /// right after the last revealed word, never inside a word and never in
    /// trailing whitespace.
    pub fn revealed(&self) -> &str {
        &self.buf[..self.end]
    }

    /// Whether no complete word is waiting (a trailing partial word may be).
    pub fn is_caught_up(&self) -> bool {
        self.pending == 0
    }

    /// Ends the turn: reveals everything pushed so far at once, including a
    /// trailing partial word, so `revealed()` is the pushed text with its
    /// trailing whitespace trimmed.
    ///
    /// This is a one-off flush, not a final state: text pushed afterwards is
    /// paced like any other. If it continues the flushed partial word without
    /// whitespace (`push("hel")`, `finish()`, `push("lo world")`), the
    /// continuation counts as a word of its own (`"lo"`), so `revealed()`
    /// shows `"hel"` until that continuation is revealed.
    pub fn finish(&mut self) {
        // `buf[..end]` already ends in non-whitespace, so `end` never moves back.
        self.end = self.buf.trim_end().len();
        self.pending = 0;
        self.credit = 0.0;
    }

    /// Words per second for the current backlog.
    fn rate(&self) -> f32 {
        let t = self.pending.saturating_sub(MIN_RATE_BACKLOG) as f32
            / (MAX_RATE_BACKLOG - MIN_RATE_BACKLOG) as f32;
        self.min_wps + (self.max_wps - self.min_wps) * t.min(1.0)
    }

    // ponytail: words are whitespace-delimited, so unspaced scripts (CJK,
    // Thai) reveal a whole run at once; upgrade path is UAX #29 word
    // boundaries (unicode-segmentation) here and in `push`.
    /// Byte offset right after the word unit that starts at `from`: its
    /// leading whitespace, then its run of non-whitespace chars.
    fn unit_end(&self, from: usize) -> usize {
        let rest = &self.buf[from..];
        let start = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        let len = rest[start..]
            .find(char::is_whitespace)
            .unwrap_or(rest.len() - start);
        from + start + len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: f32 = 12.0;
    const MAX: f32 = 20.0;
    /// One frame at 120 Hz.
    const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 120);

    fn pacer() -> Pacer {
        Pacer::new(MIN, MAX)
    }

    /// Ticks frame by frame until no complete word is pending; returns the
    /// number of frames it took.
    fn drain(p: &mut Pacer) -> usize {
        let mut frames = 0;
        while !p.is_caught_up() {
            p.tick(FRAME);
            frames += 1;
            assert!(frames < 100_000, "never drained");
        }
        frames
    }

    /// Runs `secs` seconds of frames, calling `feed` with the frame index
    /// before each tick. Returns, per frame, the words revealed and whether a
    /// complete word was pending going in.
    fn simulate(
        p: &mut Pacer,
        secs: usize,
        mut feed: impl FnMut(usize, &mut Pacer),
    ) -> Vec<(usize, bool)> {
        let mut log = Vec::with_capacity(secs * 120);
        for i in 0..secs * 120 {
            feed(i, p);
            let busy = !p.is_caught_up();
            log.push((p.tick(FRAME), busy));
        }
        log
    }

    /// Asserts the per-frame cap and that every second with a backlog
    /// throughout revealed `MIN..=MAX` words (±1 for credit carried over the
    /// window edges). Returns the total revealed.
    fn assert_paced(log: &[(usize, bool)]) -> usize {
        let cap = (MAX * FRAME.as_secs_f32()).ceil() as usize + 1;
        for (i, &(n, _)) in log.iter().enumerate() {
            assert!(n <= cap, "frame {i}: {n} words, cap {cap}");
        }
        for (i, second) in log.windows(120).enumerate() {
            if second.iter().all(|&(_, busy)| busy) {
                let n: usize = second.iter().map(|&(n, _)| n).sum();
                let ok = MIN as usize - 1..=MAX as usize + 1;
                assert!(ok.contains(&n), "{n} words in the second from frame {i}");
            }
        }
        log.iter().map(|&(n, _)| n).sum()
    }

    /// Complete words in `text`: non-whitespace chars followed by whitespace.
    fn complete_words(text: &str) -> usize {
        text.chars()
            .zip(text.chars().skip(1))
            .filter(|&(a, b)| !a.is_whitespace() && b.is_whitespace())
            .count()
    }

    #[test]
    fn rate_rises_linearly_with_backlog() {
        let rate_at = |backlog: usize| {
            let mut p = pacer();
            p.push(&"w ".repeat(backlog));
            p.rate()
        };
        for (backlog, want) in [
            (0, MIN),
            (1, MIN),
            (20, MIN + (MAX - MIN) * 19.0 / 39.0),
            (40, MAX),
            (1000, MAX),
        ] {
            let got = rate_at(backlog);
            assert!(
                (got - want).abs() < 1e-4,
                "backlog {backlog}: {got}, want {want}"
            );
        }
    }

    #[test]
    fn tiny_burst_then_trickle_stays_within_rate_bounds() {
        let mut p = pacer();
        p.push("one two three ");
        // Then one word every 8 frames: 15 words/s, between MIN and MAX.
        let log = simulate(&mut p, 10, |i, p| {
            if i % 8 == 7 {
                p.push("word ");
            }
        });
        assert!(log.iter().all(|&(_, busy)| busy), "backlog ran dry");
        let total = assert_paced(&log);
        let ok = 10 * MIN as usize - 1..=10 * MAX as usize + 1;
        assert!(ok.contains(&total), "{total} words in 10 s");
    }

    #[test]
    fn huge_burst_is_capped_at_max_rate() {
        let mut p = pacer();
        p.push(&"word ".repeat(1000));
        let log = simulate(&mut p, 10, |_, _| {});
        assert!(log.iter().all(|&(_, busy)| busy));
        let total = assert_paced(&log);
        let ok = 10 * MIN as usize - 1..=10 * MAX as usize + 1;
        assert!(ok.contains(&total), "{total} words in 10 s");
        // A backlog this deep runs at MAX the whole time.
        assert!(total >= 10 * MAX as usize - 2, "{total} words in 10 s");
        assert_eq!(p.pending, 1000 - total);
        assert_eq!(p.revealed().split_whitespace().count(), total);
    }

    #[test]
    fn a_200_word_burst_drains_faster_than_min_but_not_than_max() {
        let mut p = pacer();
        p.push(&"word ".repeat(200));
        let secs = drain(&mut p) as f32 * FRAME.as_secs_f32();
        assert!(secs < 200.0 / MIN, "{secs} s: no faster than min_wps");
        assert!(secs >= 200.0 / MAX, "{secs} s: faster than max_wps");
    }

    #[test]
    fn a_word_split_across_pushes_waits_until_complete() {
        let mut p = pacer();
        p.push("hel");
        assert_eq!(p.tick(Duration::from_secs(5)), 0);
        assert_eq!(p.revealed(), "");
        p.push("lo wor");
        assert_eq!(p.pending, 1);
        drain(&mut p);
        assert_eq!(p.revealed(), "hello");
        assert_eq!(p.tick(Duration::from_secs(5)), 0, "\"wor\" is incomplete");
        assert_eq!(p.revealed(), "hello");
        p.push("ld ");
        drain(&mut p);
        assert_eq!(p.revealed(), "hello world");
    }

    #[test]
    fn finish_flushes_the_backlog_and_the_partial_word() {
        let mut p = pacer();
        p.push("one two thr");
        p.tick(Duration::from_secs(60));
        assert_eq!(p.revealed(), "one two", "partial word revealed early");
        p.finish();
        assert_eq!(p.revealed(), "one two thr");

        let words = "word ".repeat(100);
        let mut p = pacer();
        p.push(&words);
        p.push("tail \n\n ");
        p.tick(FRAME);
        p.finish();
        assert_eq!(p.revealed(), format!("{words}tail"));
        assert!(p.is_caught_up());
        assert_eq!(p.pending, 0);
        assert_eq!(p.tick(Duration::from_secs(1)), 0);
    }

    #[test]
    fn idle_time_does_not_bank_credit() {
        let mut p = pacer();
        assert_eq!(p.tick(Duration::from_secs(60)), 0);
        p.push("late ");
        // At MIN words/s one word takes 10 frames; banked credit would make it 1.
        let frames = drain(&mut p);
        assert!((10..=11).contains(&frames), "{frames} frames");

        // Same right after a long frame drained the backlog.
        p.push("a b ");
        assert_eq!(p.tick(Duration::from_secs(60)), 2);
        p.push("c ");
        let frames = drain(&mut p);
        assert!((10..=11).contains(&frames), "{frames} frames");
    }

    #[test]
    fn whitespace_is_verbatim_and_revealed_with_the_next_word() {
        let mut p = pacer();
        p.push("\n  Para one.\n\nPara two.\n");
        let mut steps = Vec::new();
        while !p.is_caught_up() {
            if p.tick(FRAME) > 0 {
                steps.push(p.revealed().to_owned());
            }
        }
        assert_eq!(
            steps,
            [
                "\n  Para",
                "\n  Para one.",
                "\n  Para one.\n\nPara",
                "\n  Para one.\n\nPara two.",
            ]
        );

        let mut p = pacer();
        p.push("a\n\nb");
        p.finish();
        assert_eq!(p.revealed(), "a\n\nb");
    }

    #[test]
    fn zero_dt_reveals_nothing() {
        let mut p = pacer();
        p.push(&"word ".repeat(1000));
        for _ in 0..1000 {
            assert_eq!(p.tick(Duration::ZERO), 0);
        }
        assert_eq!(p.revealed(), "");
        p.tick(FRAME * 7); // leaves fractional credit behind
        let shown = p.revealed().len();
        assert_eq!(p.tick(Duration::ZERO), 0);
        assert_eq!(p.revealed().len(), shown);
    }

    #[test]
    fn counts_stay_consistent_under_random_chunks_and_frames() {
        let source =
            "Héllo wörld,\n\n日本語\u{3000}テキスト 🎉🎉 end.\t tabs\u{a0}nbsp  x ".repeat(20);
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut rand = |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };
        let mut p = pacer();
        let mut pushed = 0;
        while pushed < source.len() {
            let start = pushed;
            for _ in 0..=rand(12) {
                pushed += source[pushed..].chars().next().map_or(0, char::len_utf8);
            }
            p.push(&source[start..pushed]);
            for _ in 0..rand(4) {
                p.tick(FRAME * rand(20) as u32);
            }
            let text = &source[..pushed];
            let shown = p.revealed();
            let rest = &text[shown.len()..];
            assert!(text.starts_with(shown));
            assert!(!shown.ends_with(char::is_whitespace), "{shown:?}");
            assert!(
                shown.is_empty() || rest.starts_with(char::is_whitespace),
                "mid-word: {shown:?}"
            );
            assert_eq!(p.pending, complete_words(rest));
            assert_eq!(p.is_caught_up(), p.pending == 0);
            assert_eq!(
                shown.split_whitespace().count() + p.pending,
                complete_words(text)
            );
        }
        p.finish();
        assert_eq!(p.revealed(), source.trim_end());
    }

    #[test]
    fn multibyte_text_split_between_pushes_is_safe() {
        let mut p = pacer();
        let mut steps = Vec::new();
        for chunk in ["hé", "llo wö", "rld 日", "本語\u{3000}🎉", "🎉 ", "fin"] {
            p.push(chunk);
            drain(&mut p);
            steps.push(p.revealed().to_owned());
        }
        assert_eq!(
            steps,
            [
                "",
                "héllo",
                "héllo wörld",
                "héllo wörld 日本語",
                "héllo wörld 日本語\u{3000}🎉🎉",
                "héllo wörld 日本語\u{3000}🎉🎉",
            ]
        );
        p.finish();
        assert_eq!(p.revealed(), "héllo wörld 日本語\u{3000}🎉🎉 fin");

        // One char per push, ticking in between.
        let text = "ça\u{a0}va? 日本語 🎉\u{3000}ok\n\n👍🏽 ";
        let mut p = pacer();
        for (i, c) in text.char_indices() {
            p.push(&text[i..i + c.len_utf8()]);
            p.tick(FRAME * 30);
            assert!(text.starts_with(p.revealed()));
        }
        drain(&mut p);
        assert_eq!(p.revealed(), text.trim_end());
    }

    #[test]
    fn text_pushed_after_finish_is_paced_again() {
        let mut p = pacer();
        p.push("hel");
        p.finish();
        assert_eq!(p.revealed(), "hel");
        p.push("lo big world");
        assert_eq!(p.pending, 2, "\"lo\" and \" big\"");
        assert_eq!(p.tick(FRAME), 0, "paced, not flushed");
        drain(&mut p);
        assert_eq!(p.revealed(), "hello big");
        p.finish();
        assert_eq!(p.revealed(), "hello big world");
        p.push(" again ");
        assert_eq!(p.pending, 1, "the flushed \"world\" is not recounted");
        drain(&mut p);
        assert_eq!(p.revealed(), "hello big world again");
    }
}
