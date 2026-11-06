//! Syntax highlighting. syntect parses every line with the TextMate grammars bundled by two-face;
//! this module turns the scopes it reports into a few token classes and the classes into colours of
//! the FEMME theme. No syntect colour theme is involved, so the code colours follow `theme.rs`.

use std::collections::HashMap;
use std::sync::LazyLock;

use gpui::Rgba;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

use crate::theme::{self, DARK, LIGHT};

/// Every grammar two-face bundles (about 200), parsed lazily; the first use loads the set.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

/// Lines longer than this are shown plain: parsing a minified bundle line by line is not worth it.
const MAX_PARSE_LINE: usize = 4096;

/// What a stretch of source is, as far as the colours care.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// Everything not listed below: identifiers, operators, punctuation.
    Plain,
    /// `// comments`, `# comments`, doc comments.
    Comment,
    /// `fn`, `if`, `class`, tags, headings.
    Keyword,
    /// String and character literals, with their quotes.
    Str,
    /// Numbers, constants such as `true` and `None`, escapes.
    Number,
    /// Type and class names, attribute names.
    Type,
    /// Function and macro names.
    Function,
    /// What the grammar itself marks as wrong.
    Invalid,
}

/// Scope prefixes and their class, most specific first: a scope takes the first rule that is a
/// prefix of it. `Plain` rules stop the search, so `keyword.operator` is not a keyword.
const RULES: &[(&str, Class)] = &[
    ("comment", Class::Comment),
    ("string", Class::Str),
    ("constant.character.escape", Class::Number),
    ("constant.numeric", Class::Number),
    ("constant", Class::Number),
    ("keyword.operator", Class::Plain),
    ("keyword", Class::Keyword),
    ("storage", Class::Keyword),
    ("entity.name.function", Class::Function),
    ("entity.name.macro", Class::Function),
    ("support.function", Class::Function),
    ("support.macro", Class::Function),
    ("variable.function", Class::Function),
    ("entity.name.tag", Class::Keyword),
    ("entity.other.attribute-name", Class::Type),
    ("entity.name.type", Class::Type),
    ("entity.name.class", Class::Type),
    ("entity.name.struct", Class::Type),
    ("entity.name.enum", Class::Type),
    ("entity.name.trait", Class::Type),
    ("entity.name.namespace", Class::Type),
    ("entity.other.inherited-class", Class::Type),
    ("support.type", Class::Type),
    ("support.class", Class::Type),
    ("variable.language", Class::Number),
    ("meta.annotation", Class::Type),
    ("meta.attribute", Class::Type),
    ("markup.heading", Class::Keyword),
    ("markup.raw", Class::Str),
    ("markup.underline.link", Class::Function),
    ("markup.quote", Class::Comment),
    ("invalid", Class::Invalid),
];

/// [`RULES`] with their prefixes parsed.
static PARSED_RULES: LazyLock<Vec<(Scope, Class)>> = LazyLock::new(|| {
    RULES
        .iter()
        .filter_map(|&(prefix, class)| Some((Scope::new(prefix).ok()?, class)))
        .collect()
});

/// The class of one scope, if any rule names it.
fn rule_for(scope: Scope) -> Option<Class> {
    PARSED_RULES
        .iter()
        .find(|(prefix, _)| prefix.is_prefix_of(scope))
        .map(|&(_, class)| class)
}

/// `a` moved `t` of the way to `b`.
fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: 1.,
    }
}

/// The text colour of a class in the live theme (see [`colors`]).
pub(crate) fn color(class: Class) -> Rgba {
    colors(theme::is_dark())[class as usize]
}

/// The text colour of each class, in the order of the enum, for one theme: five hues that stay
/// apart on paper. Light: the status colours of the theme, darkened toward ink until they read on
/// paper. Dark: the same, but ink is the light end there, so the tokens (already light) stay as
/// they are, the keyword pink being the pink for text. The test holds each to 4.5:1 on its paper.
fn colors(dark: bool) -> &'static [Rgba; 8] {
    static LIGHT_COLORS: LazyLock<[Rgba; 8]> = LazyLock::new(|| {
        let p = &LIGHT;
        [
            p.ink,                     // Plain
            p.muted,                   // Comment
            mix(p.hover, p.ink, 0.30), // Keyword: pink
            mix(p.work, p.ink, 0.35),  // Str: green
            mix(p.bad, p.ink, 0.25),   // Number: red
            mix(p.pause, p.ink, 0.50), // Type: ochre
            mix(p.pr, p.ink, 0.40),    // Function: teal
            p.bad,                     // Invalid
        ]
    });
    static DARK_COLORS: LazyLock<[Rgba; 8]> = LazyLock::new(|| {
        let p = &DARK;
        [
            p.ink,                   // Plain
            p.muted,                 // Comment
            p.accent_text,           // Keyword: pink
            p.work,                  // Str: green
            mix(p.bad, p.ink, 0.25), // Number: red, a shade paler than Invalid
            p.pause,                 // Type: amber
            p.pr,                    // Function: teal
            p.bad,                   // Invalid
        ]
    });
    if dark {
        &DARK_COLORS
    } else {
        &LIGHT_COLORS
    }
}

