//! Markdown to a block model the list can lay out: paragraphs, headings, code blocks, lists,
//! quotes, tables, with the inline styles of a paragraph as byte ranges over its text, and where
//! its links go (a bare `http://` or `https://` address in the text is a link too). The parser
//! is pulldown-cmark; this module only folds its events into blocks (no HTML, no gpui), so it is
//! tested on plain strings. Text that is still streaming parses as it stands: an unclosed fence
//! is a code block to the end, an unclosed `**` is literal text.

use std::ops::Range;

use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd, TextMergeStream,
};

/// The inline styles in force over a stretch of text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    /// `**strong**`.
    pub bold: bool,
    /// `*emphasis*`.
    pub italic: bool,
    /// `` `code` ``.
    pub code: bool,
    /// `~~struck~~`.
    pub strike: bool,
    /// The text of a link.
    pub link: bool,
    /// Secondary text: the address after a link's words.
    pub dim: bool,
}

impl Style {
    fn is_plain(self) -> bool {
        self == Style::default()
    }
}

/// A styled stretch of an [`Inline`]'s text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// Where, in bytes of the text.
    pub range: Range<usize>,
    /// How it looks.
    pub style: Style,
}

/// Text with styled stretches: the spans do not overlap and are in order; text between them is
/// plain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inline {
    /// The words.
    pub text: String,
    /// The styled stretches.
    pub spans: Vec<Span>,
    /// Where a click opens what: the bytes of the text that are a link's words (or a bare
    /// address) and the address, in order, not overlapping. The address a link's words are
    /// followed by, dim, is not among them, and only web and mail addresses are.
    pub links: Vec<(Range<usize>, String)>,
}

impl Inline {
    fn push(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        if style.is_plain() {
            return;
        }
        match self.spans.last_mut() {
            Some(last) if last.style == style && last.range.end == start => {
                last.range.end = self.text.len();
            }
            _ => self.spans.push(Span {
                range: start..self.text.len(),
                style,
            }),
        }
    }

    /// Whether there is no text.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// How a table column is aligned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    /// No alignment given: left.
    #[default]
    Left,
    /// `:---:`.
    Center,
    /// `---:`.
    Right,
}

/// An item of a list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListItem {
    /// `- [ ]` / `- [x]`: the box, ticked or not.
    pub task: Option<bool>,
    /// What the item holds.
    pub blocks: Vec<Block>,
}

/// One block of a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// A paragraph.
    Paragraph(Inline),
    /// `#` to `######`.
    Heading {
        /// 1 to 6.
        level: u8,
        /// The words.
        text: Inline,
    },
    /// A fenced or indented code block.
    Code {
        /// The word after the opening fence.
        lang: Option<String>,
        /// The code, without the newline that ends it.
        code: String,
    },
    /// A bulleted or numbered list.
    List {
        /// Numbered.
        ordered: bool,
        /// The number of the first item of a numbered list.
        start: u64,
        /// The items.
        items: Vec<ListItem>,
    },
    /// `>` quoted blocks.
    Quote(Vec<Block>),
    /// A thematic break.
    Rule,
    /// A GFM table.
    Table {
        /// The alignment of each column.
        aligns: Vec<Align>,
        /// The header cells.
        head: Vec<Inline>,
        /// The body rows.
        rows: Vec<Vec<Inline>>,
    },
}

/// A parsed document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Doc {
    /// The blocks, top to bottom.
    pub blocks: Vec<Block>,
}

struct ListCtx {
    ordered: bool,
    start: u64,
    items: Vec<ListItem>,
}

struct TableCtx {
    aligns: Vec<Align>,
    head: Vec<Inline>,
    rows: Vec<Vec<Inline>>,
    row: Vec<Inline>,
}

