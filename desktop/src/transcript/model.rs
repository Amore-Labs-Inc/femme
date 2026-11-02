//! Transcript row model: the rows the transcript list virtualises, folded from
//! [`femme_proto::state::Item`]s, plus deterministic fake rows and a word stream for the rendering
//! bench.

use std::fmt::Write as _;

use femme_proto::state::{Item, ItemKind, ItemStatus};
use femme_proto::Id;

use crate::rng::Rng;

/// Bytes of text an [`Item::preview`] holds (cut back to a char boundary).
const PREVIEW_BYTES: usize = 512;

/// One row of the transcript list (what the list virtualises), folded from
/// [`femme_proto::state::Item`]s.
#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptRow {
    /// A message from the user.
    User {
        /// `seq` of its item.
        seq: u64,
        /// The message.
        text: String,
    },
    /// Agent prose: an assistant message or a plan.
    Assistant {
        /// `seq` of its item.
        seq: u64,
        /// The prose, paragraphs separated by `"\n\n"`.
        text: String,
    },
    /// A collapsed run of tool work, shown as `worked for 3m (12 tools)` over its summary.
    ToolFold {
        /// `seq` of the run's first item.
        seq: u64,
        /// ToolCall items in the run.
        tools: u32,
        /// Seconds from the run's first item to its last.
        secs: u32,
        /// What the calls did, e.g. `read 4 files, ran 2 cmds`; empty when the run made none.
        summary: String,
    },
    /// A question for the user with numbered options.
    Question {
        /// `seq` of its item.
        seq: u64,
        /// The question.
        prompt: String,
        /// The options in order, without their numbers.
        options: Vec<String>,
    },
}

impl TranscriptRow {
    /// `seq` of the item the row starts at (strictly increasing down a transcript).
    pub fn seq(&self) -> u64 {
        match self {
            TranscriptRow::User { seq, .. }
            | TranscriptRow::Assistant { seq, .. }
            | TranscriptRow::ToolFold { seq, .. }
            | TranscriptRow::Question { seq, .. } => *seq,
        }
    }

    /// The headline of a [`TranscriptRow::ToolFold`]: `worked for 45s (3 tools)` under a
    /// minute, else in whole minutes, `worked for 3m (12 tools)`; `(1 tool)` when singular.
    /// `None` for every other row.
    pub fn fold_label(&self) -> Option<String> {
        let TranscriptRow::ToolFold { tools, secs, .. } = self else {
            return None;
        };
        let plural = if *tools == 1 { "" } else { "s" };
        Some(if *secs < 60 {
            format!("worked for {secs}s ({tools} tool{plural})")
        } else {
            format!("worked for {}m ({tools} tool{plural})", secs / 60)
        })
    }
}

/// Folds items into rows one at a time, for [`fake_rows`] while it generates (so the fake items
/// never all exist at once).
///
/// Each run of consecutive Thinking/ToolCall/ToolResult/Event/TurnStart/TurnEnd items becomes one
/// [`TranscriptRow::ToolFold`]: `seq` = the run's first item, `tools` = its ToolCall items,
/// `secs` = (last item's `ts` - first item's `ts`) / 1000, never negative, and `summary` counts
/// the ToolCalls by their preview's first word, matched exactly: `Read` -> files, `Bash` -> cmds,
/// `Edit` -> edits, anything else -> others. The parts come in that order, zero counts left out,
/// singular for 1, e.g. `read 4 files, ran 2 cmds`. A run made only of TurnStart/TurnEnd/Event
/// items emits no row; a Plan item becomes an [`TranscriptRow::Assistant`] row.
#[derive(Default)]
struct Fold {
    rows: Vec<TranscriptRow>,
    /// The tool-work run in progress.
    run: Option<Run>,
}

