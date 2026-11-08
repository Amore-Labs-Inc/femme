//! A picture or a video opened over the whole window, the way the drawing board opens: a dim
//! backdrop and a card with `‹ name` over the picture, fitted whole. A left click on a transcript
//! thumbnail or on a picture or video in the files panel opens it; Esc, `‹` or a click on the
//! backdrop closes it, and the keyboard goes back to whoever had it.
//!
//! A video plays in [`crate::video::Player`] with a bar under it: play/pause, the time, a track
//! to click or drag to a place, and the sound. Keys: space plays and pauses, left and right step
//! five seconds, `m` mutes.
//!
//! The shell makes the one entity ([`Lightbox::install`]) and renders it last, like the board; any
//! view opens it with [`Lightbox::show`] or [`Lightbox::play`], without a handle passed down.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    canvas, div, img, px, relative, AnyElement, App, AppContext as _, Bounds, Context, Div, Entity,
    FocusHandle, Global, Image, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    MouseMoveEvent, ObjectFit, ParentElement as _, Pixels, Point, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window,
};

use crate::theme::{
    self, bad, bar, bar_text, ink, muted, neon, panel, paper, FONT_MONO, FONT_PIXEL, FONT_UI,
    TB_SIZE,
};
use crate::ui::{button, ruled, title_button, Glyph};
use crate::video::{clock, Clip, Player};

/// The card's size as a share of the window, as the board's.
const CARD_W: f32 = 0.94;
const CARD_H: f32 = 0.92;

/// How far left and right step, in seconds.
const STEP: f64 = 5.;

/// What the card shows.
enum Body {
    Picture(Arc<Image>),
    /// `ffprobe` and the first frame are on their way.
    Opening,
    Video(Player),
    Failed(String),
}

/// What is open: its name and what it shows.
struct Open {
    name: SharedString,
    body: Body,
}

/// The layer over the window: empty while nothing is open.
pub struct Lightbox {
    open: Option<Open>,
    focus: FocusHandle,
    /// Who had the keyboard before it opened.
    restore: Option<FocusHandle>,
    /// Just opened: the next frame takes the keyboard.
    want_focus: bool,
    /// Where the video's track was last drawn, for a click on it to find its place.
    track: Rc<Cell<Bounds<Pixels>>>,
    /// The track is being dragged.
    scrubbing: bool,
    /// Counts opens, so a video that finishes opening after another took its place is let go.
    opened: u64,
}

struct Installed(Entity<Lightbox>);

impl Global for Installed {}

fn installed(cx: &App) -> Option<Entity<Lightbox>> {
    cx.try_global::<Installed>().map(|g| g.0.clone())
}

impl Lightbox {
    /// Makes the one lightbox and lets [`Lightbox::show`] find it. The shell renders what this
    /// returns as the last child of its root.
    pub fn install(cx: &mut App) -> Entity<Self> {
        let this = cx.new(|cx| Self {
            open: None,
            focus: cx.focus_handle(),
            restore: None,
            want_focus: false,
            track: Rc::default(),
            scrubbing: false,
            opened: 0,
        });
        cx.set_global(Installed(this.clone()));
        this
    }

    /// Opens `image`, called `name`, over the window and gives it the keyboard.
    pub fn show(name: &str, image: Arc<Image>, cx: &mut App) {
        if let Some(this) = installed(cx) {
            this.update(cx, |t, cx| t.set(name, Body::Picture(image), cx));
        }
    }

