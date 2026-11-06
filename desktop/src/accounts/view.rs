//! The card's elements, in the order of the board's page: the accounts (each with its usage bars and
//! its two rotation caps), AWS, the API keys, a Claude sign-in in progress, and the line that says
//! when the box moves on. Everything here only reads the overlay; the buttons call back into
//! [`super::flow`].

use gpui::prelude::FluentBuilder as _;
use gpui::{
    div, px, relative, AnyElement, Context, Div, ElementId, Entity, FontWeight,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Pixels,
    SharedString, Size, Stateful, StatefulInteractiveElement as _, Styled as _,
};
use gpui_component::input::{Input, InputState};
use gpui_component::Sizable as _;

use super::model::{Account, AccountList, Key, Meta, Usage};
use super::{clock, AccountsOverlay, CapStatus, Load};
use crate::motion::Appear;
use crate::theme::{
    self, bad, bar, hover, ink, muted, on_bar, panel, paper, pause, work, BODY_SIZE, FONT_MONO,
    FONT_PIXEL, FONT_UI, RULE, TB_SIZE,
};
use crate::ui::{button, button_dark, card, dialog, meta, ruled, ruled_sides, st};

/// The footer: where the box moves off an account, and what its scheduler says.
pub(super) fn usage_line(meta: &Meta) -> String {
    let mut line = match meta.cap {
        Some(cap) => format!("switches account at {cap}%"),
        None => "switches account at its cap".to_owned(),
    };
    line.push_str(" · agents pause when every account is past it");
    if let Some(state) = &meta.state {
        line.push_str(" · ");
        line.push_str(state);
    }
    line
}

/// What a cap box holds for `cap`: its percent, or nothing for the default.
pub(super) fn cap_text(cap: Option<u8>) -> String {
    cap.map_or_else(String::new, |c| c.to_string())
}

/// `.empty`: a muted, centred line where a list would be (red when it is an error).
fn empty(text: impl Into<SharedString>, error: bool) -> Div {
    div()
        .py(px(14.))
        .text_center()
        .text_color(if error { bad() } else { muted() })
        .child(text.into())
}

/// The green bar on the left of the account in use (and of AWS when the CLI has an identity).
fn active_bar() -> Div {
    div()
        .absolute()
        .left_0()
        .top_0()
        .bottom_0()
        .w(px(6.))
        .bg(work())
}

/// `.meter`: a bar `fill` (0 to 1) full, amber when the window has reached its cap.
fn meter(fill: f32, hot: bool) -> Div {
    ruled(
        div()
            .flex_1()
            .h(px(12.))
            .bg(paper())
            .child(
                div()
                    .h_full()
                    .w(relative(fill))
                    .bg(if hot { pause() } else { ink() }),
            ),
    )
}

/// One window of an account: `5h  [bar]  31% · 3h 0m`.
fn bar_row(label: &'static str, usage: &Usage, now: i64, cap: Option<u8>) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(meta(label).flex_none().w(px(20.)))
        .child(meter(usage.fill(), usage.reaches(cap)))
        .child(meta(usage.text(now)).flex_none().w(px(132.)))
}

