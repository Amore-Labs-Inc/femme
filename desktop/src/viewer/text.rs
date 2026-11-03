//! Text and source files: monospace, line numbers, syntax highlighting. The file is split into lines
//! in the background and shown at once as plain text; highlighting then arrives chunk by chunk
//! (the top of the file first) and the lines it covers are recoloured as it comes. The view is as
//! tall as the file has lines, but only the lines near the screen are built: what is on screen comes
//! from a probe, because whoever hosts the view owns the vertical scroll.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::ops::Range;
use std::sync::Arc;

use gpui::{
    div, font, px, AnyElement, Context, HighlightStyle, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledText, Window,
};

use super::chrome::{self, Seen};
use super::guarded;
use super::hex::human_size;
use super::highlight::{color, Highlighter, Span, Spans};
use crate::theme::{FONT_MONO, INK, MUTED, PAPER, RULE};
use crate::ui;

/// Text size and row height, in logical pixels.
const TEXT_SIZE: f32 = 13.;
const LINE_HEIGHT: f32 = 18.;
/// A line is shown cut after this many bytes (with an ellipsis): a 300 KB line would stall the
/// text shaper.
const MAX_LINE_BYTES: usize = 2000;
/// Tab stops are this many columns apart.
const TAB: usize = 4;
/// Only this much of a file is read; the rest is left out.
const MAX_TEXT_BYTES: usize = 64 << 20;
/// Files bigger than this stay plain: highlighting would take a minute.
const MAX_HIGHLIGHT_BYTES: usize = 2 << 20;
/// How much text the first highlight chunk covers (so the top of the file is coloured quickly)...
const FIRST_CHUNK_BYTES: usize = 24 << 10;
/// ...and every later one.
const CHUNK_BYTES: usize = 256 << 10;
/// Lines built above and below the ones on screen.
const MARGIN: usize = 30;
/// Lines built before the probe has said what is on screen.
const FIRST_ROWS: usize = 60;

/// A file's text, cut into lines once.
struct Doc {
    text: String,
    /// Start and end byte of every line, without its line ending.
    lines: Vec<(u32, u32)>,
    /// The longest line, which sizes the horizontal scroll.
    longest: usize,
    /// The file held invalid UTF-8, shown as U+FFFD.
    lossy: bool,
    /// Only the first [`MAX_TEXT_BYTES`] were read.
    truncated: bool,
}

impl Doc {
    /// Splits `data` into lines (`\n` or `\r\n`).
    fn build(data: &[u8]) -> Doc {
        let truncated = data.len() > MAX_TEXT_BYTES;
        let data = &data[..data.len().min(MAX_TEXT_BYTES)];
        let (text, lossy) = match String::from_utf8_lossy(data) {
            Cow::Borrowed(s) => (s.to_owned(), false),
            Cow::Owned(s) => (s, true),
        };
        let bytes = text.as_bytes();
        let mut lines = Vec::new();
        let mut start = 0;
        let mut push = |start: usize, end: usize| {
            let end = if end > start && bytes[end - 1] == b'\r' {
                end - 1
            } else {
                end
            };
            lines.push((start as u32, end as u32));
        };
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'\n' {
                push(start, i);
                start = i + 1;
            }
        }
        if start < bytes.len() {
            push(start, bytes.len());
        }
        let longest = lines
            .iter()
            .enumerate()
            .max_by_key(|(_, (s, e))| (e - s).min(MAX_LINE_BYTES as u32))
            .map_or(0, |(i, _)| i);
        Doc {
            text,
            lines,
            longest,
            lossy,
            truncated,
        }
    }

    /// Line `i` without its line ending.
    fn line(&self, i: usize) -> &str {
        let (s, e) = self.lines[i];
        &self.text[s as usize..e as usize]
    }
}