/// Folds pulldown-cmark's events into blocks.
#[derive(Default)]
struct Builder {
    /// The blocks of the container being filled (document, quote, list item), innermost last.
    containers: Vec<Vec<Block>>,
    lists: Vec<ListCtx>,
    /// Task marker of each open list item.
    tasks: Vec<Option<bool>>,
    table: Option<TableCtx>,
    /// The text of the paragraph, heading or table cell being read.
    cur: Inline,
    /// Open `**`, `*`, `~~` (counted: they nest).
    bold: u32,
    italic: u32,
    strike: u32,
    /// The target of the link being read, and where its words start.
    link: Option<(String, usize)>,
    /// The fenced or indented code being read.
    code: Option<(Option<String>, String)>,
    heading: Option<u8>,
}

impl Builder {
    fn style(&self) -> Style {
        Style {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            link: self.link.is_some(),
            ..Style::default()
        }
    }

    fn text(&mut self, t: &str) {
        if let Some((_, code)) = &mut self.code {
            code.push_str(t);
        } else {
            let style = self.style();
            self.cur.push(t, style);
        }
    }

    /// Text of the document. A bare `http://` or `https://` address in it is a link as well
    /// (CommonMark only has `<address>`), unless it is in code or in the words of a link.
    fn words(&mut self, t: &str) {
        let mut rest = t;
        if self.code.is_none() && self.link.is_none() {
            while let Some(found) = find_url(rest) {
                self.text(&rest[..found.start]);
                let url = &rest[found.clone()];
                let at = self.cur.text.len();
                let style = Style {
                    link: true,
                    ..self.style()
                };
                self.cur.push(url, style);
                self.cur.links.push((at..at + url.len(), url.to_owned()));
                rest = &rest[found.end..];
            }
        }
        self.text(rest);
    }

    fn block(&mut self, b: Block) {
        if let Some(top) = self.containers.last_mut() {
            top.push(b);
        }
    }

