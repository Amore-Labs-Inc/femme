//! The rows of the list: one builder per kind of item. The user's message is a card on the right
//! with its attachments, the assistant's text is markdown on the paper, thinking and tool calls are
//! single lines that open on a click, a finished turn's work hides behind one thin line, and a
//! question that waits is a dialog the user answers in.

use std::time::{Duration, Instant};

use femme_proto::rpc::Answer;
use femme_proto::state::{ItemKind, ItemStatus};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, relative, AnyElement, Context, Div, InteractiveElement as _, IntoElement,
    MouseDownEvent, ParentElement as _, Rgba, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window,
};
use gpui_component::input::Input;

use super::attach;
use super::body::Parsed;
use super::live::Transcript;
use super::md_view;
use super::outbox::Sent;
use super::question::QuestionBody;
use super::select::{self, Join, Sel, Selectable, TAIL};
use super::thumb;
use super::turns::{self, Key, ToolLine};
use crate::motion::appear;
use crate::theme::{
    self, accent_deep, accent_text, bad, hover, ink, muted, neon, panel, paper, pause, work,
    BODY_SIZE, FONT_MONO, FONT_PIXEL, FONT_UI, META_SIZE,
};
use crate::ui::{card, dialog, meta, ruled, st, tb};

/// How opaque a prompt on its way is drawn: it reads as not there yet.
const SENT_OPACITY: f32 = 0.55;

/// Widest the text column gets: on a wide window the lines stay readable.
const COLUMN: f32 = 860.;

/// The most of a tool's input or output shown when its row is open.
const SHOWN_BYTES: usize = 24_000;

/// The most lines of a tool's input or output shown when its row is open.
const SHOWN_LINES: usize = 400;

/// An item younger than this when its row is first drawn fades in; older ones (history) do not.
const FRESH_MS: i64 = 1000;

/// How long after its first draw a fresh row is still sampled: its fade (180 ms) and a margin.
const FADING: Duration = Duration::from_millis(400);

/// `inner` in the text column, centred.
fn column(inner: impl IntoElement) -> AnyElement {
    div()
        .w_full()
        .flex()
        .justify_center()
        .px(px(16.))
        .py(px(3.))
        .child(div().w_full().max_w(px(COLUMN)).child(inner))
        .into_any_element()
}

/// The marker of a row that opens: closed `▸`, open `▾` (drawn from the monospace font, which has
/// both).
fn chevron(open: bool) -> Div {
    div()
        .flex_none()
        .w(px(12.))
        .font_family(FONT_MONO)
        .text_size(META_SIZE)
        .text_color(muted())
        .child(if open { "▾" } else { "▸" })
}

/// A muted monospace word, for the labels of folded rows.
fn small(text: impl IntoElement) -> Div {
    div()
        .flex_none()
        .font_family(FONT_MONO)
        .text_size(px(12.5))
        .text_color(muted())
        .child(text)
}