/// `line` as it is drawn: cut at [`MAX_LINE_BYTES`], tabs expanded to spaces, and the highlight
/// spans (in bytes of the original line) moved to match.
fn layout_line(line: &str, spans: &[Span]) -> (SharedString, Vec<(Range<usize>, HighlightStyle)>) {
    let mut end = line.len().min(MAX_LINE_BYTES);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let cut = end < line.len();
    let line = &line[..end];

    let (mut text, map) = if line.contains('\t') {
        let mut text = String::with_capacity(line.len() + 8);
        let mut map = Vec::with_capacity(line.len() + 1);
        let mut col = 0;
        for ch in line.chars() {
            map.extend(std::iter::repeat_n(text.len(), ch.len_utf8()));
            if ch == '\t' {
                let n = TAB - col % TAB;
                text.extend(std::iter::repeat_n(' ', n));
                col += n;
            } else {
                text.push(ch);
                col += 1;
            }
        }
        map.push(text.len());
        (text, Some(map))
    } else {
        (line.to_owned(), None)
    };
    let at = |b: u32| {
        let b = (b as usize).min(line.len());
        map.as_ref().map_or(b, |m| m[b])
    };
    let runs = spans
        .iter()
        .map(|s| (at(s.start)..at(s.end), s.class))
        .filter(|(r, _)| r.start < r.end)
        .map(|(r, class)| {
            let style = HighlightStyle {
                color: Some(color(class).into()),
                ..Default::default()
            };
            (r, style)
        })
        .collect();
    if cut {
        text.push('…');
    }
    (text.into(), runs)
}

/// How many characters a line number up to `n` takes.
fn digits(n: usize) -> usize {
    n.max(1).ilog10() as usize + 1
}

/// The lines to build for what the probe saw: those on screen and [`MARGIN`] more on each side. The
/// `current` ones are kept while they still cover the screen with half a margin to spare, so that
/// scrolling a line does not rebuild anything.
fn rows_for(seen: Seen, current: &Range<usize>, count: usize) -> Range<usize> {
    if seen.is_empty() {
        return current.clone();
    }
    let first = ((seen.top / LINE_HEIGHT) as usize).min(count.saturating_sub(1));
    let last = ((seen.bottom / LINE_HEIGHT).ceil() as usize).clamp(first + 1, count.max(1));
    let slack = MARGIN / 2;
    let top_ok = current.start == 0 || current.start + slack <= first;
    let bottom_ok = current.end >= count || last + slack <= current.end;
    if top_ok && bottom_ok {
        current.clone()
    } else {
        first.saturating_sub(MARGIN)..(last + MARGIN).min(count)
    }
}

/// The result of highlighting a stretch of lines.
struct Chunk {
    /// The state to continue from; `None` once the end is reached or when there is nothing to do.
    highlighter: Option<Highlighter>,
    spans: Spans,
    language: Option<&'static str>,
    done: bool,
}

/// Highlights `doc` from line `from` on, about [`CHUNK_BYTES`] of it; the first call (`hl` is
/// `None`) finds the grammar from the file name and first line.
fn highlight_chunk(doc: &Doc, from: usize, hl: Option<Highlighter>, name: &str) -> Chunk {
    let nothing = || Chunk {
        highlighter: None,
        spans: Spans::default(),
        language: None,
        done: true,
    };
    let mut hl = match hl {
        Some(hl) => hl,
        None if from == 0 && !doc.lines.is_empty() => {
            match Highlighter::for_file(name, &format!("{}\n", doc.line(0))) {
                Some(hl) => hl,
                None => return nothing(),
            }
        }
        None => return nothing(),
    };
    let budget = if from == 0 {
        FIRST_CHUNK_BYTES
    } else {
        CHUNK_BYTES
    };
    let mut to = from;
    let mut bytes = 0;
    while to < doc.lines.len() && bytes < budget {
        let (s, e) = doc.lines[to];
        bytes += (e - s) as usize + 1;
        to += 1;
    }
    let spans = hl.run((from..to).map(|i| doc.line(i)));
    Chunk {
        language: Some(hl.language()),
        done: to >= doc.lines.len(),
        highlighter: Some(hl),
        spans,
    }
}

/// A file that has been read.
struct Ready {
    doc: Arc<Doc>,
    spans: Spans,
    /// Carried between chunks; held by the running chunk while there is one.
    highlighter: Option<Highlighter>,
    language: Option<&'static str>,
    /// Chunks are still being asked for.
    highlighting: bool,
}

/// Where the reading stands.
enum State {
    Loading,
    Failed(String),
    // Boxed: the highlighter's parse state would make this variant far bigger than the others.
    Ready(Box<Ready>),
}

/// A text file: plain at once, coloured as the highlighter gets to it.
pub(crate) struct TextView {
    name: String,
    bytes: usize,
    state: State,
    /// What the probe saw of the line column in the last frame.
    seen: Seen,
    /// The lines that are built: the ones on screen and a margin.
    rows: Range<usize>,
    /// Width of the line-number column, measured from the font on every render.
    gutter: Pixels,
}