    /// Text outside a paragraph (a tight list item) becomes one.
    fn flush(&mut self) {
        if self.heading.is_none() && self.table.is_none() && !self.cur.is_empty() {
            let inline = std::mem::take(&mut self.cur);
            self.block(Block::Paragraph(inline));
        }
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(end) => self.end(end),
            Event::Text(t) => self.words(&t),
            Event::Code(t) => {
                let style = Style {
                    code: true,
                    ..self.style()
                };
                self.cur.push(&t, style);
            }
            Event::SoftBreak | Event::HardBreak => self.text("\n"),
            Event::Html(t) | Event::InlineHtml(t) => self.text(&t),
            Event::FootnoteReference(t) => self.text(&format!("[^{t}]")),
            Event::Rule => {
                self.flush();
                self.block(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                if let Some(task) = self.tasks.last_mut() {
                    *task = Some(done);
                }
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => self.text(&t),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush();
                self.heading = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                });
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.containers.push(Vec::new());
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .map(str::to_owned)
                        .filter(|l| !l.is_empty()),
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(first) => {
                self.flush();
                self.lists.push(ListCtx {
                    ordered: first.is_some(),
                    start: first.unwrap_or(1),
                    items: Vec::new(),
                });
            }
            Tag::Item => {
                self.flush();
                self.containers.push(Vec::new());
                self.tasks.push(None);
            }
            Tag::Table(aligns) => {
                self.flush();
                self.table = Some(TableCtx {
                    aligns: aligns
                        .iter()
                        .map(|a| match a {
                            Alignment::Center => Align::Center,
                            Alignment::Right => Align::Right,
                            Alignment::None | Alignment::Left => Align::Left,
                        })
                        .collect(),
                    head: Vec::new(),
                    rows: Vec::new(),
                    row: Vec::new(),
                });
            }
            Tag::TableHead | Tag::TableRow => {}
            Tag::TableCell => self.cur = Inline::default(),
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => {
                self.link = Some((dest_url.to_string(), self.cur.text.len()));
            }
            Tag::Image { .. } => self.text("[image: "),
            _ => {}
        }
    }

    fn end(&mut self, end: TagEnd) {
        match end {
            TagEnd::Paragraph => {
                let inline = std::mem::take(&mut self.cur);
                if !inline.is_empty() {
                    self.block(Block::Paragraph(inline));
                }
            }
            TagEnd::Heading(_) => {
                let text = std::mem::take(&mut self.cur);
                if let Some(level) = self.heading.take() {
                    self.block(Block::Heading { level, text });
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                if let Some(children) = self.containers.pop() {
                    self.block(Block::Quote(children));
                }
            }
            TagEnd::CodeBlock => {
                if let Some((lang, mut code)) = self.code.take() {
                    if code.ends_with('\n') {
                        code.pop();
                    }
                    self.block(Block::Code { lang, code });
                }
            }
            TagEnd::List(_) => {
                if let Some(list) = self.lists.pop() {
                    self.block(Block::List {
                        ordered: list.ordered,
                        start: list.start,
                        items: list.items,
                    });
                }
            }
            TagEnd::Item => {
                self.flush();
                let blocks = self.containers.pop().unwrap_or_default();
                let task = self.tasks.pop().flatten();
                if let Some(list) = self.lists.last_mut() {
                    list.items.push(ListItem { task, blocks });
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.block(Block::Table {
                        aligns: t.aligns,
                        head: t.head,
                        rows: t.rows,
                    });
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head = std::mem::take(&mut t.row);
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.cur);
                if let Some(t) = &mut self.table {
                    t.row.push(cell);
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                if let Some((url, from)) = self.link.take() {
                    let to = self.cur.text.len();
                    // The words carry the link style already (it was on while they were read).
                    // Show where the link goes unless the words are the address.
                    if !url.is_empty() && self.cur.text[from..] != url {
                        let style = Style {
                            dim: true,
                            ..self.style()
                        };
                        self.cur.push(&format!(" ({url})"), style);
                    }
                    if from < to && opens(&url) {
                        self.cur.links.push((from..to, url));
                    }
                }
            }
            TagEnd::Image => self.text("]"),
            _ => {}
        }
    }
}

/// Whether a click may open a link to `url`: a web address or a mail one. Anything else the
/// model writes (a `file:` path, `javascript:`, a program's own scheme, a relative path) is shown
/// but not opened.
fn opens(url: &str) -> bool {
    ["http://", "https://", "mailto:"].iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    })
}

/// Where the first bare `http://` or `https://` address in `s` is: it starts a word (not `xhttp://`),
/// runs to the next space (or `<`, `>`, `"`), and does not end in the punctuation of the sentence
/// around it.
fn find_url(s: &str) -> Option<Range<usize>> {
    let mut from = 0;
    while let Some(i) = s[from..].find("http") {
        let start = from + i;
        from = start + 4;
        let rest = &s[start..];
        let Some(scheme) = ["https://", "http://"]
            .into_iter()
            .find(|scheme| rest.starts_with(scheme))
        else {
            continue;
        };
        if s[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"'))
            .unwrap_or(rest.len());
        let url = trim_tail(&rest[..end]);
        // Something has to follow the `//`.
        if url.len() > scheme.len() {
            return Some(start..start + url.len());
        }
    }
    None
}

/// `url` without the punctuation that ends a sentence (`.,;:!?'`) and without a closing bracket
/// that has no opening one in the address (`wikipedia.org/wiki/Foo_(bar)` keeps its own).
fn trim_tail(url: &str) -> &str {
    let mut end = url.len();
    while let Some(last) = url[..end].chars().next_back() {
        let open = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' => None,
            ')' => Some('('),
            ']' => Some('['),
            _ => break,
        };
        if let Some(open) = open {
            let kept = &url[..end];
            if kept.matches(open).count() >= kept.matches(last).count() {
                break;
            }
        }
        end -= last.len_utf8();
    }
    &url[..end]
}

