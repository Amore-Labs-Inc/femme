//! The style sheet of the skin: [`gallery`] lays every token and helper of [`super`] out in every
//! state. It reads the same look table the real controls use, so it cannot drift from the app.

use gpui::{
    div, px, relative, AnyElement, Div, IntoElement, ParentElement as _, Rgba, SharedString, Styled,
};

use super::{
    apply, button, button_box, button_dark, button_disabled, card, chip, dialog, dot, frame, icon,
    icon_button, icon_button_on, look, menu, menu_item, meta, row, ruled, ruled_sides, st, strip,
    title_bar, title_button, win, Glyph, Kind, State,
};
use crate::theme::{
    self, accent, accent_soft, accent_strong, bar, ink, neon, on_accent_strong, on_bar, FONT_BRAND,
    FONT_MONO, FONT_PIXEL, FONT_UI, RULE, TITLE_SIZE,
};

/// `#RRGGBB` of an opaque colour.
fn hex(c: Rgba) -> String {
    let byte = |v: f32| (v * 255.).round() as u8;
    format!("#{:02X}{:02X}{:02X}", byte(c.r), byte(c.g), byte(c.b))
}

/// A colour chip with its token name and hex.
fn swatch(name: &str, c: Rgba) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .w(px(84.))
        .child(ruled(div().h(px(30.)).bg(c)))
        .child(meta(name.to_string()))
        .child(meta(hex(c)))
}

/// A section heading: VT323 22 px with an ink underline.
fn heading(text: &str) -> Div {
    ruled_sides(
        div()
            .pb(px(3.))
            .font_family(FONT_PIXEL)
            .text_size(TITLE_SIZE)
            .line_height(relative(1.)),
        px(0.),
        px(0.),
        RULE,
        px(0.),
    )
    .child(SharedString::from(text.to_lowercase()))
}

/// A titled stack of samples.
fn block(title: &str) -> Div {
    div().flex().flex_col().gap(px(8.)).child(heading(title))
}

/// The states the matrix shows, left to right.
const STATES: [(State, &str); 4] = [
    (State::Idle, "idle"),
    (State::Hover, "hover"),
    (State::Pressed, "pressed"),
    (State::Disabled, "disabled"),
];

/// Width of the row-label column and of one state column of the matrix.
const LABEL_W: f32 = 78.;
const CELL_W: f32 = 100.;

/// One row of the state matrix: `content` drawn in a control of `kind`, frozen in each state.
fn matrix_row(name: &str, kind: Kind, content: impl Fn() -> AnyElement) -> Div {
    div()
        .flex()
        .items_center()
        .child(div().w(px(LABEL_W)).child(meta(name.to_string())))
        .children(STATES.iter().map(|(state, _)| {
            div()
                .w(px(CELL_W))
                .h(px(42.))
                .flex()
                .items_center()
                .child(apply(frame(kind), look(kind, *state)).child(content()))
        }))
}