/// The first `SHOWN_LINES` lines or `SHOWN_BYTES` bytes of `text`, and a note on what was cut.
fn capped(text: &str) -> String {
    let mut end = text.len().min(SHOWN_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut kept = &text[..end];
    // The last line shown ends at the newline after it; whatever follows that newline is cut.
    if let Some((cut, _)) = kept.match_indices('\n').nth(SHOWN_LINES - 1) {
        if cut + 1 < kept.len() {
            kept = &kept[..cut];
        }
    }
    if kept.len() == text.len() {
        text.to_owned()
    } else {
        format!("{kept}\n… {} more bytes not shown", text.len() - kept.len())
    }
}

/// A labelled block of monospace text on the panel grey: a tool's input, its output.
fn block(label: &str, text: &str, color: Rgba, sel: &Sel) -> Div {
    ruled(
        div()
            .w_full()
            .flex()
            .flex_col()
            .bg(panel())
            .py(px(5.))
            .gap(px(2.)),
    )
    .child(div().px(px(8.)).child(meta(label.to_owned())))
    .child(
        div()
            .px(px(8.))
            .font_family(FONT_MONO)
            .text_size(px(12.5))
            .line_height(relative(1.4))
            .text_color(color)
            .child(Selectable::new(sel.slot(), capped(text))),
    )
}

/// The user's card: "you" in `label` over `text`.
fn you_card(text: String, label: Rgba, sel: &Sel) -> Div {
    let mut bubble = card().max_w(relative(0.85)).flex().flex_col().gap(px(6.));
    bubble.style().box_shadow = Some(theme::hard_shadow());
    bubble
        .child(
            div()
                .font_family(FONT_PIXEL)
                .text_size(px(18.))
                .line_height(relative(1.))
                .text_color(label)
                .child("you"),
        )
        .when(!text.is_empty(), |d| {
            d.child(
                div()
                    .text_size(BODY_SIZE)
                    .line_height(relative(1.4))
                    .child(Selectable::new(sel.slot(), text)),
            )
        })
}

/// A prompt on its way: the user's card, faded, its files named in a line under the text. It goes
/// when the daemon's `UserMessage` for it arrives.
// ponytail: names only, no thumbnails; the real bubble that replaces it has them.
fn sent_row(sent: &Sent, sel: &Sel) -> AnyElement {
    let bubble = you_card(sent.text.clone(), muted(), sel).when(!sent.files.is_empty(), |d| {
        d.child(meta(sent.files.join(", ")))
    });
    column(
        div()
            .flex()
            .justify_end()
            .opacity(SENT_OPACITY)
            .child(bubble),
    )
}

impl Transcript {
    /// The element of row `ix` of the list.
    pub(super) fn row(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(key) = self.layout.rows.get(ix).map(|r| r.key) else {
            // The row after the last item: the prompts on their way, then the working line under
            // what is streaming, centred.
            if ix != self.layout.rows.len() {
                return div().into_any_element();
            }
            let sel = Sel::new(ix, TAIL);
            return div()
                .w_full()
                .flex()
                .flex_col()
                .children(self.sent.iter().map(|sent| sent_row(sent, &sel)))
                .when_some(self.working.clone(), |d, line| {
                    d.child(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .py(px(8.))
                            .font_family(FONT_MONO)
                            .text_size(px(12.))
                            .text_color(muted())
                            .child(Selectable::new(sel.slot(), line)),
                    )
                })
                .into_any_element();
        };
        let (row, seq) = match key {
            Key::More => return self.more_row(cx),
            Key::Fold(seq) => return self.fold_row(seq, cx),
            Key::Tool(seq) => (self.tool_row(seq, &Sel::new(ix, seq), cx), seq),
            Key::Item(seq) => (self.item_row(seq, &Sel::new(ix, seq), window, cx), seq),
        };
        self.fade_in(seq, row, window, cx)
    }

    /// The row of an item made in the last second fades in and rises 4 px, once. History is drawn
    /// as it is, without keeping any state for it.
    fn fade_in(
        &mut self,
        seq: u64,
        row: AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let first = match self.fresh.get(&seq) {
            Some(at) => *at,
            None => {
                let Some(ts) = self.items.get(seq).map(|e| e.item.ts) else {
                    return row;
                };
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as i64);
                if now - ts >= FRESH_MS {
                    return row;
                }
                let at = Instant::now();
                self.fresh.insert(seq, at);
                at
            }
        };
        if first.elapsed() > crate::motion::scaled(FADING) {
            return row;
        }
        let Some(a) = appear(("row", seq), true, window, cx) else {
            return row;
        };
        div()
            .relative()
            .top(a.offset(4.))
            .opacity(a.opacity())
            .child(row)
            .into_any_element()
    }

    fn item_row(
        &mut self,
        seq: u64,
        sel: &Sel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(e) = self.items.get(seq) else {
            return div().into_any_element();
        };
        let kind = e.item.kind;
        // A long message is stored as a blob: fetch it as soon as its row is on screen. Tool
        // output waits until its row is opened.
        if !matches!(kind, ItemKind::ToolCall | ItemKind::ToolResult) {
            if let Some(hash) = self.items.wants_blob(e) {
                self.fetch_blob(hash, cx);
            }
        }
        match kind {
            ItemKind::UserMessage => self.user_row(seq, sel, cx),
            ItemKind::AssistantMessage => self.assistant_row(seq, sel, cx),
            ItemKind::Plan => self.plan_row(seq, sel),
            ItemKind::Thinking => self.thinking_row(seq, sel, cx),
            ItemKind::Question => self.question_row(seq, sel, window, cx),
            ItemKind::ToolResult => self.tool_row(seq, sel, cx),
            ItemKind::Event => self.note_row(seq, sel),
            ItemKind::TurnEnd => self.end_row(seq, sel),
            ItemKind::ToolCall | ItemKind::TurnStart => div().into_any_element(),
        }
    }

    /// The text of an item and whether it is still arriving.
    fn text_of(&self, seq: u64) -> (String, bool) {
        match self.items.get(seq) {
            Some(e) => (
                self.items.shown(e).text.into_owned(),
                e.item.status == ItemStatus::InProgress,
            ),
            None => (String::new(), false),
        }
    }

    fn user_row(&mut self, seq: u64, sel: &Sel, cx: &mut Context<Self>) -> AnyElement {
        let (text, _) = self.text_of(seq);
        let (text, files) = attach::split(&text);
        for file in files.iter().filter(|f| f.image) {
            self.want_thumb(&file.path, cx);
        }
        let bubble =
            you_card(text, accent_text(), sel).when(!files.is_empty(), |d| {
                d.child(div().flex().flex_wrap().gap(px(8.)).children(
                    files.iter().enumerate().map(|(i, file)| {
                        let thumb = self.thumbs.get(&file.path);
                        let path = file.path.clone();
                        let menu = cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            this.thumb_menu(&path, ev.position, window, cx)
                        });
                        thumb::attachment(format!("attached-{seq}-{i}"), file, thumb, menu)
                    }),
                ))
            });
        column(div().flex().justify_end().child(bubble))
    }

    fn assistant_row(&mut self, seq: u64, sel: &Sel, cx: &mut Context<Self>) -> AnyElement {
        let (text, live) = self.text_of(seq);
        // `Attached:` lines are what `femmed show` put there: the files it showed, as pictures.
        let (text, files) = attach::split(&text);
        for file in files.iter().filter(|f| f.image) {
            self.want_thumb(&file.path, cx);
        }
        let doc = self.doc(seq, &text);
        column(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .text_size(BODY_SIZE)
                .when(!doc.blocks.is_empty(), |d| {
                    d.child(md_view::doc(&doc, ink(), sel))
                })
                .when(!files.is_empty(), |d| {
                    d.child(div().flex().flex_wrap().gap(px(8.)).children(
                        files.iter().enumerate().map(|(i, file)| {
                            let thumb = self.thumbs.get(&file.path);
                            let path = file.path.clone();
                            let menu = cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                this.thumb_menu(&path, ev.position, window, cx)
                            });
                            thumb::shown(format!("shown-{seq}-{i}"), file, thumb, menu)
                        }),
                    ))
                })
                .when(live, |d| d.child(streaming())),
        )
    }

    fn plan_row(&mut self, seq: u64, sel: &Sel) -> AnyElement {
        let (text, live) = self.text_of(seq);
        let doc = self.doc(seq, &text);
        column(
            ruled(div().flex().flex_col().bg(paper()))
                .child(tb("plan"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .p(px(12.))
                        .text_size(BODY_SIZE)
                        .child(md_view::doc(&doc, ink(), sel))
                        .when(live, |d| d.child(streaming())),
                ),
        )
    }

    fn thinking_row(&mut self, seq: u64, sel: &Sel, cx: &mut Context<Self>) -> AnyElement {
        let (text, live) = self.text_of(seq);
        let open = self.open.contains(&seq);
        let first = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_owned();
        let head = div()
            .id(("think", seq))
            .flex()
            .items_center()
            .gap(px(6.))
            .py(px(2.))
            .cursor_pointer()
            .hover(|s| s.bg(panel()))
            .on_click(cx.listener(move |this, click, _, cx| {
                if select::plain_click(click) {
                    this.toggle(seq, cx);
                }
            }))
            .child(chevron(open))
            .child(small("thinking").italic())
            .when(!open, |d| {
                d.child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_size(px(13.))
                        .italic()
                        .text_color(muted())
                        .child(Selectable::new(sel.slot(), first).join(Join::Line)),
                )
            })
            .when(live, |d| d.child(streaming()));
        column(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(head)
                .when(open, |d| {
                    d.child(
                        div()
                            .pl(px(18.))
                            .text_size(px(13.))
                            .line_height(relative(1.45))
                            .italic()
                            .text_color(muted())
                            .child(Selectable::new(sel.slot(), text)),
                    )
                }),
        )
    }

    /// A status line the agent or the daemon put among the messages.
    fn note_row(&self, seq: u64, sel: &Sel) -> AnyElement {
        let (text, _) = self.text_of(seq);
        let line = text.lines().next().unwrap_or("").to_owned();
        column(
            div()
                .font_family(FONT_MONO)
                .text_size(px(12.))
                .text_color(muted())
                .child(Selectable::new(sel.slot(), format!("· {line}"))),
        )
    }

    /// A turn that did not end the usual way says how.
    fn end_row(&self, seq: u64, sel: &Sel) -> AnyElement {
        let Some(e) = self.items.get(seq) else {
            return div().into_any_element();
        };
        let failed = matches!(
            self.items.parsed(e),
            Parsed::TurnEnd(t) if matches!(t.stop, super::body::Stop::Error(_))
        );
        column(
            div()
                .font_family(FONT_MONO)
                .text_size(px(12.))
                .text_color(if failed { bad() } else { muted() })
                .child(Selectable::new(
                    sel.slot(),
                    format!("· {}", turns::end_text(&self.items, e)),
                )),
        )
    }

    /// The header above the oldest item held: older ones can be fetched.
    fn more_row(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // It is on screen, so the user is looking for what is above it.
        if !self.paging && !self.page_failed {
            self.load_older(cx);
        }
        let word = if self.paging {
            "loading older messages…"
        } else {
            "load older messages"
        };
        column(
            div()
                .id("older")
                .flex()
                .justify_center()
                .py(px(6.))
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.page_failed = false;
                    this.load_older(cx);
                }))
                .child(meta(word)),
        )
    }

    /// The thin line a finished turn's work hides behind.
    fn fold_row(&self, seq: u64, cx: &mut Context<Self>) -> AnyElement {
        let Some(info) = self.layout.folds.get(&seq) else {
            return div().into_any_element();
        };
        let (label, failed) = (info.label(), info.failed > 0);
        let open = self.open.contains(&seq);
        column(
            div()
                .id(("fold", seq))
                .flex()
                .items_center()
                .gap(px(8.))
                .py(px(3.))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(seq, cx)))
                .child(chevron(open))
                .child(small(label).when(failed, |d| d.text_color(bad())))
                .child(div().flex_1().h(px(1.)).bg(Rgba { a: 0.16, ..ink() })),
        )
    }

    /// One tool call with its answer (or a result whose call is not held).
    fn tool_row(&mut self, seq: u64, sel: &Sel, cx: &mut Context<Self>) -> AnyElement {
        let open = self.open.contains(&seq);
        let Some(view) = self.tool_view(seq, open) else {
            return div().into_any_element();
        };
        if open {
            for hash in &view.wants {
                self.fetch_blob(*hash, cx);
            }
        }
        let line = &view.line;
        let name_color = if line.failed { bad() } else { ink() };
        // An Agent call whose subagent has items: the row opens its thread.
        let agent = self
            .items
            .get(seq)
            .map(|e| e.item.item_id.clone())
            .filter(|id| {
                matches!(line.name.as_str(), "Agent" | "Task")
                    && self.items.view(Some(id)).next().is_some()
            });
        let head = div()
            .id(("tool", seq))
            .flex()
            .items_center()
            .gap(px(6.))
            .py(px(2.))
            .cursor_pointer()
            .hover(|s| s.bg(panel()))
            .on_click(cx.listener(move |this, click, _, cx| {
                if select::plain_click(click) {
                    this.toggle(seq, cx);
                }
            }))
            .child(chevron(open))
            .child(
                small(Selectable::new(sel.slot(), line.name.clone()).join(Join::Line))
                    .text_color(name_color)
                    .font_weight(gpui::FontWeight::BOLD),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .font_family(FONT_MONO)
                    .text_size(px(12.5))
                    .text_color(muted())
                    .child(Selectable::new(sel.slot(), line.hint.clone()).join(Join::Line)),
            )
            .when_some(line.result.clone(), |d, result| {
                d.child(
                    div()
                        .flex_none()
                        .max_w(relative(0.4))
                        .truncate()
                        .font_family(FONT_MONO)
                        .text_size(px(12.5))
                        .text_color(if line.failed { bad() } else { muted() })
                        .child(Selectable::new(sel.slot(), format!("→ {result}")).join(Join::Line)),
                )
            })
            .when_some(agent, |d, id| {
                d.child(
                    div()
                        .id(("agent-open", seq))
                        .flex_none()
                        .px(px(4.))
                        .font_family(FONT_MONO)
                        .text_size(px(12.))
                        .text_color(accent_text())
                        .hover(|s| s.text_color(neon()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.set_view(Some(id.clone()), cx);
                        }))
                        .child("open ›"),
                )
            })
            .when(line.running, |d| {
                d.child(div().flex_none().child(st("running", pause())))
            });
        let body = open.then(|| {
            div()
                .pl(px(18.))
                .flex()
                .flex_col()
                .gap(px(6.))
                .when_some(view.input.clone(), |d, input| {
                    d.child(block("input", &input, ink(), sel))
                })
                .when_some(view.output.clone(), |d, output| {
                    d.child(block(
                        if line.failed { "error" } else { "output" },
                        &output,
                        if line.failed { bad() } else { ink() },
                        sel,
                    ))
                })
                .when(view.loading, |d| d.child(meta("loading…")))
        });
        column(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(head)
                .children(body),
        )
    }

    /// What a tool row shows, copied out of the items so the row can be built while `self` is
    /// changed (blobs asked for).
    fn tool_view(&self, seq: u64, open: bool) -> Option<ToolView> {
        let e = self.items.get(seq)?;
        let mut wants = Vec::new();
        let mut want = |e: &super::items::Entry| wants.extend(self.items.wants_blob(e));
        if e.item.kind == ItemKind::ToolResult {
            // A result whose call is older than what is held.
            want(e);
            let (output, failed) = match self.items.parsed(e) {
                Parsed::ToolResult(t) => (Some(t.output.clone()), t.is_error),
                _ => (None, false),
            };
            let line = ToolLine {
                name: "result".into(),
                hint: String::new(),
                result: Some(
                    output
                        .as_deref()
                        .unwrap_or(&e.item.preview)
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_owned(),
                ),
                failed: failed || e.item.status == ItemStatus::Failed,
                running: false,
            };
            return Some(ToolView {
                line,
                input: None,
                output,
                loading: open && self.fetching_any(&wants),
                wants,
            });
        }
        let line = turns::tool_line(&self.items, e);
        let result = turns::result_of(&self.items, e);
        want(e);
        if let Some(r) = result {
            want(r);
        }
        let input = match self.items.parsed(e) {
            Parsed::ToolCall(t) => t.input_text(),
            _ => None,
        };
        let output = result.and_then(|r| match self.items.parsed(r) {
            Parsed::ToolResult(t) => Some(t.output.clone()),
            _ => None,
        });
        Some(ToolView {
            line,
            input,
            output,
            loading: open && self.fetching_any(&wants),
            wants,
        })
    }

    fn question_row(
        &mut self,
        seq: u64,
        sel: &Sel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(e) = self.items.get(seq) else {
            return div().into_any_element();
        };
        let pending = e.item.status == ItemStatus::Pending;
        let preview = e.item.preview.clone();
        let question = match self.items.parsed(e) {
            Parsed::Question(q) => Some(q.clone()),
            _ => None,
        };
        let Some(q) = question else {
            // No readable question: say what the daemon said.
            return column(
                div()
                    .text_color(muted())
                    .child(Selectable::new(sel.slot(), preview)),
            );
        };
        if pending && q.open() {
            self.ask_for(seq, &q.q, window, cx);
            return self.open_question(seq, &q, sel, cx);
        }
        answered_question(&q, sel)
    }

    /// The dialog of a question that waits: the prompt, the options, a field when the question
    /// takes free text, and "cancel".
    fn open_question(
        &self,
        seq: u64,
        q: &QuestionBody,
        sel: &Sel,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ask = self.asks.get(&seq);
        let sending = ask.is_some_and(|a| a.sending);
        let input = ask.and_then(|a| a.input.clone());
        let ticked = |i: usize| ask.and_then(|a| a.picks.get(i)).copied().unwrap_or(false);
        let any_ticked = ask.is_some_and(|a| a.picks.iter().any(|on| *on));
        let multi = q.q.multi;

        let options =
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .children(q.q.options.iter().enumerate().map(|(i, label)| {
                    let on = multi && ticked(i);
                    option(
                        format!("opt-{seq}-{i}"),
                        &format!("{}", i + 1),
                        label,
                        on,
                        sending,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if multi {
                            this.pick(seq, i, cx);
                        } else {
                            this.answer(
                                seq,
                                Answer::Option(u16::try_from(i).unwrap_or(u16::MAX)),
                                cx,
                            );
                        }
                    }))
                }));

        let send_text = input.is_some();
        let controls = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .when_some(input, |d, input| {
                d.child(div().flex_1().min_w(px(0.)).child(Input::new(&input)))
            })
            .when(multi || send_text, |d| {
                d.child(
                    action(
                        format!("send-{seq}"),
                        "send",
                        !sending && (any_ticked || send_text),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !(multi && this.submit_picks(seq, cx)) {
                            this.submit_text(seq, cx);
                        }
                    })),
                )
            })
            .child(
                action(format!("cancel-{seq}"), "cancel", !sending).on_click(
                    cx.listener(move |this, _, _, cx| this.answer(seq, Answer::Cancel, cx)),
                ),
            )
            .when(sending, |d| d.child(meta("sending…")));

        column(
            dialog().flex().flex_col().child(tb("question")).child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .p(px(12.))
                    .child(
                        div()
                            .text_size(BODY_SIZE)
                            .line_height(relative(1.4))
                            .child(Selectable::new(sel.slot(), q.q.prompt.clone())),
                    )
                    .when(!q.q.options.is_empty(), |d| d.child(options))
                    .child(controls),
            ),
        )
    }
}