/// A button of a card row: the pixel-font box of `ui::button`, short, dimmed while `off`.
fn small(id: impl Into<ElementId>, label: impl Into<SharedString>, off: bool) -> Stateful<Div> {
    button(id, label).h(px(26.)).when(off, |b| b.opacity(0.5))
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

/// A text box of the page: the input in a ruled paper frame.
fn field(state: &Entity<InputState>) -> Div {
    ruled(
        div()
            .h(px(36.))
            .flex()
            .items_center()
            .bg(paper())
            .child(Input::new(state).appearance(false)),
    )
}

/// A cap box: two digits in a small ruled frame.
fn cap_box(state: &Entity<InputState>) -> Div {
    ruled(
        div()
            .w(px(64.))
            .h(px(28.))
            .flex()
            .items_center()
            .bg(paper())
            .child(Input::new(state).appearance(false).small()),
    )
}

impl AccountsOverlay {
    /// The whole layer: the dimmed window and the card in its middle. `viewport` is the window's
    /// size, which the card's height is kept under.
    pub(super) fn layer(
        &self,
        viewport: Size<Pixels>,
        a: Appear,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = clock::now();
        let offset = clock::local_offset(now);
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::scrim())
            .opacity(a.opacity())
            .occlude()
            .key_context("Accounts")
            .track_focus(&self.focus)
            .font_family(FONT_UI)
            .text_size(BODY_SIZE)
            .line_height(relative(1.4))
            .text_color(ink())
            .on_key_down(cx.listener(|t, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key == "escape" {
                    t.dismiss(window, cx);
                    cx.stop_propagation();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|t, _, window, cx| t.dismiss(window, cx)),
            )
            // Centred, so a top margin moves the card by half of it: 12 px of margin is a 6 px rise.
            .child(self.card_el(viewport, now, offset, cx).mt(a.offset(12.)))
            .into_any_element()
    }

    fn card_el(
        &self,
        viewport: Size<Pixels>,
        now: i64,
        offset: i64,
        cx: &mut Context<Self>,
    ) -> Div {
        // The title bar is about 34px and the notice strip under it about 24px; the card keeps a
        // margin of 4% of the window top and bottom.
        let strip = px(if self.notice.is_some() { 24. } else { 0. });
        let body_max = viewport.height * 0.92 - px(34.) - strip;
        dialog()
            .w(px(760.))
            .max_w(relative(0.95))
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(self.title_bar(cx))
            .children(self.notice_line())
            .child(
                div()
                    .id("accounts-body")
                    .max_h(body_max)
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .p(px(3.))
                            .child(self.accounts_list(now, offset, cx))
                            .children(self.aws_card())
                            .child(self.keys_card(now, cx))
                            .children(self.login_card(cx))
                            .child(self.footer()),
                    ),
            )
    }

    /// `‹  accounts  + add`: back, the title, and the Claude sign-in.
    fn title_bar(&self, cx: &mut Context<Self>) -> Div {
        let signing_in = matches!(&self.accounts, Load::Ready(d) if d.login.is_some());
        let can_add = matches!(&self.accounts, Load::Ready(_)) && !self.busy && !signing_in;
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
                title_button("accounts-close", "‹")
                    .on_click(cx.listener(|t, _, window, cx| t.dismiss(window, cx))),
            )
            .child(div().flex_1().child("accounts"))
            .child(
                title_button("accounts-add", "+ add")
                    .when(!can_add, |b| b.opacity(0.5))
                    .on_click(cx.listener(move |t, _, _, cx| {
                        if can_add {
                            t.login_start(cx);
                        }
                    })),
            )
    }

    fn accounts_list(&self, now: i64, offset: i64, cx: &mut Context<Self>) -> Div {
        let list = div().flex().flex_col().gap(px(3.));
        match &self.accounts {
            Load::Loading => list.child(empty("loading accounts", false)),
            Load::Failed(why) => list.child(empty(why.clone(), true)),
            Load::Ready(doc) => match &doc.list {
                AccountList::Pending => list.child(empty("loading accounts", false)),
                AccountList::Error(why) => list.child(empty(why.clone(), true)),
                AccountList::Rows(rows) if rows.is_empty() => {
                    list.child(empty("no accounts yet: + add", false))
                }
                AccountList::Rows(rows) => {
                    list.children(rows.iter().map(|a| self.account_card(a, now, offset, cx)))
                }
            },
        }
    }

    /// One account: its title line with the tags and `switch`, a bar per window, a note when the
    /// reading is old or missing, and its two caps.
    fn account_card(&self, a: &Account, now: i64, offset: i64, cx: &mut Context<Self>) -> Div {
        let caps = self.caps_of(a.number);
        let five_cap = caps.five_hour.or(self.meta.cap);
        let switch = (!a.active).then(|| {
            let n = a.number;
            div().ml_auto().child(
                small(("switch", n), "switch", self.busy)
                    .on_click(cx.listener(move |t, _, _, cx| t.switch(n, cx))),
            )
        });
        let head = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .child(format!("{} · {}", a.number, a.title))
            .when(a.active, |d| d.child(st("active", work())))
            .when(a.disabled, |d| d.child(st("disabled", muted())))
            .children(a.scoped.iter().map(|s| st(s.tag(offset), ink())))
            .children(switch);
        let bars = a.bars().into_iter().map(|(label, usage)| {
            let cap = if label == "5h" { five_cap } else { caps.weekly };
            bar_row(label, usage, now, cap)
        });
        card()
            .relative()
            .flex()
            .flex_col()
            .gap(px(6.))
            .text_size(px(16.))
            .when(a.active, |d| d.pl(px(16.)).child(active_bar()))
            .child(head)
            .children(bars)
            .children(a.note(now).map(meta))
            .children(self.caps_row(a.number))
    }

    /// `rotate at  5h [__] %  7d [__] %`: the two caps of an account, empty for the box's default.
    fn caps_row(&self, number: u32) -> Option<Div> {
        let row = self.rows.get(&number)?;
        let status = match &row.status {
            CapStatus::Idle => None,
            CapStatus::Saving => Some(meta("saving")),
            CapStatus::Saved => Some(meta("saved").text_color(work())),
            CapStatus::Bad(why) => Some(meta(why.clone()).text_color(bad())),
        };
        Some(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(6.))
                .child(meta("rotate at"))
                .child(meta("5h"))
                .child(cap_box(&row.five))
                .child(meta("%"))
                .child(meta("7d"))
                .child(cap_box(&row.week))
                .child(meta("%"))
                .children(status),
        )
    }

    /// AWS: who the box's CLI runs as.
    fn aws_card(&self) -> Option<Div> {
        let Load::Ready(doc) = &self.accounts else {
            return None;
        };
        let aws = &doc.aws;
        Some(
            card()
                .relative()
                .flex()
                .flex_col()
                .gap(px(2.))
                .when(aws.active, |d| d.pl(px(16.)).child(active_bar()))
                .child(div().text_size(px(16.)).child("AWS"))
                .child(meta(aws.line.clone()))
                .children(aws.detail.clone().filter(|_| !aws.active).map(meta)),
        )
    }

    /// The API keys: the list, the one just made, and the form that makes one.
    fn keys_card(&self, now: i64, cx: &mut Context<Self>) -> Div {
        let rows = div().flex().flex_col().gap(px(6.));
        let rows = match &self.keys {
            Load::Loading => rows.child(meta("loading keys")),
            Load::Failed(why) => rows.child(meta(why.clone()).text_color(bad())),
            Load::Ready(keys) if keys.is_empty() => rows.child(meta("no keys yet")),
            Load::Ready(keys) => rows.children(keys.iter().map(|k| self.key_row(k, now, cx))),
        };
        card()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .child(div().text_size(px(16.)).child("API keys"))
                    .child(meta("another agent's standing Claude · /api/openapi.json")),
            )
            .child(rows)
            .children(self.secret_box(cx))
            .children(self.key_form(cx))
    }

    /// One key: its line, `connect GitHub` (or the code to enter, with `open` and `cancel`) and
    /// `revoke` (which asks first).
    fn key_row(&self, k: &Key, now: i64, cx: &mut Context<Self>) -> Div {
        let name = k.name.clone();
        let mut row = div().flex().flex_col().gap(px(4.));
        if self.revoking.as_deref() == Some(k.name.as_str()) {
            let (n, m) = (name.clone(), name.clone());
            return row.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(8.))
                    .child(format!("revoke {name}? whatever uses it stops working."))
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .ml_auto()
                            .child(
                                small(format!("revoke-yes-{name}"), "revoke", self.busy)
                                    .on_click(cx.listener(move |t, _, _, cx| t.revoke(&n, cx))),
                            )
                            .child(
                                small(format!("revoke-no-{m}"), "keep", false)
                                    .on_click(cx.listener(|t, _, _, cx| t.keep(cx))),
                            ),
                    ),
            );
        }
        let connect = k.github.is_none().then(|| {
            let n = name.clone();
            small(format!("gh-{name}"), "connect GitHub", self.busy)
                .on_click(cx.listener(move |t, _, _, cx| t.github(&n, true, cx)))
        });
        let n = name.clone();
        let revoke = small(format!("revoke-{name}"), "revoke", self.busy)
            .on_click(cx.listener(move |t, _, _, cx| t.ask_revoke(n.clone(), cx)));
        row = row.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(meta(k.line(now)).flex_1().min_w_0())
                .children(connect)
                .child(revoke),
        );
        if let Some(d) = &k.github {
            let (n, url) = (name.clone(), d.url.clone());
            row = row.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .child("the account that should push enters")
                    .child(
                        div()
                            .font_family(FONT_MONO)
                            .font_weight(FontWeight::BOLD)
                            .child(d.code.clone()),
                    )
                    .child("at github.com/login/device")
                    .child(
                        div()
                            .flex()
                            .gap(px(6.))
                            .ml_auto()
                            .child(
                                small(format!("gh-open-{name}"), "open", false)
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            )
                            .child(
                                small(format!("gh-x-{name}"), "cancel", self.busy).on_click(
                                    cx.listener(move |t, _, _, cx| t.github(&n, false, cx)),
                                ),
                            ),
                    ),
            );
        }
        if let Some(why) = &k.error {
            row = row.child(meta(why.clone()).text_color(bad()));
        }
        row
    }

    /// The key just made: its secret, which the box never shows again.
    fn secret_box(&self, cx: &mut Context<Self>) -> Option<Div> {
        let (name, key) = self.secret.as_ref()?;
        Some(ruled(
            div()
                .bg(panel())
                .p(px(8.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .font_family(FONT_MONO)
                                .text_size(px(13.))
                                .child(key.clone()),
                        )
                        .child(
                            small("key-copy", "copy", false)
                                .on_click(cx.listener(|t, _, _, cx| t.copy_secret(cx))),
                        )
                        .child(small("key-done", "done", false).on_click(cx.listener(
                            |t, _, _, cx| {
                                t.secret = None;
                                cx.notify();
                            },
                        ))),
                )
                .child(meta(format!("{name}: shown once, copy it now"))),
        ))
    }

    /// The form that makes a key: a name, an optional GitHub token, `+ key`. Not there while the
    /// keys could not be read (a daemon too old for them cannot make one either).
    fn key_form(&self, cx: &mut Context<Self>) -> Option<Div> {
        if !matches!(self.keys, Load::Ready(_)) {
            return None;
        }
        let f = self.fields.as_ref()?;
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(field(&f.key_name))
                .child(field(&f.key_token))
                .child(
                    button_dark("key-add", "+ key")
                        .when(self.busy, |b| b.opacity(0.5))
                        .on_click(cx.listener(|t, _, _, cx| t.key_add(cx))),
                ),
        )
    }

    /// A Claude sign-in in progress: its link, a box for the code, and the two buttons.
    fn login_card(&self, cx: &mut Context<Self>) -> Option<Div> {
        let Load::Ready(doc) = &self.accounts else {
            return None;
        };
        let url = doc.login.clone()?;
        let f = self.fields.as_ref()?;
        Some(
            card()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .id("login-link")
                        .underline()
                        .cursor_pointer()
                        .child("1. Sign in with the new account")
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
                .child(field(&f.code))
                .child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .child(
                            button_dark("login-go", "Add account")
                                .flex_1()
                                .when(self.busy, |b| b.opacity(0.5))
                                .on_click(cx.listener(|t, _, _, cx| t.login_code(cx))),
                        )
                        .child(
                            button("login-x", "Cancel")
                                .when(self.busy, |b| b.opacity(0.5))
                                .on_click(cx.listener(|t, _, _, cx| t.login_cancel(cx))),
                        ),
                ),
        )
    }

    /// The last refusal or confirmation, in red or green, pinned under the title bar so it shows
    /// wherever the list is scrolled to.
    fn notice_line(&self) -> Option<Div> {
        self.notice.as_ref().map(|n| {
            ruled_sides(
                div()
                    .flex_none()
                    .px(px(8.))
                    .py(px(2.))
                    .bg(paper())
                    .text_color(if n.bad { bad() } else { work() })
                    .child(n.text.clone()),
                px(0.),
                px(0.),
                RULE,
                px(0.),
            )
        })
    }

    fn footer(&self) -> Div {
        div()
            .py(px(8.))
            .text_center()
            .text_size(px(13.))
            .text_color(muted())
            .child(usage_line(&self.meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_footer_says_where_the_box_moves_on() {
        let mut m = Meta::default();
        assert_eq!(
            usage_line(&m),
            "switches account at its cap · agents pause when every account is past it"
        );
        m.cap = Some(60);
        m.state = Some("working (31% used, pause at 60%)".into());
        assert_eq!(
            usage_line(&m),
            "switches account at 60% · agents pause when every account is past it · working (31% used, pause at 60%)"
        );
    }

    #[test]
    fn a_cap_box_holds_the_percent_or_nothing() {
        assert_eq!(cap_text(Some(80)), "80");
        assert_eq!(cap_text(None), "");
    }
}
