//! The card's elements: the title bar, the notice and deploy lines, the list on the left and the
//! selected pull request on the right with its buttons, its confirm row and its description, and the
//! footer's auto-fix toggle.
//! Everything here only reads the overlay; the buttons call back into it.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, relative, AnyElement, Context, Div, FontWeight, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, ParentElement as _, Pixels, SharedString, Size, Stateful,
    StatefulInteractiveElement as _, Styled as _,
};

use femme_proto::rpc::Pr;

use super::{actions, badge, merge_waits, Badge, Confirm, FixButton, Load, PrsOverlay};
use crate::motion::Appear;
use crate::theme::{
    self, accent_text, bad, bar, hover, ink, muted, on_bar, paper, work, BODY_SIZE, FONT_PIXEL,
    FONT_UI, RULE, TB_SIZE,
};
use crate::transcript::md_view;
use crate::transcript::select::Sel;
use crate::ui::{button, button_dark, card, dialog, meta, row, ruled, ruled_sides, st};

/// The list's width.
const LIST_W: f32 = 340.;

/// `.empty`: a muted, centred line where a list would be (red when it is an error).
fn empty(text: impl Into<SharedString>, error: bool) -> Div {
    div()
        .py(px(14.))
        .px(px(8.))
        .text_center()
        .text_color(if error { bad() } else { muted() })
        .child(text.into())
}