/// What a tool row shows.
struct ToolView {
    line: ToolLine,
    input: Option<String>,
    output: Option<String>,
    /// The input or output is a blob that has not arrived.
    loading: bool,
    /// The blobs to fetch for it.
    wants: Vec<[u8; 32]>,
}

/// A question that was answered, or ended unanswered: the prompt and what became of it.
fn answered_question(q: &QuestionBody, sel: &Sel) -> AnyElement {
    let fate = match (q.answer_text(), &q.reason) {
        (Some(answer), _) => format!("→ {answer}"),
        (None, Some(reason)) => format!("not answered: {reason}"),
        (None, None) => "not answered".to_owned(),
    };
    column(
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div().flex().gap(px(8.)).child(small("question")).child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_size(px(13.))
                        .text_color(muted())
                        .child(Selectable::new(sel.slot(), q.q.prompt.clone()).join(Join::Line)),
                ),
            )
            .child(
                div()
                    .pl(px(12.))
                    .text_size(BODY_SIZE)
                    .font_family(FONT_UI)
                    .child(Selectable::new(sel.slot(), fate)),
            ),
    )
}

/// The tag on an item that is still arriving.
fn streaming() -> Div {
    div().flex().child(st("streaming", work()))
}

/// One option of a question: its number and its words, in a box that turns pink under the pointer
/// (dark when ticked).
fn option(id: String, n: &str, label: &str, on: bool, disabled: bool) -> gpui::Stateful<Div> {
    let (bg, fg) = if on {
        (ink(), paper())
    } else {
        (paper(), ink())
    };
    let mut b = ruled(
        div()
            .id(SharedString::from(id))
            .flex()
            .gap(px(8.))
            .px(px(10.))
            .py(px(6.))
            .bg(bg)
            .text_color(fg),
    );
    if !disabled {
        // The selected option has a paper label, so its hover is the deep pink that carries one.
        b = b
            .cursor_pointer()
            .hover(move |s| s.bg(if on { accent_deep() } else { hover() }));
    }
    b.child(
        div()
            .flex_none()
            .font_family(FONT_PIXEL)
            .text_size(px(20.))
            .line_height(relative(1.))
            .child(SharedString::from(format!("{n}."))),
    )
    .child(
        div()
            .flex_1()
            .min_w(px(0.))
            .text_size(BODY_SIZE)
            .line_height(relative(1.3))
            .child(SharedString::from(label.to_owned())),
    )
}

