//! A picture (or a video the agent showed, which is an animated picture) opened over the whole
//! window, the way the drawing board opens: a dim backdrop and a card with `‹ name` over the
//! picture, fitted whole. A left click on a transcript thumbnail opens it; Esc, `‹` or a click on
//! the backdrop closes it, and the keyboard goes back to whoever had it.
//!
//! The shell makes the one entity ([`Lightbox::install`]) and renders it last, like the board; any
//! view opens it with [`Lightbox::show`], without a handle passed down.

use std::sync::Arc;

use gpui::{
    div, img, px, relative, App, AppContext as _, Context, Entity, FocusHandle, Global, Image,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ObjectFit, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window,
};

use crate::theme::{self, bar, bar_text, ink, panel, paper, FONT_PIXEL, FONT_UI, TB_SIZE};
use crate::ui::{ruled, title_button, Glyph};

/// The card's size as a share of the window, as the board's.
const CARD_W: f32 = 0.94;
const CARD_H: f32 = 0.92;

/// What is open: its name and its picture.
struct Open {
    name: SharedString,
    image: Arc<Image>,
}

/// The layer over the window: empty while nothing is open.
pub struct Lightbox {
    open: Option<Open>,
    focus: FocusHandle,
    /// Who had the keyboard before it opened.
    restore: Option<FocusHandle>,
}

struct Installed(Entity<Lightbox>);

impl Global for Installed {}

impl Lightbox {
    /// Makes the one lightbox and lets [`Lightbox::show`] find it. The shell renders what this
    /// returns as the last child of its root.
    pub fn install(cx: &mut App) -> Entity<Self> {
        let this = cx.new(|cx| Self {
            open: None,
            focus: cx.focus_handle(),
            restore: None,
        });
        cx.set_global(Installed(this.clone()));
        this
    }

    /// Opens `image`, called `name`, over the window and gives it the keyboard.
    pub fn show(name: &str, image: Arc<Image>, window: &mut Window, cx: &mut App) {
        let Some(this) = cx.try_global::<Installed>().map(|g| g.0.clone()) else {
            return;
        };
        this.update(cx, |t, cx| {
            if t.open.is_none() {
                t.restore = window.focused(cx);
            }
            t.open = Some(Open {
                name: name.to_owned().into(),
                image,
            });
            window.focus(&t.focus, cx);
            cx.notify();
        });
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open.take().is_none() {
            return;
        }
        if let Some(back) = self.restore.take() {
            window.focus(&back, cx);
        }
        cx.notify();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key == "escape" {
            self.close(window, cx);
        }
        // its keys stay here: the composer behind it must not type
        cx.stop_propagation();
    }
}

impl Render for Lightbox {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(open) = &self.open else {
            return div().into_any_element();
        };
        let title = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .py(px(3.))
            .bg(bar())
            .text_color(bar_text())
            .font_family(FONT_PIXEL)
            .text_size(TB_SIZE)
            .line_height(relative(1.))
            .child(
                title_button("lightbox-close", Glyph::Back)
                    .on_click(cx.listener(|t, _, window, cx| t.close(window, cx))),
            )
            .child(div().flex_1().truncate().child(open.name.clone()));
        let picture = div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .bg(panel())
            .child(
                img(open.image.clone())
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .object_fit(ObjectFit::Contain),
            );
        let card = ruled(div().flex().flex_col().bg(paper()))
            .shadow(theme::hard_shadow())
            .w(relative(CARD_W))
            .h(relative(CARD_H))
            .overflow_hidden()
            // a click on the card is not a click on the backdrop
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(title)
            .child(picture);
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::scrim())
            .occlude()
            .track_focus(&self.focus)
            .font_family(FONT_UI)
            .text_color(ink())
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|t, _, window, cx| t.close(window, cx)),
            )
            .child(card)
            .into_any_element()
    }
}
