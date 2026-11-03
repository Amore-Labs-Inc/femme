//! A parsed markdown document as elements: the paragraphs and headings of the assistant's text,
//! its code blocks (monospace on the panel grey, with the language above them), lists, quotes,
//! tables. Links show as text, underlined, followed by their address.

use gpui::{
    div, px, relative, AnyElement, FontStyle, FontWeight, HighlightStyle, Hsla, IntoElement,
    ParentElement as _, Rgba, SharedString, StrikethroughStyle, Styled as _, StyledText,
    UnderlineStyle,
};

use super::markdown::{Align, Block, Doc, Inline, ListItem, Style};
use crate::theme::{FONT_MONO, HOVER, INK, MUTED, PANEL, RULE};
use crate::ui::{meta, ruled, ruled_sides};

/// Size of the code in a block and in a span.
const CODE_SIZE: f32 = 13.;

/// The elements of `doc`, one below the other, in `color`.
pub fn doc(doc: &Doc, color: Rgba) -> AnyElement {
    stack(&doc.blocks, color, 8.).into_any_element()
}

fn stack(blocks: &[Block], color: Rgba, gap: f32) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(gap))
        .text_color(color)
        .children(blocks.iter().map(|b| block(b, color)))
}

fn block(b: &Block, color: Rgba) -> AnyElement {
    match b {
        Block::Paragraph(text) => div()
            .line_height(relative(1.45))
            .child(inline(text))
            .into_any_element(),
        Block::Heading { level, text } => {
            let size = match level {
                1 => 22.,
                2 => 19.,
                3 => 17.,
                _ => 15.,
            };
            div()
                .pt(px(4.))
                .text_size(px(size))
                .line_height(relative(1.25))
                .font_weight(FontWeight::BOLD)
                .child(inline(text))
                .into_any_element()
        }
        Block::Code { lang, code } => code_block(lang.as_deref(), code),
        Block::List {
            ordered,
            start,
            items,
        } => div()
            .flex()
            .flex_col()
            .gap(px(3.))
            .children(items.iter().enumerate().map(|(i, item)| {
                let marker = if *ordered {
                    format!("{}.", start + i as u64)
                } else {
                    "•".to_owned()
                };
                list_item(&marker, item, color)
            }))
            .into_any_element(),
        Block::Quote(blocks) => ruled_sides(div(), px(0.), px(0.), px(0.), px(3.))
            .border_color(MUTED)
            .pl(px(10.))
            .child(stack(blocks, MUTED, 6.))
            .into_any_element(),
        Block::Rule => div()
            .w_full()
            .h(px(1.))
            .my(px(4.))
            .bg(MUTED)
            .into_any_element(),
        Block::Table { aligns, head, rows } => table(aligns, head, rows),
    }
}

fn list_item(marker: &str, item: &ListItem, color: Rgba) -> AnyElement {
    let mut mark = div()
        .flex_none()
        .min_w(px(22.))
        .flex()
        .justify_end()
        .text_color(MUTED)
        .line_height(relative(1.45));
    let marker = match item.task {
        Some(done) => {
            // A tick box is monospace, so its brackets line up.
            mark = mark.font_family(FONT_MONO).text_size(px(CODE_SIZE));
            if done {
                "[x]"
            } else {
                "[ ]"
            }
        }
        None => marker,
    };
    div()
        .flex()
        .gap(px(8.))
        .child(mark.child(SharedString::from(marker.to_owned())))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .child(stack(&item.blocks, color, 4.)),
        )
        .into_any_element()
}

fn code_block(lang: Option<&str>, code: &str) -> AnyElement {
    let head = lang
        .filter(|l| !l.is_empty())
        .map(|l| div().px(px(10.)).pt(px(4.)).child(meta(l.to_owned())));
    let block = ruled(div().w_full().flex().flex_col().bg(PANEL).pb(px(8.)));
    let block = match head {
        Some(head) => block.child(head),
        None => block.pt(px(8.)),
    };
    block
        .child(
            div()
                .px(px(10.))
                .font_family(FONT_MONO)
                .text_size(px(CODE_SIZE))
                .line_height(relative(1.4))
                .text_color(INK)
                .child(SharedString::from(code.to_owned())),
        )
        .into_any_element()
}

fn table(aligns: &[Align], head: &[Inline], rows: &[Vec<Inline>]) -> AnyElement {
    let cell = |text: &Inline, col: usize, bold: bool| {
        let mut c = div().flex_1().min_w(px(0.)).px(px(8.)).py(px(3.));
        c = match aligns.get(col).copied().unwrap_or_default() {
            Align::Left => c.text_left(),
            Align::Center => c.text_center(),
            Align::Right => c.text_right(),
        };
        if bold {
            c = c.font_weight(FontWeight::BOLD);
        }
        c.child(inline(text))
    };
    let mut table = ruled(div().w_full().flex().flex_col());
    if !head.is_empty() {
        table = table.child(
            ruled_sides(div().flex().bg(PANEL), px(0.), px(0.), RULE, px(0.))
                .children(head.iter().enumerate().map(|(i, t)| cell(t, i, true))),
        );
    }
    table
        .children(rows.iter().map(|row| {
            div()
                .flex()
                .children(row.iter().enumerate().map(|(i, t)| cell(t, i, false)))
        }))
        .into_any_element()
}

/// The text of `text` with its styled stretches.
pub fn inline(text: &Inline) -> StyledText {
    let highlights = text
        .spans
        .iter()
        .map(|s| (s.range.clone(), highlight(s.style)))
        .collect::<Vec<_>>();
    let mono = text
        .spans
        .iter()
        .filter(|s| s.style.code)
        .map(|s| (s.range.clone(), SharedString::from(FONT_MONO)))
        .collect::<Vec<_>>();
    StyledText::new(SharedString::from(text.text.clone()))
        .with_highlights(highlights)
        .with_font_family_overrides(mono)
}

fn highlight(s: Style) -> HighlightStyle {
    let mut h = HighlightStyle::default();
    if s.bold {
        h.font_weight = Some(FontWeight::BOLD);
    }
    if s.italic {
        h.font_style = Some(FontStyle::Italic);
    }
    if s.code {
        h.background_color = Some(Hsla::from(PANEL));
    }
    if s.strike {
        h.strikethrough = Some(StrikethroughStyle {
            thickness: px(1.),
            color: None,
        });
    }
    if s.link {
        h.underline = Some(UnderlineStyle {
            thickness: px(1.),
            color: Some(Hsla::from(HOVER)),
            wavy: false,
        });
    }
    if s.dim {
        h.color = Some(Hsla::from(MUTED));
    }
    h
}