/// Column one: the colours, the type, the marks.
fn column_tokens() -> Div {
    let palette: [(&str, Rgba); 23] = [
        ("DESK", theme::desk()),
        ("DESK_DOT", theme::desk_dot()),
        ("PAPER", theme::paper()),
        ("PANEL", theme::panel()),
        ("INK", theme::ink()),
        ("MUTED", theme::muted()),
        ("MUTED_TEXT", theme::muted_text()),
        ("HOVER", theme::hover()),
        ("NEON", theme::neon()),
        ("ACCENT_SOFT", theme::accent_soft()),
        ("ACCENT_DEEP", theme::accent_deep()),
        ("ACCENT_TEXT", theme::accent_text()),
        ("BAR", theme::bar()),
        ("BAR_TEXT", theme::bar_text()),
        ("BAR_HOVER", theme::bar_hover()),
        ("ON_BAR", theme::on_bar()),
        ("BAR_DIM", theme::bar_dim()),
        ("SUCCESS", theme::success()),
        ("WARN", theme::warn()),
        ("DANGER", theme::danger()),
        ("PR", theme::pr()),
        ("QUEUE", theme::queue()),
        ("DONE", theme::done()),
    ];
    let line = |font: &'static str, size: f32, text: &'static str| {
        div()
            .font_family(font)
            .text_size(px(size))
            .line_height(relative(1.3))
            .whitespace_nowrap()
            .child(text)
    };
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(18.))
        .w(px(380.))
        .child(
            block("colour").child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .children(palette.iter().map(|(n, c)| swatch(n, *c))),
            ),
        )
        .child(
            block("type")
                .child(line(FONT_PIXEL, 22., "title 22 px VT323: sessions"))
                .child(line(FONT_PIXEL, 20., "button 20 px VT323: send"))
                .child(line(
                    FONT_UI,
                    14.,
                    "Body 14 px Schibsted Grotesk, quick brown fox",
                ))
                .child(line(
                    FONT_MONO,
                    13.,
                    "mono 13 px JetBrains Mono NL: fn main() {}",
                ))
                .child(meta("meta 12 px mono, muted: ~/Web_Apps/femme"))
                .child(
                    div()
                        .relative()
                        .w(px(100.))
                        .h(px(40.))
                        .font_family(FONT_BRAND)
                        .text_size(px(32.))
                        .line_height(relative(1.1))
                        .child(
                            div()
                                .absolute()
                                .top(px(2.))
                                .left(px(2.))
                                .text_color(ink())
                                .child("femme"),
                        )
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .text_color(neon())
                                .child("femme"),
                        ),
                ),
        )
        .child(
            block("marks")
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(14.))
                        .child(dot_label("working", neon()))
                        .child(dot_label("blocked", theme::warn()))
                        .child(dot_label("failed", theme::danger()))
                        .child(dot_label("idle", theme::queue()))
                        .child(dot_label("done", theme::done())),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(6.))
                        .child(st("work 4", theme::work()))
                        .child(st("pause 2", theme::pause()))
                        .child(st("queue 5", theme::queue()))
                        .child(st("pr 2", theme::pr()))
                        .child(st("bad 1", theme::bad()))
                        .child(st("done 12", theme::done())),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(6.))
                        .child(chip("screenshot.png"))
                        .child(
                            chip("notes.md")
                                .child(icon_button("g-chip-x", Glyph::Close).size(px(18.))),
                        )
                        .child(chip("claude code")),
                ),
        )
}

/// A status dot with its name.
fn dot_label(name: &'static str, color: Rgba) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(5.))
        .child(dot(color))
        .child(meta(name))
}

/// The list rows in every state; hover and pressed are frozen (a real row does both live).
fn rows_block() -> Div {
    block("rows: idle, selected, hover, pressed").child(ruled(
        div()
            .flex()
            .flex_col()
            .w(px(250.))
            .child(
                row("g-r-1", false)
                    .child(dot(neon()))
                    .child("refactor auth"),
            )
            .child(
                row("g-r-2", true)
                    .child(dot(theme::warn()))
                    .child("term emulator"),
            )
            .child(
                row("g-r-3", false)
                    .bg(accent_soft())
                    .child(dot(theme::queue()))
                    .child("holder snapshot"),
            )
            .child(
                row("g-r-4", false)
                    .bg(accent_strong())
                    .text_color(on_accent_strong())
                    .child(dot(theme::danger()))
                    .child("wide chars"),
            ),
    ))
}