impl Fold {
    /// Folds in the next item; `text` gives its text (body, else preview) when it becomes a row
    /// of its own.
    fn push(&mut self, item: &Item, text: impl FnOnce() -> String) {
        let row = match item.kind {
            ItemKind::UserMessage => TranscriptRow::User {
                seq: item.seq,
                text: text(),
            },
            ItemKind::AssistantMessage | ItemKind::Plan => TranscriptRow::Assistant {
                seq: item.seq,
                text: text(),
            },
            ItemKind::Question => {
                let (prompt, options) = parse_question(&text());
                TranscriptRow::Question {
                    seq: item.seq,
                    prompt,
                    options,
                }
            }
            ItemKind::Thinking
            | ItemKind::ToolCall
            | ItemKind::ToolResult
            | ItemKind::Event
            | ItemKind::TurnStart
            | ItemKind::TurnEnd => {
                self.run.get_or_insert_with(|| Run::new(item)).add(item);
                return;
            }
        };
        self.rows.extend(self.run.take().and_then(Run::finish));
        self.rows.push(row);
    }

    /// The rows, the last run folded in.
    fn finish(mut self) -> Vec<TranscriptRow> {
        self.rows.extend(self.run.take().and_then(Run::finish));
        self.rows
    }
}

/// Summary parts of a fold, in order: (prefix, singular, plural), e.g. `read ` `4` ` files`.
const SUMMARY_PARTS: [(&str, &str, &str); 4] = [
    ("read ", "file", "files"),
    ("ran ", "cmd", "cmds"),
    ("made ", "edit", "edits"),
    ("", "other", "others"),
];

/// A run of consecutive tool-work items on its way to one [`TranscriptRow::ToolFold`].
struct Run {
    seq: u64,
    first_ts: i64,
    last_ts: i64,
    /// Saw a Thinking, ToolCall or ToolResult item; a run without one emits no row.
    work: bool,
    /// ToolCalls per [`SUMMARY_PARTS`] entry.
    calls: [u32; 4],
}

impl Run {
    fn new(first: &Item) -> Self {
        Run {
            seq: first.seq,
            first_ts: first.ts,
            last_ts: first.ts,
            work: false,
            calls: [0; 4],
        }
    }

    fn add(&mut self, item: &Item) {
        self.last_ts = item.ts;
        match item.kind {
            ItemKind::ToolCall => {
                self.work = true;
                let part = match item.preview.split_whitespace().next() {
                    Some("Read") => 0,
                    Some("Bash") => 1,
                    Some("Edit") => 2,
                    _ => 3,
                };
                self.calls[part] += 1;
            }
            ItemKind::Thinking | ItemKind::ToolResult => self.work = true,
            _ => {}
        }
    }

    fn finish(self) -> Option<TranscriptRow> {
        if !self.work {
            return None;
        }
        let mut summary = String::new();
        for (n, (prefix, one, many)) in self.calls.into_iter().zip(SUMMARY_PARTS) {
            if n == 0 {
                continue;
            }
            if !summary.is_empty() {
                summary.push_str(", ");
            }
            let noun = if n == 1 { one } else { many };
            let _ = write!(summary, "{prefix}{n} {noun}");
        }
        let millis = self.last_ts.saturating_sub(self.first_ts).max(0);
        Some(TranscriptRow::ToolFold {
            seq: self.seq,
            tools: self.calls.iter().sum(),
            secs: u32::try_from(millis / 1000).unwrap_or(u32::MAX),
            summary,
        })
    }
}

/// Splits a question body `"prompt\n1. a\n2. b"` into the prompt (first line) and the options
/// (every later non-blank line, its `{n}. ` prefix removed when it has one).
fn parse_question(text: &str) -> (String, Vec<String>) {
    let mut lines = text.lines();
    let prompt = lines.next().unwrap_or_default().to_owned();
    let options = lines
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let rest = line.trim_start_matches(|c: char| c.is_ascii_digit());
            match rest.strip_prefix(". ") {
                Some(option) if rest.len() < line.len() => option,
                _ => line,
            }
            .to_owned()
        })
        .collect();
    (prompt, options)
}