/// Parses `src` into blocks.
pub fn parse(src: &str) -> Doc {
    let mut b = Builder::default();
    b.containers.push(Vec::new());
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    // The parser splits a run of text at `~`, `_`, `&amp;`, `[`: a bare address is looked for in
    // the whole run.
    for ev in TextMergeStream::new(Parser::new_ext(src, opts)) {
        b.event(ev);
    }
    b.flush();
    Doc {
        blocks: b.containers.pop().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(text: &str) -> Block {
        Block::Paragraph(Inline {
            text: text.into(),
            ..Inline::default()
        })
    }

    fn only_paragraph(src: &str) -> Inline {
        match parse(src).blocks.as_slice() {
            [Block::Paragraph(p)] => p.clone(),
            b => panic!("{b:?}"),
        }
    }

    fn style_of(p: &Inline, word: &str) -> Style {
        let at = p.text.find(word).unwrap();
        p.spans
            .iter()
            .find(|s| s.range.contains(&at))
            .map(|s| s.style)
            .unwrap_or_default()
    }

    #[test]
    fn a_paragraph_with_inline_styles() {
        let p = only_paragraph("plain **bold** and *italic* and `code` and ~~gone~~ end");
        assert_eq!(p.text, "plain bold and italic and code and gone end");
        assert!(style_of(&p, "bold").bold);
        assert!(style_of(&p, "italic").italic);
        assert!(style_of(&p, "code").code);
        assert!(style_of(&p, "gone").strike);
        assert!(style_of(&p, "plain").is_plain());
        assert!(style_of(&p, " and ").is_plain());
        assert_eq!(p.spans.len(), 4);
        for s in &p.spans {
            assert!(p.text.is_char_boundary(s.range.start) && p.text.is_char_boundary(s.range.end));
        }
    }

    #[test]
    fn nested_styles_combine() {
        let p = only_paragraph("***both*** and **bold *and italic* still**");
        assert_eq!(p.text, "both and bold and italic still");
        let both = style_of(&p, "both");
        assert!(both.bold && both.italic);
        let mid = style_of(&p, "and italic");
        assert!(mid.bold && mid.italic);
        let still = style_of(&p, "still");
        assert!(still.bold && !still.italic);
        // Adjacent text of one style is one span.
        let words: Vec<_> = p.spans.iter().map(|s| &p.text[s.range.clone()]).collect();
        assert_eq!(words, ["both", "bold ", "and italic", " still"]);
    }

    #[test]
    fn links_show_their_words_and_where_they_go() {
        let p = only_paragraph("see [the docs](https://example.com/a) now");
        assert_eq!(p.text, "see the docs (https://example.com/a) now");
        assert!(style_of(&p, "the docs").link);
        let url = style_of(&p, "(https");
        assert!(url.dim && !url.link);
        // The words are the address: nothing is repeated.
        let p = only_paragraph("<https://example.com>");
        assert_eq!(p.text, "https://example.com");
        assert!(style_of(&p, "example").link);
    }

    /// The words and the address of each link of `p`.
    fn links_of(p: &Inline) -> Vec<(&str, &str)> {
        p.links
            .iter()
            .map(|(r, url)| (&p.text[r.clone()], url.as_str()))
            .collect()
    }

    /// The addresses `src`, a paragraph, links to.
    fn urls(src: &str) -> Vec<String> {
        only_paragraph(src)
            .links
            .into_iter()
            .map(|(_, url)| url)
            .collect()
    }

    #[test]
    fn a_link_keeps_its_target_over_its_words() {
        let p = only_paragraph("see [the docs](https://example.com/a) now");
        assert_eq!(p.text, "see the docs (https://example.com/a) now");
        // The words, not the dim address after them.
        assert_eq!(p.links, [(4..12, "https://example.com/a".to_owned())]);
        // The words are the address: one range over them, nothing repeated.
        let p =
            only_paragraph("<https://example.com> and [http://a.example/b](http://a.example/b)");
        assert_eq!(p.text, "https://example.com and http://a.example/b");
        assert_eq!(
            links_of(&p),
            [
                ("https://example.com", "https://example.com"),
                ("http://a.example/b", "http://a.example/b")
            ]
        );
        assert!(only_paragraph("plain *text*").links.is_empty());
    }

    #[test]
    fn two_links_in_one_paragraph_keep_their_own_targets() {
        let p = only_paragraph("[one](https://a.example) and [two](https://b.example/x?y=1) done");
        assert_eq!(
            links_of(&p),
            [
                ("one", "https://a.example"),
                ("two", "https://b.example/x?y=1")
            ]
        );
        assert!(p.links[0].0.end <= p.links[1].0.start);
        // Touching links stay two although their style is one stretch.
        let p = only_paragraph(
            "[http://x.example](http://x.example)[http://y.example](http://y.example)",
        );
        assert_eq!(p.spans.len(), 1);
        assert_eq!(
            p.links,
            [
                (0..16, "http://x.example".to_owned()),
                (16..32, "http://y.example".to_owned())
            ]
        );
    }

    #[test]
    fn a_link_inside_bold_keeps_both() {
        let p = only_paragraph("**read [the docs](https://example.com/a) now**");
        assert_eq!(links_of(&p), [("the docs", "https://example.com/a")]);
        let words = style_of(&p, "the docs");
        assert!(words.bold && words.link);
        // The address after the words is not part of what a click opens.
        let after = style_of(&p, "(https");
        assert!(after.bold && after.dim && !after.link);
    }

    #[test]
    fn only_web_and_mail_addresses_open() {
        let p = only_paragraph(
            "[a](file:///etc/passwd) [b](javascript:alert(1)) [c](./README.md) [d](#top) \
             [e](vscode://x/y) [f](HTTPS://Up.example) [g](mailto:me@x.example)",
        );
        assert_eq!(
            links_of(&p),
            [("f", "HTTPS://Up.example"), ("g", "mailto:me@x.example")]
        );
        // The others still say where they go.
        assert!(p
            .text
            .contains("a (file:///etc/passwd) b (javascript:alert(1)) c (./README.md)"));
        assert!(!opens("") && !opens("http:/x") && !opens("é"));
    }

    #[test]
    fn a_bare_address_is_a_link() {
        let p = only_paragraph("go to https://example.com/a?b=1&c=2 now");
        assert_eq!(p.text, "go to https://example.com/a?b=1&c=2 now");
        assert_eq!(
            links_of(&p),
            [(
                "https://example.com/a?b=1&c=2",
                "https://example.com/a?b=1&c=2"
            )]
        );
        assert!(style_of(&p, "example").link);
        assert!(style_of(&p, "go to").is_plain() && style_of(&p, "now").is_plain());
        // Several, either scheme, alone on the line, in the words of a heading.
        let p = only_paragraph("http://a.example, then https://b.example/x\nhttps://c.example");
        assert_eq!(
            links_of(&p),
            [
                ("http://a.example", "http://a.example"),
                ("https://b.example/x", "https://b.example/x"),
                ("https://c.example", "https://c.example")
            ]
        );
        // Styled text keeps its style next to the link.
        let p = only_paragraph("**https://example.com/b** and ~~old https://c.example/d~~");
        assert_eq!(
            links_of(&p),
            [
                ("https://example.com/b", "https://example.com/b"),
                ("https://c.example/d", "https://c.example/d")
            ]
        );
        let first = style_of(&p, "example.com");
        assert!(first.bold && first.link);
        let second = style_of(&p, "c.example");
        assert!(second.strike && second.link);
        assert!(style_of(&p, "old").strike && !style_of(&p, "old").link);
    }

    #[test]
    fn a_bare_address_leaves_out_the_punctuation_around_it() {
        for (src, url) in [
            ("see https://example.com.", "https://example.com"),
            ("see https://example.com/a, then", "https://example.com/a"),
            ("(see https://example.com/a)", "https://example.com/a"),
            ("[https://example.com/a]", "https://example.com/a"),
            ("is it https://example.com/a?", "https://example.com/a"),
            ("so: 'https://example.com/a'", "https://example.com/a"),
            ("\"https://example.com/a\"", "https://example.com/a"),
            ("also https://example.com/a;", "https://example.com/a"),
            ("at https://example.com/a:", "https://example.com/a"),
            ("at https://example.com/a...", "https://example.com/a"),
            ("at https://example.com/a?x=1.", "https://example.com/a?x=1"),
            // Brackets that close one of the address's own stay.
            (
                "(https://en.wikipedia.org/wiki/Foo_(bar))",
                "https://en.wikipedia.org/wiki/Foo_(bar)",
            ),
            (
                "https://en.wikipedia.org/wiki/Foo_(bar).",
                "https://en.wikipedia.org/wiki/Foo_(bar)",
            ),
            ("see http://[::1]:8080/x, ok", "http://[::1]:8080/x"),
        ] {
            assert_eq!(urls(src), [url], "{src}");
        }
    }

    #[test]
    fn an_address_that_is_not_one_is_left_alone() {
        // In code, in the words of a link (only that link counts), in a code block.
        let p = only_paragraph(
            "`https://example.com/a` and [see https://x.example/b here](https://y.example/c)",
        );
        assert_eq!(
            links_of(&p),
            [("see https://x.example/b here", "https://y.example/c")]
        );
        assert!(matches!(
            parse("```\nhttps://example.com/a\n```").blocks.as_slice(),
            [Block::Code { .. }]
        ));
        // The scheme alone, in the middle of a word, with nothing after it.
        for src in [
            "the https:// scheme",
            "xhttp://y.example",
            "see https://.",
            "see http",
            "see https://",
            "ftp://z.example/a",
        ] {
            assert!(urls(src).is_empty(), "{src}");
        }
    }

    #[test]
    fn an_address_the_parser_cut_into_pieces_is_one_link() {
        // pulldown-cmark ends a run of text at `~`, at an `&amp;`, at `_(` and at `[`.
        for url in [
            "http://example.com/~alice/page",
            "https://example.com/?a=1&b=2",
            "https://en.wikipedia.org/wiki/Foo_(bar)",
            "https://example.com/a[0]/b",
        ] {
            assert_eq!(urls(&format!("see {url} now")), [url], "{url}");
        }
        let p = only_paragraph("a https://example.com/?a=1&amp;b=2 b");
        assert_eq!(
            links_of(&p),
            [(
                "https://example.com/?a=1&b=2",
                "https://example.com/?a=1&b=2"
            )]
        );
    }

    #[test]
    fn an_address_still_streaming_is_a_link_so_far() {
        assert_eq!(urls("see https://exa"), ["https://exa"]);
        assert_eq!(
            urls("see https://example.com/pa"),
            ["https://example.com/pa"]
        );
    }

    #[test]
    fn links_belong_to_the_text_they_are_in() {
        let doc = parse(
            "# See [x](http://x.example) or http://h.example\n\n\
             | a | b |\n|---|---|\n| [c](http://c.example) | http://d.example |\n\n\
             - item http://i.example\n",
        );
        let [Block::Heading { text, .. }, Block::Table { rows, .. }, Block::List { items, .. }] =
            doc.blocks.as_slice()
        else {
            panic!("{doc:?}")
        };
        assert_eq!(
            links_of(text),
            [
                ("x", "http://x.example"),
                ("http://h.example", "http://h.example")
            ]
        );
        assert_eq!(links_of(&rows[0][0]), [("c", "http://c.example")]);
        assert_eq!(
            links_of(&rows[0][1]),
            [("http://d.example", "http://d.example")]
        );
        let Block::Paragraph(item) = &items[0].blocks[0] else {
            panic!("{items:?}")
        };
        assert_eq!(links_of(item), [("http://i.example", "http://i.example")]);
    }

    #[test]
    fn link_ranges_sit_on_character_boundaries() {
        let p =
            only_paragraph("héllo [wörld 😀](https://e.example/ü) und https://exämple.org/ü😀. ok");
        assert_eq!(
            links_of(&p),
            [
                ("wörld 😀", "https://e.example/ü"),
                ("https://exämple.org/ü😀", "https://exämple.org/ü😀")
            ]
        );
    }

    #[test]
    fn inline_code_keeps_its_text_literal() {
        let p = only_paragraph("run `a **b** <c>` now");
        assert_eq!(p.text, "run a **b** <c> now");
        assert!(style_of(&p, "**b**").code);
    }

    #[test]
    fn headings_have_levels() {
        let doc = parse("# One\n\n## Two *x*\n\n###### Six");
        let levels: Vec<(u8, String)> = doc
            .blocks
            .iter()
            .map(|b| match b {
                Block::Heading { level, text } => (*level, text.text.clone()),
                b => panic!("{b:?}"),
            })
            .collect();
        assert_eq!(
            levels,
            [(1, "One".into()), (2, "Two x".into()), (6, "Six".into())]
        );
    }

    #[test]
    fn code_blocks_keep_their_text_and_language() {
        let doc = parse("before\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\n```\nno language\n```\n\n    indented\n");
        assert_eq!(
            doc.blocks,
            [
                para("before"),
                Block::Code {
                    lang: Some("rust".into()),
                    code: "fn main() {\n    println!(\"hi\");\n}".into()
                },
                Block::Code {
                    lang: None,
                    code: "no language".into()
                },
                Block::Code {
                    lang: None,
                    code: "indented".into()
                },
            ]
        );
        // The info string's first word is the language.
        let Block::Code { lang, .. } = &parse("```python title=x\n1\n```").blocks[0] else {
            panic!()
        };
        assert_eq!(lang.as_deref(), Some("python"));
    }

    #[test]
    fn an_unfinished_fence_is_a_code_block_so_far() {
        let doc = parse("text\n\n```sh\nls -la\necho par");
        assert_eq!(
            doc.blocks,
            [
                para("text"),
                Block::Code {
                    lang: Some("sh".into()),
                    code: "ls -la\necho par".into()
                }
            ]
        );
        // An unclosed `**` is literal.
        assert_eq!(only_paragraph("half **bold").text, "half **bold");
        assert_eq!(parse("").blocks, []);
    }

    #[test]
    fn lists_nest_and_number() {
        let doc = parse("- one\n- two\n  - inner a\n  - inner b\n- three\n\n3. third\n4. fourth\n");
        let [Block::List {
            ordered: false,
            items,
            ..
        }, Block::List {
            ordered: true,
            start: 3,
            items: numbered,
        }] = doc.blocks.as_slice()
        else {
            panic!("{doc:?}")
        };
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].blocks, [para("one")]);
        // "two" and its nested list.
        let [p, Block::List { items: inner, .. }] = items[1].blocks.as_slice() else {
            panic!("{:?}", items[1])
        };
        assert_eq!(*p, para("two"));
        assert_eq!(
            inner.iter().map(|i| i.blocks.clone()).collect::<Vec<_>>(),
            [vec![para("inner a")], vec![para("inner b")]]
        );
        assert_eq!(numbered.len(), 2);
    }

    #[test]
    fn loose_list_items_keep_their_paragraphs_apart() {
        let doc = parse("1. first\n\n   more of first\n\n2. second\n");
        let [Block::List { items, .. }] = doc.blocks.as_slice() else {
            panic!("{doc:?}")
        };
        assert_eq!(items[0].blocks, [para("first"), para("more of first")]);
        assert_eq!(items[1].blocks, [para("second")]);
    }

    #[test]
    fn task_lists_carry_their_boxes() {
        let doc = parse("- [x] done\n- [ ] todo\n- plain\n");
        let [Block::List { items, .. }] = doc.blocks.as_slice() else {
            panic!()
        };
        assert_eq!(
            items.iter().map(|i| i.task).collect::<Vec<_>>(),
            [Some(true), Some(false), None]
        );
        assert_eq!(items[0].blocks, [para("done")]);
    }

    #[test]
    fn quotes_hold_blocks() {
        let doc = parse("> quoted *text*\n>\n> - item\n\nafter");
        let [Block::Quote(inner), after] = doc.blocks.as_slice() else {
            panic!("{doc:?}")
        };
        assert!(matches!(&inner[0], Block::Paragraph(p) if p.text == "quoted text"));
        assert!(matches!(&inner[1], Block::List { .. }));
        assert_eq!(*after, para("after"));
    }

    #[test]
    fn line_breaks_are_newlines() {
        assert_eq!(only_paragraph("one\ntwo").text, "one\ntwo");
        assert_eq!(only_paragraph("one  \ntwo").text, "one\ntwo");
    }

    #[test]
    fn tables_have_a_head_rows_and_alignment() {
        let doc = parse("| a | b | c |\n|:--|:-:|--:|\n| 1 | **2** | 3 |\n| 4 | 5 | 6 |\n");
        let [Block::Table { aligns, head, rows }] = doc.blocks.as_slice() else {
            panic!("{doc:?}")
        };
        assert_eq!(aligns, &[Align::Left, Align::Center, Align::Right]);
        assert_eq!(
            head.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1].text, "2");
        assert!(rows[0][1].spans[0].style.bold);
        assert_eq!(rows[1][2].text, "6");
    }

    #[test]
    fn rules_images_html_and_footnotes_are_text_or_rules() {
        let doc = parse("a\n\n---\n\nb");
        assert_eq!(doc.blocks, [para("a"), Block::Rule, para("b")]);
        assert_eq!(
            only_paragraph("![a cat](http://x/c.png) <b>hi</b>").text,
            "[image: a cat] <b>hi</b>"
        );
    }

    #[test]
    fn multibyte_text_keeps_its_spans_on_boundaries() {
        let p = only_paragraph("héllo **wörld 😀** und `ç`");
        assert_eq!(p.text, "héllo wörld 😀 und ç");
        assert!(style_of(&p, "wörld").bold);
        assert!(style_of(&p, "😀").bold);
        assert!(style_of(&p, "ç").code);
        for s in &p.spans {
            assert!(p.text.get(s.range.clone()).is_some());
        }
    }

    #[test]
    fn a_long_message_parses_into_its_blocks_in_order() {
        let src =
            "# Title\n\nIntro paragraph.\n\n- a\n- b\n\n```rust\nlet x = 1;\n```\n\n> note\n\nBye.";
        let kinds: Vec<&str> = parse(src)
            .blocks
            .iter()
            .map(|b| match b {
                Block::Heading { .. } => "h",
                Block::Paragraph(_) => "p",
                Block::List { .. } => "l",
                Block::Code { .. } => "c",
                Block::Quote(_) => "q",
                Block::Rule => "r",
                Block::Table { .. } => "t",
            })
            .collect();
        assert_eq!(kinds, ["h", "p", "l", "c", "q", "p"]);
    }

    /// How long one re-parse of a long streamed message takes (it runs per delta on the UI
    /// thread). Run with `cargo test --release -- --ignored --nocapture parse_time`.
    #[test]
    #[ignore]
    fn parse_time() {
        let unit = "## A heading\n\nSome **bold** and *italic* text with `code` and a \
                    [link](https://example.com) and a bare https://example.org/a_b/c in a \
                    paragraph that runs on for a while.\n\n\
                    - one item\n- two item with `x`\n- three\n\n\
                    ```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n\n\
                    | a | b |\n|---|---|\n| 1 | 2 |\n\n> a quote line\n\n";
        let mut text = String::new();
        while text.len() < 40_000 {
            text.push_str(unit);
        }
        let runs = 50;
        let start = std::time::Instant::now();
        for _ in 0..runs {
            std::hint::black_box(parse(std::hint::black_box(&text)));
        }
        println!(
            "parse of {} bytes: {:?}",
            text.len(),
            start.elapsed() / runs
        );
    }
}