impl TextView {
    /// Starts splitting `data` into lines in the background.
    pub(crate) fn new(name: String, data: Arc<Vec<u8>>, cx: &mut Context<Self>) -> Self {
        let bytes = data.len();
        let work = cx
            .background_executor()
            .spawn(async move { guarded("the text reader", || Ok(Doc::build(&data))) });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(doc) => State::Ready(Box::new(Ready {
                        highlighting: doc.text.len() <= MAX_HIGHLIGHT_BYTES,
                        doc: Arc::new(doc),
                        spans: Spans::default(),
                        highlighter: None,
                        language: None,
                    })),
                    Err(why) => State::Failed(why),
                };
                if let State::Ready(r) = &this.state {
                    this.rows = 0..FIRST_ROWS.min(r.doc.lines.len());
                }
                this.next_chunk(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        TextView {
            name,
            bytes,
            state: State::Loading,
            seen: Seen::default(),
            rows: 0..0,
            gutter: px(40.),
        }
    }

    /// Takes in what the probe saw of the line column, and asks for a frame if other lines have to
    /// be built.
    fn see(&mut self, seen: Seen, cx: &mut Context<Self>) {
        self.seen = seen;
        let State::Ready(r) = &self.state else {
            return;
        };
        let rows = rows_for(seen, &self.rows, r.doc.lines.len());
        if rows != self.rows {
            self.rows = rows;
            cx.notify();
        }
    }

    /// Asks the background for the next stretch of highlighting, if there is more.
    fn next_chunk(&mut self, cx: &mut Context<Self>) {
        let State::Ready(r) = &mut self.state else {
            return;
        };
        if !r.highlighting {
            return;
        }
        let doc = r.doc.clone();
        let from = r.spans.lines_done();
        let hl = r.highlighter.take();
        let name = self.name.clone();
        let work = cx.background_executor().spawn(async move {
            guarded("the highlighter", || {
                Ok(highlight_chunk(&doc, from, hl, &name))
            })
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            this.update(cx, |this, cx| this.chunk_done(result, cx)).ok();
        })
        .detach();
    }

    /// Takes a highlighted chunk in and asks for the next.
    fn chunk_done(&mut self, result: Result<Chunk, String>, cx: &mut Context<Self>) {
        let State::Ready(r) = &mut self.state else {
            return;
        };
        let again = match result {
            Ok(chunk) => {
                r.language = chunk.language.or(r.language);
                r.spans.append(chunk.spans);
                r.highlighter = chunk.highlighter;
                !chunk.done
            }
            // The highlighter crashed on this file: it stays as far as it got.
            Err(_) => false,
        };
        r.highlighting = again;
        if again {
            self.next_chunk(cx);
        }
        cx.notify();
    }

    /// The caption: lines, size, grammar and how far highlighting has got.
    fn caption(&self) -> String {
        let State::Ready(r) = &self.state else {
            return human_size(self.bytes);
        };
        let mut s = format!("{} lines · {}", r.doc.lines.len(), human_size(self.bytes));
        if let Some(language) = r.language {
            let _ = write!(s, " · {language}");
        }
        if r.highlighting {
            let done = r.spans.lines_done() * 100 / r.doc.lines.len().max(1);
            let _ = write!(s, " · highlighting {done}%");
        } else if r.doc.text.len() > MAX_HIGHLIGHT_BYTES {
            s.push_str(" · too big to highlight");
        }
        if r.doc.lossy {
            s.push_str(" · invalid UTF-8 shown as \u{fffd}");
        }
        if r.doc.truncated {
            let _ = write!(s, " · only the first {}", human_size(MAX_TEXT_BYTES));
        }
        s
    }

    /// The line-number cells and the text lines of `range`, coloured as far as highlighted.
    fn rows(&self, range: Range<usize>) -> (Vec<AnyElement>, Vec<AnyElement>) {
        let State::Ready(r) = &self.state else {
            return (Vec::new(), Vec::new());
        };
        let mut numbers = Vec::with_capacity(range.len());
        let mut lines = Vec::with_capacity(range.len());
        for i in range {
            numbers.push(
                div()
                    .flex_none()
                    .h(px(LINE_HEIGHT))
                    .px(px(8.))
                    .child(format!("{}", i + 1))
                    .into_any_element(),
            );
            let (text, runs) = layout_line(r.doc.line(i), r.spans.line(i));
            let line = if runs.is_empty() {
                div().child(text).into_any_element()
            } else {
                div()
                    .child(StyledText::new(text).with_highlights(runs))
                    .into_any_element()
            };
            lines.push(
                div()
                    .flex_none()
                    .h(px(LINE_HEIGHT))
                    .pl(px(10.))
                    .pr(px(16.))
                    .whitespace_nowrap()
                    .child(line)
                    .into_any_element(),
            );
        }
        (numbers, lines)
    }
}