/// The first [`PREVIEW_BYTES`] bytes of `text`, cut back to a char boundary.
fn preview(text: &str) -> &str {
    let mut end = text.len().min(PREVIEW_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Start of the fake timeline: an arbitrary fixed Unix ms (2026-09-21).
const FAKE_T0: i64 = 1_790_000_000_000;

/// Exactly `n` rows (n >= 1; 0 gives none), deterministic per seed: whole fake turns folded into
/// rows, cut to `n`. A turn is a UserMessage (3-40 words); in about 70 % of turns a tool group (a
/// Thinking item, then 3-15 ToolCalls such as `Read src/app.rs` or `Bash cargo test --offline`,
/// each followed by its ToolResult, a few seconds apiece); an AssistantMessage (1-6 paragraphs of
/// 20-120 words joined by `"\n\n"`); and in about 8 % of turns a Question with 2-4 options.
///
/// The last row is always an [`TranscriptRow::Assistant`] row (the one that streams during the
/// bench): when the cut lands on another kind, that row is replaced by an Assistant row with the
/// same `seq`, so `seq()` stays strictly increasing.
// ponytail: bodies are JSON per kind in femme-agents, plain text until M3
pub fn fake_rows(n: usize, seed: u64) -> Vec<TranscriptRow> {
    // Items are folded while they are made: they never all exist at once, and message bodies move
    // into their rows instead of being copied.
    let mut fold = Fold {
        rows: Vec::with_capacity(n.saturating_add(3)),
        run: None,
    };
    fake_turns(n, seed, |mut item| {
        let body = item.body.take();
        fold.push(&item, || body.unwrap_or_else(|| item.preview.clone()));
    });
    let mut rows = fold.finish();
    rows.truncate(n);
    if let Some(last) = rows.last_mut() {
        if !matches!(last, TranscriptRow::Assistant { .. }) {
            let text = assistant_text(&mut Rng::new(!seed));
            *last = TranscriptRow::Assistant {
                seq: last.seq(),
                text,
            };
        }
    }
    rows
}

/// Endless deterministic dev-flavoured word stream for the streaming bench.
#[derive(Clone, Debug)]
pub struct WordStream {
    rng: Rng,
    sentence: Sentence,
    /// Words left before the next paragraph break.
    until_break: u32,
}

impl WordStream {
    /// A stream whose words depend only on `seed`.
    pub fn new(seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let until_break = rng.range(60, 120);
        WordStream {
            rng,
            sentence: Sentence::default(),
            until_break,
        }
    }

    /// `words` words, each preceded by one space (it continues an existing text), with a
    /// `"\n\n"` paragraph break instead of a space roughly every 60-120 words. The stream
    /// carries on across calls: bursts concatenate to the same text whatever their sizes.
    pub fn next_burst(&mut self, words: usize) -> String {
        let mut out = String::with_capacity(words * 8);
        for _ in 0..words {
            if self.until_break == 0 {
                out.push_str("\n\n");
                self.until_break = self.rng.range(60, 120);
            } else {
                out.push(' ');
            }
            self.until_break -= 1;
            // The last word of a paragraph ends its sentence.
            self.sentence
                .push_word(&mut out, &mut self.rng, self.until_break == 0);
        }
        out
    }
}

/// Emits whole fake turns for `seed`, in order, until they make at least `approx_rows` rows.
fn fake_turns(approx_rows: usize, seed: u64, emit: impl FnMut(Item)) {
    let mut fake = Fake {
        rng: Rng::new(seed),
        ts: FAKE_T0,
        seq: 0,
        emit,
    };
    let mut rows = 0;
    while rows < approx_rows {
        rows += fake.turn();
    }
}

/// The generator behind [`fake_turns`]: one PRNG, the clock, and where the items go.
struct Fake<E> {
    rng: Rng,
    /// Unix ms of the last item.
    ts: i64,
    /// `seq` of the last item.
    seq: u64,
    emit: E,
}

impl<E: FnMut(Item)> Fake<E> {
    /// Emits one turn and returns how many rows the fold makes of it.
    fn turn(&mut self) -> usize {
        let gap = self.rng.range(5_000, 300_000);
        let words = self.rng.range(3, 40);
        let text = prose(&mut self.rng, words);
        self.push(ItemKind::UserMessage, gap, text);
        let mut rows = 2; // the user message and the reply
        if self.rng.chance(7, 10) {
            rows += 1; // the fold
            let gap = self.rng.range(800, 4_000);
            let words = self.rng.range(10, 60);
            let text = prose(&mut self.rng, words);
            self.push(ItemKind::Thinking, gap, text);
            for _ in 0..self.rng.range(3, 15) {
                let gap = self.rng.range(1_500, 6_000);
                let (call, result, ran) = fake_tool(&mut self.rng);
                self.push(ItemKind::ToolCall, gap, call);
                self.push(ItemKind::ToolResult, ran, result);
            }
        }
        let gap = self.rng.range(1_500, 12_000);
        let text = assistant_text(&mut self.rng);
        self.push(ItemKind::AssistantMessage, gap, text);
        if self.rng.chance(8, 100) {
            rows += 1;
            let gap = self.rng.range(50, 500);
            let text = question_text(&mut self.rng);
            self.push(ItemKind::Question, gap, text);
        }
        rows
    }

    /// Emits an item `after_ms` after the previous one, with `body` as its text.
    fn push(&mut self, kind: ItemKind, after_ms: u32, body: String) {
        self.ts += i64::from(after_ms);
        self.seq += 1;
        let seq = self.seq;
        let status = if kind == ItemKind::Question {
            ItemStatus::Pending
        } else {
            ItemStatus::Completed
        };
        (self.emit)(Item {
            sid: Id(1),
            item_id: format!("i{seq}"),
            seq,
            rev: seq,
            kind,
            status,
            preview: preview(&body).to_owned(),
            body: Some(body),
            blob_hash: None,
            ts: self.ts,
        });
    }
}

/// Sentence shape of fake prose: a sentence starts capitalised and ends with `.` after 4-16
/// words, or earlier when the caller ends it (end of a text or a paragraph).
#[derive(Clone, Debug, Default)]
struct Sentence {
    /// Words left in the current sentence; 0 = the next word starts one.
    left: u32,
}

impl Sentence {
    /// Appends one vocabulary word; `end` closes the sentence after it.
    fn push_word(&mut self, out: &mut String, rng: &mut Rng, end: bool) {
        let word = pick(rng, VOCAB);
        if self.left == 0 {
            self.left = rng.range(4, 16);
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
        } else {
            out.push_str(word);
        }
        self.left -= 1;
        if end || self.left == 0 {
            out.push('.');
            self.left = 0;
        }
    }
}

/// Appends `words` words of prose (sentences, single spaces, ending with `.`).
fn push_prose(out: &mut String, rng: &mut Rng, words: u32) {
    let mut sentence = Sentence::default();
    for i in 0..words {
        if i > 0 {
            out.push(' ');
        }
        sentence.push_word(out, rng, i + 1 == words);
    }
}

/// `words` words of prose.
fn prose(rng: &mut Rng, words: u32) -> String {
    let mut out = String::with_capacity(words as usize * 8);
    push_prose(&mut out, rng, words);
    out
}

/// An assistant reply: 1-6 paragraphs of 20-120 words joined by `"\n\n"`.
fn assistant_text(rng: &mut Rng) -> String {
    let paragraphs = rng.range(1, 6);
    let mut out = String::with_capacity(paragraphs as usize * 600);
    for p in 0..paragraphs {
        if p > 0 {
            out.push_str("\n\n");
        }
        let words = rng.range(20, 120);
        push_prose(&mut out, rng, words);
    }
    out
}

/// A question body: `"prompt?\n1. a\n2. b"` with 2-4 options of 1-4 words.
fn question_text(rng: &mut Rng) -> String {
    let words = rng.range(5, 14);
    let mut out = prose(rng, words);
    out.pop(); // the closing '.'
    out.push('?');
    for n in 1..=rng.range(2, 4) {
        let _ = write!(out, "\n{n}. ");
        let words = rng.range(1, 4);
        push_prose(&mut out, rng, words);
        out.pop(); // options are labels, without the '.'
    }
    out
}

/// One fake tool call: its `"{Verb} {arg}"` text, a short result and how long it ran (ms).
fn fake_tool(rng: &mut Rng) -> (String, String, u32) {
    match rng.below(100) {
        0..=34 => (
            format!("Read {}", pick(rng, FILES)),
            format!("{} lines", rng.range(12, 1_800)),
            rng.range(100, 900),
        ),
        35..=59 => (
            format!("Bash {}", pick(rng, COMMANDS)),
            pick(rng, OUTPUTS).to_owned(),
            rng.range(1_000, 25_000),
        ),
        60..=79 => (
            format!("Edit {}", pick(rng, FILES)),
            format!("+{} -{}", rng.range(1, 60), rng.range(0, 30)),
            rng.range(200, 1_500),
        ),
        80..=91 => (
            format!("Grep {}", pick(rng, PATTERNS)),
            format!("{} matches in {} files", rng.range(0, 40), rng.range(1, 12)),
            rng.range(100, 800),
        ),
        92..=96 => (
            format!("Glob {}", pick(rng, GLOBS)),
            format!("{} files", rng.range(1, 90)),
            rng.range(50, 400),
        ),
        _ => (
            format!("WebFetch {}", pick(rng, URLS)),
            format!("200 OK, {} KB", rng.range(4, 300)),
            rng.range(800, 6_000),
        ),
    }
}

/// A uniformly chosen entry of `list` (non-empty).
fn pick<'a>(rng: &mut Rng, list: &[&'a str]) -> &'a str {
    list[rng.below(list.len() as u32) as usize]
}