/// A coloured stretch of one line, in bytes of that line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    /// First byte.
    pub(crate) start: u32,
    /// One past the last byte.
    pub(crate) end: u32,
    /// What it is.
    pub(crate) class: Class,
}

/// The coloured stretches of consecutive lines; plain stretches are the gaps.
#[derive(Default)]
pub(crate) struct Spans {
    spans: Vec<Span>,
    /// `starts[i]` is where line `i`'s spans begin in `spans`; one entry more than lines done.
    starts: Vec<u32>,
}

impl Spans {
    /// How many lines have been highlighted.
    pub(crate) fn lines_done(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }

    /// The coloured stretches of line `i`; none if it is not highlighted (yet).
    pub(crate) fn line(&self, i: usize) -> &[Span] {
        match (self.starts.get(i), self.starts.get(i + 1)) {
            (Some(&a), Some(&b)) => &self.spans[a as usize..b as usize],
            _ => &[],
        }
    }

    /// Appends the lines of `next`, which come right after this one's.
    pub(crate) fn append(&mut self, next: Spans) {
        if self.starts.is_empty() {
            self.starts.push(0);
        }
        let base = self.spans.len() as u32;
        self.starts
            .extend(next.starts.iter().skip(1).map(|s| s + base));
        self.spans.extend(next.spans);
    }

    /// Closes the current line.
    fn end_line(&mut self) {
        if self.starts.is_empty() {
            self.starts.push(0);
        }
        self.starts.push(self.spans.len() as u32);
    }

    /// Adds a stretch to the current line, joining it to the previous one of the same class.
    fn push(&mut self, start: usize, end: usize, class: Class) {
        if class == Class::Plain || start >= end {
            return;
        }
        let line_start = self.starts.last().copied().unwrap_or(0) as usize;
        if let Some(last) = self.spans[line_start..].last_mut() {
            if last.class == class && last.end as usize == start {
                last.end = end as u32;
                return;
            }
        }
        self.spans.push(Span {
            start: start as u32,
            end: end as u32,
            class,
        });
    }
}

/// Parses one file, a chunk of lines at a time: the parse state and the scope stack carry over from
/// one call of [`Highlighter::run`] to the next.
pub(crate) struct Highlighter {
    language: &'static str,
    parse: ParseState,
    stack: ScopeStack,
    memo: HashMap<Scope, Option<Class>>,
    buf: String,
}

impl Highlighter {
    /// The highlighter for a file named `name` whose first line is `first_line`; `None` when the
    /// grammars know nothing about it (plain text).
    pub(crate) fn for_file(name: &str, first_line: &str) -> Option<Highlighter> {
        let set: &'static SyntaxSet = &SYNTAXES;
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        let ext = base.rsplit_once('.').map(|(_, e)| e);
        let syntax: &'static SyntaxReference = set
            .find_syntax_by_extension(base)
            .or_else(|| ext.and_then(|e| set.find_syntax_by_extension(e)))
            .or_else(|| set.find_syntax_by_first_line(first_line))
            .filter(|s| s.name != "Plain Text")?;
        Some(Highlighter {
            language: syntax.name.as_str(),
            parse: ParseState::new(syntax),
            stack: ScopeStack::new(),
            memo: HashMap::new(),
            buf: String::new(),
        })
    }