/// Column two: every control in every state, then the real ones, then the glyphs.
fn column_controls() -> Div {
    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(18.))
        .w(px(LABEL_W + 4. * CELL_W + 8.))
        .child(
            block("buttons")
                .child(
                    div().flex().child(div().w(px(LABEL_W))).children(
                        STATES
                            .iter()
                            .map(|(_, n)| div().w(px(CELL_W)).child(meta(*n))),
                    ),
                )
                .child(matrix_row("button", Kind::Light, || {
                    "send".into_any_element()
                }))
                .child(matrix_row("dark", Kind::Dark, || "send".into_any_element()))
                .child(matrix_row("icon", Kind::Ghost, || {
                    icon(Glyph::Plus).into_any_element()
                }))
                .child(matrix_row("icon on", Kind::GhostOn, || {
                    icon(Glyph::Files).into_any_element()
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .child(div().w(px(LABEL_W)).child(meta("title")))
                        .child(div().flex().bg(bar()).child(div().flex().children(
                            STATES.iter().map(|(state, _)| {
                                div().w(px(CELL_W)).h(px(32.)).flex().items_center().child(
                                    apply(frame(Kind::OnInk), look(Kind::OnInk, *state))
                                        .child(icon(Glyph::Close)),
                                )
                            }),
                        ))),
                ),
        )
        .child(
            block("live: point at them, press them").child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(10.))
                    .p(px(4.))
                    .child(button("g-live-1", "hover me"))
                    .child(button_dark("g-live-2", "press me"))
                    .child(button_disabled("g-live-3", "disabled"))
                    .child(icon_button("g-live-4", Glyph::Plus))
                    .child(icon_button_on("g-live-5", Glyph::Terminal))
                    .child(
                        button_box("g-live-6")
                            .child(
                                div()
                                    .font_family(FONT_BRAND)
                                    .text_size(px(24.))
                                    .line_height(relative(1.))
                                    .child("femme"),
                            )
                            .child(icon(Glyph::Down)),
                    ),
            ),
        )
        .child(
            block("glyphs").child(div().flex().flex_wrap().gap(px(8.)).children(
                Glyph::ALL.iter().enumerate().map(|(i, g)| {
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(2.))
                        .w(px(64.))
                        .child(icon_button(("g-icon", i), *g))
                        .child(meta(g.name()))
                }),
            )),
        )
        .child(rows_block())
}

/// Column three: windows, cards, dialogs, menus, rows, the strip.
fn column_surfaces() -> Div {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap(px(18.))
        .child(
            block("window and title bar").child(
                win()
                    .w(px(300.))
                    .child(title_bar("sessions").child(title_button("g-win-x", Glyph::Close)))
                    .child(
                        div()
                            .p(px(10.))
                            .child("Paper body; the desk shows in the gutter."),
                    ),
            ),
        )
        .child(
            block("card (small hard shadow)").child(
                card()
                    .w(px(300.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(div().text_size(px(17.)).child("refactor auth middleware"))
                    .child(meta("~/Web_Apps/femme  claude")),
            ),
        )
        .child(
            block("dialog (large hard shadow)").child(
                div().pr(px(4.)).pb(px(4.)).child(
                    dialog()
                        .w(px(300.))
                        .child(
                            title_bar("accounts")
                                .text_color(on_bar())
                                .child(title_button("g-dlg-x", Glyph::Back)),
                        )
                        .child(
                            div()
                                .p(px(3.))
                                .child(card().child("paper card on the grey frame")),
                        ),
                ),
            ),
        )
        .child(
            block("menu and strip")
                .child(
                    div()
                        .flex()
                        .gap(px(18.))
                        .child(
                            div().pr(px(4.)).pb(px(4.)).child(
                                menu()
                                    .w(px(190.))
                                    .child(menu_item("g-m-1").child("femme 0.1.0"))
                                    .child(
                                        menu_item("g-m-2").bg(accent()).child("check for updates"),
                                    )
                                    .child(
                                        menu_item("g-m-3")
                                            .bg(accent_strong())
                                            .text_color(on_accent_strong())
                                            .child("accounts"),
                                    )
                                    .child(
                                        menu_item("g-m-4")
                                            .child("reconnect")
                                            .child(div().flex_1())
                                            .child(meta("ctrl-r")),
                                    ),
                            ),
                        )
                        .child(
                            div().h(px(120.)).flex().child(
                                strip("g-strip")
                                    .child(icon_button_on("g-s-1", Glyph::Terminal))
                                    .child(icon_button("g-s-2", Glyph::Files)),
                            ),
                        ),
                )
                .child(meta("frozen: 2nd item hover, 3rd pressed")),
        )
}

/// The style sheet: every token and every helper of this module, each interactive control in every
/// state (idle, hover, pressed, disabled; the "live" row responds to the real pointer). Nothing in the
/// app uses it; mount it in place of the shell body to review a change to the skin, and screenshot it.
///
/// It is a plain element (no view state), so it can be dropped anywhere an element goes; give it the
/// whole window under the desk's [`theme::GUTTER`].
pub fn gallery() -> impl IntoElement {
    win()
        .size_full()
        .child(title_bar("style sheet - tokens and helpers"))
        .child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .gap(px(26.))
                .p(px(14.))
                .overflow_hidden()
                .child(column_tokens())
                .child(column_controls())
                .child(column_surfaces()),
        )
}