/// A button of the title bar: paper outline on the black, as the board's `.tb button`.
fn title_button(id: &'static str, label: &'static str) -> Stateful<Div> {
    ruled(
        div()
            .id(id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(px(22.))
            .px(px(6.))
            .text_size(px(18.))
            .line_height(relative(1.))
            .cursor_pointer()
            .hover(|s| s.bg(hover()))
            .child(label),
    )
    .border_color(on_bar())
}

/// A strip under the title bar, pinned whatever the panes are scrolled to.
fn strip(text: String, good: bool) -> Div {
    ruled_sides(
        div()
            .flex_none()
            .px(px(8.))
            .py(px(2.))
            .bg(paper())
            .text_color(if good { work() } else { bad() })
            .child(text),
        px(0.),
        px(0.),
        RULE,
        px(0.),
    )
}

/// `+12 −3`, green and red.
fn diff(pr: &Pr) -> Div {
    div()
        .flex()
        .flex_none()
        .gap(px(4.))
        .child(meta(format!("+{}", pr.additions)).text_color(work()))
        .child(meta(format!("−{}", pr.deletions)).text_color(bad()))
}

/// The row's mark: "merging…", "fixing…" and "conflicts" as status pills, "checking" faint.
fn badge_el(b: Badge) -> Div {
    match b {
        Badge::Merging => st("merging…", theme::accent()).flex_none(),
        Badge::Fixing => st("fixing…", theme::warn()).flex_none(),
        Badge::Conflicts => st("conflicts", theme::danger()).flex_none(),
        Badge::Checking => meta("checking").flex_none().text_color(theme::muted_text()),
    }
}

impl PrsOverlay {
    /// The whole layer: the dimmed window and the card in its middle, kept under the window's
    /// height `viewport`.
    pub(super) fn layer(
        &mut self,
        viewport: Size<Pixels>,
        a: Appear,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::scrim())
            .opacity(a.opacity())
            .occlude()
            .key_context("PullRequests")
            .track_focus(&self.focus)
            .font_family(FONT_UI)
            .text_size(BODY_SIZE)
            .line_height(relative(1.4))
            .text_color(ink())
            .on_key_down(cx.listener(|t, ev: &KeyDownEvent, window, cx| {
                match ev.keystroke.key.as_str() {
                    "escape" if t.confirm.is_some() => t.cancel(cx),
                    "escape" => t.dismiss(window, cx),
                    "down" => t.step(true, cx),
                    "up" => t.step(false, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|t, _, window, cx| t.dismiss(window, cx)),
            )
            // Centred, so a top margin moves the card by half of it: 12 px of margin is a 6 px rise.
            .child(self.card_el(viewport, cx).mt(a.offset(12.)))
            .into_any_element()
    }

    fn card_el(&mut self, viewport: Size<Pixels>, cx: &mut Context<Self>) -> Div {
        let notice = self.notice.as_ref().map(|n| strip(n.text.clone(), !n.bad));
        let deploy = self.deploy.as_ref().map(|d| {
            let (text, good) = d.line();
            strip(text, good)
        });
        dialog()
            .w(px(1100.))
            .max_w(relative(0.95))
            .h(viewport.height * 0.86)
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(self.title_bar(cx))
            .children(deploy)
            .children(notice)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap(px(3.))
                    .p(px(3.))
                    .child(self.list_el(cx))
                    .child(self.detail(cx)),
            )
            .children(self.footer(cx))
    }

    /// `[■] Auto-fix conflicts after a merge`, when the daemon knows the setting.
    fn footer(&self, cx: &mut Context<Self>) -> Option<Div> {
        let on = self.auto_fix?;
        let pending = self.auto.is_some();
        let check = ruled(div().flex_none().size(px(12.)).when(on, |d| d.bg(ink())));
        Some(ruled_sides(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(10.))
                .px(px(8.))
                .py(px(4.))
                .bg(paper())
                .child(
                    div()
                        .id("prs-autofix")
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(6.))
                        .cursor_pointer()
                        .when(pending, |d| d.opacity(0.5))
                        .child(check)
                        .child("Auto-fix conflicts after a merge")
                        .on_click(cx.listener(|t, _, _, cx| t.toggle_auto_fix(cx))),
                )
                .child(
                    meta(
                        "starts a fix chat for each claude-update PR a merge leaves conflicting \
                         (uses Claude usage)",
                    )
                    .flex_1()
                    .min_w_0(),
                ),
            RULE,
            px(0.),
            px(0.),
            px(0.),
        ))
    }

    /// `‹  pull requests  ↻ refresh`.
    fn title_bar(&self, cx: &mut Context<Self>) -> Div {
        let count = match &self.list {
            Load::Ready(prs) => format!("pull requests · {}", prs.len()),
            _ => "pull requests".to_owned(),
        };
        let reloading = self.reload.is_some();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .py(px(3.))
            .bg(bar())
            .text_color(on_bar())
            .font_family(FONT_PIXEL)
            .text_size(TB_SIZE)
            .line_height(relative(1.))
            .child(
                title_button("prs-close", "‹")
                    .on_click(cx.listener(|t, _, window, cx| t.dismiss(window, cx))),
            )
            .child(div().flex_1().child(count))
            .child(
                title_button("prs-refresh", "refresh")
                    .when(reloading, |b| b.opacity(0.5))
                    .on_click(cx.listener(move |t, _, _, cx| {
                        if !reloading {
                            t.refresh(cx);
                        }
                    })),
            )
    }

    fn list_el(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let list = ruled(
            div()
                .id("prs-list")
                .flex_none()
                .w(px(LIST_W))
                .h_full()
                .flex()
                .flex_col()
                .bg(paper())
                .overflow_y_scroll(),
        );
        match &self.list {
            Load::Loading => list.child(empty("loading pull requests", false)),
            Load::Failed(why) => list.child(empty(why.clone(), true)),
            Load::Ready(prs) if prs.is_empty() => list.child(empty("No open pull requests", false)),
            Load::Ready(prs) => list.children(prs.iter().map(|pr| {
                let n = pr.number;
                row(("pr", n), self.selected == Some(n))
                    .flex_col()
                    .items_start()
                    .gap(px(2.))
                    .py(px(6.))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(format!("#{n} {}", pr.title)),
                            )
                            .children(badge(pr).map(badge_el)),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .gap(px(8.))
                            .child(meta(pr.branch.clone()).flex_1().min_w_0())
                            .child(diff(pr))
                            .child(meta(pr.author.clone()).flex_none()),
                    )
                    .on_click(cx.listener(move |t, _, _, cx| t.select(n, cx)))
            })),
        }
    }

    /// The selected pull request: title, link, buttons, confirm row, description.
    fn detail(&mut self, cx: &mut Context<Self>) -> Stateful<Div> {
        let pane = ruled(
            div()
                .id("prs-detail")
                .flex_1()
                .min_w_0()
                .h_full()
                .bg(paper())
                .overflow_y_scroll(),
        );
        let Some(pr) = self.current().cloned() else {
            let empty_text = match &self.list {
                Load::Ready(prs) if prs.is_empty() => "nothing to review",
                Load::Ready(_) => "pick a pull request on the left",
                _ => "",
            };
            return pane.child(empty(empty_text, false));
        };
        let n = pr.number;
        let url = pr.url.clone();
        let doc = if pr.body.trim().is_empty() {
            meta("no description").into_any_element()
        } else {
            md_view::doc(self.doc(&pr), ink(), &Sel::new(0, n))
        };
        pane.child(
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .p(px(14.))
                .child(
                    div()
                        .text_size(px(19.))
                        .line_height(relative(1.25))
                        .font_weight(FontWeight::BOLD)
                        .child(format!("#{n} {}", pr.title)),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(8.))
                        .child(meta(format!("{} → main", pr.branch)))
                        .child(diff(&pr))
                        .child(meta(format!("by {}", pr.author))),
                )
                .child(
                    div()
                        .id("pr-link")
                        .text_color(accent_text())
                        .text_decoration_1()
                        .cursor_pointer()
                        .child(pr.url.clone())
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
                .child(self.actions(&pr, cx))
                .children(merge_waits(&pr).then(|| {
                    meta("conflicts with main: Claude resolves them, then it merges and deploys by itself")
                }))
                .children(pr.merging.then(|| {
                    div().text_color(accent_text()).child(
                        "Claude is resolving the conflicts; it merges and deploys by itself",
                    )
                }))
                .children(self.confirm_row(cx))
                .child(card().child(doc)),
        )
    }

    /// `Watch demo  Merge & deploy  Close PR`, with `Fix with Claude` or `Open fix chat` after the
    /// merge when it conflicts or a fix runs (no merge or fix while a queued merge runs), and what a
    /// button is doing.
    fn actions(&self, pr: &Pr, cx: &mut Context<Self>) -> Div {
        let what = actions(pr);
        let n = pr.number;
        let off = self.busy.is_some();
        let demo = pr.video.clone().map(|name| {
            button("pr-demo", "Watch demo")
                .when(off, |b| b.opacity(0.5))
                .on_click(cx.listener(move |t, _, _, cx| t.watch(name.clone(), cx)))
        });
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .children(demo)
            .children(what.merge.then(|| {
                button_dark("pr-merge", "Merge & deploy")
                    .when(off, |b| b.opacity(0.5))
                    .on_click(cx.listener(move |t, _, _, cx| t.ask(Confirm::Merge(n), cx)))
            }))
            .children(what.fix.map(|f| {
                match f {
                    FixButton::Start => button("pr-fix", "Fix with Claude")
                        .when(off, |b| b.opacity(0.5))
                        .on_click(cx.listener(move |t, _, _, cx| t.fix(n, cx))),
                    FixButton::Open(sid) => button("pr-open-fix", "Open fix chat")
                        .on_click(cx.listener(move |t, _, _, cx| t.show_chat(sid, cx))),
                }
            }))
            .child(
                button("pr-close", "Close PR")
                    .when(off, |b| b.opacity(0.5))
                    .on_click(cx.listener(move |t, _, _, cx| t.ask(Confirm::Close(n), cx))),
            )
            .children(self.busy.map(|b| meta(format!("{b}…"))))
    }

    /// The second click: what is about to happen, said plainly, with yes and cancel.
    fn confirm_row(&self, cx: &mut Context<Self>) -> Option<Div> {
        let (text, yes) = match self.confirm? {
            Confirm::Merge(n) => (
                format!("Squash-merge #{n} into main? The box deploys it to prod."),
                "Yes, merge & deploy",
            ),
            Confirm::Close(n) => (
                format!("Close #{n} without merging? Its branch is deleted."),
                "Yes, close it",
            ),
        };
        Some(ruled(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .px(px(10.))
                .py(px(6.))
                .bg(theme::accent_soft())
                .child(div().flex_1().min_w_0().child(text))
                .child(
                    button_dark("pr-yes", yes).on_click(cx.listener(|t, _, _, cx| t.confirmed(cx))),
                )
                .child(button("pr-no", "Cancel").on_click(cx.listener(|t, _, _, cx| t.cancel(cx)))),
        ))
    }
}