/// A small action button of the question dialog ("send", "cancel").
fn action(id: String, label: &'static str, enabled: bool) -> gpui::Stateful<Div> {
    let b = ruled(
        div()
            .id(SharedString::from(id))
            .flex()
            .flex_none()
            .items_center()
            .px(px(10.))
            .h(px(30.))
            .font_family(FONT_PIXEL)
            .text_size(px(20.))
            .line_height(relative(1.))
            .bg(paper())
            .text_color(if enabled { ink() } else { muted() }),
    );
    let b = if enabled {
        b.cursor_pointer().hover(|s| s.bg(hover()))
    } else {
        b
    };
    b.child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_text_is_shown_whole() {
        assert_eq!(capped("a\nb\n"), "a\nb\n");
        assert_eq!(capped(""), "");
    }

    #[test]
    fn the_line_cap_keeps_that_many_lines_and_says_what_was_left() {
        let exact = "x\n".repeat(SHOWN_LINES);
        assert_eq!(
            capped(&exact),
            exact,
            "a text of exactly the cap is not cut"
        );

        let more = "x\n".repeat(SHOWN_LINES + 3);
        let shown = capped(&more);
        assert_eq!(
            shown.lines().count(),
            SHOWN_LINES + 1,
            "the lines and the note"
        );
        // The last line shown loses its newline: 400 lines are 799 bytes.
        let left = more.len() - (2 * SHOWN_LINES - 1);
        assert!(
            shown.ends_with(&format!("… {left} more bytes not shown")),
            "{shown:?}"
        );
    }

    #[test]
    fn the_byte_cap_never_splits_a_character() {
        // Two bytes per character after one byte: the cap falls inside the character at 23 999.
        let text = format!("a{}", "é".repeat(SHOWN_BYTES));
        let shown = capped(&text);
        let (kept, note) = shown.split_once('\n').expect("a note line");
        assert_eq!(kept.len(), SHOWN_BYTES - 1);
        assert_eq!(
            note,
            format!("… {} more bytes not shown", text.len() - kept.len())
        );
    }
}
