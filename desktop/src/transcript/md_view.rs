//! A parsed markdown document as elements: the paragraphs and headings of the assistant's text,
//! its code blocks (monospace on the panel grey, with the language above them), lists, quotes,
//! tables. Links show as text, underlined, followed by their address; a click on the words of a
//! link, or on a bare address, opens it in the browser. Every text is a
//! [`Selectable`](super::select::Selectable): the mouse selects it and ctrl-c copies it.

use gpui::{
    div, px, relative, AnyElement, FontStyle, FontWeight, HighlightStyle, Hsla, IntoElement,
    ParentElement as _, Rgba, SharedString, StrikethroughStyle, Styled as _, UnderlineStyle,
};

use super::markdown::{Align, Block, Doc, Inline, ListItem, Style};
use super::select::{Join, Sel, Selectable, Slot};
use crate::theme::{accent_text, ink, muted, panel, FONT_MONO, RULE};
use crate::ui::{meta, ruled, ruled_sides};

/// Size of the code in a block and in a span.
const CODE_SIZE: f32 = 13.;

/// The elements of `doc`, one below the other, in `color`. Its texts take their places in the
/// reading order from `sel`, the slots of the row the document is in.
pub fn doc(doc: &Doc, color: Rgba, sel: &Sel) -> AnyElement {
    stack(&doc.blocks, color, 8., sel, Join::Block).into_any_element()
}

/// `blocks` one below the other; each of their texts is followed by `join` when copied.
fn stack(blocks: &[Block], color: Rgba, gap: f32, sel: &Sel, join: Join) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(gap))
        .text_color(color)
        .children(blocks.iter().map(|b| block(b, color, sel, join)))
}

fn block(b: &Block, color: Rgba, sel: &Sel, join: Join) -> AnyElement {
    match b {
        Block::Paragraph(text) => div()
            .line_height(relative(1.45))
            .child(inline(text, sel.slot()).join(join))
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
                .child(inline(text, sel.slot()).join(join))
                .into_any_element()
        }
        Block::Code { lang, code } => code_block(lang.as_deref(), code, sel.slot(), join),
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
                // The items copy one per line; the last one is followed by what follows the list.
                let item_join = if i + 1 == items.len() {
                    join
                } else {
                    Join::Line
                };
                list_item(&marker, item, color, sel, item_join)
            }))
            .into_any_element(),
        Block::Quote(blocks) => ruled_sides(div(), px(0.), px(0.), px(0.), px(3.))
            .border_color(muted())
            .pl(px(10.))
            .child(stack(blocks, muted(), 6., sel, Join::Block))
            .into_any_element(),
        Block::Rule => div()
            .w_full()
            .h(px(1.))
            .my(px(4.))
            .bg(muted())
            .into_any_element(),
        Block::Table { aligns, head, rows } => table(aligns, head, rows, sel, join),
    }
}

fn list_item(marker: &str, item: &ListItem, color: Rgba, sel: &Sel, join: Join) -> AnyElement {
    let mut mark = div()
        .flex_none()
        .min_w(px(22.))
        .flex()
        .justify_end()
        .text_color(muted())
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
                .child(stack(&item.blocks, color, 4., sel, join)),
        )
        .into_any_element()
}

fn code_block(lang: Option<&str>, code: &str, slot: Slot, join: Join) -> AnyElement {
    let head = lang
        .filter(|l| !l.is_empty())
        .map(|l| div().px(px(10.)).pt(px(4.)).child(meta(l.to_owned())));
    let block = ruled(div().w_full().flex().flex_col().bg(panel()).pb(px(8.)));
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
                .text_color(ink())
                .child(Selectable::new(slot, code.to_owned()).join(join)),
        )
        .into_any_element()
}

/// A table: its cells copy one per line, in rows from the top; the last cell is followed by `join`.
fn table(
    aligns: &[Align],
    head: &[Inline],
    rows: &[Vec<Inline>],
    sel: &Sel,
    join: Join,
) -> AnyElement {
    let cell = |text: &Inline, col: usize, bold: bool, join: Join| {
        let mut c = div().flex_1().min_w(px(0.)).px(px(8.)).py(px(3.));
        c = match aligns.get(col).copied().unwrap_or_default() {
            Align::Left => c.text_left(),
            Align::Center => c.text_center(),
            Align::Right => c.text_right(),
        };
        if bold {
            c = c.font_weight(FontWeight::BOLD);
        }
        c.child(inline(text, sel.slot()).join(join))
    };
    let mut table = ruled(div().w_full().flex().flex_col());
    if !head.is_empty() {
        table = table.child(
            ruled_sides(div().flex().bg(panel()), px(0.), px(0.), RULE, px(0.)).children(
                head.iter().enumerate().map(|(i, t)| {
                    let last = rows.is_empty() && i + 1 == head.len();
                    cell(t, i, true, if last { join } else { Join::Line })
                }),
            ),
        );
    }
    table
        .children(rows.iter().enumerate().map(|(r, row)| {
            div().flex().children(row.iter().enumerate().map(|(i, t)| {
                let last = r + 1 == rows.len() && i + 1 == row.len();
                cell(t, i, false, if last { join } else { Join::Line })
            }))
        }))
        .into_any_element()
}

/// The text of `text` with its styled stretches, selectable, in `slot`; a click on one of its links
/// opens the address.
pub fn inline(text: &Inline, slot: Slot) -> Selectable {
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
    Selectable::new(slot, text.text.clone())
        .highlights(highlights)
        .font_families(mono)
        .links(text.links.clone())
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
        h.background_color = Some(Hsla::from(panel()));
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
            color: Some(Hsla::from(accent_text())),
            wavy: false,
        });
    }
    if s.dim {
        h.color = Some(Hsla::from(muted()));
    }
    h
}