    /// Opens the video `clip`, called `name`, and plays it.
    pub fn play(name: &str, clip: Arc<Clip>, cx: &mut App) {
        let Some(this) = installed(cx) else {
            return;
        };
        let opened = this.update(cx, |t, cx| {
            t.set(name, Body::Opening, cx);
            t.opened
        });
        let player = cx
            .background_executor()
            .spawn(async move { Player::open(clip) });
        cx.spawn(async move |cx| {
            let player = player.await;
            this.update(cx, |t, cx| {
                let Some(open) = t.open.as_mut().filter(|_| t.opened == opened) else {
                    return;
                };
                open.body = match player {
                    Ok(p) => Body::Video(p),
                    Err(why) => Body::Failed(why),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn set(&mut self, name: &str, body: Body, cx: &mut Context<Self>) {
        self.forget(cx);
        self.want_focus |= self.open.is_none();
        self.opened += 1;
        self.open = Some(Open {
            name: name.to_owned().into(),
            body,
        });
        cx.notify();
    }

    /// Lets go of what is open: a video stops and its frames leave the sprite atlas.
    fn forget(&mut self, cx: &mut App) {
        self.scrubbing = false;
        if let Some(Open {
            body: Body::Video(mut p),
            ..
        }) = self.open.take()
        {
            for image in p.images() {
                cx.drop_image(image, None);
            }
        }
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open.is_none() {
            return;
        }
        self.forget(cx);
        if let Some(back) = self.restore.take() {
            window.focus(&back, cx);
        }
        cx.notify();
    }

    fn player(&mut self) -> Option<&mut Player> {
        match self.open.as_mut().map(|o| &mut o.body) {
            Some(Body::Video(p)) => Some(p),
            _ => None,
        }
    }

    /// Runs `f` on the video, and shows what went wrong in its place.
    fn with_player(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Player) -> Result<(), String>,
    ) {
        let Some(p) = self.player() else { return };
        if let Err(why) = f(p) {
            if let Some(open) = self.open.as_mut() {
                open.body = Body::Failed(why);
            }
        }
        cx.notify();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "escape" => self.close(window, cx),
            "space" => self.with_player(cx, Player::toggle),
            "left" => self.with_player(cx, |p| p.seek(p.position() - STEP)),
            "right" => self.with_player(cx, |p| p.seek(p.position() + STEP)),
            "m" => self.with_player(cx, |p| {
                p.toggle_mute();
                Ok(())
            }),
            _ => {}
        }
        // its keys stay here: the composer behind it must not type
        cx.stop_propagation();
    }

    /// The place on the track under `at`, in seconds.
    fn track_at(&self, at: Point<Pixels>, duration: f64) -> f64 {
        let b = self.track.get();
        let w = f32::from(b.size.width).max(1.);
        f64::from(((f32::from(at.x) - f32::from(b.origin.x)) / w).clamp(0., 1.)) * duration
    }

    fn scrub(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(d) = self.player().map(|p| p.duration()) else {
            return;
        };
        let to = self.track_at(at, d);
        self.with_player(cx, |p| p.seek(to));
    }

    /// The video's frame, fitted, and the bar under it.
    fn video(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let Some(p) = self.player() else {
            return div();
        };
        let (frame, gone) = p.frame();
        for image in gone {
            cx.drop_image(image, Some(window));
        }
        let (playing, decoding, at, length, silent, sound) = (
            p.playing(),
            p.decoding(),
            p.position(),
            p.duration(),
            p.muted(),
            p.has_sound(),
        );
        if playing || decoding {
            window.request_animation_frame();
        }
        let picture = div()
            .id("lightbox-video")
            .relative()
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .bg(gpui::black())
            .cursor_pointer()
            .on_click(cx.listener(|t, _, _, cx| t.with_player(cx, Player::toggle)))
            .when_some(frame, |d, frame| {
                d.child(
                    img(frame)
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                )
            });
        let share = if length > 0. {
            (at / length) as f32
        } else {
            0.
        };
        let track = self.track.clone();
        let rail = div()
            .id("lightbox-track")
            .relative()
            .flex_1()
            .h(px(18.))
            .cursor_pointer()
            .child(
                canvas(move |bounds, _, _| track.set(bounds), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(7.))
                    .h(px(4.))
                    .bg(muted()),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(7.))
                    .h(px(4.))
                    .w(relative(share.clamp(0., 1.)))
                    .bg(neon()),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|t, ev: &gpui::MouseDownEvent, _, cx| {
                    t.scrubbing = true;
                    t.scrub(ev.position, cx);
                }),
            )
            .on_mouse_move(cx.listener(|t, ev: &MouseMoveEvent, _, cx| {
                if t.scrubbing && ev.dragging() {
                    t.scrub(ev.position, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|t, _, _, _| t.scrubbing = false),
            );
        let controls = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .px(px(8.))
            .py(px(5.))
            .bg(panel())
            .font_family(FONT_MONO)
            .text_size(px(13.))
            .child(
                button("lightbox-play", if playing { "pause" } else { "play" })
                    .on_click(cx.listener(|t, _, _, cx| t.with_player(cx, Player::toggle))),
            )
            .child(
                div()
                    .flex_none()
                    .child(format!("{} / {}", clock(at), clock(length))),
            )
            .child(rail)
            .when(sound, |d| {
                d.child(
                    button(
                        "lightbox-mute",
                        if silent { "sound off" } else { "sound on" },
                    )
                    .on_click(cx.listener(|t, _, _, cx| {
                        t.with_player(cx, |p| {
                            p.toggle_mute();
                            Ok(())
                        })
                    })),
                )
            });
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(picture)
            .child(controls)
    }
}

/// A line in the middle of the card: what is on its way, or what went wrong.
fn note(text: impl Into<SharedString>, color: gpui::Rgba) -> Div {
    div()
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .bg(panel())
        .font_family(FONT_MONO)
        .text_color(color)
        .child(text.into())
}

impl Render for Lightbox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.open.is_some() && std::mem::take(&mut self.want_focus) {
            self.restore = window.focused(cx);
            window.focus(&self.focus, cx);
        }
        let Some(open) = &self.open else {
            return div().into_any_element();
        };
        let name = open.name.clone();
        let body: AnyElement = match &open.body {
            Body::Picture(image) => div()
                .relative()
                .flex_1()
                .min_h(px(0.))
                .w_full()
                .bg(panel())
                .child(
                    img(image.clone())
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element(),
            Body::Opening => note("opening the video", muted()).into_any_element(),
            Body::Failed(why) => note(format!("can't play it: {why}"), bad()).into_any_element(),
            Body::Video(_) => self.video(window, cx).into_any_element(),
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
            .child(div().flex_1().truncate().child(name));
        let card = ruled(div().flex().flex_col().bg(paper()))
            .shadow(theme::hard_shadow())
            .w(relative(CARD_W))
            .h(relative(CARD_H))
            .overflow_hidden()
            // a click on the card is not a click on the backdrop
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(title)
            .child(body);
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