/// Dev-flavoured vocabulary for fake prose (common words repeat to weigh them up). A few
/// multi-byte entries make previews and layout meet non-ASCII text. No entry holds whitespace.
#[rustfmt::skip]
const VOCAB: &[&str] = &[
    // Filler.
    "the", "the", "the", "a", "a", "to", "to", "of", "of", "and", "and", "in", "on", "for", "with",
    "from", "by", "at", "as", "is", "is", "are", "was", "it", "it", "this", "that", "that", "we",
    "so", "but", "not", "now", "then", "when", "if", "before", "after", "once", "each", "every",
    "all", "only", "still", "just", "also", "into", "over", "per", "because", "which", "where",
    "should", "can", "will", "again", "already", "instead", "first", "next",
    // Verbs.
    "parse", "render", "flush", "cache", "fold", "stream", "spawn", "decode", "encode", "resize",
    "scroll", "wrap", "measure", "reuse", "evict", "allocate", "clamp", "batch", "diff", "patch",
    "merge", "rebase", "build", "bench", "profile", "refactor", "inline", "split", "rename",
    "replay", "subscribe", "reconnect", "serialize", "retry", "log", "block", "await", "lock",
    "drop", "clone", "borrow", "emit", "paint", "layout", "shape",
    // Nouns.
    "row", "rows", "pane", "panes", "frame", "frames", "buffer", "socket", "cursor", "glyph",
    "glyphs", "list", "item", "items", "session", "daemon", "holder", "client", "server",
    "transcript", "terminal", "drawer", "theme", "font", "line", "lines", "width", "height",
    "offset", "index", "revision", "snapshot", "tombstone", "scrollback", "viewport", "atlas",
    "texture", "shader", "thread", "channel", "queue", "worker", "budget", "latency", "test",
    "crate", "module", "struct", "enum", "trait", "closure", "iterator", "slice", "codec",
    "packet", "handshake", "keystroke", "prompt", "branch", "commit",
    // Adjectives.
    "stale", "dirty", "hot", "cold", "lazy", "eager", "cheap", "slow", "fast", "stable", "flaky",
    "visible", "hidden", "wide", "narrow", "partial", "empty", "idle", "busy", "deterministic",
    "monotonic",
    // Identifiers and numbers.
    "RowCache", "TranscriptRow", "ItemKind", "PaneId", "WordStream", "RowPatch", "GetRows",
    "femmed", "femme-proto", "gpui", "alacritty_terminal", "quinn", "postcard", "zstd", "tokio",
    "`RowCache`", "`apply`", "`fold_label`", "`Item::body`", "`list()`", "`ScrollHandle`",
    "`seq`", "`rev`", "0", "1", "3", "60", "512", "4096", "50k", "16ms", "p99",
    // Multi-byte.
    "→", "—", "≈", "≤", "naïve", "façade",
];

