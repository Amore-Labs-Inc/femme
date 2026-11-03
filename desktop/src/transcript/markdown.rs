//! Markdown to a block model the list can lay out: paragraphs, headings, code blocks, lists,
//! quotes, tables, with the inline styles of a paragraph as byte ranges over its text. The parser
//! is pulldown-cmark; this module only folds its events into blocks (no HTML, no gpui), so it is
//! tested on plain strings. Text that is still streaming parses as it stands: an unclosed fence
//! is a code block to the end, an unclosed `**` is literal text.

use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

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
            Event::Text(t) => self.text(&t),
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
                    // The words carry the link style already (it was on while they were read).
                    // Show where the link goes unless the words are the address.
                    let words = &self.cur.text[from..];
                    if !url.is_empty() && words != url {
                        let style = Style {
                            dim: true,
                            ..self.style()
                        };
                        self.cur.push(&format!(" ({url})"), style);
                    }
                }
            }
            TagEnd::Image => self.text("]"),
            _ => {}
        }
    }
}

/// Parses `src` into blocks.
pub fn parse(src: &str) -> Doc {
    let mut b = Builder::default();
    b.containers.push(Vec::new());
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    for ev in Parser::new_ext(src, opts) {
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
            spans: vec![],
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
}