impl Render for TextView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (count, longest) = match &self.state {
            State::Loading => {
                return chrome::framed(chrome::waiting("reading"), human_size(self.bytes), PAPER);
            }
            State::Failed(why) => {
                return chrome::framed(
                    chrome::notice(
                        ("viewer-notice", cx.entity_id()),
                        "cannot show this file",
                        vec![why.clone()],
                    ),
                    human_size(self.bytes),
                    PAPER,
                );
            }
            State::Ready(r) => {
                let count = r.doc.lines.len();
                let longest = if count == 0 {
                    SharedString::default()
                } else {
                    layout_line(r.doc.line(r.doc.longest), &[]).0
                };
                (count, longest)
            }
        };
        if count == 0 {
            return chrome::framed(
                chrome::notice(("viewer-notice", cx.entity_id()), "empty file", Vec::new()),
                "0 bytes",
                PAPER,
            );
        }
        let text = window.text_system();
        let ch = text
            .ch_advance(text.resolve_font(&font(FONT_MONO)), px(TEXT_SIZE))
            .unwrap_or(px(8.));
        self.gutter = ch * digits(count) as f32 + px(16.) + RULE;

        let rows = self.rows.start.min(count)..self.rows.end.min(count);
        let (numbers, lines) = self.rows(rows.clone());
        let total = px(count as f32 * LINE_HEIGHT);
        let gap = |lines: usize| div().flex_none().h(px(lines as f32 * LINE_HEIGHT));
        // The numbers stay put when the lines scroll sideways. The column is as tall as the file;
        // the probe in it tells which lines the host's scroll area shows.
        let gutter = ui::ruled_sides(
            div()
                .relative()
                .flex_none()
                .w(self.gutter)
                .h(total)
                .flex()
                .flex_col()
                .text_right()
                .text_color(MUTED),
            px(0.),
            RULE,
            px(0.),
            px(0.),
        )
        .child(chrome::watch(self.seen, cx, Self::see))
        .child(gap(rows.start))
        .children(numbers)
        .child(gap(count - rows.end));
        // The longest line has a row of no height at the end, so that the sideways scroll reaches as
        // far whichever lines are built.
        let lines = div()
            .id(("text-x", cx.entity_id()))
            .flex()
            .flex_1()
            .min_w_0()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .child(
                div()
                    .flex_none()
                    .min_w_full()
                    .h(total)
                    .flex()
                    .flex_col()
                    .child(gap(rows.start))
                    .children(lines)
                    .child(gap(count - rows.end))
                    .child(
                        div()
                            .flex_none()
                            .h(px(0.))
                            .overflow_hidden()
                            .pl(px(10.))
                            .pr(px(16.))
                            .whitespace_nowrap()
                            .child(longest),
                    ),
            );
        let area = div()
            .flex()
            .w_full()
            .font_family(FONT_MONO)
            .text_size(px(TEXT_SIZE))
            .line_height(px(LINE_HEIGHT))
            .text_color(INK)
            .child(gutter)
            .child(lines);
        chrome::framed(area, self.caption(), PAPER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewer::highlight::Class;

    #[test]
    fn lines_split_on_lf_and_crlf_and_keep_a_last_line_without_one() {
        let doc = Doc::build(b"one\r\ntwo\n\nlast");
        let lines: Vec<&str> = (0..doc.lines.len()).map(|i| doc.line(i)).collect();
        assert_eq!(lines, ["one", "two", "", "last"]);
        assert!(!doc.lossy && !doc.truncated);
        assert_eq!(doc.longest, 3, "\"last\" is the longest line");
        assert_eq!(
            Doc::build(b"a\n").lines.len(),
            1,
            "no empty line after the final newline"
        );
        assert!(Doc::build(b"").lines.is_empty());
    }

    #[test]
    fn invalid_utf8_is_replaced_and_noted() {
        let doc = Doc::build(b"caf\xe9\nok");
        assert!(doc.lossy);
        assert_eq!(doc.line(0), "caf\u{fffd}");
    }

    #[test]
    fn digits_count_the_line_number_column() {
        assert_eq!(
            [digits(0), digits(9), digits(10), digits(999), digits(1000)],
            [1, 1, 2, 3, 4]
        );
    }

    fn style_of(class: Class) -> HighlightStyle {
        HighlightStyle {
            color: Some(color(class).into()),
            ..Default::default()
        }
    }

    #[test]
    fn tabs_expand_to_stops_and_spans_follow() {
        let spans = [Span {
            start: 1,
            end: 2,
            class: Class::Keyword,
        }];
        let (text, runs) = layout_line("\tx", &spans);
        assert_eq!(text.as_ref(), "    x");
        assert_eq!(runs, vec![(4..5, style_of(Class::Keyword))]);

        let (text, _) = layout_line("ab\tc", &[]);
        assert_eq!(text.as_ref(), "ab  c");

        // Multi-byte characters: the span is in bytes of the original line.
        let spans = [Span {
            start: 3,
            end: 4,
            class: Class::Str,
        }];
        let (text, runs) = layout_line("é\tz", &spans);
        assert_eq!(text.as_ref(), "é   z");
        assert_eq!(runs, vec![(5..6, style_of(Class::Str))]);
    }

    #[test]
    fn long_lines_are_cut_with_an_ellipsis_and_spans_are_clipped() {
        let line = "a".repeat(MAX_LINE_BYTES + 500);
        let spans = [Span {
            start: 0,
            end: line.len() as u32,
            class: Class::Comment,
        }];
        let (text, runs) = layout_line(&line, &spans);
        assert_eq!(text.len(), MAX_LINE_BYTES + '…'.len_utf8());
        assert!(text.ends_with('…'));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, 0..MAX_LINE_BYTES);
        // Cutting never splits a character.
        let wide = "é".repeat(MAX_LINE_BYTES);
        let (text, _) = layout_line(&wide, &[]);
        assert!(text.ends_with('…') && text.chars().count() == MAX_LINE_BYTES / 2 + 1);
    }

    #[test]
    fn highlighting_runs_in_chunks_until_every_line_is_covered() {
        let src = "fn main() { let x = 1; } // c\n".repeat(3000);
        let doc = Doc::build(src.as_bytes());
        let first = highlight_chunk(&doc, 0, None, "a.rs");
        assert_eq!(first.language, Some("Rust"));
        assert!(!first.done, "90 KB is more than the first chunk");
        let mut spans = Spans::default();
        let mut next = first;
        loop {
            let done = next.done;
            spans.append(next.spans);
            if done {
                break;
            }
            next = highlight_chunk(&doc, spans.lines_done(), next.highlighter, "a.rs");
        }
        assert_eq!(spans.lines_done(), 3000);
        assert_eq!(
            spans.line(2999).first().map(|s| s.class),
            Some(Class::Keyword)
        );
        assert!(
            highlight_chunk(&doc, 0, None, "notes.txt").done,
            "plain text is done at once"
        );
    }

    /// `top..bottom` lines of a file on screen.
    fn on_screen(top: usize, bottom: usize) -> Seen {
        Seen {
            width: 500.,
            top: top as f32 * LINE_HEIGHT,
            bottom: bottom as f32 * LINE_HEIGHT,
        }
    }

    #[test]
    fn the_built_rows_follow_the_screen_with_a_margin() {
        // 50 lines on screen from line 100: those and a margin of 30 each side.
        assert_eq!(rows_for(on_screen(100, 150), &(0..60), 1000), 70..180);
        // A few lines of scrolling change nothing while half a margin is spare...
        assert_eq!(rows_for(on_screen(105, 155), &(70..180), 1000), 70..180);
        // ...but near the edge of what is built, other rows are chosen.
        assert_eq!(rows_for(on_screen(160, 210), &(70..180), 1000), 130..240);
        // The rows stop at the ends of the file.
        assert_eq!(rows_for(on_screen(0, 50), &(100..200), 1000), 0..80);
        assert_eq!(rows_for(on_screen(960, 1000), &(0..60), 1000), 930..1000);
        // Nothing on screen: nothing changes.
        assert_eq!(rows_for(on_screen(0, 0), &(5..9), 1000), 5..9);
    }

    #[test]
    fn a_short_file_is_built_whole_and_kept() {
        assert_eq!(rows_for(on_screen(0, 5), &(0..10), 10), 0..10);
        assert_eq!(rows_for(on_screen(0, 5), &(0..0), 0), 0..0);
    }
}