    /// The grammar's name: `Rust`, `Markdown`, ...
    pub(crate) fn language(&self) -> &'static str {
        self.language
    }

    /// The class of the innermost scope of the stack that a rule names.
    fn class(&mut self) -> Class {
        for &scope in self.stack.as_slice().iter().rev() {
            let found = *self.memo.entry(scope).or_insert_with(|| rule_for(scope));
            if let Some(class) = found {
                return class;
            }
        }
        Class::Plain
    }

    /// Highlights `lines` (their text without the line ending) and returns the coloured stretches.
    pub(crate) fn run<'a>(&mut self, lines: impl Iterator<Item = &'a str>) -> Spans {
        let mut out = Spans::default();
        for line in lines {
            if line.len() <= MAX_PARSE_LINE {
                self.line(line, &mut out);
            }
            out.end_line();
        }
        out
    }

    /// Parses one line, keeping the state for the next.
    fn line(&mut self, line: &str, out: &mut Spans) {
        self.buf.clear();
        self.buf.push_str(line);
        self.buf.push('\n');
        let Ok(ops) = self.parse.parse_line(&self.buf, &SYNTAXES) else {
            return;
        };
        let mut pos = 0;
        let mut class = self.class();
        for (at, op) in ops {
            let at = at.min(line.len());
            if at > pos {
                out.push(pos, at, class);
                pos = at;
            }
            if self.stack.apply(&op).is_err() {
                return;
            }
            class = self.class();
        }
        out.push(pos, line.len(), class);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance of an sRGB colour.
    fn luminance(c: Rgba) -> f32 {
        let lin = |v: f32| {
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
    }

    fn contrast(a: Rgba, b: Rgba) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn every_class_reads_on_paper() {
        for (name, dark, paper) in [("light", false, LIGHT.paper), ("dark", true, DARK.paper)] {
            for class in [
                Class::Plain,
                Class::Comment,
                Class::Keyword,
                Class::Str,
                Class::Number,
                Class::Type,
                Class::Function,
                Class::Invalid,
            ] {
                let c = contrast(colors(dark)[class as usize], paper);
                assert!(c >= 4.5, "{name}: {class:?} has contrast {c:.2} on paper");
            }
        }
    }

    #[test]
    fn the_hues_stay_apart_in_both_themes() {
        for dark in [false, true] {
            let c = colors(dark);
            for a in 0..c.len() {
                for b in a + 1..c.len() {
                    // Number and Invalid are both red on purpose; the rest must differ.
                    if (a, b) == (Class::Number as usize, Class::Invalid as usize) {
                        continue;
                    }
                    assert_ne!(
                        c[a], c[b],
                        "dark={dark}: classes {a} and {b} share a colour"
                    );
                }
            }
        }
        assert_eq!(
            color(Class::Plain),
            colors(theme::is_dark())[Class::Plain as usize]
        );
    }

    #[test]
    fn spans_join_neighbours_and_index_by_line() {
        let mut a = Spans::default();
        a.push(0, 3, Class::Keyword);
        a.push(3, 5, Class::Keyword);
        a.push(5, 6, Class::Plain);
        a.push(6, 8, Class::Str);
        a.end_line();
        a.end_line();
        let mut b = Spans::default();
        b.push(1, 2, Class::Comment);
        b.end_line();
        a.append(b);
        assert_eq!(a.lines_done(), 3);
        assert_eq!(
            a.line(0),
            &[
                Span {
                    start: 0,
                    end: 5,
                    class: Class::Keyword
                },
                Span {
                    start: 6,
                    end: 8,
                    class: Class::Str
                },
            ]
        );
        assert!(a.line(1).is_empty());
        assert_eq!(
            a.line(2),
            &[Span {
                start: 1,
                end: 2,
                class: Class::Comment
            }]
        );
        assert!(a.line(3).is_empty(), "not highlighted yet");
    }

    #[test]
    fn plain_text_has_no_highlighter_and_code_has() {
        assert!(Highlighter::for_file("notes.txt", "hello\n").is_none());
        assert!(Highlighter::for_file("data", "just words\n").is_none());
        let rs = Highlighter::for_file("src/main.rs", "fn main() {}\n").unwrap();
        assert_eq!(rs.language(), "Rust");
        assert_eq!(
            Highlighter::for_file("Makefile", "all:\n")
                .unwrap()
                .language(),
            "Makefile"
        );
        // No extension: the shebang decides.
        assert!(Highlighter::for_file("run", "#!/bin/sh\n").is_some());
    }

    #[test]
    fn rust_tokens_get_their_classes() {
        let src = ["let n = 42; // note", "let s = \"hi\";"];
        let mut hl = Highlighter::for_file("a.rs", src[0]).unwrap();
        let spans = hl.run(src.iter().copied());
        let class_at = |line: usize, byte: u32| {
            spans
                .line(line)
                .iter()
                .find(|s| s.start <= byte && byte < s.end)
                .map_or(Class::Plain, |s| s.class)
        };
        assert_eq!(class_at(0, 0), Class::Keyword, "let");
        assert_eq!(class_at(0, 4), Class::Plain, "n");
        assert_eq!(class_at(0, 8), Class::Number, "42");
        assert_eq!(class_at(0, 14), Class::Comment, "// note");
        assert_eq!(class_at(1, 8), Class::Str, "the opening quote");
        assert_eq!(class_at(1, 9), Class::Str, "h");
        assert_eq!(class_at(1, 11), Class::Str, "the closing quote");
        assert_eq!(class_at(1, 12), Class::Plain, "the semicolon");
    }

    #[test]
    fn state_carries_across_calls() {
        let mut hl = Highlighter::for_file("a.rs", "").unwrap();
        let first = hl.run(["/* a block", "comment"].into_iter());
        assert!(first.line(0).iter().all(|s| s.class == Class::Comment));
        let second = hl.run(["still */ let"].into_iter());
        assert_eq!(
            second.line(0).first().map(|s| s.class),
            Some(Class::Comment)
        );
        assert_eq!(second.line(0).last().map(|s| s.class), Some(Class::Keyword));
    }
}