/// Paths for fake Read/Edit calls.
const FILES: &[&str] = &[
    "src/app.rs",
    "src/main.rs",
    "src/theme.rs",
    "src/rng.rs",
    "src/terminal/pane.rs",
    "src/terminal/element.rs",
    "src/terminal/runs.rs",
    "src/terminal/synth.rs",
    "src/transcript/model.rs",
    "src/transcript/pacer.rs",
    "crates/femme-proto/src/state.rs",
    "crates/femme-proto/src/codec.rs",
    "crates/femme-term/src/lib.rs",
    "crates/femme-store/src/lib.rs",
    "crates/femmed/src/main.rs",
    "Cargo.toml",
    "README.md",
    "docs/plan-desktop.md",
];

/// Command lines for fake Bash calls.
const COMMANDS: &[&str] = &[
    "cargo test --offline",
    "cargo test --offline --release",
    "cargo clippy --offline --all-targets -- -D warnings",
    "cargo build --release",
    "cargo fmt --check",
    "cargo run --release -- bench",
    "cargo tree -d",
    "git status --short",
    "git diff --stat",
    "rg -n RowCache src",
    "ls crates",
    "wc -l src/transcript/model.rs",
];

/// Outputs for fake Bash calls.
const OUTPUTS: &[&str] = &[
    "test result: ok. 48 passed; 0 failed; 0 ignored; finished in 0.41s",
    "test result: FAILED. 47 passed; 1 failed; 0 ignored",
    "Finished `release` profile [optimized] target(s) in 41.07s",
    "warning: unused variable: `row`",
    "error[E0308]: mismatched types",
    " M src/app.rs\n?? src/transcript/model.rs",
    "3 files changed, 120 insertions(+), 14 deletions(-)",
    "ok",
];

/// Patterns for fake Grep calls.
const PATTERNS: &[&str] = &[
    "RowCache",
    "fn apply",
    "ItemKind::ToolCall",
    "TODO",
    "unwrap()",
    "unsafe",
    "WordStream",
    "impl Render",
];

/// Patterns for fake Glob calls.
const GLOBS: &[&str] = &[
    "**/*.rs",
    "src/**/*.rs",
    "crates/*/Cargo.toml",
    "assets/fonts/**/*.ttf",
];

/// URLs for fake WebFetch calls.
const URLS: &[&str] = &[
    "https://docs.rs/gpui",
    "https://docs.rs/unicode-width",
    "https://github.com/zed-industries/zed",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn item(seq: u64, kind: ItemKind, ts: i64, text: &str) -> Item {
        Item {
            sid: Id(1),
            item_id: format!("i{seq}"),
            seq,
            rev: seq,
            kind,
            status: ItemStatus::Completed,
            preview: text.to_owned(),
            body: Some(text.to_owned()),
            blob_hash: None,
            ts,
        }
    }

    fn fold(seq: u64, tools: u32, secs: u32, summary: &str) -> TranscriptRow {
        TranscriptRow::ToolFold {
            seq,
            tools,
            secs,
            summary: summary.to_owned(),
        }
    }

    fn user(seq: u64, text: &str) -> TranscriptRow {
        TranscriptRow::User {
            seq,
            text: text.to_owned(),
        }
    }

    fn assistant(seq: u64, text: &str) -> TranscriptRow {
        TranscriptRow::Assistant {
            seq,
            text: text.to_owned(),
        }
    }

    fn assert_seq_increasing(rows: &[TranscriptRow]) {
        for pair in rows.windows(2) {
            assert!(pair[0].seq() < pair[1].seq(), "{pair:?}");
        }
    }

    /// The rows the fold makes of `items`, given in `seq` order.
    fn fold_all(items: &[Item]) -> Vec<TranscriptRow> {
        let mut fold = Fold::default();
        for it in items {
            fold.push(it, || it.preview.clone());
        }
        fold.finish()
    }

    #[test]
    fn fold_merges_adjacent_tool_items() {
        use ItemKind::*;
        let items = [
            item(1, UserMessage, 0, "fix the cache"),
            item(2, Thinking, 1_000, "look first"),
            item(3, ToolCall, 2_000, "Read src/a.rs"),
            item(4, ToolResult, 3_000, "40 lines"),
            item(5, ToolCall, 4_000, "Read src/b.rs"),
            item(6, ToolResult, 5_000, "12 lines"),
            item(7, Event, 6_000, "model changed"),
            item(8, ToolCall, 7_000, "Bash cargo test"),
            item(9, ToolResult, 9_000, "ok"),
            item(10, ToolCall, 10_000, "Grep RowCache"),
            item(11, ToolResult, 47_999, "3 matches"),
            item(12, AssistantMessage, 50_000, "Done.\n\nAll green."),
        ];
        assert_eq!(
            fold_all(&items),
            [
                user(1, "fix the cache"),
                fold(2, 4, 46, "read 2 files, ran 1 cmd, 1 other"),
                assistant(12, "Done.\n\nAll green."),
            ]
        );
    }

    #[test]
    fn fold_labels() {
        assert_eq!(
            fold(1, 3, 45, "").fold_label().as_deref(),
            Some("worked for 45s (3 tools)")
        );
        assert_eq!(
            fold(1, 12, 3 * 60 + 59, "").fold_label().as_deref(),
            Some("worked for 3m (12 tools)")
        );
        assert_eq!(
            fold(1, 1, 59, "").fold_label().as_deref(),
            Some("worked for 59s (1 tool)")
        );
        assert_eq!(
            fold(1, 0, 60, "").fold_label().as_deref(),
            Some("worked for 1m (0 tools)")
        );
        assert_eq!(user(1, "x").fold_label(), None);
        assert_eq!(assistant(1, "x").fold_label(), None);
        let question = TranscriptRow::Question {
            seq: 1,
            prompt: String::new(),
            options: vec![],
        };
        assert_eq!(question.fold_label(), None);
    }

    #[test]
    fn preview_cuts_at_a_char_boundary() {
        assert_eq!(preview(""), "");
        assert_eq!(preview("short"), "short");
        assert_eq!(preview(&"a".repeat(600)).len(), 512);
        assert_eq!(preview(&"é".repeat(300)).len(), 512);
        assert_eq!(preview(&"€".repeat(200)).len(), 510);
        assert_eq!(preview(&format!("a{}", "€".repeat(200))).len(), 511);
        assert_eq!(preview(&"🦀".repeat(200)).len(), 512);
        assert_eq!(preview(&format!("ab{}", "🦀".repeat(200))).len(), 510);
    }

    #[test]
    fn parse_question_splits_prompt_and_options() {
        assert_eq!(parse_question(""), (String::new(), vec![]));
        assert_eq!(
            parse_question("q\n1.x\n. y"),
            ("q".to_owned(), vec!["1.x".to_owned(), ". y".to_owned()])
        );
        assert_eq!(
            parse_question("Which?\n1. jose\n\n10. roll our own\nno number\n"),
            (
                "Which?".to_owned(),
                ["jose", "roll our own", "no number"]
                    .map(String::from)
                    .to_vec()
            )
        );
    }

    #[test]
    fn fake_rows_exact_length() {
        assert!(fake_rows(0, 1).is_empty());
        for n in [1, 2, 10, 1_000, 50_000] {
            assert_eq!(fake_rows(n, 1).len(), n);
        }
    }

    #[test]
    fn fake_rows_end_with_assistant_and_seq_increases() {
        for seed in 0..60 {
            for n in [1, 2, 3, 4, 5, 7, 10, 33, 100, 257] {
                let rows = fake_rows(n, seed);
                assert_eq!(rows.len(), n);
                assert!(
                    matches!(rows.last(), Some(TranscriptRow::Assistant { text, .. }) if !text.is_empty()),
                    "n {n} seed {seed}: {:?}",
                    rows.last()
                );
                assert_eq!(rows[0].seq(), 1);
                assert_seq_increasing(&rows);
            }
        }
    }

    #[test]
    fn fake_rows_are_deterministic() {
        assert_eq!(fake_rows(500, 42), fake_rows(500, 42));
        assert_ne!(fake_rows(500, 42), fake_rows(500, 43));
    }

    #[test]
    fn fake_rows_mix_kinds_plausibly() {
        for seed in [1, 2, 3] {
            let rows = fake_rows(2_000, seed);
            let (mut users, mut folds, mut questions, mut assistants) = (0, 0, 0, 0);
            let (mut seconds, mut minutes) = (0, 0);
            let mut paragraphs = 0;
            for row in &rows {
                match row {
                    TranscriptRow::User { text, .. } => {
                        users += 1;
                        let words = text.split_whitespace().count();
                        assert!((3..=40).contains(&words), "{words}");
                    }
                    TranscriptRow::Assistant { text, .. } => {
                        assistants += 1;
                        paragraphs = paragraphs.max(text.split("\n\n").count());
                    }
                    TranscriptRow::ToolFold {
                        tools,
                        secs,
                        summary,
                        ..
                    } => {
                        folds += 1;
                        assert!((3..=15).contains(tools), "{tools}");
                        // The summary's counts add up to the tool count.
                        let counted: u32 = summary
                            .split(|c: char| !c.is_ascii_digit())
                            .filter_map(|n| n.parse::<u32>().ok())
                            .sum();
                        assert_eq!(counted, *tools, "{summary}");
                        if *secs < 60 {
                            seconds += 1;
                        } else {
                            minutes += 1;
                        }
                    }
                    TranscriptRow::Question {
                        prompt, options, ..
                    } => {
                        questions += 1;
                        assert!(prompt.ends_with('?'), "{prompt}");
                        assert!((2..=4).contains(&options.len()));
                        assert!(options.iter().all(|o| !o.is_empty() && !o.ends_with('.')));
                    }
                }
            }
            let fold_share = f64::from(folds) / f64::from(users);
            assert!(
                (0.5..=0.9).contains(&fold_share),
                "folds per turn {fold_share}"
            );
            assert!(
                questions > 0 && questions * 6 < users,
                "{questions} questions in {users} turns"
            );
            assert!(assistants >= users - 1);
            assert!(seconds > 0 && minutes > 0, "{seconds}s / {minutes}m folds");
            assert!(paragraphs >= 5);
        }
    }

    /// Splits a burst into (separator, word) pairs; panics on any other whitespace.
    fn tokens(burst: &str) -> Vec<(&str, &str)> {
        let mut out = Vec::new();
        let mut rest = burst;
        while !rest.is_empty() {
            let sep = if rest.starts_with("\n\n") {
                2
            } else if rest.starts_with(' ') {
                1
            } else {
                panic!("no separator before {rest:?}");
            };
            let (sep, tail) = rest.split_at(sep);
            let end = tail.find([' ', '\n']).unwrap_or(tail.len());
            assert!(end > 0, "empty word in {burst:?}");
            assert!(!tail[..end].contains(char::is_whitespace));
            out.push((sep, &tail[..end]));
            rest = &tail[end..];
        }
        out
    }

    #[test]
    fn word_stream_counts_and_separators() {
        let mut stream = WordStream::new(5);
        assert_eq!(stream.next_burst(0), "");
        for words in [1, 2, 3, 17, 250, 1_000] {
            let burst = stream.next_burst(words);
            assert!(burst.starts_with(' ') || burst.starts_with("\n\n"));
            assert_eq!(tokens(&burst).len(), words);
        }
    }

    #[test]
    fn word_stream_breaks_paragraphs_every_60_to_120_words() {
        let text = WordStream::new(8).next_burst(20_000);
        let mut lengths = Vec::new();
        let mut words = 0;
        for (sep, _) in tokens(&text) {
            if sep == "\n\n" {
                lengths.push(words);
                words = 0;
            }
            words += 1;
        }
        assert!(words <= 120);
        assert!(lengths.len() >= 20_000 / 120, "{} breaks", lengths.len());
        assert!(
            lengths.iter().all(|n| (60..=120).contains(n)),
            "{lengths:?}"
        );
        // A paragraph closes its last sentence.
        let paragraphs: Vec<&str> = text.split("\n\n").collect();
        assert!(paragraphs[..paragraphs.len() - 1]
            .iter()
            .all(|p| p.ends_with('.')));
    }

    #[test]
    fn word_stream_is_deterministic_across_bursts() {
        let whole = WordStream::new(3).next_burst(5_000);
        let mut stream = WordStream::new(3);
        let mut pieces = String::new();
        for words in [1, 0, 7, 120, 61, 2, 4_000, 809] {
            pieces.push_str(&stream.next_burst(words));
        }
        assert_eq!(pieces, whole);
        assert_ne!(
            WordStream::new(4).next_burst(200),
            WordStream::new(3).next_burst(200)
        );
    }
}
